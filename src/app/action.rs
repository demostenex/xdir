/// Every input (keyboard or mouse) converges on one of these before it can
/// change [`super::AppState`]. Nothing outside `app` mutates state
/// directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    SelectNext,
    SelectPrevious,
    SelectIndex(usize),
    ActivateSelected,
    ActivateIndex(usize),
    GoParent,
    ToggleHidden,
}
