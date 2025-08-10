use crate::{
    state::{NodeContent, NodeRenderData, NodeState},
    widget::theme::{Colors, Style, Styles},
};
use iced::{
    widget::canvas::{Frame, Path, Stroke, Text},
    Border, Color, Font, Point, Rectangle, Shadow, Size,
};

/// A node widget that can be rendered on the canvas
pub struct Node<'a> {
    data: &'a NodeRenderData,
}

impl<'a> Node<'a> {
    /// Create a new node widget
    pub fn new(data: &'a NodeRenderData) -> Self {
        Self { data }
    }

    /// Draw the node with improved styling
    pub fn draw(&self, frame: &mut Frame, position: Point, size: Size) {
        // Calculate dimensions
        let bounds = Rectangle::new(position, size);
        let title_bounds = Rectangle {
            x: bounds.x,
            y: bounds.y,
            width: bounds.width,
            height: Style::NODE_TITLE_HEIGHT,
        };
        let content_bounds = Rectangle {
            x: bounds.x,
            y: bounds.y + Style::NODE_TITLE_HEIGHT,
            width: bounds.width,
            height: bounds.height - Style::NODE_TITLE_HEIGHT,
        };

        // Draw shadow based on state
        let shadow = match self.data.state {
            NodeState::Normal => Styles::shadow_default(),
            NodeState::Focused | NodeState::Selected => Styles::shadow_elevated(),
        };

        self.draw_shadow(frame, bounds, shadow);

        // Draw main node body
        self.draw_node_body(frame, bounds);

        // Draw title bar
        self.draw_title_bar(frame, title_bounds);

        // Draw content area
        self.draw_content(frame, content_bounds);

        // Draw border
        let border = match self.data.state {
            NodeState::Normal => Styles::border_default(),
            NodeState::Focused => Styles::border_focused(),
            NodeState::Selected => Styles::border_selected(),
        };

        self.draw_border(frame, bounds, border);
    }

    /// Draw the node shadow
    fn draw_shadow(&self, frame: &mut Frame, bounds: Rectangle, shadow: Shadow) {
        let shadow_bounds = Rectangle {
            x: bounds.x + shadow.offset.x,
            y: bounds.y + shadow.offset.y,
            width: bounds.width,
            height: bounds.height,
        };

        // Simple shadow approximation using multiple layers
        for i in 0..3 {
            let alpha = shadow.color.a * (1.0 - (i as f32 / 3.0));
            let blur_offset = shadow.blur_radius * (i as f32 / 3.0);

            let shadow_path = Path::new(|builder| {
                builder.move_to(Point::new(
                    shadow_bounds.x - blur_offset,
                    shadow_bounds.y - blur_offset,
                ));
                builder.rectangle(
                    Point::new(shadow_bounds.x - blur_offset, shadow_bounds.y - blur_offset),
                    Size::new(
                        shadow_bounds.width + blur_offset * 2.0,
                        shadow_bounds.height + blur_offset * 2.0,
                    ),
                );
            });

            frame.fill(
                &shadow_path,
                Color {
                    a: alpha,
                    ..shadow.color
                },
            );
        }
    }

    /// Draw the main node body
    fn draw_node_body(&self, frame: &mut Frame, bounds: Rectangle) {
        let background_color = match self.data.state {
            NodeState::Normal => Colors::NODE_BACKGROUND,
            NodeState::Focused | NodeState::Selected => Colors::NODE_BACKGROUND_HOVER,
        };

        let path = Path::new(|builder| {
            builder.move_to(bounds.position());
            builder.rectangle(bounds.position(), bounds.size());
        });

        frame.fill(&path, background_color);
    }

