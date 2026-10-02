//! 审核群里的投稿审核，以及普通用户这一侧的提交和撤回。
//!
//! 普通用户私聊投稿，草稿确认后进入 `pending_review`。后台任务（见 `post_missing_cards`）
//! 给每篇待审核的投稿在审核群里发一张卡片：转发的投稿内容，加一条带按钮的控制消息。
//! 管理员在群里操作：
//! - 点「通过」，按钮变成自己的分类，选一个就认领这篇投稿；
//! - 点「拒绝」，或回复卡片发 `/reject 理由`，拒绝并通知投稿人；
//! - 回复卡片发一段文字，追加到原文；
//! - `/block`、`/unblock` 拉黑或解除拉黑投稿人，`/pending` 看积压。
//!
//! 群里的任何操作都用发送者的 ID 对照管理员名单，群成员身份不算数。

use std::{collections::HashSet, sync::Arc};

use anyhow::{Result, anyhow};
use jiff::Timestamp;
use teloxide::{
    ApiError, RequestError,
    prelude::*,
    types::{
        CallbackQuery, CallbackQueryId, ChatId, InlineKeyboardMarkup, Message, MessageId,
        Recipient, User,
    },
};

use super::{
    callback::ReviewAction,
    clear_keyboard,
    command::ReviewCommand,
    edit::{self, Input, PendingInput},
    help::{HelpPage, HelpView},
    names, replace_card, reply, replying_to, view,
};
use crate::{
    app::App,
    model::{CategoryId, NotificationSubject, Post, PostId, PostStatus, SubmitterStatus},
    schedule::parse_reason,
    store::{AppendTextOutcome, ApproveOutcome, ClearOutcome, ReviewInfo},
    telegram::{self, publish_post},
};

/// `/pending` 最多列出多少篇。
const PENDING_LIST_LIMIT: i64 = 10;

pub fn is_review_group(app: &App, chat: ChatId) -> bool {
    app.review.as_ref().map(|chat| chat.id) == Some(chat)
}

/// 群里的消息是不是管理员发的。不是就当作没看见。
fn review_admin<'a>(app: &App, message: &'a Message) -> Option<&'a User> {
    message
        .from
        .as_ref()
        .filter(|user| app.config.is_admin(user.id))
}

/// 普通用户现在能不能投稿：`None` 表示没开放投稿，或者他本来就是管理员。
pub async fn submitter_status(app: &App, user: &User) -> Result<Option<SubmitterStatus>> {
    if app.config.review.group.is_none() || app.config.is_admin(user.id) {
        return Ok(None);
    }
    let status = app
        .store
        .register_submitter(user.id, Timestamp::now())
        .await?;
    Ok(Some(status))
}

// ---- 审核卡片 ----

/// 给待审核、但群里还没有卡片的投稿发卡片。`reported` 记下已经报过错的投稿，
/// 避免同一个失败每隔几秒刷一次日志。
pub async fn post_missing_cards(app: &App, reported: &mut HashSet<PostId>) {
    let Some(chat) = &app.review else {
        return;
    };
    let posts = match app.store.posts_without_review_card().await {
        Ok(posts) => posts,
        Err(error) => {
            tracing::error!("Failed to look for posts waiting for a review card: {error:#}");
            return;
        }
    };
    for post in posts {
        match post_card(app, chat.id, post).await {
            Ok(()) => {
                reported.remove(&post);
            }
            Err(error) => {
                if reported.insert(post) {
                    tracing::error!(
                        "Failed to post the review card of post {}: {error:#}",
                        post.0
                    );
                }
            }
        }
    }
}

async fn post_card(app: &App, chat: ChatId, post_id: PostId) -> Result<()> {
    let Some(info) = app.store.review_info(post_id).await? else {
        return Ok(());
    };
    if info.status != PostStatus::PendingReview {
        return Ok(());
    }
    let Some(post) = app.store.load_post(post_id).await? else {
        return Ok(());
    };
    let preview = Post {
        reply_to: None,
        ..post
    };
    let sent = publish_post(&app.bot, &Recipient::Id(chat), &preview)
        .await
        .map_err(|error| anyhow!("failed to send the preview: {error:?}"))?;
    let contents: Vec<_> = sent
        .iter()
        .map(|message| (message.chat_id, message.message_id))
        .collect();
    let first = contents
        .first()
        .map(|(_, message)| *message)
        .ok_or_else(|| anyhow!("the preview has no messages"))?;
    let control = app
        .bot
        .send_message(
            Recipient::Id(chat),
            view::review_card_text(&info, &names::user_label(app, info.submitter).await),
        )
        .reply_parameters(replying_to(first))
        .reply_markup(view::with_auto_tag_button(
            view::review_keyboard(post_id, info.has_media),
            post_id,
            app.ai.is_enabled(),
        ))
        .await?;
    app.store
        .record_review_card(post_id, &contents, (chat, control.id))
        .await?;
    tracing::info!("Posted the review card of post {}", post_id.0);
    Ok(())
}

/// 审核结束后，把审核群里的控制消息改成结果、去掉按钮，也去掉投稿人那边的按钮。
async fn close_card(app: &App, post: PostId, text: impl FnOnce(&ReviewInfo, &str) -> String) {
    if let Err(error) = try_close_card(app, post, text).await {
        tracing::warn!(
            "Failed to update the review card of post {}: {error:#}",
            post.0
        );
    }
}

