//! 面向用户的文本和键盘。

use jiff::{Timestamp, tz::TimeZone};
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, MessageId, UserId};

use super::{
    callback::{CallbackData, PeekAction, ReviewAction},
    help::{HelpPage, HelpTopic, HelpView},
};
use crate::{
    fetch::{FetchError, SkipReason, Skipped},
    model::MediaKind,
    model::{AttemptId, Category, CategoryId, NotificationSubject, PostId, PostStatus, RunId},
    schedule::format_clock,
    selection::SlotForecast,
    store::{
        AppendRejection, PeekItem, PendingReview, PostSummary, ReplaceMediaRejection, ReviewInfo,
        StockCounts,
    },
};

pub const READY: &str = "PureWaterSpiritBot 已就绪。";
pub const UNSUPPORTED_MESSAGE: &str = "暂不支持这种消息类型。";
pub const NOT_ALLOWED: &str = "你没有权限执行该操作。";
pub const CATEGORY_GONE: &str = "该队列已不存在，请使用当前队列按钮。";
pub const ALREADY_HANDLED: &str = "该操作已经处理。";
pub const NO_CATEGORIES: &str = "你还没有配置队列，暂时无法把投稿加入队列。";
pub const FETCH_UNAVAILABLE: &str = "还没有安装 gallery-dl 或 yt-dlp，暂时不能抓取链接里的媒体。";
pub const FETCHING: &str = "正在抓取链接里的媒体……";
pub const EDIT_USAGE: &str = "用法：/edit <投稿编号>，或回复投稿消息后发送 /edit。";

/// 加在等待选择队列的草稿提示上，方便在聊天里搜索。
pub const DRAFT_TAG: &str = "#draft";
/// 加在待审核的审核卡片上，库存提醒里也带着它，点一下就能搜到卡片。
pub const PENDING_REVIEW_TAG: &str = "#pending";

fn label(categories: &[Category], id: CategoryId) -> &str {
    categories
        .iter()
        .find(|category| category.id == id)
        .map_or("（已删除的分类）", |category| {
            category.label.as_str()
        })
}

pub fn draft_saved(post: PostId, message_count: usize) -> String {
    if message_count > 1 {
        format!(
            "投稿 #{}（{message_count} 条消息）已保存，请选择队列：",
            post.0
        )
    } else {
        format!("投稿 #{} 已保存，请选择队列：", post.0)
    }
}

pub fn supplement_saved(post: PostId, of: PostId, message_count: usize) -> String {
    format!(
        "投稿 #{}（{message_count} 条消息）是对已发布投稿 #{} 的补充，发布时会回复频道中的原消息。请选择队列，或取消：",
        post.0, of.0
    )
}

pub fn supplement_without_original(post: PostId, of: PostId, message_count: usize) -> String {
    format!(
        "投稿 #{}（{message_count} 条消息）是对已发布投稿 #{} 的补充。\n\
         ⚠️ 投稿 #{} 是人工确认发布的，Bot 没有记录它在频道中的消息，发布时无法回复原消息，会作为普通消息发出。请选择队列，或取消：",
        post.0, of.0, of.0
    )
}

pub fn reply_saved(post: PostId, message_count: usize, channel_message: MessageId) -> String {
    let count = if message_count > 1 {
        format!("（{message_count} 条消息）")
    } else {
        String::new()
    };
    format!(
        "投稿 #{}{count} 已保存，发布时会回复频道消息 #{}。请选择队列：",
        post.0, channel_message.0
    )
}

pub fn post_not_found(post: PostId) -> String {
    format!("找不到投稿 #{}。", post.0)
}

pub fn post_cancelled(post: PostId) -> String {
    format!("投稿 #{} 已取消。", post.0)
}

pub fn cancel_question(post: PostId) -> String {
    format!("确认取消投稿 #{} 吗？取消后不能恢复。", post.0)
}

pub fn text_appended(post: PostId) -> String {
    format!("投稿 #{} 的文字已追加。", post.0)
}

pub fn text_replaced_but_not_shown(post: PostId) -> String {
    format!(
        "投稿 #{} 的文字已替换。这条消息不是 Bot 发的，上面显示的文字不会跟着变，\
         想让它也变的话可以直接编辑这条消息。",
        post.0
    )
}

pub fn media_appended(post: PostId, added: usize, total: usize) -> String {
    format!(
        "已向投稿 #{} 追加 {added} 条媒体，现共 {total} 条消息。",
        post.0
    )
}

pub fn append_rejected(post: PostId, reason: AppendRejection) -> String {
    let reason = match reason {
        AppendRejection::TextOnly => "纯文字投稿不能追加媒体，请取消后重新投稿".to_owned(),
        AppendRejection::TooManyItems => {
            format!("相册最多 {} 条消息", crate::model::MAX_ALBUM_ITEMS)
        }
        AppendRejection::NotAlbum => {
            "图片和视频可以混合，文件只能和文件一起，动图不能放进相册".to_owned()
        }
    };
    format!("无法向投稿 #{} 追加媒体：{reason}。", post.0)
}

pub const ONE_ITEM_ONLY: &str = "这里只能替换一项，请只发一张图片、一个视频或文件。";
pub const MEDIA_EXPECTED: &str = "请回复图片、视频或文件。";

/// 替换媒体的提示正文。`items` 是帖子现在有几项媒体。
pub fn replace_media_prompt(post: PostId, item: Option<usize>, items: usize) -> String {
    let what = match item {
        Some(item) => format!(
            "发送一张图片、一个视频或文件，替换投稿 #{} 的第 {} 项。",
            post.0,
            item + 1
        ),
        None if items > 1 => format!(
            "发送新的图片、视频或文件，替换投稿 #{} 现有的全部 {items} 项媒体，可以一次发一组相册。",
            post.0
        ),
        None => format!(
            "发送新的图片、视频或文件，替换投稿 #{} 的媒体，可以一次发一组相册。",
            post.0
        ),
    };
    format!("请回复本消息，{what}\n新媒体带说明文字时会同时替换投稿文字，不带则保留原来的文字。")
}

pub fn media_replaced(
    post: PostId,
    item: Option<usize>,
    total: usize,
    text_replaced: bool,
) -> String {
    let replaced = match item {
        Some(item) => format!("投稿 #{} 的第 {} 项已替换。", post.0, item + 1),
        None => format!("投稿 #{} 的媒体已替换，现共 {total} 项。", post.0),
    };
    if text_replaced {
        format!("{replaced}说明文字已成为新的投稿文字。")
    } else {
        replaced
    }
}

