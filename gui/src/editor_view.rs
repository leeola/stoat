//! Main GPUI editor view Entity.
//!
//! This module implements the primary editor view that integrates Stoat's
//! editor engine with GPUI's rendering system.

use crate::{
    buffer_view::{BufferView, RenderedLine},
    components::command_panel::CommandPanel,
    easing,
    stoat_bridge::{process_effects, StoatBridge},
    theme::EditorTheme,
};
use gpui::{
    div, App, AppContext, Context, EventEmitter, FocusHandle, Focusable, InteractiveElement,
    IntoElement, Keystroke, ParentElement, Render, SharedString, Styled, Window,
};
use std::time::{Duration, Instant};

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
    /// Target viewport scroll offset in lines (where we want to scroll to)
    target_scroll_y: f32,
    /// Current animated scroll position in lines
    animated_scroll_y: f32,
    /// Logical scroll position (the actual state)
    scroll_y: f32,
    /// Viewport scroll offset in characters
    scroll_x: f32,
    /// Animation state
    scroll_animation_start: Option<Instant>,
    scroll_animation_duration: Duration,
    scroll_animation_from: f32,
    /// Whether animation is currently running
    animation_running: bool,
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
            target_scroll_y: 0.0,
            animated_scroll_y: 0.0,
            scroll_y: 0.0,
            scroll_x: 0.0,
            scroll_animation_start: None,
            scroll_animation_duration: Duration::from_millis(250),
            scroll_animation_from: 0.0,
            animation_running: false,
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
                    // Update viewport scroll offsets (temporarily without animation)
                    self.scroll_x = scroll_x;
                    self.scroll_y = scroll_y;

                    // For now, jump directly to the target position
                    self.animated_scroll_y = scroll_y;
                    self.target_scroll_y = scroll_y;

                    tracing::debug!(
                        "Viewport update: scroll_x={}, scroll_y={}, animated_y={}",
                        scroll_x,
                        scroll_y,
                        self.animated_scroll_y
                    );

                    // Request re-render
                    cx.notify();
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

    /// Starts the scroll animation loop.
    fn start_scroll_animation(&mut self, cx: &mut Context<'_, Self>) {
        // If already animating to the same target, let it continue
        if self.animation_running && (self.target_scroll_y - self.scroll_y).abs() < 0.01 {
            return;
        }

        // Mark as running
        self.animation_running = true;

        // Request first render frame to start animation
        cx.notify();
    }

    /// Advances the animation by one frame.
    fn tick_animation(&mut self, cx: &mut Context<'_, Self>) {
        if let Some(start_time) = self.scroll_animation_start {
            let elapsed = Instant::now().duration_since(start_time);
            let progress = easing::progress(elapsed, self.scroll_animation_duration);

            // Use the stored starting position
            let from = self.scroll_animation_from;
            let to = self.target_scroll_y;

            // Apply easing and interpolate
            let eased_position = easing::interpolate(from, to, progress, easing::ease_out_cubic);

            // Update animated position
            self.animated_scroll_y = eased_position;

            // Check if animation is complete
            if progress >= 1.0 {
                self.animated_scroll_y = self.target_scroll_y;
                self.scroll_animation_start = None;
                self.animation_running = false;
            } else {
                // Request another render frame to continue animation
                // This ensures the animation continues at ~60fps
                cx.notify();
            }
        }
    }

    /// Updates the viewport based on scroll position.
    fn update_viewport(&mut self, _window: &Window) {
        // Use the dynamically calculated visible lines
        let viewport_height = self.visible_lines.ceil() as usize;

        // Use animated scroll position for viewport calculation
        let scroll_line = self.animated_scroll_y.floor().max(0.0) as usize;

        // Set viewport - BufferView will add its own overscan
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

        // Send resize event to Stoat with line_height
        let effects = self.bridge.handle_event(stoat::EditorEvent::Resize {
            width: 1200.0, // TODO: Get actual width
            height: viewport_height_px,
            line_height: self.line_height,
        });

        // Process any effects
        for effect in effects {
            match effect {
                stoat::Effect::ViewportUpdate { scroll_x, scroll_y } => {
                    // For resize events, jump directly without animation
                    self.scroll_x = scroll_x;
                    self.scroll_y = scroll_y;
                    self.target_scroll_y = scroll_y;
                    self.animated_scroll_y = scroll_y;
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

        // Calculate the pixel offset for scrolling
        // The BufferView fetches lines with overscan, so we need to account for that
        // when calculating the scroll offset.

        // The BufferView returns lines starting from (viewport.start - overscan)
        // So if viewport starts at line 85 and overscan is 10, we get lines 75-105
        // We need to offset the rendering to show line 85 at the top of the viewport

        let overscan_lines = self.buffer_view.overscan() as f32;

        // Calculate the offset needed to position the viewport correctly
        // Use animated scroll position for smooth transitions
        let viewport_start = self.animated_scroll_y.floor().max(0.0);

        // BufferView returns lines starting from (viewport_start - overscan)
        // We need to offset by the difference between where we want to display
        // and where BufferView starts
        let buffer_start_line = (viewport_start - overscan_lines).max(0.0);
        let offset_lines = self.animated_scroll_y - buffer_start_line;

        let scroll_offset_px = offset_lines * self.line_height;

        tracing::trace!(
            "Scroll render: animated_y={}, viewport_start={}, overscan={}, offset_lines={}, offset_px={}",
            self.animated_scroll_y,
            viewport_start,
            overscan_lines,
            offset_lines,
            scroll_offset_px
        );

        // Container for the lines with scroll offset applied
        div().relative().child(
            div()
                .absolute()
                .top(gpui::px(-scroll_offset_px))
                .flex()
                .flex_col()
                .gap_0()
                .children(lines.into_iter().map(|line| self.render_line(line))),
        )
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
        // Animation temporarily disabled for debugging
        // if self.animation_running {
        //     self.tick_animation(cx);
        // }

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
