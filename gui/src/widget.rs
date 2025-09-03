//! Custom editor widget that renders EditorState.
//!
//! This widget renders editor state and forwards input events to the editor engine.

use crate::{messages::Message, theme::EditorTheme};
use iced::{
    advanced::{
        layout::{self, Layout},
        renderer::{self, Quad, Renderer as RendererTrait},
        text::Renderer as TextRenderer,
        widget::{self, Widget},
        Clipboard, Shell,
    },
    event::{self, Event},
    keyboard, mouse, Element, Length, Point, Rectangle, Size, Theme,
};
use stoat::{EditorEvent, EditorState};

/// Custom editor widget that renders an EditorState.
///
/// This widget is purely presentational - it takes the current editor state
/// and renders it, while converting user input to EditorEvents.
pub struct EditorWidget<'a> {
    /// The editor state to render (read-only)
    state: &'a EditorState,

    /// Visual theme for rendering
    theme: &'a EditorTheme,

    /// Callback for input events
    on_input: Option<Box<dyn Fn(EditorEvent) -> Message + 'a>>,
}

impl<'a> EditorWidget<'a> {
    /// Creates a new editor widget with the given state and theme.
    pub fn new(state: &'a EditorState, theme: &'a EditorTheme) -> Self {
        Self {
            state,
            theme,
            on_input: None,
        }
    }

    /// Sets the input event handler.
    pub fn on_input<F>(mut self, handler: F) -> Self
    where
        F: Fn(EditorEvent) -> Message + 'a,
    {
        self.on_input = Some(Box::new(handler));
        self
    }
}

