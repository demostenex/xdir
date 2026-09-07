//! Read-only contextual state for the PARENT and PREVIEW panes.
//!
//! Both are pure projections of `Navigation`/`FileEntry` — toolkit-agnostic,
//! synchronous, non-recursive. Neither ever mutates `current_dir` or the
//! CURRENT pane's own entries; a read failure here degrades to an empty or
//! "unavailable" context, never a crash and never a silent directory
//! change.

use std::path::{Path, PathBuf};

use crate::app::FilePreview;
use crate::core::filesystem;
use crate::model::{EntryKind, FileEntry};

/// Where `current_dir` sits relative to its own parent directory.
///
/// `current_index` is computed once here, by real `Path` identity, so the
/// UI layer never has to re-derive "which row is `current_dir`" itself —
/// in particular never by comparing display text.
#[derive(Debug, Clone, Default)]
pub struct ParentContext {
    dir: Option<PathBuf>,
    entries: Vec<FileEntry>,
    current_index: Option<usize>,
}

impl ParentContext {
    pub fn dir(&self) -> Option<&Path> {
        self.dir.as_deref()
    }

    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    /// Index into [`Self::entries`] of the entry that *is* `current_dir`,
    /// if it could be identified. `None` both when there's no logical
    /// parent (root) and when the parent listing couldn't be read at all.
    pub fn current_index(&self) -> Option<usize> {
        self.current_index
    }

    /// Builds the PARENT context for `current_dir`. Never crashes: a
    /// directory with no logical parent (root) or a parent that can't be
    /// read both degrade to an empty context rather than an error.
    pub(crate) fn build(current_dir: &Path, show_hidden: bool) -> Self {
        let Some(parent_dir) = current_dir.parent().map(Path::to_path_buf) else {
            return ParentContext::default();
        };

        let mut entries = filesystem::read_directory(&parent_dir, show_hidden).unwrap_or_default();
        let mut current_index = entries.iter().position(|entry| entry.path() == current_dir);

        // `current_dir` may itself be a dotfile that the parent's own
        // `show_hidden`-filtered listing omits (e.g. the current directory
        // is `~/.config`). It must stay representable/highlightable in
        // PARENT regardless, so synthesize it directly from the filesystem
        // rather than losing it — this is the one deliberate exception to
        // `show_hidden` filtering for PARENT, and it's local to this one
        // entry, not a change to `read_directory`'s global rule.
        if current_index.is_none() {
            if let Ok(synthetic) = FileEntry::from_path(current_dir.to_path_buf()) {
                entries.push(synthetic);
                filesystem::sort_entries(&mut entries);
                current_index = entries.iter().position(|entry| entry.path() == current_dir);
            }
        }

        ParentContext {
            dir: Some(parent_dir),
            entries,
            current_index,
        }
    }
}

/// What to show in PREVIEW about whatever is currently selected in CURRENT.
///
/// Directory/Symlink/Other stay pure *context* (Milestone 3): no file
/// content, no MIME, no thumbnail. `File` carries Milestone 4A's one piece
/// of real *content*: a bounded text preview. A symlink is never followed
/// to reach it — even a symlink to a text file stays `Symlink`, not `File`.
#[derive(Debug, Clone, Default)]
pub enum PreviewContext {
    #[default]
    None,
    Directory(Vec<FileEntry>),
    DirectoryUnavailable,
    File(FilePreview),
    Symlink,
    Other,
}

