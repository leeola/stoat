//! Select right action implementation and tests.
//!
//! Demonstrates multi-cursor selection extension to the right.

use crate::stoat::Stoat;
use gpui::Context;
use text::{Bias, Point};

impl Stoat {
    /// Extend all selections right by one character.
    ///
    /// Each selection extends independently by moving its head right while keeping
    /// the tail (anchor) fixed. Correctly handles multi-byte UTF-8 characters.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn select_right(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
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
            let line_len = snapshot.line_len(head.row);

            if head.column < line_len {
                let target = Point::new(head.row, head.column + 1);
                let new_head = snapshot.clip_point(target, Bias::Right);

                // Extend selection by moving head, keeping tail fixed
                selection.set_head(new_head, text::SelectionGoal::None);
            }
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
    fn extends_selection_right(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.select_right(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert!(!selections[0].is_empty());
            assert_eq!(selections[0].head(), text::Point::new(0, 1));
            assert_eq!(selections[0].tail(), text::Point::new(0, 0));
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
                        start: text::Point::new(0, 0), // Start of "Hello"
                        end: text::Point::new(0, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 0), // Start of "World"
                        end: text::Point::new(1, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Extend both selections right
            s.select_right(cx);

            // Verify both extended independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 1));
            assert_eq!(selections[0].tail(), text::Point::new(0, 0));
            assert_eq!(selections[1].head(), text::Point::new(1, 1));
            assert_eq!(selections[1].tail(), text::Point::new(1, 0));
        });
    }
}