pub fn replace_rejected(post: PostId, reason: ReplaceMediaRejection) -> String {
    let id = post.0;
    match reason {
        ReplaceMediaRejection::TextOnly => format!("投稿 #{id} 是纯文字投稿，没有可替换的媒体。"),
        ReplaceMediaRejection::ItemGone => {
            format!("投稿 #{id} 里已经没有这一项了，请重新点「编辑图文」。")
        }
        ReplaceMediaRejection::TooManyItems => format!(
            "无法替换投稿 #{id} 的媒体：相册最多 {} 条消息，请重新回复。",
            crate::model::MAX_ALBUM_ITEMS
        ),
        ReplaceMediaRejection::NotAlbum => format!(
            "无法替换投稿 #{id} 的媒体：图片和视频可以混合，文件只能和文件一起，\
             动图不能放进相册，请重新回复。"
        ),
    }
}

/// 为什么已过可编辑阶段的帖子无法再被修改。
pub fn locked(post: PostId, status: PostStatus) -> String {
    let id = post.0;
    match status {
        PostStatus::Published => format!(
            "投稿 #{id} 已发布，不支持修改。回复投稿消息并发送图片、视频或文件，可以作为补充投稿。"
        ),
        PostStatus::Reserved => format!("投稿 #{id} 正在发布，暂时不能修改。"),
        PostStatus::Failed => format!("投稿 #{id} 发布失败，不能修改。"),
        PostStatus::Cancelled => format!("投稿 #{id} 已取消，不能修改。"),
        PostStatus::PendingReview => format!("投稿 #{id} 正在审核，暂时不能修改。"),
        PostStatus::Rejected => format!("投稿 #{id} 没有通过审核，不能修改。"),
        PostStatus::Draft | PostStatus::Queued => {
            format!("投稿 #{id} 当前不能修改。")
        }
    }
}

pub fn fetch_failed(error: &FetchError) -> String {
    match error {
        FetchError::Blocked => "这条链接指向本机或内网地址，不会抓取。".to_owned(),
        FetchError::NoTool => FETCH_UNAVAILABLE.to_owned(),
        FetchError::Failed(reason) => format!("抓取失败：\n{reason}"),
    }
}

/// 多组媒体时，每组前面发的一行标题。
pub fn fetch_group_header(index: usize, total: usize, count: usize) -> String {
    format!("第 {index}/{total} 组，{count} 个文件：")
}

/// 抓取结束后替换「正在抓取」的提示。`failures` 是上传失败的组和原因。
pub fn fetch_done(files: usize, groups: usize, failures: &[String], skipped: &[Skipped]) -> String {
    let mut text = if groups > 1 {
        format!(
            "共抓取 {files} 个文件，分成 {groups} 组，每组是一篇单独的投稿草稿，\
             不需要的那组直接点「取消投稿」。"
        )
    } else {
        format!("已抓取 {files} 个文件。")
    };
    if !failures.is_empty() {
        text.push_str(&format!("\n\n有 {} 组没能上传：", failures.len()));
        for failure in failures {
            text.push_str(&format!("\n· {failure}"));
        }
    }
    text.push_str(&skipped_lines(skipped));
    text
}

/// 没有抓到任何能发的文件。
pub fn fetch_nothing(skipped: &[Skipped]) -> String {
    let mut text = "这条链接里没有找到可以发送的图片或视频。".to_owned();
    text.push_str(&skipped_lines(skipped));
    text
}

const MAX_LISTED_SKIPS: usize = 10;

fn skipped_lines(skipped: &[Skipped]) -> String {
    if skipped.is_empty() {
        return String::new();
    }
    let mut text = "\n\n下面这些文件没有发送：".to_owned();
    for item in skipped.iter().take(MAX_LISTED_SKIPS) {
        let reason = match item.reason {
            SkipReason::Unsupported => "格式不支持".to_owned(),
            SkipReason::TooLarge { size, limit } => format!(
                "{} MB，超过 Telegram 上传上限 {} MB",
                megabytes(size),
                limit / 1_000_000
            ),
        };
        text.push_str(&format!("\n· {}：{reason}", item.name));
    }
    if skipped.len() > MAX_LISTED_SKIPS {
        text.push_str(&format!("\n…还有 {} 个", skipped.len() - MAX_LISTED_SKIPS));
    }
    text
}

/// 以 MB 为单位、保留一位小数。
fn megabytes(bytes: u64) -> String {
    let tenths = bytes / 100_000;
    format!("{}.{}", tenths / 10, tenths % 10)
}

pub fn edit_card(post: &PostSummary) -> String {
    let state = match (post.status, &post.category) {
        (PostStatus::Queued, Some(category)) => {
            format!("已加入「{}」队列", category.label)
        }
        _ => "等待选择队列".to_owned(),
    };
    format!(
        "投稿 #{}（{} 条消息）：{state}\n\n回复本消息或投稿中的任意消息：\n\
         · 发送文字，换行追加到投稿文字\n\
         · 发送图片、视频或文件，追加到投稿\n\
         也可以点「编辑图文」修改文字或替换媒体，或直接编辑自己发的原消息。",
        post.id.0, post.message_count
    )
}

pub fn edit_keyboard(
    categories: &[Category],
    post: &PostSummary,
    auto_tag: bool,
) -> InlineKeyboardMarkup {
    match post.status {
        PostStatus::Queued => queued_keyboard(post.id, auto_tag),
        _ => category_keyboard(categories, post.id, auto_tag),
    }
}

pub fn queued(post: PostId, category: &Category) -> String {
    format!("投稿 #{} 已加入「{}」队列。", post.0, category.label)
}

pub fn moved(post: PostId, category: &Category) -> String {
    format!("投稿 #{} 已改到「{}」队列。", post.0, category.label)
}

pub const NO_OTHER_CATEGORY: &str = "没有其他分类可以改，请先用 /categories 创建。";

pub fn published(post: PostId) -> String {
    format!("投稿 #{} 已发布。", post.0)
}

pub fn send_now_question(post: PostId) -> String {
    format!("确认立即把投稿 #{} 发送到频道吗？", post.0)
}

