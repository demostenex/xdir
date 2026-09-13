//! Recursive filesystem find — the core M5T-B1 builds: a synchronous, pure
//! traversal with no `slint::`, no thread, no channel, no cancellation.
//! `app`/`ui` (M5T-B2) will decide how — or whether — to keep this off an
//! event loop; this module only has to be correct and small.
//!
//! Semantically distinct from FILTER (`AppState::filter_query`): FILTER is
//! a local, in-memory subset of entries CURRENT already loaded, and never
//! touches the filesystem again. `find_recursive` is a real, bounded
//! traversal starting at a caller-given root — the future `f` binding, not
//! `/`.

use std::io;
use std::path::{Path, PathBuf};

use crate::core::filesystem::sort_entries;
use crate::model::{EntryKind, FileEntry};

/// What one [`find_recursive`] call produced.
///
/// `results` reuses [`FileEntry`] rather than a parallel result DTO:
/// `FileEntry::path()` is already the full path (directories/entries read
/// via a nested `std::fs::read_dir` on a *child* directory already come
/// back with that child folded into `DirEntry::path()`, exactly as
/// `core::filesystem::read_directory` relies on one level up), and
/// `FileEntry::kind()` is exactly the classification find needs — nothing
/// about `FileEntry` assumes "direct child of the currently-open
/// directory", so reusing it here doesn't require a hack.
#[derive(Debug, Clone, Default)]
pub struct FindOutcome {
    /// Matches, in the deterministic order documented on
    /// [`find_recursive`]. Never longer than the `max_results` passed in.
    pub results: Vec<FileEntry>,
    /// `true` exactly when traversal stopped because `results` reached
    /// `max_results` — see [`find_recursive`]'s own doc comment for the
    /// precise meaning (it does not claim there was strictly more to find,
    /// only that the search didn't keep looking after the cap).
    pub truncated: bool,
    /// Directories a `std::fs::read_dir` call failed against during
    /// traversal (permission revoked, removed mid-search, ...) — skipped
    /// entirely rather than failing the whole search. Small, structured
    /// data, not a log: no message, no formatting, just which subtrees
    /// were left unsearched, so a future caller *can* surface that if it
    /// wants to, without this module deciding how.
    pub skipped_errors: Vec<PathBuf>,
}

