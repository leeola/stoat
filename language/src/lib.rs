pub mod grammar;
pub mod highlight;
pub mod language;

pub use highlight::{
    edit_tree, extract_highlights, extract_highlights_rope, parse, parse_rope, HighlightSpan,
    SyntaxState,
};
pub use language::{Language, LanguageRegistry, TokenStyle};
