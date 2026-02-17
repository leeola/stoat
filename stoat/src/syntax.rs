use gpui::HighlightStyle;
use std::ops::Range;
use stoat_text::HighlightCapture;
use text::{BufferSnapshot, Chunks};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HighlightId(pub u32);

const DEFAULT_SYNTAX_HIGHLIGHT_ID: HighlightId = HighlightId(u32::MAX);

/// Maps capture indices from a tree-sitter highlight query to [`HighlightId`] values.
///
/// Each language's query produces different capture names at different indices.
/// This map is built once per (theme, query) pair by matching capture names
/// against theme highlight entries using longest-prefix matching.
#[derive(Clone, Debug, Default)]
pub struct HighlightMap {
    ids: Vec<HighlightId>,
}

impl HighlightMap {
    /// Build a map from capture indices to highlight IDs.
    ///
    /// For each capture name (e.g. "function.method"), finds the theme entry
    /// with the longest matching prefix (e.g. "function.method" > "function" > fallback).
    pub fn new(theme: &SyntaxTheme, capture_names: &[String]) -> Self {
        let ids = capture_names
            .iter()
            .map(|capture_name| {
                let mut best_match: Option<(usize, HighlightId)> = None;

                for (id, (theme_name, _)) in theme.highlights.iter().enumerate() {
                    if capture_name == theme_name
                        || capture_name.starts_with(&format!("{theme_name}."))
                    {
                        let match_len = theme_name.len();
                        if best_match.is_none_or(|(best_len, _)| match_len > best_len) {
                            best_match = Some((match_len, HighlightId(id as u32)));
                        }
                    }
                }

                best_match
                    .map(|(_, id)| id)
                    .unwrap_or(DEFAULT_SYNTAX_HIGHLIGHT_ID)
            })
            .collect();

        Self { ids }
    }

    pub fn get(&self, capture_index: u32) -> HighlightId {
        self.ids
            .get(capture_index as usize)
            .copied()
            .unwrap_or(DEFAULT_SYNTAX_HIGHLIGHT_ID)
    }
}

impl HighlightId {
    pub fn is_default(&self) -> bool {
        *self == DEFAULT_SYNTAX_HIGHLIGHT_ID
    }

    pub fn style(&self, theme: &SyntaxTheme) -> Option<HighlightStyle> {
        if self.is_default() {
            return None;
        }
        theme.highlights.get(self.0 as usize).map(|entry| entry.1)
    }

    pub fn name<'a>(&self, theme: &'a SyntaxTheme) -> Option<&'a str> {
        if self.is_default() {
            return None;
        }
        theme.highlights.get(self.0 as usize).map(|e| e.0.as_str())
    }
}

impl Default for HighlightId {
    fn default() -> Self {
        DEFAULT_SYNTAX_HIGHLIGHT_ID
    }
}

#[derive(Clone, Debug)]
pub struct SyntaxTheme {
    pub highlights: Vec<(String, HighlightStyle)>,
    pub background_color: gpui::Hsla,
    pub default_text_color: gpui::Hsla,
}

impl SyntaxTheme {
    pub fn new() -> Self {
        Self {
            highlights: Vec::new(),
            background_color: gpui::rgb(0x1e1e1e).into(),
            default_text_color: gpui::rgb(0xcccccc).into(),
        }
    }

    pub fn add_highlight(&mut self, name: impl Into<String>, style: HighlightStyle) {
        self.highlights.push((name.into(), style));
    }

