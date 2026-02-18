use crate::stoat::Stoat;
use gpui::Context;
use text::{Point, SelectionGoal};

impl Stoat {
    /// Select the current line, or extend an existing line selection downward.
    ///
    /// Helix-style `x` behavior: selects from start of current line to start of
    /// next line (including the newline). If the selection already spans full lines,
    /// extends it downward by one line instead. Respects count prefix.
    pub fn select_line(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let count = self.take_count();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();
        let max_point = snapshot.max_point();

        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<Point>(&snapshot);
            if newest_sel.head() != cursor_pos {
                let id = self.selections.next_id();
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: cursor_pos,
                        end: cursor_pos,
                        reversed: false,
                        goal: SelectionGoal::None,
                    }],
                    &snapshot,
                );
            }
        }

        let mut selections = self.selections.all::<Point>(&snapshot);
        for selection in &mut selections {
            let tail = selection.tail();
            let head = selection.head();

            let is_line_sel =
                tail.column == 0 && head > tail && (head.column == 0 || head == max_point);

            if is_line_sel {
                let target_row = head.row + count;
                let new_head = if target_row <= max_point.row {
                    Point::new(target_row, 0)
                } else {
                    max_point
                };
                selection.set_head(new_head, SelectionGoal::None);
            } else {
                let line_start = Point::new(head.row, 0);
                let target_row = head.row + count;
                let new_head = if target_row <= max_point.row {
                    Point::new(target_row, 0)
                } else {
                    max_point
                };
                selection.start = line_start;
                selection.end = line_start;
                selection.reversed = false;
                selection.set_head(new_head, SelectionGoal::None);
            }
        }

        self.selections.select(selections.clone(), &snapshot);
        if let Some(last) = selections.last() {
            self.cursor.move_to(last.head());
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn single_line_buffer(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(Point::new(0, 2));
            s.select_line(cx);

            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 1);
            assert_eq!(sels[0].tail(), Point::new(0, 0));
            assert_eq!(sels[0].head(), Point::new(0, 5));
        });
    }

    #[gpui::test]
    fn selects_current_line_with_newline(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("aaa\nbbb\nccc", cx);
            s.set_cursor_position(Point::new(1, 1));
            s.select_line(cx);

            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 1);
            assert_eq!(sels[0].tail(), Point::new(1, 0));
            assert_eq!(sels[0].head(), Point::new(2, 0));
        });
    }

    #[gpui::test]
    fn repeat_extends_downward(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("aaa\nbbb\nccc\nddd", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.select_line(cx);
            s.select_line(cx);

            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 1);
            assert_eq!(sels[0].tail(), Point::new(0, 0));
            assert_eq!(sels[0].head(), Point::new(2, 0));
        });
    }

    #[gpui::test]
    fn count_selects_multiple_lines(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("aaa\nbbb\nccc\nddd", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.pending_count = Some(3);
            s.select_line(cx);

            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 1);
            assert_eq!(sels[0].tail(), Point::new(0, 0));
            assert_eq!(sels[0].head(), Point::new(3, 0));
        });
    }

    #[gpui::test]
    fn last_line_selects_to_end(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("aaa\nbbb", cx);
            s.set_cursor_position(Point::new(1, 0));
            s.select_line(cx);

            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 1);
            assert_eq!(sels[0].tail(), Point::new(1, 0));
            assert_eq!(sels[0].head(), Point::new(1, 3));
        });
    }

    #[gpui::test]
    fn extend_past_last_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("aaa\nbbb", cx);
            s.set_cursor_position(Point::new(0, 0));
            s.select_line(cx);
            s.select_line(cx);

            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 1);
            assert_eq!(sels[0].tail(), Point::new(0, 0));
            assert_eq!(sels[0].head(), Point::new(1, 3));
        });
    }
}
