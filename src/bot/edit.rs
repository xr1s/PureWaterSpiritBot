//! 管理员在私聊里编辑自己的分类、时间表和设置。
//!
//! 面板是带内联键盘的消息，点按钮时原地刷新。需要输入文字的操作（新分类的名称、
//! 时间、数量等）会发一条要求回复的提示消息，用户回复它就是输入。输入成功后，
//! 在聊天最下面发一个刷新后的新面板，并删掉旧面板、提示和沿途的回复，
//! 这样聊天里始终只有最新的面板。提示记在内存里，Bot 重启后会失效，
//! 用户重新打开面板即可。

use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::Result;
use jiff::Timestamp;
use teloxide::{
    prelude::*,
    types::{
        CallbackQuery, CallbackQueryId, ForceReply, Message, MessageEntity, MessageId, User, UserId,
    },
};

use super::{
    callback::{EditAction, SettingKind},
    clear_keyboard,
    panel::{self, Panel},
    replace_card, reply, replying_to,
};
use crate::{
    app::App,
    model::{CategoryId, PickId, PostId, SlotId},
    schedule::{
        AdminSchedule, ScheduleDefaults, format_clock, format_duration, parse_clock, parse_count,
        parse_label, parse_misfire_grace, parse_send_interval, parse_timezone,
    },
    store::{Applied, Rejection, Setting},
};

/// 所有输入提示消息的开头，用来认出已经失效的提示。
pub(super) const PROMPT_MARK: &str = "✏️ ";
/// 提示消息在内存里保留多久。
const PROMPT_TTL: Duration = Duration::from_secs(60 * 60);
const EXPIRED_PROMPT: &str =
    "这条提示已经失效了。请重新打开面板：/categories、/schedule、/settings。";

/// 用户回复提示消息时要输入的内容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    NewCategory,
    RenameCategory(CategoryId),
    NewSlot,
    SlotTime(SlotId),
    /// 给时段添加配额，已选好分类，输入数量。
    NewPick {
        slot: SlotId,
        category: CategoryId,
    },
    PickCount(PickId),
    Setting(SettingKind),
    /// 审核群里拒绝投稿时写的理由。
    RejectReason(PostId),
    /// `/peek` 里撤下投稿时写的理由。
    RemoveReason(PostId),
    /// 「编辑文字」按钮要求回复的新文字。
    PostText(PostId),
    /// 替换媒体的按钮要求回复的新媒体：替换位置为 `item` 的一项，`None` 为全部。
    PostMedia {
        post: PostId,
        item: Option<usize>,
    },
}

#[derive(Debug, Clone)]
pub struct PendingInput {
    pub(super) admin: UserId,
    pub(super) input: Input,
    /// 产生这个提示的消息（面板或控制卡片），输入完成后刷新它。
    pub(super) panel: (ChatId, MessageId),
    /// 输入成功后要一并删掉的消息：用户填错时的回复，以及 Bot 对它的说明。
    clutter: Vec<MessageId>,
    created: Instant,
}

impl PendingInput {
    pub(super) fn new(admin: UserId, input: Input, panel: (ChatId, MessageId)) -> Self {
        Self {
            admin,
            input,
            panel,
            clutter: Vec::new(),
            created: Instant::now(),
        }
    }
}

/// 等待用户回复的提示消息，以提示消息的位置为键。
#[derive(Default)]
pub struct Inputs(Mutex<HashMap<(ChatId, MessageId), PendingInput>>);

impl Inputs {
    pub(super) fn insert(&self, prompt: (ChatId, MessageId), pending: PendingInput) {
        let mut inputs = self.0.lock().expect("input registry lock is not poisoned");
        inputs.retain(|_, pending| pending.created.elapsed() < PROMPT_TTL);
        inputs.insert(prompt, pending);
    }

    pub fn get(&self, chat: ChatId, message: MessageId) -> Option<PendingInput> {
        let inputs = self.0.lock().expect("input registry lock is not poisoned");
        inputs
            .get(&(chat, message))
            .filter(|pending| pending.created.elapsed() < PROMPT_TTL)
            .cloned()
    }

    /// 记下提示下产生的多余消息，输入完成时一并清理。
    pub(super) fn track(&self, chat: ChatId, prompt: MessageId, messages: &[MessageId]) {
        let mut inputs = self.0.lock().expect("input registry lock is not poisoned");
        if let Some(pending) = inputs.get_mut(&(chat, prompt)) {
            pending.clutter.extend_from_slice(messages);
        }
    }

