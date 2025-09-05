//! Main custom text editor widget implementation.
//!
//! This module implements the iced advanced Widget trait, coordinating
//! between the buffer, layout, renderer, and event handler.

use super::{
    buffer::TextBuffer,
    cache::{GlyphCache, LayoutCache},
    event_handler::EventHandler,
    layout::EditorLayout,
    renderer::EditorRenderer,
};
use crate::{messages::Message, theme::EditorTheme};
use cosmic_text::Metrics;
use iced::{
    advanced::{
        layout::{self, Layout},
        renderer::{self},
        widget::{self, Tree, Widget},
        Clipboard, Shell,
    },
    event::{self, Event},
    mouse, Element, Length, Rectangle, Size, Theme,
};
use stoat::EditorState;

/// Custom text editor widget with cosmic-text integration
pub struct CustomTextEditor<'a> {
    /// Editor state from the engine
    state: &'a EditorState,
    /// Visual theme
    theme: &'a EditorTheme,
    /// Layout manager
    layout: EditorLayout,
    /// Event callback
    on_input: Option<Box<dyn Fn(stoat::EditorEvent) -> Message + 'a>>,
    /// Tab width setting
    tab_width: usize,
    /// Show line numbers
    show_line_numbers: bool,
    /// Highlight current line
    highlight_current_line: bool,
}

impl<'a> CustomTextEditor<'a> {
    /// Creates a new custom text editor widget
    pub fn new(state: &'a EditorState, theme: &'a EditorTheme) -> Self {
        let tab_width = 8; // Fixed tab width for now
        let layout = EditorLayout::new(tab_width);

        Self {
            state,
            theme,
            layout,
            on_input: None,
            tab_width,
            show_line_numbers: true,
            highlight_current_line: true,
        }
    }

    /// Sets the input event handler
    pub fn on_input<F>(mut self, handler: F) -> Self
    where
        F: Fn(stoat::EditorEvent) -> Message + 'a,
    {
        self.on_input = Some(Box::new(handler));
        self
    }

    /// Sets whether to show line numbers
    pub fn show_line_numbers(mut self, show: bool) -> Self {
        self.show_line_numbers = show;
        self
    }

    /// Sets whether to highlight the current line
    pub fn highlight_current_line(mut self, highlight: bool) -> Self {
        self.highlight_current_line = highlight;
        self
    }
}

/// Widget state stored in the tree
struct WidgetState {
    /// Text buffer with cosmic-text
    buffer: TextBuffer,
    /// Event handler state
    event_handler: EventHandler,
    /// Glyph cache for text rendering
    glyph_cache: GlyphCache,
    /// Layout cache for performance
    layout_cache: LayoutCache,
    /// Focus state
    is_focused: bool,
    /// Last text content (for change detection)
    last_text: String,
}

impl WidgetState {
    fn new(tab_width: usize) -> Self {
        let metrics = Metrics {
            font_size: 14.0,
            line_height: 20.0,
        };
        Self {
            buffer: TextBuffer::new(metrics, tab_width),
            event_handler: EventHandler::new(),
            glyph_cache: GlyphCache::new(),
            layout_cache: LayoutCache::new(),
            is_focused: false,
            last_text: String::new(),
        }
    }
}

