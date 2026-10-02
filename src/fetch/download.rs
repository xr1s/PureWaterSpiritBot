//! 调用 gallery-dl 和 yt-dlp，把链接里的媒体下载到一个目录。

use std::{
    ffi::OsString,
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use tokio::process::Command;
use url::Url;

use crate::{config::FetchConfig, model::MediaKind};

const GALLERY_DL: &str = "gallery-dl";
const YT_DLP: &str = "yt-dlp";
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);
/// 下载工具遇到更大的文件就跳过：Bot 上传任何文件都不能超过 50 MB。
const TOOL_SIZE_LIMIT: &str = "50M";
/// Telegram Bot API 对上传文件的限制：照片 10 MB，其他文件 50 MB。
pub const PHOTO_SIZE_LIMIT: u64 = 10_000_000;
pub const FILE_SIZE_LIMIT: u64 = 50_000_000;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DownloadError {
    #[error("neither gallery-dl nor yt-dlp is installed")]
    NoTool,
    /// 工具都没能下载到东西，附带每个工具的错误信息，一行一个。
    #[error("{0}")]
    Failed(String),
}

/// 外部下载工具。没有安装的工具是 `None`，会被跳过。
pub struct Downloader {
    gallery_dl: Option<PathBuf>,
    yt_dlp: Option<PathBuf>,
    timeout: Duration,
    max_files: u16,
}

impl Downloader {
    /// 检查两个工具是否可用，并记录结果。
    pub async fn detect(config: &FetchConfig) -> Self {
        let gallery_dl = probe(GALLERY_DL, config.gallery_dl.as_deref()).await;
        let yt_dlp = probe(YT_DLP, config.yt_dlp.as_deref()).await;
        Self {
            gallery_dl,
            yt_dlp,
            timeout: config.timeout(),
            max_files: config.max_files(),
        }
    }

    /// 没有任何工具：所有下载都会失败。
    #[cfg(test)]
    pub fn unavailable() -> Self {
        Self {
            gallery_dl: None,
            yt_dlp: None,
            timeout: Duration::from_secs(1),
            max_files: 1,
        }
    }

    pub fn is_available(&self) -> bool {
        self.gallery_dl.is_some() || self.yt_dlp.is_some()
    }

    /// 把 `url` 里的媒体下载到 `directory`，按下载顺序返回文件。
    /// 先用 gallery-dl（适合图片），没下载到东西再用 yt-dlp（适合视频）。
    pub async fn download(
        &self,
        url: &Url,
        directory: &Path,
    ) -> Result<Vec<PathBuf>, DownloadError> {
        let mut attempts = Vec::new();
        if let Some(program) = &self.gallery_dl {
            attempts.push((
                GALLERY_DL,
                program,
                self.gallery_dl_arguments(url, directory),
            ));
        }
        if let Some(program) = &self.yt_dlp {
            attempts.push((YT_DLP, program, self.yt_dlp_arguments(url, directory)));
        }
        if attempts.is_empty() {
            return Err(DownloadError::NoTool);
        }
        let mut errors = Vec::new();
        for (name, program, arguments) in attempts {
            let error = match run(program, &arguments, self.timeout).await {
                Ok(output) => {
                    let files = downloaded_files(directory, &output.stdout);
                    if !files.is_empty() {
                        return Ok(files);
                    }
                    format!("{name}: {}", failure_reason(&output))
                }
                Err(reason) => format!("{name}: {reason}"),
            };
            tracing::info!("{error}");
            errors.push(error);
        }
        Err(DownloadError::Failed(errors.join("\n")))
    }

    fn gallery_dl_arguments(&self, url: &Url, directory: &Path) -> Vec<OsString> {
        // `--` 之后的都是位置参数，链接不会被当成选项。
        let mut arguments = os_strings(&[
            "--no-input",
            "--no-mtime",
            "--filesize-max",
            TOOL_SIZE_LIMIT,
            "--range",
            &format!("1-{}", self.max_files),
            "-D",
        ]);
        arguments.push(directory.into());
        arguments.extend(os_strings(&["--", url.as_str()]));
        arguments
    }

