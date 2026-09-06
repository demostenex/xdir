//! `FilePreview`: preview *content* for a regular file, Milestone 4A's
//! addition to Milestone 3's preview *context*.
//!
//! Deliberately small: this milestone only ever produces a bounded text
//! preview (see [`crate::core::filesystem::read_text_preview`]) or one of
//! two terminal non-content states. No provider framework, no registry —
//! adding a second content kind later is a second match arm here, not a
//! new abstraction.

use std::path::Path;

use crate::core::filesystem::{self, TextPreview};

/// What PREVIEW shows for a selected regular file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilePreview {
    /// A valid-UTF-8 text prefix of the file (see
    /// [`crate::core::filesystem::MAX_TEXT_PREVIEW_BYTES`]).
    Text { content: String, truncated: bool },
    /// The file exists and was read, but its content isn't a text preview
    /// this milestone can show (contains NUL, or isn't valid UTF-8).
    Unsupported,
    /// The file couldn't be opened or read (removed, permission denied,
    /// races with the filesystem, ...). Affects PREVIEW only — never
    /// `current_dir`, never CURRENT's own listing, never a crash.
    Unavailable,
}

impl FilePreview {
    pub(crate) fn build(path: &Path) -> Self {
        match filesystem::read_text_preview(path) {
            Ok(TextPreview::Text { content, truncated }) => {
                FilePreview::Text { content, truncated }
            }
            Ok(TextPreview::Unsupported) => FilePreview::Unsupported,
            Err(_) => FilePreview::Unavailable,
        }
    }
}
