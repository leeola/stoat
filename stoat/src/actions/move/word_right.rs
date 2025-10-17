//! Move word right action implementation and tests.
//!
//! Demonstrates multi-cursor word-based movement using token analysis.

use crate::Stoat;
use gpui::Context;
use text::{Point, ToOffset};

impl Stoat {
    /// Move all cursors right by one word (symbol).
    ///
    /// Each cursor moves independently to the end of the next word/symbol.
    /// Uses token analysis to identify word boundaries.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn move_word_right(&mut self, cx: &mut Context<Self>) {
        let (buffer_snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let buffer_snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (buffer_snapshot, token_snapshot)
        };

        // Auto-sync from cursor if single selection (backward compat)
        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<Point>(&buffer_snapshot);
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
                    &buffer_snapshot,
                );
            }
        }

        // Operate on all selections
        let mut selections = self.selections.all::<Point>(&buffer_snapshot);
        for selection in &mut selections {
            let head = selection.head();
            let cursor_offset = buffer_snapshot.point_to_offset(head);

            let mut token_cursor = token_snapshot.cursor(&buffer_snapshot);
            token_cursor.next();

            let mut found_symbol_end: Option<usize> = None;

            while let Some(token) = token_cursor.item() {
                let token_end = token.range.end.to_offset(&buffer_snapshot);

                if token_end <= cursor_offset {
                    token_cursor.next();
                    continue;
                }

                if token.kind.is_symbol() {
                    found_symbol_end = Some(token_end);
                    break;
                }

                token_cursor.next();
            }

            if let Some(offset) = found_symbol_end {
                let new_pos = buffer_snapshot.offset_to_point(offset);

                // Collapse selection to new cursor position
                selection.start = new_pos;
                selection.end = new_pos;
                selection.reversed = false;
                selection.goal = text::SelectionGoal::None;
            }
        }

        // Store back and sync cursor
        self.selections.select(selections.clone(), &buffer_snapshot);
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
    fn moves_to_next_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.move_word_right(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 5));
        });
    }

    #[gpui::test]
    fn moves_multiple_cursors_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world\nfoo bar", cx);

            // Create two cursors
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 0), // Start of "hello world"
                        end: text::Point::new(0, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 0), // Start of "foo bar"
                        end: text::Point::new(1, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Move both cursors right by word
            s.move_word_right(cx);

            // Verify both moved independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 5)); // End of "hello"
            assert_eq!(selections[1].head(), text::Point::new(1, 3)); // End of "foo"
        });
    }
}
