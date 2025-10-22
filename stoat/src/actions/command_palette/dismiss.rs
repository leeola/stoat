//! Command palette dismiss action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Dismiss the command palette and return to the previous mode.
    ///
    /// Closes the command palette modal, clears all state, and returns to the mode
    /// that was active before opening the palette. This allows users to cancel command
    /// execution without side effects.
    ///
    /// # Workflow
    ///
    /// 1. Verifies we're in command_palette mode
    /// 2. Clears input buffer (drops the search Buffer entity)
    /// 3. Clears command lists (both full and filtered)
    /// 4. Resets selection index to 0
    /// 5. Clears previous_mode state
    /// 6. Emits Changed event and triggers re-render
    ///
    /// # Behavior
    ///
    /// - Only operates in command_palette mode
    /// - Clears all command palette state completely
    /// - Does NOT restore previous mode (handled by SetKeyContext action)
    /// - Memory for command list and input buffer is freed
    ///
    /// # Mode Transitions
    ///
    /// Mode restoration is now handled by the [`crate::actions::SetKeyContext`] action
    /// bound to Escape in the keymap. This action only clears the palette state.
    /// The SetKeyContext action will:
    /// 1. Set KeyContext back to TextEditor
    /// 2. Set mode to the default mode for TextEditor context
    ///
    /// # Related
    ///
    /// - [`Self::open_command_palette`] - opens palette and saves mode
    /// - [`Self::command_palette_next`] - navigates while palette is open
    /// - [`Self::command_palette_prev`] - navigates while palette is open
    /// - [`crate::actions::SetKeyContext`] - handles mode restoration on Escape
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::CommandPaletteDismiss`] action, typically bound
    /// to Escape. The GUI layer detects mode change and unmounts the palette modal.
    pub fn command_palette_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode() != "command_palette" {
            return;
        }

        debug!("Dismissing command palette");

        // Clear command palette state
        self.command_palette_input = None;
        self.command_palette_commands.clear();
        self.command_palette_filtered.clear();
        self.command_palette_selected = 0;
        self.command_palette_previous_mode = None;

        // Restore previous KeyContext (this auto-applies the default mode for that context)
        if let Some(previous_context) = self.command_palette_previous_key_context.take() {
            self.handle_set_key_context(previous_context, cx);
        } else {
            cx.emit(crate::stoat::StoatEvent::Changed);
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn dismisses_command_palette(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            // Open command palette
            let keymap = gpui::Keymap::default();
            s.open_command_palette(&keymap, cx);

            // Verify it's open
            assert_eq!(s.mode(), "command_palette");
            assert!(s.command_palette_input.is_some());

            // Dismiss it
            s.command_palette_dismiss(cx);

            // Check state is cleared
            assert!(s.command_palette_input.is_none());
            assert!(s.command_palette_commands.is_empty());
            assert!(s.command_palette_filtered.is_empty());
            assert_eq!(s.command_palette_selected, 0);
            assert!(s.command_palette_previous_mode.is_none());
        });
    }
}
