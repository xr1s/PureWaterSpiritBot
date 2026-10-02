//! 调用识图多模态模型：给图片打标签。
//!
//! 只依赖 OpenAI 兼容的 `/chat/completions` 接口，和 Bot、数据库无关：
//! 调用方负责下载图片、提供词表，并决定如何使用结果。

mod image;
mod vocabulary;

use futures::future::join_all;
use reqwest::{Client, header::CONTENT_TYPE};
use secrecy::ExposeSecret;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::json;

pub use self::{
    image::{Image, MAX_IMAGE_BYTES},
    vocabulary::{Vocabulary, VocabularyGroup, hashtag},
};
use crate::config::VisionConfig;

/// 同时发给模型的请求数。再高容易撞上服务的限流。
const CONCURRENT_REQUESTS: usize = 3;

/// 模型回答的 token 上限。回答只是一个短 JSON，这个上限防止模型跑偏时浪费费用。
const MAX_OUTPUT_TOKENS: u32 = 1024;

/// 错误信息里保留的响应正文长度。
const BODY_SNIPPET_CHARS: usize = 300;

#[derive(Debug, thiserror::Error)]
pub enum VisionError {
    #[error("the tag vocabulary is empty")]
    EmptyVocabulary,
    #[error("there are no images to analyse")]
    NoImages,
    #[error("at most {max} images can be analysed at once")]
    TooManyImages { max: usize },
    #[error("unsupported image format (expected JPEG, PNG, WebP or GIF)")]
    UnsupportedImage,
    #[error("the image is larger than {} MiB", MAX_IMAGE_BYTES / 1024 / 1024)]
    ImageTooLarge,
    #[error("the model request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("the model service answered HTTP {status}: {body}")]
    Status { status: u16, body: String },
    #[error("the model returned an empty answer")]
    EmptyAnswer,
    #[error("unexpected model response: {0}")]
    BadResponse(String),
}

#[derive(Debug)]
pub struct Vision {
    client: Client,
    endpoint: String,
    api_key: Option<secrecy::SecretString>,
    model: String,
    json_mode: bool,
    extra_body: serde_json::Map<String, serde_json::Value>,
    max_images: usize,
}

impl Vision {
    /// 配置需要已经通过 [`VisionConfig`] 的校验。
    pub fn new(config: &VisionConfig) -> Self {
        let client = Client::builder()
            .timeout(config.timeout())
            .build()
            .expect("HTTP client configuration is valid");
        Self {
            client,
            endpoint: format!(
                "{}/chat/completions",
                config.base_url.trim().trim_end_matches('/')
            ),
            api_key: config.api_key.clone(),
            model: config.model.trim().to_owned(),
            json_mode: config.json_mode(),
            extra_body: config.extra_body.clone(),
            max_images: usize::from(config.max_images()),
        }
    }

    /// 一次最多能分析几张图片。
    pub fn max_images(&self) -> usize {
        self.max_images
    }

    /// 逐张识别图片，返回所有图片里出现的词表标签：去重，按词表里的顺序排列。
    /// 任何一张失败整体就失败，避免悄悄漏掉标签。
    pub async fn tag(
        &self,
        images: &[Image],
        vocabulary: &Vocabulary,
    ) -> Result<Vec<String>, VisionError> {
        if vocabulary.is_empty() {
            return Err(VisionError::EmptyVocabulary);
        }
        self.check_count(images)?;
        let system = tag_prompt(vocabulary);
        let answers = self
            .ask_all(
                images,
                &system,
                "List the vocabulary names that appear in this image.",
            )
            .await?
            .iter()
            .map(|json| parse::<TagAnswer>(json))
            .collect::<Result<Vec<_>, _>>()?;
        let resolved = vocabulary.resolve(
            answers
                .iter()
                .flat_map(|answer| &answer.tags)
                .map(String::as_str),
        );
        tracing::debug!("Model tags {} image(s): {resolved:?}", images.len());
        Ok(resolved)
    }

    fn check_count(&self, images: &[Image]) -> Result<(), VisionError> {
        if images.is_empty() {
            return Err(VisionError::NoImages);
        }
        if images.len() > self.max_images {
            return Err(VisionError::TooManyImages {
                max: self.max_images,
            });
        }
        Ok(())
    }

