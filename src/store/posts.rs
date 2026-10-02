use std::collections::HashMap;

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DbErr, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, QueryTrait, Set, SqlErr,
    sea_query::{Expr, ExprTrait, JoinType},
};
use teloxide::types::{ChatId, MessageId, UserId};

use super::{
    Store,
    codec::{decode_user_id, timestamp, user_id},
    entities::{categories, post_messages, posts, publication_attempts, published_messages},
    lock_admin, lock_post,
};
use crate::model::{
    AttemptStatus, Candidate, Category, CategoryId, Content, IncomingMessage, MAX_ALBUM_ITEMS,
    Media, MediaKind, Post, PostId, PostStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostSummary {
    pub id: PostId,
    pub status: PostStatus,
    pub owner: Option<UserId>,
    pub category: Option<Category>,
    pub message_count: usize,
}

/// 把草稿移入队列的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueOutcome {
    Queued(Category),
    /// 该帖子已不再是此管理员的草稿。
    AlreadyHandled,
    /// 分类不存在、已归档，或属于其他管理员。
    CategoryGone,
}

/// 把已入队、还没发布的帖子改到另一个分类的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved(Category),
    /// 该帖子已不再是此管理员排队中的帖子，例如已被取消、撤下或开始发布。
    NotQueued,
    /// 分类不存在、已归档，或属于其他管理员。
    CategoryGone,
}

