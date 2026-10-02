//! 立即发送：不等发布时段，马上把草稿或已入队的帖子发到频道。
//!
//! 点「立即发送」先把按钮换成确认。确认后帖子改为发布中，再发到频道，
//! 发布尝试和定时发布一样记录在数据库里，只是不属于任何时段。

use anyhow::Result;
use jiff::Timestamp;
use teloxide::{
    prelude::*,
    types::{CallbackQuery, InlineKeyboardMarkup},
};

use super::{clear_keyboard, replace_card, view};
use crate::{
    app::App,
    model::{AttemptId, PostId},
    store::AttemptOutcome,
    telegram::publish_post,
};

/// 点「立即发送」：帖子还能发就把按钮换成确认。
pub async fn ask(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    if let Some(alert) = refusal(app, &query, post).await? {
        app.bot
            .answer_callback_query(query.id)
            .text(alert)
            .show_alert(true)
            .await?;
        return Ok(());
    }
    app.bot
        .answer_callback_query(query.id)
        .text(view::send_now_question(post))
        .await?;
    if let Some(message) = &query.message {
        app.bot
            .edit_message_reply_markup(message.chat().id, message.id())
            .reply_markup(view::send_now_confirm_keyboard(post))
            .await?;
    }
    Ok(())
}

/// 点「立即发送」或「取消投稿」的确认旁边的「返回」：换回帖子原来的按钮。
pub async fn back(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    app.bot.answer_callback_query(query.id.clone()).await?;
    show_keyboard(app, &query, post).await
}

/// 确认立即发送：占住帖子，发到频道，再把结果告诉管理员。
pub async fn confirm(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    let started = app
        .store
        .begin_send_now(post, query.from.id, Timestamp::now())
        .await?;
    let Some(attempt) = started else {
        app.bot
            .answer_callback_query(query.id)
            .text(view::ALREADY_HANDLED)
            .await?;
        if let Some(message) = &query.message {
            clear_keyboard(&app.bot, message.chat().id, message.id()).await;
        }
        return Ok(());
    };

    let outcome =
        AttemptOutcome::from(publish_post(&app.bot, &app.config.channel, &attempt.post).await);
    let report = Report::of(post, &outcome);
    app.store
        .finish_attempt(attempt.id, &attempt.post, outcome, Timestamp::now())
        .await?;

    let alert = !matches!(report, Report::Sent);
    app.bot
        .answer_callback_query(query.id.clone())
        .text(report.toast(post))
        .show_alert(alert)
        .await?;
    match report.card(post, attempt.id) {
        Some((text, keyboard)) => {
            if let Some(message) = &query.message {
                replace_card(&app.bot, message.chat().id, message.id(), text, keyboard).await;
            }
        }
        None => show_keyboard(app, &query, post).await?,
    }
    Ok(())
}

/// 这次发送的结果，以及要告诉管理员的话。
enum Report {
    Sent,
    /// 频道暂时不可用，帖子没有发出去，回到发送前的样子。
    ChannelUnavailable,
    Rejected(String),
    Unknown,
}

impl Report {
    fn of(post: PostId, outcome: &AttemptOutcome) -> Self {
        match outcome {
            AttemptOutcome::Succeeded(sent) => {
                tracing::info!(
                    "Post {} was sent right away as {} channel message(s)",
                    post.0,
                    sent.len()
                );
                Self::Sent
            }
            AttemptOutcome::Requeued(error) => {
                tracing::warn!(
                    "Post {} was not sent because the channel is unavailable: {error}",
                    post.0
                );
                Self::ChannelUnavailable
            }
            AttemptOutcome::Failed(error) => {
                tracing::error!("Post {} was rejected: {error}", post.0);
                Self::Rejected(error.clone())
            }
            AttemptOutcome::Unknown(error) => {
                tracing::error!(
                    "Outcome of sending post {} right away is unknown: {error}",
                    post.0
                );
                Self::Unknown
            }
        }
    }

    fn toast(&self, post: PostId) -> String {
        match self {
            Self::Sent => view::published(post),
            Self::ChannelUnavailable => view::send_now_channel_unavailable(post),
            Self::Rejected(reason) => view::send_now_rejected(post, reason),
            Self::Unknown => view::send_now_unknown(post),
        }
    }

