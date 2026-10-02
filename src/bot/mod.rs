mod ai;
mod callback;
mod cancel;
mod command;
mod edit;
mod fetch_link;
mod help;
mod names;
mod panel;
mod peek;
mod post_media;
mod post_text;
mod review;
mod send_now;
#[cfg(test)]
mod test_support;
pub mod view;

pub use command::{Command, ReviewCommand};
pub use edit::Inputs;
pub use names::user_name;
pub use review::post_missing_cards;

use std::sync::Arc;

use anyhow::{Context, Error, Result};
use jiff::Timestamp;
use teloxide::{
    dispatching::{UpdateFilterExt, UpdateHandler},
    prelude::*,
    types::{
        BotCommandScope, CallbackQuery, InlineKeyboardMarkup, Message, MessageId, Recipient,
        ReplyParameters, UserId,
    },
    utils::command::BotCommands,
};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{
    app::App,
    model::{Content, IncomingMessage, PostId, PostStatus, SubmitterStatus, Target},
    store::{AppendOutcome, EditOutcome, MoveOutcome, QueueOutcome},
    telegram,
};
use callback::{CallbackData, EditAction};
use help::{HelpPage, HelpView};

/// 私聊消息的发送者，前提是他是管理员。
fn authorized_sender(app: &App, message: &Message) -> Option<UserId> {
    if !message.chat.is_private() {
        return None;
    }
    let user = message.from.as_ref()?;
    app.config.is_admin(user.id).then_some(user.id)
}

/// 私聊里发消息的人：管理员，或者开放投稿时没有被拉黑的普通用户。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Sender {
    Admin(UserId),
    Submitter(UserId),
}

impl Sender {
    fn id(self) -> UserId {
        match self {
            Self::Admin(id) | Self::Submitter(id) => id,
        }
    }
}

async fn sender(app: &App, message: &Message) -> Result<Option<Sender>> {
    if !message.chat.is_private() {
        return Ok(None);
    }
    let Some(user) = message.from.as_ref() else {
        return Ok(None);
    };
    if app.config.is_admin(user.id) {
        register_admin_commands(app, user.id).await;
        return Ok(Some(Sender::Admin(user.id)));
    }
    match review::submitter_status(app, user).await? {
        None => {
            tracing::warn!(
                "Ignoring message {} from unauthorized user {}",
                message.id.0,
                user.id.0
            );
            Ok(None)
        }
        Some(SubmitterStatus::Active) => Ok(Some(Sender::Submitter(user.id))),
        Some(SubmitterStatus::Blocked) => {
            reply(app, message.chat.id, message.id, view::SUBMITTER_BLOCKED).await?;
            Ok(None)
        }
    }
}

/// 只给管理员显示命令菜单，普通用户的菜单是空的。管理员的菜单在他第一次和 Bot
/// 说话时注册，因为 Telegram 只允许给已经和 Bot 聊过天的用户设置菜单。
async fn register_admin_commands(app: &App, admin: UserId) {
    let registered = app
        .registered_commands
        .lock()
        .expect("command registry lock is not poisoned")
        .contains(&admin);
    if registered {
        return;
    }
    let scope = BotCommandScope::Chat {
        chat_id: Recipient::Id(ChatId(admin.0 as _)),
    };
    match app
        .bot
        .set_my_commands(Command::bot_commands())
        .scope(scope)
        .await
    {
        Ok(_) => {
            app.registered_commands
                .lock()
                .expect("command registry lock is not poisoned")
                .insert(admin);
        }
        Err(err) => tracing::warn!(
            "Failed to register the command menu of admin {}: {err}",
            admin.0
        ),
    }
}

/// 普通用户只能改自己还没有提交的草稿：提交审核后的内容由管理员负责，
/// 通过之后的投稿已经归管理员所有，更不能再被投稿人改动。
async fn submitter_may_edit(app: &App, post: PostId) -> Result<bool> {
    Ok(app
        .store
        .post_summary(post)
        .await?
        .is_some_and(|summary| summary.status == PostStatus::Draft && summary.owner.is_none()))
}

