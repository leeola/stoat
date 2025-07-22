//! AST node representation

use crate::kind::SyntaxKind;
use compact_str::CompactString;
use std::fmt;

/// A single AST node allocated in an arena
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node<'arena> {
    /// The syntax kind of this node
    kind: SyntaxKind,
    /// The text content of this node (if it's a leaf)
    text: CompactString,
    /// Child nodes (empty for leaf nodes)
    children: Vec<&'arena Node<'arena>>,
}

impl<'arena> Node<'arena> {
    /// Create a new leaf node with text content
    pub fn leaf(kind: SyntaxKind, text: impl Into<CompactString>) -> Self {
        Self {
            kind,
            text: text.into(),
            children: Vec::new(),
        }
    }

    /// Create a new internal node with children
    pub fn internal(kind: SyntaxKind, children: Vec<&'arena Node<'arena>>) -> Self {
        Self {
            kind,
            text: CompactString::new(""),
            children,
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
        let node = Node::leaf(SyntaxKind::String, "test");
        let display = format!("{node}");
        assert!(display.contains("String"));
        assert!(display.contains("test"));
    }
}
