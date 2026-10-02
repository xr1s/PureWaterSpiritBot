use std::{num::NonZeroU16, time::Duration};

use anyhow::{Result, bail};
use jiff::{SignedDuration, Timestamp, civil::Date, civil::Time};
use sea_orm::{DeriveActiveEnum, EnumIter};
use serde::Deserialize;
use teloxide::types::{ChatId, LinkPreviewOptions, MessageEntity, MessageId, UserId};

macro_rules! integer_id {
    ($name:ident) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub i64);
    };
}

integer_id!(PostId);
integer_id!(RunId);
integer_id!(AttemptId);
integer_id!(CategoryId);
integer_id!(SlotId);
integer_id!(PickId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(try_from = "SignedDuration")]
pub struct PositiveDuration(Duration);

impl PositiveDuration {
    pub fn get(self) -> Duration {
        self.0
    }

    pub fn signed(self) -> SignedDuration {
        SignedDuration::try_from(self.0).expect("duration was created from a SignedDuration")
    }
}

impl TryFrom<SignedDuration> for PositiveDuration {
    type Error = anyhow::Error;

    fn try_from(value: SignedDuration) -> Result<Self> {
        if !value.is_positive() {
            bail!("duration must be positive");
        }
        Ok(Self(Duration::try_from(value)?))
    }
}

// 数字值是稳定的数据库契约；不得修改或复用已有数字值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i32", db_type = "Integer")]
#[repr(i32)]
pub enum PostStatus {
    Draft = 0,
    PendingReview = 1,
    Rejected = 2,
    Queued = 3,
    Reserved = 4,
    Published = 5,
    Failed = 6,
    Cancelled = 7,
}

