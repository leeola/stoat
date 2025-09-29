//! Syntax highlighting system for Stoat
//!
//! This module provides syntax highlighting capabilities by adapting Zed's proven
//! architecture to work with Stoat's TokenMap-based AST system.
//!
//! ## Architecture
//!
//! The highlighting system consists of several key components:
//!
//! - [`HighlightMap`] - Maps syntax tokens to highlight IDs for efficient lookup
//! - [`SyntaxTheme`] - Defines color schemes and styles for different highlight categories
//! - [`HighlightedChunks`] - Iterator that provides text chunks with highlighting information
//!
//! ## Integration with Stoat
//!
//! The system integrates with Stoat's existing [`TokenMap`] infrastructure to provide
//! incremental syntax highlighting that updates efficiently as the user edits text.
//! Rather than parsing the entire document on each change, it leverages the token-based
//! approach for fast incremental updates.
//!
//! ## Performance
//!
//! The implementation follows Zed's performance patterns:
//! - Chunked text iteration for consistent styling within runs
//! - Cached highlight calculations per token type
//! - Incremental updates only for edited ranges
//! - Batched GPU text shaping operations

use gpui::HighlightStyle;
use rustc_hash::FxHashMap;
use std::ops::Range;
use stoat_rope_v3::{SyntaxKind, TokenSnapshot};
use text::{BufferSnapshot, Chunks, ToOffset};

/// A unique identifier for a syntax highlight style
///
/// This provides efficient lookup and comparison of highlight styles without
/// storing the full style information in each token.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct HighlightId(pub u32);

/// The default highlight ID used when no specific highlighting applies
const DEFAULT_SYNTAX_HIGHLIGHT_ID: HighlightId = HighlightId(u32::MAX);

/// Maps [`SyntaxKind`] tokens to [`HighlightId`] values for efficient syntax highlighting
///
/// This provides fast lookup from Stoat's token types to GPUI highlight styles.
/// The mapping is built once per theme and cached for performance.
#[derive(Clone, Debug)]
pub struct HighlightMap {
    /// Maps each SyntaxKind to its corresponding HighlightId
    mappings: FxHashMap<SyntaxKind, HighlightId>,
}

impl HighlightMap {
    /// Create a new highlight map for the given theme
    ///
    /// This analyzes the theme's highlight styles and creates efficient mappings
    /// from Stoat's [`SyntaxKind`] tokens to the appropriate highlight IDs.
    pub fn new(theme: &SyntaxTheme) -> Self {
        let mut mappings = FxHashMap::default();

        // Map syntax kinds to theme highlight IDs based on semantic meaning
        for (id, (name, _style)) in theme.highlights.iter().enumerate() {
            let highlight_id = HighlightId(id as u32);

            // Map based on common syntax highlighting categories
            match name.as_str() {
                // Keywords and language constructs
                "keyword" => mappings.insert(SyntaxKind::Keyword, highlight_id),

                // Literals
                "string" => {
                    mappings.insert(SyntaxKind::String, highlight_id);
                    mappings.insert(SyntaxKind::Char, highlight_id)
                },
                "number" => mappings.insert(SyntaxKind::Number, highlight_id),
                "boolean" => mappings.insert(SyntaxKind::Boolean, highlight_id),

                // Comments
                "comment" => {
                    mappings.insert(SyntaxKind::LineComment, highlight_id);
                    mappings.insert(SyntaxKind::BlockComment, highlight_id);
                    mappings.insert(SyntaxKind::DocComment, highlight_id)
                },

                // Identifiers and names
                "variable" | "identifier" => mappings.insert(SyntaxKind::Identifier, highlight_id),

                // Operators and punctuation
                "operator" => mappings.insert(SyntaxKind::Operator, highlight_id),
                "punctuation" => {
                    mappings.insert(SyntaxKind::Comma, highlight_id);
                    mappings.insert(SyntaxKind::Semicolon, highlight_id);
                    mappings.insert(SyntaxKind::Colon, highlight_id);
                    mappings.insert(SyntaxKind::Dot, highlight_id)
                },
                "bracket" => {
                    mappings.insert(SyntaxKind::OpenParen, highlight_id);
                    mappings.insert(SyntaxKind::CloseParen, highlight_id);
                    mappings.insert(SyntaxKind::OpenBracket, highlight_id);
                    mappings.insert(SyntaxKind::CloseBracket, highlight_id);
                    mappings.insert(SyntaxKind::OpenBrace, highlight_id);
                    mappings.insert(SyntaxKind::CloseBrace, highlight_id)
                },

                // Markdown-specific
                "markup.heading" => mappings.insert(SyntaxKind::Heading, highlight_id),
                "markup.bold" => mappings.insert(SyntaxKind::Strong, highlight_id),
                "markup.italic" => mappings.insert(SyntaxKind::Emphasis, highlight_id),
                "markup.code" => {
                    mappings.insert(SyntaxKind::CodeSpan, highlight_id);
                    mappings.insert(SyntaxKind::CodeBlock, highlight_id)
                },

                _ => None,
            };
        }

        Self { mappings }
    }

