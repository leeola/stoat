use crate::{
    buffer::item::BufferItemEvent,
    claude::state::{ChatMessage, ClaudeState, ClaudeStateEvent, ClaudeStatus},
    content_view::{ContentView, ViewType},
    editor::{element::EditorElement, style::EditorStyle, view::EditorView},
    keymap::dispatch::handle_key_common,
    stoat::{Stoat, StoatEvent},
};
use gpui::{
    div, px, rgb, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, Window,
};
use std::sync::Arc;

pub enum ClaudeViewEvent {
    CloseRequested,
}

pub struct ClaudeView {
    state: Entity<ClaudeState>,
    stoat: Entity<Stoat>,
    focus_handle: FocusHandle,
    scroll_handle: ScrollHandle,
    input_stoat: Entity<Stoat>,
    input_editor_view: Entity<EditorView>,
    input_style: Arc<EditorStyle>,
    input_active: bool,
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

        let (input_stoat, input_editor_view, input_style) = {
            let s = stoat.read(cx);
            let config = s.config().clone();
            let worktree = s.worktree.clone();
            let buffer_store = s.buffer_store.clone();
            let compiled_keymap = s.compiled_keymap.clone();

            let mut style = EditorStyle::new(&config);
            style.show_line_numbers = false;
            style.show_diff_indicators = false;
            style.show_minimap = false;
            let style = Arc::new(style);

            let sub_stoat = cx.new(|cx| {
                Stoat::new_with_text(
                    config,
                    worktree,
                    buffer_store,
                    None,
                    compiled_keymap,
                    "",
                    cx,
                )
            });

            let sub_buffer = sub_stoat.read(cx).active_buffer(cx);
            let style_clone = style.clone();
            let view = cx.new(|cx| {
                let buffer_subscription =
                    cx.subscribe(&sub_buffer, |_, _, _event: &BufferItemEvent, _cx| {});
                EditorView {
                    stoat: sub_stoat.clone(),
                    focus_handle: cx.focus_handle(),
                    this: None,
                    editor_style: style_clone,
                    _buffer_subscription: buffer_subscription,
                    merge_state: None,
                    force_cursor: false,
                }
            });
            view.update(cx, |ev, _cx| {
                ev.this = Some(view.clone());
            });

            (sub_stoat, view, style)
        };

        let main_stoat = stoat.clone();
        cx.subscribe(&input_stoat, move |this, _stoat, event: &StoatEvent, cx| {
            if let StoatEvent::Action { name, args } = event {
                this.input_active = false;
                this.input_stoat.update(cx, |s, _| s.set_mode("normal"));
                this.input_editor_view
                    .update(cx, |v, _| v.force_cursor = false);
                main_stoat.update(cx, |_, cx| {
                    cx.emit(StoatEvent::Action {
                        name: name.clone(),
                        args: args.clone(),
                    });
                });
                cx.notify();
            }
        })
        .detach();

        Self {
            state,
            stoat,
            focus_handle: cx.focus_handle(),
            scroll_handle: ScrollHandle::new(),
            input_stoat,
            input_editor_view,
            input_style,
            input_active: false,
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
        if self.input_active {
            let input_mode = self.input_stoat.read(cx).mode().to_string();

            if input_mode == "insert"
                && event.keystroke.key.as_str() == "enter"
                && !event.keystroke.modifiers.shift
            {
                self.send_input(cx);
                return;
            }

            if input_mode == "normal" && event.keystroke.key.as_str() == "escape" {
                self.input_active = false;
                self.input_editor_view
                    .update(cx, |v, _| v.force_cursor = false);
                cx.notify();
                return;
            }

            if handle_key_common(&self.input_stoat, event, cx) {
                cx.notify();
                return;
            }

            if input_mode == "insert" {
                if let Some(key_char) = &event.keystroke.key_char {
                    self.input_stoat
                        .update(cx, |stoat, cx| stoat.insert_text(key_char, cx));
                    cx.notify();
                }
            }
            return;
        }

        let mode = self.stoat.read(cx).mode().to_string();
        if mode == "normal" && event.keystroke.key.as_str() == "i" {
            self.input_active = true;
            self.input_stoat.update(cx, |s, _| s.set_mode("insert"));
            self.input_editor_view
                .update(cx, |v, _| v.force_cursor = true);
            cx.notify();
            return;
        }

        if handle_key_common(&self.stoat, event, cx) {
            cx.notify();
        }
    }

    fn send_input(&mut self, cx: &mut Context<Self>) {
        let text = self
            .input_stoat
            .read(cx)
            .active_buffer(cx)
            .read(cx)
            .buffer()
            .read(cx)
            .text();

        self.state.update(cx, |s, cx| s.send_message(&text, cx));

        self.input_stoat.update(cx, |stoat, cx| {
            let buffer_item = stoat.active_buffer(cx);
            let buffer = buffer_item.read(cx).buffer().clone();
            let len = buffer.read(cx).len();
            if len > 0 {
                buffer.update(cx, |buf, _cx| {
                    buf.edit([(0..len, "")]);
                });
            }
            stoat.set_mode("insert");
        });

        cx.notify();
    }

    pub fn state_entity(&self) -> &Entity<ClaudeState> {
        &self.state
    }

    pub fn status_bar_info(&self, cx: &App) -> (String, ClaudeStatus, &'static str) {
        let stoat = if self.input_active {
            &self.input_stoat
        } else {
            &self.stoat
        };
        let s = stoat.read(cx);
        let mode_name = s.mode();
        let display = s
            .get_mode(mode_name)
            .map(|m| m.display_name.clone())
            .unwrap_or_else(|| mode_name.to_uppercase());
        let state = self.state.read(cx);
        (display, state.status, state.permission_mode_label())
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
                    .h(px(80.0))
                    .min_h(px(80.0))
                    .border_t_1()
                    .border_color(rgb(0x3e3e42))
                    .child(EditorElement::new(
                        self.input_editor_view.clone(),
                        self.input_style.clone(),
                    )),
            )
    }
}
