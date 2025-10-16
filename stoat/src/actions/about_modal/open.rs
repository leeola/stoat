//! About modal open action implementation and tests.

use crate::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Open the about modal.
    ///
    /// Displays a modal showing Stoat version information including git commit hash
    /// and build status (clean or dirty). The about modal provides users with quick
    /// access to build information for debugging and support purposes.
    ///
    /// # Workflow
    ///
    /// 1. Saves current mode to restore on dismiss
    /// 2. Sets KeyContext to AboutModal (changes UI rendering)
    /// 3. Sets mode to "about_modal" (activates about modal keybindings)
    /// 4. Triggers UI re-render via `cx.notify()`
    ///
    /// # Behavior
    ///
    /// - Saves previous mode for restoration on dismiss
    /// - Changes KeyContext which causes GUI layer to render about modal UI
    /// - About modal shows build information from [`crate::build_info::build_info`]
    /// - Modal is dismissible via Escape key (handled by SetKeyContext action)
    ///
    /// # Mode Transitions
    ///
    /// Mode restoration is handled by the [`crate::actions::SetKeyContext`] action
    /// bound to Escape in the keymap. When Escape is pressed in about_modal mode:
    /// 1. SetKeyContext(TextEditor) is triggered
    /// 2. KeyContext changes back to TextEditor
    /// 3. Mode changes to TextEditor's default mode
    ///
    /// # Related
    ///
    /// - [`Self::about_modal_dismiss`] - clears about modal state
    /// - [`crate::actions::SetKeyContext`] - handles mode restoration on Escape
    /// - [`crate::build_info::build_info`] - provides build information to display
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::OpenAboutModal`] action, typically bound to
    /// a keyboard shortcut in the keymap. The GUI layer renders the about modal
    /// when KeyContext is AboutModal, displaying build information.
    pub fn open_about_modal(&mut self, cx: &mut Context<Self>) {
        debug!("Opening about modal");

        // Save current mode and context to restore later
        self.about_modal_previous_mode = Some(self.mode.clone());
        self.about_modal_previous_key_context = Some(self.key_context);
        self.key_context = crate::stoat::KeyContext::AboutModal;
        self.mode = "about_modal".to_string();

        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn opens_about_modal(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let initial_mode = s.mode().to_string();

            // Open about modal
            s.open_about_modal(cx);

            // Check state
            assert_eq!(s.mode(), "about_modal");
            assert_eq!(s.key_context, crate::stoat::KeyContext::AboutModal);
            assert_eq!(s.about_modal_previous_mode, Some(initial_mode));
        });
    }

    #[gpui::test]
    fn saves_previous_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            // Start in insert mode
            s.enter_insert_mode(cx);
            assert_eq!(s.mode(), "insert");

            // Open about modal
            s.open_about_modal(cx);

            // Should save insert mode
            assert_eq!(s.about_modal_previous_mode, Some("insert".to_string()));
        });
    }
}
