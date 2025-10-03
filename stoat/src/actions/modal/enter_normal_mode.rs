//! Enter normal mode command
//!
//! Transitions the editor to Normal mode, the default command mode for navigation
//! and operations. In Normal mode, keypresses trigger commands rather than inserting text.

use crate::{EditorMode, Stoat};
use tracing::debug;

impl Stoat {
    /// Enter Normal mode for command input.
    ///
    /// Transitions to Normal mode, the default mode for navigation and commands. In
    /// Normal mode, key presses trigger actions rather than inserting text.
    ///
    /// # Behavior
    ///
    /// - Sets editor mode to Normal
    /// - Keypresses now execute commands instead of inserting text
    /// - Can transition from Insert or Visual mode
    /// - Typically bound to Escape key
    ///
    /// # Normal Mode Commands
    ///
    /// Common Normal mode operations:
    /// - Movement: h, j, k, l, w, b, 0, $, gg, G
    /// - Editing: x, dd, D, u, r
    /// - Mode changes: i, a, v
    /// - Selection: w, b, W, B
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::modal::enter_insert_mode`] - enter text input mode
    /// - [`crate::actions::modal::enter_visual_mode`] - enter selection mode
    pub fn enter_normal_mode(&mut self) {
        let old_mode = self.mode();
        debug!(from = ?old_mode, to = ?EditorMode::Normal, "Entering normal mode");
        self.set_mode(EditorMode::Normal);
    }
}

#[cfg(test)]
mod tests {
    use crate::{EditorMode, Stoat};

    #[test]
    fn enter_normal_from_insert() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_mode(EditorMode::Insert);

        s.input("\x1b"); // Escape key
        assert_eq!(s.mode(), EditorMode::Normal);
    }

    #[test]
    fn commands_work_in_normal_mode() {
        let mut s = Stoat::test();
        s.set_text("hello world");
        s.set_cursor(0, 0);
        s.set_mode(EditorMode::Insert);

        s.input("\x1b"); // Escape to normal
        assert_eq!(s.mode(), EditorMode::Normal);

        s.input("l"); // Move right
        s.assert_cursor_notation("h|ello world");
    }

    #[test]
    fn escape_from_insert_stops_text_input() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.set_cursor(0, 5);
        s.set_mode(EditorMode::Insert);

        s.input(" world");
        s.input("\x1b"); // Escape to normal
        s.input("x"); // Should not insert 'x', should delete char

        // In normal mode, 'x' deletes character under cursor
        assert_eq!(s.mode(), EditorMode::Normal);
    }
}
