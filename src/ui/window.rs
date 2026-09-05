use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, StandardListViewItem, VecModel};

use crate::app::{Action, AppState};

slint::include_modules!();

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// What a press should do, decided purely from click history — no
/// filesystem/AppState knowledge here, just the timing/index/button rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClickOutcome {
    /// A first click, a click on a different row, one that arrived after
    /// the double-click window closed, or a non-left-button press: select
    /// only.
    Select,
    /// A second left click on the same row, within the window: activate.
    Activate,
}

/// Turns a sequence of row presses into single/double-click outcomes.
///
/// `StandardListView::item-pointer-event` reports every press (unlike
/// `current-item-changed`, which stays silent on a repeat click of the
/// already-selected row), but carries no click-count — so double-click
/// detection is our own timing state, kept deliberately tiny and confined
/// to the UI layer. `is_left` is a plain `bool` (not the Slint button enum)
/// so this struct has no `slint::` dependency and is trivially unit-tested.
struct ClickTracker {
    last_left_click: Option<(usize, Instant)>,
    window: Duration,
}

impl ClickTracker {
    fn new(window: Duration) -> Self {
        ClickTracker {
            last_left_click: None,
            window,
        }
    }

    /// Registers a press at `index`. A press with `is_left == false` never
    /// participates in the sequence at all — it is neither treated as a
    /// click nor does it reset or extend an in-progress one, and it never
    /// produces `Activate`.
    fn register(&mut self, index: usize, is_left: bool) -> ClickOutcome {
        if !is_left {
            return ClickOutcome::Select;
        }
        let is_double = matches!(
            self.last_left_click,
            Some((i, t)) if i == index && t.elapsed() < self.window
        );
        self.last_left_click = Some((index, Instant::now()));
        if is_double {
            ClickOutcome::Activate
        } else {
            ClickOutcome::Select
        }
    }
}

/// Builds the Slint window over `state` and runs the event loop. This
/// module is the only place in the crate allowed to name a `slint` type;
/// `core`/`model`/`app` never see one.
pub fn run(state: AppState) -> Result<(), slint::PlatformError> {
    let ui = MainWindow::new()?;
    let state = Rc::new(RefCell::new(state));
    let clicks = Rc::new(RefCell::new(ClickTracker::new(DOUBLE_CLICK_WINDOW)));

    full_refresh(&state, &ui);
    ui.invoke_focus_list();

    ui.on_key_action({
        let state = state.clone();
        let ui = ui.as_weak();
        move |text| {
            let ui = ui.unwrap();
            let action = match text.as_str() {
                "j" => Some(Action::SelectNext),
                "k" => Some(Action::SelectPrevious),
                "l" | "\n" => Some(Action::ActivateSelected),
                "h" => Some(Action::GoParent),
                _ => None,
            };
            let Some(action) = action else { return };
            apply(&state, &ui, action);
        }
    });

    // StandardListView's own click handling (built-in, and already
    // left-button-only) moved the selection and scrolled it into view
    // before this fires; we only need to mirror that into AppState.
    ui.on_selection_changed({
        let state = state.clone();
        move |index| {
            if index >= 0 {
                state
                    .borrow_mut()
                    .dispatch(Action::SelectIndex(index as usize));
            }
        }
    });

    // `current-item-changed` stays silent on a repeat click of the
    // already-selected row, so double-click can't be detected from it.
    // `item-pointer-event` fires on every press regardless — including
    // right/middle clicks, which `ClickTracker` ignores outright.
    ui.on_item_pressed({
        let state = state.clone();
        let ui = ui.as_weak();
        let clicks = clicks.clone();
        move |index, is_left| {
            let ui = ui.unwrap();
            let index = index as usize;
            let outcome = clicks.borrow_mut().register(index, is_left);
            if outcome == ClickOutcome::Activate {
                apply(&state, &ui, Action::ActivateIndex(index));
            }
            // `Select` needs no action here: a left click on a new row is
            // already selected via `current-item-changed` above, and a
            // non-left press never selects anything (StandardListView's
            // own click handling is left-button-only too).
        }
    });

    ui.run()
}

