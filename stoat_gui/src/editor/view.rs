use super::{element::EditorElement, style::EditorStyle};
use crate::{context::EditorContext, input::InputHandler};
use gpui::{
    div, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, ScrollWheelEvent, Styled, Window,
};
use stoat::{actions::*, ScrollDelta, Stoat};
use tracing::info;

pub struct EditorView {
    stoat: Stoat,
    pub input_handler: InputHandler,
    context: EditorContext,
    focus_handle: FocusHandle,
}

impl EditorView {
    pub fn new(stoat: Stoat, cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();

        Self {
            stoat,
            input_handler: InputHandler::with_default_keymap(),
            context: EditorContext::new(),
            focus_handle,
        }
    }

    /// Handle text insertion
    fn handle_insert_text(&mut self, command: &InsertText, cx: &mut Context<'_, Self>) {
        info!("Inserting text: '{}'", command.0);

        // Insert text at cursor position using Stoat's optimized method
        self.stoat.insert_text(&command.0, cx);

        // Notify for re-render
        cx.notify();
    }

    /// Handle entering insert mode
    fn handle_enter_insert_mode(&mut self, cx: &mut Context<'_, Self>) {
        info!("Entering Insert mode");
        self.stoat.set_mode(stoat::EditorMode::Insert);
        cx.notify();
    }

    /// Handle entering normal mode
    fn handle_enter_normal_mode(&mut self, cx: &mut Context<'_, Self>) {
        info!("Entering Normal mode");
        self.stoat.set_mode(stoat::EditorMode::Normal);
        cx.notify();
    }

    /// Handle app exit
    fn handle_exit_app(&mut self, cx: &mut Context<'_, Self>) {
        info!("Exiting application");
        cx.quit();
    }

    /// Update the editor context
    pub fn update_context(&mut self, update: impl FnOnce(&mut EditorContext)) {
        update(&mut self.context);
    }

    /// Get the current context
    pub fn context(&self) -> &EditorContext {
        &self.context
    }

    /// Get the current editor mode for display
    pub fn current_mode(&self) -> stoat::EditorMode {
        self.stoat.mode()
    }

    /// Command handlers for direct action execution
    fn handle_insert_text_action(
        &mut self,
        action: &InsertText,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.handle_insert_text(action, cx);
    }

    fn handle_enter_insert_mode_action(
        &mut self,
        _: &EnterInsertMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.handle_enter_insert_mode(cx);
    }

    fn handle_enter_normal_mode_action(
        &mut self,
        _: &EnterNormalMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.handle_enter_normal_mode(cx);
    }

    fn handle_exit_app_action(
        &mut self,
        _: &ExitApp,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.handle_exit_app(cx);
    }

    /// Movement command handlers
    fn handle_move_left(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.move_cursor_left(cx);
        cx.notify();
    }

    fn handle_move_right(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.move_cursor_right(cx);
        cx.notify();
    }

    fn handle_move_up(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.move_cursor_up(cx);
        cx.notify();
    }

    fn handle_move_down(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.move_cursor_down(cx);
        cx.notify();
    }

    fn handle_move_to_line_start(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.move_cursor_to_line_start();
        cx.notify();
    }

    fn handle_move_to_line_end(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.move_cursor_to_line_end(cx);
        cx.notify();
    }

    fn handle_move_to_file_start(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.move_cursor_to_file_start();
        cx.notify();
    }

    fn handle_move_to_file_end(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.move_cursor_to_file_end(cx);
        cx.notify();
    }

    fn handle_page_up(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.move_cursor_page_up(cx);
        cx.notify();
    }

    fn handle_page_down(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.move_cursor_page_down(cx);
        cx.notify();
    }

    /// Selection command handlers
    fn handle_select_next_symbol(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.select_next_symbol(cx);
        cx.notify();
    }

    fn handle_select_prev_symbol(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.select_prev_symbol(cx);
        cx.notify();
    }

    fn handle_select_next_token(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.select_next_token(cx);
        cx.notify();
    }

    fn handle_select_prev_token(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.select_prev_token(cx);
        cx.notify();
    }

    /// Deletion command handlers
    fn handle_delete_left(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.delete_left(cx);
        cx.notify();
    }

    fn handle_delete_right(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.delete_right(cx);
        cx.notify();
    }

    fn handle_delete_line(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.delete_line(cx);
        cx.notify();
    }

    fn handle_delete_to_end_of_line(&mut self, cx: &mut Context<'_, Self>) {
        self.stoat.delete_to_end_of_line(cx);
        cx.notify();
    }

    /// Handle scroll events from mouse wheel or trackpad
    fn handle_scroll(&mut self, command: &HandleScroll, cx: &mut Context<'_, Self>) {
        // Pass scroll event to Stoat for processing
        self.stoat
            .handle_scroll_event(&command.delta, command.fast_scroll, cx);

        cx.notify();
    }
}

