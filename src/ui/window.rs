use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use slint::{ComponentHandle, ModelRc, VecModel};

use crate::app::{Action, AppState, FIND_MAX_RESULTS, FilePreview, FindPhase, PreviewContext};
use crate::model::{EntryKind, FileEntry};
use crate::ui::input::{KeyStroke, Keymap, KeymapResult};

/// What a FIND worker thread hands back — always sent through
/// `find_tx`/`find_rx` (never captured directly by a `Weak::
/// upgrade_in_event_loop` closure, which must be `Send` and so could never
/// hold `Rc<RefCell<AppState>>` anyway; see `begin_find_search`'s own doc
/// comment for the full reasoning). Both fields are plain `Send` data —
/// `u64` and `io::Result<core::find::FindOutcome>` (a `Vec<FileEntry>` of
/// owned `PathBuf`/`OsString`/`EntryKind`, nothing toolkit-specific) — so
/// this can cross the thread boundary with zero `unsafe`.
struct FindCompletion {
    generation: u64,
    result: std::io::Result<crate::core::find::FindOutcome>,
}

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
    let keymap = Rc::new(RefCell::new(Keymap::new()));

    // FIND's worker→UI-thread bridge (M5T-B2). `find_rx` never leaves this
    // function's scope — it's moved once into the one closure below that
    // drains it — so there is exactly one reader, on the UI thread, always.
    // `find_tx` is `Clone`+`Send`; each search spawned by
    // `begin_find_search` gets its own clone to move into its worker
    // thread. See `begin_find_search`'s doc comment for why a channel is
    // the right shape here at all (in short: `Rc<RefCell<AppState>>` isn't
    // `Send`, so nothing crossing the thread boundary can carry it — only
    // plain data can, and only a closure already living on the UI thread,
    // set up right here, can apply that data to `state`).
    let (find_tx, find_rx) = mpsc::channel::<FindCompletion>();

    full_refresh_current(&state, &ui);
    refresh_parent(&state, &ui);
    refresh_preview(&state, &ui);
    ui.invoke_focus_list();

    // A worker calls this indirectly — via `ui_weak.upgrade_in_event_loop`
    // invoking `invoke_find_completion_ready()` on the real `ui` handle —
    // purely to wake this closure up; the actual completion payload always
    // travels through `find_tx`/`find_rx`, never as an argument here (Slint
    // callback parameters can't carry a `Vec<FileEntry>`/`io::Result`
    // anyway). Draining in a loop (`try_recv`, not `recv`) means a second
    // completion that arrived before this ran isn't left stranded until
    // some unrelated future wakeup.
    ui.on_find_completion_ready({
        let state = state.clone();
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            while let Ok(completion) = find_rx.try_recv() {
                let update = state
                    .borrow_mut()
                    .complete_find(completion.generation, completion.result);
                apply_update(&state, &ui, update);
            }
        }
    });

    // `.slint` only names the physical key (a literal character, or one
    // of the "Up"/"Down"/"Left"/"Right"/"Escape"/"Shift" tags it
    // substitutes for the keys only it can recognize) and reports raw
    // modifier flags; it
    // never decides what any of that means. `KeyStroke::new` turns that
    // into a toolkit-neutral value and `Keymap` (the keymap itself) is the
    // one place that says "j and Down both mean select-next" — and, since
    // some xdir commands are two keys long (`gg`, `g h`, ...), the one
    // place holding the one bit of state that remembers a lone `g` until
    // the next keystroke decides what it meant. The returned bool tells
    // `.slint` whether to consume the event (`Handled`) or leave it alone
    // (`Ignored`), e.g. for the window manager.
    ui.on_key_input({
        let state = state.clone();
        let keymap = keymap.clone();
        let ui = ui.as_weak();
        move |raw, shift, control, alt, meta| {
            let ui = ui.unwrap();
            let Some(stroke) = KeyStroke::new(&raw, shift, control, alt, meta) else {
                // A key `KeyStroke::new` can't even name (e.g. a function
                // key) still counts as "a new keystroke arrived" for the
                // pending-`g` contract: it must not survive to let some
                // later, unrelated key complete it as if it were `g`'s own
                // continuation. The event itself stays fully unhandled —
                // this only ever drops a stale prefix, never claims the
                // keystroke or produces an `Action`.
                keymap.borrow_mut().cancel_pending();
                return false;
            };
            // `keymap.borrow_mut()` must not still be live once `apply`
            // runs. Before M5V, a successful `GoSystemPlace`/`SelectFirst`/
            // ... synced selection through `StandardListView`'s own
            // `current-item-changed`, which re-entered `on_selection_changed`
            // below and called `keymap.borrow_mut().cancel_pending()` — the
            // same reentrancy hazard `apply`'s own doc comment describes for
            // `state`. `apply` no longer routes through any callback that
            // could do that (see its doc comment), but using the temporary
            // as the `match` scrutinee directly would still extend its
            // borrow across the whole match, so the result is copied out
            // first regardless.
            let result = keymap.borrow_mut().resolve(stroke);
            match result {
                KeymapResult::Action(action) => {
                    apply(&state, &ui, action);
                    true
                }
                // A lone `g` consumed while the keymap waits for its
                // continuation, or a whole invalid sequence (an
                // unrecognized continuation, or `Esc`) cancelled outright:
                // either way nothing reaches `AppState`, but the keystroke
                // itself is still ours, not the window manager's.
                KeymapResult::Pending | KeymapResult::Cancelled => true,
                // `/`: no `AppState` change (see `KeymapResult::EnterFilter`'s
                // own doc comment) — just show FILTER's input, pre-filled
                // with whatever query is already active, and move keyboard
                // focus to it in the same tick.
                KeymapResult::EnterFilter => {
                    begin_filter_edit(&state, &ui);
                    true
                }
                // `f`: same shape as `/` above — no `AppState` change (see
                // `KeymapResult::EnterFind`'s own doc comment), just show
                // FIND's input, pre-filled with whatever query is already
                // committed, and move keyboard focus to it.
                KeymapResult::EnterFind => {
                    begin_find_edit(&state, &ui);
                    true
                }
                KeymapResult::Unhandled => false,
            }
        }
    });

    // FILTER's input box reports its live text here on every keystroke
    // (typing, Backspace, paste, ...) — never just on commit — since the
    // query already filters CURRENT live while the box is open (see
    // `ui/main.slint`'s `filter-input`/`AppState::set_filter_query`).
    ui.on_filter_edited({
        let state = state.clone();
        let ui = ui.as_weak();
        move |text| {
            let ui = ui.unwrap();
            apply(&state, &ui, Action::SetFilterQuery(text.to_string()));
        }
    });

    // Enter, while FILTER is being edited: `AppState` needs no call at all
    // — the query already filtered CURRENT live as it was typed — this is
    // purely "close the box, give keyboard focus back to CURRENT" so a `j`
    // typed right after navigates instead of vanishing into a hidden text
    // box.
    ui.on_filter_accepted({
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            ui.set_filter_editing(false);
            ui.invoke_focus_list();
        }
    });

    // Esc, while FILTER is being edited: unlike Enter, this does reach
    // `AppState` — the milestone's frozen rule is that Esc during editing
    // clears the filter outright (no draft-vs-committed distinction), not
    // just closes the box on whatever was last typed.
    ui.on_filter_escaped({
        let state = state.clone();
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            apply(&state, &ui, Action::ClearFilter);
            ui.set_filter_editing(false);
            ui.invoke_focus_list();
        }
    });

    // Enter, while FIND is being edited: unlike FILTER, this *does* reach
    // the filesystem — but only past this point (see `begin_find_search`).
    // An empty query never starts a worker at all (§5's frozen rule): it
    // just closes the box and clears whatever FIND session existed.
    ui.on_find_accepted({
        let state = state.clone();
        let ui = ui.as_weak();
        let find_tx = find_tx.clone();
        move || {
            let ui = ui.unwrap();
            let query = ui.get_find_query().to_string();
            ui.set_find_editing(false);
            if query.is_empty() {
                apply(&state, &ui, Action::ClearFind);
            } else {
                begin_find_search(&state, &ui, &find_tx, query);
            }
            ui.invoke_focus_list();
        }
    });

    // Esc, while FIND is being edited: clears FIND outright (same
    // no-draft-vs-committed rule FILTER's own Esc-while-editing follows) —
    // this also invalidates any search still running for the *previous*
    // committed query, if `f` reopened an active FIND session and the user
    // then cancelled instead of resubmitting (see
    // `AppState::complete_find`'s generation check).
    ui.on_find_escaped({
        let state = state.clone();
        let ui = ui.as_weak();
        move || {
            let ui = ui.unwrap();
            apply(&state, &ui, Action::ClearFind);
            ui.set_find_editing(false);
            ui.invoke_focus_list();
        }
    });

    // CURRENT's row `TouchArea` (see `ui/main.slint`) reports every left
    // press here, unconditionally — including a repeat press on the
    // already-selected row. `select_index`'s no-op-on-unchanged-index guard
    // (see its doc comment) is what makes that safe: this mirrors the
    // click into `AppState` through the same `apply` every other input goes
    // through, exactly as before M5V's `StandardListView` replacement, and
    // a redundant re-selection simply reports `Update::NONE`.
    //
    // A mouse selection is a fresh interaction unrelated to any in-flight
    // keyboard sequence, so it cancels a pending `g` first — otherwise `g`,
    // click elsewhere, `h` would resolve as `Home` instead of the ordinary
    // `GoParent` the stray `h` alone should mean.
    ui.on_selection_changed({
        let state = state.clone();
        let keymap = keymap.clone();
        let ui = ui.as_weak();
        move |index| {
            keymap.borrow_mut().cancel_pending();
            if index >= 0 {
                let ui = ui.unwrap();
                apply(&state, &ui, Action::SelectIndex(index as usize));
            }
        }
    });

    // The row `TouchArea` reports every press here regardless of button —
    // including right/middle clicks, which `ClickTracker` ignores outright
    // but which still count as a fresh mouse interaction, so a pending `g`
    // is cancelled here unconditionally too, before the click/button kind
    // is even inspected.
    ui.on_item_pressed({
        let state = state.clone();
        let ui = ui.as_weak();
        let clicks = clicks.clone();
        let keymap = keymap.clone();
        move |index, is_left| {
            keymap.borrow_mut().cancel_pending();
            let ui = ui.unwrap();
            let index = index as usize;
            let outcome = clicks.borrow_mut().register(index, is_left);
            if outcome == ClickOutcome::Activate {
                apply(&state, &ui, Action::ActivateIndex(index));
            }
            // `Select` needs no action here: a left press already reports
            // `selection-changed` on its own (see `ui/main.slint`'s row
            // `TouchArea`), and a non-left press never selects anything.
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
/// `state` while calling into `ui`. Before M5V, syncing selection called
/// `StandardListView::set-current-item`, which synchronously fired
/// `current-item-changed` back into `on_selection_changed` — itself a
/// `state.borrow_mut()` call. A `SelectNext`/`SelectPrevious` that held
/// `let st = state.borrow();` across exactly that call re-entered the same
/// `RefCell` and panicked ("already borrowed") — killing the whole
/// process. `current-index`/`scroll-to-index` (M5V's own row-selection
/// API, see `ui/main.slint`) are plain property writes and a function that
/// only touches `viewport-y`; neither fires any callback back into Rust, so
/// that specific reentrancy is no longer reachable through this call chain
/// — but every helper below still borrows only long enough to copy out the
/// plain values it needs, so the discipline holds regardless of how a
/// future change might route selection back through a real callback.
///
/// `select_by`/`select_index` in `AppState` still treat re-selecting the
/// already-selected index as a no-op independently of this: a mouse press
/// on the already-current row calls `dispatch(SelectIndex(..))` with the
/// same index `AppState` already holds (see `on_selection_changed` below),
/// and without that no-op check PREVIEW would be rebuilt for nothing.
fn apply(state: &Rc<RefCell<AppState>>, ui: &MainWindow, action: Action) {
    let update = state.borrow_mut().dispatch(action);
    apply_update(state, ui, update);
}

/// The UI-refresh half of [`apply`], factored out so the FIND-completion
/// handler (`run`'s `on_find_completion_ready`) can reuse the exact same
/// current/parent/preview sync `AppState::complete_find` needs — that
/// method returns an [`crate::app::Update`] like every other state change,
/// but isn't reached through `dispatch`/`Action` (it's not a user input;
/// see its own doc comment), so it can't go through [`apply`] itself.
fn apply_update(state: &Rc<RefCell<AppState>>, ui: &MainWindow, update: crate::app::Update) {
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

/// Shows FILTER's input box and moves keyboard focus to it — pre-filled
/// with whatever query is already active, so pressing `/` again while a
/// filter is already committed reopens it for editing rather than starting
/// over (the milestone's frozen "reopen `/`" rule). Clears FIND first
/// (M5T-B2, §22): `/` while FIND is active must show the FILTER view
/// underneath, never reinterpret FIND's results as something to filter —
/// `Action::ClearFind` is a no-op when FIND wasn't active, so this is safe
/// unconditionally. `filter_query` itself is never touched by either step.
fn begin_filter_edit(state: &Rc<RefCell<AppState>>, ui: &MainWindow) {
    apply(state, ui, Action::ClearFind);
    let query = state.borrow().filter_query().to_string();
    ui.set_filter_query(query.into());
    ui.set_filter_editing(true);
    ui.invoke_focus_filter_input();
}

/// Shows FIND's input box and moves keyboard focus to it — pre-filled with
/// the currently *committed* FIND query if a session is already active
/// (§5's "reopen" rule, mirroring FILTER's own), empty otherwise. No
/// `AppState` call of its own: opening the box changes nothing — FIND's
/// existing results (if any) stay exactly as they are while the user is
/// only editing the draft, and FILTER underneath is left completely alone
/// (§23 — unlike `/`, `f` never clears anything).
fn begin_find_edit(state: &Rc<RefCell<AppState>>, ui: &MainWindow) {
    let query = state
        .borrow()
        .find_session()
        .map(|session| session.query().to_string())
        .unwrap_or_default();
    ui.set_find_query(query.into());
    ui.set_find_editing(true);
    ui.invoke_focus_find_input();
}

/// Commits `query` (already confirmed non-empty by `on_find_accepted`) and
/// starts a new recursive search for it, off the UI thread.
///
/// # Why a channel, not just a `Weak` handoff
///
/// The natural-looking shape — worker computes the result, then calls
/// `ui_weak.upgrade_in_event_loop(move |ui| { ...apply it to `state`... })`
/// — doesn't type-check: that closure must be `Send` (it's handed to
/// `invoke_from_event_loop` beneath `upgrade_in_event_loop`, crossing the
/// thread boundary at the point it's *constructed*, on the worker thread,
/// even though it only ever *runs* later, back on the UI thread), and
/// `Rc<RefCell<AppState>>` is not `Send` — `state` can never be captured by
/// anything that closure builds. So the worker is only ever given `Send`
/// data (`root`/`query`/`include_hidden`, a `u64` generation, and a cloned
/// `mpsc::Sender`), and the *only* thing its `upgrade_in_event_loop`
/// closure does is invoke a callback that wakes the completion-draining
/// closure `run` already set up — the one that *does* own `state`, because
/// it was built on the UI thread back when `run` called `on_find_completion_ready`.
/// The `Sender`/`Receiver` pair is what actually carries the payload across
/// the thread boundary; the event-loop handoff is only ever a wakeup
/// signal.
///
/// # Why the window can close mid-search safely
///
/// `Weak::upgrade_in_event_loop` (Slint 1.17.1, `i-slint-core::api`) simply
/// never calls its functor if the component has no more strong references
/// — so a worker whose window closed before it finished just has its
/// wakeup silently dropped, and the `let _ =` below discards the
/// `Result<(), EventLoopError>` for the same reason (an already-terminated
/// event loop is not a bug to panic over). `find_tx.send(..)` is likewise
/// `let _ =`: if `find_rx` no longer exists (the whole `run` scope is
/// gone), the completion has nowhere to go and is simply dropped. Neither
/// path blocks, joins, or panics.
fn begin_find_search(
    state: &Rc<RefCell<AppState>>,
    ui: &MainWindow,
    find_tx: &mpsc::Sender<FindCompletion>,
    query: String,
) {
    apply(state, ui, Action::StartFind(query));
    let (generation, root, query, include_hidden) = {
        let st = state.borrow();
        let session = st
            .find_session()
            .expect("Action::StartFind always creates a session");
        (
            session.generation(),
            session.root().to_path_buf(),
            session.query().to_string(),
            session.include_hidden(),
        )
    };

    let tx = find_tx.clone();
    let ui_weak = ui.as_weak();
    // The one deliberate exception to "`ui` never calls `core` directly"
    // (see `src/lib.rs`'s layering doc comment): this closure needs both
    // `core::find::find_recursive` *and* `slint::Weak`'s event-loop handoff
    // in the same place, and only `ui/` is allowed to know about Slint at
    // all — so the worker can't live in `app` (which must stay
    // `slint`-free) or be reached through `AppState::dispatch` (nothing
    // about a running search is a synchronous state transition; only its
    // start and its eventual completion are, and both already go through
    // `AppState` via `Action::StartFind`/`complete_find`).
    std::thread::spawn(move || {
        let result =
            crate::core::find::find_recursive(&root, &query, include_hidden, FIND_MAX_RESULTS);
        let _ = tx.send(FindCompletion { generation, result });
        let _ = ui_weak.upgrade_in_event_loop(|ui| {
            ui.invoke_find_completion_ready();
        });
    });
}

fn full_refresh_current(state: &Rc<RefCell<AppState>>, ui: &MainWindow) {
    let (rows, path_text, status_text, mode_text, index) = {
        let st = state.borrow();
        let path_text = st.current_dir().display().to_string();
        let (rows, status_text, mode_text, index) = if let Some(session) = st.find_session() {
            find_current_view(session)
        } else {
            let visible = st.visible_entries();
            let rows: Vec<EntryRow> = visible
                .iter()
                .map(|entry| EntryRow {
                    text: row_label(entry).into(),
                    icon: entry_icon(entry.kind()).into(),
                })
                .collect();
            let query = st.filter_query();
            // FILTER never re-counts the directory from the filesystem:
            // both numbers below are lengths of lists already in memory
            // (`visible_entries()`/`entries()`), not a fresh read.
            let status_text = if query.is_empty() {
                format!("{} items", st.entries().len())
            } else {
                format!("{} / {} items", visible.len(), st.entries().len())
            };
            let mode_text = if query.is_empty() {
                "NORMAL".to_string()
            } else {
                format!("FILTER: {query}")
            };
            let index = st.visible_selected_index().map(|i| i as i32).unwrap_or(-1);
            (rows, status_text, mode_text, index)
        };
        (rows, path_text, status_text, mode_text, index)
        // `st` (the borrow) is dropped here, before any `ui`/`invoke_*` call.
    };
    ui.set_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_path_text(path_text.into());
    ui.set_status_text(status_text.into());
    ui.set_mode_text(mode_text.into());
    ui.invoke_reset_scroll();
    ui.set_current_index(index);
    ui.invoke_scroll_to_index(index);
}

/// FIND's contribution to CURRENT: rows (relative-path labels — see
/// [`find_result_label`]), status text, mode text, and selected index, for
/// whichever [`crate::app::FindPhase`] `session` is currently in. PARENT is
/// never touched by any of this (`full_refresh_current`'s caller never
/// calls `refresh_parent` for FIND states) — current_dir/PARENT stay
/// exactly the real directory's, per §12.
fn find_current_view(session: &crate::app::FindSession) -> (Vec<EntryRow>, String, String, i32) {
    let mode_text = format!("FIND: {}", session.query());
    match session.phase() {
        FindPhase::Searching => (Vec::new(), "searching...".to_string(), mode_text, -1),
        FindPhase::Error(message) => {
            // Kept short and on one line deliberately (§13): the status
            // bar is 24px tall and single-row, never a place to dump a
            // full `io::Error` message.
            let _ = message; // available if a future milestone wants it surfaced
            (Vec::new(), "ERROR".to_string(), mode_text, -1)
        }
        FindPhase::Ready(ready) => {
            let rows: Vec<EntryRow> = ready
                .results
                .iter()
                .map(|entry| EntryRow {
                    text: find_result_label(entry, session.root()).into(),
                    icon: entry_icon(entry.kind()).into(),
                })
                .collect();
            let mut status_text = format!("{} results", ready.results.len());
            // `truncated` means the search stopped at `FIND_MAX_RESULTS`,
            // never that another match was proven to exist beyond it — so
            // this deliberately never renders as "N+" (see
            // `FindReady::truncated`'s own doc comment).
            if ready.truncated {
                status_text.push_str(" · limit reached");
            }
            if ready.skipped_count > 0 {
                status_text.push_str(&format!(" · {} skipped", ready.skipped_count));
            }
            let index = ready.selected.map(|i| i as i32).unwrap_or(-1);
            (rows, status_text, mode_text, index)
        }
    }
}

/// FIND's row label: the result's path *relative to the search root*
/// (§11) — never the bare basename, since two results from different
/// subtrees can share one (`src/main.rs` vs. `docs/main-notes.md`).
/// `strip_prefix` failing (defensive only — every result comes from
/// `core::find::find_recursive(root, ..)`, so it should never actually
/// fail) falls back to the full path rather than panicking or discarding
/// the result; either way this only ever affects *presentation* — identity
/// stays `entry.path()`, untouched. A trailing "/" for directories, same
/// convention as `row_label`.
fn find_result_label(entry: &FileEntry, root: &Path) -> String {
    let relative = entry.path().strip_prefix(root).unwrap_or(entry.path());
    let mut label = relative.to_string_lossy().into_owned();
    if entry.kind() == EntryKind::Directory {
        label.push('/');
    }
    label
}

fn sync_selection(state: &Rc<RefCell<AppState>>, ui: &MainWindow) {
    let index = {
        let st = state.borrow();
        if let Some(session) = st.find_session() {
            match session.phase() {
                FindPhase::Ready(ready) => ready.selected.map(|i| i as i32).unwrap_or(-1),
                FindPhase::Searching | FindPhase::Error(_) => -1,
            }
        } else {
            st.visible_selected_index().map(|i| i as i32).unwrap_or(-1)
        }
    };
    ui.set_current_index(index);
    ui.invoke_scroll_to_index(index);
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

/// CURRENT's leading glyph for `kind`, from the Nerd Font glyph set already
/// bundled with `MesloLGS Nerd Font Mono`. xbar's own status icons (see
/// `xbar/src/ui/view.rs`'s `audio_glyph`/`network_glyph`/etc.) draw from
/// that font's Material Design Icons block instead (codepoints above
/// `\u{f0000}`) — tried here first for consistency, but confirmed via this
/// milestone's own smoke testing to render as missing-glyph boxes through
/// Slint's `renderer-software` text shaping specifically (the exact same
/// codepoints, same font file, render correctly through FreeType/Xft
/// outside Slint — this is a Slint-side limitation, not a missing glyph or
/// a wrong codepoint). Using the classic Font Awesome block instead (below
/// `\u{f400}`, well inside the Basic Multilingual Plane) renders correctly
/// in this app. Built here, never in `.slint` (which never inspects
/// `EntryKind`), same rule as [`row_label`]. Deliberately coarse — one
/// glyph per [`EntryKind`] variant, no MIME/extension/application lookup,
/// no thumbnails.
fn entry_icon(kind: EntryKind) -> &'static str {
    match kind {
        EntryKind::Directory => "\u{f07b}", // nf-fa-folder
        EntryKind::File => "\u{f016}",      // nf-fa-file-o
        EntryKind::Symlink => "\u{f0c1}",   // nf-fa-link
        EntryKind::Other => "\u{f059}",     // nf-fa-question-circle
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

/// Upper bound, in `char`s, on how much text any single `.slint` `Text`
/// element in PREVIEW is ever asked to lay out at once. Not a content or
/// truncation rule (`core::filesystem::MAX_TEXT_PREVIEW_BYTES` is; this
/// constant never affects what bytes are read or what counts as
/// `truncated`) — a pure rendering-safety measure: Slint 1.17.1's
/// `renderer-software` glyph-run drawing (`i_slint_core::textlayout::
/// sharedparley`, backed by Parley) was found, via this milestone's own
/// smoke testing, to panic (`euclid::vector.rs` `Option::unwrap()` on
/// `None`) when a *single* `Text` element is handed either very many
/// wrapped/newline-delimited lines (reproduced with a 1567-line, ~70KB
/// plain-text file) or one very long unbroken run of characters
/// (reproduced with a 65535-character line). Neither `word-wrap` vs.
/// `char-wrap` nor content shape mattered — only shrinking what any one
/// `Text` element has to lay out did. So PREVIEW content is split into
/// short rows (see [`preview_text_rows`]) and rendered through the same
/// per-row `ListView` already used for PARENT and directory previews —
/// architecture hundreds of directory entries already exercise safely,
/// rather than a new mechanism.
const PREVIEW_ROW_CHUNK_CHARS: usize = 200;

/// Splits `content` into short, render-safe rows: first on `\n` (so real
/// line breaks are preserved), then further on
/// [`PREVIEW_ROW_CHUNK_CHARS`] so no single row — and so no single
/// `.slint` `Text` element — ever has to lay out an unbounded run of
/// characters. `highlighted` is always `false`; these rows have no
/// PARENT-style "current" entry to mark.
fn preview_text_rows(content: &str) -> Vec<ContextRow> {
    let mut rows = Vec::new();
    for line in content.split('\n') {
        if line.is_empty() {
            rows.push(ContextRow {
                text: String::new().into(),
                highlighted: false,
            });
            continue;
        }
        let chars: Vec<char> = line.chars().collect();
        for chunk in chars.chunks(PREVIEW_ROW_CHUNK_CHARS) {
            rows.push(ContextRow {
                text: chunk.iter().collect::<String>().into(),
                highlighted: false,
            });
        }
    }
    rows
}

/// Converts decoded RGBA8 pixels into a `slint::Image`. The one copy this
/// requires (into Slint's own `SharedPixelBuffer`, a different memory
/// representation than `Vec<u8>`) is unavoidable without either `app`
/// depending on `slint::` types (breaking the layering) or `ui` depending
/// on `AppState` fields it doesn't own — `SharedPixelBuffer::new` +
/// `make_mut_bytes` is the most direct documented path Slint 1.17.1 offers
/// for "I already decoded pixels myself, here they are".
fn to_slint_image(width: u32, height: u32, rgba: &[u8]) -> slint::Image {
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::new(width, height);
    buffer.make_mut_bytes().copy_from_slice(rgba);
    slint::Image::from_rgba8(buffer)
}

fn refresh_preview(state: &Rc<RefCell<AppState>>, ui: &MainWindow) {
    let (mode, rows, label, truncated, image) = {
        let st = state.borrow();
        match st.preview() {
            PreviewContext::None => ("none", Vec::new(), String::new(), false, None),
            PreviewContext::Directory(children) => (
                "directory",
                context_rows(children, None),
                String::new(),
                false,
                None,
            ),
            PreviewContext::DirectoryUnavailable => (
                "unavailable",
                Vec::new(),
                preview_label(&st, "Directory", "Unavailable"),
                false,
                None,
            ),
            PreviewContext::File(FilePreview::Text { content, .. }) if content.is_empty() => {
                ("empty", Vec::new(), "(empty file)".to_string(), false, None)
            }
            PreviewContext::File(FilePreview::Text { content, truncated }) => (
                "text",
                preview_text_rows(content),
                String::new(),
                *truncated,
                None,
            ),
            PreviewContext::File(FilePreview::Image {
                width,
                height,
                rgba,
            }) => (
                "image",
                Vec::new(),
                String::new(),
                false,
                Some(to_slint_image(*width, *height, rgba)),
            ),
            PreviewContext::File(FilePreview::TooLarge) => (
                "unsupported",
                Vec::new(),
                preview_label(&st, "Image", "Too large to preview"),
                false,
                None,
            ),
            PreviewContext::File(FilePreview::Unsupported) => (
                "unsupported",
                Vec::new(),
                preview_label(&st, "Regular file", "Preview not available"),
                false,
                None,
            ),
            PreviewContext::File(FilePreview::Unavailable) => (
                "unavailable",
                Vec::new(),
                preview_label(&st, "Regular file", "Unavailable"),
                false,
                None,
            ),
            PreviewContext::Symlink => (
                "symlink",
                Vec::new(),
                preview_label(&st, "Symlink", "Preview not implemented yet"),
                false,
                None,
            ),
            PreviewContext::Other => (
                "other",
                Vec::new(),
                preview_label(&st, "Other", "Preview not implemented yet"),
                false,
                None,
            ),
        }
        // `st` is dropped here, before any `ui.set_*` call.
    };
    ui.set_preview_mode(mode.into());
    ui.set_preview_entries(ModelRc::from(Rc::new(VecModel::from(rows))));
    ui.set_preview_label(label.into());
    ui.set_preview_truncated(truncated);
    ui.set_preview_image(image.unwrap_or_default());
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
    fn entry_icon_is_distinct_per_kind() {
        let kinds = [
            EntryKind::Directory,
            EntryKind::File,
            EntryKind::Symlink,
            EntryKind::Other,
        ];
        let glyphs: Vec<&str> = kinds.iter().copied().map(entry_icon).collect();

        // Every `EntryKind` gets its own glyph — no two kinds silently
        // collapse to the same icon.
        for (i, a) in glyphs.iter().enumerate() {
            for b in &glyphs[i + 1..] {
                assert_ne!(a, b, "kinds {kinds:?} must not share an icon");
            }
        }
    }

    #[test]
    fn entry_icon_is_stable_for_the_same_kind() {
        assert_eq!(
            entry_icon(EntryKind::Directory),
            entry_icon(EntryKind::Directory)
        );
        assert_eq!(entry_icon(EntryKind::File), entry_icon(EntryKind::File));
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

    fn row_texts(rows: &[ContextRow]) -> Vec<String> {
        rows.iter().map(|r| r.text.to_string()).collect()
    }

    #[test]
    fn preview_text_rows_splits_on_real_newlines() {
        let rows = preview_text_rows("first\nsecond\nthird");

        assert_eq!(row_texts(&rows), vec!["first", "second", "third"]);
        assert!(rows.iter().all(|r| !r.highlighted));
    }

    #[test]
    fn preview_text_rows_chunks_a_single_unbroken_line_to_the_row_limit() {
        // The exact shape that crashed Slint 1.17.1's software-renderer
        // glyph-run drawing before this fix: one line, far longer than
        // `PREVIEW_ROW_CHUNK_CHARS`, with no whitespace to break on.
        let content = "a".repeat(PREVIEW_ROW_CHUNK_CHARS * 3 + 7);

        let rows = preview_text_rows(&content);

        assert_eq!(rows.len(), 4);
        for row in &rows[..3] {
            assert_eq!(row.text.len(), PREVIEW_ROW_CHUNK_CHARS);
        }
        assert_eq!(rows[3].text.len(), 7);
        assert_eq!(
            rows.iter().map(|r| r.text.len()).sum::<usize>(),
            content.len()
        );
    }

    #[test]
    fn preview_text_rows_never_splits_a_multibyte_character_mid_codepoint() {
        // Chunking by `char`, not by byte, so a row boundary can never
        // land inside a multibyte UTF-8 sequence.
        let content = "é".repeat(PREVIEW_ROW_CHUNK_CHARS + 1);

        let rows = preview_text_rows(&content);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].text.chars().count(), PREVIEW_ROW_CHUNK_CHARS);
        assert_eq!(rows[1].text.chars().count(), 1);
        // Every row is itself valid UTF-8 by construction (it's a `String`);
        // this just confirms no character was truncated in the process.
        assert!(rows.iter().all(|r| r.text.chars().all(|c| c == 'é')));
    }

    #[test]
    fn preview_text_rows_of_empty_content_is_a_single_empty_row() {
        // Real callers special-case an empty file before reaching this
        // function (see `refresh_preview`'s "empty" mode); this documents
        // what the function itself does, in isolation, for that input.
        let rows = preview_text_rows("");

        assert_eq!(row_texts(&rows), vec![""]);
    }

    #[test]
    fn preview_text_rows_preserves_blank_lines() {
        let rows = preview_text_rows("a\n\nb");

        assert_eq!(row_texts(&rows), vec!["a", "", "b"]);
    }

    // --- FIND presentation (M5T-B2) ----------------------------------------

    #[test]
    fn find_result_label_shows_path_relative_to_root() {
        let dir = crate::test_support::TempDir::new();
        let sub = dir.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        let file = sub.join("main.rs");
        std::fs::write(&file, b"").unwrap();
        let entry = FileEntry::from_path(file).unwrap();

        assert_eq!(find_result_label(&entry, dir.path()), "src/main.rs");
    }

    #[test]
    fn find_result_label_distinguishes_same_basename_in_different_dirs() {
        let dir = crate::test_support::TempDir::new();
        let src = dir.path().join("src");
        let docs = dir.path().join("docs");
        std::fs::create_dir(&src).unwrap();
        std::fs::create_dir(&docs).unwrap();
        std::fs::write(src.join("main.rs"), b"").unwrap();
        std::fs::write(docs.join("main.rs"), b"").unwrap();
        let a = FileEntry::from_path(src.join("main.rs")).unwrap();
        let b = FileEntry::from_path(docs.join("main.rs")).unwrap();

        assert_ne!(
            find_result_label(&a, dir.path()),
            find_result_label(&b, dir.path())
        );
    }

    #[test]
    fn find_result_label_adds_trailing_slash_for_directories() {
        let dir = crate::test_support::TempDir::new();
        let sub = dir.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        let entry = FileEntry::from_path(sub).unwrap();

        assert_eq!(find_result_label(&entry, dir.path()), "src/");
    }

    #[cfg(unix)]
    #[test]
    fn find_result_label_does_not_panic_on_non_utf8_name() {
        use std::ffi::OsStr;
        use std::os::unix::ffi::OsStrExt;
        let dir = crate::test_support::TempDir::new();
        let name = OsStr::from_bytes(b"main-\xFF.txt");
        let path = dir.path().join(name);
        std::fs::write(&path, b"").unwrap();
        let entry = FileEntry::from_path(path).unwrap();

        // Must not panic; presentation-only, identity is untouched.
        let label = find_result_label(&entry, dir.path());

        assert!(!label.is_empty());
    }

    #[test]
    fn row_label_still_shows_bare_basename_for_normal_filter_rows() {
        // FIND's relative-path labels (above) must never leak into the
        // normal/FILTER row builder — CURRENT outside FIND keeps showing
        // exactly the basename it always has.
        let dir = crate::test_support::TempDir::new();
        let sub = dir.path().join("src");
        std::fs::create_dir(&sub).unwrap();
        let entry = FileEntry::from_path(sub).unwrap();

        assert_eq!(row_label(&entry), "src/");
    }
}
