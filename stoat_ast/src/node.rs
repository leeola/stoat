//! AST node representation

use crate::kind::SyntaxKind;
use compact_str::CompactString;
use std::fmt;

/// A single AST node allocated in an arena
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// The syntax kind of this node
    kind: SyntaxKind,
    /// The text content of this node (if it's a leaf)
    text: CompactString,
}

impl Node {
    /// Create a new AST node
    pub fn new(kind: SyntaxKind, text: impl Into<CompactString>) -> Self {
        Self {
            kind,
            text: text.into(),
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
}

impl fmt::Display for Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}({})", self.kind, self.text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_node_display() {
        let node = Node::new(SyntaxKind::String, "test");
        let display = format!("{node}");
        assert!(display.contains("String"));
        assert!(display.contains("test"));
    }
}
