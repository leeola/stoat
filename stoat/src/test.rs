//! Test utilities for Stoat editor

use crate::Stoat;
use gpui::{Pixels, Size, TestAppContext};
use text::Point;

/// Default line height in pixels for test calculations
const DEFAULT_LINE_HEIGHT: f32 = 20.0;

/// Test wrapper for Stoat that provides convenient testing methods
pub struct StoatTest {
    stoat: Stoat,
    cx: TestAppContext,
    line_height: f32,
}

impl StoatTest {
    /// Create a new StoatTest instance with default settings
    pub fn new() -> Self {
        let cx = TestAppContext::single();
        let stoat = {
            let mut app = cx.app.borrow_mut();
            Stoat::new(&mut app)
        };

        let mut test = Self {
            stoat,
            cx,
            line_height: DEFAULT_LINE_HEIGHT,
        };

        // Set default viewport size (24 lines, like a terminal)
        test.set_viewport_lines(24.0);

        test
    }

    /// Get the current buffer contents as a string
    pub fn text(&self) -> String {
        let app = self.cx.app.borrow();
        self.stoat.buffer_contents(&app)
    }

    /// Get the current cursor position as (row, column)
    pub fn cursor(&self) -> (u32, u32) {
        let pos = self.stoat.cursor_position();
        (pos.row, pos.column)
    }

    /// Insert text at the current cursor position
    pub fn insert(&mut self, text: &str) {
        let mut app = self.cx.app.borrow_mut();
        self.stoat.insert_text(text, &mut app);
    }

    /// Set the window size in pixels
    pub fn set_window_size(&mut self, size: Size<Pixels>) {
        // Calculate how many lines fit in this pixel height
        let lines = size.height.0 / self.line_height;
        self.set_viewport_lines(lines);
    }

    /// Set the viewport height in lines
    pub fn set_viewport_lines(&mut self, lines: f32) {
        self.stoat.set_visible_line_count(lines);
    }

    /// Resize the viewport to the specified number of lines
    pub fn resize_lines(&mut self, lines: f32) {
        self.set_viewport_lines(lines);
    }

    /// Set the line height for pixel/line conversions
    pub fn set_line_height(&mut self, height: f32) {
        self.line_height = height;
    }

    /// Get the current line height
    pub fn line_height(&self) -> f32 {
        self.line_height
    }

    /// Get the current viewport size in lines
    pub fn viewport_lines(&self) -> Option<f32> {
        self.stoat.visible_line_count()
    }

    /// Move cursor to specific position
    pub fn set_cursor(&mut self, row: u32, col: u32) {
        self.stoat.set_cursor_position(Point::new(row, col));
    }

    /// Assert the text content matches expected
    #[track_caller]
    pub fn assert_text(&self, expected: &str) {
        assert_eq!(self.text(), expected);
    }

    /// Assert the cursor position matches expected
    #[track_caller]
    pub fn assert_cursor(&self, row: u32, col: u32) {
        assert_eq!(self.cursor(), (row, col));
    }

    /// Get the current editor mode
    pub fn mode(&self) -> crate::EditorMode {
        self.stoat.mode()
    }

    /// Set the editor mode
    pub fn set_mode(&mut self, mode: crate::EditorMode) {
        self.stoat.set_mode(mode);
    }

    /// Get the current selection as (start_row, start_col, end_row, end_col)
    pub fn selection(&self) -> (u32, u32, u32, u32) {
        let selection = self.stoat.cursor_manager().selection();
        (
            selection.start.row,
            selection.start.column,
            selection.end.row,
            selection.end.column,
        )
    }

    /// Check if there is an active selection
    pub fn has_selection(&self) -> bool {
        !self.stoat.cursor_manager().selection().is_empty()
    }

    /// Assert the editor mode matches expected
    #[track_caller]
    pub fn assert_mode(&self, expected: crate::EditorMode) {
        assert_eq!(self.mode(), expected);
    }

    /// Assert the selection range matches expected
    #[track_caller]
    pub fn assert_selection(&self, start_row: u32, start_col: u32, end_row: u32, end_col: u32) {
        assert_eq!(self.selection(), (start_row, start_col, end_row, end_col));
    }

    /// Assert that no text is selected
    #[track_caller]
    pub fn assert_no_selection(&self) {
        assert!(
            !self.has_selection(),
            "Expected no selection, but selection exists"
        );
    }

    /// Start text selection at current cursor position
    pub fn start_selection(&mut self) {
        self.stoat.cursor_manager_mut().start_selection();
    }

    /// End text selection
    pub fn end_selection(&mut self) {
        self.stoat.cursor_manager_mut().end_selection();
    }
}

impl Stoat {
    /// Create a new Stoat instance configured for testing
    pub fn test() -> StoatTest {
        StoatTest::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_text_insertion() {
        let mut s = Stoat::test();
        s.insert("Hello World");
        s.assert_text("Hello World");
        s.assert_cursor(0, 11);
    }

    #[test]
    fn cursor_positioning() {
        let mut s = Stoat::test();
        s.insert("Line 1\nLine 2\nLine 3");
        s.set_cursor(1, 3);
        s.assert_cursor(1, 3);
    }

    #[test]
    fn viewport_sizing() {
        let mut s = Stoat::test();

        // Test line-based sizing
        s.resize_lines(30.0);
        assert_eq!(s.viewport_lines(), Some(30.0));

        // Test pixel-based sizing with default line height (20px)
        s.set_window_size(Size {
            width: Pixels(800.0),
            height: Pixels(600.0), // 600 / 20 = 30 lines
        });
        assert_eq!(s.viewport_lines(), Some(30.0));
    }

    #[test]
    fn line_height_conversion() {
        let mut s = Stoat::test();
        s.set_line_height(16.0);

        s.set_window_size(Size {
            width: Pixels(800.0),
            height: Pixels(480.0), // 480 / 16 = 30 lines
        });
        assert_eq!(s.viewport_lines(), Some(30.0));
    }

    #[test]
    fn editor_mode_handling() {
        let mut s = Stoat::test();

        // Default mode should be Normal
        s.assert_mode(crate::EditorMode::Normal);

        // Test mode switching
        s.set_mode(crate::EditorMode::Insert);
        s.assert_mode(crate::EditorMode::Insert);

        s.set_mode(crate::EditorMode::Visual);
        s.assert_mode(crate::EditorMode::Visual);

        s.set_mode(crate::EditorMode::Normal);
        s.assert_mode(crate::EditorMode::Normal);
    }

    #[test]
    fn selection_handling() {
        let mut s = Stoat::test();
        s.insert("Line 1\nLine 2\nLine 3");
        s.set_cursor(1, 2); // Position at "Line 2"

        // Initially no selection
        s.assert_no_selection();
        assert_eq!(s.has_selection(), false);

        // Start selection
        s.start_selection();

        // Move cursor to extend selection
        s.set_cursor(2, 4); // Move to "Line 3"

        // Should now have selection
        assert_eq!(s.has_selection(), true);
        s.assert_selection(1, 2, 2, 4);

        // End selection
        s.end_selection();

        // Selection should still exist but not be actively selecting
        assert_eq!(s.has_selection(), true);
    }
}
