//! CREATE and RENAME — the core M5T-C1 builds: toolkit-neutral, synchronous
//! filesystem mutation, with no `slint::`, no `AppState`, no `Navigation`
//! mutation. This is the first xdir module that changes the filesystem
//! rather than only reading it, so correctness and non-destruction take
//! priority over convenience everywhere below.
//!
//! Both operations share the same non-negotiable rule: never silently
//! replace something that already exists. CREATE uses `O_CREAT | O_EXCL`
//! (via `std::fs::OpenOptions::create_new`/`std::fs::create_dir`, both
//! already exclusive by construction) so a conflict is reported, not
//! papered over. RENAME needs more care — see [`rename_entry`]'s own doc
//! comment for why `std::fs::rename` alone can't make that same guarantee
//! on Linux.

use std::ffi::OsStr;
use std::io;
use std::path::{Component, Path, PathBuf};

/// What [`create_entry`] should create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CreateKind {
    File,
    Directory,
}

/// Creates a new, empty file or directory at `root.join(relative)` — never
/// overwriting, never following a symlink to replace whatever it points
/// at, and never creating missing intermediate directories. Returns the
/// created path on success.
///
/// # `root`
///
/// Must exist and be a directory — the same "is this a real directory"
/// check used throughout `core` (`std::fs::metadata`, which follows a
/// symlink root to its target, exactly like
/// `core::navigation::ensure_is_directory`/`core::find::find_recursive`).
/// Never canonicalized.
///
/// # `relative`
///
/// Must be a relative path with no `..` component — see
/// [`sanitize_relative_path`]. Never interpreted as shell syntax (no `~`,
/// `$VAR`, globs); every component is taken literally. A `.` component is
/// dropped rather than rejected (keeps writing `./name` harmless without
/// requiring it), but at least one real (`Normal`) component must remain
/// afterward, or this is `InvalidInput`. Escaping `root` via `..` is
/// rejected outright; escaping it via a symlinked intermediate directory
/// is not additionally guarded against — xdir is not a sandbox, and a
/// symlinked parent already behaves exactly as Unix always has.
///
/// # `kind`
///
/// `File` uses `OpenOptions::create_new` (`O_CREAT | O_EXCL` on Linux):
/// fails with `AlreadyExists` — never truncates, never follows a symlink
/// destination to replace its target — if anything is already there,
/// including a directory or a symlink (dangling or not). `Directory` uses
/// `std::fs::create_dir` (never `create_dir_all`): fails with
/// `AlreadyExists` if the target exists, and fails (typically `NotFound`)
/// if `relative`'s parent doesn't already exist — no `mkdir -p` here, ever.
///
/// Neither branch does an extra `exists()`/`metadata()` check first: the
/// exclusive-creation primitive is itself the authoritative source of
/// `AlreadyExists`/`NotFound`, so adding one would only be a redundant
/// syscall racing the real one (see [`rename_entry`]'s doc comment for why
/// that pattern is actively wrong, not just wasteful, in the RENAME case).
pub fn create_entry(root: &Path, relative: &Path, kind: CreateKind) -> io::Result<PathBuf> {
    let metadata = std::fs::metadata(root)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        ));
    }

    let sanitized = sanitize_relative_path(relative)?;
    let target = root.join(&sanitized);

    match kind {
        CreateKind::File => {
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&target)?;
        }
        CreateKind::Directory => {
            std::fs::create_dir(&target)?;
        }
    }

    Ok(target)
}

/// Validates `relative` and returns it rebuilt from only its `Normal`
/// components (so a `.` anywhere in the input is silently dropped, never
/// forwarded to the filesystem layer at all). `InvalidInput` for anything
/// that isn't a plain relative path: an absolute path (`RootDir`/`Prefix`),
/// any `ParentDir` (`..`) component — the one and only containment rule
/// this milestone enforces, deliberately not a broader sandbox — or a path
/// that normalizes away to nothing (empty, or `.`/`./.` and the like).
fn sanitize_relative_path(relative: &Path) -> io::Result<PathBuf> {
    let mut sanitized = PathBuf::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => sanitized.push(part),
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} is not a valid relative path", relative.display()),
                ));
            }
        }
    }
    if sanitized.as_os_str().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "relative path has no real component",
        ));
    }
    Ok(sanitized)
}

