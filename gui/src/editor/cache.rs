//! Performance caching for the text editor.
//!
//! This module provides caching for layout calculations and glyph rendering
//! to improve performance.

use cosmic_text::CacheKey;
use iced::advanced::image;
use std::collections::HashMap;
use swash::scale::ScaleContext;

/// Cache for glyph rendering
pub struct GlyphCache {
    /// Swash scale context for glyph rasterization
    scale_context: ScaleContext,
    /// Cached glyph images
    glyph_cache: HashMap<CacheKey, Vec<u8>>,
}

impl Clone for GlyphCache {
    fn clone(&self) -> Self {
        // ScaleContext doesn't implement Clone, so create a new one
        Self {
            scale_context: ScaleContext::new(),
            glyph_cache: self.glyph_cache.clone(),
        }
    }
}

impl Default for GlyphCache {
    fn default() -> Self {
        Self::new()
    }
}

impl GlyphCache {
    /// Creates a new glyph cache
    pub fn new() -> Self {
        Self {
            scale_context: ScaleContext::new(),
            glyph_cache: HashMap::new(),
        }
    }

    /// Gets or renders a glyph
    pub fn with_glyph<F>(&mut self, _key: CacheKey, f: F)
    where
        F: FnOnce(&[u8]),
    {
        // For now, use placeholder data
        // In a real implementation, you'd use swash to render the glyph
        let placeholder = vec![255u8; 64]; // 8x8 placeholder
        f(&placeholder);
    }

    /// Clears the glyph cache
    pub fn clear(&mut self) {
        self.glyph_cache.clear();
    }
}

/// Cache for layout calculations
pub struct LayoutCache {
    /// Cached text measurements
    measurements: HashMap<String, (f32, f32)>,
    /// Cached image handles for text rendering
    image_cache: HashMap<u64, image::Handle>,
    /// Frame counter for cache invalidation
    frame: u64,
}

impl Default for LayoutCache {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutCache {
    /// Creates a new layout cache
    pub fn new() -> Self {
        Self {
            measurements: HashMap::new(),
            image_cache: HashMap::new(),
            frame: 0,
        }
    }

    /// Measures text, using cache if available
    pub fn measure_text(&mut self, text: &str, font_size: f32) -> (f32, f32) {
        let key = format!("{}:{}", text, font_size);

        if let Some(&measurement) = self.measurements.get(&key) {
            return measurement;
        }

        // Approximate measurement for now
        let width = text.len() as f32 * font_size * 0.6;
        let height = font_size * 1.2;

        self.measurements.insert(key, (width, height));
        (width, height)
    }

    /// Gets or creates an image handle for rendered text
    pub fn get_or_create_image(
        &mut self,
        key: u64,
        create: impl FnOnce() -> image::Handle,
    ) -> image::Handle {
        self.image_cache.entry(key).or_insert_with(create).clone()
    }

    /// Advances to the next frame
    pub fn next_frame(&mut self) {
        self.frame += 1;

        // Clear old cache entries periodically
        if self.frame % 100 == 0 {
            self.clear_old_entries();
        }
    }

    /// Clears old cache entries
    fn clear_old_entries(&mut self) {
        // Keep only recent entries
        if self.measurements.len() > 1000 {
            self.measurements.clear();
        }
        if self.image_cache.len() > 100 {
            self.image_cache.clear();
        }
    }

    /// Clears the entire cache
    pub fn clear(&mut self) {
        self.measurements.clear();
        self.image_cache.clear();
        self.frame = 0;
    }
}

/// Viewport culling helper
pub struct ViewportCuller {
    /// First visible line
    pub start_line: usize,
    /// Last visible line
    pub end_line: usize,
    /// First visible column
    pub start_col: usize,
    /// Last visible column
    pub end_col: usize,
}

impl ViewportCuller {
    /// Creates a new viewport culler from scroll position and dimensions
    pub fn new(
        scroll_x: f32,
        scroll_y: f32,
        viewport_width: f32,
        viewport_height: f32,
        char_width: f32,
        line_height: f32,
    ) -> Self {
        let start_line = (scroll_y / line_height).floor() as usize;
        let visible_lines = (viewport_height / line_height).ceil() as usize + 1;
        let end_line = start_line + visible_lines;

        let start_col = (scroll_x / char_width).floor() as usize;
        let visible_cols = (viewport_width / char_width).ceil() as usize + 1;
        let end_col = start_col + visible_cols;

        Self {
            start_line,
            end_line,
            start_col,
            end_col,
        }
    }

    /// Checks if a line is visible
    pub fn is_line_visible(&self, line: usize) -> bool {
        line >= self.start_line && line < self.end_line
    }

    /// Checks if a column range is visible
    pub fn is_column_range_visible(&self, start: usize, end: usize) -> bool {
        end >= self.start_col && start < self.end_col
    }
}
