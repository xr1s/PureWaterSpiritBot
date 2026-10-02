// 普通用户投稿的审核：提交、认领（通过）、拒绝、清空文字，以及审核群里的消息记录。
// 状态变化使用 UPDATE ... WHERE status = 'pending_review'，只有先处理的管理员成功。

use std::collections::HashSet;

use anyhow::{Context, Result};
use jiff::Timestamp;
use sea_orm::sea_query::{Expr, OnConflict, Query};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, Set,
};
use serde_json::json;
use teloxide::types::{ChatId, MessageId, UserId};

use super::{
    Conn, Store,
    codec::{decode_user_id, timestamp, user_id},
    entities::{categories, post_audit_log, post_messages, posts, review_messages, submitters},
    lock_admin, lock_post,
};
use crate::model::{
    Category, CategoryId, PostAuditKind, PostId, PostStatus, ReviewMessageKind, SubmitterStatus,
};

/// Telegram 对带媒体的消息，说明文字最多这么多个字符；纯文字消息最多 4096 个。
const CAPTION_LIMIT: usize = 1024;
const TEXT_LIMIT: usize = 4096;

/// 审核卡片和通知需要的一篇投稿的概况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewInfo {
    pub id: PostId,
    pub status: PostStatus,
    pub submitter: UserId,
    pub review_note: Option<String>,
    /// 处理（通过、拒绝或撤下）这篇投稿的管理员。
    pub reviewed_by: Option<UserId>,
    /// 投稿里是否有媒体，有才能「清空文字」。
    pub has_media: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApproveOutcome {
    Approved {
        category: Category,
        submitter: UserId,
    },
    /// 已经被别人处理，或者被投稿人撤回了。
    AlreadyHandled,
    /// 分类不存在、已归档，或不属于点击的管理员。
    CategoryGone,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClearOutcome {
    Cleared,
    /// 纯文字投稿清空文字就什么都不剩了。
    NeedsText,
    NotPending,
}

/// 审核群里的卡片由哪些消息组成。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewCard {
    pub control: Option<(ChatId, MessageId)>,
    /// 转发的投稿内容消息，按投稿里的顺序。
    pub contents: Vec<(ChatId, MessageId)>,
}

/// `/pending` 列出的一篇待审核投稿。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReview {
    pub post: PostId,
    pub submitter: UserId,
    pub control: Option<(ChatId, MessageId)>,
}
impl Store {
    /// 记录一个普通用户（已经记录过就什么也不改），返回他现在是否被拉黑。
    pub async fn register_submitter(
        &self,
        user: UserId,
        now: Timestamp,
    ) -> Result<SubmitterStatus> {
        self.transaction(async move |conn| {
            let id = user_id(user)?;
            submitters::Entity::insert(submitters::ActiveModel {
                user_id: Set(id),
                status: Set(SubmitterStatus::Active),
                created_at: Set(timestamp(now)?),
                ..Default::default()
            })
            .on_conflict(
                OnConflict::column(submitters::Column::UserId)
                    .update_column(submitters::Column::UserId)
                    .to_owned(),
            )
            .exec_without_returning(conn)
            .await?;
            Ok(submitters::Entity::find_by_id(id)
                .one(conn)
                .await?
                .context("registered submitter disappeared")?
                .status)
        })
        .await
    }

    /// 拉黑或解除拉黑一个普通用户。返回 `false` 表示没有这个用户。
    pub async fn set_submitter_blocked(
        &self,
        user: UserId,
        blocked: bool,
        admin: UserId,
        reason: Option<&str>,
        now: Timestamp,
    ) -> Result<bool> {
        self.transaction(async move |conn| {
            let id = user_id(user)?;
            let query = Query::update()
                .table(submitters::Entity)
                .value(
                    submitters::Column::UserId,
                    Expr::col(submitters::Column::UserId),
                )
                .and_where(submitters::Column::UserId.eq(id))
                .to_owned();
            conn.execute(&query).await?;
            if submitters::Entity::find_by_id(id)
                .one(conn)
                .await?
                .is_none()
            {
                return Ok(false);
            }
            let mut changes = submitters::ActiveModel::default();
            if blocked {
                changes.status = Set(SubmitterStatus::Blocked);
                changes.blocked_reason = Set(reason.map(str::to_owned));
                changes.blocked_at = Set(Some(timestamp(now)?));
                changes.blocked_by_admin_id = Set(Some(user_id(admin)?));
            } else {
                changes.status = Set(SubmitterStatus::Active);
                changes.blocked_reason = Set(None);
                changes.blocked_at = Set(None);
                changes.blocked_by_admin_id = Set(None);
            }
            submitters::Entity::update_many()
                .set(changes)
                .filter(submitters::Column::UserId.eq(id))
                .exec(conn)
                .await?;
            Ok(true)
        })
        .await
    }

