use std::{collections::HashSet, str::FromStr};

use anyhow::{Context, Result};
use jiff::{Timestamp, civil::Date};
use sea_orm::{
    ColumnTrait, ConnectionTrait, EntityTrait, PaginatorTrait, QueryFilter, QueryOrder,
    QuerySelect, QueryTrait, Set,
    sea_query::{Expr, OnConflict},
};
use teloxide::types::UserId;

use super::{
    Conn, Store,
    codec::{decode_user_id, timestamp, user_id},
    entities::{posts, publication_attempts, publication_runs, published_messages},
    lock_admin, lock_post,
    posts::{
        load_posts, mark_failed, mark_published, mark_reserved, queued_candidates, requeue_posts,
    },
};
use crate::{
    model::{
        AttemptId, AttemptStatus, Post, PostId, PostStatus, RunId, RunStatus, SentMessage, Slot,
        SlotId,
    },
    selection::{Shortage, select_posts},
};

#[derive(Debug)]
pub struct PlannedAttempt {
    pub id: AttemptId,
    pub post: Post,
}

#[derive(Debug)]
pub struct Plan {
    pub run_id: RunId,
    pub attempts: Vec<PlannedAttempt>,
    pub shortages: Vec<Shortage>,
}

#[derive(Debug)]
pub enum AttemptOutcome {
    Succeeded(Vec<SentMessage>),
    /// 没有发布任何内容；帖子回到队列。
    Requeued(String),
    /// Telegram 永久拒绝了该帖子。
    Failed(String),
    /// 请求可能已生效也可能没有；需要人工检查频道。
    Unknown(String),
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Recovery {
    pub requeued: usize,
    pub unknown: usize,
}

impl Store {
    /// 已经有运行记录的槽位，以给定日期对应的 `(slot, local date)` 对表示。
    pub async fn slots_with_run(&self, dates: &[Date]) -> Result<HashSet<(SlotId, Date)>> {
        let dates: Vec<String> = dates.iter().map(Date::to_string).collect();
        let rows = publication_runs::Entity::find()
            .filter(publication_runs::Column::LocalDate.is_in(dates))
            .all(&self.db)
            .await?;
        rows.into_iter()
            .map(|row| {
                let date = row.local_date;
                let date =
                    Date::from_str(&date).with_context(|| format!("invalid run date {date:?}"))?;
                Ok((SlotId(row.slot_id), date))
            })
            .collect()
    }

    pub async fn skip_slot(
        &self,
        slot: &Slot,
        date: Date,
        scheduled_at: Timestamp,
        now: Timestamp,
    ) -> Result<()> {
        publication_runs::Entity::insert(publication_runs::ActiveModel {
            slot_id: Set(slot.id.0),
            local_date: Set(date.to_string()),
            scheduled_at: Set(timestamp(scheduled_at)?),
            completed_at: Set(Some(timestamp(now)?)),
            status: Set(RunStatus::Skipped),
            ..Default::default()
        })
        .on_conflict(
            OnConflict::columns([
                publication_runs::Column::SlotId,
                publication_runs::Column::LocalDate,
            ])
            .do_nothing_on([publication_runs::Column::Id])
            .to_owned(),
        )
        .try_insert()
        .exec(&self.db)
        .await?;
        Ok(())
    }

    /// 为 `admin` 的某个槽位创建运行记录，挑选其帖子并在同一个
    /// 事务中预留它们。如果该槽位在该日期已有运行记录则返回 `None`。
    pub async fn reserve_slot(
        &self,
        admin: UserId,
        slot: &Slot,
        date: Date,
        scheduled_at: Timestamp,
        now: Timestamp,
    ) -> Result<Option<Plan>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            let inserted = publication_runs::Entity::insert(publication_runs::ActiveModel {
                slot_id: Set(slot.id.0),
                local_date: Set(date.to_string()),
                scheduled_at: Set(timestamp(scheduled_at)?),
                started_at: Set(Some(timestamp(now)?)),
                status: Set(RunStatus::Running),
                ..Default::default()
            })
            .on_conflict(
                OnConflict::columns([
                    publication_runs::Column::SlotId,
                    publication_runs::Column::LocalDate,
                ])
                .do_nothing_on([publication_runs::Column::Id])
                .to_owned(),
            )
            .try_insert()
            .exec(conn)
            .await?;
            let sea_orm::TryInsertResult::Inserted(result) = inserted else {
                return Ok(None);
            };
            let run_id = result.last_insert_id;

            let mut locked = HashSet::new();
            let selection = loop {
                let candidates = queued_candidates(conn, admin).await?;
                let selection = select_posts(slot, &candidates);
                let mut newly_locked = false;
                for item in &selection.selected {
                    if locked.insert(item.post_id.0) {
                        lock_post(conn, item.post_id).await?;
                        newly_locked = true;
                    }
                }
                if !newly_locked {
                    break selection;
                }
            };
            let post_ids: Vec<i64> = selection
                .selected
                .iter()
                .map(|item| item.post_id.0)
                .collect();

            let mut attempt_ids = Vec::with_capacity(post_ids.len());
            for item in &selection.selected {
                let attempt_id =
                    publication_attempts::Entity::insert(publication_attempts::ActiveModel {
                        run_id: Set(Some(run_id)),
                        post_id: Set(item.post_id.0),
                        configured_category_id: Set(Some(item.configured_category.0)),
                        status: Set(AttemptStatus::Pending),
                        ..Default::default()
                    })
                    .exec(conn)
                    .await?
                    .last_insert_id;
                attempt_ids.push(attempt_id);
            }
            mark_reserved(conn, &post_ids).await?;

