use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

use crate::app::{Action, ParentContext, PreviewContext};
use crate::core::find::FindOutcome;
use crate::core::navigation::Navigation;
use crate::core::operations::{self, CreateKind};
use crate::core::places::{self, Place, SystemPlaceKind};
use crate::model::{EntryKind, FileEntry};

/// Upper bound this milestone's UI places on a single FIND search — chosen
/// here (the `app`/runtime layer), never inside `core::find`, which
/// deliberately has no opinion of its own about how many results a caller
/// wants (see the M5T-B1 decision note). Not configurable yet; a future
/// milestone's concern.
pub const FIND_MAX_RESULTS: usize = 500;

/// What a `dispatch` call actually changed, so the UI layer never has to
/// infer that by diffing entry counts or any other derived signal —
/// exactly that approach missed a real bug in Milestone 2 (`ToggleHidden`
/// changed the listing without changing `current_dir`), and a
/// same-length-different-content listing would slip past a length-only
/// check just as easily. `AppState` knows precisely which contexts it
/// recomputed on each action, so it just reports that directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Update {
    pub current_changed: bool,
    pub parent_changed: bool,
    pub preview_changed: bool,
}

impl Update {
    const NONE: Update = Update {
        current_changed: false,
        parent_changed: false,
        preview_changed: false,
    };
    const ALL: Update = Update {
        current_changed: true,
        parent_changed: true,
        preview_changed: true,
    };
    const PREVIEW_ONLY: Update = Update {
        current_changed: false,
        parent_changed: false,
        preview_changed: true,
    };

    fn or(self, other: Update) -> Update {
        Update {
            current_changed: self.current_changed || other.current_changed,
            parent_changed: self.parent_changed || other.parent_changed,
            preview_changed: self.preview_changed || other.preview_changed,
        }
    }
}

/// What [`AppState::create_entry`] did. Not `io::Result<Update>`: CREATE
/// isn't reached through `dispatch`/`Action` at all (see that method's own
/// doc comment for why — the same reasoning `complete_find` already
/// documents for FIND), so nothing forces it to share `dispatch`'s
/// `Update`-only return shape. `ui/window.rs` matches on this directly to
/// decide whether to close the editor and what, if anything, to show.
#[derive(Debug)]
pub enum CreateOutcome {
    /// The input was exactly the empty string: no filesystem call was even
    /// attempted. The caller closes the editor exactly as if Esc had been
    /// pressed — this is a deliberate "cancel", never an error.
    Cancelled,
    /// `core::operations::create_entry` succeeded: apply this `Update` and
    /// close the editor.
    Created(Update),
    /// `core::operations::create_entry` failed. Nothing in `AppState` was
    /// touched (the error is returned before any mutation), so the editor,
    /// its draft text, and the filesystem are all exactly as they were —
    /// the caller keeps the editor open and only needs to surface this
    /// message. Already `io::Error::to_string()`, never the `io::Error`
    /// itself, for the same reason [`FindPhase::Error`] stores a `String`
    /// (see its own doc comment): `AppState`'s public surface stays free of
    /// `io::Error` as a stored type.
    Failed(String),
}

/// What [`AppState::rename_selected`] did. Same shape and reasoning as
/// [`CreateOutcome`].
#[derive(Debug)]
pub enum RenameOutcome {
    /// Nothing was selected when this was called. `ui/window.rs` never
    /// actually opens RENAME's editor without a selection (`r` itself
    /// requires one), so this should be unreachable in practice — handled
    /// explicitly anyway rather than silently doing nothing unaccounted-for.
    NoSelection,
    /// `core::operations::rename_entry` succeeded. A same-basename rename
    /// (a deliberate no-op in the core — see `rename_entry`'s own doc
    /// comment) always carries `Update::NONE` here: nothing on disk or in
    /// the listing actually changed, so this never reloads `entries` or
    /// rebuilds PREVIEW for that case.
    Renamed(Update),
    /// `core::operations::rename_entry` failed. Same guarantee as
    /// [`CreateOutcome::Failed`]: nothing in `AppState` was touched, and
    /// `core::operations::rename_entry` itself never mutates the filesystem
    /// on an error path (see its own doc comment) — editor, draft, and the
    /// real file/directory are all left exactly as they were.
    Failed(String),
}

/// FIND's result set once a search completes successfully. A deliberately
/// small, flat snapshot — never re-derived, never re-sorted (`core::find`
/// already returns it in the milestone's frozen deterministic order) —
/// holding exactly what CURRENT/PREVIEW need to render it.
#[derive(Debug, Clone)]
pub struct FindReady {
    /// Real `FileEntry` values straight from `core::find::FindOutcome` —
    /// never rebuilt from a string, never copied into a parallel DTO.
    pub results: Vec<FileEntry>,
    /// Index into `results`, FIND's *own* selection — entirely separate
    /// from `AppState::selected` (the normal directory's selection), which
    /// this never reads or writes. `None` only when `results` is empty.
    pub selected: Option<usize>,
    /// Mirrors `FindOutcome::truncated`: `true` means the search stopped
    /// because it hit `FIND_MAX_RESULTS`, not that another match was ever
    /// proven to exist beyond it.
    pub truncated: bool,
    /// `FindOutcome::skipped_errors.len()` — a count, not the paths
    /// themselves: enough for the status line ("N results · K skipped")
    /// without carrying a `Vec<PathBuf>` nobody in this milestone's UI
    /// reads further.
    pub skipped_count: usize,
}

/// What a `FindSession` is currently doing. Kept as three plain variants —
/// not a `results: Vec`/`error: Option<String>` pair of fields that would
/// let "searching" and "has an error" and "has results" all be
/// (nonsensically) true or absent at once — so a caller matching on it can
/// never observe an incoherent combination.
#[derive(Debug, Clone)]
pub enum FindPhase {
    /// The worker is running; no results exist yet. PREVIEW shows nothing
    /// during this phase (see `AppState::current_preview_source`).
    Searching,
    /// The worker finished without error.
    Ready(FindReady),
    /// The worker's call to `core::find::find_recursive` itself returned
    /// `Err` (an invalid/inaccessible root — `core::find`'s own hard-error
    /// case, not a partially-skipped subtree, which it already folds into
    /// a successful `FindOutcome`). Holds `io::Error::to_string()`, not the
    /// `io::Error` itself — `Action`/`AppState` stay free of `io::Error` as
    /// a stored type, only ever converting it once, right here.
    Error(String),
}

/// One FIND search: the immutable request it was submitted with, plus its
/// current, mutable `phase`. `AppState.find` is `None` whenever FIND isn't
/// active at all — a `FindSession` only ever exists while there is one to
/// show.
#[derive(Debug, Clone)]
pub struct FindSession {
    /// Monotonic per-`AppState` counter (`AppState::next_find_generation`).
    /// The only thing a completion is ever checked against — never the
    /// query string — so two searches for the same text in a row still
    /// can't have a stale one silently mistaken for the current one (see
    /// `AppState::complete_find`).
    generation: u64,
    /// The committed query FIND is (or was) searching for — what a `/`- or
    /// `f`-reopen pre-fills, and what `mode-text` shows alongside "FIND:".
    query: String,
    /// Snapshotted `current_dir` at the moment the search started — never
    /// re-read afterward, so a real navigation that happens to land back on
    /// the same directory later doesn't retroactively change what an
    /// in-flight or completed search was run against.
    root: PathBuf,
    /// Snapshotted `Navigation::show_hidden()` at the same moment, for the
    /// same reason.
    include_hidden: bool,
    phase: FindPhase,
}

impl FindSession {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn phase(&self) -> &FindPhase {
        &self.phase
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn include_hidden(&self) -> bool {
        self.include_hidden
    }
}

/// Toolkit-agnostic application state. This is the source of truth; any UI
/// model (a Slint `ModelRc`, or anything else) is only a projection of it.
///
/// `entries` is always the *full*, unfiltered directory listing — FILTER
/// (M5T-A) never discards or rebuilds it, never re-reads the directory, and
/// never rebuilds a `FileEntry` from a string. `selected`, likewise, always
/// indexes `entries` (the full list), which is what lets a real path stay
/// identified across a filter query changing, being cleared, or `entries`
/// itself being reloaded (`toggle_hidden`) — exactly the same "by real path,
/// never by index or displayed text" discipline `toggle_hidden` already
/// used before FILTER existed. `filter_query` is presentation-layer state
/// only: an empty string means "no filter", and CURRENT's visible subset —
/// [`Self::visible_entries`]/[`Self::visible_selected_index`] — is derived
/// from `(entries, filter_query)` on demand, never cached or stored
/// separately. Public callers that pre-date FILTER (`entries()`,
/// `selected()`) keep returning the full list/full index unchanged; only
/// the UI's CURRENT-pane sync (`ui/window.rs`) reads the `visible_*`
/// projection.
pub struct AppState {
    navigation: Navigation,
    entries: Vec<FileEntry>,
    selected: Option<usize>,
    filter_query: String,
    /// FIND (M5T-B2): `None` whenever FIND is inactive. Deliberately its
    /// own, separate optional session rather than living inside
    /// `entries`/`selected` — see the struct-level doc comment's FILTER
    /// invariants, which FIND must not disturb any more than FILTER did:
    /// `entries` stays the current directory's full listing, `selected`
    /// stays its real index, regardless of whether FIND is showing
    /// something else in CURRENT entirely.
    find: Option<FindSession>,
    /// Monotonic counter handed out as each `FindSession`'s `generation` —
    /// never reset, never reused, incremented once per `start_find` call.
    next_find_generation: u64,
    parent: ParentContext,
    preview: PreviewContext,
    places: Vec<Place>,
}

impl AppState {
    /// Discovers the real system places once, at startup — see
    /// [`Self::with_places`] for why this is a snapshot, never re-read.
    pub fn new(navigation: Navigation) -> Self {
        Self::with_places(navigation, places::system_places())
    }

    /// Builds `AppState` with an explicit, already-resolved `places` list
    /// instead of discovering it from the real `$HOME`/XDG configuration.
    /// This is the seam tests use to exercise `GoSystemPlace` with
    /// deterministic, isolated paths — production always goes through
    /// [`Self::new`], which calls [`places::system_places`]. `places` is a
    /// one-time snapshot taken at construction: XDG configuration is not
    /// live-reloaded, watched, or polled.
    pub(crate) fn with_places(navigation: Navigation, places: Vec<Place>) -> Self {
        let entries = navigation.entries().unwrap_or_default();
        let selected = initial_selection(&entries);
        let parent = ParentContext::build(navigation.current_dir(), navigation.show_hidden());
        let preview = PreviewContext::build(
            selected.and_then(|i| entries.get(i)),
            navigation.show_hidden(),
        );
        AppState {
            navigation,
            entries,
            selected,
            filter_query: String::new(),
            find: None,
            next_find_generation: 0,
            parent,
            preview,
            places,
        }
    }

    pub fn current_dir(&self) -> &Path {
        self.navigation.current_dir()
    }

    pub fn entries(&self) -> &[FileEntry] {
        &self.entries
    }

    pub fn selected(&self) -> Option<usize> {
        self.selected
    }

    pub fn selected_entry(&self) -> Option<&FileEntry> {
        self.selected.and_then(|i| self.entries.get(i))
    }

    pub fn show_hidden(&self) -> bool {
        self.navigation.show_hidden()
    }

    /// FILTER's active query. Empty means no filter is active — never a
    /// distinct state from "no filter", by design (see `dispatch`'s
    /// `ClearFilter` arm and [`Self::visible_indices`]).
    pub fn filter_query(&self) -> &str {
        &self.filter_query
    }

    /// FIND's active session, if any — `None` means FIND isn't showing
    /// anything and CURRENT/PREVIEW should reflect the normal/FILTER view
    /// exactly as before this milestone. `ui/window.rs` is the only reader:
    /// when `Some`, it renders `FindPhase`-appropriate rows/status instead
    /// of [`Self::visible_entries`], and otherwise falls back to it.
    pub fn find_session(&self) -> Option<&FindSession> {
        self.find.as_ref()
    }

    /// The subset of `entries()` that FILTER's current query lets through,
    /// in the same relative order — substring match is never a reorder or
    /// a rank. Identical to `entries()` when no filter is active. This is
    /// what `ui/window.rs` binds CURRENT's row model to; it is always
    /// recomputed from `(entries, filter_query)`, never cached.
    pub fn visible_entries(&self) -> Vec<&FileEntry> {
        self.visible_indices()
            .into_iter()
            .map(|i| &self.entries[i])
            .collect()
    }

    /// `selected`'s position within [`Self::visible_entries`] — what
    /// `ui/window.rs` sets CURRENT's `current-index` to, and what a click
    /// index (reported against that same visible list) is interpreted
    /// against on the way back in. `None` both when nothing is selected and
    /// when the selected entry, for whatever reason, isn't currently
    /// visible (never the case after a `dispatch` returns, per
    /// [`Self::sync_selection_to_filter`], but this stays a lookup rather
    /// than a stored value so it can never drift out of sync with it).
    pub fn visible_selected_index(&self) -> Option<usize> {
        let selected = self.selected?;
        self.visible_indices().iter().position(|&i| i == selected)
    }

    /// Full-list indices whose entry currently matches `filter_query` —
    /// every index, in order, when the query is empty. Case-insensitive
    /// substring match against the entry's display name
    /// (`FileEntry::name()` lossily converted, the exact same presentation
    /// text `ui/window.rs::row_label` shows) — never the `Path`/`OsString`
    /// identity itself, and never a filesystem read: this only ever looks
    /// at `entries`, which is already loaded.
    fn visible_indices(&self) -> Vec<usize> {
        if self.filter_query.is_empty() {
            return (0..self.entries.len()).collect();
        }
        let query = self.filter_query.to_lowercase();
        self.entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry
                    .name()
                    .to_string_lossy()
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(i, _)| i)
            .collect()
    }

    pub fn parent(&self) -> &ParentContext {
        &self.parent
    }

