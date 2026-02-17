use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move cursor to end of line and enter insert mode.
    pub fn append_at_line_end(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        let cursor_pos = self.cursor.position();
        let line_len = snapshot.line_len(cursor_pos.row);
        let new_pos = Point::new(cursor_pos.row, line_len);

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
    fn moves_to_line_end_and_inserts(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(Point::new(0, 3));
            s.set_mode_by_name("normal", cx);
            s.append_at_line_end(cx);
            assert_eq!(s.cursor.position(), Point::new(0, 11));
            assert_eq!(s.mode(), "insert");
        });
    }
}
