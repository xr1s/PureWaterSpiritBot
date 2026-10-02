//! 自动标签：让 AI 识图，给投稿里的图片打标签。
//!
//! 只有管理员能用：在自己的草稿、排队中的帖子上，或者审核群里的待审核投稿上点按钮。
//! 结果写入帖子文字，想改就用「编辑图文」（审核群里回复卡片覆盖原文）。
//! 识别要几秒到几十秒，所以在后台任务里跑，不占着处理消息的循环。

use std::sync::Arc;

use anyhow::Result;
use jiff::Timestamp;
use teloxide::{
    net::Download,
    prelude::*,
    types::{CallbackQuery, CallbackQueryId, FileId},
};

use super::{post_text, review, view};
use crate::{
    app::App,
    model::{GeneratedText, MediaKind, PostId, PostStatus},
    store::AppendTextOutcome,
    vision::{Image, MAX_IMAGE_BYTES, Vision, VisionError, Vocabulary, VocabularyGroup, hashtag},
};

const NOT_CONFIGURED: &str = "还没有配置识图模型，不能使用自动标签。";
const BUSY: &str = "这篇投稿正在识别，请稍等。";
const NO_IMAGES: &str = "这篇投稿里没有可以识别的图片。";
const NO_TAGS_IN_VOCABULARY: &str = "词表是空的，没有可选的标签。";
const NOTHING_RECOGNIZED: &str = "没有识别到词表里的标签。";
const NOTHING_NEW: &str = "识别到的内容已经都在文字里了。";
const CALLBACK_TEXT_LIMIT: usize = 200;

/// 按钮在哪里被点。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Place {
    /// 管理员私聊里自己的草稿或排队中的帖子。
    Private,
    /// 审核群里待审核的投稿。
    Review,
}

/// 响应「自动标签」按钮：检查权限，然后在后台识别。
pub async fn handle(app: Arc<App>, query: CallbackQuery, post: PostId) -> Result<()> {
    if !app.ai.is_enabled() {
        return alert(&app, query.id, NOT_CONFIGURED).await;
    }
    let admin = query.from.id;
    if !app.config.is_admin(admin) {
        return alert(&app, query.id, view::NOT_ALLOWED).await;
    }
    let Some(message) = &query.message else {
        app.bot.answer_callback_query(query.id).await?;
        return Ok(());
    };
    let chat = message.chat();

    let Some(summary) = app.store.post_summary(post).await? else {
        return alert(&app, query.id, &view::post_not_found(post)).await;
    };
    let place = match summary.status {
        PostStatus::PendingReview => {
            review::is_review_group(&app, chat.id).then_some(Place::Review)
        }
        status if status.is_editable() => (summary.owner == Some(admin)).then_some(Place::Private),
        status => return alert(&app, query.id, &view::locked(post, status)).await,
    };
    let Some(place) = place else {
        return alert(&app, query.id, view::NOT_ALLOWED).await;
    };

    tokio::spawn(work(app, query.id, admin, chat.id, post, place));
    Ok(())
}

/// 后台任务：占住这篇投稿，识别，刷新原贴，然后用悬浮提示告诉管理员。
async fn work(
    app: Arc<App>,
    query: CallbackQueryId,
    admin: UserId,
    chat: ChatId,
    post: PostId,
    place: Place,
) {
    let Some(_running) = app.ai.begin(post) else {
        if let Err(error) = alert(&app, query, BUSY).await {
            tracing::warn!("Failed to answer the auto tag button: {error:#}");
        }
        return;
    };
    let notice = match run(&app, admin, chat, post, place).await {
        Ok(notice) => notice,
        Err(error) => {
            tracing::error!("Auto tagging failed for post {}: {error:#}", post.0);
            format!("投稿 #{} 的自动标签失败：{error}", post.0)
        }
    };
    if let Err(error) = app
        .bot
        .answer_callback_query(query)
        .text(callback_text(&notice))
        .await
    {
        tracing::warn!(
            "Failed to report the auto tag result of post {}: {error:#}",
            post.0
        );
    }
}

