use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Collapse all selections to their cursor (head) position.
    ///
    /// Non-empty selections become zero-width cursors at the head. Already-collapsed
    /// selections are unchanged.
    pub fn collapse_selection(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let _count = self.take_count();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        let mut selections = self.selections.all::<Point>(&snapshot);
        for selection in &mut selections {
            let head = selection.head();
            selection.start = head;
            selection.end = head;
            selection.reversed = false;
            selection.goal = text::SelectionGoal::None;
        }

        self.selections.select(selections.clone(), &snapshot);
        if let Some(last) = selections.last() {
            self.cursor.move_to(last.head());
        }

        self.enter_normal_mode(cx);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn collapses_selection_to_head(cx: &mut TestAppContext) {
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

            s.collapse_selection(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), Point::new(0, 7));
            assert!(selections[0].is_empty());
        });
    }

    #[gpui::test]
    fn collapses_reversed_selection_to_head(cx: &mut TestAppContext) {
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

            s.collapse_selection(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), Point::new(0, 2));
            assert!(selections[0].is_empty());
        });
    }

    #[gpui::test]
    fn noop_on_collapsed_selection(cx: &mut TestAppContext) {
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

            s.collapse_selection(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), Point::new(0, 3));
            assert!(selections[0].is_empty());
        });
    }
}
