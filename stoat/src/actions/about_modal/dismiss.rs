//! About modal dismiss action implementation and tests.

use crate::stoat::Stoat;
use gpui::Context;
use tracing::debug;

impl Stoat {
    /// Dismiss the about modal.
    ///
    /// Clears about modal state. This action is invoked when the about modal needs to
    /// be closed, and handles mode and KeyContext transitions to restore the previous state.
    ///
    /// # Workflow
    ///
    /// 1. Verifies we're in about_modal mode (guard clause)
    /// 2. Clears about_modal_previous_mode state
    /// 3. Restores previous KeyContext
    /// 4. Triggers UI re-render via `cx.notify()`
    ///
    /// # Behavior
    ///
    /// - Only operates in about_modal mode
    /// - Clears previous_mode tracking state
    /// - Restores previous KeyContext and mode
    ///
    /// # Mode Transitions
    ///
    /// Mode restoration is handled by calling [`Self::handle_set_key_context`] which
    /// applies the default mode for the restored KeyContext. This allows the keymap
    /// to control what context/mode to return to.
    ///
    /// # Related
    ///
    /// - [`Self::open_about_modal`] - opens modal and saves mode
    /// - [`Self::handle_set_key_context`] - handles mode restoration
    ///
    /// # Integration
    ///
    /// Called by [`crate::actions::AboutModalDismiss`] action, typically bound to
    /// Escape in the keymap. The GUI layer detects mode change and unmounts the
    /// about modal UI.
    pub fn about_modal_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "about_modal" {
            return;
        }

        debug!("Dismissing about modal");

        // Clear about modal state
        self.about_modal_previous_mode = None;

        // Restore previous KeyContext (this auto-applies the default mode for that context)
        if let Some(previous_context) = self.about_modal_previous_key_context.take() {
            self.handle_set_key_context(previous_context, cx);
        } else {
            cx.notify();
        }
    }
}

use crate::pane_group::view::PaneGroupView;

impl PaneGroupView {
    pub(crate) fn handle_about_modal_dismiss(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.about_modal_dismiss(cx);
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
    fn dismisses_about_modal(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            // Open about modal
            s.open_about_modal(cx);
            assert_eq!(s.mode(), "about_modal");
            assert!(s.about_modal_previous_mode.is_some());

            // Dismiss it
            s.about_modal_dismiss(cx);

            // Check state is cleared
            assert!(s.about_modal_previous_mode.is_none());
        });
    }

    #[gpui::test]
    fn noop_outside_about_modal_mode(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            assert_eq!(s.mode(), "normal");

            // Try to dismiss when not in about_modal mode
            s.about_modal_dismiss(cx);

            // Should remain in normal mode
            assert_eq!(s.mode(), "normal");
        });
    }
}
