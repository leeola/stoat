//! New line action implementation and tests.
//!
//! This module implements the [`new_line`](crate::Stoat::new_line) action, which inserts a
//! newline character at the cursor position. This is the primary action for creating new lines
//! in the buffer, typically bound to the Enter/Return key.
//!
//! After insertion, the cursor moves to the beginning of the new line.

use crate::stoat::Stoat;
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
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().clone();

        let before_selections = self.selections.disjoint_anchors_arc();
        buffer.update(cx, |buf, _| {
            buf.start_transaction();
        });
        let snapshot = buffer.read(cx).snapshot();

        // Auto-sync from cursor if single selection (backward compat)
        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<text::Point>(&snapshot);
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
                    &snapshot,
                );
            }
        }

        // Collect insertion points for all selections (sorted by offset ascending)
        let mut selections_with_offsets: Vec<_> = self
            .selections
            .all::<text::Point>(&snapshot)
            .into_iter()
            .map(|sel| {
                let offset = snapshot.point_to_offset(sel.head());
                (offset, sel)
            })
            .collect();
        selections_with_offsets.sort_by_key(|(offset, _)| *offset);

        // Collect all edits to apply at once
        let edits: Vec<_> = selections_with_offsets
            .iter()
            .map(|(offset, _)| (*offset..*offset, "\n"))
            .collect();

        // Apply all insertions at once
        buffer.update(cx, |buffer, _| {
            buffer.edit(edits);
        });

        // Calculate new positions accounting for all prior insertions
        // Each newline is 1 byte, and moves cursor to next line, column 0
        let snapshot = buffer.read(cx).snapshot();
        let mut new_selections = Vec::new();
        let id_start = self.selections.next_id();

        for (i, (old_offset, _)) in selections_with_offsets.iter().enumerate() {
            // Each newline insertion before this one shifts positions forward by 1 byte
            let shift = i;
            // After inserting newline, cursor is 1 byte after the original position + shift
            let new_offset = old_offset + shift + 1;
            let new_pos = snapshot.offset_to_point(new_offset);

            new_selections.push(text::Selection {
                id: id_start + i,
                start: new_pos,
                end: new_pos,
                reversed: false,
                goal: text::SelectionGoal::None,
            });
        }

        // Update selections to new positions
        self.selections.select(new_selections.clone(), &snapshot);

        // Sync cursor to last selection
        if let Some(last) = new_selections.last() {
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

    #[gpui::test]
    fn inserts_newlines_at_multiple_cursors(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("line1line2line3", cx);

            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 5),
                        end: text::Point::new(0, 5),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(0, 10),
                        end: text::Point::new(0, 10),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            s.new_line(cx);

            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "line1\nline2\nline3");
        });
    }
}
