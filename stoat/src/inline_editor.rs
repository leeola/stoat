//! Reusable inline editor component for modal inputs.
//!
//! Provides a simplified API for creating single-line or multi-line text inputs
//! with features like prefix, placeholder, and full editing capabilities.

use crate::actions::{DeleteLeft, InsertText};
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Render,
    Window,
};
use std::num::NonZeroU64;
use text::{Buffer, BufferId};

/// Inline editor for modal inputs.
///
/// Wraps [`Entity<Buffer>`] with a simplified API for common input patterns.
/// Supports prefix (like ":"), placeholder text, single-line or multi-line mode.
pub struct InlineEditor {
    /// The underlying text buffer
    buffer: Entity<Buffer>,
    /// Optional prefix displayed before input (e.g., ":" for command line)
    prefix: Option<String>,
    /// Optional placeholder text when buffer is empty
    placeholder: Option<String>,
    /// Whether this is a single-line editor
    single_line: bool,
    /// Focus handle for keyboard input
    focus_handle: FocusHandle,
}

impl InlineEditor {
    /// Create a new single-line inline editor.
    ///
    /// Allocates a unique BufferId starting from 1000 to avoid conflicts
    /// with regular text buffers.
    pub fn new_single_line(cx: &mut Context<Self>) -> Self {
        static NEXT_INLINE_ID: std::sync::atomic::AtomicU64 =
            std::sync::atomic::AtomicU64::new(1000);
        let id = NEXT_INLINE_ID.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

        let buffer_id = BufferId::from(NonZeroU64::new(id).expect("BufferId overflow"));
        let buffer = cx.new(|_| Buffer::new(0, buffer_id, ""));

        Self {
            buffer,
            prefix: None,
            placeholder: None,
            single_line: true,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Create a new multi-line inline editor.
    pub fn new_multi_line(cx: &mut Context<Self>) -> Self {
        let mut editor = Self::new_single_line(cx);
        editor.single_line = false;
        editor
    }

    /// Set the prefix displayed before input (e.g., ":" or ">").
    pub fn set_prefix(&mut self, prefix: impl Into<String>) {
        self.prefix = Some(prefix.into());
    }

    /// Set the placeholder text shown when buffer is empty.
    pub fn set_placeholder(&mut self, placeholder: impl Into<String>) {
        self.placeholder = Some(placeholder.into());
    }

    /// Get the current text content of the editor.
    pub fn text(&self, cx: &App) -> String {
        self.buffer.read(cx).snapshot().text()
    }

    /// Clear all text from the editor.
    pub fn clear(&mut self, cx: &mut Context<Self>) {
        self.buffer.update(cx, |buffer, _| {
            let len = buffer.len();
            if len > 0 {
                buffer.edit([(0..len, "")]);
            }
        });
    }

    /// Get a reference to the underlying buffer.
    pub fn buffer(&self) -> &Entity<Buffer> {
        &self.buffer
    }

    /// Take ownership of the underlying buffer entity.
    ///
    /// Useful when passing the buffer to other components or storing it in state.
    pub fn into_buffer(self) -> Entity<Buffer> {
        self.buffer
    }

    /// Whether this is a single-line editor.
    pub fn is_single_line(&self) -> bool {
        self.single_line
    }

    /// Handle text insertion when this editor is focused.
    fn handle_insert_text(
        &mut self,
        action: &InsertText,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.buffer.update(cx, |buf, _| {
            let len = buf.len();
            buf.edit([(len..len, action.0.as_str())]);
        });
        cx.notify();
        cx.emit(());
    }

    /// Handle backspace when this editor is focused.
    fn handle_delete_left(&mut self, _: &DeleteLeft, _window: &mut Window, cx: &mut Context<Self>) {
        self.buffer.update(cx, |buf, _| {
            let len = buf.len();
            if len > 0 {
                // Find the previous character boundary
                let text = buf.text();
                let mut end = len;
                while end > 0 {
                    end -= 1;
                    if text.is_char_boundary(end) {
                        break;
                    }
                }
                buf.edit([(end..len, "")]);
            }
        });
        cx.notify();
        cx.emit(());
    }

    /// Handle key down events and convert them to actions.
    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // Ignore special keys - let them propagate to keybinding system
        if event.keystroke.key == "escape"
            || event.keystroke.key == "enter"
            || event.keystroke.key == "tab"
            || event.keystroke.key == "up"
            || event.keystroke.key == "down"
        {
            return;
        }

        // If there's a printable character, insert it
        if let Some(key_char) = &event.keystroke.key_char {
            if !key_char.is_empty()
                && !event.keystroke.modifiers.control
                && !event.keystroke.modifiers.alt
            {
                self.handle_insert_text(&InsertText(key_char.clone()), window, cx);
            }
        }
    }
}

impl EventEmitter<()> for InlineEditor {}

impl Focusable for InlineEditor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for InlineEditor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        use gpui::{div, px, rgb, InteractiveElement, ParentElement, Styled};

