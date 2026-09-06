use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, StandardListViewItem, VecModel};

use crate::app::{Action, AppState, PreviewContext};
use crate::model::{EntryKind, FileEntry};
use crate::ui::input::{self, KeyStroke};

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

    full_refresh_current(&state, &ui);
    refresh_parent(&state, &ui);
    refresh_preview(&state, &ui);
    ui.invoke_focus_list();

    // `.slint` only names the physical key (a literal character, or one
    // of the "Up"/"Down"/"Left"/"Right" tags it substitutes for the arrow
    // keys it alone can recognize) and reports raw modifier flags; it
    // never decides what any of that means. `KeyStroke::new` turns that
    // into a toolkit-neutral value and `input::resolve` (the keymap) is
    // the one place that says "j and Down both mean select-next". The
    // returned bool tells `.slint` whether to consume the event
    // (`Handled`) or leave it alone (`Ignored`), e.g. for the window
    // manager.
    ui.on_key_input({
        let state = state.clone();
        let ui = ui.as_weak();
        move |raw, shift, control, alt, meta| {
            let ui = ui.unwrap();
            let Some(stroke) = KeyStroke::new(&raw, shift, control, alt, meta) else {
                return false;
            };
            let Some(action) = input::resolve(stroke) else {
                return false;
            };
            apply(&state, &ui, action);
            true
        }
    });

    // StandardListView's own click handling (built-in, and already
    // left-button-only) moved the selection and scrolled it into view
    // before this fires; this mirrors that into AppState through the same
    // `apply` every other input goes through — a plain `dispatch` here
    // (as before Milestone 3's PREVIEW pane) updated `AppState` correctly
    // but never told `ui` to redraw PREVIEW, since only `apply` calls
    // `refresh_preview`. `select_index`'s no-op-on-unchanged-index guard
    // (see its doc comment) keeps this safe even though Slint's own
    // `current-item-changed` already reflects the click.
    ui.on_selection_changed({
        let state = state.clone();
        let ui = ui.as_weak();
        move |index| {
            if index >= 0 {
                let ui = ui.unwrap();
                apply(&state, &ui, Action::SelectIndex(index as usize));
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

/// Runs `action` through `AppState` and syncs whichever of CURRENT/PARENT/
/// PREVIEW it reports as changed — and does *no* UI work at all for
/// `Update::NONE` (e.g. `j` already on the last entry, or an out-of-range
/// index), since nothing actually changed.
///
/// `AppState::dispatch` returns an [`crate::app::Update`] saying exactly
/// which contexts it recomputed, so this never has to *infer* that by
/// diffing entry counts or any other derived signal the way Milestone 2's
/// `apply` did — that approach missed a real bug (`ToggleHidden` changed
/// the listing without changing `current_dir`, so a directory-only check
/// silently skipped the refresh) and would just as easily miss a same-
/// length-but-different-content listing.
///
/// Neither this nor any helper it calls may hold a live `Ref`/`RefMut` on
/// `state` while calling into `ui`: `invoke_set_selection` synchronously
/// triggers `StandardListView::set-current-item`, which fires
/// `current-item-changed` back into `on_selection_changed`, which itself
/// calls `state.borrow_mut()`. A `SelectNext`/`SelectPrevious` used to hold
/// `let st = state.borrow();` across exactly that call, so every plain
/// `j`/`k` press re-entered the same `RefCell` and panicked ("already
/// borrowed") — killing the whole process. Every helper below takes
/// `&Rc<RefCell<AppState>>` and borrows only long enough to copy out the
/// plain values it needs, so the borrow is gone before any `ui.*`/
/// `invoke_*` call happens.
///
/// That same `sync_selection` round-trip is also why `select_by`/
/// `select_index` in `AppState` treat re-selecting the already-selected
/// index as a no-op: `sync_selection` below calls `invoke_set_selection`,
/// which fires `current-item-changed` back into `on_selection_changed`,
/// which dispatches `SelectIndex` a second time with the very index
/// `AppState` just set — without that no-op check, PREVIEW would be
/// rebuilt twice per keyboard press.
fn apply(state: &Rc<RefCell<AppState>>, ui: &MainWindow, action: Action) {
    let update = state.borrow_mut().dispatch(action);
    if update.current_changed {
        full_refresh_current(state, ui);
    } else if update.preview_changed {
        sync_selection(state, ui);
    }
    if update.parent_changed {
        refresh_parent(state, ui);
    }
    if update.preview_changed {
        refresh_preview(state, ui);
    }
}

fn full_refresh_current(state: &Rc<RefCell<AppState>>, ui: &MainWindow) {
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

/// A row's display label: a trailing "/" for directories, the bare name
/// otherwise. Built here (never in `.slint`, which never inspects
/// `EntryKind`) since it's the one place already converting `FileEntry`
/// into UI-facing text.
fn row_label(entry: &FileEntry) -> String {
    let name = entry.name().to_string_lossy();
    if entry.kind() == EntryKind::Directory {
        format!("{name}/")
    } else {
        name.into_owned()
    }
}

fn context_rows(entries: &[FileEntry], highlighted_index: Option<usize>) -> Vec<ContextRow> {
    entries
        .iter()
        .enumerate()
        .map(|(i, entry)| ContextRow {
            text: row_label(entry).into(),
            highlighted: Some(i) == highlighted_index,
        })
        .collect()
}

/// PARENT never needs the CURRENT/PREVIEW borrow-reentrancy dance: it has
/// no interactive `.slint` widget wired to a callback that could call back
/// into `state`, so there's nothing to keep this borrow scoped away from —
/// but it's still dropped before the `ui.set_*` calls for consistency with
/// every other helper here.
fn refresh_parent(state: &Rc<RefCell<AppState>>, ui: &MainWindow) {
    let (has_parent, rows) = {
        let st = state.borrow();
        let parent = st.parent();
        (
            parent.dir().is_some(),
            context_rows(parent.entries(), parent.current_index()),
        )
    };
    ui.set_has_parent(has_parent);
    ui.set_parent_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
}

fn refresh_preview(state: &Rc<RefCell<AppState>>, ui: &MainWindow) {
    let (mode, rows, label) = {
        let st = state.borrow();
        match st.preview() {
            PreviewContext::None => ("none", Vec::new(), String::new()),
            PreviewContext::Directory(children) => {
                ("directory", context_rows(children, None), String::new())
            }
            PreviewContext::DirectoryUnavailable => (
                "unavailable",
                Vec::new(),
                preview_label(&st, "Directory", "Unavailable"),
            ),
            PreviewContext::File => (
                "file",
                Vec::new(),
                preview_label(&st, "Regular file", "Preview not implemented yet"),
            ),
            PreviewContext::Symlink => (
                "symlink",
                Vec::new(),
                preview_label(&st, "Symlink", "Preview not implemented yet"),
            ),
            PreviewContext::Other => (
                "other",
                Vec::new(),
                preview_label(&st, "Other", "Preview not implemented yet"),
            ),
        }
        // `st` is dropped here, before any `ui.set_*` call.
    };
    ui.set_preview_mode(mode.into());
    ui.set_preview_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_preview_label(label.into());
}

/// Builds a PREVIEW placeholder message for a non-directory (or unreadable-
/// directory) selection: the selected entry's name, its kind, and a note
/// that this is context only — Milestone 3 never reads file content, MIME,
/// or generates a thumbnail; that's Milestone 4 (preview *content*
/// providers).
fn preview_label(state: &AppState, kind: &str, note: &str) -> String {
    let name = state
        .selected_entry()
        .map(|entry| entry.name().to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("{name}\n\n{kind}\n\n{note}")
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