    /// Draw the title bar
    fn draw_title_bar(&self, frame: &mut Frame, bounds: Rectangle) {
        // Title bar background with rounded top corners
        let title_path = Path::new(|builder| {
            let radius = Style::BORDER_RADIUS;
            let pos = bounds.position();
            let size = bounds.size();

            // Start from top-left corner (after radius)
            builder.move_to(Point::new(pos.x + radius, pos.y));

            // Top edge to top-right corner
            builder.line_to(Point::new(pos.x + size.width - radius, pos.y));

            // Top-right corner arc
            builder.arc_to(
                Point::new(pos.x + size.width, pos.y),
                Point::new(pos.x + size.width, pos.y + radius),
                radius,
            );

            // Right edge to bottom-right
            builder.line_to(Point::new(pos.x + size.width, pos.y + size.height));

            // Bottom edge to bottom-left
            builder.line_to(Point::new(pos.x, pos.y + size.height));

            // Left edge to top-left corner
            builder.line_to(Point::new(pos.x, pos.y + radius));

            // Top-left corner arc
            builder.arc_to(
                Point::new(pos.x, pos.y),
                Point::new(pos.x + radius, pos.y),
                radius,
            );
        });

        frame.fill(&title_path, Colors::NODE_TITLE_BACKGROUND);

        // Title text
        let title_position = Point::new(
            bounds.x + Style::NODE_TITLE_PADDING,
            bounds.y + bounds.height / 2.0,
        );

        frame.fill_text(Text {
            content: self.data.title.clone(),
            position: title_position,
            font: Font::default(),
            size: Style::TEXT_SIZE_TITLE.into(),
            color: Colors::TEXT_PRIMARY,
            horizontal_alignment: iced::alignment::Horizontal::Left,
            vertical_alignment: iced::alignment::Vertical::Center,
            line_height: iced::widget::text::LineHeight::default(),
            shaping: iced::widget::text::Shaping::Basic,
        });

        // Optional: Add node type indicator on the right
        if let NodeContent::Text { .. } = &self.data.content {
            let type_text = "TEXT";
            let type_position = Point::new(
                bounds.x + bounds.width - Style::NODE_TITLE_PADDING - 30.0,
                bounds.y + bounds.height / 2.0,
            );

            frame.fill_text(Text {
                content: type_text.to_string(),
                position: type_position,
                font: Font::default(),
                size: Style::TEXT_SIZE_SMALL.into(),
                color: Colors::TEXT_TERTIARY,
                horizontal_alignment: iced::alignment::Horizontal::Right,
                vertical_alignment: iced::alignment::Vertical::Center,
                line_height: iced::widget::text::LineHeight::default(),
                shaping: iced::widget::text::Shaping::Basic,
            });
        }
    }

    /// Draw the content area
    fn draw_content(&self, frame: &mut Frame, bounds: Rectangle) {
        let content_position = Point::new(
            bounds.x + Style::NODE_PADDING,
            bounds.y + Style::NODE_PADDING,
        );

        match &self.data.content {
            NodeContent::Text {
                lines,
                cursor_position,
                selection,
            } => {
                self.draw_text_content(
                    frame,
                    content_position,
                    lines,
                    cursor_position.as_ref(),
                    selection.as_ref(),
                );
            },
            NodeContent::RopeText {
                buffer_id: _,
                viewport: _,
                lines,
                cursors,
                selections,
            } => {
                self.draw_rope_text_content(frame, content_position, lines, cursors, selections);
            },
            NodeContent::InteractiveText {
                text,
                cursor_position,
                focused,
                placeholder,
                buffer_id: _,
            } => {
                self.draw_interactive_text_content(
                    frame,
                    content_position,
                    text,
                    *cursor_position,
                    *focused,
                    placeholder,
                );
            },
            NodeContent::AgenticChat => {
                // Don't draw content here - the actual chat widget is rendered as an overlay
                // Just show a placeholder to indicate this is the chat node
                frame.fill_text(Text {
                    content: "Chat Interface".to_string(),
                    position: content_position,
                    font: Font::default(),
                    size: Style::TEXT_SIZE_REGULAR.into(),
                    color: Colors::TEXT_SECONDARY,
                    horizontal_alignment: iced::alignment::Horizontal::Left,
                    vertical_alignment: iced::alignment::Vertical::Top,
                    line_height: iced::widget::text::LineHeight::default(),
                    shaping: iced::widget::text::Shaping::Basic,
                });
            },
            NodeContent::Empty => {
                // Draw placeholder text
                frame.fill_text(Text {
                    content: "Empty node".to_string(),
                    position: content_position,
                    font: Font::default(),
                    size: Style::TEXT_SIZE_REGULAR.into(),
                    color: Colors::TEXT_TERTIARY,
                    horizontal_alignment: iced::alignment::Horizontal::Left,
                    vertical_alignment: iced::alignment::Vertical::Top,
                    line_height: iced::widget::text::LineHeight::default(),
                    shaping: iced::widget::text::Shaping::Basic,
                });
            },
        }
    }

