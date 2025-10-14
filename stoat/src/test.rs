//! Test utilities for Stoat v4.
//!
//! This module provides GPUI-native test infrastructure for validating the Entity pattern
//! and enabling test-driven development of editor features.
//!
//! # Key Components
//!
//! - [`cursor_notation`] - DSL for cursor/selection positions in test strings
//! - [`TestStoat`] - Wrapper around [`Entity<Stoat>`] with test-oriented helpers
//!
//! # Example
//!
//! ```ignore
//! #[gpui::test]
//! fn test_insert_mode(cx: &mut TestAppContext) {
//!     let stoat = Stoat::test(cx);
//!
//!     stoat.update(cx, |s, cx| {
//!         s.enter_insert_mode(cx);
//!         s.insert_text("hello", cx);
//!     });
//!
//!     assert_eq!(stoat.buffer_text(cx), "hello");
//! }
//! ```

pub mod cursor_notation;

use crate::Stoat;
use gpui::{AppContext, Context, Entity, TestAppContext};
use text::Point;

/// Wrapper around [`Entity<Stoat>`] that provides test-oriented helper methods.
///
/// This wrapper makes tests cleaner by providing convenient accessors for common
/// operations like reading buffer text, cursor position, and mode. It holds both
/// the entity and the test context, so you don't need to pass `cx` to every method.
///
/// # Creation
///
/// Use [`Stoat::test`] or [`Stoat::test_with_text`] to create instances:
///
/// ```ignore
/// let mut stoat = Stoat::test(cx);  // cx is now owned by stoat
/// let mut stoat = Stoat::test_with_text("hello", cx);
/// ```
///
/// Note: Once created, `cx` is borrowed by the `TestStoat` for its lifetime.
///
/// # Usage
///
/// The wrapper provides both read and update operations without needing `cx`:
///
/// ```ignore
/// // Read operations - no cx needed!
/// let text = stoat.buffer_text();
/// let pos = stoat.cursor_position();
/// let mode = stoat.mode();
///
/// // Update operations - no outer cx needed!
/// stoat.update(|s, cx| {
///     s.insert_text("hello", cx);
/// });
/// ```
pub struct TestStoat<'a> {
    entity: Entity<Stoat>,
    cx: &'a mut TestAppContext,
}

impl<'a> TestStoat<'a> {
    /// Create a new TestStoat with the given initial text.
    ///
    /// Called by [`Stoat::test`] and [`Stoat::test_with_text`].
    pub fn new(text: &str, cx: &'a mut TestAppContext) -> Self {
        let entity = cx.new(|cx| {
            let stoat = Stoat::new(cx);

            // Always update the buffer to replace welcome text (even with empty string)
            // Use Rust language for better tokenization in tests
            stoat.active_buffer(cx).update(cx, |item, cx| {
                item.set_language(stoat_text::Language::Rust);
                item.buffer().update(cx, |buffer, _| {
                    let len = buffer.len();
                    buffer.edit([(0..len, text)]);
                });
                let _ = item.reparse(cx);
            });

            stoat
        });

        Self { entity, cx }
    }

    /// Get access to the underlying [`Entity<Stoat>`].
    ///
    /// Use this when you need to interact with APIs that expect an entity directly.
    pub fn entity(&self) -> &Entity<Stoat> {
        &self.entity
    }

    /// Update the Stoat entity.
    ///
    /// No need to pass `cx` - it's stored in the wrapper!
    pub fn update<R>(&mut self, f: impl FnOnce(&mut Stoat, &mut Context<Stoat>) -> R) -> R {
        self.entity.update(self.cx, f)
    }

    /// Get the current buffer text.
    ///
    /// No need to pass `cx` - it's stored in the wrapper!
    pub fn buffer_text(&self) -> String {
        self.cx.read_entity(&self.entity, |s, cx| {
            cx.read_entity(&s.active_buffer(cx), |item, cx| {
                cx.read_entity(item.buffer(), |buffer, _| buffer.text())
            })
        })
    }

    /// Get the current cursor position.
    ///
    /// Returns the cursor as a [`text::Point`] with row and column.
    pub fn cursor_position(&self) -> Point {
        self.cx
            .read_entity(&self.entity, |s, _| s.cursor_position())
    }

    /// Get the current mode.
    pub fn mode(&self) -> String {
        self.cx
            .read_entity(&self.entity, |s, _| s.mode().to_string())
    }

    /// Get the current selection.
    ///
    /// Returns a copy of the current selection including start, end, and reversed flag.
    pub fn selection(&self) -> crate::cursor::Selection {
        self.cx
            .read_entity(&self.entity, |s, _| s.selection().clone())
    }

