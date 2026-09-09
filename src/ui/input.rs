//! Keyboard input adapter and keymap.
//!
//! `.slint` only *names* what was pressed (a literal character, or one of
//! a handful of named keys it alone can recognize via its own `Key.*`
//! constants) and reports raw modifier flags — it never decides what any
//! of that means for xdir. Everything below this line is plain Rust with
//! zero `slint::` dependency, and is exactly where "j and Down both mean
//! select-next" is decided, in one place.
//!
//! ```text
//! Slint KeyEvent -> (ui/window.rs, the only place that reads slint types)
//!       |
//!       v
//! KeyStroke::new(raw text, modifier flags)   <- this module, no slint::
//!       |
//!       v
//! Keymap::resolve(KeyStroke) -> KeymapResult <- the keymap itself
//! ```
//!
//! Some `xdir` commands are two keys long (`gg`, `g h`, ...): the first key
//! (`g`) never produces an [`Action`] by itself, it just commits the keymap
//! to [`KeymapState::PendingGo`] until the next keystroke decides what `g`
//! meant. This is pure input-interaction state — never a timer, never a
//! filesystem/`AppState` concept — so it lives here, next to `KeyStroke`
//! and `resolve`, not in `core`/`app`.

use crate::app::Action;
use crate::core::places::SystemPlaceKind;

/// A key identity, independent of how any toolkit represents it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Char(char),
    Enter,
    Escape,
    /// Either physical Shift key, pressed by itself (not yet combined with
    /// the letter it's meant to shift). Distinguished from `Char`/other
    /// keys because it is the modifier that types `g D` — see
    /// [`Keymap::resolve_pending_go`].
    Shift,
    Up,
    Down,
    Left,
    Right,
}

/// Which modifier keys were held. `xdir`'s own shortcuts only ever fire
/// with `control`/`alt`/`meta` clear (see [`allowed`]) — any combination of
/// those is left alone for the window manager or a future xdir feature to
/// claim. `shift` alone is permitted only for the handful of bindings that
/// are explicitly uppercase (`G`, `g D`) — see [`allowed`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub meta: bool,
}

/// A single, fully-identified key press: what key, with what modifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyStroke {
    pub key: Key,
    pub modifiers: Modifiers,
}

impl KeyStroke {
    /// Builds a stroke from the raw text `.slint`'s `key-pressed` handler
    /// forwards. For a plain character key that raw text is exactly what
    /// Slint gave it (e.g. `"j"`, `"."`, `"\n"` for Enter, `"G"` for
    /// Shift+g — Slint already applies Shift to the reported text itself,
    /// this layer never infers casing from the modifier flags); for the
    /// arrow keys, Escape, and either physical Shift key, `.slint`
    /// substitutes one of the fixed tag strings below because only it can
    /// compare against its own `Key.UpArrow`/`Key.Escape`/`Key.Shift`/
    /// `Key.ShiftR` &co. constants — naming a key is not the same as
    /// deciding what it means. Returns `None` for anything unrecognized
    /// (empty text, an unrecognized multi-character tag), which the caller
    /// must treat as "ignored", never as a crash or a default action.
    pub(crate) fn new(
        raw: &str,
        shift: bool,
        control: bool,
        alt: bool,
        meta: bool,
    ) -> Option<Self> {
        let key = match raw {
            "Up" => Key::Up,
            "Down" => Key::Down,
            "Left" => Key::Left,
            "Right" => Key::Right,
            "Escape" => Key::Escape,
            "Shift" => Key::Shift,
            _ => {
                let mut chars = raw.chars();
                let ch = chars.next()?;
                if chars.next().is_some() {
                    return None;
                }
                if ch == '\n' || ch == '\r' {
                    Key::Enter
                } else {
                    Key::Char(ch)
                }
            }
        };
        Some(KeyStroke {
            key,
            modifiers: Modifiers {
                shift,
                control,
                alt,
                meta,
            },
        })
    }
}

