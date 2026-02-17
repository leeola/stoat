//! Move right action implementation and tests.
//!
//! Demonstrates multi-cursor support using [`SelectionsCollection`]. Each cursor
//! moves independently, and the selection storage uses anchors for persistence
//! across buffer edits.

use crate::stoat::Stoat;
use gpui::Context;
use text::{Bias, Point};

impl Stoat {
    /// Move all cursors right one character.
    ///
    /// Each cursor moves independently to the next character position. Uses
    /// [`SelectionsCollection`](crate::SelectionsCollection) for efficient
    /// multi-cursor support with anchor-based storage.
    ///
    /// Updates both the new selections field and the legacy cursor field for
    /// backward compatibility during migration.
    ///
    /// # Performance
    ///
    /// - O(n) where n = number of selections
    /// - Positions stored as anchors (survive buffer edits)
    /// - Selections automatically merged if they overlap after move
    pub fn move_right(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer();
        let snapshot = buffer.read(cx).snapshot();

        // Sync selections from cursor if they're out of sync (for backward compatibility)
        // This handles cases where other actions only updated cursor
        // Only sync if we have exactly one selection (default state)
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

        // Get all current selections resolved to Points
        let mut selections = self.selections.all::<Point>(&snapshot);

        // Move each selection's head right by one character
        for selection in &mut selections {
            let head = selection.head();
            let line_len = snapshot.line_len(head.row);

            if head.column < line_len {
                let target = Point::new(head.row, head.column + 1);
                let clipped = snapshot.clip_point(target, Bias::Right);

                // Collapse selection to new cursor position
                selection.start = clipped;
                selection.end = clipped;
                selection.reversed = false;
                selection.goal = text::SelectionGoal::None;
            }
        }

        // Store updated selections (converts to anchors)
        self.selections.select(selections.clone(), &snapshot);

        // Update legacy cursor field for backward compatibility
        // Use the newest (last) selection as the primary cursor
        if let Some(last_sel) = selections.last() {
            self.cursor.move_to(last_sel.head());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_right_one_character(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_right(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 1));
        });
    }

    #[gpui::test]
    fn no_op_at_end_of_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hi", cx);

            // Sync selections with current cursor position before using move_right
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let cursor_pos = s.cursor.position();
            let id = s.selections.next_id();
            s.selections.select(
                vec![text::Selection {
                    id,
                    start: cursor_pos,
                    end: cursor_pos,
                    reversed: false,
                    goal: text::SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.move_right(cx);

            // Verify using new multi-cursor API - should be no-op at end of line
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 2));
        });
    }

    #[gpui::test]
    fn moves_multiple_cursors_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Hello\nWorld", cx);

            // Create two cursors at start of each line
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 0),
                        end: text::Point::new(0, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 0),
                        end: text::Point::new(1, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Move both cursors right
            s.move_right(cx);

            // Verify both moved independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 1));
            assert_eq!(selections[1].head(), text::Point::new(1, 1));
        });
    }
}
