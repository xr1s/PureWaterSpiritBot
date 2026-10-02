use teloxide::utils::command::{BotCommands, ParseError};

/// 机器人能识别的命令。描述会成为 Telegram 的命令菜单。
#[derive(BotCommands, Clone, Debug, PartialEq, Eq)]
#[command(rename_rule = "lowercase")]
pub enum Command {
    #[command(hide)]
    Start,
    #[command(description = "查看各分类的库存")]
    Stock,
    /// 原始参数文本；命令没有参数时为空。
    #[command(
        description = "编辑投稿：/edit <投稿编号>，或回复投稿消息",
        parse_with = raw_argument
    )]
    Edit(String),
    #[command(description = "管理我的分类")]
    Categories,
    #[command(description = "管理我的发布时段")]
    Schedule,
    #[command(description = "修改时区和提醒等设置")]
    Settings,
    #[command(description = "按发送顺序查看排队的投稿")]
    Peek,
    #[command(description = "查看命令列表和使用说明")]
    Help,
}

/// 审核群里的命令，只在审核群里注册。
#[derive(BotCommands, Clone, Debug, PartialEq, Eq)]
#[command(rename_rule = "lowercase")]
pub enum ReviewCommand {
    /// 回复审核卡片使用，后面可以写拒绝理由；不写就用默认理由。
    #[command(
        description = "拒绝投稿：回复审核卡片，后面可写理由",
        parse_with = raw_argument
    )]
    Reject(String),
    /// 回复审核卡片使用，后面可以写拉黑的原因。
    #[command(
        description = "拉黑投稿人：回复审核卡片，后面可写原因",
        parse_with = raw_argument
    )]
    Block(String),
    #[command(description = "解除拉黑：回复审核卡片")]
    Unblock,
    #[command(description = "查看待审核的投稿")]
    Pending,
    #[command(description = "查看审核命令的使用说明")]
    Help,
}

/// 原样保留参数文本。默认的 `String` 解析器会触发
/// `unreachable_code` lint，因为它的错误类型是不可构造的。
fn raw_argument(text: String) -> Result<(String,), ParseError> {
    Ok((text,))
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOT: &str = "PureBot";

    #[test]
    fn parses_commands_without_arguments() {
        assert_eq!(Command::parse("/start", BOT).ok(), Some(Command::Start));
        assert_eq!(Command::parse("/help", BOT).ok(), Some(Command::Help));
        assert_eq!(
            Command::parse("/stock@PureBot", BOT).ok(),
            Some(Command::Stock)
        );
    }

    #[test]
    fn keeps_the_edit_argument_text() {
        assert_eq!(
            Command::parse("/edit", BOT).ok(),
            Some(Command::Edit(String::new()))
        );
        assert_eq!(
            Command::parse("/edit #12", BOT).ok(),
            Some(Command::Edit("#12".to_owned()))
        );
    }

    #[test]
    fn parses_review_commands_with_optional_reasons() {
        assert_eq!(
            ReviewCommand::parse("/reject", BOT).ok(),
            Some(ReviewCommand::Reject(String::new()))
        );
        assert_eq!(
            ReviewCommand::parse("/reject@PureBot 不符合要求", BOT).ok(),
            Some(ReviewCommand::Reject("不符合要求".to_owned()))
        );
        assert_eq!(
            ReviewCommand::parse("/block spam", BOT).ok(),
            Some(ReviewCommand::Block("spam".to_owned()))
        );
        assert_eq!(
            ReviewCommand::parse("/unblock", BOT).ok(),
            Some(ReviewCommand::Unblock)
        );
        assert_eq!(
            ReviewCommand::parse("/pending@PureBot", BOT).ok(),
            Some(ReviewCommand::Pending)
        );
        assert_eq!(
            ReviewCommand::parse("/help", BOT).ok(),
            Some(ReviewCommand::Help)
        );
        assert!(ReviewCommand::parse("/stock", BOT).is_err());
        assert!(ReviewCommand::parse("/reject@OtherBot", BOT).is_err());
    }

    #[test]
    fn rejects_unknown_commands_and_other_bots() {
        assert!(Command::parse("/unknown", BOT).is_err());
        assert!(Command::parse("/stock@OtherBot", BOT).is_err());
    }

    #[test]
    fn parses_peek() {
        assert_eq!(Command::parse("/peek", BOT).ok(), Some(Command::Peek));
        assert_eq!(
            Command::parse("/peek@PureBot", BOT).ok(),
            Some(Command::Peek)
        );
    }

    #[test]
    fn hides_start_from_the_menu() {
        let names: Vec<_> = Command::bot_commands()
            .into_iter()
            .map(|command| command.command)
            .collect();
        assert_eq!(
            names,
            [
                "/stock",
                "/edit",
                "/categories",
                "/schedule",
                "/settings",
                "/peek",
                "/help"
            ]
        );
    }
}