async fn try_close_card(
    app: &App,
    post: PostId,
    text: impl FnOnce(&ReviewInfo, &str) -> String,
) -> Result<()> {
    let Some(info) = app.store.review_info(post).await? else {
        return Ok(());
    };
    let card = app.store.review_card(post).await?;
    if let Some((chat, message)) = card.control {
        let submitter = names::user_label(app, info.submitter).await;
        replace_card(
            &app.bot,
            chat,
            message,
            text(&info, &submitter),
            InlineKeyboardMarkup::default(),
        )
        .await;
    }
    if let Some((chat, message)) = app.store.control_message(post).await? {
        clear_keyboard(&app.bot, chat, message).await;
    }
    Ok(())
}

/// 把审核群里预览消息的文字刷新成投稿现在的文字。
pub(super) async fn refresh_preview(app: &App, post: PostId) {
    if let Err(error) = try_refresh_preview(app, post).await {
        tracing::warn!(
            "Failed to refresh the preview of post {}: {error:#}",
            post.0
        );
    }
}

async fn try_refresh_preview(app: &App, post: PostId) -> Result<()> {
    let card = app.store.review_card(post).await?;
    let loaded = app.store.load_post(post).await?;
    let (Some((chat, message)), Some(loaded)) = (card.contents.first(), loaded) else {
        return Ok(());
    };
    let Some(content) = loaded.messages.first() else {
        return Ok(());
    };
    match telegram::edit_preview(&app.bot, *chat, *message, content).await {
        Ok(()) | Err(RequestError::Api(ApiError::MessageNotModified)) => Ok(()),
        Err(error) => Err(error.into()),
    }
}

// ---- 处理投稿 ----

/// 拒绝一篇待审核的投稿：通知投稿人，更新卡片。返回 `false` 表示已经被处理过了。
async fn reject(app: &App, post: PostId, admin: &User, note: Option<String>) -> Result<bool> {
    let note = note.or_else(|| app.config.default_reject_note().map(ToOwned::to_owned));
    let now = Timestamp::now();
    let Some(submitter) = app
        .store
        .reject_post(post, admin.id, note.as_deref(), now)
        .await?
    else {
        return Ok(false);
    };
    tracing::info!("Admin {} rejected post {}", admin.id.0, post.0);
    app.store
        .enqueue_notification(
            NotificationSubject::SubmissionRejected(post),
            &[submitter],
            now,
        )
        .await?;
    let name = admin.full_name();
    close_card(app, post, |info, submitter| {
        view::review_rejected(info, submitter, &name, note.as_deref())
    })
    .await;
    Ok(true)
}

async fn approve(
    app: &App,
    post: PostId,
    admin: &User,
    category: CategoryId,
) -> Result<ApproveOutcome> {
    let now = Timestamp::now();
    let outcome = app
        .store
        .approve_post(post, admin.id, category, now)
        .await?;
    if let ApproveOutcome::Approved {
        category,
        submitter,
    } = &outcome
    {
        tracing::info!(
            "Admin {} approved post {} into category {}",
            admin.id.0,
            post.0,
            category.id.0
        );
        app.store
            .enqueue_notification(
                NotificationSubject::SubmissionApproved(post),
                &[*submitter],
                now,
            )
            .await?;
        let name = admin.full_name();
        let label = category.label.clone();
        close_card(app, post, |info, submitter| {
            view::review_approved(info, submitter, &name, &label)
        })
        .await;
    }
    Ok(outcome)
}

// ---- 审核群里的命令和回复 ----

/// 被回复的那条消息属于哪篇投稿的审核卡片。
async fn replied_card(app: &App, message: &Message) -> Result<Option<PostId>> {
    match message.reply_to_message() {
        Some(reply) => app.store.find_review_post(message.chat.id, reply.id).await,
        None => Ok(None),
    }
}

pub async fn handle_command(app: Arc<App>, message: Message, command: ReviewCommand) -> Result<()> {
    let Some(admin) = review_admin(&app, &message) else {
        return Ok(());
    };
    let chat = message.chat.id;
    match command {
        ReviewCommand::Pending => return pending(&app, &message).await,
        ReviewCommand::Help => return help(&app, &message).await,
        _ => {}
    }
    let Some(post) = replied_card(&app, &message).await? else {
        return reply(&app, chat, message.id, view::REVIEW_REPLY_NEEDED).await;
    };
    match command {
        ReviewCommand::Reject(note) => {
            let note = Some(note.trim().to_owned()).filter(|note| !note.is_empty());
            if !reject(&app, post, admin, note).await? {
                return reply(&app, chat, message.id, view::ALREADY_HANDLED).await;
            }
            Ok(())
        }
        ReviewCommand::Block(reason) => {
            let reason = Some(reason.trim().to_owned()).filter(|reason| !reason.is_empty());
            set_blocked(&app, &message, admin, post, true, reason).await
        }
        ReviewCommand::Unblock => set_blocked(&app, &message, admin, post, false, None).await,
        ReviewCommand::Pending | ReviewCommand::Help => Ok(()),
    }
}

