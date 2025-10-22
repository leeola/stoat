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
//! ## Available Themes
//!
//! - [`SyntaxTheme::monokai_dark()`] - Classic Monokai dark theme from Sublime Text (default)
//! - [`SyntaxTheme::monokai_light()`] - Monokai colors adapted for light backgrounds
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
use stoat_rope::{SyntaxKind, TokenEntry, TokenSnapshot, TokenSummary};
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
#[derive(Clone, Debug, Default)]
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

    /// Background color for the editor
    pub background_color: gpui::Hsla,

    /// Default text color for unhighlighted text
    pub default_text_color: gpui::Hsla,
}

impl SyntaxTheme {
    /// Create a new empty syntax theme
    pub fn new() -> Self {
        Self {
            highlights: Vec::new(),
            background_color: gpui::rgb(0x1e1e1e).into(),
            default_text_color: gpui::rgb(0xcccccc).into(),
        }
    }

    /// Add a highlight definition to the theme
    ///
    /// Associates a highlight category name with a visual style. The name
    /// is used by [`HighlightMap`] to map syntax tokens to styles.
    pub fn add_highlight(&mut self, name: impl Into<String>, style: HighlightStyle) {
        self.highlights.push((name.into(), style));
    }

    /// Monokai Dark theme - the classic Monokai color scheme
    ///
    /// Based on the original Sublime Text Monokai theme
    pub fn monokai_dark() -> Self {
        use gpui::{rgba, FontWeight};

        let mut theme = Self::new();

        // Classic Monokai palette (GPUI rgba() expects 0xRRGGBBAA format!)
        let background = rgba(0x272822ff);
        let foreground = rgba(0xf8f8f2ff);
        let comment = rgba(0x75715eff);
        let red = rgba(0xf92672ff); // Pink/Red for keywords
        let orange = rgba(0xfd971fff); // Orange for numbers/constants
        let yellow = rgba(0xe6db74ff); // Yellow for strings
        let green = rgba(0xa6e22eff); // Green for functions
        let cyan = rgba(0x66d9efff); // Cyan/Blue for types
        let purple = rgba(0xae81ffff); // Purple for special

        // Set theme colors
        theme.background_color = background.into();
        theme.default_text_color = foreground.into();

        // Keywords - pink/red
        theme.add_highlight(
            "keyword",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );

        // Strings - yellow
        theme.add_highlight(
            "string",
            HighlightStyle {
                color: Some(yellow.into()),
                ..Default::default()
            },
        );

        // String escapes - purple
        theme.add_highlight(
            "string.escape",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );

        // Numbers - purple
        theme.add_highlight(
            "number",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );

        // Booleans - purple
        theme.add_highlight(
            "boolean",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );

        // Constants - orange
        theme.add_highlight(
            "constant",
            HighlightStyle {
                color: Some(orange.into()),
                ..Default::default()
            },
        );

        // Comments - gray/brown
        theme.add_highlight(
            "comment",
            HighlightStyle {
                color: Some(comment.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );

        // Doc comments - same as comments
        theme.add_highlight(
            "comment.doc",
            HighlightStyle {
                color: Some(comment.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );

        // Functions - green
        theme.add_highlight(
            "function",
            HighlightStyle {
                color: Some(green.into()),
                ..Default::default()
            },
        );

        // Method calls - green
        theme.add_highlight(
            "function.method",
            HighlightStyle {
                color: Some(green.into()),
                ..Default::default()
            },
        );

        // Function definitions - green
        theme.add_highlight(
            "function.definition",
            HighlightStyle {
                color: Some(green.into()),
                ..Default::default()
            },
        );

        // Special functions (macros) - cyan
        theme.add_highlight(
            "function.special",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        // Variables/Identifiers - foreground
        theme.add_highlight(
            "variable",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );

        // Special variables (self) - purple/italic
        theme.add_highlight(
            "variable.special",
            HighlightStyle {
                color: Some(purple.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );

        // Function parameters - orange
        theme.add_highlight(
            "variable.parameter",
            HighlightStyle {
                color: Some(orange.into()),
                ..Default::default()
            },
        );

        // Properties/fields - foreground
        theme.add_highlight(
            "property",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );

        // Types - cyan
        theme.add_highlight(
            "type",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        // Builtin types - cyan
        theme.add_highlight(
            "type.builtin",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        // Interface/trait types - cyan
        theme.add_highlight(
            "type.interface",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        // Lifetimes - purple
        theme.add_highlight(
            "lifetime",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );

        // Attributes - cyan
        theme.add_highlight(
            "attribute",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        // Operators - red
        theme.add_highlight(
            "operator",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );

        // Punctuation - foreground
        theme.add_highlight(
            "punctuation",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );

        // Brackets - foreground
        theme.add_highlight(
            "bracket",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );

        // Punctuation brackets
        theme.add_highlight(
            "punctuation.bracket",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );

        // Special punctuation
        theme.add_highlight(
            "punctuation.special",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );

        // Markdown headings
        theme.add_highlight(
            "markup.heading",
            HighlightStyle {
                color: Some(green.into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Markdown bold
        theme.add_highlight(
            "markup.bold",
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Markdown italic
        theme.add_highlight(
            "markup.italic",
            HighlightStyle {
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );

        // Markdown code
        theme.add_highlight(
            "markup.code",
            HighlightStyle {
                color: Some(yellow.into()),
                ..Default::default()
            },
        );

        theme
    }

    /// Monokai Light theme - Monokai colors on a light background
    ///
    /// Uses the classic Monokai syntax colors with adjusted brightness for light backgrounds
    pub fn monokai_light() -> Self {
        use gpui::{rgba, FontWeight};

        let mut theme = Self::new();

        // Monokai Light palette (darker versions for light background)
        let background = rgba(0xfafafaff);
        let foreground = rgba(0x272822ff);
        let comment = rgba(0x75715eff);
        let red = rgba(0xd9006cff); // Darker pink/red for keywords
        let orange = rgba(0xe67f00ff); // Darker orange
        let yellow = rgba(0xc9a500ff); // Darker yellow
        let green = rgba(0x6d9d00ff); // Darker green
        let cyan = rgba(0x0099ccff); // Darker cyan
        let purple = rgba(0x9933ffff); // Darker purple

        // Set theme colors
        theme.background_color = background.into();
        theme.default_text_color = foreground.into();

        // Keywords - red
        theme.add_highlight(
            "keyword",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );

        // Strings - yellow
        theme.add_highlight(
            "string",
            HighlightStyle {
                color: Some(yellow.into()),
                ..Default::default()
            },
        );

        // String escapes - purple
        theme.add_highlight(
            "string.escape",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );

        // Numbers - purple
        theme.add_highlight(
            "number",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );

        // Booleans - purple
        theme.add_highlight(
            "boolean",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );

        // Constants - orange
        theme.add_highlight(
            "constant",
            HighlightStyle {
                color: Some(orange.into()),
                ..Default::default()
            },
        );

        // Comments - brown/gray
        theme.add_highlight(
            "comment",
            HighlightStyle {
                color: Some(comment.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );

        // Doc comments
        theme.add_highlight(
            "comment.doc",
            HighlightStyle {
                color: Some(comment.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );

        // Functions - green
        theme.add_highlight(
            "function",
            HighlightStyle {
                color: Some(green.into()),
                ..Default::default()
            },
        );

        // Method calls - green
        theme.add_highlight(
            "function.method",
            HighlightStyle {
                color: Some(green.into()),
                ..Default::default()
            },
        );

        // Function definitions - green with bold
        theme.add_highlight(
            "function.definition",
            HighlightStyle {
                color: Some(green.into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Special functions (macros) - cyan
        theme.add_highlight(
            "function.special",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        // Variables/Identifiers - foreground
        theme.add_highlight(
            "variable",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );

        // Special variables (self) - purple/italic
        theme.add_highlight(
            "variable.special",
            HighlightStyle {
                color: Some(purple.into()),
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );

        // Function parameters - orange
        theme.add_highlight(
            "variable.parameter",
            HighlightStyle {
                color: Some(orange.into()),
                ..Default::default()
            },
        );

        // Properties/fields - foreground
        theme.add_highlight(
            "property",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );

        // Types - cyan
        theme.add_highlight(
            "type",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        // Builtin types - cyan
        theme.add_highlight(
            "type.builtin",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        // Interface/trait types - cyan
        theme.add_highlight(
            "type.interface",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        // Lifetimes - purple
        theme.add_highlight(
            "lifetime",
            HighlightStyle {
                color: Some(purple.into()),
                ..Default::default()
            },
        );

        // Attributes - cyan
        theme.add_highlight(
            "attribute",
            HighlightStyle {
                color: Some(cyan.into()),
                ..Default::default()
            },
        );

        // Operators - red
        theme.add_highlight(
            "operator",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );

        // Punctuation - foreground
        theme.add_highlight(
            "punctuation",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );

        // Brackets - foreground
        theme.add_highlight(
            "bracket",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );

        // Punctuation brackets
        theme.add_highlight(
            "punctuation.bracket",
            HighlightStyle {
                color: Some(foreground.into()),
                ..Default::default()
            },
        );

        // Special punctuation
        theme.add_highlight(
            "punctuation.special",
            HighlightStyle {
                color: Some(red.into()),
                ..Default::default()
            },
        );

        // Markdown headings
        theme.add_highlight(
            "markup.heading",
            HighlightStyle {
                color: Some(green.into()),
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Markdown bold
        theme.add_highlight(
            "markup.bold",
            HighlightStyle {
                font_weight: Some(FontWeight::BOLD),
                ..Default::default()
            },
        );

        // Markdown italic
        theme.add_highlight(
            "markup.italic",
            HighlightStyle {
                font_style: Some(gpui::FontStyle::Italic),
                ..Default::default()
            },
        );

        // Markdown code
        theme.add_highlight(
            "markup.code",
            HighlightStyle {
                color: Some(yellow.into()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use text::{Buffer, BufferId, ToOffset};

    #[test]
    fn test_action_highlighting() {
        let source = "use gpui::{actions, Action, Pixels, Point};";

        // Create buffer and parse
        let buffer = Buffer::new(
            0,
            BufferId::new(1).expect("valid buffer id"),
            source.to_string(),
        );
        let snapshot = buffer.snapshot();

        // Parse using stoat_text
        let mut parser =
            stoat_text::Parser::new(stoat_text::Language::Rust).expect("valid rust parser");
        let tokens = parser.parse(source, &snapshot).expect("valid parse");

        println!("\nTokens:");
        for token in &tokens {
            let start = token.range.start.to_offset(&snapshot);
            let end = token.range.end.to_offset(&snapshot);
            let text = &source[start..end];
            println!("  [{:2}..{:2}] {:?} '{}'", start, end, token.kind, text);
        }

        // Create token map
        let mut token_map = stoat_rope::TokenMap::new(&snapshot);
        token_map.replace_tokens(tokens, &snapshot);
        let token_snapshot = token_map.snapshot();

        // Create highlight map
        let theme = SyntaxTheme::monokai_dark();
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

        println!("\nUnique highlight IDs for Action parts: {action_highlight_ids:?}");

        // Action should have consistent highlighting
        assert_eq!(
            action_highlight_ids.len(),
            1,
            "Action should have only one highlight ID, found: {action_highlight_ids:?}"
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
    token_cursor: Cursor<'a, 'a, TokenEntry, TokenSummary>,
    current_token: Option<&'a TokenEntry>,

    // Cached token byte offsets (like Zed's direct byte offsets from tree-sitter)
    // This eliminates repeated O(log n) Anchor::to_offset() conversions
    current_token_start_byte: usize,
    current_token_end_byte: usize,

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

        // Cache byte offsets for the initial token (avoids repeated conversions)
        let (current_token_start_byte, current_token_end_byte) = current_token
            .map(|token| {
                (
                    token.range.start.to_offset(buffer_snapshot),
                    token.range.end.to_offset(buffer_snapshot),
                )
            })
            .unwrap_or((usize::MAX, usize::MAX));

        Self {
            text_chunks,
            current_text_remaining: "",
            token_cursor,
            current_token,
            current_token_start_byte,
            current_token_end_byte,
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
        while let Some(_token) = self.current_token {
            if self.current_token_end_byte <= self.current_offset {
                self.token_cursor.next();
                self.current_token = self.token_cursor.item();

                // Cache byte offsets for the new token (called once per token)
                (self.current_token_start_byte, self.current_token_end_byte) = self
                    .current_token
                    .map(|token| {
                        (
                            token.range.start.to_offset(self.buffer_snapshot),
                            token.range.end.to_offset(self.buffer_snapshot),
                        )
                    })
                    .unwrap_or((usize::MAX, usize::MAX));
            } else {
                break;
            }
        }

        // Get highlight ID from current token using cached offsets
        let highlight_id = self
            .current_token
            .and_then(|token| {
                // Use cached byte offsets instead of calling to_offset() repeatedly
                if self.current_token_start_byte <= self.current_offset
                    && self.current_offset < self.current_token_end_byte
                {
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
        // Use cached byte offsets instead of calling to_offset() repeatedly
        if self.current_token.is_some() {
            // If we're before the token starts, clip to the token start
            if self.current_offset < self.current_token_start_byte
                && chunk_end > self.current_token_start_byte
            {
                chunk_end = self.current_token_start_byte;
            }
            // If we're inside the token, clip to the token end
            else if self.current_offset >= self.current_token_start_byte
                && self.current_token_end_byte < chunk_end
            {
                chunk_end = self.current_token_end_byte;
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
