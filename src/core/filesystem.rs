use std::io::{self, Read};
use std::path::Path;

use crate::model::{EntryKind, FileEntry};

/// Upper bound on how much of a regular file's content a text preview ever
/// reads. Chosen so PREVIEW can show real content without ever loading an
/// arbitrarily large file into memory.
pub const MAX_TEXT_PREVIEW_BYTES: usize = 64 * 1024;

/// Result of attempting to read a bounded text preview of a regular file.
/// Never carries more than [`MAX_TEXT_PREVIEW_BYTES`] of content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextPreview {
    /// `content` is a valid-UTF-8 prefix of the file, at most
    /// `MAX_TEXT_PREVIEW_BYTES` bytes. `truncated` is `true` when the file
    /// actually had more bytes than that.
    Text { content: String, truncated: bool },
    /// The read prefix contains a NUL byte, or is not valid UTF-8 — treated
    /// as "not a text file" rather than guessed at.
    Unsupported,
}

/// Reads at most `MAX_TEXT_PREVIEW_BYTES + 1` bytes from `path` (the `+ 1`
/// exists only to detect whether the file continues past the limit, and to
/// give the UTF-8 check below real evidence about what's *at* the boundary)
/// and classifies the result. Never reads the whole file, and never reads a
/// single byte past `MAX_TEXT_PREVIEW_BYTES + 1`: `Read::take` bounds every
/// read, regardless of the file's real size. `content` never exceeds
/// `MAX_TEXT_PREVIEW_BYTES` bytes.
///
/// A NUL byte anywhere in the up-to-`MAX_TEXT_PREVIEW_BYTES + 1` bytes
/// actually read is always `Unsupported`.
///
/// The UTF-8 check validates that *whole* buffer — not a pre-emptively
/// truncated `MAX`-byte prefix — before ever cutting anything, so a
/// malformed sequence that straddles the boundary can't be hidden by
/// discarding the one extra byte that would have exposed it:
///
/// - When the whole file fit in the read (`truncated == false`), any UTF-8
///   error is real: the file itself isn't valid UTF-8, full stop.
/// - When we stopped early (`truncated == true`) and the full
///   `MAX_TEXT_PREVIEW_BYTES + 1` buffer is entirely valid UTF-8, the
///   character straddling the `MAX`-byte cut (if any) is dropped whole —
///   found via `str::is_char_boundary`, never by guessing — and the result
///   is `Text` with the remaining, always-complete characters.
/// - When it's *not* entirely valid, `Utf8Error::error_len()` on that same
///   intact buffer says why: `None` means the only problem is an
///   incomplete sequence at the very end (not enough of the `MAX + 1` bytes
///   to finish a character that could still be valid) — a boundary
///   artifact, not evidence of malformation, so it's trimmed back to
///   `Utf8Error::valid_up_to()` and returned as `Text`. `Some(_)` means the
///   bytes we actually read already prove a real encoding error — even one
///   using only the extra byte as its evidence — so the result is
///   `Unsupported`, exactly like malformed bytes anywhere else in the file.
///
/// Never `String::from_utf8_lossy`: every byte kept in `content` was
/// already validated, either by the whole-buffer check or by
/// `valid_up_to()`/`is_char_boundary`.
pub fn read_text_preview(path: &Path) -> io::Result<TextPreview> {
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(MAX_TEXT_PREVIEW_BYTES + 1);
    file.take((MAX_TEXT_PREVIEW_BYTES + 1) as u64)
        .read_to_end(&mut buf)?;

    let truncated = buf.len() > MAX_TEXT_PREVIEW_BYTES;

    if buf.contains(&0u8) {
        return Ok(TextPreview::Unsupported);
    }

    if !truncated {
        return match String::from_utf8(buf) {
            Ok(content) => Ok(TextPreview::Text {
                content,
                truncated: false,
            }),
            Err(_) => Ok(TextPreview::Unsupported),
        };
    }

    // `truncated`: `buf` holds exactly `MAX_TEXT_PREVIEW_BYTES + 1` bytes
    // (the file continues past this). Validate all of it before cutting
    // anything.
    match std::str::from_utf8(&buf) {
        Ok(full) => {
            // Entirely valid UTF-8. Still must not keep more than MAX
            // bytes of content, so back off from MAX to the nearest
            // character boundary at or before it — at most 3 steps, since
            // no UTF-8 character is longer than 4 bytes.
            let mut cut = MAX_TEXT_PREVIEW_BYTES;
            while !full.is_char_boundary(cut) {
                cut -= 1;
            }
            buf.truncate(cut);
            let content = String::from_utf8(buf).expect("cut at a validated char boundary");
            Ok(TextPreview::Text {
                content,
                truncated: true,
            })
        }
        Err(err) if err.error_len().is_none() => {
            // No evidence of malformation in the MAX+1 bytes we actually
            // read: just not enough of them to finish the character that
            // starts at `valid_up_to()`. That position is always <= MAX
            // here (there's at least one further, incomplete byte after it
            // within this MAX+1-byte buffer), so this never needs to keep
            // more than MAX bytes.
            buf.truncate(err.valid_up_to().min(MAX_TEXT_PREVIEW_BYTES));
            let content = String::from_utf8(buf).expect("valid_up_to() is a UTF-8 boundary");
            Ok(TextPreview::Text {
                content,
                truncated: true,
            })
        }
        Err(_) => Ok(TextPreview::Unsupported),
    }
}

