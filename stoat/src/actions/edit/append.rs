use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move cursor right one character (clamped to line end) and enter insert mode.
    pub fn append(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        let cursor_pos = self.cursor.position();
        let line_len = snapshot.line_len(cursor_pos.row);
        let new_col = (cursor_pos.column + 1).min(line_len);
        let new_pos = Point::new(cursor_pos.row, new_col);

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
    fn appends_after_cursor(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello", cx);
            s.set_cursor_position(Point::new(0, 2));
            s.set_mode_by_name("normal", cx);
            s.append(cx);
            assert_eq!(s.cursor.position(), Point::new(0, 3));
            assert_eq!(s.mode(), "insert");
        });
    }

    #[gpui::test]
    fn clamps_to_line_end(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hi", cx);
            s.set_cursor_position(Point::new(0, 2));
            s.set_mode_by_name("normal", cx);
            s.append(cx);
            assert_eq!(s.cursor.position(), Point::new(0, 2));
            assert_eq!(s.mode(), "insert");
        });
    }
}