async fn help(app: &App, message: &Message) -> Result<()> {
    let menu = HelpView::Menu(HelpPage::Review);
    app.bot
        .send_message(message.chat.id, view::help_text(menu))
        .reply_markup(view::help_keyboard(menu, false))
        .await?;
    Ok(())
}

async fn set_blocked(
    app: &App,
    message: &Message,
    admin: &User,
    post: PostId,
    blocked: bool,
    reason: Option<String>,
) -> Result<()> {
    let chat = message.chat.id;
    let Some(info) = app.store.review_info(post).await? else {
        return reply(app, chat, message.id, view::REVIEW_SUBMITTER_UNKNOWN).await;
    };
    let known = app
        .store
        .set_submitter_blocked(
            info.submitter,
            blocked,
            admin.id,
            reason.as_deref(),
            Timestamp::now(),
        )
        .await?;
    if !known {
        return reply(app, chat, message.id, view::REVIEW_SUBMITTER_UNKNOWN).await;
    }
    tracing::info!(
        "Admin {} {} submitter {}",
        admin.id.0,
        if blocked { "blocked" } else { "unblocked" },
        info.submitter.0
    );
    let name = names::user_name(app, info.submitter).await;
    let text = if blocked {
        view::review_blocked(&name)
    } else {
        view::review_unblocked(&name)
    };
    reply(app, chat, message.id, &text).await
}

async fn pending(app: &App, message: &Message) -> Result<()> {
    let (total, items) = app.store.pending_reviews(PENDING_LIST_LIMIT).await?;
    let mut submitters = Vec::with_capacity(items.len());
    for item in &items {
        submitters.push(names::user_name(app, item.submitter).await);
    }
    let text = view::pending_list(total, &items, &submitters, |item| {
        item.control
            .and_then(|(chat, message)| message_link(chat, message))
    });
    reply(app, message.chat.id, message.id, &text).await
}

/// 超级群里一条消息的链接。只有 `-100` 开头的超级群 ID 才能拼出链接。
fn message_link(chat: ChatId, message: MessageId) -> Option<String> {
    let internal = chat.0.checked_neg()? - 1_000_000_000_000;
    (internal > 0).then(|| format!("https://t.me/c/{internal}/{}", message.0))
}

/// 审核群里不是命令的消息：回复审核卡片并发送文字，追加到原文。
pub async fn handle_reply(app: Arc<App>, message: Message) -> Result<()> {
    let Some(admin) = review_admin(&app, &message) else {
        return Ok(());
    };
    if let Some(replied) = message.reply_to_message() {
        // 回复的是「拒绝并写理由」的提示：这是理由，不是新的原文。
        if let Some(pending) = app.inputs.get(message.chat.id, replied.id) {
            return edit::handle_reply(&app, &message, replied.id, pending).await;
        }
        if edit::is_prompt(replied.text()) {
            return reply(
                &app,
                message.chat.id,
                message.id,
                view::REVIEW_PROMPT_EXPIRED,
            )
            .await;
        }
    }
    let Some(post) = replied_card(&app, &message).await? else {
        return Ok(());
    };
    let chat = message.chat.id;
    // 以 `/` 开头但不是已知命令的文字，不能当作新的原文。
    if message.text().is_some_and(|text| text.starts_with('/')) {
        return Ok(());
    }
    let incoming = match message.text() {
        Some(_) => telegram::incoming_message(&message, admin.id),
        None => None,
    };
    let Some(incoming) = incoming else {
        return reply(&app, chat, message.id, view::REVIEW_TEXT_ONLY).await;
    };
    let outcome = app
        .store
        .append_review_text(post, admin.id, &incoming.content, Timestamp::now())
        .await?;
    let text = match outcome {
        AppendTextOutcome::Appended { .. } => {
            tracing::info!("Admin {} appended text to post {}", admin.id.0, post.0);
            refresh_preview(&app, post).await;
            view::text_appended(post)
        }
        AppendTextOutcome::TooLong { limit } => view::review_text_too_long(limit),
        AppendTextOutcome::NothingNew => return Ok(()),
        AppendTextOutcome::NotFound | AppendTextOutcome::Locked(_) => {
            view::ALREADY_HANDLED.to_owned()
        }
    };
    reply(&app, chat, message.id, &text).await
}

// ---- 审核卡片上的按钮 ----

