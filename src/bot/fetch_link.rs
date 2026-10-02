//! 管理员发一条只含链接的消息时，抓取链接里的媒体，每组最多 10 个文件发回给管理员。
//! 每一组都成为一篇独立的投稿草稿，带分类按钮，不需要的那组可以直接取消。

use std::sync::Arc;

use anyhow::{Result, bail};
use teloxide::{
    prelude::*,
    types::{Message, MessageEntity, UserId},
};
use url::Url;

use super::{create_draft, reply, replying_to, view};
use crate::{
    app::App,
    fetch::{self, MediaFile},
    model::{IncomingMessage, MAX_ALBUM_ITEMS},
    telegram,
};

const FAILED: &str = "抓取时出错了，请稍后再试。";

pub async fn start(app: Arc<App>, message: Message, admin: UserId, link: Url) -> Result<()> {
    let chat = message.chat.id;
    if !app.fetcher.is_available() {
        return reply(&app, chat, message.id, view::FETCH_UNAVAILABLE).await;
    }
    let status = app
        .bot
        .send_message(chat, view::FETCHING)
        .reply_parameters(replying_to(message.id))
        .await?;
    // 下载可能要几十秒，放到后台做，不能堵住这个聊天里的其他消息。
    tokio::spawn(async move {
        let text = match fetch_and_send(&app, chat, admin, link).await {
            Ok(text) => text,
            Err(error) => {
                tracing::error!("Failed to fetch a link for admin {}: {error:#}", admin.0);
                FAILED.to_owned()
            }
        };
        if let Err(error) = app.bot.edit_message_text(chat, status.id, text).await {
            tracing::warn!("Failed to update message {}: {error}", status.id.0);
        }
    });
    Ok(())
}

/// 抓取并发送，返回要显示给管理员的结果说明。
async fn fetch_and_send(app: &App, chat: ChatId, admin: UserId, link: Url) -> Result<String> {
    let fetched = match app.fetcher.fetch(link).await {
        Ok(fetched) => fetched,
        Err(error) => return Ok(view::fetch_failed(&error)),
    };
    if fetched.files.is_empty() {
        return Ok(view::fetch_nothing(&fetched.skipped));
    }
    let caption = fetch::source_caption(&fetched.source);
    let groups: Vec<&[MediaFile]> = fetched.files.chunks(MAX_ALBUM_ITEMS).collect();
    let mut failures = Vec::new();
    for (index, group) in groups.iter().enumerate() {
        let number = index + 1;
        if groups.len() > 1 {
            let header = view::fetch_group_header(number, groups.len(), group.len());
            app.bot.send_message(chat, header).await?;
        }
        if let Err(error) = send_group(app, chat, admin, group, &caption).await {
            tracing::warn!("Failed to send group {number} of a fetched link: {error:#}");
            failures.push(format!("第 {number} 组：{error}"));
        }
    }
    Ok(view::fetch_done(
        fetched.files.len(),
        groups.len(),
        &failures,
        &fetched.skipped,
    ))
}

