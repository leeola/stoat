use crate::{editor_element::EditorElement, file_finder::FileFinder};
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, KeyDownEvent,
    ParentElement, Render, ScrollWheelEvent, Styled, Window, div, point, prelude::FluentBuilder,
    rgb,
};
use stoat_v4::{Stoat, actions::*, scroll};

pub struct EditorView {
    pub(crate) stoat: Entity<Stoat>,
    focus_handle: FocusHandle,
    this: Option<Entity<Self>>,
}

impl EditorView {
    pub fn new(stoat: Entity<Stoat>, cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();

        Self {
            stoat,
            focus_handle,
            this: None,
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

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Handle direct keyboard input in insert and file_finder modes
        let mode = self.stoat.read(cx).mode().to_string();
        if mode == "insert" || mode == "file_finder" {
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
                    div.child(FileFinder::new(query, files, selected, preview))
                },
            )
    }
}