pub fn send_now_channel_unavailable(post: PostId) -> String {
    format!("频道暂时不可用，投稿 #{} 没有发送，请稍后再试。", post.0)
}

pub fn send_now_rejected(post: PostId, reason: &str) -> String {
    format!("投稿 #{} 发送失败，Telegram 拒绝了它：{reason}", post.0)
}

pub fn send_now_unknown(post: PostId) -> String {
    format!(
        "投稿 #{} 是否已经发到频道无法确认，请检查频道后选择处理方式。",
        post.0
    )
}

pub fn resolved(attempt: AttemptId, published: bool) -> String {
    if published {
        format!("发布尝试 #{} 已确认为已发布。", attempt.0)
    } else {
        format!("发布尝试 #{} 已重新入队。", attempt.0)
    }
}

const CATEGORY_COLUMNS: usize = 4;

pub fn category_keyboard(
    categories: &[Category],
    post: PostId,
    auto_tag: bool,
) -> InlineKeyboardMarkup {
    let buttons: Vec<_> = categories
        .iter()
        .map(|category| {
            let data = CallbackData::Queue {
                post,
                category: category.id,
            };
            InlineKeyboardButton::callback(category.label.clone(), data.encode())
        })
        .collect();
    let mut rows: Vec<_> = buttons
        .chunks(CATEGORY_COLUMNS)
        .map(<[_]>::to_vec)
        .collect();
    rows.extend(post_action_rows(post, auto_tag));
    InlineKeyboardMarkup::new(rows)
}

fn cancel_button(post: PostId) -> InlineKeyboardButton {
    InlineKeyboardButton::callback("取消投稿", CallbackData::Cancel { post }.encode())
}

fn edit_post_button(post: PostId) -> InlineKeyboardButton {
    InlineKeyboardButton::callback("编辑图文", CallbackData::EditPost { post }.encode())
}

const MEDIA_ITEM_COLUMNS: usize = 5;

/// 点「编辑图文」之后的按钮：编辑文字、替换媒体；多于一项媒体时
/// 还可以只替换其中一项。最后一行返回。
pub fn edit_post_keyboard(post: PostId, items: usize) -> InlineKeyboardMarkup {
    let replace_all = if items > 1 {
        "替换全部媒体"
    } else {
        "替换媒体"
    };
    let mut rows = vec![vec![
        InlineKeyboardButton::callback("编辑文字", CallbackData::EditPostText { post }.encode()),
        InlineKeyboardButton::callback(
            replace_all,
            CallbackData::ReplaceMedia { post, item: None }.encode(),
        ),
    ]];
    if items > 1 {
        let buttons: Vec<_> = (0..items)
            .map(|item| {
                let data = CallbackData::ReplaceMedia {
                    post,
                    item: Some(item),
                };
                InlineKeyboardButton::callback(format!("第 {} 项", item + 1), data.encode())
            })
            .collect();
        rows.extend(buttons.chunks(MEDIA_ITEM_COLUMNS).map(<[_]>::to_vec));
    }
    rows.push(vec![back_to_post_button(post)]);
    InlineKeyboardMarkup::new(rows)
}

fn auto_tag_button(post: PostId) -> InlineKeyboardButton {
    InlineKeyboardButton::callback("自动标签", CallbackData::AutoTag { post }.encode())
}

fn send_now_button(post: PostId) -> InlineKeyboardButton {
    InlineKeyboardButton::callback("立即发送", CallbackData::SendNow { post }.encode())
}

/// 草稿和已入队的帖子上共有的两行按钮：编辑图文、自动标签，立即发送、取消投稿。
/// `auto_tag` 为假（没有配置识图模型）时，第一行没有「自动标签」。
fn post_action_rows(post: PostId, auto_tag: bool) -> [Vec<InlineKeyboardButton>; 2] {
    let mut edit = vec![edit_post_button(post)];
    if auto_tag {
        edit.push(auto_tag_button(post));
    }
    [edit, vec![send_now_button(post), cancel_button(post)]]
}

/// 在键盘末尾加一行「自动标签」。`enabled` 为假（没有配置识图模型）时原样返回。
pub fn with_auto_tag_button(
    keyboard: InlineKeyboardMarkup,
    post: PostId,
    enabled: bool,
) -> InlineKeyboardMarkup {
    if !enabled {
        return keyboard;
    }
    keyboard.append_row([auto_tag_button(post)])
}

/// 点「立即发送」之后的确认按钮。
pub fn send_now_confirm_keyboard(post: PostId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback(
            "确认立即发送",
            CallbackData::ConfirmSendNow { post }.encode(),
        ),
        back_to_post_button(post),
    ]])
}

/// 点「取消投稿」之后的确认按钮。
pub fn cancel_confirm_keyboard(post: PostId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback(
            "确认取消投稿",
            CallbackData::ConfirmCancel { post }.encode(),
        ),
        back_to_post_button(post),
    ]])
}

fn back_to_post_button(post: PostId) -> InlineKeyboardButton {
    InlineKeyboardButton::callback("« 返回", CallbackData::BackToPost { post }.encode())
}

/// 已入队的帖子上的按钮：改分类在上，下面是和草稿一样的两行。
pub fn queued_keyboard(post: PostId, auto_tag: bool) -> InlineKeyboardMarkup {
    let change =
        InlineKeyboardButton::callback("改分类", CallbackData::ChangeCategory { post }.encode());
    let [edit, send] = post_action_rows(post, auto_tag);
    InlineKeyboardMarkup::new([vec![change], edit, send])
}

/// 改分类时的按钮：除了当前分类之外的分类，最后一行返回。
pub fn move_keyboard(categories: &[Category], post: PostId) -> InlineKeyboardMarkup {
    let buttons: Vec<_> = categories
        .iter()
        .map(|category| {
            let data = CallbackData::MoveTo {
                post,
                category: category.id,
            };
            InlineKeyboardButton::callback(category.label.clone(), data.encode())
        })
        .collect();
    let mut rows: Vec<_> = buttons
        .chunks(CATEGORY_COLUMNS)
        .map(<[_]>::to_vec)
        .collect();
    rows.push(vec![InlineKeyboardButton::callback(
        "« 返回",
        CallbackData::KeepCategory { post }.encode(),
    )]);
    InlineKeyboardMarkup::new(rows)
}