/// 识别并把结果写进帖子，返回要告诉管理员的话。
async fn run(app: &App, admin: UserId, chat: ChatId, post: PostId, place: Place) -> Result<String> {
    let vision = app
        .ai
        .vision
        .as_ref()
        .expect("the button is refused without a model");
    let images = collect_images(app, vision, post).await?;
    if images.images.is_empty() {
        return Ok(with_remarks(NO_IMAGES.to_owned(), &images));
    }

    let generated = match recognize_tags(app, vision, &images.images).await? {
        Recognized::Nothing(notice) => return Ok(with_remarks(notice.to_owned(), &images)),
        Recognized::Found(generated) => generated,
    };

    let outcome = app
        .store
        .append_generated_text(post, admin, &generated, Timestamp::now())
        .await?;
    let mut notice = match outcome {
        AppendTextOutcome::Appended { added } => {
            tracing::info!("Auto tagging added text to post {}", post.0);
            preview_refreshed(app, chat, post, place).await;
            format!("已添加标签：{}", added.trim())
        }
        AppendTextOutcome::NothingNew => NOTHING_NEW.to_owned(),
        AppendTextOutcome::TooLong { limit } => {
            format!("文字加上识别的结果会超过 {limit} 个字符的上限，放不下。")
        }
        AppendTextOutcome::NotFound => view::post_not_found(post),
        AppendTextOutcome::Locked(status) => view::locked(post, status),
    };
    notice = with_remarks(notice, &images);
    Ok(notice)
}