    fn remove(&self, chat: ChatId, message: MessageId) {
        let mut inputs = self.0.lock().expect("input registry lock is not poisoned");
        inputs.remove(&(chat, message));
    }
}

/// 某条消息是不是 Bot 发出的输入提示，不管它还有没有效。
pub fn is_prompt(text: Option<&str>) -> bool {
    text.is_some_and(|text| text.starts_with(PROMPT_MARK))
}

/// 对一条失效提示的回复该说什么。
pub const fn expired_prompt_notice() -> &'static str {
    EXPIRED_PROMPT
}

/// 用一条新消息打开面板，对应 `/categories`、`/schedule` 和 `/settings`。
pub async fn open_panel(app: &App, admin: UserId, chat: ChatId, action: EditAction) -> Result<()> {
    let panel = render(app, admin, action).await;
    app.bot
        .send_message(chat, panel.text)
        .reply_markup(panel.keyboard)
        .await?;
    Ok(())
}

/// 处理面板上的一次按钮点击。
pub async fn handle_action(app: &App, query: CallbackQuery, action: EditAction) -> Result<()> {
    let admin = query.from.id;
    let Some(message) = &query.message else {
        app.bot.answer_callback_query(query.id).await?;
        return Ok(());
    };
    let target = (message.chat().id, message.id());
    match action {
        EditAction::NewCategory
        | EditAction::RenameCategory(_)
        | EditAction::NewSlot
        | EditAction::SlotTime(_)
        | EditAction::AddPickCategory { .. }
        | EditAction::PickCount(_)
        | EditAction::Setting(_) => ask_for_input(app, query.id, admin, target, action).await,
        EditAction::MoveCategory { .. }
        | EditAction::ArchiveCategory {
            confirmed: true, ..
        }
        | EditAction::DeleteSlot {
            confirmed: true, ..
        }
        | EditAction::DeletePick(_)
        | EditAction::AddFallbackCategory { .. }
        | EditAction::RemoveFallback { .. } => change(app, query.id, admin, target, action).await,
        _ => {
            app.bot.answer_callback_query(query.id).await?;
            show(app, admin, target, action).await;
            Ok(())
        }
    }
}

/// 处理对输入提示的回复。
pub async fn handle_reply(
    app: &App,
    message: &Message,
    prompt: MessageId,
    pending: PendingInput,
) -> Result<()> {
    let chat = message.chat.id;
    if pending.admin != message.from.as_ref().map_or(pending.admin, |user| user.id) {
        return Ok(());
    }
    if let Input::PostMedia { post, item } = pending.input {
        return super::post_media::accept(app, message, prompt, &pending, post, item).await;
    }
    let Some(text) = message.text() else {
        return complain(app, message, prompt, "请回复文字。").await;
    };
    match pending.input {
        Input::RejectReason(post) => {
            return super::review::finish_reject(app, message, prompt, &pending, post, text).await;
        }
        Input::RemoveReason(post) => {
            return super::peek::finish_remove(app, message, prompt, &pending, post, text).await;
        }
        Input::PostText(post) => {
            return super::post_text::finish(app, message, prompt, &pending, post).await;
        }
        _ => {}
    }
    match apply_input(app, pending.admin, pending.input, text).await? {
        InputResult::Retry(notice) => complain(app, message, prompt, &notice).await,
        InputResult::Gone(notice) => {
            app.inputs.remove(chat, prompt);
            reply(app, chat, message.id, &notice).await
        }
        InputResult::Done { notice, next } => {
            app.inputs.remove(chat, prompt);
            // 先发新面板再删旧的，任何时候聊天里都至少有一个可用的面板。
            let mut fresh = render(app, pending.admin, next).await;
            fresh.text = format!("✅ {notice}\n\n{}", fresh.text);
            app.bot
                .send_message(chat, fresh.text)
                .reply_markup(fresh.keyboard)
                .await?;
            let mut stale = vec![prompt, message.id];
            stale.extend(&pending.clutter);
            delete_messages(&app.bot, chat, &stale).await;
            // 旧面板可能已经超过 Telegram 允许删除的 48 小时，删不掉就去掉它的按钮。
            if app.bot.delete_message(chat, pending.panel.1).await.is_err() {
                clear_keyboard(&app.bot, chat, pending.panel.1).await;
            }
            Ok(())
        }
    }
}