    /// Get the highlight ID for a given syntax kind
    ///
    /// Returns the appropriate [`HighlightId`] for the token type, or the default
    /// highlight if no specific mapping exists.
    pub fn get(&self, kind: SyntaxKind) -> HighlightId {
        self.mappings
            .get(&kind)
            .copied()
            .unwrap_or(DEFAULT_SYNTAX_HIGHLIGHT_ID)
    }
}

impl HighlightId {
    /// Check if this is the default (unhighlighted) ID
    pub fn is_default(&self) -> bool {
        *self == DEFAULT_SYNTAX_HIGHLIGHT_ID
    }

    /// Get the highlight style for this ID from the theme
    ///
    /// Returns the [`HighlightStyle`] to apply to text with this highlight ID,
    /// or `None` if this is the default unhighlighted text.
    pub fn style(&self, theme: &SyntaxTheme) -> Option<HighlightStyle> {
        if self.is_default() {
            return None;
        }
        theme.highlights.get(self.0 as usize).map(|entry| entry.1)
    }

    /// Get the name of this highlight category from the theme
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

impl Default for HighlightMap {
    fn default() -> Self {
        Self {
            mappings: FxHashMap::default(),
        }
    }
}

/// A syntax theme defining colors and styles for different highlight categories
///
/// This stores the mapping from highlight category names (like "keyword", "string")
/// to their visual styles. Themes can be loaded from external files or defined
/// programmatically.
#[derive(Clone, Debug)]
pub struct SyntaxTheme {
    /// List of highlight definitions: (name, style)
    ///
    /// The index in this vector corresponds to the [`HighlightId`] value,
    /// allowing fast lookup during rendering.
    pub highlights: Vec<(String, HighlightStyle)>,
}

impl SyntaxTheme {
    /// Create a new empty syntax theme
    pub fn new() -> Self {
        Self {
            highlights: Vec::new(),
        }
    }

    /// Add a highlight definition to the theme
    ///
    /// Associates a highlight category name with a visual style. The name
    /// is used by [`HighlightMap`] to map syntax tokens to styles.
    pub fn add_highlight(&mut self, name: impl Into<String>, style: HighlightStyle) {
        self.highlights.push((name.into(), style));
    }

