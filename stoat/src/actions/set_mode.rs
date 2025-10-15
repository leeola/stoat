//! Mode switching action implementation and tests.
//!
//! Provides functionality to switch between modes within the current KeyContext.
//! Mode determines which keybindings are active without changing the rendered UI.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Set the active mode within the current KeyContext.
    ///
    /// Changes which keybindings are active without changing the rendered UI. Used for
    /// transitions like git_status to git_filter within the Git context, or normal to
    /// insert within the TextEditor context.
    ///
    /// # Arguments
    ///
    /// * `mode_name` - Name of the mode to activate (e.g., "normal", "insert", "git_filter")
    /// * `cx` - GPUI context for event emission
    ///
    /// # Workflow
    ///
    /// 1. Updates internal mode string
    /// 2. Logs the mode change
    /// 3. Emits Changed event for status bar updates
    /// 4. Triggers UI re-render via `cx.notify()`
    ///
    /// # Behavior
    ///
    /// - Does NOT change KeyContext (UI stays the same)
    /// - Changes active keybindings based on mode in keymap.toml
    /// - Example: Switching from "git_status" to "git_filter" within Git context
    /// - Git modal stays visible, but different keys are active
    ///
    /// # Mode vs Context
    ///
    /// - **Mode**: Which keybindings are active (normal, insert, git_filter)
    /// - **KeyContext**: Which UI is rendered (TextEditor, Git modal, FileFinder)
    /// - Mode changes = keybinding changes (UI stays same)
    /// - Context changes = UI changes (modal appears/disappears)
    ///
    /// # Example
    ///
    /// Within Git context:
    /// 1. Start in "git_status" mode (browsing changed files)
    /// 2. Press key bound to [`crate::actions::SetMode`] with "git_filter" argument
    /// 3. This method switches mode to "git_filter"
    /// 4. Git modal stays visible, but now filter input is active
    /// 5. Different keybindings become available (e.g., Enter to apply filter)
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::SetMode`] action, bound in keymap.toml to keys
    /// that switch modes within a context (e.g., `/` in git_status mode to enter
    /// git_filter mode). The GUI layer listens to Changed event to update status bar.
    ///
    /// # Related
    ///
    /// - [`Self::set_mode`] - internal method to set mode without events
    /// - [`Self::handle_set_key_context`] - changes context and mode together
    /// - [`crate::actions::SetKeyContext`] - switches UI context
    /// - Keymap bindings - define which keys are active in each mode
    pub fn set_mode_by_name(&mut self, mode_name: &str, cx: &mut Context<Self>) {
        self.mode = mode_name.to_string();
        debug!(mode = mode_name, "Set mode");
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
    fn changes_mode_within_context(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, cx| {
            // Start in normal mode
            assert_eq!(s.mode(), "normal");
            let initial_context = s.key_context();

            // Switch to insert mode
            s.set_mode_by_name("insert", cx);

            // Mode changed but context stayed same
            assert_eq!(s.mode(), "insert");
            assert_eq!(s.key_context(), initial_context);
        });
    }

    #[gpui::test]
    fn switches_between_git_modes(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, cx| {
            // Enter Git context (starts in git_status mode)
            s.handle_set_key_context(KeyContext::Git, cx);
            assert_eq!(s.key_context(), KeyContext::Git);
            assert_eq!(s.mode(), "git_status");

            // Switch to git_filter mode
            s.set_mode_by_name("git_filter", cx);

            // Mode changed, context stayed Git
            assert_eq!(s.mode(), "git_filter");
            assert_eq!(s.key_context(), KeyContext::Git);

            // Switch back to git_status
            s.set_mode_by_name("git_status", cx);
            assert_eq!(s.mode(), "git_status");
            assert_eq!(s.key_context(), KeyContext::Git);
        });
    }

    #[gpui::test]
    fn allows_arbitrary_mode_names(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, cx| {
            // Can set any string as mode (validation is keymap's responsibility)
            s.set_mode_by_name("custom_mode", cx);
            assert_eq!(s.mode(), "custom_mode");

            s.set_mode_by_name("another_mode", cx);
            assert_eq!(s.mode(), "another_mode");
        });
    }

    #[gpui::test]
    fn preserves_context_when_changing_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, cx| {
            // Test that mode changes preserve context across different contexts
            let contexts = vec![
                KeyContext::TextEditor,
                KeyContext::Git,
                KeyContext::FileFinder,
            ];

            for context in contexts {
                s.handle_set_key_context(context, cx);
                let initial_context = s.key_context();

                // Change mode
                s.set_mode_by_name("test_mode", cx);

                // Context should be unchanged
                assert_eq!(s.key_context(), initial_context);
            }
        });
    }
}