/// 把一组文件发给管理员，再把发出去的消息当作管理员自己发来的，保存成一篇草稿。
async fn send_group(
    app: &App,
    chat: ChatId,
    admin: UserId,
    files: &[MediaFile],
    caption: &(String, Vec<MessageEntity>),
) -> Result<()> {
    let sent = telegram::upload_media(&app.bot, chat, files, caption).await?;
    let batch: Vec<IncomingMessage> = sent
        .iter()
        .filter_map(|message| telegram::incoming_message(message, admin))
        .collect();
    if batch.is_empty() {
        bail!("Telegram returned no messages");
    }
    create_draft(app, &batch, None).await
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        fs,
        os::unix::fs::PermissionsExt,
        path::{Path, PathBuf},
        sync::atomic::{AtomicI32, Ordering},
    };

    use serde_json::{Value, json};
    use teloxide::types::MessageId;
    use wiremock::{
        Mock, MockServer, Request, Respond, ResponseTemplate,
        matchers::{method, path, path_regex},
    };

    use super::*;
    use crate::{
        bot::test_support::{
            CHAT, admin_message_with_entities, bodies_of, fixture, mock_failure, mock_method,
            mock_sequence, sent_text,
        },
        fetch::Fetcher,
    };

    /// 像 gallery-dl 一样把 `count` 张图片下载到 `-D` 指定的目录，并打印它们的路径。
    fn gallery_dl(directory: &Path, count: u32) -> PathBuf {
        use std::io::Write;

        let tool = directory.join("gallery-dl");
        let mut file = fs::File::create(&tool).unwrap();
        writeln!(
            file,
            r#"#!/bin/sh
while [ $# -gt 0 ]; do
  if [ "$1" = "-D" ]; then dir="$2"; fi
  shift
done
[ -n "$dir" ] || exit 0
i=1
while [ $i -le {count} ]; do
  name=$(printf %02d $i).jpg
  printf x > "$dir/$name"
  echo "$dir/$name"
  i=$((i+1))
done"#
        )
        .unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::set_permissions(&tool, fs::Permissions::from_mode(0o755)).unwrap();
        tool
    }

    /// 应答 `sendMediaGroup`：按请求里的图片数量返回同样多条带图片的消息。
    struct AlbumResponder(AtomicI32);

    impl Respond for AlbumResponder {
        fn respond(&self, request: &Request) -> ResponseTemplate {
            let body = String::from_utf8_lossy(&request.body);
            let count = body.matches("\"type\":\"photo\"").count();
            let messages: Vec<Value> = (0..count)
                .map(|index| {
                    let id = self.0.fetch_add(1, Ordering::SeqCst);
                    let mut message = json!({
                        "message_id": id,
                        "date": 1,
                        "chat": { "id": CHAT, "type": "private", "first_name": "Alice" },
                        "media_group_id": "album",
                        "photo": [{
                            "file_id": format!("file-{id}"),
                            "file_unique_id": format!("unique-{id}"),
                            "width": 1,
                            "height": 1,
                        }],
                    });
                    if index == 0 {
                        message["caption"] = json!("source");
                    }
                    message
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "ok": true, "result": messages }))
        }
    }

    async fn website() -> (MockServer, Url) {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/gallery"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let link = Url::parse(&format!("{}/gallery?utm_source=x", server.uri())).unwrap();
        (server, link)
    }

    #[tokio::test]
    async fn a_link_becomes_one_draft_per_group_of_ten() {
        let tools = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::with_local_tool(gallery_dl(tools.path(), 12)).await;
        let fixture = fixture(fetcher).await;
        let app = &fixture.app;
        let (_website, link) = website().await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmediagroup"))
            .respond_with(AlbumResponder(AtomicI32::new(701)))
            .expect(2)
            .mount(&fixture.server)
            .await;
        // 两行组标题，两条带分类按钮的草稿控制消息。
        mock_sequence(&fixture.server, "sendmessage", 900, Some(4)).await;

        let text = fetch_and_send(app, ChatId(CHAT), UserId(CHAT.unsigned_abs()), link)
            .await
            .unwrap();

        assert!(text.contains("共抓取 12 个文件，分成 2 组"), "{text}");
        let first = app
            .store
            .find_post_by_message(ChatId(CHAT), MessageId(701))
            .await
            .unwrap()
            .expect("the first group is a draft");
        let second = app
            .store
            .find_post_by_message(ChatId(CHAT), MessageId(711))
            .await
            .unwrap()
            .expect("the second group is a draft");
        assert_ne!(first, second);
        let loaded = app.store.load_post(first).await.unwrap().unwrap();
        assert_eq!(loaded.messages.len(), 10);
        assert_eq!(
            app.store
                .load_post(second)
                .await
                .unwrap()
                .unwrap()
                .messages
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn the_caption_links_to_the_source_without_tracking_parameters() {
        let tools = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::with_local_tool(gallery_dl(tools.path(), 2)).await;
        let fixture = fixture(fetcher).await;
        let app = &fixture.app;
        let (website, link) = website().await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmediagroup"))
            .respond_with(AlbumResponder(AtomicI32::new(701)))
            .mount(&fixture.server)
            .await;
        mock_sequence(&fixture.server, "sendmessage", 900, None).await;

        fetch_and_send(app, ChatId(CHAT), UserId(CHAT.unsigned_abs()), link)
            .await
            .unwrap();

        let uploads = bodies_of(&fixture.server, "sendmediagroup").await;
        let expected = format!("{}/gallery", website.uri());
        assert!(uploads[0].contains("\"text_link\""), "{}", uploads[0]);
        assert!(uploads[0].contains(&expected), "{}", uploads[0]);
        assert!(!uploads[0].contains("utm_source"), "{}", uploads[0]);
        assert!(uploads[0].contains("source"), "{}", uploads[0]);
    }

    #[tokio::test]
    async fn a_failed_group_is_reported_and_the_others_still_go_out() {
        let tools = tempfile::tempdir().unwrap();
        let fetcher = Fetcher::with_local_tool(gallery_dl(tools.path(), 12)).await;
        let fixture = fixture(fetcher).await;
        let app = &fixture.app;
        let (_website, link) = website().await;
        // 第一组上传失败，第二组成功。
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmediagroup"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "ok": false,
                "error_code": 400,
                "description": "Bad Request: PHOTO_INVALID_DIMENSIONS",
            })))
            .up_to_n_times(1)
            .with_priority(1)
            .mount(&fixture.server)
            .await;
        Mock::given(method("POST"))
            .and(path_regex("(?i).*/sendmediagroup"))
            .respond_with(AlbumResponder(AtomicI32::new(701)))
            .with_priority(2)
            .mount(&fixture.server)
            .await;
        mock_sequence(&fixture.server, "sendmessage", 900, None).await;

        let text = fetch_and_send(app, ChatId(CHAT), UserId(CHAT.unsigned_abs()), link)
            .await
            .unwrap();

        assert!(text.contains("有 1 组没能上传"), "{text}");
        assert!(text.contains("第 1 组"), "{text}");
        assert!(text.contains("PHOTO_INVALID_DIMENSIONS"), "{text}");
        let sent = app
            .store
            .find_post_by_message(ChatId(CHAT), MessageId(701))
            .await
            .unwrap();
        assert!(sent.is_some());
    }

    fn link_message(text: &str) -> teloxide::types::Message {
        let length = text.encode_utf16().count();
        admin_message_with_entities(
            5,
            text,
            json!([{ "type": "url", "offset": 0, "length": length }]),
        )
    }

    #[tokio::test]
    async fn without_a_download_tool_the_admin_is_told() {
        let fixture = fixture(Fetcher::unavailable()).await;
        let app = Arc::new(fixture.app);
        mock_method(&fixture.server, "sendmessage", sent_text(1), Some(1)).await;

        let message = link_message("https://example.com/a");
        let link = crate::fetch::single_link(&message).unwrap();
        start(app, message, UserId(CHAT.unsigned_abs()), link)
            .await
            .unwrap();

        let sent = bodies_of(&fixture.server, "sendmessage").await;
        assert!(sent[0].contains("gallery-dl"), "{}", sent[0]);
        assert!(sent[0].contains("reply_parameters"), "{}", sent[0]);
    }

    #[tokio::test]
    async fn an_unsupported_link_reports_the_error() {
        let tools = tempfile::tempdir().unwrap();
        let failing = tools.path().join("gallery-dl");
        fs::write(
            &failing,
            "#!/bin/sh\n[ \"$1\" = \"--version\" ] && exit 0\necho 'Unsupported URL' >&2\nexit 64\n",
        )
        .unwrap();
        fs::set_permissions(&failing, fs::Permissions::from_mode(0o755)).unwrap();
        let fixture = fixture(Fetcher::with_local_tool(failing).await).await;
        let app = &fixture.app;
        let (_website, link) = website().await;
        mock_failure(&fixture.server, "sendmediagroup", "must not be called").await;

        let text = fetch_and_send(app, ChatId(CHAT), UserId(CHAT.unsigned_abs()), link)
            .await
            .unwrap();

        assert!(text.contains("抓取失败"), "{text}");
        assert!(text.contains("Unsupported URL"), "{text}");
    }
}
