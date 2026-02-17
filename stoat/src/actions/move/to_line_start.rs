//! Move to line start action implementation and tests.
//!
//! Demonstrates multi-cursor horizontal position movement.

use crate::stoat::Stoat;
use gpui::Context;
use text::Point;

impl Stoat {
    /// Move all cursors to start of their respective lines.
    ///
    /// Each cursor moves independently to column 0 of its current line.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn move_to_line_start(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let _count = self.take_count();
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
            let new_pos = Point::new(head.row, 0);

            // Collapse selection to line start
            selection.start = new_pos;
            selection.end = new_pos;
            selection.reversed = false;
            selection.goal = text::SelectionGoal::None;
        }

        // Store back and sync cursor
        self.selections.select(selections.clone(), &snapshot);
        if let Some(last) = selections.last() {
            self.cursor.move_to(last.head());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_column_zero(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello World", cx);
            s.move_to_line_start(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 0));
        });
    }

    #[gpui::test]
    fn moves_multiple_cursors_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello\nWorld", cx);

            // Create two cursors in middle of lines
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 3),
                        end: text::Point::new(0, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 3),
                        end: text::Point::new(1, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Move both cursors to line start
            s.move_to_line_start(cx);

            // Verify both moved to column 0
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 0));
            assert_eq!(selections[1].head(), text::Point::new(1, 0));
        });
    }
}
