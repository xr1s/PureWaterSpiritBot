use anyhow::{Context, Result};
use jiff::Timestamp;
use teloxide::{
    RequestError,
    prelude::*,
    types::{InlineKeyboardMarkup, UserId},
};

use crate::{
    app::App,
    bot::{self, view},
    model::NotificationSubject,
    selection,
    store::PendingNotification,
};

/// 发送排队中的通知。投递语义为至少一次：投递被
/// 中断的通知会被重新发送。
pub async fn deliver(app: &App) -> Result<()> {
    app.store
        .cancel_notifications_except_for(&app.config.admin_ids())
        .await?;
    for notification in app.store.undelivered_notifications().await? {
        app.store.mark_notification_sending(notification.id).await?;
        if let Err(error) = send(app, &notification).await {
            app.store
                .mark_notification_failed(notification.id, is_retryable(&error), &error.to_string())
                .await?;
        }
    }
    Ok(())
}

async fn send(app: &App, notification: &PendingNotification) -> Result<(), DeliveryError> {
    let (text, keyboard) = render(app, notification.subject, notification.recipient).await?;
    let mut request = app.bot.send_message(notification.recipient, text);
    if let Some(keyboard) = keyboard {
        request = request.reply_markup(keyboard);
    }
    let message = request.await?;
    app.store
        .mark_notification_sent(notification.id, message.id, Timestamp::now())
        .await?;
    Ok(())
}

async fn render(
    app: &App,
    subject: NotificationSubject,
    recipient: UserId,
) -> Result<(String, Option<InlineKeyboardMarkup>)> {
    if let Some(rendered) = view::static_notification(subject) {
        return Ok(rendered);
    }
    if let NotificationSubject::SubmissionRejected(post) = subject {
        let info = app
            .store
            .review_info(post)
            .await?
            .with_context(|| format!("post {} not found", post.0))?;
        let text = view::submission_rejected(post, info.review_note.as_deref());
        return Ok((text, None));
    }
    if let NotificationSubject::PostRemoved(post) = subject {
        let info = app
            .store
            .review_info(post)
            .await?
            .with_context(|| format!("post {} not found", post.0))?;
        let summary = app.store.post_summary(post).await?;
        let category = summary
            .and_then(|summary| summary.category)
            .map(|category| category.label);
        let by = match info.reviewed_by {
            Some(admin) => bot::user_name(app, admin).await,
            None => "?".to_owned(),
        };
        let text = view::post_removed(post, category.as_deref(), &by, info.review_note.as_deref());
        return Ok((text, None));
    }
    let schedule = app
        .store
        .load_schedule(recipient, &app.config.schedule)
        .await?
        .with_context(|| format!("{} is not an active admin", recipient.0))?;
    let counts = app.store.stock_counts(recipient).await?;
    let unknown_attempts = app.store.unknown_attempt_count(recipient).await?;
    let candidates = app.store.queued_candidates(recipient).await?;
    let pending_reviews = app.store.pending_review_count().await?;
    let forecast = selection::forecast(&schedule.slots, &candidates);
    let text = view::stock_reminder(
        &schedule.categories,
        &counts,
        unknown_attempts,
        pending_reviews,
        &forecast,
    );
    Ok((text, None))
}

#[derive(Debug, thiserror::Error)]
enum DeliveryError {
    #[error(transparent)]
    Telegram(#[from] RequestError),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

/// Telegram API 错误（机器人被屏蔽、聊天已删除）重试也不会消失。
fn is_retryable(error: &DeliveryError) -> bool {
    !matches!(error, DeliveryError::Telegram(RequestError::Api(_)))
}