    /// 投稿人提交审核。返回 `false` 表示这篇投稿已经不是他的草稿了。
    pub async fn submit_for_review(
        &self,
        post: PostId,
        submitter: UserId,
        now: Timestamp,
    ) -> Result<bool> {
        self.transaction(async move |conn| {
            lock_post(conn, post).await?;
            let changed = posts::Entity::update_many()
                .set(posts::ActiveModel {
                    status: Set(PostStatus::PendingReview),
                    submitted_at: Set(Some(timestamp(now)?)),
                    ..Default::default()
                })
                .filter(posts::Column::Id.eq(post.0))
                .filter(posts::Column::SubmitterUserId.eq(user_id(submitter)?))
                .filter(posts::Column::OwnerAdminId.is_null())
                .filter(posts::Column::Status.eq(PostStatus::Draft))
                .exec(conn)
                .await?;
            if changed.rows_affected == 0 {
                return Ok(false);
            }
            record_audit(conn, post, submitter, PostAuditKind::Submitted, None, now).await?;
            anyhow::Ok(true)
        })
        .await
    }

    /// 投稿人撤回自己的草稿或待审核的投稿。返回撤回前的状态，`None` 表示撤不了。
    pub async fn withdraw_submission(
        &self,
        post: PostId,
        submitter: UserId,
        now: Timestamp,
    ) -> Result<Option<PostStatus>> {
        self.transaction(async move |conn| {
            lock_post(conn, post).await?;
            let previous = posts::Entity::find_by_id(post.0)
                .filter(posts::Column::SubmitterUserId.eq(user_id(submitter)?))
                .filter(posts::Column::OwnerAdminId.is_null())
                .filter(posts::Column::Status.is_in([PostStatus::Draft, PostStatus::PendingReview]))
                .one(conn)
                .await?
                .map(|row| row.status);
            let Some(previous) = previous else {
                return Ok(None);
            };
            posts::Entity::update_many()
                .col_expr(posts::Column::Status, Expr::value(PostStatus::Cancelled))
                .filter(posts::Column::Id.eq(post.0))
                .exec(conn)
                .await?;
            if previous == PostStatus::PendingReview {
                record_audit(conn, post, submitter, PostAuditKind::Withdrawn, None, now).await?;
            }
            anyhow::Ok(Some(previous))
        })
        .await
    }

    /// 一篇投稿的审核概况。
    pub async fn review_info(&self, post: PostId) -> Result<Option<ReviewInfo>> {
        let Some(row) = posts::Entity::find_by_id(post.0).one(&self.db).await? else {
            return Ok(None);
        };
        let media = post_messages::Entity::find()
            .filter(post_messages::Column::PostId.eq(post.0))
            .filter(post_messages::Column::MediaKind.is_not_null())
            .count(&self.db)
            .await?;
        Ok(Some(ReviewInfo {
            id: post,
            status: row.status,
            submitter: decode_user_id(row.submitter_user_id)?,
            review_note: row.review_note,
            reviewed_by: row.reviewed_by_admin_id.map(decode_user_id).transpose()?,
            has_media: media > 0,
        }))
    }

    /// 待审核、但审核群里还没有卡片的投稿，按提交顺序。
    pub async fn posts_without_review_card(&self) -> Result<Vec<PostId>> {
        let pending: Vec<i64> = posts::Entity::find()
            .filter(posts::Column::Status.eq(PostStatus::PendingReview))
            .order_by_asc(posts::Column::SubmittedAt)
            .order_by_asc(posts::Column::Id)
            .select_only()
            .column(posts::Column::Id)
            .into_tuple()
            .all(&self.db)
            .await?;
        let carded: Vec<i64> = review_messages::Entity::find()
            .filter(review_messages::Column::PostId.is_in(pending.clone()))
            .select_only()
            .column(review_messages::Column::PostId)
            .distinct()
            .into_tuple()
            .all(&self.db)
            .await?;
        let carded: HashSet<i64> = carded.into_iter().collect();
        Ok(pending
            .into_iter()
            .filter(|id| !carded.contains(id))
            .map(PostId)
            .collect())
    }

    /// 记下 Bot 在审核群里为投稿发出的消息。
    pub async fn record_review_card(
        &self,
        post: PostId,
        contents: &[(ChatId, MessageId)],
        control: (ChatId, MessageId),
    ) -> Result<()> {
        self.transaction(async move |conn| {
            for (position, (chat, message)) in contents.iter().enumerate() {
                insert_review_message(
                    conn,
                    post,
                    *chat,
                    *message,
                    i32::try_from(position)?,
                    ReviewMessageKind::Content,
                )
                .await?;
            }
            insert_review_message(
                conn,
                post,
                control.0,
                control.1,
                0,
                ReviewMessageKind::Control,
            )
            .await?;
            anyhow::Ok(())
        })
        .await
    }