impl Focusable for EditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Update viewport dimensions in Stoat
        let viewport_size = window.viewport_size();
        let style = EditorStyle::default();
        let visible_lines = viewport_size.height.0 / style.line_height.0;
        self.stoat.set_visible_line_count(visible_lines);

        // Update scroll animation and schedule next frame if still animating
        if self.stoat.is_scroll_animating() {
            let still_animating = self.stoat.update_scroll_animation();
            if still_animating {
                // Request next frame for smooth animation
                window.request_animation_frame();
            }
        }

        // Determine mode string for key context
        let mode_str = match self.stoat.mode() {
            stoat::EditorMode::Normal => "normal",
            stoat::EditorMode::Insert => "insert",
            stoat::EditorMode::Visual => "visual",
        };

        // Wrap the editor element in a div that can handle keyboard input
        div()
            .id("editor") // Give it an ID to make it interactive
            .key_context({
                let mut ctx = gpui::KeyContext::new_with_defaults();
                ctx.add("Editor");
                ctx.set("mode", mode_str);
                ctx
            })
            .track_focus(&self.focus_handle) // Make the div focusable
            .size_full() // Take full width and height (100%)
            // Register command handlers
            .on_action(cx.listener(Self::handle_insert_text_action))
            .on_action(cx.listener(Self::handle_enter_insert_mode_action))
            .on_action(cx.listener(Self::handle_enter_normal_mode_action))
            .on_action(cx.listener(Self::handle_exit_app_action))
            // Movement handlers
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &MoveLeft, _window: &mut Window, cx| {
                    editor.handle_move_left(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &MoveRight, _window: &mut Window, cx| {
                    editor.handle_move_right(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &MoveUp, _window: &mut Window, cx| {
                    editor.handle_move_up(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &MoveDown, _window: &mut Window, cx| {
                    editor.handle_move_down(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &MoveToLineStart, _window: &mut Window, cx| {
                    editor.handle_move_to_line_start(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &MoveToLineEnd, _window: &mut Window, cx| {
                    editor.handle_move_to_line_end(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &MoveToFileStart, _window: &mut Window, cx| {
                    editor.handle_move_to_file_start(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &MoveToFileEnd, _window: &mut Window, cx| {
                    editor.handle_move_to_file_end(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &PageUp, _window: &mut Window, cx| {
                    editor.handle_page_up(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &PageDown, _window: &mut Window, cx| {
                    editor.handle_page_down(cx);
                },
            ))
            // Selection handlers
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &SelectNextSymbol, _window: &mut Window, cx| {
                    editor.handle_select_next_symbol(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &SelectPrevSymbol, _window: &mut Window, cx| {
                    editor.handle_select_prev_symbol(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &SelectNextToken, _window: &mut Window, cx| {
                    editor.handle_select_next_token(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &SelectPrevToken, _window: &mut Window, cx| {
                    editor.handle_select_prev_token(cx);
                },
            ))
            // Deletion handlers
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &DeleteLeft, _window: &mut Window, cx| {
                    editor.handle_delete_left(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &DeleteRight, _window: &mut Window, cx| {
                    editor.handle_delete_right(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &DeleteLine, _window: &mut Window, cx| {
                    editor.handle_delete_line(cx);
                },
            ))
            .on_action(cx.listener(
                |editor: &mut EditorView, _: &DeleteToEndOfLine, _window: &mut Window, cx| {
                    editor.handle_delete_to_end_of_line(cx);
                },
            ))
            // Handle text input in insert mode as fallback (when no action matched)
            .on_key_down(cx.listener(
                |editor: &mut EditorView, event: &gpui::KeyDownEvent, _, cx| {
                    // Only insert text in insert mode when no action matched
                    if editor.stoat.mode() == stoat::EditorMode::Insert {
                        if let Some(ref key_char) = event.keystroke.key_char {
                            // Only insert if no control/alt modifiers
                            if !event.keystroke.modifiers.control && !event.keystroke.modifiers.alt
                            {
                                editor.stoat.insert_text(key_char, cx);
                                cx.notify();
                            }
                        }
                    }
                },
            ))
            // Handle scroll wheel events
            .on_scroll_wheel(cx.listener(
                |editor: &mut EditorView, event: &ScrollWheelEvent, _window: &mut Window, cx| {
                    // Convert GPUI's ScrollWheelEvent to our HandleScroll command
                    // Invert Y direction to match standard text editor scroll behavior
                    let delta = match event.delta {
                        gpui::ScrollDelta::Pixels(pixels) => {
                            ScrollDelta::Pixels(gpui::point(pixels.x, -pixels.y))
                        },
                        gpui::ScrollDelta::Lines(lines) => {
                            ScrollDelta::Lines(gpui::point(lines.x, -lines.y))
                        },
                    };

                    let scroll_command = HandleScroll {
                        position: event.position,
                        delta,
                        fast_scroll: event.modifiers.alt,
                    };

                    editor.handle_scroll(&scroll_command, cx);
                },
            ))
            .child(EditorElement::new(self.stoat.clone()))
    }
}