/// Renames `source` to `new_name` — changing only its basename, within its
/// existing parent directory. Never a move: `new_name` is always exactly
/// one path component (see [`validate_new_name`]), never a path of its
/// own. Returns the renamed path (`source`'s parent joined with
/// `new_name`) on success.
///
/// # Why not `std::fs::rename`
///
/// `std::fs::rename(old, new)` on Linux is `rename(2)`, which *replaces*
/// `new` if it already exists — silently, and atomically, which is exactly
/// the problem: there is no safe way to check "does `new` exist?" first
/// and act on the answer, because another process (or another part of
/// xdir, later) can create it in the gap between the check and the
/// rename — the classic TOCTOU race. A conflict here must never destroy
/// the existing destination, so the check-then-act pattern is not an
/// option regardless of how small the gap is.
///
/// # The atomic no-clobber primitive
///
/// `rustix::fs::renameat_with(.., RenameFlags::NOREPLACE)` wraps Linux's
/// `renameat2(2)` with `RENAME_NOREPLACE`: the kernel itself refuses the
/// rename in one atomic step if the destination already exists, returning
/// `EEXIST` — which `rustix::io::Errno`'s `From` conversion into
/// `std::io::Error` turns into `ErrorKind::AlreadyExists` (the OS's own
/// natural mapping, not one xdir invents). `rustix` was already resolved
/// in `Cargo.lock` at this exact version (transitively, well before this
/// milestone — see `Cargo.toml`'s own comment on the dependency), so this
/// declares it directly rather than reaching for a raw `libc`/manual
/// syscall: the wrapper is entirely safe, so xdir's own code stays at
/// zero `unsafe`. `fs::CWD` (`AT_FDCWD`) is used as both directory file
/// descriptors purely because `renameat2` requires *some* base for a
/// relative path — `source`/the built destination are always absolute in
/// practice (both come from an absolute `root`), so `CWD` is never
/// actually consulted.
///
/// # Symlinks
///
/// `source` is never dereferenced to decide anything — `renameat2` (like
/// `rename(2)`) always operates on the directory entry named by its path,
/// never on whatever a symlink there points at. A symlink, including a
/// dangling one (its target need not exist), renames exactly like any
/// other entry; its target is never read, followed, or touched.
///
/// # No existence pre-check
///
/// Neither `source` nor the destination is `exists()`/`metadata()`-checked
/// before the syscall: `renameat2` is itself the authoritative source for
/// both "source is missing" (`NotFound`) and "destination already exists"
/// (`AlreadyExists`, guaranteed atomic by `NOREPLACE`) — an extra check
/// first would only add a redundant syscall without improving the
/// guarantee (and, for the destination specifically, would reintroduce
/// exactly the TOCTOU gap `NOREPLACE` exists to close).
pub fn rename_entry(source: &Path, new_name: &OsStr) -> io::Result<PathBuf> {
    validate_new_name(new_name)?;

    // Same basename: a deliberate no-op, not an error — but `source` must
    // still be a real entry, same as every other rename. `symlink_metadata`
    // (never `metadata`) exists purely to *prove the entry itself exists*,
    // exactly like `source`'s identity everywhere else in this function:
    // it must not follow the entry if it happens to be a symlink, or a
    // same-name no-op on a dangling symlink (a real, existing entry) would
    // wrongly fail as if `source` were missing. This is the only existence
    // check in the whole function — every other path below reaches
    // `renameat_with`/`RENAME_NOREPLACE` directly, which is itself the
    // authoritative check (see this function's own doc comment); this
    // branch returns before ever reaching it, so nothing else here would
    // otherwise catch a missing `source`.
    if source.file_name() == Some(new_name) {
        std::fs::symlink_metadata(source)?;
        return Ok(source.to_path_buf());
    }

    let Some(parent) = source.parent() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent to rename within", source.display()),
        ));
    };
    let destination = parent.join(new_name);

    rustix::fs::renameat_with(
        rustix::fs::CWD,
        source,
        rustix::fs::CWD,
        destination.as_path(),
        rustix::fs::RenameFlags::NOREPLACE,
    )?;

    Ok(destination)
}

