//! Select previous token action implementation and tests.
//!
//! Demonstrates multi-cursor selection extension to previous token.

use crate::Stoat;
use gpui::Context;
use std::ops::Range;
use text::{Point, ToOffset};

impl Stoat {
    /// Extend all selections to the previous token.
    ///
    /// Each selection extends independently by finding the previous token from its head position
    /// and extending to it while keeping the tail (anchor) fixed.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn select_prev_token(&mut self, cx: &mut Context<Self>) {
        let (snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (snapshot, token_snapshot)
        };

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
            // Handle non-reversed selection: flip it to reversed
            if !selection.is_empty() && !selection.reversed {
                let start = selection.start;
                let end = selection.end;
                selection.start = end;
                selection.end = start;
                selection.reversed = true;
                continue;
            }

            let head = selection.head();
            let cursor_offset = snapshot.point_to_offset(head);

            let mut token_cursor = token_snapshot.cursor(&snapshot);
            token_cursor.next();

            let mut prev_token: Option<(usize, usize)> = None;

            while let Some(token) = token_cursor.item() {
                let token_start = token.range.start.to_offset(&snapshot);
                let token_end = token.range.end.to_offset(&snapshot);

                if token_start >= cursor_offset {
                    break;
                }

                if token.kind.is_token() {
                    if token_start < cursor_offset && cursor_offset < token_end {
                        prev_token = Some((token_start, cursor_offset));
                        break;
                    }

                    if token_end <= cursor_offset {
                        prev_token = Some((token_start, token_end));
                    }
                }

                token_cursor.next();
            }

            let found_token: Option<Range<usize>> = prev_token.map(|(start, end)| start..end);

            if let Some(range) = found_token {
                let selection_start = snapshot.offset_to_point(range.start);
                // Extend selection by moving head to start of token
                selection.set_head(selection_start, text::SelectionGoal::None);
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
    fn selects_previous_token(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("foo.bar", cx);
            s.set_cursor_position(text::Point::new(0, 4));
            s.select_prev_token(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 3)); // Selects the "."
            assert_eq!(selections[0].tail(), text::Point::new(0, 4));
        });
    }

    #[gpui::test]
    fn extends_multiple_selections_independently(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("foo.bar\nbaz.qux", cx);

            // Create two cursors
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    text::Selection {
                        id,
                        start: text::Point::new(0, 4), // After "."
                        end: text::Point::new(0, 4),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 4), // After "."
                        end: text::Point::new(1, 4),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Extend both selections to previous token
            s.select_prev_token(cx);

            // Verify both extended independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 3)); // Selects "."
            assert_eq!(selections[0].tail(), text::Point::new(0, 4));
            assert_eq!(selections[1].head(), text::Point::new(1, 3)); // Selects "."
            assert_eq!(selections[1].tail(), text::Point::new(1, 4));
        });
    }
}