    /// 管理员回复审核群里的某条消息时，找到对应的投稿。
    pub async fn find_review_post(
        &self,
        chat: ChatId,
        message: MessageId,
    ) -> Result<Option<PostId>> {
        let post: Option<i64> = review_messages::Entity::find()
            .filter(review_messages::Column::ChatId.eq(chat.0))
            .filter(review_messages::Column::MessageId.eq(message.0))
            .select_only()
            .column(review_messages::Column::PostId)
            .into_tuple()
            .one(&self.db)
            .await?;
        Ok(post.map(PostId))
    }

    pub async fn review_card(&self, post: PostId) -> Result<ReviewCard> {
        let rows = review_messages::Entity::find()
            .filter(review_messages::Column::PostId.eq(post.0))
            .order_by_asc(review_messages::Column::Kind)
            .order_by_asc(review_messages::Column::Position)
            .all(&self.db)
            .await?;
        let mut card = ReviewCard::default();
        for row in rows {
            let location = (ChatId(row.chat_id), MessageId(row.message_id));
            match row.kind {
                ReviewMessageKind::Control => card.control = Some(location),
                ReviewMessageKind::Content => card.contents.push(location),
            }
        }
        Ok(card)
    }

    /// 管理员认领投稿：投稿进入他选的分类的队列。
    pub async fn approve_post(
        &self,
        post: PostId,
        admin: UserId,
        category: CategoryId,
        now: Timestamp,
    ) -> Result<ApproveOutcome> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            lock_post(conn, post).await?;
            let admin_id = user_id(admin)?;
            let row = categories::Entity::find_by_id(category.0)
                .filter(categories::Column::AdminUserId.eq(admin_id))
                .filter(categories::Column::ArchivedAt.is_null())
                .one(conn)
                .await?;
            let Some(category_row) = row else {
                return Ok(ApproveOutcome::CategoryGone);
            };
            let changed = posts::Entity::update_many()
                .set(posts::ActiveModel {
                    status: Set(PostStatus::Queued),
                    owner_admin_id: Set(Some(admin_id)),
                    category_id: Set(Some(category_row.id)),
                    queued_at: Set(Some(timestamp(now)?)),
                    reviewed_by_admin_id: Set(Some(admin_id)),
                    reviewed_at: Set(Some(timestamp(now)?)),
                    ..Default::default()
                })
                .filter(posts::Column::Id.eq(post.0))
                .filter(posts::Column::Status.eq(PostStatus::PendingReview))
                .exec(conn)
                .await?;
            if changed.rows_affected == 0 {
                return Ok(ApproveOutcome::AlreadyHandled);
            }
            let submitter = posts::Entity::find_by_id(post.0)
                .one(conn)
                .await?
                .context("approved post disappeared")?
                .submitter_user_id;
            record_audit(conn, post, admin, PostAuditKind::Approved, None, now).await?;
            anyhow::Ok(ApproveOutcome::Approved {
                category: Category {
                    id: CategoryId(category_row.id),
                    label: category_row.label,
                },
                submitter: decode_user_id(submitter)?,
            })
        })
        .await
    }

    /// 拒绝一篇待审核的投稿，`note` 是告诉投稿人的理由。
    /// 返回投稿人，`None` 表示投稿已经被处理过了。
    pub async fn reject_post(
        &self,
        post: PostId,
        admin: UserId,
        note: Option<&str>,
        now: Timestamp,
    ) -> Result<Option<UserId>> {
        self.transaction(async move |conn| {
            lock_post(conn, post).await?;
            let changed = posts::Entity::update_many()
                .set(posts::ActiveModel {
                    status: Set(PostStatus::Rejected),
                    reviewed_by_admin_id: Set(Some(user_id(admin)?)),
                    reviewed_at: Set(Some(timestamp(now)?)),
                    review_note: Set(note.map(str::to_owned)),
                    ..Default::default()
                })
                .filter(posts::Column::Id.eq(post.0))
                .filter(posts::Column::Status.eq(PostStatus::PendingReview))
                .exec(conn)
                .await?;
            if changed.rows_affected == 0 {
                return Ok(None);
            }
            let submitter = posts::Entity::find_by_id(post.0)
                .one(conn)
                .await?
                .context("rejected post disappeared")?
                .submitter_user_id;
            let detail = note.map(|note| json!({ "note": note }).to_string());
            record_audit(conn, post, admin, PostAuditKind::Rejected, detail, now).await?;
            anyhow::Ok(Some(decode_user_id(submitter)?))
        })
        .await
    }

    /// 清空待审核的媒体投稿的文字。
    pub async fn clear_review_text(
        &self,
        post: PostId,
        admin: UserId,
        now: Timestamp,
    ) -> Result<ClearOutcome> {
        self.transaction(async move |conn| {
            lock_post(conn, post).await?;
            if !is_pending(conn, post).await? {
                return Ok(ClearOutcome::NotPending);
            }
            let media = post_messages::Entity::find()
                .filter(post_messages::Column::PostId.eq(post.0))
                .filter(post_messages::Column::MediaKind.is_not_null())
                .count(conn)
                .await?;
            if media == 0 {
                return Ok(ClearOutcome::NeedsText);
            }
            let snapshot = snapshot_texts(conn, post).await?;
            post_messages::Entity::update_many()
                .set(post_messages::ActiveModel {
                    text: Set(None),
                    entities_json: Set("[]".to_owned()),
                    ..Default::default()
                })
                .filter(post_messages::Column::PostId.eq(post.0))
                .exec(conn)
                .await?;
            record_audit(
                conn,
                post,
                admin,
                PostAuditKind::TextCleared,
                Some(snapshot),
                now,
            )
            .await?;
            anyhow::Ok(ClearOutcome::Cleared)
        })
        .await
    }

    pub async fn pending_review_count(&self) -> Result<i64> {
        Ok(i64::try_from(
            posts::Entity::find()
                .filter(posts::Column::Status.eq(PostStatus::PendingReview))
                .count(&self.db)
                .await?,
        )?)
    }

    /// 待审核投稿的总数，以及最早的 `limit` 篇。
    pub async fn pending_reviews(&self, limit: i64) -> Result<(i64, Vec<PendingReview>)> {
        let total = self.pending_review_count().await?;
        let rows = posts::Entity::find()
            .filter(posts::Column::Status.eq(PostStatus::PendingReview))
            .order_by_asc(posts::Column::SubmittedAt)
            .order_by_asc(posts::Column::Id)
            .limit(u64::try_from(limit)?)
            .all(&self.db)
            .await?;
        let mut pending = Vec::with_capacity(rows.len());
        for row in rows {
            let control = review_messages::Entity::find()
                .filter(review_messages::Column::PostId.eq(row.id))
                .filter(review_messages::Column::Kind.eq(ReviewMessageKind::Control))
                .one(&self.db)
                .await?;
            pending.push(PendingReview {
                post: PostId(row.id),
                submitter: decode_user_id(row.submitter_user_id)?,
                control: control
                    .map(|message| (ChatId(message.chat_id), MessageId(message.message_id))),
            });
        }
        Ok((total, pending))
    }

    /// 载入一篇投稿的全部消息，用来在审核群里预览。
    pub async fn load_post(&self, post: PostId) -> Result<Option<crate::model::Post>> {
        if posts::Entity::find_by_id(post.0)
            .one(&self.db)
            .await?
            .is_none()
        {
            return Ok(None);
        }
        let mut loaded = super::posts::load_posts(&self.db, &[post.0]).await?;
        Ok(loaded.pop())
    }
}

