//! Command palette open action implementation and tests.

use crate::{stoat::Stoat, stoat_actions::build_command_list};
use gpui::Context;
use std::num::NonZeroU64;
use text::{Buffer, BufferId};
use tracing::debug;

impl Stoat {
    /// Open the command palette modal.
    ///
    /// Builds a list of all available commands from action metadata and creates
    /// an input buffer for fuzzy search. The command palette provides a searchable
    /// interface to all registered actions in the editor.
    ///
    /// # Workflow
    ///
    /// 1. Saves current mode to restore on dismiss
    /// 2. Builds command list from all registered actions with metadata
    /// 3. Creates empty input buffer (BufferId 3) for search query
    /// 4. Initializes filtered list (initially shows all commands)
    /// 5. Sets KeyContext to CommandPalette and mode to "command_palette"
    ///
    /// # Behavior
    ///
    /// - Saves previous mode for restoration on dismiss
    /// - All commands are initially visible (no filter applied)
    /// - Selection starts at first command (index 0)
    /// - Integrates with fuzzy matching via [`Self::filter_commands`]
    /// - Input buffer updates trigger re-filtering in GUI layer
    ///
    /// # Related
    ///
    /// - [`Self::command_palette_dismiss`] - closes palette and restores mode
    /// - [`Self::command_palette_next`] - navigates down the list
    /// - [`Self::command_palette_prev`] - navigates up the list
    /// - [`Self::filter_commands`] - helper that filters commands by query
    /// - [`build_command_list`] - helper that builds list from action metadata
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::OpenCommandPalette`] action, typically bound to
    /// a keyboard shortcut like Space+P in the keymap. The GUI layer renders the
    /// palette modal using [`Self::command_palette_input`] and
    /// [`Self::command_palette_filtered`] accessors.
    pub fn open_command_palette(&mut self, _keymap: &gpui::Keymap, cx: &mut Context<Self>) {
        debug!(from_mode = self.mode(), "Opening command palette");

        // Save current mode to restore later
        self.command_palette_previous_mode = Some(self.mode.clone());

        // Build command list from action metadata
        let commands = build_command_list();
        debug!(command_count = commands.len(), "Built command list");

        // Create input buffer for search query
        let buffer_id = BufferId::from(NonZeroU64::new(3).unwrap()); // Use ID 3 for command palette
        let input_buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));

        // Initialize command palette state
        self.command_palette_input = Some(input_buffer);
        self.command_palette_commands = commands.clone();
        self.command_palette_filtered = commands;
        self.command_palette_selected = 0;

        // Enter command_palette mode
        self.key_context = crate::stoat::KeyContext::CommandPalette;
        self.mode = "command_palette".into();
        debug!("Entered command_palette mode");

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn opens_command_palette(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let initial_mode = s.mode().to_string();

            // Open command palette
            let keymap = gpui::Keymap::default();
            s.open_command_palette(&keymap, cx);

            // Check state
            assert_eq!(s.mode(), "command_palette");
            assert_eq!(s.key_context, crate::stoat::KeyContext::CommandPalette);
            assert!(s.command_palette_input.is_some());
            assert!(!s.command_palette_commands.is_empty());
            assert!(!s.command_palette_filtered.is_empty());
            assert_eq!(s.command_palette_selected, 0);
            assert_eq!(s.command_palette_previous_mode, Some(initial_mode));
        });
    }
}
