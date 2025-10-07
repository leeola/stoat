use crate::editor_element::EditorElement;
use gpui::{
    div, rgb, App, Context, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Render, Styled, Window,
};
use stoat_v4::{actions::*, Stoat};

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

    fn handle_key_down(
        &mut self,
        event: &KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        // Only handle direct keyboard input in insert mode
        let mode = self.stoat.read(cx).mode().to_string();
        if mode == "insert" {
            if let Some(key_char) = &event.keystroke.key_char {
                self.stoat.update(cx, |stoat, cx| {
                    stoat.insert_text(key_char, cx);
                });
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
            .on_action(cx.listener(Self::handle_move_up))
            .on_action(cx.listener(Self::handle_move_down))
            .on_action(cx.listener(Self::handle_move_left))
            .on_action(cx.listener(Self::handle_move_right))
            .on_action(cx.listener(Self::handle_enter_insert_mode))
            .on_action(cx.listener(Self::handle_enter_normal_mode))
            .on_key_down(cx.listener(Self::handle_key_down))
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
    }
}
