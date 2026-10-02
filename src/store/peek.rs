// `/peek` 查看排队投稿，以及超级管理员把投稿从队列里撤下。

use std::collections::HashMap;

use anyhow::Result;
use jiff::Timestamp;
use sea_orm::{
    ColumnTrait, EntityTrait, QueryFilter, QueryOrder, QuerySelect, QueryTrait, Set,
    sea_query::{Expr, ExprTrait, JoinType},
};
use serde_json::json;
use teloxide::types::UserId;

use super::{
    Store,
    codec::{decode_user_id, timestamp, user_id},
    entities::{categories, post_audit_log, post_messages, posts},
};
use crate::model::{Category, CategoryId, MediaKind, PostAuditKind, PostId, PostStatus};

/// 列表里的一篇投稿。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeekItem {
    pub id: PostId,
    pub category: Category,
    /// 投稿里每条带媒体的消息的类型，纯文字投稿为空。
    pub media: Vec<MediaKind>,
    /// 投稿里第一段文字（正文或说明文字）。
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoveOutcome {
    Removed {
        owner: UserId,
    },
    /// 投稿不存在，或已经不在队列里（已被选中发布、已发布、已取消……）。
    NotQueued,
}

impl Store {
    /// 各管理员排队中的投稿数，没有排队投稿的管理员不在其中。
    pub async fn queued_counts_by_admin(&self) -> Result<HashMap<UserId, i64>> {
        let rows = posts::Entity::find()
            .filter(posts::Column::Status.eq(PostStatus::Queued))
            .select_only()
            .column(posts::Column::OwnerAdminId)
            .column_as(posts::Column::Id.count(), "count")
            .group_by(posts::Column::OwnerAdminId)
            .into_tuple::<(Option<i64>, i64)>()
            .all(&self.db)
            .await?;
        let mut counts = HashMap::new();
        for (owner, count) in rows {
            if let Some(owner) = owner {
                counts.insert(decode_user_id(owner)?, count);
            }
        }
        Ok(counts)
    }

    /// 按 `ids` 的顺序取这些投稿在列表里显示的内容。不存在或没有分类的投稿不在结果里。
    pub async fn peek_items(&self, ids: &[PostId]) -> Result<Vec<PeekItem>> {
        let raw: Vec<i64> = ids.iter().map(|id| id.0).collect();
        let mut query = posts::Entity::find().filter(posts::Column::Id.is_in(raw.clone()));
        QueryTrait::query(&mut query).join(
            JoinType::InnerJoin,
            categories::Entity,
            Expr::col((posts::Entity, posts::Column::CategoryId))
                .equals((categories::Entity, categories::Column::Id)),
        );
        let categories: HashMap<i64, Category> = query
            .select_only()
            .column(posts::Column::Id)
            .column(categories::Column::Id)
            .column(categories::Column::Label)
            .into_tuple::<(i64, i64, String)>()
            .all(&self.db)
            .await?
            .into_iter()
            .map(|(id, category, label)| {
                let category = Category {
                    id: CategoryId(category),
                    label,
                };
                (id, category)
            })
            .collect();
        let messages = post_messages::Entity::find()
            .filter(post_messages::Column::PostId.is_in(raw))
            .order_by_asc(post_messages::Column::PostId)
            .order_by_asc(post_messages::Column::Position)
            .all(&self.db)
            .await?;
        let items = ids
            .iter()
            .filter_map(|id| {
                let category = categories.get(&id.0)?.clone();
                let own = messages.iter().filter(|message| message.post_id == id.0);
                let media = own
                    .clone()
                    .filter_map(|message| message.media_kind)
                    .collect();
                let text = own
                    .filter_map(|message| message.text.clone())
                    .find(|text| !text.trim().is_empty());
                Some(PeekItem {
                    id: *id,
                    category,
                    media,
                    text,
                })
            })
            .collect();
        Ok(items)
    }

