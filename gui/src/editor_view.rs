//! Main GPUI editor view Entity.
//!
//! This module implements the primary editor view that integrates Stoat's
//! editor engine with GPUI's rendering system.

use crate::{
    buffer_view::{BufferView, RenderedLine},
    components::help_small::HelpSmall,
    stoat_bridge::{process_effects, StoatBridge},
    theme::EditorTheme,
};
use gpui::{
    div, App, AppContext, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, Keystroke, ParentElement, Render, SharedString, Styled, Window,
};

/// Main editor view Entity for GPUI.
pub struct EditorView {
    /// Focus handle for keyboard input
    focus_handle: FocusHandle,
    /// Bridge to Stoat editor engine
    bridge: StoatBridge,
    /// Buffer view for efficient rendering
    buffer_view: BufferView,
    /// Editor theme
    theme: EditorTheme,
    /// Font settings
    font_family: SharedString,
    font_size: f32,
    line_height: f32,
    /// Whether to show help dialog
    show_help: bool,
}

impl EditorView {
    /// Creates a new editor view.
    pub fn new(cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();

        Self {
            focus_handle,
            bridge: StoatBridge::new(),
            buffer_view: BufferView::new(),
            theme: EditorTheme::default(),
            font_family: "JetBrains Mono".into(),
            font_size: 14.0,
            line_height: 20.0,
            show_help: false,
        }
    }

    /// Creates a new editor view with initial text.
    pub fn with_text(text: &str, cx: &mut Context<'_, Self>) -> Self {
        let focus_handle = cx.focus_handle();

        Self {
            focus_handle,
            bridge: StoatBridge::with_text(text),
            buffer_view: BufferView::new(),
            theme: EditorTheme::default(),
            font_family: "JetBrains Mono".into(),
            font_size: 14.0,
            line_height: 20.0,
            show_help: true,
        }
    }

    /// Handles a keystroke event.
    pub fn handle_keystroke(
        &mut self,
        keystroke: &Keystroke,
        _window: &mut Window,
        cx: &mut Context<'_, Self>,
    ) {
        tracing::debug!("EditorView handling keystroke: {:?}", keystroke);

        // Process the keystroke through Stoat
        let effects = self.bridge.handle_keystroke(keystroke);

        // Handle any effects
        if !effects.is_empty() {
            cx.spawn(async move |_handle, _cx| {
                if let Err(e) = process_effects(effects).await {
                    tracing::error!("Failed to process effects: {}", e);
                }
            })
            .detach();
        }

        // Invalidate buffer cache for changed lines (simplified for now)
        // TODO: Track actual changed lines from the engine
        self.buffer_view.invalidate_all();

        // Notify GPUI to re-render
        cx.notify();

        // Emit event for status updates
        cx.emit(EditorEvent::StateChanged);
    }

    /// Returns the current text content.
    pub fn text(&self) -> String {
        self.bridge.text()
    }

    /// Returns the current cursor position.
    pub fn cursor_position(&self) -> (usize, usize) {
        self.bridge.cursor_position()
    }

    /// Returns the current editing mode.
    pub fn mode(&self) -> String {
        self.bridge.mode()
    }

    /// Returns whether the buffer has unsaved changes.
    pub fn is_dirty(&self) -> bool {
        self.bridge.is_dirty()
    }

    /// Updates the viewport based on scroll position.
    fn update_viewport(&mut self, _window: &Window) {
        // Calculate visible lines based on window size and scroll position
        // For now, use a fixed viewport
        let viewport_height = 30; // TODO: Calculate from actual window height
        let scroll_line = 0; // TODO: Track scroll position

        self.buffer_view
            .set_viewport(scroll_line, scroll_line + viewport_height);
    }

    /// Renders the buffer lines.
    fn render_buffer_lines(&mut self, _window: &mut Window) -> impl IntoElement {
        let state = self.bridge.engine.state();
        let lines = self.buffer_view.visible_lines(state);

        div()
            .flex()
            .flex_col()
            .gap_0()
            .children(lines.into_iter().map(|line| self.render_line(line)))
    }

    /// Renders a single line.
    fn render_line(&self, line: RenderedLine) -> impl IntoElement {
        div()
            .flex()
            .flex_row()
            .h(gpui::px(self.line_height))
            .child(
                // Line number gutter
                div()
                    .w(gpui::px(50.0))
                    .px_2()
                    .text_color(self.theme.line_number)
                    .child(SharedString::from(format!("{:4}", line.line_number + 1))),
            )
            .child(
                // Line content
                div().flex_1().child(line.styled_text),
            )
    }

    /// Renders the status bar.
    fn render_status_bar(&self, _window: &mut Window) -> impl IntoElement {
        let (line, col) = self.cursor_position();
        let mode = self.mode();
        let dirty = if self.is_dirty() { " [+]" } else { "" };

        div()
            .flex()
            .flex_row()
            .justify_between()
            .h(gpui::px(24.0))
            .px_3()
            .bg(self.theme.status_bar_bg)
            .text_color(self.theme.status_bar_fg)
            .border_t_1()
            .border_color(self.theme.line_number)
            .child(
                // Left side: mode
                div().child(SharedString::from(format!("-- {} --", mode))),
            )
            .child(
                // Right side: position and dirty flag
                div().child(SharedString::from(format!(
                    "{}:{}{}",
                    line + 1,
                    col + 1,
                    dirty
                ))),
            )
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Update viewport based on current window size
        self.update_viewport(window);

        let main_view = div()
            .key_context("EditorView")
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(|this, event: &gpui::KeyDownEvent, window, cx| {
                this.handle_keystroke(&event.keystroke, window, cx);
            }))
            .bg(self.theme.background)
            .text_color(self.theme.foreground)
            .size_full()
            .font_family(self.font_family.clone())
            .text_size(gpui::px(self.font_size))
            .flex()
            .flex_col()
            .child(
                // Main editor area
                div()
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_buffer_lines(window)),
            )
            .child(
                // Status bar
                self.render_status_bar(window),
            );

        // Render with the small help popup (always shown for design iteration)
        div()
            .relative()
            .size_full()
            .child(main_view)
            .child(cx.new(|_cx| HelpSmall::new(self.theme.clone())))
    }
}

impl EventEmitter<EditorEvent> for EditorView {}

impl Focusable for EditorView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

/// Events emitted by the editor view.
#[derive(Debug, Clone)]
pub enum EditorEvent {
    /// Editor state has changed
    StateChanged,
    /// Text content has changed
    TextChanged,
    /// Cursor position has changed
    CursorMoved,
    /// Mode has changed
    ModeChanged,
    /// File saved
    FileSaved,
}
