use std::{
    collections::HashSet,
    fs,
    num::NonZeroU16,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::{Map, Value};
use teloxide::types::{Recipient, UserId};
use url::Url;

use crate::{model::PositiveDuration, schedule::ScheduleDefaults};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub token: SecretString,
    pub channel: Recipient,
    pub admins: Vec<AdminConfig>,
    pub database: DatabaseConfig,
    pub submission: SubmissionConfig,
    #[serde(default)]
    pub review: ReviewConfig,
    #[serde(default)]
    pub fetch: FetchConfig,
    /// 识图多模态模型。不配置则不启用自动标签。
    pub vision: Option<VisionConfig>,
    pub schedule: ScheduleDefaults,
}

/// 抓取链接里的媒体所用的外部工具。没有安装的工具会被跳过；
/// 两个都没有时，Bot 不处理链接。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FetchConfig {
    /// `gallery-dl` 的路径或命令名，不配置则从 PATH 里找。
    pub gallery_dl: Option<PathBuf>,
    /// `yt-dlp` 的路径或命令名，不配置则从 PATH 里找。
    pub yt_dlp: Option<PathBuf>,
    /// 单个工具处理一条链接的最长时间，默认两分钟。
    pub timeout: Option<PositiveDuration>,
    /// 一条链接最多下载多少个文件，默认 30 个。
    pub max_files: Option<NonZeroU16>,
}

impl FetchConfig {
    pub fn timeout(&self) -> Duration {
        self.timeout
            .map_or(Duration::from_secs(120), PositiveDuration::get)
    }

    pub fn max_files(&self) -> u16 {
        self.max_files.map_or(30, NonZeroU16::get)
    }
}

/// 识图多模态模型，使用 OpenAI 兼容的 `/chat/completions` 接口。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisionConfig {
    /// 接口地址，例如 `https://api.openai.com/v1`。请求发往它后面加 `/chat/completions`。
    pub base_url: String,
    /// 访问密钥。本地部署的服务可能不需要。
    pub api_key: Option<SecretString>,
    pub model: String,
    /// 单次请求的最长时间，默认一分钟。
    pub timeout: Option<PositiveDuration>,
    /// 一次最多分析多少张图片，默认 10 张。
    pub max_images: Option<NonZeroU16>,
    /// 是否请求 JSON 输出模式（`response_format`）。有的服务不认这个参数，可以关掉，默认开启。
    pub json_mode: Option<bool>,
    /// 原样合并进每个请求体顶层的额外字段，用来传服务特有的参数，
    /// 例如 DeepSeek 关闭思考模式的 `thinking = { type = "disabled" }`。
    #[serde(default)]
    pub extra_body: Map<String, Value>,
}

impl VisionConfig {
    pub fn timeout(&self) -> Duration {
        self.timeout
            .map_or(Duration::from_secs(60), PositiveDuration::get)
    }

    pub fn max_images(&self) -> u16 {
        self.max_images.map_or(10, NonZeroU16::get)
    }

    pub fn json_mode(&self) -> bool {
        self.json_mode.unwrap_or(true)
    }

