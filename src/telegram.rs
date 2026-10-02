use anyhow::{Context, Result as AnyhowResult};
use teloxide::{
    ApiError, RequestError,
    prelude::*,
    types::{
        ChatFullInfo, ChatMemberKind, FileId, InputFile, InputMedia, InputMediaDocument,
        InputMediaPhoto, InputMediaVideo, Me, MessageEntity, MessageId, MessageKind, MessageOrigin,
        Recipient, ReplyParameters,
    },
};
use tokio::time::sleep;

use crate::{
    fetch::MediaFile,
    model::{Content, IncomingMessage, Media, MediaKind, Post, SentMessage},
    store::AttemptOutcome,
};

/// 把私聊消息转成提交内容。对无法重新发布的消息类型
/// （贴纸、投票等）返回 `None`。
pub fn incoming_message(message: &Message, submitter: UserId) -> Option<IncomingMessage> {
    let text = message.text().or_else(|| message.caption());
    let media = media_of(message);
    if text.is_none() && media.is_none() {
        return None;
    }
    let entities = message
        .entities()
        .or_else(|| message.caption_entities())
        .unwrap_or_default()
        .to_vec();
    Some(IncomingMessage {
        submitter,
        source_chat_id: message.chat.id,
        source_message_id: message.id,
        media_group_id: message.media_group_id().map(|id| id.0.clone()),
        target: None,
        channel_reply: None,
        content: Content {
            text: text.map(ToOwned::to_owned),
            entities,
            link_preview_options: message.link_preview_options().cloned(),
            show_caption_above_media: message.show_caption_above_media(),
            media,
        },
    })
}

/// 用户从其他聊天回复的频道消息（如果它属于 `channel`）。
/// 对关联讨论组中频道帖子副本的回复同样算数。
pub fn channel_reply(message: &Message, channel: ChatId) -> Option<MessageId> {
    let MessageKind::Common(common) = &message.kind else {
        return None;
    };
    let reply = common.external_reply.as_ref()?;
    if let MessageOrigin::Channel {
        chat, message_id, ..
    } = &reply.origin
        && chat.id == channel
    {
        return Some(*message_id);
    }
    let in_channel = reply.chat.as_ref().is_some_and(|chat| chat.id == channel);
    reply.message_id.filter(|_| in_channel)
}

fn media_of(message: &Message) -> Option<Media> {
    // 动图同时带有文档，因此必须最后检查文档。
    let (kind, file) = if let Some(photo) = message.photo().and_then(|sizes| sizes.last()) {
        (MediaKind::Photo, &photo.file)
    } else if let Some(video) = message.video() {
        (MediaKind::Video, &video.file)
    } else if let Some(animation) = message.animation() {
        (MediaKind::Animation, &animation.file)
    } else {
        (MediaKind::Document, &message.document()?.file)
    };
    Some(Media {
        kind,
        file_id: file.id.0.clone(),
        file_unique_id: file.unique_id.0.clone(),
        has_spoiler: message.has_media_spoiler(),
    })
}

/// 用只读请求检查机器人能否向配置的频道发帖。
pub async fn verify_channel(bot: &Bot, me: &Me, channel: &Recipient) -> AnyhowResult<ChatFullInfo> {
    let chat = bot
        .get_chat(channel.clone())
        .await
        .context("failed to query configured channel")?;
    if !chat.is_channel() {
        anyhow::bail!("configured channel is not a Telegram channel");
    }
    let member = bot
        .get_chat_member(chat.id, me.id)
        .await
        .context("failed to query teloxide::Bot channel permissions")?;
    if !member.kind.can_post_messages() {
        anyhow::bail!("bot does not have permission to post in the configured channel");
    }
    tracing::info!(
        "Channel verified: {} (bot @{} may post)",
        chat.title().unwrap_or("untitled"),
        me.username()
    );
    Ok(chat)
}