    /// Create a default syntax theme with common programming language highlights
    ///
    /// This provides reasonable defaults for syntax highlighting that work well
    /// with most programming languages and markdown content.
    pub fn default_dark() -> Self {
        use gpui::{rgba, FontWeight};

        let mut theme = Self::new();

        // Keywords - purple/violet
        theme.add_highlight(
            "keyword",
            HighlightStyle {
                color: Some(rgba(0xc792ea).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Strings - green
        theme.add_highlight(
            "string",
            HighlightStyle {
                color: Some(rgba(0xc3e88d).into()),
                ..Default::default()
            },
        );

        // Numbers - orange
        theme.add_highlight(
            "number",
            HighlightStyle {
                color: Some(rgba(0xff9a85).into()),
                ..Default::default()
            },
        );

        // Booleans - cyan
        theme.add_highlight(
            "boolean",
            HighlightStyle {
                color: Some(rgba(0x89ddff).into()),
                ..Default::default()
            },
        );

        // Comments - gray/muted
        theme.add_highlight(
            "comment",
            HighlightStyle {
                color: Some(rgba(0x717cb4).into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );

        // Variables/Identifiers - white/default
        theme.add_highlight(
            "variable",
            HighlightStyle {
                color: Some(rgba(0xeeffff).into()),
                ..Default::default()
            },
        );

        // Operators - cyan
        theme.add_highlight(
            "operator",
            HighlightStyle {
                color: Some(rgba(0x89ddff).into()),
                ..Default::default()
            },
        );

        // Punctuation - light gray
        theme.add_highlight(
            "punctuation",
            HighlightStyle {
                color: Some(rgba(0x89ddff).into()),
                ..Default::default()
            },
        );

        // Brackets - yellow
        theme.add_highlight(
            "bracket",
            HighlightStyle {
                color: Some(rgba(0xffcb6b).into()),
                ..Default::default()
            },
        );

        // Markdown headings - blue, bold
        theme.add_highlight(
            "markup.heading",
            HighlightStyle {
                color: Some(rgba(0x82aaff).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Markdown bold - keep color, add weight
        theme.add_highlight(
            "markup.bold",
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Markdown italic - keep color, add style
        theme.add_highlight(
            "markup.italic",
            HighlightStyle {
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );

        // Markdown code - darker background
        theme.add_highlight(
            "markup.code",
            HighlightStyle {
                color: Some(rgba(0xc792ea).into()),
                background_color: Some(rgba(0x2a2139).into()),
                ..Default::default()
            },
        );

        theme
    }
}

impl Default for SyntaxTheme {
    fn default() -> Self {
        Self::default_dark()
    }
}

/// A chunk of text with consistent syntax highlighting
///
/// Similar to Zed's [`Chunk`] but adapted for Stoat's token-based highlighting.
/// Each chunk represents a contiguous piece of text that should be rendered
/// with the same highlight style.
#[derive(Clone, Debug)]
pub struct HighlightedChunk<'a> {
    /// The text content of this chunk
    pub text: &'a str,
    /// The highlight ID for this chunk (if any)
    pub highlight_id: Option<HighlightId>,
}

/// Iterator that yields text chunks with syntax highlighting information
///
/// This provides an efficient way to iterate through buffer content while
/// applying syntax highlighting. It integrates Stoat's [`TokenSnapshot`] with
/// the text buffer to produce chunks of consistently-styled text.
pub struct HighlightedChunks<'a> {
    /// Text chunks from the underlying buffer
    text_chunks: Chunks<'a>,
    /// Token snapshot for highlight information
    token_snapshot: &'a TokenSnapshot,
    /// Buffer snapshot for token queries
    buffer_snapshot: &'a BufferSnapshot,
    /// Highlight map for token-to-style conversion
    highlight_map: &'a HighlightMap,
    /// Current byte offset in the text
    current_offset: usize,
    /// End offset of the range we're processing
    end_offset: usize,
    /// Current text chunk being processed
    current_text_chunk: Option<&'a str>,
    /// Remaining text in current chunk
    current_text_remaining: &'a str,
}

impl<'a> HighlightedChunks<'a> {
    /// Create a new highlighted chunks iterator
    ///
    /// Processes text in the given byte range, applying syntax highlighting
    /// based on the token snapshot and highlight map.
    pub fn new(
        range: Range<usize>,
        buffer_snapshot: &'a BufferSnapshot,
        token_snapshot: &'a TokenSnapshot,
        highlight_map: &'a HighlightMap,
    ) -> Self {
        let text_chunks = buffer_snapshot.as_rope().chunks_in_range(range.clone());

        Self {
            text_chunks,
            token_snapshot,
            buffer_snapshot,
            highlight_map,
            current_offset: range.start,
            end_offset: range.end,
            current_text_chunk: None,
            current_text_remaining: "",
        }
    }
}

impl<'a> Iterator for HighlightedChunks<'a> {
    type Item = HighlightedChunk<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        // If we've reached the end, stop
        if self.current_offset >= self.end_offset {
            return None;
        }

        // Get the next text chunk if we don't have one
        if self.current_text_remaining.is_empty() {
            if let Some(chunk) = self.text_chunks.next() {
                self.current_text_chunk = Some(chunk);
                self.current_text_remaining = chunk;
            } else {
                return None;
            }
        }

        // Find the token at the current offset
        let highlight_id = self
            .token_snapshot
            .token_at_offset(self.current_offset, self.buffer_snapshot)
            .and_then(|token| token.highlight_id)
            .map(HighlightId)
            .map(|id| if id.is_default() { None } else { Some(id) })
            .unwrap_or(None);

        // Find the next token boundary to determine chunk length
        let mut chunk_end = self.current_offset + self.current_text_remaining.len();
        chunk_end = chunk_end.min(self.end_offset);

        // Look for the next token that might change highlighting
        let tokens_in_range = self
            .token_snapshot
            .tokens_in_range(self.current_offset..chunk_end, self.buffer_snapshot);

        // Find the earliest token boundary that changes highlighting
        for token in tokens_in_range {
            let token_start = token.range.start.to_offset(self.buffer_snapshot);
            let token_highlight = token
                .highlight_id
                .map(HighlightId)
                .map(|id| if id.is_default() { None } else { Some(id) })
                .unwrap_or(None);

            // If this token starts after our current position and has different highlighting
            if token_start > self.current_offset && token_highlight != highlight_id {
                chunk_end = chunk_end.min(token_start);
                break;
            }
        }

        // Calculate how much of the current text chunk to consume
        let chunk_len = chunk_end - self.current_offset;
        let text_to_take = chunk_len.min(self.current_text_remaining.len());

        // Extract the text for this highlighted chunk
        let (chunk_text, remaining_text) = self.current_text_remaining.split_at(text_to_take);
        self.current_text_remaining = remaining_text;
        self.current_offset += text_to_take;

        Some(HighlightedChunk {
            text: chunk_text,
            highlight_id,
        })
    }
}
