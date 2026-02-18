use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Flip (swap anchor and head) for all selections.
    ///
    /// Each selection's direction is reversed: the head becomes the tail and
    /// vice versa. The cursor syncs to the new head of the primary selection.
    pub fn flip_selection(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let _count = self.take_count();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        let mut selections = self.selections.all::<Point>(&snapshot);
        for selection in &mut selections {
            selection.reversed = !selection.reversed;
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
    fn flips_forward_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello World", cx);
            let snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![text::Selection {
                    id,
                    start: Point::new(0, 2),
                    end: Point::new(0, 7),
                    reversed: false,
                    goal: text::SelectionGoal::None,
                }],
                &snapshot,
            );

            s.flip_selection(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert!(selections[0].reversed);
            assert_eq!(selections[0].head(), Point::new(0, 2));
            assert_eq!(selections[0].tail(), Point::new(0, 7));
        });
    }

    #[gpui::test]
    fn flips_reversed_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello World", cx);
            let snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![text::Selection {
                    id,
                    start: Point::new(0, 2),
                    end: Point::new(0, 7),
                    reversed: true,
                    goal: text::SelectionGoal::None,
                }],
                &snapshot,
            );

            s.flip_selection(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert!(!selections[0].reversed);
            assert_eq!(selections[0].head(), Point::new(0, 7));
        });
    }

    #[gpui::test]
    fn noop_on_collapsed(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);

            let snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![text::Selection {
                    id,
                    start: Point::new(0, 3),
                    end: Point::new(0, 3),
                    reversed: false,
                    goal: text::SelectionGoal::None,
                }],
                &snapshot,
            );

            s.flip_selection(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), Point::new(0, 3));
        });
    }
}