/// 更改已存储帖子内容的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    Done(PostId),
    NotFound,
    /// 该帖子已过可更改内容的阶段。
    Locked(PostId, PostStatus),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendRejection {
    /// 纯文本帖子没有可添加媒体的相册。
    TextOnly,
    TooManyItems,
    /// 媒体混合后的结果无法作为一个相册发送。
    NotAlbum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended {
        total: usize,
    },
    NotFound,
    Locked(PostStatus),
    Rejected(AppendRejection),
    /// Telegram 重新投递了已存储的消息。
    Duplicate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceMediaRejection {
    /// 纯文本帖子没有可替换的媒体。
    TextOnly,
    /// 要替换的那一项已经不存在，例如帖子的媒体刚被整组替换过。
    ItemGone,
    TooManyItems,
    /// 替换后的媒体无法作为一个相册发送。
    NotAlbum,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceMediaOutcome {
    Replaced {
        total: usize,
        /// 新媒体带有说明文字，它已成为帖子的文字。
        text_replaced: bool,
    },
    NotFound,
    Locked(PostStatus),
    Rejected(ReplaceMediaRejection),
    /// Telegram 重新投递了已存储的消息。
    Duplicate,
}

#[derive(Debug, Default)]
pub struct StockCounts {
    pub(crate) queued: HashMap<CategoryId, i64>,
    pub draft: i64,
    pub reserved: i64,
    pub failed: i64,
}

impl StockCounts {
    pub fn queued_in(&self, category: &Category) -> i64 {
        self.queued.get(&category.id).copied().unwrap_or(0)
    }
}
impl post_messages::Model {
    fn into_content(self) -> Result<Content> {
        let media = match (
            self.media_kind,
            self.media_file_id,
            self.media_file_unique_id,
        ) {
            (Some(kind), Some(file_id), Some(file_unique_id)) => Some(Media {
                kind,
                file_id,
                file_unique_id,
                has_spoiler: self.media_has_spoiler,
            }),
            (None, None, None) => None,
            _ => bail!("stored message has incomplete media columns"),
        };
        Ok(Content {
            text: self.text,
            entities: serde_json::from_str(&self.entities_json)?,
            link_preview_options: self
                .link_preview_json
                .map(|json| serde_json::from_str(&json))
                .transpose()?,
            show_caption_above_media: self.show_caption_above_media,
            media,
        })
    }
}

fn message_changes(content: &Content) -> Result<post_messages::ActiveModel> {
    let media = content.media.as_ref();
    Ok(post_messages::ActiveModel {
        text: Set(content.text.clone()),
        entities_json: Set(serde_json::to_string(&content.entities)?),
        link_preview_json: Set(content
            .link_preview_options
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?),
        show_caption_above_media: Set(content.show_caption_above_media),
        media_kind: Set(media.map(|media| media.kind)),
        media_file_id: Set(media.map(|media| media.file_id.clone())),
        media_file_unique_id: Set(media.map(|media| media.file_unique_id.clone())),
        media_has_spoiler: Set(media.is_some_and(|media| media.has_spoiler)),
        ..Default::default()
    })
}

fn new_message(
    post_id: i64,
    position: usize,
    message: &IncomingMessage,
) -> Result<post_messages::ActiveModel> {
    Ok(post_messages::ActiveModel {
        post_id: Set(post_id),
        position: Set(i32::try_from(position)?),
        source_chat_id: Set(message.source_chat_id.0),
        source_message_id: Set(message.source_message_id.0),
        ..message_changes(&message.content)?
    })
}

impl Store {
    /// 把这些消息存为一个草稿，可选择回复某条频道消息。如果其中任何一条
    /// 之前已存储过则返回 `None`，这发生在 Telegram 重新投递
    /// 更新时。
    pub async fn create_draft(
        &self,
        messages: &[IncomingMessage],
        now: Timestamp,
        reply_to: Option<MessageId>,
    ) -> Result<Option<PostId>> {
        let owner = messages.first().map(|message| message.submitter);
        self.create_draft_owned(messages, owner, now, reply_to)
            .await
    }

    /// 普通用户（非管理员）的投稿草稿：不属于任何管理员，审核通过后才会被某个管理员认领。
    pub async fn create_submission_draft(
        &self,
        messages: &[IncomingMessage],
        now: Timestamp,
    ) -> Result<Option<PostId>> {
        self.create_draft_owned(messages, None, now, None).await
    }

    async fn create_draft_owned(
        &self,
        messages: &[IncomingMessage],
        owner: Option<UserId>,
        now: Timestamp,
        reply_to: Option<MessageId>,
    ) -> Result<Option<PostId>> {
        let first = messages
            .first()
            .context("a draft needs at least one message")?;
        let result = self
            .transaction(async move |conn| {
                let post_id = posts::Entity::insert(posts::ActiveModel {
                    submitter_user_id: Set(user_id(first.submitter)?),
                    owner_admin_id: Set(owner.map(user_id).transpose()?),
                    status: Set(PostStatus::Draft),
                    reply_to_message_id: Set(reply_to.map(|message| message.0)),
                    created_at: Set(timestamp(now)?),
                    control_chat_id: Set(Some(first.source_chat_id.0)),
                    ..Default::default()
                })
                .exec(conn)
                .await?
                .last_insert_id;
                let rows = messages
                    .iter()
                    .enumerate()
                    .map(|(position, message)| new_message(post_id, position, message))
                    .collect::<Result<Vec<_>>>()?;
                post_messages::Entity::insert_many(rows)
                    .exec_without_returning(conn)
                    .await?;
                anyhow::Ok(Some(PostId(post_id)))
            })
            .await;
        match result {
            Err(error)
                if error.downcast_ref::<DbErr>().is_some_and(|error| {
                    matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_)))
                }) =>
            {
                Ok(None)
            }
            result => result,
        }
    }

    pub async fn set_control_message(
        &self,
        post_id: PostId,
        chat_id: ChatId,
        message_id: MessageId,
    ) -> Result<()> {
        posts::Entity::update_many()
            .set(posts::ActiveModel {
                control_chat_id: Set(Some(chat_id.0)),
                control_message_id: Set(Some(message_id.0)),
                ..Default::default()
            })
            .filter(posts::Column::Id.eq(post_id.0))
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// 把 `admin` 的草稿移入其某个分类。
    pub async fn queue_post(
        &self,
        post_id: PostId,
        admin: UserId,
        category_id: CategoryId,
        now: Timestamp,
    ) -> Result<QueueOutcome> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            lock_post(conn, post_id).await?;
            let admin = user_id(admin)?;
            let row = categories::Entity::find_by_id(category_id.0)
                .filter(categories::Column::AdminUserId.eq(admin))
                .filter(categories::Column::ArchivedAt.is_null())
                .one(conn)
                .await?;
            let Some(category) = row else {
                return Ok(QueueOutcome::CategoryGone);
            };
            let changed = posts::Entity::update_many()
                .set(posts::ActiveModel {
                    category_id: Set(Some(category.id)),
                    status: Set(PostStatus::Queued),
                    queued_at: Set(Some(timestamp(now)?)),
                    ..Default::default()
                })
                .filter(posts::Column::Id.eq(post_id.0))
                .filter(posts::Column::OwnerAdminId.eq(admin))
                .filter(posts::Column::Status.eq(PostStatus::Draft))
                .exec(conn)
                .await?;
            if changed.rows_affected == 0 {
                return Ok(QueueOutcome::AlreadyHandled);
            }
            anyhow::Ok(QueueOutcome::Queued(Category {
                id: CategoryId(category.id),
                label: category.label,
            }))
        })
        .await
    }

    /// 把 `admin` 排队中的帖子改到其另一个分类。入队时间不变，
    /// 所以它在新分类的队列里仍按原来的入队时间排序。
    pub async fn move_queued_post(
        &self,
        post_id: PostId,
        admin: UserId,
        category_id: CategoryId,
    ) -> Result<MoveOutcome> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            lock_post(conn, post_id).await?;
            let admin = user_id(admin)?;
            let row = categories::Entity::find_by_id(category_id.0)
                .filter(categories::Column::AdminUserId.eq(admin))
                .filter(categories::Column::ArchivedAt.is_null())
                .one(conn)
                .await?;
            let Some(category) = row else {
                return Ok(MoveOutcome::CategoryGone);
            };
            let changed = posts::Entity::update_many()
                .set(posts::ActiveModel {
                    category_id: Set(Some(category.id)),
                    ..Default::default()
                })
                .filter(posts::Column::Id.eq(post_id.0))
                .filter(posts::Column::OwnerAdminId.eq(admin))
                .filter(posts::Column::Status.eq(PostStatus::Queued))
                .exec(conn)
                .await?;
            if changed.rows_affected == 0 {
                // 某些驱动报告的是实际变更行数，而不是无操作更新匹配到的行数。
                let unchanged = posts::Entity::find_by_id(post_id.0)
                    .filter(posts::Column::OwnerAdminId.eq(admin))
                    .filter(posts::Column::Status.eq(PostStatus::Queued))
                    .filter(posts::Column::CategoryId.eq(category.id))
                    .select_only()
                    .column(posts::Column::Id)
                    .into_tuple::<i64>()
                    .one(conn)
                    .await?;
                if unchanged.is_none() {
                    return Ok(MoveOutcome::NotQueued);
                }
            }
            anyhow::Ok(MoveOutcome::Moved(Category {
                id: CategoryId(category.id),
                label: category.label,
            }))
        })
        .await
    }

    /// 取消 `owner` 尚未发布的帖子。如果帖子已发布、正在发布
    /// 或之前已被取消，则返回 `false`。
    pub async fn cancel_post(&self, post_id: PostId, owner: UserId) -> Result<bool> {
        let changed = posts::Entity::update_many()
            .set(posts::ActiveModel {
                status: Set(PostStatus::Cancelled),
                ..Default::default()
            })
            .filter(posts::Column::Id.eq(post_id.0))
            .filter(posts::Column::OwnerAdminId.eq(user_id(owner)?))
            .filter(posts::Column::Status.is_in([PostStatus::Draft, PostStatus::Queued]))
            .exec(&self.db)
            .await?;
        Ok(changed.rows_affected == 1)
    }

    pub async fn post_summary(&self, post_id: PostId) -> Result<Option<PostSummary>> {
        let Some(row) = posts::Entity::find_by_id(post_id.0).one(&self.db).await? else {
            return Ok(None);
        };
        let category = match row.category_id {
            Some(id) => categories::Entity::find_by_id(id)
                .one(&self.db)
                .await?
                .map(|row| Category {
                    id: CategoryId(row.id),
                    label: row.label,
                }),
            None => None,
        };
        let message_count = post_messages::Entity::find()
            .filter(post_messages::Column::PostId.eq(post_id.0))
            .count(&self.db)
            .await?;
        Ok(Some(PostSummary {
            id: post_id,
            status: row.status,
            owner: row.owner_admin_id.map(decode_user_id).transpose()?,
            category,
            message_count: usize::try_from(message_count)?,
        }))
    }

    /// 已发布帖子的第一条频道消息。手动确认为已发布的帖子
    /// 没有该消息，因为机器人从未获知消息 ID。
    pub async fn channel_message_of(&self, post_id: PostId) -> Result<Option<MessageId>> {
        let mut query = published_messages::Entity::find();
        QueryTrait::query(&mut query).join(
            JoinType::InnerJoin,
            publication_attempts::Entity,
            Expr::col((
                published_messages::Entity,
                published_messages::Column::AttemptId,
            ))
            .equals((
                publication_attempts::Entity,
                publication_attempts::Column::Id,
            )),
        );
        let row = query
            .filter(publication_attempts::Column::PostId.eq(post_id.0))
            .filter(publication_attempts::Column::Status.eq(AttemptStatus::Succeeded))
            .filter(published_messages::Column::Position.eq(0))
            .order_by_desc(published_messages::Column::Id)
            .one(&self.db)
            .await?;
        Ok(row.map(|row| MessageId(row.message_id)))
    }

    /// 携带某帖子按钮的机器人消息（如果有）。
    pub async fn control_message(&self, post_id: PostId) -> Result<Option<(ChatId, MessageId)>> {
        let row = posts::Entity::find_by_id(post_id.0).one(&self.db).await?;
        Ok(row
            .and_then(|row| row.control_chat_id.zip(row.control_message_id))
            .map(|(chat, message)| (ChatId(chat), MessageId(message))))
    }

    /// 开启某帖子的已提交消息，是针对该帖子进行回复的自然目标。
    pub async fn first_message(&self, post_id: PostId) -> Result<Option<(ChatId, MessageId)>> {
        let row = post_messages::Entity::find()
            .filter(post_messages::Column::PostId.eq(post_id.0))
            .order_by_asc(post_messages::Column::Position)
            .one(&self.db)
            .await?;
        Ok(row.map(|row| (ChatId(row.source_chat_id), MessageId(row.source_message_id))))
    }

    /// 找出用户回复其某条消息时所指的帖子：可能是
    /// 已提交的消息，也可能是机器人的控制消息。
    pub async fn find_post_by_message(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
    ) -> Result<Option<PostId>> {
        let submitted = post_messages::Entity::find()
            .filter(post_messages::Column::SourceChatId.eq(chat_id.0))
            .filter(post_messages::Column::SourceMessageId.eq(message_id.0))
            .one(&self.db)
            .await?;
        if let Some(row) = submitted {
            return Ok(Some(PostId(row.post_id)));
        }
        let controlled = posts::Entity::find()
            .filter(posts::Column::ControlChatId.eq(chat_id.0))
            .filter(posts::Column::ControlMessageId.eq(message_id.0))
            .one(&self.db)
            .await?;
        Ok(controlled.map(|row| PostId(row.id)))
    }

    /// 应用用户对已提交消息所做的编辑。帖子不再可编辑后
    /// 不会有任何变化。
    pub async fn update_message(
        &self,
        chat_id: ChatId,
        message_id: MessageId,
        content: &Content,
    ) -> Result<EditOutcome> {
        // 原始消息的归属不可变；在事务开始前确定需要锁定的目标。
        let row = post_messages::Entity::find()
            .filter(post_messages::Column::SourceChatId.eq(chat_id.0))
            .filter(post_messages::Column::SourceMessageId.eq(message_id.0))
            .one(&self.db)
            .await?;
        let Some(row) = row else {
            return Ok(EditOutcome::NotFound);
        };
        let post_id = PostId(row.post_id);
        self.transaction(async move |conn| {
            lock_post(conn, post_id).await?;
            let Some(post) = posts::Entity::find_by_id(post_id.0).one(conn).await? else {
                return Ok(EditOutcome::NotFound);
            };
            let status = post.status;
            if !status.is_editable() {
                return Ok(EditOutcome::Locked(post_id, status));
            }
            post_messages::Entity::update_many()
                .set(message_changes(content)?)
                .filter(post_messages::Column::Id.eq(row.id))
                .exec(conn)
                .await?;
            anyhow::Ok(EditOutcome::Done(post_id))
        })
        .await
    }

    /// 用 `content` 的文本替换帖子的文本。文本帖子获得新
    /// 文本；媒体帖子则把它作为第一条消息的说明文字，这样相册最终
    /// 只有一条说明文字。
    pub async fn replace_text(&self, post_id: PostId, content: &Content) -> Result<EditOutcome> {
        self.transaction(async move |conn| {
            lock_post(conn, post_id).await?;
            let Some(post) = posts::Entity::find_by_id(post_id.0).one(conn).await? else {
                return Ok(EditOutcome::NotFound);
            };
            let status = post.status;
            if !status.is_editable() {
                return Ok(EditOutcome::Locked(post_id, status));
            }
            store_replaced_text(conn, post_id, content).await?;
            anyhow::Ok(EditOutcome::Done(post_id))
        })
        .await
    }

    /// 把媒体消息添加到尚未发布的帖子末尾。
    pub async fn append_messages(
        &self,
        post_id: PostId,
        messages: &[IncomingMessage],
    ) -> Result<AppendOutcome> {
        let result = self
            .transaction(async move |conn| {
                lock_post(conn, post_id).await?;
                let Some(post) = posts::Entity::find_by_id(post_id.0).one(conn).await? else {
                    return Ok(AppendOutcome::NotFound);
                };
                let status = post.status;
                if !status.is_editable() {
                    return Ok(AppendOutcome::Locked(status));
                }
                if any_stored(conn, messages).await? {
                    return Ok(AppendOutcome::Duplicate);
                }

                let existing = post_messages::Entity::find()
                    .filter(post_messages::Column::PostId.eq(post_id.0))
                    .order_by_asc(post_messages::Column::Position)
                    .all(conn)
                    .await?;
                let Some(existing_kinds) = existing
                    .iter()
                    .map(|row| row.media_kind)
                    .collect::<Option<Vec<_>>>()
                else {
                    return Ok(AppendOutcome::Rejected(AppendRejection::TextOnly));
                };
                let added_kinds = messages
                    .iter()
                    .map(|message| message.content.media.as_ref().map(|media| media.kind))
                    .collect::<Option<Vec<_>>>()
                    .context("appended messages must carry media")?;
                let kinds = [existing_kinds, added_kinds].concat();
                if kinds.len() > MAX_ALBUM_ITEMS {
                    return Ok(AppendOutcome::Rejected(AppendRejection::TooManyItems));
                }
                if !MediaKind::can_share_album(&kinds) {
                    return Ok(AppendOutcome::Rejected(AppendRejection::NotAlbum));
                }

                let first_position = existing.last().map_or(0, |row| row.position + 1);
                if !messages.is_empty() {
                    let first_position = usize::try_from(first_position)?;
                    let rows = messages
                        .iter()
                        .enumerate()
                        .map(|(offset, message)| {
                            new_message(post_id.0, first_position + offset, message)
                        })
                        .collect::<Result<Vec<_>>>()?;
                    post_messages::Entity::insert_many(rows)
                        .exec_without_returning(conn)
                        .await?;
                }
                anyhow::Ok(AppendOutcome::Appended { total: kinds.len() })
            })
            .await;
        match result {
            Err(error)
                if error.downcast_ref::<DbErr>().is_some_and(|error| {
                    matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_)))
                }) =>
            {
                Ok(AppendOutcome::Duplicate)
            }
            result => result,
        }
    }

    /// 用 `messages` 替换尚未发布的帖子的媒体：`item` 为 `Some` 时只替换该位置的
    /// 一项，`messages` 必须恰好一条；为 `None` 时整组替换。新媒体带说明文字时，
    /// 它成为帖子的文字，否则保留原来的文字。被替换的项改以新消息为来源，
    /// 之后编辑新消息也会同步到帖子。
    pub async fn replace_media(
        &self,
        post_id: PostId,
        item: Option<usize>,
        messages: &[IncomingMessage],
    ) -> Result<ReplaceMediaOutcome> {
        let result = self
            .transaction(async move |conn| {
                lock_post(conn, post_id).await?;
                let Some(post) = posts::Entity::find_by_id(post_id.0).one(conn).await? else {
                    return Ok(ReplaceMediaOutcome::NotFound);
                };
                let status = post.status;
                if !status.is_editable() {
                    return Ok(ReplaceMediaOutcome::Locked(status));
                }
                if any_stored(conn, messages).await? {
                    return Ok(ReplaceMediaOutcome::Duplicate);
                }

                let existing = post_messages::Entity::find()
                    .filter(post_messages::Column::PostId.eq(post_id.0))
                    .order_by_asc(post_messages::Column::Position)
                    .all(conn)
                    .await?;
                let Some(existing_kinds) = existing
                    .iter()
                    .map(|row| row.media_kind)
                    .collect::<Option<Vec<_>>>()
                else {
                    return Ok(ReplaceMediaOutcome::Rejected(
                        ReplaceMediaRejection::TextOnly,
                    ));
                };
                let added_kinds = messages
                    .iter()
                    .map(|message| message.content.media.as_ref().map(|media| media.kind))
                    .collect::<Option<Vec<_>>>()
                    .context("replacement messages must carry media")?;
                let kinds = match item {
                    Some(index) => {
                        let [added] = added_kinds[..] else {
                            bail!("a single item must be replaced by exactly one message");
                        };
                        if index >= existing_kinds.len() {
                            return Ok(ReplaceMediaOutcome::Rejected(
                                ReplaceMediaRejection::ItemGone,
                            ));
                        }
                        let mut kinds = existing_kinds;
                        kinds[index] = added;
                        kinds
                    }
                    None if added_kinds.is_empty() => bail!("no replacement media"),
                    None => added_kinds,
                };
                if kinds.len() > MAX_ALBUM_ITEMS {
                    return Ok(ReplaceMediaOutcome::Rejected(
                        ReplaceMediaRejection::TooManyItems,
                    ));
                }
                if kinds.len() > 1 && !MediaKind::can_share_album(&kinds) {
                    return Ok(ReplaceMediaOutcome::Rejected(
                        ReplaceMediaRejection::NotAlbum,
                    ));
                }

                let caption = messages
                    .iter()
                    .map(|message| &message.content)
                    .find(|content| content.text.is_some());
                match item {
                    Some(index) => {
                        store_replaced_item(conn, &existing[index], &messages[0]).await?;
                    }
                    None => store_replaced_album(conn, post_id, &existing, messages).await?,
                }
                if let Some(caption) = caption {
                    store_replaced_text(conn, post_id, caption).await?;
                }
                anyhow::Ok(ReplaceMediaOutcome::Replaced {
                    total: kinds.len(),
                    text_replaced: caption.is_some(),
                })
            })
            .await;
        match result {
            Err(error)
                if error.downcast_ref::<DbErr>().is_some_and(|error| {
                    matches!(error.sql_err(), Some(SqlErr::UniqueConstraintViolation(_)))
                }) =>
            {
                Ok(ReplaceMediaOutcome::Duplicate)
            }
            result => result,
        }
    }

    /// `admin` 各状态的帖子数量，以及每个分类中排队的数量。
    pub async fn stock_counts(&self, admin: UserId) -> Result<StockCounts> {
        let rows = posts::Entity::find()
            .filter(posts::Column::OwnerAdminId.eq(user_id(admin)?))
            .select_only()
            .column(posts::Column::Status)
            .column(posts::Column::CategoryId)
            .column_as(posts::Column::Id.count(), "count")
            .group_by(posts::Column::Status)
            .group_by(posts::Column::CategoryId)
            .into_tuple::<(PostStatus, Option<i64>, i64)>()
            .all(&self.db)
            .await?;
        let mut counts = StockCounts::default();
        for (status, category, count) in rows {
            match status {
                PostStatus::Queued => {
                    let category = category.context("queued post without category")?;
                    *counts.queued.entry(CategoryId(category)).or_insert(0) += count;
                }
                PostStatus::Draft => counts.draft += count,
                PostStatus::Reserved => counts.reserved += count,
                PostStatus::Failed => counts.failed += count,
                PostStatus::Published
                | PostStatus::Cancelled
                | PostStatus::PendingReview
                | PostStatus::Rejected => {}
            }
        }
        Ok(counts)
    }

    pub async fn queued_candidates(&self, admin: UserId) -> Result<Vec<Candidate>> {
        queued_candidates(&self.db, admin).await
    }
}

