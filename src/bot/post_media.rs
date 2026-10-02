//! 「编辑图文」：编辑帖子的文字，或替换帖子的媒体。
//!
//! 点「编辑图文」后，控制消息上的按钮换成可以编辑的内容：编辑文字、替换全部媒体，
//! 多于一项媒体时还有每一项。选好后 Bot 发一条要求回复的提示，按钮换回原样。
//! 回复图片、视频或文件就替换对应的媒体，因此很久以前发的、在 Telegram 里
//! 已经不能再编辑的消息，也能换掉其中的图片。
//!
//! 新媒体成为帖子内容的来源，所以回复会留在聊天里，只删除提示和沿途的说明。

use anyhow::{Context, Result};
use teloxide::{
    prelude::*,
    types::{CallbackQuery, ForceReply, Message, MessageId},
};

use super::{
    edit::{self, Input, PROMPT_MARK, PendingInput},
    post_text, reply, replying_to, send_now, view,
};
use crate::{
    app::App,
    model::{IncomingMessage, PostId, Target},
    store::{ReplaceMediaOutcome, ReplaceMediaRejection},
    telegram,
};

/// 点「编辑图文」：把按钮换成可以编辑的内容。纯文字帖子没有媒体，直接编辑文字。
pub async fn open_menu(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    if let Some(alert) = send_now::refusal(app, &query, post).await? {
        app.bot
            .answer_callback_query(query.id)
            .text(alert)
            .show_alert(true)
            .await?;
        return Ok(());
    }
    let items = media_items(app, post).await?;
    if items == 0 {
        return post_text::ask(app, query, post).await;
    }
    app.bot.answer_callback_query(query.id.clone()).await?;
    if let Some(message) = &query.message {
        app.bot
            .edit_message_reply_markup(message.chat().id, message.id())
            .reply_markup(view::edit_post_keyboard(post, items))
            .await?;
    }
    Ok(())
}

/// 菜单里的「编辑文字」：按钮换回原样，再发出编辑文字的提示。
pub async fn edit_text(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    restore_card(app, &query, post).await;
    post_text::ask(app, query, post).await
}

/// 菜单里的「替换媒体」或「第 N 项」：按钮换回原样，发出要求回复新媒体的提示。
pub async fn ask(app: &App, query: CallbackQuery, post: PostId, item: Option<usize>) -> Result<()> {
    let admin = query.from.id;
    let items = media_items(app, post).await?;
    let alert = match send_now::refusal(app, &query, post).await? {
        Some(alert) => Some(alert),
        None if items == 0 => Some(view::replace_rejected(
            post,
            ReplaceMediaRejection::TextOnly,
        )),
        None if item.is_some_and(|item| item >= items) => Some(view::replace_rejected(
            post,
            ReplaceMediaRejection::ItemGone,
        )),
        None => None,
    };
    restore_card(app, &query, post).await;
    if let Some(alert) = alert {
        app.bot
            .answer_callback_query(query.id)
            .text(alert)
            .show_alert(true)
            .await?;
        return Ok(());
    }
    app.bot.answer_callback_query(query.id.clone()).await?;
    let Some(card) = &query.message else {
        return Ok(());
    };
    let chat = card.chat().id;
    let prompt = app
        .bot
        .send_message(
            chat,
            format!(
                "{PROMPT_MARK}{}",
                view::replace_media_prompt(post, item, items)
            ),
        )
        .reply_parameters(replying_to(card.id()))
        .reply_markup(ForceReply::new())
        .await?;
    app.inputs.insert(
        (chat, prompt.id),
        PendingInput::new(admin, Input::PostMedia { post, item }, (chat, card.id())),
    );
    Ok(())
}

/// 管理员回复了替换媒体的提示。相册的各条消息分别到达，交给收集器凑齐后
/// 由 [`finish`] 一起处理。
pub async fn accept(
    app: &App,
    message: &Message,
    prompt: MessageId,
    pending: &PendingInput,
    post: PostId,
    item: Option<usize>,
) -> Result<()> {
    let incoming = telegram::incoming_message(message, pending.admin)
        .filter(|incoming| incoming.content.media.is_some());
    let Some(mut incoming) = incoming else {
        return edit::complain(app, message, prompt, view::MEDIA_EXPECTED).await;
    };
    incoming.target = Some(Target::Replace { post, item, prompt });
    app.collector.push(incoming);
    Ok(())
}

