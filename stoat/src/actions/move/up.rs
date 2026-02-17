//! Move up action implementation and tests.
//!
//! Demonstrates multi-cursor vertical movement with goal column preservation.
//! Each cursor independently moves up while maintaining its horizontal position preference.

use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors up one display line.
    ///
    /// Each cursor moves independently to the previous display line while preserving its goal
    /// column. With DisplayMap, this handles soft-wrapped lines and folded regions correctly.
    /// The goal column tracks the desired horizontal position across vertical movements,
    /// allowing navigation through lines of varying lengths.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    ///
    /// # Related Actions
    ///
    /// - [`move_down`](crate::Stoat::move_down) - Move down one line
    /// - [`page_up`](crate::Stoat::page_up) - Move up one page
    pub fn move_up(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        // Get DisplaySnapshot
        let display_snapshot = self.display_map(cx).update(cx, |dm, cx| dm.snapshot(cx));

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

            // Convert to display coordinates
            let display_point = display_snapshot.point_to_display_point(head, sum_tree::Bias::Left);

            // Move up in display space
            let new_pos = if display_point.row > 0 {
                // Determine goal column from selection's goal or current column
                let goal_column = match selection.goal {
                    text::SelectionGoal::HorizontalPosition(pos) => pos as u32,
                    _ => display_point.column,
                };

                let target_display_point = stoat_text_transform::DisplayPoint {
                    row: display_point.row - 1,
                    column: goal_column,
                };

                // Convert back to buffer coordinates
                let new_buffer_pos = display_snapshot
                    .display_point_to_point(target_display_point, sum_tree::Bias::Left);

                // Preserve goal column
                selection.goal = text::SelectionGoal::HorizontalPosition(goal_column as f64);

                Some(new_buffer_pos)
            } else {
                None // Already at top
            };

            // Apply the new position if we moved
            if let Some(new_pos) = new_pos {
                selection.start = new_pos;
                selection.end = new_pos;
                selection.reversed = false;
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

        self.ensure_cursor_visible(cx);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_up_one_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(2, 0));
            s.move_up(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(1, 0));
        });
    }

    #[gpui::test]
    fn no_op_at_first_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1", cx);
            s.set_cursor_position(text::Point::new(0, 3));
            s.move_up(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 3));
        });
    }

    #[gpui::test]
    fn preserves_goal_column(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Short\nVery long line\nShort", cx);
            s.set_cursor_position(text::Point::new(1, 10));
            s.move_up(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            // Should clamp to "Short" length (5)
            assert_eq!(selections[0].head(), text::Point::new(0, 5));

            // But moving down should return to column 10
            s.move_down(cx);
            let selections = s.active_selections(cx);
            assert_eq!(selections[0].head(), text::Point::new(1, 10));
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
                        start: text::Point::new(1, 3), // Middle of "Line 2"
                        end: text::Point::new(1, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(2, 3), // Middle of "Line 3"
                        end: text::Point::new(2, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Move both cursors up
            s.move_up(cx);

            // Verify both moved independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 3)); // Moved to Line 1
            assert_eq!(selections[1].head(), text::Point::new(1, 3)); // Moved to Line 2
        });
    }
}
