/// `/help` 的两页菜单：私聊里的常用命令，和审核群里的审核命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpPage {
    Commands,
    Review,
}

/// 帮助消息当前显示的内容：某一页菜单，或某个命令的详细介绍。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpView {
    Menu(HelpPage),
    Topic(HelpTopic),
}

/// `/help` 菜单里的一个条目，对应一个需要详细介绍的命令。
/// 两个 `/help` 本身都不在其中：它们的消息正文就是用法简介。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HelpTopic {
    Stock,
    Edit,
    Categories,
    Schedule,
    Settings,
    Reject,
    Block,
    Unblock,
    Pending,
    Peek,
}

impl HelpTopic {
    /// 私聊菜单的显示顺序，与 `Command` 一致。
    pub const COMMANDS: [Self; 6] = [
        Self::Stock,
        Self::Edit,
        Self::Categories,
        Self::Schedule,
        Self::Settings,
        Self::Peek,
    ];

    /// 审核菜单的显示顺序，与 `ReviewCommand` 一致。
    pub const REVIEW: [Self; 4] = [Self::Reject, Self::Block, Self::Unblock, Self::Pending];

    /// 所有条目。
    pub fn every() -> impl Iterator<Item = Self> {
        Self::COMMANDS.into_iter().chain(Self::REVIEW)
    }

    /// 这个条目在哪一页菜单里。
    pub fn page(self) -> HelpPage {
        if Self::REVIEW.contains(&self) {
            HelpPage::Review
        } else {
            HelpPage::Commands
        }
    }

    /// 命令名，不含斜杠；同时用作按钮载荷。
    pub fn name(self) -> &'static str {
        match self {
            Self::Stock => "stock",
            Self::Edit => "edit",
            Self::Categories => "categories",
            Self::Schedule => "schedule",
            Self::Settings => "settings",
            Self::Reject => "reject",
            Self::Block => "block",
            Self::Unblock => "unblock",
            Self::Pending => "pending",
            Self::Peek => "peek",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::every().find(|topic| topic.name() == name)
    }

