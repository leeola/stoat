//! AddSelectionBelow action implementation.
//!
//! Adds a cursor on the display line below at the same column position, enabling
//! columnar multi-cursor editing. Uses DisplayMap for correct handling of
//! soft-wrapped lines, folded regions, and inlay hints.

use crate::stoat::Stoat;
use gpui::Context;
use text::{Point, Selection, SelectionGoal};

impl Stoat {
    /// Add a cursor on the display line below at the same column position.
    ///
    /// Creates a new cursor one display line below the newest selection, preserving
    /// the column position in display space. Enables columnar editing by repeatedly
    /// invoking this action to build up vertical cursor stacks.
    ///
    /// # Algorithm
    ///
    /// 1. Get the newest selection (most recently added)
    /// 2. Convert to display coordinates
    /// 3. If already at last display line, no-op
    /// 4. Create cursor at same display column on next display line
    /// 5. Convert back to buffer coordinates
    /// 6. Add new selection to collection
    ///
    /// # DisplayMap Integration
    ///
    /// Unlike the previous simplified implementation, this correctly handles:
    /// - Display wrapping (soft wraps) - moves within wrapped lines
    /// - Folded regions - skips over folded code blocks
    /// - Inlay hints - maintains visual column position
    ///
    /// # Edge Cases
    ///
    /// - Last display line: No-op (can't go below)
    /// - Column beyond line length: Clamped by DisplayMap conversion
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

        // Get DisplaySnapshot for display-space operations
        let display_snapshot = self.display_map(cx).update(cx, |dm, cx| dm.snapshot(cx));
        let max_display_point = display_snapshot.max_point();

        // Get newest selection as Point
        let newest: Selection<Point> = self.selections.newest(&snapshot);

        // Convert to display coordinates
        let display_point =
            display_snapshot.point_to_display_point(newest.end, sum_tree::Bias::Left);

        // Check if on last display line
        if display_point.row >= max_display_point.row {
            return;
        }

        // Move down one display row, preserving column
        let target_display_point = stoat_display_map::DisplayPoint {
            row: display_point.row + 1,
            column: display_point.column,
        };

        // Convert back to buffer coordinates
        let new_point =
            display_snapshot.display_point_to_point(target_display_point, sum_tree::Bias::Left);

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
