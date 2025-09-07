//! Rendering pipeline for the text editor.
//!
//! This module handles all rendering operations including text, cursor,
//! selections, line numbers, and scrollbars.

use super::{
    buffer::{TextBuffer, FONT_SYSTEM, SWASH_CACHE},
    cache::GlyphCache,
    layout::EditorLayout,
};
use crate::theme::EditorTheme;
use iced::{
    advanced::{
        image::{self, Renderer as ImageRenderer},
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
    pub scale_factor: f32,
}

impl<'a> EditorRenderer<'a> {
    /// Creates a new renderer with the given configuration
    pub fn new(theme: &'a EditorTheme, layout: &'a EditorLayout, scale_factor: f32) -> Self {
        Self {
            theme,
            layout,
            show_line_numbers: true,
            highlight_current_line: true,
            scale_factor,
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

    /// Draws the main text content using cosmic_text layout runs with proper glyph rendering
    fn draw_text_content(
        &self,
        renderer: &mut Renderer,
        buffer: &TextBuffer,
        _glyph_cache: &mut GlyphCache,
    ) {
        let text_area = self.layout.text_area();
        let metrics = buffer.metrics();

        // Create pixel buffer for text rendering at scaled resolution for high-DPI
        let logical_w = text_area.width.ceil() as u32;
        let logical_h = text_area.height.ceil() as u32;
        let image_w = (logical_w as f32 * self.scale_factor).ceil() as u32;
        let image_h = (logical_h as f32 * self.scale_factor).ceil() as u32;

        // Debug: Text area and font metrics
        // eprintln!("DEBUG: Text area size: {}x{}", image_w, image_h);
        // eprintln!("DEBUG: Font metrics - size: {}, line_height: {}", metrics.font_size,
        // metrics.line_height);

        if image_w == 0 || image_h == 0 {
            return;
        }

        // Create RGBA pixel buffer with transparent background
        // IMPORTANT: We store as u32 but need RGBA byte order in memory
        // On little-endian systems, u32 is stored with LSB first
        // So for RGBA bytes [R,G,B,A] we need u32 = 0xAABBGGRR
        // Use transparent pixels (alpha = 0) so background layers show through
        let mut pixels = vec![0u32; (image_w * image_h) as usize];

        // Get the font system and swash cache
        let mut font_system = FONT_SYSTEM.lock().unwrap();
        let mut swash_cache = SWASH_CACHE.lock().unwrap();

        let mut glyph_count = 0;
        let mut pixel_count = 0;

        // Render glyphs to pixel buffer
        for run in buffer.layout_runs() {
            // Check if this run is visible
            let run_y = run.line_top - self.layout.scroll_y;
            if run_y + metrics.line_height < 0.0 || run_y > text_area.height {
                continue; // Skip invisible runs
            }

            // Process each glyph in the run
            for (idx, glyph) in run.glyphs.iter().enumerate() {
                glyph_count += 1;

                // Skip if glyph is outside visible area (simple bounds check)
                if glyph.x < self.layout.scroll_x - 50.0
                    || glyph.x > self.layout.scroll_x + text_area.width + 50.0
                {
                    continue;
                }

                // Use the baseline position that cosmic-text provides
                // run.line_y already contains the correct baseline position for this line
                let run_line_y = run.line_y;

                // Get physical glyph for rendering
                // Pass (0, 0) since we handle positioning ourselves in the pixel calculation
                // Use scale_factor to render at higher resolution for high-DPI
                let physical = glyph.physical((0., 0.), self.scale_factor);

                // Use theme text color
                let text_color = cosmic_text::Color::rgba(
                    (self.theme.text_color.r * 255.0) as u8,
                    (self.theme.text_color.g * 255.0) as u8,
                    (self.theme.text_color.b * 255.0) as u8,
                    (self.theme.text_color.a * 255.0) as u8,
                );

                // Track pixel bounds for this glyph
                let mut min_x = i32::MAX;
                let mut max_x = i32::MIN;
                let mut glyph_pixel_count = 0;

                swash_cache.with_pixels(
                    &mut *font_system,
                    physical.cache_key,
                    text_color,
                    |x, y, color| {
                        // Calculate final pixel position
                        // glyph.x already contains the horizontal position from cosmic-text
                        // physical.x/y contain the glyph's rendered position offset (includes
                        // baseline) x/y are the pixel offsets within the
                        // glyph bitmap

                        // Calculate pixel position from glyph position
                        // Scale up positions for high-DPI rendering
                        let px = ((glyph.x - self.layout.scroll_x) * self.scale_factor) as i32 + x;
                        let py =
                            ((run_line_y - self.layout.scroll_y) * self.scale_factor) as i32 + y;

                        // Track the actual pixel bounds of this glyph
                        min_x = min_x.min(x);
                        max_x = max_x.max(x);
                        glyph_pixel_count += 1;

                        if px >= 0 && px < image_w as i32 && py >= 0 && py < image_h as i32 {
                            let idx = (py * image_w as i32 + px) as usize;
                            if idx < pixels.len() {
                                pixel_count += 1;
                                // Extract ARGB components from cosmic-text
                                let argb = color.0;

                                // Convert ARGB to RGBA format for iced
                                // cosmic-text gives us ARGB, we need RGBA
                                let alpha = (argb >> 24) & 0xFF;
                                let text_r = (argb >> 16) & 0xFF;
                                let text_g = (argb >> 8) & 0xFF;
                                let text_b = argb & 0xFF;

                                match alpha {
                                    0 => {
                                        // Fully transparent, skip
                                    },
                                    255 => {
                                        // Fully opaque, direct write
                                        // Pack as 0xAABBGGRR for little-endian RGBA byte order
                                        let rgba =
                                            text_r | (text_g << 8) | (text_b << 16) | (0xFF << 24);
                                        pixels[idx] = rgba;
                                    },
                                    _ => {
                                        // Alpha blend using integer math (like cosmic-edit)
                                        let existing = pixels[idx];
                                        // Unpack from 0xAABBGGRR format
                                        let bg_r = existing & 0xFF;
                                        let bg_g = (existing >> 8) & 0xFF;
                                        let bg_b = (existing >> 16) & 0xFF;

                                        let inv_alpha = 255 - alpha;

                                        // Blend each channel using integer math
                                        let r = ((text_r * alpha + bg_r * inv_alpha) / 255) & 0xFF;
                                        let g = ((text_g * alpha + bg_g * inv_alpha) / 255) & 0xFF;
                                        let b = ((text_b * alpha + bg_b * inv_alpha) / 255) & 0xFF;

                                        // Pack as 0xAABBGGRR for little-endian RGBA byte order
                                        let rgba = r | (g << 8) | (b << 16) | (0xFF << 24);
                                        pixels[idx] = rgba;
                                    },
                                }
                                // Debug first few pixels
                                // if pixel_count <= 10 {
                                //     eprintln!("      Pixel at ({}, {}): ARGB={:08X} ->
                                // RGBA={:08X}", px, py, argb, rgba);
                                // }
                            }
                        }
                    },
                );
            }
        }

        // eprintln!("DEBUG: Rendered {} glyphs, {} pixels modified", glyph_count, pixel_count);

        // Convert pixel buffer to bytes for image
        let pixels_u8 =
            unsafe { std::slice::from_raw_parts(pixels.as_ptr() as *const u8, pixels.len() * 4) };

        // Create image handle from pixel buffer
        let handle = image::Handle::from_rgba(image_w, image_h, pixels_u8.to_vec());

        // Draw the image to the screen at logical size (scaled down from physical size)
        let image_bounds = Rectangle::new(
            Point::new(text_area.x, text_area.y),
            Size::new(logical_w as f32, logical_h as f32),
        );

        // eprintln!("DEBUG: Drawing image at {:?}", image_bounds);

        // Use nearest filtering for pixel-perfect text
        let mut img = image::Image::new(handle);
        img.filter_method = image::FilterMethod::Nearest;

        <Renderer as ImageRenderer>::draw_image(renderer, img, image_bounds);
    }

    /// Draws the cursor
    fn draw_cursor(&self, renderer: &mut Renderer, buffer: &TextBuffer, cursor: TextPosition) {
        let _metrics = buffer.metrics();
        let text_area = self.layout.text_area();

        // Better cursor positioning with proper character width
        let char_width = self.theme.char_width();
        let line_height = self.theme.line_height_px();

        // Text is rendered starting at text_area.x without padding
        // Cursor should align with the text
        let text_start_x = text_area.x;

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

/// Blends a foreground pixel with a background pixel using alpha blending
fn blend_pixel(background: u32, foreground: u32) -> u32 {
    // Extract RGBA components from foreground (cosmic-text format: RGBA)
    let fg_a = ((foreground >> 24) & 0xFF) as f32 / 255.0;
    let fg_r = ((foreground >> 16) & 0xFF) as f32 / 255.0;
    let fg_g = ((foreground >> 8) & 0xFF) as f32 / 255.0;
    let fg_b = (foreground & 0xFF) as f32 / 255.0;

    // Extract RGBA components from background
    let bg_a = ((background >> 24) & 0xFF) as f32 / 255.0;
    let bg_r = ((background >> 16) & 0xFF) as f32 / 255.0;
    let bg_g = ((background >> 8) & 0xFF) as f32 / 255.0;
    let bg_b = (background & 0xFF) as f32 / 255.0;

    // Alpha blend
    let out_a = fg_a + bg_a * (1.0 - fg_a);
    let out_r = if out_a > 0.0 {
        (fg_r * fg_a + bg_r * bg_a * (1.0 - fg_a)) / out_a
    } else {
        0.0
    };
    let out_g = if out_a > 0.0 {
        (fg_g * fg_a + bg_g * bg_a * (1.0 - fg_a)) / out_a
    } else {
        0.0
    };
    let out_b = if out_a > 0.0 {
        (fg_b * fg_a + bg_b * bg_a * (1.0 - fg_a)) / out_a
    } else {
        0.0
    };

    // Convert back to u32
    ((out_a * 255.0) as u32) << 24
        | ((out_r * 255.0) as u32) << 16
        | ((out_g * 255.0) as u32) << 8
        | ((out_b * 255.0) as u32)
}