pub async fn handle_action(app: &App, query: CallbackQuery, action: ReviewAction) -> Result<()> {
    let Some(message) = &query.message else {
        app.bot.answer_callback_query(query.id).await?;
        return Ok(());
    };
    // 只认审核群里的按钮，并且点的人必须是管理员。
    if !is_review_group(app, message.chat().id) || !app.config.is_admin(query.from.id) {
        return alert(app, query.id, view::NOT_ALLOWED).await;
    }
    let (chat, message_id) = (message.chat().id, message.id());
    let admin = &query.from;
    match action {
        ReviewAction::Approve(post) => {
            let Some(info) = pending_info(app, post).await? else {
                return alert(app, query.id, view::ALREADY_HANDLED).await;
            };
            let categories = app.store.categories(admin.id).await?;
            if categories.is_empty() {
                return alert(app, query.id, view::REVIEW_NO_CATEGORIES).await;
            }
            app.bot.answer_callback_query(query.id).await?;
            set_keyboard(
                app,
                chat,
                message_id,
                view::review_category_keyboard(info.id, &categories),
            )
            .await;
            Ok(())
        }
        ReviewAction::Back(post) => {
            let Some(info) = pending_info(app, post).await? else {
                return alert(app, query.id, view::ALREADY_HANDLED).await;
            };
            app.bot.answer_callback_query(query.id).await?;
            set_keyboard(
                app,
                chat,
                message_id,
                view::with_auto_tag_button(
                    view::review_keyboard(post, info.has_media),
                    post,
                    app.ai.is_enabled(),
                ),
            )
            .await;
            Ok(())
        }
        ReviewAction::Category { post, category } => {
            match approve(app, post, admin, category).await? {
                ApproveOutcome::Approved { .. } => {
                    app.bot.answer_callback_query(query.id).await?;
                    Ok(())
                }
                ApproveOutcome::AlreadyHandled => alert(app, query.id, view::ALREADY_HANDLED).await,
                ApproveOutcome::CategoryGone => {
                    let categories = app.store.categories(admin.id).await?;
                    set_keyboard(
                        app,
                        chat,
                        message_id,
                        view::review_category_keyboard(post, &categories),
                    )
                    .await;
                    alert(app, query.id, view::CATEGORY_GONE).await
                }
            }
        }
        ReviewAction::Reject(post) => {
            if reject(app, post, admin, None).await? {
                app.bot.answer_callback_query(query.id).await?;
                Ok(())
            } else {
                alert(app, query.id, view::ALREADY_HANDLED).await
            }
        }
        ReviewAction::RejectWithReason(post) => {
            let Some(info) = pending_info(app, post).await? else {
                return alert(app, query.id, view::ALREADY_HANDLED).await;
            };
            app.bot.answer_callback_query(query.id).await?;
            edit::ask_reason(
                app,
                admin,
                (chat, message_id),
                Input::RejectReason(info.id),
                &view::reject_reason_prompt(info.id),
            )
            .await
        }
        ReviewAction::ClearText(post) => {
            let text = match app
                .store
                .clear_review_text(post, admin.id, Timestamp::now())
                .await?
            {
                ClearOutcome::Cleared => {
                    tracing::info!("Admin {} cleared the text of post {}", admin.id.0, post.0);
                    refresh_preview(app, post).await;
                    app.bot.answer_callback_query(query.id).await?;
                    return Ok(());
                }
                ClearOutcome::NeedsText => "纯文字投稿不能清空文字。",
                ClearOutcome::NotPending => view::ALREADY_HANDLED,
            };
            alert(app, query.id, text).await
        }
    }
}

/// 管理员回复了「拒绝并写理由」的提示：用他写的理由拒绝这篇投稿。
pub async fn finish_reject(
    app: &App,
    message: &Message,
    prompt: MessageId,
    pending: &PendingInput,
    post: PostId,
    text: &str,
) -> Result<()> {
    let reason = match parse_reason(text) {
        Ok(reason) => reason,
        Err(error) => {
            return edit::complain(app, message, prompt, &format!("{error}，请重新回复。")).await;
        }
    };
    let Some(admin) = review_admin(app, message) else {
        return Ok(());
    };
    let rejected = reject(app, post, admin, Some(reason)).await?;
    edit::close_prompt(app, message, prompt, pending).await;
    if rejected {
        Ok(())
    } else {
        reply(app, message.chat.id, message.id, view::ALREADY_HANDLED).await
    }
}

async fn pending_info(app: &App, post: PostId) -> Result<Option<ReviewInfo>> {
    Ok(app
        .store
        .review_info(post)
        .await?
        .filter(|info| info.status == PostStatus::PendingReview))
}

async fn alert(app: &App, query: CallbackQueryId, text: &str) -> Result<()> {
    app.bot
        .answer_callback_query(query)
        .text(text)
        .show_alert(true)
        .await?;
    Ok(())
}

/// 换掉消息的按钮。失败只记录日志：消息可能已经没了，或者按钮没有变化。
async fn set_keyboard(app: &App, chat: ChatId, message: MessageId, keyboard: InlineKeyboardMarkup) {
    if let Err(error) = app
        .bot
        .edit_message_reply_markup(chat, message)
        .reply_markup(keyboard)
        .await
    {
        tracing::warn!(
            "Failed to change the buttons of message {}: {error}",
            message.0
        );
    }
}

// ---- 普通用户的提交和撤回 ----

pub async fn handle_submit(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    match submitter_status(app, &query.from).await? {
        Some(SubmitterStatus::Active) => {}
        Some(SubmitterStatus::Blocked) => {
            return alert(app, query.id, view::SUBMITTER_BLOCKED).await;
        }
        None => return alert(app, query.id, view::NOT_ALLOWED).await,
    }
    if !app
        .store
        .submit_for_review(post, query.from.id, Timestamp::now())
        .await?
    {
        return alert(app, query.id, view::SUBMISSION_CLOSED).await;
    }
    tracing::info!(
        "Post {} submitted for review by {}",
        post.0,
        query.from.id.0
    );
    app.bot.answer_callback_query(query.id).await?;
    if let Some(message) = &query.message {
        replace_card(
            &app.bot,
            message.chat().id,
            message.id(),
            view::submission_submitted(post),
            view::withdraw_keyboard(post),
        )
        .await;
    }
    Ok(())
}