impl PreviewContext {
    pub(crate) fn build(selected: Option<&FileEntry>, show_hidden: bool) -> Self {
        let Some(entry) = selected else {
            return PreviewContext::None;
        };

        match entry.kind() {
            EntryKind::Directory => match filesystem::read_directory(entry.path(), show_hidden) {
                Ok(children) => PreviewContext::Directory(children),
                Err(_) => PreviewContext::DirectoryUnavailable,
            },
            EntryKind::File => PreviewContext::File(FilePreview::build(entry.path())),
            EntryKind::Symlink => PreviewContext::Symlink,
            EntryKind::Other => PreviewContext::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    fn names(entries: &[FileEntry]) -> Vec<String> {
        entries
            .iter()
            .map(|e| e.name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn parent_context_points_at_the_parent_directory_and_highlights_current_dir() {
        let dir = TempDir::new();
        let current = dir.path().join("current");
        fs::create_dir(&current).unwrap();
        fs::create_dir(dir.path().join("sibling")).unwrap();

        let parent = ParentContext::build(&current, false);

        assert_eq!(parent.dir(), Some(dir.path()));
        assert_eq!(names(parent.entries()), vec!["current", "sibling"]);
        let highlighted = parent.current_index().and_then(|i| parent.entries().get(i));
        assert_eq!(highlighted.map(FileEntry::path), Some(current.as_path()));
    }

    #[test]
    fn parent_context_of_root_has_no_parent_and_does_not_crash() {
        let parent = ParentContext::build(Path::new("/"), false);

        assert_eq!(parent.dir(), None);
        assert!(parent.entries().is_empty());
        assert_eq!(parent.current_index(), None);
    }

    #[test]
    fn parent_context_respects_show_hidden_for_siblings() {
        let dir = TempDir::new();
        let current = dir.path().join("current");
        fs::create_dir(&current).unwrap();
        // A regular file, so it never outranks the "current" directory in
        // the dirs-first sort regardless of `show_hidden`.
        fs::write(dir.path().join(".hidden-sibling"), b"").unwrap();

        let hidden = ParentContext::build(&current, false);
        assert_eq!(names(hidden.entries()), vec!["current"]);

        let shown = ParentContext::build(&current, true);
        assert_eq!(names(shown.entries()), vec!["current", ".hidden-sibling"]);
    }

    #[test]
    fn current_dir_stays_representable_in_parent_even_when_hidden_and_show_hidden_is_false() {
        let dir = TempDir::new();
        let current = dir.path().join(".config");
        fs::create_dir(&current).unwrap();
        fs::create_dir(dir.path().join("visible")).unwrap();

        let parent = ParentContext::build(&current, false);

        // ".config" would normally be filtered out of a show_hidden=false
        // listing of `dir`, but it's current_dir, so it must still be
        // present and identifiable.
        assert!(names(parent.entries()).contains(&".config".to_string()));
        let highlighted = parent.current_index().and_then(|i| parent.entries().get(i));
        assert_eq!(highlighted.map(FileEntry::path), Some(current.as_path()));
    }

    #[cfg(unix)]
    #[test]
    fn parent_highlight_identity_survives_a_non_utf8_current_dir_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;

        let dir = TempDir::new();
        // 0x66, 0x80 ("f" followed by a lone continuation byte): invalid
        // UTF-8, but a perfectly valid Unix filename.
        let raw_name = OsStr::from_bytes(&[0x66, 0x80, 0x6f]).to_os_string();
        let current = dir.path().join(&raw_name);
        fs::create_dir(&current).unwrap();

        let parent = ParentContext::build(&current, false);

        // Identity must be established via `Path` equality on the real
        // name, never via a lossy string conversion.
        let highlighted = parent.current_index().and_then(|i| parent.entries().get(i));
        assert_eq!(highlighted.map(FileEntry::path), Some(current.as_path()));
    }

    #[test]
    fn preview_of_a_directory_lists_its_immediate_children_sorted_dirs_first() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("alpha")).unwrap();
        fs::write(dir.path().join("z_file"), b"").unwrap();
        fs::create_dir_all(dir.path().join("alpha/nested")).unwrap();
        let alpha = FileEntry::from_path(dir.path().join("alpha")).unwrap();

        let preview = PreviewContext::build(Some(&alpha), false);

        match preview {
            PreviewContext::Directory(children) => {
                assert_eq!(names(&children), vec!["nested"]);
            }
            other => panic!("expected Directory preview, got {other:?}"),
        }
    }

    #[test]
    fn preview_of_a_directory_respects_show_hidden() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/.hidden"), b"").unwrap();
        fs::write(dir.path().join("target/visible"), b"").unwrap();
        let target = FileEntry::from_path(dir.path().join("target")).unwrap();

        let hidden = PreviewContext::build(Some(&target), false);
        match hidden {
            PreviewContext::Directory(children) => assert_eq!(names(&children), vec!["visible"]),
            other => panic!("expected Directory preview, got {other:?}"),
        }

        let shown = PreviewContext::build(Some(&target), true);
        match shown {
            PreviewContext::Directory(children) => {
                assert_eq!(names(&children), vec![".hidden", "visible"])
            }
            other => panic!("expected Directory preview, got {other:?}"),
        }
    }

    #[test]
    fn preview_of_a_regular_utf8_file_shows_its_content() {
        let dir = TempDir::new();
        fs::write(dir.path().join("file.txt"), b"content").unwrap();
        let file = FileEntry::from_path(dir.path().join("file.txt")).unwrap();

        let preview = PreviewContext::build(Some(&file), false);

        match preview {
            PreviewContext::File(FilePreview::Text { content, truncated }) => {
                assert_eq!(content, "content");
                assert!(!truncated);
            }
            other => panic!("expected a Text file preview, got {other:?}"),
        }
    }