    pub fn preview(&self) -> &PreviewContext {
        &self.preview
    }

    /// Applies `action`, the only way any part of this state changes, and
    /// reports which of CURRENT/PARENT/PREVIEW actually changed.
    pub fn dispatch(&mut self, action: Action) -> Update {
        match action {
            Action::SelectNext => self.select_by(1),
            Action::SelectPrevious => self.select_by(-1),
            Action::SelectIndex(index) => self.select_index(index),
            Action::SelectFirst => self.select_first(),
            Action::SelectLast => self.select_last(),
            Action::ActivateSelected => self.activate_selected(),
            Action::ActivateIndex(index) => {
                // `index` is a position in whatever list CURRENT is
                // currently rendering — FIND's results when FIND is Ready,
                // otherwise the FILTER-visible (or full, if unfiltered)
                // list — exactly what a click reports it against.
                let in_bounds = if let Some(session) = self.find.as_ref() {
                    matches!(&session.phase, FindPhase::Ready(ready) if index < ready.results.len())
                } else {
                    index < self.visible_indices().len()
                };
                if !in_bounds {
                    // A no-op `select_index` still leaves `self.selected`
                    // pointing at whatever was selected before — and
                    // `activate_selected` acts on `self.selected`, not on
                    // `index`. Without this check, an out-of-range index
                    // would silently activate the *previous* selection
                    // instead of doing nothing.
                    Update::NONE
                } else {
                    let selected = self.select_index(index);
                    let activated = self.activate_selected();
                    selected.or(activated)
                }
            }
            Action::GoParent => self.go_parent(),
            Action::GoSystemPlace(kind) => self.go_system_place(kind),
            Action::ToggleHidden => self.toggle_hidden(),
            Action::SetFilterQuery(query) => self.set_filter_query(query),
            Action::ClearFilter => self.clear_filter(),
            Action::StartFind(query) => self.start_find(query),
            Action::ClearFind => self.clear_find(),
            Action::CancelCurrentView => self.cancel_current_view(),
        }
    }

    /// Clamped move by `delta` within whatever CURRENT is rendering right
    /// now: FIND's own results when FIND is `Ready` (never touching the
    /// normal directory's `selected` — see `FindReady::selected`'s doc
    /// comment), otherwise the FILTER-visible (or full) ordering exactly as
    /// before this milestone. A no-op when it doesn't actually change the
    /// selection reports `Update::NONE` rather than `PREVIEW_ONLY`: nothing
    /// changed, so nothing should be re-read.
    fn select_by(&mut self, delta: i32) -> Update {
        if let Some(session) = self.find.as_mut() {
            let changed = match &mut session.phase {
                FindPhase::Ready(ready) => Self::move_find_selection(ready, delta),
                // Searching/Error: nothing to navigate yet.
                _ => false,
            };
            if !changed {
                return Update::NONE;
            }
            self.refresh_preview();
            return Update::PREVIEW_ONLY;
        }

        let visible = self.visible_indices();
        if visible.is_empty() {
            return Update::NONE;
        }
        let current_pos = self
            .selected
            .and_then(|i| visible.iter().position(|&v| v == i))
            .map(|p| p as i32)
            .unwrap_or(-1);
        let last = visible.len() as i32 - 1;
        let next_pos = (current_pos + delta).clamp(0, last) as usize;
        let next = visible[next_pos];
        if self.selected == Some(next) {
            return Update::NONE;
        }
        self.selected = Some(next);
        self.refresh_preview();
        Update::PREVIEW_ONLY
    }

    /// Moves `ready.selected` by `delta`, clamped within `ready.results`.
    /// Returns whether it actually changed. A free function (not a method)
    /// since it only ever touches the `FindReady` it's handed — no access
    /// to `self` needed, and none given, so it can't accidentally reach
    /// past FIND's own selection into the normal one.
    fn move_find_selection(ready: &mut FindReady, delta: i32) -> bool {
        if ready.results.is_empty() {
            return false;
        }
        let last = ready.results.len() as i32 - 1;
        let current = ready.selected.map(|i| i as i32).unwrap_or(-1);
        let next = (current + delta).clamp(0, last) as usize;
        if ready.selected == Some(next) {
            return false;
        }
        ready.selected = Some(next);
        true
    }

    /// Selects the entry at `index` in whatever CURRENT is rendering right
    /// now — FIND's results when FIND is `Ready` (see [`Self::select_by`]),
    /// otherwise a position in the FILTER-visible (or full) list, exactly
    /// what a click reports it against and what `select_first`/
    /// `select_last` pass in. Equal to a full-list index whenever no filter
    /// or FIND is active. A no-op both for an out-of-range index and for
    /// re-selecting the entry that's already selected — the latter matters
    /// because the UI layer's own selection-sync (`sync_selection`)
    /// round-trips through Slint's `current-item-changed` back into this
    /// same call with the index it was just told to set; without this
    /// check that round-trip would rebuild PREVIEW a second time for
    /// nothing.
    fn select_index(&mut self, index: usize) -> Update {
        if let Some(session) = self.find.as_mut() {
            return match &mut session.phase {
                FindPhase::Ready(ready) => {
                    if index >= ready.results.len() || ready.selected == Some(index) {
                        return Update::NONE;
                    }
                    ready.selected = Some(index);
                    self.refresh_preview();
                    Update::PREVIEW_ONLY
                }
                _ => Update::NONE,
            };
        }

        let visible = self.visible_indices();
        let Some(&full_index) = visible.get(index) else {
            return Update::NONE;
        };
        if self.selected == Some(full_index) {
            return Update::NONE;
        }
        self.selected = Some(full_index);
        self.refresh_preview();
        Update::PREVIEW_ONLY
    }

    /// Selects the first entry of whatever's current (`gg`). A no-op —
    /// `Update::NONE`, no preview rebuild — both when nothing is currently
    /// shown and when the first entry is already selected, via the same
    /// re-selection guard as [`Self::select_index`].
    fn select_first(&mut self) -> Update {
        if let Some(session) = self.find.as_ref() {
            return match &session.phase {
                FindPhase::Ready(ready) if !ready.results.is_empty() => self.select_index(0),
                _ => Update::NONE,
            };
        }
        if self.visible_indices().is_empty() {
            Update::NONE
        } else {
            self.select_index(0)
        }
    }

    /// Selects the last entry of whatever's current (`G`). Same guarantees
    /// as [`Self::select_first`], mirrored for the other end.
    fn select_last(&mut self) -> Update {
        if let Some(session) = self.find.as_ref() {
            return match &session.phase {
                FindPhase::Ready(ready) if !ready.results.is_empty() => {
                    self.select_index(ready.results.len() - 1)
                }
                _ => Update::NONE,
            };
        }
        let visible_len = self.visible_indices().len();
        if visible_len == 0 {
            Update::NONE
        } else {
            self.select_index(visible_len - 1)
        }
    }

    /// Navigates to the snapshot [`Place`] of the given kind, if one was
    /// discovered at startup (see [`Self::with_places`]). A `kind` missing
    /// from the snapshot (not configured, or deduplicated away by an
    /// earlier place sharing its path), a target already equal to the
    /// current directory, or a target that no longer resolves to a
    /// directory (e.g. removed since discovery) are all safe no-ops —
    /// `current_dir`/CURRENT are left completely untouched, exactly like
    /// any other failed navigation.
    fn go_system_place(&mut self, kind: SystemPlaceKind) -> Update {
        let Some(place) = self.places.iter().find(|place| place.kind() == kind) else {
            return Update::NONE;
        };
        if place.path() == self.navigation.current_dir() {
            return Update::NONE;
        }
        let target = place.path().to_path_buf();
        if self.navigation.navigate_to(&target).is_ok() {
            self.reload();
            Update::ALL
        } else {
            Update::NONE
        }
    }

    /// Activates the selected entry. Only directories (or symlinks that
    /// resolve to one) cause navigation; activating a regular file is a
    /// deliberate no-op in this milestone (openers arrive later).
    /// Activates whatever's currently selected. When FIND is `Ready`, a
    /// selected `Directory` result navigates there (and, via
    /// [`Self::reload`], clears both FIND and FILTER — a real directory
    /// change always does); any other kind (`File`/`Symlink`/`Other`) is a
    /// deliberate no-op — openers belong to a future milestone. A failed
    /// navigation leaves FIND (and `current_dir`) completely untouched, the
    /// same "no side effect on failure" rule every other navigation here
    /// already follows.
    fn activate_selected(&mut self) -> Update {
        if let Some(session) = self.find.as_ref() {
            return match &session.phase {
                FindPhase::Ready(ready) => {
                    let Some(entry) = ready.selected.and_then(|i| ready.results.get(i)) else {
                        return Update::NONE;
                    };
                    if entry.kind() != EntryKind::Directory {
                        return Update::NONE;
                    }
                    let target = entry.path().to_path_buf();
                    if self.navigation.navigate_to(&target).is_ok() {
                        self.reload();
                        Update::ALL
                    } else {
                        Update::NONE
                    }
                }
                _ => Update::NONE,
            };
        }

        let Some(target) = self
            .selected_entry()
            .map(|entry| entry.path().to_path_buf())
        else {
            return Update::NONE;
        };
        if self.navigation.navigate_to(&target).is_ok() {
            self.reload();
            Update::ALL
        } else {
            Update::NONE
        }
    }

    fn go_parent(&mut self) -> Update {
        if self.navigation.go_to_parent().is_ok() {
            self.reload();
            Update::ALL
        } else {
            Update::NONE
        }
    }

    /// Reloads the listing for a directory that actually changed —
    /// `activate_selected`/`go_parent`/`go_system_place` all call this only
    /// after `Navigation` confirms the target really is a new
    /// `current_dir`, never on a failed or same-directory navigation
    /// (those return `Update::NONE` before ever reaching here). FILTER and
    /// FIND (M5T-B2) are both scoped to one directory's listing, so both
    /// are cleared unconditionally here: a new directory never inherits the
    /// previous one's query or search — `self.find = None` also means any
    /// worker still running for the old directory reports back to a
    /// generation that no longer exists, so `complete_find` discards it
    /// (see its own doc comment).
    fn reload(&mut self) {
        self.entries = self.navigation.entries().unwrap_or_default();
        self.selected = initial_selection(&self.entries);
        self.filter_query.clear();
        self.find = None;
        self.refresh_contexts();
    }

    /// Flips `show_hidden` and reloads the current directory. Unlike
    /// [`Self::reload`] (used when the directory itself changes, where
    /// resetting to the first entry is the only sensible choice), this
    /// keeps the same *entry* selected across the reload by its real path
    /// — never by the index or by the text shown in the UI — falling back
    /// to the first entry only if the previously selected one is no longer
    /// listed (e.g. a dotfile that just got hidden again), or to no
    /// selection if the directory is now empty.
    fn toggle_hidden(&mut self) -> Update {
        // `.` never reruns FIND automatically (that would couple a plain
        // Action to the worker/generation scheduler) — clearing it here
        // both drops any results computed under the old `show_hidden` and
        // invalidates a still-running search's completion (see
        // `complete_find`'s generation check). The user can press `f`
        // again afterward.
        self.find = None;

        let selected_path = self
            .selected_entry()
            .map(|entry| entry.path().to_path_buf());

        self.navigation
            .set_show_hidden(!self.navigation.show_hidden());
        self.entries = self.navigation.entries().unwrap_or_default();

        self.selected = selected_path
            .and_then(|path| {
                self.entries
                    .iter()
                    .position(|entry| entry.path() == path.as_path())
            })
            .or_else(|| initial_selection(&self.entries));
        // `.` never touches `current_dir`, so — unlike `reload` — FILTER's
        // query is kept exactly as-is and simply reapplied over the
        // refreshed `entries`. The path-preserving pick above can still
        // land on an entry the active query filters out (e.g. a dotfile
        // that just became visible again but doesn't match); this clamps
        // it to the first still-visible entry instead, same as any other
        // filter-visibility change.
        self.clamp_selection_to_filter();

        self.refresh_contexts();
        Update::ALL
    }

    /// The one rule for "does `selected` still make sense against the
    /// current filter", shared by every place that needs to re-settle it: a
    /// query edit ([`Self::sync_selection_to_filter`]), a query being
    /// cleared ([`Self::clear_filter`] — clearing is really just "filter
    /// changed to empty", so the exact same rule applies), and `entries`
    /// itself changing under an unchanged query
    /// ([`Self::clamp_selection_to_filter`], from `toggle_hidden`).
    ///
    /// "Still visible" and "no selection at all" are treated as exactly the
    /// same case on purpose — both mean "nothing to preserve" — which is
    /// the fix for a V1 bug: `clamp_selection_to_filter` used to skip doing
    /// anything at all when `selected` was already `None`, on the
    /// unstated assumption that it could never be `None` there. It *can*:
    /// `toggle_hidden`'s own upstream fallback happens to always land on
    /// `Some` when `entries` isn't empty, which quietly worked around the
    /// gap, but that made the gap invisible rather than closing it, one
    /// call site relying on another's incidental behavior instead of its
    /// own contract. Falling to `visible.first()` in both the "invisible"
    /// and "unset" cases removes that hidden coupling — and is also
    /// exactly the fix V1's `clear_filter` needed: clearing a zero-match
    /// filter now lands on the first full entry (deterministic, no
    /// selection-history state) instead of leaving `selected` stuck at
    /// `None` with a non-empty listing behind it.
    ///
    /// Returns whether `selected` actually changed, so each caller reports
    /// `preview_changed` precisely instead of always rebuilding it.
    fn resettle_selection_to_filter(&mut self) -> bool {
        let visible = self.visible_indices();
        let still_visible = self.selected.is_some_and(|i| visible.contains(&i));
        if still_visible {
            return false;
        }
        let next = visible.first().copied();
        let changed = next != self.selected;
        self.selected = next;
        changed
    }

