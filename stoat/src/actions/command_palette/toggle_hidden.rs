//! Command palette toggle hidden action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Toggle showing hidden commands in the command palette.
    ///
    /// Toggles the [`Self::command_palette_show_hidden`] flag and re-filters the command
    /// list to show or hide commands marked as hidden. Hidden commands are typically
    /// context-specific actions that cannot be executed from the command palette (e.g.,
    /// dismiss actions for modals).
    ///
    /// # Workflow
    ///
    /// 1. Toggle the show_hidden flag
    /// 2. Re-filter commands based on current query and new hidden state
    /// 3. Reset selection to first command
    /// 4. Emit Changed event and trigger re-render
    ///
    /// # Behavior
    ///
    /// - Only operates in command_palette mode
    /// - Preserves current search query
    /// - Resets selection to index 0 to avoid out-of-bounds
    /// - Hidden commands are filtered based on [`crate::actions::ActionMetadata::hidden`]
    ///
    /// # Related
    ///
    /// - [`Self::open_command_palette`] - opens palette with hidden commands filtered
    /// - [`Self::filter_commands`] - applies filtering logic with hidden state
    /// - [`crate::actions::ActionMetadata::hidden`] - metadata trait for marking actions hidden
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::ToggleCommandPaletteHidden`] action, typically bound
    /// to Ctrl-H in command palette mode. Users can press this to reveal commands that
    /// are normally hidden but may be useful in some contexts.
    pub fn toggle_command_palette_hidden(&mut self, cx: &mut Context<Self>) {
        if self.mode() != "command_palette" {
            return;
        }

        debug!(
            "Toggling command palette hidden commands: {} -> {}",
            self.command_palette_show_hidden, !self.command_palette_show_hidden
        );

        // Toggle the flag
        self.command_palette_show_hidden = !self.command_palette_show_hidden;

        // Get current query from input buffer
        let query = if let Some(input) = &self.command_palette_input {
            input.read(cx).text()
        } else {
            String::new()
        };

        // Re-filter commands with new hidden state
        self.filter_commands(&query);

        // Reset selection to avoid out-of-bounds issues
        self.command_palette_selected = 0;

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn toggles_show_hidden_flag(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            // Open command palette
            let keymap = gpui::Keymap::default();
            s.open_command_palette(&keymap, cx);

            // Verify initial state
            assert_eq!(s.mode(), "command_palette");
            assert!(!s.command_palette_show_hidden);

            // Toggle it
            s.toggle_command_palette_hidden(cx);
            assert!(s.command_palette_show_hidden);

            // Toggle it back
            s.toggle_command_palette_hidden(cx);
            assert!(!s.command_palette_show_hidden);
        });
    }

    #[gpui::test]
    fn resets_selection_on_toggle(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            // Open command palette
            let keymap = gpui::Keymap::default();
            s.open_command_palette(&keymap, cx);

            // Set selection to something other than 0
            s.command_palette_selected = 5;

            // Toggle hidden commands
            s.toggle_command_palette_hidden(cx);

            // Selection should be reset
            assert_eq!(s.command_palette_selected, 0);
        });
    }
}
