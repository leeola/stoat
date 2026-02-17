//! Delete to end of line action implementation and tests.
//!
//! This module implements the [`delete_to_end_of_line`](crate::Stoat::delete_to_end_of_line)
//! action, which removes all text from the cursor position to the end of the current line.
//! Unlike [`delete_line`](crate::Stoat::delete_line), this action preserves the newline
//! character and keeps the cursor at its current position.
//!
//! This action is useful for quickly clearing the remainder of a line without affecting the
//! line structure.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Delete from cursor to end of line.
    ///
    /// Removes all text from the cursor position to the end of the current line,
    /// preserving the newline character. The cursor stays at its current position.
    ///
    /// # Behavior
    ///
    /// - Deletes from cursor to end of line (exclusive of newline)
    /// - Cursor stays at current position
    /// - If cursor is already at end of line: no effect
    /// - Empty lines remain empty
    ///
    /// # Implementation
    ///
    /// Compares cursor column with line length to determine if deletion is needed.
    /// Only performs deletion if there's text between cursor and line end.
    ///
    /// # Related Actions
    ///
    /// - [`delete_line`](crate::Stoat::delete_line) - Delete entire line including newline
    /// - [`delete_right`](crate::Stoat::delete_right) - Delete single character
    pub fn delete_to_end_of_line(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().clone();

        let before_selections = self.selections.disjoint_anchors_arc();
        buffer.update(cx, |buf, _| {
            buf.start_transaction();
        });
        let buffer_snapshot = buffer.read(cx).snapshot();

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
            let line_len = buffer_snapshot.line_len(pos.row);

            if pos.column < line_len {
                let end = text::Point::new(pos.row, line_len);
                debug!(from = ?pos, to = ?end, "Delete to end of line");

                let start_offset = buffer_snapshot.point_to_offset(pos);
                let end_offset = buffer_snapshot.point_to_offset(end);
                edits.push((start_offset..end_offset, ""));
            } else {
                debug!(pos = ?pos, "Already at end of line, nothing to delete");
            }
        }

        // Apply all deletions at once
        if !edits.is_empty() {
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

            let tx = buffer.update(cx, |buf, _| buf.end_transaction());
            if let Some((tx_id, _)) = tx {
                self.selection_history
                    .insert_transaction(tx_id, before_selections);
                self.selection_history
                    .set_after_selections(tx_id, self.selections.disjoint_anchors_arc());
            }

            // Reparse
            buffer_item.update(cx, |item, cx| {
                let _ = item.reparse(cx);
            });

            cx.emit(crate::stoat::StoatEvent::Changed);
        } else {
            buffer.update(cx, |buf, _| {
                buf.end_transaction();
            });
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn deletes_to_end_of_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello World", cx);
            s.set_cursor_position(text::Point::new(0, 6));
            s.delete_to_end_of_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hello ");
        });
    }

    #[gpui::test]
    fn preserves_newline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello\nWorld", cx);
            s.set_cursor_position(text::Point::new(0, 2));
            s.delete_to_end_of_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "He\nWorld");
        });
    }

    #[gpui::test]
    fn no_op_at_end_of_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.delete_to_end_of_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hello");
        });
    }

    #[gpui::test]
    fn cursor_stays_in_place(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello World", cx);
            s.set_cursor_position(text::Point::new(0, 6));
            s.delete_to_end_of_line(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 6));
        });
    }
}
