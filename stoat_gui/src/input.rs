use crate::{context::EditorContext, keymap::Keymap};
use gpui::{Action, Context, KeyDownEvent, Task, Timer};
use std::time::Duration;

/// Handles keyboard input and converts keystrokes to actions
pub struct InputHandler {
    keymap: Keymap,
    pending_keystrokes: Vec<String>,
    timeout_task: Option<Task<()>>,
}

impl InputHandler {
    pub fn new() -> Self {
        Self {
            keymap: Keymap::default(),
            pending_keystrokes: Vec::new(),
            timeout_task: None,
        }
    }

    /// Handle a key down event from GPUI
    pub fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        context: &EditorContext,
        cx: &mut Context<'_, crate::editor::view::EditorView>,
    ) -> Option<Box<dyn Action>> {
        let keystroke = self.event_to_keystroke(event);
        println!("Converted keystroke: {keystroke}");
        self.pending_keystrokes.push(keystroke.clone());

        // Try to find a matching binding
        if let Some(binding) = self.keymap.find_binding(&self.pending_keystrokes, context) {
            let action = binding.action.boxed_clone();
            self.clear_pending();
            return Some(action);
        }

        // Check if this could be the start of a multi-keystroke sequence
        if self
            .keymap
            .has_partial_match(&self.pending_keystrokes, context)
        {
            self.start_timeout(cx);
            None
        } else {
            // No match and no partial match - clear and try single keystroke
            self.clear_pending();
            self.pending_keystrokes.push(keystroke);

            if let Some(binding) = self.keymap.find_binding(&self.pending_keystrokes, context) {
                let action = binding.action.boxed_clone();
                self.clear_pending();
                Some(action)
            } else {
                self.clear_pending();
                None
            }
        }
    }

    /// Convert GPUI KeyDownEvent to string representation
    fn event_to_keystroke(&self, event: &KeyDownEvent) -> String {
        let mut parts = Vec::new();

        if event.keystroke.modifiers.platform {
            parts.push("cmd");
        }
        if event.keystroke.modifiers.control {
            parts.push("ctrl");
        }
        if event.keystroke.modifiers.alt {
            parts.push("alt");
        }
        if event.keystroke.modifiers.shift {
            parts.push("shift");
        }

        parts.push(&event.keystroke.key);
        parts.join("-")
    }

    /// Start timeout for multi-keystroke sequences
    fn start_timeout(&mut self, cx: &mut Context<'_, crate::editor::view::EditorView>) {
        self.timeout_task = Some(cx.spawn(async move |weak_handle, cx| {
            Timer::after(Duration::from_millis(1000)).await;

            if let Some(handle) = weak_handle.upgrade() {
                handle
                    .update(cx, |view, cx| {
                        view.input_handler.clear_pending();
                        cx.notify();
                    })
                    .ok();
            }
        }));
    }

    /// Clear pending keystrokes and timeout
    fn clear_pending(&mut self) {
        self.pending_keystrokes.clear();
        if let Some(task) = self.timeout_task.take() {
            task.detach();
        }
    }

    /// Get the current keymap
    pub fn keymap(&self) -> &Keymap {
        &self.keymap
    }

    /// Set a new keymap
    pub fn set_keymap(&mut self, keymap: Keymap) {
        self.keymap = keymap;
        self.clear_pending();
    }

    /// Initialize with default keymap
    pub fn with_default_keymap() -> Self {
        let mut handler = Self::new();
        handler.set_keymap(Keymap::load_default());
        handler
    }

    /// Get current pending keystrokes (for UI display)
    pub fn pending_keystrokes(&self) -> &[String] {
        &self.pending_keystrokes
    }
}

impl Default for InputHandler {
    fn default() -> Self {
        Self::new()
    }
}
