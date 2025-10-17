//! Move left action implementation and tests.

use crate::Stoat;
use gpui::Context;
use text::{Bias, Point};

impl Stoat {
    /// Move all cursors left one character.
    ///
    /// Each cursor moves independently to the previous character position. Correctly handles
    /// multi-byte UTF-8 characters by clipping to the nearest character boundary. Uses
    /// [`SelectionsCollection`](crate::SelectionsCollection) for multi-cursor support.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    ///
    /// # Related Actions
    ///
    /// - [`move_right`](crate::Stoat::move_right) - Move right one character
    /// - [`move_word_left`](crate::Stoat::move_word_left) - Move left one word
    pub fn move_left(&mut self, cx: &mut Context<Self>) {
        // In anchored selection mode, use selection extension instead of cursor movement
        if self.is_mode_anchored() {
            self.select_left(cx);
            return;
        }

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
            if head.column > 0 {
                let target = Point::new(head.row, head.column - 1);
                let clipped = snapshot.clip_point(target, Bias::Left);

                // Collapse selection to new cursor position
                selection.start = clipped;
                selection.end = clipped;
                selection.reversed = false;
                selection.goal = text::SelectionGoal::None;
            }
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
    fn moves_left_one_character(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.move_left(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 4));
        });
    }

    #[gpui::test]
    fn no_op_at_start_of_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hi", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_left(cx);

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

            // Create two cursors
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 5), // End of "Hello"
                        end: text::Point::new(0, 5),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 5), // End of "World"
                        end: text::Point::new(1, 5),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Move both cursors left
            s.move_left(cx);

            // Verify both moved independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 4));
            assert_eq!(selections[1].head(), text::Point::new(1, 4));
        });
    }
}