        let text = self.buffer.read(cx).snapshot().text();
        let display_text = if text.is_empty() {
            self.placeholder.clone().unwrap_or_default()
        } else {
            format!("{}{}", self.prefix.as_deref().unwrap_or(""), text)
        };

        div()
            .track_focus(&self.focus_handle(cx))
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_action(cx.listener(Self::handle_insert_text))
            .on_action(cx.listener(Self::handle_delete_left))
            .h(px(24.0))
            .bg(rgb(0x252526))
            .border_1()
            .border_color(rgb(0x3e3e42))
            .px(px(8.0))
            .flex()
            .items_center()
            .text_color(rgb(0xd4d4d4))
            .text_size(px(12.0))
            .child(display_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui::test]
    fn creates_single_line_editor(cx: &mut gpui::TestAppContext) {
        let editor = cx.new(|cx| InlineEditor::new_single_line(cx));

        assert!(cx.read_entity(&editor, |e, _| e.is_single_line()));
        assert!(cx.read_entity(&editor, |e, cx| e.text(cx).is_empty()));
    }

    #[gpui::test]
    fn creates_multi_line_editor(cx: &mut gpui::TestAppContext) {
        let editor = cx.new(|cx| InlineEditor::new_multi_line(cx));

        assert!(!cx.read_entity(&editor, |e, _| e.is_single_line()));
    }

    #[gpui::test]
    fn sets_prefix(cx: &mut gpui::TestAppContext) {
        let editor = cx.new(|cx| InlineEditor::new_single_line(cx));
        editor.update(cx, |editor, _| {
            editor.set_prefix(":");
        });

        let prefix = cx.read_entity(&editor, |e, _| e.prefix.clone());
        assert_eq!(prefix, Some(":".to_string()));
    }

    #[gpui::test]
    fn sets_placeholder(cx: &mut gpui::TestAppContext) {
        let editor = cx.new(|cx| InlineEditor::new_single_line(cx));
        editor.update(cx, |editor, _| {
            editor.set_placeholder("Type command...");
        });

        let placeholder = cx.read_entity(&editor, |e, _| e.placeholder.clone());
        assert_eq!(placeholder, Some("Type command...".to_string()));
    }

    #[gpui::test]
    fn clears_content(cx: &mut gpui::TestAppContext) {
        let editor = cx.new(|cx| InlineEditor::new_single_line(cx));

        // Insert some text
        editor.update(cx, |editor, cx| {
            editor.buffer.update(cx, |buffer, _| {
                buffer.edit([(0..0, "test")]);
            });
        });

        let text = cx.read_entity(&editor, |e, cx| e.text(cx));
        assert_eq!(text, "test");

        // Clear
        editor.update(cx, |editor, cx| {
            editor.clear(cx);
        });

        let text = cx.read_entity(&editor, |e, cx| e.text(cx));
        assert!(text.is_empty());
    }

    #[gpui::test]
    fn gets_text(cx: &mut gpui::TestAppContext) {
        let editor = cx.new(|cx| InlineEditor::new_single_line(cx));

        editor.update(cx, |editor, cx| {
            editor.buffer.update(cx, |buffer, _| {
                buffer.edit([(0..0, "hello world")]);
            });
        });

        let text = cx.read_entity(&editor, |e, cx| e.text(cx));
        assert_eq!(text, "hello world");
    }
}
