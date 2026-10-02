use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::Duration,
};

use anyhow::Result;
use jiff::{Timestamp, civil::Date};
use teloxide::types::UserId;
use tokio::sync::watch;

use crate::{
    app::App,
    bot,
    model::{NotificationSubject, RunStatus, Slot, SlotId},
    schedule::AdminSchedule,
    store::AttemptOutcome,
    telegram::publish_post,
};

const TICK: Duration = Duration::from_secs(1);

/// 调度循环在多次 tick 之间保留的状态。
#[derive(Default)]
struct State {
    /// 每个管理员最近一次已入队库存提醒的本地日期。入队本身是幂等的，
    /// 记住它只是为了少写数据库。
    last_reminder: HashMap<UserId, Date>,
    /// 已经记录过的时间表错误，避免每秒重复刷日志。
    reported_errors: HashSet<(UserId, String)>,
}

/// 按每个管理员的时间表运行发布时段，并安排每日库存提醒。
pub async fn run(app: Arc<App>, mut shutdown: watch::Receiver<bool>) {
    match app.store.recover_interrupted(Timestamp::now()).await {
        Ok(recovery) if recovery.requeued + recovery.unknown == 0 => {}
        Ok(recovery) => tracing::warn!(
            "Recovered interrupted publications: {} requeued, {} with unknown outcome",
            recovery.requeued,
            recovery.unknown
        ),
        Err(error) => tracing::error!("Failed to recover interrupted publications: {error:#}"),
    }

    let mut state = State::default();
    if let Err(error) = log_schedules(&app, &mut state).await {
        tracing::error!("Failed to read today's schedule state: {error:#}");
    }

    loop {
        if let Err(error) = tick(&app, &mut shutdown, &mut state).await {
            tracing::error!("Schedule processing failed: {error:#}");
        }
        tokio::select! {
            () = tokio::time::sleep(TICK) => {}
            _ = shutdown.changed() => return,
        }
    }
}

/// 读取所有管理员的时间表。配置有误的管理员只记一次错误并被跳过，不影响其他人。
async fn load_schedules(app: &App, state: &mut State) -> Result<Vec<AdminSchedule>> {
    let loaded = app.store.load_schedules(&app.config.schedule).await?;
    let mut schedules = Vec::with_capacity(loaded.len());
    for (admin, schedule) in loaded {
        match schedule {
            Ok(schedule) => {
                state
                    .reported_errors
                    .retain(|(reported, _)| *reported != admin);
                schedules.push(schedule);
            }
            Err(error) => {
                let message = format!("{error:#}");
                if state.reported_errors.insert((admin, message.clone())) {
                    tracing::error!("Skipping admin {}: {message}", admin.0);
                }
            }
        }
    }
    Ok(schedules)
}

/// 告诉运维人员调度器今天打算做什么。
async fn log_schedules(app: &App, state: &mut State) -> Result<()> {
    let schedules = load_schedules(app, state).await?;
    let now = Timestamp::now();
    for schedule in &schedules {
        let today = now.to_zoned(schedule.timezone.clone()).date();
        let finished = app.store.slots_with_run(&[today]).await?;
        let local_time = now.to_zoned(schedule.timezone.clone()).time();
        for slot in &schedule.slots {
            let state = if finished.contains(&(slot.id, today)) {
                "already ran today"
            } else if slot.time <= local_time {
                "due now"
            } else {
                "waiting"
            };
            tracing::info!(
                "Admin {} slot {} at {}: {state}",
                schedule.admin.0,
                slot.id.0,
                slot.time
            );
        }
    }
    Ok(())
}

async fn tick(app: &App, shutdown: &mut watch::Receiver<bool>, state: &mut State) -> Result<()> {
    let schedules = load_schedules(app, state).await?;
    run_due_slots(app, shutdown, &schedules).await?;
    for schedule in &schedules {
        enqueue_reminder(app, schedule, &mut state.last_reminder).await?;
    }
    Ok(())
}

/// 一个已到期、等待运行的时段。
struct DueSlot<'a> {
    scheduled: Timestamp,
    date: Date,
    schedule: &'a AdminSchedule,
    slot: &'a Slot,
}

