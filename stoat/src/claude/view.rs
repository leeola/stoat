use crate::{
    buffer::item::BufferItemEvent,
    claude::state::{AssistantBlock, ChatMessage, ClaudeState, ClaudeStateEvent, ClaudeStatus},
    content_view::{ContentView, ViewType},
    editor::{element::EditorElement, style::EditorStyle, view::EditorView},
    keymap::dispatch::handle_key_common,
    stoat::{KeyContext, Stoat, StoatEvent},
};
use gpui::{
    div, px, rgb, App, AppContext, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, ScrollHandle,
    StatefulInteractiveElement, Styled, Window,
};
use std::{collections::HashSet, sync::Arc};

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
    expanded_thinking: HashSet<usize>,
    draft_text: Option<String>,
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
            let services = s.services.clone();

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
                    services,
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
                this.input_stoat.update(cx, |s, _| s.set_mode("normal"));
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
            expanded_thinking: HashSet::new(),
            draft_text: None,
        }
    }

    pub fn stoat(&self) -> &Entity<Stoat> {
        &self.stoat
    }

    #[cfg(any(test, feature = "dev-tools"))]
    pub(crate) fn input_stoat(&self) -> &Entity<Stoat> {
        &self.input_stoat
    }

    pub(crate) fn input_is_focused(&self, window: &Window, cx: &App) -> bool {
        self.input_editor_view
            .read(cx)
            .focus_handle
            .is_focused(window)
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let main_ctx = self.stoat.read(cx).key_context();

        // When main stoat is in an overlay context (command palette, finder, etc.
        // opened from input), route keys through main stoat which has the correct
        // key_context for the overlay's keymap and text input handling.
        if main_ctx != KeyContext::Claude {
            if main_ctx == KeyContext::TextEditor {
                self.stoat
                    .update(cx, |s, _| s.set_key_context(KeyContext::Claude));
            } else {
                if handle_key_common(&self.stoat, event, cx) {
                    cx.notify();
                }
                return;
            }
        }

        if self.input_is_focused(window, cx) {
            let input_mode = self.input_stoat.read(cx).mode().to_string();

            if input_mode == "insert"
                && event.keystroke.key.as_str() == "enter"
                && !event.keystroke.modifiers.shift
            {
                self.send_input(cx);
                return;
            }

            if input_mode == "normal" && event.keystroke.key.as_str() == "escape" {
                window.focus(&self.focus_handle, cx);
                cx.notify();
                return;
            }

            if input_mode == "normal" && event.keystroke.key.as_str() == "q" {
                self.state.update(cx, |s, cx| s.stop(cx));
                return;
            }

            if input_mode == "normal" {
                let key = event.keystroke.key.as_str();
                let has_history = !self.state.read(cx).input_history.is_empty();
                if (key == "k" || key == "up") && has_history {
                    if self.draft_text.is_none() {
                        self.draft_text = Some(self.get_input_text(cx));
                    }
                    if let Some(text) = self.state.update(cx, |s, _| s.history_up()) {
                        self.set_input_text(&text, cx);
                        cx.notify();
                    }
                    return;
                }
                if (key == "j" || key == "down") && self.draft_text.is_some() {
                    match self.state.update(cx, |s, _| s.history_down()) {
                        Some(text) => self.set_input_text(&text, cx),
                        None => {
                            if let Some(draft) = self.draft_text.take() {
                                self.set_input_text(&draft, cx);
                            }
                        },
                    }
                    cx.notify();
                    return;
                }
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
        if mode == "normal" && event.keystroke.key.as_str() == "q" {
            self.state.update(cx, |s, cx| s.stop(cx));
            return;
        }

        if mode == "normal" && event.keystroke.key.as_str() == "i" {
            let input_handle = self.input_editor_view.read(cx).focus_handle.clone();
            window.focus(&input_handle, cx);
            self.input_stoat.update(cx, |s, _| s.set_mode("insert"));
            cx.notify();
            return;
        }

        if handle_key_common(&self.stoat, event, cx) {
            cx.notify();
        }
    }

    fn get_input_text(&self, cx: &Context<Self>) -> String {
        self.input_stoat
            .read(cx)
            .active_buffer(cx)
            .read(cx)
            .buffer()
            .read(cx)
            .text()
    }

    fn set_input_text(&self, text: &str, cx: &mut Context<Self>) {
        self.input_stoat.update(cx, |stoat, cx| {
            let buffer_item = stoat.active_buffer(cx);
            let buffer = buffer_item.read(cx).buffer().clone();
            let len = buffer.read(cx).len();
            buffer.update(cx, |buf, _cx| {
                buf.edit([(0..len, text)]);
            });
        });
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

    pub fn status_bar_info(
        &self,
        window: &Window,
        cx: &App,
    ) -> (String, ClaudeStatus, &'static str) {
        let stoat = if self.input_is_focused(window, cx) {
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

const BG_PRIMARY: u32 = 0x1e1e1e;
const BG_ELEVATED: u32 = 0x252526;
const BG_CODE: u32 = 0x1a1a1a;
const BORDER: u32 = 0x3e3e42;
const TEXT_PRIMARY: u32 = 0xcccccc;
const TEXT_MUTED: u32 = 0x808080;
const TEXT_ERROR: u32 = 0xf44747;
const TEXT_TOOL: u32 = 0xce9178;
const ACCENT_THINKING: u32 = 0x569cd6;

impl Render for ClaudeView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);
        let primary_session = state.primary_session_id.clone();
        let status = state.status;
        let permission_label = state.permission_mode_label();
        let model = state.model.clone();

        let mut messages_container = div()
            .id("claude-messages")
            .flex_1()
            .min_w(px(0.0))
            .overflow_y_scroll()
            .track_scroll(&self.scroll_handle)
            .px(px(12.0))
            .py(px(8.0));

        for (i, msg) in state.messages.iter().enumerate() {
            let msg_element = match msg {
                ChatMessage::User { text, session_id } => {
                    let is_sub = is_sub_agent(session_id, &primary_session);
                    let mut card = div()
                        .id(gpui::ElementId::Name(format!("msg-{i}").into()))
                        .mb(px(8.0))
                        .bg(rgb(BG_ELEVATED))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .rounded(px(6.0))
                        .px(px(10.0))
                        .py(px(6.0))
                        .text_color(rgb(TEXT_PRIMARY))
                        .text_size(px(13.0))
                        .child(text.to_string());
                    if is_sub {
                        card = card
                            .ml(px(16.0))
                            .border_l_2()
                            .border_color(rgb(ACCENT_THINKING));
                    }
                    card
                },
                ChatMessage::Assistant { blocks, session_id } => {
                    let is_sub = is_sub_agent(session_id, &primary_session);
                    let mut container = div()
                        .id(gpui::ElementId::Name(format!("msg-{i}").into()))
                        .mb(px(8.0))
                        .pl(px(4.0));
                    if is_sub {
                        container = container
                            .ml(px(16.0))
                            .border_l_2()
                            .border_color(rgb(ACCENT_THINKING))
                            .pl(px(8.0))
                            .child(
                                div()
                                    .text_color(rgb(TEXT_MUTED))
                                    .text_size(px(10.0))
                                    .mb(px(2.0))
                                    .child("Sub-agent"),
                            );
                    }
                    for (bi, block) in blocks.iter().enumerate() {
                        container = match block {
                            AssistantBlock::Text { text } => {
                                container.child(render_text_blocks(text, i, bi))
                            },
                            AssistantBlock::ToolUse {
                                name,
                                input_summary,
                            } => container.child(
                                div()
                                    .mb(px(4.0))
                                    .border_l_2()
                                    .border_color(rgb(TEXT_TOOL))
                                    .bg(rgb(BG_ELEVATED))
                                    .rounded_r(px(4.0))
                                    .px(px(8.0))
                                    .py(px(4.0))
                                    .child(
                                        div()
                                            .text_color(rgb(TEXT_TOOL))
                                            .text_size(px(12.0))
                                            .font_weight(gpui::FontWeight::BOLD)
                                            .child(name.clone()),
                                    )
                                    .child(
                                        div()
                                            .text_color(rgb(TEXT_MUTED))
                                            .text_size(px(11.0))
                                            .child(input_summary.clone()),
                                    ),
                            ),
                            AssistantBlock::Thinking { text } => {
                                let expanded = self.expanded_thinking.contains(&i);
                                let header_label =
                                    if expanded { "v Thinking" } else { "> Thinking" };
                                let mut section = div().mb(px(4.0)).child(
                                    div()
                                        .id(gpui::ElementId::Name(
                                            format!("thinking-{i}-{bi}").into(),
                                        ))
                                        .cursor_pointer()
                                        .text_color(rgb(ACCENT_THINKING))
                                        .text_size(px(11.0))
                                        .on_click(cx.listener(move |this, _, _window, _cx| {
                                            if this.expanded_thinking.contains(&i) {
                                                this.expanded_thinking.remove(&i);
                                            } else {
                                                this.expanded_thinking.insert(i);
                                            }
                                        }))
                                        .child(header_label),
                                );
                                if expanded {
                                    section = section.child(
                                        div()
                                            .border_l_2()
                                            .border_color(rgb(ACCENT_THINKING))
                                            .pl(px(8.0))
                                            .mt(px(2.0))
                                            .text_color(rgb(TEXT_MUTED))
                                            .text_size(px(12.0))
                                            .child(text.clone()),
                                    );
                                }
                                container.child(section)
                            },
                            AssistantBlock::RedactedThinking => container.child(
                                div()
                                    .mb(px(4.0))
                                    .text_color(rgb(TEXT_MUTED))
                                    .text_size(px(11.0))
                                    .child("[thinking redacted]"),
                            ),
                            AssistantBlock::ServerToolUse { name } => container.child(
                                div()
                                    .mb(px(4.0))
                                    .border_l_2()
                                    .border_color(rgb(TEXT_TOOL))
                                    .bg(rgb(BG_ELEVATED))
                                    .rounded_r(px(4.0))
                                    .px(px(8.0))
                                    .py(px(4.0))
                                    .child(
                                        div()
                                            .text_color(rgb(TEXT_TOOL))
                                            .text_size(px(12.0))
                                            .child(name.clone()),
                                    ),
                            ),
                            AssistantBlock::Unknown => container.child(
                                div()
                                    .mb(px(4.0))
                                    .text_color(rgb(TEXT_MUTED))
                                    .text_size(px(11.0))
                                    .child("[unknown content]"),
                            ),
                        };
                    }
                    container
                },
                ChatMessage::System { text, model } => div()
                    .id(gpui::ElementId::Name(format!("msg-{i}").into()))
                    .mb(px(8.0))
                    .flex()
                    .justify_center()
                    .child(div().text_color(rgb(TEXT_MUTED)).text_size(px(11.0)).child(
                        if let Some(m) = model {
                            format!("{text} ({m})")
                        } else {
                            text.clone()
                        },
                    )),
                ChatMessage::Error { text } => div()
                    .id(gpui::ElementId::Name(format!("msg-{i}").into()))
                    .mb(px(8.0))
                    .bg(rgb(0x3a1515))
                    .border_1()
                    .border_color(rgb(TEXT_ERROR))
                    .rounded(px(6.0))
                    .px(px(10.0))
                    .py(px(6.0))
                    .text_color(rgb(TEXT_ERROR))
                    .text_size(px(13.0))
                    .child(text.to_string()),
                ChatMessage::Result {
                    duration_ms,
                    num_turns,
                    cost_usd,
                    ..
                } => {
                    let secs = *duration_ms as f64 / 1000.0;
                    div()
                        .id(gpui::ElementId::Name(format!("msg-{i}").into()))
                        .mb(px(8.0))
                        .border_t_1()
                        .border_color(rgb(BORDER))
                        .pt(px(4.0))
                        .flex()
                        .justify_center()
                        .child(div().text_color(rgb(TEXT_MUTED)).text_size(px(10.0)).child(
                            format!("Completed in {secs:.1}s - {num_turns} turns - ${cost_usd:.4}"),
                        ))
                },
            };
            messages_container = messages_container.child(msg_element);
        }

        if status == ClaudeStatus::Responding {
            messages_container = messages_container.child(
                div()
                    .text_color(rgb(TEXT_MUTED))
                    .text_size(px(11.0))
                    .py(px(4.0))
                    .child("Claude is responding..."),
            );
        }

        let mut input_footer = div()
            .flex()
            .px(px(8.0))
            .py(px(2.0))
            .gap(px(8.0))
            .text_size(px(10.0))
            .text_color(rgb(TEXT_MUTED));

        let mode_color = match permission_label {
            "accept-edits" => 0xdcdcaa,
            "plan-only" => 0x569cd6,
            "full-access" => 0xf44747,
            _ => 0x4ec9b0,
        };
        input_footer = input_footer.child(
            div()
                .px(px(4.0))
                .rounded(px(3.0))
                .bg(rgb(BG_ELEVATED))
                .text_color(rgb(mode_color))
                .child(permission_label),
        );

        if let Some(m) = &model {
            input_footer = input_footer.child(div().child(m.clone()));
        }

        input_footer = input_footer.child(div().child(Self::status_label(status)));

        div()
            .id("claude-view")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(BG_PRIMARY))
            .font_family("Menlo")
            .child(messages_container)
            .child(
                div()
                    .min_h(px(80.0))
                    .border_t_1()
                    .border_color(rgb(BORDER))
                    .flex()
                    .flex_col()
                    .child(input_footer)
                    .child(
                        div()
                            .track_focus(&self.input_editor_view.read(cx).focus_handle)
                            .flex_1()
                            .child(EditorElement::new(
                                self.input_editor_view.clone(),
                                self.input_style.clone(),
                            )),
                    ),
            )
    }
}

fn is_sub_agent(session_id: &str, primary: &Option<String>) -> bool {
    match primary {
        Some(p) => !session_id.is_empty() && session_id != p,
        None => false,
    }
}

fn render_text_blocks(text: &str, msg_idx: usize, block_idx: usize) -> gpui::Div {
    use crate::{
        hover::{render_markdown, SectionKind},
        syntax::SyntaxTheme,
    };
    use gpui::{Hsla, SharedString};

    let theme = SyntaxTheme::monokai_dark();
    let sections = render_markdown(text, &theme);
    let mut container = div()
        .min_w(px(0.0))
        .text_color(rgb(TEXT_PRIMARY))
        .text_size(px(13.0))
        .mb(px(4.0));

    for (ti, section) in sections.iter().enumerate() {
        container = match section.kind {
            SectionKind::Prose => {
                let mut row = div()
                    .id(gpui::ElementId::Name(
                        format!("prose-{msg_idx}-{block_idx}-{ti}").into(),
                    ))
                    .flex()
                    .flex_row()
                    .flex_wrap();
                for span in &section.spans {
                    let mut span_el = div().child(SharedString::from(span.text.clone()));
                    let default_prose: Hsla = rgb(TEXT_PRIMARY).into();
                    if span.color != default_prose {
                        span_el = span_el.text_color(span.color);
                    }
                    if span.font_weight != gpui::FontWeight::NORMAL {
                        span_el = span_el.font_weight(span.font_weight);
                    }
                    if span.font_style == gpui::FontStyle::Italic {
                        span_el = span_el.italic();
                    }
                    row = row.child(span_el);
                }
                container.child(row)
            },
            SectionKind::Code => {
                let mut code_row = div().flex().flex_row().flex_wrap().text_size(px(12.0));
                for span in &section.spans {
                    let span_el = div()
                        .child(SharedString::from(span.text.clone()))
                        .text_color(span.color);
                    code_row = code_row.child(span_el);
                }
                container.child(
                    div()
                        .id(gpui::ElementId::Name(
                            format!("code-{msg_idx}-{block_idx}-{ti}").into(),
                        ))
                        .mb(px(4.0))
                        .bg(rgb(BG_CODE))
                        .border_1()
                        .border_color(rgb(BORDER))
                        .rounded(px(4.0))
                        .px(px(8.0))
                        .py(px(6.0))
                        .child(code_row),
                )
            },
        };
    }
    container
}
