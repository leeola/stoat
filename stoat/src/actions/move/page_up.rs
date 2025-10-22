//! Page up action implementation and tests.
//!
//! Demonstrates multi-cursor page navigation with goal column preservation.

use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors up by one page.
    ///
    /// Each cursor moves independently up by approximately one viewport height,
    /// while preserving its goal column.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn page_up(&mut self, cx: &mut Context<Self>) {
        let lines_per_page = self.viewport_lines.unwrap_or(30.0).floor() as u32;

        if lines_per_page == 0 {
            return;
        }

        let buffer_snapshot = {
            let buffer_item = self.active_buffer(cx).read(cx);
            buffer_item.buffer().read(cx).snapshot()
        };

        // Auto-sync from cursor if single selection (backward compat)
        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<Point>(&buffer_snapshot);
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
                    &buffer_snapshot,
                );
            }
        }

        // Operate on all selections
        let mut selections = self.selections.all::<Point>(&buffer_snapshot);
        for selection in &mut selections {
            // Reset goal if selection has a range
            if !selection.is_empty() {
                selection.goal = text::SelectionGoal::None;
            }

            let head = selection.head();
            let new_row = head.row.saturating_sub(lines_per_page);
            let line_len = buffer_snapshot.line_len(new_row);

            // Determine goal column from selection's goal or current column
            let goal_column = match selection.goal {
                text::SelectionGoal::HorizontalPosition(pos) => pos as u32,
                _ => head.column,
            };

            let new_column = goal_column.min(line_len);
            let new_pos = Point::new(new_row, new_column);

            // Collapse selection to new cursor position, preserving goal
            selection.start = new_pos;
            selection.end = new_pos;
            selection.reversed = false;
            selection.goal = text::SelectionGoal::HorizontalPosition(goal_column as f64);
        }

        // Store back and sync cursor
        self.selections.select(selections.clone(), &buffer_snapshot);
        if let Some(last) = selections.last() {
            let goal_col = match last.goal {
                text::SelectionGoal::HorizontalPosition(pos) => pos as u32,
                _ => last.head().column,
            };
            self.cursor.move_to(last.head());
            self.cursor.set_goal_column(goal_col);

            // Scroll to show the last cursor
            let target_scroll_y = last.head().row.saturating_sub(3) as f32;
            self.scroll
                .start_animation_to(gpui::point(self.scroll.position.x, target_scroll_y));
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_up_one_page(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let mut lines = vec![];
            for i in 0..50 {
                lines.push(format!("Line {i}"));
            }
            s.insert_text(&lines.join("\n"), cx);
            s.set_cursor_position(text::Point::new(40, 0));
            s.page_up(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head().row, 10); // 40 - 30
        });
    }

    #[gpui::test]
    fn moves_multiple_cursors_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let mut lines = vec![];
            for i in 0..50 {
                lines.push(format!("Line {i}"));
            }
            s.insert_text(&lines.join("\n"), cx);

            // Create two cursors
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(35, 0),
                        end: text::Point::new(35, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(45, 0),
                        end: text::Point::new(45, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Move both cursors up by page
            s.page_up(cx);

            // Verify both moved independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head().row, 5); // 35 - 30
            assert_eq!(selections[1].head().row, 15); // 45 - 30
        });
    }
}