async fn run_due_slots(
    app: &App,
    shutdown: &mut watch::Receiver<bool>,
    schedules: &[AdminSchedule],
) -> Result<()> {
    let now = Timestamp::now();
    let dates: HashSet<Date> = schedules
        .iter()
        .map(|schedule| now.to_zoned(schedule.timezone.clone()).date())
        .collect();
    let dates: Vec<Date> = dates.into_iter().collect();
    let finished = app.store.slots_with_run(&dates).await?;
    let due = due_slots(schedules, now, &finished)?;

    // 仍在宽限期内的时段按从旧到新的顺序运行，更早的直接丢弃。
    // 所有管理员的时段共用一个队列串行执行，避免同时向同一个频道发送。
    for item in due {
        if *shutdown.borrow() {
            break;
        }
        let now = Timestamp::now();
        if now.duration_since(item.scheduled) > item.schedule.misfire_grace.signed() {
            app.store
                .skip_slot(item.slot, item.date, item.scheduled, now)
                .await?;
            tracing::warn!(
                "Skipped expired publication slot {} of admin {}",
                item.slot.id.0,
                item.schedule.admin.0
            );
        } else {
            execute_slot(app, shutdown, &item).await?;
        }
    }
    Ok(())
}

/// 找出此刻已到期、还没有运行记录的时段，按计划时间从早到晚排序。
/// 每个管理员按自己的时区计算“今天”和时段的触发时刻。
fn due_slots<'a>(
    schedules: &'a [AdminSchedule],
    now: Timestamp,
    finished: &HashSet<(SlotId, Date)>,
) -> Result<Vec<DueSlot<'a>>> {
    let mut due = Vec::new();
    for schedule in schedules {
        let date = now.to_zoned(schedule.timezone.clone()).date();
        for slot in &schedule.slots {
            // 还没有配额的时段不用运行，也不留下运行记录。
            if slot.picks.is_empty() || finished.contains(&(slot.id, date)) {
                continue;
            }
            let scheduled = date
                .to_datetime(slot.time)
                .to_zoned(schedule.timezone.clone())?
                .timestamp();
            // 时段生效之前的触发点不补发，否则刚新建的时段会把今天已过的时间当成错过。
            if scheduled <= now && scheduled >= slot.effective_from {
                due.push(DueSlot {
                    scheduled,
                    date,
                    schedule,
                    slot,
                });
            }
        }
    }
    due.sort_by_key(|item| item.scheduled);
    Ok(due)
}

