//! 抓取链接里的媒体：跟随跳转，去掉追踪参数，再调用外部工具下载。

mod download;
mod link;

#[cfg(all(test, unix))]
use std::path::PathBuf;

use teloxide::types::MessageEntity;
use tempfile::TempDir;
use tokio::sync::Semaphore;
use url::Url;

use self::{
    download::{DownloadError, Downloader},
    link::{ResolveError, Resolver, strip_tracking_params},
};
pub use self::{
    download::{MediaFile, SkipReason, Skipped},
    link::single_link,
};
use crate::config::FetchConfig;

/// 同时最多处理几条链接。下载会占用网络和磁盘，不必让多条链接一起跑。
const MAX_CONCURRENT_FETCHES: usize = 2;

/// 放在每组媒体的说明文字里、指向来源链接的文字。
const SOURCE_LABEL: &str = "source";

pub struct Fetcher {
    resolver: Resolver,
    downloader: Downloader,
    permits: Semaphore,
}

/// 抓取的结果。文件在临时目录里，这个值被丢弃时目录连同文件一起删除，
/// 所以要在文件发送完之后再丢弃它。
pub struct Fetched {
    /// 要写在说明文字里的来源：跳转之后的地址，去掉了追踪参数。
    pub source: Url,
    pub files: Vec<MediaFile>,
    pub skipped: Vec<Skipped>,
    _directory: TempDir,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum FetchError {
    /// 链接（或它跳转到的地址）指向本机或内网。
    #[error("the link does not point to a public web address")]
    Blocked,
    #[error("neither gallery-dl nor yt-dlp is installed")]
    NoTool,
    #[error("{0}")]
    Failed(String),
}

impl Fetcher {
    pub async fn new(config: &FetchConfig) -> Self {
        Self::with_parts(Resolver::new(), Downloader::detect(config).await)
    }

    /// 没有任何下载工具，用于不涉及抓取的测试。
    #[cfg(test)]
    pub fn unavailable() -> Self {
        Self::with_parts(Resolver::new(), Downloader::unavailable())
    }

    /// 用一个本地的下载工具脚本，并允许访问本机地址，用于端到端的测试。
    #[cfg(all(test, unix))]
    pub async fn with_local_tool(tool: PathBuf) -> Self {
        let config = FetchConfig {
            gallery_dl: Some(tool),
            yt_dlp: Some("/nonexistent/yt-dlp".into()),
            ..FetchConfig::default()
        };
        Self::with_parts(
            Resolver::allowing_private_addresses(),
            Downloader::detect(&config).await,
        )
    }

    fn with_parts(resolver: Resolver, downloader: Downloader) -> Self {
        Self {
            resolver,
            downloader,
            permits: Semaphore::new(MAX_CONCURRENT_FETCHES),
        }
    }

    pub fn is_available(&self) -> bool {
        self.downloader.is_available()
    }

    pub async fn fetch(&self, link: Url) -> Result<Fetched, FetchError> {
        let _permit = self
            .permits
            .acquire()
            .await
            .expect("the semaphore is never closed");
        let resolved = match self.resolver.resolve_redirects(link).await {
            Ok(url) => url,
            Err(ResolveError::Blocked(address)) => {
                tracing::warn!("Refusing to fetch {address}");
                return Err(FetchError::Blocked);
            }
            // 有些站点不让这一步的请求通过，但下载工具自己也许可以，所以用走到的最后一个地址继续。
            Err(ResolveError::Failed { last, reason }) => {
                tracing::warn!("Could not follow the redirects of {last}: {reason}");
                last
            }
        };
        let directory = tempfile::Builder::new()
            .prefix("purewaterspiritbot-")
            .tempdir()
            .map_err(|error| FetchError::Failed(error.to_string()))?;
        // 给工具的是跳转之后、没有清理过的地址：有些参数是打开页面所需要的凭证。
        let paths = self
            .downloader
            .download(&resolved, directory.path())
            .await
            .map_err(|error| match error {
                DownloadError::NoTool => FetchError::NoTool,
                DownloadError::Failed(reason) => FetchError::Failed(reason),
            })?;
        let (files, skipped) = download::classify(paths);
        tracing::info!(
            "Fetched {} file(s) from {resolved}, skipped {}",
            files.len(),
            skipped.len()
        );
        Ok(Fetched {
            source: strip_tracking_params(&resolved),
            files,
            skipped,
            _directory: directory,
        })
    }
}

/// 每组媒体第一条的说明文字：一个指向来源的链接。
pub fn source_caption(source: &Url) -> (String, Vec<MessageEntity>) {
    let length = SOURCE_LABEL.encode_utf16().count();
    (
        SOURCE_LABEL.to_owned(),
        vec![MessageEntity::text_link(source.clone(), 0, length)],
    )
}

#[cfg(all(test, unix))]
mod tests {
    use std::{fs, os::unix::fs::PermissionsExt, path::Path};