/// Lê as entradas de `path`.
///
/// Diretórios vêm antes dos demais tipos de entrada; dentro de cada grupo a
/// ordenação é determinística (por nome). Dotfiles são ocultados a menos
/// que `show_hidden` seja `true`. Não faz travessia recursiva.
pub fn read_directory(path: &Path, show_hidden: bool) -> io::Result<Vec<FileEntry>> {
    let mut entries = Vec::new();

    for dir_entry in std::fs::read_dir(path)? {
        let file_entry = FileEntry::from_dir_entry(&dir_entry?)?;

        if !show_hidden && file_entry.is_hidden() {
            continue;
        }

        entries.push(file_entry);
    }

    sort_entries(&mut entries);

    Ok(entries)
}

/// Sorts `entries` the same way [`read_directory`] always has: directories
/// before everything else, then deterministically by name within each
/// group. Exposed so a caller that builds a `FileEntry` list outside a
/// single `read_directory` call (e.g. splicing in one synthesized entry)
/// can keep the exact same ordering instead of re-deriving it.
pub(crate) fn sort_entries(entries: &mut [FileEntry]) {
    entries.sort_by(|a, b| {
        sort_group(a.kind())
            .cmp(&sort_group(b.kind()))
            .then_with(|| a.name().cmp(b.name()))
    });
}

