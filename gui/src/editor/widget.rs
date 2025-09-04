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
    /// Text buffer with cosmic-text
    buffer: TextBuffer,
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
        let metrics = Metrics {
            font_size: theme.font_size,
            line_height: theme.line_height_px(),
        };

        let mut buffer = TextBuffer::new(metrics, tab_width);

        // Convert state buffer to text and set in cosmic-text buffer
        let text = state.buffer.rope().to_string();
        buffer.set_text(&text);
        buffer.shape_as_needed();

        let layout = EditorLayout::new(tab_width);

        Self {
            state,
            theme,
            buffer,
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
    /// Event handler state
    event_handler: EventHandler,
    /// Glyph cache for text rendering
    glyph_cache: GlyphCache,
    /// Layout cache for performance
    layout_cache: LayoutCache,
    /// Focus state
    is_focused: bool,
}

impl Default for WidgetState {
    fn default() -> Self {
        Self {
            event_handler: EventHandler::new(),
            glyph_cache: GlyphCache::new(),
            layout_cache: LayoutCache::new(),
            is_focused: false,
        }
    }
}

impl<'a> Widget<Message, Theme, iced::Renderer> for CustomTextEditor<'a> {
    fn tag(&self) -> widget::tree::Tag {
        widget::tree::Tag::of::<WidgetState>()
    }

    fn state(&self) -> widget::tree::State {
        widget::tree::State::new(WidgetState::default())
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
        // Calculate required size based on buffer content
        let (buffer_width, buffer_height) = self.buffer.size();
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

        // Update gutter width if line numbers are enabled
        if self.show_line_numbers {
            let char_width = self.theme.char_width();
            editor_layout.update_gutter_width(self.buffer.line_count(), char_width, true);
        }

        // Create renderer
        let mut renderer_impl = EditorRenderer::new(self.theme, &editor_layout);
        renderer_impl.show_line_numbers = self.show_line_numbers;
        renderer_impl.highlight_current_line = self.highlight_current_line;

        // Draw everything
        renderer_impl.draw(
            renderer,
            &self.buffer,
            &mut state.glyph_cache.clone(), // Clone for now to avoid borrow issues
            Some(self.state.cursor.position),
            self.state.cursor.selection.map(|s| s),
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
            // Get cursor position for mouse events
            let cursor_position = cursor.position().unwrap_or_default();

            // Process event
            if let Some(editor_event) = state.event_handler.process_event(
                event,
                &self.layout,
                &self.buffer,
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
                let metrics = self.buffer.metrics();
                let (start_line, end_line) = self.layout.visible_line_range(metrics);
                let v_scrollbar = self.layout.vertical_scrollbar_bounds(
                    start_line,
                    end_line,
                    self.buffer.line_count(),
                );

                if v_scrollbar.contains(position) {
                    return mouse::Interaction::Grabbing;
                }

                if let Some(h_scrollbar) = self.layout.horizontal_scrollbar_bounds(&self.buffer) {
                    if h_scrollbar.contains(position) {
                        return mouse::Interaction::Grabbing;
                    }
                }
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