/// 输入有误：回复说明原因，提示保持有效。这条回复和用户填错的消息都记下来，
/// 等输入成功时一并清理。
pub(super) async fn complain(
    app: &App,
    message: &Message,
    prompt: MessageId,
    notice: &str,
) -> Result<()> {
    complain_about(app, message.chat.id, &[message.id], prompt, notice).await
}

/// 同 [`complain`]，针对一次回复的多条消息（例如一组相册）。说明回复给第一条。
pub(super) async fn complain_about(
    app: &App,
    chat: ChatId,
    replies: &[MessageId],
    prompt: MessageId,
    notice: &str,
) -> Result<()> {
    let mut request = app.bot.send_message(chat, notice);
    if let Some(first) = replies.first() {
        request = request.reply_parameters(replying_to(*first));
    }
    let sent = request.await?;
    let mut clutter = replies.to_vec();
    clutter.push(sent.id);
    app.inputs.track(chat, prompt, &clutter);
    Ok(())
}

/// 逐条删除消息。失败只记录日志：消息可能已经不存在，或者超过了允许删除的时间。
async fn delete_messages(bot: &Bot, chat: ChatId, messages: &[MessageId]) {
    for message in messages {
        if let Err(error) = bot.delete_message(chat, *message).await {
            tracing::warn!("Failed to delete message {}: {error}", message.0);
        }
    }
}

enum InputResult {
    /// 输入不合法，提示仍然有效，用户可以重新回复。
    Retry(String),
    /// 要改的对象已经不存在，提示作废。
    Gone(String),
    Done {
        notice: String,
        /// 完成后面板要刷新成什么。
        next: EditAction,
    },
}

fn done(notice: impl Into<String>, next: EditAction) -> InputResult {
    InputResult::Done {
        notice: notice.into(),
        next,
    }
}

fn retry(error: &anyhow::Error) -> InputResult {
    InputResult::Retry(format!("{error}，请重新回复。"))
}

fn rejection(reason: &Rejection) -> InputResult {
    match reason {
        Rejection::NotFound => InputResult::Gone(rejection_text(reason)),
        _ => InputResult::Retry(format!("{}，请重新回复。", rejection_text(reason))),
    }
}

/// 被拒绝的修改该怎么告诉用户。
pub fn rejection_text(reason: &Rejection) -> String {
    match reason {
        Rejection::NotFound => "这一项已经不存在了，请重新打开面板".to_owned(),
        Rejection::DuplicateLabel => "已经有同名的分类".to_owned(),
        Rejection::DuplicateTime => "已经有这个时间的时段".to_owned(),
        Rejection::PickExists => "这个时段里已经有该分类的配额".to_owned(),
        Rejection::FallbackExists => "已经添加过这个补位分类".to_owned(),
        Rejection::FallbackIsOwnCategory => "不能用配额自己的分类补位".to_owned(),
        Rejection::CategoryHasPosts { count } => {
            format!("该分类里还有 {count} 篇排队或正在发布的投稿，暂时不能删除")
        }
        Rejection::CategoryInUse { slot_times } => format!(
            "{} 的时段还在使用该分类，请先修改这些时段的配额或补位",
            slot_times.join("、")
        ),
    }
}

