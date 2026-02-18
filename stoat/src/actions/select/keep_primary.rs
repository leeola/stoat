use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Remove all selections except the primary (newest) one.
    ///
    /// The primary selection is the most recently added. All other cursors
    /// and selections are discarded. Syncs the legacy cursor to the primary.
    pub fn keep_primary_selection(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let _count = self.take_count();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        let primary = self.selections.newest::<Point>(&snapshot);
        let kept = vec![text::Selection {
            id: self.selections.next_id(),
            start: primary.start,
            end: primary.end,
            reversed: primary.reversed,
            goal: primary.goal,
        }];

        self.selections.select(kept.clone(), &snapshot);
        if let Some(sel) = kept.first() {
            self.cursor.move_to(sel.head());
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn keeps_only_primary(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello\nWorld\nFoo", cx);
            let snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: Point::new(0, 0),
                        end: Point::new(0, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: Point::new(1, 0),
                        end: Point::new(1, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 2,
                        start: Point::new(2, 0),
                        end: Point::new(2, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &snapshot,
            );

            s.keep_primary_selection(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
        });
    }

    #[gpui::test]
    fn noop_with_single_cursor(cx: &mut TestAppContext) {
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

            s.keep_primary_selection(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), Point::new(0, 3));
        });
    }
}