enum Recognized {
    /// 没有可写入的结果，附上要告诉管理员的原因。
    Nothing(&'static str),
    Found(GeneratedText),
}

async fn recognize_tags(app: &App, vision: &Vision, images: &[Image]) -> Result<Recognized> {
    let vocabulary = vocabulary(app).await?;
    if vocabulary.is_empty() {
        return Ok(Recognized::Nothing(NO_TAGS_IN_VOCABULARY));
    }
    let names = vision.tag(images, &vocabulary).await.map_err(explain)?;
    let hashtags: Vec<String> = names.iter().filter_map(|name| hashtag(name)).collect();
    if hashtags.is_empty() {
        return Ok(Recognized::Nothing(NOTHING_RECOGNIZED));
    }
    Ok(Recognized::Found(GeneratedText::Tags(hashtags)))
}

/// 词表：所有未归档的标签，按分组整理。名字不能变成合法 hashtag 的标签不给模型选，
/// 因为选了也发不出去。
async fn vocabulary(app: &App) -> Result<Vocabulary> {
    let groups = app.store.tag_groups().await?;
    let groups = groups
        .into_iter()
        .map(|group| {
            let (valid, invalid): (Vec<_>, Vec<_>) = group
                .tags
                .into_iter()
                .partition(|name| hashtag(name).is_some());
            if !invalid.is_empty() {
                tracing::warn!(
                    "Tags that cannot be hashtags were left out of the vocabulary: {}",
                    invalid.join(", ")
                );
            }
            VocabularyGroup {
                label: group.label,
                tags: valid,
            }
        })
        .collect();
    Ok(Vocabulary::new(groups))
}

fn explain(error: VisionError) -> anyhow::Error {
    anyhow::anyhow!("{error}")
}

/// 帖子里能交给模型的图片，以及没能用上的情况。
struct Collected {
    images: Vec<Image>,
    /// 下载失败、不是图片或太大而跳过的文件数。
    skipped: usize,
    /// 图片比一次能识别的多，只取了前面的。
    limit: Option<usize>,
}

async fn collect_images(app: &App, vision: &Vision, post: PostId) -> Result<Collected> {
    let Some(loaded) = app.store.load_post(post).await? else {
        return Ok(Collected {
            images: Vec::new(),
            skipped: 0,
            limit: None,
        });
    };
    // 视频和动图没有可以直接识别的画面，文档可能是图片也可能不是，下载之后再判断。
    let files: Vec<String> = loaded
        .messages
        .iter()
        .filter_map(|content| content.media.as_ref())
        .filter(|media| matches!(media.kind, MediaKind::Photo | MediaKind::Document))
        .map(|media| media.file_id.clone())
        .collect();
    let limit = (files.len() > vision.max_images()).then_some(vision.max_images());
    let mut downloads = Vec::new();
    for file_id in files.iter().take(vision.max_images()) {
        downloads.push(download(&app.bot, file_id).await);
    }

    let mut images = Vec::new();
    let mut skipped = 0;
    for downloaded in downloads {
        match downloaded.map(Image::new) {
            Ok(Ok(image)) => images.push(image),
            Ok(Err(error)) => {
                tracing::info!("Skipping a file of post {}: {error}", post.0);
                skipped += 1;
            }
            Err(error) => {
                tracing::warn!("Failed to download a file of post {}: {error:#}", post.0);
                skipped += 1;
            }
        }
    }
    Ok(Collected {
        images,
        skipped,
        limit,
    })
}

async fn download(bot: &Bot, file_id: &str) -> Result<Vec<u8>> {
    let file = bot.get_file(FileId(file_id.to_owned())).await?;
    anyhow::ensure!(
        file.size as usize <= MAX_IMAGE_BYTES,
        "the file is larger than {} MiB",
        MAX_IMAGE_BYTES / 1024 / 1024
    );
    let mut bytes = Vec::new();
    bot.download_file(&file.path, &mut bytes).await?;
    Ok(bytes)
}

/// 把帖子第一条消息上显示的文字刷新成现在的文字。
async fn preview_refreshed(app: &App, chat: ChatId, post: PostId, place: Place) {
    match place {
        Place::Private => {
            post_text::refresh_preview(app, chat, post).await;
        }
        Place::Review => review::refresh_preview(app, post).await,
    }
}

fn callback_text(notice: &str) -> String {
    notice.chars().take(CALLBACK_TEXT_LIMIT).collect()
}

/// 在结果后面补充没能识别的文件和被截掉的图片。
fn with_remarks(mut notice: String, images: &Collected) -> String {
    if images.skipped > 0 {
        notice.push_str(&format!("\n有 {} 个文件无法识别，已跳过。", images.skipped));
    }
    if let Some(limit) = images.limit {
        notice.push_str(&format!("\n图片太多，只识别了前 {limit} 张。"));
    }
    notice
}

async fn alert(app: &App, query: CallbackQueryId, text: &str) -> Result<()> {
    app.bot
        .answer_callback_query(query)
        .text(text)
        .show_alert(true)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path, path_regex},
    };

    use super::*;
    use crate::{
        app::Ai,
        bot::test_support::{
            CHAT, Fixture, admin_callback, bodies_of, fixture, mock_failure, mock_method, sent_text,
        },
        config::VisionConfig,
        fetch::Fetcher,
        store::test_support::{ADMIN, incoming, incoming_media, insert_tag},
    };

    const PNG: [u8; 11] = [0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];

    /// 连着 mock Telegram 和 mock 模型服务的夹具。模型服务收到的请求在 `llm` 里。
    async fn setup(answer: &str) -> (Fixture, MockServer) {
        let llm = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "choices": [{ "message": { "content": answer } }]
            })))
            .mount(&llm)
            .await;
        let mut fixture = fixture(Fetcher::unavailable()).await;
        let config = VisionConfig {
            base_url: format!("{}/v1", llm.uri()),
            api_key: None,
            model: "test-model".to_owned(),
            timeout: None,
            max_images: None,
            json_mode: None,
            extra_body: Default::default(),
        };
        fixture.app.ai = Ai::new(Some(&config));
        mock_method(
            &fixture.server,
            "getfile",
            json!({
                "file_id": "f",
                "file_unique_id": "u",
                "file_size": PNG.len(),
                "file_path": "photos/a.png"
            }),
            None,
        )
        .await;
        Mock::given(method("GET"))
            .and(path_regex(r"/file/bot.*photos.*a\.png"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(PNG.to_vec()))
            .mount(&fixture.server)
            .await;
        (fixture, llm)
    }

    async fn photo_draft(app: &App, caption: Option<&str>) -> PostId {
        app.store
            .create_draft(
                &[incoming_media(10, MediaKind::Photo, caption)],
                Timestamp::UNIX_EPOCH,
                None,
            )
            .await
            .unwrap()
            .unwrap()
    }

    async fn text_of(app: &App, post: PostId) -> Option<String> {
        app.store.load_post(post).await.unwrap().unwrap().messages[0]
            .text
            .clone()
    }

    async fn run_task(app: &App, post: PostId) -> String {
        run(app, ADMIN, ChatId(CHAT), post, Place::Private)
            .await
            .unwrap()
    }

    async fn llm_requests(llm: &MockServer) -> Vec<Value> {
        llm.received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn tags_from_the_vocabulary_are_added_to_the_caption() {
        let (fixture, llm) = setup(r#"{"tags": ["钟离", "不存在"]}"#).await;
        let app = &fixture.app;
        mock_method(
            &fixture.server,
            "editmessagecaption",
            sent_text(10),
            Some(1),
        )
        .await;
        insert_tag(&app.store, "原神", "钟离").await;
        insert_tag(&app.store, "崩铁", "流萤").await;
        let post = photo_draft(app, Some("cap")).await;

        let notice = run_task(app, post).await;

        assert!(notice.contains("已添加标签：#钟离"), "{notice}");
        assert_eq!(text_of(app, post).await.as_deref(), Some("cap\n#钟离"));
        let requests = llm_requests(&llm).await;
        assert_eq!(requests.len(), 1);
        let system = requests[0]["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("[原神] 钟离\n[崩铁] 流萤"), "{system}");
        // 发给模型的是图片本身。
        assert!(requests[0].to_string().contains("data:image/png;base64,"));
        let edits = bodies_of(&fixture.server, "editmessagecaption").await;
        assert!(edits[0].contains("#钟离"), "{}", edits[0]);
    }

    #[tokio::test]
    async fn a_message_the_bot_cannot_edit_still_updates_the_post() {
        let (fixture, _llm) = setup(r#"{"tags": ["钟离"]}"#).await;
        let app = &fixture.app;
        mock_failure(
            &fixture.server,
            "editmessagecaption",
            "Bad Request: message can't be edited",
        )
        .await;
        insert_tag(&app.store, "原神", "钟离").await;
        let post = photo_draft(app, Some("cap")).await;

        let notice = run_task(app, post).await;

        assert_eq!(notice, "已添加标签：#钟离");
        assert_eq!(text_of(app, post).await.as_deref(), Some("cap\n#钟离"));
    }

    #[tokio::test]
    async fn pressing_twice_does_not_duplicate_tags() {
        let (fixture, _llm) = setup(r#"{"tags": ["钟离"]}"#).await;
        let app = &fixture.app;
        mock_method(&fixture.server, "editmessagecaption", sent_text(10), None).await;
        insert_tag(&app.store, "原神", "钟离").await;
        let post = photo_draft(app, Some("cap")).await;

        run_task(app, post).await;
        let notice = run_task(app, post).await;

        assert_eq!(notice, NOTHING_NEW);
        assert_eq!(text_of(app, post).await.as_deref(), Some("cap\n#钟离"));
    }

    #[tokio::test]
    async fn names_that_cannot_be_hashtags_are_not_offered_to_the_model() {
        let (fixture, llm) = setup(r#"{"tags": []}"#).await;
        let app = &fixture.app;
        insert_tag(&app.store, "原神", "钟离").await;
        insert_tag(&app.store, "原神", "艾莉丝·某").await;
        let post = photo_draft(app, Some("cap")).await;

        let notice = run_task(app, post).await;

        assert_eq!(notice, NOTHING_RECOGNIZED);
        let requests = llm_requests(&llm).await;
        let system = requests[0]["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("[原神] 钟离"), "{system}");
        assert!(!system.contains("艾莉丝"), "{system}");
        assert_eq!(text_of(app, post).await.as_deref(), Some("cap"));
    }

    #[tokio::test]
    async fn an_empty_vocabulary_does_not_call_the_model() {
        let (fixture, llm) = setup(r#"{"tags": []}"#).await;
        let app = &fixture.app;
        let post = photo_draft(app, Some("cap")).await;

        assert_eq!(run_task(app, post).await, NO_TAGS_IN_VOCABULARY);
        assert!(llm_requests(&llm).await.is_empty());
    }

    #[tokio::test]
    async fn posts_without_usable_images_do_not_call_the_model() {
        let (fixture, llm) = setup(r#"{"tags": []}"#).await;
        let app = &fixture.app;
        insert_tag(&app.store, "原神", "钟离").await;
        let video = app
            .store
            .create_draft(
                &[incoming_media(11, MediaKind::Video, None)],
                Timestamp::UNIX_EPOCH,
                None,
            )
            .await
            .unwrap()
            .unwrap();
        let text = app
            .store
            .create_draft(&[incoming(12, "just text")], Timestamp::UNIX_EPOCH, None)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(run_task(app, video).await, NO_IMAGES);
        assert_eq!(run_task(app, text).await, NO_IMAGES);
        assert!(llm_requests(&llm).await.is_empty());
    }

    #[tokio::test]
    async fn a_document_that_is_not_an_image_is_skipped_with_a_note() {
        let (fixture, llm) = setup(r#"{"tags": []}"#).await;
        let app = &fixture.app;
        insert_tag(&app.store, "原神", "钟离").await;
        // 下载到的内容不是图片。
        fixture.server.reset().await;
        mock_method(
            &fixture.server,
            "getfile",
            json!({
                "file_id": "f", "file_unique_id": "u", "file_size": 5, "file_path": "docs/a.zip"
            }),
            None,
        )
        .await;
        Mock::given(method("GET"))
            .and(path_regex(r"/file/bot.*docs.*a\.zip"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"PK\x03\x04".to_vec()))
            .mount(&fixture.server)
            .await;
        let post = app
            .store
            .create_draft(
                &[incoming_media(13, MediaKind::Document, None)],
                Timestamp::UNIX_EPOCH,
                None,
            )
            .await
            .unwrap()
            .unwrap();

        let notice = run_task(app, post).await;

        assert!(notice.starts_with(NO_IMAGES), "{notice}");
        assert!(notice.contains("有 1 个文件无法识别"), "{notice}");
        assert!(llm_requests(&llm).await.is_empty());
    }

    #[tokio::test]
    async fn a_failing_model_is_reported_without_touching_the_post() {
        let (fixture, llm) = setup("unused").await;
        llm.reset().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(500).set_body_string("upstream down"))
            .mount(&llm)
            .await;
        let app = &fixture.app;
        insert_tag(&app.store, "原神", "钟离").await;
        let post = photo_draft(app, Some("cap")).await;

        let error = run(app, ADMIN, ChatId(CHAT), post, Place::Private)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("HTTP 500"), "{error}");
        assert_eq!(text_of(app, post).await.as_deref(), Some("cap"));
    }

    #[tokio::test]
    async fn a_pending_submission_gets_the_tags_too() {
        let (fixture, _llm) = setup(r#"{"tags": ["钟离"]}"#).await;
        let app = &fixture.app;
        insert_tag(&app.store, "原神", "钟离").await;
        let submitter = UserId(900);
        let mut message = incoming_media(14, MediaKind::Photo, Some("from a user"));
        message.submitter = submitter;
        message.source_chat_id = ChatId(900);
        let post = app
            .store
            .create_submission_draft(&[message], Timestamp::UNIX_EPOCH)
            .await
            .unwrap()
            .unwrap();
        assert!(
            app.store
                .submit_for_review(post, submitter, Timestamp::UNIX_EPOCH)
                .await
                .unwrap()
        );

        let notice = run(app, ADMIN, ChatId(-1000), post, Place::Review)
            .await
            .unwrap();

        assert!(notice.contains("已添加标签：#钟离"), "{notice}");
        assert_eq!(
            text_of(app, post).await.as_deref(),
            Some("from a user\n#钟离")
        );
    }

    #[tokio::test]
    async fn the_task_reports_to_the_admin_and_frees_the_post() {
        let (fixture, _llm) = setup(r#"{"tags": ["钟离"]}"#).await;
        mock_method(&fixture.server, "answercallbackquery", json!(true), Some(1)).await;
        mock_method(&fixture.server, "editmessagecaption", sent_text(10), None).await;
        let app = Arc::new(fixture.app);
        insert_tag(&app.store, "原神", "钟离").await;
        let post = photo_draft(&app, Some("cap")).await;

        work(
            app.clone(),
            CallbackQueryId("query-1".to_owned()),
            ADMIN,
            ChatId(CHAT),
            post,
            Place::Private,
        )
        .await;

        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains("已添加标签：#钟离"), "{}", answers[0]);
        assert!(bodies_of(&fixture.server, "sendmessage").await.is_empty());
        // 结束之后可以再处理。
        assert!(app.ai.begin(post).is_some());
    }

    #[tokio::test]
    async fn a_post_being_processed_refuses_a_second_press() {
        let (fixture, llm) = setup(r#"{"tags": []}"#).await;
        mock_method(&fixture.server, "answercallbackquery", json!(true), Some(1)).await;
        let app = Arc::new(fixture.app);
        let post = photo_draft(&app, Some("cap")).await;
        let running = app.ai.begin(post).expect("the post is free");

        work(
            app.clone(),
            CallbackQueryId("query-1".to_owned()),
            ADMIN,
            ChatId(CHAT),
            post,
            Place::Private,
        )
        .await;

        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains(BUSY), "{}", answers[0]);
        assert!(llm_requests(&llm).await.is_empty());
        drop(running);
    }

    async fn press(app: Arc<App>, post: PostId, from: Option<u64>) {
        let data = crate::bot::callback::CallbackData::AutoTag { post }.encode();
        let mut query = admin_callback(&data);
        if let Some(from) = from {
            query.from.id = UserId(from);
        }
        handle(app, query, post).await.unwrap();
    }

    #[tokio::test]
    async fn the_button_is_refused_without_a_model() {
        let fixture = fixture(Fetcher::unavailable()).await;
        mock_method(&fixture.server, "answercallbackquery", json!(true), Some(1)).await;
        let app = Arc::new(fixture.app);
        let post = photo_draft(&app, Some("cap")).await;

        press(app, post, None).await;

        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains(NOT_CONFIGURED), "{}", answers[0]);
    }

    #[tokio::test]
    async fn only_the_owner_may_use_the_button_on_a_draft() {
        let (fixture, llm) = setup(r#"{"tags": []}"#).await;
        mock_method(&fixture.server, "answercallbackquery", json!(true), Some(2)).await;
        let app = Arc::new(fixture.app);
        let post = photo_draft(&app, Some("cap")).await;

        // 不是管理员的人，和另一个管理员（example 配置里的 987654321 没有这篇草稿）。
        press(app.clone(), post, Some(555)).await;
        press(app.clone(), post, Some(987654321)).await;

        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert_eq!(answers.len(), 2);
        assert!(
            answers.iter().all(|a| a.contains(view::NOT_ALLOWED)),
            "{answers:?}"
        );
        assert!(llm_requests(&llm).await.is_empty());
    }

    #[tokio::test]
    async fn a_pending_submission_is_off_limits_outside_the_review_group() {
        let (fixture, llm) = setup(r#"{"tags": []}"#).await;
        mock_method(&fixture.server, "answercallbackquery", json!(true), Some(1)).await;
        let app = Arc::new(fixture.app);
        let submitter = UserId(900);
        let mut message = incoming(15, "from a user");
        message.submitter = submitter;
        message.source_chat_id = ChatId(900);
        let post = app
            .store
            .create_submission_draft(&[message], Timestamp::UNIX_EPOCH)
            .await
            .unwrap()
            .unwrap();
        app.store
            .submit_for_review(post, submitter, Timestamp::UNIX_EPOCH)
            .await
            .unwrap();

        // 点击发生在私聊里，不是审核群。
        press(app, post, None).await;

        let answers = bodies_of(&fixture.server, "answercallbackquery").await;
        assert!(answers[0].contains(view::NOT_ALLOWED), "{}", answers[0]);
        assert!(llm_requests(&llm).await.is_empty());
    }
}
