use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Select all text in the buffer.
    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        let end = snapshot.max_point();
        let id = self.selections.next_id();
        self.selections.select(
            vec![text::Selection {
                id,
                start: text::Point::new(0, 0),
                end,
                reversed: false,
                goal: text::SelectionGoal::None,
            }],
            &snapshot,
        );
        self.cursor.move_to(end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn selects_entire_buffer(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello\nworld", cx);
            s.select_all(cx);
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].start, text::Point::new(0, 0));
            assert_eq!(selections[0].end, text::Point::new(1, 5));
        });
    }
}
