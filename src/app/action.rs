use crate::core::places::SystemPlaceKind;

/// Every input (keyboard or mouse) converges on one of these before it can
/// change [`super::AppState`]. Nothing outside `app` mutates state
/// directly.
///
/// No longer `Copy` as of the FILTER foundation (M5T-A): `SetFilterQuery`
/// carries an owned `String`, the live query text typed into FILTER's
/// input box. Every other variant is still trivially `Clone`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SelectNext,
    SelectPrevious,
    SelectIndex(usize),
    SelectFirst,
    SelectLast,
    ActivateSelected,
    ActivateIndex(usize),
    GoParent,
    GoSystemPlace(SystemPlaceKind),
    ToggleHidden,
    /// Replaces FILTER's active query outright (never appended/diffed) and
    /// re-derives CURRENT's visible entries from it — see
    /// [`super::AppState`]'s `set_filter_query`. Sent on every keystroke
    /// while FILTER is being edited, not just on commit: the query already
    /// filters CURRENT live, so "commit" (Enter) never has to touch
    /// `AppState` at all, only UI focus.
    SetFilterQuery(String),
    /// Clears FILTER's query back to empty, restoring the full listing. A
    /// no-op (`Update::NONE`) when no filter is active, exactly like any
    /// other already-there action in this enum.
    ClearFilter,
    /// Commits a non-empty FIND query (M5T-B2) and starts a new recursive
    /// search snapshot (`root`/`include_hidden` taken from `current_dir`/
    /// `show_hidden` at this exact moment, under a freshly-incremented
    /// generation). Unlike `SetFilterQuery`, this is dispatched exactly
    /// once per search — while FIND's box is being edited, the draft text
    /// lives only in the UI (`ui/main.slint`'s `find-query`) and never
    /// reaches `AppState` until Enter. Actually running the traversal is
    /// `ui/window.rs`'s job (spawning a `std::thread` and calling
    /// `core::find::find_recursive` off the UI thread); this action only
    /// records the request and flips FIND to `Searching`.
    StartFind(String),
    /// Clears FIND back to inactive, restoring the underlying FILTER/normal
    /// view. Also what invalidates a pending search: the completion arrives
    /// later and checks its generation against the (now different, or
    /// simply absent) current one — see `AppState::complete_find`. A no-op
    /// when FIND isn't active.
    ClearFind,
    /// `Esc` with no editor focused: cancels whichever transient view is on
    /// top — FIND if active, else FILTER, else a true no-op. Never a
    /// generalized "modes" stack; just this one frozen priority order (see
    /// `AppState::cancel_current_view`).
    CancelCurrentView,
}