    /// Create a TestStoat with cursor and selection from marked notation.
    ///
    /// Uses the cursor notation DSL to specify initial cursor/selection positions.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// // Cursor at position 6
    /// let stoat = Stoat::test_with_cursor_notation("hello |world", cx);
    ///
    /// // Selection with cursor at end
    /// let stoat = Stoat::test_with_cursor_notation("<|hello||>", cx);
    /// ```
    pub fn test_with_cursor_notation(
        marked_text: &str,
        cx: &'a mut TestAppContext,
    ) -> anyhow::Result<Self> {
        let parsed = cursor_notation::parse(marked_text)?;

        let mut test_stoat = Self::new(&parsed.text, cx);

        test_stoat.update(|s, cx| {
            // Set cursor position if we have one
            if let Some(&offset) = parsed.cursors.first() {
                let point = offset_to_point(&parsed.text, offset);
                s.set_cursor_position(point);
            }

            // Set selection if we have one
            if let Some(sel) = parsed.selections.first() {
                let start = offset_to_point(&parsed.text, sel.range.start);
                let end = offset_to_point(&parsed.text, sel.range.end);

                // Create selection with proper cursor position and reversed flag
                let selection = crate::cursor::Selection {
                    start,
                    end,
                    reversed: sel.cursor_at_start,
                };

                s.cursor.set_selection(selection);
            }
        });

        Ok(test_stoat)
    }

    /// Convert current buffer state to cursor notation string.
    ///
    /// Returns the buffer text with cursor and selection markers.
    pub fn to_cursor_notation(&self) -> String {
        let text = self.buffer_text();
        let cursor_pos = self.cursor_position();
        let selection = self.selection();

        let cursor_offset = point_to_offset(&text, cursor_pos);

        if selection.is_empty() {
            // Just a cursor
            cursor_notation::format(&text, &[cursor_offset], &[])
        } else {
            // Selection
            let start_offset = point_to_offset(&text, selection.start);
            let end_offset = point_to_offset(&text, selection.end);

            let notation_sel = cursor_notation::Selection {
                range: start_offset..end_offset,
                cursor_at_start: selection.reversed,
            };

            cursor_notation::format(&text, &[], &[notation_sel])
        }
    }

    /// Assert that buffer state matches expected cursor notation.
    ///
    /// Compares the current buffer state (text, cursor, selection) against
    /// the expected marked string.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// stoat.assert_cursor_notation("hello |world");
    /// stoat.assert_cursor_notation("<|hello||>");
    /// ```
    pub fn assert_cursor_notation(&self, expected: &str) {
        let actual = self.to_cursor_notation();
        assert_eq!(
            actual, expected,
            "Buffer state doesn't match expected cursor notation"
        );
    }
}

/// Convert absolute byte offset to Point (row, column).
fn offset_to_point(text: &str, offset: usize) -> Point {
    let mut current_offset = 0;
    let mut row = 0;

    for line in text.lines() {
        let line_len = line.len();
        let line_end = current_offset + line_len;

        if offset <= line_end {
            // Offset is on this line
            let col = offset - current_offset;
            return Point::new(row, col as u32);
        }

        // Move past this line plus newline
        current_offset = line_end + 1; // +1 for \n
        row += 1;
    }

    // Offset is at or past the end
    Point::new(row, offset.saturating_sub(current_offset) as u32)
}

