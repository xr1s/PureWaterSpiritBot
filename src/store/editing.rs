// 管理员对自己分类、时段和设置的修改。
// 每个操作都带上管理员的 ID，并在事务里确认被改的对象属于他。
// 输入的格式校验由 `schedule` 里的解析函数完成。

use std::num::NonZeroU16;

use anyhow::Result;
use jiff::{Timestamp, civil::Time};
use sea_orm::{
    ColumnTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder, QuerySelect, Set,
};
use teloxide::types::UserId;

use super::{
    Conn, Store,
    codec::{timestamp, user_id},
    entities::{admins, categories, posts, slot_pick_fallbacks, slot_picks, slots},
    lock_admin,
};
use crate::{
    model::{Category, CategoryId, PickId, PostStatus, SlotId},
    schedule::format_clock,
};

/// 修改被拒绝的原因。这些都是用户能理解、也能自己处理的情况。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rejection {
    /// 对象不存在、已归档，或不属于这个管理员。
    NotFound,
    DuplicateLabel,
    DuplicateTime,
    /// 这个时段里已经有该分类的配额。
    PickExists,
    FallbackExists,
    FallbackIsOwnCategory,
    /// 分类里还有排队或正在发布的投稿。
    CategoryHasPosts {
        count: i64,
    },
    /// 分类还被这些时段（以 `HH:MM` 表示）的配额或补位使用。
    CategoryInUse {
        slot_times: Vec<String>,
    },
}

/// 一次修改的结果：完成，或者因为某个原因被拒绝（此时没有任何改动）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Applied<T> {
    Done(T),
    Rejected(Rejection),
}

