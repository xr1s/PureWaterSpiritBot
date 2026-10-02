//! 需要连着 mock Telegram 的测试所共用的夹具。

use std::{
    sync::atomic::{AtomicI32, Ordering},
    time::Duration,
};

use chrono::{DateTime, Utc};
use serde_json::{Value, json};
use teloxide::{
    prelude::*,
    types::{
        AcceptedGiftTypes, CallbackQueryId, Chat, ChatFullInfo, ChatFullInfoKind,
        ChatFullInfoPublic, ChatFullInfoPublicChannel, ChatFullInfoPublicKind, ChatKind,
        ChatPrivate, MaybeInaccessibleMessage, MediaKind, MediaText, MessageCommon, MessageId,
        MessageKind, User,
    },
};
use wiremock::{
    Mock, MockServer, Request, Respond, ResponseTemplate,
    matchers::{method, path_regex},
};

use crate::{
    app::App, bot::Inputs, collector::Collector, config::Config, fetch::Fetcher, store::Store,
};

const EXAMPLE: &str = include_str!("../../PureWaterSpiritBot.example.toml");

/// 管理员（测试数据库里的管理员 42）的私聊。
pub const CHAT: i64 = 42;

pub struct Fixture {
    _directory: tempfile::TempDir,
    pub server: MockServer,
    pub app: App,
}

pub fn channel() -> ChatFullInfo {
    ChatFullInfo {
        id: ChatId(-100123),
        kind: ChatFullInfoKind::Public(Box::new(ChatFullInfoPublic {
            title: None,
            kind: ChatFullInfoPublicKind::Channel(ChatFullInfoPublicChannel {
                username: None,
                linked_chat_id: None,
                can_send_paid_media: false,
            }),
            description: None,
            invite_link: None,
            has_protected_content: false,
            available_reactions: None,
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

pub async fn fixture(fetcher: Fetcher) -> Fixture {
    let server = MockServer::start().await;
    let (directory, store) = Store::open_temporary().await;
    let (collector, _batches) = Collector::new(Duration::from_secs(1));
    let app = App {
        bot: Bot::new("test-token").set_api_url(format!("{}/", server.uri()).parse().unwrap()),
        channel: channel(),
        review: None,
        config: Config::parse(&EXAMPLE.replace("123456789", "42")).unwrap(),
        store,
        collector,
        fetcher,
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

fn chat() -> Chat {
    Chat {
        id: ChatId(CHAT),
        kind: ChatKind::Private(ChatPrivate {
            username: None,
            first_name: Some(String::from("Alice")),
            last_name: None,
        }),
    }
}

/// 管理员在私聊里发的一条文字消息。
pub fn admin_message(id: i32, text: &str) -> Message {
    Message {
        id: MessageId(id),
        thread_id: None,
        from: Some(User {
            id: UserId(CHAT as _),
            is_bot: false,
            first_name: String::from("Alice"),
            last_name: None,
            username: None,
            language_code: None,
            is_premium: false,
            added_to_attachment_menu: false,
        }),
        sender_chat: None,
        date: DateTime::<Utc>::from_timestamp(1, 0).unwrap(),
        chat: chat(),
        is_topic_message: false,
        via_bot: None,
        sender_business_bot: None,
        kind: MessageKind::Common(MessageCommon {
            author_signature: None,
            paid_star_count: None,
            effect_id: None,
            forward_origin: None,
            reply_to_message: None,
            external_reply: None,
            quote: None,
            reply_to_story: None,
            sender_boost_count: None,
            edit_date: None,
            media_kind: MediaKind::Text(MediaText {
                text: text.to_string(),
                entities: Vec::new(),
                link_preview_options: None,
            }),
            reply_markup: None,
            is_automatic_forward: false,
            has_protected_content: false,
            is_from_offline: false,
            business_connection_id: None,
        }),
    }
}

/// 带格式的文字消息，`entities` 是 Telegram 的 `MessageEntity` 数组。
pub fn admin_message_with_entities(id: i32, text: &str, entities: Value) -> Message {
    serde_json::from_value(json!({
        "message_id": id,
        "date": 1,
        "chat": chat(),
        "from": { "id": CHAT, "is_bot": false, "first_name": "Alice" },
        "text": text,
        "entities": entities,
    }))
    .unwrap()
}

/// 管理员点击私聊里（消息 7 上）的按钮。
pub fn admin_callback(data: &str) -> CallbackQuery {
    CallbackQuery {
        id: CallbackQueryId(String::from("query-1")),
        from: User {
            id: UserId(CHAT as _),
            is_bot: false,
            first_name: String::from("Alice"),
            last_name: None,
            username: None,
            language_code: None,
            is_premium: false,
            added_to_attachment_menu: false,
        },
        message: Some(MaybeInaccessibleMessage::Regular(Box::new(Message {
            id: MessageId(7),
            thread_id: None,
            from: None,
            sender_chat: None,
            date: DateTime::<Utc>::from_timestamp(1, 0).unwrap(),
            chat: chat(),
            is_topic_message: false,
            via_bot: None,
            sender_business_bot: None,
            kind: MessageKind::Empty {},
        }))),
        inline_message_id: None,
        chat_instance: String::from("instance"),
        data: Some(data.to_string()),
        game_short_name: None,
    }
}

/// Bot 在管理员私聊里发出的一条文字消息。
pub fn sent_text(id: i32) -> Value {
    json!({ "message_id": id, "date": 1, "chat": chat(), "text": "sent" })
}

/// 每次调用返回一条新的文字消息，消息 ID 依次递增。
pub struct Sequence(AtomicI32);

impl Sequence {
    pub fn starting_at(id: i32) -> Self {
        Self(AtomicI32::new(id))
    }
}

impl Respond for Sequence {
    fn respond(&self, _: &Request) -> ResponseTemplate {
        let id = self.0.fetch_add(1, Ordering::SeqCst);
        ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": sent_text(id) }))
    }
}

/// 让 Telegram 的某个方法成功返回 `result`。`calls` 是期望被调用的次数。
pub async fn mock_method(server: &MockServer, name: &str, result: Value, calls: Option<u64>) {
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

/// 让 Telegram 的某个方法用 `Sequence` 应答。
pub async fn mock_sequence(server: &MockServer, name: &str, first_id: i32, calls: Option<u64>) {
    let mock = Mock::given(method("POST"))
        .and(path_regex(format!("(?i).*/{name}")))
        .respond_with(Sequence::starting_at(first_id));
    match calls {
        Some(calls) => mock.expect(calls).mount(server).await,
        None => mock.mount(server).await,
    }
}

/// Telegram 对某个方法返回 400 错误。
pub async fn mock_failure(server: &MockServer, name: &str, description: &str) {
    Mock::given(method("POST"))
        .and(path_regex(format!("(?i).*/{name}")))
        .respond_with(ResponseTemplate::new(400).set_body_json(json!({
            "ok": false,
            "error_code": 400,
            "description": description,
        })))
        .mount(server)
        .await;
}

/// 某个方法收到过的请求正文。
pub async fn bodies_of(server: &MockServer, name: &str) -> Vec<String> {
    let name = name.to_ascii_lowercase();
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|request| request.url.path().to_ascii_lowercase().ends_with(&name))
        .map(|request| String::from_utf8_lossy(&request.body).into_owned())
        .collect()
}
