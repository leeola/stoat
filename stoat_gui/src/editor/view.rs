use super::element::EditorElement;
use crate::{
    context::EditorContext,
    input::InputHandler,
    keymap::{TestActionA, TestActionCmdS, TestActionEscape},
};
use gpui::{
    div, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, Window,
};
use stoat::Stoat;

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

    /// Handle test actions
    fn handle_test_action_a(
        &mut self,
        _action: &TestActionA,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        println!("Test action A received!");
        cx.notify();
    }

    fn handle_test_action_cmd_s(
        &mut self,
        _action: &TestActionCmdS,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        println!("Test action Cmd+S received!");
        cx.notify();
    }

    fn handle_test_action_escape(
        &mut self,
        _action: &TestActionEscape,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        println!("Test action Escape received!");
        cx.notify();
    }

    /// Update the editor context
    pub fn update_context(&mut self, update: impl FnOnce(&mut EditorContext)) {
        update(&mut self.context);
    }

    /// Get the current context
    pub fn context(&self) -> &EditorContext {
        &self.context
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
            .on_action(cx.listener(Self::handle_test_action_a))
            .on_action(cx.listener(Self::handle_test_action_cmd_s))
            .on_action(cx.listener(Self::handle_test_action_escape))
            .child(EditorElement::new(self.stoat.clone()))
    }
}