impl<'a> Widget<Message, Theme, iced::Renderer> for CustomTextEditor<'a> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<WidgetState>()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(WidgetState::new(self.tab_width))
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // Calculate required size based on content
        // For now use estimated sizes
        let line_count = self.state.buffer.rope().to_string().lines().count();
        let max_line_width = 80.0; // Estimated
        let buffer_width = Some(max_line_width * self.theme.char_width());
        let buffer_height = Some(line_count as f32 * self.theme.line_height_px());
        let content_size = Size::new(
            buffer_width.unwrap_or(100.0)
                + self.layout.padding * 2.0
                + self.layout.gutter_width
                + self.layout.scrollbar_width,
            buffer_height.unwrap_or(100.0)
                + self.layout.padding * 2.0
                + self.layout.scrollbar_width,
        );

        // Resolve within limits
        let size = limits.width(Length::Fill).height(Length::Fill).resolve(
            Length::Fill,
            Length::Fill,
            content_size,
        );

        layout::Node::new(size)
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<WidgetState>();
        let mut editor_layout = self.layout.clone();
        editor_layout.set_bounds(layout.bounds());

        // We can't mutate the buffer here since draw takes &self
        // For now, create a temporary buffer for rendering
        let current_text = self.state.buffer.rope().to_string();
        let metrics = Metrics {
            font_size: self.theme.font_size,
            line_height: self.theme.line_height_px(),
        };
        let mut temp_buffer = TextBuffer::new(metrics, self.tab_width);
        temp_buffer.set_text(&current_text);
        temp_buffer.shape_as_needed();

        // Update gutter width if line numbers are enabled
        if self.show_line_numbers {
            let char_width = self.theme.char_width();
            editor_layout.update_gutter_width(temp_buffer.line_count(), char_width, true);
        }

        // Create renderer
        let mut renderer_impl = EditorRenderer::new(self.theme, &editor_layout);
        renderer_impl.show_line_numbers = self.show_line_numbers;
        renderer_impl.highlight_current_line = self.highlight_current_line;

        // Draw everything
        renderer_impl.draw(
            renderer,
            &temp_buffer,
            &mut state.glyph_cache.clone(), // Clone for now to avoid borrow issues
            Some(self.state.cursor.position),
            self.state.cursor.selection,
        );
    }

    fn on_event(
        &mut self,
        tree: &mut Tree,
        event: Event,
        _layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        let state = tree.state.downcast_mut::<WidgetState>();

        if let Some(ref handler) = self.on_input {
            match event {
                // Handle keyboard events
                Event::Keyboard(iced::keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                    let editor_event = stoat::EditorEvent::KeyPress { key, modifiers };
                    let message = handler(editor_event);
                    shell.publish(message);
                    return event::Status::Captured;
                },
                // Handle mouse clicks
                Event::Mouse(iced::mouse::Event::ButtonPressed(button)) => {
                    if let Some(position) = cursor.position() {
                        // TODO: Convert position to text position using cosmic-text hit testing
                        let editor_event = stoat::EditorEvent::MouseClick { position, button };
                        let message = handler(editor_event);
                        shell.publish(message);
                        return event::Status::Captured;
                    }
                },
                // Handle scroll
                Event::Mouse(iced::mouse::Event::WheelScrolled { delta }) => {
                    let (delta_x, delta_y) = match delta {
                        iced::mouse::ScrollDelta::Lines { x, y } => (x * 20.0, y * 20.0),
                        iced::mouse::ScrollDelta::Pixels { x, y } => (x, y),
                    };
                    let editor_event = stoat::EditorEvent::Scroll {
                        delta_x,
                        delta_y: -delta_y,
                    };
                    let message = handler(editor_event);
                    shell.publish(message);
                    return event::Status::Captured;
                },
                _ => {},
            }

            // Fall back to event handler for complex events
            let cursor_position = cursor.position().unwrap_or_default();
            if let Some(editor_event) = state.event_handler.process_event(
                event,
                &self.layout,
                &state.buffer,
                cursor_position,
            ) {
                let message = handler(editor_event);
                shell.publish(message);
                return event::Status::Captured;
            }
        }

        event::Status::Ignored
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _viewport: &Rectangle,
        _renderer: &iced::Renderer,
    ) -> mouse::Interaction {
        let _state = tree.state.downcast_ref::<WidgetState>();

        if let Some(position) = cursor.position() {
            if layout.bounds().contains(position) {
                // Check if over text area
                let text_area = self.layout.text_area();
                if text_area.contains(position) {
                    return mouse::Interaction::Text;
                }

                // Check if over scrollbars
                let metrics = Metrics {
                    font_size: self.theme.font_size,
                    line_height: self.theme.line_height_px(),
                };
                let (start_line, end_line) = self.layout.visible_line_range(metrics);
                let v_scrollbar = self.layout.vertical_scrollbar_bounds(
                    start_line,
                    end_line,
                    self.state.buffer.rope().to_string().lines().count(),
                );

                if v_scrollbar.contains(position) {
                    return mouse::Interaction::Grabbing;
                }

                // TODO: Implement horizontal scrollbar bounds check
                // if let Some(h_scrollbar) = self.layout.horizontal_scrollbar_bounds(&buffer) {
                //     if h_scrollbar.contains(position) {
                //         return mouse::Interaction::Grabbing;
                //     }
                // }
            }
        }

        mouse::Interaction::default()
    }
}

impl<'a> From<CustomTextEditor<'a>> for Element<'a, Message, Theme, iced::Renderer> {
    fn from(editor: CustomTextEditor<'a>) -> Self {
        Element::new(editor)
    }
}