            let posts = load_posts(conn, &post_ids).await?;
            let attempts = attempt_ids
                .into_iter()
                .zip(posts)
                .map(|(id, post)| PlannedAttempt {
                    id: AttemptId(id),
                    post,
                })
                .collect();
            anyhow::Ok(Some(Plan {
                run_id: RunId(run_id),
                attempts,
                shortages: selection.shortages,
            }))
        })
        .await
    }

    /// 立即发送 `admin` 的草稿或排队中的帖子：不属于任何时段，所以没有运行记录。
    /// 帖子改为发布中，同时记下一次已开始发送的尝试，调用方随后发布并用
    /// [`Store::finish_attempt`] 记录结果。帖子不是他的草稿或排队中的帖子时返回 `None`。
    pub async fn begin_send_now(
        &self,
        post_id: PostId,
        admin: UserId,
        now: Timestamp,
    ) -> Result<Option<PlannedAttempt>> {
        self.transaction(async move |conn| {
            lock_admin(conn, admin).await?;
            lock_post(conn, post_id).await?;
            let changed = posts::Entity::update_many()
                .filter(posts::Column::Id.eq(post_id.0))
                .filter(posts::Column::OwnerAdminId.eq(user_id(admin)?))
                .filter(posts::Column::Status.is_in([PostStatus::Draft, PostStatus::Queued]))
                .col_expr(posts::Column::Status, Expr::value(PostStatus::Reserved))
                .col_expr(posts::Column::QueuedAt, Expr::value(timestamp(now)?))
                .exec(conn)
                .await?;
            if changed.rows_affected == 0 {
                return Ok(None);
            }
            let attempt_id =
                publication_attempts::Entity::insert(publication_attempts::ActiveModel {
                    post_id: Set(post_id.0),
                    status: Set(AttemptStatus::Sending),
                    started_at: Set(Some(timestamp(now)?)),
                    ..Default::default()
                })
                .exec(conn)
                .await?
                .last_insert_id;
            let post = load_posts(conn, &[post_id.0])
                .await?
                .pop()
                .context("the reserved post disappeared")?;
            anyhow::Ok(Some(PlannedAttempt {
                id: AttemptId(attempt_id),
                post,
            }))
        })
        .await
    }

    pub async fn mark_attempt_sending(
        &self,
        attempt_id: AttemptId,
        now: Timestamp,
    ) -> Result<bool> {
        let changed = publication_attempts::Entity::update_many()
            .filter(publication_attempts::Column::Id.eq(attempt_id.0))
            .filter(publication_attempts::Column::Status.eq(AttemptStatus::Pending))
            .col_expr(
                publication_attempts::Column::Status,
                Expr::value(AttemptStatus::Sending),
            )
            .col_expr(
                publication_attempts::Column::StartedAt,
                Expr::value(timestamp(now)?),
            )
            .exec(&self.db)
            .await?;
        Ok(changed.rows_affected == 1)
    }

    pub async fn finish_attempt(
        &self,
        attempt_id: AttemptId,
        post: &Post,
        outcome: AttemptOutcome,
        now: Timestamp,
    ) -> Result<()> {
        self.transaction(async move |conn| {
            lock_attempt(conn, attempt_id).await?;
            let Some(attempt) = publication_attempts::Entity::find_by_id(attempt_id.0)
                .one(conn)
                .await?
            else {
                return Ok(());
            };
            anyhow::ensure!(
                attempt.post_id == post.id.0,
                "attempt does not belong to the supplied post"
            );
            let finishable = sea_orm::Condition::any()
                .add(
                    publication_attempts::Column::Status
                        .is_in([AttemptStatus::Pending, AttemptStatus::Sending]),
                )
                .add(
                    sea_orm::Condition::all()
                        .add(publication_attempts::Column::Status.eq(AttemptStatus::Unknown))
                        .add(publication_attempts::Column::ResolvedByUserId.is_null())
                        .add(publication_attempts::Column::ResolvedAt.is_null()),
                );
            if !matches!(
                attempt.status,
                AttemptStatus::Pending | AttemptStatus::Sending
            ) && !(attempt.status == AttemptStatus::Unknown
                && attempt.resolved_by_user_id.is_none()
                && attempt.resolved_at.is_none())
            {
                return Ok(());
            }
            lock_post(conn, post.id).await?;
            let current_post = posts::Entity::find_by_id(post.id.0).one(conn).await?;
            if !current_post.is_some_and(|post| post.status == PostStatus::Reserved) {
                return Ok(());
            }
            let (status, error) = match &outcome {
                AttemptOutcome::Succeeded(_) => (AttemptStatus::Succeeded, None),
                AttemptOutcome::Requeued(error) => (AttemptStatus::Requeued, Some(error)),
                AttemptOutcome::Failed(error) => (AttemptStatus::Failed, Some(error)),
                AttemptOutcome::Unknown(error) => (AttemptStatus::Unknown, Some(error)),
            };
            let changed = publication_attempts::Entity::update_many()
                .filter(publication_attempts::Column::Id.eq(attempt_id.0))
                .filter(publication_attempts::Column::PostId.eq(post.id.0))
                .filter(finishable)
                .col_expr(publication_attempts::Column::Status, Expr::value(status))
                .col_expr(
                    publication_attempts::Column::CompletedAt,
                    Expr::value(timestamp(now)?),
                )
                .col_expr(
                    publication_attempts::Column::Error,
                    Expr::value(error.cloned()),
                )
                .exec(conn)
                .await?;
            if changed.rows_affected == 0 {
                return Ok(());
            }
            match &outcome {
                AttemptOutcome::Succeeded(messages) => {
                    for (position, message) in messages.iter().enumerate() {
                        published_messages::Entity::insert(published_messages::ActiveModel {
                            attempt_id: Set(attempt_id.0),
                            chat_id: Set(message.chat_id.0),
                            message_id: Set(message.message_id.0),
                            position: Set(i32::try_from(position)?),
                            ..Default::default()
                        })
                        .exec_without_returning(conn)
                        .await?;
                    }
                    mark_published(conn, post.id.0, now).await?;
                }
                AttemptOutcome::Requeued(_) => requeue_posts(conn, &[post.id.0], now).await?,
                AttemptOutcome::Failed(_) => mark_failed(conn, post.id.0).await?,
                AttemptOutcome::Unknown(_) => {}
            }
            anyhow::Ok(())
        })
        .await
    }

    /// 停止一次运行：尚未发送的尝试会把其帖子放回队列。
    pub async fn abort_run(&self, run_id: RunId, now: Timestamp) -> Result<()> {
        self.transaction(async move |conn| {
            requeue_pending_attempts(conn, Some(run_id), now).await?;
            publication_runs::Entity::update_many()
                .filter(publication_runs::Column::Id.eq(run_id.0))
                .filter(publication_runs::Column::Status.eq(RunStatus::Running))
                .col_expr(
                    publication_runs::Column::Status,
                    Expr::value(RunStatus::Aborted),
                )
                .col_expr(
                    publication_runs::Column::CompletedAt,
                    Expr::value(timestamp(now)?),
                )
                .exec(conn)
                .await?;
            anyhow::Ok(())
        })
        .await
    }

    pub async fn finish_run(&self, run_id: RunId, status: RunStatus, now: Timestamp) -> Result<()> {
        anyhow::ensure!(
            status != RunStatus::Running,
            "a run must finish with a terminal status"
        );
        publication_runs::Entity::update_many()
            .filter(publication_runs::Column::Id.eq(run_id.0))
            .filter(publication_runs::Column::Status.eq(RunStatus::Running))
            .col_expr(publication_runs::Column::Status, Expr::value(status))
            .col_expr(
                publication_runs::Column::CompletedAt,
                Expr::value(timestamp(now)?),
            )
            .exec(&self.db)
            .await?;
        Ok(())
    }

    /// 在进程停止后清理。从未开始的尝试会重新入队；
    /// 请求已在途中的尝试无法与成功区分开，会变为未知。
    pub async fn recover_interrupted(&self, now: Timestamp) -> Result<Recovery> {
        self.transaction(async move |conn| {
            let requeued = requeue_pending_attempts(conn, None, now).await?;
            let unknown = publication_attempts::Entity::update_many()
                .filter(publication_attempts::Column::Status.eq(AttemptStatus::Sending))
                .col_expr(
                    publication_attempts::Column::Status,
                    Expr::value(AttemptStatus::Unknown),
                )
                .col_expr(
                    publication_attempts::Column::CompletedAt,
                    Expr::value(timestamp(now)?),
                )
                .col_expr(
                    publication_attempts::Column::Error,
                    Expr::value("process stopped while the Telegram request was in flight"),
                )
                .exec(conn)
                .await?
                .rows_affected;
            publication_runs::Entity::update_many()
                .filter(publication_runs::Column::Status.eq(RunStatus::Running))
                .col_expr(
                    publication_runs::Column::Status,
                    Expr::value(RunStatus::Aborted),
                )
                .col_expr(
                    publication_runs::Column::CompletedAt,
                    Expr::value(timestamp(now)?),
                )
                .exec(conn)
                .await?;
            anyhow::Ok(Recovery {
                requeued,
                unknown: usize::try_from(unknown)?,
            })
        })
        .await
    }

    /// 自 `cutoff` 起或更早就一直未知的尝试，连同拥有
    /// 该帖子、需要去查看频道的管理员。
    pub async fn stale_unknown_attempts(
        &self,
        cutoff: Timestamp,
    ) -> Result<Vec<(AttemptId, UserId)>> {
        let rows = publication_attempts::Entity::find()
            .filter(publication_attempts::Column::Status.eq(AttemptStatus::Unknown))
            .filter(publication_attempts::Column::CompletedAt.lte(timestamp(cutoff)?))
            .all(&self.db)
            .await?;
        let mut result = Vec::with_capacity(rows.len());
        for row in rows {
            if let Some(post) = posts::Entity::find_by_id(row.post_id).one(&self.db).await? {
                let owner = post
                    .owner_admin_id
                    .context("published post without owner")?;
                result.push((AttemptId(row.id), decode_user_id(owner)?));
            }
        }
        Ok(result)
    }

    /// `admin` 有多少尝试在等待人工检查频道。
    pub async fn unknown_attempt_count(&self, admin: UserId) -> Result<i64> {
        let owned_posts = posts::Entity::find()
            .select_only()
            .column(posts::Column::Id)
            .filter(posts::Column::OwnerAdminId.eq(user_id(admin)?))
            .into_query();
        let count = publication_attempts::Entity::find()
            .filter(publication_attempts::Column::Status.eq(AttemptStatus::Unknown))
            .filter(publication_attempts::Column::PostId.in_subquery(owned_posts))
            .count(&self.db)
            .await?;
        Ok(i64::try_from(count)?)
    }

    /// 某次尝试所属帖子的拥有者管理员。
    pub async fn attempt_owner(&self, attempt_id: AttemptId) -> Result<Option<UserId>> {
        let Some(attempt) = publication_attempts::Entity::find_by_id(attempt_id.0)
            .one(&self.db)
            .await?
        else {
            return Ok(None);
        };
        let post = posts::Entity::find_by_id(attempt.post_id)
            .one(&self.db)
            .await?;
        post.and_then(|post| post.owner_admin_id)
            .map(decode_user_id)
            .transpose()
    }

    /// 记录人工对结果未知的尝试所做的决定。返回该
    /// 尝试所属的帖子；如果该尝试已不再是未知状态则返回 `None`。
    pub async fn resolve_unknown_attempt(
        &self,
        attempt_id: AttemptId,
        published: bool,
        actor: UserId,
        now: Timestamp,
    ) -> Result<Option<PostId>> {
        self.transaction(async move |conn| {
            lock_attempt(conn, attempt_id).await?;
            let attempt = publication_attempts::Entity::find_by_id(attempt_id.0)
                .filter(publication_attempts::Column::Status.eq(AttemptStatus::Unknown))
                .one(conn)
                .await?;
            let Some(attempt) = attempt else {
                return Ok(None);
            };
            let post_id = attempt.post_id;
            lock_post(conn, PostId(post_id)).await?;
            let post = posts::Entity::find_by_id(post_id).one(conn).await?;
            if !post.is_some_and(|post| post.status == PostStatus::Reserved) {
                return Ok(None);
            }
            let status = if published {
                AttemptStatus::Succeeded
            } else {
                AttemptStatus::Requeued
            };
            let changed = publication_attempts::Entity::update_many()
                .filter(publication_attempts::Column::Id.eq(attempt_id.0))
                .filter(publication_attempts::Column::Status.eq(AttemptStatus::Unknown))
                .col_expr(publication_attempts::Column::Status, Expr::value(status))
                .col_expr(
                    publication_attempts::Column::ResolvedByUserId,
                    Expr::value(user_id(actor)?),
                )
                .col_expr(
                    publication_attempts::Column::ResolvedAt,
                    Expr::value(timestamp(now)?),
                )
                .exec(conn)
                .await?;
            if changed.rows_affected == 0 {
                return Ok(None);
            }
            if published {
                mark_published(conn, post_id, now).await?;
            } else {
                requeue_posts(conn, &[post_id], now).await?;
            }
            anyhow::Ok(Some(PostId(post_id)))
        })
        .await
    }
}

