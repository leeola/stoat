//! Goto previous diff hunk action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use text::ToPoint;

impl Stoat {
    /// Jump to the previous diff hunk.
    ///
    /// Moves the cursor to the start of the previous git diff hunk before the current cursor
    /// position. Uses the buffer item's diff (computed via
    /// [`crate::buffer::item::BufferItem::diff`]) to find hunks. Wraps around to the last hunk
    /// if at the beginning of the file.
    ///
    /// # Workflow
    ///
    /// 1. Gets current cursor row
    /// 2. Gets buffer snapshot for anchor-to-point conversion
    /// 3. Gets diff from active buffer item
    /// 4. Searches backward for first hunk before cursor (or wraps to last hunk)
    /// 5. Moves cursor to hunk start row, column 0
    /// 6. Ensures cursor is visible via [`Stoat::ensure_cursor_visible`]
    ///
    /// # Behavior
    ///
    /// - Searches backward from cursor position
    /// - Wraps to last hunk if no hunks before cursor
    /// - Does nothing if no diff or no hunks
    /// - Works in any mode (not modal-specific)
    ///
    /// # Related
    ///
    /// - [`Stoat::goto_next_hunk`] - navigate to next hunk
    /// - [`Stoat::ensure_cursor_visible`] - scroll to make cursor visible
    /// - [`crate::buffer::item::BufferItem::diff`] - source of hunk data
    pub fn goto_prev_hunk(&mut self, cx: &mut Context<Self>) {
        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        let diff = buffer_item.read(cx).diff();
        if let Some(diff) = diff {
            // Find previous hunk before cursor
            let prev_hunk = diff
                .hunks
                .iter()
                .rev()
                .find(|hunk| {
                    let hunk_start_row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
                    hunk_start_row < cursor_row
                })
                .or_else(|| diff.hunks.last()); // Wrap to last hunk

            if let Some(hunk) = prev_hunk {
                let target_row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
                let target_pos = text::Point::new(target_row, 0);

                // Update cursor
                self.cursor.move_to(target_pos);

                // Sync selections to cursor position
                let id = self.selections.next_id();
                self.selections.select(
                    vec![text::Selection {
                        id,
                        start: target_pos,
                        end: target_pos,
                        reversed: false,
                        goal: text::SelectionGoal::None,
                    }],
                    &buffer_snapshot,
                );

                self.ensure_cursor_visible(cx);

                tracing::debug!("Jumped to previous diff hunk at row {}", target_row);
                cx.emit(crate::stoat::StoatEvent::Changed);
                cx.notify();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn jumps_to_prev_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            // Just verify it doesn't panic when no diff exists
            s.goto_prev_hunk(cx);
        });
    }
}
