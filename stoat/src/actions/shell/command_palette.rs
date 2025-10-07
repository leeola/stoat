//! Command palette modal for fuzzy command search and execution
//!
//! Provides a searchable interface to discover and execute all available commands in the editor.
//! Commands are built from the keymap bindings, showing each action's name and description.

use crate::{CommandInfo, Stoat};
use gpui::{App, AppContext, Keymap};
use std::{any::TypeId, num::NonZeroU64};
use text::{Buffer, BufferId};
use tracing::debug;

impl Stoat {
    /// Open the command palette modal.
    ///
    /// Builds a list of all available commands from the keymap bindings and creates
    /// an input buffer for fuzzy search. Transitions to command_palette mode.
    ///
    /// # Behavior
    ///
    /// - Saves current mode to restore later
    /// - Builds command list from all keymap bindings
    /// - Creates empty input buffer for search query
    /// - Initializes filtered commands list (initially all commands)
    /// - Sets mode to "command_palette"
    ///
    /// # Related
    ///
    /// See also:
    /// - [`crate::Stoat::command_palette_dismiss`] - close command palette
    /// - [`crate::Stoat::command_palette_next`] - navigate down
    /// - [`crate::Stoat::command_palette_prev`] - navigate up
    /// - [`crate::Stoat::command_palette_execute`] - execute selected command
    pub fn open_command_palette(&mut self, keymap: &Keymap, cx: &mut App) {
        debug!(from_mode = self.mode(), "Opening command palette");

        // Save current mode to restore later
        self.command_palette_previous_mode = Some(self.current_mode.clone());

        // Build command list from keymap
        let commands = build_command_list(keymap);
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
        self.set_mode("command_palette");
    }

    /// Move to the next command in the command palette list.
    ///
    /// Moves the selection highlight down to the next command in the filtered list.
    /// If at the end of the list, stays at the last command.
    ///
    /// # Behavior
    ///
    /// - Increments selected index if not at end
    /// - Clamps to list bounds
    /// - No-op if command palette is not open
    pub fn command_palette_next(&mut self) {
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
    }

    /// Move to the previous command in the command palette list.
    ///
    /// Moves the selection highlight up to the previous command in the filtered list.
    /// If at the beginning of the list, stays at the first command.
    ///
    /// # Behavior
    ///
    /// - Decrements selected index if not at start
    /// - Clamps to list bounds
    /// - No-op if command palette is not open
    pub fn command_palette_prev(&mut self) {
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
    }

    /// Dismiss the command palette and return to the previous mode.
    ///
    /// Closes the command palette modal, clears all state, and returns
    /// to the mode that was active before opening the palette.
    ///
    /// # Behavior
    ///
    /// - Returns to previous mode (or normal if none)
    /// - Clears input buffer
    /// - Clears command lists
    /// - Resets selection index
    /// - No-op if command palette is not open
    pub fn command_palette_dismiss(&mut self) {
        if self.mode() != "command_palette" {
            return;
        }

        debug!("Dismissing command palette");

        // Restore previous mode or default to normal
        let previous_mode = self
            .command_palette_previous_mode
            .take()
            .unwrap_or_else(|| "normal".to_string());
        self.set_mode(&previous_mode);

        // Clear command palette state
        self.command_palette_input = None;
        self.command_palette_commands.clear();
        self.command_palette_filtered.clear();
        self.command_palette_selected = 0;
    }

    /// Get the TypeId of the currently selected command.
    ///
    /// Returns the TypeId of the selected command's action for dispatch,
    /// or None if the command palette is not open or no command is selected.
    ///
    /// # Returns
    ///
    /// [`Some(TypeId)`] of the selected command's action, or [`None`]
    pub fn command_palette_selected_type_id(&self) -> Option<TypeId> {
        if self.mode() != "command_palette" {
            return None;
        }

        self.command_palette_filtered
            .get(self.command_palette_selected)
            .map(|cmd| cmd.type_id)
    }

    /// Accessor for command palette input buffer (for GUI layer).
    pub fn command_palette_input(&self) -> &Option<gpui::Entity<Buffer>> {
        &self.command_palette_input
    }

    /// Accessor for filtered commands list (for GUI layer).
    pub fn command_palette_filtered(&self) -> &[CommandInfo] {
        &self.command_palette_filtered
    }

    /// Accessor for selected command index (for GUI layer).
    pub fn command_palette_selected(&self) -> usize {
        self.command_palette_selected
    }
}

/// Build the list of all available commands from the keymap.
///
/// Iterates through all bindings in the keymap and extracts command information
/// including name, description, keystroke, and TypeId for dispatch.
///
/// # Arguments
///
/// * `keymap` - The keymap to extract commands from
///
/// # Returns
///
/// A vector of [`CommandInfo`] structs representing all available commands
fn build_command_list(keymap: &Keymap) -> Vec<CommandInfo> {
    use std::collections::HashMap;

    let mut commands_by_type_id: HashMap<TypeId, CommandInfo> = HashMap::new();

    // Iterate through all bindings
    for binding in keymap.bindings() {
        let action = binding.action();
        let type_id = action.type_id();

        // Skip if we've already seen this action type
        if commands_by_type_id.contains_key(&type_id) {
            continue;
        }

        // Get action name and description, skip if either unavailable
        let Some(name) = crate::actions::action_name(action) else {
            continue;
        };
        let Some(description) = crate::actions::description(action) else {
            continue;
        };

        commands_by_type_id.insert(
            type_id,
            CommandInfo {
                name: name.to_string(),
                description: description.to_string(),
                type_id,
            },
        );
    }

    // Convert to sorted vector
    let mut commands: Vec<CommandInfo> = commands_by_type_id.into_values().collect();
    commands.sort_by(|a, b| a.name.cmp(&b.name));

    commands
}