/// Whether `key`, pressed with `modifiers`, is even eligible to mean
/// anything to xdir — independent of whether any binding actually claims
/// it. `control`/`alt`/`meta` are never used by any xdir shortcut and are
/// always left alone for the window manager. `shift` is permitted only
/// together with a `Key::Char` that is itself an uppercase ASCII letter:
/// Slint already bakes Shift into the reported text for letter keys (a
/// real Shift+g arrives as `"G"`), so an uppercase letter is exactly the
/// signal that shift was legitimately used to produce *this* key — a
/// lowercase letter (or any non-letter) reported alongside `shift: true`
/// is not a real xdir shortcut and is rejected, which is what keeps
/// `Shift+j`/`Shift+k` from silently reusing the `j`/`k` bindings.
/// Whether `stroke` is a physical Shift key, held by itself with no other
/// modifier — the transient event a real keyboard sends *before* the
/// letter it shifts, never a completed command on its own. `Ctrl`/`Alt`/
/// `Meta` are deliberately excluded: only Shift is ever needed to produce
/// one of xdir's uppercase continuations (`G`, `g D`), so only Shift gets
/// this exception — see [`Keymap::resolve_pending_go`].
fn is_pure_shift(stroke: KeyStroke) -> bool {
    stroke.key == Key::Shift
        && !stroke.modifiers.control
        && !stroke.modifiers.alt
        && !stroke.modifiers.meta
}

fn allowed(modifiers: Modifiers, key: Key) -> bool {
    if modifiers.control || modifiers.alt || modifiers.meta {
        return false;
    }
    if modifiers.shift {
        matches!(key, Key::Char(c) if c.is_ascii_uppercase())
    } else {
        true
    }
}

/// Keymap interaction state. `PendingGo` exists purely because `gg`/`g h`/
/// etc. are two keys long — it carries no filesystem/`AppState` knowledge
/// and never outlives more than one extra keystroke (or an explicit
/// [`Keymap::cancel_pending`] call).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeymapState {
    Normal,
    PendingGo,
}

/// What resolving one [`KeyStroke`] against the current [`KeymapState`]
/// produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum KeymapResult {
    /// A complete command: dispatch this action.
    Action(Action),
    /// The first key of a multi-key command (`g`) was consumed; the keymap
    /// is now waiting for the next keystroke to decide what it meant. Never
    /// produces an `Action` by itself.
    Pending,
    /// A pending prefix was resolved to nothing (an unrecognized
    /// continuation, or an explicit `Esc`): the whole sequence — prefix and
    /// this keystroke both — is consumed, and the keymap is back to
    /// `Normal`. The stroke that cancelled it is never reinterpreted as a
    /// fresh `Normal`-mode keystroke.
    Cancelled,
    /// This keystroke means nothing to xdir at all (an unbound key, or one
    /// carrying a modifier xdir never claims) — left alone for the window
    /// manager or ignored outright.
    Unhandled,
}

/// The keymap: the one and only place that says what a keystroke means for
/// xdir. Vim keys and arrow keys are aliases of the exact same `Action` —
/// neither Slint nor any other layer repeats that pairing. Owns exactly one
/// bit of state ([`KeymapState`]) to support the two-key `g...` commands;
/// there is no timer, no clock, and no thread anywhere in this file — a
/// pending `g` waits indefinitely for the next keystroke or an explicit
/// [`Self::cancel_pending`].
pub(crate) struct Keymap {
    state: KeymapState,
}

impl Keymap {
    pub(crate) fn new() -> Self {
        Keymap {
            state: KeymapState::Normal,
        }
    }

    /// Drops any pending `g` prefix without producing an action. Callers
    /// must invoke this before processing any input that represents a
    /// fresh, unrelated interaction — in particular a mouse selection or
    /// activation — so a `g` left dangling from an abandoned keyboard
    /// sequence can never be silently completed by an unrelated later
    /// keystroke (e.g. `g`, click elsewhere, `h` must move to the parent
    /// directory, never jump Home).
    pub(crate) fn cancel_pending(&mut self) {
        self.state = KeymapState::Normal;
    }

