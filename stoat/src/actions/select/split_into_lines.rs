//! SplitSelectionIntoLines action implementation.
//!
//! Converts multi-line selections into one cursor per line, enabling efficient
//! column editing workflows. Based on Zed's implementation at `editor.rs:14125-14183`.

use crate::stoat::Stoat;
use gpui::Context;
use text::{Point, Selection, SelectionGoal};

impl Stoat {
    /// Split multi-line selections into one cursor per line.
    ///
    /// For each multi-line selection, creates a separate cursor at the end of
    /// each line within the selection range. Single-line selections remain unchanged.
    ///
    /// # Algorithm
    ///
    /// 1. Get all current selections as Point ranges
    /// 2. For each selection:
    ///    - If single-line: keep unchanged
    ///    - If multi-line:
    ///      - Create cursor at end of each intermediate line (start.row to end.row-1)
    ///      - Add cursor at selection end (unless it's at column 0)
    /// 3. Update selections with new cursors
    ///
    /// # Edge Cases
    ///
    /// - Multi-line selection ending at column 0: Skips the last line for ergonomics
    /// - Empty lines: Cursor placed at column 0
    /// - Single-line selections: Preserved as-is
    ///
    /// # Related
    ///
    /// - Based on Zed's `split_selection_into_lines()` at `editor.rs:14125-14183`
    /// - Used for column editing workflows
    /// - Complements [`AddSelectionAbove`] and [`AddSelectionBelow`] (when implemented)
    ///
    /// [`AddSelectionAbove`]: crate::actions::AddSelectionAbove
    /// [`AddSelectionBelow`]: crate::actions::AddSelectionBelow
    pub fn split_selection_into_lines(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().read(cx);
        let snapshot = buffer.snapshot();

        // Get all current selections as Points
        let current_selections = self.active_selections(cx);
        let mut new_selections = Vec::new();

        for selection in current_selections {
            let start = selection.start;
            let end = selection.end;

            if start.row == end.row {
                // Single-line selection - keep as is
                new_selections.push(selection);
            } else {
                // Multi-line selection - split into per-line cursors

                // Add cursor at end of each intermediate line
                for row in start.row..end.row {
                    let line_len = snapshot.line_len(row);
                    let point = Point::new(row, line_len);

                    new_selections.push(Selection {
                        id: self.selections.next_id(),
                        start: point,
                        end: point,
                        reversed: false,
                        goal: SelectionGoal::None,
                    });
                }

                // Handle last line
                // Skip if selection ends at column 0 (ergonomic: selecting lines with V in vim)
                if end.column > 0 {
                    new_selections.push(Selection {
                        id: self.selections.next_id(),
                        start: end,
                        end,
                        reversed: false,
                        goal: SelectionGoal::None,
                    });
                }
            }
        }

        // Update selections
        self.selections.select(new_selections, &snapshot);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;
    use gpui::TestAppContext;
    use text::{Point, Selection, SelectionGoal};

    #[gpui::test]
    fn splits_multi_line_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("line1\nline2\nline3\n", cx);
        stoat.update(|s, cx| {
            // Create selection spanning all three lines
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(2, 5),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.split_selection_into_lines(cx);

            // Should have 3 cursors at end of each line
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 3);
            assert_eq!(sels[0].start, Point::new(0, 5)); // End of "line1"
            assert_eq!(sels[0].end, Point::new(0, 5));
            assert_eq!(sels[1].start, Point::new(1, 5)); // End of "line2"
            assert_eq!(sels[1].end, Point::new(1, 5));
            assert_eq!(sels[2].start, Point::new(2, 5)); // End of "line3"
            assert_eq!(sels[2].end, Point::new(2, 5));
        });
    }

    #[gpui::test]
    fn skips_last_line_when_ending_at_column_zero(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("line1\nline2\nline3\n", cx);
        stoat.update(|s, cx| {
            // Select lines 1-2, ending at start of line 3
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(2, 0), // Column 0 of line 3
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.split_selection_into_lines(cx);

            // Should have only 2 cursors (not 3)
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 2);
            assert_eq!(sels[0].start, Point::new(0, 5));
            assert_eq!(sels[1].start, Point::new(1, 5));
        });
    }

    #[gpui::test]
    fn preserves_single_line_selections(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("single line\n", cx);
        stoat.update(|s, cx| {
            // Create single-line selection
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(0, 6),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.split_selection_into_lines(cx);

            // Should remain unchanged
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 1);
            assert_eq!(sels[0].start, Point::new(0, 0));
            assert_eq!(sels[0].end, Point::new(0, 6));
        });
    }

    #[gpui::test]
    fn handles_empty_lines(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("line1\n\nline3\n", cx);
        stoat.update(|s, cx| {
            // Select across empty line
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(2, 5),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.split_selection_into_lines(cx);

            // Should have 3 cursors, middle one at column 0
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 3);
            assert_eq!(sels[0].start, Point::new(0, 5));
            assert_eq!(sels[1].start, Point::new(1, 0)); // Empty line
            assert_eq!(sels[2].start, Point::new(2, 5));
        });
    }

    #[gpui::test]
    fn handles_multiple_selections(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("a\nb\nc\nd\ne\nf\n", cx);
        stoat.update(|s, cx| {
            // Create two multi-line selections
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![
                    Selection {
                        id,
                        start: Point::new(0, 0),
                        end: Point::new(1, 1), // Lines 0-1
                        reversed: false,
                        goal: SelectionGoal::None,
                    },
                    Selection {
                        id: id + 1,
                        start: Point::new(3, 0),
                        end: Point::new(4, 1), // Lines 3-4
                        reversed: false,
                        goal: SelectionGoal::None,
                    },
                ],
                &buffer_snapshot,
            );

            s.split_selection_into_lines(cx);

            // Should have 4 cursors total (2 per original selection)
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 4);

            // First original selection split
            assert_eq!(sels[0].start, Point::new(0, 1)); // End of "a"
            assert_eq!(sels[1].start, Point::new(1, 1)); // At "b" column 1

            // Second original selection split
            assert_eq!(sels[2].start, Point::new(3, 1)); // End of "d"
            assert_eq!(sels[3].start, Point::new(4, 1)); // At "e" column 1
        });
    }

    #[gpui::test]
    fn selection_at_eof(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("line1\nline2", cx); // No trailing newline
        stoat.update(|s, cx| {
            // Select to EOF
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(1, 5),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.split_selection_into_lines(cx);

            // Should have 2 cursors
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 2);
            assert_eq!(sels[0].start, Point::new(0, 5));
            assert_eq!(sels[1].start, Point::new(1, 5)); // EOF position
        });
    }

    #[gpui::test]
    fn two_line_selection_ending_mid_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("first\nsecond\n", cx);
        stoat.update(|s, cx| {
            // Select from start of line 1 to middle of line 2
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 0),
                    end: Point::new(1, 3),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.split_selection_into_lines(cx);

            // Should have 2 cursors
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 2);
            assert_eq!(sels[0].start, Point::new(0, 5)); // End of "first"
            assert_eq!(sels[1].start, Point::new(1, 3)); // "sec|ond"
        });
    }
}
