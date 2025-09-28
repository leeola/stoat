use super::element::EditorElement;
use crate::{
    commands::*,
    context::EditorContext,
    input::InputHandler,
    modal::{ModalHandler, ModalResult},
};
use gpui::{
    div, Action, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Window,
};
use stoat::Stoat;
use tracing::{debug, info};

pub struct EditorView {
    stoat: Stoat,
    pub input_handler: InputHandler,
    context: EditorContext,
    focus_handle: FocusHandle,
    modal_handler: ModalHandler,
}

impl EditorView {
    pub fn new(stoat: Stoat, cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();

        Self {
            stoat,
            input_handler: InputHandler::with_default_keymap(),
            context: EditorContext::new(),
            focus_handle,
            modal_handler: ModalHandler::new(),
        }
    }

    /// Handle command execution from the modal system or keybindings
    fn execute_command(
        &mut self,
        command: Box<dyn Action>,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        info!("Executing command");

        // Handle specific command types
        if let Some(insert_text) = command.as_any().downcast_ref::<InsertText>() {
            self.handle_insert_text(insert_text, cx);
        } else if command.as_any().downcast_ref::<EnterInsertMode>().is_some() {
            self.handle_enter_insert_mode(cx);
        } else if command.as_any().downcast_ref::<EnterNormalMode>().is_some() {
            self.handle_enter_normal_mode(cx);
        } else if command.as_any().downcast_ref::<ExitApp>().is_some() {
            self.handle_exit_app(cx);
        } else if command.as_any().downcast_ref::<MoveLeft>().is_some() {
            self.handle_move_left(cx);
        } else if command.as_any().downcast_ref::<MoveRight>().is_some() {
            self.handle_move_right(cx);
        } else if command.as_any().downcast_ref::<MoveUp>().is_some() {
            self.handle_move_up(cx);
        } else if command.as_any().downcast_ref::<MoveDown>().is_some() {
            self.handle_move_down(cx);
        } else if command.as_any().downcast_ref::<MoveToLineStart>().is_some() {
            self.handle_move_to_line_start(cx);
        } else if command.as_any().downcast_ref::<MoveToLineEnd>().is_some() {
            self.handle_move_to_line_end(cx);
        } else if command.as_any().downcast_ref::<MoveToFileStart>().is_some() {
            self.handle_move_to_file_start(cx);
        } else if command.as_any().downcast_ref::<MoveToFileEnd>().is_some() {
            self.handle_move_to_file_end(cx);
        } else if command.as_any().downcast_ref::<PageUp>().is_some() {
            self.handle_page_up(cx);
        } else if command.as_any().downcast_ref::<PageDown>().is_some() {
            self.handle_page_down(cx);
        } else if command.as_any().downcast_ref::<DeleteLeft>().is_some() {
            self.handle_delete_left(cx);
        } else if command.as_any().downcast_ref::<DeleteRight>().is_some() {
            self.handle_delete_right(cx);
        } else if command.as_any().downcast_ref::<DeleteLine>().is_some() {
            self.handle_delete_line(cx);
        } else if command
            .as_any()
            .downcast_ref::<DeleteToEndOfLine>()
            .is_some()
        {
            self.handle_delete_to_end_of_line(cx);
        } else {
            debug!("Unhandled command");
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
        // Mode change is already handled by modal system
        cx.notify();
    }

    /// Handle entering normal mode
    fn handle_enter_normal_mode(&mut self, cx: &mut Context<'_, Self>) {
        info!("Entering Normal mode");
        // Mode change is already handled by modal system
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

    /// Handle a key press through the modal system
    pub fn handle_key(&mut self, key: &str, window: &mut Window, cx: &mut Context<'_, Self>) {
        info!("EditorView received key: '{}'", key);
        let result = self.modal_handler.handle_key(key);

        match result {
            ModalResult::Command(command) => {
                self.execute_command(command, window, cx);
            },
            ModalResult::None => {
                debug!("No command generated for key '{}'", key);
            },
        }
    }

    /// Get the current editor mode for display
    pub fn current_mode(&self) -> crate::modal::EditorMode {
        self.modal_handler.current_mode()
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
        self.modal_handler.switch_to_insert();
        self.handle_enter_insert_mode(cx);
    }

    fn handle_enter_normal_mode_action(
        &mut self,
        _: &EnterNormalMode,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        self.modal_handler.switch_to_normal();
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
        // FIXME: Need to get viewport height from element - using estimated values for now
        let viewport_height = 600.0; // Approximate viewport height in pixels
        let line_height = 20.0; // Approximate line height in pixels
        self.stoat
            .move_cursor_page_up(cx, viewport_height, line_height);
        cx.notify();
    }

    fn handle_page_down(&mut self, cx: &mut Context<'_, Self>) {
        // FIXME: Need to get viewport height from element - using estimated values for now
        let viewport_height = 600.0; // Approximate viewport height in pixels
        let line_height = 20.0; // Approximate line height in pixels
        self.stoat
            .move_cursor_page_down(cx, viewport_height, line_height);
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
}

impl Focusable for EditorView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Focus the editor handle
        window.focus(&self.focus_handle);

        // Wrap the editor element in a div that can handle keyboard input
        div()
            .id("editor") // Give it an ID to make it interactive
            .key_context("EditorView") // Set key context for action dispatch
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
            // Handle keyboard input directly for modal system
            .on_key_down(cx.listener(
                |editor: &mut EditorView, event: &gpui::KeyDownEvent, window: &mut Window, cx| {
                    // Build key string with modifiers
                    let mut key_string = String::new();

                    // Add modifier prefixes
                    if event.keystroke.modifiers.control {
                        key_string.push_str("ctrl-");
                    }
                    if event.keystroke.modifiers.alt {
                        key_string.push_str("alt-");
                    }
                    if event.keystroke.modifiers.shift && event.keystroke.key_char.is_none() {
                        // Only add shift prefix for non-character keys
                        key_string.push_str("shift-");
                    }

                    // Add the key itself
                    if let Some(ref key_char) = event.keystroke.key_char {
                        key_string.push_str(key_char);
                    } else {
                        key_string.push_str(&event.keystroke.key);
                    }

                    editor.handle_key(&key_string, window, cx);
                },
            ))
            .child(EditorElement::new(self.stoat.clone()))
    }
}
