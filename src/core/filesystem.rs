use std::io;
use std::path::Path;

use crate::model::{EntryKind, FileEntry};

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
}