    /// 发送结束后，按钮所在的消息要变成的文字和按钮；为空表示消息保持原样，
    /// 只把按钮换回发送前的。
    fn card(&self, post: PostId, attempt: AttemptId) -> Option<(String, InlineKeyboardMarkup)> {
        match self {
            Self::Sent => Some((view::published(post), InlineKeyboardMarkup::default())),
            Self::ChannelUnavailable => None,
            Self::Rejected(reason) => Some((
                view::send_now_rejected(post, reason),
                InlineKeyboardMarkup::default(),
            )),
            Self::Unknown => Some((
                view::publication_unknown(attempt),
                view::resolve_keyboard(attempt),
            )),
        }
    }
}

/// 帖子不能立即发送或取消时，要弹窗告诉管理员的话。
pub(super) async fn refusal(
    app: &App,
    query: &CallbackQuery,
    post: PostId,
) -> Result<Option<String>> {
    let summary = app
        .store
        .post_summary(post)
        .await?
        .filter(|summary| summary.owner == Some(query.from.id));
    Ok(match summary {
        None => Some(view::post_not_found(post)),
        Some(summary) if !summary.status.is_editable() => Some(view::locked(post, summary.status)),
        Some(_) => None,
    })
}

/// 把按钮所在消息的按钮换成帖子现在该有的，帖子已经不能改了就去掉按钮。
pub(super) async fn show_keyboard(app: &App, query: &CallbackQuery, post: PostId) -> Result<()> {
    let admin = query.from.id;
    let summary = app
        .store
        .post_summary(post)
        .await?
        .filter(|summary| summary.owner == Some(admin) && summary.status.is_editable());
    let Some(message) = &query.message else {
        return Ok(());
    };
    let (chat, id) = (message.chat().id, message.id());
    match summary {
        Some(summary) => {
            let categories = app.store.categories(admin).await?;
            app.bot
                .edit_message_reply_markup(chat, id)
                .reply_markup(view::edit_keyboard(
                    &categories,
                    &summary,
                    app.ai.is_enabled(),
                ))
                .await?;
        }
        None => clear_keyboard(&app.bot, chat, id).await,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        bot::{
            callback::CallbackData,
            test_support::{
                Fixture, admin_callback, bodies_of, fixture, mock_failure, mock_method,
                mock_sequence, sent_text,
            },
        },
        fetch::Fetcher,
        model::PostStatus,
        store::test_support::{ADMIN, incoming, queue},
    };

    async fn draft(app: &App) -> PostId {
        app.store
            .create_draft(
                &[incoming(10, "hello channel")],
                Timestamp::UNIX_EPOCH,
                None,
            )
            .await
            .unwrap()
            .unwrap()
    }

    async fn status_of(app: &App, post: PostId) -> PostStatus {
        app.store.post_summary(post).await.unwrap().unwrap().status
    }

    fn click(data: CallbackData) -> CallbackQuery {
        admin_callback(&data.encode())
    }

    async fn setup() -> Fixture {
        let fixture = fixture(Fetcher::unavailable()).await;
        mock_method(&fixture.server, "answercallbackquery", json!(true), Some(1)).await;
        fixture
    }

    #[tokio::test]
    async fn asking_swaps_the_buttons_for_a_confirmation() {
        let fixture = setup().await;
        let app = &fixture.app;
        mock_method(
            &fixture.server,
            "editmessagereplymarkup",
            sent_text(7),
            Some(1),
        )
        .await;
        let post = draft(app).await;

        ask(app, click(CallbackData::SendNow { post }), post)
            .await
            .unwrap();

        let edits = bodies_of(&fixture.server, "editmessagereplymarkup").await;
        assert!(edits[0].contains(&format!("y:{}", post.0)), "{}", edits[0]);
        assert!(edits[0].contains(&format!("b:{}", post.0)), "{}", edits[0]);
        assert!(bodies_of(&fixture.server, "sendmessage").await.is_empty());
        assert_eq!(status_of(app, post).await, PostStatus::Draft);
    }

    #[tokio::test]
    async fn asking_about_a_post_that_cannot_be_sent_only_shows_a_warning() {
        let fixture = setup().await;
        let app = &fixture.app;
        let post = draft(app).await;
        assert!(app.store.cancel_post(post, ADMIN).await.unwrap());

        ask(app, click(CallbackData::SendNow { post }), post)
            .await
            .unwrap();

        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains("已取消"), "{}", answers[0]);
        assert!(
            bodies_of(&fixture.server, "editmessagereplymarkup")
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn going_back_restores_the_buttons_of_the_post() {
        let fixture = setup().await;
        let app = &fixture.app;
        mock_method(
            &fixture.server,
            "editmessagereplymarkup",
            sent_text(7),
            Some(1),
        )
        .await;
        let post = draft(app).await;

        back(app, click(CallbackData::BackToPost { post }), post)
            .await
            .unwrap();

        let edits = bodies_of(&fixture.server, "editmessagereplymarkup").await;
        assert!(edits[0].contains("立即发送"), "{}", edits[0]);
        assert!(edits[0].contains("取消投稿"), "{}", edits[0]);
        assert_eq!(status_of(app, post).await, PostStatus::Draft);
    }

    #[tokio::test]
    async fn confirming_sends_a_draft_that_has_no_category_yet() {
        let fixture = setup().await;
        let app = &fixture.app;
        mock_sequence(&fixture.server, "sendmessage", 700, Some(1)).await;
        mock_method(&fixture.server, "editmessagetext", sent_text(7), Some(1)).await;
        let post = draft(app).await;

        confirm(app, click(CallbackData::ConfirmSendNow { post }), post)
            .await
            .unwrap();

        assert_eq!(status_of(app, post).await, PostStatus::Published);
        assert!(app.store.channel_message_of(post).await.unwrap().is_some());
        let sent = bodies_of(&fixture.server, "sendmessage").await;
        assert!(sent[0].contains("hello channel"), "{}", sent[0]);
        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains("已发布"), "{}", answers[0]);
        let cards = bodies_of(&fixture.server, "editmessagetext").await;
        assert!(cards[0].contains("已发布"), "{}", cards[0]);
    }

    #[tokio::test]
    async fn confirming_sends_a_queued_post_without_waiting_for_a_slot() {
        let fixture = setup().await;
        let app = &fixture.app;
        mock_sequence(&fixture.server, "sendmessage", 700, Some(1)).await;
        mock_method(&fixture.server, "editmessagetext", sent_text(7), Some(1)).await;
        let post = draft(app).await;
        queue(&app.store, post, "gi", Timestamp::UNIX_EPOCH).await;

        confirm(app, click(CallbackData::ConfirmSendNow { post }), post)
            .await
            .unwrap();

        assert_eq!(status_of(app, post).await, PostStatus::Published);
        assert!(app.store.queued_candidates(ADMIN).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_unavailable_channel_leaves_the_draft_as_it_was() {
        let fixture = setup().await;
        let app = &fixture.app;
        mock_failure(
            &fixture.server,
            "sendmessage",
            "Bad Request: chat not found",
        )
        .await;
        mock_method(
            &fixture.server,
            "editmessagereplymarkup",
            sent_text(7),
            Some(1),
        )
        .await;
        let post = draft(app).await;

        confirm(app, click(CallbackData::ConfirmSendNow { post }), post)
            .await
            .unwrap();

        assert_eq!(status_of(app, post).await, PostStatus::Draft);
        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains("频道暂时不可用"), "{}", answers[0]);
        let edits = bodies_of(&fixture.server, "editmessagereplymarkup").await;
        assert!(edits[0].contains("立即发送"), "{}", edits[0]);
    }

    #[tokio::test]
    async fn a_rejected_post_is_reported_and_marked_failed() {
        let fixture = setup().await;
        let app = &fixture.app;
        mock_failure(
            &fixture.server,
            "sendmessage",
            "Bad Request: message is too long",
        )
        .await;
        mock_method(&fixture.server, "editmessagetext", sent_text(7), Some(1)).await;
        let post = draft(app).await;

        confirm(app, click(CallbackData::ConfirmSendNow { post }), post)
            .await
            .unwrap();

        assert_eq!(status_of(app, post).await, PostStatus::Failed);
        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains("发送失败"), "{}", answers[0]);
        let cards = bodies_of(&fixture.server, "editmessagetext").await;
        assert!(cards[0].contains("发送失败"), "{}", cards[0]);
    }

    #[tokio::test]
    async fn a_post_that_was_already_handled_is_not_sent() {
        let fixture = setup().await;
        let app = &fixture.app;
        mock_method(
            &fixture.server,
            "editmessagereplymarkup",
            sent_text(7),
            Some(1),
        )
        .await;
        let post = draft(app).await;
        assert!(app.store.cancel_post(post, ADMIN).await.unwrap());

        confirm(app, click(CallbackData::ConfirmSendNow { post }), post)
            .await
            .unwrap();

        assert_eq!(status_of(app, post).await, PostStatus::Cancelled);
        assert!(bodies_of(&fixture.server, "sendmessage").await.is_empty());
        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains(view::ALREADY_HANDLED), "{}", answers[0]);
    }
}
