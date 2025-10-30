//! Delete right action implementation and tests.
//!
//! This module implements the [`delete_right`](crate::Stoat::delete_right) action, which deletes
//! the character at the cursor position (forward delete). Unlike [`delete_left`], this action only
//! operates on the main buffer and doesn't route to finder/palette inputs.
//!
//! When at the end of a line, this action merges with the next line by deleting the newline
//! character.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Delete character at cursor position (delete right).
    ///
    /// Removes the character under the cursor without moving the cursor position. At line
    /// endings, merges the current line with the next line by removing the newline.
    ///
    /// # Behavior
    ///
    /// - Mid-line: Deletes character at cursor, cursor stays in place
    /// - At line end: Merges with next line (removes newline), cursor stays in place
    /// - At buffer end: No-op (nothing to delete)
    ///
    /// # Related Actions
    ///
    /// - [`delete_left`](crate::Stoat::delete_left) - Delete character before cursor
    /// - [`delete_word_right`](crate::Stoat::delete_word_right) - Delete next word
    pub fn delete_right(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
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

        // Collect deletion ranges for all selections
        let selections = self.selections.all::<text::Point>(&snapshot);
        let mut edits = Vec::new();
        let max_row = snapshot.max_point().row;

        for selection in &selections {
            let pos = selection.head();
            let line_len = snapshot.line_len(pos.row);

            if pos.column < line_len {
                // Delete character on same line
                let start = snapshot.point_to_offset(pos);
                let end = snapshot.point_to_offset(text::Point::new(pos.row, pos.column + 1));
                edits.push((start..end, ""));
            } else if pos.row < max_row {
                // At line end: merge with next line
                let start = snapshot.point_to_offset(pos);
                let end = snapshot.point_to_offset(text::Point::new(pos.row + 1, 0));
                edits.push((start..end, ""));
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

            // Notify LSP servers of the change
            self.send_did_change_notification(cx);

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
    fn deletes_character_at_cursor(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.delete_right(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "ello");
            assert_eq!(s.cursor.position(), text::Point::new(0, 0));
        });
    }

    #[gpui::test]
    fn merges_lines_at_line_end(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello\nWorld", cx);
            s.set_cursor_position(text::Point::new(0, 5));
            s.delete_right(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "HelloWorld");
        });
    }

    #[gpui::test]
    fn no_op_at_buffer_end(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hi", cx);
            s.delete_right(cx);
            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "Hi");
        });
    }

    #[gpui::test]
    fn deletes_at_multiple_cursors(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("abc\ndef\nghi", cx);

            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 0),
                        end: text::Point::new(0, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 0),
                        end: text::Point::new(1, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 2,
                        start: text::Point::new(2, 0),
                        end: text::Point::new(2, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            s.delete_right(cx);

            let content = s.active_buffer(cx).read(cx).buffer().read(cx).text();
            assert_eq!(content, "bc\nef\nhi");
        });
    }
}
