use crate::{
    buffer::item::BufferItemEvent,
    editor::{element::EditorElement, style::EditorStyle},
    keymap::{
        compiled::CompiledKey,
        dispatch::{dispatch_editor_action, dispatch_pane_action},
    },
    scroll,
    stoat::{KeyContext, Stoat},
};
use gpui::{
    div, point, App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, ScrollWheelEvent, Styled, Subscription, Window,
};
use std::sync::Arc;
use tracing::debug;

pub struct EditorView {
    pub(crate) stoat: Entity<Stoat>,
    focus_handle: FocusHandle,
    this: Option<Entity<Self>>,
    /// Cached editor style (Arc makes cloning cheap - just bumps refcount)
    pub(crate) editor_style: Arc<EditorStyle>,
    /// Subscription to BufferItem events for automatic UI updates when diagnostics change
    _buffer_subscription: Subscription,
    // NOTE: Selection state (add_selections_state, select_next_state, select_prev_state)
    // is tracked in Stoat struct, not here. EditorView is just a view layer.
}

impl EditorView {
    pub fn new(stoat: Entity<Stoat>, cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();

        // Create cached editor style once from config (Arc makes cloning cheap)
        let config = stoat.read(cx).config().clone();
        let editor_style = Arc::new(EditorStyle::new(&config));

        // Subscribe to BufferItem events to automatically re-render when diagnostics change
        let active_buffer = stoat.read(cx).active_buffer(cx);
        let buffer_subscription =
            cx.subscribe(&active_buffer, |_editor_view, _buffer_item, event, cx| {
                match event {
                    BufferItemEvent::DiagnosticsUpdated => {
                        cx.notify(); // Trigger re-render when diagnostics change
                    },
                }
            });

        Self {
            stoat,
            focus_handle,
            this: None,
            editor_style,
            _buffer_subscription: buffer_subscription,
        }
    }

    pub fn set_entity(&mut self, entity: Entity<Self>) {
        self.this = Some(entity);
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        debug!(
            "KeyDownEvent: keystroke={:?}, key_char={:?}",
            event.keystroke, event.keystroke.key_char
        );

        let no_modifiers = !event.keystroke.modifiers.control
            && !event.keystroke.modifiers.alt
            && !event.keystroke.modifiers.shift
            && !event.keystroke.modifiers.platform;

        // Replace-char interceptor: when replace_pending is set, the next key replaces
        if self.stoat.read(cx).replace_pending {
            if let Some(key_char) = &event.keystroke.key_char {
                if key_char == "\u{1b}" {
                    self.stoat
                        .update(cx, |stoat, _| stoat.replace_pending = false);
                } else {
                    self.stoat.update(cx, |stoat, cx| {
                        stoat.replace_pending = false;
                        stoat.replace_char_with(key_char, cx);
                    });
                }
            } else {
                self.stoat
                    .update(cx, |stoat, _| stoat.replace_pending = false);
            }
            cx.notify();
            return;
        }

        // Digit accumulation for count prefix in normal/visual modes
        if no_modifiers {
            let key_context = self.stoat.read(cx).key_context();
            let mode = self.stoat.read(cx).mode().to_string();
            if key_context == KeyContext::TextEditor && (mode == "normal" || mode == "visual") {
                if let Some(key_char) = &event.keystroke.key_char {
                    if let Some(digit) = key_char.chars().next().and_then(|c| c.to_digit(10)) {
                        let has_pending = self.stoat.read(cx).pending_count.is_some();
                        if digit >= 1 || has_pending {
                            self.stoat.update(cx, |stoat, _| {
                                let current = stoat.pending_count.unwrap_or(0);
                                stoat.pending_count =
                                    Some(current.saturating_mul(10).saturating_add(digit));
                            });
                            return;
                        }
                    }
                }
            }
        }

        let compiled_key = CompiledKey::from_keystroke(&event.keystroke);

        // Look up in compiled keymap
        let matched_action = {
            let stoat = self.stoat.read(cx);
            stoat
                .compiled_keymap
                .lookup(&compiled_key, stoat)
                .map(|binding| binding.action.clone())
        };

        if let Some(action) = matched_action {
            if dispatch_editor_action(&self.stoat, &action, cx) {
                self.stoat.update(cx, |stoat, _| stoat.pending_count = None);
                cx.notify();
                return;
            }
            if dispatch_pane_action(&self.stoat, &action, cx) {
                self.stoat.update(cx, |stoat, _| stoat.pending_count = None);
                cx.notify();
                return;
            }
            self.stoat.update(cx, |stoat, _| stoat.pending_count = None);
            return;
        }

        // No keymap match: InsertText fallback for text-input contexts
        let key_context = self.stoat.read(cx).key_context();
        let mode = self.stoat.read(cx).mode().to_string();

        let should_insert = match key_context {
            KeyContext::FileFinder | KeyContext::CommandPalette | KeyContext::BufferFinder => true,
            KeyContext::TextEditor => mode == "insert",
            _ => false,
        };

        if should_insert {
            if let Some(key_char) = &event.keystroke.key_char {
                self.stoat.update(cx, |stoat, cx| {
                    stoat.insert_text(key_char, cx);
                });
                cx.notify();
            }
        }

        self.stoat.update(cx, |stoat, _| stoat.pending_count = None);
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let view_entity = self
            .this
            .clone()
            .expect("EditorView entity not set - call set_entity() after creation");

        div()
            .id("editor")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_scroll_wheel(cx.listener(
                |view: &mut EditorView,
                 event: &ScrollWheelEvent,
                 _window: &mut Window,
                 cx: &mut Context<'_, EditorView>| {
                    // Invert Y direction for natural scrolling
                    let delta = match event.delta {
                        gpui::ScrollDelta::Pixels(pixels) => {
                            scroll::ScrollDelta::Pixels(point(pixels.x, -pixels.y))
                        },
                        gpui::ScrollDelta::Lines(lines) => {
                            scroll::ScrollDelta::Lines(point(lines.x, -lines.y))
                        },
                    };
                    let fast_scroll = event.modifiers.alt;

                    view.stoat.update(cx, |stoat, cx| {
                        stoat.handle_scroll(&delta, fast_scroll, cx);
                    });
                    cx.notify();
                },
            ))
            .relative() // Enable absolute positioning for children
            .size_full()
            .child(EditorElement::new(view_entity, self.editor_style.clone()))
        // FIXME: Minimap rendering will be integrated into EditorElement following Zed's approach
    }
}

impl crate::content_view::ContentView for EditorView {
    fn view_type(&self) -> crate::content_view::ViewType {
        crate::content_view::ViewType::Editor
    }

    fn stoat(&self) -> Option<&Entity<Stoat>> {
        Some(&self.stoat)
    }
}
