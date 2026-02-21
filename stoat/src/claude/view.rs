use crate::{
    claude::state::{ChatMessage, ClaudeState, ClaudeStateEvent, ClaudeStatus},
    content_view::{ContentView, ViewType},
};
use gpui::{
    div, px, rgb, App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, ScrollHandle, StatefulInteractiveElement,
    Styled, Window,
};

#[derive(Clone, Copy, PartialEq)]
enum InputMode {
    Normal,
    Insert,
}

pub enum ClaudeViewEvent {
    CloseRequested,
}

pub struct ClaudeView {
    state: Entity<ClaudeState>,
    focus_handle: FocusHandle,
    mode: InputMode,
    scroll_handle: ScrollHandle,
}

impl ClaudeView {
    pub fn new(state: Entity<ClaudeState>, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&state, |_this, _state, event, cx| match event {
            ClaudeStateEvent::Updated => cx.notify(),
            ClaudeStateEvent::CloseRequested => {
                cx.emit(ClaudeViewEvent::CloseRequested);
            },
        })
        .detach();

        Self {
            state,
            focus_handle: cx.focus_handle(),
            mode: InputMode::Normal,
            scroll_handle: ScrollHandle::new(),
        }
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.mode {
            InputMode::Normal => self.handle_normal_key(event, cx),
            InputMode::Insert => self.handle_insert_key(event, cx),
        }
    }

    fn handle_normal_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "i" => {
                self.mode = InputMode::Insert;
                cx.notify();
            },
            "q" => {
                self.state.update(cx, |s, cx| s.request_close(cx));
            },
            _ => {},
        }
    }

    fn handle_insert_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) {
        match event.keystroke.key.as_str() {
            "escape" => {
                self.mode = InputMode::Normal;
                cx.notify();
            },
            "enter" => {
                self.state.update(cx, |s, cx| s.send_message(cx));
                cx.notify();
            },
            "backspace" => {
                self.state.update(cx, |s, _| {
                    let text = &mut s.input_text;
                    if !text.is_empty() {
                        let boundary = text.char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                        text.truncate(boundary);
                    }
                });
                cx.notify();
            },
            _ => {
                if let Some(key_char) = &event.keystroke.key_char {
                    if !key_char.is_empty()
                        && !event.keystroke.modifiers.control
                        && !event.keystroke.modifiers.alt
                    {
                        self.state.update(cx, |s, _| {
                            s.input_text.push_str(key_char);
                        });
                        cx.notify();
                    }
                }
            },
        }
    }

    fn mode_label(&self) -> &'static str {
        match self.mode {
            InputMode::Normal => "NORMAL",
            InputMode::Insert => "INSERT",
        }
    }

    fn status_label(status: ClaudeStatus) -> &'static str {
        match status {
            ClaudeStatus::Idle => "Idle",
            ClaudeStatus::Connecting => "Connecting...",
            ClaudeStatus::Responding => "Responding...",
        }
    }
}

impl ContentView for ClaudeView {
    fn view_type(&self) -> ViewType {
        ViewType::Claude
    }
}

impl EventEmitter<ClaudeViewEvent> for ClaudeView {}

impl Focusable for ClaudeView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ClaudeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let status = state.status;
        let input_text = state.input_text.clone();
        let mode_label = self.mode_label();

        let mut messages_container = div()
            .id("claude-messages")
            .flex_1()
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .px(px(12.0))
            .py(px(8.0));

        for (i, msg) in state.messages.iter().enumerate() {
            let (role, text, role_color) = match msg {
                ChatMessage::User(t) => ("You", t.as_str(), rgb(0x569cd6)),
                ChatMessage::Assistant(t) => ("Claude", t.as_str(), rgb(0x4ec9b0)),
                ChatMessage::System(t) => ("System", t.as_str(), rgb(0x808080)),
                ChatMessage::Error(t) => ("Error", t.as_str(), rgb(0xf44747)),
            };

            messages_container = messages_container.child(
                div()
                    .id(gpui::ElementId::Name(format!("msg-{i}").into()))
                    .mb(px(8.0))
                    .child(
                        div()
                            .text_color(role_color)
                            .text_size(px(11.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child(format!("{role}:")),
                    )
                    .child(
                        div()
                            .text_color(rgb(0xd4d4d4))
                            .text_size(px(13.0))
                            .child(text.to_string()),
                    ),
            );
        }

        let cursor = if self.mode == InputMode::Insert {
            "\u{2588}"
        } else {
            ""
        };
        let input_display = format!("{input_text}{cursor}");

        div()
            .id("claude-view")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x1e1e1e))
            .font_family("Menlo")
            // Header
            .child(
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .bg(rgb(0x252526))
                    .border_b_1()
                    .border_color(rgb(0x3e3e42))
                    .flex()
                    .justify_between()
                    .child(
                        div()
                            .text_color(rgb(0xe0e0e0))
                            .text_size(px(13.0))
                            .font_weight(gpui::FontWeight::BOLD)
                            .child("Claude"),
                    )
                    .child(
                        div()
                            .text_color(rgb(0x808080))
                            .text_size(px(11.0))
                            .child(Self::status_label(status)),
                    ),
            )
            // Messages
            .child(messages_container)
            // Input area
            .child(
                div()
                    .px(px(12.0))
                    .py(px(6.0))
                    .bg(rgb(0x252526))
                    .border_t_1()
                    .border_color(rgb(0x3e3e42))
                    .flex()
                    .gap(px(8.0))
                    .child(
                        div()
                            .text_color(rgb(0x569cd6))
                            .text_size(px(10.0))
                            .child(mode_label),
                    )
                    .child(
                        div()
                            .flex_1()
                            .text_color(rgb(0xd4d4d4))
                            .text_size(px(13.0))
                            .child(if input_display.is_empty() {
                                "Press i to type...".to_string()
                            } else {
                                input_display
                            }),
                    ),
            )
    }
}