    /// Draw text content with syntax highlighting support (future)
    fn draw_text_content(
        &self,
        frame: &mut Frame,
        start_position: Point,
        lines: &[String],
        cursor_position: Option<&crate::state::CursorPosition>,
        selection: Option<&crate::state::TextSelection>,
    ) {
        let char_width = 8.0; // Approximate monospace character width

        // Draw selection if present
        if let Some(sel) = selection {
            let selection_color = Colors::SELECTION_BACKGROUND;

            // Calculate selection bounds (simplified for single line)
            if sel.start.line == sel.end.line {
                let y = start_position.y + (sel.start.line as f32 * Style::LINE_HEIGHT);
                let x_start = start_position.x + (sel.start.column as f32 * char_width);
                let x_end = start_position.x + (sel.end.column as f32 * char_width);

                let selection_path = Path::new(|builder| {
                    builder.rectangle(
                        Point::new(x_start, y),
                        Size::new(x_end - x_start, Style::LINE_HEIGHT),
                    );
                });

                frame.fill(&selection_path, selection_color);
            }
        }

        // Draw each line of text
        for (line_index, line) in lines.iter().enumerate() {
            let line_position = Point::new(
                start_position.x,
                start_position.y + (line_index as f32 * Style::LINE_HEIGHT),
            );

            frame.fill_text(Text {
                content: line.clone(),
                position: line_position,
                font: Font::MONOSPACE,
                size: Style::TEXT_SIZE_REGULAR.into(),
                color: Colors::TEXT_PRIMARY,
                horizontal_alignment: iced::alignment::Horizontal::Left,
                vertical_alignment: iced::alignment::Vertical::Top,
                line_height: iced::widget::text::LineHeight::default(),
                shaping: iced::widget::text::Shaping::Basic,
            });
        }

        // Draw cursor if present
        if let Some(cursor) = cursor_position {
            let cursor_x = start_position.x + (cursor.column as f32 * char_width);
            let cursor_y = start_position.y + (cursor.line as f32 * Style::LINE_HEIGHT);

            // Blinking cursor effect could be added with animation
            let cursor_path = Path::new(|builder| {
                builder.rectangle(
                    Point::new(cursor_x, cursor_y),
                    Size::new(2.0, Style::LINE_HEIGHT),
                );
            });

            frame.fill(&cursor_path, Colors::ACCENT_PRIMARY);
        }
    }

    /// Draw rope-based text content with zero-allocation line iteration
    fn draw_rope_text_content(
        &self,
        frame: &mut Frame,
        start_position: Point,
        lines: &[String],
        cursors: &[crate::state::CursorPosition],
        selections: &[crate::state::TextSelection],
    ) {
        let char_width = 8.0; // Approximate monospace character width

        // Draw the actual text lines from the rope buffer
        for (line_index, line) in lines.iter().enumerate() {
            let line_position = Point::new(
                start_position.x,
                start_position.y + (line_index as f32 * Style::LINE_HEIGHT),
            );

            frame.fill_text(Text {
                content: line.clone(),
                position: line_position,
                font: Font::MONOSPACE,
                size: Style::TEXT_SIZE_REGULAR.into(),
                color: Colors::TEXT_PRIMARY, // Primary color for actual content
                horizontal_alignment: iced::alignment::Horizontal::Left,
                vertical_alignment: iced::alignment::Vertical::Top,
                line_height: iced::widget::text::LineHeight::default(),
                shaping: iced::widget::text::Shaping::Basic,
            });
        }

        // Draw cursors if present
        for cursor in cursors {
            let cursor_x = start_position.x + (cursor.column as f32 * char_width);
            let cursor_y = start_position.y + (cursor.line as f32 * Style::LINE_HEIGHT);

            let cursor_path = Path::new(|builder| {
                builder.rectangle(
                    Point::new(cursor_x, cursor_y),
                    Size::new(2.0, Style::LINE_HEIGHT),
                );
            });

            frame.fill(&cursor_path, Colors::ACCENT_SUCCESS); // Different color for rope cursors
        }

        // Draw selections if present
        for selection in selections {
            // For now, only support single-line selections like the original
            if selection.start.line == selection.end.line {
                let selection_color = Colors::SELECTION_BACKGROUND;
                let y = start_position.y + (selection.start.line as f32 * Style::LINE_HEIGHT);
                let x_start = start_position.x + (selection.start.column as f32 * char_width);
                let x_end = start_position.x + (selection.end.column as f32 * char_width);

                let selection_path = Path::new(|builder| {
                    builder.rectangle(
                        Point::new(x_start, y),
                        Size::new(x_end - x_start, Style::LINE_HEIGHT),
                    );
                });

                frame.fill(&selection_path, selection_color);
            }
        }
    }

