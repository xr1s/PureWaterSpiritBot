use teloxide::types::UserId;

use super::help::{HelpPage, HelpTopic, HelpView};
use crate::model::{AttemptId, CategoryId, PickId, PostId, SlotId};

/// 内联键盘按钮的载荷，按 Telegram 要求最多 64 字节。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CallbackData {
    /// 把帖子放入队列。
    Queue { post: PostId, category: CategoryId },
    /// 判定一次结果未知的发布尝试是否已到达频道。
    Resolve { attempt: AttemptId, published: bool },
    /// 草稿或已入队的帖子：点「取消投稿」，把按钮换成确认。
    Cancel { post: PostId },
    /// 确认取消投稿：放弃尚未发布的帖子。
    ConfirmCancel { post: PostId },
    /// 点「编辑图文」：把按钮换成可以编辑的内容；纯文字帖子直接编辑文字。
    EditPost { post: PostId },
    /// 编辑帖子的文字：弹出一条要求回复新文字的提示。
    EditPostText { post: PostId },
    /// 替换帖子的媒体：弹出一条要求回复新媒体的提示。`item` 是要替换的位置
    /// （从 0 开始），`None` 表示替换全部媒体。
    ReplaceMedia { post: PostId, item: Option<usize> },
    /// 已入队的帖子：展开分类列表，准备改分类。
    ChangeCategory { post: PostId },
    /// 已入队的帖子：改到这个分类。
    MoveTo { post: PostId, category: CategoryId },
    /// 已入队的帖子：不改了，收起分类列表。
    KeepCategory { post: PostId },
    /// 打开帮助：`None` 是命令列表，`Some` 是某个命令的详细介绍。
    Help(HelpView),
    /// 分类、时间表和设置的编辑面板上的操作。
    Edit(EditAction),
    /// 普通用户把草稿提交审核。
    Submit { post: PostId },
    /// 普通用户撤回草稿或待审核的投稿。
    Withdraw { post: PostId },
    /// 审核群里审核卡片上的操作。
    Review(ReviewAction),
    /// 超级管理员 `/peek` 里的操作。
    Peek(PeekAction),
    /// 让 AI 从词表里选出帖子图片里出现的角色或作品，追加成 hashtag。
    AutoTag { post: PostId },
    /// 草稿或已入队的帖子：点「立即发送」，把按钮换成确认。
    SendNow { post: PostId },
    /// 确认立即发送：不等发布时段，马上发到频道。
    ConfirmSendNow { post: PostId },
    /// 不发送也不取消了，收起确认，换回帖子原来的按钮。
    BackToPost { post: PostId },
}

/// `/peek` 里的按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeekAction {
    /// 超级管理员选要查看的管理员。
    Admins,
    /// 某个管理员按发送顺序排列的排队投稿，第 `page` 页，从 0 开始。
    List { admin: UserId, page: u32 },
    /// 把这一页的投稿内容都发出来。
    SendPage { admin: UserId, page: u32 },
    /// 发出一篇投稿的内容和它的控制卡片。
    View(PostId),
    /// 超级管理员撤下别人的投稿：直接撤下。
    Remove(PostId),
    /// 超级管理员撤下别人的投稿：要求回复一条理由。
    RemoveWithReason(PostId),
    /// 自己的投稿：点「取消投稿」，把按钮换成确认。
    Cancel(PostId),
    /// 确认取消自己的投稿。
    ConfirmCancel(PostId),
    /// 不取消了，换回卡片原来的按钮。
    Back(PostId),
}

impl PeekAction {
    pub fn encode(&self) -> String {
        match self {
            Self::Admins => "p".to_owned(),
            Self::List { admin, page } => format!("pl:{}:{page}", admin.0),
            Self::SendPage { admin, page } => format!("ps:{}:{page}", admin.0),
            Self::View(post) => format!("pv:{}", post.0),
            Self::Remove(post) => format!("px:{}", post.0),
            Self::RemoveWithReason(post) => format!("pw:{}", post.0),
            Self::Cancel(post) => format!("pc:{}", post.0),
            Self::ConfirmCancel(post) => format!("pk:{}", post.0),
            Self::Back(post) => format!("pb:{}", post.0),
        }
    }

