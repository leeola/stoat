//! Select next token action implementation and tests.
//!
//! Demonstrates multi-cursor selection extension to next token.

use crate::Stoat;
use gpui::Context;
use std::ops::Range;
use text::{Point, ToOffset};

impl Stoat {
    /// Extend all selections to the next token.
    ///
    /// Each selection extends independently by finding the next token from its head position
    /// and extending to it while keeping the tail (anchor) fixed.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn select_next_token(&mut self, cx: &mut Context<Self>) {
        let (snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (snapshot, token_snapshot)
        };

        // Auto-sync from cursor if single selection (backward compat)
        // In normal mode, replace existing non-empty selections with empty selection at cursor
        // so each 'W' creates a new selection instead of extending the old one
        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<Point>(&snapshot);
            let should_reset = if self.mode == "normal" || self.mode == "insert" {
                // In normal/insert mode, reset if:
                // 1. There's a non-empty selection (for WW behavior), OR
                // 2. Head doesn't match cursor (for cursor/selection sync)
                !newest_sel.is_empty() || newest_sel.head() != cursor_pos
            } else {
                // In visual mode, only reset if head doesn't match cursor
                newest_sel.head() != cursor_pos
            };

            if should_reset {
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
            // Handle reversed selection: flip it to non-reversed
            if !selection.is_empty() && selection.reversed {
                let start = selection.start;
                let end = selection.end;
                selection.start = start;
                selection.end = end;
                selection.reversed = false;
                continue;
            }

            let head = selection.head();
            let cursor_offset = snapshot.point_to_offset(head);

            let mut token_cursor = token_snapshot.cursor(&snapshot);
            token_cursor.next();

            let mut found_token: Option<Range<usize>> = None;

            while let Some(token) = token_cursor.item() {
                let token_start = token.range.start.to_offset(&snapshot);
                let token_end = token.range.end.to_offset(&snapshot);

                // Skip tokens that start before cursor
                if token_start < cursor_offset {
                    token_cursor.next();
                    continue;
                }

                if token.kind.is_token() {
                    found_token = Some(token_start..token_end);
                    break;
                }

                token_cursor.next();
            }

            if let Some(range) = found_token {
                if self.mode == "normal" || self.mode == "insert" {
                    // In normal/insert mode: select just the token itself
                    let selection_start = snapshot.offset_to_point(range.start);
                    let selection_end = snapshot.offset_to_point(range.end);
                    selection.start = selection_start;
                    selection.end = selection_end;
                    selection.reversed = false;
                } else {
                    // In visual mode: extend from current tail to token end
                    let selection_end = snapshot.offset_to_point(range.end);
                    selection.set_head(selection_end, text::SelectionGoal::None);
                }
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
    fn selects_next_token(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("foo.bar", cx);
            s.set_cursor_position(text::Point::new(0, 3));
            s.select_next_token(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].head(), text::Point::new(0, 4)); // Selects the "."
            assert_eq!(selections[0].tail(), text::Point::new(0, 3));
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
                        start: text::Point::new(0, 3), // After "foo"
                        end: text::Point::new(0, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 3), // After "baz"
                        end: text::Point::new(1, 3),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Extend both selections to next token
            s.select_next_token(cx);

            // Verify both extended independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            assert_eq!(selections[0].head(), text::Point::new(0, 4)); // Selects "."
            assert_eq!(selections[0].tail(), text::Point::new(0, 3));
            assert_eq!(selections[1].head(), text::Point::new(1, 4)); // Selects "."
            assert_eq!(selections[1].tail(), text::Point::new(1, 3));
        });
    }
}