    /// If `selected` no longer makes sense against the active filter —
    /// invisible, or unset while a visible entry now exists (see
    /// [`Self::resettle_selection_to_filter`]) — moves it to the first
    /// visible entry, or to no selection if none are visible. A no-op
    /// whenever `selected` is already visible — in particular always a
    /// no-op when no filter is active, since every entry is then visible
    /// by definition. `toggle_hidden` always rebuilds PREVIEW right after
    /// this regardless (`refresh_contexts`, unconditional on `Update::ALL`),
    /// so unlike its siblings below this doesn't need to report whether
    /// selection changed.
    fn clamp_selection_to_filter(&mut self) {
        self.resettle_selection_to_filter();
    }

    /// Replaces FILTER's active query outright and re-derives selection
    /// from it: the previously selected entry stays selected if the new
    /// query still lets it through, otherwise selection falls to the first
    /// still-visible entry, or to none if nothing matches (see
    /// [`Self::resettle_selection_to_filter`]). Dispatched on every
    /// keystroke while FILTER is being edited — the query already filters
    /// CURRENT live, so committing (Enter) never needs to call back into
    /// `AppState` at all.
    fn set_filter_query(&mut self, query: String) -> Update {
        self.filter_query = query;
        let changed = self.resettle_selection_to_filter();
        if changed {
            self.refresh_preview();
        }
        Update {
            current_changed: true,
            parent_changed: false,
            preview_changed: changed,
        }
    }

    /// Clears FILTER's query back to empty. `selected` is preserved exactly
    /// when it's already a real, visible entry (every entry is visible once
    /// the query is empty, so this is a no-op whenever anything at all was
    /// selected going in) — restored to the first full entry only in the
    /// one case that has nothing to preserve: a zero-match query had
    /// already reset `selected` to `None` (see
    /// [`Self::resettle_selection_to_filter`]) while `entries` itself is
    /// non-empty. No selection *history* is kept — this is the smallest
    /// rule that never leaves a non-empty listing with nothing selected.
    fn clear_filter(&mut self) -> Update {
        if self.filter_query.is_empty() {
            return Update::NONE;
        }
        self.filter_query.clear();
        let changed = self.resettle_selection_to_filter();
        if changed {
            self.refresh_preview();
        }
        Update {
            current_changed: true,
            parent_changed: false,
            preview_changed: changed,
        }
    }

    /// Starts a new FIND search for `query` (never called with an empty
    /// one — `ui/window.rs` branches to [`Self::clear_find`] instead; see
    /// `Action::StartFind`'s own doc comment). Snapshots `root`/
    /// `include_hidden` from `current_dir`/`show_hidden` right now, hands
    /// out a fresh generation, and sets the phase to `Searching` — this
    /// method never touches the filesystem itself and never blocks;
    /// actually running `core::find::find_recursive` off the UI thread and
    /// eventually calling [`Self::complete_find`] back is `ui/window.rs`'s
    /// job (see that module's own doc comments for why the boundary sits
    /// exactly there).
    fn start_find(&mut self, query: String) -> Update {
        let previous_preview_source = self.preview_source_identity();
        self.next_find_generation += 1;
        self.find = Some(FindSession {
            generation: self.next_find_generation,
            query,
            root: self.navigation.current_dir().to_path_buf(),
            include_hidden: self.navigation.show_hidden(),
            phase: FindPhase::Searching,
        });
        // `Searching` always has a `None` preview source (see
        // `current_preview_source`) — but whether that's actually a
        // *change* depends on what was showing a moment ago: nothing, if
        // CURRENT already had no selection, or a real entry, if it did.
        // See `finish_preview_transition`'s own doc comment for why this
        // is a path-identity comparison, never a presentation/`Debug` one.
        let preview_changed = self.finish_preview_transition(previous_preview_source);
        Update {
            current_changed: true,
            parent_changed: false,
            preview_changed,
        }
    }

    /// Clears FIND back to inactive. A no-op when it already is. Otherwise
    /// this is also what invalidates any search still in flight: once
    /// `self.find` is `None`, [`Self::complete_find`] has no session left
    /// to match a generation against, so a completion that arrives later
    /// is silently discarded (`Update::NONE`) — never applied, never
    /// causing a panic. PREVIEW goes back to the normal/FILTER selection
    /// (`current_preview_source` falls through to `selected_entry` the
    /// instant `self.find` is `None`) — but only actually rebuilds, and
    /// only reports `preview_changed`, when that's a different entry than
    /// FIND was just showing (see `finish_preview_transition`).
    fn clear_find(&mut self) -> Update {
        if self.find.is_none() {
            return Update::NONE;
        }
        let previous_preview_source = self.preview_source_identity();
        self.find = None;
        let preview_changed = self.finish_preview_transition(previous_preview_source);
        Update {
            current_changed: true,
            parent_changed: false,
            preview_changed,
        }
    }

    /// `Esc` with no editor focused (`Action::CancelCurrentView`): cancels
    /// whichever transient view is currently on top. FIND, if active, takes
    /// priority over FILTER — this is the one and only priority rule, not
    /// a general "modes" stack, since only these two transient views exist.
    fn cancel_current_view(&mut self) -> Update {
        if self.find.is_some() {
            self.clear_find()
        } else {
            self.clear_filter()
        }
    }

    /// Creates a new file or (if `raw_input` ends with `/`) directory inside
    /// `current_dir`, from CREATE's editor text — called from
    /// `ui/window.rs`'s `on_create_accepted`, never from a `dispatch`/
    /// `Action` path: nothing about running `core::operations::create_entry`
    /// is a plain synchronous keystroke transition the way every `Action`
    /// variant is, and forcing `dispatch` itself to return `io::Result`
    /// would mean every other, purely in-memory `Action` carrying that
    /// shape too for a single caller's benefit. `raw_input` is used exactly
    /// as typed — never trimmed — so a name with meaningful leading/
    /// trailing spaces is preserved verbatim; only a *literally* empty
    /// string is special-cased as [`CreateOutcome::Cancelled`] before any
    /// filesystem call is even attempted. Available only while CREATE's
    /// editor is open, which `ui/window.rs` itself never opens while FIND
    /// is active (see `begin_create_edit`) — this method has no FIND
    /// awareness of its own and does not need any: it only ever touches
    /// `entries`/`selected`/PREVIEW, exactly like `toggle_hidden`.
    pub fn create_entry(&mut self, raw_input: &str) -> CreateOutcome {
        if raw_input.is_empty() {
            return CreateOutcome::Cancelled;
        }
        let kind = if raw_input.ends_with('/') {
            CreateKind::Directory
        } else {
            CreateKind::File
        };
        let relative = Path::new(raw_input);
        match operations::create_entry(self.navigation.current_dir(), relative, kind) {
            Ok(created_path) => {
                let previous_preview_source = self.preview_source_identity();
                // M5T-C2 V2 audit fix #1: identity alone (compared below by
                // `finish_preview_transition`) misses the one case where
                // PREVIEW's *source* doesn't change at all but its
                // *content* does — creating something directly inside the
                // very directory PREVIEW is already showing. Read *before*
                // `entries`/`selected` are touched: `created_path`'s parent
                // is compared against the currently-previewed directory's
                // own path, both real `PathBuf`s, never a presentation
                // string. `EntryKind::Directory` is required (not, say,
                // `Symlink`-to-directory) because that's exactly what
                // `PreviewContext::Directory` itself requires to have
                // listed `created_path`'s parent's children in the first
                // place.
                let created_inside_previewed_directory = matches!(
                    self.current_preview_source(),
                    Some(entry)
                        if entry.kind() == EntryKind::Directory
                            && created_path.parent() == Some(entry.path())
                );
                // M5T-C2 V3 audit fix: captured *before* `entries`/
                // `selected` are touched, by real path identity — never an
                // index (reload/sort can renumber it) and never a
                // presentation string/basename. When `created_path` is a
                // *nested* path (`dir/new.txt`), it is never itself a
                // top-level entry of `current_dir`, so the lookup just
                // below always misses for it; without this fallback the
                // selection then fell straight through to
                // `initial_selection` — jumping to the first entry for no
                // reason, even though nothing about the top-level listing
                // (or what should stay selected in it) actually changed.
                let previous_selected_path = self
                    .selected_entry()
                    .map(|entry| entry.path().to_path_buf());
                self.entries = self.navigation.entries().unwrap_or_default();
                self.selected = self
                    .entries
                    .iter()
                    .position(|entry| entry.path() == created_path.as_path())
                    .or_else(|| {
                        // Only reached when `created_path` isn't itself a
                        // top-level entry (a nested create) — a direct
                        // create always resolves in the branch above and
                        // never falls through to preserving the *previous*
                        // selection instead.
                        previous_selected_path.as_ref().and_then(|path| {
                            self.entries
                                .iter()
                                .position(|entry| entry.path() == path.as_path())
                        })
                    })
                    .or_else(|| initial_selection(&self.entries));
                // FILTER stays authoritative over visibility regardless of
                // which of the three branches above `selected` came from —
                // a preserved-but-now-filtered-out selection is resettled
                // exactly like any other, same as before this fix.
                self.clamp_selection_to_filter();
                // Only one of these two ever actually rebuilds PREVIEW —
                // never both: `finish_preview_transition` already does so
                // exactly when identity moved, so the explicit
                // `refresh_preview()` below only runs in the one case it
                // provably didn't (`created_inside_previewed_directory`,
                // computed above from the *pre*-reload source, is only
                // trusted when identity turns out to still be exactly what
                // it was).
                let preview_changed = if self.finish_preview_transition(previous_preview_source) {
                    true
                } else if created_inside_previewed_directory {
                    self.refresh_preview();
                    true
                } else {
                    false
                };
                CreateOutcome::Created(Update {
                    current_changed: true,
                    // Creating a new entry inside `current_dir` never
                    // changes what PARENT shows (its own parent's listing,
                    // highlighting `current_dir` itself) — same reasoning
                    // `select_by`/`set_filter_query` already document.
                    parent_changed: false,
                    preview_changed,
                })
            }
            Err(err) => CreateOutcome::Failed(err.to_string()),
        }
    }

    /// Renames the currently selected entry's basename to `raw_input`, from
    /// RENAME's editor text — called from `ui/window.rs`'s
    /// `on_rename_accepted`. Same non-`Action` reasoning as
    /// [`Self::create_entry`]. `raw_input` is used exactly as typed, with no
    /// trimming — `core::operations::rename_entry`'s own
    /// `validate_new_name` is the sole judge of whether it's a valid single
    /// path component. `ui/window.rs` never opens RENAME's editor without a
    /// selection or with a non-UTF-8 basename to prefill (see
    /// `rename_prefill_name`), so [`RenameOutcome::NoSelection`] should be
    /// unreachable in practice; it is still handled explicitly rather than
    /// assumed away.
    pub fn rename_selected(&mut self, raw_input: &str) -> RenameOutcome {
        let Some(entry) = self.selected_entry() else {
            return RenameOutcome::NoSelection;
        };
        let source = entry.path().to_path_buf();
        match operations::rename_entry(&source, OsStr::new(raw_input)) {
            Ok(renamed_path) => {
                if renamed_path == source {
                    // Same-basename success: `core::operations::rename_entry`
                    // never touched the filesystem for this branch (see its
                    // own doc comment), so there is nothing here to reload
                    // either — no reload, no selection change, no PREVIEW
                    // rebuild, and so `Update::NONE`, not merely
                    // `preview_changed: false`.
                    return RenameOutcome::Renamed(Update::NONE);
                }
                let previous_preview_source = self.preview_source_identity();
                self.entries = self.navigation.entries().unwrap_or_default();
                self.selected = self
                    .entries
                    .iter()
                    .position(|entry| entry.path() == renamed_path.as_path())
                    .or_else(|| initial_selection(&self.entries));
                self.clamp_selection_to_filter();
                let preview_changed = self.finish_preview_transition(previous_preview_source);
                RenameOutcome::Renamed(Update {
                    current_changed: true,
                    // Renaming stays within `current_dir`'s own listing —
                    // never changes what PARENT shows, same reasoning as
                    // `create_entry` above.
                    parent_changed: false,
                    preview_changed,
                })
            }
            Err(err) => RenameOutcome::Failed(err.to_string()),
        }
    }

    /// Applies a FIND worker's result — called from `ui/window.rs` once a
    /// completion has crossed back onto the UI thread (never from the
    /// worker thread itself; see that module's doc comments). Not an
    /// `Action`: nothing about this is a user input, and stuffing an
    /// `io::Result` through the same enum every keystroke/click also flows
    /// through would mean `Action` (and everything that matches on it)
    /// carrying a variant no real input ever produces.
    ///
    /// `generation` is checked against the *current* session's — by id,
    /// never by comparing query strings, so two back-to-back searches for
    /// the same text still can't have a stale completion mistaken for the
    /// live one. Three ways a completion is stale, all reported the same
    /// (`Update::NONE`, `self.find` left exactly as it is): FIND was
    /// cleared while this search was running (`self.find` is now `None`
    /// entirely), a newer search superseded it (`self.find`'s generation
    /// moved on), or `.` was pressed meanwhile (also clears `self.find`,
    /// covered by the first case).
    pub fn complete_find(&mut self, generation: u64, result: io::Result<FindOutcome>) -> Update {
        match self.find.as_ref() {
            Some(session) if session.generation == generation => {}
            _ => return Update::NONE,
        }
        // Read before mutating `self.find.phase` below — `Searching`'s
        // preview source is always `None` (see `current_preview_source`),
        // so this is really just documenting "nothing was showing yet",
        // but going through the same snapshot-then-compare helper as
        // `start_find`/`clear_find` keeps all three transitions provably
        // consistent rather than special-casing this one.
        let previous_preview_source = self.preview_source_identity();
        let session = self.find.as_mut().expect("checked above");
        session.phase = match result {
            Ok(outcome) => FindPhase::Ready(FindReady {
                selected: if outcome.results.is_empty() {
                    None
                } else {
                    Some(0)
                },
                results: outcome.results,
                truncated: outcome.truncated,
                skipped_count: outcome.skipped_errors.len(),
            }),
            Err(err) => FindPhase::Error(err.to_string()),
        };
        // A real change only when landing on `Ready` with a first result
        // (`None` -> `Some(path)`) — zero results or an `Error` both leave
        // the preview source at `None`, same as `Searching`, so
        // `finish_preview_transition` correctly reports no change and
        // skips rebuilding `PreviewContext` for nothing.
        let preview_changed = self.finish_preview_transition(previous_preview_source);
        Update {
            current_changed: true,
            parent_changed: false,
            preview_changed,
        }
    }

