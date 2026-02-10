//! Help modal dismiss action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Dismiss the help modal.
    ///
    /// Clears help modal state. This action is invoked when the help modal needs to
    /// be closed, but the actual mode and KeyContext transitions are handled by the
    /// [`crate::actions::SetKeyContext`] action.
    ///
    /// # Workflow
    ///
    /// 1. Verifies we're in help_modal mode (guard clause)
    /// 2. Clears help_modal_previous_mode state
    /// 3. Triggers UI re-render via `cx.notify()`
    ///
    /// # Behavior
    ///
    /// - Only operates in help_modal mode
    /// - Clears previous_mode tracking state
    /// - Does NOT change mode or KeyContext (handled by SetKeyContext action)
    ///
    /// # Mode Transitions
    ///
    /// Mode restoration is now handled by the [`crate::actions::SetKeyContext`] action
    /// bound to Escape in the keymap. When Escape is pressed:
    /// 1. SetKeyContext(TextEditor) is triggered
    /// 2. This dismiss action clears state
    /// 3. SetKeyContext handles mode restoration
    ///
    /// This separation allows the keymap to control what context/mode to return to,
    /// rather than hardcoding it in the action.
    ///
    /// # Related
    ///
    /// - [`Self::open_help_modal`] - opens modal and saves mode
    /// - [`crate::actions::SetKeyContext`] - handles mode restoration on Escape
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::HelpModalDismiss`] action, typically bound to
    /// Escape in the keymap. The GUI layer detects mode change and unmounts the
    /// help modal UI.
    pub fn help_modal_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "help_modal" {
            return;
        }

        debug!("Dismissing help modal");

        // Clear help modal state
        self.help_modal_previous_mode = None;

        // Restore previous KeyContext (this auto-applies the default mode for that context)
        if let Some(previous_context) = self.help_modal_previous_key_context.take() {
            self.handle_set_key_context(previous_context, cx);
        } else {
            cx.notify();
        }
    }
}

use crate::pane_group::view::PaneGroupView;

impl PaneGroupView {
    pub(crate) fn handle_help_modal_dismiss(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.help_modal_dismiss(cx);
                });
            });
        }
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn dismisses_help_modal(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            // Open help modal
            s.open_help_modal(cx);
            assert_eq!(s.mode(), "help_modal");
            assert!(s.help_modal_previous_mode.is_some());

            // Dismiss it
            s.help_modal_dismiss(cx);

            // Check state is cleared (mode change handled by SetKeyContext)
            assert!(s.help_modal_previous_mode.is_none());
        });
    }

    #[gpui::test]
    fn noop_outside_help_modal_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            assert_eq!(s.mode(), "normal");

            // Try to dismiss when not in help_modal mode
            s.help_modal_dismiss(cx);

            // Should remain in normal mode
            assert_eq!(s.mode(), "normal");
        });
    }
}