async fn apply_input(app: &App, admin: UserId, input: Input, text: &str) -> Result<InputResult> {
    let now = Timestamp::now();
    let store = &app.store;
    let result = match input {
        Input::NewCategory => match parse_label(text) {
            Err(error) => retry(&error),
            Ok(label) => match store.add_category(admin, &label).await? {
                Applied::Done(category) => {
                    tracing::info!("Admin {} added category {}", admin.0, category.id.0);
                    done(
                        format!("已添加分类「{}」。", category.label),
                        EditAction::Categories,
                    )
                }
                Applied::Rejected(reason) => rejection(&reason),
            },
        },
        Input::RenameCategory(category) => match parse_label(text) {
            Err(error) => retry(&error),
            Ok(label) => match store.rename_category(admin, category, &label).await? {
                Applied::Done(()) => done("已改名。", EditAction::Category(category)),
                Applied::Rejected(reason) => rejection(&reason),
            },
        },
        Input::NewSlot => match parse_clock(text) {
            Err(error) => retry(&error),
            Ok(time) => match store.add_slot(admin, time, now).await? {
                Applied::Done(slot) => {
                    tracing::info!("Admin {} added slot {}", admin.0, slot.0);
                    done(
                        format!(
                            "已添加 {} 时段，点「添加配额」开始设置。",
                            format_clock(time)
                        ),
                        EditAction::Slot(slot),
                    )
                }
                Applied::Rejected(reason) => rejection(&reason),
            },
        },
        Input::SlotTime(slot) => match parse_clock(text) {
            Err(error) => retry(&error),
            Ok(time) => match store.set_slot_time(admin, slot, time, now).await? {
                Applied::Done(()) => done(
                    format!("时间已改为 {}，从现在起生效。", format_clock(time)),
                    EditAction::Slot(slot),
                ),
                Applied::Rejected(reason) => rejection(&reason),
            },
        },
        Input::NewPick { slot, category } => match parse_count(text) {
            Err(error) => retry(&error),
            Ok(count) => match store.add_pick(admin, slot, category, count).await? {
                Applied::Done(pick) => done("已添加配额。", EditAction::Pick(pick)),
                Applied::Rejected(reason) => rejection(&reason),
            },
        },
        Input::PickCount(pick) => match parse_count(text) {
            Err(error) => retry(&error),
            Ok(count) => match store.set_pick_count(admin, pick, count).await? {
                Applied::Done(()) => done("数量已修改。", EditAction::Pick(pick)),
                Applied::Rejected(reason) => rejection(&reason),
            },
        },
        Input::RejectReason(_)
        | Input::RemoveReason(_)
        | Input::PostText(_)
        | Input::PostMedia { .. } => {
            anyhow::bail!("reason and post content inputs are handled before apply_input")
        }
        Input::Setting(kind) => match parse_setting(kind, text) {
            Err(error) => retry(&error),
            Ok(setting) => {
                store.update_setting(admin, setting, now).await?;
                tracing::info!("Admin {} changed a setting", admin.0);
                done("设置已保存。", EditAction::Settings)
            }
        },
    };
    Ok(result)
}

/// 把用户回复的文字解析成设置。回复「默认」恢复默认值，提醒时间回复「关闭」不再提醒。
fn parse_setting(kind: SettingKind, text: &str) -> Result<Setting> {
    let value = text.trim();
    let keyword = value.to_lowercase();
    let reset = keyword == "默认" || keyword == "default";
    Ok(match kind {
        SettingKind::Timezone if reset => Setting::Timezone(None),
        SettingKind::Timezone => Setting::Timezone(Some(parse_timezone(value)?)),
        SettingKind::MisfireGrace if reset => Setting::MisfireGrace(None),
        SettingKind::MisfireGrace => {
            Setting::MisfireGrace(Some(format_duration(parse_misfire_grace(value)?)))
        }
        SettingKind::SendInterval if reset => Setting::SendInterval(None),
        SettingKind::SendInterval => {
            Setting::SendInterval(Some(format_duration(parse_send_interval(value)?)))
        }
        SettingKind::Reminder if keyword == "关闭" || keyword == "off" => Setting::Reminder(None),
        SettingKind::Reminder => Setting::Reminder(Some(format_clock(parse_clock(value)?))),
    })
}

/// 发一条要求回复的提示消息，并记下它在等什么。
async fn ask_for_input(
    app: &App,
    query: CallbackQueryId,
    admin: UserId,
    panel: (ChatId, MessageId),
    action: EditAction,
) -> Result<()> {
    let schedule = load(app, admin).await;
    let prompt = schedule
        .as_ref()
        .and_then(|schedule| prompt_for(schedule, &app.config.schedule, action));
    let Some((input, text)) = prompt else {
        app.bot
            .answer_callback_query(query)
            .text("找不到这一项，请重新打开面板。")
            .show_alert(true)
            .await?;
        return Ok(());
    };
    app.bot.answer_callback_query(query).await?;
    let sent = app
        .bot
        .send_message(panel.0, format!("{PROMPT_MARK}{text}"))
        .reply_markup(ForceReply::new())
        .await?;
    app.inputs
        .insert((panel.0, sent.id), PendingInput::new(admin, input, panel));
    Ok(())
}