    /// 超级管理员把一篇还在队列里的投稿撤下，`note` 是告诉原管理员的理由。
    /// 用「比较并交换」更新，和发布时段抢同一篇时只有先到的成功。
    pub async fn remove_queued_post(
        &self,
        post: PostId,
        by: UserId,
        note: Option<&str>,
        now: Timestamp,
    ) -> Result<RemoveOutcome> {
        self.transaction(async move |conn| {
            let by_id = user_id(by)?;
            let changed = posts::Entity::update_many()
                .set(posts::ActiveModel {
                    status: Set(PostStatus::Rejected),
                    reviewed_by_admin_id: Set(Some(by_id)),
                    reviewed_at: Set(Some(timestamp(now)?)),
                    review_note: Set(note.map(str::to_owned)),
                    ..Default::default()
                })
                .filter(posts::Column::Id.eq(post.0))
                .filter(posts::Column::Status.eq(PostStatus::Queued))
                .exec(conn)
                .await?;
            if changed.rows_affected == 0 {
                return Ok(RemoveOutcome::NotQueued);
            }
            let owner = posts::Entity::find_by_id(post.0)
                .one(conn)
                .await?
                .and_then(|row| row.owner_admin_id);
            let Some(owner) = owner else {
                return Ok(RemoveOutcome::NotQueued);
            };
            let detail = note.map(|note| json!({ "note": note }).to_string());
            post_audit_log::Entity::insert(post_audit_log::ActiveModel {
                post_id: Set(post.0),
                actor_user_id: Set(by_id),
                kind: Set(PostAuditKind::Removed),
                detail_json: Set(detail),
                created_at: Set(timestamp(now)?),
                ..Default::default()
            })
            .exec_without_returning(conn)
            .await?;
            anyhow::Ok(RemoveOutcome::Removed {
                owner: decode_user_id(owner)?,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::store::test_support::{ADMIN, incoming, incoming_media, queue};

    const SUPER: UserId = UserId(1);

    async fn queued(store: &Store, id: i32, label: &str, at: i64) -> PostId {
        let post = store
            .create_draft(
                &[incoming(id, &format!("post {id}"))],
                Timestamp::UNIX_EPOCH,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        queue(store, post, label, Timestamp::from_second(at).unwrap()).await;
        post
    }

    #[tokio::test]
    async fn counts_queued_posts_per_admin() {
        let (_directory, store) = Store::open_temporary().await;
        assert!(store.queued_counts_by_admin().await.unwrap().is_empty());
        queued(&store, 1, "gi", 10).await;
        queued(&store, 2, "hsr", 20).await;
        // 草稿不算。
        store
            .create_draft(&[incoming(3, "draft")], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap();
        let counts = store.queued_counts_by_admin().await.unwrap();
        assert_eq!(counts.get(&ADMIN), Some(&2));
        assert_eq!(counts.len(), 1);
    }

    #[tokio::test]
    async fn items_follow_the_requested_order_and_skip_unknown_posts() {
        let (_directory, store) = Store::open_temporary().await;
        let first = queued(&store, 1, "gi", 10).await;
        let second = queued(&store, 2, "hsr", 20).await;
        // 草稿没有分类。
        let draft = store
            .create_draft(&[incoming(3, "draft")], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap();

        let items = store
            .peek_items(&[second, PostId(999), draft, first])
            .await
            .unwrap();
        assert_eq!(
            items.iter().map(|item| item.id).collect::<Vec<_>>(),
            [second, first]
        );
        assert_eq!(items[0].category.label, "hsr");
        assert_eq!(items[0].text.as_deref(), Some("post 2"));
        assert!(store.peek_items(&[]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn describes_media_posts_by_their_media_and_caption() {
        let (_directory, store) = Store::open_temporary().await;
        let mut first = incoming_media(1, MediaKind::Photo, Some("caption"));
        first.submitter = ADMIN;
        let second = incoming_media(2, MediaKind::Photo, None);
        let post = store
            .create_draft(&[first, second], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap();
        queue(&store, post, "gi", Timestamp::UNIX_EPOCH).await;
        let item = &store.peek_items(&[post]).await.unwrap()[0];
        assert_eq!(item.media, [MediaKind::Photo, MediaKind::Photo]);
        assert_eq!(item.text.as_deref(), Some("caption"));
    }

    #[tokio::test]
    async fn removing_marks_the_post_rejected_and_keeps_the_reason() {
        let (_directory, store) = Store::open_temporary().await;
        store
            .sync_admins(&[ADMIN, SUPER], Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        let post = queued(&store, 1, "gi", 10).await;
        let outcome = store
            .remove_queued_post(post, SUPER, Some("重复了"), Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        assert_eq!(outcome, RemoveOutcome::Removed { owner: ADMIN });

        let info = store.review_info(post).await.unwrap().unwrap();
        assert_eq!(info.status, PostStatus::Rejected);
        assert_eq!(info.review_note.as_deref(), Some("重复了"));
        assert_eq!(info.reviewed_by, Some(SUPER));
        assert!(store.queued_candidates(ADMIN).await.unwrap().is_empty());

        // 只能撤一次。
        let again = store
            .remove_queued_post(post, SUPER, None, Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        assert_eq!(again, RemoveOutcome::NotQueued);
    }

    #[tokio::test]
    async fn only_queued_posts_can_be_removed() {
        let (_directory, store) = Store::open_temporary().await;
        let draft = store
            .create_draft(&[incoming(1, "draft")], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .remove_queued_post(draft, SUPER, None, Timestamp::UNIX_EPOCH)
                .await
                .unwrap(),
            RemoveOutcome::NotQueued
        );
        assert_eq!(
            store
                .remove_queued_post(PostId(999), SUPER, None, Timestamp::UNIX_EPOCH)
                .await
                .unwrap(),
            RemoveOutcome::NotQueued
        );
    }
}
