pub mod bracket;
pub mod scope;
pub mod symbol;

use stoat_text::Language;
use text::BufferSnapshot;

pub trait SyntaxIndex: Clone {
    fn rebuild(
        tree: &stoat_text::tree_sitter::Tree,
        source: &str,
        buffer: &BufferSnapshot,
        language: Language,
    ) -> Self;
}
