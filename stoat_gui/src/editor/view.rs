use super::element::EditorElement;
use crate::{
    commands::*,
    context::EditorContext,
    input::InputHandler,
    modal::{ModalHandler, ModalResult},
};
use gpui::{
    div, Action, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, Window,
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
        } else {
            debug!("Unhandled command");
        }
    }

    /// Handle text insertion
    fn handle_insert_text(&mut self, command: &InsertText, cx: &mut Context<'_, Self>) {
        info!("Inserting text: '{}'", command.0);
        // TODO: Implement actual text insertion into stoat buffer
        println!("Inserting text: {}", command.0);
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
            // Register command handlers
            .on_action(cx.listener(Self::handle_insert_text_action))
            .on_action(cx.listener(Self::handle_enter_insert_mode_action))
            .on_action(cx.listener(Self::handle_enter_normal_mode_action))
            .on_action(cx.listener(Self::handle_exit_app_action))
            // Handle keyboard input directly for modal system
            .on_key_down(cx.listener(
                |editor: &mut EditorView, event: &gpui::KeyDownEvent, window: &mut Window, cx| {
                    let key_string = event.keystroke.key.to_string();
                    editor.handle_key(&key_string, window, cx);
                },
            ))
            .child(EditorElement::new(self.stoat.clone()))
    }
}
