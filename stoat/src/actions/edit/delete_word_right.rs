//! Delete word right action implementation and tests.
//!
//! This module implements the [`delete_word_right`](crate::Stoat::delete_word_right) action,
//! which deletes from the cursor position to the end of the next symbol/word. Like
//! [`delete_word_left`](crate::Stoat::delete_word_left), this action uses the
//! [`TokenSnapshot`](crate::buffer_item::TokenSnapshot) to identify word boundaries.
//!
//! The cursor stays at its current position after deletion, making this action useful for
//! removing forward text without changing cursor placement.

use crate::Stoat;
use gpui::Context;
use text::ToOffset;

impl Stoat {
    /// Delete word (symbol) after cursor.
    ///
    /// Deletes from the cursor position to the end of the next symbol,
    /// removing the next word along with any intervening whitespace.
    ///
    /// # Behavior
    ///
    /// - Finds next symbol boundary using [`TokenSnapshot`](crate::buffer_item::TokenSnapshot)
    /// - Deletes from cursor to symbol end
    /// - Cursor stays at current position
    /// - If no next symbol, does nothing
    /// - Triggers reparse for syntax highlighting
    ///
    /// # Implementation
    ///
    /// Iterates through tokens starting from the cursor position to find the next symbol.
    /// Once found, deletes from cursor to the end of that symbol.
    ///
    /// # Related Actions
    ///
    /// - [`delete_word_left`](crate::Stoat::delete_word_left) - Delete previous word
    /// - [`delete_right`](crate::Stoat::delete_right) - Delete single character
    pub fn delete_word_right(&mut self, cx: &mut Context<Self>) {
        // Get buffer and token snapshots
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        // Create a cursor to iterate through tokens
        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut found_symbol_end: Option<usize> = None;

        // Iterate through tokens to find the next symbol
        while let Some(token) = token_cursor.item() {
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // Skip tokens that are entirely before the cursor
            if token_end <= cursor_offset {
                token_cursor.next();
                continue;
            }

            // Check if this token is a symbol
            if token.kind.is_symbol() {
                // Found a symbol - delete to its end
                found_symbol_end = Some(token_end);
                break;
            }

            // Not a symbol, keep looking
            token_cursor.next();
        }

        // Delete from cursor to symbol end if found
        if let Some(end_offset) = found_symbol_end {
            let delete_end = buffer_snapshot.offset_to_point(end_offset);

            // Perform deletion
            let buffer = self.active_buffer(cx).read(cx).buffer().clone();
            buffer.update(cx, |buffer, _| {
                let start = buffer.point_to_offset(cursor_pos);
                let end = buffer.point_to_offset(delete_end);
                buffer.edit([(start..end, "")]);
            });

            // Cursor stays in place

            // Reparse
            self.active_buffer(cx).update(cx, |item, cx| {
                let _ = item.reparse(cx);
            });

            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn deletes_next_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.delete_word_right(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, " world");
        });
    }

    #[gpui::test]
    fn cursor_stays_in_place(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(text::Point::new(0, 6));
            let pos_before = s.cursor.position();
            s.delete_word_right(cx);
            assert_eq!(s.cursor.position(), pos_before);
        });
    }

    #[gpui::test]
    fn no_op_without_next_symbol(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello   ", cx);
            s.delete_word_right(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello   ");
        });
    }
}
