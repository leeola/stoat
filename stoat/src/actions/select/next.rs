//! SelectNext action implementation.
//!
//! Adds a selection at the next occurrence of the currently selected text,
//! enabling multi-cursor editing of repeated patterns. Based on Zed's
//! implementation at `editor.rs:14590-14610` and `editor.rs:14383-14530`.

use crate::{editor::state::SelectNextState, stoat::Stoat};
use gpui::Context;
use std::ops::Range;
use text::{Point, Selection, SelectionGoal, ToOffset};

impl Stoat {
    /// Add a selection at the next occurrence of the current selection text.
    ///
    /// This action finds the next occurrence of the currently selected text
    /// and adds a new selection at that location, enabling multi-cursor workflows
    /// for editing repeated patterns.
    ///
    /// # Algorithm
    ///
    /// 1. Get newest selection and extract its text as the search query
    /// 2. Find next occurrence of the query after the newest selection
    /// 3. If found and doesn't overlap existing selections, add new selection
    /// 4. Track state to enable repeated invocations
    /// 5. Wrap around to buffer start if no matches after current position
    ///
    /// # State Management
    ///
    /// Uses [`SelectNextState`] to track:
    /// - `query`: The search text from the original selection
    /// - `wordwise`: Whether to match whole words only
    /// - `done`: Whether search has wrapped or exhausted matches
    ///
    /// State persists across invocations until selection text changes or
    /// selections are modified externally.
    ///
    /// # Edge Cases
    ///
    /// - Empty selection: No-op (nothing to search for)
    /// - No more matches: Wraps to beginning and searches from start
    /// - Overlapping matches: Skips matches that overlap existing selections
    /// - All matches selected: `done` flag set, no more additions
    ///
    /// # Related
    ///
    /// - Based on Zed's `select_next()` at `editor.rs:14590-14610`
    /// - Complements [`select_all_matches`](Self::select_all_matches)
    /// - Uses [`find_next_occurrence`] for search logic
    pub fn select_next(&mut self, cx: &mut Context<Self>) {
        self.record_selection_change();
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().read(cx);
        let snapshot = buffer.snapshot();

        // Get newest selection and extract query text
        let newest: Selection<Point> = self.selections.newest(&snapshot);
        if newest.start == newest.end {
            // Empty selection, nothing to search for
            return;
        }

        let query = snapshot
            .text_for_range(newest.start..newest.end)
            .collect::<String>();

        // Get or reset state based on query match
        let state_matches = self
            .select_next_state
            .as_ref()
            .is_some_and(|state| state.query == query);

        if !state_matches {
            self.select_next_state = Some(SelectNextState {
                query: query.clone(),
                wordwise: false,
                done: false,
            });
        }

        let state = self.select_next_state.as_ref().unwrap();
        if state.done {
            // Already exhausted all matches
            return;
        }

        // Find next occurrence after newest selection
        let start_offset = newest.end.to_offset(&snapshot);
        let next_match = find_next_occurrence(&snapshot, &query, start_offset, true);

        if let Some(range) = next_match {
            // Check if this match overlaps with any existing selection
            let all_selections = self.active_selections(cx);
            let overlaps = all_selections.iter().any(|sel| {
                let sel_start = sel.start.to_offset(&snapshot);
                let sel_end = sel.end.to_offset(&snapshot);
                !(range.end <= sel_start || range.start >= sel_end)
            });

            if !overlaps {
                // Add new selection at match
                let start_point = snapshot.offset_to_point(range.start);
                let end_point = snapshot.offset_to_point(range.end);

                let new_selection = Selection {
                    id: self.selections.next_id(),
                    start: start_point,
                    end: end_point,
                    reversed: false,
                    goal: SelectionGoal::None,
                };

                let mut all_selections: Vec<Selection<Point>> = self.active_selections(cx);
                all_selections.push(new_selection);
                self.selections.select(all_selections, &snapshot);
                let newest = self.selections.newest::<Point>(&snapshot);
                self.cursor.move_to(newest.head());

                cx.notify();
            } else {
                // Found match but it overlaps - mark as done
                if let Some(state) = &mut self.select_next_state {
                    state.done = true;
                }
            }
        } else {
            // No more matches found, mark as done
            if let Some(state) = &mut self.select_next_state {
                state.done = true;
            }
        }
    }
}