    /// 并发地逐张提问，结果保持图片的顺序，任何一张失败就整体失败。
    /// 返回每张图片的回答里的 JSON 对象文本。
    async fn ask_all(
        &self,
        images: &[Image],
        system: &str,
        instruction: &str,
    ) -> Result<Vec<String>, VisionError> {
        let mut answers = Vec::with_capacity(images.len());
        for batch in images.chunks(CONCURRENT_REQUESTS) {
            let mut pending = Vec::with_capacity(batch.len());
            for image in batch {
                pending.push(self.ask(system, instruction, image));
            }
            for answer in join_all(pending).await {
                answers.push(answer?);
            }
        }
        Ok(answers)
    }

    /// 提一次问，返回回答里的 JSON 对象文本。
    async fn ask(
        &self,
        system: &str,
        instruction: &str,
        image: &Image,
    ) -> Result<String, VisionError> {
        // 有的服务（如 DeepSeek 的 JSON 模式）偶尔会返回空内容，重试一次通常就好。
        let content = match self.complete(system, instruction, image).await {
            Err(VisionError::EmptyAnswer) => {
                tracing::warn!("The model returned an empty answer; retrying once");
                self.complete(system, instruction, image).await?
            }
            other => other?,
        };
        tracing::debug!("Model answered: {content}");
        json_object(&content)
            .map(str::to_owned)
            .ok_or_else(|| VisionError::BadResponse(snippet(content.as_bytes())))
    }

    /// 发一次请求，返回模型回答的文本。
    async fn complete(
        &self,
        system: &str,
        instruction: &str,
        image: &Image,
    ) -> Result<String, VisionError> {
        let mut body = json!({
            "model": self.model,
            "max_tokens": MAX_OUTPUT_TOKENS,
            "messages": [
                { "role": "system", "content": system },
                {
                    "role": "user",
                    "content": [
                        { "type": "text", "text": instruction },
                        { "type": "image_url", "image_url": { "url": image.data_url() } },
                    ],
                },
            ],
        });
        if self.json_mode {
            body["response_format"] = json!({ "type": "json_object" });
        }
        for (key, value) in &self.extra_body {
            body[key] = value.clone();
        }
        let mut request = self
            .client
            .post(&self.endpoint)
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body).expect("a JSON value always serializes"));
        if let Some(key) = &self.api_key {
            request = request.bearer_auth(key.expose_secret());
        }

        let response = request.send().await?;
        let status = response.status();
        let bytes = response.bytes().await?;
        if !status.is_success() {
            return Err(VisionError::Status {
                status: status.as_u16(),
                body: snippet(&bytes),
            });
        }
        let completion: Completion = serde_json::from_slice(&bytes)
            .map_err(|error| VisionError::BadResponse(format!("{error}: {}", snippet(&bytes))))?;
        let choice =
            completion.choices.into_iter().next().ok_or_else(|| {
                VisionError::BadResponse(format!("no choices: {}", snippet(&bytes)))
            })?;
        choice
            .message
            .content
            .filter(|content| !content.trim().is_empty())
            .ok_or(VisionError::EmptyAnswer)
    }
}

#[derive(Deserialize)]
struct Completion {
    choices: Vec<Choice>,
}

#[derive(Deserialize)]
struct Choice {
    message: ChoiceMessage,
}

#[derive(Deserialize)]
struct ChoiceMessage {
    content: Option<String>,
}

#[derive(Deserialize)]
struct TagAnswer {
    #[serde(default)]
    tags: Vec<String>,
}

fn tag_prompt(vocabulary: &Vocabulary) -> String {
    format!(
        "You label images posted to a Telegram channel. Identify which characters or works \
from the vocabulary below appear in the image.\n\
\n\
Rules:\n\
- Use only names that appear in the vocabulary, spelled exactly as written. Never invent names.\n\
- Include a name only when you are confident. Judge by appearance and by any text or logos in the image.\n\
- Some names are whole series or franchises; include one when the image clearly belongs to it.\n\
- If nothing matches, return an empty list.\n\
\n\
Reply with JSON only: {{\"tags\": [\"name\", ...]}}\n\
\n\
Vocabulary, one group per line in the form [group] name, name, ...:\n\
{}",
        vocabulary.listing()
    )
}