    pub fn monokai_dark() -> Self {
        use gpui::{rgba, FontWeight};

        let mut theme = Self::new();

        let background = rgba(0x272822ff);
        let foreground = rgba(0xf8f8f2ff);
        let comment = rgba(0x75715eff);
        let red = rgba(0xf92672ff);
        let orange = rgba(0xfd971fff);
        let yellow = rgba(0xe6db74ff);
        let green = rgba(0xa6e22eff);
        let cyan = rgba(0x66d9efff);
        let purple = rgba(0xae81ffff);

        theme.background_color = background.into();
        theme.default_text_color = foreground.into();

        theme.add_highlight(
            "keyword",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "string",
            HighlightStyle {
                color: Some(yellow.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "string.escape",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "string.special",
            HighlightStyle {
                color: Some(orange.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "number",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "boolean",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "constant",
            HighlightStyle {
                color: Some(orange.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "comment",
            HighlightStyle {
                color: Some(comment.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function",
            HighlightStyle {
                color: Some(green.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function.method",
            HighlightStyle {
                color: Some(green.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function.definition",
            HighlightStyle {
                color: Some(green.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function.special",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function.macro",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "variable",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "variable.special",
            HighlightStyle {
                color: Some(purple.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "variable.builtin",
            HighlightStyle {
                color: Some(purple.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "variable.parameter",
            HighlightStyle {
                color: Some(orange.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "property",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "type",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "type.builtin",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "type.interface",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "constructor",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "lifetime",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "label",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "attribute",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "operator",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "escape",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "punctuation",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "punctuation.bracket",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "punctuation.delimiter",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "punctuation.special",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "markup.heading",
            HighlightStyle {
                color: Some(green.into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "markup.bold",
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "markup.italic",
            HighlightStyle {
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "markup.code",
            HighlightStyle {
                color: Some(yellow.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "text.title",
            HighlightStyle {
                color: Some(green.into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "text.literal",
            HighlightStyle {
                color: Some(yellow.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "text.uri",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        theme
    }

    pub fn monokai_light() -> Self {
        use gpui::{rgba, FontWeight};

        let mut theme = Self::new();

        let background = rgba(0xfafafaff);
        let foreground = rgba(0x272822ff);
        let comment = rgba(0x75715eff);
        let red = rgba(0xd9006cff);
        let orange = rgba(0xe67f00ff);
        let yellow = rgba(0xc9a500ff);
        let green = rgba(0x6d9d00ff);
        let cyan = rgba(0x0099ccff);
        let purple = rgba(0x9933ffff);

        theme.background_color = background.into();
        theme.default_text_color = foreground.into();

        theme.add_highlight(
            "keyword",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "string",
            HighlightStyle {
                color: Some(yellow.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "string.escape",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "string.special",
            HighlightStyle {
                color: Some(orange.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "number",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "boolean",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "constant",
            HighlightStyle {
                color: Some(orange.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "comment",
            HighlightStyle {
                color: Some(comment.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function",
            HighlightStyle {
                color: Some(green.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function.method",
            HighlightStyle {
                color: Some(green.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function.definition",
            HighlightStyle {
                color: Some(green.into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function.special",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function.macro",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "variable",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "variable.special",
            HighlightStyle {
                color: Some(purple.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "variable.builtin",
            HighlightStyle {
                color: Some(purple.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "variable.parameter",
            HighlightStyle {
                color: Some(orange.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "property",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "type",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "type.builtin",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "type.interface",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "constructor",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "lifetime",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "label",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "attribute",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "operator",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "escape",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "punctuation",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "punctuation.bracket",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "punctuation.delimiter",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "punctuation.special",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "markup.heading",
            HighlightStyle {
                color: Some(green.into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "markup.bold",
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "markup.italic",
            HighlightStyle {
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "markup.code",
            HighlightStyle {
                color: Some(yellow.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "text.title",
            HighlightStyle {
                color: Some(green.into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "text.literal",
            HighlightStyle {
                color: Some(yellow.into()),
                ..Default::default()
            },
        );
        theme.add_highlight(
            "text.uri",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        theme
    }
}

impl Default for SyntaxTheme {
    fn default() -> Self {
        Self::monokai_dark()
    }
}

#[derive(Clone, Debug)]
pub struct HighlightedChunk<'a> {
    pub text: &'a str,
    pub highlight_id: Option<HighlightId>,
}

/// Iterator that yields text chunks with syntax highlighting from tree-sitter captures.
///
/// Walks rope chunks while maintaining a capture index pointer. For overlapping
/// captures at a given position, the last one wins (innermost/most-specific),
/// matching tree-sitter's pattern ordering convention.
pub struct HighlightedChunks<'a> {
    text_chunks: Chunks<'a>,
    current_text_remaining: &'a str,
    captures: &'a [HighlightCapture],
    capture_idx: usize,
    highlight_map: &'a HighlightMap,
    current_offset: usize,
    end_offset: usize,
}

impl<'a> HighlightedChunks<'a> {
    pub fn new(
        range: Range<usize>,
        buffer_snapshot: &'a BufferSnapshot,
        captures: &'a [HighlightCapture],
        highlight_map: &'a HighlightMap,
    ) -> Self {
        let text_chunks = buffer_snapshot.as_rope().chunks_in_range(range.clone());

        let mut capture_idx = 0;
        while capture_idx < captures.len() && captures[capture_idx].byte_range.end <= range.start {
            capture_idx += 1;
        }

        Self {
            text_chunks,
            current_text_remaining: "",
            captures,
            capture_idx,
            highlight_map,
            current_offset: range.start,
            end_offset: range.end,
        }
    }
}

impl<'a> Iterator for HighlightedChunks<'a> {
    type Item = HighlightedChunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.current_offset >= self.end_offset {
            return None;
        }

        if self.current_text_remaining.is_empty() {
            self.current_text_remaining = self.text_chunks.next()?;
        }

        // Advance past captures that ended before current position
        while self.capture_idx < self.captures.len()
            && self.captures[self.capture_idx].byte_range.end <= self.current_offset
        {
            self.capture_idx += 1;
        }

        // Find the active capture at current_offset (last one wins for overlaps)
        let mut active_capture: Option<&HighlightCapture> = None;
        for cap in &self.captures[self.capture_idx..] {
            if cap.byte_range.start > self.current_offset {
                break;
            }
            if cap.byte_range.start <= self.current_offset
                && self.current_offset < cap.byte_range.end
            {
                active_capture = Some(cap);
            }
        }

        let highlight_id = active_capture
            .map(|cap| self.highlight_map.get(cap.capture_index))
            .filter(|id| !id.is_default());

        // Determine chunk end: min of text chunk end, range end, and next capture boundary
        let mut chunk_end = self.current_offset + self.current_text_remaining.len();
        chunk_end = chunk_end.min(self.end_offset);

        if let Some(cap) = active_capture {
            chunk_end = chunk_end.min(cap.byte_range.end);
        }

        // Clip to the start of the next non-overlapping capture
        for cap in &self.captures[self.capture_idx..] {
            if cap.byte_range.start > self.current_offset && cap.byte_range.start < chunk_end {
                chunk_end = cap.byte_range.start;
                break;
            }
        }

        let text_to_take = (chunk_end - self.current_offset).min(self.current_text_remaining.len());
        let (chunk_text, remaining_text) = self.current_text_remaining.split_at(text_to_take);
        self.current_text_remaining = remaining_text;
        self.current_offset += text_to_take;

        Some(HighlightedChunk {
            text: chunk_text,
            highlight_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_text::{HighlightQuery, Language, Parser};
    use text::{Buffer, BufferId};

    #[test]
    fn highlight_map_prefix_matching() {
        let mut theme = SyntaxTheme::new();
        theme.add_highlight(
            "function",
            HighlightStyle {
                ..Default::default()
            },
        );
        theme.add_highlight(
            "function.method",
            HighlightStyle {
                ..Default::default()
            },
        );

        let capture_names = vec![
            "function".to_string(),
            "function.method".to_string(),
            "function.macro".to_string(),
            "unknown_thing".to_string(),
        ];

        let map = HighlightMap::new(&theme, &capture_names);

        assert_eq!(map.get(0), HighlightId(0));
        assert_eq!(map.get(1), HighlightId(1));
        assert_eq!(map.get(2), HighlightId(0));
        assert!(map.get(3).is_default());
    }

    #[test]
    fn highlighted_chunks_basic() {
        let source = "fn main() {}";
        let buffer = Buffer::new(0, BufferId::new(1).unwrap(), source.to_string());
        let snapshot = buffer.snapshot();

        let mut parser = Parser::new(Language::Rust).unwrap();
        parser.parse(source).unwrap();
        let query = HighlightQuery::new(Language::Rust).unwrap();
        let tree = parser.tree().unwrap();
        let captures = query.captures(tree, source.as_bytes(), 0..source.len());

        let theme = SyntaxTheme::monokai_dark();
        let highlight_map = HighlightMap::new(&theme, query.capture_names());

        let chunks: Vec<_> =
            HighlightedChunks::new(0..source.len(), &snapshot, &captures, &highlight_map).collect();

        assert!(!chunks.is_empty());

        let full_text: String = chunks.iter().map(|c| c.text).collect();
        assert_eq!(full_text, source);

        let fn_chunk = chunks.iter().find(|c| c.text == "fn").unwrap();
        assert!(fn_chunk.highlight_id.is_some());
    }

    #[test]
    fn highlighted_chunks_plain_text() {
        let source = "hello world";
        let buffer = Buffer::new(0, BufferId::new(1).unwrap(), source.to_string());
        let snapshot = buffer.snapshot();
        let captures = vec![];
        let theme = SyntaxTheme::monokai_dark();
        let highlight_map = HighlightMap::default();

        let chunks: Vec<_> =
            HighlightedChunks::new(0..source.len(), &snapshot, &captures, &highlight_map).collect();

        let full_text: String = chunks.iter().map(|c| c.text).collect();
        assert_eq!(full_text, source);
        assert!(chunks.iter().all(|c| c.highlight_id.is_none()));
    }
}
