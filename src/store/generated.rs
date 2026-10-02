// 把人工回复和 AI 生成的标签追加到帖子文字。

use anyhow::Result;
use jiff::Timestamp;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, QueryOrder, Set};
use teloxide::types::UserId;

use super::{
    Store,
    entities::{post_messages, posts},
    lock_post,
    review::{record_audit, snapshot_texts, text_limit, utf16_length},
};
use crate::model::{Content, GeneratedText, PostAuditKind, PostId, PostStatus};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppendTextOutcome {
    /// 已经追加。`added` 是追加的那段文字。
    Appended {
        added: String,
    },
    /// 没有可追加的文字，或生成的内容都已经在文字里了。
    NothingNew,
    /// 追加之后文字太长，Telegram 发不出去。`limit` 是允许的最大长度。
    TooLong {
        limit: usize,
    },
    NotFound,
    /// 帖子已经不能修改文字了。
    Locked(PostStatus),
}

impl Store {
    /// 换行追加用户文字，保留原文及双方的 Telegram 实体。
    pub async fn append_text(
        &self,
        post: PostId,
        actor: UserId,
        addition: &Content,
        now: Timestamp,
    ) -> Result<AppendTextOutcome> {
        self.append_input_text(post, actor, addition, now, false)
            .await
    }

    /// 只允许追加到待审核投稿，并记录修改前的文字。
    pub async fn append_review_text(
        &self,
        post: PostId,
        actor: UserId,
        addition: &Content,
        now: Timestamp,
    ) -> Result<AppendTextOutcome> {
        self.append_input_text(post, actor, addition, now, true)
            .await
    }

    async fn append_input_text(
        &self,
        post: PostId,
        actor: UserId,
        addition: &Content,
        now: Timestamp,
        review: bool,
    ) -> Result<AppendTextOutcome> {
        self.transaction(async move |conn| {
            lock_post(conn, post).await?;
            let Some(post_row) = posts::Entity::find_by_id(post.0).one(conn).await? else {
                return Ok(AppendTextOutcome::NotFound);
            };
            let allowed = if review {
                post_row.status == PostStatus::PendingReview
            } else {
                post_row.status.is_editable()
            };
            if !allowed {
                return Ok(AppendTextOutcome::Locked(post_row.status));
            }
            let Some(addition_text) = addition.text.as_deref().filter(|text| !text.is_empty())
            else {
                return Ok(AppendTextOutcome::NothingNew);
            };
            let Some(row) = post_messages::Entity::find()
                .filter(post_messages::Column::PostId.eq(post.0))
                .order_by_asc(post_messages::Column::Position)
                .one(conn)
                .await?
            else {
                return Ok(AppendTextOutcome::NotFound);
            };
            let existing = row.text.as_deref().unwrap_or_default();
            let separator = if existing.is_empty() { "" } else { "\n" };
            let added = format!("{separator}{addition_text}");
            let text = format!("{existing}{added}");
            let limit = text_limit(conn, post).await?;
            if utf16_length(&text) > limit {
                return Ok(AppendTextOutcome::TooLong { limit });
            }
            let offset = utf16_length(existing) + separator.len();
            let mut entities: Vec<teloxide::types::MessageEntity> =
                serde_json::from_str(&row.entities_json)?;
            entities.extend(addition.entities.iter().cloned().map(|mut entity| {
                entity.offset += offset;
                entity
            }));
            let snapshot = if review {
                Some(snapshot_texts(conn, post).await?)
            } else {
                None
            };
            post_messages::Entity::update_many()
                .set(post_messages::ActiveModel {
                    text: Set(Some(text)),
                    entities_json: Set(serde_json::to_string(&entities)?),
                    ..Default::default()
                })
                .filter(post_messages::Column::Id.eq(row.id))
                .exec(conn)
                .await?;
            if let Some(snapshot) = snapshot {
                record_audit(
                    conn,
                    post,
                    actor,
                    PostAuditKind::TextReplaced,
                    Some(snapshot),
                    now,
                )
                .await?;
            }
            Ok(AppendTextOutcome::Appended { added })
        })
        .await
    }