/// Runs `action` through `AppState` and syncs the UI: a full rebuild if the
/// directory changed, or just the selection otherwise.
///
/// Neither branch may hold a live `Ref`/`RefMut` on `state` while calling
/// into `ui`: `invoke_set_selection` synchronously triggers
/// `StandardListView::set-current-item`, which fires `current-item-changed`
/// back into `on_selection_changed`, which itself calls
/// `state.borrow_mut()`. A `SelectNext`/`SelectPrevious` used to hold `let
/// st = state.borrow();` across exactly that call, so every plain `j`/`k`
/// press re-entered the same `RefCell` and panicked
/// ("already borrowed") — killing the whole process. Every helper below
/// takes `&Rc<RefCell<AppState>>` and borrows only long enough to copy out
/// the plain values it needs, so the borrow is gone before any `ui.*`/
/// `invoke_*` call happens.
fn apply(state: &Rc<RefCell<AppState>>, ui: &MainWindow, action: Action) {
    let dir_before = state.borrow().current_dir().to_path_buf();
    state.borrow_mut().dispatch(action);
    let dir_changed = state.borrow().current_dir() != dir_before;
    if dir_changed {
        full_refresh(state, ui);
    } else {
        sync_selection(state, ui);
    }
}

fn full_refresh(state: &Rc<RefCell<AppState>>, ui: &MainWindow) {
    let (items, path_text, status_text, index) = {
        let st = state.borrow();
        let items: Vec<StandardListViewItem> = st
            .entries()
            .iter()
            .map(|entry| StandardListViewItem::from(entry.name().to_string_lossy().as_ref()))
            .collect();
        let path_text = st.current_dir().display().to_string();
        let status_text = format!("{} items", st.entries().len());
        let index = st.selected().map(|i| i as i32).unwrap_or(-1);
        (items, path_text, status_text, index)
        // `st` (the borrow) is dropped here, before any `ui`/`invoke_*` call.
    };
    ui.set_entries(ModelRc::from(Rc::new(VecModel::from(items))));
    ui.set_path_text(path_text.into());
    ui.set_status_text(status_text.into());
    ui.invoke_reset_scroll();
    ui.invoke_set_selection(index);
}

fn sync_selection(state: &Rc<RefCell<AppState>>, ui: &MainWindow) {
    let index = state.borrow().selected().map(|i| i as i32).unwrap_or(-1);
    ui.invoke_set_selection(index);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_left_clicks_on_the_same_index_within_the_window_activate() {
        let mut clicks = ClickTracker::new(Duration::from_millis(400));

        assert_eq!(clicks.register(2, true), ClickOutcome::Select);
        assert_eq!(clicks.register(2, true), ClickOutcome::Activate);
    }

    #[test]
    fn clicks_on_different_indices_never_activate() {
        let mut clicks = ClickTracker::new(Duration::from_millis(400));

        assert_eq!(clicks.register(2, true), ClickOutcome::Select);
        assert_eq!(clicks.register(7, true), ClickOutcome::Select);
        // Clicking back on 2 right after clicking 7 is a fresh first click,
        // not a double-click against the earlier press on 2.
        assert_eq!(clicks.register(2, true), ClickOutcome::Select);
    }

    #[test]
    fn a_second_click_after_the_window_closes_does_not_activate() {
        let mut clicks = ClickTracker::new(Duration::from_millis(0));

        assert_eq!(clicks.register(3, true), ClickOutcome::Select);
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(clicks.register(3, true), ClickOutcome::Select);
    }

    #[test]
    fn non_left_buttons_never_participate_in_the_sequence() {
        let mut clicks = ClickTracker::new(Duration::from_millis(400));

        // A right click never activates by itself...
        assert_eq!(clicks.register(4, false), ClickOutcome::Select);
        assert_eq!(clicks.register(4, false), ClickOutcome::Select);

        // ...and does not corrupt a real left-click sequence around it:
        // left, right (ignored), left again on the same index still
        // activates.
        assert_eq!(clicks.register(4, true), ClickOutcome::Select);
        assert_eq!(clicks.register(4, false), ClickOutcome::Select);
        assert_eq!(clicks.register(4, true), ClickOutcome::Activate);
    }
}