pub fn resolve_keyboard(attempt: AttemptId) -> InlineKeyboardMarkup {
    let button = |text: &str, published| {
        let data = CallbackData::Resolve { attempt, published };
        InlineKeyboardButton::callback(text, data.encode())
    };
    InlineKeyboardMarkup::new([[button("已发布", true), button("未发布，重新入队", false)]])
}

const HELP_MENU: &str = "直接向我发送文字、图片、视频或文件即可投稿。\n\
    我会先把投稿保存为草稿，你再选择要加入的队列，\n\
    之后会按计划自动发布到频道。\n\
    只发一条链接也可以，我会抓取链接里的图片和视频。\n\n\
    点击下面的命令查看详细介绍：";

const REVIEW_HELP_MENU: &str = "审核群里的投稿审核流程：\n\
    · 点卡片上的「通过」，再选一个自己的分类，投稿就进入你的队列\n\
    · 点「拒绝」，或回复卡片发送 /reject 理由，拒绝并通知投稿人\n\
    · 回复卡片发送文字，会换行追加到原文（图片和文件不会被替换）；\n\
    \x20\x20带图片的投稿可以点「清空文字」\n\
    · 只有管理员名单里的人可以操作\n\n\
    点击下面的命令查看详细介绍：";

pub fn help_text(view: HelpView) -> &'static str {
    match view {
        HelpView::Menu(HelpPage::Commands) => HELP_MENU,
        HelpView::Menu(HelpPage::Review) => REVIEW_HELP_MENU,
        HelpView::Topic(topic) => topic.details(),
    }
}

/// 某一页的命令列表：每个命令一个按钮。`paged` 为真时，在末尾加一个去另一页的按钮。
fn help_menu_keyboard(page: HelpPage, paged: bool) -> InlineKeyboardMarkup {
    let topics: &[HelpTopic] = match page {
        HelpPage::Commands => &HelpTopic::COMMANDS,
        HelpPage::Review => &HelpTopic::REVIEW,
    };
    let mut rows: Vec<_> = topics
        .iter()
        .map(|topic| {
            let data = CallbackData::Help(HelpView::Topic(*topic));
            vec![InlineKeyboardButton::callback(
                topic.button_label(),
                data.encode(),
            )]
        })
        .collect();
    if paged {
        let (label, other) = match page {
            HelpPage::Commands => ("审核命令 »", HelpPage::Review),
            HelpPage::Review => ("« 常用命令", HelpPage::Commands),
        };
        let data = CallbackData::Help(HelpView::Menu(other));
        rows.push(vec![InlineKeyboardButton::callback(label, data.encode())]);
    }
    InlineKeyboardMarkup::new(rows)
}

/// 帮助消息在 `view` 下的按钮。详细介绍页只有一个返回按钮，回到它所在的那一页。
pub fn help_keyboard(view: HelpView, paged: bool) -> InlineKeyboardMarkup {
    match view {
        HelpView::Menu(page) => help_menu_keyboard(page, paged),
        HelpView::Topic(topic) => {
            let data = CallbackData::Help(HelpView::Menu(topic.page()));
            InlineKeyboardMarkup::new([[InlineKeyboardButton::callback("« 返回", data.encode())]])
        }
    }
}

pub fn stock(categories: &[Category], counts: &StockCounts) -> String {
    let mut lines = vec!["可发布库存".to_owned()];
    lines.extend(category_counts(categories, counts));
    lines.push(String::new());
    lines.push("其他状态".to_owned());
    lines.push(format!("待分类：{}", counts.draft));
    lines.push(format!("发布中：{}", counts.reserved));
    lines.push(format!("失败：{}", counts.failed));
    lines.join("\n")
}

pub fn stock_reminder(
    categories: &[Category],
    counts: &StockCounts,
    unknown_attempts: i64,
    pending_reviews: i64,
    forecast: &[SlotForecast],
) -> String {
    let mut lines = vec!["库存提醒".to_owned()];
    lines.extend(category_counts(categories, counts));
    if unknown_attempts > 0 {
        lines.push(format!("待人工确认：{unknown_attempts}"));
    }
    if counts.draft > 0 {
        lines.push(format!("待分类稿件：{} {DRAFT_TAG}", counts.draft));
    }
    if pending_reviews > 0 {
        lines.push(format!(
            "待审核投稿：{pending_reviews} {PENDING_REVIEW_TAG}"
        ));
    }
    lines.push(String::new());
    lines.push("下一发布日计划".to_owned());
    for slot in forecast {
        let mut line = format!(
            "{}：{}/{}",
            format_clock(slot.time),
            slot.selected,
            slot.planned
        );
        if !slot.shortages.is_empty() {
            let shortages: Vec<_> = slot
                .shortages
                .iter()
                .map(|shortage| {
                    format!(
                        "{} 缺 {}",
                        label(categories, shortage.category),
                        shortage.missing
                    )
                })
                .collect();
            line.push_str(&format!("（{}）", shortages.join("，")));
        }
        lines.push(line);
    }
    lines.join("\n")
}

fn category_counts<'a>(
    categories: &'a [Category],
    counts: &'a StockCounts,
) -> impl Iterator<Item = String> + 'a {
    categories
        .iter()
        .map(|category| format!("{}：{}", category.label, counts.queued_in(category)))
}

pub fn publication_unknown(attempt: AttemptId) -> String {
    format!(
        "发布尝试 #{} 的结果无法确认。请检查频道后选择处理方式：",
        attempt.0
    )
}

pub fn publication_failed(attempt: AttemptId) -> String {
    format!("发布尝试 #{} 永久失败，请检查投稿和日志。", attempt.0)
}

pub fn submission_approved(post: PostId) -> String {
    format!("你的投稿 #{} 通过了审核，会按计划发布到频道。", post.0)
}

/// 被拒绝的通知。理由是管理员写的，或者配置里的默认理由，两者都没有就只告知结果。
pub fn submission_rejected(post: PostId, note: Option<&str>) -> String {
    match note {
        Some(note) => format!("你的投稿 #{} 没有通过审核。\n理由：{note}", post.0),
        None => format!("你的投稿 #{} 没有通过审核。", post.0),
    }
}

pub fn run_aborted(run: RunId) -> String {
    format!("发布运行 #{} 因频道不可用而中止。", run.0)
}