pub(super) async fn record_audit(
    conn: &impl ConnectionTrait,
    post: PostId,
    actor: UserId,
    kind: PostAuditKind,
    detail: Option<String>,
    now: Timestamp,
) -> Result<()> {
    post_audit_log::Entity::insert(post_audit_log::ActiveModel {
        post_id: Set(post.0),
        actor_user_id: Set(user_id(actor)?),
        kind: Set(kind),
        detail_json: Set(detail),
        created_at: Set(timestamp(now)?),
        ..Default::default()
    })
    .exec_without_returning(conn)
    .await?;
    Ok(())
}

async fn insert_review_message(
    conn: &Conn,
    post: PostId,
    chat: ChatId,
    message: MessageId,
    position: i32,
    kind: ReviewMessageKind,
) -> Result<()> {
    review_messages::Entity::insert(review_messages::ActiveModel {
        post_id: Set(post.0),
        chat_id: Set(chat.0),
        message_id: Set(message.0),
        position: Set(position),
        kind: Set(kind),
        ..Default::default()
    })
    .exec_without_returning(conn)
    .await?;
    Ok(())
}

async fn is_pending(conn: &Conn, post: PostId) -> Result<bool> {
    let status = posts::Entity::find_by_id(post.0)
        .one(conn)
        .await?
        .map(|row| row.status);
    Ok(status == Some(PostStatus::PendingReview))
}