    #[test]
    fn preview_of_a_binary_file_is_unsupported_not_a_directory_listing() {
        let dir = TempDir::new();
        fs::write(dir.path().join("file.bin"), [b'a', 0u8, b'b']).unwrap();
        let file = FileEntry::from_path(dir.path().join("file.bin")).unwrap();

        let preview = PreviewContext::build(Some(&file), false);

        assert!(matches!(
            preview,
            PreviewContext::File(FilePreview::Unsupported)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn preview_of_a_symlink_is_not_treated_as_directory_even_when_it_points_to_one() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let real = dir.path().join("real");
        let link = dir.path().join("link");
        fs::create_dir(&real).unwrap();
        symlink(&real, &link).unwrap();
        let link_entry = FileEntry::from_path(link).unwrap();
        assert_eq!(link_entry.kind(), EntryKind::Symlink);

        let preview = PreviewContext::build(Some(&link_entry), false);

        assert!(matches!(preview, PreviewContext::Symlink));
    }

    #[cfg(unix)]
    #[test]
    fn preview_of_a_symlink_to_a_text_file_stays_symlink_never_reads_through_it() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let real = dir.path().join("real.txt");
        let link = dir.path().join("text-link");
        fs::write(&real, "not shown").unwrap();
        symlink(&real, &link).unwrap();
        let link_entry = FileEntry::from_path(link).unwrap();

        let preview = PreviewContext::build(Some(&link_entry), false);

        assert!(matches!(preview, PreviewContext::Symlink));
    }

    #[test]
    fn preview_of_a_file_removed_after_being_selected_is_unavailable_not_a_crash() {
        let dir = TempDir::new();
        let path = dir.path().join("ghost.txt");
        fs::write(&path, "gone soon").unwrap();
        let entry = FileEntry::from_path(path.clone()).unwrap();
        fs::remove_file(&path).unwrap();

        let preview = PreviewContext::build(Some(&entry), false);

        assert!(matches!(
            preview,
            PreviewContext::File(FilePreview::Unavailable)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn preview_of_a_symlink_to_a_png_stays_symlink_never_reads_through_it() {
        use std::os::unix::fs::symlink;

        let dir = TempDir::new();
        let real = dir.path().join("real.png");
        let link = dir.path().join("image-link");
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save_with_format(&real, image::ImageFormat::Png)
            .unwrap();
        symlink(&real, &link).unwrap();
        let link_entry = FileEntry::from_path(link).unwrap();

        let preview = PreviewContext::build(Some(&link_entry), false);

        assert!(matches!(preview, PreviewContext::Symlink));
    }

    #[test]
    fn preview_of_a_valid_png_is_image() {
        let dir = TempDir::new();
        let path = dir.path().join("photo.png");
        image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 9, 9, 255]))
            .save_with_format(&path, image::ImageFormat::Png)
            .unwrap();
        let entry = FileEntry::from_path(path).unwrap();

        let preview = PreviewContext::build(Some(&entry), false);

        match preview {
            PreviewContext::File(FilePreview::Image { width, height, .. }) => {
                assert_eq!((width, height), (4, 4));
            }
            other => panic!("expected an Image preview, got {other:?}"),
        }
    }

    #[test]
    fn oversized_image_selection_does_not_change_current_dir_or_entries() {
        let dir = TempDir::new();
        image::RgbaImage::from_pixel(
            crate::core::image::MAX_IMAGE_DIMENSION + 1,
            1,
            image::Rgba([0, 0, 0, 255]),
        )
        .save_with_format(dir.path().join("huge.png"), image::ImageFormat::Png)
        .unwrap();
        fs::write(dir.path().join("sibling.txt"), b"x").unwrap();
        let entry = FileEntry::from_path(dir.path().join("huge.png")).unwrap();

        let preview = PreviewContext::build(Some(&entry), false);

        assert!(matches!(
            preview,
            PreviewContext::File(FilePreview::TooLarge)
        ));
        // Building a PREVIEW never touches the filesystem beyond the
        // selected entry itself — nothing here could have changed
        // `current_dir` or CURRENT's own listing, but assert the sibling
        // is still exactly what it was as a concrete, non-tautological
        // check.
        assert_eq!(
            fs::read(dir.path().join("sibling.txt")).unwrap(),
            b"x".to_vec()
        );
    }

    #[test]
    fn preview_of_no_selection_is_neutral() {
        let preview = PreviewContext::build(None, false);

        assert!(matches!(preview, PreviewContext::None));
    }

    #[test]
    fn preview_of_a_directory_that_cannot_be_read_is_unavailable_not_a_crash() {
        let dir = TempDir::new();
        let ghost = FileEntry::from_path(dir.path().join("does-not-exist"));
        // The entry itself couldn't even be built (nothing there to stat);
        // simulate the "existed a moment ago, gone now" race by building
        // the context against a path we know `read_directory` will fail
        // on, using a real FileEntry for a directory we then remove.
        assert!(ghost.is_err(), "sanity: path really doesn't exist");

        let removed = dir.path().join("removed");
        fs::create_dir(&removed).unwrap();
        let entry = FileEntry::from_path(removed.clone()).unwrap();
        fs::remove_dir(&removed).unwrap();

        let preview = PreviewContext::build(Some(&entry), false);

        assert!(matches!(preview, PreviewContext::DirectoryUnavailable));
    }
}