/// 在 `card` 下发一条要求回复理由的提示，并记下它在等什么。私聊里用强制回复；
/// 群里强制回复会作用到所有人，所以改成在提示里 @ 这位管理员，由他回复。
pub(super) async fn ask_reason(
    app: &App,
    admin: &User,
    card: (ChatId, MessageId),
    input: Input,
    text: &str,
) -> Result<()> {
    let (chat, card_message) = card;
    let sent = if chat.is_user() {
        app.bot
            .send_message(chat, format!("{PROMPT_MARK}{text}"))
            .reply_parameters(replying_to(card_message))
            .reply_markup(ForceReply::new())
            .await?
    } else {
        let name = admin.full_name();
        let body = format!("{PROMPT_MARK}{name}，{text}");
        // 实体的位置和长度按 UTF-16 码元计算。
        let offset = PROMPT_MARK.encode_utf16().count();
        let length = name.encode_utf16().count();
        app.bot
            .send_message(chat, body)
            .reply_parameters(replying_to(card_message))
            .entities([MessageEntity::text_mention(admin.clone(), offset, length)])
            .await?
    };
    app.inputs
        .insert((chat, sent.id), PendingInput::new(admin.id, input, card));
    Ok(())
}

/// 理由输入完成后清理：删掉提示；私聊里还删掉用户的回复和沿途的说明。
/// 群里不去删别人的消息，那需要 Bot 有删除权限。
pub(super) async fn close_prompt(
    app: &App,
    message: &Message,
    prompt: MessageId,
    pending: &PendingInput,
) {
    dismiss_prompt(app, message.chat.id, prompt, pending, &[message.id]).await;
}

/// 输入完成后清理：删掉提示；私聊里还删掉 `replies` 和沿途的说明。
/// 要留在聊天里的回复（例如成为帖子内容的新媒体）不要放进 `replies`。
pub(super) async fn dismiss_prompt(
    app: &App,
    chat: ChatId,
    prompt: MessageId,
    pending: &PendingInput,
    replies: &[MessageId],
) {
    app.inputs.remove(chat, prompt);
    let mut stale = vec![prompt];
    if chat.is_user() {
        stale.extend(replies);
        stale.extend(&pending.clutter);
    }
    delete_messages(&app.bot, chat, &stale).await;
}

fn zone_name(schedule: &AdminSchedule) -> &str {
    schedule.timezone.iana_name().unwrap_or("自定义偏移")
}

/// 这个操作要用户输入什么，以及提示消息的正文。对象不存在时返回 `None`。
fn prompt_for(
    schedule: &AdminSchedule,
    defaults: &ScheduleDefaults,
    action: EditAction,
) -> Option<(Input, String)> {
    let slot = |id: SlotId| schedule.slots.iter().find(|slot| slot.id == id);
    let category = |id: CategoryId| {
        schedule
            .categories
            .iter()
            .find(|category| category.id == id)
    };
    Some(match action {
        EditAction::NewCategory => (
            Input::NewCategory,
            "请回复本消息，发送新分类的名称（最多 24 个字符）。".to_owned(),
        ),
        EditAction::RenameCategory(id) => (
            Input::RenameCategory(id),
            format!(
                "请回复本消息，发送分类「{}」的新名称。",
                category(id)?.label
            ),
        ),
        EditAction::NewSlot => (
            Input::NewSlot,
            format!(
                "请回复本消息，发送新时段的发布时间，格式 HH:MM，例如 10:00。\n按时区 {} 计算。",
                zone_name(schedule)
            ),
        ),
        EditAction::SlotTime(id) => (
            Input::SlotTime(id),
            format!(
                "请回复本消息，发送 {} 时段的新发布时间，格式 HH:MM。\n按时区 {} 计算。",
                format_clock(slot(id)?.time),
                zone_name(schedule)
            ),
        ),
        EditAction::AddPickCategory {
            slot: slot_id,
            category: category_id,
        } => (
            Input::NewPick {
                slot: slot_id,
                category: category_id,
            },
            format!(
                "请回复本消息，发送 {} 时段每天从「{}」取多少条（1 到 99）。",
                format_clock(slot(slot_id)?.time),
                category(category_id)?.label
            ),
        ),
        EditAction::PickCount(id) => {
            let (slot, pick) = panel::find_pick(schedule, id)?;
            (
                Input::PickCount(id),
                format!(
                    "请回复本消息，发送 {} 时段「{}」的新数量（1 到 99），现在是 {}。",
                    format_clock(slot.time),
                    schedule
                        .categories
                        .iter()
                        .find(|category| category.id == pick.category)
                        .map_or("?", |category| category.label.as_str()),
                    pick.count
                ),
            )
        }
        EditAction::Setting(kind) => (Input::Setting(kind), setting_prompt(defaults, kind)),
        _ => return None,
    })
}