/// 管理员设置里可以修改的一项。`None` 表示恢复默认（提醒时间为 `None` 表示不提醒）。
/// 值必须是 `schedule` 解析函数格式化后的文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Setting {
    Timezone(Option<String>),
    MisfireGrace(Option<String>),
    SendInterval(Option<String>),
    Reminder(Option<String>),
}
impl Store {
    pub async fn add_category(&self, admin: UserId, label: &str) -> Result<Applied<Category>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            let admin = user_id(admin)?;
            if label_taken(conn, admin, label, None).await? {
                return Ok(Applied::Rejected(Rejection::DuplicateLabel));
            }
            let last = categories::Entity::find()
                .filter(categories::Column::AdminUserId.eq(admin))
                .filter(categories::Column::ArchivedAt.is_null())
                .order_by_desc(categories::Column::Position)
                .one(conn)
                .await?;
            let inserted = categories::Entity::insert(categories::ActiveModel {
                admin_user_id: Set(admin),
                label: Set(label.to_owned()),
                active_label: Set(Some(label.to_owned())),
                position: Set(last.map_or(0, |row| row.position) + 1),
                archived_at: Set(None),
                ..Default::default()
            })
            .exec(conn)
            .await?;
            anyhow::Ok(Applied::Done(Category {
                id: CategoryId(inserted.last_insert_id),
                label: label.to_owned(),
            }))
        })
        .await
    }

    pub async fn rename_category(
        &self,
        admin: UserId,
        category: CategoryId,
        label: &str,
    ) -> Result<Applied<()>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            let admin = user_id(admin)?;
            if !owns_category(conn, admin, category.0).await? {
                return Ok(Applied::Rejected(Rejection::NotFound));
            }
            if label_taken(conn, admin, label, Some(category.0)).await? {
                return Ok(Applied::Rejected(Rejection::DuplicateLabel));
            }
            categories::Entity::update_many()
                .filter(categories::Column::Id.eq(category.0))
                .col_expr(
                    categories::Column::Label,
                    sea_orm::sea_query::Expr::value(label),
                )
                .col_expr(
                    categories::Column::ActiveLabel,
                    sea_orm::sea_query::Expr::value(label),
                )
                .exec(conn)
                .await?;
            anyhow::Ok(Applied::Done(()))
        })
        .await
    }

    /// 把分类在显示顺序里往前或往后挪一位。已经在头或尾时什么也不做。
    pub async fn move_category(
        &self,
        admin: UserId,
        category: CategoryId,
        up: bool,
    ) -> Result<Applied<()>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            let admin = user_id(admin)?;
            let mut ids: Vec<i64> = categories::Entity::find()
                .filter(categories::Column::AdminUserId.eq(admin))
                .filter(categories::Column::ArchivedAt.is_null())
                .order_by_asc(categories::Column::Position)
                .order_by_asc(categories::Column::Id)
                .select_only()
                .column(categories::Column::Id)
                .into_tuple()
                .all(conn)
                .await?;
            let Some(index) = ids.iter().position(|id| *id == category.0) else {
                return Ok(Applied::Rejected(Rejection::NotFound));
            };
            let neighbor = if up {
                index.checked_sub(1)
            } else {
                Some(index + 1)
            };
            if let Some(neighbor) = neighbor.filter(|neighbor| *neighbor < ids.len()) {
                ids.swap(index, neighbor);
            }
            // 顺便把 position 重排成连续的整数，数据库里手工导入的数据可能全是同一个值。
            for (position, id) in ids.iter().enumerate() {
                categories::Entity::update_many()
                    .filter(categories::Column::Id.eq(*id))
                    .col_expr(
                        categories::Column::Position,
                        sea_orm::sea_query::Expr::value(i32::try_from(position)?),
                    )
                    .exec(conn)
                    .await?;
            }
            anyhow::Ok(Applied::Done(()))
        })
        .await
    }

    /// 归档分类。分类里还有排队的投稿，或者还被时段使用时会被拒绝。
    pub async fn archive_category(
        &self,
        admin: UserId,
        category: CategoryId,
        now: Timestamp,
    ) -> Result<Applied<()>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            let admin = user_id(admin)?;
            if !owns_category(conn, admin, category.0).await? {
                return Ok(Applied::Rejected(Rejection::NotFound));
            }
            let count = posts::Entity::find()
                .filter(posts::Column::CategoryId.eq(category.0))
                .filter(posts::Column::Status.is_in([PostStatus::Queued, PostStatus::Reserved]))
                .count(conn)
                .await?;
            if count > 0 {
                return Ok(Applied::Rejected(Rejection::CategoryHasPosts {
                    count: i64::try_from(count)?,
                }));
            }
            let slot_times = slot_times_using(conn, admin, category.0).await?;
            if !slot_times.is_empty() {
                return Ok(Applied::Rejected(Rejection::CategoryInUse { slot_times }));
            }
            categories::Entity::update_many()
                .filter(categories::Column::Id.eq(category.0))
                .col_expr(
                    categories::Column::ArchivedAt,
                    sea_orm::sea_query::Expr::value(timestamp(now)?),
                )
                .col_expr(
                    categories::Column::ActiveLabel,
                    sea_orm::sea_query::Expr::value(None::<String>),
                )
                .exec(conn)
                .await?;
            anyhow::Ok(Applied::Done(()))
        })
        .await
    }

    /// 新建一个没有配额的时段。它从现在起生效，不会补发今天已经过去的时刻。
    pub async fn add_slot(
        &self,
        admin: UserId,
        time: Time,
        now: Timestamp,
    ) -> Result<Applied<SlotId>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            let admin = user_id(admin)?;
            let time = format_clock(time);
            if time_taken(conn, admin, &time, None).await? {
                return Ok(Applied::Rejected(Rejection::DuplicateTime));
            }
            let inserted = slots::Entity::insert(slots::ActiveModel {
                admin_user_id: Set(admin),
                time: Set(time.clone()),
                active_time: Set(Some(time)),
                effective_from: Set(timestamp(now)?),
                archived_at: Set(None),
                ..Default::default()
            })
            .exec(conn)
            .await?;
            anyhow::Ok(Applied::Done(SlotId(inserted.last_insert_id)))
        })
        .await
    }

    /// 修改时段的时刻，并让它从现在起重新生效。
    pub async fn set_slot_time(
        &self,
        admin: UserId,
        slot: SlotId,
        time: Time,
        now: Timestamp,
    ) -> Result<Applied<()>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            let admin = user_id(admin)?;
            let time = format_clock(time);
            if !owns_slot(conn, admin, slot.0).await? {
                return Ok(Applied::Rejected(Rejection::NotFound));
            }
            if time_taken(conn, admin, &time, Some(slot.0)).await? {
                return Ok(Applied::Rejected(Rejection::DuplicateTime));
            }
            slots::Entity::update_many()
                .filter(slots::Column::Id.eq(slot.0))
                .col_expr(
                    slots::Column::Time,
                    sea_orm::sea_query::Expr::value(time.clone()),
                )
                .col_expr(
                    slots::Column::ActiveTime,
                    sea_orm::sea_query::Expr::value(time),
                )
                .col_expr(
                    slots::Column::EffectiveFrom,
                    sea_orm::sea_query::Expr::value(timestamp(now)?),
                )
                .exec(conn)
                .await?;
            anyhow::Ok(Applied::Done(()))
        })
        .await
    }

    /// 删除（归档）时段。已经发生过的运行记录仍然保留。
    pub async fn archive_slot(
        &self,
        admin: UserId,
        slot: SlotId,
        now: Timestamp,
    ) -> Result<Applied<()>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            if !owns_slot(conn, user_id(admin)?, slot.0).await? {
                return Ok(Applied::Rejected(Rejection::NotFound));
            }
            slots::Entity::update_many()
                .filter(slots::Column::Id.eq(slot.0))
                .col_expr(
                    slots::Column::ArchivedAt,
                    sea_orm::sea_query::Expr::value(timestamp(now)?),
                )
                .col_expr(
                    slots::Column::ActiveTime,
                    sea_orm::sea_query::Expr::value(None::<String>),
                )
                .exec(conn)
                .await?;
            anyhow::Ok(Applied::Done(()))
        })
        .await
    }

    pub async fn add_pick(
        &self,
        admin: UserId,
        slot: SlotId,
        category: CategoryId,
        count: NonZeroU16,
    ) -> Result<Applied<PickId>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            let admin = user_id(admin)?;
            if !owns_slot(conn, admin, slot.0).await?
                || !owns_category(conn, admin, category.0).await?
            {
                return Ok(Applied::Rejected(Rejection::NotFound));
            }
            let existing = slot_picks::Entity::find()
                .filter(slot_picks::Column::SlotId.eq(slot.0))
                .filter(slot_picks::Column::CategoryId.eq(category.0))
                .count(conn)
                .await?;
            if existing > 0 {
                return Ok(Applied::Rejected(Rejection::PickExists));
            }
            let last = slot_picks::Entity::find()
                .filter(slot_picks::Column::SlotId.eq(slot.0))
                .order_by_desc(slot_picks::Column::Position)
                .one(conn)
                .await?;
            let inserted = slot_picks::Entity::insert(slot_picks::ActiveModel {
                slot_id: Set(slot.0),
                category_id: Set(category.0),
                count: Set(i32::from(count.get())),
                position: Set(last.map_or(0, |row| row.position) + 1),
                ..Default::default()
            })
            .exec(conn)
            .await?;
            anyhow::Ok(Applied::Done(PickId(inserted.last_insert_id)))
        })
        .await
    }

    pub async fn set_pick_count(
        &self,
        admin: UserId,
        pick: PickId,
        count: NonZeroU16,
    ) -> Result<Applied<()>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            if owned_pick(conn, user_id(admin)?, pick.0).await?.is_none() {
                return Ok(Applied::Rejected(Rejection::NotFound));
            }
            slot_picks::Entity::update_many()
                .filter(slot_picks::Column::Id.eq(pick.0))
                .col_expr(
                    slot_picks::Column::Count,
                    sea_orm::sea_query::Expr::value(i32::from(count.get())),
                )
                .exec(conn)
                .await?;
            anyhow::Ok(Applied::Done(()))
        })
        .await
    }

    pub async fn delete_pick(&self, admin: UserId, pick: PickId) -> Result<Applied<()>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            if owned_pick(conn, user_id(admin)?, pick.0).await?.is_none() {
                return Ok(Applied::Rejected(Rejection::NotFound));
            }
            slot_pick_fallbacks::Entity::delete_many()
                .filter(slot_pick_fallbacks::Column::PickId.eq(pick.0))
                .exec(conn)
                .await?;
            slot_picks::Entity::delete_by_id(pick.0).exec(conn).await?;
            anyhow::Ok(Applied::Done(()))
        })
        .await
    }

    /// 在配额的补位列表末尾追加一个分类。
    pub async fn add_fallback(
        &self,
        admin: UserId,
        pick: PickId,
        category: CategoryId,
    ) -> Result<Applied<()>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            let admin = user_id(admin)?;
            let Some((_, own_category)) = owned_pick(conn, admin, pick.0).await? else {
                return Ok(Applied::Rejected(Rejection::NotFound));
            };
            if !owns_category(conn, admin, category.0).await? {
                return Ok(Applied::Rejected(Rejection::NotFound));
            }
            if own_category == category.0 {
                return Ok(Applied::Rejected(Rejection::FallbackIsOwnCategory));
            }
            let existing = slot_pick_fallbacks::Entity::find()
                .filter(slot_pick_fallbacks::Column::PickId.eq(pick.0))
                .filter(slot_pick_fallbacks::Column::CategoryId.eq(category.0))
                .count(conn)
                .await?;
            if existing > 0 {
                return Ok(Applied::Rejected(Rejection::FallbackExists));
            }
            let last = slot_pick_fallbacks::Entity::find()
                .filter(slot_pick_fallbacks::Column::PickId.eq(pick.0))
                .order_by_desc(slot_pick_fallbacks::Column::Position)
                .one(conn)
                .await?;
            slot_pick_fallbacks::Entity::insert(slot_pick_fallbacks::ActiveModel {
                pick_id: Set(pick.0),
                category_id: Set(category.0),
                position: Set(last.map_or(0, |row| row.position) + 1),
            })
            .exec_without_returning(conn)
            .await?;
            anyhow::Ok(Applied::Done(()))
        })
        .await
    }

    pub async fn remove_fallback(
        &self,
        admin: UserId,
        pick: PickId,
        category: CategoryId,
    ) -> Result<Applied<()>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            if owned_pick(conn, user_id(admin)?, pick.0).await?.is_none() {
                return Ok(Applied::Rejected(Rejection::NotFound));
            }
            slot_pick_fallbacks::Entity::delete_many()
                .filter(slot_pick_fallbacks::Column::PickId.eq(pick.0))
                .filter(slot_pick_fallbacks::Column::CategoryId.eq(category.0))
                .exec(conn)
                .await?;
            anyhow::Ok(Applied::Done(()))
        })
        .await
    }

    /// 修改管理员的一项设置。修改时区会让他所有的时段从现在起重新生效，
    /// 否则换了时区之后，同一个时段可能在同一天里按新的本地日期再运行一次。
    pub async fn update_setting(
        &self,
        admin: UserId,
        setting: Setting,
        now: Timestamp,
    ) -> Result<()> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            let admin = user_id(admin)?;
            let mut target = admins::ActiveModel {
                user_id: Set(admin),
                ..Default::default()
            };
            match &setting {
                Setting::Timezone(value) => {
                    target.timezone = Set(value.clone());
                    slots::Entity::update_many()
                        .filter(slots::Column::AdminUserId.eq(admin))
                        .filter(slots::Column::ArchivedAt.is_null())
                        .col_expr(
                            slots::Column::EffectiveFrom,
                            sea_orm::sea_query::Expr::value(timestamp(now)?),
                        )
                        .exec(conn)
                        .await?;
                }
                Setting::MisfireGrace(value) => {
                    target.misfire_grace = Set(value.clone());
                }
                Setting::SendInterval(value) => {
                    target.send_interval = Set(value.clone());
                }
                Setting::Reminder(value) => {
                    target.reminder_time = Set(value.clone());
                }
            }
            admins::Entity::update_many()
                .filter(admins::Column::UserId.eq(admin))
                .set(target)
                .exec(conn)
                .await?;
            anyhow::Ok(())
        })
        .await
    }
}