async fn handle_message(app: Arc<App>, message: Message) -> Result<()> {
    let Some(sender) = sender(&app, &message).await? else {
        return Ok(());
    };
    let user = sender.id();

    if let Some(reply) = message.reply_to_message() {
        if let Some(pending) = app.inputs.get(message.chat.id, reply.id) {
            return edit::handle_reply(&app, &message, reply.id, pending).await;
        }
        // 回复的是已经失效的输入提示（例如 Bot 重启过），不能当作投稿。
        if edit::is_prompt(reply.text()) {
            return self::reply(
                &app,
                message.chat.id,
                message.id,
                edit::expired_prompt_notice(),
            )
            .await;
        }
    }

    let target = replied_post(&app, &message).await?;
    if let (Sender::Submitter(_), Some(target)) = (sender, target)
        && !submitter_may_edit(&app, target).await?
    {
        return self::reply(
            &app,
            message.chat.id,
            message.id,
            &view::submission_locked(target),
        )
        .await;
    }

    if sender == Sender::Admin(user)
        && target.is_none()
        && let Some(link) = crate::fetch::single_link(&message)
    {
        return fetch_link::start(app, message, user, link).await;
    }

    let Some(mut incoming) = telegram::incoming_message(&message, user) else {
        return reply(&app, message.chat.id, message.id, view::UNSUPPORTED_MESSAGE).await;
    };
    if sender == Sender::Admin(user) {
        incoming.channel_reply = telegram::channel_reply(&message, app.channel.id);
    }
    let Some(target) = target else {
        app.collector.push(incoming);
        return Ok(());
    };
    if incoming.content.media.is_none() {
        return append_text(&app, &message, target, &incoming.content, user).await;
    }
    incoming.target = Some(Target::Append(target));
    app.collector.push(incoming);
    Ok(())
}

/// 应用用户对其先前提交的消息所做的编辑。
async fn handle_edited_message(app: Arc<App>, message: Message) -> Result<()> {
    let Some(sender) = sender(&app, &message).await? else {
        return Ok(());
    };
    let Some(incoming) = telegram::incoming_message(&message, sender.id()) else {
        return Ok(());
    };
    if matches!(sender, Sender::Submitter(_)) {
        let post = app
            .store
            .find_post_by_message(message.chat.id, message.id)
            .await?;
        if let Some(post) = post
            && !submitter_may_edit(&app, post).await?
        {
            return reply(
                &app,
                message.chat.id,
                message.id,
                &view::submission_locked(post),
            )
            .await;
        }
    }
    match app
        .store
        .update_message(message.chat.id, message.id, &incoming.content)
        .await?
    {
        EditOutcome::Done(_) | EditOutcome::NotFound => Ok(()),
        EditOutcome::Locked(post, status) => {
            reply(
                &app,
                message.chat.id,
                message.id,
                &view::locked(post, status),
            )
            .await
        }
    }
}

/// 消息所回复的帖子（如果有）。回复帖子的某条消息即表示修改该帖子。
async fn replied_post(app: &App, message: &Message) -> Result<Option<PostId>> {
    match message.reply_to_message() {
        Some(reply) => {
            app.store
                .find_post_by_message(message.chat.id, reply.id)
                .await
        }
        None => Ok(None),
    }
}

/// 更新处理器树：消息、被编辑的消息和按钮点击。
pub fn handler() -> UpdateHandler<Error> {
    // 固定命令注册
    let command_message = dptree::filter(|message: Message| message.text().is_some())
        .filter_command::<Command>()
        .endpoint(handle_command);
    // 直接无视未注册的命令
    let unknown_command_message =
        dptree::filter(is_command_text).endpoint(async || Ok::<_, Error>(()));
    // 审核群里的消息只由审核流程处理：先试命令，其余的文字回复追加到原文。
    let review_commands = dptree::filter(|app: Arc<App>, message: Message| {
        review::is_review_group(&app, message.chat.id)
    })
    .filter_command::<ReviewCommand>()
    .endpoint(review::handle_command);
    let review_replies =
        dptree::filter::<_, _, (Arc<App>, Message), _>(|app: Arc<App>, message: Message| {
            review::is_review_group(&app, message.chat.id)
        })
        .endpoint::<_, (Arc<App>, Message)>(review::handle_reply);
    let message_handler = Update::filter_message()
        // 命令是纯文本；以 `/` 开头的说明文字（caption）视为提交。
        .branch(command_message)
        // 其他以 `/` 开头的文本会被丢弃，而不是作为帖子提交。
        .branch(unknown_command_message)
        // 审核群的命令和消息
        .branch(review_commands)
        .branch(review_replies)
        .endpoint::<_, (Arc<App>, Message)>(handle_message);
    let edited_message_handler =
        Update::filter_edited_message().endpoint::<_, (Arc<App>, Message)>(handle_edited_message);
    let callback_query_handler =
        Update::filter_callback_query().endpoint::<_, (Arc<App>, CallbackQuery)>(handle_callback);
    dptree::entry()
        .branch(message_handler)
        .branch(edited_message_handler)
        .branch(callback_query_handler)
}