    fn yt_dlp_arguments(&self, url: &Url, directory: &Path) -> Vec<OsString> {
        let mut arguments = os_strings(&[
            "--no-warnings",
            "--no-playlist",
            "--playlist-end",
            &self.max_files.to_string(),
            "--max-filesize",
            TOOL_SIZE_LIMIT,
            "-f",
            "b[ext=mp4]/bv*[ext=mp4]+ba[ext=m4a]/b",
            "--no-simulate",
            "--print",
            "after_move:filepath",
            "-o",
        ]);
        arguments.push(directory.join("%(autonumber)03d.%(ext)s").into());
        arguments.extend(os_strings(&["--", url.as_str()]));
        arguments
    }
}

fn os_strings(values: &[&str]) -> Vec<OsString> {
    values.iter().map(OsString::from).collect()
}

/// 运行 `--version` 确认工具可用。
async fn probe(name: &str, configured: Option<&Path>) -> Option<PathBuf> {
    let program = configured.unwrap_or_else(|| Path::new(name)).to_owned();
    match run(&program, &os_strings(&["--version"]), PROBE_TIMEOUT).await {
        Ok(output) if output.status.success() => {
            tracing::info!(
                "Found {name} {} at {}",
                String::from_utf8_lossy(&output.stdout).trim(),
                program.display()
            );
            Some(program)
        }
        Ok(output) => {
            tracing::warn!("{name} is not usable: {}", failure_reason(&output));
            None
        }
        Err(reason) => {
            tracing::warn!("{name} is not available ({}): {reason}", program.display());
            None
        }
    }
}

async fn run(
    program: &Path,
    arguments: &[OsString],
    timeout: Duration,
) -> Result<std::process::Output, String> {
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .kill_on_drop(true);
    match tokio::time::timeout(timeout, command.output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(error)) => Err(error.to_string()),
        Err(_) => Err(format!("timed out after {timeout:?}")),
    }
}

/// 工具没有下载到东西时，用它输出的最后一行错误说明原因。
fn failure_reason(output: &std::process::Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let last_line = stderr.lines().rev().find(|line| !line.trim().is_empty());
    match last_line {
        Some(line) => line.trim().to_owned(),
        None if output.status.success() => "nothing was downloaded".to_owned(),
        None => format!("exited with {}", output.status),
    }
}

/// 工具在标准输出里逐行打印下载好的文件路径（gallery-dl 会给已存在的文件加 `# ` 前缀）。
/// 没有打印时，退而求其次读目录里的文件。
fn downloaded_files(directory: &Path, stdout: &[u8]) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    for line in String::from_utf8_lossy(stdout).lines() {
        let path = PathBuf::from(line.trim().trim_start_matches("# "));
        if path.is_file() && !files.contains(&path) {
            files.push(path);
        }
    }
    if !files.is_empty() {
        return files;
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(directory)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && !is_partial(path))
        .collect();
    files.sort();
    files
}

fn is_partial(path: &Path) -> bool {
    matches!(
        extension(path).as_deref(),
        Some("part" | "ytdl" | "json" | "tmp")
    )
}

fn extension(path: &Path) -> Option<String> {
    path.extension()
        .map(|extension| extension.to_string_lossy().to_ascii_lowercase())
}

/// 下载到的一个可以发给 Telegram 的文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFile {
    pub path: PathBuf,
    pub kind: MediaKind,
}

/// 下载到的文件里，不能发给 Telegram 的一个，以及原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skipped {
    pub name: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 不是 Telegram 能作为照片或视频发出的格式。
    Unsupported,
    TooLarge {
        size: u64,
        limit: u64,
    },
}

