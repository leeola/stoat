use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors down by half a page.
    ///
    /// Each cursor moves independently down by approximately half the viewport height in display
    /// space, while preserving its goal column. Scrolls to keep the cursor visible.
    pub fn half_page_down(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let count = self.take_count();
        let half_page = (self.viewport_lines.unwrap_or(30.0) / 2.0).floor() as u32;

        if half_page == 0 {
            return;
        }

        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let buffer_snapshot = buffer.read(cx).snapshot();

        let display_snapshot = self.display_map(cx).update(cx, |dm, cx| dm.snapshot(cx));
        let max_display_point = display_snapshot.max_point();

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

        let mut selections = self.selections.all::<Point>(&buffer_snapshot);
        for selection in &mut selections {
            if !selection.is_empty() {
                selection.goal = text::SelectionGoal::None;
            }

            let head = selection.head();
            let display_point = display_snapshot.point_to_display_point(head, sum_tree::Bias::Left);
            let new_display_row =
                (display_point.row + half_page * count).min(max_display_point.row);

            let goal_column = match selection.goal {
                text::SelectionGoal::HorizontalPosition(pos) => pos as u32,
                _ => display_point.column,
            };

            let target_display_point = stoat_text_transform::DisplayPoint {
                row: new_display_row,
                column: goal_column,
            };

            let new_pos =
                display_snapshot.display_point_to_point(target_display_point, sum_tree::Bias::Left);

            selection.start = new_pos;
            selection.end = new_pos;
            selection.reversed = false;
            selection.goal = text::SelectionGoal::HorizontalPosition(goal_column as f64);
        }

        self.selections.select(selections.clone(), &buffer_snapshot);
        if let Some(last) = selections.last() {
            let goal_col = match last.goal {
                text::SelectionGoal::HorizontalPosition(pos) => pos as u32,
                _ => last.head().column,
            };
            self.cursor.move_to(last.head());
            self.cursor.set_goal_column(goal_col);

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
    fn moves_down_half_page(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let mut lines = vec![];
            for i in 0..50 {
                lines.push(format!("Line {i}"));
            }
            s.insert_text(&lines.join("\n"), cx);
            s.set_cursor_position(Point::new(10, 0));
            s.half_page_down(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head().row, 25); // 10 + 15
        });
    }

    #[gpui::test]
    fn clamps_at_bottom(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let mut lines = vec![];
            for i in 0..50 {
                lines.push(format!("Line {i}"));
            }
            s.insert_text(&lines.join("\n"), cx);
            s.set_cursor_position(Point::new(45, 0));
            s.half_page_down(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head().row, 49);
        });
    }
}