async fn handle_command(app: Arc<App>, message: Message, command: Command) -> Result<()> {
    let Some(user) = authorized_sender(&app, &message) else {
        // 普通用户只能用 /start 看投稿说明，其他命令是管理员的。
        if command == Command::Start
            && message.chat.is_private()
            && app.config.review.group.is_some()
            && message
                .from
                .as_ref()
                .is_some_and(|user| !app.config.is_admin(user.id))
        {
            return send(&app, message.chat.id, view::START_SUBMITTER).await;
        }
        return Ok(());
    };
    register_admin_commands(&app, user).await;
    match command {
        Command::Start => send(&app, message.chat.id, view::READY).await,
        Command::Stock => {
            let categories = app.store.categories(user).await?;
            let counts = app.store.stock_counts(user).await?;
            send(&app, message.chat.id, &view::stock(&categories, &counts)).await
        }
        Command::Edit(argument) => edit_command(&app, &message, user, &argument).await,
        Command::Categories => {
            edit::open_panel(&app, user, message.chat.id, EditAction::Categories).await
        }
        Command::Schedule => {
            edit::open_panel(&app, user, message.chat.id, EditAction::Schedule).await
        }
        Command::Settings => {
            edit::open_panel(&app, user, message.chat.id, EditAction::Settings).await
        }
        Command::Peek => peek::open(&app, user, message.chat.id).await,
        Command::Help => {
            let menu = HelpView::Menu(HelpPage::Commands);
            let keyboard = view::help_keyboard(menu, help_is_paged(&app, message.chat.id));
            app.bot
                .send_message(message.chat.id, view::help_text(menu))
                .reply_markup(keyboard)
                .await?;
            Ok(())
        }
    }
}

/// 消息是否为以 `/command` 开头的文本。
fn is_command_text(message: Message) -> bool {
    message.text().and_then(command_name).is_some()
}

/// `/edit <id>`，或作为对帖子消息的回复发送 `/edit`：发送一条新的控制消息，
/// 说明如何修改该帖子，并提供入队和取消按钮。
async fn edit_command(app: &App, message: &Message, user: UserId, argument: &str) -> Result<()> {
    let chat_id = message.chat.id;
    let target = replied_post(app, message).await?;
    let post = match (argument.split_whitespace().next(), target) {
        (Some(argument), _) => match argument.trim_start_matches('#').parse() {
            Ok(id) => PostId(id),
            Err(_) => return reply(app, chat_id, message.id, view::EDIT_USAGE).await,
        },
        (None, Some(target)) => target,
        (None, None) => return reply(app, chat_id, message.id, view::EDIT_USAGE).await,
    };
    // 别的管理员的投稿对他不可见。
    let summary = app
        .store
        .post_summary(post)
        .await?
        .filter(|summary| summary.owner == Some(user));
    let Some(summary) = summary else {
        return reply(app, chat_id, message.id, &view::post_not_found(post)).await;
    };
    if !summary.status.is_editable() {
        return reply(
            app,
            chat_id,
            message.id,
            &view::locked(post, summary.status),
        )
        .await;
    }
    let first_message = app
        .store
        .first_message(post)
        .await?
        .filter(|(chat, _)| *chat == chat_id)
        .map_or(message.id, |(_, id)| id);
    let categories = app.store.categories(user).await?;
    let card = app
        .bot
        .send_message(chat_id, view::edit_card(&summary))
        .reply_parameters(replying_to(first_message))
        .reply_markup(view::edit_keyboard(
            &categories,
            &summary,
            app.ai.is_enabled(),
        ))
        .await?;
    app.store.set_control_message(post, chat_id, card.id).await
}

