//! Main GPUI editor view Entity.
//!
//! This module implements the primary editor view that integrates Stoat's
//! editor engine with GPUI's rendering system.

use crate::{
    buffer_view::{BufferView, RenderedLine},
    components::command_panel::CommandPanel,
    stoat_bridge::{StoatBridge, process_effects},
    theme::EditorTheme,
};
use gpui::{
    App, AppContext, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, Keystroke, ParentElement, Render, SharedString, Styled, Window, div,
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
    /// Current mode for help display
    help_mode: String,
    /// Available commands for help display
    help_commands: Vec<(String, String)>,
    /// Viewport scroll offset in lines
    scroll_y: f32,
    /// Viewport scroll offset in characters
    scroll_x: f32,
    /// Actual viewport height in pixels
    viewport_height_px: f32,
    /// Visible line count based on actual viewport
    visible_lines: f32,
}

impl EditorView {
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
            show_help: false,
            help_mode: "Normal".to_string(),
            help_commands: vec![],
            scroll_y: 0.0,
            scroll_x: 0.0,
            viewport_height_px: 776.0, // 800px window - 24px status bar
            visible_lines: 38.0,       // 776px / 20px line height
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

        // Handle effects
        for effect in effects {
            match effect {
                stoat::Effect::ShowHelp {
                    visible,
                    mode,
                    commands,
                } => {
                    // Update help state from the effect
                    self.show_help = visible;
                    self.help_mode = mode;
                    self.help_commands = commands;
                    tracing::debug!(
                        "Updated help state: visible={}, mode={}",
                        visible,
                        self.help_mode
                    );
                },
                stoat::Effect::CommandContextChanged { mode, commands } => {
                    // Update command panel content when context changes
                    self.help_mode = mode;
                    self.help_commands = commands;
                    tracing::debug!(
                        "Updated command context: mode={}, {} commands",
                        self.help_mode,
                        self.help_commands.len()
                    );
                },
                stoat::Effect::ViewportUpdate { scroll_x, scroll_y } => {
                    // Update viewport scroll offsets
                    self.scroll_x = scroll_x;
                    self.scroll_y = scroll_y;
                    tracing::debug!(
                        "Updated viewport: scroll_x={}, scroll_y={}",
                        scroll_x,
                        scroll_y
                    );
                },
                // Handle other effects asynchronously
                other_effect => {
                    cx.spawn(async move |_handle, _cx| {
                        if let Err(e) = process_effects(vec![other_effect]).await {
                            tracing::error!("Failed to process effect: {}", e);
                        }
                    })
                    .detach();
                },
            }
        }

        // Invalidate buffer cache for changed lines (simplified for now)
        // TODO: Track actual changed lines from the engine
        self.buffer_view.invalidate_all();

        // Notify GPUI to re-render
        cx.notify();

        // Emit event for status updates
        cx.emit(EditorEvent::StateChanged);
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
        // Use the dynamically calculated visible lines
        let viewport_height = self.visible_lines.ceil() as usize;
        let scroll_line = self.scroll_y.floor() as usize;

        self.buffer_view
            .set_viewport(scroll_line, scroll_line + viewport_height);
    }

    /// Updates the visible line count based on actual viewport height.
    fn update_visible_lines(&mut self, viewport_height_px: f32, cx: &mut Context<'_, Self>) {
        self.viewport_height_px = viewport_height_px;
        self.visible_lines = (viewport_height_px / self.line_height).floor();
        tracing::debug!(
            "Updated visible lines: height_px={}, line_height={}, visible_lines={}",
            viewport_height_px,
            self.line_height,
            self.visible_lines
        );

        // Send resize event to Stoat
        let effects = self.bridge.handle_event(stoat::EditorEvent::Resize {
            width: 1200.0, // TODO: Get actual width
            height: viewport_height_px,
        });

        // Process any effects
        for effect in effects {
            match effect {
                stoat::Effect::ViewportUpdate { scroll_x, scroll_y } => {
                    self.scroll_x = scroll_x;
                    self.scroll_y = scroll_y;
                    tracing::debug!("Viewport updated from resize: scroll_y={}", scroll_y);
                },
                other => {
                    cx.spawn(async move |_handle, _cx| {
                        if let Err(e) = process_effects(vec![other]).await {
                            tracing::error!("Failed to process effect: {}", e);
                        }
                    })
                    .detach();
                },
            }
        }
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
                div().child(SharedString::from(format!("-- {mode} --"))),
            )
            .child(
                // Right side: position and dirty flag
                div().child(SharedString::from(format!(
                    "{line}:{col}{dirty}",
                    line = line + 1,
                    col = col + 1
                ))),
            )
    }
}

impl Render for EditorView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        // Calculate available height for editor (window height minus status bar)
        // This is a simplified approach - ideally we'd measure the actual element
        let status_bar_height = 24.0; // Height of status bar in pixels
        let window_height = 800.0; // TODO: Get actual window height from GPUI
        let available_height = window_height - status_bar_height; // 776px

        // Update visible lines if height changed significantly
        if (available_height - self.viewport_height_px).abs() > 1.0 {
            self.update_visible_lines(available_height, cx);
        }

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
                    .id("editor_area")
                    .flex_1()
                    .overflow_hidden()
                    .child(self.render_buffer_lines(window)),
            )
            .child(
                // Status bar
                self.render_status_bar(window),
            );

        // Conditionally render with the help popup based on state
        let container = div().relative().size_full().child(main_view);

        if self.show_help {
            container.child(cx.new(|_cx| {
                CommandPanel::new(
                    self.theme.clone(),
                    self.help_mode.clone(),
                    self.help_commands.clone(),
                )
            }))
        } else {
            container
        }
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
}