/// 用只读请求检查机器人是否在审核群里、能不能发消息。
pub async fn verify_review_group(
    bot: &Bot,
    me: &Me,
    group: &Recipient,
) -> AnyhowResult<ChatFullInfo> {
    let chat = bot
        .get_chat(group.clone())
        .await
        .context("failed to query configured review chat")?;
    if !chat.is_group() && !chat.is_supergroup() {
        anyhow::bail!("configured review chat is not a group");
    }
    let member = bot
        .get_chat_member(chat.id, me.id)
        .await
        .context("failed to query teloxide::Bot membership of the review chat")?;
    if !member.kind.is_present() {
        anyhow::bail!("bot is not a member of the review chat");
    }
    if let ChatMemberKind::Restricted(restricted) = &member.kind
        && !restricted.can_send_messages
    {
        anyhow::bail!("bot is not allowed to send messages in the review chat");
    }
    tracing::info!(
        "Review chat verified: {} (teloxide::Bot @{} is a member)",
        chat.title().unwrap_or("untitled"),
        me.username()
    );
    Ok(chat)
}

/// 把 teloxide::Bot 之前发出的预览消息的文字改成 `content` 的文字：纯文字消息改正文，
/// 带媒体的消息改说明文字。
pub async fn edit_preview(
    bot: &Bot,
    chat: ChatId,
    message: MessageId,
    content: &Content,
) -> Result<(), RequestError> {
    let text = content.text.clone().unwrap_or_default();
    if content.media.is_none() {
        let mut request = bot.edit_message_text(chat, message, text);
        if !content.entities.is_empty() {
            request = request.entities(content.entities.clone());
        }
        if let Some(options) = &content.link_preview_options {
            request = request.link_preview_options(options.clone());
        }
        request.await?;
    } else {
        let mut request = bot
            .edit_message_caption(chat, message)
            .caption(text)
            .show_caption_above_media(content.show_caption_above_media);
        if !content.entities.is_empty() {
            request = request.caption_entities(content.entities.clone());
        }
        request.await?;
    }
    Ok(())
}

/// 把下载好的文件上传到 `chat`：一个文件单独发，多个文件作为一个相册。
/// 说明文字放在第一个文件上。速率限制会被等待过去。
pub async fn upload_media(
    bot: &Bot,
    chat: ChatId,
    files: &[MediaFile],
    caption: &(String, Vec<MessageEntity>),
) -> Result<Vec<Message>, RequestError> {
    loop {
        let result = match files {
            [single] => upload_single(bot, chat, single, caption)
                .await
                .map(|message| vec![message]),
            many => {
                bot.send_media_group(chat, upload_album(many, caption))
                    .await
            }
        };
        match result {
            Err(RequestError::RetryAfter(wait)) => {
                tracing::warn!(
                    "Telegram rate limit while uploading; retrying after {:?}",
                    wait.duration()
                );
                sleep(wait.duration()).await;
            }
            other => return other,
        }
    }
}

async fn upload_single(
    bot: &Bot,
    chat: ChatId,
    file: &MediaFile,
    (text, entities): &(String, Vec<MessageEntity>),
) -> Result<Message, RequestError> {
    let input = InputFile::file(file.path.clone());
    match file.kind {
        MediaKind::Photo => {
            bot.send_photo(chat, input)
                .caption(text.clone())
                .caption_entities(entities.clone())
                .await
        }
        MediaKind::Video => {
            bot.send_video(chat, input)
                .caption(text.clone())
                .caption_entities(entities.clone())
                .await
        }
        MediaKind::Animation | MediaKind::Document => {
            bot.send_document(chat, input)
                .caption(text.clone())
                .caption_entities(entities.clone())
                .await
        }
    }
}

fn upload_album(
    files: &[MediaFile],
    (text, entities): &(String, Vec<MessageEntity>),
) -> Vec<InputMedia> {
    files
        .iter()
        .enumerate()
        .map(|(index, file)| {
            let input = InputFile::file(file.path.clone());
            let caption = (index == 0).then(|| (text.clone(), entities.clone()));
            match file.kind {
                MediaKind::Video => {
                    let mut item = InputMediaVideo::new(input);
                    if let Some((text, entities)) = caption {
                        item.caption = Some(text);
                        item.caption_entities = Some(entities);
                    }
                    InputMedia::Video(item)
                }
                _ => {
                    let mut item = InputMediaPhoto::new(input);
                    if let Some((text, entities)) = caption {
                        item.caption = Some(text);
                        item.caption_entities = Some(entities);
                    }
                    InputMedia::Photo(item)
                }
            }
        })
        .collect()
}

macro_rules! with_reply {
    ($request:expr, $reply:expr) => {{
        let mut request = $request;
        if let Some(reply) = $reply {
            request = request.reply_parameters(reply.clone());
        }
        request
    }};
}