    /// Draw interactive text content with visual indicators for editing mode
    fn draw_interactive_text_content(
        &self,
        frame: &mut Frame,
        start_position: Point,
        text: &str,
        cursor_position: usize,
        _focused: bool,
        placeholder: &str,
    ) {
        let char_width = 8.0; // Approximate monospace character width

        // Use the provided text
        let lines: Vec<&str> = if text.is_empty() {
            vec![placeholder]
        } else {
            text.lines().collect()
        };

        // No special background - use normal node background like other content types

        // Draw text content
        for (line_index, line) in lines.iter().enumerate() {
            let line_position = Point::new(
                start_position.x,
                start_position.y + (line_index as f32 * Style::LINE_HEIGHT),
            );

            let text_color = if text.is_empty() {
                Colors::TEXT_TERTIARY // Placeholder text color
            } else {
                Colors::TEXT_PRIMARY // Normal text color
            };

            frame.fill_text(Text {
                content: line.to_string(),
                position: line_position,
                font: Font::MONOSPACE,
                size: Style::TEXT_SIZE_REGULAR.into(),
                color: text_color,
                horizontal_alignment: iced::alignment::Horizontal::Left,
                vertical_alignment: iced::alignment::Vertical::Top,
                line_height: iced::widget::text::LineHeight::default(),
                shaping: iced::widget::text::Shaping::Basic,
            });
        }

        // Draw cursor position
        if let Some((line, column)) = Self::cursor_to_line_column(text, cursor_position) {
            let cursor_x = start_position.x + (column as f32 * char_width);
            let cursor_y = start_position.y + (line as f32 * Style::LINE_HEIGHT);

            let cursor_path = Path::new(|builder| {
                builder.rectangle(
                    Point::new(cursor_x, cursor_y),
                    Size::new(2.0, Style::LINE_HEIGHT),
                );
            });

            // Use same cursor color as regular text nodes
            frame.fill(&cursor_path, Colors::ACCENT_SUCCESS);
        }
    }

    /// Convert text editor cursor position to line/column coordinates
    fn cursor_to_line_column(text: &str, cursor_position: usize) -> Option<(usize, usize)> {
        if text.is_empty() {
            return Some((0, 0));
        }

        let mut line = 0;
        let mut column = 0;
        let mut current_pos = 0;

        for ch in text.chars() {
            if current_pos == cursor_position {
                return Some((line, column));
            }

            if ch == '\n' {
                line += 1;
                column = 0;
            } else {
                column += 1;
            }

            current_pos += ch.len_utf8();
        }

        Some((line, column))
    }

    /// Draw the node border
    fn draw_border(&self, frame: &mut Frame, bounds: Rectangle, border: Border) {
        let path = Path::new(|builder| {
            let radius = border.radius;
            let pos = bounds.position();
            let size = bounds.size();

            // Draw rounded rectangle border
            if radius.top_left > 0.0
                || radius.top_right > 0.0
                || radius.bottom_left > 0.0
                || radius.bottom_right > 0.0
            {
                // Start from top-left corner (after radius)
                builder.move_to(Point::new(pos.x + radius.top_left, pos.y));

                // Top edge
                builder.line_to(Point::new(pos.x + size.width - radius.top_right, pos.y));

                // Top-right corner
                if radius.top_right > 0.0 {
                    builder.arc_to(
                        Point::new(pos.x + size.width, pos.y),
                        Point::new(pos.x + size.width, pos.y + radius.top_right),
                        radius.top_right,
                    );
                }

                // Right edge
                builder.line_to(Point::new(
                    pos.x + size.width,
                    pos.y + size.height - radius.bottom_right,
                ));

                // Bottom-right corner
                if radius.bottom_right > 0.0 {
                    builder.arc_to(
                        Point::new(pos.x + size.width, pos.y + size.height),
                        Point::new(
                            pos.x + size.width - radius.bottom_right,
                            pos.y + size.height,
                        ),
                        radius.bottom_right,
                    );
                }

                // Bottom edge
                builder.line_to(Point::new(pos.x + radius.bottom_left, pos.y + size.height));

                // Bottom-left corner
                if radius.bottom_left > 0.0 {
                    builder.arc_to(
                        Point::new(pos.x, pos.y + size.height),
                        Point::new(pos.x, pos.y + size.height - radius.bottom_left),
                        radius.bottom_left,
                    );
                }

                // Left edge
                builder.line_to(Point::new(pos.x, pos.y + radius.top_left));

                // Top-left corner
                if radius.top_left > 0.0 {
                    builder.arc_to(
                        Point::new(pos.x, pos.y),
                        Point::new(pos.x + radius.top_left, pos.y),
                        radius.top_left,
                    );
                }
            } else {
                // Simple rectangle
                builder.rectangle(pos, size);
            }
        });

        frame.stroke(
            &path,
            Stroke::default()
                .with_width(border.width)
                .with_color(border.color),
        );
    }
}
