//! Enter visual mode command
//!
//! Transitions the editor to Visual mode for text selection. In Visual mode, movement
//! commands extend the selection rather than just moving the cursor.

use crate::{EditorMode, Stoat};

impl Stoat {
    /// Enter Visual mode for text selection.
    ///
    /// Transitions to Visual mode for selecting text. Movement commands extend the
    /// selection rather than moving the cursor.
    ///
    /// # Behavior
    ///
    /// - Sets editor mode to Visual
    /// - Creates selection anchor at current cursor position
    /// - Movement commands now extend selection
    /// - Can transition from Normal or Insert mode
    /// - Typically bound to 'v' key
    ///
    /// # Visual Mode Operations
    ///
    /// In Visual mode:
    /// - Movement keys extend selection: h, j, k, l, w, b, W, B, etc.
    /// - Selection can be deleted: x, d
    /// - Selection can be copied: y
    /// - Escape returns to Normal mode
    ///
    /// # Selection Types
    ///
    /// Visual mode supports:
    /// - Character-wise selection (v)
    /// - Line-wise selection (V) - not yet implemented
    /// - Block selection (Ctrl-V) - not yet implemented
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::actions::modal::enter_normal_mode`] - return to command mode
    /// - [`crate::actions::modal::enter_insert_mode`] - enter text input mode
    /// - [`crate::actions::selection`] - selection operations
    pub fn enter_visual_mode(&mut self) {
        self.set_mode(EditorMode::Visual);
    }
}

#[cfg(test)]
mod tests {
    use crate::{EditorMode, Stoat};

    #[test]
    fn enter_visual_from_normal() {
        let mut s = Stoat::test();
        s.set_text("hello");
        assert_eq!(s.mode(), EditorMode::Normal);

        s.input("v");
        assert_eq!(s.mode(), EditorMode::Visual);
    }

    #[test]
    fn visual_mode_enables_selection() {
        let mut s = Stoat::test();
        s.set_text("hello world");
        s.set_cursor(0, 0);

        s.input("v"); // Enter visual mode
        assert_eq!(s.mode(), EditorMode::Visual);

        s.input("w"); // Select forward to next word
        let (start_row, start_col, end_row, end_col) = s.selection();
        // Should have a non-empty selection
        assert!(start_row != end_row || start_col != end_col);
    }

    #[test]
    fn escape_exits_visual_mode() {
        let mut s = Stoat::test();
        s.set_text("hello");
        s.input("v"); // Enter visual
        assert_eq!(s.mode(), EditorMode::Visual);

        s.input("\x1b"); // Escape
        assert_eq!(s.mode(), EditorMode::Normal);
    }
}