/// Recursively finds entries under `root` whose basename contains `query`
/// (case-insensitive substring, identical matching semantics to FILTER —
/// see `AppState::visible_indices`), starting at `root`'s own children.
///
/// # Root
///
/// `root` must exist and be a directory (a symlink that resolves to one is
/// accepted — the same rule `core::navigation::ensure_is_directory` already
/// applies to every other "is this path a directory" check in xdir) or
/// this returns `Err` immediately, before any traversal. `root` itself is
/// never a candidate result (see "Results", below) and is never
/// canonicalized — xdir's logical-path philosophy applies here exactly as
/// it does in `core::navigation`.
///
/// # Query
///
/// An empty `query` short-circuits to an empty, non-truncated
/// [`FindOutcome`] *without* reading any directory — an empty substring
/// would match everything, and "list the whole subtree" is never what an
/// empty query means here.
///
/// # `include_hidden`
///
/// `false`: an entry whose basename starts with `.` is excluded from
/// results, and — if it's a directory — never descended into either, in
/// one and the same check (see [`visit_dir`]). `true`: hidden entries can
/// both match and be descended into. Always an explicit parameter, never
/// read from any global/`Navigation` state — the future UI layer owns
/// deciding what to pass (almost certainly `Navigation::show_hidden()`).
///
/// # Symlinks
///
/// A symlink can itself be a result (its own basename is matched exactly
/// like any other entry's), but is never traversed through — recursion
/// only ever happens for an entry whose [`EntryKind`] is `Directory`, and
/// `FileEntry::from_dir_entry` (via `DirEntry::file_type`, which does not
/// follow symlinks) classifies a symlink as `EntryKind::Symlink` even when
/// it points at a directory, never as `Directory`. This is also exactly
/// why a symlink cycle can't loop this traversal: nothing here ever opens
/// a symlink's target, so a link back to an ancestor (or to itself) is
/// just another leaf result, never a path back into the recursion.
///
/// # Results
///
/// `root` itself is never a result — only its descendants are considered.
/// A directory that matches `query` is both included in `results` *and*
/// still descended into (matching doesn't stop traversal). Every
/// [`EntryKind`] can appear in `results` if its basename matches; only
/// `Directory` entries are ever recursed into. File *contents* are never
/// read — matching is basename-only.
///
/// # Ordering
///
/// Deterministic: a pre-order, depth-first walk where each directory's own
/// children are visited in the exact order `core::filesystem::sort_entries`
/// already defines for every other listing in xdir (directories before
/// other kinds, then by raw name ordering within each group) — reused
/// directly, not reimplemented. This is not a sort of full result paths;
/// it falls out of visiting every directory's children in that fixed order
/// and descending immediately when a child is itself a directory.
///
/// # `max_results`
///
/// `0` short-circuits to an empty, non-truncated outcome, exactly like an
/// empty query — no directory is read. Otherwise, traversal stops the
/// moment `results.len()` would exceed `max_results`, and
/// [`FindOutcome::truncated`] is set. The cap is checked before opening
/// each further directory and before considering each further entry, so
/// no `read_dir` call — and no further work at all — happens once the cap
/// is reached.
///
/// # Errors during traversal
///
/// `root` itself failing — it doesn't exist, isn't a directory, or exists
/// as a directory but can't actually be opened by `read_dir` (permission
/// denied, race since the initial check, ...) — is always a hard `Err`
/// from this function; no directory that could not itself be opened ever
/// contributes results silently. Every *descendant* directory is held to a
/// different, deliberately more forgiving standard: once traversal is
/// underway, a child whose `read_dir` fails is recorded in
/// [`FindOutcome::skipped_errors`] and skipped, and the rest of the search
/// continues. Within a directory that *did* open successfully, either an
/// individual `DirEntry` the iterator yields as `Err` (removed mid-read) or
/// a `FileEntry::from_dir_entry` failure for one specific entry (a
/// metadata race on just that entry) marks that directory as partially
/// skipped the same way — still exactly one `skipped_errors` entry per
/// directory no matter how many individual entries inside it failed —
/// while every entry that *did* classify successfully is still used.
///
/// # Performance
///
/// Exactly one `read_dir` call per directory actually traversed, no file
/// ever opened, and `max_results` is consulted before doing any further
/// work — never a `Vec` of the whole subtree before filtering.
pub fn find_recursive(
    root: &Path,
    query: &str,
    include_hidden: bool,
    max_results: usize,
) -> io::Result<FindOutcome> {
    // Root validation only — the same "is this a real directory" check
    // `core::navigation::ensure_is_directory` makes (`std::fs::metadata`,
    // which follows a symlink root to its target), kept local to this
    // module rather than reaching into `navigation` for a private helper
    // (see the milestone's own "don't touch filesystem/navigation
    // foundations" instruction).
    let metadata = std::fs::metadata(root)?;
    if !metadata.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a directory", root.display()),
        ));
    }

    let mut outcome = FindOutcome::default();

    if query.is_empty() || max_results == 0 {
        return Ok(outcome);
    }

    let query_lower = query.to_lowercase();
    // The root call, and only the root call, propagates a `read_dir`
    // failure with `?` — every recursive call `visit_dir` makes on a
    // *child* directory (below) instead catches its own `Err` and folds
    // it into `skipped_errors`. This is the one `visit_dir` doing double
    // duty as both "the root's own traversal step" and "the primitive a
    // child recurses through": which behavior applies is entirely decided
    // by which of the two call sites is running, never by a second
    // `read_dir(root)` call or a separate root-only code path.
    visit_dir(
        root,
        &query_lower,
        include_hidden,
        max_results,
        &mut outcome,
    )?;
    Ok(outcome)
}

