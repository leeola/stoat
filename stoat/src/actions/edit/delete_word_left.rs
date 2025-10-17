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

            let mut prev_symbol_start: Option<usize> = None;

            // Iterate through tokens to find the previous symbol
            while let Some(token) = token_cursor.item() {
                let token_start = token.range.start.to_offset(&buffer_snapshot);
                let token_end = token.range.end.to_offset(&buffer_snapshot);

                if token_start >= pos_offset {
                    break;
                }

                if token.kind.is_symbol() {
                    if token_start < pos_offset && pos_offset <= token_end {
                        prev_symbol_start = Some(token_start);
                        break;
                    }

                    if token_end < pos_offset {
                        prev_symbol_start = Some(token_start);
                    }
                }

                token_cursor.next();
            }

            if let Some(start_offset) = prev_symbol_start {
                edits.push((start_offset..pos_offset, ""));
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