/// 这篇投稿的文字最多允许多长：第一条消息带媒体就是说明文字的上限，否则是正文的上限。
pub(super) async fn text_limit(conn: &impl ConnectionTrait, post: PostId) -> Result<usize> {
    let first = post_messages::Entity::find()
        .filter(post_messages::Column::PostId.eq(post.0))
        .order_by_asc(post_messages::Column::Position)
        .one(conn)
        .await?
        .context("post has no messages")?;
    Ok(if first.media_kind.is_some() {
        CAPTION_LIMIT
    } else {
        TEXT_LIMIT
    })
}

/// Telegram 按 UTF-16 码元数计算长度。
pub(super) fn utf16_length(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 投稿当前各条消息的文字，写进审计日志。
pub(super) async fn snapshot_texts(conn: &impl ConnectionTrait, post: PostId) -> Result<String> {
    let rows = post_messages::Entity::find()
        .filter(post_messages::Column::PostId.eq(post.0))
        .order_by_asc(post_messages::Column::Position)
        .all(conn)
        .await?;
    let messages: Vec<_> = rows
        .into_iter()
        .map(|row| {
            json!({
                "position": row.position,
                "text": row.text,
                "entities_json": row.entities_json,
                "link_preview_json": row.link_preview_json,
            })
        })
        .collect();
    Ok(json!({ "messages": messages }).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::MediaKind;
    use crate::store::Store;
    use crate::store::test_support::{
        ADMIN, category_id, incoming, incoming_media, insert_category,
    };

    const NOW: Timestamp = Timestamp::UNIX_EPOCH;
    const SUBMITTER: UserId = UserId(900);

    fn from_submitter(message_id: i32, text: &str) -> crate::model::IncomingMessage {
        let mut message = incoming(message_id, text);
        message.submitter = SUBMITTER;
        message.source_chat_id = ChatId(900);
        message
    }

    async fn pending_post(store: &Store, text: &str) -> PostId {
        let id = 1000 + i32::try_from(text.len()).unwrap();
        let post = store
            .create_submission_draft(&[from_submitter(id, text)], NOW)
            .await
            .unwrap()
            .unwrap();
        assert!(store.submit_for_review(post, SUBMITTER, NOW).await.unwrap());
        post
    }

    async fn status_of(store: &Store, post: PostId) -> PostStatus {
        store.review_info(post).await.unwrap().unwrap().status
    }

    async fn texts(store: &Store, post: PostId) -> Vec<Option<String>> {
        store
            .load_post(post)
            .await
            .unwrap()
            .unwrap()
            .messages
            .into_iter()
            .map(|content| content.text)
            .collect()
    }

    async fn events(store: &Store, post: PostId) -> Vec<(PostAuditKind, Option<String>)> {
        post_audit_log::Entity::find()
            .filter(post_audit_log::Column::PostId.eq(post.0))
            .order_by_asc(post_audit_log::Column::Id)
            .select_only()
            .column(post_audit_log::Column::Kind)
            .column(post_audit_log::Column::DetailJson)
            .into_tuple()
            .all(&store.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_submission_has_no_owner_until_it_is_approved() {
        let (_directory, store) = Store::open_temporary().await;
        let post = store
            .create_submission_draft(&[from_submitter(1, "hello")], NOW)
            .await
            .unwrap()
            .unwrap();
        let summary = store.post_summary(post).await.unwrap().unwrap();
        assert_eq!(summary.owner, None);
        assert_eq!(summary.status, PostStatus::Draft);

        // 没有归属的草稿不会被任何管理员的库存统计到。
        assert_eq!(store.stock_counts(ADMIN).await.unwrap().draft, 0);
    }

    #[tokio::test]
    async fn only_the_submitter_can_submit_and_only_once() {
        let (_directory, store) = Store::open_temporary().await;
        let post = store
            .create_submission_draft(&[from_submitter(1, "a")], NOW)
            .await
            .unwrap()
            .unwrap();
        assert!(!store.submit_for_review(post, UserId(1), NOW).await.unwrap());
        assert!(store.submit_for_review(post, SUBMITTER, NOW).await.unwrap());
        assert!(!store.submit_for_review(post, SUBMITTER, NOW).await.unwrap());
        assert_eq!(status_of(&store, post).await, PostStatus::PendingReview);
        assert_eq!(events(&store, post).await[0].0, PostAuditKind::Submitted);
    }

    #[tokio::test]
    async fn an_admins_own_draft_cannot_be_submitted_for_review() {
        let (_directory, store) = Store::open_temporary().await;
        let post = store
            .create_draft(&[incoming(1, "mine")], NOW, None)
            .await
            .unwrap()
            .unwrap();
        assert!(!store.submit_for_review(post, ADMIN, NOW).await.unwrap());
    }

    #[tokio::test]
    async fn the_submitter_can_withdraw_until_it_is_handled() {
        let (_directory, store) = Store::open_temporary().await;
        let draft = store
            .create_submission_draft(&[from_submitter(1, "draft")], NOW)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .withdraw_submission(draft, SUBMITTER, NOW)
                .await
                .unwrap(),
            Some(PostStatus::Draft)
        );
        assert_eq!(
            store
                .withdraw_submission(draft, SUBMITTER, NOW)
                .await
                .unwrap(),
            None
        );

        let pending = pending_post(&store, "pending").await;
        assert_eq!(
            store
                .withdraw_submission(pending, UserId(1), NOW)
                .await
                .unwrap(),
            None
        );
        assert_eq!(
            store
                .withdraw_submission(pending, SUBMITTER, NOW)
                .await
                .unwrap(),
            Some(PostStatus::PendingReview)
        );
        assert_eq!(status_of(&store, pending).await, PostStatus::Cancelled);
        let kinds: Vec<_> = events(&store, pending)
            .await
            .into_iter()
            .map(|e| e.0)
            .collect();
        assert_eq!(kinds, [PostAuditKind::Submitted, PostAuditKind::Withdrawn]);

        // 已经通过的投稿撤不回。
        let approved = pending_post(&store, "approved").await;
        let gi = category_id(&store, "gi").await;
        store.approve_post(approved, ADMIN, gi, NOW).await.unwrap();
        assert_eq!(
            store
                .withdraw_submission(approved, SUBMITTER, NOW)
                .await
                .unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn approving_queues_the_post_for_the_approving_admin() {
        let (_directory, store) = Store::open_temporary().await;
        let post = pending_post(&store, "nice").await;
        let gi = category_id(&store, "gi").await;

        let outcome = store.approve_post(post, ADMIN, gi, NOW).await.unwrap();
        let ApproveOutcome::Approved {
            category,
            submitter,
        } = outcome
        else {
            panic!("not approved: {outcome:?}");
        };
        assert_eq!(category.label, "gi");
        assert_eq!(submitter, SUBMITTER);

        let summary = store.post_summary(post).await.unwrap().unwrap();
        assert_eq!(summary.status, PostStatus::Queued);
        assert_eq!(summary.owner, Some(ADMIN));
        assert_eq!(store.queued_candidates(ADMIN).await.unwrap().len(), 1);
        assert_eq!(
            store
                .stock_counts(ADMIN)
                .await
                .unwrap()
                .queued_in(&category),
            1
        );
        assert_eq!(
            store.approve_post(post, ADMIN, gi, NOW).await.unwrap(),
            ApproveOutcome::AlreadyHandled
        );
        assert_eq!(
            events(&store, post).await.last().unwrap().0,
            PostAuditKind::Approved
        );
    }

    #[tokio::test]
    async fn approving_needs_a_category_of_the_approving_admin() {
        let (_directory, store) = Store::open_temporary().await;
        let other = UserId(7);
        store.sync_admins(&[ADMIN, other], NOW).await.unwrap();
        let foreign = insert_category(&store, other, "theirs").await;
        let post = pending_post(&store, "x").await;

        assert_eq!(
            store.approve_post(post, ADMIN, foreign, NOW).await.unwrap(),
            ApproveOutcome::CategoryGone
        );
        assert_eq!(status_of(&store, post).await, PostStatus::PendingReview);
        assert!(matches!(
            store.approve_post(post, other, foreign, NOW).await.unwrap(),
            ApproveOutcome::Approved { .. }
        ));
        assert_eq!(
            store.post_summary(post).await.unwrap().unwrap().owner,
            Some(other)
        );
    }

    #[tokio::test]
    async fn rejecting_keeps_the_note_and_the_post_out_of_every_queue() {
        let (_directory, store) = Store::open_temporary().await;
        let post = pending_post(&store, "meh").await;
        assert_eq!(
            store
                .reject_post(post, ADMIN, Some("不符合要求"), NOW)
                .await
                .unwrap(),
            Some(SUBMITTER)
        );
        let info = store.review_info(post).await.unwrap().unwrap();
        assert_eq!(info.status, PostStatus::Rejected);
        assert_eq!(info.review_note.as_deref(), Some("不符合要求"));
        assert!(store.queued_candidates(ADMIN).await.unwrap().is_empty());

        assert_eq!(
            store.reject_post(post, ADMIN, None, NOW).await.unwrap(),
            None
        );
        let gi = category_id(&store, "gi").await;
        assert_eq!(
            store.approve_post(post, ADMIN, gi, NOW).await.unwrap(),
            ApproveOutcome::AlreadyHandled
        );
    }

    #[tokio::test]
    async fn rejecting_without_a_note_stores_none() {
        let (_directory, store) = Store::open_temporary().await;
        let post = pending_post(&store, "meh").await;
        store.reject_post(post, ADMIN, None, NOW).await.unwrap();
        let info = store.review_info(post).await.unwrap().unwrap();
        assert_eq!(info.review_note, None);
    }

    #[tokio::test]
    async fn appending_text_preserves_and_remembers_the_original() {
        let (_directory, store) = Store::open_temporary().await;
        let post = pending_post(&store, "original").await;

        let outcome = store
            .append_review_text(post, ADMIN, &incoming(2, "better").content, NOW)
            .await
            .unwrap();
        assert!(matches!(
            outcome,
            crate::store::AppendTextOutcome::Appended { .. }
        ));
        assert_eq!(
            texts(&store, post).await,
            [Some("original\nbetter".to_owned())]
        );

        let events = events(&store, post).await;
        let (kind, detail) = events.last().unwrap();
        assert_eq!(*kind, PostAuditKind::TextReplaced);
        assert!(detail.as_deref().unwrap().contains("original"));
    }

    #[tokio::test]
    async fn appending_text_is_limited_to_what_telegram_accepts() {
        let (_directory, store) = Store::open_temporary().await;
        let text_post = pending_post(&store, "short").await;
        let long_text = "a".repeat(1500);
        assert!(matches!(
            store
                .append_review_text(text_post, ADMIN, &incoming(2, &long_text).content, NOW)
                .await
                .unwrap(),
            crate::store::AppendTextOutcome::Appended { .. }
        ));
        let too_long = "a".repeat(4097);
        assert_eq!(
            store
                .append_review_text(text_post, ADMIN, &incoming(3, &too_long).content, NOW)
                .await
                .unwrap(),
            crate::store::AppendTextOutcome::TooLong { limit: 4096 }
        );

        let mut photo = from_submitter(10, "");
        photo.content = incoming_media(10, MediaKind::Photo, Some("cap")).content;
        let media_post = store
            .create_submission_draft(&[photo], NOW)
            .await
            .unwrap()
            .unwrap();
        store
            .submit_for_review(media_post, SUBMITTER, NOW)
            .await
            .unwrap();
        assert_eq!(
            store
                .append_review_text(media_post, ADMIN, &incoming(4, &long_text).content, NOW)
                .await
                .unwrap(),
            crate::store::AppendTextOutcome::TooLong { limit: 1024 }
        );
        // 表情符号在 UTF-16 里占两个码元：513 个就超了 1024。
        let emoji = "😀".repeat(513);
        assert_eq!(
            store
                .append_review_text(media_post, ADMIN, &incoming(5, &emoji).content, NOW)
                .await
                .unwrap(),
            crate::store::AppendTextOutcome::TooLong { limit: 1024 }
        );
        assert_eq!(texts(&store, media_post).await, [Some("cap".to_owned())]);
    }

    #[tokio::test]
    async fn only_pending_posts_can_be_appended_to_or_cleared() {
        let (_directory, store) = Store::open_temporary().await;
        let post = pending_post(&store, "x").await;
        store.reject_post(post, ADMIN, None, NOW).await.unwrap();
        assert_eq!(
            store
                .append_review_text(post, ADMIN, &incoming(2, "y").content, NOW)
                .await
                .unwrap(),
            crate::store::AppendTextOutcome::Locked(PostStatus::Rejected)
        );
        assert_eq!(
            store.clear_review_text(post, ADMIN, NOW).await.unwrap(),
            ClearOutcome::NotPending
        );
    }

    #[tokio::test]
    async fn clearing_text_only_works_for_media_posts() {
        let (_directory, store) = Store::open_temporary().await;
        let text_post = pending_post(&store, "words").await;
        assert_eq!(
            store
                .clear_review_text(text_post, ADMIN, NOW)
                .await
                .unwrap(),
            ClearOutcome::NeedsText
        );
        assert_eq!(texts(&store, text_post).await, [Some("words".to_owned())]);

        let mut first = from_submitter(20, "");
        first.content = incoming_media(20, MediaKind::Photo, Some("caption")).content;
        let mut second = from_submitter(21, "");
        second.content = incoming_media(21, MediaKind::Photo, None).content;
        let album = store
            .create_submission_draft(&[first, second], NOW)
            .await
            .unwrap()
            .unwrap();
        store
            .submit_for_review(album, SUBMITTER, NOW)
            .await
            .unwrap();
        assert_eq!(
            store.clear_review_text(album, ADMIN, NOW).await.unwrap(),
            ClearOutcome::Cleared
        );
        assert_eq!(texts(&store, album).await, [None, None]);
        assert!(
            events(&store, album)
                .await
                .last()
                .unwrap()
                .1
                .as_deref()
                .unwrap()
                .contains("caption")
        );
        assert!(store.review_info(album).await.unwrap().unwrap().has_media);
        assert!(
            !store
                .review_info(text_post)
                .await
                .unwrap()
                .unwrap()
                .has_media
        );
    }

    #[tokio::test]
    async fn review_cards_are_found_by_any_of_their_messages() {
        let (_directory, store) = Store::open_temporary().await;
        let first = pending_post(&store, "a").await;
        let second = pending_post(&store, "bb").await;
        assert_eq!(
            store.posts_without_review_card().await.unwrap(),
            vec![first, second]
        );

        let chat = ChatId(-100);
        store
            .record_review_card(
                first,
                &[(chat, MessageId(10)), (chat, MessageId(11))],
                (chat, MessageId(12)),
            )
            .await
            .unwrap();
        assert_eq!(
            store.posts_without_review_card().await.unwrap(),
            vec![second]
        );
        for message in [10, 11, 12] {
            assert_eq!(
                store
                    .find_review_post(chat, MessageId(message))
                    .await
                    .unwrap(),
                Some(first)
            );
        }
        assert_eq!(
            store.find_review_post(chat, MessageId(99)).await.unwrap(),
            None
        );

        let card = store.review_card(first).await.unwrap();
        assert_eq!(card.control, Some((chat, MessageId(12))));
        assert_eq!(
            card.contents,
            [(chat, MessageId(10)), (chat, MessageId(11))]
        );
    }

    #[tokio::test]
    async fn handled_posts_no_longer_need_a_card() {
        let (_directory, store) = Store::open_temporary().await;
        let post = pending_post(&store, "a").await;
        store
            .withdraw_submission(post, SUBMITTER, NOW)
            .await
            .unwrap();
        assert!(store.posts_without_review_card().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn pending_reviews_are_listed_oldest_first_with_a_total() {
        let (_directory, store) = Store::open_temporary().await;
        let first = pending_post(&store, "a").await;
        let second = pending_post(&store, "bb").await;
        let third = pending_post(&store, "ccc").await;
        store.reject_post(second, ADMIN, None, NOW).await.unwrap();
        store
            .record_review_card(
                first,
                &[(ChatId(-100), MessageId(1))],
                (ChatId(-100), MessageId(2)),
            )
            .await
            .unwrap();

        let (total, listed) = store.pending_reviews(10).await.unwrap();
        assert_eq!(total, 2);
        assert_eq!(store.pending_review_count().await.unwrap(), 2);
        assert_eq!(
            listed.iter().map(|item| item.post).collect::<Vec<_>>(),
            [first, third]
        );
        assert_eq!(listed[0].submitter, SUBMITTER);
        assert_eq!(listed[0].control, Some((ChatId(-100), MessageId(2))));
        assert_eq!(listed[1].control, None);
        assert_eq!(store.pending_reviews(1).await.unwrap().1.len(), 1);
    }

    #[tokio::test]
    async fn submitters_can_be_blocked_and_unblocked() {
        let (_directory, store) = Store::open_temporary().await;
        assert_eq!(
            store.register_submitter(SUBMITTER, NOW).await.unwrap(),
            SubmitterStatus::Active
        );
        assert!(
            store
                .set_submitter_blocked(SUBMITTER, true, ADMIN, Some("spam"), NOW)
                .await
                .unwrap()
        );
        // 再次记录不会解除拉黑。
        assert_eq!(
            store.register_submitter(SUBMITTER, NOW).await.unwrap(),
            SubmitterStatus::Blocked
        );

        assert!(
            store
                .set_submitter_blocked(SUBMITTER, false, ADMIN, None, NOW)
                .await
                .unwrap()
        );
        assert_eq!(
            store.register_submitter(SUBMITTER, NOW).await.unwrap(),
            SubmitterStatus::Active
        );
        assert!(
            !store
                .set_submitter_blocked(UserId(12345), true, ADMIN, None, NOW)
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn a_submitters_post_keeps_the_review_note_for_the_notification() {
        let (_directory, store) = Store::open_temporary().await;
        let post = pending_post(&store, "x").await;
        let info = store.review_info(post).await.unwrap().unwrap();
        assert_eq!(info.submitter, SUBMITTER);
        assert_eq!(store.review_info(PostId(9999)).await.unwrap(), None);
        assert_eq!(
            store.load_post(PostId(9999)).await.unwrap().map(|p| p.id),
            None
        );
    }
}
