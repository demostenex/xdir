//! `FilePreview`: preview *content* for a regular file. Milestone 4A added
//! bounded text previews; Milestone 4B adds bounded image previews.
//!
//! Deliberately small: no provider framework, no registry — a third
//! content kind later is a third match arm in [`FilePreview::build`], not a
//! new abstraction.

use std::path::Path;

use crate::core::filesystem::{self, TextPreview};
use crate::core::image::{self, ImagePreview};

/// What PREVIEW shows for a selected regular file.
#[derive(Debug, Clone, PartialEq)]
pub enum FilePreview {
    /// A valid-UTF-8 text prefix of the file (see
    /// [`crate::core::filesystem::MAX_TEXT_PREVIEW_BYTES`]).
    Text { content: String, truncated: bool },
    /// Decoded RGBA8 pixels for a PNG/JPEG within
    /// [`crate::core::image`]'s size limits.
    Image {
        width: u32,
        height: u32,
        rgba: Vec<u8>,
    },
    /// Recognized as an image (PNG/JPEG) but rejected by a size limit
    /// before decode was ever attempted — distinct from `Unsupported` so
    /// the UI can say *why* there's no preview.
    TooLarge,
    /// The file exists and was read, but its content isn't a preview this
    /// milestone can show: not a recognized/decodable PNG or JPEG, and (as
    /// text) contains NUL or isn't valid UTF-8.
    Unsupported,
    /// The file couldn't be opened or read (removed, permission denied,
    /// races with the filesystem, ...). Affects PREVIEW only — never
    /// `current_dir`, never CURRENT's own listing, never a crash.
    Unavailable,
}

impl FilePreview {
    /// Tries image first, since it's the cheaper and more specific check
    /// (a signature sniff plus a header-only dimension read before any
    /// pixel data is touched — see [`crate::core::image::read_image_preview`]).
    /// Only a file that isn't a recognized, size-safe image at all falls
    /// through to the text policy, so an ordinary text file never pays for
    /// an image-format probe beyond that cheap signature check, and a PNG
    /// or JPEG is never read a second time as "maybe text".
    ///
    /// `TooLarge` is terminal here on purpose: it does *not* fall through
    /// to the text policy. Reading a multi-hundred-megabyte image's binary
    /// bytes as "maybe text" would only ever produce `Unsupported` anyway
    /// (NUL bytes are all but guaranteed in the first 64KiB of PNG/JPEG
    /// data) — falling through would just waste a read and lose the
    /// specific "too large" reason the UI is supposed to show instead of
    /// the generic "unsupported" message.
    pub(crate) fn build(path: &Path) -> Self {
        match image::read_image_preview(path) {
            Ok(ImagePreview::Image {
                width,
                height,
                rgba,
            }) => {
                return FilePreview::Image {
                    width,
                    height,
                    rgba,
                };
            }
            Ok(ImagePreview::TooLarge) => return FilePreview::TooLarge,
            Ok(ImagePreview::Unsupported) => return FilePreview::Unsupported,
            Ok(ImagePreview::NotAnImage) => {}
            Err(_) => return FilePreview::Unavailable,
        }

        match filesystem::read_text_preview(path) {
            Ok(TextPreview::Text { content, truncated }) => {
                FilePreview::Text { content, truncated }
            }
            Ok(TextPreview::Unsupported) => FilePreview::Unsupported,
            Err(_) => FilePreview::Unavailable,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    #[test]
    fn text_file_with_image_extension_falls_back_to_text_preview() {
        let dir = TempDir::new();
        // `.png` extension, real UTF-8 text content: format detection is
        // content-based only (see `core::image::read_image_preview`), so
        // this must never be classified as `Image` or `Unsupported` just
        // because of its name — it falls through to the same text policy
        // an ordinary `.txt` file gets.
        let path = dir.path().join("notes.png");
        fs::write(&path, "just some text\n").unwrap();

        let preview = FilePreview::build(&path);

        assert_eq!(
            preview,
            FilePreview::Text {
                content: "just some text\n".to_string(),
                truncated: false,
            }
        );
    }
}
