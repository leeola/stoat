//! Help modal open action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Open the help modal.
    ///
    /// Displays a full-screen modal showing comprehensive keybinding reference organized
    /// by category. The help modal provides users with quick access to all available
    /// commands and their keyboard shortcuts.
    ///
    /// # Workflow
    ///
    /// 1. Saves current mode to restore on dismiss
    /// 2. Sets KeyContext to HelpModal (changes UI rendering)
    /// 3. Sets mode to "help_modal" (activates help modal keybindings)
    /// 4. Triggers UI re-render via `cx.notify()`
    ///
    /// # Behavior
    ///
    /// - Saves previous mode for restoration on dismiss
    /// - Changes KeyContext which causes GUI layer to render help modal UI
    /// - Help modal shows all available commands grouped by category
    /// - Modal is dismissible via Escape key (handled by SetKeyContext action)
    ///
    /// # Mode Transitions
    ///
    /// Mode restoration is handled by the [`crate::actions::SetKeyContext`] action
    /// bound to Escape in the keymap. When Escape is pressed in help_modal mode:
    /// 1. SetKeyContext(TextEditor) is triggered
    /// 2. KeyContext changes back to TextEditor
    /// 3. Mode changes to TextEditor's default mode
    ///
    /// # Related
    ///
    /// - [`Self::help_modal_dismiss`] - clears help modal state
    /// - [`crate::actions::SetKeyContext`] - handles mode restoration on Escape
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::OpenHelpModal`] action, typically bound to
    /// a keyboard shortcut like ? or F1 in the keymap. The GUI layer renders the
    /// help modal when KeyContext is HelpModal, showing keybindings from the keymap
    /// configuration.
    pub fn open_help_modal(&mut self, cx: &mut Context<Self>) {
        debug!("Opening help modal");

        // Save current mode and context to restore later
        // TODO: Context restoration should be configurable via keymap once we have
        // concrete use cases to guide the design of keymap-based abstractions
        self.help_modal_previous_mode = Some(self.mode.clone());
        self.help_modal_previous_key_context = Some(self.key_context);
        self.key_context = crate::stoat::KeyContext::HelpModal;
        self.mode = "help_modal".to_string();

        cx.notify();
    }
}

use crate::pane_group::view::PaneGroupView;

impl PaneGroupView {
    pub(crate) fn handle_open_help_modal(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_help_modal(cx);
                });
            });
        }
        self.help_overlay_visible = false;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn opens_help_modal(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            let initial_mode = s.mode().to_string();

            // Open help modal
            s.open_help_modal(cx);

            // Check state
            assert_eq!(s.mode(), "help_modal");
            assert_eq!(s.key_context, crate::stoat::KeyContext::HelpModal);
            assert_eq!(s.help_modal_previous_mode, Some(initial_mode));
        });
    }

    #[gpui::test]
    fn saves_previous_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            // Start in insert mode
            s.enter_insert_mode(cx);
            assert_eq!(s.mode(), "insert");

            // Open help modal
            s.open_help_modal(cx);

            // Should save insert mode
            assert_eq!(s.help_modal_previous_mode, Some("insert".to_string()));
        });
    }
}