async fn append_text(
    app: &App,
    message: &Message,
    post: PostId,
    content: &Content,
    actor: UserId,
) -> Result<()> {
    let text = match app
        .store
        .append_text(post, actor, content, Timestamp::now())
        .await?
    {
        crate::store::AppendTextOutcome::Appended { .. } => {
            post_text::refresh_preview(app, message.chat.id, post).await;
            view::text_appended(post)
        }
        crate::store::AppendTextOutcome::NothingNew => return Ok(()),
        crate::store::AppendTextOutcome::TooLong { limit } => view::review_text_too_long(limit),
        crate::store::AppendTextOutcome::NotFound => view::post_not_found(post),
        crate::store::AppendTextOutcome::Locked(status) => view::locked(post, status),
    };
    reply(app, message.chat.id, message.id, &text).await
}

async fn handle_callback(app: Arc<App>, query: CallbackQuery) -> Result<()> {
    let Some(data) = query.data.as_deref().and_then(CallbackData::decode) else {
        app.bot.answer_callback_query(query.id).await?;
        return Ok(());
    };
    // 提交、撤回和审核群里的按钮有各自的权限规则，不走下面管理员的私聊按钮检查。
    match data {
        CallbackData::Submit { post } => return review::handle_submit(&app, query, post).await,
        CallbackData::Withdraw { post } => {
            return review::handle_withdraw(&app, query, post).await;
        }
        CallbackData::Review(action) => return review::handle_action(&app, query, action).await,
        CallbackData::Peek(action) => return peek::handle_action(&app, query, action).await,
        CallbackData::AutoTag { post } => return ai::handle(app, query, post).await,
        _ => {}
    }
    if !app.config.is_admin(query.from.id) {
        app.bot
            .answer_callback_query(query.id)
            .text(view::NOT_ALLOWED)
            .show_alert(true)
            .await?;
        return Ok(());
    }

    // 携带按钮的消息要变成的文本和键盘（如果有变化）。
    let mut card = None;
    let answer = match data {
        CallbackData::Help(help) => return show_help(&app, query, help).await,
        CallbackData::Edit(action) => return edit::handle_action(&app, query, action).await,
        CallbackData::EditPost { post } => return post_media::open_menu(&app, query, post).await,
        CallbackData::EditPostText { post } => {
            return post_media::edit_text(&app, query, post).await;
        }
        CallbackData::ReplaceMedia { post, item } => {
            return post_media::ask(&app, query, post, item).await;
        }
        CallbackData::SendNow { post } => return send_now::ask(&app, query, post).await,
        CallbackData::ConfirmSendNow { post } => return send_now::confirm(&app, query, post).await,
        CallbackData::BackToPost { post } => return send_now::back(&app, query, post).await,
        CallbackData::Cancel { post } => return cancel::ask(&app, query, post).await,
        CallbackData::ConfirmCancel { post } => return cancel::confirm(&app, query, post).await,
        CallbackData::Submit { .. }
        | CallbackData::Withdraw { .. }
        | CallbackData::Review(_)
        | CallbackData::Peek(_)
        | CallbackData::AutoTag { .. } => {
            return Ok(());
        }
        CallbackData::Queue { post, category } => {
            let outcome = app
                .store
                .queue_post(post, query.from.id, category, Timestamp::now())
                .await?;
            match outcome {
                QueueOutcome::Queued(category) => {
                    tracing::info!("Post {} queued in category {}", post.0, category.id.0);
                    let text = view::queued(post, &category);
                    card = Some((text.clone(), queued_card_keyboard(&app, post)));
                    text
                }
                QueueOutcome::AlreadyHandled => view::ALREADY_HANDLED.to_owned(),
                QueueOutcome::CategoryGone => {
                    app.bot
                        .answer_callback_query(query.id)
                        .text(view::CATEGORY_GONE)
                        .show_alert(true)
                        .await?;
                    if let Some(message) = &query.message {
                        let categories = app.store.categories(query.from.id).await?;
                        app.bot
                            .edit_message_reply_markup(message.chat().id, message.id())
                            .reply_markup(view::category_keyboard(
                                &categories,
                                post,
                                app.ai.is_enabled(),
                            ))
                            .await?;
                    }
                    return Ok(());
                }
            }
        }
        CallbackData::Resolve { attempt, published } => {
            // 只有投稿所属的管理员和超级管理员可以确认结果。
            let owner = app.store.attempt_owner(attempt).await?;
            if owner != Some(query.from.id) && !app.config.is_super(query.from.id) {
                app.bot
                    .answer_callback_query(query.id)
                    .text(view::NOT_ALLOWED)
                    .show_alert(true)
                    .await?;
                return Ok(());
            }
            let resolved = app
                .store
                .resolve_unknown_attempt(attempt, published, query.from.id, Timestamp::now())
                .await?;
            if let Some(post) = resolved {
                if published {
                    mark_control_published(&app, post).await;
                }
                view::resolved(attempt, published)
            } else {
                view::ALREADY_HANDLED.to_owned()
            }
        }
        CallbackData::ChangeCategory { post } => {
            return show_move_options(&app, query, post).await;
        }
        CallbackData::KeepCategory { post } => {
            app.bot.answer_callback_query(query.id).await?;
            if let Some(message) = &query.message {
                app.bot
                    .edit_message_reply_markup(message.chat().id, message.id())
                    .reply_markup(queued_card_keyboard(&app, post))
                    .await?;
            }
            return Ok(());
        }
        CallbackData::MoveTo { post, category } => {
            match app
                .store
                .move_queued_post(post, query.from.id, category)
                .await?
            {
                MoveOutcome::Moved(category) => {
                    tracing::info!("Post {} moved to category {}", post.0, category.id.0);
                    let text = view::moved(post, &category);
                    card = Some((text.clone(), queued_card_keyboard(&app, post)));
                    text
                }
                MoveOutcome::NotQueued => view::ALREADY_HANDLED.to_owned(),
                MoveOutcome::CategoryGone => {
                    return show_move_options_with_alert(&app, query, post, view::CATEGORY_GONE)
                        .await;
                }
            }
        }
    };

    app.bot.answer_callback_query(query.id).text(answer).await?;
    if let Some(message) = &query.message {
        let (chat_id, message_id) = (message.chat().id, message.id());
        match card {
            Some((text, keyboard)) => {
                replace_card(&app.bot, chat_id, message_id, text, keyboard).await
            }
            None => clear_keyboard(&app.bot, chat_id, message_id).await,
        }
    }
    Ok(())
}

