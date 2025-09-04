//! cosmic-text Buffer wrapper and text management.
//!
//! This module provides integration with cosmic-text for proper text shaping,
//! layout, and rendering. It handles font systems, metrics, and text buffers.

use cosmic_text::{Attrs, Buffer, Family, FontSystem, Metrics, Shaping};
use once_cell::sync::Lazy;
use std::sync::Mutex;

/// Global font system instance (cosmic-text requires a singleton)
pub static FONT_SYSTEM: Lazy<Mutex<FontSystem>> = Lazy::new(|| Mutex::new(FontSystem::new()));

/// Wrapper around cosmic-text Buffer with our text management
pub struct TextBuffer {
    /// The underlying cosmic-text buffer
    buffer: Buffer,
    /// Text attributes (font family, size, etc.)
    attrs: Attrs<'static>,
    /// Tab width in spaces
    _tab_width: usize,
}

impl TextBuffer {
    /// Creates a new text buffer with the given metrics
    pub fn new(metrics: Metrics, tab_width: usize) -> Self {
        let attrs = Attrs::new().family(Family::Monospace);
        let mut buffer = Buffer::new_empty(metrics);

        // Set initial empty text
        {
            let mut font_system = FONT_SYSTEM.lock().unwrap();
            buffer.set_text(&mut font_system, "", attrs, Shaping::Advanced);
        }

        // Set tab width
        {
            let mut font_system = FONT_SYSTEM.lock().unwrap();
            buffer.set_tab_width(&mut font_system, tab_width as u16);
        }

        Self {
            buffer,
            attrs,
            _tab_width: tab_width,
        }
    }

    /// Sets the text content of the buffer
    pub fn set_text(&mut self, text: &str) {
        let mut font_system = FONT_SYSTEM.lock().unwrap();
        self.buffer
            .set_text(&mut font_system, text, self.attrs, Shaping::Advanced);
    }

    /// Updates metrics (font size, line height)
    pub fn set_metrics(&mut self, metrics: Metrics) {
        let mut font_system = FONT_SYSTEM.lock().unwrap();
        self.buffer.set_metrics(&mut font_system, metrics);
    }

    /// Sets the tab width in spaces
    pub fn set_tab_width(&mut self, width: usize) {
        self._tab_width = width;
        let mut font_system = FONT_SYSTEM.lock().unwrap();
        self.buffer.set_tab_width(&mut font_system, width as u16);
    }

    /// Shapes the text as needed for layout
    pub fn shape_as_needed(&mut self) {
        let mut font_system = FONT_SYSTEM.lock().unwrap();
        self.buffer.shape_until_scroll(&mut font_system, true);
    }

    /// Returns the buffer size (width, height) in pixels
    pub fn size(&self) -> (Option<f32>, Option<f32>) {
        self.buffer.size()
    }

    /// Iterates over layout runs for rendering
    pub fn layout_runs(&self) -> impl Iterator<Item = cosmic_text::LayoutRun<'_>> + '_ {
        self.buffer.layout_runs()
    }

    /// Gets the metrics for this buffer
    pub fn metrics(&self) -> Metrics {
        self.buffer.metrics()
    }

    /// Converts a point (x, y) to a text position
    pub fn hit(&self, x: f32, y: f32) -> Option<cosmic_text::Cursor> {
        self.buffer.hit(x, y)
    }

    /// Gets the number of lines in the buffer
    pub fn line_count(&self) -> usize {
        self.buffer.lines.len()
    }

    /// Access to the underlying buffer for advanced operations
    pub fn inner(&self) -> &Buffer {
        &self.buffer
    }

    /// Mutable access to the underlying buffer
    pub fn inner_mut(&mut self) -> &mut Buffer {
        &mut self.buffer
    }
}

/// Helper to calculate visual columns accounting for tabs
pub fn calculate_visual_column(text: &str, byte_offset: usize, tab_width: usize) -> usize {
    let mut visual_col = 0;
    let mut byte_pos = 0;

    for ch in text.chars() {
        if byte_pos >= byte_offset {
            break;
        }

        if ch == '\t' {
            // Tab extends to next tab stop
            visual_col += tab_width - (visual_col % tab_width);
        } else {
            visual_col += 1;
        }

        byte_pos += ch.len_utf8();
    }

    visual_col
}

/// Helper to convert visual column to byte offset
pub fn visual_column_to_byte_offset(
    text: &str,
    target_visual_col: usize,
    tab_width: usize,
) -> usize {
    let mut visual_col = 0;
    let mut byte_offset = 0;

    for ch in text.chars() {
        if visual_col >= target_visual_col {
            break;
        }

        if ch == '\t' {
            let tab_cols = tab_width - (visual_col % tab_width);
            if visual_col + tab_cols > target_visual_col {
                // Target is in the middle of this tab
                break;
            }
            visual_col += tab_cols;
        } else {
            visual_col += 1;
        }

        byte_offset += ch.len_utf8();
    }

    byte_offset
}