    use teloxide::types::MessageEntityKind;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    use super::*;
    use crate::model::MediaKind;

    fn tool(directory: &Path, name: &str, body: &str) -> PathBuf {
        use std::io::Write;

        let path = directory.join(name);
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "#!/bin/sh\n{body}").unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    async fn downloader(tool: PathBuf) -> Downloader {
        let config = FetchConfig {
            gallery_dl: Some(tool),
            yt_dlp: Some("/nonexistent/yt-dlp".into()),
            ..FetchConfig::default()
        };
        Downloader::detect(&config).await
    }

    #[test]
    fn the_caption_links_to_the_source() {
        let source = Url::parse("https://example.com/a?b=1").unwrap();
        let (text, entities) = source_caption(&source);
        assert_eq!(text, "source");
        assert_eq!(entities.len(), 1);
        assert_eq!((entities[0].offset, entities[0].length), (0, 6));
        assert_eq!(
            entities[0].kind,
            MessageEntityKind::TextLink { url: source }
        );
    }

    #[tokio::test]
    async fn downloads_with_the_resolved_address_and_reports_the_clean_one() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/short"))
            .respond_with(
                ResponseTemplate::new(302).insert_header("location", "/final?id=7&utm_source=x"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/final"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;

        let tools = tempfile::tempdir().unwrap();
        let record = tools.path().join("arguments.txt");
        let gallery_dl = tool(
            tools.path(),
            "gallery-dl",
            &format!(
                r#"
echo "$@" >> "{}"
while [ $# -gt 0 ]; do
  if [ "$1" = "-D" ]; then dir="$2"; fi
  shift
done
[ -n "$dir" ] || exit 0
echo x > "$dir/a.jpg"
echo x > "$dir/b.gif"
echo "$dir/a.jpg"
echo "$dir/b.gif"
"#,
                record.display()
            ),
        );
        let fetcher = Fetcher::with_parts(
            Resolver::allowing_private_addresses(),
            downloader(gallery_dl).await,
        );

        let fetched = fetcher
            .fetch(Url::parse(&format!("{}/short", server.uri())).unwrap())
            .await
            .unwrap();

        assert_eq!(fetched.source.path(), "/final");
        assert_eq!(fetched.source.query(), Some("id=7"));
        assert_eq!(fetched.files.len(), 1);
        assert_eq!(fetched.files[0].kind, MediaKind::Photo);
        assert!(fetched.files[0].path.is_file());
        assert_eq!(fetched.skipped.len(), 1);
        let recorded = fs::read_to_string(record).unwrap();
        assert!(
            recorded.trim_end().ends_with("/final?id=7&utm_source=x"),
            "{recorded}"
        );

        // 文件在 `Fetched` 被丢弃时一起删除。
        let file = fetched.files[0].path.clone();
        drop(fetched);
        assert!(!file.exists());
    }

    #[tokio::test]
    async fn refuses_a_local_address() {
        let fetcher = Fetcher::unavailable();
        let error = fetcher
            .fetch(Url::parse("http://127.0.0.1:1/a").unwrap())
            .await
            .err()
            .unwrap();
        assert_eq!(error, FetchError::Blocked);
    }

    #[tokio::test]
    async fn without_tools_nothing_is_downloaded() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200))
            .mount(&server)
            .await;
        let fetcher = Fetcher::with_parts(
            Resolver::allowing_private_addresses(),
            Downloader::unavailable(),
        );
        let error = fetcher
            .fetch(Url::parse(&server.uri()).unwrap())
            .await
            .err()
            .unwrap();
        assert_eq!(error, FetchError::NoTool);
    }
}