async fn owns_category(conn: &Conn, admin: i64, category: i64) -> Result<bool> {
    let count = categories::Entity::find()
        .filter(categories::Column::Id.eq(category))
        .filter(categories::Column::AdminUserId.eq(admin))
        .filter(categories::Column::ArchivedAt.is_null())
        .count(conn)
        .await?;
    Ok(count > 0)
}

async fn owns_slot(conn: &Conn, admin: i64, slot: i64) -> Result<bool> {
    let count = slots::Entity::find()
        .filter(slots::Column::Id.eq(slot))
        .filter(slots::Column::AdminUserId.eq(admin))
        .filter(slots::Column::ArchivedAt.is_null())
        .count(conn)
        .await?;
    Ok(count > 0)
}

/// 属于该管理员、且所在时段未归档的配额，返回它的时段和分类。
async fn owned_pick(conn: &Conn, admin: i64, pick: i64) -> Result<Option<(i64, i64)>> {
    let Some(pick) = slot_picks::Entity::find_by_id(pick).one(conn).await? else {
        return Ok(None);
    };
    if owns_slot(conn, admin, pick.slot_id).await? {
        Ok(Some((pick.slot_id, pick.category_id)))
    } else {
        Ok(None)
    }
}

/// 管理员的未归档分类里是否已有这个名称，`except` 是要排除的分类（改名时排除自己）。
async fn label_taken(conn: &Conn, admin: i64, label: &str, except: Option<i64>) -> Result<bool> {
    let count = categories::Entity::find()
        .filter(categories::Column::AdminUserId.eq(admin))
        .filter(categories::Column::ArchivedAt.is_null())
        .filter(categories::Column::Label.eq(label))
        .filter(categories::Column::Id.ne(except.unwrap_or(-1)))
        .count(conn)
        .await?;
    Ok(count > 0)
}