/// 点「改分类」：把按钮换成可以改到的分类，排除帖子现在所在的分类。
async fn show_move_options(app: &App, query: CallbackQuery, post: PostId) -> Result<()> {
    show_move_options_with_alert(app, query, post, "").await
}

/// `alert` 非空时用弹窗提示，比如刚选的分类已被删除。
async fn show_move_options_with_alert(
    app: &App,
    query: CallbackQuery,
    post: PostId,
    alert: &str,
) -> Result<()> {
    let summary = app.store.post_summary(post).await?;
    let queued = summary.as_ref().filter(|summary| {
        summary.status == PostStatus::Queued && summary.owner == Some(query.from.id)
    });
    let Some(summary) = queued else {
        app.bot
            .answer_callback_query(query.id)
            .text(view::ALREADY_HANDLED)
            .await?;
        if let Some(message) = &query.message {
            clear_keyboard(&app.bot, message.chat().id, message.id()).await;
        }
        return Ok(());
    };
    let current = summary.category.as_ref().map(|category| category.id);
    let others: Vec<_> = app
        .store
        .categories(query.from.id)
        .await?
        .into_iter()
        .filter(|category| Some(category.id) != current)
        .collect();
    if others.is_empty() {
        app.bot
            .answer_callback_query(query.id)
            .text(view::NO_OTHER_CATEGORY)
            .show_alert(true)
            .await?;
        return Ok(());
    }
    let answer = app.bot.answer_callback_query(query.id);
    if alert.is_empty() {
        answer.await?;
    } else {
        answer.text(alert).show_alert(true).await?;
    }
    if let Some(message) = &query.message {
        app.bot
            .edit_message_reply_markup(message.chat().id, message.id())
            .reply_markup(view::move_keyboard(&others, post))
            .await?;
    }
    Ok(())
}

