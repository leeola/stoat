use crate::{
    command_palette::CommandPalette, editor_element::EditorElement, file_finder::FileFinder,
};
use gpui::{
    div, point, prelude::FluentBuilder, rgb, App, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, KeyDownEvent, ParentElement, Render, ScrollHandle,
    ScrollWheelEvent, Styled, Window,
};
use stoat_v4::{actions::*, scroll, Stoat};

pub struct EditorView {
    pub(crate) stoat: Entity<Stoat>,
    focus_handle: FocusHandle,
    this: Option<Entity<Self>>,
    keymap: gpui::Keymap,
    file_finder_scroll: ScrollHandle,
    command_palette_scroll: ScrollHandle,
}

impl EditorView {
    pub fn new(stoat: Entity<Stoat>, cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();
        let keymap = stoat_v4::keymap::create_default_keymap();

        Self {
            stoat,
            focus_handle,
            this: None,
            keymap,
            file_finder_scroll: ScrollHandle::new(),
            command_palette_scroll: ScrollHandle::new(),
        }
    }

    pub fn set_entity(&mut self, entity: Entity<Self>) {
        self.this = Some(entity);
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

    fn handle_new_line(&mut self, _: &NewLine, _window: &mut Window, cx: &mut Context<'_, Self>) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.new_line(cx);
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

    fn handle_open_file_finder(
        &mut self,
        _: &OpenFileFinder,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.open_file_finder(cx);
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

        let selected = self.stoat.read(cx).file_finder_selected();
        self.file_finder_scroll.scroll_to_item(selected);

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

        let selected = self.stoat.read(cx).file_finder_selected();
        self.file_finder_scroll.scroll_to_item(selected);

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

    fn handle_open_command_palette(
        &mut self,
        _: &OpenCommandPalette,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.stoat.update(cx, |stoat, cx| {
            stoat.open_command_palette(&self.keymap, cx);
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

        let selected = self.stoat.read(cx).command_palette_selected();
        self.command_palette_scroll.scroll_to_item(selected);

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

        let selected = self.stoat.read(cx).command_palette_selected();
        self.command_palette_scroll.scroll_to_item(selected);

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
        // Handle direct keyboard input in insert, file_finder, and command_palette modes
        let mode = self.stoat.read(cx).mode().to_string();
        if mode == "insert" || mode == "file_finder" || mode == "command_palette" {
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
        let view_entity = self
            .this
            .clone()
            .expect("EditorView entity not set - call set_entity() after creation");

        // Clone scroll handles for use in closures
        let file_finder_scroll = self.file_finder_scroll.clone();
        let command_palette_scroll = self.command_palette_scroll.clone();

        // Gather file finder data if in file_finder mode
        let file_finder_data = if mode == "file_finder" {
            let stoat = self.stoat.read(cx);
            let query = stoat
                .file_finder_input()
                .map(|buffer| {
                    let buffer_snapshot = buffer.read(cx).snapshot();
                    buffer_snapshot.text()
                })
                .unwrap_or_default();
            let files = stoat.file_finder_filtered().to_vec();
            let selected = stoat.file_finder_selected();
            let preview = stoat.file_finder_preview().cloned();
            Some((query, files, selected, preview))
        } else {
            None
        };

        // Gather command palette data if in command_palette mode
        let command_palette_data = if mode == "command_palette" {
            let stoat = self.stoat.read(cx);
            let query = stoat
                .command_palette_input()
                .map(|buffer| {
                    let buffer_snapshot = buffer.read(cx).snapshot();
                    buffer_snapshot.text()
                })
                .unwrap_or_default();
            let commands = stoat.command_palette_filtered().to_vec();
            let selected = stoat.command_palette_selected();
            Some((query, commands, selected))
        } else {
            None
        };

        // Format mode indicator vim-style
        let mode_text = match mode.as_str() {
            "insert" => "-- INSERT --".to_string(),
            "normal" => "-- NORMAL --".to_string(),
            _ => format!("-- {} --", mode.to_uppercase()),
        };

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
            .on_action(cx.listener(Self::handle_new_line))
            .on_action(cx.listener(Self::handle_move_up))
            .on_action(cx.listener(Self::handle_move_down))
            .on_action(cx.listener(Self::handle_move_left))
            .on_action(cx.listener(Self::handle_move_right))
            .on_action(cx.listener(Self::handle_move_to_line_start))
            .on_action(cx.listener(Self::handle_move_to_line_end))
            .on_action(cx.listener(Self::handle_enter_insert_mode))
            .on_action(cx.listener(Self::handle_enter_normal_mode))
            .on_action(cx.listener(Self::handle_enter_space_mode))
            .on_action(cx.listener(Self::handle_enter_pane_mode))
            .on_action(cx.listener(Self::handle_open_file_finder))
            .on_action(cx.listener(Self::handle_file_finder_next))
            .on_action(cx.listener(Self::handle_file_finder_prev))
            .on_action(cx.listener(Self::handle_file_finder_select))
            .on_action(cx.listener(Self::handle_file_finder_dismiss))
            .on_action(cx.listener(Self::handle_open_command_palette))
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
            .size_full()
            .relative()
            .child(EditorElement::new(view_entity))
            .child(
                div()
                    .absolute()
                    .bottom_0()
                    .right_0()
                    .p_2()
                    .text_xs()
                    .text_color(rgb(0xcccccc))
                    .bg(rgb(0x2a2a2a))
                    .child(mode_text),
            )
            .when_some(
                file_finder_data,
                |div, (query, files, selected, preview)| {
                    div.child(FileFinder::new(
                        query,
                        files,
                        selected,
                        preview,
                        file_finder_scroll.clone(),
                    ))
                },
            )
            .when_some(command_palette_data, |div, (query, commands, selected)| {
                div.child(CommandPalette::new(
                    query,
                    commands,
                    selected,
                    command_palette_scroll.clone(),
                ))
            })
    }
}