#[derive(Debug, PartialEq, Eq)]
pub enum PublishFailure {
    /// 完全无法向该频道发帖（频道不存在、被踢出、没有权限）。
    ChannelUnavailable(String),
    /// Telegram 拒绝了这次发帖；重试也没有用。
    Rejected(String),
    /// 请求可能已生效，因此不得自动重发该帖子。
    Unknown(String),
}

impl From<Result<Vec<SentMessage>, PublishFailure>> for AttemptOutcome {
    fn from(result: Result<Vec<SentMessage>, PublishFailure>) -> Self {
        match result {
            Ok(sent) => Self::Succeeded(sent),
            Err(PublishFailure::ChannelUnavailable(error)) => Self::Requeued(error),
            Err(PublishFailure::Rejected(error)) => Self::Failed(error),
            Err(PublishFailure::Unknown(error)) => Self::Unknown(error),
        }
    }
}

/// 以一次请求发布一个帖子：单条消息，或多消息帖子对应的相册。
/// 补充先前帖子的帖子会回复它；如果该消息此时已不存在，
/// Telegram 会改为以普通消息发送。
/// 速率限制会被等待过去，因此失败意味着请求已得到答复或已丢失。
pub async fn publish_post(
    bot: &Bot,
    channel: &Recipient,
    post: &Post,
) -> Result<Vec<SentMessage>, PublishFailure> {
    let request = match post.messages.as_slice() {
        [] => return Err(PublishFailure::Rejected("post has no messages".to_owned())),
        [single] => Request::Single(single),
        many => Request::Album(album(many)?),
    };
    let reply = post
        .reply_to
        .map(|target| ReplyParameters::new(target).allow_sending_without_reply());
    loop {
        let result = match &request {
            Request::Single(content) => send_single(bot, channel.clone(), content, reply.as_ref())
                .await
                .map(|message| vec![message]),
            Request::Album(media) => {
                with_reply!(
                    bot.send_media_group(channel.clone(), media.clone()),
                    reply.as_ref()
                )
                .await
            }
        };
        match result {
            Ok(messages) => {
                return Ok(messages
                    .iter()
                    .map(|message| SentMessage {
                        chat_id: message.chat.id,
                        message_id: message.id,
                    })
                    .collect());
            }
            Err(RequestError::RetryAfter(wait)) => {
                tracing::warn!(
                    "Telegram rate limit while publishing; retrying after {:?}",
                    wait.duration()
                );
                sleep(wait.duration()).await;
            }
            Err(error) => return Err(classify(error)),
        }
    }
}

enum Request<'a> {
    Single(&'a Content),
    Album(Vec<InputMedia>),
}

macro_rules! with_caption {
    ($request:expr, $content:expr) => {{
        let mut request = $request;
        if let Some(text) = &$content.text {
            request = request.caption(text.clone());
        }
        if !$content.entities.is_empty() {
            request = request.caption_entities($content.entities.clone());
        }
        request
    }};
}

async fn send_single(
    bot: &Bot,
    channel: Recipient,
    content: &Content,
    reply: Option<&ReplyParameters>,
) -> Result<Message, RequestError> {
    let Some(media) = &content.media else {
        let mut request = with_reply!(
            bot.send_message(channel, content.text.clone().unwrap_or_default()),
            reply
        );
        if !content.entities.is_empty() {
            request = request.entities(content.entities.clone());
        }
        if let Some(options) = &content.link_preview_options {
            request = request.link_preview_options(options.clone());
        }
        return request.await;
    };
    let file = InputFile::file_id(FileId(media.file_id.clone()));
    let above = content.show_caption_above_media;
    match media.kind {
        MediaKind::Photo => {
            with_reply!(with_caption!(bot.send_photo(channel, file), content), reply)
                .show_caption_above_media(above)
                .has_spoiler(media.has_spoiler)
                .await
        }
        MediaKind::Video => {
            with_reply!(with_caption!(bot.send_video(channel, file), content), reply)
                .show_caption_above_media(above)
                .has_spoiler(media.has_spoiler)
                .await
        }
        MediaKind::Animation => {
            with_reply!(
                with_caption!(bot.send_animation(channel, file), content),
                reply
            )
            .show_caption_above_media(above)
            .has_spoiler(media.has_spoiler)
            .await
        }
        MediaKind::Document => {
            with_reply!(
                with_caption!(bot.send_document(channel, file), content),
                reply
            )
            .await
        }
    }
}