/// 在帮助消息上原地切换命令列表和详细介绍。
async fn show_help(app: &App, query: CallbackQuery, help: HelpView) -> Result<()> {
    app.bot.answer_callback_query(query.id).await?;
    let Some(message) = &query.message else {
        return Ok(());
    };
    let chat = message.chat().id;
    replace_card(
        &app.bot,
        chat,
        message.id(),
        view::help_text(help).to_owned(),
        view::help_keyboard(help, help_is_paged(app, chat)),
    )
    .await;
    Ok(())
}

/// 私聊里的帮助分两页（常用命令、审核命令），前提是开放了投稿审核；
/// 审核群里的帮助只有审核命令那一页。
fn help_is_paged(app: &App, chat: ChatId) -> bool {
    chat.is_user() && app.config.review.group.is_some()
}

/// 投稿被超级管理员撤下后，把所属管理员那条控制消息改成说明，去掉上面的取消按钮。
pub async fn mark_control_removed(app: &App, post: PostId) {
    replace_control(app, post, view::control_removed(post)).await;
}

/// 投稿在别的地方（例如 `/peek`）被取消后，把它的控制消息改成说明，去掉上面的按钮。
pub async fn mark_control_cancelled(app: &App, post: PostId) {
    replace_control(app, post, view::post_cancelled(post)).await;
}

async fn replace_control(app: &App, post: PostId, text: String) {
    match app.store.control_message(post).await {
        Ok(Some((chat_id, message_id))) => {
            replace_card(
                &app.bot,
                chat_id,
                message_id,
                text,
                InlineKeyboardMarkup::default(),
            )
            .await;
        }
        Ok(None) => {}
        Err(error) => tracing::warn!(
            "Failed to look up the control message of post {}: {error:#}",
            post.0
        ),
    }
}

/// 把已到达频道的帖子的控制消息改成普通提示，
/// 从而移除取消按钮。失败只记录日志：消息可能已不存在，
/// 而帖子无论如何都已发布。
pub async fn mark_control_published(app: &App, post: PostId) {
    let control = match app.store.control_message(post).await {
        Ok(control) => control,
        Err(error) => {
            tracing::warn!(
                "Failed to look up the control message of post {}: {error:#}",
                post.0
            );
            return;
        }
    };
    if let Some((chat_id, message_id)) = control {
        replace_card(
            &app.bot,
            chat_id,
            message_id,
            view::published(post),
            InlineKeyboardMarkup::default(),
        )
        .await;
    }
}

/// 重写控制消息。失败只记录日志，与 [`clear_keyboard`] 相同。
async fn replace_card(
    bot: &Bot,
    chat_id: ChatId,
    message_id: MessageId,
    text: String,
    keyboard: InlineKeyboardMarkup,
) {
    if let Err(error) = bot
        .edit_message_text(chat_id, message_id, text)
        .reply_markup(keyboard)
        .await
    {
        tracing::warn!("Failed to update message {}: {error}", message_id.0);
    }
}

/// 把收集器产出的批次转成草稿或帖子修改，直到收集器
/// 被丢弃为止。
pub async fn create_drafts(app: Arc<App>, mut batches: UnboundedReceiver<Vec<IncomingMessage>>) {
    while let Some(batch) = batches.recv().await {
        if let Err(error) = handle_batch(&app, &batch).await {
            tracing::error!("Failed to handle a submission: {error:#}");
        }
    }
}

async fn handle_batch(app: &App, batch: &[IncomingMessage]) -> Result<()> {
    let first = batch.first().context("empty draft batch")?;
    if let Some(Target::Replace { post, item, prompt }) = first.target {
        return post_media::finish(app, batch, post, item, prompt).await;
    }
    if !app.config.is_admin(first.submitter) {
        return handle_submission_batch(app, batch).await;
    }
    match first.target {
        Some(Target::Append(target)) => add_media(app, target, batch).await,
        Some(Target::Replace { .. }) | None => create_draft(app, batch, None).await,
    }
}

