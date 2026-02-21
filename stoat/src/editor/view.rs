use crate::{
    buffer::item::BufferItemEvent,
    editor::{element::EditorElement, merge::align::extract_merge_content, style::EditorStyle},
    git::conflict::ConflictViewKind,
    keymap::dispatch::handle_key_common,
    scroll,
    stoat::{KeyContext, Stoat},
};
use gpui::{
    div, point, px, AnyElement, App, AppContext, Context, Entity, FocusHandle, Focusable, Hsla,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, ScrollWheelEvent, Styled,
    Subscription, Window,
};
use std::sync::Arc;
use tracing::debug;

pub(crate) struct MergeState {
    pub ours_view: Entity<EditorView>,
    pub result_view: Entity<EditorView>,
    pub theirs_view: Entity<EditorView>,
    resolution_count: usize,
    file_idx: usize,
}

pub struct EditorView {
    pub(crate) stoat: Entity<Stoat>,
    focus_handle: FocusHandle,
    this: Option<Entity<Self>>,
    /// Cached editor style (Arc makes cloning cheap - just bumps refcount)
    pub(crate) editor_style: Arc<EditorStyle>,
    /// Subscription to BufferItem events for automatic UI updates when diagnostics change
    _buffer_subscription: Subscription,
    /// When in merge mode, holds the 3 sub-editor views
    merge_state: Option<MergeState>,
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
            merge_state: None,
        }
    }

    pub fn set_entity(&mut self, entity: Entity<Self>) {
        self.this = Some(entity);
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    /// Rebuild the 3 sub-editors from current buffer + resolution state.
    pub(crate) fn rebuild_merge_state(&mut self, cx: &mut Context<'_, Self>) {
        let (content, language, config, worktree, buffer_store, compiled_keymap) = {
            let stoat = self.stoat.read(cx);
            let buffer_item = stoat.active_buffer(cx);
            let text = buffer_item.read(cx).buffer().read(cx).text();
            let conflicts = buffer_item.read(cx).conflicts().to_vec();
            let language = buffer_item.read(cx).language();
            let file_idx = stoat.conflict_state.file_idx;
            let resolutions = &stoat.conflict_state.resolutions;
            let content = extract_merge_content(&text, &conflicts, resolutions, file_idx);
            let config = stoat.config().clone();
            let worktree = stoat.worktree.clone();
            let buffer_store = stoat.buffer_store.clone();
            let compiled_keymap = stoat.compiled_keymap.clone();
            (
                content,
                language,
                config,
                worktree,
                buffer_store,
                compiled_keymap,
            )
        };

        let mut merge_style = (*self.editor_style).clone();
        merge_style.show_diff_indicators = false;
        merge_style.show_minimap = false;
        let merge_style = Arc::new(merge_style);

        let create_sub_editor = |text: &str, cx: &mut Context<'_, Self>| -> Entity<EditorView> {
            let config = config.clone();
            let worktree = worktree.clone();
            let buffer_store = buffer_store.clone();
            let compiled_keymap = compiled_keymap.clone();
            let merge_style = merge_style.clone();

            let sub_stoat = cx.new(|cx| {
                let s = Stoat::new_with_text(
                    config,
                    worktree,
                    buffer_store,
                    None,
                    compiled_keymap,
                    text,
                    cx,
                );
                s.active_buffer(cx).update(cx, |item, cx| {
                    item.set_language(language);
                    let _ = item.reparse(cx);
                });
                s
            });

            let sub_buffer = sub_stoat.read(cx).active_buffer(cx);
            let view = cx.new(|cx| {
                let buffer_subscription = cx.subscribe(&sub_buffer, |_, _, _event, _cx| {});
                EditorView {
                    stoat: sub_stoat,
                    focus_handle: cx.focus_handle(),
                    this: None,
                    editor_style: merge_style,
                    _buffer_subscription: buffer_subscription,
                    merge_state: None,
                }
            });
            view.update(cx, |ev, _cx| {
                ev.this = Some(view.clone());
            });
            view
        };

        // Set scroll limit on main stoat based on content line count
        let line_count = content.ours.chars().filter(|&c| c == '\n').count() as u32;
        self.stoat
            .update(cx, |s, _| s.merge_display_row_count = Some(line_count));

        let ours_view = create_sub_editor(&content.ours, cx);
        let result_view = create_sub_editor(&content.result, cx);
        let theirs_view = create_sub_editor(&content.theirs, cx);

        let main_stoat = self.stoat.clone();
        ours_view.update(cx, |ev, cx| {
            ev.stoat.update(cx, |s, _| {
                s.merge_highlight_rows = content.ours_highlights.clone();
                s.scroll_source = Some(main_stoat.clone());
            });
        });
        result_view.update(cx, |ev, cx| {
            ev.stoat.update(cx, |s, _| {
                s.merge_highlight_rows = content.result_highlights.clone();
                s.scroll_source = Some(main_stoat.clone());
            });
        });
        theirs_view.update(cx, |ev, cx| {
            ev.stoat.update(cx, |s, _| {
                s.merge_highlight_rows = content.theirs_highlights.clone();
                s.scroll_source = Some(main_stoat.clone());
            });
        });

        let resolution_count = self.stoat.read(cx).conflict_state.resolutions.len();
        let file_idx = self.stoat.read(cx).conflict_state.file_idx;
        self.merge_state = Some(MergeState {
            ours_view,
            result_view,
            theirs_view,
            resolution_count,
            file_idx,
        });
    }

    /// Update the result sub-editor's buffer content after a resolution change.
    pub(crate) fn rebuild_result_content(&mut self, cx: &mut Context<'_, Self>) {
        let merge = match &self.merge_state {
            Some(m) => m,
            None => return,
        };

        let content = {
            let stoat = self.stoat.read(cx);
            let buffer_item = stoat.active_buffer(cx);
            let text = buffer_item.read(cx).buffer().read(cx).text();
            let conflicts = buffer_item.read(cx).conflicts().to_vec();
            let file_idx = stoat.conflict_state.file_idx;
            let resolutions = &stoat.conflict_state.resolutions;
            extract_merge_content(&text, &conflicts, resolutions, file_idx)
        };

        let result_view = merge.result_view.clone();
        let main_stoat = self.stoat.clone();
        result_view.update(cx, |ev, cx| {
            let buffer = ev
                .stoat
                .read(cx)
                .active_buffer(cx)
                .read(cx)
                .buffer()
                .clone();
            buffer.update(cx, |buf, _| {
                let old_len = buf.text().len();
                buf.edit([(0..old_len, content.result.as_str())]);
            });
            ev.stoat.read(cx).active_buffer(cx).update(cx, |item, cx| {
                let _ = item.reparse(cx);
            });
            ev.stoat.update(cx, |s, _| {
                s.merge_highlight_rows = content.result_highlights;
                s.scroll_source = Some(main_stoat);
            });
        });
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

        // Find-char interceptor: when find_char_pending is set, the next key triggers the find
        if let Some(find_mode) = self.stoat.read(cx).find_char_pending {
            if let Some(key_char) = &event.keystroke.key_char {
                if key_char == "\u{1b}" {
                    self.stoat
                        .update(cx, |stoat, _| stoat.find_char_pending = None);
                } else {
                    self.stoat.update(cx, |stoat, cx| {
                        stoat.find_char_pending = None;
                        stoat.find_char_with(key_char, find_mode, cx);
                    });
                }
            } else {
                self.stoat
                    .update(cx, |stoat, _| stoat.find_char_pending = None);
            }
            cx.notify();
            return;
        }

        // Select-regex interceptor: accumulate pattern characters and live-preview.
        // Named keys are checked first via the `key` field because GPUI sets
        // key_char for some named keys (e.g. Enter has key_char "\n").
        if self.stoat.read(cx).select_regex_pending.is_some() {
            match event.keystroke.key.as_ref() {
                "enter" => self.stoat.update(cx, |s, cx| s.select_regex_submit(cx)),
                "backspace" => self.stoat.update(cx, |stoat, cx| {
                    if let Some(pattern) = &mut stoat.select_regex_pending {
                        pattern.pop();
                    }
                    stoat.select_regex_preview(cx);
                }),
                "escape" => self
                    .stoat
                    .update(cx, |stoat, cx| stoat.select_regex_cancel(cx)),
                _ => {
                    if let Some(key_char) = &event.keystroke.key_char {
                        self.stoat.update(cx, |stoat, cx| {
                            if let Some(pattern) = &mut stoat.select_regex_pending {
                                pattern.push_str(key_char);
                            }
                            stoat.select_regex_preview(cx);
                        });
                    }
                },
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

        if handle_key_common(&self.stoat, event, cx) {
            cx.notify();
            return;
        }

        // Editor-specific: insert text in TextEditor insert mode
        let key_context = self.stoat.read(cx).key_context();
        let mode = self.stoat.read(cx).mode().to_string();
        if key_context == KeyContext::TextEditor && mode == "insert" {
            if let Some(key_char) = &event.keystroke.key_char {
                self.stoat
                    .update(cx, |stoat, cx| stoat.insert_text(key_char, cx));
                cx.notify();
            }
        }
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

        let use_merge = {
            let s = self.stoat.read(cx);
            s.is_in_conflict_review() && s.conflict_view_kind == ConflictViewKind::Merge
        };

        // Manage merge_state lifecycle
        if use_merge && self.merge_state.is_none() {
            self.rebuild_merge_state(cx);
        } else if use_merge {
            let (current_file_idx, current_count) = {
                let stoat = self.stoat.read(cx);
                (
                    stoat.conflict_state.file_idx,
                    stoat.conflict_state.resolutions.len(),
                )
            };

            if self
                .merge_state
                .as_ref()
                .is_some_and(|m| m.file_idx != current_file_idx)
            {
                self.rebuild_merge_state(cx);
            } else if self
                .merge_state
                .as_ref()
                .is_some_and(|m| m.resolution_count != current_count)
            {
                self.rebuild_result_content(cx);
                if let Some(m) = &mut self.merge_state {
                    m.resolution_count = current_count;
                }
            }
        } else if self.merge_state.is_some() {
            self.merge_state = None;
        }

        let child: AnyElement = if use_merge {
            let merge = self.merge_state.as_ref().unwrap();
            let style = self.editor_style.clone();

            let separator_color: Hsla = style.merge_separator_color;

            div()
                .flex()
                .flex_row()
                .size_full()
                .child(
                    div()
                        .flex_1()
                        .child(EditorElement::new(merge.ours_view.clone(), style.clone())),
                )
                .child(div().w(px(1.0)).h_full().bg(separator_color))
                .child(
                    div()
                        .flex_1()
                        .child(EditorElement::new(merge.result_view.clone(), style.clone())),
                )
                .child(div().w(px(1.0)).h_full().bg(separator_color))
                .child(
                    div()
                        .flex_1()
                        .child(EditorElement::new(merge.theirs_view.clone(), style)),
                )
                .into_any_element()
        } else {
            EditorElement::new(view_entity, self.editor_style.clone()).into_any_element()
        };

        div()
            .id("editor")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::handle_key_down))
            .on_scroll_wheel(cx.listener(
                |view: &mut EditorView,
                 event: &ScrollWheelEvent,
                 _window: &mut Window,
                 cx: &mut Context<'_, EditorView>| {
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
            .relative()
            .size_full()
            .child(child)
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
