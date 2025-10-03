//! Enter insert mode command
//!
//! Transitions the editor from Normal or Visual mode to Insert mode, enabling direct
//! text entry. This is the modal editing mode for typing and pasting text.

use crate::{EditorMode, Stoat};
use tracing::debug;

impl Stoat {
    /// Enter Insert mode for text input.
    ///
    /// Transitions to Insert mode, where keypresses insert text rather than triggering
    /// commands. This is the primary mode for text entry and modification.
    ///
    /// # Behavior
    ///
    /// - Sets editor mode to Insert
    /// - Most keys now insert text instead of executing commands
    /// - Special keys (Escape, arrows) remain functional
    /// - Can transition from Normal or Visual mode
    ///
    /// # Common Bindings
    ///
    /// - `i` - enter insert mode at cursor
    /// - `I` - enter insert mode at start of line
    /// - `a` - enter insert mode after cursor
    /// - `A` - enter insert mode at end of line
    /// - `o` - insert new line below and enter insert mode
    /// - `O` - insert new line above and enter insert mode
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::modal::enter_normal_mode`] - return to command mode
    /// - [`crate::actions::modal::enter_visual_mode`] - enter selection mode
    pub fn enter_insert_mode(&mut self) {
        let old_mode = self.mode();
        debug!(from = ?old_mode, to = ?EditorMode::Insert, "Entering insert mode");
        self.set_mode(EditorMode::Insert);
    }
}

#[cfg(test)]
mod tests {
    use crate::{EditorMode, Stoat};

    #[test]
    fn enter_insert_from_normal() {
        let mut s = Stoat::test();
        s.set_text("hello");
        assert_eq!(s.mode(), EditorMode::Normal);

        s.input("i");
        assert_eq!(s.mode(), EditorMode::Insert);
    }

    #[test]
    fn insert_text_in_insert_mode() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);

        s.input("i");
        assert_eq!(s.mode(), EditorMode::Insert);

        s.input(" world");
        s.assert_cursor_notation("hello world|");
    }

    #[test]
    fn enter_insert_preserves_cursor() {
        let mut s = Stoat::test();
        s.set_text("hello world");
        s.set_cursor(0, 6);

        s.input("i");
        s.assert_cursor_notation("hello |world");
    }
}
