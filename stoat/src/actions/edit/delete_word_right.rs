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
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let buffer_snapshot = buffer.read(cx).snapshot();
        let token_snapshot = buffer_item.read(cx).token_snapshot();

        // Auto-sync from cursor if single selection (backward compat)
        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<text::Point>(&buffer_snapshot);
            if newest_sel.head() != cursor_pos {
                let id = self.selections.next_id();
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: cursor_pos,
                        end: cursor_pos,
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    }],
                    &buffer_snapshot,
                );
            }
        }

        // Collect deletion ranges for all selections
        let selections = self.selections.all::<text::Point>(&buffer_snapshot);
        let mut edits = Vec::new();

        for selection in &selections {
            let pos = selection.head();
            let pos_offset = buffer_snapshot.point_to_offset(pos);

            let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
            token_cursor.next();

            let mut found_symbol_end: Option<usize> = None;

            // Iterate through tokens to find the next symbol
            while let Some(token) = token_cursor.item() {
                let token_end = token.range.end.to_offset(&buffer_snapshot);

                if token_end <= pos_offset {
                    token_cursor.next();
                    continue;
                }

                if token.kind.is_symbol() {
                    found_symbol_end = Some(token_end);
                    break;
                }

                token_cursor.next();
            }

            if let Some(end_offset) = found_symbol_end {
                edits.push((pos_offset..end_offset, ""));
            }
        }

        // Apply all deletions at once
        if !edits.is_empty() {
            let buffer = buffer.clone();
            buffer.update(cx, |buffer, _| {
                buffer.edit(edits);
            });

            // Get updated selections (anchors have auto-adjusted)
            let snapshot = buffer.read(cx).snapshot();
            let updated_selections = self.selections.all::<text::Point>(&snapshot);

            // Sync cursor to last selection
            if let Some(last) = updated_selections.last() {
                self.cursor.move_to(last.head());
            }

            // Reparse
            buffer_item.update(cx, |item, cx| {
                let _ = item.reparse(cx);
            });

            cx.emit(crate::stoat::StoatEvent::Changed);
        }

        cx.notify();
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
