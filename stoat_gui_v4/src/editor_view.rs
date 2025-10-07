use gpui::{
    div, white, App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    ParentElement, Render, Styled, Window,
};
use stoat_v4::{actions::*, Stoat};

pub struct EditorView {
    stoat: Entity<Stoat>,
    focus_handle: FocusHandle,
}

impl EditorView {
    pub fn new(stoat: Entity<Stoat>, cx: &mut Context<Self>) -> Self {
        let focus_handle = cx.focus_handle();

        Self {
            stoat,
            focus_handle,
        }
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
        self.stoat.update(cx, |stoat, cx| {
            stoat.delete_left(cx);
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
}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for EditorView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self
            .stoat
            .read(cx)
            .buffer_item()
            .read(cx)
            .buffer()
            .read(cx)
            .text();

        div()
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::handle_insert_text))
            .on_action(cx.listener(Self::handle_delete_left))
            .on_action(cx.listener(Self::handle_move_up))
            .on_action(cx.listener(Self::handle_move_down))
            .on_action(cx.listener(Self::handle_move_left))
            .on_action(cx.listener(Self::handle_move_right))
            .on_action(cx.listener(Self::handle_enter_insert_mode))
            .on_action(cx.listener(Self::handle_enter_normal_mode))
            .bg(white())
            .size_full()
            .child(div().child(text))
    }
}