pub async fn handle_withdraw(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    let Some(previous) = app
        .store
        .withdraw_submission(post, query.from.id, Timestamp::now())
        .await?
    else {
        return alert(app, query.id, view::SUBMISSION_CLOSED).await;
    };
    tracing::info!("Post {} withdrawn by {}", post.0, query.from.id.0);
    app.bot.answer_callback_query(query.id).await?;
    if let Some(message) = &query.message {
        replace_card(
            &app.bot,
            message.chat().id,
            message.id(),
            view::submission_withdrawn(post),
            InlineKeyboardMarkup::default(),
        )
        .await;
    }
    if previous == PostStatus::PendingReview {
        close_card(app, post, view::review_withdrawn).await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{
        sync::atomic::{AtomicI32, Ordering},
        time::Duration,
    };

    use serde_json::{Value, json};
    use teloxide::types::{AcceptedGiftTypes, ChatFullInfo, ChatFullInfoKind, ChatFullInfoPrivate};
    use wiremock::{
        Mock, MockServer, Request, Respond, ResponseTemplate,
        matchers::{body_string_contains, method, path_regex},
    };

    use super::*;
    use crate::{
        bot::{Inputs, callback::CallbackData, edit::PendingInput},
        collector::Collector,
        config::Config,
        model::{NotificationSubject, PostStatus},
        store::{
            Store,
            test_support::{ADMIN, category_id, incoming},
        },
    };

    const EXAMPLE: &str = include_str!("../../PureWaterSpiritBot.example.toml");
    const REVIEW_GROUP: i64 = -100999;
    const SUBMITTER: UserId = UserId(900);

    /// 每次调用返回一条新的消息，消息 ID 依次递增。
    struct Sequence(AtomicI32);

    impl Respond for Sequence {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let id = self.0.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": message(id),
            }))
        }
    }

    fn message(id: i32) -> Value {
        json!({
            "message_id": id,
            "date": 1,
            "chat": { "id": REVIEW_GROUP, "type": "supergroup", "title": "review" },
            "text": "hello"
        })
    }

    struct Fixture {
        _directory: tempfile::TempDir,
        server: MockServer,
        app: App,
    }

    fn review() -> ChatFullInfo {
        ChatFullInfo {
            id: ChatId(-100999),
            kind: ChatFullInfoKind::Private(Box::new(ChatFullInfoPrivate {
                username: None,
                first_name: None,
                last_name: None,
                bio: None,
                has_private_forwards: false,
                has_restricted_voice_and_video_messages: false,
                personal_chat: None,
                birthdate: None,
                business_intro: None,
                business_location: None,
                business_opening_hours: None,
            })),
            photo: None,
            pinned_message: None,
            message_auto_delete_time: None,
            has_hidden_members: false,
            has_aggressive_anti_spam_enabled: false,
            accepted_gift_types: AcceptedGiftTypes {
                unlimited_gifts: false,
                limited_gifts: false,
                unique_gifts: false,
                premium_subscription: false,
            },
            accent_color_id: None,
            background_custom_emoji_id: None,
            profile_accent_color_id: None,
            profile_background_custom_emoji_id: None,
            emoji_status_custom_emoji_id: None,
            emoji_status_expiration_date: None,
            has_visible_history: false,
            max_reaction_count: 0,
        }
    }

    /// 一个连着 mock Telegram 的 App：管理员是测试数据库里的管理员 42，审核群是 `REVIEW_GROUP`。
    async fn fixture(default_reject_note: Option<&str>) -> Fixture {
        let server = MockServer::start().await;
        let source = format!(
            "{}\n[review]\ngroup = {REVIEW_GROUP}\n{}",
            EXAMPLE.replace("123456789", "42"),
            default_reject_note.map_or_else(String::new, |default_reject_note| format!(
                "\ndefault_reject_note = {default_reject_note:?}"
            ))
        );
        let (directory, store) = Store::open_temporary().await;
        let (collector, _batches) = Collector::new(Duration::from_secs(1));
        let app = App {
            bot: Bot::new("test-token").set_api_url(format!("{}/", server.uri()).parse().unwrap()),
            channel: crate::bot::test_support::channel(),
            review: Some(review()),
            config: Config::parse(&source).unwrap(),
            store,
            collector,
            fetcher: crate::fetch::Fetcher::unavailable(),
            inputs: Inputs::default(),
            registered_commands: Default::default(),
            ai: Default::default(),
        };
        Fixture {
            _directory: directory,
            server,
            app,
        }
    }

    async fn mock_method(server: &MockServer, name: &str, result: Value, calls: Option<u64>) {
        let mock = Mock::given(method("POST"))
            .and(path_regex(format!("(?i).*/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": result,
            })));
        match calls {
            Some(calls) => mock.expect(calls).mount(server).await,
            None => mock.mount(server).await,
        }
    }

    async fn submit(app: &App, text: &str) -> PostId {
        let mut message = incoming(1000 + i32::try_from(text.len()).unwrap(), text);
        message.submitter = SUBMITTER;
        message.source_chat_id = ChatId(900);
        let post = app
            .store
            .create_submission_draft(&[message], Timestamp::UNIX_EPOCH)
            .await
            .unwrap()
            .unwrap();
        assert!(
            app.store
                .submit_for_review(post, SUBMITTER, Timestamp::UNIX_EPOCH)
                .await
                .unwrap()
        );
        post
    }

    fn user(id: u64) -> User {
        serde_json::from_value(json!({
            "id": id,
            "is_bot": false,
            "first_name": format!("User{id}"),
        }))
        .unwrap()
    }

    fn callback(from: u64, chat: i64, data: &str) -> CallbackQuery {
        serde_json::from_value(json!({
            "id": "query-1",
            "from": { "id": from, "is_bot": false, "first_name": format!("User{from}") },
            "chat_instance": "instance",
            "data": data,
            "message": {
                "message_id": 7,
                "date": 1,
                "chat": { "id": chat, "type": "supergroup", "title": "chat" },
                "text": "card"
            },
        }))
        .unwrap()
    }

    async fn status_of(app: &App, post: PostId) -> PostStatus {
        app.store.review_info(post).await.unwrap().unwrap().status
    }

    async fn notifications(app: &App) -> Vec<(NotificationSubject, UserId)> {
        app.store
            .undelivered_notifications()
            .await
            .unwrap()
            .into_iter()
            .map(|notification| (notification.subject, notification.recipient))
            .collect()
    }

    #[test]
    fn links_to_messages_in_supergroups() {
        assert_eq!(
            message_link(ChatId(-1001234567890), MessageId(42)).as_deref(),
            Some("https://t.me/c/1234567890/42")
        );
    }

    #[test]
    fn no_link_for_chats_without_a_supergroup_id() {
        assert_eq!(message_link(ChatId(-123456), MessageId(1)), None);
        assert_eq!(message_link(ChatId(123456), MessageId(1)), None);
        assert_eq!(message_link(ChatId(i64::MIN), MessageId(1)), None);
    }

    #[tokio::test]
    async fn posts_one_card_per_pending_submission_and_only_once() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(Sequence(AtomicI32::new(500)))
            // 预览一条，控制消息一条。
            .expect(2)
            .mount(&fixture.server)
            .await;
        let post = submit(app, "hello").await;

        let mut reported = HashSet::new();
        post_missing_cards(app, &mut reported).await;
        post_missing_cards(app, &mut reported).await;

        assert!(reported.is_empty());
        assert!(
            app.store
                .posts_without_review_card()
                .await
                .unwrap()
                .is_empty()
        );
        let card = app.store.review_card(post).await.unwrap();
        assert_eq!(card.contents, [(ChatId(REVIEW_GROUP), MessageId(500))]);
        assert_eq!(card.control, Some((ChatId(REVIEW_GROUP), MessageId(501))));
        assert_eq!(
            app.store
                .find_review_post(ChatId(REVIEW_GROUP), MessageId(500))
                .await
                .unwrap(),
            Some(post)
        );
    }

    #[tokio::test]
    async fn a_failing_card_is_reported_once_and_retried() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "ok": false,
                "error_code": 400,
                "description": "Bad Request: something went wrong",
            })))
            .mount(&fixture.server)
            .await;
        let post = submit(app, "hello").await;

        let mut reported = HashSet::new();
        post_missing_cards(app, &mut reported).await;
        assert_eq!(reported, HashSet::from([post]));
        // 失败的卡片留在待发列表里，下一轮继续尝试。
        assert_eq!(app.store.posts_without_review_card().await.unwrap(), [post]);
        post_missing_cards(app, &mut reported).await;
        assert_eq!(reported.len(), 1);
    }

    #[tokio::test]
    async fn approving_queues_the_post_and_notifies_the_submitter() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "editmessagetext", message(1), None).await;
        let post = submit(app, "hello").await;
        let gi = category_id(&app.store, "gi").await;

        let outcome = approve(app, post, &user(ADMIN.0), gi).await.unwrap();
        assert!(matches!(outcome, ApproveOutcome::Approved { .. }));
        assert_eq!(status_of(app, post).await, PostStatus::Queued);
        assert_eq!(app.store.queued_candidates(ADMIN).await.unwrap().len(), 1);
        assert_eq!(
            notifications(app).await,
            [(NotificationSubject::SubmissionApproved(post), SUBMITTER)]
        );

        // 第二个点的人什么也改变不了，也不会再发一次通知。
        let again = approve(app, post, &user(ADMIN.0), gi).await.unwrap();
        assert_eq!(again, ApproveOutcome::AlreadyHandled);
        assert_eq!(notifications(app).await.len(), 1);
    }

    #[tokio::test]
    async fn rejecting_uses_the_given_note_or_the_default() {
        let fixture = fixture(Some("不符合要求")).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "editmessagetext", message(1), None).await;

        let plain = submit(app, "a").await;
        assert!(reject(app, plain, &user(ADMIN.0), None).await.unwrap());
        let info = app.store.review_info(plain).await.unwrap().unwrap();
        assert_eq!(info.status, PostStatus::Rejected);
        assert_eq!(info.review_note.as_deref(), Some("不符合要求"));

        let custom = submit(app, "bb").await;
        let note = Some("画质太差".to_owned());
        assert!(reject(app, custom, &user(ADMIN.0), note).await.unwrap());
        let info = app.store.review_info(custom).await.unwrap().unwrap();
        assert_eq!(info.review_note.as_deref(), Some("画质太差"));

        assert!(!reject(app, custom, &user(ADMIN.0), None).await.unwrap());
        let rejected: Vec<_> = notifications(app).await;
        assert_eq!(
            rejected,
            [
                (NotificationSubject::SubmissionRejected(plain), SUBMITTER),
                (NotificationSubject::SubmissionRejected(custom), SUBMITTER),
            ]
        );
    }

    #[tokio::test]
    async fn rejecting_without_any_note_tells_the_submitter_only_the_result() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "editmessagetext", message(1), None).await;
        let post = submit(app, "a").await;
        assert!(reject(app, post, &user(ADMIN.0), None).await.unwrap());
        let info = app.store.review_info(post).await.unwrap().unwrap();
        assert_eq!(info.review_note, None);
    }

    #[tokio::test]
    async fn only_admins_in_the_review_group_can_press_the_buttons() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/answercallbackquery"))
            .and(body_string_contains("\"show_alert\":true"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": true })),
            )
            .expect(2)
            .mount(&fixture.server)
            .await;
        let post = submit(app, "a").await;
        let data = ReviewAction::Reject(post);

        // 群里的普通成员。
        let outsider = callback(5, REVIEW_GROUP, &CallbackData::Review(data).encode());
        handle_action(app, outsider, data).await.unwrap();
        // 管理员，但不是在审核群里（例如有人把按钮转发到别处）。
        let elsewhere = callback(ADMIN.0, -100555, &CallbackData::Review(data).encode());
        handle_action(app, elsewhere, data).await.unwrap();

        assert_eq!(status_of(app, post).await, PostStatus::PendingReview);
        assert!(notifications(app).await.is_empty());
    }

    #[tokio::test]
    async fn pressing_approve_shows_the_categories_of_the_admin_who_pressed() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "answercallbackquery", json!(true), Some(1)).await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/editmessagereplymarkup"))
            .and(body_string_contains("vc:"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": message(7),
            })))
            .expect(1)
            .mount(&fixture.server)
            .await;
        let post = submit(app, "a").await;
        let action = ReviewAction::Approve(post);
        let query = callback(
            ADMIN.0,
            REVIEW_GROUP,
            &CallbackData::Review(action).encode(),
        );

        handle_action(app, query, action).await.unwrap();
        // 只是换了按钮，投稿还在等待处理。
        assert_eq!(status_of(app, post).await, PostStatus::PendingReview);
    }

    #[tokio::test]
    async fn an_admin_without_categories_cannot_approve() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        // 配置里的第二个管理员，数据库里还没有任何分类。
        let newcomer = UserId(987654321);
        app.store
            .sync_admins(&[ADMIN, newcomer], Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/answercallbackquery"))
            .and(body_string_contains("\"show_alert\":true"))
            .and(body_string_contains("/categories"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": true })),
            )
            .expect(1)
            .mount(&fixture.server)
            .await;
        let post = submit(app, "a").await;
        let action = ReviewAction::Approve(post);
        let data = CallbackData::Review(action).encode();
        handle_action(app, callback(newcomer.0, REVIEW_GROUP, &data), action)
            .await
            .unwrap();
        assert_eq!(status_of(app, post).await, PostStatus::PendingReview);
    }

    fn group_message(from: u64, id: i32, text: &str) -> Message {
        serde_json::from_value(json!({
            "message_id": id,
            "date": 1,
            "chat": { "id": REVIEW_GROUP, "type": "supergroup", "title": "review" },
            "from": { "id": from, "is_bot": false, "first_name": format!("User{from}") },
            "text": text,
        }))
        .unwrap()
    }

    /// 点「拒绝并写理由」，返回 Bot 发出的提示消息的 ID。
    async fn press_reject_with_reason(app: &App, post: PostId) -> PendingInput {
        let action = ReviewAction::RejectWithReason(post);
        let data = CallbackData::Review(action).encode();
        handle_action(app, callback(ADMIN.0, REVIEW_GROUP, &data), action)
            .await
            .unwrap();
        app.inputs
            .get(ChatId(REVIEW_GROUP), MessageId(500))
            .expect("the prompt is waiting for a reply")
    }

    #[tokio::test]
    async fn reject_with_reason_asks_for_a_reply_and_then_rejects_with_it() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "answercallbackquery", json!(true), None).await;
        mock_method(&fixture.server, "deletemessage", json!(true), None).await;
        mock_method(&fixture.server, "editmessagetext", message(1), None).await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            // 提示里 @ 这位管理员，而不是对群里所有人强制回复。
            .and(body_string_contains("text_mention"))
            .respond_with(Sequence(AtomicI32::new(500)))
            .expect(1)
            .mount(&fixture.server)
            .await;
        let post = submit(app, "a").await;

        let pending = press_reject_with_reason(app, post).await;
        // 提示只是等待回复，投稿还没被处理。
        assert_eq!(status_of(app, post).await, PostStatus::PendingReview);

        let reply = group_message(ADMIN.0, 600, "  画质太差  ");
        finish_reject(app, &reply, MessageId(500), &pending, post, "  画质太差  ")
            .await
            .unwrap();

        let info = app.store.review_info(post).await.unwrap().unwrap();
        assert_eq!(info.status, PostStatus::Rejected);
        assert_eq!(info.review_note.as_deref(), Some("画质太差"));
        assert_eq!(
            notifications(app).await,
            [(NotificationSubject::SubmissionRejected(post), SUBMITTER)]
        );
        // 输入完成，提示作废。
        assert!(
            app.inputs
                .get(ChatId(REVIEW_GROUP), MessageId(500))
                .is_none()
        );
    }

    #[tokio::test]
    async fn a_blank_reason_keeps_the_prompt_open() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "answercallbackquery", json!(true), None).await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(Sequence(AtomicI32::new(500)))
            .mount(&fixture.server)
            .await;
        let post = submit(app, "a").await;
        let pending = press_reject_with_reason(app, post).await;

        let reply = group_message(ADMIN.0, 600, "   ");
        finish_reject(app, &reply, MessageId(500), &pending, post, "   ")
            .await
            .unwrap();

        assert_eq!(status_of(app, post).await, PostStatus::PendingReview);
        assert!(
            app.inputs
                .get(ChatId(REVIEW_GROUP), MessageId(500))
                .is_some()
        );
        assert!(notifications(app).await.is_empty());
    }

    #[tokio::test]
    async fn a_reason_for_a_post_handled_meanwhile_changes_nothing() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "answercallbackquery", json!(true), None).await;
        mock_method(&fixture.server, "deletemessage", json!(true), None).await;
        mock_method(&fixture.server, "editmessagetext", message(1), None).await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(Sequence(AtomicI32::new(500)))
            .mount(&fixture.server)
            .await;
        let post = submit(app, "a").await;
        let pending = press_reject_with_reason(app, post).await;

        // 另一位管理员在这期间已经通过了它。
        let gi = category_id(&app.store, "gi").await;
        approve(app, post, &user(ADMIN.0), gi).await.unwrap();

        let reply = group_message(ADMIN.0, 600, "太晚了");
        finish_reject(app, &reply, MessageId(500), &pending, post, "太晚了")
            .await
            .unwrap();
        assert_eq!(status_of(app, post).await, PostStatus::Queued);
        assert_eq!(notifications(app).await.len(), 1);
    }

    #[tokio::test]
    async fn replying_appends_text_and_refreshes_the_review_preview() {
        let fixture = fixture(None).await;
        let app = Arc::new(fixture.app);
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(Sequence(AtomicI32::new(500)))
            .mount(&fixture.server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/editmessagetext"))
            .and(body_string_contains("better text"))
            .and(body_string_contains("\"message_id\":500"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": message(500),
            })))
            .expect(1)
            .mount(&fixture.server)
            .await;
        let post = submit(&app, "original").await;
        post_missing_cards(&app, &mut HashSet::new()).await;
        let mut value = serde_json::to_value(group_message(ADMIN.0, 600, "better text")).unwrap();
        value["reply_to_message"] =
            serde_json::to_value(group_message(ADMIN.0, 500, "original")).unwrap();
        handle_reply(app.clone(), serde_json::from_value(value).unwrap())
            .await
            .unwrap();
        assert_eq!(
            app.store.load_post(post).await.unwrap().unwrap().messages[0]
                .text
                .as_deref(),
            Some("original\nbetter text")
        );
    }

    #[tokio::test]
    async fn submitting_is_refused_for_blocked_users_and_when_review_is_closed() {
        let fixture = fixture(None).await;
        let app = &fixture.app;
        let post = app
            .store
            .create_submission_draft(
                &[{
                    let mut message = incoming(1, "draft");
                    message.submitter = SUBMITTER;
                    message.source_chat_id = ChatId(900);
                    message
                }],
                Timestamp::UNIX_EPOCH,
            )
            .await
            .unwrap()
            .unwrap();
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/answercallbackquery"))
            .and(body_string_contains("\"show_alert\":true"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": true })),
            )
            .expect(2)
            .mount(&fixture.server)
            .await;

        // 被拉黑的用户提交不了。
        submitter_status(app, &user(SUBMITTER.0)).await.unwrap();
        app.store
            .set_submitter_blocked(SUBMITTER, true, ADMIN, None, Timestamp::UNIX_EPOCH)
            .await
            .unwrap();
        let blocked = callback(SUBMITTER.0, 900, "s:1");
        handle_submit(app, blocked, post).await.unwrap();
        assert_eq!(status_of(app, post).await, PostStatus::Draft);

        // 管理员自己不走这条路。
        let admin = callback(ADMIN.0, 900, "s:1");
        handle_submit(app, admin, post).await.unwrap();
        assert_eq!(status_of(app, post).await, PostStatus::Draft);
    }

    #[tokio::test]
    async fn nobody_can_submit_when_no_review_group_is_configured() {
        let server = MockServer::start().await;
        let (_directory, store) = Store::open_temporary().await;
        let (collector, _batches) = Collector::new(Duration::from_secs(1));
        let app = App {
            bot: Bot::new("test-token").set_api_url(format!("{}/", server.uri()).parse().unwrap()),
            channel: crate::bot::test_support::channel(),
            review: None,
            config: Config::parse(&EXAMPLE.replace("123456789", "42")).unwrap(),
            store,
            collector,
            fetcher: crate::fetch::Fetcher::unavailable(),
            inputs: Inputs::default(),
            registered_commands: Default::default(),
            ai: Default::default(),
        };
        assert_eq!(
            submitter_status(&app, &user(SUBMITTER.0)).await.unwrap(),
            None
        );
    }
}