    fn validate(&self) -> Result<()> {
        let url = Url::parse(&self.base_url).context("vision.base_url must be a URL")?;
        ensure!(
            matches!(url.scheme(), "http" | "https") && url.has_host(),
            "vision.base_url must be an http or https URL"
        );
        ensure!(
            !self.model.trim().is_empty(),
            "vision.model must not be empty"
        );
        ensure!(
            !self.extra_body.contains_key("model") && !self.extra_body.contains_key("messages"),
            "vision.extra_body must not set model or messages"
        );
        Ok(())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewConfig {
    /// 审核群的 ID。配置了它才开放普通用户投稿。
    pub group: Option<Recipient>,
    /// 拒绝投稿时没有写理由，就把这段话发给投稿人。
    pub default_reject_note: Option<String>,
}

/// 可以提交帖子并管理自己队列的用户。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminConfig {
    pub user_id: UserId,
    /// 超级管理员可以查看并操作其他管理员的帖子。
    #[serde(default, rename = "super")]
    pub is_super: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    pub url: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubmissionConfig {
    pub media_group_wait: PositiveDuration,
}

impl Config {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let content = fs::read_to_string(path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        Self::parse(&content).with_context(|| format!("invalid configuration {}", path.display()))
    }

    pub fn parse(content: &str) -> Result<Self> {
        let config: Self = toml::from_str(content)?;
        config.validate()?;
        Ok(config)
    }

    pub fn is_admin(&self, user_id: UserId) -> bool {
        self.admins.iter().any(|admin| admin.user_id == user_id)
    }

    pub fn is_super(&self, user_id: UserId) -> bool {
        self.admins
            .iter()
            .any(|admin| admin.user_id == user_id && admin.is_super)
    }

    /// 审核群的默认拒绝理由，如果配置了的话。
    pub fn default_reject_note(&self) -> Option<&str> {
        self.review.default_reject_note.as_deref()
    }

    pub fn admin_ids(&self) -> Vec<UserId> {
        self.admins.iter().map(|admin| admin.user_id).collect()
    }

    fn validate(&self) -> Result<()> {
        ensure!(!self.admins.is_empty(), "admins must not be empty");
        let mut user_ids = HashSet::new();
        for admin in &self.admins {
            ensure!(
                admin.user_id.0 != 0 && user_ids.insert(admin.user_id),
                "admins must contain unique, non-zero user IDs"
            );
        }
        ensure!(
            !self.database.url.trim().is_empty(),
            "database.url must not be empty"
        );
        if let Recipient::ChannelUsername(username) = &self.channel {
            validate_channel_username(username)?;
        }
        if let Some(Recipient::ChannelUsername(username)) = &self.review.group {
            validate_channel_username(username)?;
        }
        if let Some(note) = &self.review.default_reject_note {
            ensure!(
                !note.trim().is_empty(),
                "review.default_reject_note must not be empty"
            );
        }
        if let Some(vision) = &self.vision {
            vision.validate()?;
        }
        Ok(())
    }
}

fn validate_channel_username(username: &str) -> Result<()> {
    let value = username
        .strip_prefix('@')
        .context("channel username must start with @")?;
    let valid_chars = value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
    ensure!(
        (5..=32).contains(&value.len()) && valid_chars,
        "channel username must contain 5-32 ASCII letters, digits, or underscores"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use teloxide::types::ChatId;

    const EXAMPLE: &str = include_str!("../PureWaterSpiritBot.example.toml");

    #[test]
    fn parses_example_configuration() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.admins.len(), 2);
        assert!(config.is_admin(UserId(123456789)));
        assert!(config.is_super(UserId(123456789)));
        assert!(config.is_admin(UserId(987654321)));
        assert!(!config.is_super(UserId(987654321)));
        assert!(!config.is_admin(UserId(1)));
    }

    #[test]
    fn rejects_empty_and_duplicate_admins() {
        let no_admins = Config::parse(&EXAMPLE.replace("[[admins]]", "[[unused]]"));
        assert!(no_admins.is_err());
        let duplicate = format!("{EXAMPLE}\n[[admins]]\nuser_id = 123456789\n");
        assert!(Config::parse(&duplicate).is_err());
    }

    #[test]
    fn rejects_non_positive_duration() {
        let source = EXAMPLE.replace("send_interval = \"3s\"", "send_interval = \"0s\"");
        assert!(Config::parse(&source).is_err());
    }

    #[test]
    fn rejects_unknown_field() {
        let source = format!("unexpected = 1\n{EXAMPLE}");
        assert!(Config::parse(&source).is_err());
    }

    #[test]
    fn the_review_group_is_optional() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert!(config.review.group.is_none());
        assert!(config.default_reject_note().is_none());

        let source = format!(
            "{}\n[review]\ngroup = -1002222222222\ndefault_reject_note = \"不符合频道的投稿要求\"",
            EXAMPLE
        );
        let config = Config::parse(&source).unwrap();
        assert_eq!(
            config.review.group,
            Some(Recipient::Id(ChatId(-1002222222222)))
        );
        assert_eq!(config.default_reject_note(), Some("不符合频道的投稿要求"));
    }