/// One directory's worth of work: read it, sort its children exactly like
/// every other xdir listing, then for each child in that order — match it
/// against `query_lower`, and, if it's a real (non-symlink) directory,
/// recurse. `query_lower` is already lowercased once by the caller so this
/// never repeats that work per entry per call.
///
/// Returns `Err` exactly when this directory's own `read_dir` call fails —
/// [`find_recursive`] propagates that `Err` for `root` (via `?`); every
/// recursive call below instead catches it and records the *child's* path
/// in `skipped_errors`, so only the caller's position (root vs. a
/// descendant found during traversal) decides which policy applies, not a
/// second code path.
fn visit_dir(
    dir: &Path,
    query_lower: &str,
    include_hidden: bool,
    max_results: usize,
    outcome: &mut FindOutcome,
) -> io::Result<()> {
    if outcome.results.len() >= max_results {
        outcome.truncated = true;
        return Ok(());
    }

    let read_dir = std::fs::read_dir(dir)?;

    let mut entries = Vec::new();
    // Set at most once per `visit_dir` call, no matter how many
    // individual entries inside `dir` fail — `dir` itself lands in
    // `skipped_errors` at most once either way (see this function's own
    // doc comment).
    let mut dir_partially_skipped = false;
    for dir_entry in read_dir {
        let dir_entry = match dir_entry {
            Ok(dir_entry) => dir_entry,
            Err(_) => {
                // The iterator itself hit an error partway through (e.g.
                // the directory was removed mid-read) — the entries
                // already collected are still real and worth keeping;
                // just note this directory as (partially) skipped rather
                // than discarding everything gathered so far.
                dir_partially_skipped = true;
                continue;
            }
        };
        let Ok(file_entry) = FileEntry::from_dir_entry(&dir_entry) else {
            // A `file_type()`/metadata race on this one entry (removed
            // between being listed and classified) — this entry couldn't
            // be classified at all, so it can neither match nor be
            // recursed into; the directory it came from is reported the
            // same way any other partial read is, not silently dropped.
            dir_partially_skipped = true;
            continue;
        };
        if !include_hidden && file_entry.is_hidden() {
            continue;
        }
        entries.push(file_entry);
    }
    if dir_partially_skipped {
        outcome.skipped_errors.push(dir.to_path_buf());
    }

    sort_entries(&mut entries);

    for entry in entries {
        if outcome.results.len() >= max_results {
            outcome.truncated = true;
            return Ok(());
        }

        if matches_query(&entry, query_lower) {
            outcome.results.push(entry.clone());
            if outcome.results.len() >= max_results {
                outcome.truncated = true;
                return Ok(());
            }
        }

        // Only a real directory is ever descended into — a symlink is
        // always some other `EntryKind` here even when it points at a
        // directory (see this module's and `FileEntry::from_dir_entry`'s
        // own doc comments), so this can never follow one.
        if entry.kind() == EntryKind::Directory {
            // A *child's* `read_dir` failure is never propagated further
            // up — only recorded against that child's own path — so one
            // unreadable subtree can never abort the rest of the search,
            // all the way up to the root call in `find_recursive`.
            if visit_dir(
                entry.path(),
                query_lower,
                include_hidden,
                max_results,
                outcome,
            )
            .is_err()
            {
                outcome.skipped_errors.push(entry.path().to_path_buf());
            }
        }
    }
    Ok(())
}

