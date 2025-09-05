//! Rendering pipeline for the text editor.
//!
//! This module handles all rendering operations including text, cursor,
//! selections, line numbers, and scrollbars.

use super::{buffer::TextBuffer, cache::GlyphCache, layout::EditorLayout};
use crate::theme::EditorTheme;
use iced::{
    advanced::{
        renderer::{Quad, Renderer as RendererTrait},
        text::Renderer as TextRenderer,
    },
    Border, Color, Point, Rectangle, Renderer, Size,
};
use stoat::actions::{TextPosition, TextRange};

/// Handles all rendering operations for the text editor
pub struct EditorRenderer<'a> {
    pub theme: &'a EditorTheme,
    pub layout: &'a EditorLayout,
    pub show_line_numbers: bool,
    pub highlight_current_line: bool,
}

impl<'a> EditorRenderer<'a> {
    /// Creates a new renderer with the given configuration
    pub fn new(theme: &'a EditorTheme, layout: &'a EditorLayout) -> Self {
        Self {
            theme,
            layout,
            show_line_numbers: true,
            highlight_current_line: true,
        }
    }

    /// Main rendering function - draws all layers
    pub fn draw(
        &self,
        renderer: &mut Renderer,
        buffer: &TextBuffer,
        glyph_cache: &mut GlyphCache,
        cursor_pos: Option<TextPosition>,
        selection: Option<TextRange>,
    ) {
        // Layer 1: Background
        self.draw_background(renderer);

        // Layer 2: Line highlight (if enabled)
        if let Some(cursor) = cursor_pos {
            if self.highlight_current_line {
                self.draw_line_highlight(renderer, cursor.line);
            }
        }

        // Layer 3: Selection (behind text)
        if let Some(sel) = selection {
            self.draw_selection(renderer, buffer, sel);
        }

        // Layer 4: Line numbers (if enabled)
        if self.show_line_numbers {
            self.draw_line_numbers(renderer, buffer);
        }

        // Layer 5: Main text content
        self.draw_text_content(renderer, buffer, glyph_cache);

        // Layer 6: Cursor
        if let Some(cursor) = cursor_pos {
            self.draw_cursor(renderer, buffer, cursor);
        }

        // Layer 7: Scrollbars
        self.draw_scrollbars(renderer, buffer);
    }

    /// Draws the background
    fn draw_background(&self, renderer: &mut Renderer) {
        let quad = Quad {
            bounds: self.layout.bounds,
            border: Border {
                color: self.theme.cursor_color,
                width: 1.0,
                radius: 4.0.into(),
            },
            shadow: Default::default(),
        };

        renderer.fill_quad(quad, self.theme.background_color);
    }

    /// Draws line highlight for the current line
    fn draw_line_highlight(&self, renderer: &mut Renderer, line: usize) {
        // Use theme's line height directly
        let line_height = self.theme.line_height_px();
        let line_y = line as f32 * line_height - self.layout.scroll_y;

        let text_area = self.layout.text_area();
        let highlight_rect = Rectangle::new(
            Point::new(text_area.x, text_area.y + line_y),
            Size::new(text_area.width, line_height),
        );

        // Only draw if visible
        if highlight_rect.y >= text_area.y && highlight_rect.y < text_area.y + text_area.height {
            let quad = Quad {
                bounds: highlight_rect,
                border: Border::default(),
                shadow: Default::default(),
            };

            let highlight_color = Color::from_rgba(
                self.theme.selection_color.r,
                self.theme.selection_color.g,
                self.theme.selection_color.b,
                0.1, // Very subtle highlight
            );

            renderer.fill_quad(quad, highlight_color);
        }
    }