/// 内容不依赖实时数据的通知的文本和可选键盘。
pub fn static_notification(
    subject: NotificationSubject,
) -> Option<(String, Option<InlineKeyboardMarkup>)> {
    match subject {
        NotificationSubject::PublicationUnknown(attempt) => Some((
            publication_unknown(attempt),
            Some(resolve_keyboard(attempt)),
        )),
        NotificationSubject::PublicationFailed(attempt) => {
            Some((publication_failed(attempt), None))
        }
        NotificationSubject::RunAborted(run) => Some((run_aborted(run), None)),
        NotificationSubject::SubmissionApproved(post) => Some((submission_approved(post), None)),
        NotificationSubject::SubmissionRejected(_)
        | NotificationSubject::PostRemoved(_)
        | NotificationSubject::StockReminder(_) => None,
    }
}

// ---- 普通用户投稿 ----

pub const START_SUBMITTER: &str = r#"直接向我发送图片、视频或文件即可投稿。
我会先保存为草稿，你确认后提交审核。
通过审核的投稿会发布到频道，审核结果会通知你。"#;
pub const SUBMITTER_BLOCKED: &str = "你已被限制投稿。";
pub const SUBMISSION_CLOSED: &str = "这个投稿已经处理过了。";

pub fn submission_saved(post: PostId, message_count: usize) -> String {
    if message_count > 1 {
        format!(
            "投稿 #{}（{message_count} 条消息）已保存。确认后点「提交审核」，管理员审核后会通知你结果。\n\
             提交前可以回复本投稿，发送文字或图片追加。",
            post.0
        )
    } else {
        format!(
            "投稿 #{} 已保存。确认后点「提交审核」，管理员审核后会通知你结果。\n\
             提交前可以回复本投稿，发送文字或图片追加。",
            post.0
        )
    }
}

pub fn submission_keyboard(post: PostId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        InlineKeyboardButton::callback("提交审核", CallbackData::Submit { post }.encode()),
        InlineKeyboardButton::callback("取消投稿", CallbackData::Withdraw { post }.encode()),
    ]])
}

pub fn submission_locked(post: PostId) -> String {
    format!(
        "投稿 #{} 已经提交或处理过了，不能再修改。想投新稿，请直接发送内容，不要回复旧投稿。",
        post.0
    )
}

pub fn submission_submitted(post: PostId) -> String {
    format!("投稿 #{} 已提交，等待管理员审核，结果会通知你。", post.0)
}

pub fn withdraw_keyboard(post: PostId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[InlineKeyboardButton::callback(
        "撤回",
        CallbackData::Withdraw { post }.encode(),
    )]])
}

pub fn submission_withdrawn(post: PostId) -> String {
    format!("投稿 #{} 已撤回。", post.0)
}

// ---- 审核群 ----

pub const REVIEW_NO_CATEGORIES: &str = "你还没有分类，请先私聊 Bot 用 /categories 创建。";
pub const REVIEW_REPLY_NEEDED: &str = "请回复要处理的审核卡片（投稿内容或下面的控制消息）。";
pub const REVIEW_TEXT_ONLY: &str = "只能追加文字，图片和文件不行。";

/// 审核卡片的正文。`submitter` 是投稿人的称呼，卡片上要能认出是谁。
pub fn review_card_text(info: &ReviewInfo, submitter: &str) -> String {
    let mut text = format!(
        "投稿 #{} · 来自 {submitter}\n状态：待审核 {PENDING_REVIEW_TAG}\n\n\
         · 回复投稿内容或本消息并发送文字，会换行追加到原文\n\
         · 回复本消息发送 /reject 理由，或点「拒绝并写理由」，拒绝这篇投稿",
        info.id.0
    );
    if info.has_media {
        text.push_str("\n· 点「清空文字」可以去掉文字，只留图片");
    }
    text
}

pub fn review_keyboard(post: PostId, has_media: bool) -> InlineKeyboardMarkup {
    let button = |text: &str, action: ReviewAction| {
        InlineKeyboardButton::callback(text, CallbackData::Review(action).encode())
    };
    let mut rows = vec![
        vec![
            button("通过", ReviewAction::Approve(post)),
            button("拒绝", ReviewAction::Reject(post)),
        ],
        vec![button("拒绝并写理由", ReviewAction::RejectWithReason(post))],
    ];
    if has_media {
        rows.push(vec![button("清空文字", ReviewAction::ClearText(post))]);
    }
    InlineKeyboardMarkup::new(rows)
}

/// 点「通过」之后显示点击者自己的分类，所有人都能看到。
pub fn review_category_keyboard(post: PostId, categories: &[Category]) -> InlineKeyboardMarkup {
    let button = |text: &str, action: ReviewAction| {
        InlineKeyboardButton::callback(text, CallbackData::Review(action).encode())
    };
    let choices: Vec<_> = categories
        .iter()
        .map(|category| {
            button(
                &category.label,
                ReviewAction::Category {
                    post,
                    category: category.id,
                },
            )
        })
        .collect();
    let mut rows: Vec<_> = choices.chunks(2).map(<[_]>::to_vec).collect();
    rows.push(vec![button("« 返回", ReviewAction::Back(post))]);
    InlineKeyboardMarkup::new(rows)
}

pub fn review_approved(info: &ReviewInfo, submitter: &str, admin: &str, category: &str) -> String {
    format!(
        "投稿 #{} · 来自 {submitter}\n✅ 已通过，{admin} 认领，进入「{category}」",
        info.id.0
    )
}

pub fn review_rejected(
    info: &ReviewInfo,
    submitter: &str,
    admin: &str,
    note: Option<&str>,
) -> String {
    let mut text = format!(
        "投稿 #{} · 来自 {submitter}\n❌ 已被 {admin} 拒绝",
        info.id.0
    );
    if let Some(note) = note {
        text.push_str(&format!("\n理由：{note}"));
    }
    text
}

pub fn review_withdrawn(info: &ReviewInfo, submitter: &str) -> String {
    format!("投稿 #{} · 来自 {submitter}\n↩️ 投稿人已撤回", info.id.0)
}

pub fn review_text_too_long(limit: usize) -> String {
    format!("文字太长了，这篇投稿最多 {limit} 个字符（表情符号按两个字符算）。")
}

pub fn review_blocked(name: &str) -> String {
    format!("已拉黑 {name}，他不能再投稿了。回复他的任意一张审核卡片发送 /unblock 可以解除。")
}

pub fn review_unblocked(name: &str) -> String {
    format!("已解除对 {name} 的拉黑。")
}

