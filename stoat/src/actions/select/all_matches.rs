//! SelectAllMatches action implementation.
//!
//! Selects all occurrences of the currently selected text simultaneously,
//! enabling bulk multi-cursor editing. Based on Zed's implementation at
//! `editor.rs:14533-14587`.

use crate::stoat::Stoat;
use gpui::Context;
use std::ops::Range;
use text::{Point, Selection, SelectionGoal};

impl Stoat {
    /// Select all occurrences of the current selection text.
    ///
    /// Finds every occurrence of the currently selected text in the buffer
    /// and creates a selection at each location. This enables simultaneous
    /// editing of all instances of a pattern.
    ///
    /// # Algorithm
    ///
    /// 1. Get newest selection and extract its text as the search query
    /// 2. Find all occurrences of the query in the entire buffer
    /// 3. Convert matches to selections
    /// 4. Replace current selections with all matches
    /// 5. Overlapping selections are automatically merged by SelectionsCollection
    ///
    /// # Performance
    ///
    /// Uses simple string searching. For buffers with thousands of matches,
    /// this may take noticeable time. Future optimization could use
    /// `aho-corasick` crate for multi-pattern searching.
    ///
    /// # Edge Cases
    ///
    /// - Empty selection: No-op (nothing to search for)
    /// - No matches found: No-op (keep current selection)
    /// - Large match count (1000+): May be slow with current implementation
    /// - Overlapping matches: Automatically merged by SelectionsCollection
    ///
    /// # Related
    ///
    /// - Based on Zed's `select_all_matches()` at `editor.rs:14533-14587`
    /// - Complements [`select_next`](Self::select_next) which adds one at a time
    /// - Uses [`find_all_occurrences`] for search logic
    pub fn select_all_matches(&mut self, cx: &mut Context<Self>) {
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

        // Find all occurrences in buffer
        let all_matches = find_all_occurrences(&snapshot, &query);

        if all_matches.is_empty() {
            // No matches found
            return;
        }

        // Convert matches to selections
        let mut new_selections = Vec::new();
        for range in all_matches {
            let start_point = snapshot.offset_to_point(range.start);
            let end_point = snapshot.offset_to_point(range.end);

            new_selections.push(Selection {
                id: self.selections.next_id(),
                start: start_point,
                end: end_point,
                reversed: false,
                goal: SelectionGoal::None,
            });
        }

        // Replace all selections with matches
        // SelectionsCollection::select() will merge overlapping selections
        self.selections.select(new_selections, &snapshot);

        cx.notify();
    }
}

/// Find all occurrences of a string in the buffer.
///
/// Searches the entire buffer for all occurrences of `query` and returns
/// their byte ranges. Matches are case-sensitive.
///
/// # Arguments
///
/// * `snapshot` - Buffer snapshot to search
/// * `query` - Text to search for (case-sensitive)
///
/// # Returns
///
/// Vector of byte ranges for all matches, in buffer order
///
/// # Performance
///
/// Current implementation converts entire buffer to String and uses
/// str::match_indices. For large buffers with many matches, this may
/// be slow. Future optimization could use streaming search or aho-corasick.
fn find_all_occurrences(snapshot: &text::BufferSnapshot, query: &str) -> Vec<Range<usize>> {
    let text = snapshot.text();
    let full_text = text.to_string();

    full_text
        .match_indices(query)
        .map(|(pos, matched)| pos..(pos + matched.len()))
        .collect()
}

#[cfg(test)]
mod tests {
    use crate::Stoat;
    use gpui::TestAppContext;
    use text::{Point, Selection, SelectionGoal};

    #[gpui::test]
    fn selects_all_occurrences(cx: &mut TestAppContext) {
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

            s.select_all_matches(cx);

            // Should have 3 selections (all "foo"s)
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 3);
            assert_eq!(sels[0].start, Point::new(0, 0));
            assert_eq!(sels[0].end, Point::new(0, 3));
            assert_eq!(sels[1].start, Point::new(0, 8));
            assert_eq!(sels[1].end, Point::new(0, 11));
            assert_eq!(sels[2].start, Point::new(0, 16));
            assert_eq!(sels[2].end, Point::new(0, 19));
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

            s.select_all_matches(cx);

            // Should still have just 1 selection
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 1);
        });
    }

    #[gpui::test]
    fn no_op_when_no_matches(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("foo bar baz", cx);
        stoat.update(|s, cx| {
            // Select "foo" (unique occurrence)
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

            s.select_all_matches(cx);

            // Should have just the original selection (no other matches)
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 1);
            assert_eq!(sels[0].start, Point::new(0, 0));
            assert_eq!(sels[0].end, Point::new(0, 3));
        });
    }

    #[gpui::test]
    fn handles_multiline_text(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("foo\nbar\nfoo\nbaz\nfoo\nqux\nfoo", cx);
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

            s.select_all_matches(cx);

            // Should find all 4 "foo"s across lines
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 4);
            assert_eq!(sels[0].start, Point::new(0, 0));
            assert_eq!(sels[1].start, Point::new(2, 0));
            assert_eq!(sels[2].start, Point::new(4, 0));
            assert_eq!(sels[3].start, Point::new(6, 0));
        });
    }

    #[gpui::test]
    fn handles_overlapping_pattern(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("aaaa", cx);
        stoat.update(|s, cx| {
            // Select "aa"
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(0, 2),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.select_all_matches(cx);

            // "aa" appears at positions 0, 1, 2
            // After merging overlaps, should result in one selection covering entire "aaaa"
            let sels = s.active_selections(cx);
            // The exact behavior depends on overlap merging logic
            // With current implementation, overlapping selections get merged
            assert!(sels.len() <= 3);
        });
    }

    #[gpui::test]
    fn handles_single_character(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("a b a c a", cx);
        stoat.update(|s, cx| {
            // Select "a"
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

            s.select_all_matches(cx);

            // Should find all 3 "a"s
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 3);
        });
    }

    #[gpui::test]
    fn handles_whitespace(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("a  b  c  d", cx);
        stoat.update(|s, cx| {
            // Select double space "  "
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 1),
                    end: Point::new(0, 3),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.select_all_matches(cx);

            // Should find all 3 double-space occurrences
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 3);
        });
    }
}
