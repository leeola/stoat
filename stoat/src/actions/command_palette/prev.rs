//! Command palette prev action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Move to the previous command in the command palette list.
    ///
    /// Moves the selection highlight up to the previous command in the filtered list.
    /// Used for keyboard navigation through command search results.
    ///
    /// # Workflow
    ///
    /// 1. Verifies we're in command_palette mode
    /// 2. Checks if selection can move up (not at start)
    /// 3. Decrements selection index
    /// 4. Triggers re-render via `cx.notify()`
    ///
    /// # Behavior
    ///
    /// - Only operates in command_palette mode
    /// - If at start of list, stays at first command (no wrapping)
    /// - Selection index updated immediately for GUI rendering
    /// - Does not modify the filtered list or search query
    ///
    /// # Related
    ///
    /// - [`Self::command_palette_next`] - moves selection down
    /// - [`Self::open_command_palette`] - initializes palette state
    /// - [`Self::filter_commands`] - resets selection when filter changes
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::CommandPalettePrev`] action, typically bound
    /// to Up or Ctrl+P. The GUI layer uses [`Self::command_palette_selected`]
    /// accessor to highlight the selected command.
    pub fn command_palette_prev(&mut self, cx: &mut Context<Self>) {
        if self.mode() != "command_palette" {
            return;
        }

        if self.command_palette_selected > 0 {
            self.command_palette_selected -= 1;
            debug!(
                selected = self.command_palette_selected,
                "Command palette: prev"
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
    fn moves_to_prev_command(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let keymap = gpui::Keymap::default();
            s.open_command_palette(&keymap, cx);

            if s.command_palette_filtered.len() > 1 {
                // Move to second item
                s.command_palette_selected = 1;
                s.command_palette_prev(cx);
                assert_eq!(s.command_palette_selected, 0);
            }
        });
    }

    #[gpui::test]
    fn stays_at_start_of_list(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let keymap = gpui::Keymap::default();
            s.open_command_palette(&keymap, cx);

            // Already at start (index 0)
            assert_eq!(s.command_palette_selected, 0);

            // Try to move before start
            s.command_palette_prev(cx);

            // Should stay at first position
            assert_eq!(s.command_palette_selected, 0);
        });
    }
}
