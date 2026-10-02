use std::{collections::HashSet, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use jiff::{SignedDuration, Timestamp};
use tokio::sync::watch;

use super::notifications;
use crate::{app::App, bot, model::NotificationSubject};

const TICK: Duration = Duration::from_secs(5);
/// 一次尝试可以保持未知状态多久，超时后会请用户去查看频道。
const UNKNOWN_GRACE: SignedDuration = SignedDuration::from_mins(10);

/// 升级处理未解决的发布，并投递通知。
pub async fn run(app: Arc<App>, mut shutdown: watch::Receiver<bool>) {
    let mut reported_cards = HashSet::new();
    loop {
        bot::post_missing_cards(&app, &mut reported_cards).await;
        if let Err(error) = tick(&app).await {
            tracing::error!("Housekeeping failed: {error:#}");
        }
        tokio::select! {
            () = tokio::time::sleep(TICK) => {}
            _ = shutdown.changed() => return,
        }
    }
}

async fn tick(app: &App) -> Result<()> {
    let now = Timestamp::now();
    let cutoff = now
        .checked_sub(UNKNOWN_GRACE)
        .context("unknown publication cutoff is outside the supported timestamp range")?;
    for (attempt, owner) in app.store.stale_unknown_attempts(cutoff).await? {
        app.store
            .enqueue_notification(
                NotificationSubject::PublicationUnknown(attempt),
                &[owner],
                now,
            )
            .await?;
    }

    notifications::deliver(app).await
}