/// Telegram 相册只能包含照片和视频，或者只包含文档。
fn album(messages: &[Content]) -> Result<Vec<InputMedia>, PublishFailure> {
    let kinds: Option<Vec<MediaKind>> = messages
        .iter()
        .map(|content| content.media.as_ref().map(|media| media.kind))
        .collect();
    let publishable = kinds.is_some_and(|kinds| MediaKind::can_share_album(&kinds));
    if !publishable {
        return Err(PublishFailure::Rejected(
            "post cannot be published as an album".to_owned(),
        ));
    }
    Ok(messages.iter().filter_map(album_item).collect())
}

fn album_item(content: &Content) -> Option<InputMedia> {
    let media = content.media.as_ref()?;
    let file = InputFile::file_id(FileId(media.file_id.clone()));
    let caption = content.text.clone();
    let entities = (!content.entities.is_empty()).then(|| content.entities.clone());
    let above = content.show_caption_above_media;
    match media.kind {
        MediaKind::Photo => {
            let mut item = InputMediaPhoto::new(file);
            item.caption = caption;
            item.caption_entities = entities;
            item.show_caption_above_media = above;
            item.has_spoiler = media.has_spoiler;
            Some(InputMedia::Photo(item))
        }
        MediaKind::Video => {
            let mut item = InputMediaVideo::new(file);
            item.caption = caption;
            item.caption_entities = entities;
            item.show_caption_above_media = above;
            item.has_spoiler = media.has_spoiler;
            Some(InputMedia::Video(item))
        }
        MediaKind::Document => {
            let mut item = InputMediaDocument::new(file);
            item.caption = caption;
            item.caption_entities = entities;
            Some(InputMedia::Document(item))
        }
        MediaKind::Animation => None,
    }
}

fn classify(error: RequestError) -> PublishFailure {
    let description = error.to_string();
    match error {
        RequestError::Api(ref api) if is_channel_error(api) => {
            PublishFailure::ChannelUnavailable(description)
        }
        RequestError::MigrateToChatId(_) => PublishFailure::ChannelUnavailable(description),
        RequestError::Api(ApiError::Unknown(_)) => PublishFailure::Unknown(description),
        RequestError::Api(_) => PublishFailure::Rejected(description),
        _ => PublishFailure::Unknown(description),
    }
}

