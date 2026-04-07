pub mod grammar;
pub mod highlight;
pub mod language;

pub use highlight::{extract_highlights, parse, HighlightSpan, SyntaxState};
pub use language::{Language, LanguageRegistry, TokenStyle};