    /// Draws text selection
    fn draw_selection(&self, renderer: &mut Renderer, buffer: &TextBuffer, selection: TextRange) {
        let metrics = buffer.metrics();
        let text_area = self.layout.text_area();

        // For simplicity, handle single-line selection first
        // TODO: Handle multi-line selections
        if selection.start.line == selection.end.line {
            let line_y = selection.start.line as f32 * metrics.line_height - self.layout.scroll_y;

            // Calculate x positions using visual columns
            let start_x = selection.start.visual_column as f32 * metrics.font_size * 0.6; // Approximate char width
            let end_x = selection.end.visual_column as f32 * metrics.font_size * 0.6;

            let sel_rect = Rectangle::new(
                Point::new(
                    text_area.x + start_x - self.layout.scroll_x,
                    text_area.y + line_y,
                ),
                Size::new(end_x - start_x, metrics.line_height),
            );

            // Only draw if visible
            if sel_rect.y >= text_area.y && sel_rect.y < text_area.y + text_area.height {
                let quad = Quad {
                    bounds: sel_rect,
                    border: Border::default(),
                    shadow: Default::default(),
                };

                renderer.fill_quad(quad, self.theme.selection_color);
            }
        }
    }

    /// Draws line numbers in the gutter
    fn draw_line_numbers(&self, renderer: &mut Renderer, buffer: &TextBuffer) {
        let metrics = buffer.metrics();
        let (start_line, end_line) = self.layout.visible_line_range(metrics);
        let line_count = buffer.line_count();

        for line_num in start_line..end_line.min(line_count) {
            let y = line_num as f32 * metrics.line_height - self.layout.scroll_y;
            let text = format!("{:>4}", line_num + 1);

            let position = Point::new(
                self.layout.bounds.x + self.layout.padding,
                self.layout.bounds.y + self.layout.padding + y,
            );

            renderer.fill_text(
                iced::advanced::text::Text {
                    content: text,
                    bounds: Size::new(self.layout.gutter_width, metrics.line_height),
                    size: iced::Pixels(self.theme.font_size),
                    line_height: iced::widget::text::LineHeight::default(),
                    font: self.theme.font,
                    horizontal_alignment: iced::alignment::Horizontal::Right,
                    vertical_alignment: iced::alignment::Vertical::Top,
                    shaping: iced::widget::text::Shaping::Basic,
                    wrapping: iced::widget::text::Wrapping::None,
                },
                position,
                self.theme.line_number_color,
                self.layout.bounds,
            );
        }
    }

    /// Draws the main text content using cosmic_text layout runs
    fn draw_text_content(
        &self,
        renderer: &mut Renderer,
        buffer: &TextBuffer,
        _glyph_cache: &mut GlyphCache,
    ) {
        let text_area = self.layout.text_area();
        let metrics = buffer.metrics();

        // For now, use simple text rendering
        // TODO: Implement proper glyph rendering with cosmic_text
        for run in buffer.layout_runs() {
            // Check if this run is visible
            let run_y = run.line_top - self.layout.scroll_y;
            if run_y + metrics.line_height < 0.0 || run_y > text_area.height {
                continue; // Skip invisible runs
            }

            // Draw the run text
            let text_x = text_area.x - self.layout.scroll_x;
            let text_y = text_area.y + run_y;

            renderer.fill_text(
                iced::advanced::text::Text {
                    content: run.text.to_string(),
                    bounds: Size::new(text_area.width, metrics.line_height),
                    size: iced::Pixels(self.theme.font_size),
                    line_height: iced::widget::text::LineHeight::default(),
                    font: self.theme.font,
                    horizontal_alignment: iced::alignment::Horizontal::Left,
                    vertical_alignment: iced::alignment::Vertical::Top,
                    shaping: iced::widget::text::Shaping::Advanced,
                    wrapping: iced::widget::text::Wrapping::None,
                },
                Point::new(text_x, text_y),
                self.theme.text_color,
                self.layout.bounds,
            );
        }
    }

