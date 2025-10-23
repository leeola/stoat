//! AddSelectionBelow action implementation.
//!
//! Adds a cursor on the line below at the same column position, enabling
//! columnar multi-cursor editing. Simplified version that works with Point
//! coordinates instead of Zed's DisplayMap approach.

use crate::stoat::Stoat;
use gpui::Context;
use text::{Point, Selection, SelectionGoal};

impl Stoat {
    /// Add a cursor on the line below at the same column position.
    ///
    /// Creates a new cursor one line below the newest selection, preserving
    /// the column position. Enables columnar editing by repeatedly invoking
    /// this action to build up vertical cursor stacks.
    ///
    /// # Algorithm
    ///
    /// 1. Get the newest selection (most recently added)
    /// 2. If already at last line, no-op
    /// 3. Create cursor at same column on next line
    /// 4. Clamp column to line length if necessary
    /// 5. Add new selection to collection
    ///
    /// # Simplified Architecture
    ///
    /// Unlike Zed's DisplayMap-based implementation, this uses simple Point
    /// coordinates and works for unwrapped text. Does not handle:
    /// - Display wrapping (soft wraps)
    /// - Folded regions
    /// - Inlay hints
    ///
    /// # Edge Cases
    ///
    /// - Last line: No-op (can't go below)
    /// - Column beyond line length: Clamped to line end
    /// - Empty buffer: No-op
    ///
    /// # Related
    ///
    /// - Complements [`add_selection_above`](Self::add_selection_above)
    /// - Based on Zed's approach at `editor.rs:14203-14263`
    pub fn add_selection_below(&mut self, cx: &mut Context<Self>) {
        let buffer_item = self.active_buffer(cx);
        let buffer = buffer_item.read(cx).buffer().read(cx).snapshot();
        let snapshot = buffer;

        // Get newest selection as Point
        let newest: Selection<Point> = self.selections.newest(&snapshot);

        // Check if on last line
        let max_row = snapshot.max_point().row;
        if newest.end.row >= max_row {
            return;
        }

        // Calculate position one line below
        let target_row = newest.end.row + 1;
        let target_column = newest.end.column;

        // Clamp to line length
        let line_len = snapshot.line_len(target_row);
        let clamped_column = target_column.min(line_len);

        let new_point = Point::new(target_row, clamped_column);

        // Create new cursor
        let new_selection = Selection {
            id: self.selections.next_id(),
            start: new_point,
            end: new_point,
            reversed: false,
            goal: SelectionGoal::None,
        };

        // Add to existing selections
        let mut all_selections: Vec<Selection<Point>> = self.active_selections(cx);
        all_selections.push(new_selection);
        self.selections.select(all_selections, &snapshot);

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;
    use gpui::TestAppContext;
    use text::{Point, Selection, SelectionGoal};

    #[gpui::test]
    fn adds_cursor_below(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("line1\nline2\nline3\n", cx);
        stoat.update(|s, cx| {
            // Start at line 1
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(1, 2),
                    end: Point::new(1, 2),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.add_selection_below(cx);

            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 2);
            assert_eq!(sels[0].start, Point::new(1, 2)); // Original
            assert_eq!(sels[1].start, Point::new(2, 2)); // Added below
        });
    }

    #[gpui::test]
    fn no_op_at_last_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("line1\nline2", cx);
        stoat.update(|s, cx| {
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let max_row = buffer_snapshot.max_point().row;

            // Start at last line
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(max_row, 2),
                    end: Point::new(max_row, 2),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.add_selection_below(cx);

            // Should still have 1 selection
            assert_eq!(s.active_selections(cx).len(), 1);
        });
    }

    #[gpui::test]
    fn clamps_to_shorter_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("longer line\nshort\n", cx);
        stoat.update(|s, cx| {
            // Start at column 10 on line 0
            let buffer_snapshot = s.active_buffer(cx).read(cx).buffer().read(cx).snapshot();
            let id = s.selections.next_id();
            s.selections.select(
                vec![Selection {
                    id,
                    start: Point::new(0, 10),
                    end: Point::new(0, 10),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &buffer_snapshot,
            );

            s.add_selection_below(cx);

            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 2);
            // "short" is only 5 chars, so should clamp to column 5
            assert_eq!(sels[1].start.column, 5);
        });
    }

    #[gpui::test]
    fn builds_columnar_stack(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test_with_text("a\nb\nc\nd\ne\n", cx);
        stoat.update(|s, cx| {
            // Start at line 0
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

            // Add 4 cursors below
            s.add_selection_below(cx);
            s.add_selection_below(cx);
            s.add_selection_below(cx);
            s.add_selection_below(cx);

            // Should have 5 total cursors (one on each line)
            let sels = s.active_selections(cx);
            assert_eq!(sels.len(), 5);
            assert_eq!(sels[0].start.row, 0);
            assert_eq!(sels[1].start.row, 1);
            assert_eq!(sels[2].start.row, 2);
            assert_eq!(sels[3].start.row, 3);
            assert_eq!(sels[4].start.row, 4);
        });
    }
}
