//! Select up action implementation and tests.
//!
//! Demonstrates multi-cursor selection extension upward with goal column preservation.

use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Extend all selections up by one display line.
    ///
    /// Each selection extends independently by moving its head up one display line while
    /// preserving goal column and keeping the tail (anchor) fixed. With DisplayMap, this
    /// correctly handles soft-wrapped lines and folded regions.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn select_up(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        // Get DisplaySnapshot for display-space operations
        let display_snapshot = self.display_map(cx).update(cx, |dm, cx| dm.snapshot(cx));

        // Auto-sync from cursor if single selection (backward compat)
        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<Point>(&snapshot);
            if newest_sel.head() != cursor_pos {
                let id = self.selections.next_id();
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
            let head = selection.head();

            // Convert to display coordinates
            let display_point = display_snapshot.point_to_display_point(head, sum_tree::Bias::Left);

            // Move up in display space
            if display_point.row > 0 {
                // Determine goal column from selection's goal or current column
                let goal_column = match selection.goal {
                    text::SelectionGoal::HorizontalPosition(pos) => pos as u32,
                    _ => display_point.column,
                };

                let target_display_point = stoat_display_map::DisplayPoint {
                    row: display_point.row - 1,
                    column: goal_column,
                };

                // Convert back to buffer coordinates
                let new_head = display_snapshot
                    .display_point_to_point(target_display_point, sum_tree::Bias::Left);

                // Extend selection by moving head, keeping tail fixed
                selection.set_head(
                    new_head,
                    text::SelectionGoal::HorizontalPosition(goal_column as f64),
                );
            }
        }

        // Store back and sync cursor
        self.selections.select(selections.clone(), &snapshot);
        if let Some(last) = selections.last() {
            let goal_col = match last.goal {
                text::SelectionGoal::HorizontalPosition(pos) => pos as u32,
                _ => last.head().column,
            };
            self.cursor.move_to(last.head());
            self.cursor.set_goal_column(goal_col);
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn extends_selection_up(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line1\nLine2", cx);
            s.set_cursor_position(text::Point::new(1, 0));
            s.select_up(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert!(!selections[0].is_empty());
            assert_eq!(selections[0].head(), text::Point::new(0, 0));
            assert_eq!(selections[0].tail(), text::Point::new(1, 0));
        });
    }

    #[gpui::test]
    fn extends_multiple_selections_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3\nLine 4", cx);

            // Create two cursors on different lines
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(1, 3),
                        end: text::Point::new(1, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(3, 3),
                        end: text::Point::new(3, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Extend both selections up
            s.select_up(cx);

            // Verify both extended independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 3));
            assert_eq!(selections[0].tail(), text::Point::new(1, 3));
            assert_eq!(selections[1].head(), text::Point::new(2, 3));
            assert_eq!(selections[1].tail(), text::Point::new(3, 3));
        });
    }
}