    #[test]
    fn the_vision_section_is_optional() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert!(config.vision.is_none());

        let source = format!(
            "{EXAMPLE}\n[vision]\nbase_url = \"https://api.example.com/v1\"\nmodel = \"m\"\n"
        );
        let vision = Config::parse(&source).unwrap().vision.unwrap();
        assert!(vision.api_key.is_none());
        assert_eq!(vision.timeout(), Duration::from_secs(60));
        assert_eq!(vision.max_images(), 10);
        assert!(vision.json_mode());
        assert!(vision.extra_body.is_empty());

        let source = format!(
            "{EXAMPLE}\n[vision]\nbase_url = \"http://localhost:11434/v1\"\nmodel = \"m\"\n\
             api_key = \"k\"\ntimeout = \"20s\"\nmax_images = 4\njson_mode = false\n\
             [vision.extra_body]\nthinking = {{ type = \"disabled\" }}\n"
        );
        let vision = Config::parse(&source).unwrap().vision.unwrap();
        assert_eq!(vision.timeout(), Duration::from_secs(20));
        assert_eq!(vision.max_images(), 4);
        assert!(!vision.json_mode());
        assert_eq!(
            vision.extra_body["thinking"],
            serde_json::json!({ "type": "disabled" })
        );
    }

    #[test]
    fn rejects_an_invalid_vision_section() {
        for section in [
            "base_url = \"not a url\"\nmodel = \"m\"",
            "base_url = \"ftp://example.com\"\nmodel = \"m\"",
            "base_url = \"https://example.com/v1\"\nmodel = \"  \"",
            "base_url = \"https://example.com/v1\"\nmodel = \"m\"\ntimeout = \"0s\"",
            "base_url = \"https://example.com/v1\"\nmodel = \"m\"\n[vision.extra_body]\nmodel = \"x\"",
        ] {
            let source = format!("{EXAMPLE}\n[vision]\n{section}\n");
            assert!(Config::parse(&source).is_err(), "{section}");
        }
    }

    #[test]
    fn rejects_a_blank_default_reject_note() {
        let source = format!("{EXAMPLE}\n[review]\ndefault_reject_note = \"  \"\n");
        assert!(Config::parse(&source).is_err());
    }

    #[test]
    fn the_fetch_section_is_optional() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.fetch.timeout(), Duration::from_secs(120));
        assert_eq!(config.fetch.max_files(), 30);
        assert!(config.fetch.gallery_dl.is_none());

        let source = format!(
            "{EXAMPLE}\n[fetch]\ngallery_dl = \"/usr/bin/gallery-dl\"\ntimeout = \"30s\"\nmax_files = 12\n"
        );
        let config = Config::parse(&source).unwrap();
        assert_eq!(config.fetch.timeout(), Duration::from_secs(30));
        assert_eq!(config.fetch.max_files(), 12);
        assert_eq!(
            config.fetch.gallery_dl.as_deref(),
            Some(Path::new("/usr/bin/gallery-dl"))
        );
    }

    #[test]
    fn rejects_zero_max_files() {
        let source = format!("{EXAMPLE}\n[fetch]\nmax_files = 0\n");
        assert!(Config::parse(&source).is_err());
    }

    #[test]
    fn rejects_the_old_per_bot_categories() {
        let source = format!("{EXAMPLE}\n[[categories]]\nkey = \"gi\"\nlabel = \"GI\"\n");
        assert!(Config::parse(&source).is_err());
    }
}
