use std::{collections::HashMap, num::NonZeroU16, str::FromStr};

use anyhow::{Context, Result, anyhow};
use jiff::{SignedDuration, Timestamp, civil::Time, tz::TimeZone};
use sea_orm::{
    ColumnTrait, Condition, ConnectionTrait, EntityTrait, QueryFilter, QueryOrder, Set,
    sea_query::OnConflict,
};
use teloxide::types::UserId;

use super::{
    Store,
    codec::{decode_timestamp, decode_user_id, timestamp, user_id},
    entities::{admins, categories, slot_pick_fallbacks, slot_picks, slots},
};
use crate::{
    model::{Category, CategoryId, Pick, PickId, PositiveDuration, Slot, SlotId},
    schedule::{AdminSchedule, ScheduleDefaults},
};

/// 应用配置后，管理员列表发生的变化。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct AdminSync {
    pub deactivated: Vec<UserId>,
}

impl Store {
    /// 让 `configured` 中的管理员变为活跃，其他所有人变为停用。停用的管理员
    /// 保留其行，因为帖子和发布历史会引用他们。
    pub async fn sync_admins(&self, configured: &[UserId], now: Timestamp) -> Result<AdminSync> {
        let ids = configured
            .iter()
            .copied()
            .map(user_id)
            .collect::<Result<Vec<_>>>()?;
        let now = timestamp(now)?;
        self.transaction(async move |conn| {
            if !ids.is_empty() {
                let rows = ids
                    .iter()
                    .copied()
                    .map(|id| admins::ActiveModel {
                        user_id: Set(id),
                        active: Set(true),
                        created_at: Set(now),
                        timezone: Set(None),
                        misfire_grace: Set(None),
                        send_interval: Set(None),
                        reminder_time: Set(None),
                    })
                    .collect::<Vec<_>>();
                admins::Entity::insert_many(rows)
                    .on_conflict(
                        OnConflict::column(admins::Column::UserId)
                            .update_column(admins::Column::Active)
                            .to_owned(),
                    )
                    .exec_without_returning(conn)
                    .await?;
            }
            let target = Condition::all()
                .add(admins::Column::Active.eq(true))
                .add(admins::Column::UserId.is_not_in(ids));
            let deactivated = admins::Entity::find()
                .filter(target.clone())
                .order_by_asc(admins::Column::UserId)
                .all(conn)
                .await?;
            admins::Entity::update_many()
                .set(admins::ActiveModel {
                    active: Set(false),
                    ..Default::default()
                })
                .filter(target)
                .exec(conn)
                .await?;
            anyhow::Ok(AdminSync {
                deactivated: deactivated
                    .into_iter()
                    .map(|row| decode_user_id(row.user_id))
                    .collect::<Result<_>>()?,
            })
        })
        .await
    }

    /// 管理员当前可以入队帖子的分类，按显示顺序排列。
    pub async fn categories(&self, admin: UserId) -> Result<Vec<Category>> {
        active_categories(&self.db, admin).await
    }

    /// 所有活跃管理员的计划。某个计划损坏不会影响其他计划。
    pub async fn load_schedules(
        &self,
        defaults: &ScheduleDefaults,
    ) -> Result<Vec<(UserId, Result<AdminSchedule>)>> {
        let rows = admin_rows(&self.db, None).await?;
        let mut loaded = Vec::with_capacity(rows.len());
        for row in rows {
            let admin = decode_user_id(row.user_id)?;
            let schedule = load_schedule(&self.db, row, defaults)
                .await
                .with_context(|| format!("invalid schedule of admin {}", admin.0));
            loaded.push((admin, schedule));
        }
        Ok(loaded)
    }

    /// 某位活跃管理员的计划；如果不是活跃管理员则为 `None`。
    pub async fn load_schedule(
        &self,
        admin: UserId,
        defaults: &ScheduleDefaults,
    ) -> Result<Option<AdminSchedule>> {
        let row = admin_rows(&self.db, Some(user_id(admin)?))
            .await?
            .into_iter()
            .next();
        match row {
            Some(row) => Ok(Some(
                load_schedule(&self.db, row, defaults)
                    .await
                    .with_context(|| format!("invalid schedule of admin {}", admin.0))?,
            )),
            None => Ok(None),
        }
    }
}

type AdminRow = admins::Model;

async fn admin_rows(conn: &impl ConnectionTrait, only: Option<i64>) -> Result<Vec<AdminRow>> {
    let mut query = admins::Entity::find()
        .filter(admins::Column::Active.eq(true))
        .order_by_asc(admins::Column::UserId);
    if let Some(id) = only {
        query = query.filter(admins::Column::UserId.eq(id));
    }
    Ok(query.all(conn).await?)
}