impl PostStatus {
    /// 处于该状态的帖子内容是否仍可更改。
    pub const fn is_editable(self) -> bool {
        matches!(self, Self::Draft | Self::Queued)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i32", db_type = "Integer")]
#[repr(i32)]
pub enum MediaKind {
    Photo = 0,
    Video = 1,
    Animation = 2,
    Document = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i32", db_type = "Integer")]
#[repr(i32)]
pub enum RunStatus {
    Running = 0,
    Completed = 1,
    CompletedWithIssues = 2,
    Aborted = 3,
    Skipped = 4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i32", db_type = "Integer")]
#[repr(i32)]
pub enum AttemptStatus {
    Pending = 0,
    Sending = 1,
    Succeeded = 2,
    Requeued = 3,
    Failed = 4,
    Unknown = 5,
    NotAttempted = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i32", db_type = "Integer")]
#[repr(i32)]
pub enum NotificationKind {
    PublicationUnknown = 0,
    PublicationFailed = 1,
    RunAborted = 2,
    StockReminder = 3,
    SubmissionApproved = 4,
    SubmissionRejected = 5,
    PostRemoved = 6,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i32", db_type = "Integer")]
#[repr(i32)]
pub enum NotificationStatus {
    Pending = 0,
    Sending = 1,
    Sent = 2,
    Failed = 3,
    Cancelled = 4,
}

/// Telegram 一个相册最多允许这么多项。
pub const MAX_ALBUM_ITEMS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i32", db_type = "Integer")]
#[repr(i32)]
pub enum SubmitterStatus {
    Active = 0,
    Blocked = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i32", db_type = "Integer")]
#[repr(i32)]
pub enum ReviewMessageKind {
    Content = 0,
    Control = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumIter, DeriveActiveEnum)]
#[sea_orm(rs_type = "i32", db_type = "Integer")]
#[repr(i32)]
pub enum PostAuditKind {
    Submitted = 0,
    Withdrawn = 1,
    Approved = 2,
    Rejected = 3,
    TextReplaced = 4,
    TextCleared = 5,
    Removed = 6,
}

impl MediaKind {
    /// 这些类型的媒体能否作为一个相册一起发送：照片和视频
    /// 可以任意混合，文档只能与文档一起，动图不能放入相册。
    pub fn can_share_album(kinds: &[Self]) -> bool {
        let visual = |kind: &Self| matches!(kind, Self::Photo | Self::Video);
        let document = |kind: &Self| matches!(kind, Self::Document);
        kinds.iter().all(visual) || kinds.iter().all(document)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Media {
    pub kind: MediaKind,
    pub file_id: String,
    pub file_unique_id: String,
    pub has_spoiler: bool,
}

/// AI 生成、要写入帖子文字的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GeneratedText {
    /// 已经带 `#` 的 hashtag。
    Tags(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedTextChange {
    pub text: String,
    pub entities: Vec<MessageEntity>,
    pub added: String,
}

impl GeneratedText {
    /// 根据已有文字和实体生成更新后的内容；没有新内容时为 `None`。
    ///
    /// 已经在文字里的标签不会重复追加，所以同一个按钮可以放心多点几次。
    /// 标签会接在文字最后一行（如果那一行全是 hashtag）后面，否则另起一行。
    pub fn change(
        &self,
        existing: &str,
        entities: &[MessageEntity],
    ) -> Option<GeneratedTextChange> {
        match self {
            Self::Tags(tags) => {
                let added = tag_suffix(tags, existing)?;
                Some(GeneratedTextChange {
                    text: format!("{existing}{added}"),
                    entities: entities.to_vec(),
                    added,
                })
            }
        }
    }
}

fn tag_suffix(tags: &[String], existing: &str) -> Option<String> {
    let present: Vec<String> = existing.split_whitespace().map(str::to_lowercase).collect();
    let mut fresh: Vec<&str> = Vec::new();
    for tag in tags {
        let key = tag.to_lowercase();
        if !present.contains(&key) && !fresh.iter().any(|t| t.to_lowercase() == key) {
            fresh.push(tag);
        }
    }
    if fresh.is_empty() {
        return None;
    }
    let separator = if ends_with_hashtag_line(existing) {
        " "
    } else {
        tag_break(existing)
    };
    Some(format!("{separator}{}", fresh.join(" ")))
}

fn tag_break(existing: &str) -> &'static str {
    if existing.trim().is_empty() { "" } else { "\n" }
}

/// 文字的最后一行是否只由 hashtag 组成。
fn ends_with_hashtag_line(existing: &str) -> bool {
    let last = existing.lines().last().unwrap_or("");
    let mut words = last.split_whitespace().peekable();
    words.peek().is_some() && words.all(|word| word.starts_with('#') && word.len() > 1)
}

/// 一条 Telegram 消息的载荷：纯文本，或带说明文字的媒体项。
#[derive(Debug, Clone, PartialEq)]
pub struct Content {
    pub text: Option<String>,
    pub entities: Vec<MessageEntity>,
    pub link_preview_options: Option<LinkPreviewOptions>,
    pub show_caption_above_media: bool,
    pub media: Option<Media>,
}

#[derive(Debug, Clone)]
pub struct IncomingMessage {
    pub submitter: UserId,
    pub source_chat_id: ChatId,
    pub source_message_id: MessageId,
    pub media_group_id: Option<String>,
    /// 该消息所修改的帖子，当用户回复了其某条消息或替换媒体的提示时设置。
    pub target: Option<Target>,
    /// 用户从其他聊天回复的频道消息（如果有）。
    pub channel_reply: Option<MessageId>,
    pub content: Content,
}

/// 一条消息要怎样修改已有的帖子。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    /// 用户回复了帖子的某条消息：媒体追加到帖子末尾。
    Append(PostId),
    /// 管理员回复了替换媒体的提示 `prompt`：替换帖子中位置为 `item`（从 0 开始）
    /// 的那一项，`None` 表示替换全部媒体。
    Replace {
        post: PostId,
        item: Option<usize>,
        prompt: MessageId,
    },
}

/// 机器人发送到目标聊天的消息。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SentMessage {
    pub chat_id: ChatId,
    pub message_id: MessageId,
}

/// 已存储、可供发布的帖子。多于一条消息表示相册。
#[derive(Debug, Clone)]
pub struct Post {
    pub id: PostId,
    pub messages: Vec<Content>,
    /// 该帖子所答复的频道消息：用户回复了它，或者该帖子
    /// 是对以该消息发布的帖子的补充。
    pub reply_to: Option<MessageId>,
}

/// 选槽算法所看到的一个排队中的帖子。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub id: PostId,
    pub category: CategoryId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Category {
    pub id: CategoryId,
    pub label: String,
}

#[derive(Debug, Clone)]
pub struct Pick {
    pub id: PickId,
    pub category: CategoryId,
    pub count: NonZeroU16,
    pub fallback: Vec<CategoryId>,
}

#[derive(Debug, Clone)]
pub struct Slot {
    pub id: SlotId,
    /// 所属管理员时区下的本地时间。
    pub time: Time,
    /// 早于此时刻安排的触发不会再执行。
    pub effective_from: Timestamp,
    pub picks: Vec<Pick>,
}

impl Slot {
    pub fn planned_count(&self) -> u16 {
        self.picks.iter().map(|pick| pick.count.get()).sum()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotificationSubject {
    PublicationUnknown(AttemptId),
    PublicationFailed(AttemptId),
    RunAborted(RunId),
    StockReminder(Date),
    /// 投稿通过了审核，发给投稿人。
    SubmissionApproved(PostId),
    /// 投稿被拒绝，发给投稿人。
    SubmissionRejected(PostId),
    /// 已排队的投稿被超级管理员撤下，发给投稿所属的管理员。
    PostRemoved(PostId),
}

impl NotificationSubject {
    pub fn kind(self) -> NotificationKind {
        match self {
            Self::PublicationUnknown(_) => NotificationKind::PublicationUnknown,
            Self::PublicationFailed(_) => NotificationKind::PublicationFailed,
            Self::RunAborted(_) => NotificationKind::RunAborted,
            Self::StockReminder(_) => NotificationKind::StockReminder,
            Self::SubmissionApproved(_) => NotificationKind::SubmissionApproved,
            Self::SubmissionRejected(_) => NotificationKind::SubmissionRejected,
            Self::PostRemoved(_) => NotificationKind::PostRemoved,
        }
    }

    pub fn id(self) -> i64 {
        match self {
            Self::PublicationUnknown(id) | Self::PublicationFailed(id) => id.0,
            Self::RunAborted(id) => id.0,
            Self::SubmissionApproved(id) | Self::SubmissionRejected(id) | Self::PostRemoved(id) => {
                id.0
            }
            Self::StockReminder(date) => {
                i64::from(date.year()) * 10_000
                    + i64::from(date.month()) * 100
                    + i64::from(date.day())
            }
        }
    }

    pub fn from_parts(kind: NotificationKind, id: i64) -> Result<Self> {
        Ok(match kind {
            NotificationKind::PublicationUnknown => Self::PublicationUnknown(AttemptId(id)),
            NotificationKind::PublicationFailed => Self::PublicationFailed(AttemptId(id)),
            NotificationKind::RunAborted => Self::RunAborted(RunId(id)),
            NotificationKind::SubmissionApproved => Self::SubmissionApproved(PostId(id)),
            NotificationKind::SubmissionRejected => Self::SubmissionRejected(PostId(id)),
            NotificationKind::PostRemoved => Self::PostRemoved(PostId(id)),
            NotificationKind::StockReminder => {
                let year = i16::try_from(id / 10_000)?;
                let month = i8::try_from(id / 100 % 100)?;
                let day = i8::try_from(id % 100)?;
                Self::StockReminder(Date::new(year, month, day)?)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(names: &[&str]) -> GeneratedText {
        GeneratedText::Tags(names.iter().map(|name| (*name).to_owned()).collect())
    }

    fn added(generated: GeneratedText, existing: &str) -> Option<String> {
        generated.change(existing, &[]).map(|change| change.added)
    }

    #[test]
    fn tags_start_a_new_line_or_fill_the_text() {
        assert_eq!(added(tags(&["#a", "#b"]), "").as_deref(), Some("#a #b"));
        assert_eq!(added(tags(&["#a"]), "  \n").as_deref(), Some("#a"));
        assert_eq!(
            added(tags(&["#a", "#b"]), "caption").as_deref(),
            Some("\n#a #b")
        );
    }

    #[test]
    fn tags_join_a_trailing_hashtag_line() {
        assert_eq!(
            added(tags(&["#b"]), "caption\n\n#a").as_deref(),
            Some(" #b")
        );
        // 最后一行混着别的文字，就不算 hashtag 行。
        assert_eq!(
            added(tags(&["#b"]), "look #a here").as_deref(),
            Some("\n#b")
        );
    }

    #[test]
    fn tags_already_in_the_text_are_not_added_again() {
        assert_eq!(added(tags(&["#a", "#B"]), "x\n\n#A #b"), None);
        assert_eq!(added(tags(&["#a", "#b"]), "x #a").as_deref(), Some("\n#b"));
        assert_eq!(added(tags(&["#a", "#A"]), "").as_deref(), Some("#a"));
        assert_eq!(added(tags(&[]), "x"), None);
    }

    #[test]
    fn stock_reminder_subject_round_trips() {
        let subject = NotificationSubject::StockReminder(jiff::civil::date(2026, 9, 30));
        let restored = NotificationSubject::from_parts(subject.kind(), subject.id()).unwrap();
        assert_eq!(restored, subject);
    }

    #[test]
    fn submission_subjects_round_trip() {
        for subject in [
            NotificationSubject::SubmissionApproved(PostId(7)),
            NotificationSubject::SubmissionRejected(PostId(8)),
            NotificationSubject::PostRemoved(PostId(9)),
        ] {
            let restored = NotificationSubject::from_parts(subject.kind(), subject.id()).unwrap();
            assert_eq!(restored, subject);
        }
    }
}
