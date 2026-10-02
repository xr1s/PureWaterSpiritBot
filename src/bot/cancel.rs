//! 取消投稿：放弃尚未发布的草稿或已入队的帖子。
//!
//! 点「取消投稿」先把按钮换成确认，确认后帖子改为已取消，不能恢复。

use anyhow::Result;
use teloxide::{
    prelude::*,
    types::{CallbackQuery, InlineKeyboardMarkup},
};

use super::{clear_keyboard, replace_card, send_now, view};
use crate::{app::App, model::PostId};

/// 点「取消投稿」：帖子还能取消就把按钮换成确认。
pub async fn ask(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    if let Some(alert) = send_now::refusal(app, &query, post).await? {
        app.bot
            .answer_callback_query(query.id)
            .text(alert)
            .show_alert(true)
            .await?;
        return Ok(());
    }
    app.bot
        .answer_callback_query(query.id)
        .text(view::cancel_question(post))
        .await?;
    if let Some(message) = &query.message {
        app.bot
            .edit_message_reply_markup(message.chat().id, message.id())
            .reply_markup(view::cancel_confirm_keyboard(post))
            .await?;
    }
    Ok(())
}

/// 确认取消投稿：帖子改为已取消，按钮所在的消息改成取消的结果。
pub async fn confirm(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    let cancelled = app.store.cancel_post(post, query.from.id).await?;
    let answer = if cancelled {
        tracing::info!("Post {} cancelled", post.0);
        view::post_cancelled(post)
    } else {
        view::ALREADY_HANDLED.to_owned()
    };
    app.bot
        .answer_callback_query(query.id.clone())
        .text(answer.clone())
        .await?;
    if let Some(message) = &query.message {
        let (chat, id) = (message.chat().id, message.id());
        if cancelled {
            replace_card(&app.bot, chat, id, answer, InlineKeyboardMarkup::default()).await;
        } else {
            clear_keyboard(&app.bot, chat, id).await;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use serde_json::json;

    use super::*;
    use crate::{
        bot::{
            callback::CallbackData,
            test_support::{Fixture, admin_callback, bodies_of, fixture, mock_method, sent_text},
        },
        fetch::Fetcher,
        model::PostStatus,
        store::test_support::{ADMIN, incoming},
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

        ask(app, click(CallbackData::Cancel { post }), post)
            .await
            .unwrap();

        let edits = bodies_of(&fixture.server, "editmessagereplymarkup").await;
        assert!(edits[0].contains(&format!("x:{}", post.0)), "{}", edits[0]);
        assert!(edits[0].contains(&format!("b:{}", post.0)), "{}", edits[0]);
        assert_eq!(status_of(app, post).await, PostStatus::Draft);
    }

    #[tokio::test]
    async fn asking_about_a_post_that_cannot_be_cancelled_only_shows_a_warning() {
        let fixture = setup().await;
        let app = &fixture.app;
        let post = draft(app).await;
        assert!(app.store.cancel_post(post, ADMIN).await.unwrap());

        ask(app, click(CallbackData::Cancel { post }), post)
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
    async fn confirming_cancels_the_post() {
        let fixture = setup().await;
        let app = &fixture.app;
        mock_method(&fixture.server, "editmessagetext", sent_text(7), Some(1)).await;
        let post = draft(app).await;

        confirm(app, click(CallbackData::ConfirmCancel { post }), post)
            .await
            .unwrap();

        assert_eq!(status_of(app, post).await, PostStatus::Cancelled);
        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains("已取消"), "{}", answers[0]);
        let cards = bodies_of(&fixture.server, "editmessagetext").await;
        assert!(cards[0].contains("已取消"), "{}", cards[0]);
    }

    #[tokio::test]
    async fn confirming_a_post_that_was_already_handled_changes_nothing() {
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

        confirm(app, click(CallbackData::ConfirmCancel { post }), post)
            .await
            .unwrap();

        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains(view::ALREADY_HANDLED), "{}", answers[0]);
        assert!(
            bodies_of(&fixture.server, "editmessagetext")
                .await
                .is_empty()
        );
    }
}