/// 把待处理的尝试标记为未尝试并让其帖子重新入队，针对某次运行或所有运行。
async fn requeue_pending_attempts(
    conn: &Conn,
    run_id: Option<RunId>,
    now: Timestamp,
) -> Result<usize> {
    let mut query = publication_attempts::Entity::find()
        .filter(publication_attempts::Column::Status.eq(AttemptStatus::Pending));
    if let Some(run_id) = run_id {
        query = query.filter(publication_attempts::Column::RunId.eq(run_id.0));
    }
    let pending = query
        .order_by_asc(publication_attempts::Column::Id)
        .all(conn)
        .await?;
    let mut post_ids = Vec::with_capacity(pending.len());
    for attempt in pending {
        lock_attempt(conn, AttemptId(attempt.id)).await?;
        let changed = publication_attempts::Entity::update_many()
            .filter(publication_attempts::Column::Id.eq(attempt.id))
            .filter(publication_attempts::Column::Status.eq(AttemptStatus::Pending))
            .col_expr(
                publication_attempts::Column::Status,
                Expr::value(AttemptStatus::NotAttempted),
            )
            .exec(conn)
            .await?;
        if changed.rows_affected != 0 {
            lock_post(conn, PostId(attempt.post_id)).await?;
            let post = posts::Entity::find_by_id(attempt.post_id).one(conn).await?;
            if post.is_some_and(|post| post.status == PostStatus::Reserved) {
                post_ids.push(attempt.post_id);
            }
        }
    }
    requeue_posts(conn, &post_ids, now).await?;
    Ok(post_ids.len())
}