    /// 菜单按钮上的文字。
    pub fn button_label(self) -> &'static str {
        match self {
            Self::Stock => "/stock 查看库存",
            Self::Edit => "/edit 编辑投稿",
            Self::Categories => "/categories 管理分类",
            Self::Schedule => "/schedule 管理发布时段",
            Self::Settings => "/settings 时区和提醒设置",
            Self::Reject => "/reject 拒绝投稿",
            Self::Block => "/block 拉黑投稿人",
            Self::Unblock => "/unblock 解除拉黑",
            Self::Pending => "/pending 待审核的投稿",
            Self::Peek => "/peek 按发送顺序查看投稿",
        }
    }

    /// 详细介绍页面的正文。
    pub fn details(self) -> &'static str {
        match self {
            Self::Stock => {
                "/stock：查看各分类的库存\n\n\
                 显示每个分类中已入队、等待发布的投稿数量，\n\
                 以及其他状态的投稿数量：\n\
                 · 待分类：已保存，但还没有选择队列\n\
                 · 发布中：正在发布\n\
                 · 失败：发布失败\n\n\
                 只统计你自己的投稿。"
            }
            Self::Edit => {
                "/edit：编辑投稿\n\n\
                 用法：\n\
                 · /edit <投稿编号>，例如 /edit 12 或 /edit #12\n\
                 · 回复投稿中的任意一条消息并发送 /edit\n\n\
                 Bot 会发回一张控制卡片，已入队的投稿可以改分类，也可以取消。\
                 之后回复卡片或投稿中的任意消息：\n\
                 · 发送文字，替换投稿文字\n\
                 · 发送图片、视频或文件，追加到投稿\n\
                 也可以直接编辑原消息。\n\n\
                 点卡片上的「编辑图文」，可以修改文字，或者替换媒体：\
                 整组替换，或者只替换相册中的某一项。\
                 原消息太久远、已经不能在 Telegram 里编辑时，可以用这种方式换图。\n\n\
                 投稿发布后不能再修改。回复已发布的投稿并发送图片、\
                 视频或文件，会作为补充投稿，发布时回复频道中的原消息。"
            }
            Self::Categories => {
                "/categories：管理你的分类\n\n\
                 分类是你的投稿队列。投稿时要选一个分类，\n\
                 发布时段会按配额从分类里取稿。分类只属于你自己。\n\n\
                 · 新增分类：点「新增分类」，再回复提示消息发送名称\n\
                 · 改名、调整顺序、删除：点击分类\n\n\
                 分类里还有排队的投稿，或者还被某个时段使用时，不能删除。"
            }
            Self::Schedule => {
                "/schedule：管理你的发布时段\n\n\
                 每个时段有一个每天的发布时间，以及若干配额：\n\
                 每天在这个时间从某个分类取几条。\n\n\
                 · 新增时段：点「新增时段」，回复 HH:MM 格式的时间\n\
                 · 添加配额：进入时段，点「添加配额」，选分类再回复数量\n\
                 · 补位：某个分类的稿子不够时，按顺序从补位分类里补。\
                 进入配额，点「添加补位」；点已有的补位可以去掉\n\n\
                 时间按你的时区计算，可以在 /settings 里修改。\
                 新建或修改后的时段从现在起生效，不会补发今天已经过去的时间。"
            }
            Self::Settings => {
                "/settings：修改你的设置\n\n\
                 · 时区：发布时段和提醒使用的时区，例如 Asia/Shanghai\n\
                 · 补发宽限期：错过发布时间（例如 Bot 停机）后，\
                 在这段时间内仍会补发，超过就跳过\n\
                 · 发送间隔：同一时段里两条投稿之间的间隔\n\
                 · 库存提醒：每天这个时间私聊你当前的库存和发布计划\n\n\
                 点击要改的项，再回复提示消息。回复「默认」恢复默认值，\
                 库存提醒回复「关闭」不再提醒。"
            }
            Self::Peek => {
                "/peek：按发送顺序查看排队的投稿\n\n\
                 用法：发送 /peek，得到你的排队投稿列表，最先发出的在前，\
                 每篇标出预计发出的时间，每页 8 篇。\
                 「没有时段会发」表示现在的发布时段不会取到这篇投稿。\n\n\
                 · 点「#编号」：把这篇投稿原样发给你，并附一张控制卡片\n\
                 · 点「发送本页内容」：一次发出本页的所有投稿\n\
                 · 卡片上点「取消投稿」：确认后取消，不能恢复\n\n\
                 时间是按当前队列预测的：发布时段优先取最晚入队的投稿，\
                 之后新排进的投稿会排到前面。\n\n\
                 超级管理员发送 /peek 后先选管理员，可以查看任何管理员的队列。\
                 别人的投稿卡片上有「撤下」和「撤下并写理由」，\
                 撤下后不会再发布，原管理员会收到通知。\
                 已经被发布时段选中、正在发布的投稿不能撤下。"
            }
            Self::Reject => {
                "/reject：拒绝投稿\n\n\
                 用法：回复审核卡片（投稿内容，或下面带按钮的消息），\
                 发送 /reject 理由。\n\n\
                 · 理由会发给投稿人；只发 /reject 不写理由，\
                 就用默认理由，没有配置默认理由则只告知结果\n\
                 · 卡片上的「拒绝」按钮一律用默认理由；\
                 点「拒绝并写理由」，再回复一条理由\n\
                 · 拒绝后不能撤销，投稿人需要重新投稿"
            }
            Self::Block => {
                "/block：拉黑投稿人\n\n\
                 用法：回复这位投稿人的任意一张审核卡片，\
                 发送 /block 原因。\n\n\
                 · 被拉黑的人不能再投稿\n\
                 · 原因只做记录，不会告诉对方\n\
                 · 他已经提交的投稿不受影响，仍需要处理"
            }
            Self::Unblock => {
                "/unblock：解除拉黑\n\n\
                 用法：回复被拉黑的投稿人的任意一张审核卡片，发送 /unblock。\n\n\
                 需要找到他的一张审核卡片；没有的话，\
                 可以让他重新投一篇，再对新卡片操作。"
            }
            Self::Pending => {
                "/pending：查看待审核的投稿\n\n\
                 显示待审核的总数，以及最早的 10 篇，附带审核卡片的链接，\
                 点击可以跳到对应的卡片。\n\n\
                 群里的命令建议从命令菜单选择，\
                 或者带上机器人的名字，例如 /pending@机器人名。"
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use teloxide::utils::command::BotCommands;

    use super::*;
    use crate::bot::{Command, ReviewCommand};

    #[test]
    fn names_round_trip() {
        for topic in HelpTopic::every() {
            assert_eq!(HelpTopic::from_name(topic.name()), Some(topic));
        }
        assert_eq!(HelpTopic::from_name("start"), None);
        assert_eq!(HelpTopic::from_name("help"), None);
    }

    #[test]
    fn topics_know_their_page() {
        for topic in HelpTopic::COMMANDS {
            assert_eq!(topic.page(), HelpPage::Commands);
        }
        for topic in HelpTopic::REVIEW {
            assert_eq!(topic.page(), HelpPage::Review);
        }
    }

    fn topic_names(topics: &[HelpTopic]) -> Vec<String> {
        topics
            .iter()
            .map(|topic| format!("/{}", topic.name()))
            .collect()
    }

    fn visible_except_help(commands: Vec<teloxide::types::BotCommand>) -> Vec<String> {
        commands
            .into_iter()
            .map(|command| command.command)
            .filter(|command| command != "/help")
            .collect()
    }

    #[test]
    fn covers_every_visible_command_except_help() {
        assert_eq!(
            topic_names(&HelpTopic::COMMANDS),
            visible_except_help(Command::bot_commands())
        );
    }

    #[test]
    fn covers_every_review_command_except_help() {
        assert_eq!(
            topic_names(&HelpTopic::REVIEW),
            visible_except_help(ReviewCommand::bot_commands())
        );
    }
}