fn is_channel_error(error: &ApiError) -> bool {
    matches!(
        error,
        ApiError::ChatNotFound
            | ApiError::NotEnoughRightsToPostMessages
            | ApiError::BotKicked
            | ApiError::BotKickedFromChannel
            | ApiError::InvalidToken
    )
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, method, path_regex},
    };

    use super::*;
    use crate::model::PostId;

    fn sent_message(id: i32) -> serde_json::Value {
        json!({
            "message_id": id,
            "date": 0,
            "chat": { "id": -100123, "type": "channel", "title": "test" },
            "text": "hello"
        })
    }

    fn bot(server: &MockServer) -> Bot {
        Bot::new("test-token").set_api_url(format!("{}/", server.uri()).parse().unwrap())
    }

    fn text(value: &str) -> Content {
        Content {
            text: Some(value.to_owned()),
            entities: Vec::new(),
            link_preview_options: None,
            show_caption_above_media: false,
            media: None,
        }
    }

    fn photo(file_id: &str, caption: Option<&str>) -> Content {
        Content {
            text: caption.map(ToOwned::to_owned),
            media: Some(Media {
                kind: MediaKind::Photo,
                file_id: file_id.to_owned(),
                file_unique_id: format!("unique-{file_id}"),
                has_spoiler: false,
            }),
            ..text("")
        }
    }

    fn channel() -> Recipient {
        Recipient::Id(ChatId(-100123))
    }

    #[tokio::test]
    async fn publishes_text_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "ok": true, "result": sent_message(77) })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let post = Post {
            id: PostId(1),
            messages: vec![text("hello")],
            reply_to: None,
        };
        let sent = publish_post(&bot(&server), &channel(), &post)
            .await
            .unwrap();
        assert_eq!(
            sent,
            vec![SentMessage {
                chat_id: ChatId(-100123),
                message_id: MessageId(77)
            }]
        );
    }

    #[tokio::test]
    async fn supplement_replies_to_the_earlier_channel_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmessage"))
            .and(body_string_contains("\"reply_parameters\""))
            .and(body_string_contains("\"allow_sending_without_reply\":true"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "ok": true, "result": sent_message(78) })),
            )
            .expect(1)
            .mount(&server)
            .await;
        let post = Post {
            id: PostId(2),
            messages: vec![text("more")],
            reply_to: Some(MessageId(77)),
        };
        publish_post(&bot(&server), &channel(), &post)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn publishes_multi_message_post_as_one_album_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmediagroup"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "ok": true,
                "result": [sent_message(10), sent_message(11)]
            })))
            .expect(1)
            .mount(&server)
            .await;
        let post = Post {
            id: PostId(1),
            messages: vec![photo("a", Some("caption")), photo("b", None)],
            reply_to: None,
        };
        let sent = publish_post(&bot(&server), &channel(), &post)
            .await
            .unwrap();
        assert_eq!(sent.len(), 2);
    }

    #[tokio::test]
    async fn retries_after_rate_limit() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_json(json!({
                "ok": false,
                "error_code": 429,
                "description": "Too Many Requests: retry after 0",
                "parameters": { "retry_after": 0 }
            })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "ok": true, "result": sent_message(5) })),
            )
            .with_priority(2)
            .mount(&server)
            .await;
        let post = Post {
            id: PostId(1),
            messages: vec![text("hello")],
            reply_to: None,
        };
        assert!(publish_post(&bot(&server), &channel(), &post).await.is_ok());
    }

    #[tokio::test]
    async fn classifies_missing_channel_as_unavailable() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "ok": false,
                "error_code": 400,
                "description": "Bad Request: chat not found"
            })))
            .mount(&server)
            .await;
        let post = Post {
            id: PostId(1),
            messages: vec![text("hello")],
            reply_to: None,
        };
        let failure = publish_post(&bot(&server), &channel(), &post)
            .await
            .unwrap_err();
        assert!(matches!(failure, PublishFailure::ChannelUnavailable(_)));
    }

    fn message_replying_externally(origin_chat: i64, reply_chat: i64, reply_id: i32) -> Message {
        serde_json::from_value(json!({
            "message_id": 5,
            "date": 0,
            "chat": { "id": 42, "type": "private", "first_name": "a" },
            "from": { "id": 42, "is_bot": false, "first_name": "a" },
            "text": "B",
            "external_reply": {
                "origin": {
                    "type": "channel",
                    "date": 0,
                    "chat": { "id": origin_chat, "type": "channel", "title": "channel" },
                    "message_id": 77
                },
                "chat": { "id": reply_chat, "type": "supergroup", "title": "group" },
                "message_id": reply_id,
                "photo": [{ "file_id": "a", "file_unique_id": "b", "width": 1, "height": 1 }]
            }
        }))
        .unwrap()
    }

    #[test]
    fn finds_the_channel_message_a_user_replied_to() {
        let channel = ChatId(-100123);
        let direct = message_replying_externally(-100123, -100123, 77);
        assert_eq!(channel_reply(&direct, channel), Some(MessageId(77)));

        // 对讨论组中频道帖子副本的回复。
        let copy = message_replying_externally(-100123, -100999, 9);
        assert_eq!(channel_reply(&copy, channel), Some(MessageId(77)));

        let foreign = message_replying_externally(-100555, -100555, 77);
        assert_eq!(channel_reply(&foreign, channel), None);
    }

    #[test]
    fn plain_messages_have_no_channel_reply() {
        let message: Message = serde_json::from_value(json!({
            "message_id": 5,
            "date": 0,
            "chat": { "id": 42, "type": "private", "first_name": "a" },
            "from": { "id": 42, "is_bot": false, "first_name": "a" },
            "text": "B"
        }))
        .unwrap();
        assert_eq!(channel_reply(&message, ChatId(-100123)), None);
    }

    #[test]
    fn rejects_albums_mixing_documents_and_photos() {
        let mut document = photo("b", None);
        document.media.as_mut().unwrap().kind = MediaKind::Document;
        let failure = album(&[photo("a", None), document]).unwrap_err();
        assert!(matches!(failure, PublishFailure::Rejected(_)));
    }
}
