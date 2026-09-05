use std::ffi::{OsStr, OsString};
use std::fs::DirEntry;
use std::io;
use std::path::{Path, PathBuf};

/// Classificação de uma entrada do filesystem.
///
/// Symlinks nunca são seguidos apenas para classificação: um symlink é
/// sempre `Symlink`, mesmo que aponte para um diretório.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Directory,
    File,
    Symlink,
    Other,
}

/// Uma entrada de diretório, ainda sem nenhuma informação destinada à UI
/// (MIME, tamanho formatado, ícones, thumbnails, permissões, owner/group).
#[derive(Debug, Clone)]
pub struct FileEntry {
    path: PathBuf,
    name: OsString,
    kind: EntryKind,
}

impl FileEntry {
    /// Constrói uma `FileEntry` a partir de um `DirEntry` do `std`.
    ///
    /// `DirEntry::file_type` não segue symlinks, então um symlink é
    /// classificado como `EntryKind::Symlink` independentemente do que ele
    /// aponta.
    pub(crate) fn from_dir_entry(entry: &DirEntry) -> io::Result<Self> {
        let file_type = entry.file_type()?;

        let kind = if file_type.is_symlink() {
            EntryKind::Symlink
        } else if file_type.is_dir() {
            EntryKind::Directory
        } else if file_type.is_file() {
            EntryKind::File
        } else {
            EntryKind::Other
        };

        Ok(FileEntry {
            path: entry.path(),
            name: entry.file_name(),
            kind,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn name(&self) -> &OsStr {
        &self.name
    }

    pub fn kind(&self) -> EntryKind {
        self.kind
    }

    /// Um arquivo é considerado oculto quando seu nome começa com `.`,
    /// seguindo a convenção Unix. A checagem é feita byte a byte para
    /// funcionar mesmo quando o nome não é UTF-8 válido.
    pub fn is_hidden(&self) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            self.name.as_bytes().first() == Some(&b'.')
        }
        #[cfg(not(unix))]
        {
            self.name.to_string_lossy().starts_with('.')
        }
    }
}