/// 用回复的媒体替换帖子的媒体。
pub async fn finish(
    app: &App,
    batch: &[IncomingMessage],
    post: PostId,
    item: Option<usize>,
    prompt: MessageId,
) -> Result<()> {
    let first = batch.first().context("empty replacement batch")?;
    let chat = first.source_chat_id;
    let replies: Vec<_> = batch
        .iter()
        .map(|message| message.source_message_id)
        .collect();
    let Some(pending) = app.inputs.get(chat, prompt) else {
        return reply(
            app,
            chat,
            first.source_message_id,
            edit::expired_prompt_notice(),
        )
        .await;
    };
    if item.is_some() && batch.len() > 1 {
        return edit::complain_about(app, chat, &replies, prompt, view::ONE_ITEM_ONLY).await;
    }
    let outcome = app.store.replace_media(post, item, batch).await?;
    let failure = match outcome {
        ReplaceMediaOutcome::Replaced {
            total,
            text_replaced,
        } => {
            tracing::info!(
                "Admin {} replaced the media of post {} ({} item(s))",
                pending.admin.0,
                post.0,
                batch.len()
            );
            // 新媒体已经是帖子的内容，留在聊天里。
            edit::dismiss_prompt(app, chat, prompt, &pending, &[]).await;
            let notice = view::media_replaced(post, item, total, text_replaced);
            return reply(app, chat, first.source_message_id, &notice).await;
        }
        ReplaceMediaOutcome::Rejected(
            reason @ (ReplaceMediaRejection::TooManyItems | ReplaceMediaRejection::NotAlbum),
        ) => {
            let notice = view::replace_rejected(post, reason);
            return edit::complain_about(app, chat, &replies, prompt, &notice).await;
        }
        ReplaceMediaOutcome::Duplicate => {
            tracing::info!("Ignoring already stored media for post {}", post.0);
            return Ok(());
        }
        ReplaceMediaOutcome::Rejected(reason) => view::replace_rejected(post, reason),
        ReplaceMediaOutcome::NotFound => view::post_not_found(post),
        ReplaceMediaOutcome::Locked(status) => view::locked(post, status),
    };
    // 没法再替换了：提示作废，回复的媒体也一并清理。
    edit::dismiss_prompt(app, chat, prompt, &pending, &replies).await;
    super::send(app, chat, &failure).await
}

/// 帖子有几项媒体；纯文字帖子是 0。
async fn media_items(app: &App, post: PostId) -> Result<usize> {
    let messages = app
        .store
        .load_post(post)
        .await?
        .map(|loaded| loaded.messages)
        .unwrap_or_default();
    let all_media = messages.iter().all(|content| content.media.is_some());
    Ok(if all_media { messages.len() } else { 0 })
}

/// 把控制消息的按钮换回帖子现在该有的。失败只记录日志：按钮可能本来就是这样。
async fn restore_card(app: &App, query: &CallbackQuery, post: PostId) {
    if let Err(error) = send_now::show_keyboard(app, query, post).await {
        tracing::debug!(
            "Failed to restore the buttons of post {}: {error:#}",
            post.0
        );
    }
}

#[cfg(test)]
mod tests {
    use jiff::Timestamp;
    use serde_json::json;

    use super::*;
    use crate::{
        bot::{
            callback::CallbackData,
            test_support::{
                CHAT, admin_callback, admin_message, bodies_of, fixture, mock_method,
                mock_sequence, sent_text,
            },
        },
        fetch::Fetcher,
        model::MediaKind,
        store::test_support::{incoming, incoming_media},
    };

    async fn draft(app: &App, messages: &[IncomingMessage]) -> PostId {
        app.store
            .create_draft(messages, Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap()
    }

    async fn album(app: &App) -> PostId {
        draft(
            app,
            &[
                incoming_media(10, MediaKind::Photo, Some("caption")),
                incoming_media(11, MediaKind::Photo, None),
            ],
        )
        .await
    }

    /// 点「第 `item` 项」，Bot 发出提示（消息 501）之后，取出等待回复的输入。
    async fn press(app: &App, post: PostId, item: Option<usize>) -> PendingInput {
        let data = CallbackData::ReplaceMedia { post, item }.encode();
        ask(app, admin_callback(&data), post, item).await.unwrap();
        app.inputs
            .get(ChatId(CHAT), MessageId(501))
            .expect("the prompt is waiting for a reply")
    }

    async fn mock_buttons(server: &wiremock::MockServer) {
        mock_method(server, "answercallbackquery", json!(true), None).await;
        mock_method(server, "editmessagereplymarkup", sent_text(7), None).await;
    }

    #[tokio::test]
    async fn the_menu_lists_every_item_of_an_album() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_buttons(&fixture.server).await;
        let post = album(app).await;

        let data = CallbackData::EditPost { post }.encode();
        open_menu(app, admin_callback(&data), post).await.unwrap();

        let edits = bodies_of(&fixture.server, "editmessagereplymarkup").await;
        assert_eq!(edits.len(), 1);
        assert!(edits[0].contains("替换全部媒体"), "{}", edits[0]);
        assert!(edits[0].contains("第 2 项"), "{}", edits[0]);
    }