    pub fn decode(data: &str) -> Option<Self> {
        if data == "p" {
            return Some(Self::Admins);
        }
        let mut parts = data.split(':');
        let tag = parts.next()?;
        let numbers: Vec<&str> = parts.collect();
        let post = |numbers: &[&str]| match numbers {
            [post] => Some(PostId(post.parse().ok()?)),
            _ => None,
        };
        let page = |numbers: &[&str]| match numbers {
            [admin, page] => Some((UserId(admin.parse().ok()?), page.parse().ok()?)),
            _ => None,
        };
        match tag {
            "pl" => page(&numbers).map(|(admin, page)| Self::List { admin, page }),
            "ps" => page(&numbers).map(|(admin, page)| Self::SendPage { admin, page }),
            "pv" => post(&numbers).map(Self::View),
            "px" => post(&numbers).map(Self::Remove),
            "pw" => post(&numbers).map(Self::RemoveWithReason),
            "pc" => post(&numbers).map(Self::Cancel),
            "pk" => post(&numbers).map(Self::ConfirmCancel),
            "pb" => post(&numbers).map(Self::Back),
            _ => None,
        }
    }
}

/// 审核群里审核卡片上的按钮。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewAction {
    /// 点「通过」：按钮换成点击者自己的分类。
    Approve(PostId),
    /// 选好分类，认领并通过。
    Category { post: PostId, category: CategoryId },
    /// 从分类列表回到「通过 / 拒绝」。
    Back(PostId),
    /// 用默认理由拒绝。
    Reject(PostId),
    /// 拒绝并写理由：要求回复一条理由。
    RejectWithReason(PostId),
    /// 清空媒体投稿的文字。
    ClearText(PostId),
}

impl ReviewAction {
    pub fn encode(&self) -> String {
        match *self {
            Self::Approve(post) => format!("va:{}", post.0),
            Self::Category { post, category } => format!("vc:{}:{}", post.0, category.0),
            Self::Back(post) => format!("vb:{}", post.0),
            Self::Reject(post) => format!("vx:{}", post.0),
            Self::RejectWithReason(post) => format!("vw:{}", post.0),
            Self::ClearText(post) => format!("vt:{}", post.0),
        }
    }

    pub fn decode(data: &str) -> Option<Self> {
        let mut parts = data.split(':');
        let tag = parts.next()?;
        let post = PostId(parts.next()?.parse().ok()?);
        let category: Option<i64> = parts.next().map(str::parse).transpose().ok()?;
        if parts.next().is_some() {
            return None;
        }
        match (tag, category) {
            ("va", None) => Some(Self::Approve(post)),
            ("vc", Some(category)) => Some(Self::Category {
                post,
                category: CategoryId(category),
            }),
            ("vb", None) => Some(Self::Back(post)),
            ("vx", None) => Some(Self::Reject(post)),
            ("vw", None) => Some(Self::RejectWithReason(post)),
            ("vt", None) => Some(Self::ClearText(post)),
            _ => None,
        }
    }
}

/// 管理员设置里可以修改的一项。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Timezone,
    MisfireGrace,
    SendInterval,
    Reminder,
}

impl SettingKind {
    const fn code(self) -> &'static str {
        match self {
            Self::Timezone => "z",
            Self::MisfireGrace => "g",
            Self::SendInterval => "i",
            Self::Reminder => "r",
        }
    }

    fn from_code(code: &str) -> Option<Self> {
        [
            Self::Timezone,
            Self::MisfireGrace,
            Self::SendInterval,
            Self::Reminder,
        ]
        .into_iter()
        .find(|kind| kind.code() == code)
    }
}

