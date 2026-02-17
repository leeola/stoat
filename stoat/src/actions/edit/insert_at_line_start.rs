use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move cursor to first non-whitespace character of line and enter insert mode.
    pub fn insert_at_line_start(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        let cursor_pos = self.cursor.position();
        let line_text = snapshot
            .text_for_range(
                snapshot.point_to_offset(Point::new(cursor_pos.row, 0))
                    ..snapshot.point_to_offset(Point::new(
                        cursor_pos.row,
                        snapshot.line_len(cursor_pos.row),
                    )),
            )
            .collect::<String>();

        let first_non_ws = line_text
            .chars()
            .position(|c| !c.is_whitespace())
            .unwrap_or(0) as u32;

        let new_pos = Point::new(cursor_pos.row, first_non_ws);

        self.cursor.move_to(new_pos);
        let id = self.selections.next_id();
        self.selections.select(
            vec![text::Selection {
                id,
                start: new_pos,
                end: new_pos,
                reversed: false,
                goal: text::SelectionGoal::None,
            }],
            &snapshot,
        );

        self.set_mode_by_name("insert", cx);
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
            s.set_cursor_position(Point::new(0, 7));
            s.set_mode_by_name("normal", cx);
            s.insert_at_line_start(cx);
            assert_eq!(s.cursor.position(), Point::new(0, 4));
            assert_eq!(s.mode(), "insert");
        });
    }

    #[gpui::test]
    fn handles_no_leading_whitespace(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(Point::new(0, 3));
            s.set_mode_by_name("normal", cx);
            s.insert_at_line_start(cx);
            assert_eq!(s.cursor.position(), Point::new(0, 0));
            assert_eq!(s.mode(), "insert");
        });
    }
}