    #[tokio::test]
    async fn a_text_post_goes_straight_to_the_text_prompt() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_buttons(&fixture.server).await;
        mock_sequence(&fixture.server, "sendmessage", 501, Some(1)).await;
        let post = draft(app, &[incoming(10, "hello")]).await;

        let data = CallbackData::EditPost { post }.encode();
        open_menu(app, admin_callback(&data), post).await.unwrap();

        let pending = app.inputs.get(ChatId(CHAT), MessageId(501)).unwrap();
        assert_eq!(pending.input, Input::PostText(post));
        assert!(
            bodies_of(&fixture.server, "editmessagereplymarkup")
                .await
                .is_empty()
        );
    }

    #[tokio::test]
    async fn choosing_an_item_asks_for_the_new_media() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_buttons(&fixture.server).await;
        mock_sequence(&fixture.server, "sendmessage", 501, Some(1)).await;
        let post = album(app).await;

        let pending = press(app, post, Some(1)).await;

        assert_eq!(
            pending.input,
            Input::PostMedia {
                post,
                item: Some(1)
            }
        );
        let sent = bodies_of(&fixture.server, "sendmessage").await;
        assert!(sent[0].contains("force_reply"), "{}", sent[0]);
        assert!(sent[0].contains("第 2 项"), "{}", sent[0]);
    }

    #[tokio::test]
    async fn an_item_that_is_gone_gets_an_alert_and_no_prompt() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_buttons(&fixture.server).await;
        mock_method(&fixture.server, "sendmessage", sent_text(501), Some(0)).await;
        let post = album(app).await;

        let data = CallbackData::ReplaceMedia {
            post,
            item: Some(5),
        }
        .encode();
        ask(app, admin_callback(&data), post, Some(5))
            .await
            .unwrap();

        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains("show_alert"), "{}", answers[0]);
    }

    #[tokio::test]
    async fn the_reply_replaces_the_item_and_stays_in_the_chat() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_buttons(&fixture.server).await;
        mock_sequence(&fixture.server, "sendmessage", 501, Some(2)).await;
        // 只删提示，新媒体留在聊天里。
        mock_method(&fixture.server, "deletemessage", json!(true), Some(1)).await;
        let post = album(app).await;
        press(app, post, Some(1)).await;

        let reply = incoming_media(600, MediaKind::Video, None);
        finish(app, &[reply], post, Some(1), MessageId(501))
            .await
            .unwrap();

        let loaded = app.store.load_post(post).await.unwrap().unwrap();
        let media = loaded.messages[1].media.as_ref().unwrap();
        assert_eq!(media.kind, MediaKind::Video);
        assert_eq!(media.file_id, "file-600");
        assert_eq!(loaded.messages[0].text.as_deref(), Some("caption"));
        let deleted = bodies_of(&fixture.server, "deletemessage").await;
        assert!(deleted[0].contains("501"), "{}", deleted[0]);
        let sent = bodies_of(&fixture.server, "sendmessage").await;
        assert!(sent[1].contains("第 2 项已替换"), "{}", sent[1]);
        assert!(app.inputs.get(ChatId(CHAT), MessageId(501)).is_none());
    }

    #[tokio::test]
    async fn an_album_cannot_replace_a_single_item() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_buttons(&fixture.server).await;
        mock_sequence(&fixture.server, "sendmessage", 501, Some(2)).await;
        let post = album(app).await;
        press(app, post, Some(0)).await;

        let replies = [
            incoming_media(600, MediaKind::Photo, None),
            incoming_media(601, MediaKind::Photo, None),
        ];
        finish(app, &replies, post, Some(0), MessageId(501))
            .await
            .unwrap();

        let loaded = app.store.load_post(post).await.unwrap().unwrap();
        assert_eq!(
            loaded.messages[0].media.as_ref().unwrap().file_id,
            "file-10"
        );
        let sent = bodies_of(&fixture.server, "sendmessage").await;
        assert!(sent[1].contains("只能替换一项"), "{}", sent[1]);
        // 提示仍然有效，可以重新回复。
        assert!(app.inputs.get(ChatId(CHAT), MessageId(501)).is_some());
    }

    #[tokio::test]
    async fn a_text_reply_keeps_the_prompt_open() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = &fixture.app;
        mock_buttons(&fixture.server).await;
        mock_sequence(&fixture.server, "sendmessage", 501, Some(2)).await;
        let post = album(app).await;
        let pending = press(app, post, None).await;

        let reply = admin_message(600, "not media");
        accept(app, &reply, MessageId(501), &pending, post, None)
            .await
            .unwrap();

        let sent = bodies_of(&fixture.server, "sendmessage").await;
        assert!(sent[1].contains("请回复图片"), "{}", sent[1]);
        assert!(app.inputs.get(ChatId(CHAT), MessageId(501)).is_some());
    }
}
