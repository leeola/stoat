//! Buffer adapter for bridging stoat's rope with GPUI rendering

use gpui::{App, Context, Entity, EventEmitter};
use parking_lot::RwLock;
use std::{ops::Range, sync::Arc};
use stoat::{EditorEngine, EditorState};

/// Buffer adapter that bridges stoat's rope with GPUI
pub struct Buffer {
    engine: Arc<RwLock<EditorEngine>>,
    /// Cached line information for efficient rendering
    line_cache: Vec<LineInfo>,
    /// Version counter for invalidation
    version: usize,
}

#[derive(Clone, Debug)]
pub struct LineInfo {
    /// Start byte offset in the rope
    pub start_offset: usize,
    /// End byte offset (including newline if present)
    pub end_offset: usize,
    /// Text content without newline
    pub text: String,
    /// Whether this line ends with a newline
    pub has_newline: bool,
}

impl Buffer {
    pub fn new(engine: Arc<RwLock<EditorEngine>>) -> Self {
        let mut buffer = Self {
            engine,
            line_cache: Vec::new(),
            version: 0,
        };
        buffer.rebuild_line_cache();
        buffer
    }

    /// Rebuild the line cache from the current buffer state
    fn rebuild_line_cache(&mut self) {
        self.line_cache.clear();
        let engine = self.engine.read();
        let state = engine.state();
        let text = state.text();

        if text.is_empty() {
            self.line_cache.push(LineInfo {
                start_offset: 0,
                end_offset: 0,
                text: String::new(),
                has_newline: false,
            });
            return;
        }

        let mut offset = 0;
        for line in text.lines() {
            let line_len = line.len();
            let has_newline = offset + line_len < text.len();

            self.line_cache.push(LineInfo {
                start_offset: offset,
                end_offset: offset + line_len + if has_newline { 1 } else { 0 },
                text: line.to_string(),
                has_newline,
            });

            offset += line_len + if has_newline { 1 } else { 0 };
        }

        // Handle case where text ends with newline
        if text.ends_with('\n') {
            self.line_cache.push(LineInfo {
                start_offset: text.len(),
                end_offset: text.len(),
                text: String::new(),
                has_newline: false,
            });
        }

        self.version += 1;
    }

    /// Get the number of lines in the buffer
    pub fn line_count(&self) -> usize {
        self.line_cache.len()
    }

    /// Get a specific line
    pub fn line(&self, index: usize) -> Option<&LineInfo> {
        self.line_cache.get(index)
    }

    /// Get lines in a range
    pub fn lines_in_range(&self, range: Range<usize>) -> &[LineInfo] {
        let start = range.start.min(self.line_cache.len());
        let end = range.end.min(self.line_cache.len());
        &self.line_cache[start..end]
    }

    /// Get the full text content
    pub fn text(&self) -> String {
        self.engine.read().state().text()
    }

    /// Get text for a byte range
    pub fn text_for_range(&self, range: Range<usize>) -> String {
        let text = self.text();
        let start = range.start.min(text.len());
        let end = range.end.min(text.len());
        text[start..end].to_string()
    }

    /// Convert a byte offset to a line and column
    pub fn offset_to_point(&self, offset: usize) -> (usize, usize) {
        let mut remaining = offset;

        for (line_idx, line) in self.line_cache.iter().enumerate() {
            if remaining <= line.text.len() {
                return (line_idx, remaining);
            }
            remaining -= line.text.len() + if line.has_newline { 1 } else { 0 };
        }

        // Past end of buffer
        let last_line = self.line_cache.len().saturating_sub(1);
        let last_col = self.line_cache.last().map(|l| l.text.len()).unwrap_or(0);
        (last_line, last_col)
    }

    /// Convert a line and column to a byte offset
    pub fn point_to_offset(&self, line: usize, column: usize) -> usize {
        let mut offset = 0;

        for (idx, line_info) in self.line_cache.iter().enumerate() {
            if idx == line {
                return offset + column.min(line_info.text.len());
            }
            offset += line_info.text.len() + if line_info.has_newline { 1 } else { 0 };
        }

        offset
    }

    /// Update the buffer (called when the engine state changes)
    pub fn update(&mut self, cx: &mut Context<Self>) {
        self.rebuild_line_cache();
        cx.notify();
    }

    /// Get the current version for cache invalidation
    pub fn version(&self) -> usize {
        self.version
    }

    /// Get visible lines for a viewport
    pub fn visible_lines(&self, start_line: usize, height: usize) -> Vec<RenderedLine> {
        let end_line = (start_line + height).min(self.line_count());
        let engine = self.engine.read();
        let state = engine.state();
        let cursor_pos = state.cursor_position();
        let cursor_line = cursor_pos.line as usize;
        let cursor_col = cursor_pos.column as usize;

        let mut lines = Vec::with_capacity(height);

        for line_idx in start_line..end_line {
            if let Some(line_info) = self.line(line_idx) {
                lines.push(RenderedLine {
                    line_number: line_idx + 1,
                    text: line_info.text.clone(),
                    has_cursor: line_idx == cursor_line,
                    cursor_column: if line_idx == cursor_line {
                        Some(cursor_col)
                    } else {
                        None
                    },
                    is_selected: false, // FIXME: Implement selection rendering
                });
            }
        }

        // Pad with empty lines if needed
        while lines.len() < height {
            lines.push(RenderedLine {
                line_number: 0,
                text: String::new(),
                has_cursor: false,
                cursor_column: None,
                is_selected: false,
            });
        }

        lines
    }
}

/// A line prepared for rendering
#[derive(Debug, Clone)]
pub struct RenderedLine {
    pub line_number: usize,
    pub text: String,
    pub has_cursor: bool,
    pub cursor_column: Option<usize>,
    pub is_selected: bool,
}

impl EventEmitter<BufferEvent> for Buffer {}

#[derive(Debug, Clone)]
pub enum BufferEvent {
    Changed,
}
