//! Simplified custom text editor widget that compiles.
//!
//! This is a working stub implementation that can be expanded later.

use crate::{messages::Message, theme::EditorTheme};
use iced::{
    advanced::{
        layout::{self, Layout},
        renderer::{self, Quad, Renderer as RendererTrait},
        text::Renderer as TextRenderer,
        widget::{Tree, Widget},
        Clipboard, Shell,
    },
    event::{self, Event},
    keyboard, mouse, Border, Element, Length, Point, Rectangle, Size, Theme,
};
use stoat::EditorState;

/// Simplified custom text editor widget
pub struct SimpleCustomTextEditor<'a> {
    /// Editor state from the engine
    state: &'a EditorState,
    /// Visual theme
    theme: &'a EditorTheme,
    /// Event callback
    on_input: Option<Box<dyn Fn(stoat::EditorEvent) -> Message + 'a>>,
    /// Tab width setting
    tab_width: usize,
}

impl<'a> SimpleCustomTextEditor<'a> {
    /// Creates a new custom text editor widget
    pub fn new(state: &'a EditorState, theme: &'a EditorTheme) -> Self {
        Self {
            state,
            theme,
            on_input: None,
            tab_width: 8,
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

    /// Sets whether to show line numbers (stub for compatibility)
    pub fn show_line_numbers(self, _show: bool) -> Self {
        self
    }

    /// Sets whether to highlight the current line (stub for compatibility)  
    pub fn highlight_current_line(self, _highlight: bool) -> Self {
        self
    }
}

impl<'a> Widget<Message, Theme, iced::Renderer> for SimpleCustomTextEditor<'a> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &self,
        _tree: &mut Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();

        // Draw background
        let quad = Quad {
            bounds,
            border: Border {
                color: self.theme.cursor_color, // Use cursor_color as border
                width: 1.0,
                radius: 4.0.into(),
            },
            shadow: Default::default(),
        };

        renderer.fill_quad(quad, self.theme.background_color);

        // Draw simple text representation with tabs
        let text = self.state.buffer.rope().to_string();
        let lines: Vec<&str> = text.lines().collect();

        let line_height = self.theme.line_height_px();
        let char_width = self.theme.char_width();

        for (line_idx, line_text) in lines.iter().enumerate().take(50) {
            let y = bounds.y + (line_idx as f32 * line_height);

            // Skip lines outside viewport
            if y > bounds.y + bounds.height {
                break;
            }

            // Expand tabs to spaces for display
            let expanded = expand_tabs(line_text, self.tab_width);

            // Draw line number if enabled
            if self.theme.show_line_numbers {
                let line_num = format!("{:4} ", line_idx + 1);
                renderer.fill_text(
                    iced::advanced::text::Text {
                        content: line_num,
                        bounds: Size::new(char_width * 5.0, line_height),
                        size: iced::Pixels(self.theme.font_size),
                        line_height: iced::widget::text::LineHeight::default(),
                        font: self.theme.font,
                        horizontal_alignment: iced::alignment::Horizontal::Right,
                        vertical_alignment: iced::alignment::Vertical::Top,
                        shaping: iced::widget::text::Shaping::Basic,
                        wrapping: iced::widget::text::Wrapping::None,
                    },
                    Point::new(bounds.x, y),
                    self.theme.line_number_color,
                    bounds,
                );
            }

            // Draw the line text
            let text_x = if self.theme.show_line_numbers {
                bounds.x + char_width * 5.5
            } else {
                bounds.x
            };

            renderer.fill_text(
                iced::advanced::text::Text {
                    content: expanded,
                    bounds: Size::new(bounds.width, line_height),
                    size: iced::Pixels(self.theme.font_size),
                    line_height: iced::widget::text::LineHeight::default(),
                    font: self.theme.font,
                    horizontal_alignment: iced::alignment::Horizontal::Left,
                    vertical_alignment: iced::alignment::Vertical::Top,
                    shaping: iced::widget::text::Shaping::Basic,
                    wrapping: iced::widget::text::Wrapping::None,
                },
                Point::new(text_x, y),
                self.theme.text_color,
                bounds,
            );
        }

        // Draw cursor
        let cursor_pos = self.state.cursor.position;
        let cursor_x = if self.theme.show_line_numbers {
            bounds.x + char_width * 5.5 + (cursor_pos.visual_column as f32 * char_width)
        } else {
            bounds.x + (cursor_pos.visual_column as f32 * char_width)
        };
        let cursor_y = bounds.y + (cursor_pos.line as f32 * line_height);

        if cursor_x < bounds.x + bounds.width && cursor_y < bounds.y + bounds.height {
            let cursor_quad = Quad {
                bounds: Rectangle::new(Point::new(cursor_x, cursor_y), Size::new(2.0, line_height)),
                border: Default::default(),
                shadow: Default::default(),
            };

            renderer.fill_quad(cursor_quad, self.theme.cursor_color);
        }
    }

    fn on_event(
        &mut self,
        _tree: &mut Tree,
        event: Event,
        _layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        if let Some(ref handler) = self.on_input {
            match event {
                Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. }) => {
                    let editor_event = stoat::EditorEvent::KeyPress { key, modifiers };
                    let message = handler(editor_event);
                    shell.publish(message);
                    return event::Status::Captured;
                },
                Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                    if let Some(position) = cursor.position() {
                        let editor_event = stoat::EditorEvent::MouseClick { position, button };
                        let message = handler(editor_event);
                        shell.publish(message);
                        return event::Status::Captured;
                    }
                },
                Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                    let (delta_x, delta_y) = match delta {
                        mouse::ScrollDelta::Lines { x, y } => (x * 20.0, y * 20.0),
                        mouse::ScrollDelta::Pixels { x, y } => (x, y),
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
        }

        event::Status::Ignored
    }
}

impl<'a> From<SimpleCustomTextEditor<'a>> for Element<'a, Message, Theme, iced::Renderer> {
    fn from(editor: SimpleCustomTextEditor<'a>) -> Self {
        Element::new(editor)
    }
}

/// Helper function to expand tabs to spaces
fn expand_tabs(text: &str, tab_width: usize) -> String {
    let mut result = String::new();
    let mut col = 0;

    for ch in text.chars() {
        if ch == '\t' {
            let spaces_to_add = tab_width - (col % tab_width);
            for _ in 0..spaces_to_add {
                result.push(' ');
                col += 1;
            }
        } else {
            result.push(ch);
            col += 1;
        }
    }

    result
}
