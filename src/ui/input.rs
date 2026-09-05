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
//! resolve(KeyStroke) -> Option<Action>       <- the keymap itself
//! ```

use crate::app::Action;

/// A key identity, independent of how any toolkit represents it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Key {
    Char(char),
    Enter,
    Up,
    Down,
    Left,
    Right,
}

/// Which modifier keys were held. `xdir`'s own shortcuts only ever fire
/// with none of these set (see [`Modifiers::none`]) — any combination is
/// left alone for the window manager or a future xdir feature to claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct Modifiers {
    pub shift: bool,
    pub control: bool,
    pub alt: bool,
    pub meta: bool,
}

impl Modifiers {
    pub(crate) fn none(&self) -> bool {
        !(self.shift || self.control || self.alt || self.meta)
    }
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
    /// Slint gave it (e.g. `"j"`, `"."`, `"\n"` for Enter); for the four
    /// arrow keys, `.slint` substitutes one of the fixed tag strings below
    /// because only it can compare against its own `Key.UpArrow` &co.
    /// constants — naming a key is not the same as deciding what it means.
    /// Returns `None` for anything unrecognized (empty text, an
    /// unrecognized multi-character tag), which the caller must treat as
    /// "ignored", never as a crash or a default action.
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

/// The keymap: the one and only place that says what a keystroke means
/// for xdir. Vim keys and arrow keys are aliases of the exact same
/// `Action` — neither Slint nor any other layer repeats that pairing.
pub(crate) fn resolve(stroke: KeyStroke) -> Option<Action> {
    if !stroke.modifiers.none() {
        return None;
    }
    match stroke.key {
        Key::Char('j') | Key::Down => Some(Action::SelectNext),
        Key::Char('k') | Key::Up => Some(Action::SelectPrevious),
        Key::Char('h') | Key::Left => Some(Action::GoParent),
        Key::Char('l') | Key::Right | Key::Enter => Some(Action::ActivateSelected),
        Key::Char('.') => Some(Action::ToggleHidden),
        _ => None,
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

    #[test]
    fn vim_keys_and_arrow_aliases_resolve_to_the_same_action() {
        assert_eq!(resolve(plain("j")), Some(Action::SelectNext));
        assert_eq!(resolve(plain("Down")), Some(Action::SelectNext));

        assert_eq!(resolve(plain("k")), Some(Action::SelectPrevious));
        assert_eq!(resolve(plain("Up")), Some(Action::SelectPrevious));

        assert_eq!(resolve(plain("h")), Some(Action::GoParent));
        assert_eq!(resolve(plain("Left")), Some(Action::GoParent));

        assert_eq!(resolve(plain("l")), Some(Action::ActivateSelected));
        assert_eq!(resolve(plain("Right")), Some(Action::ActivateSelected));
        assert_eq!(resolve(plain("\n")), Some(Action::ActivateSelected));
    }

    #[test]
    fn dot_toggles_hidden_files() {
        assert_eq!(resolve(plain(".")), Some(Action::ToggleHidden));
    }

    #[test]
    fn any_modifier_makes_a_normally_recognized_key_ignored() {
        assert_eq!(
            resolve(with_modifiers("j", false, false, false, true)),
            None
        ); // Super+j
        assert_eq!(
            resolve(with_modifiers("Down", false, false, false, true)),
            None
        ); // Super+Down
        assert_eq!(
            resolve(with_modifiers("j", false, true, false, false)),
            None
        ); // Ctrl+j
        assert_eq!(
            resolve(with_modifiers("j", false, false, true, false)),
            None
        ); // Alt+j
        assert_eq!(
            resolve(with_modifiers("j", true, false, false, false)),
            None
        ); // Shift+j
    }

    #[test]
    fn unknown_keys_are_ignored_not_defaulted() {
        assert_eq!(resolve(plain("q")), None);
        assert_eq!(resolve(plain("x")), None);
    }

    #[test]
    fn key_stroke_new_rejects_empty_and_unrecognized_multi_char_text() {
        assert_eq!(KeyStroke::new("", false, false, false, false), None);
        assert_eq!(KeyStroke::new("F1", false, false, false, false), None);
    }
}