async fn lock_attempt(conn: &impl ConnectionTrait, attempt_id: AttemptId) -> Result<()> {
    publication_attempts::Entity::update_many()
        .filter(publication_attempts::Column::Id.eq(attempt_id.0))
        .col_expr(
            publication_attempts::Column::Id,
            Expr::col(publication_attempts::Column::Id),
        )
        .exec(conn)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use jiff::civil::date;

    use super::*;
    use crate::store::Store;
    use crate::{
        model::PostStatus,
        store::test_support::{ADMIN, incoming, insert_slot, queue},
    };

    async fn slot(store: &Store, category: &str, count: u16) -> Slot {
        insert_slot(store, ADMIN, "10:00", &[(category, count, &[])]).await
    }

    async fn queued_store(count: i32) -> (tempfile::TempDir, Store) {
        let (directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        for message_id in 0..count {
            let post_id = store
                .create_draft(&[incoming(message_id, "text")], now, None)
                .await
                .unwrap()
                .unwrap();
            queue(&store, post_id, "gi", now).await;
        }
        (directory, store)
    }

    async fn status_of(store: &Store, post: &Post) -> PostStatus {
        posts::Entity::find_by_id(post.id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap()
            .status
    }

    #[tokio::test]
    async fn reserves_posts_once_per_slot_and_date() {
        let (_directory, store) = queued_store(3).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 2).await;
        let day = date(1970, 1, 1);

        let plan = store
            .reserve_slot(ADMIN, &slot, day, now, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(plan.attempts.len(), 2);
        assert!(plan.shortages.is_empty());
        assert_eq!(store.queued_candidates(ADMIN).await.unwrap().len(), 1);
        assert!(
            store
                .reserve_slot(ADMIN, &slot, day, now, now)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .slots_with_run(&[day])
                .await
                .unwrap()
                .contains(&(slot.id, day))
        );
    }

    #[tokio::test]
    async fn concurrent_reservations_create_one_run_and_reserve_a_post_once() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 1).await;
        let day = date(1970, 1, 1);
        let post_id = store.queued_candidates(ADMIN).await.unwrap()[0].id;

        let (first, second) = tokio::join!(
            store.reserve_slot(ADMIN, &slot, day, now, now),
            store.reserve_slot(ADMIN, &slot, day, now, now),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_ne!(first.is_some(), second.is_some());
        let plan = first.or(second).unwrap();
        assert_eq!(plan.attempts.len(), 1);
        assert_eq!(plan.attempts[0].post.id, post_id);
        assert!(plan.shortages.is_empty());

        let runs = publication_runs::Entity::find()
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, plan.run_id.0);
        assert_eq!(runs[0].slot_id, slot.id.0);
        assert_eq!(runs[0].local_date, day.to_string());
        assert_eq!(runs[0].status, RunStatus::Running);
        let attempts = publication_attempts::Entity::find()
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].id, plan.attempts[0].id.0);
        assert_eq!(attempts[0].run_id, Some(plan.run_id.0));
        assert_eq!(attempts[0].post_id, post_id.0);
        assert_eq!(attempts[0].status, AttemptStatus::Pending);
        assert_eq!(
            status_of(&store, &plan.attempts[0].post).await,
            PostStatus::Reserved
        );
        assert!(store.queued_candidates(ADMIN).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn concurrent_immediate_sends_reserve_a_post_once() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let post_id = draft(&store, 1).await;

        let (first, second) = tokio::join!(
            store.begin_send_now(post_id, ADMIN, now),
            store.begin_send_now(post_id, ADMIN, now),
        );
        let first = first.unwrap();
        let second = second.unwrap();
        assert_ne!(first.is_some(), second.is_some());
        let attempt = first.or(second).unwrap();
        assert_eq!(attempt.post.id, post_id);

        let attempts = publication_attempts::Entity::find()
            .all(&store.db)
            .await
            .unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].id, attempt.id.0);
        assert_eq!(attempts[0].post_id, post_id.0);
        assert_eq!(attempts[0].run_id, None);
        assert_eq!(attempts[0].status, AttemptStatus::Sending);
        assert_eq!(status_of(&store, &attempt.post).await, PostStatus::Reserved);
        assert_eq!(
            publication_runs::Entity::find()
                .count(&store.db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn an_aborted_attempt_cannot_be_claimed_for_sending() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 1).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        let attempt = &plan.attempts[0];
        store.abort_run(plan.run_id, now).await.unwrap();

        assert!(!store.mark_attempt_sending(attempt.id, now).await.unwrap());
        let row = publication_attempts::Entity::find_by_id(attempt.id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, AttemptStatus::NotAttempted);
        assert_eq!(row.started_at, None);
        assert_eq!(status_of(&store, &attempt.post).await, PostStatus::Queued);
    }

    #[tokio::test]
    async fn concurrent_sending_claims_succeed_only_once() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 1).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        let attempt = &plan.attempts[0];

        let (first, second) = tokio::join!(
            store.mark_attempt_sending(attempt.id, now),
            store.mark_attempt_sending(attempt.id, now),
        );
        assert_ne!(first.unwrap(), second.unwrap());
        let row = publication_attempts::Entity::find_by_id(attempt.id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, AttemptStatus::Sending);
        assert_eq!(row.started_at, Some(timestamp(now).unwrap()));
        assert_eq!(status_of(&store, &attempt.post).await, PostStatus::Reserved);
    }

    #[tokio::test]
    async fn late_finish_run_does_not_overwrite_an_aborted_run() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let later = Timestamp::from_second(1).unwrap();
        let slot = slot(&store, "gi", 1).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        store.abort_run(plan.run_id, now).await.unwrap();
        let aborted = publication_runs::Entity::find_by_id(plan.run_id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();

        for status in [RunStatus::Completed, RunStatus::CompletedWithIssues] {
            store.finish_run(plan.run_id, status, later).await.unwrap();
            let row = publication_runs::Entity::find_by_id(plan.run_id.0)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(row, aborted);
        }
        store.abort_run(plan.run_id, later).await.unwrap();
        let row = publication_runs::Entity::find_by_id(plan.run_id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row, aborted);
    }

    #[tokio::test]
    async fn abort_and_repeated_finish_do_not_overwrite_a_completed_run() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let later = Timestamp::from_second(1).unwrap();
        let slot = slot(&store, "gi", 1).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        let attempt = &plan.attempts[0];
        assert!(store.mark_attempt_sending(attempt.id, now).await.unwrap());
        store
            .finish_attempt(
                attempt.id,
                &attempt.post,
                AttemptOutcome::Succeeded(Vec::new()),
                now,
            )
            .await
            .unwrap();
        store
            .finish_run(plan.run_id, RunStatus::Completed, now)
            .await
            .unwrap();
        let completed = publication_runs::Entity::find_by_id(plan.run_id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(completed.status, RunStatus::Completed);
        assert_eq!(completed.completed_at, Some(timestamp(now).unwrap()));

        store.abort_run(plan.run_id, later).await.unwrap();
        store
            .finish_run(plan.run_id, RunStatus::CompletedWithIssues, later)
            .await
            .unwrap();
        let row = publication_runs::Entity::find_by_id(plan.run_id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row, completed);
        assert_eq!(
            status_of(&store, &attempt.post).await,
            PostStatus::Published
        );
    }

    #[tokio::test]
    async fn unknown_attempt_can_be_requeued() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 1).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        let attempt = &plan.attempts[0];
        store.mark_attempt_sending(attempt.id, now).await.unwrap();
        store
            .finish_attempt(
                attempt.id,
                &attempt.post,
                AttemptOutcome::Unknown("lost".into()),
                now,
            )
            .await
            .unwrap();
        assert_eq!(status_of(&store, &attempt.post).await, PostStatus::Reserved);
        assert_eq!(store.unknown_attempt_count(ADMIN).await.unwrap(), 1);
        assert_eq!(
            store.stale_unknown_attempts(now).await.unwrap(),
            vec![(attempt.id, ADMIN)]
        );

        assert_eq!(
            store
                .resolve_unknown_attempt(attempt.id, false, UserId(42), now)
                .await
                .unwrap(),
            Some(attempt.post.id)
        );
        assert_eq!(
            store
                .resolve_unknown_attempt(attempt.id, false, UserId(42), now)
                .await
                .unwrap(),
            None
        );
        assert_eq!(status_of(&store, &attempt.post).await, PostStatus::Queued);
    }

    #[tokio::test]
    async fn aborting_a_run_requeues_unsent_posts() {
        let (_directory, store) = queued_store(2).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 2).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        let first = &plan.attempts[0];
        store.mark_attempt_sending(first.id, now).await.unwrap();
        store
            .finish_attempt(
                first.id,
                &first.post,
                AttemptOutcome::Succeeded(Vec::new()),
                now,
            )
            .await
            .unwrap();
        store.abort_run(plan.run_id, now).await.unwrap();

        assert_eq!(status_of(&store, &first.post).await, PostStatus::Published);
        assert_eq!(
            status_of(&store, &plan.attempts[1].post).await,
            PostStatus::Queued
        );
    }

    #[tokio::test]
    async fn recovery_requeues_pending_and_flags_in_flight_attempts() {
        let (_directory, store) = queued_store(2).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 2).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        store
            .mark_attempt_sending(plan.attempts[0].id, now)
            .await
            .unwrap();

        let recovery = store.recover_interrupted(now).await.unwrap();
        assert_eq!(
            recovery,
            Recovery {
                requeued: 1,
                unknown: 1
            }
        );
        assert_eq!(
            status_of(&store, &plan.attempts[0].post).await,
            PostStatus::Reserved
        );
        assert_eq!(
            status_of(&store, &plan.attempts[1].post).await,
            PostStatus::Queued
        );
        assert_eq!(
            store.recover_interrupted(now).await.unwrap(),
            Recovery::default()
        );
    }

    #[tokio::test]
    async fn recovery_does_not_revive_a_terminal_post_with_a_pending_attempt() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 1).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        let attempt = &plan.attempts[0];
        posts::Entity::update_many()
            .filter(posts::Column::Id.eq(attempt.post.id.0))
            .col_expr(posts::Column::Status, Expr::value(PostStatus::Published))
            .exec(&store.db)
            .await
            .unwrap();

        assert_eq!(
            store.recover_interrupted(now).await.unwrap(),
            Recovery::default()
        );
        assert_eq!(
            status_of(&store, &attempt.post).await,
            PostStatus::Published
        );
        let attempt = publication_attempts::Entity::find_by_id(attempt.id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attempt.status, AttemptStatus::NotAttempted);
    }

    #[tokio::test]
    async fn a_terminal_attempt_cannot_be_marked_sending_again() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 1).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        let attempt = &plan.attempts[0];
        store
            .finish_attempt(
                attempt.id,
                &attempt.post,
                AttemptOutcome::Succeeded(Vec::new()),
                now,
            )
            .await
            .unwrap();
        store.mark_attempt_sending(attempt.id, now).await.unwrap();

        assert_eq!(
            store.recover_interrupted(now).await.unwrap(),
            Recovery::default()
        );
        let row = publication_attempts::Entity::find_by_id(attempt.id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, AttemptStatus::Succeeded);
        assert_eq!(
            status_of(&store, &attempt.post).await,
            PostStatus::Published
        );
    }

    async fn draft(store: &Store, message_id: i32) -> PostId {
        store
            .create_draft(&[incoming(message_id, "text")], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap()
    }

    fn sent(message_id: i32) -> SentMessage {
        SentMessage {
            chat_id: teloxide::types::ChatId(-100),
            message_id: teloxide::types::MessageId(message_id),
        }
    }

    async fn late_finish_preserves_manual_resolution(published: bool) {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let candidate = store.queued_candidates(ADMIN).await.unwrap().remove(0);
        let attempt = store
            .begin_send_now(candidate.id, ADMIN, now)
            .await
            .unwrap()
            .unwrap();
        store.recover_interrupted(now).await.unwrap();
        assert_eq!(
            store
                .resolve_unknown_attempt(attempt.id, published, ADMIN, now)
                .await
                .unwrap(),
            Some(attempt.post.id)
        );
        let resolved = publication_attempts::Entity::find_by_id(attempt.id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();

        // 回队投稿可能已经属于新的发送尝试。
        let next = if published {
            None
        } else {
            Some(
                store
                    .begin_send_now(attempt.post.id, ADMIN, now)
                    .await
                    .unwrap()
                    .unwrap(),
            )
        };
        for outcome in [
            AttemptOutcome::Succeeded(vec![sent(500)]),
            AttemptOutcome::Requeued("late failure".into()),
        ] {
            store
                .finish_attempt(attempt.id, &attempt.post, outcome, now)
                .await
                .unwrap();
            let after = publication_attempts::Entity::find_by_id(attempt.id.0)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(after, resolved);
        }
        assert_eq!(
            status_of(&store, &attempt.post).await,
            if published {
                PostStatus::Published
            } else {
                PostStatus::Reserved
            }
        );
        assert_eq!(
            published_messages::Entity::find()
                .filter(published_messages::Column::AttemptId.eq(attempt.id.0))
                .count(&store.db)
                .await
                .unwrap(),
            0
        );
        if let Some(next) = next {
            let next = publication_attempts::Entity::find_by_id(next.id.0)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(next.status, AttemptStatus::Sending);
        }
    }

    #[tokio::test]
    async fn late_finish_does_not_overwrite_manual_publication() {
        late_finish_preserves_manual_resolution(true).await;
    }

    #[tokio::test]
    async fn late_finish_does_not_overwrite_manual_requeue_or_a_new_attempt() {
        late_finish_preserves_manual_resolution(false).await;
    }

    #[tokio::test]
    async fn finishing_an_attempt_twice_does_not_duplicate_published_messages() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let candidate = store.queued_candidates(ADMIN).await.unwrap().remove(0);
        let attempt = store
            .begin_send_now(candidate.id, ADMIN, now)
            .await
            .unwrap()
            .unwrap();
        for _ in 0..2 {
            store
                .finish_attempt(
                    attempt.id,
                    &attempt.post,
                    AttemptOutcome::Succeeded(vec![sent(500), sent(501)]),
                    now,
                )
                .await
                .unwrap();
        }
        assert_eq!(
            published_messages::Entity::find()
                .filter(published_messages::Column::AttemptId.eq(attempt.id.0))
                .count(&store.db)
                .await
                .unwrap(),
            2
        );
        assert_eq!(
            status_of(&store, &attempt.post).await,
            PostStatus::Published
        );
    }

    #[tokio::test]
    async fn manual_resolution_does_not_revive_a_post_that_is_not_reserved() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let candidate = store.queued_candidates(ADMIN).await.unwrap().remove(0);
        let attempt = store
            .begin_send_now(candidate.id, ADMIN, now)
            .await
            .unwrap()
            .unwrap();
        store.recover_interrupted(now).await.unwrap();
        let before = publication_attempts::Entity::find_by_id(attempt.id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        posts::Entity::update_many()
            .filter(posts::Column::Id.eq(attempt.post.id.0))
            .col_expr(posts::Column::Status, Expr::value(PostStatus::Cancelled))
            .exec(&store.db)
            .await
            .unwrap();

        for published in [true, false] {
            assert_eq!(
                store
                    .resolve_unknown_attempt(attempt.id, published, ADMIN, now)
                    .await
                    .unwrap(),
                None
            );
            let after = publication_attempts::Entity::find_by_id(attempt.id.0)
                .one(&store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(after, before);
            assert_eq!(
                status_of(&store, &attempt.post).await,
                PostStatus::Cancelled
            );
        }
        store
            .finish_attempt(
                attempt.id,
                &attempt.post,
                AttemptOutcome::Succeeded(vec![sent(500)]),
                now,
            )
            .await
            .unwrap();
        let after = publication_attempts::Entity::find_by_id(attempt.id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(after, before);
        assert_eq!(
            status_of(&store, &attempt.post).await,
            PostStatus::Cancelled
        );
        assert_eq!(
            published_messages::Entity::find()
                .filter(published_messages::Column::AttemptId.eq(attempt.id.0))
                .count(&store.db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn finish_attempt_rejects_a_different_post() {
        let (_directory, store) = queued_store(2).await;
        let now = Timestamp::UNIX_EPOCH;
        let candidates = store.queued_candidates(ADMIN).await.unwrap();
        let first = store
            .begin_send_now(candidates[0].id, ADMIN, now)
            .await
            .unwrap()
            .unwrap();
        let second = store
            .begin_send_now(candidates[1].id, ADMIN, now)
            .await
            .unwrap()
            .unwrap();
        assert!(
            store
                .finish_attempt(
                    first.id,
                    &second.post,
                    AttemptOutcome::Succeeded(vec![sent(500)]),
                    now
                )
                .await
                .is_err()
        );
        let row = publication_attempts::Entity::find_by_id(first.id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, AttemptStatus::Sending);
        assert_eq!(status_of(&store, &first.post).await, PostStatus::Reserved);
        assert_eq!(status_of(&store, &second.post).await, PostStatus::Reserved);
        assert_eq!(
            published_messages::Entity::find()
                .filter(published_messages::Column::AttemptId.eq(first.id.0))
                .count(&store.db)
                .await
                .unwrap(),
            0
        );
    }

    #[tokio::test]
    async fn a_late_success_can_finish_an_unresolved_unknown_attempt() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let candidate = store.queued_candidates(ADMIN).await.unwrap().remove(0);
        let attempt = store
            .begin_send_now(candidate.id, ADMIN, now)
            .await
            .unwrap()
            .unwrap();
        store.recover_interrupted(now).await.unwrap();
        store
            .finish_attempt(
                attempt.id,
                &attempt.post,
                AttemptOutcome::Succeeded(vec![sent(500)]),
                now,
            )
            .await
            .unwrap();
        let row = publication_attempts::Entity::find_by_id(attempt.id.0)
            .one(&store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(row.status, AttemptStatus::Succeeded);
        assert_eq!(
            status_of(&store, &attempt.post).await,
            PostStatus::Published
        );
        assert_eq!(
            store.channel_message_of(attempt.post.id).await.unwrap(),
            Some(sent(500).message_id)
        );
    }

    #[tokio::test]
    async fn a_draft_without_a_category_can_be_sent_right_away() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let post = draft(&store, 1).await;

        let attempt = store
            .begin_send_now(post, ADMIN, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(attempt.post.id, post);
        assert_eq!(status_of(&store, &attempt.post).await, PostStatus::Reserved);
        assert!(
            store
                .begin_send_now(post, ADMIN, now)
                .await
                .unwrap()
                .is_none()
        );

        store
            .finish_attempt(
                attempt.id,
                &attempt.post,
                AttemptOutcome::Succeeded(vec![sent(500)]),
                now,
            )
            .await
            .unwrap();
        assert_eq!(
            status_of(&store, &attempt.post).await,
            PostStatus::Published
        );
        assert_eq!(
            store.channel_message_of(post).await.unwrap(),
            Some(sent(500).message_id)
        );
    }

    #[tokio::test]
    async fn a_queued_post_can_be_sent_right_away_and_leaves_the_queue() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let candidate = store.queued_candidates(ADMIN).await.unwrap().remove(0);

        let attempt = store
            .begin_send_now(candidate.id, ADMIN, now)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(status_of(&store, &attempt.post).await, PostStatus::Reserved);
        assert!(store.queued_candidates(ADMIN).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_the_owners_draft_or_queued_post_can_be_sent_right_away() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let post = draft(&store, 1).await;

        assert!(
            store
                .begin_send_now(post, UserId(999), now)
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            store
                .begin_send_now(PostId(404), ADMIN, now)
                .await
                .unwrap()
                .is_none()
        );
        assert!(store.cancel_post(post, ADMIN).await.unwrap());
        assert!(
            store
                .begin_send_now(post, ADMIN, now)
                .await
                .unwrap()
                .is_none()
        );
    }

    #[tokio::test]
    async fn an_unsent_draft_goes_back_to_a_draft_and_a_queued_post_to_the_queue() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let queued = store.queued_candidates(ADMIN).await.unwrap().remove(0).id;
        let draft = draft(&store, 50).await;

        for post in [queued, draft] {
            let attempt = store
                .begin_send_now(post, ADMIN, now)
                .await
                .unwrap()
                .unwrap();
            store
                .finish_attempt(
                    attempt.id,
                    &attempt.post,
                    AttemptOutcome::Requeued("channel down".into()),
                    now,
                )
                .await
                .unwrap();
        }

        let status = |post| {
            let store = store.clone();
            async move {
                let loaded = store.post_summary(post).await.unwrap().unwrap();
                loaded.status
            }
        };
        assert_eq!(status(queued).await, PostStatus::Queued);
        assert_eq!(status(draft).await, PostStatus::Draft);
    }

    #[tokio::test]
    async fn a_send_cut_short_by_a_restart_is_flagged_as_unknown() {
        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::UNIX_EPOCH;
        let post = draft(&store, 1).await;
        let attempt = store
            .begin_send_now(post, ADMIN, now)
            .await
            .unwrap()
            .unwrap();

        let recovery = store.recover_interrupted(now).await.unwrap();

        assert_eq!(
            recovery,
            Recovery {
                requeued: 0,
                unknown: 1
            }
        );
        assert_eq!(store.unknown_attempt_count(ADMIN).await.unwrap(), 1);
        assert_eq!(status_of(&store, &attempt.post).await, PostStatus::Reserved);
        store
            .resolve_unknown_attempt(attempt.id, false, ADMIN, now)
            .await
            .unwrap();
        assert_eq!(
            store.post_summary(post).await.unwrap().unwrap().status,
            PostStatus::Draft
        );
    }

    #[tokio::test]
    async fn supplement_replies_to_the_channel_message_of_its_target() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 1).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        let attempt = &plan.attempts[0];
        let sent = SentMessage {
            chat_id: teloxide::types::ChatId(-100),
            message_id: teloxide::types::MessageId(500),
        };
        store
            .finish_attempt(
                attempt.id,
                &attempt.post,
                AttemptOutcome::Succeeded(vec![sent]),
                now,
            )
            .await
            .unwrap();

        let original = store.channel_message_of(attempt.post.id).await.unwrap();
        assert_eq!(original, Some(sent.message_id));
        let supplement = store
            .create_draft(&[incoming(50, "more")], now, original)
            .await
            .unwrap()
            .unwrap();
        queue(&store, supplement, "gi", now).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 2), now, now)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(plan.attempts[0].post.id, supplement);
        assert_eq!(plan.attempts[0].post.reply_to, Some(sent.message_id));
        assert_eq!(attempt.post.reply_to, None);
    }

    #[tokio::test]
    async fn post_confirmed_by_hand_has_no_channel_message() {
        let (_directory, store) = queued_store(1).await;
        let now = Timestamp::UNIX_EPOCH;
        let slot = slot(&store, "gi", 1).await;
        let plan = store
            .reserve_slot(ADMIN, &slot, date(1970, 1, 1), now, now)
            .await
            .unwrap()
            .unwrap();
        let attempt = &plan.attempts[0];
        store
            .finish_attempt(
                attempt.id,
                &attempt.post,
                AttemptOutcome::Unknown("timeout".to_owned()),
                now,
            )
            .await
            .unwrap();
        let actor = teloxide::types::UserId(1);
        store
            .resolve_unknown_attempt(attempt.id, true, actor, now)
            .await
            .unwrap();
        assert_eq!(
            status_of(&store, &attempt.post).await,
            PostStatus::Published
        );
        assert_eq!(
            store.channel_message_of(attempt.post.id).await.unwrap(),
            None
        );
    }
}