    /// Rebuilds both PARENT and PREVIEW. Used whenever `current_dir` or
    /// `show_hidden` changed — anything that could move PARENT's target
    /// necessarily also invalidates PREVIEW, since PREVIEW's target
    /// (`selected_entry`) is itself relative to `current_dir`.
    fn refresh_contexts(&mut self) {
        self.parent =
            ParentContext::build(self.navigation.current_dir(), self.navigation.show_hidden());
        self.refresh_preview();
    }

    /// Rebuilds only PREVIEW. Used on a plain selection move, so a `j`/`k`
    /// press never re-reads PARENT's listing for no reason.
    fn refresh_preview(&mut self) {
        self.preview =
            PreviewContext::build(self.current_preview_source(), self.navigation.show_hidden());
    }

    /// The entry PREVIEW should currently reflect: FIND's selected result
    /// while FIND is `Ready`, nothing at all while it's `Searching`/
    /// `Error` (there is no meaningful "selected result" yet), and the
    /// normal/FILTER selection otherwise — exactly `Self::selected_entry`,
    /// unaffected by FIND either way. This is the one seam that lets
    /// [`Self::refresh_preview`] stay a single call site: every action that
    /// changes what should be previewed (a FIND selection move, a
    /// completion arriving, FIND being cleared, a plain `j`/`k`, ...) just
    /// calls it, and this decides what "the selection" means right now.
    fn current_preview_source(&self) -> Option<&FileEntry> {
        match self.find.as_ref().map(|session| &session.phase) {
            Some(FindPhase::Ready(ready)) => ready.selected.and_then(|i| ready.results.get(i)),
            Some(FindPhase::Searching) | Some(FindPhase::Error(_)) => None,
            None => self.selected_entry(),
        }
    }

    /// [`Self::current_preview_source`]'s *identity* — `entry.path()`,
    /// never a presentation string, `Debug` output, or basename — as an
    /// owned, borrow-free snapshot a caller can take before mutating
    /// `self.find` and still compare afterward. `None` and `None` compare
    /// equal regardless of *why* there's no source (no selection at all
    /// vs. `Searching`/`Error`), which is exactly the M5T-B2 V2 audit's
    /// point: sameness of identity is all that should ever decide
    /// `preview_changed`, never which phase produced it.
    fn preview_source_identity(&self) -> Option<PathBuf> {
        self.current_preview_source()
            .map(|entry| entry.path().to_path_buf())
    }

    /// Rebuilds PREVIEW only if its source's identity actually moved away
    /// from `previous` (a snapshot the caller took via
    /// [`Self::preview_source_identity`] *before* changing `self.find`),
    /// and returns whether it did — exactly the `preview_changed` value
    /// [`Self::start_find`]/[`Self::complete_find`]/[`Self::clear_find`]
    /// each report. Shared by all three because none of them can tell
    /// up front whether the source moved (unlike FILTER's own edits,
    /// which already know via `resettle_selection_to_filter`'s return
    /// value) — comparing by `PathBuf` identity before/after is the
    /// smallest correct way to find out, and reusing one helper for it
    /// means the three transitions can't quietly drift into comparing it
    /// three different ways. Valid here specifically because none of
    /// these three transitions ever change `show_hidden` — the other
    /// input `PreviewContext::build` takes — so identity alone is
    /// sufficient, not just convenient.
    fn finish_preview_transition(&mut self, previous: Option<PathBuf>) -> bool {
        let changed = previous != self.preview_source_identity();
        if changed {
            self.refresh_preview();
        }
        changed
    }
}

