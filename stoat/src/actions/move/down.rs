//! Move down action implementation and tests.
//!
//! Demonstrates multi-cursor vertical movement with goal column preservation.
//! Works symmetrically with [`move_up`](crate::Stoat::move_up).

use crate::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors down one line.
    ///
    /// Each cursor moves independently to the next line while preserving its goal column.
    /// The goal column tracks the desired horizontal position across vertical movements,
    /// allowing navigation through lines of varying lengths.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    ///
    /// # Related Actions
    ///
    /// - [`move_up`](crate::Stoat::move_up) - Move up one line
    /// - [`page_down`](crate::Stoat::page_down) - Move down one page
    pub fn move_down(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();
        let max_row = snapshot.max_point().row;

        // Auto-sync from cursor if single selection (backward compat)
        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<Point>(&snapshot);
            if newest_sel.head() != cursor_pos {
                let id = self.selections.next_id();
                // Preserve cursor's goal column in synced selection
                let goal =
                    text::SelectionGoal::HorizontalPosition(self.cursor.goal_column() as f64);
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: cursor_pos,
                        end: cursor_pos,
                        reversed: false,
                        goal,
                    }],
                    &snapshot,
                );
            }
        }

        // Operate on all selections
        let mut selections = self.selections.all::<Point>(&snapshot);
        for selection in &mut selections {
            // Reset goal if selection has a range
            if !selection.is_empty() {
                selection.goal = text::SelectionGoal::None;
            }

            let head = selection.head();
            if head.row < max_row {
                let target_row = head.row + 1;
                let line_len = snapshot.line_len(target_row);

                // Determine goal column from selection's goal or current column
                let goal_column = match selection.goal {
                    text::SelectionGoal::HorizontalPosition(pos) => pos as u32,
                    _ => head.column,
                };

                let target_column = goal_column.min(line_len);
                let new_pos = Point::new(target_row, target_column);

                // Collapse selection to new cursor position, preserving goal
                selection.start = new_pos;
                selection.end = new_pos;
                selection.reversed = false;
                selection.goal = text::SelectionGoal::HorizontalPosition(goal_column as f64);
            }
        }

        // Store back and sync cursor
        self.selections.select(selections.clone(), &snapshot);
        if let Some(last) = selections.last() {
            // Extract goal column from selection and update cursor
            let goal_col = match last.goal {
                text::SelectionGoal::HorizontalPosition(pos) => pos as u32,
                _ => last.head().column,
            };
            self.cursor.move_to(last.head());
            self.cursor.set_goal_column(goal_col);
        }

        self.ensure_cursor_visible();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_down_one_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_down(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(1, 0));
        });
    }

    #[gpui::test]
    fn no_op_at_last_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2", cx);
            // Set cursor to last line explicitly
            s.set_cursor_position(text::Point::new(1, 0));
            s.move_down(cx);

            // Verify using new multi-cursor API - should stay on last line
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head().row, 1);
        });
    }

    #[gpui::test]
    fn preserves_goal_column(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Very long line\nShort\nVery long line", cx);
            s.set_cursor_position(text::Point::new(0, 10));
            s.move_down(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            // Should clamp to "Short" length (5)
            assert_eq!(selections[0].head(), text::Point::new(1, 5));

            // Moving down again should return to column 10
            s.move_down(cx);
            let selections = s.active_selections(cx);
            assert_eq!(selections[0].head(), text::Point::new(2, 10));
        });
    }

    #[gpui::test]
    fn moves_multiple_cursors_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);

            // Create two cursors
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 3), // Middle of "Line 1"
                        end: text::Point::new(0, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 3), // Middle of "Line 2"
                        end: text::Point::new(1, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Move both cursors down
            s.move_down(cx);

            // Verify both moved independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(1, 3)); // Moved to Line 2
            assert_eq!(selections[1].head(), text::Point::new(2, 3)); // Moved to Line 3
        });
    }
}