fn parse<T: DeserializeOwned>(json: &str) -> Result<T, VisionError> {
    serde_json::from_str(json)
        .map_err(|error| VisionError::BadResponse(format!("{error}: {}", snippet(json.as_bytes()))))
}

/// 取出回答里的 JSON 对象。有的模型即使被要求只输出 JSON，也会包上代码块或加一句说明。
fn json_object(content: &str) -> Option<&str> {
    let start = content.find('{')?;
    let end = content.rfind('}')?;
    (start < end).then(|| &content[start..=end])
}

fn snippet(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .take(BODY_SNIPPET_CHARS)
        .collect()
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{body_string_contains, header, method, path},
    };

    use super::{
        image::fixtures::{jpeg, png},
        vocabulary::fixtures::group,
        *,
    };

    fn config(server: &MockServer) -> VisionConfig {
        VisionConfig {
            base_url: format!("{}/v1/", server.uri()),
            api_key: Some("secret".to_owned().into()),
            model: "test-model".to_owned(),
            timeout: None,
            max_images: None,
            json_mode: None,
            extra_body: Default::default(),
        }
    }

    fn vision(server: &MockServer) -> Vision {
        Vision::new(&config(server))
    }

    fn vocabulary() -> Vocabulary {
        Vocabulary::new(vec![
            group("原神", &["钟离", "甘雨", "刻晴"]),
            group("崩铁", &["流萤"]),
        ])
    }

    fn completion(content: &str) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{ "message": { "role": "assistant", "content": content } }]
        }))
    }

    fn png_image() -> Image {
        Image::new(png()).unwrap()
    }

    fn jpeg_image() -> Image {
        Image::new(jpeg()).unwrap()
    }

    async fn request_bodies(server: &MockServer) -> Vec<Value> {
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .map(|request| serde_json::from_slice(&request.body).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn tags_come_from_the_vocabulary_only() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(header("authorization", "Bearer secret"))
            .and(body_string_contains("data:image/png;base64,"))
            .respond_with(completion(
                r##"{"tags": ["甘雨", "钟离", "不存在", "#刻晴"]}"##,
            ))
            .expect(1)
            .mount(&server)
            .await;

        let tags = vision(&server)
            .tag(&[png_image()], &vocabulary())
            .await
            .unwrap();
        assert_eq!(tags, ["钟离", "甘雨", "刻晴"]);

        let body = &request_bodies(&server).await[0];
        assert_eq!(body["model"], "test-model");
        assert_eq!(body["response_format"]["type"], "json_object");
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.contains("[原神] 钟离, 甘雨, 刻晴\n[崩铁] 流萤"));
    }

    #[tokio::test]
    async fn tags_of_all_images_are_combined() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("data:image/png"))
            .respond_with(completion(r#"{"tags": ["流萤", "钟离"]}"#))
            .mount(&server)
            .await;
        Mock::given(body_string_contains("data:image/jpeg"))
            .respond_with(completion(r#"{"tags": ["钟离", "甘雨"]}"#))
            .mount(&server)
            .await;

        let tags = vision(&server)
            .tag(&[png_image(), jpeg_image()], &vocabulary())
            .await
            .unwrap();
        assert_eq!(tags, ["钟离", "甘雨", "流萤"]);
    }

    #[tokio::test]
    async fn tolerates_code_fences_and_missing_tags() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("data:image/png"))
            .respond_with(completion("```json\n{\"tags\": [\"刻晴\"]}\n```"))
            .mount(&server)
            .await;
        Mock::given(body_string_contains("data:image/jpeg"))
            .respond_with(completion("{}"))
            .mount(&server)
            .await;

        let tags = vision(&server)
            .tag(&[png_image(), jpeg_image()], &vocabulary())
            .await
            .unwrap();
        assert_eq!(tags, ["刻晴"]);
    }

    #[tokio::test]
    async fn json_mode_can_be_turned_off() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(completion(r#"{"tags": []}"#))
            .mount(&server)
            .await;
        let mut config = config(&server);
        config.json_mode = Some(false);

        Vision::new(&config)
            .tag(&[png_image()], &vocabulary())
            .await
            .unwrap();
        assert!(
            request_bodies(&server).await[0]
                .get("response_format")
                .is_none()
        );
    }

    #[tokio::test]
    async fn extra_body_fields_are_merged_into_the_request() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(completion(r#"{"tags": []}"#))
            .mount(&server)
            .await;
        let mut config = config(&server);
        config.extra_body = json!({ "thinking": { "type": "disabled" }, "max_tokens": 64 })
            .as_object()
            .unwrap()
            .clone();

        Vision::new(&config)
            .tag(&[png_image()], &vocabulary())
            .await
            .unwrap();
        let body = &request_bodies(&server).await[0];
        assert_eq!(body["thinking"], json!({ "type": "disabled" }));
        assert_eq!(body["max_tokens"], 64);
        assert_eq!(body["model"], "test-model");
    }

    #[tokio::test]
    async fn an_empty_answer_is_retried_once() {
        let empty = || completion("");
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(empty())
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .respond_with(completion(r#"{"tags": ["钟离"]}"#))
            .mount(&server)
            .await;

        let tags = vision(&server)
            .tag(&[png_image()], &vocabulary())
            .await
            .unwrap();
        assert_eq!(tags, ["钟离"]);
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn an_answer_that_stays_empty_is_reported() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "choices": [{ "message": { "content": null } }] })),
            )
            .mount(&server)
            .await;

        let result = vision(&server).tag(&[png_image()], &vocabulary()).await;
        assert!(
            matches!(result, Err(VisionError::EmptyAnswer)),
            "{result:?}"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn no_api_key_means_no_authorization_header() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(completion(r#"{"tags": []}"#))
            .mount(&server)
            .await;
        let mut config = config(&server);
        config.api_key = None;

        Vision::new(&config)
            .tag(&[png_image()], &vocabulary())
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        assert!(!requests[0].headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn service_errors_carry_the_status_and_body() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).set_body_string("slow down"))
            .mount(&server)
            .await;

        let error = vision(&server)
            .tag(&[png_image()], &vocabulary())
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            VisionError::Status { status: 429, ref body } if body == "slow down"
        ));
    }

    #[tokio::test]
    async fn one_failed_image_fails_the_whole_request() {
        let server = MockServer::start().await;
        Mock::given(body_string_contains("data:image/png"))
            .respond_with(completion(r#"{"tags": ["钟离"]}"#))
            .mount(&server)
            .await;
        Mock::given(body_string_contains("data:image/jpeg"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;

        let result = vision(&server)
            .tag(&[png_image(), jpeg_image()], &vocabulary())
            .await;
        assert!(matches!(
            result,
            Err(VisionError::Status { status: 500, .. })
        ));
    }

    #[tokio::test]
    async fn unusable_answers_are_reported() {
        for reply in [
            completion("I cannot help with that"),
            completion(r#"{"tags": "钟离"}"#),
            ResponseTemplate::new(200).set_body_string("<html>gateway</html>"),
            ResponseTemplate::new(200).set_body_json(json!({ "choices": [] })),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(reply)
                .mount(&server)
                .await;
            let result = vision(&server).tag(&[png_image()], &vocabulary()).await;
            assert!(
                matches!(result, Err(VisionError::BadResponse(_))),
                "{result:?}"
            );
        }
    }

    #[tokio::test]
    async fn refuses_bad_input_before_calling_the_service() {
        let server = MockServer::start().await;
        let vision = vision(&server);
        let empty = Vocabulary::new(vec![group("空", &[])]);

        assert!(matches!(
            vision.tag(&[png_image()], &empty).await,
            Err(VisionError::EmptyVocabulary)
        ));
        assert!(matches!(
            vision.tag(&[], &vocabulary()).await,
            Err(VisionError::NoImages)
        ));
        let too_many = vec![png_image(); vision.max_images() + 1];
        assert!(matches!(
            vision.tag(&too_many, &vocabulary()).await,
            Err(VisionError::TooManyImages { max: 10 })
        ));
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[test]
    fn extracts_the_json_object_from_chatty_answers() {
        assert_eq!(json_object("Sure! {\"a\": 1} Done."), Some("{\"a\": 1}"));
        assert_eq!(json_object("no braces"), None);
        assert_eq!(json_object("} {"), None);
    }
}