    /// Draws the cursor
    fn draw_cursor(&self, renderer: &mut Renderer, buffer: &TextBuffer, cursor: TextPosition) {
        let _metrics = buffer.metrics();
        let text_area = self.layout.text_area();

        // Better cursor positioning with proper character width
        let char_width = self.theme.char_width();
        let line_height = self.theme.line_height_px();

        // Account for line numbers gutter if shown
        let text_start_x = if self.show_line_numbers {
            text_area.x + self.layout.gutter_width + self.layout.padding
        } else {
            text_area.x + self.layout.padding
        };

        // Calculate cursor position
        let cursor_x =
            text_start_x + (cursor.visual_column as f32 * char_width) - self.layout.scroll_x;
        let cursor_y = text_area.y + (cursor.line as f32 * line_height) - self.layout.scroll_y;

        // Create cursor rectangle (2px wide for visibility)
        let cursor_rect =
            Rectangle::new(Point::new(cursor_x, cursor_y), Size::new(2.0, line_height));

        // Only draw if visible in viewport
        if cursor_rect.x >= text_area.x
            && cursor_rect.x <= text_area.x + text_area.width
            && cursor_rect.y >= text_area.y
            && cursor_rect.y <= text_area.y + text_area.height
        {
            // Draw cursor with a subtle animation effect (could add blinking later)
            let quad = Quad {
                bounds: cursor_rect,
                border: Border::default(),
                shadow: Default::default(),
            };

            renderer.fill_quad(quad, self.theme.cursor_color);

            // Optional: Draw a subtle glow around cursor for better visibility
            let glow_rect = Rectangle::new(
                Point::new(cursor_x - 1.0, cursor_y),
                Size::new(4.0, line_height),
            );

            let glow_quad = Quad {
                bounds: glow_rect,
                border: Border::default(),
                shadow: Default::default(),
            };

            let glow_color = Color::from_rgba(
                self.theme.cursor_color.r,
                self.theme.cursor_color.g,
                self.theme.cursor_color.b,
                0.2, // Semi-transparent glow
            );

            renderer.fill_quad(glow_quad, glow_color);
        }
    }

    /// Draws scrollbars
    fn draw_scrollbars(&self, renderer: &mut Renderer, buffer: &TextBuffer) {
        let metrics = buffer.metrics();
        let (start_line, end_line) = self.layout.visible_line_range(metrics);
        let total_lines = buffer.line_count();

        // Vertical scrollbar
        let v_scrollbar = self
            .layout
            .vertical_scrollbar_bounds(start_line, end_line, total_lines);
        if v_scrollbar.width > 0.0 {
            let track_quad = Quad {
                bounds: Rectangle::new(
                    Point::new(v_scrollbar.x, self.layout.bounds.y),
                    Size::new(v_scrollbar.width, self.layout.bounds.height),
                ),
                border: Border::default(),
                shadow: Default::default(),
            };

            // Draw track
            renderer.fill_quad(track_quad, Color::from_rgba(0.5, 0.5, 0.5, 0.2));

            // Draw thumb
            let thumb_quad = Quad {
                bounds: v_scrollbar,
                border: Border {
                    radius: (v_scrollbar.width / 2.0).into(),
                    ..Default::default()
                },
                shadow: Default::default(),
            };

            renderer.fill_quad(thumb_quad, Color::from_rgba(0.7, 0.7, 0.7, 0.5));
        }

        // Horizontal scrollbar (if needed)
        if let Some(h_scrollbar) = self.layout.horizontal_scrollbar_bounds(buffer) {
            let track_quad = Quad {
                bounds: Rectangle::new(
                    Point::new(self.layout.bounds.x, h_scrollbar.y),
                    Size::new(self.layout.bounds.width, h_scrollbar.height),
                ),
                border: Border::default(),
                shadow: Default::default(),
            };

            // Draw track
            renderer.fill_quad(track_quad, Color::from_rgba(0.5, 0.5, 0.5, 0.2));

            // Draw thumb
            let thumb_quad = Quad {
                bounds: h_scrollbar,
                border: Border {
                    radius: (h_scrollbar.height / 2.0).into(),
                    ..Default::default()
                },
                shadow: Default::default(),
            };

            renderer.fill_quad(thumb_quad, Color::from_rgba(0.7, 0.7, 0.7, 0.5));
        }
    }
}