async fn execute_slot(
    app: &App,
    shutdown: &mut watch::Receiver<bool>,
    item: &DueSlot<'_>,
) -> Result<()> {
    let DueSlot {
        scheduled,
        date,
        schedule,
        slot,
    } = *item;
    let admin = schedule.admin;
    let Some(plan) = app
        .store
        .reserve_slot(admin, slot, date, scheduled, Timestamp::now())
        .await?
    else {
        return Ok(());
    };
    for shortage in &plan.shortages {
        tracing::warn!(
            "Publication slot {} is short {} post(s) for category {}",
            slot.id.0,
            shortage.missing,
            shortage.category.0
        );
    }

    tracing::info!(
        "Slot {} started: {} post(s) reserved",
        slot.id.0,
        plan.attempts.len()
    );
    let mut had_issue = !plan.shortages.is_empty();
    let last = plan.attempts.len().saturating_sub(1);
    for (index, attempt) in plan.attempts.iter().enumerate() {
        if *shutdown.borrow() {
            app.store.abort_run(plan.run_id, Timestamp::now()).await?;
            return Ok(());
        }
        let claimed = app
            .store
            .mark_attempt_sending(attempt.id, Timestamp::now())
            .await?;
        if !claimed {
            tracing::info!(
                "Publication attempt {} is no longer pending; stopping slot execution",
                attempt.id.0
            );
            return Ok(());
        }
        let outcome =
            AttemptOutcome::from(publish_post(&app.bot, &app.config.channel, &attempt.post).await);
        let channel_unavailable = matches!(outcome, AttemptOutcome::Requeued(_));
        match &outcome {
            AttemptOutcome::Succeeded(sent) => tracing::info!(
                "Published post {} as {} channel message(s)",
                attempt.post.id.0,
                sent.len()
            ),
            AttemptOutcome::Requeued(error) => {
                tracing::warn!(
                    "Post {} was put back in the queue: {error}",
                    attempt.post.id.0
                )
            }
            AttemptOutcome::Failed(error) => {
                tracing::error!("Post {} was rejected: {error}", attempt.post.id.0)
            }
            AttemptOutcome::Unknown(error) => {
                tracing::error!("Outcome of post {} is unknown: {error}", attempt.post.id.0)
            }
        }
        let rejected = matches!(outcome, AttemptOutcome::Failed(_));
        let published = matches!(outcome, AttemptOutcome::Succeeded(_));
        had_issue |= !matches!(outcome, AttemptOutcome::Succeeded(_));
        app.store
            .finish_attempt(attempt.id, &attempt.post, outcome, Timestamp::now())
            .await?;
        if published {
            bot::mark_control_published(app, attempt.post.id).await;
        }

        if channel_unavailable {
            app.store.abort_run(plan.run_id, Timestamp::now()).await?;
            notify(app, admin, NotificationSubject::RunAborted(plan.run_id)).await?;
            return Ok(());
        }
        if rejected {
            notify(
                app,
                admin,
                NotificationSubject::PublicationFailed(attempt.id),
            )
            .await?;
        }
        if index < last {
            tokio::select! {
                () = tokio::time::sleep(schedule.send_interval.get()) => {}
                _ = shutdown.changed() => {}
            }
        }
    }

    let status = if had_issue {
        RunStatus::CompletedWithIssues
    } else {
        RunStatus::Completed
    };
    tracing::info!("Slot {} finished: {:?}", slot.id.0, status);
    app.store
        .finish_run(plan.run_id, status, Timestamp::now())
        .await
}

async fn notify(app: &App, admin: UserId, subject: NotificationSubject) -> Result<()> {
    app.store
        .enqueue_notification(subject, &[admin], Timestamp::now())
        .await
}

/// 到了管理员设置的提醒时间后，为他入队当天的库存提醒。入队是幂等的，
/// `last_reminder` 只是为了少写数据库。
async fn enqueue_reminder(
    app: &App,
    schedule: &AdminSchedule,
    last_reminder: &mut HashMap<UserId, Date>,
) -> Result<()> {
    let Some(reminder) = schedule.reminder else {
        return Ok(());
    };
    let local_now = Timestamp::now().to_zoned(schedule.timezone.clone());
    let today = local_now.date();
    if local_now.time() < reminder || last_reminder.get(&schedule.admin) == Some(&today) {
        return Ok(());
    }
    notify(
        app,
        schedule.admin,
        NotificationSubject::StockReminder(today),
    )
    .await?;
    last_reminder.insert(schedule.admin, today);
    Ok(())
}

