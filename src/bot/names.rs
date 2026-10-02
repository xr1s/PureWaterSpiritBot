//! 按用户 ID 向 Telegram 查询用户的名字。名字不存进数据库，显示时现查；
//! 查不到（例如用户从没和 Bot 说过话）就退回用户 ID。

use teloxide::{prelude::*, types::ChatFullInfo};

use crate::app::App;

async fn lookup(app: &App, user: UserId) -> Option<ChatFullInfo> {
    let chat = ChatId(i64::try_from(user.0).ok()?);
    app.bot.get_chat(chat).await.ok()
}

fn full_name(chat: &ChatFullInfo) -> Option<String> {
    let parts: Vec<&str> = [chat.first_name(), chat.last_name()]
        .into_iter()
        .flatten()
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// 用户的称呼：名字，没有名字就用 `@用户名`，都查不到就用用户 ID。
pub async fn user_name(app: &App, user: UserId) -> String {
    if let Some(chat) = lookup(app, user).await {
        if let Some(name) = full_name(&chat) {
            return name;
        }
        if let Some(username) = chat.username() {
            return format!("@{username}");
        }
    }
    user.0.to_string()
}

/// 用户的完整称呼，审核卡片上要能认出是谁：名字、用户名和 ID。
pub async fn user_label(app: &App, user: UserId) -> String {
    let Some(chat) = lookup(app, user).await else {
        return format!("用户（{}）", user.0);
    };
    let name = full_name(&chat).unwrap_or_else(|| "用户".to_owned());
    match chat.username() {
        Some(username) => format!("{name} @{username}（{}）", user.0),
        None => format!("{name}（{}）", user.0),
    }
}