    pub(crate) fn resolve(&mut self, stroke: KeyStroke) -> KeymapResult {
        match self.state {
            KeymapState::Normal => self.resolve_normal(stroke),
            KeymapState::PendingGo => self.resolve_pending_go(stroke),
        }
    }

    fn resolve_normal(&mut self, stroke: KeyStroke) -> KeymapResult {
        if !allowed(stroke.modifiers, stroke.key) {
            return KeymapResult::Unhandled;
        }
        match stroke.key {
            Key::Char('j') | Key::Down => KeymapResult::Action(Action::SelectNext),
            Key::Char('k') | Key::Up => KeymapResult::Action(Action::SelectPrevious),
            Key::Char('h') | Key::Left => KeymapResult::Action(Action::GoParent),
            Key::Char('l') | Key::Right | Key::Enter => {
                KeymapResult::Action(Action::ActivateSelected)
            }
            Key::Char('.') => KeymapResult::Action(Action::ToggleHidden),
            Key::Char('G') => KeymapResult::Action(Action::SelectLast),
            Key::Char('g') => {
                self.state = KeymapState::PendingGo;
                KeymapResult::Pending
            }
            _ => KeymapResult::Unhandled,
        }
    }

    /// Resolves the key that follows a pending `g`. Every branch leaves
    /// `PendingGo` — either into a completed command or a cancelled one —
    /// so this always resets `self.state` first and lets the match below
    /// only decide the `KeymapResult`, with one exception: a bare physical
    /// Shift press (see [`is_pure_shift`]) is itself `Unhandled`, but must
    /// NOT reset `self.state` back to `Normal`. Typing `g D` physically is
    /// `g`, then Shift-down, then `d` (which Slint reports as `"D"`) —
    /// without this exception, the Shift-down event alone would already
    /// have cancelled the pending `g` before the real `"D"` keystroke ever
    /// arrived, and `g D` would silently never resolve to `Documents`.
    fn resolve_pending_go(&mut self, stroke: KeyStroke) -> KeymapResult {
        if is_pure_shift(stroke) {
            return KeymapResult::Unhandled;
        }
        self.state = KeymapState::Normal;
        if !allowed(stroke.modifiers, stroke.key) {
            // Not ours (e.g. Super+g, j while a WM-owned combo fires): the
            // pending prefix still can't survive it, but the keystroke
            // itself is left for the window manager, not consumed.
            return KeymapResult::Unhandled;
        }
        match stroke.key {
            Key::Char('g') => KeymapResult::Action(Action::SelectFirst),
            Key::Char('h') => KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Home)),
            Key::Char('d') => {
                KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Downloads))
            }
            Key::Char('D') => {
                KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Documents))
            }
            Key::Char('p') => {
                KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Pictures))
            }
            Key::Char('m') => KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Music)),
            Key::Char('v') => KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Videos)),
            // Anything else — including a bare `Esc` (recognized but bound
            // to nothing here) — cancels the whole sequence. The stroke is
            // consumed, never re-fed through `resolve_normal`.
            _ => KeymapResult::Cancelled,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(raw: &str) -> KeyStroke {
        KeyStroke::new(raw, false, false, false, false).expect("recognized key")
    }

    fn with_modifiers(raw: &str, shift: bool, control: bool, alt: bool, meta: bool) -> KeyStroke {
        KeyStroke::new(raw, shift, control, alt, meta).expect("recognized key")
    }

    fn resolve_once(raw: &str) -> KeymapResult {
        Keymap::new().resolve(plain(raw))
    }

    #[test]
    fn vim_keys_and_arrow_aliases_resolve_to_the_same_action() {
        assert_eq!(resolve_once("j"), KeymapResult::Action(Action::SelectNext));
        assert_eq!(
            resolve_once("Down"),
            KeymapResult::Action(Action::SelectNext)
        );

        assert_eq!(
            resolve_once("k"),
            KeymapResult::Action(Action::SelectPrevious)
        );
        assert_eq!(
            resolve_once("Up"),
            KeymapResult::Action(Action::SelectPrevious)
        );

        assert_eq!(resolve_once("h"), KeymapResult::Action(Action::GoParent));
        assert_eq!(resolve_once("Left"), KeymapResult::Action(Action::GoParent));

        assert_eq!(
            resolve_once("l"),
            KeymapResult::Action(Action::ActivateSelected)
        );
        assert_eq!(
            resolve_once("Right"),
            KeymapResult::Action(Action::ActivateSelected)
        );
        assert_eq!(
            resolve_once("\n"),
            KeymapResult::Action(Action::ActivateSelected)
        );
    }

    #[test]
    fn dot_toggles_hidden_files() {
        assert_eq!(
            resolve_once("."),
            KeymapResult::Action(Action::ToggleHidden)
        );
    }

    #[test]
    fn any_modifier_makes_a_normally_recognized_key_unhandled() {
        let mut keymap = Keymap::new();
        assert_eq!(
            keymap.resolve(with_modifiers("j", false, false, false, true)),
            KeymapResult::Unhandled
        ); // Super+j
        assert_eq!(
            keymap.resolve(with_modifiers("Down", false, false, false, true)),
            KeymapResult::Unhandled
        ); // Super+Down
        assert_eq!(
            keymap.resolve(with_modifiers("j", false, true, false, false)),
            KeymapResult::Unhandled
        ); // Ctrl+j
        assert_eq!(
            keymap.resolve(with_modifiers("j", false, false, true, false)),
            KeymapResult::Unhandled
        ); // Alt+j
    }

    #[test]
    fn unknown_keys_are_ignored_not_defaulted() {
        assert_eq!(resolve_once("q"), KeymapResult::Unhandled);
        assert_eq!(resolve_once("x"), KeymapResult::Unhandled);
    }

    #[test]
    fn key_stroke_new_rejects_empty_and_unrecognized_multi_char_text() {
        assert_eq!(KeyStroke::new("", false, false, false, false), None);
        assert_eq!(KeyStroke::new("F1", false, false, false, false), None);
    }

    // --- g-prefixed commands ---------------------------------------------

    #[test]
    fn g_enters_pending_go_without_action() {
        let mut keymap = Keymap::new();
        assert_eq!(keymap.resolve(plain("g")), KeymapResult::Pending);
    }

    #[test]
    fn gg_resolves_to_select_first() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        assert_eq!(
            keymap.resolve(plain("g")),
            KeymapResult::Action(Action::SelectFirst)
        );
    }

    #[test]
    fn uppercase_g_resolves_to_select_last() {
        let mut keymap = Keymap::new();
        let stroke = with_modifiers("G", true, false, false, false);
        assert_eq!(
            keymap.resolve(stroke),
            KeymapResult::Action(Action::SelectLast)
        );
    }

    #[test]
    fn gh_resolves_to_home() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        assert_eq!(
            keymap.resolve(plain("h")),
            KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Home))
        );
    }

    #[test]
    fn gd_resolves_to_downloads() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        assert_eq!(
            keymap.resolve(plain("d")),
            KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Downloads))
        );
    }

    #[test]
    fn g_uppercase_d_resolves_to_documents() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        let stroke = with_modifiers("D", true, false, false, false);
        assert_eq!(
            keymap.resolve(stroke),
            KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Documents))
        );
    }

    /// A pure physical Shift-down, as a real keyboard reports it: the
    /// `"Shift"` tag with `shift: true` and no other modifier.
    fn shift_key() -> KeyStroke {
        KeyStroke::new("Shift", true, false, false, false).expect("recognized key")
    }

    #[test]
    fn physical_g_then_shift_then_d_resolves_to_documents() {
        // Models the real keystroke sequence a keyboard sends for `g D`:
        // `g`, then Shift pressed down on its own, then `d` (reported as
        // `"D"` because Shift is now held) — not the single synthetic
        // Shift+D event `g_uppercase_d_resolves_to_documents` uses.
        let mut keymap = Keymap::new();
        assert_eq!(keymap.resolve(plain("g")), KeymapResult::Pending);

        // The bare Shift-down must not cancel the pending `g` — it is not
        // a continuation, just the modifier the next key needs.
        assert_eq!(keymap.resolve(shift_key()), KeymapResult::Unhandled);

        let d_while_shifted = with_modifiers("D", true, false, false, false);
        assert_eq!(
            keymap.resolve(d_while_shifted),
            KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Documents))
        );
    }

    #[test]
    fn physical_g_then_shift_then_invalid_key_cancels() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        assert_eq!(keymap.resolve(shift_key()), KeymapResult::Unhandled);

        // `J` is not a valid continuation of `g` (only `G`, in Normal mode,
        // means anything) — the sequence cancels, same as any other
        // invalid continuation.
        let j_while_shifted = with_modifiers("J", true, false, false, false);
        assert_eq!(keymap.resolve(j_while_shifted), KeymapResult::Cancelled);
    }

    #[test]
    fn shift_alone_in_normal_is_unhandled() {
        let mut keymap = Keymap::new();
        assert_eq!(keymap.resolve(shift_key()), KeymapResult::Unhandled);
    }

    #[test]
    fn forbidden_modifier_while_pending_does_not_preserve_the_prefix() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        // Unlike a bare Shift, Ctrl (even combined with an otherwise-valid
        // continuation) is never given the "preserve PendingGo" exception.
        let ctrl_x = with_modifiers("x", false, true, false, false);
        assert_eq!(keymap.resolve(ctrl_x), KeymapResult::Unhandled);

        // The pending `g` must not have survived: a plain `j` right after
        // is ordinary `SelectNext`, not a continuation of some stale `g`.
        assert_eq!(
            keymap.resolve(plain("j")),
            KeymapResult::Action(Action::SelectNext)
        );
    }

    #[test]
    fn physical_shift_then_g_resolves_to_select_last() {
        // `G` typed physically: Shift-down (received on its own first),
        // then `g` reported as `"G"` because Shift is held.
        let mut keymap = Keymap::new();
        assert_eq!(keymap.resolve(shift_key()), KeymapResult::Unhandled);

        let g_while_shifted = with_modifiers("G", true, false, false, false);
        assert_eq!(
            keymap.resolve(g_while_shifted),
            KeymapResult::Action(Action::SelectLast)
        );
    }

    #[test]
    fn gp_resolves_to_pictures() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        assert_eq!(
            keymap.resolve(plain("p")),
            KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Pictures))
        );
    }

    #[test]
    fn gm_resolves_to_music() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        assert_eq!(
            keymap.resolve(plain("m")),
            KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Music))
        );
    }

    #[test]
    fn gv_resolves_to_videos() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        assert_eq!(
            keymap.resolve(plain("v")),
            KeymapResult::Action(Action::GoSystemPlace(SystemPlaceKind::Videos))
        );
    }

    #[test]
    fn escape_cancels_pending_go() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        assert_eq!(keymap.resolve(plain("Escape")), KeymapResult::Cancelled);
    }

    #[test]
    fn invalid_go_sequence_is_consumed_and_cancels_prefix() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        // `j` is not a valid continuation of `g`; the whole `g j` sequence
        // is cancelled, never reinterpreted as a plain `SelectNext`.
        assert_eq!(keymap.resolve(plain("j")), KeymapResult::Cancelled);
    }

    #[test]
    fn after_invalid_sequence_next_key_is_normal_again() {
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));
        assert_eq!(keymap.resolve(plain("q")), KeymapResult::Cancelled);
        // `q`'s cancellation must not itself leave any residual state:
        // `j` right after it is a completely ordinary `SelectNext`.
        assert_eq!(
            keymap.resolve(plain("j")),
            KeymapResult::Action(Action::SelectNext)
        );
    }

    #[test]
    fn pending_go_is_reset_by_mouse_selection() {
        // Stands in for `ui::window`'s `on_selection_changed` callback,
        // which calls `cancel_pending` before dispatching the click.
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));

        keymap.cancel_pending();

        // `h` is ordinary `GoParent` again, never the `Home` place a stale
        // `g` prefix would have produced.
        assert_eq!(
            keymap.resolve(plain("h")),
            KeymapResult::Action(Action::GoParent)
        );
    }

    #[test]
    fn pending_go_is_reset_by_mouse_activation() {
        // Stands in for `ui::window`'s `on_item_pressed` callback (double-
        // click activation), which also calls `cancel_pending` first.
        let mut keymap = Keymap::new();
        keymap.resolve(plain("g"));

        keymap.cancel_pending();

        assert_eq!(
            keymap.resolve(plain("d")),
            KeymapResult::Unhandled // bare "d" has no Normal-mode binding
        );
    }

    #[test]
    fn pending_go_is_reset_by_an_unrecognized_keystroke() {
        // Stands in for `ui::window`'s `on_key_input` callback: a raw event
        // `KeyStroke::new` can't even name (e.g. a function key) never
        // reaches `Keymap::resolve` at all, so `ui::window` calls
        // `cancel_pending` directly on that path instead — this is the
        // seam that guarantees a `g` pending at that point cannot survive
        // to let the next real key complete it as `g`'s continuation.
        let mut keymap = Keymap::new();
        assert_eq!(keymap.resolve(plain("g")), KeymapResult::Pending);

        keymap.cancel_pending();

        // `j` right after is ordinary `SelectNext`, never a stale
        // continuation of the abandoned `g` (e.g. `gh`'s `Home`).
        assert_eq!(
            keymap.resolve(plain("j")),
            KeymapResult::Action(Action::SelectNext)
        );
    }

    #[test]
    fn g_with_ctrl_alt_meta_super_is_not_handled() {
        for (control, alt, meta) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let mut keymap = Keymap::new();
            let stroke = with_modifiers("G", true, control, alt, meta);
            assert_eq!(
                keymap.resolve(stroke),
                KeymapResult::Unhandled,
                "control={control} alt={alt} meta={meta}"
            );
        }
    }

    #[test]
    fn gd_with_forbidden_modifier_is_not_handled() {
        for (control, alt, meta) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let mut keymap = Keymap::new();
            keymap.resolve(plain("g"));
            let stroke = with_modifiers("D", true, control, alt, meta);
            assert_eq!(
                keymap.resolve(stroke),
                KeymapResult::Unhandled,
                "control={control} alt={alt} meta={meta}"
            );
        }
    }

    #[test]
    fn shift_j_does_not_become_select_next() {
        let mut keymap = Keymap::new();
        let stroke = with_modifiers("j", true, false, false, false);
        assert_eq!(keymap.resolve(stroke), KeymapResult::Unhandled);
    }

    #[test]
    fn shift_k_does_not_become_select_previous() {
        let mut keymap = Keymap::new();
        let stroke = with_modifiers("k", true, false, false, false);
        assert_eq!(keymap.resolve(stroke), KeymapResult::Unhandled);
    }

    #[test]
    fn d_alone_does_not_delete_or_produce_documents_action() {
        // Bare `D` (future permanent-delete) is not implemented; only
        // `g D` means Documents in this milestone.
        let mut keymap = Keymap::new();
        let stroke = with_modifiers("D", true, false, false, false);
        assert_eq!(keymap.resolve(stroke), KeymapResult::Unhandled);
    }

    #[test]
    fn reserved_future_keys_remain_unimplemented() {
        for raw in ["/", "f", "a", "r", "y", "x", "p", "d", " "] {
            let mut keymap = Keymap::new();
            assert_eq!(
                keymap.resolve(plain(raw)),
                KeymapResult::Unhandled,
                "key {raw:?} must remain unbound in Normal mode this milestone"
            );
        }
    }
}
