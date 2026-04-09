pub mod grammar;
pub mod highlight;
pub mod highlight_map;
pub mod language;
pub mod structural_diff;
pub mod syntax_map;

pub use highlight::{
    drop_syntax_in_background, edit_tree, extract_highlights, extract_highlights_rope,
    extract_highlights_rope_with_cache, parse, parse_rope, parse_rope_range, parse_rope_within,
    ExtractedHighlights, HighlightSpan, InjectionTreeCache, SyntaxState,
};
pub use highlight_map::{HighlightId, HighlightMap};
pub use language::{Language, LanguageRegistry};
pub use syntax_map::{LayerKey, SyntaxLayer, SyntaxMap, SyntaxSnapshot};
