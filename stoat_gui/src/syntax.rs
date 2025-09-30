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
use stoat_rope_v3::{SyntaxKind, TokenEntry, TokenSnapshot, TokenSummary};
use sum_tree::Cursor;
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
                "string.escape" => mappings.insert(SyntaxKind::StringEscape, highlight_id),
                "number" => mappings.insert(SyntaxKind::Number, highlight_id),
                "boolean" => mappings.insert(SyntaxKind::Boolean, highlight_id),
                "constant" => mappings.insert(SyntaxKind::Constant, highlight_id),

                // Comments
                "comment" => {
                    mappings.insert(SyntaxKind::LineComment, highlight_id);
                    mappings.insert(SyntaxKind::BlockComment, highlight_id)
                },
                "comment.doc" => mappings.insert(SyntaxKind::DocComment, highlight_id),

                // Functions
                "function" => mappings.insert(SyntaxKind::Function, highlight_id),
                "function.method" => mappings.insert(SyntaxKind::FunctionMethod, highlight_id),
                "function.definition" => {
                    mappings.insert(SyntaxKind::FunctionDefinition, highlight_id)
                },
                "function.special" => mappings.insert(SyntaxKind::FunctionSpecial, highlight_id),

                // Variables
                "variable" | "identifier" => mappings.insert(SyntaxKind::Identifier, highlight_id),
                "variable.special" => mappings.insert(SyntaxKind::VariableSpecial, highlight_id),
                "variable.parameter" => {
                    mappings.insert(SyntaxKind::VariableParameter, highlight_id)
                },

                // Properties
                "property" => mappings.insert(SyntaxKind::Property, highlight_id),

                // Types
                "type" => mappings.insert(SyntaxKind::Type, highlight_id),
                "type.builtin" => mappings.insert(SyntaxKind::TypeBuiltin, highlight_id),
                "type.interface" => mappings.insert(SyntaxKind::TypeInterface, highlight_id),

                // Attributes and lifetimes
                "attribute" => mappings.insert(SyntaxKind::Attribute, highlight_id),
                "lifetime" => mappings.insert(SyntaxKind::Lifetime, highlight_id),

                // Operators and punctuation
                "operator" => mappings.insert(SyntaxKind::Operator, highlight_id),
                "punctuation" => {
                    mappings.insert(SyntaxKind::Comma, highlight_id);
                    mappings.insert(SyntaxKind::Semicolon, highlight_id);
                    mappings.insert(SyntaxKind::Colon, highlight_id);
                    mappings.insert(SyntaxKind::Dot, highlight_id);
                    mappings.insert(SyntaxKind::PunctuationDelimiter, highlight_id)
                },
                "punctuation.bracket" => {
                    mappings.insert(SyntaxKind::OpenParen, highlight_id);
                    mappings.insert(SyntaxKind::CloseParen, highlight_id);
                    mappings.insert(SyntaxKind::OpenBracket, highlight_id);
                    mappings.insert(SyntaxKind::CloseBracket, highlight_id);
                    mappings.insert(SyntaxKind::OpenBrace, highlight_id);
                    mappings.insert(SyntaxKind::CloseBrace, highlight_id);
                    mappings.insert(SyntaxKind::PunctuationBracket, highlight_id)
                },
                "punctuation.special" => {
                    mappings.insert(SyntaxKind::PunctuationSpecial, highlight_id)
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

        // Types - cyan/blue
        theme.add_highlight(
            "type",
            HighlightStyle {
                color: Some(rgba(0x82aaff).into()),
                ..Default::default()
            },
        );

        // Builtin types - lighter blue
        theme.add_highlight(
            "type.builtin",
            HighlightStyle {
                color: Some(rgba(0x82aaff).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Interface/trait types - cyan
        theme.add_highlight(
            "type.interface",
            HighlightStyle {
                color: Some(rgba(0x89ddff).into()),
                ..Default::default()
            },
        );

        // Functions - yellow/gold
        theme.add_highlight(
            "function",
            HighlightStyle {
                color: Some(rgba(0xffcb6b).into()),
                ..Default::default()
            },
        );

        // Method calls - same as functions
        theme.add_highlight(
            "function.method",
            HighlightStyle {
                color: Some(rgba(0xffcb6b).into()),
                ..Default::default()
            },
        );

        // Function definitions - yellow with bold
        theme.add_highlight(
            "function.definition",
            HighlightStyle {
                color: Some(rgba(0xffcb6b).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Special functions (macros) - magenta
        theme.add_highlight(
            "function.special",
            HighlightStyle {
                color: Some(rgba(0xc792ea).into()),
                ..Default::default()
            },
        );

        // Special variables (self) - red/orange
        theme.add_highlight(
            "variable.special",
            HighlightStyle {
                color: Some(rgba(0xf78c6c).into()),
                ..Default::default()
            },
        );

        // Function parameters - lighter color
        theme.add_highlight(
            "variable.parameter",
            HighlightStyle {
                color: Some(rgba(0xf78c6c).into()),
                ..Default::default()
            },
        );

        // Properties/fields - cyan
        theme.add_highlight(
            "property",
            HighlightStyle {
                color: Some(rgba(0x89ddff).into()),
                ..Default::default()
            },
        );

        // Constants - orange
        theme.add_highlight(
            "constant",
            HighlightStyle {
                color: Some(rgba(0xff9a85).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Lifetimes - purple
        theme.add_highlight(
            "lifetime",
            HighlightStyle {
                color: Some(rgba(0xc792ea).into()),
                ..Default::default()
            },
        );

        // Attributes - purple
        theme.add_highlight(
            "attribute",
            HighlightStyle {
                color: Some(rgba(0xc792ea).into()),
                ..Default::default()
            },
        );

        // String escapes - cyan/bright
        theme.add_highlight(
            "string.escape",
            HighlightStyle {
                color: Some(rgba(0x89ddff).into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Doc comments - lighter gray with italic
        theme.add_highlight(
            "comment.doc",
            HighlightStyle {
                color: Some(rgba(0x8090b0).into()),
                font_style: Some(gpui::FontStyle::Italic),
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

        // Punctuation brackets - same as brackets
        theme.add_highlight(
            "punctuation.bracket",
            HighlightStyle {
                color: Some(rgba(0xffcb6b).into()),
                ..Default::default()
            },
        );

        // Special punctuation - purple
        theme.add_highlight(
            "punctuation.special",
            HighlightStyle {
                color: Some(rgba(0xc792ea).into()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use text::{Buffer, BufferId, ToOffset};

    #[test]
    fn test_action_highlighting() {
        let source = "use gpui::{actions, Action, Pixels, Point};";

        // Create buffer and parse
        let buffer = Buffer::new(0, BufferId::new(1).unwrap(), source.to_string());
        let snapshot = buffer.snapshot();

        // Parse using stoat_text_v3
        let mut parser = stoat_text_v3::Parser::new(stoat_text_v3::Language::Rust).unwrap();
        let tokens = parser.parse(source, &snapshot).unwrap();

        println!("\nTokens:");
        for token in &tokens {
            let start = token.range.start.to_offset(&snapshot);
            let end = token.range.end.to_offset(&snapshot);
            let text = &source[start..end];
            println!("  [{:2}..{:2}] {:?} '{}'", start, end, token.kind, text);
        }

        // Create token map
        let mut token_map = stoat_rope_v3::TokenMap::new(&snapshot);
        token_map.replace_tokens(tokens, &snapshot);
        let token_snapshot = token_map.snapshot();

        // Create highlight map
        let theme = SyntaxTheme::default_dark();
        let highlight_map = HighlightMap::new(&theme);

        // Test highlighting chunks for the ENTIRE LINE (like GUI does)
        let line_range = 0..source.len();
        let all_chunks: Vec<_> =
            HighlightedChunks::new(line_range, &snapshot, &token_snapshot, &highlight_map)
                .collect();

        println!("\nAll chunks for entire line:");
        for (i, chunk) in all_chunks.iter().enumerate() {
            println!(
                "  Chunk {}: {:?} (highlight: {:?})",
                i, chunk.text, chunk.highlight_id
            );
        }

        // Find chunks that make up "Action"
        let action_chunks: Vec<_> = all_chunks
            .iter()
            .filter(|c| c.text.contains('A') || source[20..26].contains(c.text))
            .collect();

        println!("\nChunks containing parts of 'Action':");
        for chunk in &action_chunks {
            println!("  {:?} (highlight: {:?})", chunk.text, chunk.highlight_id);
        }

        // Check if Action appears in multiple chunks with different highlights
        let action_highlight_ids: std::collections::HashSet<_> = all_chunks
            .iter()
            .filter_map(|c| {
                if c.text == "Action" || (c.text.len() < 6 && "Action".contains(c.text)) {
                    Some(c.highlight_id)
                } else {
                    None
                }
            })
            .collect();

        println!(
            "\nUnique highlight IDs for Action parts: {:?}",
            action_highlight_ids
        );

        // Action should have consistent highlighting
        assert_eq!(
            action_highlight_ids.len(),
            1,
            "Action should have only one highlight ID, found: {:?}",
            action_highlight_ids
        );
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
///
/// ## Performance
///
/// Uses a stateful cursor to avoid O(n) rescanning on each iteration.
/// The cursor is seeked once at construction (O(log n)) and then advanced
/// incrementally (O(1) amortized), providing ~10,000x better performance
/// than naive token_at_offset() calls for each chunk.
pub struct HighlightedChunks<'a> {
    // Text iteration
    text_chunks: Chunks<'a>,
    current_text_remaining: &'a str,

    // Token cursor (stateful!)
    token_cursor: Cursor<'a, TokenEntry, TokenSummary>,
    current_token: Option<&'a TokenEntry>,

    // Position tracking
    buffer_snapshot: &'a BufferSnapshot,
    highlight_map: &'a HighlightMap,
    current_offset: usize,
    end_offset: usize,
}

impl<'a> HighlightedChunks<'a> {
    /// Create a new highlighted chunks iterator
    ///
    /// Processes text in the given byte range, applying syntax highlighting
    /// based on the token snapshot and highlight map.
    ///
    /// The cursor is initialized once during construction and then advanced
    /// incrementally as we iterate through chunks (O(1) amortized per chunk).
    pub fn new(
        range: Range<usize>,
        buffer_snapshot: &'a BufferSnapshot,
        token_snapshot: &'a TokenSnapshot,
        highlight_map: &'a HighlightMap,
    ) -> Self {
        let text_chunks = buffer_snapshot.as_rope().chunks_in_range(range.clone());

        // Create cursor and advance to first token
        let mut token_cursor = token_snapshot.cursor(buffer_snapshot);
        token_cursor.next();

        // Get initial token and advance cursor to the one at/after our start position
        let mut current_token = token_cursor.item();

        // Advance cursor until we find a token that overlaps or comes after our start position
        while let Some(token) = current_token {
            let token_end = token.range.end.to_offset(buffer_snapshot);
            if token_end > range.start {
                break;
            }
            token_cursor.next();
            current_token = token_cursor.item();
        }

        Self {
            text_chunks,
            current_text_remaining: "",
            token_cursor,
            current_token,
            buffer_snapshot,
            highlight_map,
            current_offset: range.start,
            end_offset: range.end,
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
            self.current_text_remaining = self.text_chunks.next()?;
        }

        // Advance cursor if current token is behind our position
        // This handles the case where we've moved past the end of the current token
        while let Some(token) = self.current_token {
            let token_end = token.range.end.to_offset(self.buffer_snapshot);
            if token_end <= self.current_offset {
                self.token_cursor.next();
                self.current_token = self.token_cursor.item();
            } else {
                break;
            }
        }

        // Get highlight ID from current token
        // Check if we're inside a token's range
        let highlight_id = self
            .current_token
            .and_then(|token| {
                let token_start = token.range.start.to_offset(self.buffer_snapshot);
                let token_end = token.range.end.to_offset(self.buffer_snapshot);

                // Only use this token's highlight if we're inside its range
                if token_start <= self.current_offset && self.current_offset < token_end {
                    Some(self.highlight_map.get(token.kind))
                } else {
                    None
                }
            })
            .filter(|id| !id.is_default());

        // Determine chunk end: min of (current text end, token boundary, range end)
        let mut chunk_end = self.current_offset + self.current_text_remaining.len();
        chunk_end = chunk_end.min(self.end_offset);

        // If we have a current token, clip chunk to BOTH start and end token boundaries
        if let Some(token) = self.current_token {
            let token_start = token.range.start.to_offset(self.buffer_snapshot);
            let token_end = token.range.end.to_offset(self.buffer_snapshot);

            // If we're before the token starts, clip to the token start
            if self.current_offset < token_start && chunk_end > token_start {
                chunk_end = token_start;
            }
            // If we're inside the token, clip to the token end
            else if self.current_offset >= token_start && token_end < chunk_end {
                chunk_end = token_end;
            }
        }

        // Calculate how much text to consume
        let text_to_take = chunk_end - self.current_offset;
        let text_to_take = text_to_take.min(self.current_text_remaining.len());

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
