//! Delete line action implementation and tests.
//!
//! This module implements the [`delete_line`](crate::Stoat::delete_line) action, which removes
//! the entire line where the cursor is positioned. The implementation handles both regular lines
//! (which include the trailing newline) and the last line of the buffer (which may not have a
//! trailing newline).
//!
//! After deletion, the cursor moves to the beginning of the line (or the next line if the
//! deleted line was not the last line).

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Delete current line.
    ///
    /// Removes the entire line where the cursor is positioned, including the trailing
    /// newline (except for the last line). The cursor moves to the beginning of the line.
    ///
    /// # Behavior
    ///
    /// - For non-last lines: deletes from line start to next line start (includes newline)
    /// - For last line: deletes from line start to line end (no newline to delete)
    /// - Cursor moves to beginning of the line (or next line if not last)
    /// - Empty buffer remains empty
    ///
    /// # Implementation
    ///
    /// Determines line boundaries based on whether the current line is the last line in the
    /// buffer. For non-last lines, includes the newline character in the deletion range.
    ///
    /// # Related Actions
    ///
    /// - [`delete_to_end_of_line`](crate::Stoat::delete_to_end_of_line) - Delete to line end only
    /// - [`new_line`](crate::Stoat::new_line) - Insert new line
    pub fn delete_line(&mut self, cx: &mut Context<Self>) {
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
        let row_count = buffer_snapshot.row_count();

        for selection in &selections {
            let pos = selection.head();
            let line_start = text::Point::new(pos.row, 0);

            let line_end = if pos.row < row_count - 1 {
                text::Point::new(pos.row + 1, 0)
            } else {
                let line_len = buffer_snapshot.line_len(pos.row);
                text::Point::new(pos.row, line_len)
            };

            debug!(row = pos.row, from = ?line_start, to = ?line_end, "Deleting line");

            let start_offset = buffer_snapshot.point_to_offset(line_start);
            let end_offset = buffer_snapshot.point_to_offset(line_end);
            edits.push((start_offset..end_offset, ""));
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
    fn deletes_entire_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(1, 3));
            s.delete_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Line 1\nLine 3");
        });
    }

    #[gpui::test]
    fn deletes_last_line_without_newline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2", cx);
            s.set_cursor_position(text::Point::new(1, 0));
            s.delete_line(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Line 1\n");
        });
    }

    #[gpui::test]
    fn moves_cursor_to_line_start(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello\nWorld", cx);
            s.set_cursor_position(text::Point::new(0, 3));
            s.delete_line(cx);
            assert_eq!(s.cursor.position(), text::Point::new(0, 0));
        });
    }
}