#[cfg(test)]
mod tests {
    use jiff::{SignedDuration, civil::date, tz::TimeZone};
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path_regex},
    };

    use super::*;
    use crate::{
        model::{AttemptStatus, PositiveDuration, RunId},
        schedule::tests::{pick, slot},
        store::{
            Store,
            entities::{publication_attempts, publication_runs},
            test_support,
        },
    };

    fn schedule(admin: u64, timezone: &str, slots: Vec<Slot>) -> AdminSchedule {
        AdminSchedule {
            admin: UserId(admin),
            timezone: TimeZone::get(timezone).unwrap(),
            misfire_grace: PositiveDuration::try_from(SignedDuration::from_hours(2)).unwrap(),
            send_interval: PositiveDuration::try_from(SignedDuration::from_secs(3)).unwrap(),
            reminder: None,
            categories: Vec::new(),
            slots,
        }
    }

    fn at(text: &str) -> Timestamp {
        text.parse().unwrap()
    }

    fn keys(due: &[DueSlot<'_>]) -> Vec<String> {
        due.iter()
            .map(|item| format!("{}:{}", item.schedule.admin.0, item.slot.id.0))
            .collect()
    }

    async fn revoked_pending_attempt_is_not_published(recover: bool) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .and(body_string_contains("first post"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": {
                    "message_id": 77, "date": 0,
                    "chat": { "id": -100123, "type": "channel", "title": "test" },
                    "text": "first post"
                }
            })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .and(body_string_contains("revoked post"))
            .respond_with(ResponseTemplate::new(500))
            .expect(0)
            .mount(&server)
            .await;

        let (_directory, store) = Store::open_temporary().await;
        let now = Timestamp::now();
        let mut posts = Vec::new();
        // 选择顺序是最新入队优先；让第一篇投稿比被撤销的投稿更新。
        for (id, text, queued_at) in [
            (1, "first post", now),
            (2, "revoked post", now - SignedDuration::from_secs(1)),
        ] {
            let post = store
                .create_draft(&[test_support::incoming(id, text)], now, None)
                .await
                .unwrap()
                .unwrap();
            test_support::queue(&store, post, "gi", queued_at).await;
            posts.push(post);
        }
        let slot =
            test_support::insert_slot(&store, test_support::ADMIN, "09:00", &[("gi", 2, &[])])
                .await;
        let mut schedule = schedule(test_support::ADMIN.0, "UTC", vec![slot]);
        // 撤销第二次尝试后显式唤醒投稿之间的等待。
        schedule.send_interval = PositiveDuration::try_from(SignedDuration::from_hours(1)).unwrap();
        let mut config = crate::config::Config::parse(
            &include_str!("../../PureWaterSpiritBot.example.toml").replace("123456789", "42"),
        )
        .unwrap();
        config.channel = teloxide::types::ChatId(-100123).into();
        let (collector, _batches) = crate::collector::Collector::new(Duration::from_secs(1));
        let app = App {
            bot: teloxide::Bot::new("test-token")
                .set_api_url(format!("{}/", server.uri()).parse().unwrap()),
            channel: serde_json::from_value(json!({
                "id": -100123, "type": "channel", "title": "test",
                "max_reaction_count": 0,
                "accepted_gift_types": {
                    "unlimited_gifts": false, "limited_gifts": false,
                    "unique_gifts": false, "premium_subscription": false
                }
            }))
            .unwrap(),
            review: None,
            config,
            store,
            collector,
            fetcher: crate::fetch::Fetcher::unavailable(),
            inputs: Default::default(),
            registered_commands: Default::default(),
            ai: Default::default(),
        };
        let item = DueSlot {
            scheduled: now,
            date: now.to_zoned(TimeZone::UTC).date(),
            schedule: &schedule,
            slot: &schedule.slots[0],
        };
        let (sender, mut shutdown) = watch::channel(false);
        let revoke = async {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
            let first = loop {
                let attempt = publication_attempts::Entity::find()
                    .filter(publication_attempts::Column::PostId.eq(posts[0].0))
                    .one(&app.store.db)
                    .await
                    .unwrap();
                if attempt
                    .as_ref()
                    .is_some_and(|row| row.status == AttemptStatus::Succeeded)
                {
                    break attempt.unwrap();
                }
                if tokio::time::Instant::now() >= deadline {
                    let revoked = publication_attempts::Entity::find()
                        .filter(publication_attempts::Column::PostId.eq(posts[1].0))
                        .one(&app.store.db)
                        .await
                        .unwrap();
                    let request_count = server.received_requests().await.unwrap().len();
                    panic!(
                        "first publication did not succeed: first status={:?}, revoked status={:?}, Telegram request count={request_count}",
                        attempt.map(|row| row.status),
                        revoked.map(|row| row.status),
                    );
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            };
            if recover {
                let recovery = app
                    .store
                    .recover_interrupted(Timestamp::now())
                    .await
                    .unwrap();
                assert_eq!(recovery.requeued, 1);
                assert_eq!(recovery.unknown, 0);
            } else {
                app.store
                    .abort_run(RunId(first.run_id.unwrap()), Timestamp::now())
                    .await
                    .unwrap();
            }
            let revoked = publication_attempts::Entity::find()
                .filter(publication_attempts::Column::PostId.eq(posts[1].0))
                .one(&app.store.db)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(revoked.status, AttemptStatus::NotAttempted);
            // 保持 shutdown 为 false，使 execute_slot 进入认领检查，而不是走关闭分支。
            sender.send(false).unwrap();
            first.run_id.unwrap()
        };
        let (result, run_id) = tokio::time::timeout(Duration::from_secs(10), async {
            tokio::join!(execute_slot(&app, &mut shutdown, &item), revoke)
        })
        .await
        .expect("scheduler did not finish after the pending attempt was revoked");
        result.unwrap();
        let run = publication_runs::Entity::find_by_id(run_id)
            .one(&app.store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(run.status, RunStatus::Aborted);
        let revoked = publication_attempts::Entity::find()
            .filter(publication_attempts::Column::PostId.eq(posts[1].0))
            .one(&app.store.db)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(revoked.status, AttemptStatus::NotAttempted);
        server.verify().await;
    }

    #[tokio::test]
    async fn execute_slot_does_not_publish_an_attempt_revoked_by_abort() {
        revoked_pending_attempt_is_not_published(false).await;
    }

    #[tokio::test]
    async fn execute_slot_does_not_publish_an_attempt_revoked_by_recovery() {
        revoked_pending_attempt_is_not_published(true).await;
    }

    #[test]
    fn uses_each_admins_own_timezone() {
        // 2026-01-01T02:30Z 在上海是 10:30，在 UTC 是 02:30。
        let now = at("2026-01-01T02:30:00Z");
        let shanghai = schedule(
            1,
            "Asia/Shanghai",
            vec![slot(1, "10:00", vec![pick("gi", 1, &[])])],
        );
        let utc = schedule(2, "UTC", vec![slot(2, "10:00", vec![pick("gi", 1, &[])])]);
        let schedules = [shanghai, utc];
        let due = due_slots(&schedules, now, &HashSet::new()).unwrap();
        assert_eq!(keys(&due), ["1:1"]);
        assert_eq!(due[0].date, date(2026, 1, 1));
        assert_eq!(due[0].scheduled, at("2026-01-01T02:00:00Z"));
    }

    #[test]
    fn orders_slots_of_all_admins_by_scheduled_time() {
        let now = at("2026-01-01T12:00:00Z");
        let first = schedule(
            1,
            "UTC",
            vec![
                slot(1, "11:00", vec![pick("gi", 1, &[])]),
                slot(2, "09:00", vec![pick("gi", 1, &[])]),
            ],
        );
        let second = schedule(2, "UTC", vec![slot(3, "10:00", vec![pick("gi", 1, &[])])]);
        let schedules = [first, second];
        let due = due_slots(&schedules, now, &HashSet::new()).unwrap();
        assert_eq!(keys(&due), ["1:2", "2:3", "1:1"]);
    }

    #[test]
    fn skips_finished_future_empty_and_not_yet_effective_slots() {
        let now = at("2026-01-01T12:00:00Z");
        let mut created_later = slot(3, "10:00", vec![pick("gi", 1, &[])]);
        created_later.effective_from = at("2026-01-01T11:00:00Z");
        let schedules = [schedule(
            1,
            "UTC",
            vec![
                slot(1, "09:00", vec![pick("gi", 1, &[])]),
                slot(2, "18:00", vec![pick("gi", 1, &[])]),
                created_later,
                slot(4, "08:00", vec![pick("gi", 1, &[])]),
                slot(5, "07:00", Vec::new()),
            ],
        )];
        let finished = HashSet::from([(SlotId(1), date(2026, 1, 1))]);
        let due = due_slots(&schedules, now, &finished).unwrap();
        assert_eq!(keys(&due), ["1:4"]);
    }

    #[test]
    fn a_run_on_one_day_does_not_block_the_next_day() {
        let schedules = [schedule(
            1,
            "UTC",
            vec![slot(1, "09:00", vec![pick("gi", 1, &[])])],
        )];
        let finished = HashSet::from([(SlotId(1), date(2026, 1, 1))]);
        let next_day = at("2026-01-02T09:30:00Z");
        assert_eq!(
            keys(&due_slots(&schedules, next_day, &finished).unwrap()),
            ["1:1"]
        );
    }
}
