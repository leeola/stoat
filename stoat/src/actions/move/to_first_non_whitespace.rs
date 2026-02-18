use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors to the first non-whitespace character on their line.
    ///
    /// Scans forward from column 0 of each cursor's line, skipping spaces and tabs.
    /// If the line is all whitespace, moves to column 0.
    pub fn move_to_first_non_whitespace(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let _count = self.take_count();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

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
                        goal: text::SelectionGoal::None,
                    }],
                    &snapshot,
                );
            }
        }

        let mut selections = self.selections.all::<Point>(&snapshot);
        for selection in &mut selections {
            let head = selection.head();
            let line_start_offset = snapshot.point_to_offset(Point::new(head.row, 0));
            let chars = snapshot.chars_at(line_start_offset);

            let mut col = 0u32;
            for ch in chars {
                if ch == '\n' || !ch.is_whitespace() {
                    break;
                }
                col += ch.len_utf8() as u32;
            }

            let new_pos = Point::new(head.row, col);
            selection.start = new_pos;
            selection.end = new_pos;
            selection.reversed = false;
            selection.goal = text::SelectionGoal::None;
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
    fn moves_to_first_non_whitespace(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("    hello", cx);
            s.set_cursor_position(Point::new(0, 8));
            s.move_to_first_non_whitespace(cx);
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 4));
        });
    }

    #[gpui::test]
    fn moves_to_zero_on_no_whitespace(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(Point::new(0, 3));
            s.move_to_first_non_whitespace(cx);
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 0));
        });
    }

    #[gpui::test]
    fn handles_tabs(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("\t\thello", cx);
            s.set_cursor_position(Point::new(0, 5));
            s.move_to_first_non_whitespace(cx);
            assert_eq!(s.active_selections(cx)[0].head(), Point::new(0, 2));
        });
    }
}