async fn time_taken(conn: &Conn, admin: i64, time: &str, except: Option<i64>) -> Result<bool> {
    let count = slots::Entity::find()
        .filter(slots::Column::AdminUserId.eq(admin))
        .filter(slots::Column::ArchivedAt.is_null())
        .filter(slots::Column::Time.eq(time))
        .filter(slots::Column::Id.ne(except.unwrap_or(-1)))
        .count(conn)
        .await?;
    Ok(count > 0)
}

/// 还在使用某个分类（作为配额或补位）的未归档时段的时刻，按时间排序去重。
async fn slot_times_using(conn: &Conn, admin: i64, category: i64) -> Result<Vec<String>> {
    let fallback_picks: Vec<i64> = slot_pick_fallbacks::Entity::find()
        .filter(slot_pick_fallbacks::Column::CategoryId.eq(category))
        .select_only()
        .column(slot_pick_fallbacks::Column::PickId)
        .into_tuple()
        .all(conn)
        .await?;
    let slot_ids: Vec<i64> = slot_picks::Entity::find()
        .filter(
            sea_orm::Condition::any()
                .add(slot_picks::Column::CategoryId.eq(category))
                .add(slot_picks::Column::Id.is_in(fallback_picks)),
        )
        .select_only()
        .column(slot_picks::Column::SlotId)
        .into_tuple()
        .all(conn)
        .await?;
    let mut times: Vec<String> = slots::Entity::find()
        .filter(slots::Column::Id.is_in(slot_ids))
        .filter(slots::Column::AdminUserId.eq(admin))
        .filter(slots::Column::ArchivedAt.is_null())
        .select_only()
        .column(slots::Column::Time)
        .into_tuple()
        .all(conn)
        .await?;
    times.sort();
    times.dedup();
    Ok(times)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::{
        schedule::{ScheduleDefaults, parse_clock},
        store::test_support::{ADMIN, category_id, defaults, incoming, insert_category, queue},
    };

    const NOW: Timestamp = Timestamp::UNIX_EPOCH;

    fn clock(text: &str) -> Time {
        parse_clock(text).unwrap()
    }

    fn count(value: u16) -> NonZeroU16 {
        NonZeroU16::new(value).unwrap()
    }

    fn done<T: std::fmt::Debug>(applied: Applied<T>) -> T {
        match applied {
            Applied::Done(value) => value,
            Applied::Rejected(reason) => panic!("rejected: {reason:?}"),
        }
    }

    fn rejected<T: std::fmt::Debug>(applied: Applied<T>) -> Rejection {
        match applied {
            Applied::Rejected(reason) => reason,
            Applied::Done(value) => panic!("unexpectedly done: {value:?}"),
        }
    }

    async fn schedule(store: &Store, admin: UserId) -> crate::schedule::AdminSchedule {
        let defaults: ScheduleDefaults = defaults();
        store
            .load_schedule(admin, &defaults)
            .await
            .unwrap()
            .unwrap()
    }

    async fn other_admin(store: &Store) -> UserId {
        let other = UserId(7);
        store.sync_admins(&[ADMIN, other], NOW).await.unwrap();
        other
    }

    #[tokio::test]
    async fn new_categories_are_ordered_last() {
        let (_directory, store) = Store::open_temporary().await;
        let first = done(store.add_category(ADMIN, "新分类").await.unwrap());
        let second = done(store.add_category(ADMIN, "另一个").await.unwrap());
        assert_ne!(first.id, second.id);

        let labels: Vec<_> = store
            .categories(ADMIN)
            .await
            .unwrap()
            .into_iter()
            .map(|category| category.label)
            .collect();
        assert_eq!(labels[labels.len() - 2..], ["新分类", "另一个"]);
    }

    #[tokio::test]
    async fn category_labels_are_unique_per_admin() {
        let (_directory, store) = Store::open_temporary().await;
        assert_eq!(
            rejected(store.add_category(ADMIN, "gi").await.unwrap()),
            Rejection::DuplicateLabel
        );
        let other = other_admin(&store).await;
        done(store.add_category(other, "gi").await.unwrap());

        let gi = category_id(&store, "gi").await;
        assert_eq!(
            rejected(store.rename_category(ADMIN, gi, "hsr").await.unwrap()),
            Rejection::DuplicateLabel
        );
        // 改成自己当前的名称不算重复。
        done(store.rename_category(ADMIN, gi, "gi").await.unwrap());
        done(store.rename_category(ADMIN, gi, "原神").await.unwrap());
        assert_eq!(store.categories(ADMIN).await.unwrap()[0].label, "原神");
        let renamed = categories::Entity::find_by_id(gi.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(renamed.active_label.as_deref(), Some("原神"));
        done(store.add_category(ADMIN, "gi").await.unwrap());
    }

    #[tokio::test]
    async fn admins_cannot_touch_each_others_categories() {
        let (_directory, store) = Store::open_temporary().await;
        let other = other_admin(&store).await;
        let gi = category_id(&store, "gi").await;
        assert_eq!(
            rejected(store.rename_category(other, gi, "x").await.unwrap()),
            Rejection::NotFound
        );
        assert_eq!(
            rejected(store.move_category(other, gi, false).await.unwrap()),
            Rejection::NotFound
        );
        assert_eq!(
            rejected(store.archive_category(other, gi, NOW).await.unwrap()),
            Rejection::NotFound
        );
    }

    #[tokio::test]
    async fn moves_categories_up_and_down() {
        let (_directory, store) = Store::open_temporary().await;
        let order = |categories: Vec<Category>| {
            categories
                .into_iter()
                .map(|category| category.label)
                .collect::<Vec<_>>()
        };
        // 测试数据的 position 全是默认值 0，移动时会先重排。
        let hsr = category_id(&store, "hsr").await;
        done(store.move_category(ADMIN, hsr, true).await.unwrap());
        assert_eq!(
            order(store.categories(ADMIN).await.unwrap()),
            ["hsr", "gi", "other", "misc"]
        );
        // 已经在头部：不变。
        done(store.move_category(ADMIN, hsr, true).await.unwrap());
        done(store.move_category(ADMIN, hsr, false).await.unwrap());
        done(store.move_category(ADMIN, hsr, false).await.unwrap());
        assert_eq!(
            order(store.categories(ADMIN).await.unwrap()),
            ["gi", "other", "hsr", "misc"]
        );
        let misc = category_id(&store, "misc").await;
        done(store.move_category(ADMIN, misc, false).await.unwrap());
        assert_eq!(
            order(store.categories(ADMIN).await.unwrap()),
            ["gi", "other", "hsr", "misc"]
        );
    }

    #[tokio::test]
    async fn archiving_needs_an_unused_category_without_queued_posts() {
        let (_directory, store) = Store::open_temporary().await;
        let gi = category_id(&store, "gi").await;
        let hsr = category_id(&store, "hsr").await;
        let slot = done(store.add_slot(ADMIN, clock("10:00"), NOW).await.unwrap());
        let pick = done(store.add_pick(ADMIN, slot, gi, count(1)).await.unwrap());
        done(store.add_fallback(ADMIN, pick, hsr).await.unwrap());

        let in_use = |times: &[&str]| Rejection::CategoryInUse {
            slot_times: times.iter().map(ToString::to_string).collect(),
        };
        assert_eq!(
            rejected(store.archive_category(ADMIN, gi, NOW).await.unwrap()),
            in_use(&["10:00"])
        );
        assert_eq!(
            rejected(store.archive_category(ADMIN, hsr, NOW).await.unwrap()),
            in_use(&["10:00"])
        );

        let draft = store
            .create_draft(&[incoming(1, "a")], NOW, None)
            .await
            .unwrap()
            .unwrap();
        queue(&store, draft, "other", NOW).await;
        let other = category_id(&store, "other").await;
        assert_eq!(
            rejected(store.archive_category(ADMIN, other, NOW).await.unwrap()),
            Rejection::CategoryHasPosts { count: 1 }
        );

        let misc = category_id(&store, "misc").await;
        done(store.archive_category(ADMIN, misc, NOW).await.unwrap());
        assert_eq!(store.categories(ADMIN).await.unwrap().len(), 3);
        let archived = categories::Entity::find_by_id(misc.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archived.archived_at, Some(timestamp(NOW).unwrap()));
        assert_eq!(archived.active_label, None);
        // 归档后名称和 key 都可以重新使用。
        done(store.add_category(ADMIN, "misc").await.unwrap());

        done(store.delete_pick(ADMIN, pick).await.unwrap());
        done(store.archive_category(ADMIN, gi, NOW).await.unwrap());
        done(store.archive_category(ADMIN, hsr, NOW).await.unwrap());
    }

    #[tokio::test]
    async fn slot_times_are_unique_and_edits_restart_the_slot() {
        let (_directory, store) = Store::open_temporary().await;
        let slot = done(store.add_slot(ADMIN, clock("10:00"), NOW).await.unwrap());
        assert_eq!(
            rejected(store.add_slot(ADMIN, clock("10:00"), NOW).await.unwrap()),
            Rejection::DuplicateTime
        );
        let second = done(store.add_slot(ADMIN, clock("9:00"), NOW).await.unwrap());
        assert_eq!(
            rejected(
                store
                    .set_slot_time(ADMIN, second, clock("10:00"), NOW)
                    .await
                    .unwrap()
            ),
            Rejection::DuplicateTime
        );

        let later = NOW + jiff::SignedDuration::from_hours(5);
        done(
            store
                .set_slot_time(ADMIN, slot, clock("11:30"), later)
                .await
                .unwrap(),
        );
        let loaded = schedule(&store, ADMIN).await;
        // 按时刻排序，9:00 在前。
        let times: Vec<_> = loaded
            .slots
            .iter()
            .map(|slot| format_clock(slot.time))
            .collect();
        assert_eq!(times, ["09:00", "11:30"]);
        assert_eq!(loaded.slots[1].effective_from, later);
        assert_eq!(loaded.slots[0].effective_from, NOW);
        assert!(loaded.slots.iter().all(|slot| slot.picks.is_empty()));
        let edited = slots::Entity::find_by_id(slot.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(edited.active_time.as_deref(), Some("11:30"));
        let replacement = done(store.add_slot(ADMIN, clock("10:00"), later).await.unwrap());
        assert_eq!(schedule(&store, ADMIN).await.slots.len(), 3);
        done(store.archive_slot(ADMIN, replacement, later).await.unwrap());

        done(store.archive_slot(ADMIN, slot, later).await.unwrap());
        assert_eq!(schedule(&store, ADMIN).await.slots.len(), 1);
        let archived = slots::Entity::find_by_id(slot.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(archived.archived_at, Some(timestamp(later).unwrap()));
        assert_eq!(archived.active_time, None);
        // 归档后这个时刻可以重新使用。
        done(store.add_slot(ADMIN, clock("11:30"), later).await.unwrap());
        assert_eq!(
            rejected(store.archive_slot(ADMIN, slot, later).await.unwrap()),
            Rejection::NotFound
        );
    }

    #[tokio::test]
    async fn picks_and_fallbacks_follow_the_rules() {
        let (_directory, store) = Store::open_temporary().await;
        let gi = category_id(&store, "gi").await;
        let hsr = category_id(&store, "hsr").await;
        let other = category_id(&store, "other").await;
        let slot = done(store.add_slot(ADMIN, clock("10:00"), NOW).await.unwrap());

        let pick = done(store.add_pick(ADMIN, slot, gi, count(2)).await.unwrap());
        assert_eq!(
            rejected(store.add_pick(ADMIN, slot, gi, count(1)).await.unwrap()),
            Rejection::PickExists
        );
        assert_eq!(
            rejected(store.add_fallback(ADMIN, pick, gi).await.unwrap()),
            Rejection::FallbackIsOwnCategory
        );
        done(store.add_fallback(ADMIN, pick, other).await.unwrap());
        done(store.add_fallback(ADMIN, pick, hsr).await.unwrap());
        assert_eq!(
            rejected(store.add_fallback(ADMIN, pick, hsr).await.unwrap()),
            Rejection::FallbackExists
        );
        done(store.set_pick_count(ADMIN, pick, count(5)).await.unwrap());

        let loaded = schedule(&store, ADMIN).await;
        let loaded_pick = &loaded.slots[0].picks[0];
        assert_eq!(loaded_pick.id, pick);
        assert_eq!(loaded_pick.count.get(), 5);
        assert_eq!(loaded_pick.fallback, [other, hsr]);

        // 去掉中间一个再追加，顺序按追加先后排。
        done(store.remove_fallback(ADMIN, pick, other).await.unwrap());
        done(store.add_fallback(ADMIN, pick, other).await.unwrap());
        let loaded = schedule(&store, ADMIN).await;
        assert_eq!(loaded.slots[0].picks[0].fallback, [hsr, other]);

        done(store.delete_pick(ADMIN, pick).await.unwrap());
        assert!(schedule(&store, ADMIN).await.slots[0].picks.is_empty());
        assert_eq!(
            rejected(store.delete_pick(ADMIN, pick).await.unwrap()),
            Rejection::NotFound
        );
    }

    #[tokio::test]
    async fn admins_cannot_edit_each_others_slots() {
        let (_directory, store) = Store::open_temporary().await;
        let other = other_admin(&store).await;
        let foreign = insert_category(&store, other, "x").await;
        let gi = category_id(&store, "gi").await;
        let slot = done(store.add_slot(ADMIN, clock("10:00"), NOW).await.unwrap());
        let pick = done(store.add_pick(ADMIN, slot, gi, count(1)).await.unwrap());

        assert_eq!(
            rejected(
                store
                    .set_slot_time(other, slot, clock("11:00"), NOW)
                    .await
                    .unwrap()
            ),
            Rejection::NotFound
        );
        assert_eq!(
            rejected(store.archive_slot(other, slot, NOW).await.unwrap()),
            Rejection::NotFound
        );
        assert_eq!(
            rejected(
                store
                    .add_pick(other, slot, foreign, count(1))
                    .await
                    .unwrap()
            ),
            Rejection::NotFound
        );
        assert_eq!(
            rejected(store.set_pick_count(other, pick, count(3)).await.unwrap()),
            Rejection::NotFound
        );
        assert_eq!(
            rejected(store.delete_pick(other, pick).await.unwrap()),
            Rejection::NotFound
        );
        assert_eq!(
            rejected(store.add_fallback(other, pick, foreign).await.unwrap()),
            Rejection::NotFound
        );
        // 自己的时段也不能引用别人的分类。
        assert_eq!(
            rejected(
                store
                    .add_pick(ADMIN, slot, foreign, count(1))
                    .await
                    .unwrap()
            ),
            Rejection::NotFound
        );
        // 两个管理员可以在同一时刻各有一个时段。
        done(store.add_slot(other, clock("10:00"), NOW).await.unwrap());
    }

    #[tokio::test]
    async fn settings_can_be_changed_and_reset() {
        let (_directory, store) = Store::open_temporary().await;
        let set = |setting| store.update_setting(ADMIN, setting, NOW);
        set(Setting::MisfireGrace(Some("30m".to_owned())))
            .await
            .unwrap();
        set(Setting::SendInterval(Some("5s".to_owned())))
            .await
            .unwrap();
        set(Setting::Reminder(Some("20:00".to_owned())))
            .await
            .unwrap();
        set(Setting::Timezone(Some("Asia/Shanghai".to_owned())))
            .await
            .unwrap();

        let loaded = schedule(&store, ADMIN).await;
        assert_eq!(loaded.misfire_grace.get().as_secs(), 30 * 60);
        assert_eq!(loaded.send_interval.get().as_secs(), 5);
        assert_eq!(loaded.reminder, Some(clock("20:00")));
        assert_eq!(loaded.timezone.iana_name(), Some("Asia/Shanghai"));

        set(Setting::MisfireGrace(None)).await.unwrap();
        set(Setting::SendInterval(None)).await.unwrap();
        set(Setting::Reminder(None)).await.unwrap();
        set(Setting::Timezone(None)).await.unwrap();
        let loaded = schedule(&store, ADMIN).await;
        assert_eq!(loaded.misfire_grace.get(), defaults().misfire_grace.get());
        assert_eq!(loaded.send_interval.get(), defaults().send_interval.get());
        assert_eq!(loaded.reminder, None);
        assert_eq!(loaded.timezone.iana_name(), defaults().timezone.iana_name());
    }

    #[tokio::test]
    async fn changing_the_timezone_restarts_all_slots() {
        let (_directory, store) = Store::open_temporary().await;
        done(store.add_slot(ADMIN, clock("10:00"), NOW).await.unwrap());
        done(store.add_slot(ADMIN, clock("12:00"), NOW).await.unwrap());
        let later = NOW + jiff::SignedDuration::from_hours(3);
        store
            .update_setting(
                ADMIN,
                Setting::Timezone(Some("Asia/Tokyo".to_owned())),
                later,
            )
            .await
            .unwrap();
        let loaded = schedule(&store, ADMIN).await;
        assert!(loaded.slots.iter().all(|slot| slot.effective_from == later));

        // 其他设置不影响生效时间。
        let even_later = later + jiff::SignedDuration::from_hours(1);
        store
            .update_setting(
                ADMIN,
                Setting::Reminder(Some("20:00".to_owned())),
                even_later,
            )
            .await
            .unwrap();
        let loaded = schedule(&store, ADMIN).await;
        assert!(loaded.slots.iter().all(|slot| slot.effective_from == later));
    }
}
