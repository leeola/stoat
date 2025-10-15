//! Delete word left action implementation and tests.
//!
//! This module implements the [`delete_word_left`](crate::Stoat::delete_word_left) action, which
//! deletes from the start of the previous symbol/word to the cursor position. This action uses
//! the [`TokenSnapshot`](crate::buffer_item::TokenSnapshot) from the active buffer to identify
//! word boundaries based on syntax tokens.
//!
//! The implementation differentiates between deleting within a symbol (cursor inside a word) and
//! deleting across symbols (cursor between words), providing vim-like word deletion behavior.

use crate::Stoat;
use gpui::Context;
use text::ToOffset;

impl Stoat {
    /// Delete word (symbol) before cursor.
    ///
    /// Deletes from the start of the previous symbol to the cursor position,
    /// removing the previous word along with any intervening whitespace.
    ///
    /// # Behavior
    ///
    /// - Finds previous symbol boundary using [`TokenSnapshot`](crate::buffer_item::TokenSnapshot)
    /// - Deletes from symbol start to cursor
    /// - Moves cursor to deletion start
    /// - If no previous symbol, does nothing
    /// - Triggers reparse for syntax highlighting
    ///
    /// # Implementation
    ///
    /// Uses the token cursor to iterate through syntax tokens and find the start of the
    /// previous symbol. Handles both cases where the cursor is inside a symbol and where
    /// it's positioned after a symbol.
    ///
    /// # Related Actions
    ///
    /// - [`delete_word_right`](crate::Stoat::delete_word_right) - Delete next word
    /// - [`delete_left`](crate::Stoat::delete_left) - Delete single character
    pub fn delete_word_left(&mut self, cx: &mut Context<Self>) {
        // Get buffer and token snapshots
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        let cursor_pos = self.cursor.position();
        let cursor_offset = buffer_snapshot.point_to_offset(cursor_pos);

        let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
        token_cursor.next();

        let mut prev_symbol_start: Option<usize> = None;

        // Iterate through tokens to find the previous symbol
        while let Some(token) = token_cursor.item() {
            let token_start = token.range.start.to_offset(&buffer_snapshot);
            let token_end = token.range.end.to_offset(&buffer_snapshot);

            // If we've passed the cursor, we're done
            if token_start >= cursor_offset {
                break;
            }

            // Check if this token is a symbol
            if token.kind.is_symbol() {
                // If cursor is inside or at the end of this token, delete from start to cursor
                if token_start < cursor_offset && cursor_offset <= token_end {
                    prev_symbol_start = Some(token_start);
                    break;
                }

                // Track symbols that end strictly before cursor
                if token_end < cursor_offset {
                    prev_symbol_start = Some(token_start);
                }
            }

            token_cursor.next();
        }

        // Delete from symbol start to cursor if found
        if let Some(start_offset) = prev_symbol_start {
            let delete_start = buffer_snapshot.offset_to_point(start_offset);

            // Perform deletion
            let buffer = self.active_buffer(cx).read(cx).buffer().clone();
            buffer.update(cx, |buffer, _| {
                let start = buffer.point_to_offset(delete_start);
                let end = buffer.point_to_offset(cursor_pos);
                buffer.edit([(start..end, "")]);
            });

            // Move cursor to deletion start
            self.cursor.move_to(delete_start);

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
    fn deletes_previous_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.delete_word_left(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "hello ");
        });
    }

    #[gpui::test]
    fn deletes_to_word_start_when_mid_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(text::Point::new(0, 3));
            s.delete_word_left(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "lo");
        });
    }

    #[gpui::test]
    fn no_op_without_previous_symbol(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("   ", cx);
            s.delete_word_left(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "   ");
        });
    }
}