impl<'a> Widget<Message, Theme, iced::Renderer> for EditorWidget<'a> {
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &self,
        _tree: &mut widget::Tree,
        _renderer: &iced::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        layout::Node::new(limits.max())
    }

    fn draw(
        &self,
        _tree: &widget::Tree,
        renderer: &mut iced::Renderer,
        _theme: &Theme,
        _style: &renderer::Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        _viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();

        // Draw background
        let background_quad = Quad {
            bounds,
            border: Default::default(),
            shadow: Default::default(),
        };

        renderer.fill_quad(
            background_quad,
            iced::Background::Color(self.theme.background_color),
        );

        // Calculate text metrics
        let char_width = self.theme.char_width();
        let line_height = self.theme.line_height_px();

        // Draw text content line by line
        let scroll_x = self.state.viewport.scroll_x;
        let scroll_y = self.state.viewport.scroll_y;

        let start_line = (scroll_y / line_height) as usize;
        let visible_lines = (bounds.height / line_height) as usize + 2; // +2 for partial lines

        for (line_index, line_text) in self
            .state
            .buffer
            .lines()
            .enumerate()
            .skip(start_line)
            .take(visible_lines)
        {
            let y = line_index as f32 * line_height - scroll_y + bounds.y;

            // Skip lines that are completely outside viewport
            if y + line_height < bounds.y || y > bounds.y + bounds.height {
                continue;
            }

            // Draw line numbers if enabled
            if self.theme.show_line_numbers {
                let line_number = format!("{:4} ", line_index + 1);
                let line_num_x = bounds.x + 5.0;

                renderer.fill_text(
                    iced::advanced::text::Text {
                        content: line_number,
                        bounds: Size::new(char_width * 5.0, line_height),
                        size: iced::Pixels(self.theme.font_size),
                        line_height: iced::widget::text::LineHeight::default(),
                        font: self.theme.font,
                        horizontal_alignment: iced::alignment::Horizontal::Right,
                        vertical_alignment: iced::alignment::Vertical::Top,
                        shaping: iced::widget::text::Shaping::Advanced,
                        wrapping: iced::widget::text::Wrapping::None,
                    },
                    Point::new(line_num_x, y),
                    self.theme.text_color,
                    bounds,
                );
            }

            // Calculate text starting position
            let text_start_x = if self.theme.show_line_numbers {
                bounds.x + char_width * 5.5 - scroll_x
            } else {
                bounds.x - scroll_x
            };

            // Draw the line text
            if !line_text.is_empty() {
                renderer.fill_text(
                    iced::advanced::text::Text {
                        content: line_text.to_string(),
                        bounds: Size::new(bounds.width, line_height),
                        size: iced::Pixels(self.theme.font_size),
                        line_height: iced::widget::text::LineHeight::default(),
                        font: self.theme.font,
                        horizontal_alignment: iced::alignment::Horizontal::Left,
                        vertical_alignment: iced::alignment::Vertical::Top,
                        shaping: iced::widget::text::Shaping::Advanced,
                        wrapping: iced::widget::text::Wrapping::None,
                    },
                    Point::new(text_start_x, y),
                    self.theme.text_color,
                    bounds,
                );
            }
        }

        // Draw cursor
        self.draw_cursor(
            renderer,
            bounds,
            char_width,
            line_height,
            scroll_x,
            scroll_y,
        );

        // Draw selection if any
        if let Some(selection) = self.state.cursor.selection {
            self.draw_selection(
                renderer,
                bounds,
                selection,
                char_width,
                line_height,
                scroll_x,
                scroll_y,
            );
        }
    }

    fn on_event(
        &mut self,
        _tree: &mut widget::Tree,
        event: Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &iced::Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) -> event::Status {
        if let Some(ref handler) = self.on_input {
            match event {
                Event::Keyboard(keyboard::Event::KeyPressed {
                    key,
                    location: _,
                    modifiers,
                    text,
                    modified_key: _,
                    physical_key: _,
                }) => {
                    // Determine the effective key and modifiers based on the key type
                    let (effective_key, effective_modifiers) = match (&key, text) {
                        // If it's a Character key and we have text, use the text
                        // (this handles shifted chars like "?")
                        (keyboard::Key::Character(_), Some(text)) if !text.is_empty() => {
                            // Remove SHIFT since it's already applied in the text
                            let mut mods = modifiers;
                            mods.remove(keyboard::Modifiers::SHIFT);
                            (keyboard::Key::Character(text), mods)
                        },
                        // For everything else (Named keys, empty text, etc.), use the original
                        _ => (key.clone(), modifiers),
                    };

                    // Send KeyPress event with the effective key and modifiers
                    let key_event = EditorEvent::KeyPress {
                        key: effective_key,
                        modifiers: effective_modifiers,
                    };
                    let message = handler(key_event);
                    shell.publish(message);

                    return event::Status::Captured;
                },

                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                    if let Some(cursor_position) = cursor.position() {
                        let bounds = layout.bounds();
                        if bounds.contains(cursor_position) {
                            let editor_event = EditorEvent::MouseClick {
                                position: cursor_position,
                                button: mouse::Button::Left,
                            };
                            let message = handler(editor_event);
                            shell.publish(message);
                            return event::Status::Captured;
                        }
                    }
                },

                Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                    let (delta_x, delta_y) = match delta {
                        mouse::ScrollDelta::Lines { x, y } => (x * 20.0, y * 20.0),
                        mouse::ScrollDelta::Pixels { x, y } => (x, y),
                    };

                    let editor_event = EditorEvent::Scroll {
                        delta_x,
                        delta_y: -delta_y, // Invert Y for natural scrolling
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

impl<'a> EditorWidget<'a> {
    /// Draws the text cursor at the current position.
    fn draw_cursor(
        &self,
        renderer: &mut iced::Renderer,
        bounds: Rectangle,
        char_width: f32,
        line_height: f32,
        scroll_x: f32,
        scroll_y: f32,
    ) {
        let cursor_pos = self.state.cursor.position;

        // Calculate cursor pixel position using visual columns
        let text_start_x = if self.theme.show_line_numbers {
            bounds.x + char_width * 5.5
        } else {
            bounds.x
        };

        // Use the visual column from the cursor position
        let visual_column = if cursor_pos.visual_column > 0 || cursor_pos.column == 0 {
            cursor_pos.visual_column
        } else {
            // Fallback: calculate visual column if not set
            if let Some(line) = self.state.line(cursor_pos.line) {
                self.calculate_visual_column(&line, cursor_pos.column, self.state.tab_width)
            } else {
                cursor_pos.column
            }
        };

        let cursor_x = text_start_x + (visual_column as f32 * char_width) - scroll_x;
        let cursor_y = bounds.y + (cursor_pos.line as f32 * line_height) - scroll_y;

        // Only draw cursor if it's visible in viewport
        if cursor_x >= bounds.x
            && cursor_x <= bounds.x + bounds.width
            && cursor_y >= bounds.y
            && cursor_y <= bounds.y + bounds.height
        {
            let cursor_quad = Quad {
                bounds: Rectangle::new(Point::new(cursor_x, cursor_y), Size::new(2.0, line_height)),
                border: Default::default(),
                shadow: Default::default(),
            };

            renderer.fill_quad(
                cursor_quad,
                iced::Background::Color(self.theme.cursor_color),
            );
        }
    }

    /// Draws text selection highlighting.
    fn draw_selection(
        &self,
        renderer: &mut iced::Renderer,
        bounds: Rectangle,
        selection: stoat::actions::TextRange,
        char_width: f32,
        line_height: f32,
        scroll_x: f32,
        scroll_y: f32,
    ) {
        let start_pos = selection.start;
        let end_pos = selection.end;

        let text_start_x = if self.theme.show_line_numbers {
            bounds.x + char_width * 5.5
        } else {
            bounds.x
        };

        // Simple single-line selection for now
        if start_pos.line == end_pos.line {
            // Get visual columns for selection start and end
            let line_text = self
                .state
                .line(start_pos.line)
                .unwrap_or_else(|| String::new());
            let start_visual =
                self.calculate_visual_column(&line_text, start_pos.column, self.state.tab_width);
            let end_visual =
                self.calculate_visual_column(&line_text, end_pos.column, self.state.tab_width);

            let sel_x = text_start_x + (start_visual as f32 * char_width) - scroll_x;
            let sel_y = bounds.y + (start_pos.line as f32 * line_height) - scroll_y;
            let sel_width = (end_visual - start_visual) as f32 * char_width;

            if sel_y >= bounds.y && sel_y <= bounds.y + bounds.height {
                let selection_quad = Quad {
                    bounds: Rectangle::new(
                        Point::new(sel_x, sel_y),
                        Size::new(sel_width, line_height),
                    ),
                    border: Default::default(),
                    shadow: Default::default(),
                };

                renderer.fill_quad(
                    selection_quad,
                    iced::Background::Color(self.theme.selection_color),
                );
            }
        }
        // TODO: Handle multi-line selections
    }
}

impl<'a> EditorWidget<'a> {
    /// Calculate the visual column position accounting for tabs
    fn calculate_visual_column(&self, line: &str, char_column: usize, tab_width: usize) -> usize {
        let mut visual_col = 0;
        let mut char_col = 0;

        for ch in line.chars() {
            if char_col >= char_column {
                break;
            }

            if ch == '\t' {
                // Tab aligns to next tab stop
                visual_col = (visual_col / tab_width + 1) * tab_width;
            } else {
                visual_col += 1;
            }

            char_col += 1;
        }

        visual_col
    }
}

impl<'a> From<EditorWidget<'a>> for Element<'a, Message, Theme, iced::Renderer> {
    fn from(widget: EditorWidget<'a>) -> Self {
        Element::new(widget)
    }
}
