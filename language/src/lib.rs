pub mod brackets;
pub mod grammar;
pub mod highlight;
pub mod highlight_map;
pub mod indent;
pub mod language;
pub mod structural_diff;
pub mod symbols;
pub mod syntax_map;
pub mod textobject;

pub use brackets::{matching_bracket, matching_bracket_from_tree};
pub use highlight::{
    drop_syntax_in_background, edit_tree, extract_highlights, parse, parse_rope, parse_rope_range,
    parse_rope_within, HighlightSpan, SyntaxState,
};
pub use highlight_map::{HighlightId, HighlightMap};
pub use indent::{line_leading_whitespace, newline_indent, suggested_indent};
pub use language::{language_for_fence_token, Language, LanguageRegistry};
pub use symbols::{extract_references, extract_symbols, RefKind, RefSite, SymbolDef, SymbolKind};
pub use syntax_map::{
    LayerKey, LayerSummary, SyntaxLayer, SyntaxMap, SyntaxMapCapture, SyntaxSnapshot,
};
pub use textobject::{collect_capture_ranges, collect_capture_starts, find_smallest_capture_at};
pub use tree_sitter::{Node, Query, Tree};