/// 编辑面板上的一个操作。分为三类：打开某个面板、弹出提示等待用户回复输入、
/// 直接修改数据。带 `bool` 的删除类操作需要确认：`false` 是询问，`true` 是确认。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditAction {
    Categories,
    NewCategory,
    Category(CategoryId),
    RenameCategory(CategoryId),
    MoveCategory {
        category: CategoryId,
        up: bool,
    },
    ArchiveCategory {
        category: CategoryId,
        confirmed: bool,
    },
    Schedule,
    NewSlot,
    Slot(SlotId),
    SlotTime(SlotId),
    DeleteSlot {
        slot: SlotId,
        confirmed: bool,
    },
    /// 给时段添加配额：选择分类。
    AddPick(SlotId),
    /// 给时段添加配额：已选好分类，等待输入数量。
    AddPickCategory {
        slot: SlotId,
        category: CategoryId,
    },
    Pick(PickId),
    PickCount(PickId),
    DeletePick(PickId),
    /// 给配额添加补位：选择分类。
    AddFallback(PickId),
    AddFallbackCategory {
        pick: PickId,
        category: CategoryId,
    },
    RemoveFallback {
        pick: PickId,
        category: CategoryId,
    },
    Settings,
    Setting(SettingKind),
}

impl EditAction {
    pub fn encode(&self) -> String {
        match *self {
            Self::Categories => "ec".to_owned(),
            Self::NewCategory => "ecn".to_owned(),
            Self::Category(id) => format!("ecv:{}", id.0),
            Self::RenameCategory(id) => format!("ecr:{}", id.0),
            Self::MoveCategory { category, up } => {
                format!("ec{}:{}", if up { "u" } else { "d" }, category.0)
            }
            Self::ArchiveCategory {
                category,
                confirmed,
            } => format!("ec{}:{}", if confirmed { "x" } else { "a" }, category.0),
            Self::Schedule => "es".to_owned(),
            Self::NewSlot => "esn".to_owned(),
            Self::Slot(id) => format!("esv:{}", id.0),
            Self::SlotTime(id) => format!("est:{}", id.0),
            Self::DeleteSlot { slot, confirmed } => {
                format!("es{}:{}", if confirmed { "x" } else { "d" }, slot.0)
            }
            Self::AddPick(id) => format!("esp:{}", id.0),
            Self::AddPickCategory { slot, category } => {
                format!("espc:{}:{}", slot.0, category.0)
            }
            Self::Pick(id) => format!("epv:{}", id.0),
            Self::PickCount(id) => format!("epn:{}", id.0),
            Self::DeletePick(id) => format!("epx:{}", id.0),
            Self::AddFallback(id) => format!("epf:{}", id.0),
            Self::AddFallbackCategory { pick, category } => {
                format!("epfc:{}:{}", pick.0, category.0)
            }
            Self::RemoveFallback { pick, category } => {
                format!("epr:{}:{}", pick.0, category.0)
            }
            Self::Settings => "et".to_owned(),
            Self::Setting(kind) => format!("et{}", kind.code()),
        }
    }

    pub fn decode(data: &str) -> Option<Self> {
        let mut parts = data.split(':');
        let tag = parts.next()?;
        let first: Option<i64> = parts.next().map(str::parse).transpose().ok()?;
        let second: Option<i64> = parts.next().map(str::parse).transpose().ok()?;
        if parts.next().is_some() {
            return None;
        }
        let category = |value: Option<i64>| value.map(CategoryId);
        let action = match (tag, first, second) {
            ("ec", None, None) => Self::Categories,
            ("ecn", None, None) => Self::NewCategory,
            ("ecv", Some(id), None) => Self::Category(CategoryId(id)),
            ("ecr", Some(id), None) => Self::RenameCategory(CategoryId(id)),
            ("ecu" | "ecd", Some(id), None) => Self::MoveCategory {
                category: CategoryId(id),
                up: tag == "ecu",
            },
            ("eca" | "ecx", Some(id), None) => Self::ArchiveCategory {
                category: CategoryId(id),
                confirmed: tag == "ecx",
            },
            ("es", None, None) => Self::Schedule,
            ("esn", None, None) => Self::NewSlot,
            ("esv", Some(id), None) => Self::Slot(SlotId(id)),
            ("est", Some(id), None) => Self::SlotTime(SlotId(id)),
            ("esd" | "esx", Some(id), None) => Self::DeleteSlot {
                slot: SlotId(id),
                confirmed: tag == "esx",
            },
            ("esp", Some(id), None) => Self::AddPick(SlotId(id)),
            ("espc", Some(slot), Some(_)) => Self::AddPickCategory {
                slot: SlotId(slot),
                category: category(second)?,
            },
            ("epv", Some(id), None) => Self::Pick(PickId(id)),
            ("epn", Some(id), None) => Self::PickCount(PickId(id)),
            ("epx", Some(id), None) => Self::DeletePick(PickId(id)),
            ("epf", Some(id), None) => Self::AddFallback(PickId(id)),
            ("epfc", Some(pick), Some(_)) => Self::AddFallbackCategory {
                pick: PickId(pick),
                category: category(second)?,
            },
            ("epr", Some(pick), Some(_)) => Self::RemoveFallback {
                pick: PickId(pick),
                category: category(second)?,
            },
            ("et", None, None) => Self::Settings,
            (tag, None, None) => Self::Setting(SettingKind::from_code(tag.strip_prefix("et")?)?),
            _ => return None,
        };
        Some(action)
    }
}

