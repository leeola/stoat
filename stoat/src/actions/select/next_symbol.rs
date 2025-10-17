//! Select next symbol action implementation and tests.
//!
//! Demonstrates multi-cursor selection extension to next symbol.

use crate::Stoat;
use gpui::Context;
use std::ops::Range;
use text::{Point, ToOffset};

impl Stoat {
    /// Extend all selections to the next symbol.
    ///
    /// Each selection extends independently by finding the next symbol from its head position
    /// and extending to it while keeping the tail (anchor) fixed. Uses vim `w` behavior:
    /// skips symbols that start at or before the cursor.
    ///
    /// Updates both the new selections field and legacy cursor field for backward compatibility.
    pub fn select_next_symbol(&mut self, cx: &mut Context<Self>) {
        let (snapshot, token_snapshot) = {
            let buffer_item = self.active_buffer(cx).read(cx);
            let snapshot = buffer_item.buffer().read(cx).snapshot();
            let token_snapshot = buffer_item.token_snapshot();
            (snapshot, token_snapshot)
        };

        // Auto-sync from cursor if single selection (backward compat)
        // In non-anchored mode, replace existing non-empty selections with empty selection at
        // cursor so each 'w' creates a new selection instead of extending the old one
        let cursor_pos = self.cursor.position();
        if self.selections.count() == 1 {
            let newest_sel = self.selections.newest::<Point>(&snapshot);
            let should_reset = if self.is_mode_anchored() {
                // In anchored selection mode, only reset if head doesn't match cursor
                newest_sel.head() != cursor_pos
            } else {
                // In non-anchored mode, reset if:
                // 1. There's a non-empty selection (for ww behavior), OR
                // 2. Head doesn't match cursor (for cursor/selection sync)
                !newest_sel.is_empty() || newest_sel.head() != cursor_pos
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

            let mut found_symbol: Option<Range<usize>> = None;

            while let Some(token) = token_cursor.item() {
                let token_start = token.range.start.to_offset(&snapshot);
                let token_end = token.range.end.to_offset(&snapshot);

                // Skip tokens that start before cursor
                if token_start < cursor_offset {
                    token_cursor.next();
                    continue;
                }

                // Found first symbol after cursor
                if token.kind.is_symbol() {
                    found_symbol = Some(token_start..token_end);
                    break;
                }

                token_cursor.next();
            }

            if let Some(range) = found_symbol {
                if self.is_mode_anchored() {
                    // In anchored selection mode: extend from current tail to symbol end
                    let selection_end = snapshot.offset_to_point(range.end);
                    selection.set_head(selection_end, text::SelectionGoal::None);
                } else {
                    // In non-anchored mode: select just the symbol itself
                    let selection_start = snapshot.offset_to_point(range.start);
                    let selection_end = snapshot.offset_to_point(range.end);
                    selection.start = selection_start;
                    selection.end = selection_end;
                    selection.reversed = false;
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
    fn selects_next_word(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("hello world", cx);
            s.set_cursor_position(text::Point::new(0, 0));
            s.select_next_symbol(cx);

            // Verify using new multi-cursor API
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            // In normal mode: select the symbol at/after cursor ("hello")
            assert_eq!(selections[0].head(), text::Point::new(0, 5)); // end of "hello"
            assert_eq!(selections[0].tail(), text::Point::new(0, 0)); // start of "hello"
        });
    }

    #[gpui::test]
    fn extends_multiple_selections_independently(cx: &mut TestAppContext) {
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
                        start: text::Point::new(0, 0), // Start of "hello"
                        end: text::Point::new(0, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                    text::Selection {
                        id: id + 1,
                        start: text::Point::new(1, 0), // Start of "foo"
                        end: text::Point::new(1, 0),
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            // Extend both selections to next symbol
            s.select_next_symbol(cx);

            // Verify both extended independently
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 2);
            // In normal mode: each selects the symbol at/after cursor
            assert_eq!(selections[0].head(), text::Point::new(0, 5)); // end of "hello"
            assert_eq!(selections[0].tail(), text::Point::new(0, 0)); // start of "hello"
            assert_eq!(selections[1].head(), text::Point::new(1, 3)); // end of "foo"
            assert_eq!(selections[1].tail(), text::Point::new(1, 0)); // start of "foo"
        });
    }
}