/// 这些消息中是否有已经存储过的，Telegram 重新投递更新时会发生。
async fn any_stored(conn: &impl ConnectionTrait, messages: &[IncomingMessage]) -> Result<bool> {
    for message in messages {
        let known = post_messages::Entity::find()
            .filter(post_messages::Column::SourceChatId.eq(message.source_chat_id.0))
            .filter(post_messages::Column::SourceMessageId.eq(message.source_message_id.0))
            .count(conn)
            .await?;
        if known > 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 把一行换成 `message` 的媒体和来源，保留这一行原来的文字。
async fn store_replaced_item(
    conn: &impl ConnectionTrait,
    row: &post_messages::Model,
    message: &IncomingMessage,
) -> Result<()> {
    let media = message
        .content
        .media
        .as_ref()
        .context("replacement messages must carry media")?;
    post_messages::Entity::update_many()
        .set(post_messages::ActiveModel {
            source_chat_id: Set(message.source_chat_id.0),
            source_message_id: Set(message.source_message_id.0),
            media_kind: Set(Some(media.kind)),
            media_file_id: Set(Some(media.file_id.clone())),
            media_file_unique_id: Set(Some(media.file_unique_id.clone())),
            media_has_spoiler: Set(media.has_spoiler),
            ..Default::default()
        })
        .filter(post_messages::Column::Id.eq(row.id))
        .exec(conn)
        .await?;
    Ok(())
}

/// 删掉帖子现有的消息，换成 `messages`。原来的文字（如果有）
/// 放到新的第一条消息上；新消息带了说明文字时，调用方随后会替换它。
async fn store_replaced_album(
    conn: &impl ConnectionTrait,
    post_id: PostId,
    existing: &[post_messages::Model],
    messages: &[IncomingMessage],
) -> Result<()> {
    let kept = existing.iter().find(|row| row.text.is_some());
    post_messages::Entity::delete_many()
        .filter(post_messages::Column::PostId.eq(post_id.0))
        .exec(conn)
        .await?;
    let rows = messages
        .iter()
        .enumerate()
        .map(|(position, message)| new_message(post_id.0, position, message))
        .collect::<Result<Vec<_>>>()?;
    post_messages::Entity::insert_many(rows)
        .exec_without_returning(conn)
        .await?;
    if let Some(kept) = kept {
        post_messages::Entity::update_many()
            .set(post_messages::ActiveModel {
                text: Set(kept.text.clone()),
                entities_json: Set(kept.entities_json.clone()),
                show_caption_above_media: Set(kept.show_caption_above_media),
                ..Default::default()
            })
            .filter(post_messages::Column::PostId.eq(post_id.0))
            .filter(post_messages::Column::Position.eq(0))
            .exec(conn)
            .await?;
    }
    Ok(())
}

/// 把帖子的文字换成 `content` 的文字，不检查状态。文本帖子获得新文本；
/// 媒体帖子把它作为第一条消息的说明文字，其余消息的说明文字清空，
/// 这样相册最终只有一条说明文字。
pub(super) async fn store_replaced_text(
    conn: &impl ConnectionTrait,
    post_id: PostId,
    content: &Content,
) -> Result<()> {
    let rows = post_messages::Entity::find()
        .filter(post_messages::Column::PostId.eq(post_id.0))
        .order_by_asc(post_messages::Column::Position)
        .all(conn)
        .await?;
    let entities = serde_json::to_string(&content.entities)?;
    for (index, row) in rows.into_iter().enumerate() {
        let media_kind = row.media_kind;
        if index > 0 {
            post_messages::Entity::update_many()
                .set(post_messages::ActiveModel {
                    text: Set(None),
                    entities_json: Set("[]".to_owned()),
                    ..Default::default()
                })
                .filter(post_messages::Column::Id.eq(row.id))
                .exec(conn)
                .await?;
            continue;
        }
        let link_preview = match media_kind {
            Some(_) => None,
            None => content
                .link_preview_options
                .as_ref()
                .map(serde_json::to_string)
                .transpose()?,
        };
        post_messages::Entity::update_many()
            .set(post_messages::ActiveModel {
                text: Set(content.text.clone()),
                entities_json: Set(entities.clone()),
                link_preview_json: Set(link_preview),
                ..Default::default()
            })
            .filter(post_messages::Column::Id.eq(row.id))
            .exec(conn)
            .await?;
    }
    Ok(())
}

/// 某位管理员的排队帖子，最新的在前。
pub(super) async fn queued_candidates(
    conn: &impl ConnectionTrait,
    admin: UserId,
) -> Result<Vec<Candidate>> {
    let rows = posts::Entity::find()
        .filter(posts::Column::Status.eq(PostStatus::Queued))
        .filter(posts::Column::OwnerAdminId.eq(user_id(admin)?))
        .order_by_desc(posts::Column::QueuedAt)
        .order_by_desc(posts::Column::Id)
        .all(conn)
        .await?;
    rows.into_iter()
        .map(|row| {
            let category = row.category_id.context("queued post without category")?;
            Ok(Candidate {
                id: PostId(row.id),
                category: CategoryId(category),
            })
        })
        .collect()
}

/// 加载完整的帖子，按 `ids` 的顺序返回。
pub(super) async fn load_posts(conn: &impl ConnectionTrait, ids: &[i64]) -> Result<Vec<Post>> {
    let rows = post_messages::Entity::find()
        .filter(post_messages::Column::PostId.is_in(ids.iter().copied()))
        .order_by_asc(post_messages::Column::PostId)
        .order_by_asc(post_messages::Column::Position)
        .all(conn)
        .await?;
    let mut by_post: HashMap<i64, Vec<Content>> = HashMap::new();
    for row in rows {
        by_post
            .entry(row.post_id)
            .or_default()
            .push(row.into_content()?);
    }
    let reply_targets: HashMap<i64, Option<i32>> = posts::Entity::find()
        .filter(posts::Column::Id.is_in(ids.iter().copied()))
        .all(conn)
        .await?
        .into_iter()
        .map(|row| (row.id, row.reply_to_message_id))
        .collect();
    let mut loaded = Vec::with_capacity(ids.len());
    for &id in ids {
        let messages = by_post
            .remove(&id)
            .with_context(|| format!("post {id} has no stored messages"))?;
        let reply_to = reply_targets.get(&id).copied().flatten().map(MessageId);
        loaded.push(Post {
            id: PostId(id),
            messages,
            reply_to,
        });
    }
    Ok(loaded)
}

/// 让没有发出去的帖子回到发布前的地方：有分类的回队列，
/// 没选分类就「立即发送」的草稿回到草稿。
pub(super) async fn requeue_posts(
    conn: &impl ConnectionTrait,
    ids: &[i64],
    now: Timestamp,
) -> Result<()> {
    posts::Entity::update_many()
        .set(posts::ActiveModel {
            status: Set(PostStatus::Queued),
            queued_at: Set(Some(timestamp(now)?)),
            ..Default::default()
        })
        .filter(posts::Column::Id.is_in(ids.iter().copied()))
        .filter(posts::Column::CategoryId.is_not_null())
        .exec(conn)
        .await?;
    posts::Entity::update_many()
        .set(posts::ActiveModel {
            status: Set(PostStatus::Draft),
            queued_at: Set(None),
            ..Default::default()
        })
        .filter(posts::Column::Id.is_in(ids.iter().copied()))
        .filter(posts::Column::CategoryId.is_null())
        .exec(conn)
        .await?;
    Ok(())
}

pub(super) async fn mark_published(
    conn: &impl ConnectionTrait,
    id: i64,
    now: Timestamp,
) -> Result<()> {
    posts::Entity::update_many()
        .set(posts::ActiveModel {
            status: Set(PostStatus::Published),
            published_at: Set(Some(timestamp(now)?)),
            ..Default::default()
        })
        .filter(posts::Column::Id.eq(id))
        .exec(conn)
        .await?;
    Ok(())
}

pub(super) async fn mark_failed(conn: &impl ConnectionTrait, id: i64) -> Result<()> {
    posts::Entity::update_many()
        .set(posts::ActiveModel {
            status: Set(PostStatus::Failed),
            ..Default::default()
        })
        .filter(posts::Column::Id.eq(id))
        .exec(conn)
        .await?;
    Ok(())
}

pub(super) async fn mark_reserved(conn: &impl ConnectionTrait, ids: &[i64]) -> Result<()> {
    posts::Entity::update_many()
        .set(posts::ActiveModel {
            status: Set(PostStatus::Reserved),
            ..Default::default()
        })
        .filter(posts::Column::Id.is_in(ids.iter().copied()))
        .exec(conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use jiff::SignedDuration;

    use super::*;
    use crate::store::Store;
    use crate::store::codec::decode_timestamp;
    use crate::store::test_support::{
        ADMIN, category_id, incoming, incoming_media, insert_category, queue,
    };

    #[tokio::test]
    async fn stores_draft_and_queues_it() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let messages = [incoming(10, "first"), incoming(11, "second")];
        let post_id = store
            .create_draft(&messages, now, None)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            queue(&store, post_id, "gi", now).await,
            QueueOutcome::Queued(_)
        ));
        assert_eq!(
            queue(&store, post_id, "gi", now).await,
            QueueOutcome::AlreadyHandled
        );

        let candidates = store.queued_candidates(ADMIN).await.unwrap();
        assert_eq!(
            candidates,
            vec![Candidate {
                id: post_id,
                category: category_id(&store, "gi").await
            }]
        );
        let posts = load_posts(&store.db, &[post_id.0]).await.unwrap();
        let texts: Vec<_> = posts[0]
            .messages
            .iter()
            .map(|content| content.text.clone().unwrap())
            .collect();
        assert_eq!(texts, ["first", "second"]);
    }

    #[tokio::test]
    async fn ignores_redelivered_messages() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let messages = [incoming(10, "hello")];
        assert!(
            store
                .create_draft(&messages, now, None)
                .await
                .unwrap()
                .is_some()
        );
        assert!(
            store
                .create_draft(&messages, now, None)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn duplicate_source_rolls_back_the_entire_draft() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        store
            .create_draft(&[incoming(10, "known")], now, None)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            store
                .create_draft(&[incoming(11, "new"), incoming(10, "known")], now, None)
                .await
                .unwrap(),
            None
        );
        assert_eq!(posts::Entity::find().count(&store.db).await.unwrap(), 1);
        assert_eq!(
            post_messages::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
            1
        );
        assert!(
            store
                .create_draft(&[incoming(11, "new")], now, None)
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn concurrent_redelivery_creates_only_one_draft() {
        let (_directory, store) = Store::open_temporary().await;
        let messages = [incoming(10, "hello")];
        let (first, second) = tokio::join!(
            store.create_draft(&messages, Timestamp::UNIX_EPOCH, None),
            store.create_draft(&messages, Timestamp::UNIX_EPOCH, None),
        );
        assert_ne!(first.unwrap().is_some(), second.unwrap().is_some());
        assert_eq!(posts::Entity::find().count(&store.db).await.unwrap(), 1);
        assert_eq!(
            post_messages::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
            1
        );
    }

    #[tokio::test]
    async fn old_drafts_can_still_be_queued() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let post_id = draft(&store, &[incoming(1, "late")]).await;
        let weeks_later = now + SignedDuration::from_hours(24 * 28);
        assert!(matches!(
            queue(&store, post_id, "gi", weeks_later).await,
            QueueOutcome::Queued(_)
        ));
    }

    async fn draft(store: &Store, messages: &[IncomingMessage]) -> PostId {
        let now = Timestamp::UNIX_EPOCH;
        store
            .create_draft(messages, now, None)
            .await
            .unwrap()
            .unwrap()
    }

    async fn texts(store: &Store, post_id: PostId) -> Vec<Option<String>> {
        let posts = load_posts(&store.db, &[post_id.0]).await.unwrap();
        posts[0]
            .messages
            .iter()
            .map(|content| content.text.clone())
            .collect()
    }

    #[tokio::test]
    async fn cancelled_posts_leave_the_queue_and_cannot_be_restored() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let queued = draft(&store, &[incoming(1, "a")]).await;
        queue(&store, queued, "gi", now).await;
        let waiting = draft(&store, &[incoming(2, "b")]).await;

        assert!(store.cancel_post(queued, ADMIN).await.unwrap());
        assert!(store.cancel_post(waiting, ADMIN).await.unwrap());
        assert!(!store.cancel_post(waiting, ADMIN).await.unwrap());
        assert!(store.queued_candidates(ADMIN).await.unwrap().is_empty());
        assert_eq!(
            queue(&store, waiting, "gi", now).await,
            QueueOutcome::AlreadyHandled
        );
    }

    #[tokio::test]
    async fn reserved_posts_cannot_be_cancelled() {
        let (_directory, store) = Store::open_temporary().await;
        let post = draft(&store, &[incoming(1, "a")]).await;
        queue(&store, post, "gi", Timestamp::UNIX_EPOCH).await;
        mark_reserved(&store.db, &[post.0]).await.unwrap();
        assert!(!store.cancel_post(post, ADMIN).await.unwrap());
    }

    #[tokio::test]
    async fn finds_posts_by_submitted_or_control_message() {
        let (_directory, store) = Store::open_temporary().await;
        let post = draft(&store, &[incoming(10, "a")]).await;
        store
            .set_control_message(post, ChatId(42), MessageId(99))
            .await
            .unwrap();
        let find = |id| store.find_post_by_message(ChatId(42), MessageId(id));
        assert_eq!(find(10).await.unwrap(), Some(post));
        assert_eq!(find(99).await.unwrap(), Some(post));
        assert_eq!(find(5).await.unwrap(), None);
    }

    #[tokio::test]
    async fn draft_keeps_the_channel_message_it_replies_to() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let post = store
            .create_draft(&[incoming(1, "B")], now, Some(MessageId(77)))
            .await
            .unwrap()
            .unwrap();
        let plain = draft(&store, &[incoming(2, "C")]).await;
        let posts = load_posts(&store.db, &[post.0, plain.0]).await.unwrap();
        assert_eq!(posts[0].reply_to, Some(MessageId(77)));
        assert_eq!(posts[1].reply_to, None);
    }

    #[tokio::test]
    async fn control_message_is_the_latest_one_set() {
        let (_directory, store) = Store::open_temporary().await;
        let post = draft(&store, &[incoming(1, "a")]).await;
        assert_eq!(store.control_message(post).await.unwrap(), None);
        store
            .set_control_message(post, ChatId(42), MessageId(7))
            .await
            .unwrap();
        store
            .set_control_message(post, ChatId(42), MessageId(9))
            .await
            .unwrap();
        assert_eq!(
            store.control_message(post).await.unwrap(),
            Some((ChatId(42), MessageId(9)))
        );
        assert_eq!(store.control_message(PostId(999)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn first_message_is_the_one_at_position_zero() {
        let (_directory, store) = Store::open_temporary().await;
        let post = draft(&store, &[incoming(20, "a"), incoming(21, "b")]).await;
        assert_eq!(
            store.first_message(post).await.unwrap(),
            Some((ChatId(42), MessageId(20)))
        );
        assert_eq!(store.first_message(PostId(999)).await.unwrap(), None);
    }

    #[tokio::test]
    async fn edited_message_updates_stored_content_until_locked() {
        let (_directory, store) = Store::open_temporary().await;
        let post = draft(&store, &[incoming(10, "typo")]).await;
        let fixed = incoming(10, "fixed").content;

        let outcome = store
            .update_message(ChatId(42), MessageId(10), &fixed)
            .await
            .unwrap();
        assert_eq!(outcome, EditOutcome::Done(post));
        assert_eq!(texts(&store, post).await, [Some("fixed".to_owned())]);

        let unknown = store
            .update_message(ChatId(42), MessageId(11), &fixed)
            .await
            .unwrap();
        assert_eq!(unknown, EditOutcome::NotFound);

        queue(&store, post, "gi", Timestamp::UNIX_EPOCH).await;
        mark_reserved(&store.db, &[post.0]).await.unwrap();
        let locked = store
            .update_message(ChatId(42), MessageId(10), &incoming(10, "late").content)
            .await
            .unwrap();
        assert_eq!(locked, EditOutcome::Locked(post, PostStatus::Reserved));
        assert_eq!(texts(&store, post).await, [Some("fixed".to_owned())]);
    }

    #[tokio::test]
    async fn replacing_text_keeps_a_single_caption_on_the_first_message() {
        let (_directory, store) = Store::open_temporary().await;
        let album = draft(
            &store,
            &[
                incoming_media(1, MediaKind::Photo, None),
                incoming_media(2, MediaKind::Photo, Some("old")),
            ],
        )
        .await;
        let outcome = store
            .replace_text(album, &incoming(3, "new").content)
            .await
            .unwrap();
        assert_eq!(outcome, EditOutcome::Done(album));
        assert_eq!(texts(&store, album).await, [Some("new".to_owned()), None]);

        let text_post = draft(&store, &[incoming(4, "old text")]).await;
        store
            .replace_text(text_post, &incoming(5, "new text").content)
            .await
            .unwrap();
        assert_eq!(
            texts(&store, text_post).await,
            [Some("new text".to_owned())]
        );
    }

    #[tokio::test]
    async fn appends_media_after_existing_messages() {
        let (_directory, store) = Store::open_temporary().await;
        let post = draft(&store, &[incoming_media(1, MediaKind::Photo, Some("cap"))]).await;
        let added = [
            incoming_media(2, MediaKind::Video, None),
            incoming_media(3, MediaKind::Photo, None),
        ];
        let outcome = store.append_messages(post, &added).await.unwrap();
        assert_eq!(outcome, AppendOutcome::Appended { total: 3 });
        assert_eq!(texts(&store, post).await.len(), 3);
        assert_eq!(
            store.append_messages(post, &added).await.unwrap(),
            AppendOutcome::Duplicate
        );
    }

    #[tokio::test]
    async fn concurrent_appends_keep_unique_positions_and_the_album_limit() {
        let (_directory, store) = Store::open_temporary().await;
        let initial: Vec<_> = (1..=9)
            .map(|id| incoming_media(id, MediaKind::Photo, None))
            .collect();
        let post = draft(&store, &initial).await;
        let first = [incoming_media(10, MediaKind::Photo, None)];
        let second = [incoming_media(11, MediaKind::Photo, None)];
        let (first, second) = tokio::join!(
            store.append_messages(post, &first),
            store.append_messages(post, &second),
        );
        let outcomes = [first.unwrap(), second.unwrap()];
        assert!(outcomes.contains(&AppendOutcome::Appended {
            total: MAX_ALBUM_ITEMS,
        }));
        assert!(outcomes.contains(&AppendOutcome::Rejected(AppendRejection::TooManyItems,)));
        let rows = post_messages::Entity::find()
            .filter(post_messages::Column::PostId.eq(post.0))
            .order_by_asc(post_messages::Column::Position)
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(rows.len(), MAX_ALBUM_ITEMS);
        assert_eq!(
            rows.iter().map(|row| row.position).collect::<Vec<_>>(),
            (0..i32::try_from(MAX_ALBUM_ITEMS).unwrap()).collect::<Vec<_>>()
        );
        assert_eq!(
            load_posts(&store.db, &[post.0]).await.unwrap()[0]
                .messages
                .len(),
            MAX_ALBUM_ITEMS
        );
    }

    #[tokio::test]
    async fn rejects_media_that_cannot_form_an_album() {
        let (_directory, store) = Store::open_temporary().await;
        let text_post = draft(&store, &[incoming(1, "text")]).await;
        assert_eq!(
            store
                .append_messages(text_post, &[incoming_media(2, MediaKind::Photo, None)])
                .await
                .unwrap(),
            AppendOutcome::Rejected(AppendRejection::TextOnly)
        );

        let photo = draft(&store, &[incoming_media(3, MediaKind::Photo, None)]).await;
        assert_eq!(
            store
                .append_messages(photo, &[incoming_media(4, MediaKind::Document, None)])
                .await
                .unwrap(),
            AppendOutcome::Rejected(AppendRejection::NotAlbum)
        );

        let full: Vec<_> = (10..19)
            .map(|id| incoming_media(id, MediaKind::Photo, None))
            .collect();
        let album = draft(&store, &full).await;
        assert_eq!(
            store
                .append_messages(
                    album,
                    &[
                        incoming_media(30, MediaKind::Photo, None),
                        incoming_media(31, MediaKind::Photo, None)
                    ]
                )
                .await
                .unwrap(),
            AppendOutcome::Rejected(AppendRejection::TooManyItems)
        );
    }

    async fn rows_of(store: &Store, post: PostId) -> Vec<post_messages::Model> {
        post_messages::Entity::find()
            .filter(post_messages::Column::PostId.eq(post.0))
            .order_by_asc(post_messages::Column::Position)
            .all(&store.db)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn replaces_one_item_and_keeps_the_text() {
        let (_directory, store) = Store::open_temporary().await;
        let post = draft(
            &store,
            &[
                incoming_media(1, MediaKind::Photo, Some("cap")),
                incoming_media(2, MediaKind::Photo, None),
            ],
        )
        .await;
        let outcome = store
            .replace_media(post, Some(1), &[incoming_media(5, MediaKind::Video, None)])
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ReplaceMediaOutcome::Replaced {
                total: 2,
                text_replaced: false
            }
        );
        let rows = rows_of(&store, post).await;
        assert_eq!(rows[0].media_file_id.as_deref(), Some("file-1"));
        assert_eq!(rows[0].text.as_deref(), Some("cap"));
        assert_eq!(rows[1].media_file_id.as_deref(), Some("file-5"));
        assert_eq!(rows[1].media_kind, Some(MediaKind::Video));
        assert_eq!(rows[1].source_message_id, 5);

        // 再投递一次同样的消息不会重复替换。
        assert_eq!(
            store
                .replace_media(post, Some(1), &[incoming_media(5, MediaKind::Video, None)])
                .await
                .unwrap(),
            ReplaceMediaOutcome::Duplicate
        );
    }

    #[tokio::test]
    async fn a_caption_on_the_new_item_becomes_the_post_text() {
        let (_directory, store) = Store::open_temporary().await;
        let post = draft(
            &store,
            &[
                incoming_media(1, MediaKind::Photo, Some("old")),
                incoming_media(2, MediaKind::Photo, None),
            ],
        )
        .await;
        let outcome = store
            .replace_media(
                post,
                Some(1),
                &[incoming_media(5, MediaKind::Photo, Some("new"))],
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ReplaceMediaOutcome::Replaced {
                total: 2,
                text_replaced: true
            }
        );
        assert_eq!(texts(&store, post).await, [Some("new".to_owned()), None]);
    }

    #[tokio::test]
    async fn replaces_the_whole_album_and_keeps_the_text() {
        let (_directory, store) = Store::open_temporary().await;
        let post = draft(
            &store,
            &[
                incoming_media(1, MediaKind::Photo, None),
                incoming_media(2, MediaKind::Photo, Some("cap")),
                incoming_media(3, MediaKind::Photo, None),
            ],
        )
        .await;
        let outcome = store
            .replace_media(
                post,
                None,
                &[
                    incoming_media(7, MediaKind::Video, None),
                    incoming_media(8, MediaKind::Photo, None),
                ],
            )
            .await
            .unwrap();
        assert_eq!(
            outcome,
            ReplaceMediaOutcome::Replaced {
                total: 2,
                text_replaced: false
            }
        );
        let rows = rows_of(&store, post).await;
        let sources: Vec<_> = rows.iter().map(|row| row.source_message_id).collect();
        assert_eq!(sources, [7, 8]);
        assert_eq!(texts(&store, post).await, [Some("cap".to_owned()), None]);
    }

    #[tokio::test]
    async fn rejects_replacements_that_do_not_fit() {
        let (_directory, store) = Store::open_temporary().await;
        let text_post = draft(&store, &[incoming(1, "text")]).await;
        assert_eq!(
            store
                .replace_media(
                    text_post,
                    None,
                    &[incoming_media(2, MediaKind::Photo, None)]
                )
                .await
                .unwrap(),
            ReplaceMediaOutcome::Rejected(ReplaceMediaRejection::TextOnly)
        );

        let album = draft(
            &store,
            &[
                incoming_media(3, MediaKind::Photo, None),
                incoming_media(4, MediaKind::Photo, None),
            ],
        )
        .await;
        assert_eq!(
            store
                .replace_media(album, Some(2), &[incoming_media(5, MediaKind::Photo, None)])
                .await
                .unwrap(),
            ReplaceMediaOutcome::Rejected(ReplaceMediaRejection::ItemGone)
        );
        assert_eq!(
            store
                .replace_media(
                    album,
                    Some(0),
                    &[incoming_media(6, MediaKind::Document, None)]
                )
                .await
                .unwrap(),
            ReplaceMediaOutcome::Rejected(ReplaceMediaRejection::NotAlbum)
        );
        let too_many: Vec<_> = (10..21)
            .map(|id| incoming_media(id, MediaKind::Photo, None))
            .collect();
        assert_eq!(
            store.replace_media(album, None, &too_many).await.unwrap(),
            ReplaceMediaOutcome::Rejected(ReplaceMediaRejection::TooManyItems)
        );

        // 单独一个动图可以换掉单张图片。
        let single = draft(&store, &[incoming_media(30, MediaKind::Photo, None)]).await;
        assert!(matches!(
            store
                .replace_media(
                    single,
                    None,
                    &[incoming_media(31, MediaKind::Animation, None)]
                )
                .await
                .unwrap(),
            ReplaceMediaOutcome::Replaced { total: 1, .. }
        ));
    }

    #[tokio::test]
    async fn published_posts_cannot_receive_media() {
        let (_directory, store) = Store::open_temporary().await;
        let post = draft(&store, &[incoming_media(1, MediaKind::Photo, None)]).await;
        queue(&store, post, "gi", Timestamp::UNIX_EPOCH).await;
        mark_published(&store.db, post.0, Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        assert_eq!(
            store
                .append_messages(post, &[incoming_media(2, MediaKind::Photo, None)])
                .await
                .unwrap(),
            AppendOutcome::Locked(PostStatus::Published)
        );
        assert_eq!(
            store
                .replace_media(post, None, &[incoming_media(3, MediaKind::Photo, None)])
                .await
                .unwrap(),
            ReplaceMediaOutcome::Locked(PostStatus::Published)
        );
    }

    #[tokio::test]
    async fn only_the_owner_can_queue_or_cancel_a_draft() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let other = UserId(7);
        store.sync_admins(&[ADMIN, other], now).await.unwrap();
        let foreign = insert_category(&store, other, "x").await;
        let post = draft(&store, &[incoming(1, "a")]).await;

        assert_eq!(
            store.queue_post(post, ADMIN, foreign, now).await.unwrap(),
            QueueOutcome::CategoryGone
        );
        assert_eq!(
            store.queue_post(post, other, foreign, now).await.unwrap(),
            QueueOutcome::AlreadyHandled
        );
        assert!(!store.cancel_post(post, other).await.unwrap());
        assert!(store.cancel_post(post, ADMIN).await.unwrap());
    }

    #[tokio::test]
    async fn a_queued_post_can_move_to_another_category_and_keeps_its_queue_time() {
        let (_directory, store) = Store::open_temporary().await;
        let queued_at = Timestamp::UNIX_EPOCH;
        let post = draft(&store, &[incoming(1, "a")]).await;
        queue(&store, post, "gi", queued_at).await;
        let hsr = category_id(&store, "hsr").await;

        let outcome = store.move_queued_post(post, ADMIN, hsr).await.unwrap();
        assert!(matches!(outcome, MoveOutcome::Moved(category) if category.id == hsr));
        let summary = store.post_summary(post).await.unwrap().unwrap();
        assert_eq!(summary.status, PostStatus::Queued);
        assert_eq!(summary.category.map(|category| category.id), Some(hsr));
        assert_eq!(stored_queue_time(&store, post).await, queued_at);
    }

    async fn stored_queue_time(store: &Store, post: PostId) -> Timestamp {
        let row = posts::Entity::find_by_id(post.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        decode_timestamp(row.queued_at.unwrap()).unwrap()
    }

    #[tokio::test]
    async fn moving_to_the_current_category_still_returns_moved() {
        let (_directory, store) = Store::open_temporary().await;
        let queued_at = Timestamp::UNIX_EPOCH;
        let post = draft(&store, &[incoming(1, "a")]).await;
        queue(&store, post, "gi", queued_at).await;
        let category = category_id(&store, "gi").await;
        assert_eq!(
            store.move_queued_post(post, ADMIN, category).await.unwrap(),
            MoveOutcome::Moved(Category {
                id: category,
                label: "gi".to_owned()
            })
        );
        let summary = store.post_summary(post).await.unwrap().unwrap();
        assert_eq!(summary.status, PostStatus::Queued);
        assert_eq!(stored_queue_time(&store, post).await, queued_at);
    }

    #[tokio::test]
    async fn only_queued_posts_of_the_owner_can_move_to_own_categories() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let other = UserId(7);
        store.sync_admins(&[ADMIN, other], now).await.unwrap();
        let foreign = insert_category(&store, other, "x").await;
        let hsr = category_id(&store, "hsr").await;

        let draft_post = draft(&store, &[incoming(1, "a")]).await;
        assert_eq!(
            store
                .move_queued_post(draft_post, ADMIN, hsr)
                .await
                .unwrap(),
            MoveOutcome::NotQueued
        );

        let post = draft(&store, &[incoming(2, "b")]).await;
        queue(&store, post, "gi", now).await;
        assert_eq!(
            store.move_queued_post(post, ADMIN, foreign).await.unwrap(),
            MoveOutcome::CategoryGone
        );
        assert_eq!(
            store.move_queued_post(post, other, foreign).await.unwrap(),
            MoveOutcome::NotQueued
        );

        mark_reserved(&store.db, &[post.0]).await.unwrap();
        assert_eq!(
            store.move_queued_post(post, ADMIN, hsr).await.unwrap(),
            MoveOutcome::NotQueued
        );
    }

    #[tokio::test]
    async fn stock_and_candidates_only_cover_the_admin() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let other = UserId(7);
        store.sync_admins(&[ADMIN, other], now).await.unwrap();
        let post = draft(&store, &[incoming(1, "a")]).await;
        queue(&store, post, "gi", now).await;
        draft(&store, &[incoming(2, "b")]).await;

        let counts = store.stock_counts(ADMIN).await.unwrap();
        let gi = store.categories(ADMIN).await.unwrap().remove(0);
        assert_eq!(counts.queued_in(&gi), 1);
        assert_eq!(counts.draft, 1);
        assert_eq!(store.queued_candidates(ADMIN).await.unwrap().len(), 1);

        let other_counts = store.stock_counts(other).await.unwrap();
        assert_eq!(other_counts.draft, 0);
        assert!(store.queued_candidates(other).await.unwrap().is_empty());
        assert_eq!(category_id(&store, "gi").await, gi.id);
    }
}
