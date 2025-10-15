//! New line action implementation and tests.
//!
//! This module implements the [`new_line`](crate::Stoat::new_line) action, which inserts a
//! newline character at the cursor position. This is the primary action for creating new lines
//! in the buffer, typically bound to the Enter/Return key.
//!
//! After insertion, the cursor moves to the beginning of the new line.

use crate::Stoat;
use gpui::Context;

impl Stoat {
    /// Insert newline at cursor.
    ///
    /// Inserts a newline character (`\n`) at the current cursor position, splitting the line
    /// if the cursor is mid-line. The cursor moves to the beginning of the newly created line.
    ///
    /// # Behavior
    ///
    /// - Inserts `\n` at cursor position
    /// - Moves cursor to next line, column 0
    /// - Triggers reparse for syntax highlighting
    /// - Emits Changed event for UI updates
    ///
    /// # Related Actions
    ///
    /// - [`insert_text`](crate::Stoat::insert_text) - Insert arbitrary text
    /// - [`delete_line`](crate::Stoat::delete_line) - Delete entire line
    pub fn new_line(&mut self, cx: &mut Context<Self>) {
        let cursor = self.cursor.position();
        let buffer = self.active_buffer(cx).read(cx).buffer().clone();
        buffer.update(cx, |buffer, _| {
            let offset = buffer.point_to_offset(cursor);
            buffer.edit([(offset..offset, "\n")]);
        });

        // Move cursor to next line, column 0
        self.cursor.move_to(text::Point::new(cursor.row + 1, 0));

        // Reparse
        self.active_buffer(cx).update(cx, |item, cx| {
            let _ = item.reparse(cx);
        });

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn inserts_newline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.new_line(cx);
            s.insert_text("World", cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hello\nWorld");
        });
    }

    #[gpui::test]
    fn moves_cursor_to_next_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hi", cx);
            s.new_line(cx);
            assert_eq!(s.cursor.position(), text::Point::new(1, 0));
        });
    }

    #[gpui::test]
    fn splits_line_when_mid_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("HelloWorld", cx);
            s.set_cursor_position(text::Point::new(0, 5));
            s.new_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hello\nWorld");
            assert_eq!(s.cursor.position(), text::Point::new(1, 0));
        });
    }
}