pub const REVIEW_SUBMITTER_UNKNOWN: &str = "找不到这位投稿人。";

/// `/pending` 的回复：总数和最早的几篇，带上卡片的链接方便跳转。
/// `names` 是每篇投稿人的称呼，和 `items` 一一对应。
pub fn pending_list(
    total: i64,
    items: &[PendingReview],
    names: &[String],
    link: impl Fn(&PendingReview) -> Option<String>,
) -> String {
    if total == 0 {
        return "没有待审核的投稿。".to_owned();
    }
    let mut lines = vec![format!("待审核 {total} 篇，最早的 {} 篇：", items.len())];
    for (item, name) in items.iter().zip(names) {
        match link(item) {
            Some(link) => lines.push(format!("#{} 来自 {name}  {link}", item.post.0)),
            None => lines.push(format!("#{} 来自 {name}", item.post.0)),
        }
    }
    lines.join("\n")
}

pub const REVIEW_PROMPT_EXPIRED: &str = "这条提示已经失效了，请重新点审核卡片上的按钮。";

pub fn reject_reason_prompt(post: PostId) -> String {
    format!(
        "请回复本消息，写下拒绝投稿 #{} 的理由（最多 300 个字符），会发给投稿人。",
        post.0
    )
}

// ---- /peek ----

/// 每页列出多少篇排队投稿。
pub const PEEK_PAGE_SIZE: usize = 8;
pub const PEEK_ROW_COUNT: usize = 4;
/// 按钮和摘要里最多显示多少个字符的文字。
const SNIPPET_CHARS: usize = 30;

pub const PEEK_NO_ONE: &str = "没有其他管理员。";
pub const PEEK_PICK_ADMIN: &str = "选择要查看的管理员：";
pub const PEEK_NOT_QUEUED: &str = "这篇投稿已经不在队列里了。";

/// 列表里的一行：一篇投稿和它预计发出的时间，`None` 表示没有时段会发它。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeekRow {
    pub at: Option<Timestamp>,
    pub item: PeekItem,
}

pub fn peek_send_failed(post: PostId) -> String {
    format!("投稿 #{} 发送失败，请查看日志。", post.0)
}

pub fn peek_admin_button(name: &str, queued: i64, admin: UserId) -> Vec<InlineKeyboardButton> {
    let data = CallbackData::Peek(PeekAction::List { admin, page: 0 });
    vec![InlineKeyboardButton::callback(
        format!("{name} · 排队 {queued}"),
        data.encode(),
    )]
}

pub fn peek_admins_keyboard(rows: Vec<Vec<InlineKeyboardButton>>) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(rows)
}

fn peek_button(text: &str, action: PeekAction) -> InlineKeyboardButton {
    InlineKeyboardButton::callback(text, CallbackData::Peek(action).encode())
}

/// 总共有多少页，至少一页。
pub fn peek_page_count(total: usize) -> usize {
    total.div_ceil(PEEK_PAGE_SIZE).max(1)
}

fn media_label(kind: MediaKind) -> &'static str {
    match kind {
        MediaKind::Photo => "图片",
        MediaKind::Video => "视频",
        MediaKind::Animation => "动图",
        MediaKind::Document => "文件",
    }
}

fn snippet(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let flat = flat.trim();
    if flat.chars().count() > SNIPPET_CHARS {
        let head: String = flat.chars().take(SNIPPET_CHARS).collect();
        format!("{head}…")
    } else {
        flat.to_owned()
    }
}

/// 一篇投稿的一行摘要：媒体的种类和数量，加上文字的开头。
pub fn peek_summary(item: &PeekItem) -> String {
    let text = item.text.as_deref().map(snippet).filter(|t| !t.is_empty());
    let media = match item.media.as_slice() {
        [] => None,
        [first, rest @ ..] if rest.iter().all(|kind| kind == first) => {
            Some(format!("[{}×{}]", media_label(*first), item.media.len()))
        }
        _ => Some(format!("[媒体×{}]", item.media.len())),
    };
    match (media, text) {
        (Some(media), Some(text)) => format!("{media} {text}"),
        (Some(media), None) => media,
        (None, Some(text)) => text,
        (None, None) => "（空）".to_owned(),
    }
}

fn format_send_time(at: Option<Timestamp>, zone: &TimeZone) -> String {
    match at {
        Some(at) => at
            .to_zoned(zone.clone())
            .strftime("%m-%d %H:%M")
            .to_string(),
        None => "没有时段会发".to_owned(),
    }
}

/// 某个管理员的一页排队投稿，按预计的发送顺序排列。
pub fn peek_list(
    name: &str,
    total: usize,
    page_index: usize,
    rows: &[PeekRow],
    zone: &TimeZone,
) -> String {
    if rows.is_empty() {
        return format!("{name}：没有排队的投稿。");
    }
    let mut lines = vec![
        format!(
            "{name}：共 {total} 篇排队投稿，第 {}/{} 页，按发送顺序排列",
            page_index + 1,
            peek_page_count(total)
        ),
        "按当前队列预测，之后新排进的投稿会排到前面。".to_owned(),
    ];
    for row in rows {
        lines.push(format!(
            "#{} · {} · {} · {}",
            row.item.id.0,
            format_send_time(row.at, zone),
            row.item.category.label,
            peek_summary(&row.item)
        ));
    }
    lines.join("\n")
}

/// 列表下面的按钮。`back` 为真时（超级管理员）最后一行回到选管理员。
pub fn peek_list_keyboard(
    admin: UserId,
    total: usize,
    page_index: usize,
    rows: &[PeekRow],
    back: bool,
) -> InlineKeyboardMarkup {
    let page_number = u32::try_from(page_index).unwrap_or(0);
    let list = |page: u32| PeekAction::List { admin, page };
    let mut keyboard: Vec<Vec<InlineKeyboardButton>> = rows
        .chunks(PEEK_ROW_COUNT)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    let post = row.item.id;
                    peek_button(&format!("#{}", post.0), PeekAction::View(post))
                })
                .collect()
        })
        .collect();
    let mut nav = Vec::new();
    if page_number > 0 {
        nav.push(peek_button("« 上一页", list(page_number - 1)));
    }
    if page_index + 1 < peek_page_count(total) {
        nav.push(peek_button("下一页 »", list(page_number + 1)));
    }
    if !nav.is_empty() {
        keyboard.push(nav);
    }
    if !rows.is_empty() {
        let page = PeekAction::SendPage {
            admin,
            page: page_number,
        };
        keyboard.push(vec![peek_button("发送本页内容", page)]);
    }
    if back {
        keyboard.push(vec![peek_button("« 返回", PeekAction::Admins)]);
    }
    InlineKeyboardMarkup::new(keyboard)
}