/// 普通用户的一批消息：追加到他自己还没提交的草稿，或者成为新的投稿草稿。
async fn handle_submission_batch(app: &App, batch: &[IncomingMessage]) -> Result<()> {
    let first = batch.first().context("empty draft batch")?;
    match first.target {
        Some(Target::Append(target)) if submitter_may_edit(app, target).await? => {
            add_media(app, target, batch).await
        }
        Some(Target::Append(target)) => {
            let (chat, message) = (first.source_chat_id, first.source_message_id);
            reply(app, chat, message, &view::submission_locked(target)).await
        }
        Some(Target::Replace { .. }) | None => create_submission(app, batch).await,
    }
}

async fn create_submission(app: &App, batch: &[IncomingMessage]) -> Result<()> {
    let first = batch.first().context("empty draft batch")?;
    let Some(post) = app
        .store
        .create_submission_draft(batch, Timestamp::now())
        .await?
    else {
        tracing::info!(
            "Ignoring already stored message {}",
            first.source_message_id.0
        );
        return Ok(());
    };
    tracing::info!(
        "Submission draft {} created from {} message(s) of {}",
        post.0,
        batch.len(),
        first.submitter.0
    );
    let chat_id = first.source_chat_id;
    let control = app
        .bot
        .send_message(chat_id, view::submission_saved(post, batch.len()))
        .reply_parameters(replying_to(first.source_message_id))
        .reply_markup(view::submission_keyboard(post))
        .await
        .context("failed to send submission controls")?;
    app.store
        .set_control_message(post, chat_id, control.id)
        .await
}

/// 向用户所回复的帖子添加媒体。已发布的帖子无法更改，
/// 因此这些媒体会成为新的草稿，并在频道中回复该帖子。
async fn add_media(app: &App, target: PostId, batch: &[IncomingMessage]) -> Result<()> {
    let first = batch.first().context("empty draft batch")?;
    let (chat_id, first_id) = (first.source_chat_id, first.source_message_id);
    let Some(summary) = app.store.post_summary(target).await? else {
        return reply(app, chat_id, first_id, &view::post_not_found(target)).await;
    };
    if summary.status == PostStatus::Published {
        let channel_message = app.store.channel_message_of(target).await?;
        let reply = DraftReply::Supplement {
            of: target,
            channel_message,
        };
        return create_draft(app, batch, Some(reply)).await;
    }
    let text = match app.store.append_messages(target, batch).await? {
        AppendOutcome::Appended { total } => view::media_appended(target, batch.len(), total),
        AppendOutcome::NotFound => view::post_not_found(target),
        AppendOutcome::Locked(status) => view::locked(target, status),
        AppendOutcome::Rejected(reason) => view::append_rejected(target, reason),
        AppendOutcome::Duplicate => {
            tracing::info!("Ignoring already stored media for post {}", target.0);
            return Ok(());
        }
    };
    reply(app, chat_id, first_id, &text).await
}

/// 草稿发布时所回复的对象。
#[derive(Clone, Copy)]
enum DraftReply {
    /// 用户向已发布的帖子添加了媒体。
    Supplement {
        of: PostId,
        /// 如果发布是手动确认的则未知，因为机器人从未
        /// 获知消息 ID。
        channel_message: Option<MessageId>,
    },
    /// 用户所回复的频道消息。
    ChannelMessage(MessageId),
}

impl DraftReply {
    fn channel_message(self) -> Option<MessageId> {
        match self {
            Self::Supplement {
                channel_message, ..
            } => channel_message,
            Self::ChannelMessage(message) => Some(message),
        }
    }
}

