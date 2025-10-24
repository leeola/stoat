//! Move to file end action implementation and tests.
//!
//! Demonstrates multi-cursor file navigation.

use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors to the end of the file.
    ///
    /// All cursors collapse to a single cursor at the last position in the file.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn move_to_file_end(&mut self, cx: &mut Context<Self>) {
        let buffer_snapshot = {
            let buffer_item = self.active_buffer(cx).read(cx);
            buffer_item.buffer().read(cx).snapshot()
        };

        let last_row = buffer_snapshot.row_count().saturating_sub(1);
        let last_line_len = buffer_snapshot.line_len(last_row);
        let new_pos = Point::new(last_row, last_line_len);

        // Move all cursors to file end - they will merge into one
        let id = self.selections.next_id();
        self.selections.select(
            vec![text::Selection {
                id,
                start: new_pos,
                end: new_pos,
                reversed: false,
                goal: text::SelectionGoal::None,
            }],
            &buffer_snapshot,
        );

        self.cursor.move_to(new_pos);
        self.ensure_cursor_visible(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_last_position(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_to_file_end(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(2, 6));
        });
    }

    #[gpui::test]
    fn merges_multiple_cursors(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);

            // Create multiple cursors
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
                ],
                &buffer_snapshot,
            );

            // Move to file end - should merge into single cursor
            s.move_to_file_end(cx);

            // Verify merged to single cursor at end
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(2, 6));
        });
    }
}