/// 发出投稿内容之后的控制卡片。
pub fn peek_card(post: PostId, category: &str, owner: &str) -> String {
    format!(
        "投稿 #{} · 「{category}」· 属于 {owner}\n状态：排队中",
        post.0
    )
}

/// 控制卡片的按钮：自己的投稿可以取消，别人的投稿（只有超级管理员看得到）可以撤下。
pub fn peek_card_keyboard(post: PostId, own: bool) -> InlineKeyboardMarkup {
    if own {
        return InlineKeyboardMarkup::new([[peek_button("取消投稿", PeekAction::Cancel(post))]]);
    }
    InlineKeyboardMarkup::new([[
        peek_button("撤下", PeekAction::Remove(post)),
        peek_button("撤下并写理由", PeekAction::RemoveWithReason(post)),
    ]])
}

/// 控制卡片上点「取消投稿」之后的确认按钮。
pub fn peek_cancel_confirm_keyboard(post: PostId) -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new([[
        peek_button("确认取消投稿", PeekAction::ConfirmCancel(post)),
        peek_button("« 返回", PeekAction::Back(post)),
    ]])
}

pub fn peek_removed(post: PostId, owner: &str) -> String {
    format!("投稿 #{} 已撤下，已通知 {owner}。", post.0)
}

pub fn remove_reason_prompt(post: PostId, owner: &str) -> String {
    format!(
        "请回复本消息，写下撤下投稿 #{} 的理由（最多 300 个字符），会发给 {owner}。",
        post.0
    )
}

/// 投稿所属管理员收到的通知。
pub fn post_removed(post: PostId, category: Option<&str>, by: &str, note: Option<&str>) -> String {
    let queue = match category {
        Some(category) => format!("「{category}」队列"),
        None => "队列".to_owned(),
    };
    let mut text = format!(
        "你的投稿 #{} 被超级管理员 {by} 从{queue}里撤下了，不会再发布。",
        post.0
    );
    if let Some(note) = note {
        text.push_str(&format!("\n理由：{note}"));
    }
    text
}