async fn create_draft(
    app: &App,
    batch: &[IncomingMessage],
    reply: Option<DraftReply>,
) -> Result<()> {
    let first = batch.first().context("empty draft batch")?;
    let now = Timestamp::now();
    let reply = reply.or_else(|| {
        batch
            .iter()
            .find_map(|message| message.channel_reply)
            .map(DraftReply::ChannelMessage)
    });
    let reply_to = reply.and_then(DraftReply::channel_message);
    let Some(post) = app.store.create_draft(batch, now, reply_to).await? else {
        tracing::info!(
            "Ignoring already stored message {}",
            first.source_message_id.0
        );
        return Ok(());
    };

    tracing::info!("Draft {} created from {} message(s)", post.0, batch.len());
    let text = match reply {
        Some(DraftReply::Supplement {
            of,
            channel_message: Some(_),
        }) => view::supplement_saved(post, of, batch.len()),
        Some(DraftReply::Supplement {
            of,
            channel_message: None,
        }) => view::supplement_without_original(post, of, batch.len()),
        Some(DraftReply::ChannelMessage(message)) => view::reply_saved(post, batch.len(), message),
        None => view::draft_saved(post, batch.len()),
    };
    let chat_id = first.source_chat_id;
    let categories = app.store.categories(first.submitter).await?;
    let text = if categories.is_empty() {
        format!("{text}\n\n{}", view::NO_CATEGORIES)
    } else {
        text
    };
    let text = format!("{text} {}", view::DRAFT_TAG);
    let control = app
        .bot
        .send_message(chat_id, text)
        .reply_parameters(replying_to(first.source_message_id))
        .reply_markup(view::category_keyboard(
            &categories,
            post,
            app.ai.is_enabled(),
        ))
        .await
        .context("failed to send draft controls")?;
    app.store
        .set_control_message(post, chat_id, control.id)
        .await
}

/// 移除内联键盘。失败只记录日志：消息可能已不存在或
/// 键盘已被清除。
pub async fn clear_keyboard(bot: &Bot, chat_id: ChatId, message_id: MessageId) {
    if let Err(error) = bot
        .edit_message_reply_markup(chat_id, message_id)
        .reply_markup(InlineKeyboardMarkup::default())
        .await
    {
        tracing::warn!(
            "Failed to remove inline keyboard from message {}: {error}",
            message_id.0
        );
    }
}

/// 已入队的帖子上的按钮；没有配置识图模型时没有「自动标签」。
fn queued_card_keyboard(app: &App, post: PostId) -> InlineKeyboardMarkup {
    view::queued_keyboard(post, app.ai.is_enabled())
}

async fn send(app: &App, chat_id: ChatId, text: &str) -> Result<()> {
    app.bot.send_message(chat_id, text).await?;
    Ok(())
}

/// 以回复的形式发送 `text`，让用户看出它针对的是哪条消息。
async fn reply(app: &App, chat_id: ChatId, to: MessageId, text: &str) -> Result<()> {
    app.bot
        .send_message(chat_id, text)
        .reply_parameters(replying_to(to))
        .await?;
    Ok(())
}

/// 回复一条消息；如果用户已将其删除，则退化为发送普通消息。
pub fn replying_to(message_id: MessageId) -> ReplyParameters {
    ReplyParameters::new(message_id).allow_sending_without_reply()
}

/// 从 `/stock@BotName arg` 这类消息中取出命令，不含斜杠和机器人名。
fn command_name(text: &str) -> Option<&str> {
    let word = text.split_whitespace().next()?.strip_prefix('/')?;
    word.split('@').next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn direct_text_reply_appends_instead_of_replacing() {
        use super::test_support::{admin_message, fixture, mock_method, sent_text};
        use crate::{
            fetch::Fetcher,
            store::test_support::{ADMIN, incoming},
        };

        let fixture = fixture(Fetcher::unavailable()).await;
        let app = Arc::new(fixture.app);
        let post = app
            .store
            .create_draft(&[incoming(10, "source")], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap();
        mock_method(&fixture.server, "sendmessage", sent_text(501), None).await;
        mock_method(&fixture.server, "editmessagetext", sent_text(10), None).await;
        let mut value = serde_json::to_value(admin_message(11, "#tag")).unwrap();
        value["reply_to_message"] = serde_json::to_value(admin_message(10, "source")).unwrap();
        handle_message(app.clone(), serde_json::from_value(value).unwrap())
            .await
            .unwrap();
        assert_eq!(
            app.store.load_post(post).await.unwrap().unwrap().messages[0]
                .text
                .as_deref(),
            Some("source\n#tag")
        );
        assert_eq!(
            app.store.post_summary(post).await.unwrap().unwrap().owner,
            Some(ADMIN)
        );
    }

    #[test]
    fn extracts_command_names() {
        assert_eq!(command_name("/stock"), Some("stock"));
        assert_eq!(command_name("/stock@PureBot now"), Some("stock"));
        assert_eq!(command_name("hello /stock"), None);
        assert_eq!(command_name(""), None);
    }
}
