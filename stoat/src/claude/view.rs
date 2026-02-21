use crate::{
    claude::state::{ChatMessage, ClaudeState, ClaudeStateEvent, ClaudeStatus},
    content_view::{ContentView, ViewType},
    keymap::dispatch::handle_key_common,
    stoat::Stoat,
};
use gpui::{
    div, px, rgb, App, Context, Entity, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, ScrollHandle, StatefulInteractiveElement,
    Styled, Window,
};

pub enum ClaudeViewEvent {
    CloseRequested,
}

pub struct ClaudeView {
    state: Entity<ClaudeState>,
    stoat: Entity<Stoat>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
}

impl ClaudeView {
    pub fn new(state: Entity<ClaudeState>, stoat: Entity<Stoat>, cx: &mut Context<Self>) -> Self {
        cx.subscribe(&state, |_this, _state, event, cx| match event {
            ClaudeStateEvent::Updated => cx.notify(),
            ClaudeStateEvent::CloseRequested => {
                cx.emit(ClaudeViewEvent::CloseRequested);
            },
        })
        .detach();

        Self {
            state,
            stoat,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
        }
    }

    pub fn stoat(&self) -> &Entity<Stoat> {
        &self.stoat
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let mode = self.stoat.read(cx).mode().to_string();

        if mode == "insert" {
            match event.keystroke.key.as_str() {
                "enter" => {
                    self.state.update(cx, |s, cx| s.send_message(cx));
                    cx.notify();
                    return;
                },
                "backspace" => {
                    self.state.update(cx, |s, _| {
                        let text = &mut s.input_text;
                        if !text.is_empty() {
                            let boundary =
                                text.char_indices().next_back().map(|(i, _)| i).unwrap_or(0);
                            text.truncate(boundary);
                        }
                    });
                    cx.notify();
                    return;
                },
                _ => {},
            }
        }

        if handle_key_common(&self.stoat, event, cx) {
            cx.notify();
            return;
        }

        // Claude-specific: insert text in Claude insert mode
        if mode == "insert" {
            if let Some(key_char) = &event.keystroke.key_char {
                if !key_char.is_empty()
                    && !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.alt
                {
                    self.state
                        .update(cx, |s, _| s.input_text.push_str(key_char));
                    cx.notify();
                }
            }
        }
    }

    pub fn state_entity(&self) -> &Entity<ClaudeState> {
        &self.state
    }

    pub fn status_bar_info(&self, cx: &App) -> (String, ClaudeStatus) {
        let stoat = self.stoat.read(cx);
        let mode_name = stoat.mode();
        let display = stoat
            .get_mode(mode_name)
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| mode_name.to_uppercase());
        let status = self.state.read(cx).status;
        (display, status)
    }

    pub fn status_label(status: ClaudeStatus) -> &'static str {
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
        let input_text = state.input_text.clone();
        let mode = self.stoat.read(cx).mode().to_string();

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

        let cursor = if mode == "insert" { "\u{2588}" } else { "" };
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
            .child(messages_container)
            .child(
                div()
                    .px(px(12.0))
                    .py(px(4.0))
                    .border_t_1()
                    .border_color(rgb(0x3e3e42))
                    .text_color(rgb(0xd4d4d4))
                    .text_size(px(13.0))
                    .child(if input_display.is_empty() {
                        "Press i to type...".to_string()
                    } else {
                        input_display
                    }),
            )
    }
}
