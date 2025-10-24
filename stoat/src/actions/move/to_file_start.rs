//! Move to file start action implementation and tests.
//!
//! Demonstrates multi-cursor file navigation.

use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors to the beginning of the file.
    ///
    /// All cursors collapse to a single cursor at position (0, 0).
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn move_to_file_start(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        // Move all cursors to (0, 0) - they will merge into one
        let id = self.selections.next_id();
        self.selections.select(
            vec![text::Selection {
                id,
                start: Point::new(0, 0),
                end: Point::new(0, 0),
                reversed: false,
                goal: text::SelectionGoal::None,
            }],
            &snapshot,
        );

        self.cursor.move_to(Point::new(0, 0));
        self.ensure_cursor_visible(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_row_zero_column_zero(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.move_to_file_start(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 0));
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
                        start: text::Point::new(0, 3),
                        end: text::Point::new(0, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(2, 3),
                        end: text::Point::new(2, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Move to file start - should merge into single cursor
            s.move_to_file_start(cx);

            // Verify merged to single cursor at (0, 0)
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 0));
        });
    }
}