/// 投稿所属管理员那条带「取消」按钮的控制消息，被撤下后的样子。
pub fn control_removed(post: PostId) -> String {
    format!("投稿 #{} 已被超级管理员撤下。", post.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(media: &[MediaKind], text: Option<&str>) -> PeekItem {
        PeekItem {
            id: PostId(1),
            category: Category {
                id: CategoryId(1),
                label: "原神".to_owned(),
            },
            media: media.to_vec(),
            text: text.map(ToOwned::to_owned),
        }
    }

    #[test]
    fn summaries_show_the_media_and_the_start_of_the_text() {
        assert_eq!(peek_summary(&item(&[], Some("你好\n世界"))), "你好 世界");
        assert_eq!(peek_summary(&item(&[], None)), "（空）");
        assert_eq!(
            peek_summary(&item(&[MediaKind::Photo; 3], Some("说明"))),
            "[图片×3] 说明"
        );
        assert_eq!(peek_summary(&item(&[MediaKind::Video], None)), "[视频×1]");
        assert_eq!(
            peek_summary(&item(&[MediaKind::Photo, MediaKind::Video], None)),
            "[媒体×2]"
        );
    }

    #[test]
    fn long_text_is_cut_with_an_ellipsis() {
        let summary = peek_summary(&item(&[], Some(&"字".repeat(40))));
        assert_eq!(summary.chars().count(), 31);
        assert!(summary.ends_with('…'));
    }

    #[test]
    fn page_counts_round_up_and_are_at_least_one() {
        assert_eq!(peek_page_count(0), 1);
        assert_eq!(peek_page_count(6), 1);
        assert_eq!(peek_page_count(11), 2);
        assert_eq!(peek_page_count(18), 3);
    }

    fn buttons(keyboard: &InlineKeyboardMarkup) -> Vec<String> {
        keyboard
            .inline_keyboard
            .iter()
            .flatten()
            .map(|button| button.text.clone())
            .collect()
    }

    fn rows(ids: std::ops::RangeInclusive<i64>) -> Vec<PeekRow> {
        ids.map(|id| PeekRow {
            at: None,
            item: PeekItem {
                id: PostId(id),
                ..item(&[], Some("x"))
            },
        })
        .collect()
    }

    #[test]
    fn the_list_keyboard_pages_and_offers_to_send_the_page() {
        let page = rows(1..=8);
        let middle = peek_list_keyboard(UserId(9), 18, 1, &page, true);
        assert_eq!(
            buttons(&middle),
            [
                "#1",
                "#2",
                "#3",
                "#4",
                "#5",
                "#6",
                "#7",
                "#8",
                "« 上一页",
                "下一页 »",
                "发送本页内容",
                "« 返回"
            ]
        );
        let first = peek_list_keyboard(UserId(9), 18, 0, &page, true);
        assert!(!buttons(&first).contains(&"« 上一页".to_owned()));
        let last = peek_list_keyboard(UserId(9), 18, 2, &page, true);
        assert!(!buttons(&last).contains(&"下一页 »".to_owned()));
        let own = peek_list_keyboard(UserId(9), 18, 2, &page, false);
        assert!(!buttons(&own).contains(&"« 返回".to_owned()));
    }

    #[test]
    fn an_empty_list_only_offers_to_go_back() {
        let keyboard = peek_list_keyboard(UserId(9), 0, 0, &[], true);
        assert_eq!(buttons(&keyboard), ["« 返回"]);
        let own = peek_list_keyboard(UserId(9), 0, 0, &[], false);
        assert!(buttons(&own).is_empty());
    }

    #[test]
    fn the_list_shows_when_each_post_is_sent() {
        let mut page = rows(1..=2);
        page[0].at = Some("2026-01-02T09:00:00Z".parse().unwrap());
        let text = peek_list("Bob", 2, 0, &page, &TimeZone::UTC);
        assert_eq!(
            text,
            "Bob：共 2 篇排队投稿，第 1/1 页，按发送顺序排列\n\
             按当前队列预测，之后新排进的投稿会排到前面。\n\
             #1 · 01-02 09:00 · 原神 · x\n\
             #2 · 没有时段会发 · 原神 · x"
        );
        assert_eq!(
            peek_list("Bob", 0, 0, &[], &TimeZone::UTC),
            "Bob：没有排队的投稿。"
        );
    }

    #[test]
    fn own_posts_can_be_cancelled_and_others_removed() {
        assert_eq!(buttons(&peek_card_keyboard(PostId(1), true)), ["取消投稿"]);
        assert_eq!(
            buttons(&peek_card_keyboard(PostId(1), false)),
            ["撤下", "撤下并写理由"]
        );
        assert_eq!(
            buttons(&peek_cancel_confirm_keyboard(PostId(1))),
            ["确认取消投稿", "« 返回"]
        );
    }

    #[test]
    fn the_removal_notice_names_who_removed_it_and_why() {
        assert_eq!(
            post_removed(PostId(12), Some("原神"), "Alice", Some("重复了")),
            "你的投稿 #12 被超级管理员 Alice 从「原神」队列里撤下了，不会再发布。\n理由：重复了"
        );
        assert_eq!(
            post_removed(PostId(12), None, "Alice", None),
            "你的投稿 #12 被超级管理员 Alice 从队列里撤下了，不会再发布。"
        );
    }

    #[test]
    fn queued_posts_offer_to_change_the_category_above_the_post_actions() {
        assert_eq!(
            buttons(&queued_keyboard(PostId(1), false)),
            ["改分类", "编辑图文", "立即发送", "取消投稿"]
        );
        assert_eq!(
            buttons(&queued_keyboard(PostId(1), true)),
            ["改分类", "编辑图文", "自动标签", "立即发送", "取消投稿"]
        );
        let category = |id, label: &str| Category {
            id: CategoryId(id),
            label: label.to_owned(),
        };
        let keyboard = move_keyboard(&[category(1, "原神"), category(2, "崩铁")], PostId(1));
        assert_eq!(buttons(&keyboard), ["原神", "崩铁", "« 返回"]);
    }

    #[test]
    fn draft_categories_are_laid_out_four_per_row() {
        let categories: Vec<_> = (1..=6)
            .map(|id| Category {
                id: CategoryId(id),
                label: format!("c{id}"),
            })
            .collect();
        let keyboard = category_keyboard(&categories, PostId(1), true);
        let row_sizes: Vec<_> = keyboard.inline_keyboard.iter().map(Vec::len).collect();
        assert_eq!(row_sizes, [4, 2, 2, 2]);

        let keyboard = move_keyboard(&categories, PostId(1));
        let row_sizes: Vec<_> = keyboard.inline_keyboard.iter().map(Vec::len).collect();
        assert_eq!(row_sizes, [4, 2, 1]);
    }

    #[test]
    fn drafts_show_the_post_actions_in_two_rows() {
        let categories = [Category {
            id: CategoryId(1),
            label: "原神".to_owned(),
        }];
        let with_tags = category_keyboard(&categories, PostId(1), true);
        assert_eq!(
            buttons(&with_tags),
            ["原神", "编辑图文", "自动标签", "立即发送", "取消投稿"]
        );
        let rows: Vec<_> = with_tags.inline_keyboard.iter().skip(1).collect();
        assert_eq!(rows[0].len(), 2);
        assert_eq!(rows[1].len(), 2);

        let without_tags = category_keyboard(&categories, PostId(1), false);
        assert_eq!(
            buttons(&without_tags),
            ["原神", "编辑图文", "立即发送", "取消投稿"]
        );
    }

    #[test]
    fn the_auto_tag_button_is_added_to_review_cards_only_when_enabled() {
        let plain = review_keyboard(PostId(1), false);
        assert_eq!(with_auto_tag_button(plain.clone(), PostId(1), false), plain);
        assert_eq!(
            buttons(&with_auto_tag_button(plain, PostId(1), true)),
            ["通过", "拒绝", "拒绝并写理由", "自动标签"]
        );
    }

    #[test]
    fn the_edit_menu_offers_single_items_only_for_albums() {
        assert_eq!(
            buttons(&edit_post_keyboard(PostId(1), 1)),
            ["编辑文字", "替换媒体", "« 返回"]
        );
        let album = edit_post_keyboard(PostId(1), 7);
        assert_eq!(
            buttons(&album),
            [
                "编辑文字",
                "替换全部媒体",
                "第 1 项",
                "第 2 项",
                "第 3 项",
                "第 4 项",
                "第 5 项",
                "第 6 项",
                "第 7 项",
                "« 返回"
            ]
        );
        let row_sizes: Vec<_> = album.inline_keyboard.iter().map(Vec::len).collect();
        assert_eq!(row_sizes, [2, 5, 2, 1]);
    }

    #[test]
    fn sending_now_asks_for_confirmation() {
        assert_eq!(
            buttons(&send_now_confirm_keyboard(PostId(1))),
            ["确认立即发送", "« 返回"]
        );
    }

    #[test]
    fn cancelling_asks_for_confirmation() {
        assert_eq!(
            buttons(&cancel_confirm_keyboard(PostId(1))),
            ["确认取消投稿", "« 返回"]
        );
    }

    #[test]
    fn stock_reminder_lists_unsorted_drafts_and_pending_reviews_only_when_there_are_some() {
        let mut counts = StockCounts::default();
        let none = stock_reminder(&[], &counts, 0, 0, &[]);
        assert!(!none.contains(DRAFT_TAG));
        assert!(!none.contains(PENDING_REVIEW_TAG));
        counts.draft = 2;
        let some = stock_reminder(&[], &counts, 0, 3, &[]);
        assert!(some.contains(&format!("待分类稿件：2 {DRAFT_TAG}")));
        assert!(some.contains(&format!("待审核投稿：3 {PENDING_REVIEW_TAG}")));
    }

    #[test]
    fn review_cards_offer_a_reject_with_reason_button() {
        let keyboard = review_keyboard(PostId(1), false);
        assert_eq!(buttons(&keyboard), ["通过", "拒绝", "拒绝并写理由"]);
        assert_eq!(
            buttons(&review_keyboard(PostId(1), true)),
            ["通过", "拒绝", "拒绝并写理由", "清空文字"]
        );
    }
}