/// Validates that `new_name` is exactly one real path component — never
/// empty, never `.`/`..`, never containing a `/` (which would make it a
/// path of more than one component, i.e. a move in disguise), never an
/// absolute path. The `c == new_name` check (byte-exact `OsStr`
/// comparison, never a lossy/string one) is what catches `new_name`
/// containing a `/` at all: `Path::new(new_name).components()` would then
/// yield either more than one component or a single one whose bytes don't
/// match `new_name` verbatim, either of which this rejects. Leading/
/// trailing spaces are real filename bytes, never trimmed.
fn validate_new_name(new_name: &OsStr) -> io::Result<()> {
    if new_name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "new name is empty",
        ));
    }
    let mut components = Path::new(new_name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(part)), None) if part == new_name => Ok(()),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "new name must be exactly one path component",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    // --- CREATE --------------------------------------------------------

    #[test]
    fn create_file_creates_empty_file() {
        let root = TempDir::new();

        let created =
            create_entry(root.path(), Path::new("new-file.txt"), CreateKind::File).unwrap();

        assert_eq!(created, root.path().join("new-file.txt"));
        assert_eq!(fs::read(&created).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn create_directory_creates_directory() {
        let root = TempDir::new();

        let created =
            create_entry(root.path(), Path::new("new-dir"), CreateKind::Directory).unwrap();

        assert_eq!(created, root.path().join("new-dir"));
        assert!(fs::metadata(&created).unwrap().is_dir());
    }

    #[test]
    fn create_nested_file_in_existing_parent() {
        let root = TempDir::new();
        fs::create_dir(root.path().join("dir")).unwrap();

        let created =
            create_entry(root.path(), Path::new("dir/file.txt"), CreateKind::File).unwrap();

        assert_eq!(created, root.path().join("dir").join("file.txt"));
        assert!(created.is_file());
    }

    #[test]
    fn create_nested_directory_in_existing_parent() {
        let root = TempDir::new();
        fs::create_dir(root.path().join("dir")).unwrap();

        let created =
            create_entry(root.path(), Path::new("dir/subdir"), CreateKind::Directory).unwrap();

        assert_eq!(created, root.path().join("dir").join("subdir"));
        assert!(created.is_dir());
    }

    #[test]
    fn create_does_not_create_missing_parents() {
        let root = TempDir::new();

        let result = create_entry(root.path(), Path::new("missing/file.txt"), CreateKind::File);

        assert!(result.is_err());
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn create_rejects_absolute_path() {
        let root = TempDir::new();

        let result = create_entry(root.path(), Path::new("/tmp/escaped"), CreateKind::File);

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn create_rejects_parent_component() {
        let root = TempDir::new();

        let result = create_entry(
            root.path(),
            Path::new("dir/../../escaped"),
            CreateKind::File,
        );

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn create_rejects_empty_relative_path() {
        let root = TempDir::new();

        let result = create_entry(root.path(), Path::new(""), CreateKind::File);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);

        // "." alone normalizes to nothing real either.
        let result = create_entry(root.path(), Path::new("."), CreateKind::File);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[test]
    fn create_existing_file_returns_already_exists() {
        let root = TempDir::new();
        fs::write(root.path().join("exists.txt"), b"original").unwrap();

        let result = create_entry(root.path(), Path::new("exists.txt"), CreateKind::File);

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn create_existing_directory_returns_already_exists() {
        let root = TempDir::new();
        fs::create_dir(root.path().join("exists")).unwrap();

        let result = create_entry(root.path(), Path::new("exists"), CreateKind::Directory);

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
    }

    #[test]
    fn create_does_not_truncate_existing_file() {
        let root = TempDir::new();
        let path = root.path().join("sentinel.txt");
        fs::write(&path, b"SENTINEL-CONTENT").unwrap();

        let result = create_entry(root.path(), Path::new("sentinel.txt"), CreateKind::File);

        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"SENTINEL-CONTENT");
    }

    #[cfg(unix)]
    #[test]
    fn create_existing_symlink_is_not_replaced() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new();
        let target = root.path().join("target.txt");
        fs::write(&target, b"target-content").unwrap();
        let link = root.path().join("link.txt");
        symlink(&target, &link).unwrap();

        let result = create_entry(root.path(), Path::new("link.txt"), CreateKind::File);

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        // The symlink itself is untouched, still pointing at the same target.
        assert_eq!(fs::read_link(&link).unwrap(), target);
        assert_eq!(fs::read(&target).unwrap(), b"target-content");
    }

    #[test]
    fn create_hidden_name() {
        let root = TempDir::new();

        let created = create_entry(root.path(), Path::new(".hidden"), CreateKind::File).unwrap();

        assert_eq!(created, root.path().join(".hidden"));
        assert!(created.is_file());
    }

    #[test]
    fn create_name_with_spaces() {
        let root = TempDir::new();

        let created = create_entry(
            root.path(),
            Path::new("a name with spaces.txt"),
            CreateKind::File,
        )
        .unwrap();

        assert!(created.is_file());
    }

    #[test]
    fn create_invalid_root_returns_error() {
        let root = TempDir::new();
        let missing = root.path().join("does-not-exist");

        let result = create_entry(&missing, Path::new("file.txt"), CreateKind::File);

        assert!(result.is_err());
    }

    #[test]
    fn create_regular_file_as_root_returns_error() {
        let root = TempDir::new();
        let file = root.path().join("not-a-dir.txt");
        fs::write(&file, b"").unwrap();

        let result = create_entry(&file, Path::new("file.txt"), CreateKind::File);

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
    }

    #[cfg(unix)]
    #[test]
    fn create_non_utf8_name_preserves_identity() {
        use std::os::unix::ffi::OsStrExt;
        let root = TempDir::new();
        let name = OsStr::from_bytes(b"non-utf8-\xFF.txt");

        let created = create_entry(root.path(), Path::new(name), CreateKind::File).unwrap();

        assert_eq!(created.file_name(), Some(name));
        assert!(fs::symlink_metadata(&created).unwrap().is_file());
    }

    // --- RENAME ----------------------------------------------------------

    #[test]
    fn rename_file_changes_basename() {
        let root = TempDir::new();
        let source = root.path().join("old.txt");
        fs::write(&source, b"content").unwrap();

        let renamed = rename_entry(&source, OsStr::new("new.txt")).unwrap();

        assert_eq!(renamed, root.path().join("new.txt"));
        assert!(!source.exists());
        assert_eq!(fs::read(&renamed).unwrap(), b"content");
    }

    #[test]
    fn rename_directory_changes_basename() {
        let root = TempDir::new();
        let source = root.path().join("old-dir");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("inner.txt"), b"inner").unwrap();

        let renamed = rename_entry(&source, OsStr::new("new-dir")).unwrap();

        assert_eq!(renamed, root.path().join("new-dir"));
        assert!(!source.exists());
        assert_eq!(fs::read(renamed.join("inner.txt")).unwrap(), b"inner");
    }

    #[cfg(unix)]
    #[test]
    fn rename_symlink_renames_link_not_target() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new();
        let target = root.path().join("target.txt");
        fs::write(&target, b"target-content").unwrap();
        let link = root.path().join("link.txt");
        symlink(&target, &link).unwrap();

        let renamed = rename_entry(&link, OsStr::new("renamed-link.txt")).unwrap();

        assert_eq!(renamed, root.path().join("renamed-link.txt"));
        // The target itself was never touched.
        assert_eq!(fs::read(&target).unwrap(), b"target-content");
        // The old link entry is gone; the new one is still a symlink,
        // still pointing at exactly the same (never-followed) target.
        assert!(fs::symlink_metadata(&link).is_err());
        assert!(
            fs::symlink_metadata(&renamed)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&renamed).unwrap(), target);
    }

    #[cfg(unix)]
    #[test]
    fn rename_dangling_symlink() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new();
        let missing_target = root.path().join("never-existed");
        let link = root.path().join("dangling.txt");
        symlink(&missing_target, &link).unwrap();

        let renamed = rename_entry(&link, OsStr::new("still-dangling.txt")).unwrap();

        assert_eq!(renamed, root.path().join("still-dangling.txt"));
        assert_eq!(fs::read_link(&renamed).unwrap(), missing_target);
    }

    #[test]
    fn rename_same_name_is_successful_noop() {
        let root = TempDir::new();
        let source = root.path().join("same.txt");
        fs::write(&source, b"content").unwrap();
        let before_mtime = fs::metadata(&source).unwrap().modified().unwrap();

        let renamed = rename_entry(&source, OsStr::new("same.txt")).unwrap();

        assert_eq!(renamed, source);
        assert_eq!(fs::read(&source).unwrap(), b"content");
        // Never touched the filesystem: mtime is bit-for-bit the same.
        assert_eq!(
            fs::metadata(&source).unwrap().modified().unwrap(),
            before_mtime
        );
    }

    /// M5T-C1 V2 audit: the same-basename branch used to skip straight to
    /// `Ok(source)` with no existence check at all, so a same-name rename
    /// of a `source` that doesn't exist wrongly succeeded — never creating
    /// anything, but reporting success for an entry that was never real.
    #[test]
    fn rename_missing_source_same_name_returns_error() {
        let root = TempDir::new();
        let source = root.path().join("missing.txt");

        let result = rename_entry(&source, OsStr::new("missing.txt"));

        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::NotFound);
        // No side effect: the same-name no-op path must never create
        // anything as a byproduct of checking existence.
        assert!(fs::symlink_metadata(&source).is_err());
    }

    /// Pins the exact reason `symlink_metadata` (not `metadata`) is used
    /// for the same-name existence check: a dangling symlink is a real,
    /// existing directory entry even though following it fails, so a
    /// same-name no-op on one must still succeed — and must not disturb
    /// the link or its (still-missing) target in the process.
    #[cfg(unix)]
    #[test]
    fn rename_dangling_symlink_same_name_is_successful_noop() {
        use std::os::unix::fs::symlink;
        let root = TempDir::new();
        let missing_target = root.path().join("target-inexistente");
        let link = root.path().join("link.txt");
        symlink(&missing_target, &link).unwrap();

        let renamed = rename_entry(&link, OsStr::new("link.txt")).unwrap();

        assert_eq!(renamed, link);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert_eq!(fs::read_link(&link).unwrap(), missing_target);
    }

    #[test]
    fn rename_rejects_empty_name() {
        let root = TempDir::new();
        let source = root.path().join("a.txt");
        fs::write(&source, b"").unwrap();

        let result = rename_entry(&source, OsStr::new(""));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(source.exists());
    }

    #[test]
    fn rename_rejects_dot() {
        let root = TempDir::new();
        let source = root.path().join("a.txt");
        fs::write(&source, b"").unwrap();

        let result = rename_entry(&source, OsStr::new("."));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(source.exists());
    }

    #[test]
    fn rename_rejects_dot_dot() {
        let root = TempDir::new();
        let source = root.path().join("a.txt");
        fs::write(&source, b"").unwrap();

        let result = rename_entry(&source, OsStr::new(".."));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(source.exists());
    }

    #[test]
    fn rename_rejects_path_separator() {
        let root = TempDir::new();
        let source = root.path().join("a.txt");
        fs::write(&source, b"").unwrap();

        let result = rename_entry(&source, OsStr::new("a/b"));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(source.exists());
    }

    #[test]
    fn rename_rejects_absolute_name() {
        let root = TempDir::new();
        let source = root.path().join("a.txt");
        fs::write(&source, b"").unwrap();

        let result = rename_entry(&source, OsStr::new("/tmp/escaped.txt"));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
        assert!(source.exists());
    }

    #[test]
    fn rename_missing_source_returns_error() {
        let root = TempDir::new();
        let source = root.path().join("does-not-exist.txt");

        let result = rename_entry(&source, OsStr::new("new-name.txt"));

        assert!(result.is_err());
    }

    /// The explicit, obligatory no-clobber proof: not just "returns an
    /// error", but that *both* sides of the failed rename are provably
    /// untouched byte-for-byte, guaranteed by `renameat2`'s
    /// `RENAME_NOREPLACE` failing the whole operation atomically before it
    /// ever touches either directory entry.
    #[test]
    fn rename_to_existing_file_does_not_overwrite() {
        let root = TempDir::new();
        let source = root.path().join("source.txt");
        let destination = root.path().join("dest.txt");
        fs::write(&source, b"SOURCE").unwrap();
        fs::write(&destination, b"DESTINATION").unwrap();

        let result = rename_entry(&source, OsStr::new("dest.txt"));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).unwrap(), b"SOURCE");
        assert_eq!(fs::read(&destination).unwrap(), b"DESTINATION");
    }

    #[test]
    fn rename_to_existing_directory_does_not_overwrite() {
        let root = TempDir::new();
        let source = root.path().join("source.txt");
        fs::write(&source, b"SOURCE").unwrap();
        let destination = root.path().join("dest-dir");
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("marker.txt"), b"MARKER").unwrap();

        let result = rename_entry(&source, OsStr::new("dest-dir"));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(fs::read(&source).unwrap(), b"SOURCE");
        assert_eq!(fs::read(destination.join("marker.txt")).unwrap(), b"MARKER");
    }

    #[test]
    fn rename_directory_to_existing_directory_does_not_overwrite() {
        let root = TempDir::new();
        let source = root.path().join("source-dir");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("keep.txt"), b"KEEP").unwrap();
        let destination = root.path().join("dest-dir");
        fs::create_dir(&destination).unwrap();

        let result = rename_entry(&source, OsStr::new("dest-dir"));

        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::AlreadyExists);
        assert!(source.is_dir());
        assert_eq!(fs::read(source.join("keep.txt")).unwrap(), b"KEEP");
    }

    #[test]
    fn rename_hidden_name() {
        let root = TempDir::new();
        let source = root.path().join("visible.txt");
        fs::write(&source, b"content").unwrap();

        let renamed = rename_entry(&source, OsStr::new(".hidden")).unwrap();

        assert_eq!(renamed, root.path().join(".hidden"));
        assert_eq!(fs::read(&renamed).unwrap(), b"content");
    }

    #[test]
    fn rename_name_with_spaces() {
        let root = TempDir::new();
        let source = root.path().join("plain.txt");
        fs::write(&source, b"content").unwrap();

        let renamed = rename_entry(&source, OsStr::new("a name with spaces.txt")).unwrap();

        assert_eq!(fs::read(&renamed).unwrap(), b"content");
    }

    #[cfg(unix)]
    #[test]
    fn rename_non_utf8_source() {
        use std::os::unix::ffi::OsStrExt;
        let root = TempDir::new();
        let name = OsStr::from_bytes(b"non-utf8-\xFF.txt");
        let source = root.path().join(name);
        fs::write(&source, b"content").unwrap();

        let renamed = rename_entry(&source, OsStr::new("renamed.txt")).unwrap();

        assert_eq!(renamed, root.path().join("renamed.txt"));
        assert!(fs::symlink_metadata(&source).is_err());
        assert_eq!(fs::read(&renamed).unwrap(), b"content");
    }

    #[cfg(unix)]
    #[test]
    fn rename_to_non_utf8_name_preserves_identity() {
        use std::os::unix::ffi::OsStrExt;
        let root = TempDir::new();
        let source = root.path().join("plain.txt");
        fs::write(&source, b"content").unwrap();
        let new_name = OsStr::from_bytes(b"renamed-\xFF.txt");

        let renamed = rename_entry(&source, new_name).unwrap();

        assert_eq!(renamed.file_name(), Some(new_name));
        assert_eq!(fs::read(&renamed).unwrap(), b"content");
    }
}