impl CallbackData {
    pub fn encode(&self) -> String {
        match self {
            Self::Queue { post, category } => format!("q:{}:{}", post.0, category.0),
            Self::Resolve { attempt, published } => {
                format!("r:{}:{}", attempt.0, if *published { "p" } else { "n" })
            }
            Self::Cancel { post } => format!("c:{}", post.0),
            Self::ConfirmCancel { post } => format!("x:{}", post.0),
            Self::EditPost { post } => format!("t:{}", post.0),
            Self::EditPostText { post } => format!("u:{}", post.0),
            Self::ReplaceMedia { post, item: None } => format!("i:{}", post.0),
            Self::ReplaceMedia {
                post,
                item: Some(item),
            } => format!("i:{}:{item}", post.0),
            Self::ChangeCategory { post } => format!("m:{}", post.0),
            Self::MoveTo { post, category } => format!("g:{}:{}", post.0, category.0),
            Self::KeepCategory { post } => format!("k:{}", post.0),
            Self::Help(HelpView::Menu(HelpPage::Commands)) => "h".to_owned(),
            Self::Help(HelpView::Menu(HelpPage::Review)) => "hr".to_owned(),
            Self::Help(HelpView::Topic(topic)) => format!("h:{}", topic.name()),
            Self::Edit(action) => action.encode(),
            Self::Submit { post } => format!("s:{}", post.0),
            Self::Withdraw { post } => format!("w:{}", post.0),
            Self::Review(action) => action.encode(),
            Self::Peek(action) => action.encode(),
            Self::AutoTag { post } => format!("a:{}", post.0),
            Self::SendNow { post } => format!("n:{}", post.0),
            Self::ConfirmSendNow { post } => format!("y:{}", post.0),
            Self::BackToPost { post } => format!("b:{}", post.0),
        }
    }

