use std::path::Path;

use crate::app::{Action, ParentContext, PreviewContext};
use crate::core::navigation::Navigation;
use crate::core::places::{self, Place, SystemPlaceKind};
use crate::model::FileEntry;

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
                // `index` is a position in the currently *visible*
                // (filtered) list — exactly what a click reports it
                // against, since that's the list CURRENT renders. Equal to
                // a full-list index whenever no filter is active.
                if index >= self.visible_indices().len() {
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
        }
    }

    /// Clamped move by `delta`, entirely within the currently *visible*
    /// (filtered) ordering — `j`/`k`/arrows never land on a filtered-out
    /// entry. Identical to a plain full-list move when no filter is
    /// active. A no-op when it doesn't actually change the selection (e.g.
    /// already on the last visible entry and moving further down) reports
    /// `Update::NONE` rather than `PREVIEW_ONLY`: nothing changed, so
    /// nothing should be re-read.
    fn select_by(&mut self, delta: i32) -> Update {
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

    /// Selects the entry at `visible_index`, a position in the currently
    /// *visible* (filtered) list — exactly what a click reports it
    /// against, and what `select_first`/`select_last` pass in. Equal to a
    /// full-list index whenever no filter is active. A no-op both for an
    /// out-of-range index and for re-selecting the entry that's already
    /// selected — the latter matters because the UI layer's own
    /// selection-sync (`sync_selection`) round-trips through Slint's
    /// `current-item-changed` back into this same call with the index it
    /// was just told to set; without this check that round-trip would
    /// rebuild PREVIEW a second time for nothing.
    fn select_index(&mut self, visible_index: usize) -> Update {
        let visible = self.visible_indices();
        let Some(&full_index) = visible.get(visible_index) else {
            return Update::NONE;
        };
        if self.selected == Some(full_index) {
            return Update::NONE;
        }
        self.selected = Some(full_index);
        self.refresh_preview();
        Update::PREVIEW_ONLY
    }

    /// Selects the first *visible* entry (`gg`). A no-op — `Update::NONE`,
    /// no preview rebuild — both when nothing is currently visible and when
    /// the first visible entry is already selected, via the same
    /// re-selection guard as [`Self::select_index`].
    fn select_first(&mut self) -> Update {
        if self.visible_indices().is_empty() {
            Update::NONE
        } else {
            self.select_index(0)
        }
    }

    /// Selects the last *visible* entry (`G`). Same no-op guarantees as
    /// [`Self::select_first`], mirrored for the other end of the visible
    /// list.
    fn select_last(&mut self) -> Update {
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
    fn activate_selected(&mut self) -> Update {
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
    /// (those return `Update::NONE` before ever reaching here). FILTER is
    /// scoped to one directory's listing, so it's cleared unconditionally
    /// here: a new directory never inherits the previous one's query.
    fn reload(&mut self) {
        self.entries = self.navigation.entries().unwrap_or_default();
        self.selected = initial_selection(&self.entries);
        self.filter_query.clear();
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
        self.preview = PreviewContext::build(self.selected_entry(), self.navigation.show_hidden());
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
}