/// Convert Point (row, column) to absolute byte offset.
fn point_to_offset(text: &str, point: Point) -> usize {
    let mut offset = 0;
    let mut row = 0;

    for line in text.lines() {
        if row == point.row {
            return offset + point.column as usize;
        }

        offset += line.len() + 1; // +1 for \n
        row += 1;
    }

    // Point is past the end
    offset + point.column as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Stoat;

    #[gpui::test]
    fn creates_test_stoat(cx: &mut TestAppContext) {
        let stoat = Stoat::test(cx);

        // Should start in normal mode
        assert_eq!(stoat.mode(), "normal");

        // Should have empty buffer (for testing)
        assert_eq!(stoat.buffer_text(), "");
    }

    #[gpui::test]
    fn creates_test_stoat_with_text(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_text("hello world", cx);

        assert_eq!(stoat.buffer_text(), "hello world");
    }

    #[gpui::test]
    fn helper_reads_buffer_text(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_text("test", cx);

        assert_eq!(stoat.buffer_text(), "test");
    }

    #[gpui::test]
    fn helper_reads_cursor_position(cx: &mut TestAppContext) {
        let stoat = Stoat::test(cx);

        assert_eq!(stoat.cursor_position(), Point::new(0, 0));
    }

    #[gpui::test]
    fn helper_reads_mode(cx: &mut TestAppContext) {
        let stoat = Stoat::test(cx);

        assert_eq!(stoat.mode(), "normal");
    }

    // ===== Cursor Notation Tests =====

    #[gpui::test]
    fn test_with_cursor_notation_cursor_only(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("hello |world", cx).unwrap();

        assert_eq!(stoat.buffer_text(), "hello world");
        assert_eq!(stoat.cursor_position(), Point::new(0, 6));
        assert!(stoat.selection().is_empty());
    }

    #[gpui::test]
    fn test_with_cursor_notation_multiline(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("line1\nli|ne2\nline3", cx).unwrap();

        assert_eq!(stoat.buffer_text(), "line1\nline2\nline3");
        assert_eq!(stoat.cursor_position(), Point::new(1, 2));
    }

    #[gpui::test]
    fn test_with_cursor_notation_selection_cursor_at_end(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("<|hello||>", cx).unwrap();

        assert_eq!(stoat.buffer_text(), "hello");
        let selection = stoat.selection();
        assert!(!selection.is_empty());
        assert_eq!(selection.start, Point::new(0, 0));
        assert_eq!(selection.end, Point::new(0, 5));
        assert!(!selection.reversed);
    }

    #[gpui::test]
    fn test_with_cursor_notation_selection_cursor_at_start(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("<||hello|>", cx).unwrap();

        assert_eq!(stoat.buffer_text(), "hello");
        let selection = stoat.selection();
        assert!(!selection.is_empty());
        assert_eq!(selection.start, Point::new(0, 0));
        assert_eq!(selection.end, Point::new(0, 5));
        assert!(selection.reversed);
    }

    #[gpui::test]
    fn to_cursor_notation_cursor_only(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("hello world", cx);

        stoat.update(|s, _cx| {
            s.set_cursor_position(Point::new(0, 6));
        });

        assert_eq!(stoat.to_cursor_notation(), "hello |world");
    }

    #[gpui::test]
    fn to_cursor_notation_multiline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("line1\nline2\nline3", cx);

        stoat.update(|s, _cx| {
            s.set_cursor_position(Point::new(1, 2));
        });

        assert_eq!(stoat.to_cursor_notation(), "line1\nli|ne2\nline3");
    }

    #[gpui::test]
    fn to_cursor_notation_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("hello", cx);

        stoat.update(|s, _cx| {
            let sel = crate::cursor::Selection::new(Point::new(0, 0), Point::new(0, 5));
            s.cursor.set_selection(sel);
        });

        assert_eq!(stoat.to_cursor_notation(), "<|hello||>");
    }

    #[gpui::test]
    fn assert_cursor_notation_success(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("hello |world", cx).unwrap();
        stoat.assert_cursor_notation("hello |world");
    }

    #[gpui::test]
    #[should_panic(expected = "Buffer state doesn't match expected cursor notation")]
    fn assert_cursor_notation_failure(cx: &mut TestAppContext) {
        let stoat = Stoat::test_with_cursor_notation("hello |world", cx).unwrap();
        stoat.assert_cursor_notation("hello| world");
    }

    #[gpui::test]
    fn round_trip_cursor_notation(cx: &mut TestAppContext) {
        let input = "hello |world\nfoo bar";
        let stoat = Stoat::test_with_cursor_notation(input, cx).unwrap();
        assert_eq!(stoat.to_cursor_notation(), input);
    }

    #[gpui::test]
    fn round_trip_cursor_notation_selection(cx: &mut TestAppContext) {
        let input = "<|hello||> world";
        let stoat = Stoat::test_with_cursor_notation(input, cx).unwrap();
        assert_eq!(stoat.to_cursor_notation(), input);
    }

    // ===== Offset/Point Conversion Tests =====

    #[test]
    fn offset_to_point_single_line() {
        assert_eq!(offset_to_point("hello", 0), Point::new(0, 0));
        assert_eq!(offset_to_point("hello", 3), Point::new(0, 3));
        assert_eq!(offset_to_point("hello", 5), Point::new(0, 5));
    }

    #[test]
    fn offset_to_point_multiline() {
        let text = "line1\nline2\nline3";
        assert_eq!(offset_to_point(text, 0), Point::new(0, 0));
        assert_eq!(offset_to_point(text, 6), Point::new(1, 0)); // Start of line2
        assert_eq!(offset_to_point(text, 8), Point::new(1, 2)); // Middle of line2
        assert_eq!(offset_to_point(text, 12), Point::new(2, 0)); // Start of line3
    }

    #[test]
    fn point_to_offset_single_line() {
        assert_eq!(point_to_offset("hello", Point::new(0, 0)), 0);
        assert_eq!(point_to_offset("hello", Point::new(0, 3)), 3);
        assert_eq!(point_to_offset("hello", Point::new(0, 5)), 5);
    }

    #[test]
    fn point_to_offset_multiline() {
        let text = "line1\nline2\nline3";
        assert_eq!(point_to_offset(text, Point::new(0, 0)), 0);
        assert_eq!(point_to_offset(text, Point::new(1, 0)), 6); // Start of line2
        assert_eq!(point_to_offset(text, Point::new(1, 2)), 8); // Middle of line2
        assert_eq!(point_to_offset(text, Point::new(2, 0)), 12); // Start of line3
    }

    #[test]
    fn offset_point_round_trip() {
        let text = "hello\nworld\ntest";
        let offsets = vec![0, 3, 6, 10, 12];

        for offset in offsets {
            let point = offset_to_point(text, offset);
            let back = point_to_offset(text, point);
            assert_eq!(offset, back, "Round trip failed for offset {}", offset);
        }
    }
}
