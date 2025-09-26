use super::element::EditorElement;
use crate::{
    actions::*,
    context::EditorContext,
    input::InputHandler,
    modal::{ModalAction, ModalHandler},
};
use gpui::{
    div, App, Context, FocusHandle, Focusable, InteractiveElement, IntoElement, ParentElement,
    Render, Window,
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

    /// Handle modal key actions
    fn handle_modal_key_i(&mut self, _: &ModalKeyI, _: &mut Window, cx: &mut Context<'_, Self>) {
        debug!("ModalKeyI action triggered");
        self.handle_key("i", cx);
    }
    fn handle_modal_key_escape(
        &mut self,
        _: &ModalKeyEscape,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        debug!("ModalKeyEscape action triggered");
        self.handle_key("escape", cx);
    }
    fn handle_modal_key_a(&mut self, _: &ModalKeyA, _: &mut Window, cx: &mut Context<'_, Self>) {
        debug!("ModalKeyA action triggered");
        self.handle_key("a", cx);
    }
    fn handle_modal_key_b(&mut self, _: &ModalKeyB, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("b", cx);
    }
    fn handle_modal_key_c(&mut self, _: &ModalKeyC, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("c", cx);
    }
    fn handle_modal_key_d(&mut self, _: &ModalKeyD, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("d", cx);
    }
    fn handle_modal_key_e(&mut self, _: &ModalKeyE, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("e", cx);
    }
    fn handle_modal_key_f(&mut self, _: &ModalKeyF, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("f", cx);
    }
    fn handle_modal_key_g(&mut self, _: &ModalKeyG, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("g", cx);
    }
    fn handle_modal_key_h(&mut self, _: &ModalKeyH, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("h", cx);
    }
    fn handle_modal_key_j(&mut self, _: &ModalKeyJ, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("j", cx);
    }
    fn handle_modal_key_k(&mut self, _: &ModalKeyK, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("k", cx);
    }
    fn handle_modal_key_l(&mut self, _: &ModalKeyL, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("l", cx);
    }
    fn handle_modal_key_m(&mut self, _: &ModalKeyM, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("m", cx);
    }
    fn handle_modal_key_n(&mut self, _: &ModalKeyN, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("n", cx);
    }
    fn handle_modal_key_o(&mut self, _: &ModalKeyO, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("o", cx);
    }
    fn handle_modal_key_p(&mut self, _: &ModalKeyP, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("p", cx);
    }
    fn handle_modal_key_q(&mut self, _: &ModalKeyQ, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("q", cx);
    }
    fn handle_modal_key_r(&mut self, _: &ModalKeyR, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("r", cx);
    }
    fn handle_modal_key_s(&mut self, _: &ModalKeyS, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("s", cx);
    }
    fn handle_modal_key_t(&mut self, _: &ModalKeyT, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("t", cx);
    }
    fn handle_modal_key_u(&mut self, _: &ModalKeyU, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("u", cx);
    }
    fn handle_modal_key_v(&mut self, _: &ModalKeyV, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("v", cx);
    }
    fn handle_modal_key_w(&mut self, _: &ModalKeyW, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("w", cx);
    }
    fn handle_modal_key_x(&mut self, _: &ModalKeyX, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("x", cx);
    }
    fn handle_modal_key_y(&mut self, _: &ModalKeyY, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("y", cx);
    }
    fn handle_modal_key_z(&mut self, _: &ModalKeyZ, _: &mut Window, cx: &mut Context<'_, Self>) {
        self.handle_key("z", cx);
    }
    fn handle_modal_key_space(
        &mut self,
        _: &ModalKeySpace,
        _: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        debug!("ModalKeySpace action triggered");
        self.handle_key(" ", cx);
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
    pub fn handle_key(&mut self, key: &str, cx: &mut Context<'_, Self>) -> ModalAction {
        info!("EditorView received key: '{}'", key);
        let action = self.modal_handler.handle_key(key);

        match &action {
            ModalAction::ModeChanged => {
                info!(
                    "Mode changed to: {}",
                    self.modal_handler.current_mode().as_str()
                );
                cx.notify();
            },
            ModalAction::InsertText(text) => {
                info!("Character input: '{}'", text);
                // TODO: Implement actual text insertion into stoat buffer
                println!("Inserting text: {}", text);
                cx.notify();
            },
            ModalAction::Quit => {
                info!("Quit action triggered");
                cx.quit();
            },
            ModalAction::None => {
                debug!("No action taken for key '{}'", key);
            },
        }

        action
    }

    /// Get the current editor mode for display
    pub fn current_mode(&self) -> crate::modal::EditorMode {
        self.modal_handler.current_mode()
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
            .on_action(cx.listener(Self::handle_modal_key_i))
            .on_action(cx.listener(Self::handle_modal_key_escape))
            .on_action(cx.listener(Self::handle_modal_key_a))
            .on_action(cx.listener(Self::handle_modal_key_b))
            .on_action(cx.listener(Self::handle_modal_key_c))
            .on_action(cx.listener(Self::handle_modal_key_d))
            .on_action(cx.listener(Self::handle_modal_key_e))
            .on_action(cx.listener(Self::handle_modal_key_f))
            .on_action(cx.listener(Self::handle_modal_key_g))
            .on_action(cx.listener(Self::handle_modal_key_h))
            .on_action(cx.listener(Self::handle_modal_key_j))
            .on_action(cx.listener(Self::handle_modal_key_k))
            .on_action(cx.listener(Self::handle_modal_key_l))
            .on_action(cx.listener(Self::handle_modal_key_m))
            .on_action(cx.listener(Self::handle_modal_key_n))
            .on_action(cx.listener(Self::handle_modal_key_o))
            .on_action(cx.listener(Self::handle_modal_key_p))
            .on_action(cx.listener(Self::handle_modal_key_q))
            .on_action(cx.listener(Self::handle_modal_key_r))
            .on_action(cx.listener(Self::handle_modal_key_s))
            .on_action(cx.listener(Self::handle_modal_key_t))
            .on_action(cx.listener(Self::handle_modal_key_u))
            .on_action(cx.listener(Self::handle_modal_key_v))
            .on_action(cx.listener(Self::handle_modal_key_w))
            .on_action(cx.listener(Self::handle_modal_key_x))
            .on_action(cx.listener(Self::handle_modal_key_y))
            .on_action(cx.listener(Self::handle_modal_key_z))
            .on_action(cx.listener(Self::handle_modal_key_space))
            .child(EditorElement::new(self.stoat.clone()))
    }
}