fn initial_selection(entries: &[FileEntry]) -> Option<usize> {
    if entries.is_empty() { None } else { Some(0) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;
    use std::fs;

    /// Builds `AppState` for tests that are not specifically exercising
    /// `GoSystemPlace`/Places. `AppState::new` now takes its `places`
    /// snapshot from `places::system_places()` — the real `$HOME`/XDG
    /// configuration of whatever machine runs the test — so every test
    /// that doesn't care about Places uses this instead, with an empty
    /// snapshot, to stay fully deterministic. Tests that DO exercise
    /// Places call `AppState::with_places` directly with their own
    /// controlled `Vec<Place>` (see `state_with_places` further down).
    fn test_state(navigation: Navigation) -> AppState {
        AppState::with_places(navigation, Vec::new())
    }

    fn names(state: &AppState) -> Vec<String> {
        state
            .entries()
            .iter()
            .map(|e| e.name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn loads_entries_from_the_starting_directory() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        fs::write(dir.path().join("b.txt"), b"").unwrap();

        let state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        assert_eq!(names(&state), vec!["a.txt", "b.txt"]);
    }

    #[test]
    fn empty_directory_has_no_selection() {
        let dir = TempDir::new();

        let state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        assert_eq!(state.selected(), None);
    }

    #[test]
    fn non_empty_directory_selects_the_first_entry() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();

        let state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn select_next_does_not_pass_the_last_entry() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        fs::write(dir.path().join("b.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::SelectNext);
        state.dispatch(Action::SelectNext);
        state.dispatch(Action::SelectNext);

        assert_eq!(state.selected(), Some(1));
    }

    #[test]
    fn select_previous_does_not_pass_the_first_entry() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        fs::write(dir.path().join("b.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::SelectPrevious);
        state.dispatch(Action::SelectPrevious);

        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn activate_selected_enters_a_directory() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::ActivateSelected);

        assert_eq!(state.current_dir(), dir.path().join("sub"));
    }

    #[test]
    fn activate_index_selects_then_enters_a_directory() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a_file.txt"), b"").unwrap();
        fs::create_dir(dir.path().join("z_dir")).unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        // sorted: directories first, so "z_dir" is index 0 and "a_file.txt" is index 1
        assert_eq!(names(&state), vec!["z_dir", "a_file.txt"]);

        state.dispatch(Action::ActivateIndex(0));

        assert_eq!(state.current_dir(), dir.path().join("z_dir"));
    }

    /// M5T-A V2 audit's bug #1 ("GoParent em /"): the audit assumed
    /// `Navigation::go_to_parent()` returns `io::Result<bool>` with
    /// `Ok(false)` at the root, which `AppState::go_parent()`'s
    /// `.is_ok()` check would then wrongly treat as success — reloading
    /// (clearing FILTER) without `current_dir` actually changing. The real
    /// signature is `io::Result<()>`, and at the root it returns `Err(_)`
    /// (`src/core/navigation.rs::go_to_parent`, "current directory has no
    /// parent"), which `.is_ok()` already reports as failure — so
    /// `go_parent` already takes the `Update::NONE` branch and never calls
    /// `reload()`. This test is kept exactly as the audit specified,
    /// passing unmodified: xdir being Linux-only makes `/` a real,
    /// deterministic parent-less directory to prove it against directly.
    #[test]
    fn go_parent_at_root_keeps_filter() {
        let mut state = test_state(Navigation::new(std::path::PathBuf::from("/")).unwrap());
        state.dispatch(Action::SetFilterQuery("x".to_string()));

        let update = state.dispatch(Action::GoParent);

        assert_eq!(update, Update::NONE, "update was {update:?}");
        assert_eq!(state.current_dir(), std::path::Path::new("/"));
        assert_eq!(state.filter_query(), "x");
    }

    #[test]
    fn go_parent_navigates_up_and_reloads_entries() {
        let dir = TempDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("inner.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(sub).unwrap());
        assert_eq!(names(&state), vec!["inner.txt"]);

        state.dispatch(Action::GoParent);

        assert_eq!(state.current_dir(), dir.path());
        assert_eq!(names(&state), vec!["sub"]);
    }

    #[test]
    fn changing_directory_resets_selection() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("sub")).unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::ActivateSelected);

        assert_eq!(state.selected(), None); // "sub" is empty
    }

    #[test]
    fn invalid_index_does_not_corrupt_state() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::SelectIndex(999));
        assert_eq!(state.selected(), Some(0));

        state.dispatch(Action::ActivateIndex(999));
        assert_eq!(state.selected(), Some(0));
        assert_eq!(state.current_dir(), dir.path());
    }

    #[test]
    fn toggle_hidden_reveals_and_hides_dotfiles() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".hidden"), b"").unwrap();
        fs::write(dir.path().join("visible.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        assert!(!state.show_hidden());
        assert_eq!(names(&state), vec!["visible.txt"]);

        state.dispatch(Action::ToggleHidden);
        assert!(state.show_hidden());
        assert_eq!(names(&state), vec![".hidden", "visible.txt"]);

        state.dispatch(Action::ToggleHidden);
        assert!(!state.show_hidden());
        assert_eq!(names(&state), vec!["visible.txt"]);
    }

    #[test]
    fn toggle_hidden_preserves_selection_on_the_same_entry_by_path() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".hidden"), b"").unwrap();
        fs::write(dir.path().join("visible.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        // Only "visible.txt" is listed yet, so it's the one selected.
        let selected_path = state.selected_entry().unwrap().path().to_path_buf();

        state.dispatch(Action::ToggleHidden);

        // ".hidden" now sorts before "visible.txt"; the selection must have
        // followed "visible.txt" by identity, not stayed pinned to index 0.
        assert_eq!(names(&state), vec![".hidden", "visible.txt"]);
        assert_eq!(
            state.selected_entry().unwrap().path().to_path_buf(),
            selected_path
        );
    }

    #[test]
    fn toggle_hidden_falls_back_to_the_first_entry_when_the_selected_one_disappears() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".hidden"), b"").unwrap();
        fs::write(dir.path().join("visible.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::ToggleHidden); // show hidden: [".hidden", "visible.txt"]
        state.dispatch(Action::SelectIndex(0)); // select ".hidden"

        state.dispatch(Action::ToggleHidden); // hide again: ".hidden" disappears

        assert_eq!(names(&state), vec!["visible.txt"]);
        assert_eq!(state.selected(), Some(0));
    }

    #[test]
    fn toggle_hidden_on_a_dotfile_only_directory_can_empty_or_repopulate_the_listing() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".only"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        assert_eq!(state.selected(), None);

        state.dispatch(Action::ToggleHidden);
        assert_eq!(state.selected(), Some(0));

        state.dispatch(Action::ToggleHidden);
        assert_eq!(state.selected(), None);
    }

    #[test]
    fn moving_selection_updates_preview_but_not_parent() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("a_dir")).unwrap();
        fs::create_dir(dir.path().join("b_dir")).unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        assert_eq!(names(&state), vec!["a_dir", "b_dir"]);
        let parent_before = state.parent().entries().len();

        let update = state.dispatch(Action::SelectNext);

        assert_eq!(
            update,
            Update {
                current_changed: false,
                parent_changed: false,
                preview_changed: true,
            }
        );
        assert_eq!(state.parent().entries().len(), parent_before);
        match state.preview() {
            PreviewContext::Directory(children) => assert!(children.is_empty()),
            other => panic!("expected an (empty) Directory preview for b_dir, got {other:?}"),
        }
    }

    #[test]
    fn entering_a_directory_updates_current_parent_and_preview() {
        let dir = TempDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::create_dir(sub.join("inner")).unwrap();
        fs::create_dir(sub.join("inner").join("leaf")).unwrap();
        // Sorts after "sub" (dirs-first, then alphabetical), so "sub"
        // stays the initial selection at index 0.
        fs::create_dir(dir.path().join("zzz_sibling")).unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let update = state.dispatch(Action::ActivateSelected);

        assert_eq!(update, Update::ALL);
        assert_eq!(state.current_dir(), sub);
        assert_eq!(state.parent().dir(), Some(dir.path()));
        let highlighted = state
            .parent()
            .current_index()
            .and_then(|i| state.parent().entries().get(i));
        assert_eq!(highlighted.map(FileEntry::path), Some(sub.as_path()));
        // Entering "sub" resets the selection to its first entry, "inner";
        // PREVIEW must reflect *that* entry's own children ("leaf"), not
        // "sub"'s.
        assert_eq!(
            state.selected_entry().map(FileEntry::name),
            Some(std::ffi::OsStr::new("inner"))
        );
        match state.preview() {
            PreviewContext::Directory(children) => {
                assert_eq!(
                    children.iter().map(|e| e.name().to_owned()).next(),
                    Some(std::ffi::OsString::from("leaf"))
                );
            }
            other => panic!("expected Directory preview for the selected entry, got {other:?}"),
        }
    }

    #[test]
    fn toggle_hidden_updates_current_parent_and_preview() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".hidden"), b"").unwrap();
        fs::create_dir(dir.path().join("visible_dir")).unwrap();
        fs::write(dir.path().join("visible_dir/.child_hidden"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let update = state.dispatch(Action::ToggleHidden);

        assert_eq!(update, Update::ALL);
        assert!(state.show_hidden());
        match state.preview() {
            PreviewContext::Directory(children) => {
                let child_names: Vec<String> = children
                    .iter()
                    .map(|e| e.name().to_string_lossy().into_owned())
                    .collect();
                assert!(child_names.contains(&".child_hidden".to_string()));
            }
            other => panic!("expected Directory preview for visible_dir, got {other:?}"),
        }
    }

    #[test]
    fn a_no_op_action_reports_no_update() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        // "a.txt" is a regular file: ActivateSelected is a deliberate no-op.
        let update = state.dispatch(Action::ActivateSelected);

        assert_eq!(update, Update::NONE);
    }

    #[test]
    fn select_next_at_the_last_entry_is_a_no_op_update() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        fs::write(dir.path().join("b.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::SelectNext); // now on "b.txt", the last entry
        assert_eq!(state.selected(), Some(1));

        let update = state.dispatch(Action::SelectNext);

        assert_eq!(state.selected(), Some(1));
        assert_eq!(update, Update::NONE);
    }

    #[test]
    fn select_previous_at_the_first_entry_is_a_no_op_update() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        fs::write(dir.path().join("b.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        assert_eq!(state.selected(), Some(0)); // already on "a.txt", the first entry

        let update = state.dispatch(Action::SelectPrevious);

        assert_eq!(state.selected(), Some(0));
        assert_eq!(update, Update::NONE);
    }

    #[test]
    fn select_index_of_the_already_selected_entry_is_a_no_op_update() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        assert_eq!(state.selected(), Some(0));

        let update = state.dispatch(Action::SelectIndex(0));

        assert_eq!(update, Update::NONE);
    }

    #[test]
    fn reselecting_the_same_image_is_a_no_op_update_no_second_decode() {
        let dir = TempDir::new();
        image::RgbaImage::from_pixel(2, 2, image::Rgba([1, 2, 3, 255]))
            .save_with_format(dir.path().join("a.png"), image::ImageFormat::Png)
            .unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        assert_eq!(state.selected(), Some(0));
        assert!(matches!(
            state.preview(),
            crate::app::PreviewContext::File(crate::app::FilePreview::Image { .. })
        ));

        // `refresh_preview` (and so `FilePreview::build`'s decode) only
        // runs when `select_index` actually changes the selection — this
        // dispatch, re-selecting the entry already selected, must be a
        // pure no-op and therefore never re-decode the image.
        let update = state.dispatch(Action::SelectIndex(0));

        assert_eq!(update, Update::NONE);
    }

    #[test]
    fn activate_index_out_of_range_is_a_complete_no_op() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("directory")).unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        // "directory" is selected (the only entry).
        assert_eq!(state.selected(), Some(0));

        let update = state.dispatch(Action::ActivateIndex(999));

        // An out-of-range ActivateIndex must not fall back to activating
        // whatever was already selected.
        assert_eq!(update, Update::NONE);
        assert_eq!(state.selected(), Some(0));
        assert_eq!(state.current_dir(), dir.path());
    }

    // --- SelectFirst / SelectLast ---------------------------------------

    fn three_entry_state() -> (TempDir, AppState) {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"contents-a").unwrap();
        fs::write(dir.path().join("b.txt"), b"contents-b").unwrap();
        fs::write(dir.path().join("c.txt"), b"contents-c").unwrap();
        let state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        (dir, state)
    }

    #[test]
    fn select_first_moves_to_first_item() {
        let (_dir, mut state) = three_entry_state();
        state.dispatch(Action::SelectNext);
        state.dispatch(Action::SelectNext);
        assert_eq!(state.selected(), Some(2));

        let update = state.dispatch(Action::SelectFirst);

        assert_eq!(state.selected(), Some(0));
        assert_eq!(update, Update::PREVIEW_ONLY);
    }

    #[test]
    fn select_first_when_already_first_returns_none() {
        let (_dir, mut state) = three_entry_state();
        assert_eq!(state.selected(), Some(0));

        let update = state.dispatch(Action::SelectFirst);

        assert_eq!(update, Update::NONE);
    }

    #[test]
    fn select_last_moves_to_last_item() {
        let (_dir, mut state) = three_entry_state();
        assert_eq!(state.selected(), Some(0));

        let update = state.dispatch(Action::SelectLast);

        assert_eq!(state.selected(), Some(2));
        assert_eq!(update, Update::PREVIEW_ONLY);
    }

    #[test]
    fn select_last_when_already_last_returns_none() {
        let (_dir, mut state) = three_entry_state();
        state.dispatch(Action::SelectLast);
        assert_eq!(state.selected(), Some(2));

        let update = state.dispatch(Action::SelectLast);

        assert_eq!(update, Update::NONE);
    }

    #[test]
    fn select_first_on_empty_returns_none() {
        let dir = TempDir::new();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        assert_eq!(state.selected(), None);

        let update = state.dispatch(Action::SelectFirst);

        assert_eq!(update, Update::NONE);
    }

    #[test]
    fn select_last_on_empty_returns_none() {
        let dir = TempDir::new();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        assert_eq!(state.selected(), None);

        let update = state.dispatch(Action::SelectLast);

        assert_eq!(update, Update::NONE);
    }

    #[test]
    fn select_first_real_change_rebuilds_preview_exactly_once() {
        let (_dir, mut state) = three_entry_state();
        state.dispatch(Action::SelectLast);
        let before = format!("{:?}", state.preview());

        let update = state.dispatch(Action::SelectFirst);

        assert_eq!(update, Update::PREVIEW_ONLY);
        assert_ne!(format!("{:?}", state.preview()), before);
        assert!(!update.current_changed);
        assert!(!update.parent_changed);
    }

    // --- GoSystemPlace ----------------------------------------------------

    fn place_dir(root: &TempDir, name: &str) -> std::path::PathBuf {
        let path = root.path().join(name);
        fs::create_dir(&path).unwrap();
        path
    }

    fn state_with_places(start: std::path::PathBuf, places: Vec<Place>) -> AppState {
        AppState::with_places(Navigation::new(start).unwrap(), places)
    }

    #[test]
    fn go_home_navigates_to_home_place() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let home = place_dir(&root, "home");
        let places = vec![Place::new_for_test(SystemPlaceKind::Home, home.clone())];
        let mut state = state_with_places(start, places);

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Home));

        assert_eq!(update, Update::ALL);
        assert_eq!(state.current_dir(), home);
    }

    #[test]
    fn go_downloads_navigates_to_downloads() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let downloads = place_dir(&root, "downloads");
        let places = vec![Place::new_for_test(
            SystemPlaceKind::Downloads,
            downloads.clone(),
        )];
        let mut state = state_with_places(start, places);

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Downloads));

        assert_eq!(update, Update::ALL);
        assert_eq!(state.current_dir(), downloads);
    }

    #[test]
    fn go_documents_navigates_to_documents() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let documents = place_dir(&root, "documents");
        let places = vec![Place::new_for_test(
            SystemPlaceKind::Documents,
            documents.clone(),
        )];
        let mut state = state_with_places(start, places);

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Documents));

        assert_eq!(update, Update::ALL);
        assert_eq!(state.current_dir(), documents);
    }

    #[test]
    fn go_pictures_navigates_to_pictures() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let pictures = place_dir(&root, "pictures");
        let places = vec![Place::new_for_test(
            SystemPlaceKind::Pictures,
            pictures.clone(),
        )];
        let mut state = state_with_places(start, places);

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Pictures));

        assert_eq!(update, Update::ALL);
        assert_eq!(state.current_dir(), pictures);
    }

    #[test]
    fn go_music_navigates_to_music() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let music = place_dir(&root, "music");
        let places = vec![Place::new_for_test(SystemPlaceKind::Music, music.clone())];
        let mut state = state_with_places(start, places);

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Music));

        assert_eq!(update, Update::ALL);
        assert_eq!(state.current_dir(), music);
    }

    #[test]
    fn go_videos_navigates_to_videos() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let videos = place_dir(&root, "videos");
        let places = vec![Place::new_for_test(SystemPlaceKind::Videos, videos.clone())];
        let mut state = state_with_places(start, places);

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Videos));

        assert_eq!(update, Update::ALL);
        assert_eq!(state.current_dir(), videos);
    }

    #[test]
    fn missing_place_is_noop() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let mut state = state_with_places(start.clone(), Vec::new());

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Home));

        assert_eq!(update, Update::NONE);
        assert_eq!(state.current_dir(), start);
    }

    #[test]
    fn target_same_as_current_is_noop() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let places = vec![Place::new_for_test(SystemPlaceKind::Home, start.clone())];
        let mut state = state_with_places(start.clone(), places);

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Home));

        assert_eq!(update, Update::NONE);
        assert_eq!(state.current_dir(), start);
    }

    #[test]
    fn disappearing_place_does_not_change_current_dir() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let ghost = root.path().join("ghost");
        fs::create_dir(&ghost).unwrap();
        let places = vec![Place::new_for_test(SystemPlaceKind::Home, ghost.clone())];
        fs::remove_dir(&ghost).unwrap();
        let mut state = state_with_places(start.clone(), places);

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Home));

        assert_eq!(update, Update::NONE);
        assert_eq!(state.current_dir(), start);
    }

    #[test]
    fn failed_place_navigation_does_not_change_current_entries() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        fs::write(start.join("keep.txt"), b"").unwrap();
        let ghost = root.path().join("ghost");
        fs::create_dir(&ghost).unwrap();
        let places = vec![Place::new_for_test(SystemPlaceKind::Home, ghost.clone())];
        fs::remove_dir(&ghost).unwrap();
        let mut state = state_with_places(start, places);

        state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Home));

        assert_eq!(names(&state), vec!["keep.txt"]);
    }

    #[test]
    fn successful_place_navigation_updates_parent_current_and_preview() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let home = place_dir(&root, "home");
        fs::create_dir(home.join("inner")).unwrap();
        let places = vec![Place::new_for_test(SystemPlaceKind::Home, home.clone())];
        let mut state = state_with_places(start, places);

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Home));

        assert_eq!(update, Update::ALL);
        assert_eq!(state.current_dir(), home);
        assert_eq!(state.parent().dir(), Some(root.path()));
        assert_eq!(names(&state), vec!["inner"]);
        match state.preview() {
            PreviewContext::Directory(children) => assert!(children.is_empty()),
            other => panic!("expected Directory preview for inner, got {other:?}"),
        }
    }

    // --- FILTER (M5T-A) --------------------------------------------------

    /// Four files whose names deliberately share/don't-share the substring
    /// "cargo", case varied on purpose (`filter_is_case_insensitive`).
    /// Byte-order sort (`core::filesystem::sort_entries`) puts them in
    /// exactly this order: uppercase `C` (0x43) sorts before lowercase
    /// `c`/`m`/`r`.
    fn filter_fixture_dir() -> TempDir {
        let dir = TempDir::new();
        fs::write(dir.path().join("Cargo.toml"), b"").unwrap();
        fs::write(dir.path().join("cargo.lock"), b"").unwrap();
        fs::write(dir.path().join("my-cargo-notes.txt"), b"").unwrap();
        fs::write(dir.path().join("readme.md"), b"").unwrap();
        dir
    }

    fn filter_state() -> (TempDir, AppState) {
        let dir = filter_fixture_dir();
        let state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        (dir, state)
    }

    fn visible_names(state: &AppState) -> Vec<String> {
        state
            .visible_entries()
            .iter()
            .map(|e| e.name().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn empty_query_shows_all_entries() {
        let (_dir, state) = filter_state();

        assert_eq!(state.filter_query(), "");
        assert_eq!(visible_names(&state), names(&state));
    }

    #[test]
    fn filter_matches_substring() {
        let (_dir, mut state) = filter_state();

        state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        assert_eq!(
            visible_names(&state),
            vec!["Cargo.toml", "cargo.lock", "my-cargo-notes.txt"]
        );
    }

    #[test]
    fn filter_is_case_insensitive() {
        let (_dir, mut state) = filter_state();

        state.dispatch(Action::SetFilterQuery("CARGO".to_string()));

        assert_eq!(
            visible_names(&state),
            vec!["Cargo.toml", "cargo.lock", "my-cargo-notes.txt"]
        );
    }

    #[test]
    fn filter_no_match_returns_empty_visible_list() {
        let (_dir, mut state) = filter_state();

        state.dispatch(Action::SetFilterQuery("zzz-no-match".to_string()));

        assert!(state.visible_entries().is_empty());
    }

    #[test]
    fn filter_does_not_mutate_full_entries() {
        let (_dir, mut state) = filter_state();
        let before = names(&state);

        state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        // `entries()` (the full list) is untouched by filtering — only
        // `visible_entries()` is a subset.
        assert_eq!(names(&state), before);
    }

    #[test]
    fn selected_path_is_preserved_when_still_visible() {
        let (_dir, mut state) = filter_state();
        let selected_path = state.selected_entry().unwrap().path().to_path_buf();
        assert_eq!(
            selected_path.file_name().unwrap(),
            std::ffi::OsStr::new("Cargo.toml")
        );

        state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        assert_eq!(
            state.selected_entry().unwrap().path(),
            selected_path.as_path()
        );
    }

    #[test]
    fn selected_path_falls_to_first_result_when_filtered_out() {
        let (_dir, mut state) = filter_state();
        state.dispatch(Action::SelectLast); // "readme.md" — "cargo" excludes it
        assert_eq!(
            state.selected_entry().unwrap().name(),
            std::ffi::OsStr::new("readme.md")
        );

        let update = state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        assert_eq!(
            state.selected_entry().unwrap().name(),
            std::ffi::OsStr::new("Cargo.toml")
        );
        assert!(update.preview_changed);
    }

    #[test]
    fn no_results_clear_selection() {
        let (_dir, mut state) = filter_state();

        state.dispatch(Action::SetFilterQuery("zzz-no-match".to_string()));

        assert!(state.selected_entry().is_none());
        assert_eq!(state.visible_selected_index(), None);
    }

    /// M5T-A V2 audit's bug #2: V1's `clear_filter` left `selected` at
    /// `None` after a zero-match query, even with a non-empty full listing
    /// right behind it. Fixed via `resettle_selection_to_filter` — no
    /// selection-history state, deterministically the first full entry.
    #[test]
    fn clear_filter_after_zero_matches_restores_selection() {
        let (_dir, mut state) = filter_state();
        state.dispatch(Action::SetFilterQuery("zzz-no-match".to_string()));
        assert!(state.selected_entry().is_none());
        assert!(matches!(state.preview(), PreviewContext::None));

        let update = state.dispatch(Action::ClearFilter);

        // Full listing back, a real entry selected, PREVIEW matching it.
        assert_eq!(visible_names(&state), names(&state));
        assert_eq!(
            state.selected_entry().map(FileEntry::name),
            Some(std::ffi::OsStr::new("Cargo.toml"))
        );
        assert!(
            !matches!(state.preview(), PreviewContext::None),
            "PREVIEW must follow the restored selection, not stay empty"
        );
        assert!(update.current_changed);
        assert!(!update.parent_changed);
        assert!(update.preview_changed);
    }

    #[test]
    fn clearing_filter_restores_full_entries() {
        let (_dir, mut state) = filter_state();
        let full = names(&state);
        state.dispatch(Action::SetFilterQuery("cargo".to_string()));
        assert_ne!(visible_names(&state), full);

        state.dispatch(Action::ClearFilter);

        assert_eq!(visible_names(&state), full);
        assert_eq!(state.filter_query(), "");
    }

    #[test]
    fn clearing_filter_preserves_real_selected_path_when_possible() {
        let (_dir, mut state) = filter_state();
        state.dispatch(Action::SetFilterQuery("cargo".to_string()));
        let selected_path = state.selected_entry().unwrap().path().to_path_buf();

        state.dispatch(Action::ClearFilter);

        assert_eq!(
            state.selected_entry().unwrap().path(),
            selected_path.as_path()
        );
    }

    #[test]
    fn filtering_does_not_change_current_dir() {
        let (_dir, mut state) = filter_state();
        let dir_before = state.current_dir().to_path_buf();

        state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        assert_eq!(state.current_dir(), dir_before.as_path());
    }

    #[test]
    fn filtering_does_not_rebuild_parent() {
        let (_dir, mut state) = filter_state();

        let update = state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        assert!(!update.parent_changed);
    }

    #[test]
    fn preview_unchanged_when_selected_path_stays_same() {
        let (_dir, mut state) = filter_state();
        // "Cargo.toml" is selected initially and still matches "cargo".

        let update = state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        assert!(!update.preview_changed);
    }

    #[test]
    fn preview_changes_once_when_filter_changes_selection() {
        let (_dir, mut state) = filter_state();
        state.dispatch(Action::SelectLast); // "readme.md"

        let update = state.dispatch(Action::SetFilterQuery("cargo".to_string()));
        assert!(update.preview_changed);
        let preview_after = format!("{:?}", state.preview());

        // Narrowing further, while the same entry ("Cargo.toml") stays the
        // best/first match, must not rebuild PREVIEW a second time.
        let update2 = state.dispatch(Action::SetFilterQuery("cargo.".to_string()));
        assert!(!update2.preview_changed);
        assert_eq!(format!("{:?}", state.preview()), preview_after);
    }

    #[test]
    fn toggle_hidden_keeps_active_query() {
        let (_dir, mut state) = filter_state();
        state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        state.dispatch(Action::ToggleHidden);

        assert_eq!(state.filter_query(), "cargo");
    }

    /// M5T-A V2 audit's bug #3: `clamp_selection_to_filter` skipped doing
    /// anything whenever `selected` was already `None`. In V1 that gap was
    /// unreachable in practice — `toggle_hidden`'s own path-preserving pick
    /// already falls back to `initial_selection` (the first *full* entry)
    /// whenever `entries` isn't empty, so `clamp_selection_to_filter` never
    /// actually saw `None` with a non-empty listing behind it — but that
    /// made the gap invisible rather than real, one call site's behavior
    /// quietly covering for another's incomplete one. `resettle_selection_
    /// to_filter` (shared with `set_filter_query`/`clear_filter`) treats
    /// "unset" and "invisible" identically, closing the gap in the rule
    /// itself rather than relying on an upstream coincidence.
    #[test]
    fn toggle_hidden_can_restore_selection_when_filter_gains_results() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".cargo-hidden"), b"").unwrap();
        fs::write(dir.path().join("readme.md"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::SetFilterQuery(".cargo".to_string()));
        assert!(state.visible_entries().is_empty());
        assert!(state.selected_entry().is_none(), "expected no selection");

        state.dispatch(Action::ToggleHidden);

        assert_eq!(
            state.selected_entry().map(|e| e.name().to_owned()),
            Some(std::ffi::OsString::from(".cargo-hidden"))
        );
        assert!(
            !matches!(state.preview(), PreviewContext::None),
            "PREVIEW must follow the newly-visible selection"
        );
    }

    /// The inverse path: a matching, selected entry that `ToggleHidden`
    /// itself hides again must leave FILTER with zero visible results and
    /// no selection — never a stale index pointing at an entry CURRENT no
    /// longer shows.
    #[test]
    fn toggle_hidden_can_clear_selection_when_filter_loses_its_match() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".cargo-hidden"), b"").unwrap();
        fs::write(dir.path().join("readme.md"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::ToggleHidden); // show hidden: ".cargo-hidden" visible
        state.dispatch(Action::SetFilterQuery(".cargo".to_string()));
        assert_eq!(
            state.selected_entry().map(|e| e.name().to_owned()),
            Some(std::ffi::OsString::from(".cargo-hidden"))
        );

        state.dispatch(Action::ToggleHidden); // hide it again

        assert!(state.visible_entries().is_empty());
        assert!(state.selected_entry().is_none());
        assert!(matches!(state.preview(), PreviewContext::None));
    }

    #[test]
    fn toggle_hidden_reapplies_filter() {
        let dir = TempDir::new();
        fs::write(dir.path().join(".cargo-hidden"), b"").unwrap();
        fs::write(dir.path().join("cargo.lock"), b"").unwrap();
        fs::write(dir.path().join("readme.md"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::SetFilterQuery("cargo".to_string()));
        assert_eq!(visible_names(&state), vec!["cargo.lock"]);

        state.dispatch(Action::ToggleHidden); // reveals dotfiles

        // The newly revealed ".cargo-hidden" also matches "cargo" and must
        // show up in the reapplied filter — with no directory read beyond
        // the single one `ToggleHidden` already does.
        assert_eq!(visible_names(&state), vec![".cargo-hidden", "cargo.lock"]);
    }

    #[test]
    fn successful_directory_navigation_clears_filter() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("cargo-project")).unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::SetFilterQuery("cargo".to_string()));
        assert_eq!(state.filter_query(), "cargo");

        state.dispatch(Action::ActivateSelected); // enters "cargo-project"

        assert_eq!(state.filter_query(), "");
    }

    #[test]
    fn failed_navigation_keeps_filter() {
        let dir = TempDir::new();
        fs::write(dir.path().join("cargo-file.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        // The only (filtered-in) entry is a regular file: `ActivateSelected`
        // is a deliberate no-op, exactly as it was before FILTER existed.
        let update = state.dispatch(Action::ActivateSelected);

        assert_eq!(update, Update::NONE);
        assert_eq!(state.filter_query(), "cargo");
    }

    #[test]
    fn same_directory_noop_keeps_filter() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let places = vec![Place::new_for_test(SystemPlaceKind::Home, start.clone())];
        let mut state = state_with_places(start, places);
        state.dispatch(Action::SetFilterQuery("x".to_string()));

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Home));

        assert_eq!(update, Update::NONE);
        assert_eq!(state.filter_query(), "x");
    }

    #[test]
    fn go_system_place_success_clears_filter() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let home = place_dir(&root, "home");
        let places = vec![Place::new_for_test(SystemPlaceKind::Home, home.clone())];
        let mut state = state_with_places(start, places);
        state.dispatch(Action::SetFilterQuery("x".to_string()));

        state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Home));

        assert_eq!(state.filter_query(), "");
    }

    /// §27: an accented query must match an accented filename by plain
    /// `str::to_lowercase` substring containment — no NFC/NFD
    /// normalization required this milestone, just a real non-ASCII
    /// character surviving the whole path unmangled.
    #[test]
    fn filter_matches_accented_substring_case_insensitively() {
        let dir = TempDir::new();
        fs::write(dir.path().join("documentação.txt"), b"").unwrap();
        fs::write(dir.path().join("plain.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        state.dispatch(Action::SetFilterQuery("AÇÃO".to_string()));

        assert_eq!(visible_names(&state), vec!["documentação.txt"]);
    }

    /// Enter's "commit" (see `ui/window.rs::on_filter_accepted`) makes no
    /// further `AppState` call — the query already filters CURRENT live as
    /// it's typed, so this documents the invariant that makes that
    /// correct: once `SetFilterQuery` is dispatched, FILTER is already
    /// exactly as "committed" as pressing Enter would ever make it.
    #[test]
    fn enter_commits_filter_and_returns_normal() {
        let (_dir, mut state) = filter_state();

        state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        assert_eq!(state.filter_query(), "cargo");
        assert_eq!(
            visible_names(&state),
            vec!["Cargo.toml", "cargo.lock", "my-cargo-notes.txt"]
        );
    }

    #[test]
    fn escape_from_filter_clears_and_returns_normal() {
        let (_dir, mut state) = filter_state();
        state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        state.dispatch(Action::ClearFilter);

        assert_eq!(state.filter_query(), "");
        assert_eq!(visible_names(&state), names(&state));
    }

    #[test]
    fn normal_without_filter_escape_is_noop() {
        let (_dir, mut state) = filter_state();
        assert_eq!(state.filter_query(), "");

        let update = state.dispatch(Action::ClearFilter);

        assert_eq!(update, Update::NONE);
    }

    /// What `ui/window.rs::begin_filter_edit` pre-fills the input box with
    /// on a second `/` (§8, "reabrir `/`") — the query must survive an
    /// unrelated action (a plain selection move) untouched.
    #[test]
    fn reopening_filter_preloads_the_active_query() {
        let (_dir, mut state) = filter_state();
        state.dispatch(Action::SetFilterQuery("cargo".to_string()));

        state.dispatch(Action::SelectNext);

        assert_eq!(state.filter_query(), "cargo");
    }

    // --- FIND (M5T-B2) -----------------------------------------------------
    //
    // These never call `core::find::find_recursive` (that's M5T-B1's own
    // job, already tested there) — a `FindOutcome` is built directly and
    // handed to `complete_find`, exactly as `ui/window.rs`'s completion
    // handler would after a worker actually ran. Real `FileEntry` values
    // still come from real files on disk (`FileEntry::from_path`/
    // `from_dir_entry` are the only constructors — nothing here rebuilds
    // one from a bare string), so identity stays exactly what production
    // code would see.

    fn find_fixture_dir() -> TempDir {
        let dir = TempDir::new();
        fs::write(dir.path().join("readme.md"), b"").unwrap();
        fs::create_dir(dir.path().join("src")).unwrap();
        fs::write(dir.path().join("src").join("main.rs"), b"fn main() {}").unwrap();
        fs::create_dir(dir.path().join("docs")).unwrap();
        fs::write(dir.path().join("docs").join("main-notes.md"), b"notes").unwrap();
        dir
    }

    fn find_state() -> (TempDir, AppState) {
        let dir = find_fixture_dir();
        let state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        (dir, state)
    }

    fn find_entry(path: std::path::PathBuf) -> FileEntry {
        FileEntry::from_path(path).expect("fixture path must exist")
    }

    fn find_outcome(results: Vec<FileEntry>) -> FindOutcome {
        FindOutcome {
            results,
            truncated: false,
            skipped_errors: Vec::new(),
        }
    }

    #[test]
    fn starting_find_preserves_normal_entries() {
        let (_dir, mut state) = find_state();
        let before = names(&state);

        state.dispatch(Action::StartFind("main".to_string()));

        assert_eq!(names(&state), before);
    }

    #[test]
    fn starting_find_preserves_normal_selected() {
        let (_dir, mut state) = find_state();
        let before = state.selected();

        state.dispatch(Action::StartFind("main".to_string()));

        assert_eq!(state.selected(), before);
    }

    #[test]
    fn starting_find_preserves_active_filter() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::SetFilterQuery("main".to_string()));

        state.dispatch(Action::StartFind("readme".to_string()));

        assert_eq!(state.filter_query(), "main");
    }

    #[test]
    fn find_request_snapshots_current_dir() {
        let (dir, mut state) = find_state();

        state.dispatch(Action::StartFind("main".to_string()));

        assert_eq!(state.find_session().unwrap().root(), dir.path());
    }

    #[test]
    fn find_request_snapshots_show_hidden() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::ToggleHidden); // show_hidden: true (no FIND active yet)
        assert!(state.show_hidden());

        state.dispatch(Action::StartFind("main".to_string()));

        assert!(state.find_session().unwrap().include_hidden());
    }

    #[test]
    fn new_find_gets_new_generation() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::StartFind("a".to_string()));
        let first = state.find_session().unwrap().generation();
        state.dispatch(Action::ClearFind);

        state.dispatch(Action::StartFind("b".to_string()));

        assert_ne!(state.find_session().unwrap().generation(), first);
    }

    #[test]
    fn new_find_supersedes_previous_generation() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::StartFind("a".to_string()));
        let first = state.find_session().unwrap().generation();

        // Reopening and resubmitting without an explicit `ClearFind` in
        // between — `f` again, then Enter on a different query.
        state.dispatch(Action::StartFind("b".to_string()));

        assert_ne!(state.find_session().unwrap().generation(), first);
        assert_eq!(state.find_session().unwrap().query(), "b");
    }

    #[test]
    fn stale_completion_is_ignored() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::StartFind("a".to_string()));
        let stale_generation = state.find_session().unwrap().generation();
        state.dispatch(Action::StartFind("b".to_string()));

        let update = state.complete_find(stale_generation, Ok(FindOutcome::default()));

        assert_eq!(update, Update::NONE);
        // The live session ("b") is completely unaffected by the stale
        // completion for the superseded one ("a").
        assert_eq!(state.find_session().unwrap().query(), "b");
        assert!(matches!(
            state.find_session().unwrap().phase(),
            FindPhase::Searching
        ));
    }

    #[test]
    fn clear_find_invalidates_pending_completion() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::StartFind("a".to_string()));
        let generation = state.find_session().unwrap().generation();

        state.dispatch(Action::ClearFind);
        let update = state.complete_find(generation, Ok(FindOutcome::default()));

        assert_eq!(update, Update::NONE);
        assert!(state.find_session().is_none());
    }

    #[test]
    fn successful_completion_exposes_results() {
        let (dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![find_entry(dir.path().join("src").join("main.rs"))]);

        state.complete_find(generation, Ok(outcome));

        match state.find_session().unwrap().phase() {
            FindPhase::Ready(ready) => assert_eq!(ready.results.len(), 1),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn successful_completion_selects_first_result() {
        let (dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![find_entry(dir.path().join("src").join("main.rs"))]);

        state.complete_find(generation, Ok(outcome));

        match state.find_session().unwrap().phase() {
            FindPhase::Ready(ready) => assert_eq!(ready.selected, Some(0)),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn zero_results_have_no_find_selection() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::StartFind("zzz-no-match".to_string()));
        let generation = state.find_session().unwrap().generation();

        state.complete_find(generation, Ok(FindOutcome::default()));

        match state.find_session().unwrap().phase() {
            FindPhase::Ready(ready) => assert_eq!(ready.selected, None),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn find_selection_does_not_mutate_normal_selected() {
        let (dir, mut state) = find_state();
        let normal_selected = state.selected();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![
            find_entry(dir.path().join("docs").join("main-notes.md")),
            find_entry(dir.path().join("src").join("main.rs")),
        ]);
        state.complete_find(generation, Ok(outcome));

        state.dispatch(Action::SelectNext);

        assert_eq!(state.selected(), normal_selected);
    }

    #[test]
    fn clearing_find_restores_normal_selection() {
        let (dir, mut state) = find_state();
        let normal_selected_path = state.selected_entry().unwrap().path().to_path_buf();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![find_entry(dir.path().join("src").join("main.rs"))]);
        state.complete_find(generation, Ok(outcome));

        state.dispatch(Action::ClearFind);

        assert_eq!(
            state.selected_entry().unwrap().path(),
            normal_selected_path.as_path()
        );
    }

    #[test]
    fn clearing_find_restores_normal_preview() {
        let (dir, mut state) = find_state();
        let normal_preview = format!("{:?}", state.preview());
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![find_entry(dir.path().join("src").join("main.rs"))]);
        state.complete_find(generation, Ok(outcome));
        assert_ne!(format!("{:?}", state.preview()), normal_preview);

        state.dispatch(Action::ClearFind);

        assert_eq!(format!("{:?}", state.preview()), normal_preview);
    }

    #[test]
    fn clearing_find_restores_underlying_filter_view() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::SetFilterQuery("main".to_string()));
        let filtered_before = visible_names(&state);
        state.dispatch(Action::StartFind("readme".to_string()));

        state.dispatch(Action::ClearFind);

        assert_eq!(state.filter_query(), "main");
        assert_eq!(visible_names(&state), filtered_before);
    }

    #[test]
    fn find_selection_next_previous_uses_find_results() {
        let (dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![
            find_entry(dir.path().join("docs").join("main-notes.md")),
            find_entry(dir.path().join("src").join("main.rs")),
        ]);
        state.complete_find(generation, Ok(outcome));

        state.dispatch(Action::SelectNext);
        match state.find_session().unwrap().phase() {
            FindPhase::Ready(ready) => assert_eq!(ready.selected, Some(1)),
            other => panic!("expected Ready, got {other:?}"),
        }

        state.dispatch(Action::SelectPrevious);
        match state.find_session().unwrap().phase() {
            FindPhase::Ready(ready) => assert_eq!(ready.selected, Some(0)),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn gg_and_g_use_find_results() {
        let (dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![
            find_entry(dir.path().join("docs").join("main-notes.md")),
            find_entry(dir.path().join("src").join("main.rs")),
        ]);
        state.complete_find(generation, Ok(outcome));
        state.dispatch(Action::SelectNext); // now at index 1

        state.dispatch(Action::SelectFirst); // gg
        match state.find_session().unwrap().phase() {
            FindPhase::Ready(ready) => assert_eq!(ready.selected, Some(0)),
            other => panic!("expected Ready, got {other:?}"),
        }

        state.dispatch(Action::SelectLast); // G
        match state.find_session().unwrap().phase() {
            FindPhase::Ready(ready) => assert_eq!(ready.selected, Some(1)),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn find_preview_follows_selected_result() {
        let (dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![
            find_entry(dir.path().join("docs").join("main-notes.md")),
            find_entry(dir.path().join("src").join("main.rs")),
        ]);
        state.complete_find(generation, Ok(outcome));
        let preview_first = format!("{:?}", state.preview());

        let update = state.dispatch(Action::SelectNext);

        assert!(update.preview_changed);
        assert_ne!(format!("{:?}", state.preview()), preview_first);
    }

    #[test]
    fn directory_result_activation_navigates_and_clears_find() {
        let (dir, mut state) = find_state();
        state.dispatch(Action::StartFind("src".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![find_entry(dir.path().join("src"))]);
        state.complete_find(generation, Ok(outcome));

        let update = state.dispatch(Action::ActivateSelected);

        assert_eq!(update, Update::ALL);
        assert_eq!(state.current_dir(), dir.path().join("src"));
        assert!(state.find_session().is_none());
    }

    #[test]
    fn non_directory_result_activation_is_noop() {
        let (dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![find_entry(dir.path().join("src").join("main.rs"))]);
        state.complete_find(generation, Ok(outcome));

        let update = state.dispatch(Action::ActivateSelected);

        assert_eq!(update, Update::NONE);
        assert_eq!(state.current_dir(), dir.path());
        assert!(state.find_session().is_some());
    }

    #[test]
    fn failed_directory_navigation_keeps_find() {
        let (dir, mut state) = find_state();
        let ghost = dir.path().join("ghost_dir");
        fs::create_dir(&ghost).unwrap();
        let ghost_entry = find_entry(ghost.clone());
        fs::remove_dir(&ghost).unwrap(); // exists no more by activation time

        state.dispatch(Action::StartFind("ghost".to_string()));
        let generation = state.find_session().unwrap().generation();
        state.complete_find(generation, Ok(find_outcome(vec![ghost_entry])));

        let update = state.dispatch(Action::ActivateSelected);

        assert_eq!(update, Update::NONE);
        assert!(state.find_session().is_some());
    }

    #[test]
    fn successful_go_parent_clears_find() {
        let dir = TempDir::new();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("main.rs"), b"").unwrap();
        let mut state = test_state(Navigation::new(sub).unwrap());
        state.dispatch(Action::StartFind("main".to_string()));
        assert!(state.find_session().is_some());

        let update = state.dispatch(Action::GoParent);

        assert_eq!(update, Update::ALL);
        assert!(state.find_session().is_none());
        assert_eq!(state.current_dir(), dir.path());
    }

    #[test]
    fn failed_go_parent_keeps_find() {
        let mut state = test_state(Navigation::new(std::path::PathBuf::from("/")).unwrap());
        state.dispatch(Action::StartFind("x".to_string()));
        assert!(state.find_session().is_some());

        let update = state.dispatch(Action::GoParent);

        assert_eq!(update, Update::NONE);
        assert!(state.find_session().is_some());
    }

    #[test]
    fn system_place_navigation_clears_find_on_success() {
        let root = TempDir::new();
        let start = place_dir(&root, "start");
        let home = place_dir(&root, "home");
        let places = vec![Place::new_for_test(SystemPlaceKind::Home, home.clone())];
        let mut state = state_with_places(start, places);
        state.dispatch(Action::StartFind("x".to_string()));

        let update = state.dispatch(Action::GoSystemPlace(SystemPlaceKind::Home));

        assert_eq!(update, Update::ALL);
        assert!(state.find_session().is_none());
        assert_eq!(state.current_dir(), home);
    }

    #[test]
    fn toggle_hidden_clears_find_without_auto_rerun() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        assert!(state.find_session().is_some());

        state.dispatch(Action::ToggleHidden);

        assert!(state.find_session().is_none());
        assert!(state.show_hidden());
    }

    #[test]
    fn completion_error_produces_find_error_state() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();

        state.complete_find(
            generation,
            Err(io::Error::new(io::ErrorKind::NotFound, "gone")),
        );

        match state.find_session().unwrap().phase() {
            FindPhase::Error(message) => assert!(!message.is_empty()),
            other => panic!("expected Error, got {other:?}"),
        }
    }

    #[test]
    fn stale_error_completion_is_ignored() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::StartFind("a".to_string()));
        let stale_generation = state.find_session().unwrap().generation();
        state.dispatch(Action::StartFind("b".to_string()));

        let update = state.complete_find(
            stale_generation,
            Err(io::Error::new(io::ErrorKind::NotFound, "gone")),
        );

        assert_eq!(update, Update::NONE);
        assert!(matches!(
            state.find_session().unwrap().phase(),
            FindPhase::Searching
        ));
    }

    #[test]
    fn truncated_outcome_is_preserved_for_status() {
        let (dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = FindOutcome {
            results: vec![find_entry(dir.path().join("src").join("main.rs"))],
            truncated: true,
            skipped_errors: Vec::new(),
        };

        state.complete_find(generation, Ok(outcome));

        match state.find_session().unwrap().phase() {
            FindPhase::Ready(ready) => assert!(ready.truncated),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    #[test]
    fn skipped_error_count_is_preserved_for_status() {
        let (dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = FindOutcome {
            results: vec![find_entry(dir.path().join("src").join("main.rs"))],
            truncated: false,
            skipped_errors: vec![std::path::PathBuf::from("/some/blocked/dir")],
        };

        state.complete_find(generation, Ok(outcome));

        match state.find_session().unwrap().phase() {
            FindPhase::Ready(ready) => assert_eq!(ready.skipped_count, 1),
            other => panic!("expected Ready, got {other:?}"),
        }
    }

    // --- FIND preview_changed precision (M5T-B2 V2 audit) -------------------
    //
    // V1's `start_find`/`complete_find`/`clear_find` reported
    // `preview_changed: true` unconditionally, even across a `None` ->
    // `None` transition — rebuilding `PreviewContext` (and telling
    // `ui/window.rs` to re-sync it) for nothing. These pin the corrected,
    // identity-based rule (`finish_preview_transition`) at exactly the
    // boundary that used to be wrong: same source before/after -> `false`,
    // no rebuild.

    #[test]
    fn start_find_from_no_preview_does_not_report_preview_change() {
        let dir = TempDir::new(); // empty: no selection, no preview, going in
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        assert!(state.selected_entry().is_none());
        assert!(matches!(state.preview(), PreviewContext::None));

        let update = state.dispatch(Action::StartFind("main".to_string()));

        assert!(update.current_changed);
        assert!(!update.parent_changed);
        assert!(!update.preview_changed);
    }

    #[test]
    fn start_find_from_existing_preview_reports_preview_change() {
        let (_dir, mut state) = find_state();
        assert!(state.selected_entry().is_some());

        let update = state.dispatch(Action::StartFind("main".to_string()));

        assert!(update.preview_changed);
    }

    #[test]
    fn zero_result_completion_does_not_report_preview_change() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::StartFind("zzz-no-match".to_string()));
        let generation = state.find_session().unwrap().generation();

        let update = state.complete_find(generation, Ok(FindOutcome::default()));

        assert!(update.current_changed);
        assert!(!update.parent_changed);
        assert!(!update.preview_changed);
    }

    #[test]
    fn error_completion_does_not_report_preview_change() {
        let (_dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();

        let update = state.complete_find(
            generation,
            Err(io::Error::new(io::ErrorKind::NotFound, "gone")),
        );

        assert!(update.current_changed);
        assert!(!update.parent_changed);
        assert!(!update.preview_changed);
    }

    #[test]
    fn non_empty_completion_reports_preview_change() {
        let (dir, mut state) = find_state();
        state.dispatch(Action::StartFind("main".to_string()));
        let generation = state.find_session().unwrap().generation();
        let outcome = find_outcome(vec![find_entry(dir.path().join("src").join("main.rs"))]);

        let update = state.complete_find(generation, Ok(outcome));

        assert!(update.preview_changed);
    }

    #[test]
    fn clear_find_to_no_underlying_preview_does_not_report_preview_change() {
        let dir = TempDir::new(); // empty: normal/FILTER view has no preview either
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::StartFind("main".to_string())); // Searching: preview None
        assert!(matches!(state.preview(), PreviewContext::None));

        let update = state.dispatch(Action::ClearFind);

        assert!(update.current_changed);
        assert!(!update.preview_changed);
    }

    #[test]
    fn clear_find_restoring_underlying_preview_reports_preview_change() {
        let (_dir, mut state) = find_state(); // normal selection has a real preview
        state.dispatch(Action::StartFind("zzz-no-match".to_string())); // Searching: preview None

        let update = state.dispatch(Action::ClearFind);

        assert!(update.preview_changed);
    }

    // `stale_completion_remains_update_none` (audit item 8): already
    // covered exactly by `stale_completion_is_ignored` above (asserts
    // `update == Update::NONE`, which is stronger than just
    // `preview_changed == false` — nothing about the stale session is
    // touched at all) — not duplicated here per the audit's own
    // instruction.

    // --- CREATE (M5T-C2) --------------------------------------------------

    #[test]
    fn create_entry_with_empty_input_is_cancelled() {
        let dir = TempDir::new();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let outcome = state.create_entry("");

        assert!(matches!(outcome, CreateOutcome::Cancelled));
        assert_eq!(state.entries().len(), 0);
    }

    #[test]
    fn create_entry_without_trailing_slash_creates_a_file() {
        let dir = TempDir::new();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let outcome = state.create_entry("new-file.txt");

        assert!(matches!(outcome, CreateOutcome::Created(_)));
        let created = dir.path().join("new-file.txt");
        assert!(created.is_file());
        assert_eq!(
            state.selected_entry().map(FileEntry::path),
            Some(created.as_path())
        );
    }

    #[test]
    fn create_entry_with_trailing_slash_creates_a_directory() {
        let dir = TempDir::new();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let outcome = state.create_entry("new-dir/");

        assert!(matches!(outcome, CreateOutcome::Created(_)));
        let created = dir.path().join("new-dir");
        assert!(created.is_dir());
        assert_eq!(
            state.selected_entry().map(FileEntry::path),
            Some(created.as_path())
        );
    }

    #[test]
    fn create_entry_does_not_trim_the_literal_input() {
        let dir = TempDir::new();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let outcome = state.create_entry("  padded.txt  ");

        assert!(matches!(outcome, CreateOutcome::Created(_)));
        assert!(dir.path().join("  padded.txt  ").is_file());
    }

    #[test]
    fn create_entry_selects_the_new_entry_and_never_touches_parent() {
        let dir = TempDir::new();
        fs::write(dir.path().join("existing.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let outcome = state.create_entry("brand-new.txt");

        match outcome {
            CreateOutcome::Created(update) => {
                assert!(update.current_changed);
                assert!(!update.parent_changed);
            }
            other => panic!("expected Created, got {other:?}"),
        }
        assert_eq!(
            state
                .selected_entry()
                .map(|e| e.name().to_string_lossy().into_owned()),
            Some("brand-new.txt".to_string())
        );
    }

    /// M5T-C2 V2 audit fix #1: creating something *inside* the directory
    /// PREVIEW is already showing must refresh PREVIEW even though its
    /// *source* (the previewed directory itself) never changes identity —
    /// identity comparison alone (`finish_preview_transition`) cannot see a
    /// content-only change.
    ///
    /// M5T-C2 V3 audit strengthening: the original version of this test
    /// used "dir" as the *only* top-level entry, so `initial_selection()`
    /// (the old, buggy nested-create fallback) happened to rediscover
    /// "dir" anyway — masking exactly the V3 bug (a nested create falling
    /// through to the first entry instead of preserving the previous
    /// selection). This fixture has three top-level entries with "dir" not
    /// first, and explicitly moves selection onto it before CREATE, so a
    /// regression of the V3 fix would make this fail on `selected_entry`
    /// alone, independent of whatever PREVIEW's own assertions below prove.
    #[test]
    fn create_inside_previewed_directory_refreshes_preview() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("aaa")).unwrap();
        fs::create_dir(dir.path().join("dir")).unwrap();
        fs::write(dir.path().join("dir").join("old.txt"), b"").unwrap();
        fs::write(dir.path().join("zzz.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        // Sorted (directories first, then alphabetically): "aaa", "dir",
        // "zzz.txt" — the initial selection lands on "aaa", not "dir".
        assert_eq!(
            state.selected_entry().map(FileEntry::name),
            Some(std::ffi::OsStr::new("aaa"))
        );
        state.dispatch(Action::SelectNext);
        assert_eq!(
            state.selected_entry().map(FileEntry::name),
            Some(std::ffi::OsStr::new("dir"))
        );
        match state.preview() {
            PreviewContext::Directory(children) => {
                assert_eq!(children.len(), 1, "expected only old.txt before CREATE");
            }
            other => panic!("expected Directory preview, got {other:?}"),
        }

        let outcome = state.create_entry("dir/new.txt");

        match outcome {
            CreateOutcome::Created(update) => assert!(update.preview_changed),
            other => panic!("expected Created, got {other:?}"),
        }
        // The selected entry is still "dir" itself — identity never moved,
        // and crucially never fell back to "aaa" (the M5T-C2 V3 bug).
        assert_eq!(
            state.selected_entry().map(FileEntry::name),
            Some(std::ffi::OsStr::new("dir"))
        );
        match state.preview() {
            PreviewContext::Directory(children) => {
                let names: Vec<String> = children
                    .iter()
                    .map(|e| e.name().to_string_lossy().into_owned())
                    .collect();
                assert_eq!(names, vec!["new.txt".to_string(), "old.txt".to_string()]);
            }
            other => panic!("expected Directory preview, got {other:?}"),
        }
    }

    /// M5T-C2 V3 audit: a nested create (`created_path` not itself a
    /// top-level entry of `current_dir`) must preserve whatever was
    /// selected before, by real path identity, rather than falling through
    /// to `initial_selection` — dedicated from the PREVIEW test above so
    /// this specific selection contract has its own, minimal proof,
    /// independent of what PREVIEW happens to show. Three top-level
    /// entries, "dir" deliberately not first (nor last), so this cannot
    /// accidentally pass via `initial_selection()` or a lucky sort
    /// position.
    #[test]
    fn create_nested_path_preserves_previous_selection() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("aaa")).unwrap();
        fs::create_dir(dir.path().join("dir")).unwrap();
        fs::write(dir.path().join("zzz.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        // Sorted: "aaa", "dir", "zzz.txt" — select "dir" explicitly.
        state.dispatch(Action::SelectNext);
        assert_eq!(
            state.selected_entry().map(FileEntry::name),
            Some(std::ffi::OsStr::new("dir"))
        );

        let outcome = state.create_entry("dir/new.txt");

        assert!(
            dir.path().join("dir").join("new.txt").is_file(),
            "expected dir/new.txt to have been created on disk"
        );
        match outcome {
            CreateOutcome::Created(_) => {}
            other => panic!("expected Created, got {other:?}"),
        }
        // Must still be "dir", never "aaa" (the old buggy
        // `initial_selection` fallback) and never anything else.
        assert_eq!(
            state.selected_entry().map(FileEntry::name),
            Some(std::ffi::OsStr::new("dir"))
        );
    }

    /// The counterpart proof: a sibling created elsewhere, irrelevant to
    /// the directory currently selected/previewed, must not report a
    /// PREVIEW change — content invalidation is scoped exactly to "created
    /// inside the previewed directory", never a blanket "any CREATE
    /// rebuilds PREVIEW". FILTER (`"dir"`) is what keeps "dir" selected
    /// after the reload: the newly created "zzz.txt" would otherwise become
    /// the freshly-created entry `create_entry` itself selects (see its own
    /// doc comment), which would trivially also change identity and defeat
    /// the point of this test.
    #[test]
    fn create_irrelevant_sibling_does_not_report_preview_change_when_selection_stays() {
        let dir = TempDir::new();
        fs::create_dir(dir.path().join("dir")).unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::SetFilterQuery("dir".to_string()));
        assert_eq!(
            state.selected_entry().map(FileEntry::name),
            Some(std::ffi::OsStr::new("dir"))
        );

        let outcome = state.create_entry("zzz.txt");

        match outcome {
            CreateOutcome::Created(update) => assert!(!update.preview_changed),
            other => panic!("expected Created, got {other:?}"),
        }
        // "zzz.txt" doesn't match the "dir" filter, so selection must have
        // settled back onto "dir" rather than following the new entry.
        assert_eq!(
            state.selected_entry().map(FileEntry::name),
            Some(std::ffi::OsStr::new("dir"))
        );
        match state.preview() {
            PreviewContext::Directory(children) => assert!(children.is_empty()),
            other => panic!("expected an (empty) Directory preview, got {other:?}"),
        }
    }

    #[test]
    fn create_entry_conflict_leaves_the_filesystem_and_editor_state_untouched() {
        let dir = TempDir::new();
        fs::write(dir.path().join("exists.txt"), b"ORIGINAL").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        let entries_before = state.entries().len();

        let outcome = state.create_entry("exists.txt");

        match outcome {
            CreateOutcome::Failed(message) => assert!(!message.is_empty()),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(
            fs::read(dir.path().join("exists.txt")).unwrap(),
            b"ORIGINAL"
        );
        assert_eq!(state.entries().len(), entries_before);
    }

    #[test]
    fn create_entry_rejecting_a_parent_component_is_failed_not_a_panic() {
        let dir = TempDir::new();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let outcome = state.create_entry("../escaped.txt");

        assert!(matches!(outcome, CreateOutcome::Failed(_)));
        assert!(!dir.path().parent().unwrap().join("escaped.txt").exists());
    }

    #[test]
    fn create_entry_preserves_an_active_filter_query() {
        let dir = TempDir::new();
        fs::write(dir.path().join("aaa.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::SetFilterQuery("aaa".to_string()));

        state.create_entry("zzz.txt");

        assert_eq!(state.filter_query(), "aaa");
    }

    #[test]
    fn create_entry_hidden_by_the_active_filter_falls_back_to_first_visible() {
        let dir = TempDir::new();
        fs::write(dir.path().join("aaa.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::SetFilterQuery("aaa".to_string()));

        // "zzz.txt" does not match the "aaa" filter, so it must not become
        // selected even though it's now the newest entry.
        state.create_entry("zzz.txt");

        assert_eq!(
            state
                .selected_entry()
                .map(|e| e.name().to_string_lossy().into_owned()),
            Some("aaa.txt".to_string())
        );
    }

    #[test]
    fn create_entry_is_a_free_function_call_not_an_action_variant() {
        // Documents the architectural decision directly: CREATE never goes
        // through `dispatch`/`Action` (see `create_entry`'s own doc
        // comment) — there is deliberately no `Action::CreateEntry` variant
        // to construct here, unlike every other state transition in this
        // file's other tests.
        let dir = TempDir::new();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        let _: CreateOutcome = state.create_entry("standalone.txt");
        assert!(dir.path().join("standalone.txt").exists());
    }

    // --- RENAME (M5T-C2) ---------------------------------------------------

    #[test]
    fn rename_selected_with_no_selection_returns_no_selection() {
        let dir = TempDir::new(); // empty directory: nothing selected
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        assert_eq!(state.selected(), None);

        let outcome = state.rename_selected("anything.txt");

        assert!(matches!(outcome, RenameOutcome::NoSelection));
    }

    #[test]
    fn rename_selected_changes_basename_and_reselects_it() {
        let dir = TempDir::new();
        fs::write(dir.path().join("old.txt"), b"content").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let outcome = state.rename_selected("new.txt");

        match outcome {
            RenameOutcome::Renamed(update) => {
                assert!(update.current_changed);
                assert!(!update.parent_changed);
            }
            other => panic!("expected Renamed, got {other:?}"),
        }
        let renamed = dir.path().join("new.txt");
        assert!(renamed.is_file());
        assert!(!dir.path().join("old.txt").exists());
        assert_eq!(
            state.selected_entry().map(FileEntry::path),
            Some(renamed.as_path())
        );
    }

    #[test]
    fn rename_selected_to_the_same_name_reports_update_none() {
        let dir = TempDir::new();
        fs::write(dir.path().join("same.txt"), b"content").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let outcome = state.rename_selected("same.txt");

        match outcome {
            RenameOutcome::Renamed(update) => assert_eq!(update, Update::NONE),
            other => panic!("expected Renamed(Update::NONE), got {other:?}"),
        }
        assert!(dir.path().join("same.txt").exists());
    }

    #[test]
    fn rename_selected_conflict_leaves_both_entries_and_selection_untouched() {
        let dir = TempDir::new();
        fs::write(dir.path().join("aaa.txt"), b"AAA").unwrap();
        fs::write(dir.path().join("bbb.txt"), b"BBB").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        // Sorted alphabetically: "aaa.txt" is selected first.
        let selected_before = state
            .selected_entry()
            .map(FileEntry::path)
            .map(Path::to_path_buf);

        let outcome = state.rename_selected("bbb.txt");

        match outcome {
            RenameOutcome::Failed(message) => assert!(!message.is_empty()),
            other => panic!("expected Failed, got {other:?}"),
        }
        assert_eq!(fs::read(dir.path().join("aaa.txt")).unwrap(), b"AAA");
        assert_eq!(fs::read(dir.path().join("bbb.txt")).unwrap(), b"BBB");
        assert_eq!(
            state
                .selected_entry()
                .map(FileEntry::path)
                .map(Path::to_path_buf),
            selected_before
        );
    }

    #[test]
    fn rename_selected_rejects_an_empty_new_name() {
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());

        let outcome = state.rename_selected("");

        assert!(matches!(outcome, RenameOutcome::Failed(_)));
        assert!(dir.path().join("a.txt").exists());
    }

    #[test]
    fn rename_selected_preserves_an_active_filter_query() {
        let dir = TempDir::new();
        fs::write(dir.path().join("aaa.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::SetFilterQuery("aaa".to_string()));

        state.rename_selected("aaa-renamed.txt");

        assert_eq!(state.filter_query(), "aaa");
    }

    #[test]
    fn rename_selected_hidden_by_the_active_filter_falls_back_to_first_visible() {
        let dir = TempDir::new();
        fs::write(dir.path().join("aaa.txt"), b"").unwrap();
        fs::write(dir.path().join("aaa-second.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        state.dispatch(Action::SetFilterQuery("aaa".to_string()));
        // Sorted alphabetically, "-" (0x2D) sorts before "." (0x2E):
        // "aaa-second.txt" is index 0 and so the initial selection.
        assert_eq!(
            state.selected_entry().map(FileEntry::name),
            Some(std::ffi::OsStr::new("aaa-second.txt"))
        );

        // Renaming the selected "aaa-second.txt" to something the "aaa"
        // filter no longer matches must not leave it selected-but-invisible.
        state.rename_selected("zzz.txt");

        assert_eq!(
            state
                .selected_entry()
                .map(|e| e.name().to_string_lossy().into_owned()),
            Some("aaa.txt".to_string())
        );
    }

    #[test]
    fn rename_selected_is_a_free_function_call_not_an_action_variant() {
        // Same architectural point as `create_entry_is_a_free_function_call_
        // not_an_action_variant`: there is deliberately no
        // `Action::RenameSelected` variant.
        let dir = TempDir::new();
        fs::write(dir.path().join("a.txt"), b"").unwrap();
        let mut state = test_state(Navigation::new(dir.path().to_path_buf()).unwrap());
        let _: RenameOutcome = state.rename_selected("b.txt");
        assert!(dir.path().join("b.txt").exists());
    }
}
