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
        // Check if entering a mode with anchored selection
        if let Some(mode_meta) = self.get_mode(mode_name) {
            if mode_meta.anchored_selection && self.mode != mode_name {
                let cursor_pos = self.cursor.position();
                let buffer_item = self.active_buffer(cx);
                let snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

                // Initialize selection at cursor if needed
                if self.selections.count() == 1 {
                    let newest = self.selections.newest::<text::Point>(&snapshot);
                    if newest.is_empty() && newest.head() == cursor_pos {
                        // Already have empty selection at cursor - good
                    } else {
                        // Create new empty selection at cursor to anchor this mode
                        let id = self.selections.next_id();
                        self.selections.select(
                            vec![text::Selection {
                                id,
                                start: cursor_pos,
                                end: cursor_pos,
                                reversed: false,
                                goal: text::SelectionGoal::None,
                            }],
                            &snapshot,
                        );
                    }
                }
            }
        }

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

    #[gpui::test]
    fn initializes_selection_on_visual_mode_entry(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(1, 3));

            // Enter visual mode
            s.set_mode_by_name("visual", cx);

            // Should have selection at cursor position
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].start, text::Point::new(1, 3));
            assert_eq!(selections[0].end, text::Point::new(1, 3));
        });
    }

    #[gpui::test]
    fn visual_mode_j_extends_selection_down(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(0, 2));

            // Enter visual mode
            s.set_mode_by_name("visual", cx);

            // Press j (SelectDown in visual mode)
            s.select_down(cx);

            // Selection should extend from (0,2) to (1,2)
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].tail(), text::Point::new(0, 2));
            assert_eq!(selections[0].head(), text::Point::new(1, 2));
        });
    }

    #[gpui::test]
    fn visual_mode_k_extends_selection_up(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("Line 1\nLine 2\nLine 3", cx);
            s.set_cursor_position(text::Point::new(1, 2));

            // Enter visual mode and move up
            s.set_mode_by_name("visual", cx);
            s.select_up(cx);

            // Selection should extend from (1,2) to (0,2)
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].tail(), text::Point::new(1, 2));
            assert_eq!(selections[0].head(), text::Point::new(0, 2));
        });
    }

    #[gpui::test]
    fn visual_mode_b_extends_selection_backward(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("foo bar baz", cx);
            s.set_cursor_position(text::Point::new(0, 8)); // Start at 'b' in "baz"

            // Enter visual mode
            s.set_mode_by_name("visual", cx);

            // Press b once (select_prev_symbol in visual mode)
            s.select_prev_symbol(cx);

            // Selection should extend backward from (0,8) to start of "bar" (0,4)
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].tail(), text::Point::new(0, 8)); // anchor at start position
            assert_eq!(selections[0].head(), text::Point::new(0, 4)); // head at start of "bar"
            assert!(selections[0].reversed);

            // Press b again - should extend further back to "foo" (0,0)
            s.select_prev_symbol(cx);

            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].tail(), text::Point::new(0, 8)); // anchor should stay at start
            assert_eq!(selections[0].head(), text::Point::new(0, 0)); // head should be at start of "foo"
            assert!(selections[0].reversed);
        });
    }

    #[gpui::test]
    fn visual_mode_cursor_back_extends_selection(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| {
            s.insert_text("foo bar baz", cx);
            s.set_cursor_position(text::Point::new(0, 8)); // Start at 'b' in "baz"

            // Enter visual mode
            s.set_mode_by_name("visual", cx);

            // Call move_word_left - in visual mode this should delegate to select_prev_symbol
            s.move_word_left(cx);

            // In visual mode, move_word_left should extend the selection backward
            let selections = s.active_selections(cx);
            assert_eq!(selections.len(), 1);
            assert_eq!(selections[0].tail(), text::Point::new(0, 8)); // anchor stays
            assert_eq!(selections[0].head(), text::Point::new(0, 4)); // extends to "bar"
            assert!(selections[0].reversed);
        });
    }
}