fn setting_prompt(defaults: &ScheduleDefaults, kind: SettingKind) -> String {
    match kind {
        SettingKind::Timezone => format!(
            "请回复本消息，发送时区的 IANA 名称，例如 Asia/Shanghai。\n回复「默认」恢复默认值（{}）。",
            defaults.timezone.iana_name().unwrap_or("自定义偏移")
        ),
        SettingKind::MisfireGrace => format!(
            "请回复本消息，发送补发宽限期，例如 30m、2h（1 分钟到 24 小时）。\n\
             错过发布时间后，在这段时间内仍会补发。回复「默认」恢复默认值（{}）。",
            format_duration(defaults.misfire_grace)
        ),
        SettingKind::SendInterval => format!(
            "请回复本消息，发送同一时段内两条投稿之间的发送间隔，例如 3s（1 秒到 10 分钟）。\n\
             回复「默认」恢复默认值（{}）。",
            format_duration(defaults.send_interval)
        ),
        SettingKind::Reminder => {
            "请回复本消息，发送每日库存提醒的时间，格式 HH:MM，例如 20:00。\n回复「关闭」不再提醒。"
                .to_owned()
        }
    }
}

/// 直接修改数据的操作：成功后刷新面板，被拒绝就弹窗说明原因。
async fn change(
    app: &App,
    query: CallbackQueryId,
    admin: UserId,
    panel: (ChatId, MessageId),
    action: EditAction,
) -> Result<()> {
    let now = Timestamp::now();
    let store = &app.store;
    let schedule = load(app, admin).await;
    let (applied, next) = match action {
        EditAction::MoveCategory { category, up } => (
            store.move_category(admin, category, up).await?,
            EditAction::Categories,
        ),
        EditAction::ArchiveCategory { category, .. } => (
            store.archive_category(admin, category, now).await?,
            EditAction::Categories,
        ),
        EditAction::DeleteSlot { slot, .. } => (
            store.archive_slot(admin, slot, now).await?,
            EditAction::Schedule,
        ),
        EditAction::DeletePick(pick) => {
            let slot = schedule
                .as_ref()
                .and_then(|schedule| panel::find_pick(schedule, pick))
                .map(|(slot, _)| slot.id);
            match slot {
                Some(slot) => (
                    store.delete_pick(admin, pick).await?,
                    EditAction::Slot(slot),
                ),
                None => (Applied::Rejected(Rejection::NotFound), EditAction::Schedule),
            }
        }
        EditAction::AddFallbackCategory { pick, category } => (
            store.add_fallback(admin, pick, category).await?,
            EditAction::Pick(pick),
        ),
        EditAction::RemoveFallback { pick, category } => (
            store.remove_fallback(admin, pick, category).await?,
            EditAction::Pick(pick),
        ),
        _ => return Ok(()),
    };
    match applied {
        Applied::Done(()) => {
            app.bot.answer_callback_query(query).await?;
        }
        Applied::Rejected(reason) => {
            app.bot
                .answer_callback_query(query)
                .text(rejection_text(&reason))
                .show_alert(true)
                .await?;
            if reason != Rejection::NotFound {
                return Ok(());
            }
        }
    }
    show(app, admin, panel, next).await;
    Ok(())
}

async fn load(app: &App, admin: UserId) -> Option<AdminSchedule> {
    match app.store.load_schedule(admin, &app.config.schedule).await {
        Ok(schedule) => schedule,
        Err(error) => {
            tracing::error!(
                "Failed to load the schedule of admin {}: {error:#}",
                admin.0
            );
            None
        }
    }
}

/// 把面板消息刷新成 `action` 对应的面板。
async fn show(app: &App, admin: UserId, target: (ChatId, MessageId), action: EditAction) {
    let panel = render(app, admin, action).await;
    replace_card(&app.bot, target.0, target.1, panel.text, panel.keyboard).await;
}

