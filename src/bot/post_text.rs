//! 编辑帖子的文字。
//!
//! 点「编辑图文」里的「编辑文字」（纯文字帖子直接点「编辑图文」）后，
//! Bot 发一条要求回复的提示。管理员可以直接从帖子消息的文字
//! （图片说明）上复制原文，回复新文字，Bot 替换帖子的文字，
//! 把结果同步到帖子第一条消息上（只有 Bot 自己发的消息才能改），
//! 并删掉提示和管理员的回复。

use anyhow::Result;
use teloxide::{
    ApiError, RequestError,
    prelude::*,
    types::{CallbackQuery, ForceReply, Message, MessageId},
};

use super::{
    edit::{self, Input, PROMPT_MARK, PendingInput},
    replying_to, send, view,
};
use crate::{app::App, model::PostId, store::EditOutcome, telegram};

/// 响应「编辑文字」按钮：发出当前文字和要求回复的提示。
pub async fn ask(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    let admin = query.from.id;
    let summary = app
        .store
        .post_summary(post)
        .await?
        .filter(|summary| summary.owner == Some(admin));
    let alert = match &summary {
        None => Some(view::post_not_found(post)),
        Some(summary) if !summary.status.is_editable() => Some(view::locked(post, summary.status)),
        Some(_) => None,
    };
    let Some(card) = &query.message else {
        app.bot.answer_callback_query(query.id).await?;
        return Ok(());
    };
    if let Some(alert) = alert {
        app.bot
            .answer_callback_query(query.id)
            .text(alert)
            .show_alert(true)
            .await?;
        return Ok(());
    }
    app.bot.answer_callback_query(query.id).await?;

    let chat = card.chat().id;
    let current = app
        .store
        .load_post(post)
        .await?
        .and_then(|loaded| loaded.messages.into_iter().next())
        .filter(|content| content.text.is_some());
    let hint = if current.is_some() {
        "\n想在原文上修改：长按投稿消息的文字（图片说明）复制，粘贴到回复里再改。"
    } else {
        "\n这篇投稿现在没有文字。"
    };
    let prompt = app
        .bot
        .send_message(
            chat,
            format!(
                "{PROMPT_MARK}请回复本消息，发送投稿 #{} 新的文字，会替换原来的文字。{hint}",
                post.0
            ),
        )
        .reply_parameters(replying_to(card.id()))
        .reply_markup(ForceReply::new())
        .await?;
    app.inputs.insert(
        (chat, prompt.id),
        PendingInput::new(admin, Input::PostText(post), (chat, card.id())),
    );
    Ok(())
}

/// 管理员回复了提示：用他发的文字替换帖子的文字。
pub async fn finish(
    app: &App,
    message: &Message,
    prompt: MessageId,
    pending: &PendingInput,
    post: PostId,
) -> Result<()> {
    let Some(incoming) = telegram::incoming_message(message, pending.admin) else {
        return edit::complain(app, message, prompt, "请回复文字。").await;
    };
    let chat = message.chat.id;
    let outcome = app.store.replace_text(post, &incoming.content).await?;
    let notice = match outcome {
        EditOutcome::Done(_) => {
            tracing::info!(
                "Admin {} edited the text of post {}",
                pending.admin.0,
                post.0
            );
            match refresh_preview(app, chat, post).await {
                Preview::NotEditable => Some(view::text_replaced_but_not_shown(post)),
                Preview::Shown | Preview::Elsewhere => None,
            }
        }
        EditOutcome::NotFound => Some(view::post_not_found(post)),
        EditOutcome::Locked(post, status) => Some(view::locked(post, status)),
    };
    edit::close_prompt(app, message, prompt, pending).await;
    match notice {
        Some(notice) => send(app, chat, &notice).await,
        None => Ok(()),
    }
}

pub(super) enum Preview {
    /// 帖子第一条消息上的文字已经改成新的。
    Shown,
    /// 第一条消息不在这个聊天里（比如普通用户的投稿），没有什么可同步的。
    Elsewhere,
    /// 第一条消息不是 Bot 发的，Telegram 不允许 Bot 改它。
    NotEditable,
}

