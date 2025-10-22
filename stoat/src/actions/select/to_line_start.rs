//! Select to line start action implementation and tests.
//!
//! Demonstrates multi-cursor selection extension to line start.

use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Extend all selections to the start of their lines.
    ///
    /// Each selection extends independently by moving its head to column 0 while keeping
    /// the tail (anchor) fixed.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn select_to_line_start(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        // Auto-sync from cursor if single selection (backward compat)
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

        // Operate on all selections
        let mut selections = self.selections.all::<Point>(&snapshot);
        for selection in &mut selections {
            let head = selection.head();
            let new_head = Point::new(head.row, 0);

            // Extend selection by moving head, keeping tail fixed
            selection.set_head(new_head, text::SelectionGoal::None);
        }

        // Store back and sync cursor
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
    fn extends_to_line_start(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.select_to_line_start(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert!(!selections[0].is_empty());
            assert_eq!(selections[0].head(), text::Point::new(0, 0));
            assert_eq!(selections[0].tail(), text::Point::new(0, 5));
        });
    }

    #[gpui::test]
    fn extends_multiple_selections_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello\nWorld", cx);

            // Create two cursors
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 3), // Middle of "Hello"
                        end: text::Point::new(0, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 4), // Middle of "World"
                        end: text::Point::new(1, 4),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Extend both selections to line start
            s.select_to_line_start(cx);

            // Verify both extended independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 0));
            assert_eq!(selections[0].tail(), text::Point::new(0, 3));
            assert_eq!(selections[1].head(), text::Point::new(1, 0));
            assert_eq!(selections[1].tail(), text::Point::new(1, 4));
        });
    }
}
