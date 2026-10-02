use base64::prelude::*;

use super::VisionError;

/// 单张图片的大小上限。多数服务对请求体有上限，base64 之后体积还会增加三分之一。
pub const MAX_IMAGE_BYTES: usize = 10 * 1024 * 1024;

/// 一张可以发给模型的图片。格式由文件头判断，不依赖 Telegram 给的文件名或 MIME。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    mime: &'static str,
    bytes: Vec<u8>,
}

impl Image {
    pub fn new(bytes: Vec<u8>) -> Result<Self, VisionError> {
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(VisionError::ImageTooLarge);
        }
        let mime = sniff_mime(&bytes).ok_or(VisionError::UnsupportedImage)?;
        Ok(Self { mime, bytes })
    }

    pub(super) fn data_url(&self) -> String {
        format!(
            "data:{};base64,{}",
            self.mime,
            BASE64_STANDARD.encode(&self.bytes)
        )
    }
}

fn sniff_mime(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

#[cfg(test)]
pub(super) mod fixtures {
    pub fn jpeg() -> Vec<u8> {
        vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3]
    }

    pub fn png() -> Vec<u8> {
        vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_formats_by_their_header() {
        let webp = [b"RIFF".as_slice(), &[0, 0, 0, 0], b"WEBPVP8 "].concat();
        for (bytes, mime) in [
            (fixtures::jpeg(), "image/jpeg"),
            (fixtures::png(), "image/png"),
            (b"GIF89a...".to_vec(), "image/gif"),
            (webp, "image/webp"),
        ] {
            assert_eq!(Image::new(bytes).unwrap().mime, mime);
        }
    }

    #[test]
    fn builds_a_data_url() {
        let image = Image::new(fixtures::jpeg()).unwrap();
        assert_eq!(image.data_url(), "data:image/jpeg;base64,/9j/4AECAw==");
    }

    #[test]
    fn rejects_unknown_formats_and_oversized_files() {
        assert!(matches!(
            Image::new(b"not an image".to_vec()),
            Err(VisionError::UnsupportedImage)
        ));
        assert!(matches!(
            Image::new(Vec::new()),
            Err(VisionError::UnsupportedImage)
        ));
        let mut huge = fixtures::jpeg();
        huge.resize(MAX_IMAGE_BYTES + 1, 0);
        assert!(matches!(Image::new(huge), Err(VisionError::ImageTooLarge)));
    }
}
