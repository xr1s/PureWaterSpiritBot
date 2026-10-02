use jiff::{SignedDuration, Timestamp, tz::TimeZone};
use sea_orm::{ActiveModelTrait, ColumnTrait, EntityTrait, QueryFilter, Set};
use teloxide::types::{ChatId, MessageId, UserId};

use super::{
    Store,
    codec::{timestamp, user_id},
    entities::{categories, slot_pick_fallbacks, slot_picks, slots},
};
use crate::{
    model::{
        CategoryId, Content, IncomingMessage, Media, MediaKind, PositiveDuration, PostId, Slot,
        SlotId,
    },
    schedule::{ScheduleDefaults, tests::pick},
    store::QueueOutcome,
};

/// 每个测试帖子所属的管理员。
pub const ADMIN: UserId = UserId(42);

pub async fn seed(store: &Store) {
    store
        .sync_admins(&[ADMIN], Timestamp::UNIX_EPOCH)
        .await
        .unwrap();
    // 顺序必须和 `schedule::tests::cid` 的映射一致：分类 ID 依次是 1 到 4。
    for label in ["gi", "hsr", "other", "misc"] {
        insert_category(store, ADMIN, label).await;
    }
}

pub fn defaults() -> ScheduleDefaults {
    ScheduleDefaults {
        timezone: TimeZone::UTC,
        misfire_grace: PositiveDuration::try_from(SignedDuration::from_hours(2)).unwrap(),
        send_interval: PositiveDuration::try_from(SignedDuration::from_secs(3)).unwrap(),
    }
}

pub async fn insert_category(store: &Store, admin: UserId, label: &str) -> CategoryId {
    let row = categories::ActiveModel {
        admin_user_id: Set(user_id(admin).unwrap()),
        label: Set(label.to_owned()),
        position: Set(0),
        archived_at: Set(None),
        active_label: Set(Some(label.to_owned())),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
    CategoryId(row.id)
}

/// `ADMIN` 的某个活跃分类的 ID，测试里分类的名称就是 `label`。
pub async fn category_id(store: &Store, label: &str) -> CategoryId {
    let row = categories::Entity::find()
        .filter(categories::Column::AdminUserId.eq(user_id(ADMIN).unwrap()))
        .filter(categories::Column::Label.eq(label))
        .filter(categories::Column::ArchivedAt.is_null())
        .one(&store.db)
        .await
        .unwrap()
        .unwrap();
    CategoryId(row.id)
}

/// 把 `ADMIN` 的一个草稿放入其某个分类的队列。
pub async fn queue(store: &Store, post: PostId, label: &str, now: Timestamp) -> QueueOutcome {
    let category = category_id(store, label).await;
    store.queue_post(post, ADMIN, category, now).await.unwrap()
}

/// 存储 `admin` 的一个槽位。每次挑选为 `(分类名称, 数量, 补位分类名称)`。
pub async fn insert_slot(
    store: &Store,
    admin: UserId,
    time: &str,
    picks: &[(&str, u16, &[&str])],
) -> Slot {
    let id = {
        let slot = slots::ActiveModel {
            admin_user_id: Set(user_id(admin).unwrap()),
            time: Set(time.to_owned()),
            effective_from: Set(timestamp(Timestamp::UNIX_EPOCH).unwrap()),
            archived_at: Set(None),
            active_time: Set(Some(time.to_owned())),
            ..Default::default()
        }
        .insert(&store.db)
        .await
        .unwrap();
        let slot_id = slot.id;
        let lookup = |category: &str| {
            let query = categories::Entity::find()
                .filter(categories::Column::AdminUserId.eq(user_id(admin).unwrap()))
                .filter(categories::Column::Label.eq(category.to_owned()));
            async move { query.one(&store.db).await.unwrap().unwrap().id }
        };
        for (position, (category, count, fallback)) in picks.iter().enumerate() {
            let category_id = lookup(category).await;
            let row = slot_picks::ActiveModel {
                slot_id: Set(slot_id),
                category_id: Set(category_id),
                count: Set(i32::from(*count)),
                position: Set(i32::try_from(position).unwrap()),
                ..Default::default()
            }
            .insert(&store.db)
            .await
            .unwrap();
            for (position, fallback) in fallback.iter().enumerate() {
                let fallback_id = lookup(fallback).await;
                slot_pick_fallbacks::ActiveModel {
                    pick_id: Set(row.id),
                    category_id: Set(fallback_id),
                    position: Set(i32::try_from(position).unwrap()),
                }
                .insert(&store.db)
                .await
                .unwrap();
            }
        }
        slot_id
    };
    let mut slot = crate::schedule::tests::slot(
        id,
        time,
        picks
            .iter()
            .map(|(category, count, fallback)| pick(category, *count, fallback))
            .collect(),
    );
    slot.id = SlotId(id);
    slot
}

pub fn incoming(message_id: i32, text: &str) -> IncomingMessage {
    IncomingMessage {
        submitter: ADMIN,
        source_chat_id: ChatId(42),
        source_message_id: MessageId(message_id),
        media_group_id: None,
        target: None,
        channel_reply: None,
        content: Content {
            text: Some(text.to_owned()),
            entities: Vec::new(),
            link_preview_options: None,
            show_caption_above_media: false,
            media: None,
        },
    }
}

pub fn incoming_media(message_id: i32, kind: MediaKind, caption: Option<&str>) -> IncomingMessage {
    let mut message = incoming(message_id, "");
    message.content.text = caption.map(ToOwned::to_owned);
    message.content.media = Some(Media {
        kind,
        file_id: format!("file-{message_id}"),
        file_unique_id: format!("unique-{message_id}"),
        has_spoiler: false,
    });
    message
}

/// 往词表里加一个标签，分组不存在就先建。
pub async fn insert_tag(store: &Store, group: &str, name: &str) {
    use super::entities::{tag_groups, tags};

    let existing = tag_groups::Entity::find()
        .filter(tag_groups::Column::Label.eq(group))
        .one(&store.db)
        .await
        .unwrap();
    let group_id = match existing {
        Some(row) => row.id,
        None => {
            tag_groups::ActiveModel {
                label: Set(group.to_owned()),
                position: Set(0),
                archived_at: Set(None),
                active_label: Set(Some(group.to_owned())),
                ..Default::default()
            }
            .insert(&store.db)
            .await
            .unwrap()
            .id
        }
    };
    tags::ActiveModel {
        group_id: Set(group_id),
        name: Set(name.to_owned()),
        archived_at: Set(None),
        active_name: Set(Some(name.to_ascii_lowercase())),
        ..Default::default()
    }
    .insert(&store.db)
    .await
    .unwrap();
}