async fn active_categories(conn: &impl ConnectionTrait, admin: UserId) -> Result<Vec<Category>> {
    let rows = categories::Entity::find()
        .filter(categories::Column::AdminUserId.eq(user_id(admin)?))
        .filter(categories::Column::ArchivedAt.is_null())
        .order_by_asc(categories::Column::Position)
        .order_by_asc(categories::Column::Id)
        .all(conn)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| Category {
            id: CategoryId(row.id),
            label: row.label,
        })
        .collect())
}

async fn load_schedule(
    conn: &impl ConnectionTrait,
    row: AdminRow,
    defaults: &ScheduleDefaults,
) -> Result<AdminSchedule> {
    let admins::Model {
        user_id,
        timezone,
        misfire_grace,
        send_interval,
        reminder_time: reminder,
        ..
    } = row;
    let timezone = match timezone {
        Some(name) => TimeZone::get(&name).with_context(|| format!("invalid timezone {name:?}"))?,
        None => defaults.timezone.clone(),
    };
    let duration = |value: Option<String>, name: &str, default: PositiveDuration| {
        value.map_or(Ok(default), |text| {
            let parsed = SignedDuration::from_str(&text)
                .with_context(|| format!("invalid {name} {text:?}"))?;
            PositiveDuration::try_from(parsed).with_context(|| format!("invalid {name} {text:?}"))
        })
    };
    let reminder = reminder
        .map(|text| {
            Time::from_str(&text).with_context(|| format!("invalid reminder_time {text:?}"))
        })
        .transpose()?;

    let categories = active_categories(conn, decode_user_id(user_id)?).await?;
    let schedule = AdminSchedule {
        admin: decode_user_id(user_id)?,
        timezone,
        misfire_grace: duration(misfire_grace, "misfire_grace", defaults.misfire_grace)?,
        send_interval: duration(send_interval, "send_interval", defaults.send_interval)?,
        reminder,
        categories,
        slots: load_slots(conn, user_id).await?,
    };
    schedule.validate()?;
    Ok(schedule)
}

