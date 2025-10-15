//! Command palette next action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Move to the next command in the command palette list.
    ///
    /// Moves the selection highlight down to the next command in the filtered list.
    /// Used for keyboard navigation through command search results.
    ///
    /// # Workflow
    ///
    /// 1. Verifies we're in command_palette mode
    /// 2. Checks if selection can move down (not at end)
    /// 3. Increments selection index
    /// 4. Triggers re-render via `cx.notify()`
    ///
    /// # Behavior
    ///
    /// - Only operates in command_palette mode
    /// - If at end of list, stays at last command (no wrapping)
    /// - Selection index updated immediately for GUI rendering
    /// - Does not modify the filtered list or search query
    ///
    /// # Related
    ///
    /// - [`Self::command_palette_prev`] - moves selection up
    /// - [`Self::open_command_palette`] - initializes palette state
    /// - [`Self::filter_commands`] - resets selection when filter changes
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::CommandPaletteNext`] action, typically bound
    /// to Down or Ctrl+N. The GUI layer uses [`Self::command_palette_selected`]
    /// accessor to highlight the selected command.
    pub fn command_palette_next(&mut self, cx: &mut Context<Self>) {
        if self.mode() != "command_palette" {
            return;
        }

        if self.command_palette_selected + 1 < self.command_palette_filtered.len() {
            self.command_palette_selected += 1;
            debug!(
                selected = self.command_palette_selected,
                "Command palette: next"
            );
        }

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn moves_to_next_command(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            // Open command palette and verify we have commands
            let keymap = gpui::Keymap::default();
            s.open_command_palette(&keymap, cx);

            if s.command_palette_filtered.len() > 1 {
                let initial_selected = s.command_palette_selected;
                s.command_palette_next(cx);
                assert_eq!(s.command_palette_selected, initial_selected + 1);
            }
        });
    }

    #[gpui::test]
    fn stays_at_end_of_list(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let keymap = gpui::Keymap::default();
            s.open_command_palette(&keymap, cx);

            // Move to end
            let count = s.command_palette_filtered.len();
            s.command_palette_selected = count.saturating_sub(1);

            // Try to move past end
            s.command_palette_next(cx);

            // Should stay at last position
            assert_eq!(s.command_palette_selected, count.saturating_sub(1));
        });
    }
}
