//! Efficient buffer rendering with line caching for GPUI.
//!
//! This module provides a performant view of the text buffer that minimizes
//! allocations and re-rendering by caching rendered lines and only updating
//! what has changed.

use gpui::{HighlightStyle, SharedString, StyledText};
use std::{collections::HashMap, ops::Range};
use stoat::EditorState;

/// A rendered line with cached styling information.
pub struct RenderedLine {
    /// The line number (0-indexed)
    pub line_number: usize,
    /// The styled text for this line
    pub styled_text: StyledText,
    /// Hash of the line content for change detection
    pub content_hash: u64,
}

/// Efficient buffer view with line caching for performance.
pub struct BufferView {
    /// Cached rendered lines
    line_cache: HashMap<usize, RenderedLine>,
    /// Currently visible viewport range
    viewport: Range<usize>,
    /// Number of lines to render beyond viewport for smooth scrolling
    overscan: usize,
}

impl BufferView {
    /// Creates a new buffer view with default settings.
    pub fn new() -> Self {
        Self {
            line_cache: HashMap::new(),
            viewport: 0..30, // Default viewport size
            overscan: 5,
        }
    }

    /// Updates the visible viewport range.
    pub fn set_viewport(&mut self, start_line: usize, end_line: usize) {
        self.viewport = start_line..end_line;
        // Clean up cache entries far outside the viewport
        self.cleanup_cache();
    }

    /// Gets the visible lines for rendering, using cache when possible.
    pub fn visible_lines(&mut self, state: &EditorState) -> Vec<RenderedLine> {
        let mut lines = Vec::new();

        // Calculate range with overscan
        let start = self.viewport.start.saturating_sub(self.overscan);
        let end = (self.viewport.end + self.overscan).min(state.line_count());

        for line_idx in start..end {
            if let Some(line_content) = state.line(line_idx) {
                // TODO: Implement proper caching when StyledText supports Clone
                // For now, render every line fresh
                let rendered = self.render_line(line_idx, &line_content, state);
                lines.push(rendered);
            }
        }

        lines
    }

    /// Renders a single line with appropriate styling.
    fn render_line(&self, line_number: usize, content: &str, state: &EditorState) -> RenderedLine {
        // StyledText requires owned strings, so we need to convert
        let content_string = gpui::SharedString::from(content.to_string());
        let mut styled_text = StyledText::new(content_string);

        // Apply syntax highlighting if available
        let highlights = self.compute_highlights(content, state);
        if !highlights.is_empty() {
            styled_text = styled_text.with_highlights(highlights);
        }

        // Apply cursor/selection highlighting
        if let Some(cursor_highlights) = self.compute_cursor_highlights(line_number, state) {
            styled_text = styled_text.with_highlights(cursor_highlights);
        }

        RenderedLine {
            line_number,
            styled_text,
            content_hash: hash_string(content),
        }
    }

    /// Computes syntax highlights for a line.
    fn compute_highlights(
        &self,
        _content: &str,
        _state: &EditorState,
    ) -> Vec<(Range<usize>, HighlightStyle)> {
        // TODO: Integrate with tree-sitter or other syntax highlighting
        // For now, return empty highlights
        vec![]
    }

    /// Computes cursor and selection highlights for a line.
    fn compute_cursor_highlights(
        &self,
        line_number: usize,
        state: &EditorState,
    ) -> Option<Vec<(Range<usize>, HighlightStyle)>> {
        let cursor_pos = state.cursor_position();

        // Check if cursor is on this line
        if cursor_pos.line == line_number {
            let cursor_style = HighlightStyle {
                background_color: Some(gpui::rgba(0x3080FF40).into()),
                ..Default::default()
            };

            // Highlight the character at cursor position
            let start = cursor_pos.column;
            let end = (cursor_pos.column + 1).min(state.line(line_number)?.len());

            return Some(vec![(start..end, cursor_style)]);
        }

        // Check for selection on this line
        if let Some(selection) = state.selection() {
            let selection_style = HighlightStyle {
                background_color: Some(gpui::rgba(0x4080FF30).into()),
                ..Default::default()
            };

            // Calculate intersection of selection with this line
            if selection.start.line <= line_number && selection.end.line >= line_number {
                let line_content = state.line(line_number)?;
                let start = if selection.start.line == line_number {
                    selection.start.column
                } else {
                    0
                };
                let end = if selection.end.line == line_number {
                    selection.end.column.min(line_content.len())
                } else {
                    line_content.len()
                };

                return Some(vec![(start..end, selection_style)]);
            }
        }

        None
    }

    /// Invalidates cached lines that have changed.
    pub fn invalidate_lines(&mut self, changed_lines: Range<usize>) {
        for line_idx in changed_lines {
            self.line_cache.remove(&line_idx);
        }
    }

    /// Invalidates the entire cache.
    pub fn invalidate_all(&mut self) {
        self.line_cache.clear();
    }

    /// Cleans up cache entries that are far from the current viewport.
    fn cleanup_cache(&mut self) {
        let keep_range = self.viewport.start.saturating_sub(self.overscan * 3)
            ..self.viewport.end + self.overscan * 3;

        self.line_cache
            .retain(|&line_idx, _| keep_range.contains(&line_idx));
    }

    /// Returns the number of cached lines.
    pub fn cache_size(&self) -> usize {
        self.line_cache.len()
    }
}

impl Default for BufferView {
    fn default() -> Self {
        Self::new()
    }
}

/// Simple hash function for string content.
fn hash_string(s: &str) -> u64 {
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };

    let mut hasher = DefaultHasher::new();
    s.hash(&mut hasher);
    hasher.finish()
}

/// Performance metrics for buffer rendering.
#[derive(Debug, Default)]
pub struct BufferMetrics {
    /// Number of cache hits
    pub cache_hits: usize,
    /// Number of cache misses
    pub cache_misses: usize,
    /// Total lines rendered
    pub lines_rendered: usize,
    /// Average render time per line in microseconds
    pub avg_render_time_us: f64,
}

impl BufferMetrics {
    /// Resets all metrics.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Returns the cache hit rate as a percentage.
    pub fn cache_hit_rate(&self) -> f64 {
        if self.lines_rendered == 0 {
            0.0
        } else {
            (self.cache_hits as f64 / self.lines_rendered as f64) * 100.0
        }
    }
}
