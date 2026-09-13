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
}