    /// 把 `addition` 写入帖子文字。草稿、排队中和待审核的帖子可以修改；
    /// 待审核的帖子会在审计日志里记下修改前的内容，和覆盖原文一样。
    ///
    /// 读取和写入在同一个事务里，AI 识别期间管理员改过文字也不会被盖掉。
    pub async fn append_generated_text(
        &self,
        post: PostId,
        actor: UserId,
        addition: &GeneratedText,
        now: Timestamp,
    ) -> Result<AppendTextOutcome> {
        let addition = addition.clone();
        self.transaction(async move |conn| {
            lock_post(conn, post).await?;
            let Some(row) = posts::Entity::find_by_id(post.0).one(conn).await? else {
                return Ok(AppendTextOutcome::NotFound);
            };
            let status = row.status;
            let pending = status == PostStatus::PendingReview;
            if !status.is_editable() && !pending {
                return Ok(AppendTextOutcome::Locked(status));
            }

            let first = post_messages::Entity::find()
                .filter(post_messages::Column::PostId.eq(post.0))
                .order_by_asc(post_messages::Column::Position)
                .one(conn)
                .await?;
            let Some(row) = first else {
                return Ok(AppendTextOutcome::NotFound);
            };
            let existing = row.text.clone().unwrap_or_default();
            let entities: Vec<teloxide::types::MessageEntity> =
                serde_json::from_str(&row.entities_json)?;
            let Some(change) = addition.change(&existing, &entities) else {
                return Ok(AppendTextOutcome::NothingNew);
            };
            let text = change.text;
            let limit = text_limit(conn, post).await?;
            if utf16_length(&text) > limit {
                return Ok(AppendTextOutcome::TooLong { limit });
            }

            let entities = serde_json::to_string(&change.entities)?;

            let snapshot = if pending {
                Some(snapshot_texts(conn, post).await?)
            } else {
                None
            };
            post_messages::Entity::update_many()
                .set(post_messages::ActiveModel {
                    text: Set(Some(text)),
                    entities_json: Set(entities),
                    ..Default::default()
                })
                .filter(post_messages::Column::Id.eq(row.id))
                .exec(conn)
                .await?;
            if let Some(snapshot) = snapshot {
                record_audit(
                    conn,
                    post,
                    actor,
                    PostAuditKind::TextReplaced,
                    Some(snapshot),
                    now,
                )
                .await?;
            }
            anyhow::Ok(AppendTextOutcome::Appended {
                added: change.added,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use teloxide::types::{ChatId, MessageEntity};

    use super::*;
    use crate::store::Store;
    use crate::{
        model::MediaKind,
        store::{
            entities::post_audit_log,
            test_support::{ADMIN, incoming, incoming_media, queue},
        },
    };

    const NOW: Timestamp = Timestamp::UNIX_EPOCH;

    fn tags(names: &[&str]) -> GeneratedText {
        GeneratedText::Tags(names.iter().map(|name| (*name).to_owned()).collect())
    }

    #[tokio::test]
    async fn manual_append_preserves_links_and_shifts_entities_in_utf16() {
        let (_directory, store) = Store::open_temporary().await;
        let mut message = incoming_media(10, MediaKind::Photo, Some("😀source"));
        let link = MessageEntity::text_link("https://example.com".parse().unwrap(), 2, 6);
        message.content.entities.push(link.clone());
        let post = store
            .create_draft(&[message], NOW, None)
            .await
            .unwrap()
            .unwrap();
        let mut addition = incoming(11, "#tag bold").content;
        addition.entities.push(MessageEntity::bold(5, 4));
        assert!(matches!(
            store
                .append_text(post, ADMIN, &addition, NOW)
                .await
                .unwrap(),
            AppendTextOutcome::Appended { .. }
        ));
        let content = &store.load_post(post).await.unwrap().unwrap().messages[0];
        assert_eq!(content.text.as_deref(), Some("😀source\n#tag bold"));
        assert_eq!(content.entities, vec![link, MessageEntity::bold(14, 4)]);
    }

    #[tokio::test]
    async fn manual_append_checks_the_combined_length_without_changing_text() {
        let (_directory, store) = Store::open_temporary().await;
        let caption = "x".repeat(1020);
        let post = caption_draft(&store, Some(&caption)).await;
        let addition = incoming(11, "😀😀").content;
        assert_eq!(
            store
                .append_text(post, ADMIN, &addition, NOW)
                .await
                .unwrap(),
            AppendTextOutcome::TooLong { limit: 1024 }
        );
        assert_eq!(
            text_of(&store, post).await.as_deref(),
            Some(caption.as_str())
        );
    }

    #[tokio::test]
    async fn manual_append_handles_empty_captions_and_plain_text_limits() {
        let (_directory, store) = Store::open_temporary().await;
        let post = caption_draft(&store, None).await;
        let addition = incoming(11, "#tag").content;
        store
            .append_text(post, ADMIN, &addition, NOW)
            .await
            .unwrap();
        assert_eq!(text_of(&store, post).await.as_deref(), Some("#tag"));
        let original = "x".repeat(4092);
        let post = store
            .create_draft(&[incoming(12, &original)], NOW, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .append_text(post, ADMIN, &addition, NOW)
                .await
                .unwrap(),
            AppendTextOutcome::TooLong { limit: 4096 }
        );
        assert_eq!(
            text_of(&store, post).await.as_deref(),
            Some(original.as_str())
        );
        assert_eq!(
            store
                .append_text(PostId(999), ADMIN, &addition, NOW)
                .await
                .unwrap(),
            AppendTextOutcome::NotFound
        );
    }

    #[tokio::test]
    async fn manual_append_audits_reviews_and_keeps_entry_points_separate() {
        let (_directory, store) = Store::open_temporary().await;
        let submitter = UserId(900);
        let mut message = incoming(20, "original");
        message.submitter = submitter;
        message.source_chat_id = ChatId(900);
        let post = store
            .create_submission_draft(&[message], NOW)
            .await
            .unwrap()
            .unwrap();
        let addition = incoming(21, "#tag").content;
        assert_eq!(
            store
                .append_review_text(post, ADMIN, &addition, NOW)
                .await
                .unwrap(),
            AppendTextOutcome::Locked(PostStatus::Draft)
        );
        assert!(store.submit_for_review(post, submitter, NOW).await.unwrap());
        let before = audit_count(&store, post).await;
        assert_eq!(
            store
                .append_text(post, submitter, &addition, NOW)
                .await
                .unwrap(),
            AppendTextOutcome::Locked(PostStatus::PendingReview)
        );
        assert!(matches!(
            store
                .append_review_text(post, ADMIN, &addition, NOW)
                .await
                .unwrap(),
            AppendTextOutcome::Appended { .. }
        ));
        assert_eq!(
            text_of(&store, post).await.as_deref(),
            Some("original\n#tag")
        );
        assert_eq!(audit_count(&store, post).await, before + 1);
    }

    async fn caption_draft(store: &Store, caption: Option<&str>) -> PostId {
        store
            .create_draft(&[incoming_media(10, MediaKind::Photo, caption)], NOW, None)
            .await
            .unwrap()
            .unwrap()
    }

    async fn text_of(store: &Store, post: PostId) -> Option<String> {
        store.load_post(post).await.unwrap().unwrap().messages[0]
            .text
            .clone()
    }

    async fn audit_count(store: &Store, post: PostId) -> i64 {
        use sea_orm::PaginatorTrait;
        i64::try_from(
            post_audit_log::Entity::find()
                .filter(post_audit_log::Column::PostId.eq(post.0))
                .count(&store.db)
                .await
                .unwrap(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn appends_to_a_draft_and_to_a_queued_post_without_an_audit_entry() {
        let (_directory, store) = Store::open_temporary().await;
        let post = caption_draft(&store, Some("cap")).await;
        let outcome = store
            .append_generated_text(post, ADMIN, &tags(&["#a", "#b"]), NOW)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            AppendTextOutcome::Appended {
                added: "\n#a #b".to_owned()
            }
        );
        assert_eq!(text_of(&store, post).await.as_deref(), Some("cap\n#a #b"));

        queue(&store, post, "gi", NOW).await;
        let outcome = store
            .append_generated_text(post, ADMIN, &tags(&["#c"]), NOW)
            .await
            .unwrap();
        assert!(matches!(outcome, AppendTextOutcome::Appended { .. }));
        assert_eq!(
            text_of(&store, post).await.as_deref(),
            Some("cap\n#a #b #c")
        );
        assert_eq!(audit_count(&store, post).await, 0);
    }

    #[tokio::test]
    async fn a_post_without_text_gets_just_the_generated_text() {
        let (_directory, store) = Store::open_temporary().await;
        let post = caption_draft(&store, None).await;
        store
            .append_generated_text(post, ADMIN, &tags(&["#a", "#b"]), NOW)
            .await
            .unwrap();
        assert_eq!(text_of(&store, post).await.as_deref(), Some("#a #b"));
    }

    #[tokio::test]
    async fn existing_entities_are_kept() {
        let (_directory, store) = Store::open_temporary().await;
        let mut message = incoming_media(10, MediaKind::Photo, Some("source"));
        message.content.entities = vec![MessageEntity::bold(0, 6)];
        let post = store
            .create_draft(&[message], NOW, None)
            .await
            .unwrap()
            .unwrap();

        store
            .append_generated_text(post, ADMIN, &tags(&["#a"]), NOW)
            .await
            .unwrap();

        let content = &store.load_post(post).await.unwrap().unwrap().messages[0];
        assert_eq!(content.text.as_deref(), Some("source\n#a"));
        assert_eq!(content.entities, [MessageEntity::bold(0, 6)]);
    }

    #[tokio::test]
    async fn repeating_the_same_content_changes_nothing() {
        let (_directory, store) = Store::open_temporary().await;
        let post = caption_draft(&store, Some("cap")).await;
        let addition = tags(&["#a"]);
        store
            .append_generated_text(post, ADMIN, &addition, NOW)
            .await
            .unwrap();
        let again = store
            .append_generated_text(post, ADMIN, &addition, NOW)
            .await
            .unwrap();
        assert_eq!(again, AppendTextOutcome::NothingNew);
        assert_eq!(text_of(&store, post).await.as_deref(), Some("cap\n#a"));
    }

    #[tokio::test]
    async fn refuses_text_beyond_the_caption_limit() {
        let (_directory, store) = Store::open_temporary().await;
        let caption = "字".repeat(1020);
        let post = caption_draft(&store, Some(&caption)).await;
        let outcome = store
            .append_generated_text(post, ADMIN, &tags(&["#很长的标签"]), NOW)
            .await
            .unwrap();
        assert_eq!(outcome, AppendTextOutcome::TooLong { limit: 1024 });
        assert_eq!(
            text_of(&store, post).await.as_deref(),
            Some(caption.as_str())
        );
    }

    #[tokio::test]
    async fn a_pending_submission_is_appended_to_and_audited() {
        let (_directory, store) = Store::open_temporary().await;
        let submitter = UserId(900);
        let mut message = incoming(20, "from a user");
        message.submitter = submitter;
        message.source_chat_id = ChatId(900);
        let post = store
            .create_submission_draft(&[message], NOW)
            .await
            .unwrap()
            .unwrap();
        assert!(store.submit_for_review(post, submitter, NOW).await.unwrap());
        let before = audit_count(&store, post).await;

        let outcome = store
            .append_generated_text(post, ADMIN, &tags(&["#b"]), NOW)
            .await
            .unwrap();
        assert!(matches!(outcome, AppendTextOutcome::Appended { .. }));
        assert_eq!(
            text_of(&store, post).await.as_deref(),
            Some("from a user\n#b")
        );
        assert_eq!(audit_count(&store, post).await, before + 1);
    }

    #[tokio::test]
    async fn unknown_and_finished_posts_are_refused() {
        let (_directory, store) = Store::open_temporary().await;
        assert_eq!(
            store
                .append_generated_text(PostId(999), ADMIN, &tags(&["#a"]), NOW)
                .await
                .unwrap(),
            AppendTextOutcome::NotFound
        );
        let post = caption_draft(&store, Some("cap")).await;
        assert!(store.cancel_post(post, ADMIN).await.unwrap());
        assert!(matches!(
            store
                .append_generated_text(post, ADMIN, &tags(&["#a"]), NOW)
                .await
                .unwrap(),
            AppendTextOutcome::Locked(PostStatus::Cancelled)
        ));
    }
}
