//! KeyContext switching action implementation and tests.
//!
//! Provides functionality to switch between different UI contexts (TextEditor, Git,
//! FileFinder, etc.) with automatic mode restoration. KeyContext determines which UI
//! is rendered while mode determines interaction within that context.

use crate::Stoat;
use gpui::Context;
use tracing::{debug, warn};

impl Stoat {
    /// Set the active KeyContext (action handler).
    ///
    /// Changes which UI is rendered (e.g., TextEditor, Git modal, FileFinder) by setting
    /// the KeyContext and automatically switching to that context's default mode. The
    /// KeyContext determines the high-level "what's showing" while mode determines "how
    /// you interact with it".
    ///
    /// # Arguments
    ///
    /// * `context` - The KeyContext to activate (TextEditor, Git, FileFinder, etc.)
    /// * `cx` - GPUI context for event emission
    ///
    /// # Workflow
    ///
    /// 1. Sets the new KeyContext via [`Self::set_key_context`]
    /// 2. Looks up context metadata to find default mode
    /// 3. Sets mode to the context's default mode via [`Self::set_mode`]
    /// 4. Emits Changed event for status bar updates
    /// 5. Triggers UI re-render via `cx.notify()`
    ///
    /// # Behavior
    ///
    /// - Each KeyContext has a default mode defined in keymap.toml
    /// - Setting context automatically switches to default mode
    /// - Example: SetKeyContext(Git) sets context to Git and mode to "git_status"
    /// - Within Git context, can switch between git_status and git_filter modes
    /// - Logs warning if no metadata found for context
    ///
    /// # Context vs Mode
    ///
    /// - **KeyContext**: What UI is rendered (TextEditor, Git modal, FileFinder)
    /// - **Mode**: How you interact with it (normal, insert, git_status, git_filter)
    /// - Context changes = UI changes (modal appears/disappears)
    /// - Mode changes = keybinding changes (within same UI)
    ///
    /// # Example
    ///
    /// When opening git status modal:
    /// 1. User presses bound key for [`crate::actions::SetKeyContext`]
    /// 2. This method is called with [`crate::stoat::KeyContext::Git`]
    /// 3. Context changes to Git (modal appears in GUI)
    /// 4. Mode changes to "git_status" (default for Git context)
    /// 5. User can then switch to "git_filter" mode while staying in Git context
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::SetKeyContext`] action, bound in keymap.toml to
    /// keys that switch contexts (e.g., Escape to return to TextEditor, Space+g for
    /// Git). The GUI layer listens to the Changed event to update rendered UI.
    ///
    /// # Related
    ///
    /// - [`Self::set_key_context`] - internal method to set context without events
    /// - [`Self::set_mode`] - internal method to set mode without events
    /// - [`Self::get_key_context_meta`] - retrieves context metadata
    /// - [`crate::actions::SetMode`] - changes mode within current context
    pub fn handle_set_key_context(
        &mut self,
        context: crate::stoat::KeyContext,
        cx: &mut Context<Self>,
    ) {
        // Set the new context
        self.set_key_context(context);

        // Look up and set the default mode for this context
        if let Some(meta) = self.get_key_context_meta(context) {
            let default_mode = meta.default_mode.clone();
            self.set_mode(&default_mode);
            debug!(context = ?context, mode = %default_mode, "Set KeyContext with default mode");
        } else {
            warn!(context = ?context, "No metadata found for KeyContext, mode unchanged");
        }

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stoat::KeyContext;
    use gpui::TestAppContext;

    #[gpui::test]
    fn sets_context_and_default_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, cx| {
            // Should start in TextEditor context with normal mode
            assert_eq!(s.key_context(), KeyContext::TextEditor);
            assert_eq!(s.mode(), "normal");

            // Switch to Git context
            s.handle_set_key_context(KeyContext::Git, cx);

            // Should change context and mode
            assert_eq!(s.key_context(), KeyContext::Git);
            assert_eq!(s.mode(), "git_status"); // Default mode for Git context
        });
    }

    #[gpui::test]
    fn switches_between_contexts(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, cx| {
            // Start in TextEditor
            assert_eq!(s.key_context(), KeyContext::TextEditor);

            // Go to FileFinder
            s.handle_set_key_context(KeyContext::FileFinder, cx);
            assert_eq!(s.key_context(), KeyContext::FileFinder);
            assert_eq!(s.mode(), "file_finder");

            // Go to Git
            s.handle_set_key_context(KeyContext::Git, cx);
            assert_eq!(s.key_context(), KeyContext::Git);
            assert_eq!(s.mode(), "git_status");

            // Return to TextEditor
            s.handle_set_key_context(KeyContext::TextEditor, cx);
            assert_eq!(s.key_context(), KeyContext::TextEditor);
            assert_eq!(s.mode(), "normal");
        });
    }

    #[gpui::test]
    fn resets_mode_when_changing_context(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, cx| {
            // Start in TextEditor, switch to insert mode
            s.enter_insert_mode(cx);
            assert_eq!(s.mode(), "insert");

            // Switch to Git context
            s.handle_set_key_context(KeyContext::Git, cx);

            // Mode should reset to Git's default
            assert_eq!(s.mode(), "git_status");

            // Return to TextEditor
            s.handle_set_key_context(KeyContext::TextEditor, cx);

            // Mode should reset to TextEditor's default
            assert_eq!(s.mode(), "normal");
        });
    }

    #[gpui::test]
    fn applies_context_default_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, cx| {
            // Each context should apply its default mode
            let test_cases = vec![
                (KeyContext::TextEditor, "normal"),
                (KeyContext::Git, "git_status"),
                (KeyContext::FileFinder, "file_finder"),
                (KeyContext::BufferFinder, "buffer_finder"),
                (KeyContext::CommandPalette, "command_palette"),
                (KeyContext::HelpModal, "help_modal"),
            ];

            for (context, expected_mode) in test_cases {
                s.handle_set_key_context(context, cx);
                assert_eq!(
                    s.key_context(),
                    context,
                    "Failed to set context {context:?}"
                );
                assert_eq!(
                    s.mode(),
                    expected_mode,
                    "Wrong default mode for context {context:?}"
                );
            }
        });
    }
}