/// 把下载到的文件分成能发的和不能发的。能发的保持原来的顺序。
pub fn classify(paths: Vec<PathBuf>) -> (Vec<MediaFile>, Vec<Skipped>) {
    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for path in paths {
        let name = path
            .file_name()
            .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
        let kind = match extension(&path).as_deref() {
            Some("jpg" | "jpeg" | "png" | "webp") => MediaKind::Photo,
            Some("mp4" | "mov" | "m4v") => MediaKind::Video,
            _ => {
                skipped.push(Skipped {
                    name,
                    reason: SkipReason::Unsupported,
                });
                continue;
            }
        };
        let limit = if kind == MediaKind::Photo {
            PHOTO_SIZE_LIMIT
        } else {
            FILE_SIZE_LIMIT
        };
        let size = std::fs::metadata(&path).map_or(0, |metadata| metadata.len());
        if size > limit {
            skipped.push(Skipped {
                name,
                reason: SkipReason::TooLarge { size, limit },
            });
        } else {
            files.push(MediaFile { path, kind });
        }
    }
    (files, skipped)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn url() -> Url {
        Url::parse("https://example.com/a").unwrap()
    }

    #[cfg(unix)]
    fn script(directory: &Path, name: &str, body: &str) -> PathBuf {
        use std::{io::Write, os::unix::fs::PermissionsExt};

        let path = directory.join(name);
        let mut file = fs::File::create(&path).unwrap();
        writeln!(file, "#!/bin/sh\n{body}").unwrap();
        file.sync_all().unwrap();
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn downloader(gallery_dl: Option<PathBuf>, yt_dlp: Option<PathBuf>) -> Downloader {
        Downloader {
            gallery_dl,
            yt_dlp,
            timeout: Duration::from_secs(10),
            max_files: 5,
        }
    }

    /// 像 gallery-dl 一样把两个文件下载到 `-D` 指定的目录，并打印路径。
    #[cfg(unix)]
    const WRITES_TWO_IMAGES: &str = r##"
while [ $# -gt 0 ]; do
  if [ "$1" = "-D" ]; then dir="$2"; fi
  shift
done
[ -n "$dir" ] || exit 0
echo a > "$dir/b.jpg"
echo b > "$dir/a.png"
echo "$dir/b.jpg"
echo "# $dir/a.png"
"##;

    #[tokio::test]
    #[cfg(unix)]
    async fn uses_the_printed_paths_in_order() {
        let tools = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let tool = script(tools.path(), "gallery-dl", WRITES_TWO_IMAGES);

        let files = downloader(Some(tool), None)
            .download(&url(), work.path())
            .await
            .unwrap();
        let names: Vec<_> = files
            .iter()
            .map(|file| file.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["b.jpg", "a.png"]);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn falls_back_to_the_second_tool() {
        let tools = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let failing = script(
            tools.path(),
            "gallery-dl",
            "echo \"[gallery-dl][error] Unsupported URL\" >&2\nexit 64",
        );
        let working = script(
            tools.path(),
            "yt-dlp",
            &format!(
                "dir=\"{}\"\necho v > \"$dir/001.mp4\"\necho \"$dir/001.mp4\"",
                work.path().display()
            ),
        );

        let files = downloader(Some(failing), Some(working))
            .download(&url(), work.path())
            .await
            .unwrap();
        assert_eq!(files, [work.path().join("001.mp4")]);
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn reports_the_last_line_of_the_error() {
        let tools = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let failing = script(
            tools.path(),
            "gallery-dl",
            "echo \"first line\" >&2\necho \"Unsupported URL\" >&2\nexit 64",
        );
        let error = downloader(Some(failing), None)
            .download(&url(), work.path())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            DownloadError::Failed("gallery-dl: Unsupported URL".to_owned())
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn reports_every_error_when_every_tool_fails() {
        let tools = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let gallery_dl = script(
            tools.path(),
            "gallery-dl",
            "echo \"AuthorizationError: login required\" >&2\nexit 16",
        );
        let yt_dlp = script(
            tools.path(),
            "yt-dlp",
            "echo \"ERROR: Unsupported URL\" >&2\nexit 1",
        );
        let error = downloader(Some(gallery_dl), Some(yt_dlp))
            .download(&url(), work.path())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            DownloadError::Failed(
                "gallery-dl: AuthorizationError: login required\nyt-dlp: ERROR: Unsupported URL"
                    .to_owned()
            )
        );
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn passes_the_link_after_a_separator() {
        let tools = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let record = work.path().join("arguments.txt");
        let tool = script(
            tools.path(),
            "gallery-dl",
            &format!("echo \"$@\" > \"{}\"\nexit 1", record.display()),
        );
        let _ = downloader(Some(tool), None)
            .download(&url(), work.path())
            .await;
        let recorded = fs::read_to_string(record).unwrap();
        assert!(recorded.trim_end().ends_with("-- https://example.com/a"));
        assert!(recorded.contains("--range 1-5"));
    }

    #[tokio::test]
    #[cfg(unix)]
    async fn a_tool_that_takes_too_long_is_stopped() {
        let tools = tempfile::tempdir().unwrap();
        let work = tempfile::tempdir().unwrap();
        let slow = script(tools.path(), "gallery-dl", "sleep 30");
        let mut downloader = downloader(Some(slow), None);
        downloader.timeout = Duration::from_millis(200);
        let error = downloader.download(&url(), work.path()).await.unwrap_err();
        assert!(matches!(error, DownloadError::Failed(reason) if reason.contains("timed out")));
    }

    #[tokio::test]
    async fn no_tool_is_reported() {
        let work = tempfile::tempdir().unwrap();
        let error = Downloader::unavailable()
            .download(&url(), work.path())
            .await
            .unwrap_err();
        assert_eq!(error, DownloadError::NoTool);
        assert!(!Downloader::unavailable().is_available());
    }

    #[tokio::test]
    async fn a_missing_program_is_not_detected() {
        let config = FetchConfig {
            gallery_dl: Some(PathBuf::from("/nonexistent/gallery-dl")),
            yt_dlp: Some(PathBuf::from("/nonexistent/yt-dlp")),
            ..FetchConfig::default()
        };
        assert!(!Downloader::detect(&config).await.is_available());
    }

    #[test]
    fn falls_back_to_the_directory_listing() {
        let work = tempfile::tempdir().unwrap();
        fs::write(work.path().join("2.jpg"), "x").unwrap();
        fs::write(work.path().join("1.jpg"), "x").unwrap();
        fs::write(work.path().join("3.jpg.part"), "x").unwrap();
        let files = downloaded_files(work.path(), b"");
        assert_eq!(
            files,
            [work.path().join("1.jpg"), work.path().join("2.jpg")]
        );
    }

    #[test]
    fn classifies_by_type_and_size() {
        let work = tempfile::tempdir().unwrap();
        let write = |name: &str, size: usize| {
            let path = work.path().join(name);
            fs::write(&path, vec![0u8; size]).unwrap();
            path
        };
        let paths = vec![
            write("a.JPG", 10),
            write("b.mp4", 10),
            write("c.gif", 10),
            write("d.png", usize::try_from(PHOTO_SIZE_LIMIT).unwrap() + 1),
            write("e.webm", 10),
        ];
        let (files, skipped) = classify(paths);

        let kinds: Vec<_> = files.iter().map(|file| file.kind).collect();
        assert_eq!(kinds, [MediaKind::Photo, MediaKind::Video]);
        assert_eq!(
            skipped
                .iter()
                .map(|skipped| (skipped.name.as_str(), skipped.reason))
                .collect::<Vec<_>>(),
            [
                ("c.gif", SkipReason::Unsupported),
                (
                    "d.png",
                    SkipReason::TooLarge {
                        size: PHOTO_SIZE_LIMIT + 1,
                        limit: PHOTO_SIZE_LIMIT
                    }
                ),
                ("e.webm", SkipReason::Unsupported),
            ]
        );
    }
}
