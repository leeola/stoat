use crate::inline_editor::InlineEditor;
use gpui::{
    div, px, rgb, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, Styled, Window,
};

pub enum QuickInputEvent {
    Changed(String),
    Confirm(String),
    Dismiss,
}

pub struct QuickInput {
    input: Entity<InlineEditor>,
    focus_handle: FocusHandle,
}

impl QuickInput {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let input = cx.new(|cx| {
            let mut editor = InlineEditor::new_single_line(cx);
            editor.set_prefix("/");
            editor
        });

        Self {
            input,
            focus_handle: cx.focus_handle(),
        }
    }

    pub fn text(&self, cx: &App) -> String {
        self.input.read(cx).text(cx)
    }

    pub fn set_text(&self, text: &str, cx: &mut Context<Self>) {
        self.input.update(cx, |editor, cx| {
            editor.clear(cx);
            editor.buffer().update(cx, |buf, _| {
                buf.edit([(0..0, text)]);
            });
        });
    }

    pub fn clear(&self, cx: &mut Context<Self>) {
        self.input.update(cx, |editor, cx| {
            editor.clear(cx);
        });
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event.keystroke.key.as_str() {
            "escape" => {
                cx.emit(QuickInputEvent::Dismiss);
            },
            "enter" => {
                let text = self.text(cx);
                cx.emit(QuickInputEvent::Confirm(text));
            },
            "backspace" => {
                self.input.update(cx, |editor, cx| {
                    let buf = editor.buffer().clone();
                    buf.update(cx, |buf, _| {
                        let len = buf.len();
                        if len > 0 {
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
                });
                let text = self.text(cx);
                cx.emit(QuickInputEvent::Changed(text));
                cx.notify();
            },
            _ => {
                if let Some(key_char) = &event.keystroke.key_char {
                    if !key_char.is_empty()
                        && !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.alt
                    {
                        self.input.update(cx, |editor, cx| {
                            editor.buffer().update(cx, |buf, _| {
                                let len = buf.len();
                                buf.edit([(len..len, key_char.as_str())]);
                            });
                            cx.notify();
                        });
                        let text = self.text(cx);
                        cx.emit(QuickInputEvent::Changed(text));
                        cx.notify();
                    }
                } else if event.keystroke.key == "space" {
                    self.input.update(cx, |editor, cx| {
                        editor.buffer().update(cx, |buf, _| {
                            let len = buf.len();
                            buf.edit([(len..len, " ")]);
                        });
                        cx.notify();
                    });
                    let text = self.text(cx);
                    cx.emit(QuickInputEvent::Changed(text));
                    cx.notify();
                }
            },
        }
    }
}

impl EventEmitter<QuickInputEvent> for QuickInput {}

impl Focusable for QuickInput {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for QuickInput {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.input.read(cx).text(cx);
        let display = if text.is_empty() {
            "/ search...".to_string()
        } else {
            format!("/{text}")
        };

        div()
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .h(px(28.0))
            .bg(rgb(0x252526))
            .border_1()
            .border_color(rgb(0x3e3e42))
            .px(px(8.0))
            .flex()
            .items_center()
            .text_color(rgb(0xd4d4d4))
            .text_size(px(12.0))
            .child(display)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    fn creates_empty(cx: &mut TestAppContext) {
        let input = cx.new(QuickInput::new);
        let text = cx.read_entity(&input, |qi, cx| qi.text(cx));
        assert!(text.is_empty());
    }

    #[gpui::test]
    fn set_and_get_text(cx: &mut TestAppContext) {
        let input = cx.new(QuickInput::new);
        input.update(cx, |qi, cx| qi.set_text("hello", cx));
        let text = cx.read_entity(&input, |qi, cx| qi.text(cx));
        assert_eq!(text, "hello");
    }

    #[gpui::test]
    fn clear_text(cx: &mut TestAppContext) {
        let input = cx.new(QuickInput::new);
        input.update(cx, |qi, cx| qi.set_text("hello", cx));
        input.update(cx, |qi, cx| qi.clear(cx));
        let text = cx.read_entity(&input, |qi, cx| qi.text(cx));
        assert!(text.is_empty());
    }
}