/// Find the next occurrence of a string in the buffer after a given offset.
///
/// Searches the buffer for the next occurrence of `query` starting after
/// `after_offset`. If `wrap` is true and no match is found, wraps around
/// to search from the beginning.
///
/// # Arguments
///
/// * `snapshot` - Buffer snapshot to search
/// * `query` - Text to search for (case-sensitive)
/// * `after_offset` - Start searching after this byte offset
/// * `wrap` - Whether to wrap around to buffer start if no match found
///
/// # Returns
///
/// Byte range of the match, or None if not found
fn find_next_occurrence(
    snapshot: &text::BufferSnapshot,
    query: &str,
    after_offset: usize,
    wrap: bool,
) -> Option<Range<usize>> {
    let text = snapshot.text();
    let full_text = text.to_string();

    // Search from after_offset to end
    if let Some(pos) = full_text[after_offset..].find(query) {
        let match_start = after_offset + pos;
        let match_end = match_start + query.len();
        return Some(match_start..match_end);
    }

    // If wrap enabled, search from beginning to after_offset
    if wrap {
        if let Some(pos) = full_text[..after_offset].find(query) {
            let match_end = pos + query.len();
            return Some(pos..match_end);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use crate::Stoat;
    use gpui::TestAppContext;
    use text::{Point, Selection, SelectionGoal};

    #[gpui::test]
    fn finds_next_occurrence(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("foo bar foo baz foo", cx);
        stoat.update(|s, cx| {
            // Select first "foo"
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(0, 3),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.select_next(cx);

            // Should have 2 selections now
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 2);
            assert_eq!(sels[0].start, Point::new(0, 0));
            assert_eq!(sels[0].end, Point::new(0, 3));
            assert_eq!(sels[1].start, Point::new(0, 8));
            assert_eq!(sels[1].end, Point::new(0, 11));
        });
    }

    #[gpui::test]
    fn wraps_around_buffer(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("foo bar foo", cx);
        stoat.update(|s, cx| {
            // Select last "foo"
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 8),
                    end: Point::new(0, 11),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.select_next(cx);

            // Should wrap to first "foo"
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 2);
            assert_eq!(sels[0].start, Point::new(0, 0));
            assert_eq!(sels[0].end, Point::new(0, 3));
            assert_eq!(sels[1].start, Point::new(0, 8));
            assert_eq!(sels[1].end, Point::new(0, 11));
        });
    }

    #[gpui::test]
    fn no_op_with_empty_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("foo bar foo", cx);
        stoat.update(|s, cx| {
            // Cursor at position 0 (empty selection)
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(0, 0),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.select_next(cx);

            // Should still have just 1 selection
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 1);
        });
    }

    #[gpui::test]
    fn stops_when_all_occurrences_selected(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("x x x", cx);
        stoat.update(|s, cx| {
            // Select first "x"
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(0, 1),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            // Select next twice
            s.select_next(cx);
            s.select_next(cx);

            // Should have all 3 "x"s selected
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 3);

            // Try selecting next again - should be no-op
            s.select_next(cx);
            assert_eq!(s.active_selections(cx).len(), 3);
        });
    }

    #[gpui::test]
    fn handles_multiline_text(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("foo\nbar\nfoo\nbaz\nfoo", cx);
        stoat.update(|s, cx| {
            // Select first "foo" on line 0
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(0, 3),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.select_next(cx);

            // Should find "foo" on line 2
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 2);
            assert_eq!(sels[1].start, Point::new(2, 0));
            assert_eq!(sels[1].end, Point::new(2, 3));
        });
    }
}