    pub fn decode(data: &str) -> Option<Self> {
        if data.starts_with('e') {
            return EditAction::decode(data).map(Self::Edit);
        }
        if data.starts_with('v') {
            return ReviewAction::decode(data).map(Self::Review);
        }
        if data.starts_with('p') {
            return PeekAction::decode(data).map(Self::Peek);
        }
        if data == "h" {
            return Some(Self::Help(HelpView::Menu(HelpPage::Commands)));
        }
        if data == "hr" {
            return Some(Self::Help(HelpView::Menu(HelpPage::Review)));
        }
        let (tag, rest) = data.split_once(':')?;
        if tag == "h" {
            return Some(Self::Help(HelpView::Topic(HelpTopic::from_name(rest)?)));
        }
        if tag == "c" {
            return Some(Self::Cancel {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "x" {
            return Some(Self::ConfirmCancel {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "t" {
            return Some(Self::EditPost {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "u" {
            return Some(Self::EditPostText {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "i" {
            let (post, item) = match rest.split_once(':') {
                Some((post, item)) => (post, Some(item.parse().ok()?)),
                None => (rest, None),
            };
            return Some(Self::ReplaceMedia {
                post: PostId(post.parse().ok()?),
                item,
            });
        }
        if tag == "m" {
            return Some(Self::ChangeCategory {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "k" {
            return Some(Self::KeepCategory {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "s" {
            return Some(Self::Submit {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "w" {
            return Some(Self::Withdraw {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "a" {
            return Some(Self::AutoTag {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "n" {
            return Some(Self::SendNow {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "y" {
            return Some(Self::ConfirmSendNow {
                post: PostId(rest.parse().ok()?),
            });
        }
        if tag == "b" {
            return Some(Self::BackToPost {
                post: PostId(rest.parse().ok()?),
            });
        }
        let (id, argument) = rest.split_once(':')?;
        let id = id.parse().ok()?;
        match tag {
            "q" => Some(Self::Queue {
                post: PostId(id),
                category: CategoryId(argument.parse().ok()?),
            }),
            "g" => Some(Self::MoveTo {
                post: PostId(id),
                category: CategoryId(argument.parse().ok()?),
            }),
            "r" => Some(Self::Resolve {
                attempt: AttemptId(id),
                published: match argument {
                    "p" => true,
                    "n" => false,
                    _ => return None,
                },
            }),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let queue = CallbackData::Queue {
            post: PostId(i64::MAX),
            category: CategoryId(i64::MAX),
        };
        assert!(queue.encode().len() <= 64);
        assert_eq!(CallbackData::decode(&queue.encode()), Some(queue));

        for published in [true, false] {
            let resolve = CallbackData::Resolve {
                attempt: AttemptId(7),
                published,
            };
            assert_eq!(CallbackData::decode(&resolve.encode()), Some(resolve));
        }

        let post = PostId(i64::MAX);
        for data in [
            CallbackData::Cancel { post },
            CallbackData::ConfirmCancel { post },
            CallbackData::EditPost { post },
            CallbackData::EditPostText { post },
            CallbackData::ReplaceMedia { post, item: None },
            CallbackData::ReplaceMedia {
                post,
                item: Some(usize::MAX),
            },
            CallbackData::ChangeCategory { post },
            CallbackData::KeepCategory { post },
            CallbackData::AutoTag { post },
            CallbackData::SendNow { post },
            CallbackData::ConfirmSendNow { post },
            CallbackData::BackToPost { post },
            CallbackData::MoveTo {
                post,
                category: CategoryId(i64::MAX),
            },
        ] {
            assert!(data.encode().len() <= 64);
            assert_eq!(CallbackData::decode(&data.encode()), Some(data));
        }

        for page in [HelpPage::Commands, HelpPage::Review] {
            let menu = CallbackData::Help(HelpView::Menu(page));
            assert_eq!(CallbackData::decode(&menu.encode()), Some(menu));
        }
        for topic in HelpTopic::every() {
            let help = CallbackData::Help(HelpView::Topic(topic));
            assert_eq!(CallbackData::decode(&help.encode()), Some(help));
        }
    }

    fn every_edit_action() -> Vec<EditAction> {
        let (category, slot, pick) = (CategoryId(i64::MAX), SlotId(i64::MAX), PickId(i64::MAX));
        let mut actions = vec![
            EditAction::Categories,
            EditAction::NewCategory,
            EditAction::Category(category),
            EditAction::RenameCategory(category),
            EditAction::Schedule,
            EditAction::NewSlot,
            EditAction::Slot(slot),
            EditAction::SlotTime(slot),
            EditAction::AddPick(slot),
            EditAction::AddPickCategory { slot, category },
            EditAction::Pick(pick),
            EditAction::PickCount(pick),
            EditAction::DeletePick(pick),
            EditAction::AddFallback(pick),
            EditAction::AddFallbackCategory { pick, category },
            EditAction::RemoveFallback { pick, category },
            EditAction::Settings,
        ];
        for flag in [true, false] {
            actions.push(EditAction::MoveCategory { category, up: flag });
            actions.push(EditAction::ArchiveCategory {
                category,
                confirmed: flag,
            });
            actions.push(EditAction::DeleteSlot {
                slot,
                confirmed: flag,
            });
        }
        for kind in [
            SettingKind::Timezone,
            SettingKind::MisfireGrace,
            SettingKind::SendInterval,
            SettingKind::Reminder,
        ] {
            actions.push(EditAction::Setting(kind));
        }
        actions
    }

    #[test]
    fn edit_actions_round_trip_within_the_size_limit() {
        for action in every_edit_action() {
            let data = action.encode();
            assert!(data.len() <= 64, "{data}");
            assert_eq!(EditAction::decode(&data), Some(action), "{data}");
            assert_eq!(
                CallbackData::decode(&data),
                Some(CallbackData::Edit(action)),
                "{data}"
            );
        }
    }

    #[test]
    fn submission_and_review_buttons_round_trip() {
        let post = PostId(i64::MAX);
        let mut all = vec![
            CallbackData::Submit { post },
            CallbackData::Withdraw { post },
        ];
        for action in [
            ReviewAction::Approve(post),
            ReviewAction::Category {
                post,
                category: CategoryId(i64::MAX),
            },
            ReviewAction::Back(post),
            ReviewAction::Reject(post),
            ReviewAction::RejectWithReason(post),
            ReviewAction::ClearText(post),
        ] {
            all.push(CallbackData::Review(action));
        }
        for data in all {
            let encoded = data.encode();
            assert!(encoded.len() <= 64, "{encoded}");
            assert_eq!(CallbackData::decode(&encoded), Some(data), "{encoded}");
        }
    }

    #[test]
    fn peek_buttons_round_trip_and_fit_in_a_callback() {
        let admin = UserId(u64::MAX);
        let post = PostId(i64::MAX);
        for action in [
            PeekAction::Admins,
            PeekAction::List {
                admin,
                page: u32::MAX,
            },
            PeekAction::List { admin, page: 0 },
            PeekAction::SendPage {
                admin,
                page: u32::MAX,
            },
            PeekAction::View(post),
            PeekAction::Remove(post),
            PeekAction::RemoveWithReason(post),
            PeekAction::Cancel(post),
            PeekAction::ConfirmCancel(post),
            PeekAction::Back(post),
        ] {
            let data = CallbackData::Peek(action);
            let encoded = data.encode();
            assert!(encoded.len() <= 64, "{encoded}");
            assert_eq!(CallbackData::decode(&encoded), Some(data), "{encoded}");
        }
    }

    #[test]
    fn rejects_malformed_peek_buttons() {
        for data in [
            "pa:1", "pl:1", "pl:1:x", "pl:1:2:3", "ps:x:1", "pc", "pk:1:2", "pb:x", "pv", "pv:x",
            "px:1:2", "pz:1", "pp",
        ] {
            assert_eq!(CallbackData::decode(data), None, "{data}");
        }
    }

    #[test]
    fn rejects_malformed_review_buttons() {
        for data in [
            "s", "s:x", "s:1:2", "w:", "v", "va", "va:x", "vc:1", "vc:1:x", "va:1:2", "vq:1",
            "vb:1:2", "vx:1:1",
        ] {
            assert_eq!(CallbackData::decode(data), None, "{data}");
        }
    }

    #[test]
    fn rejects_malformed_edit_actions() {
        for data in [
            "e", "ecv", "ecv:x", "ecv:1:2", "espc:1", "espc:1:x", "es:1", "etq", "et:1", "ecz:1",
            "epr:1",
        ] {
            assert_eq!(EditAction::decode(data), None, "{data}");
            assert_eq!(CallbackData::decode(data), None, "{data}");
        }
    }

    #[test]
    fn rejects_garbage() {
        for data in [
            "",
            "q",
            "q:1",
            "q:x:1",
            "r:1:x",
            "z:1:1",
            "q:1:gi",
            "c",
            "c:x",
            "c:1:2",
            "x:x",
            "x:1:2",
            "h:",
            "h:x",
            "h:stock:1",
            "a:1:t",
            "n:x",
            "y:1:2",
            "b",
            "u:x",
            "i",
            "i:x",
            "i:1:x",
            "i:1:2:3",
        ] {
            assert_eq!(CallbackData::decode(data), None, "{data}");
        }
    }
}
