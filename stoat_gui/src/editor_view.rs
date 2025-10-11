use crate::{editor_element::EditorElement, editor_style::EditorStyle};
use gpui::{
    div, point, App, AppContext, Context, Entity, FocusHandle, Focusable, InteractiveElement,
    IntoElement, KeyDownEvent, ParentElement, Render, ScrollWheelEvent, Styled, Window,
};
use std::sync::Arc;
use stoat::{actions::*, scroll, Stoat};

pub struct EditorView {
    pub(crate) stoat: Entity<Stoat>,
    focus_handle: FocusHandle,
    this: Option<Entity<Self>>,
    minimap_view: Option<Entity<EditorView>>,
    /// Cached editor style (Arc makes cloning cheap - just bumps refcount)
    editor_style: Arc<EditorStyle>,
}

impl EditorView {
    pub fn new(stoat: Entity<Stoat>, cx: &mut Context<'_, Self>) -> Self {
        eprintln!("[PERF] EditorView::new() - creating main editor");

        let focus_handle = cx.focus_handle();

        // Create cached editor style once (Arc makes cloning cheap)
        let editor_style = Arc::new(EditorStyle::default());

        // Create minimap Stoat
        eprintln!("[PERF] EditorView::new() - creating minimap Stoat");
        let minimap_stoat = stoat.update(cx, |stoat, cx| stoat.create_minimap(cx));

        // Wrap minimap Stoat in EditorView (following Zed's architecture)
        eprintln!("[PERF] EditorView::new() - wrapping minimap in EditorView");
        let minimap_view = cx.new(|cx| {
            let minimap_focus_handle = cx.focus_handle();
            EditorView {
                stoat: minimap_stoat,
                focus_handle: minimap_focus_handle,
                this: None,
                minimap_view: None, // Minimap doesn't have its own minimap
                editor_style: Arc::new(EditorStyle::default()), // Minimap has its own style
            }
        });

        // Set entity on minimap view so it can render
        eprintln!("[PERF] EditorView::new() - setting entity on minimap view");
        minimap_view.update(cx, |minimap, _cx| {
            minimap.set_entity(minimap_view.clone());
        });

        Self {
            stoat,
            focus_handle,
            this: None,
            minimap_view: Some(minimap_view),
            editor_style,
        }
    }

    pub fn set_entity(&mut self, entity: Entity<Self>) {
        self.this = Some(entity);
    }

    pub fn is_focused(&self, window: &Window) -> bool {
        self.focus_handle.is_focused(window)
    }

    pub fn minimap_view(&self) -> Option<&Entity<EditorView>> {
        self.minimap_view.as_ref()
    }

    // ==== Action handlers ====