/// 把帖子第一条消息上显示的文字改成帖子现在的文字。
pub(super) async fn refresh_preview(app: &App, chat: ChatId, post: PostId) -> Preview {
    match try_refresh_preview(app, chat, post).await {
        Ok(preview) => preview,
        Err(error) => {
            tracing::warn!(
                "Failed to refresh the preview of post {}: {error:#}",
                post.0
            );
            Preview::NotEditable
        }
    }
}

async fn try_refresh_preview(app: &App, chat: ChatId, post: PostId) -> Result<Preview> {
    let first = app
        .store
        .first_message(post)
        .await?
        .filter(|(source_chat, _)| *source_chat == chat);
    let loaded = app.store.load_post(post).await?;
    let (Some((_, message)), Some(content)) = (
        first,
        loaded.as_ref().and_then(|loaded| loaded.messages.first()),
    ) else {
        return Ok(Preview::Elsewhere);
    };
    match telegram::edit_preview(&app.bot, chat, message, content).await {
        Ok(()) | Err(RequestError::Api(ApiError::MessageNotModified)) => Ok(Preview::Shown),
        Err(error) => {
            tracing::info!("Message {} cannot be edited: {error}", message.0);
            Ok(Preview::NotEditable)
        }
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use serde_json::json;
    use teloxide::types::{MessageEntityKind, UserId};

    use super::*;
    use crate::{
        bot::{
            callback::CallbackData,
            test_support::{
                CHAT, admin_callback, admin_message, admin_message_with_entities, bodies_of,
                fixture, mock_failure, mock_method, mock_sequence, sent_text,
            },
        },
        fetch::Fetcher,
        model::MediaKind,
        store::test_support::{ADMIN, incoming_media},
    };

    async fn draft_with_caption(app: &App, caption: &str) -> PostId {
        app.store
            .create_draft(
                &[incoming_media(10, MediaKind::Photo, Some(caption))],
                Timestamp::UNIX_EPOCH,
                None,
            )
            .await
            .unwrap()
            .unwrap()
    }

    /// 点按钮、Bot 发出提示之后，取出等待回复的输入。
    async fn press(app: &App, post: PostId) -> PendingInput {
        let data = CallbackData::EditPostText { post }.encode();
        ask(app, admin_callback(&data), post).await.unwrap();
        app.inputs
            .get(ChatId(CHAT), MessageId(501))
            .expect("the prompt is waiting for a reply")
    }

    #[tokio::test]
    async fn the_button_sends_the_current_text_and_a_prompt() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "answercallbackquery", json!(true), None).await;
        mock_sequence(&fixture.server, "sendmessage", 501, Some(1)).await;
        let post = draft_with_caption(app, "old caption").await;

        let pending = press(app, post).await;

        assert_eq!(pending.input, Input::PostText(post));
        let sent = bodies_of(&fixture.server, "sendmessage").await;
        assert!(sent[0].contains("force_reply"), "{}", sent[0]);
        assert!(sent[0].contains(PROMPT_MARK), "{}", sent[0]);
        assert!(sent[0].contains("图片说明"), "{}", sent[0]);
    }

    #[tokio::test]
    async fn a_post_without_text_only_gets_the_prompt() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "answercallbackquery", json!(true), None).await;
        mock_sequence(&fixture.server, "sendmessage", 501, Some(1)).await;
        let post = app
            .store
            .create_draft(
                &[incoming_media(10, MediaKind::Photo, None)],
                Timestamp::UNIX_EPOCH,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        let pending = press(app, post).await;
        assert_eq!(pending.input, Input::PostText(post));
    }

    #[tokio::test]
    async fn the_reply_replaces_the_text_and_updates_the_preview() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "answercallbackquery", json!(true), None).await;
        mock_sequence(&fixture.server, "sendmessage", 501, Some(1)).await;
        mock_method(
            &fixture.server,
            "editmessagecaption",
            sent_text(10),
            Some(1),
        )
        .await;
        // 提示和管理员的回复。
        mock_method(&fixture.server, "deletemessage", json!(true), Some(2)).await;
        let post = draft_with_caption(app, "old caption").await;
        let pending = press(app, post).await;

        let reply = admin_message_with_entities(
            600,
            "new caption",
            json!([{ "type": "bold", "offset": 0, "length": 3 }]),
        );
        finish(app, &reply, MessageId(501), &pending, post)
            .await
            .unwrap();

        let loaded = app.store.load_post(post).await.unwrap().unwrap();
        let content = &loaded.messages[0];
        assert_eq!(content.text.as_deref(), Some("new caption"));
        assert_eq!(content.entities.len(), 1);
        assert_eq!(content.entities[0].kind, MessageEntityKind::Bold);
        let edits = bodies_of(&fixture.server, "editmessagecaption").await;
        assert!(edits[0].contains("new caption"), "{}", edits[0]);
        assert!(edits[0].contains("caption_entities"), "{}", edits[0]);
        // 输入完成，提示作废。
        assert!(app.inputs.get(ChatId(CHAT), MessageId(501)).is_none());
    }

    #[tokio::test]
    async fn a_message_the_bot_cannot_edit_gets_a_notice() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "answercallbackquery", json!(true), None).await;
        mock_sequence(&fixture.server, "sendmessage", 501, Some(2)).await;
        mock_failure(
            &fixture.server,
            "editmessagecaption",
            "Bad Request: message can't be edited",
        )
        .await;
        mock_method(&fixture.server, "deletemessage", json!(true), None).await;
        let post = draft_with_caption(app, "old caption").await;
        let pending = press(app, post).await;

        let reply = admin_message(600, "new caption");
        finish(app, &reply, MessageId(501), &pending, post)
            .await
            .unwrap();

        // 文字照样替换了，并且告诉管理员显示的文字没有变。
        let loaded = app.store.load_post(post).await.unwrap().unwrap();
        assert_eq!(loaded.messages[0].text.as_deref(), Some("new caption"));
        let sent = bodies_of(&fixture.server, "sendmessage").await;
        assert!(sent[1].contains("不是 Bot 发的"), "{}", sent[1]);
    }

    #[tokio::test]
    async fn a_reply_that_is_not_text_keeps_the_prompt_open() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "answercallbackquery", json!(true), None).await;
        mock_sequence(&fixture.server, "sendmessage", 501, None).await;
        let post = draft_with_caption(app, "old caption").await;
        let pending = press(app, post).await;

        // 一条什么内容都没有的消息，不能当作文字。
        let empty: Message = serde_json::from_value(json!({
            "message_id": 600,
            "date": 1,
            "chat": { "id": CHAT, "type": "private", "first_name": "Alice" },
            "from": { "id": CHAT, "is_bot": false, "first_name": "Alice" },
            "new_chat_title": "x",
        }))
        .unwrap();
        finish(app, &empty, MessageId(501), &pending, post)
            .await
            .unwrap();

        let loaded = app.store.load_post(post).await.unwrap().unwrap();
        assert_eq!(loaded.messages[0].text.as_deref(), Some("old caption"));
        assert!(app.inputs.get(ChatId(CHAT), MessageId(501)).is_some());
    }

    #[tokio::test]
    async fn a_post_that_does_not_exist_gets_an_alert_and_no_prompt() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "answercallbackquery", json!(true), Some(1)).await;
        mock_method(&fixture.server, "sendmessage", sent_text(1), Some(0)).await;

        let post = PostId(999);
        let data = CallbackData::EditPostText { post }.encode();
        ask(app, admin_callback(&data), post).await.unwrap();

        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains("show_alert"), "{}", answers[0]);
    }

    #[tokio::test]
    async fn another_admins_post_cannot_be_edited() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        let other = UserId(77);
        app.store
            .sync_admins(&[ADMIN, other], Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        let mut message = incoming_media(10, MediaKind::Photo, Some("theirs"));
        message.submitter = other;
        let post = app
            .store
            .create_draft(&[message], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap();
        mock_method(&fixture.server, "answercallbackquery", json!(true), Some(1)).await;
        mock_method(&fixture.server, "sendmessage", sent_text(1), Some(0)).await;

        let data = CallbackData::EditPostText { post }.encode();
        ask(app, admin_callback(&data), post).await.unwrap();
    }
}
