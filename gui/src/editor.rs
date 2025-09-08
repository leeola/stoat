//! Core editor view implementation for GPUI

use crate::{
    actions::*,
    buffer::Buffer,
    element::EditorElement,
    theme::{EditorTheme, ThemeSettings},
    vim::VimMode,
};
use anyhow::Result;
use gpui::{
    div, prelude::*, px, App, Context, Entity, EventEmitter, FocusHandle, Focusable, KeyBinding,
    ParentElement, Render, SharedString, Styled, Window,
};
use parking_lot::RwLock;
use std::sync::Arc;
use stoat::{EditorEngine, EditorState};

/// The main editor view entity for GPUI
pub struct Editor {
    /// Core editor engine from stoat
    engine: Arc<RwLock<EditorEngine>>,
    /// Text buffer adapter
    buffer: Entity<Buffer>,
    /// Current vim mode state
    vim_mode: VimMode,
    /// Focus handle for keyboard input
    focus_handle: FocusHandle,
    /// Editor theme
    theme: EditorTheme,
    /// Font size in pixels
    font_size: f32,
    /// Line height multiplier
    line_height: f32,
    /// Cursor blink state
    cursor_visible: bool,
    /// Scroll position (line, column)
    scroll_position: (f32, f32),
    /// Viewport dimensions in characters
    viewport_size: (usize, usize),
}

impl Editor {
    pub fn new(initial_text: Option<String>, cx: &mut Context<Self>) -> Self {
        let engine = if let Some(text) = initial_text {
            Arc::new(RwLock::new(EditorEngine::with_text(&text)))
        } else {
            Arc::new(RwLock::new(EditorEngine::new()))
        };
        let buffer = cx.new(|_cx| Buffer::new(engine.clone()));
        let focus_handle = cx.focus_handle();

        // Set up cursor blink timer
        let cursor_visible = true;
        // FIXME: Implement proper cursor blinking with async context

        Self {
            engine,
            buffer,
            vim_mode: VimMode::Normal,
            focus_handle,
            theme: EditorTheme::default(),
            font_size: 14.0,
            line_height: 1.4,
            cursor_visible,
            scroll_position: (0.0, 0.0),
            viewport_size: (80, 24),
        }
    }

    /// Get the current editor state
    pub fn state(&self) -> EditorState {
        self.engine.read().state().clone()
    }

    /// Handle cursor movement
    fn move_cursor(&mut self, direction: MoveDirection, cx: &mut Context<Self>) {
        // For now, just trigger a re-render
        // FIXME: Implement actual cursor movement via stoat engine
        cx.notify();
    }

    /// Enter insert mode
    fn enter_insert_mode(&mut self, cx: &mut Context<Self>) {
        self.vim_mode = VimMode::Insert;
        cx.notify();
    }

    /// Exit to normal mode
    fn escape(&mut self, cx: &mut Context<Self>) {
        self.vim_mode = VimMode::Normal;
        cx.notify();
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state();
        let theme = &self.theme;

        div()
            .key_context("Editor")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(|this, _: &MoveLeft, _window, cx| {
                this.move_cursor(MoveDirection::Left, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveRight, _window, cx| {
                this.move_cursor(MoveDirection::Right, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveUp, _window, cx| {
                this.move_cursor(MoveDirection::Up, cx);
            }))
            .on_action(cx.listener(|this, _: &MoveDown, _window, cx| {
                this.move_cursor(MoveDirection::Down, cx);
            }))
            .on_action(cx.listener(|this, _: &EnterInsertMode, _window, cx| {
                this.enter_insert_mode(cx);
            }))
            .on_action(cx.listener(|this, _: &Escape, _window, cx| {
                this.escape(cx);
            }))
            .bg(theme.background)
            .text_color(theme.foreground)
            .size_full()
            .font_family("JetBrains Mono")
            .child(EditorElement::new(
                state,
                self.vim_mode,
                theme.clone(),
                self.font_size,
                self.line_height,
                self.cursor_visible,
                self.scroll_position,
            ))
    }
}

impl EventEmitter<EditorEvent> for Editor {}

impl Focusable for Editor {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

#[derive(Debug, Clone, Copy)]
pub enum MoveDirection {
    Left,
    Right,
    Up,
    Down,
}

#[derive(Debug, Clone)]
pub enum EditorEvent {
    ContentChanged,
    CursorMoved,
    ModeChanged,
}

/// Register key bindings for the editor
pub fn register_key_bindings(cx: &mut App) {
    // Normal mode bindings
    cx.bind_keys([
        KeyBinding::new("h", MoveLeft, Some("Editor && vim_mode == normal")),
        KeyBinding::new("j", MoveDown, Some("Editor && vim_mode == normal")),
        KeyBinding::new("k", MoveUp, Some("Editor && vim_mode == normal")),
        KeyBinding::new("l", MoveRight, Some("Editor && vim_mode == normal")),
        KeyBinding::new("i", EnterInsertMode, Some("Editor && vim_mode == normal")),
        KeyBinding::new("escape", Escape, Some("Editor")),
    ]);
}