async fn load_slots(conn: &impl ConnectionTrait, admin: i64) -> Result<Vec<Slot>> {
    let slot_rows = slots::Entity::find()
        .filter(slots::Column::AdminUserId.eq(admin))
        .filter(slots::Column::ArchivedAt.is_null())
        .order_by_asc(slots::Column::Time)
        .order_by_asc(slots::Column::Id)
        .all(conn)
        .await?;
    let slot_ids: Vec<i64> = slot_rows.iter().map(|row| row.id).collect();

    let pick_rows = slot_picks::Entity::find()
        .filter(slot_picks::Column::SlotId.is_in(slot_ids))
        .order_by_asc(slot_picks::Column::Position)
        .order_by_asc(slot_picks::Column::Id)
        .all(conn)
        .await?;
    let pick_ids: Vec<i64> = pick_rows.iter().map(|row| row.id).collect();
    let fallback_rows = slot_pick_fallbacks::Entity::find()
        .filter(slot_pick_fallbacks::Column::PickId.is_in(pick_ids))
        .order_by_asc(slot_pick_fallbacks::Column::PickId)
        .order_by_asc(slot_pick_fallbacks::Column::Position)
        .all(conn)
        .await?;

    let mut fallbacks: HashMap<i64, Vec<CategoryId>> = HashMap::new();
    for row in fallback_rows {
        fallbacks
            .entry(row.pick_id)
            .or_default()
            .push(CategoryId(row.category_id));
    }
    let mut picks: HashMap<i64, Vec<Pick>> = HashMap::new();
    for row in pick_rows {
        let slot_picks::Model {
            id: pick_id,
            slot_id,
            category_id: category,
            count,
            ..
        } = row;
        let count = u16::try_from(count)
            .ok()
            .and_then(NonZeroU16::new)
            .ok_or_else(|| anyhow!("pick count {count} is out of range"))?;
        picks.entry(slot_id).or_default().push(Pick {
            id: PickId(pick_id),
            category: CategoryId(category),
            count,
            fallback: fallbacks.remove(&pick_id).unwrap_or_default(),
        });
    }

    slot_rows
        .into_iter()
        .map(|row| {
            let slots::Model {
                id,
                time,
                effective_from,
                ..
            } = row;
            Ok(Slot {
                id: SlotId(id),
                time: Time::from_str(&time)
                    .with_context(|| format!("invalid slot time {time:?}"))?,
                effective_from: decode_timestamp(effective_from)?,
                picks: picks.remove(&id).unwrap_or_default(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {

    use super::*;
    use crate::store::Store;
    use crate::{
        schedule::tests::cid,
        store::test_support::{ADMIN, defaults, insert_category, insert_slot},
    };

    #[tokio::test]
    async fn sync_deactivates_admins_missing_from_the_configuration() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let (first, second, third) = (UserId(1), UserId(2), UserId(3));

        let sync = store.sync_admins(&[first, second], now).await.unwrap();
        assert_eq!(sync.deactivated, vec![ADMIN]);
        let sync = store.sync_admins(&[second, third], now).await.unwrap();
        assert_eq!(sync.deactivated, vec![first]);
        assert_eq!(
            store.sync_admins(&[second, third], now).await.unwrap(),
            AdminSync::default()
        );

        let active: Vec<_> = store
            .load_schedules(&defaults())
            .await
            .unwrap()
            .into_iter()
            .map(|(admin, _)| admin)
            .collect();
        assert_eq!(active, vec![second, third]);

        store.sync_admins(&[first], now).await.unwrap();
        let active = store.load_schedules(&defaults()).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].0, first);
    }

    #[tokio::test]
    async fn a_new_admin_starts_without_categories_or_slots() {
        let (_directory, store) = Store::open_temporary().await;
        let newcomer = UserId(7);
        store
            .sync_admins(&[ADMIN, newcomer], Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        let schedule = store
            .load_schedule(newcomer, &defaults())
            .await
            .unwrap()
            .unwrap();
        assert!(schedule.categories.is_empty());
        assert!(schedule.slots.is_empty());
        assert!(schedule.reminder.is_none());
        assert!(store.categories(newcomer).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn syncing_an_existing_admin_preserves_settings_and_created_at() {
        let (_directory, store) = Store::open_temporary().await;
        admins::Entity::update_many()
            .set(admins::ActiveModel {
                timezone: Set(Some("Asia/Shanghai".into())),
                misfire_grace: Set(Some("30m".into())),
                send_interval: Set(Some("1s".into())),
                reminder_time: Set(Some("20:00".into())),
                ..Default::default()
            })
            .filter(admins::Column::UserId.eq(user_id(ADMIN).unwrap()))
            .exec(&store.db)
            .await
            .unwrap();
        store.sync_admins(&[], Timestamp::UNIX_EPOCH).await.unwrap();
        store
            .sync_admins(&[ADMIN], "2026-01-01T00:00:00Z".parse().unwrap())
            .await
            .unwrap();
        let row = admins::Entity::find_by_id(user_id(ADMIN).unwrap())
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert!(row.active);
        assert_eq!(row.created_at, timestamp(Timestamp::UNIX_EPOCH).unwrap());
        assert_eq!(row.timezone.as_deref(), Some("Asia/Shanghai"));
        assert_eq!(row.misfire_grace.as_deref(), Some("30m"));
        assert_eq!(row.send_interval.as_deref(), Some("1s"));
        assert_eq!(row.reminder_time.as_deref(), Some("20:00"));
    }

    #[tokio::test]
    async fn loads_picks_fallbacks_and_overrides() {
        let (_directory, store) = Store::open_temporary().await;
        insert_slot(
            &store,
            ADMIN,
            "10:00",
            &[("gi", 2, &["other", "hsr"]), ("hsr", 1, &[])],
        )
        .await;
        {
            admins::Entity::update_many()
                .set(admins::ActiveModel {
                    timezone: Set(Some("Asia/Shanghai".into())),
                    misfire_grace: Set(Some("30m".into())),
                    send_interval: Set(Some("1s".into())),
                    reminder_time: Set(Some("20:00".into())),
                    ..Default::default()
                })
                .filter(admins::Column::UserId.eq(user_id(ADMIN).unwrap()))
                .exec(&store.db)
                .await
                .unwrap();
        }

        let schedule = store
            .load_schedule(ADMIN, &defaults())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(schedule.timezone.iana_name(), Some("Asia/Shanghai"));
        assert_eq!(schedule.misfire_grace.get().as_secs(), 30 * 60);
        assert_eq!(schedule.send_interval.get().as_secs(), 1);
        assert_eq!(schedule.reminder, Some("20:00".parse().unwrap()));
        assert_eq!(schedule.categories.len(), 4);

        let slot = &schedule.slots[0];
        assert_eq!(slot.picks.len(), 2);
        assert_eq!(slot.picks[0].count.get(), 2);
        assert_eq!(slot.picks[0].fallback, [cid("other"), cid("hsr")]);
        assert!(slot.picks[1].fallback.is_empty());
    }

    #[tokio::test]
    async fn a_broken_schedule_does_not_hide_other_admins() {
        let (_directory, store) = Store::open_temporary().await;
        let other = UserId(7);
        store
            .sync_admins(&[ADMIN, other], Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        insert_category(&store, other, "a").await;
        {
            admins::Entity::update_many()
                .set(admins::ActiveModel {
                    timezone: Set(Some("Nowhere/Land".into())),
                    ..Default::default()
                })
                .filter(admins::Column::UserId.eq(user_id(ADMIN).unwrap()))
                .exec(&store.db)
                .await
                .unwrap();
        }
        let loaded = store.load_schedules(&defaults()).await.unwrap();
        assert_eq!(loaded.len(), 2);
        // 按用户 ID 排序：7 在 42 前面。
        assert_eq!(loaded[0].1.as_ref().unwrap().categories.len(), 1);
        assert!(loaded[1].1.is_err());
    }
}
