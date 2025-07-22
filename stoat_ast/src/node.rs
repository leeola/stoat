//! AST node representation

use crate::{
    kind::SyntaxKind,
    position::{TextInfo, TextPos, TextRangeExt},
};
use compact_str::CompactString;
use std::{fmt, ops::Range};

/// A single AST node allocated in an arena
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node<'arena> {
    /// The syntax kind of this node
    kind: SyntaxKind,
    /// The text content of this node (if it's a leaf)
    text: CompactString,
    /// Child nodes (empty for leaf nodes)
    children: Vec<&'arena Node<'arena>>,
    /// Position of this node in the source text
    range: Range<TextPos>,
    /// Cached metadata about this subtree
    info: TextInfo,
}

impl<'arena> Node<'arena> {
    /// Create a new leaf node with text content
    pub fn leaf(kind: SyntaxKind, text: impl Into<CompactString>, range: Range<TextPos>) -> Self {
        let text = text.into();
        let info = TextInfo::from_text(&text);
        Self {
            kind,
            text,
            children: Vec::new(),
            range,
            info,
        }
    }

    /// Create a new internal node with children
    pub fn internal(kind: SyntaxKind, children: Vec<&'arena Node<'arena>>) -> Self {
        // Calculate range from children
        let range = if children.is_empty() {
            Range::<TextPos>::from_offsets(0, 0)
        } else {
            let start = children.first().expect("children is not empty").range.start;
            let end = children.last().expect("children is not empty").range.end;
            start..end
        };

        // Calculate combined info from children
        let info = children
            .iter()
            .map(|child| child.info)
            .fold(TextInfo::empty(), |acc, info| acc.combine(&info));

        Self {
            kind,
            text: CompactString::new(""),
            children,
            range,
            info,
        }
    }

    /// Get the syntax kind of this node
    pub fn kind(&self) -> SyntaxKind {
        self.kind
    }

    /// Get the text content of this node
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Get the children of this node
    pub fn children(&self) -> &[&'arena Node<'arena>] {
        &self.children
    }

    /// Check if this is a leaf node
    pub fn is_leaf(&self) -> bool {
        self.children.is_empty()
    }

    /// Get the position range of this node
    pub fn range(&self) -> Range<TextPos> {
        self.range.clone()
    }

    /// Get the text info for this subtree
    pub fn text_info(&self) -> TextInfo {
        self.info
    }

    /// Get the start position of this node
    pub fn start_pos(&self) -> TextPos {
        self.range.start
    }

    /// Get the end position of this node
    pub fn end_pos(&self) -> TextPos {
        self.range.end
    }

    /// Get the byte length of this node
    pub fn len_bytes(&self) -> usize {
        self.info.bytes
    }

    /// Get the character count of this node
    pub fn len_chars(&self) -> usize {
        self.info.chars
    }

    /// Get the token count in this subtree
    pub fn len_tokens(&self) -> usize {
        self.info.tokens
    }

    /// Get the newline count in this subtree
    pub fn len_newlines(&self) -> usize {
        self.info.newlines
    }
}

impl<'arena> fmt::Display for Node<'arena> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_leaf() {
            write!(f, "{:?}({})", self.kind, self.text)
        } else {
            write!(f, "{:?}[{}]", self.kind, self.children.len())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_display() {
        let node = Node::leaf(
            SyntaxKind::String,
            "test",
            Range::<TextPos>::from_offsets(0, 4),
        );
        let display = format!("{node}");
        assert!(display.contains("String"));
        assert!(display.contains("test"));
    }

    #[test]
    fn test_position_tracking() {
        let leaf = Node::leaf(
            SyntaxKind::Text,
            "hello\nworld",
            Range::<TextPos>::from_offsets(0, 11),
        );
        assert_eq!(leaf.len_bytes(), 11);
        assert_eq!(leaf.len_chars(), 11);
        assert_eq!(leaf.len_tokens(), 1);
        assert_eq!(leaf.len_newlines(), 1);
        assert_eq!(leaf.start_pos(), TextPos(0));
        assert_eq!(leaf.end_pos(), TextPos(11));
    }
}
