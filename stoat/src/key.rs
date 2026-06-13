//! Native key vocabulary for the shared keymap and terminal-input core.
//!
//! [`keymap`](crate::keymap) matches bindings against these events, and
//! [`key_encode`](crate::run::key_encode) turns them into the bytes a terminal
//! program reads from stdin. The GUI builds them from its windowing layer's
//! keystrokes at the boundary. The set mirrors the subset of terminal key
//! events the editor actually handles: printable characters, the common named
//! keys, and function keys, with the four modifier bits.

use std::ops::{BitOr, BitOrAssign};

/// A keyboard key, named independently of any platform scan code.
///
/// [`Char`](Self::Char) carries the produced character, already case-folded
/// for Shift; [`F`](Self::F) is a function-key number. [`Null`](Self::Null)
/// is a no-op key that produces no terminal input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum KeyCode {
    Char(char),
    Esc,
    Enter,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Insert,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Null,
    F(u8),
}

/// The set of modifier keys held during a key event, as a bitset.
///
/// The bit assignments are private: only set membership is observable, via
/// [`Self::contains`]. Combine with `|` / `|=`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct KeyModifiers(u8);

impl KeyModifiers {
    pub const NONE: KeyModifiers = KeyModifiers(0);
    pub const SHIFT: KeyModifiers = KeyModifiers(0b0001);
    pub const CONTROL: KeyModifiers = KeyModifiers(0b0010);
    pub const ALT: KeyModifiers = KeyModifiers(0b0100);
    pub const SUPER: KeyModifiers = KeyModifiers(0b1000);

    pub const fn empty() -> KeyModifiers {
        KeyModifiers(0)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Whether every modifier in `other` is also set in `self`.
    pub const fn contains(self, other: KeyModifiers) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Clear the modifiers in `other` from the set.
    pub fn remove(&mut self, other: KeyModifiers) {
        self.0 &= !other.0;
    }
}

impl BitOr for KeyModifiers {
    type Output = KeyModifiers;

    fn bitor(self, rhs: KeyModifiers) -> KeyModifiers {
        KeyModifiers(self.0 | rhs.0)
    }
}

impl BitOrAssign for KeyModifiers {
    fn bitor_assign(&mut self, rhs: KeyModifiers) {
        self.0 |= rhs.0;
    }
}

/// A key press: which key, and the modifiers held with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct KeyEvent {
    pub code: KeyCode,
    pub modifiers: KeyModifiers,
}

impl KeyEvent {
    pub fn new(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent { code, modifiers }
    }
}

#[cfg(test)]
mod tests {
    use super::KeyModifiers;

    #[test]
    fn modifiers_set_ops() {
        let cs = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
        assert!(cs.contains(KeyModifiers::CONTROL));
        assert!(cs.contains(KeyModifiers::SHIFT));
        assert!(!cs.contains(KeyModifiers::ALT));
        assert!(cs.contains(KeyModifiers::empty()));

        let mut m = cs;
        m |= KeyModifiers::ALT;
        assert!(m.contains(KeyModifiers::ALT));
        m.remove(KeyModifiers::SHIFT);
        assert!(!m.contains(KeyModifiers::SHIFT));
        assert!(m.contains(KeyModifiers::CONTROL | KeyModifiers::ALT));

        assert!(KeyModifiers::NONE.is_empty());
        assert!(!cs.is_empty());
    }
}