/// 渲染 `action` 对应的面板。时间表无法加载时给出说明，而不是什么也不显示。
async fn render(app: &App, admin: UserId, action: EditAction) -> Panel {
    let Some(schedule) = load(app, admin).await else {
        return Panel {
            text: "读取你的配置时出错了，暂时无法编辑，请联系超级管理员。".to_owned(),
            keyboard: teloxide::types::InlineKeyboardMarkup::default(),
        };
    };
    view(&schedule, action).unwrap_or_else(|| panel::missing(EditAction::Categories))
}

/// 打开面板的操作对应的面板，其他操作返回 `None`。
fn view(schedule: &AdminSchedule, action: EditAction) -> Option<Panel> {
    Some(match action {
        EditAction::Categories => panel::categories(schedule),
        EditAction::Category(id) => panel::category(schedule, id),
        EditAction::ArchiveCategory {
            category,
            confirmed: false,
        } => panel::confirm_archive_category(schedule, category),
        EditAction::Schedule => panel::schedule(schedule),
        EditAction::Slot(id) => panel::slot(schedule, id),
        EditAction::DeleteSlot {
            slot,
            confirmed: false,
        } => panel::confirm_delete_slot(schedule, slot),
        EditAction::AddPick(id) => panel::choose_pick_category(schedule, id),
        EditAction::Pick(id) => panel::pick(schedule, id),
        EditAction::AddFallback(id) => panel::choose_fallback_category(schedule, id),
        EditAction::Settings => panel::settings(schedule),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::tests::{category, pick, slot};

    fn sample_schedule() -> AdminSchedule {
        let mut first = pick("gi", 2, &[]);
        first.id = PickId(10);
        AdminSchedule {
            admin: UserId(1),
            timezone: jiff::tz::TimeZone::get("Asia/Shanghai").unwrap(),
            misfire_grace: crate::model::PositiveDuration::try_from(
                jiff::SignedDuration::from_hours(2),
            )
            .unwrap(),
            send_interval: crate::model::PositiveDuration::try_from(
                jiff::SignedDuration::from_secs(3),
            )
            .unwrap(),
            reminder: None,
            categories: vec![category(1, "原神")],
            slots: vec![slot(5, "10:00", vec![first])],
        }
    }

    fn defaults() -> ScheduleDefaults {
        crate::store::test_support::defaults()
    }

    #[test]
    fn parses_settings_and_their_reset_keywords() {
        assert_eq!(
            parse_setting(SettingKind::Timezone, " asia/tokyo ").unwrap(),
            Setting::Timezone(Some("Asia/Tokyo".to_owned()))
        );
        assert_eq!(
            parse_setting(SettingKind::Timezone, "默认").unwrap(),
            Setting::Timezone(None)
        );
        assert_eq!(
            parse_setting(SettingKind::MisfireGrace, "90m").unwrap(),
            Setting::MisfireGrace(Some("1h 30m".to_owned()))
        );
        assert_eq!(
            parse_setting(SettingKind::SendInterval, "DEFAULT").unwrap(),
            Setting::SendInterval(None)
        );
        assert_eq!(
            parse_setting(SettingKind::Reminder, "9:30").unwrap(),
            Setting::Reminder(Some("09:30".to_owned()))
        );
        assert_eq!(
            parse_setting(SettingKind::Reminder, "关闭").unwrap(),
            Setting::Reminder(None)
        );
        assert!(parse_setting(SettingKind::Timezone, "Nowhere").is_err());
        assert!(parse_setting(SettingKind::SendInterval, "0s").is_err());
        // 「默认」对提醒时间没有意义，只能关闭。
        assert!(parse_setting(SettingKind::Reminder, "默认").is_err());
    }

    #[test]
    fn prompts_name_the_object_being_edited() {
        let schedule = sample_schedule();
        let defaults = defaults();
        let (input, text) = prompt_for(
            &schedule,
            &defaults,
            EditAction::RenameCategory(CategoryId(1)),
        )
        .unwrap();
        assert_eq!(input, Input::RenameCategory(CategoryId(1)));
        assert!(text.contains("原神"));

        let (input, text) =
            prompt_for(&schedule, &defaults, EditAction::PickCount(PickId(10))).unwrap();
        assert_eq!(input, Input::PickCount(PickId(10)));
        assert!(text.contains("10:00") && text.contains("现在是 2"));

        let (_, text) = prompt_for(&schedule, &defaults, EditAction::NewSlot).unwrap();
        assert!(text.contains("Asia/Shanghai"));
    }

    #[test]
    fn prompts_for_missing_objects_are_refused() {
        let schedule = sample_schedule();
        let defaults = defaults();
        for action in [
            EditAction::RenameCategory(CategoryId(9)),
            EditAction::SlotTime(SlotId(9)),
            EditAction::PickCount(PickId(9)),
            EditAction::AddPickCategory {
                slot: SlotId(9),
                category: CategoryId(1),
            },
            EditAction::AddPickCategory {
                slot: SlotId(5),
                category: CategoryId(9),
            },
            // 不需要输入的操作没有提示。
            EditAction::Categories,
        ] {
            assert!(
                prompt_for(&schedule, &defaults, action).is_none(),
                "{action:?}"
            );
        }
    }

    #[test]
    fn every_setting_has_a_prompt_mentioning_how_to_reset() {
        let defaults = defaults();
        for kind in [
            SettingKind::Timezone,
            SettingKind::MisfireGrace,
            SettingKind::SendInterval,
        ] {
            assert!(setting_prompt(&defaults, kind).contains("默认"));
        }
        assert!(setting_prompt(&defaults, SettingKind::Reminder).contains("关闭"));
    }

    #[test]
    fn view_only_handles_panel_actions() {
        let schedule = sample_schedule();
        assert!(view(&schedule, EditAction::Categories).is_some());
        assert!(view(&schedule, EditAction::Settings).is_some());
        assert!(view(&schedule, EditAction::NewCategory).is_none());
        assert!(
            view(
                &schedule,
                EditAction::ArchiveCategory {
                    category: CategoryId(1),
                    confirmed: true
                }
            )
            .is_none()
        );
    }

    #[test]
    fn recognizes_prompt_messages() {
        assert!(is_prompt(Some(&format!("{PROMPT_MARK}请回复"))));
        assert!(!is_prompt(Some("投稿 #1 已保存")));
        assert!(!is_prompt(None));
    }

    #[test]
    fn registry_returns_and_forgets_pending_inputs() {
        let inputs = Inputs::default();
        let key = (ChatId(1), MessageId(2));
        assert!(inputs.get(key.0, key.1).is_none());
        inputs.insert(
            key,
            PendingInput {
                admin: UserId(1),
                input: Input::NewSlot,
                panel: (ChatId(1), MessageId(1)),
                clutter: Vec::new(),
                created: Instant::now(),
            },
        );
        assert_eq!(inputs.get(key.0, key.1).unwrap().input, Input::NewSlot);
        // 读取不会消耗，校验失败后用户还能重新回复。
        assert!(inputs.get(key.0, key.1).is_some());
        inputs.remove(key.0, key.1);
        assert!(inputs.get(key.0, key.1).is_none());
    }

    #[test]
    fn tracks_messages_to_clean_up_once_the_input_succeeds() {
        let inputs = Inputs::default();
        let key = (ChatId(1), MessageId(2));
        let pending = PendingInput {
            admin: UserId(1),
            input: Input::NewSlot,
            panel: (ChatId(1), MessageId(1)),
            clutter: Vec::new(),
            created: Instant::now(),
        };
        inputs.insert(key, pending);
        inputs.track(key.0, key.1, &[MessageId(5), MessageId(6)]);
        inputs.track(key.0, key.1, &[MessageId(8)]);
        let clutter = inputs.get(key.0, key.1).unwrap().clutter;
        assert_eq!(clutter, [MessageId(5), MessageId(6), MessageId(8)]);
        // 提示已经不在了就什么也不做。
        inputs.track(ChatId(9), MessageId(9), &[MessageId(1)]);
    }

    #[test]
    fn rejections_are_explained_in_chinese() {
        assert!(rejection_text(&Rejection::DuplicateLabel).contains("同名"));
        assert!(
            rejection_text(&Rejection::CategoryInUse {
                slot_times: vec!["10:00".to_owned(), "14:00".to_owned()]
            })
            .contains("10:00、14:00")
        );
        assert!(rejection_text(&Rejection::CategoryHasPosts { count: 3 }).contains('3'));
    }
}