    fn handle_insert_text(
        &mut self,
        command: &InsertText,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.insert_text(&command.0, cx);
        });
        cx.notify();
    }

    fn handle_delete_left(
        &mut self,
        _: &DeleteLeft,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        let mode = self.stoat.read(cx).mode().to_string();

        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_left(cx);
        });

        // For file finder, update the filtered list
        if mode == "file_finder" {
            let query = self
                .stoat
                .read(cx)
                .file_finder_input()
                .map(|buffer| {
                    let buffer_snapshot = buffer.read(cx).snapshot();
                    buffer_snapshot.text()
                })
                .unwrap_or_default();

            self.stoat.update(cx, |stoat, cx| {
                stoat.filter_files(&query, cx);
            });
        }

        cx.notify();
    }

    fn handle_delete_right(
        &mut self,
        _: &DeleteRight,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_right(cx);
        });
        cx.notify();
    }

    fn handle_delete_word_left(
        &mut self,
        _: &DeleteWordLeft,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_word_left(cx);
        });
        cx.notify();
    }

    fn handle_delete_word_right(
        &mut self,
        _: &DeleteWordRight,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_word_right(cx);
        });
        cx.notify();
    }

    fn handle_select_next_symbol(
        &mut self,
        _: &SelectNextSymbol,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_next_symbol(cx);
        });
        cx.notify();
    }

    fn handle_select_prev_symbol(
        &mut self,
        _: &SelectPrevSymbol,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_prev_symbol(cx);
        });
        cx.notify();
    }

    fn handle_select_next_token(
        &mut self,
        _: &SelectNextToken,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_next_token(cx);
        });
        cx.notify();
    }

    fn handle_select_prev_token(
        &mut self,
        _: &SelectPrevToken,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_prev_token(cx);
        });
        cx.notify();
    }

    fn handle_select_left(
        &mut self,
        _: &SelectLeft,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_left(cx);
        });
        cx.notify();
    }

    fn handle_select_right(
        &mut self,
        _: &SelectRight,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_right(cx);
        });
        cx.notify();
    }

    fn handle_select_up(&mut self, _: &SelectUp, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_up(cx);
        });
        cx.notify();
    }

    fn handle_select_down(
        &mut self,
        _: &SelectDown,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_down(cx);
        });
        cx.notify();
    }

    fn handle_select_to_line_start(
        &mut self,
        _: &SelectToLineStart,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_to_line_start(cx);
        });
        cx.notify();
    }

    fn handle_select_to_line_end(
        &mut self,
        _: &SelectToLineEnd,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.select_to_line_end(cx);
        });
        cx.notify();
    }

    fn handle_new_line(&mut self, _: &NewLine, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.new_line(cx);
        });
        cx.notify();
    }

    fn handle_delete_line(
        &mut self,
        _: &DeleteLine,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_line(cx);
        });
        cx.notify();
    }

    fn handle_delete_to_end_of_line(
        &mut self,
        _: &DeleteToEndOfLine,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_to_end_of_line(cx);
        });
        cx.notify();
    }

    fn handle_move_up(&mut self, _: &MoveUp, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_up(cx);
        });
        cx.notify();
    }

    fn handle_move_down(&mut self, _: &MoveDown, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_down(cx);
        });
        cx.notify();
    }

    fn handle_move_left(&mut self, _: &MoveLeft, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_left(cx);
        });
        cx.notify();
    }

    fn handle_move_right(
        &mut self,
        _: &MoveRight,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_right(cx);
        });
        cx.notify();
    }

    fn handle_move_to_line_start(
        &mut self,
        _: &MoveToLineStart,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_to_line_start(cx);
        });
        cx.notify();
    }

    fn handle_move_to_line_end(
        &mut self,
        _: &MoveToLineEnd,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_to_line_end(cx);
        });
        cx.notify();
    }

    fn handle_move_word_left(
        &mut self,
        _: &MoveWordLeft,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_word_left(cx);
        });
        cx.notify();
    }

    fn handle_move_word_right(
        &mut self,
        _: &MoveWordRight,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_word_right(cx);
        });
        cx.notify();
    }

    fn handle_move_to_file_start(
        &mut self,
        _: &MoveToFileStart,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_to_file_start(cx);
        });
        cx.notify();
    }

    fn handle_move_to_file_end(
        &mut self,
        _: &MoveToFileEnd,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.move_to_file_end(cx);
        });
        cx.notify();
    }

    fn handle_page_up(&mut self, _: &PageUp, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.page_up(cx);
        });
        cx.notify();
    }

    fn handle_page_down(&mut self, _: &PageDown, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.page_down(cx);
        });
        cx.notify();
    }

    fn handle_enter_insert_mode(
        &mut self,
        _: &EnterInsertMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.enter_insert_mode(cx);
        });
        cx.notify();
    }

    fn handle_enter_normal_mode(
        &mut self,
        _: &EnterNormalMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.enter_normal_mode(cx);
        });
        cx.notify();
    }

    fn handle_enter_space_mode(
        &mut self,
        _: &EnterSpaceMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.enter_space_mode(cx);
        });
        cx.notify();
    }

    fn handle_enter_pane_mode(
        &mut self,
        _: &EnterPaneMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.enter_pane_mode(cx);
        });
        cx.notify();
    }

    fn handle_file_finder_next(
        &mut self,
        _: &FileFinderNext,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.file_finder_next(cx);
        });

        let _selected = self.stoat.read(cx).file_finder_selected();
        cx.notify();
    }

    fn handle_file_finder_prev(
        &mut self,
        _: &FileFinderPrev,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.file_finder_prev(cx);
        });

        let _selected = self.stoat.read(cx).file_finder_selected();
        cx.notify();
    }

    fn handle_file_finder_select(
        &mut self,
        _: &FileFinderSelect,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.file_finder_select(cx);
        });
        cx.notify();
    }

    fn handle_file_finder_dismiss(
        &mut self,
        _: &FileFinderDismiss,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.file_finder_dismiss(cx);
        });
        cx.notify();
    }

    fn handle_buffer_finder_next(
        &mut self,
        _: &BufferFinderNext,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.buffer_finder_next(cx);
        });
        cx.notify();
    }

    fn handle_buffer_finder_prev(
        &mut self,
        _: &BufferFinderPrev,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.buffer_finder_prev(cx);
        });
        cx.notify();
    }

    fn handle_buffer_finder_select(
        &mut self,
        _: &BufferFinderSelect,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.buffer_finder_select(cx);
        });
        cx.notify();
    }

    fn handle_buffer_finder_dismiss(
        &mut self,
        _: &BufferFinderDismiss,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.buffer_finder_dismiss(cx);
        });
        cx.notify();
    }

    fn handle_git_status_next(
        &mut self,
        _: &GitStatusNext,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.git_status_next(cx);
        });
        cx.notify();
    }

    fn handle_git_status_prev(
        &mut self,
        _: &GitStatusPrev,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.git_status_prev(cx);
        });
        cx.notify();
    }

    fn handle_git_status_select(
        &mut self,
        _: &GitStatusSelect,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.git_status_select(cx);
        });
        cx.notify();
    }

    fn handle_git_status_dismiss(
        &mut self,
        _: &GitStatusDismiss,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.git_status_dismiss(cx);
        });
        cx.notify();
    }

    fn handle_command_palette_next(
        &mut self,
        _: &CommandPaletteNext,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.command_palette_next(cx);
        });

        let _selected = self.stoat.read(cx).command_palette_selected();

        cx.notify();
    }

    fn handle_command_palette_prev(
        &mut self,
        _: &CommandPalettePrev,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.command_palette_prev(cx);
        });

        let _selected = self.stoat.read(cx).command_palette_selected();

        cx.notify();
    }

    fn handle_command_palette_execute(
        &mut self,
        _: &CommandPaletteExecute,
        window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Get the selected command's TypeId
        let type_id = self.stoat.read(cx).command_palette_selected_type_id();

        // Dismiss the command palette first
        self.stoat.update(cx, |stoat, cx| {
            stoat.command_palette_dismiss(cx);
        });

        // Dispatch the selected command
        if let Some(type_id) = type_id {
            crate::dispatch::dispatch_command_by_type_id(type_id, window, cx);
        }

        cx.notify();
    }

    fn handle_command_palette_dismiss(
        &mut self,
        _: &CommandPaletteDismiss,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.command_palette_dismiss(cx);
        });
        cx.notify();
    }

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Handle direct keyboard input in insert, file_finder, buffer_finder, and command_palette
        // modes
        let mode = self.stoat.read(cx).mode().to_string();
        if mode == "insert"
            || mode == "file_finder"
            || mode == "buffer_finder"
            || mode == "command_palette"
        {
            if let Some(key_char) = &event.keystroke.key_char {
                self.stoat.update(cx, |stoat, cx| {
                    stoat.insert_text(key_char, cx);
                });

                // For file finder, update the filtered list
                if mode == "file_finder" {
                    let query = self
                        .stoat
                        .read(cx)
                        .file_finder_input()
                        .map(|buffer| {
                            let buffer_snapshot = buffer.read(cx).snapshot();
                            buffer_snapshot.text()
                        })
                        .unwrap_or_default();

                    self.stoat.update(cx, |stoat, cx| {
                        stoat.filter_files(&query, cx);
                    });
                }

                // For command palette, filtering already happens in insert_text
                // No additional action needed here

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
        let mode = self.stoat.read(cx).mode().to_string();
        let is_minimap = mode == "minimap";
        eprintln!(
            "[PERF] EditorView::render() - is_minimap={}, entity_id={:?}",
            is_minimap,
            self.this.as_ref().map(|e| e.entity_id())
        );

        let view_entity = self
            .this
            .clone()
            .expect("EditorView entity not set - call set_entity() after creation");

        div()
            .id("editor")
            .key_context({
                let mut ctx = gpui::KeyContext::new_with_defaults();
                ctx.add("Editor");
                ctx.set("mode", mode.clone());
                ctx
            })
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::handle_insert_text))
            .on_action(cx.listener(Self::handle_delete_left))
            .on_action(cx.listener(Self::handle_delete_right))
            .on_action(cx.listener(Self::handle_delete_word_left))
            .on_action(cx.listener(Self::handle_delete_word_right))
            .on_action(cx.listener(Self::handle_select_next_symbol))
            .on_action(cx.listener(Self::handle_select_prev_symbol))
            .on_action(cx.listener(Self::handle_select_next_token))
            .on_action(cx.listener(Self::handle_select_prev_token))
            .on_action(cx.listener(Self::handle_select_left))
            .on_action(cx.listener(Self::handle_select_right))
            .on_action(cx.listener(Self::handle_select_up))
            .on_action(cx.listener(Self::handle_select_down))
            .on_action(cx.listener(Self::handle_select_to_line_start))
            .on_action(cx.listener(Self::handle_select_to_line_end))
            .on_action(cx.listener(Self::handle_new_line))
            .on_action(cx.listener(Self::handle_delete_line))
            .on_action(cx.listener(Self::handle_delete_to_end_of_line))
            .on_action(cx.listener(Self::handle_move_up))
            .on_action(cx.listener(Self::handle_move_down))
            .on_action(cx.listener(Self::handle_move_left))
            .on_action(cx.listener(Self::handle_move_right))
            .on_action(cx.listener(Self::handle_move_to_line_start))
            .on_action(cx.listener(Self::handle_move_to_line_end))
            .on_action(cx.listener(Self::handle_move_word_left))
            .on_action(cx.listener(Self::handle_move_word_right))
            .on_action(cx.listener(Self::handle_move_to_file_start))
            .on_action(cx.listener(Self::handle_move_to_file_end))
            .on_action(cx.listener(Self::handle_page_up))
            .on_action(cx.listener(Self::handle_page_down))
            .on_action(cx.listener(Self::handle_enter_insert_mode))
            .on_action(cx.listener(Self::handle_enter_normal_mode))
            .on_action(cx.listener(Self::handle_enter_space_mode))
            .on_action(cx.listener(Self::handle_enter_pane_mode))
            .on_action(cx.listener(Self::handle_file_finder_next))
            .on_action(cx.listener(Self::handle_file_finder_prev))
            .on_action(cx.listener(Self::handle_file_finder_select))
            .on_action(cx.listener(Self::handle_file_finder_dismiss))
            .on_action(cx.listener(Self::handle_buffer_finder_next))
            .on_action(cx.listener(Self::handle_buffer_finder_prev))
            .on_action(cx.listener(Self::handle_buffer_finder_select))
            .on_action(cx.listener(Self::handle_buffer_finder_dismiss))
            .on_action(cx.listener(Self::handle_git_status_next))
            .on_action(cx.listener(Self::handle_git_status_prev))
            .on_action(cx.listener(Self::handle_git_status_select))
            .on_action(cx.listener(Self::handle_git_status_dismiss))
            .on_action(cx.listener(Self::handle_command_palette_next))
            .on_action(cx.listener(Self::handle_command_palette_prev))
            .on_action(cx.listener(Self::handle_command_palette_execute))
            .on_action(cx.listener(Self::handle_command_palette_dismiss))
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