fn sort_group(kind: EntryKind) -> u8 {
    match kind {
        EntryKind::Directory => 0,
        EntryKind::File | EntryKind::Symlink | EntryKind::Other => 1,
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
            .map(|entry| entry.name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn reads_entries_from_directory() {
        let dir = TempDir::new();
        fs::write(dir.path().join("b.txt"), b"").unwrap();
        fs::write(dir.path().join("a.txt"), b"").unwrap();

        let entries = read_directory(dir.path(), false).unwrap();

        assert_eq!(names(&entries), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn directories_come_before_files() {
        let dir = TempDir::new();
        fs::write(dir.path().join("z_file.txt"), b"").unwrap();
        fs::create_dir(dir.path().join("a_dir")).unwrap();

        let entries = read_directory(dir.path(), false).unwrap();

        assert_eq!(names(&entries), vec!["a_dir", "z_file.txt"]);
    }

    #[test]
    fn ordering_is_deterministic_across_repeated_reads() {
        let dir = TempDir::new();
        fs::write(dir.path().join("c.txt"), b"").unwrap();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        fs::create_dir(dir.path().join("b_dir")).unwrap();

        let first = read_directory(dir.path(), false).unwrap();
        let second = read_directory(dir.path(), false).unwrap();

        assert_eq!(names(&first), names(&second));
        assert_eq!(names(&first), vec!["b_dir", "a.txt", "c.txt"]);
    }

    #[test]
    fn hidden_files_are_excluded_by_default() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".hidden"), b"").unwrap();
        fs::write(dir.path().join("visible.txt"), b"").unwrap();

        let entries = read_directory(dir.path(), false).unwrap();

        assert_eq!(names(&entries), vec!["visible.txt"]);
    }

    #[test]
    fn hidden_files_are_included_when_requested() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".hidden"), b"").unwrap();
        fs::write(dir.path().join("visible.txt"), b"").unwrap();

        let entries = read_directory(dir.path(), true).unwrap();

        assert_eq!(names(&entries), vec![".hidden", "visible.txt"]);
    }

    #[test]
    fn text_preview_of_a_small_utf8_file_returns_its_exact_content() {
        let dir = TempDir::new();
        let path = dir.path().join("small.txt");
        fs::write(&path, "hello, xdir\n").unwrap();

        let preview = read_text_preview(&path).unwrap();

        assert_eq!(
            preview,
            TextPreview::Text {
                content: "hello, xdir\n".to_string(),
                truncated: false,
            }
        );
    }

    #[test]
    fn text_preview_of_an_empty_file_is_valid_empty_text() {
        let dir = TempDir::new();
        let path = dir.path().join("empty.txt");
        fs::write(&path, b"").unwrap();

        let preview = read_text_preview(&path).unwrap();

        assert_eq!(
            preview,
            TextPreview::Text {
                content: String::new(),
                truncated: false,
            }
        );
    }

    #[test]
    fn text_preview_of_a_file_over_the_limit_is_truncated_to_exactly_the_limit() {
        let dir = TempDir::new();
        let path = dir.path().join("large.txt");
        // ASCII-only content, so the byte cutoff can't land mid-codepoint;
        // that case is covered separately by
        // `valid_utf8_multibyte_crossing_preview_boundary_is_truncated_text`.
        let content = "a".repeat(MAX_TEXT_PREVIEW_BYTES + 100);
        fs::write(&path, &content).unwrap();

        let preview = read_text_preview(&path).unwrap();

        match preview {
            TextPreview::Text { content, truncated } => {
                assert_eq!(content.len(), MAX_TEXT_PREVIEW_BYTES);
                assert!(truncated);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn text_preview_of_a_file_exactly_at_the_limit_is_not_truncated() {
        let dir = TempDir::new();
        let path = dir.path().join("exact.txt");
        let content = "b".repeat(MAX_TEXT_PREVIEW_BYTES);
        fs::write(&path, &content).unwrap();

        let preview = read_text_preview(&path).unwrap();

        match preview {
            TextPreview::Text { content, truncated } => {
                assert_eq!(content.len(), MAX_TEXT_PREVIEW_BYTES);
                assert!(!truncated);
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn text_preview_of_a_file_containing_nul_is_unsupported() {
        let dir = TempDir::new();
        let path = dir.path().join("binary.bin");
        fs::write(&path, [b'a', b'b', 0u8, b'c']).unwrap();

        let preview = read_text_preview(&path).unwrap();

        assert_eq!(preview, TextPreview::Unsupported);
    }

    #[test]
    fn text_preview_of_invalid_utf8_is_unsupported() {
        let dir = TempDir::new();
        let path = dir.path().join("invalid-utf8.bin");
        // 0x66, 0x80: "f" followed by a lone continuation byte, never valid
        // UTF-8 on its own.
        fs::write(&path, [0x66, 0x80]).unwrap();

        let preview = read_text_preview(&path).unwrap();

        assert_eq!(preview, TextPreview::Unsupported);
    }

    #[test]
    fn text_preview_of_a_missing_file_is_an_io_error() {
        let dir = TempDir::new();
        let path = dir.path().join("does-not-exist.txt");

        assert!(read_text_preview(&path).is_err());
    }

    #[test]
    fn valid_utf8_multibyte_crossing_preview_boundary_is_truncated_text() {
        let dir = TempDir::new();
        let path = dir.path().join("boundary.txt");
        // "é" (0xC3 0xA9) placed so its two bytes sit at indices MAX-1 and
        // MAX: both bytes are inside the MAX+1-byte read, so the intact
        // buffer is entirely valid UTF-8, but keeping only MAX bytes of
        // content would cut off exactly its second byte — a real UTF-8-
        // valid file whose multibyte character straddles the cutoff.
        let mut content = "a".repeat(MAX_TEXT_PREVIEW_BYTES - 1).into_bytes();
        content.extend_from_slice("é".as_bytes());
        content.extend_from_slice(&"a".repeat(200).into_bytes());
        assert!(content.len() > MAX_TEXT_PREVIEW_BYTES + 1);
        fs::write(&path, &content).unwrap();

        let preview = read_text_preview(&path).unwrap();

        match preview {
            TextPreview::Text { content, truncated } => {
                assert!(truncated);
                // The straddling "é" must be dropped whole, not split: the
                // content is exactly the ASCII prefix before it, and it
                // must itself be valid UTF-8 (already guaranteed by
                // `content` being a `String`) with no partial character.
                assert_eq!(content, "a".repeat(MAX_TEXT_PREVIEW_BYTES - 1));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn valid_continuation_in_extra_byte_does_not_make_valid_file_unsupported() {
        // Same scenario as
        // `valid_utf8_multibyte_crossing_preview_boundary_is_truncated_text`,
        // named to match the specific regression this guards: the extra
        // (MAX+1-th) byte being a *valid* continuation byte must not, by
        // itself, ever cause `Unsupported`.
        let dir = TempDir::new();
        let path = dir.path().join("valid-boundary.txt");
        let mut content = "a".repeat(MAX_TEXT_PREVIEW_BYTES - 1).into_bytes();
        content.extend_from_slice("é".as_bytes());
        content.extend(std::iter::repeat_n(b'a', 200));
        fs::write(&path, &content).unwrap();

        let preview = read_text_preview(&path).unwrap();

        match preview {
            TextPreview::Text { content, truncated } => {
                assert!(truncated);
                assert_eq!(content, "a".repeat(MAX_TEXT_PREVIEW_BYTES - 1));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn invalid_continuation_in_the_extra_byte_is_unsupported() {
        let dir = TempDir::new();
        let path = dir.path().join("invalid-boundary.bin");
        // 0xC3 (lead byte of a 2-byte sequence) at index MAX-1, followed
        // immediately by 'A' (0x41, not a valid continuation byte) as the
        // MAX+1-th byte. The MAX+1-byte read already contains definitive
        // evidence that this sequence is malformed — not merely cut short
        // — so this must be `Unsupported`, not `Text`: discarding the
        // extra byte before validating (the pre-fix behavior) would have
        // hidden this evidence and reported `Text` instead.
        let mut content = "a".repeat(MAX_TEXT_PREVIEW_BYTES - 1).into_bytes();
        content.push(0xC3);
        content.push(b'A');
        content.extend(std::iter::repeat_n(b'a', 200));
        fs::write(&path, &content).unwrap();

        let preview = read_text_preview(&path).unwrap();

        assert_eq!(preview, TextPreview::Unsupported);
    }

    #[test]
    fn four_byte_sequence_crossing_boundary_without_enough_bytes_remains_safe() {
        let dir = TempDir::new();
        let path = dir.path().join("four-byte-boundary.txt");
        // "😀" (F0 9F 98 80, 4 bytes) starting at MAX-1: only its first two
        // bytes (F0 9F) fall inside the MAX+1-byte read. That 2-byte
        // prefix is a valid but incomplete start of a 4-byte sequence —
        // not evidence of malformation — so it must be dropped whole, not
        // reported as `Unsupported`, and never completed by reading past
        // MAX+1.
        let mut content = "a".repeat(MAX_TEXT_PREVIEW_BYTES - 1).into_bytes();
        content.extend_from_slice("😀".as_bytes());
        content.extend(std::iter::repeat_n(b'a', 200));
        fs::write(&path, &content).unwrap();

        let preview = read_text_preview(&path).unwrap();

        match preview {
            TextPreview::Text { content, truncated } => {
                assert!(truncated);
                assert_eq!(content, "a".repeat(MAX_TEXT_PREVIEW_BYTES - 1));
            }
            other => panic!("expected Text, got {other:?}"),
        }
    }

    #[test]
    fn invalid_utf8_before_preview_boundary_remains_unsupported() {
        let dir = TempDir::new();
        let path = dir.path().join("invalid-then-long.bin");
        // A lone continuation byte (0x80) early on is invalid UTF-8
        // regardless of what follows (`error_len() == Some(1)`), not an
        // artifact of where the read stopped — must stay Unsupported even
        // though the file is also long enough to be truncated.
        let mut content = vec![b'a'; 10];
        content.push(0x80);
        content.extend(std::iter::repeat_n(b'a', MAX_TEXT_PREVIEW_BYTES + 200));
        fs::write(&path, &content).unwrap();

        let preview = read_text_preview(&path).unwrap();

        assert_eq!(preview, TextPreview::Unsupported);
    }

    #[test]
    fn multibyte_character_ending_exactly_at_limit_is_not_truncated_due_to_utf8() {
        let dir = TempDir::new();
        let path = dir.path().join("exact-multibyte.txt");
        // "é" is 2 bytes; padded with (MAX - 2) ASCII bytes so the file is
        // exactly MAX_TEXT_PREVIEW_BYTES long and "é" ends precisely at the
        // limit, with nothing beyond it.
        let mut content = "a".repeat(MAX_TEXT_PREVIEW_BYTES - 2).into_bytes();
        content.extend_from_slice("é".as_bytes());
        assert_eq!(content.len(), MAX_TEXT_PREVIEW_BYTES);
        fs::write(&path, &content).unwrap();

        let preview = read_text_preview(&path).unwrap();

        assert_eq!(
            preview,
            TextPreview::Text {
                content: String::from_utf8(content).unwrap(),
                truncated: false,
            }
        );
    }
}