/// Case-insensitive substring match against the entry's presentation name
/// — `to_string_lossy`, never the raw `OsStr`/`Path` identity, exactly the
/// same rule FILTER already uses (`AppState::visible_indices`). `query`
/// must already be lowercased by the caller.
fn matches_query(entry: &FileEntry, query_lower: &str) -> bool {
    entry
        .name()
        .to_string_lossy()
        .to_lowercase()
        .contains(query_lower)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    fn names(outcome: &FindOutcome) -> Vec<String> {
        outcome
            .results
            .iter()
            .map(|e| e.name().to_string_lossy().into_owned())
            .collect()
    }

    fn paths(outcome: &FindOutcome) -> Vec<PathBuf> {
        outcome
            .results
            .iter()
            .map(|e| e.path().to_path_buf())
            .collect()
    }

    #[test]
    fn empty_query_returns_no_results() {
        let dir = TempDir::new();
        fs::write(dir.path().join("main.rs"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "", false, 100).unwrap();

        assert!(outcome.results.is_empty());
        assert!(!outcome.truncated);
        assert!(outcome.skipped_errors.is_empty());
    }

    #[test]
    fn zero_max_results_returns_no_results() {
        let dir = TempDir::new();
        fs::write(dir.path().join("main.rs"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "main", false, 0).unwrap();

        assert!(outcome.results.is_empty());
        // Reaching the (zero) cap without ever starting a search is not
        // what `truncated` means here — nothing was cut off, there was
        // never an attempt.
        assert!(!outcome.truncated);
    }

    #[test]
    fn finds_matching_file_in_root() {
        let dir = TempDir::new();
        fs::write(dir.path().join("main.rs"), b"").unwrap();
        fs::write(dir.path().join("lib.rs"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "main", false, 100).unwrap();

        assert_eq!(names(&outcome), vec!["main.rs"]);
    }

    #[test]
    fn finds_matching_nested_file() {
        let dir = TempDir::new();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("main.rs"), b"").unwrap();
        let examples = dir.path().join("examples");
        fs::create_dir(&examples).unwrap();
        fs::write(examples.join("main.rs"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "main", false, 100).unwrap();

        assert_eq!(
            paths(&outcome),
            vec![examples.join("main.rs"), src.join("main.rs")]
        );
    }

    #[test]
    fn finds_matching_directory() {
        let dir = TempDir::new();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("main.rs"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "src", false, 100).unwrap();

        assert_eq!(paths(&outcome), vec![src]);
    }

    #[test]
    fn search_is_case_insensitive() {
        let dir = TempDir::new();
        fs::write(dir.path().join("Main.rs"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "MAIN", false, 100).unwrap();

        assert_eq!(names(&outcome), vec!["Main.rs"]);
    }

    #[test]
    fn search_is_substring_based() {
        let dir = TempDir::new();
        fs::write(dir.path().join("MainWindow.rs"), b"").unwrap();
        fs::write(dir.path().join("domain-main-test.txt"), b"").unwrap();
        fs::write(dir.path().join("unrelated.txt"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "main", false, 100).unwrap();

        assert_eq!(
            names(&outcome),
            vec!["MainWindow.rs", "domain-main-test.txt"]
        );
    }

    #[test]
    fn does_not_search_file_contents() {
        let dir = TempDir::new();
        // The name never mentions "needle"; only the content does. A
        // content search would find this — basename-only must not.
        fs::write(dir.path().join("haystack.txt"), b"needle").unwrap();

        let outcome = find_recursive(dir.path(), "needle", false, 100).unwrap();

        assert!(outcome.results.is_empty());
    }

    #[test]
    fn root_itself_is_not_a_result() {
        let dir = TempDir::new();
        let named = dir.path().join("findme");
        fs::create_dir(&named).unwrap();
        fs::write(named.join("inner.txt"), b"").unwrap();

        // Searching *inside* "findme" for "findme": the root itself must
        // never be a candidate, only its descendants.
        let outcome = find_recursive(&named, "findme", false, 100).unwrap();

        assert!(outcome.results.is_empty());
    }

    #[test]
    fn hidden_files_are_skipped_when_disabled() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".hidden-main.rs"), b"").unwrap();
        fs::write(dir.path().join("main.rs"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "main", false, 100).unwrap();

        assert_eq!(names(&outcome), vec!["main.rs"]);
    }

    #[test]
    fn hidden_directories_are_not_traversed_when_disabled() {
        let dir = TempDir::new();
        let hidden = dir.path().join(".hidden");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("main.rs"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "main", false, 100).unwrap();

        assert!(outcome.results.is_empty());
    }

    #[test]
    fn hidden_entries_are_found_when_enabled() {
        let dir = TempDir::new();
        let hidden = dir.path().join(".hidden");
        fs::create_dir(&hidden).unwrap();
        fs::write(hidden.join("main.rs"), b"").unwrap();
        fs::write(dir.path().join(".hidden-main.rs"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "main", true, 100).unwrap();

        // Directories sort before files at each level, and recursion
        // happens immediately on visiting a directory — so `.hidden`
        // (a directory, even though it doesn't itself match) is descended
        // into before `.hidden-main.rs` (a file) is even considered.
        assert_eq!(
            paths(&outcome),
            vec![hidden.join("main.rs"), dir.path().join(".hidden-main.rs")]
        );
    }

    #[cfg(unix)]
    #[test]
    fn symlink_itself_can_match() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let target = dir.path().join("target.txt");
        fs::write(&target, b"").unwrap();
        let link = dir.path().join("main-link");
        symlink(&target, &link).unwrap();

        let outcome = find_recursive(dir.path(), "main", false, 100).unwrap();

        assert_eq!(names(&outcome), vec!["main-link"]);
        assert_eq!(outcome.results[0].kind(), EntryKind::Symlink);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_directory_is_not_traversed() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let real = dir.path().join("real_main_dir");
        fs::create_dir(&real).unwrap();
        fs::write(real.join("inner.rs"), b"").unwrap();
        let link = dir.path().join("linked_main_dir");
        symlink(&real, &link).unwrap();

        let outcome = find_recursive(dir.path(), "inner", false, 100).unwrap();

        // "inner.rs" is only reachable by descending into either the real
        // directory (not named "inner", so this query never visits it
        // directly — it's only found because `real_main_dir` itself is
        // *not* what's being searched for here) — reached only through
        // `real`, never through the symlink.
        assert_eq!(paths(&outcome), vec![real.join("inner.rs")]);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_cycle_does_not_loop() {
        use std::os::unix::fs::symlink;
        let dir = TempDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        // A symlink inside `sub` pointing back at `dir` itself: if
        // symlinked directories were ever followed, this would recurse
        // forever. `find_recursive` must simply treat it as a
        // `Symlink`-kind leaf result and terminate normally.
        let cycle = sub.join("back-to-root");
        symlink(dir.path(), &cycle).unwrap();
        fs::write(sub.join("real.txt"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "", false, 100).unwrap();
        // (empty query short-circuits — just proving no query is required
        // to trigger the hazard this guards against)
        assert!(outcome.results.is_empty());

        let outcome = find_recursive(dir.path(), "back", false, 100).unwrap();
        assert_eq!(names(&outcome), vec!["back-to-root"]);
        assert_eq!(outcome.results[0].kind(), EntryKind::Symlink);
    }

    #[test]
    fn results_are_deterministic() {
        let dir = TempDir::new();
        fs::write(dir.path().join("c_main.txt"), b"").unwrap();
        fs::write(dir.path().join("a_main.txt"), b"").unwrap();
        fs::create_dir(dir.path().join("b_main_dir")).unwrap();

        let first = find_recursive(dir.path(), "main", false, 100).unwrap();
        let second = find_recursive(dir.path(), "main", false, 100).unwrap();

        assert_eq!(paths(&first), paths(&second));
        // Directories before files, then raw-name order within each group
        // — the exact same rule `core::filesystem::sort_entries` applies
        // everywhere else in xdir.
        assert_eq!(
            names(&first),
            vec!["b_main_dir", "a_main.txt", "c_main.txt"]
        );
    }

    #[test]
    fn max_results_is_respected() {
        let dir = TempDir::new();
        for i in 0..5 {
            fs::write(dir.path().join(format!("main-{i}.txt")), b"").unwrap();
        }

        let outcome = find_recursive(dir.path(), "main", false, 2).unwrap();

        assert_eq!(outcome.results.len(), 2);
    }

    #[test]
    fn outcome_reports_truncation() {
        let dir = TempDir::new();
        for i in 0..5 {
            fs::write(dir.path().join(format!("main-{i}.txt")), b"").unwrap();
        }

        let outcome = find_recursive(dir.path(), "main", false, 2).unwrap();
        assert!(outcome.truncated);

        let full = find_recursive(dir.path(), "main", false, 100).unwrap();
        assert!(!full.truncated);
    }

    #[test]
    fn invalid_root_returns_error() {
        let dir = TempDir::new();
        let missing = dir.path().join("does-not-exist");

        let result = find_recursive(&missing, "main", false, 100);

        assert!(result.is_err());
    }

    #[test]
    fn regular_file_as_root_returns_error() {
        let dir = TempDir::new();
        let file = dir.path().join("not-a-dir.txt");
        fs::write(&file, b"").unwrap();

        let result = find_recursive(&file, "main", false, 100);

        assert!(result.is_err());
    }

    /// Distinguishes root policy from child policy (M5T-B1 V2 audit):
    /// `root` exists and passes the directory check (`std::fs::metadata`,
    /// which needs no permission bits on `root` itself, only execute on
    /// its parent components — so this still succeeds), but its own
    /// `read_dir` call fails. That must be a hard `Err` from
    /// `find_recursive`, never silently folded into `skipped_errors` with
    /// an `Ok` empty outcome — the exact bug this test guards against.
    #[cfg(unix)]
    #[test]
    fn unreadable_root_returns_error() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let root = dir.path().join("unreadable-root");
        fs::create_dir(&root).unwrap();
        fs::write(root.join("main.rs"), b"").unwrap();

        let original = fs::metadata(&root).unwrap().permissions();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o000)).unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            find_recursive(&root, "main", false, 100)
        }));
        fs::set_permissions(&root, original).unwrap();
        let result = result.unwrap();

        match result {
            Err(_) => {} // proven: root read_dir failure is a hard `Err`.
            Ok(outcome) => {
                // Running as root (or another context where permission
                // bits don't actually block reads) makes this scenario
                // unreproducible — not a failure of the policy this test
                // exists to prove, just an environment where it can't be
                // observed (mirrors
                // `child_disappearing_or_unreadable_does_not_destroy_whole_search`'s
                // own fallback, below).
                assert_eq!(outcome.results.len(), 1);
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn child_disappearing_or_unreadable_does_not_destroy_whole_search() {
        use std::os::unix::fs::PermissionsExt;
        let dir = TempDir::new();
        let blocked = dir.path().join("blocked");
        fs::create_dir(&blocked).unwrap();
        fs::write(blocked.join("main-inside.txt"), b"").unwrap();
        fs::write(dir.path().join("main-visible.txt"), b"").unwrap();

        let original = fs::metadata(&blocked).unwrap().permissions();
        fs::set_permissions(&blocked, fs::Permissions::from_mode(0o000)).unwrap();

        // Run inside a closure so permissions are always restored (even on
        // panic/assertion failure) — leaving an unreadable directory
        // behind would break this test's own `TempDir` cleanup.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            find_recursive(dir.path(), "main", false, 100).unwrap()
        }));
        fs::set_permissions(&blocked, original).unwrap();
        let outcome = result.unwrap();

        if outcome.skipped_errors.is_empty() {
            // Running as root (or another context where permission bits
            // don't actually block reads) makes this scenario
            // unreproducible — the search then legitimately finds both
            // files, which is not a failure of the policy this test
            // exists to prove, just an environment where it can't be
            // observed.
            assert_eq!(
                paths(&outcome),
                vec![
                    blocked.join("main-inside.txt"),
                    dir.path().join("main-visible.txt")
                ]
            );
        } else {
            assert_eq!(outcome.skipped_errors, vec![blocked.clone()]);
            assert_eq!(paths(&outcome), vec![dir.path().join("main-visible.txt")]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_filename_does_not_panic() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new();
        // 0xFF is never valid UTF-8 on its own.
        let name = OsStr::from_bytes(b"main-\xFF.txt");
        fs::write(dir.path().join(name), b"").unwrap();

        let outcome = find_recursive(dir.path(), "main", false, 100).unwrap();

        assert_eq!(outcome.results.len(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_identity_is_preserved() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = TempDir::new();
        let name = OsStr::from_bytes(b"main-\xFF.txt");
        let expected_path = dir.path().join(name);
        fs::write(&expected_path, b"").unwrap();

        let outcome = find_recursive(dir.path(), "main", false, 100).unwrap();

        // The identity survives byte-for-byte — never lossily reencoded.
        assert_eq!(outcome.results[0].path(), expected_path.as_path());
        assert_eq!(outcome.results[0].name(), name);
    }

    #[test]
    fn directory_match_does_not_prevent_descent() {
        let dir = TempDir::new();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        // Named to also match "src", so a *single* call can prove descent:
        // finding it is only possible by having descended into "src".
        fs::write(src.join("src-inner.rs"), b"").unwrap();

        let outcome = find_recursive(dir.path(), "src", false, 100).unwrap();

        // One call proves all three properties together: "src" itself is a
        // result (a directory match becomes a result), "src-inner.rs" is
        // also a result (only reachable by having descended into "src"
        // despite it already having matched), and the order between them
        // is the same deterministic pre-order every other test relies on.
        assert_eq!(paths(&outcome), vec![src.clone(), src.join("src-inner.rs")]);
    }

    #[cfg(unix)]
    #[test]
    fn other_entry_kind_matching_is_safe() {
        use std::os::unix::net::UnixListener;
        let dir = TempDir::new();
        let socket_path = dir.path().join("main.sock");
        // A bound Unix domain socket creates a real, deterministic
        // non-regular-file, non-directory, non-symlink filesystem entry
        // with `std` alone — no new crate needed to exercise `Other`.
        let _listener = UnixListener::bind(&socket_path).unwrap();

        let outcome = find_recursive(dir.path(), "main", false, 100).unwrap();

        assert_eq!(names(&outcome), vec!["main.sock"]);
        assert_eq!(outcome.results[0].kind(), EntryKind::Other);
    }
}
