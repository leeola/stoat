//! Core AST node structure for the rope AST

use crate::{kind::SyntaxKind, semantic::SemanticInfo};
use compact_str::CompactString;
use smallvec::SmallVec;
use std::sync::Arc;

// Type used for storing counts and offsets
pub type Count = usize;

/// Text position in the source
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextPos(pub Count);

/// Text range in the source
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TextRange {
    pub start: TextPos,
    pub end: TextPos,
}

impl TextRange {
    /// Create a new text range
    pub fn new(start: Count, end: Count) -> Self {
        Self {
            start: TextPos(start),
            end: TextPos(end),
        }
    }

    /// Get the length of this range
    pub fn len(&self) -> Count {
        self.end.0.saturating_sub(self.start.0)
    }

    /// Check if this range is empty
    pub fn is_empty(&self) -> bool {
        self.start.0 >= self.end.0
    }
}

/// Metadata about text and structure, cached for efficient traversal
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextInfo {
    /// Total bytes in this subtree
    pub bytes: Count,
    /// Total characters in this subtree
    pub chars: Count,
    /// Total tokens in this subtree
    pub tokens: Count,
    /// Total newlines in this subtree
    pub newlines: Count,
}

impl TextInfo {
    /// Create text info for an empty node
    pub fn empty() -> Self {
        Self {
            bytes: 0,
            chars: 0,
            tokens: 0,
            newlines: 0,
        }
    }

    /// Create text info from a text string
    pub fn from_text(text: &str) -> Self {
        Self {
            bytes: text.len(),
            chars: text.chars().count(),
            tokens: 1,
            newlines: text.chars().filter(|&c| c == '\n').count(),
        }
    }

    /// Combine two text infos
    pub fn combine(&self, other: &Self) -> Self {
        Self {
            bytes: self.bytes + other.bytes,
            chars: self.chars + other.chars,
            tokens: self.tokens + other.tokens,
            newlines: self.newlines + other.newlines,
        }
    }
}

/// Constants for node sizing (inspired by ropey)
/// Target ~1KB nodes for cache efficiency
pub const MAX_BYTES: usize = 4096;
pub const MAX_CHILDREN: usize = 16;
pub const MIN_BYTES: usize = MAX_BYTES / 2 - MAX_BYTES / 32;
pub const MIN_CHILDREN: usize = MAX_CHILDREN / 2;

/// A node in the rope AST
#[derive(Debug, Clone)]
pub enum AstNode {
    /// Token node (leaf) containing actual text
    Token {
        kind: SyntaxKind,
        text: CompactString,
        range: TextRange,
        /// Optional semantic information
        semantic: Option<SemanticInfo>,
    },

    /// Syntax node (internal) with children
    Syntax {
        kind: SyntaxKind,
        /// Children with their cached text info
        /// Using SmallVec to optimize for nodes with few children
        children: SmallVec<[(Arc<AstNode>, TextInfo); 4]>,
        /// Cached combined text info for this node
        info: TextInfo,
        /// Text range this node covers
        range: TextRange,
        /// Optional semantic information
        semantic: Option<SemanticInfo>,
    },
}

impl AstNode {
    /// Create a new token node
    pub fn token(kind: SyntaxKind, text: CompactString, range: TextRange) -> Self {
        AstNode::Token {
            kind,
            text,
            range,
            semantic: None,
        }
    }

    /// Create a new syntax node
    pub fn syntax(kind: SyntaxKind, range: TextRange) -> Self {
        AstNode::Syntax {
            kind,
            children: SmallVec::new(),
            info: TextInfo::empty(),
            range,
            semantic: None,
        }
    }

    /// Create a new token node with semantic info
    pub fn token_with_semantic(
        kind: SyntaxKind,
        text: CompactString,
        range: TextRange,
        semantic: SemanticInfo,
    ) -> Self {
        AstNode::Token {
            kind,
            text,
            range,
            semantic: Some(semantic),
        }
    }

    /// Create a new syntax node with semantic info
    pub fn syntax_with_semantic(
        kind: SyntaxKind,
        range: TextRange,
        semantic: SemanticInfo,
    ) -> Self {
        AstNode::Syntax {
            kind,
            children: SmallVec::new(),
            info: TextInfo::empty(),
            range,
            semantic: Some(semantic),
        }
    }

    /// Get the height of this node (for balancing)
    pub fn height(&self) -> usize {
        match self {
            AstNode::Token { .. } => 0,
            AstNode::Syntax { children, .. } => {
                children
                    .iter()
                    .map(|(child, _)| child.height())
                    .max()
                    .unwrap_or(0)
                    + 1
            },
        }
    }

    /// Get the syntax kind of this node
    pub fn kind(&self) -> SyntaxKind {
        match self {
            AstNode::Token { kind, .. } => *kind,
            AstNode::Syntax { kind, .. } => *kind,
        }
    }

    /// Get the text range of this node
    pub fn range(&self) -> TextRange {
        match self {
            AstNode::Token { range, .. } => *range,
            AstNode::Syntax { range, .. } => *range,
        }
    }

    /// Get the text info for this node
    pub fn text_info(&self) -> TextInfo {
        match self {
            AstNode::Token { text, .. } => TextInfo::from_text(text),
            AstNode::Syntax { info, .. } => *info,
        }
    }

    /// Check if this is a token (leaf) node
    pub fn is_token(&self) -> bool {
        matches!(self, AstNode::Token { .. })
    }

    /// Check if this is a syntax (internal) node
    pub fn is_syntax(&self) -> bool {
        matches!(self, AstNode::Syntax { .. })
    }

    /// Get the text of a token node
    pub fn token_text(&self) -> Option<&str> {
        match self {
            AstNode::Token { text, .. } => Some(text),
            AstNode::Syntax { .. } => None,
        }
    }

    /// Get the children of a syntax node
    pub fn children(&self) -> Option<&[(Arc<AstNode>, TextInfo)]> {
        match self {
            AstNode::Token { .. } => None,
            AstNode::Syntax { children, .. } => Some(children),
        }
    }

    /// Get the semantic information for this node
    pub fn semantic(&self) -> Option<&SemanticInfo> {
        match self {
            AstNode::Token { semantic, .. } => semantic.as_ref(),
            AstNode::Syntax { semantic, .. } => semantic.as_ref(),
        }
    }

    /// Set semantic information for this node
    pub fn with_semantic(self, semantic: SemanticInfo) -> Self {
        match self {
            AstNode::Token {
                kind, text, range, ..
            } => AstNode::Token {
                kind,
                text,
                range,
                semantic: Some(semantic),
            },
            AstNode::Syntax {
                kind,
                children,
                info,
                range,
                ..
            } => AstNode::Syntax {
                kind,
                children,
                info,
                range,
                semantic: Some(semantic),
            },
        }
    }

    /// Remove semantic information from this node
    pub fn without_semantic(self) -> Self {
        match self {
            AstNode::Token {
                kind, text, range, ..
            } => AstNode::Token {
                kind,
                text,
                range,
                semantic: None,
            },
            AstNode::Syntax {
                kind,
                children,
                info,
                range,
                ..
            } => AstNode::Syntax {
                kind,
                children,
                info,
                range,
                semantic: None,
            },
        }
    }

    /// Add a child to a syntax node (for building)
    pub fn add_child(&mut self, child: Arc<AstNode>) -> Result<(), AstError> {
        match self {
            AstNode::Token { .. } => Err(AstError::CannotAddChildToToken),
            AstNode::Syntax { children, info, .. } => {
                let child_info = child.text_info();
                children.push((child, child_info));
                *info = info.combine(&child_info);
                Ok(())
            },
        }
    }

    /// Check if this node needs splitting (too large)
    pub fn needs_split(&self) -> bool {
        match self {
            AstNode::Token { text, .. } => text.len() > MAX_BYTES,
            AstNode::Syntax { children, .. } => children.len() > MAX_CHILDREN,
        }
    }

    /// Collect all text recursively
    pub fn collect_text(&self, buffer: &mut String) {
        match self {
            AstNode::Token { text, .. } => buffer.push_str(text.as_str()),
            AstNode::Syntax { children, .. } => {
                for (child, _) in children {
                    child.collect_text(buffer);
                }
            },
        }
    }

    /// Split a token node at the given offset (relative to the token start)
    pub fn split_token_at(&self, offset: usize) -> Result<(Arc<AstNode>, Arc<AstNode>), AstError> {
        match self {
            AstNode::Token {
                kind, text, range, ..
            } => {
                if offset == 0 || offset >= text.len() {
                    return Err(AstError::InvalidSplitOffset {
                        offset,
                        max: text.len(),
                    });
                }

                // Split the text
                let (left_text, right_text) = text.split_at(offset);

                // Create two new token nodes
                let left_node = Arc::new(AstNode::Token {
                    kind: *kind,
                    text: left_text.into(),
                    range: TextRange::new(range.start.0, range.start.0 + offset),
                    semantic: None, // Split tokens lose semantic info
                });

                let right_node = Arc::new(AstNode::Token {
                    kind: *kind,
                    text: right_text.into(),
                    range: TextRange::new(range.start.0 + offset, range.end.0),
                    semantic: None, // Split tokens lose semantic info
                });

                Ok((left_node, right_node))
            },
            AstNode::Syntax { .. } => Err(AstError::CannotSplitSyntaxNode),
        }
    }

    /// Try to merge this node with another node of the same kind
    pub fn try_merge_with(&self, other: &AstNode) -> Result<Arc<AstNode>, AstError> {
        match (self, other) {
            (
                AstNode::Token {
                    kind: k1,
                    text: t1,
                    range: r1,
                    ..
                },
                AstNode::Token {
                    kind: k2,
                    text: t2,
                    range: r2,
                    ..
                },
            ) => {
                // Only merge if same kind and adjacent
                if k1 != k2 {
                    return Err(AstError::IncompatibleNodeKinds);
                }

                if r1.end.0 != r2.start.0 {
                    return Err(AstError::NonAdjacentNodes);
                }

                // Check if merged size would exceed limits
                let new_size = t1.len() + t2.len();
                if new_size > MAX_BYTES {
                    return Err(AstError::NodeTooLarge);
                }

                // Create merged token
                let mut merged_text = CompactString::new("");
                merged_text.push_str(t1.as_str());
                merged_text.push_str(t2.as_str());

                Ok(Arc::new(AstNode::Token {
                    kind: *k1,
                    text: merged_text,
                    range: TextRange::new(r1.start.0, r2.end.0),
                    semantic: None, // Merged nodes lose semantic info
                }))
            },
            (
                AstNode::Syntax {
                    kind: k1,
                    children: c1,
                    range: r1,
                    ..
                },
                AstNode::Syntax {
                    kind: k2,
                    children: c2,
                    range: r2,
                    ..
                },
            ) => {
                // Only merge if same kind and adjacent
                if k1 != k2 {
                    return Err(AstError::IncompatibleNodeKinds);
                }

                if r1.end.0 != r2.start.0 {
                    return Err(AstError::NonAdjacentNodes);
                }

                // Check if merged size would exceed limits
                let total_children = c1.len() + c2.len();
                if total_children > MAX_CHILDREN {
                    return Err(AstError::NodeTooLarge);
                }

                // Create merged syntax node
                let mut merged_children = SmallVec::new();

                // Add all children from first node
                for (child, info) in c1 {
                    merged_children.push((child.clone(), *info));
                }

                // Add all children from second node
                for (child, info) in c2 {
                    merged_children.push((child.clone(), *info));
                }

                // Calculate combined info
                let merged_info = merged_children
                    .iter()
                    .map(|(_, info)| info)
                    .fold(TextInfo::empty(), |acc, info| acc.combine(info));

                Ok(Arc::new(AstNode::Syntax {
                    kind: *k1,
                    children: merged_children,
                    info: merged_info,
                    range: TextRange::new(r1.start.0, r2.end.0),
                    semantic: None, // Merged nodes lose semantic info
                }))
            },
            _ => Err(AstError::IncompatibleNodeKinds),
        }
    }

    /// Check if this node is underfull and should be merged
    pub fn is_underfull(&self) -> bool {
        match self {
            AstNode::Token { text, .. } => text.len() < MIN_BYTES,
            AstNode::Syntax { children, .. } => children.len() < MIN_CHILDREN,
        }
    }

    /// Split a syntax node in half
    pub fn split_syntax_node(&self) -> Result<(Arc<AstNode>, Arc<AstNode>), AstError> {
        match self {
            AstNode::Token { .. } => Err(AstError::CannotSplitToken),
            AstNode::Syntax {
                kind,
                children,
                range,
                ..
            } => {
                if children.len() < 2 {
                    return Err(AstError::NodeTooSmallToSplit);
                }

                let mid = children.len() / 2;
                let (left_children, right_children) = children.split_at(mid);

                // Calculate info for left node
                let left_info = left_children
                    .iter()
                    .map(|(_, info)| info)
                    .fold(TextInfo::empty(), |acc, info| acc.combine(info));

                // Calculate info for right node
                let right_info = right_children
                    .iter()
                    .map(|(_, info)| info)
                    .fold(TextInfo::empty(), |acc, info| acc.combine(info));

                // Calculate ranges
                let left_end = left_children
                    .last()
                    .map(|(node, _)| node.range().end.0)
                    .unwrap_or(range.start.0);
                let right_start = right_children
                    .first()
                    .map(|(node, _)| node.range().start.0)
                    .unwrap_or(left_end);

                let left_node = Arc::new(AstNode::Syntax {
                    kind: *kind,
                    children: left_children.into(),
                    info: left_info,
                    range: TextRange::new(range.start.0, left_end),
                    semantic: None, // Split nodes lose semantic info
                });

                let right_node = Arc::new(AstNode::Syntax {
                    kind: *kind,
                    children: right_children.into(),
                    info: right_info,
                    range: TextRange::new(right_start, range.end.0),
                    semantic: None, // Split nodes lose semantic info
                });

                Ok((left_node, right_node))
            },
        }
    }
}

/// Errors that can occur when working with AST nodes
#[derive(Debug, thiserror::Error)]
pub enum AstError {
    #[error("Cannot add child to token node")]
    CannotAddChildToToken,

    #[error("Node is too large and needs splitting")]
    NodeTooLarge,

    #[error("Invalid range: {0:?}")]
    InvalidRange(TextRange),

    #[error("Invalid split offset: {offset} (max: {max})")]
    InvalidSplitOffset { offset: usize, max: usize },

    #[error("Cannot split syntax node")]
    CannotSplitSyntaxNode,

    #[error("Incompatible node kinds for merge")]
    IncompatibleNodeKinds,

    #[error("Nodes are not adjacent")]
    NonAdjacentNodes,

    #[error("Not implemented")]
    NotImplemented,

    #[error("Cannot split token node")]
    CannotSplitToken,

    #[error("Node is too small to split")]
    NodeTooSmallToSplit,

    #[error("Overlapping edits detected: {first:?} and {second:?}")]
    OverlappingEdits { first: TextRange, second: TextRange },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_text_range() {
        let range = TextRange::new(5, 10);
        assert_eq!(range.len(), 5);
        assert!(!range.is_empty());

        let empty = TextRange::new(10, 10);
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());
    }

    #[test]
    fn test_text_info() {
        let info1 = TextInfo::from_text("hello");
        assert_eq!(info1.bytes, 5);
        assert_eq!(info1.chars, 5);
        assert_eq!(info1.tokens, 1);
        assert_eq!(info1.newlines, 0);

        let info2 = TextInfo::from_text("world\n");
        assert_eq!(info2.bytes, 6);
        assert_eq!(info2.chars, 6);
        assert_eq!(info2.tokens, 1);
        assert_eq!(info2.newlines, 1);

        let combined = info1.combine(&info2);
        assert_eq!(combined.bytes, 11);
        assert_eq!(combined.chars, 11);
        assert_eq!(combined.tokens, 2);
        assert_eq!(combined.newlines, 1);
    }

    #[test]
    fn test_ast_node_creation() {
        let token = AstNode::token(SyntaxKind::Text, "hello".into(), TextRange::new(0, 5));
        assert!(token.is_token());
        assert!(!token.is_syntax());
        assert_eq!(token.kind(), SyntaxKind::Text);
        assert_eq!(token.token_text(), Some("hello"));

        let syntax = AstNode::syntax(SyntaxKind::Paragraph, TextRange::new(0, 10));
        assert!(!syntax.is_token());
        assert!(syntax.is_syntax());
        assert_eq!(syntax.kind(), SyntaxKind::Paragraph);
    }

    #[test]
    fn test_add_child() {
        let mut parent = AstNode::syntax(SyntaxKind::Paragraph, TextRange::new(0, 10));

        let child = Arc::new(AstNode::token(
            SyntaxKind::Text,
            "hello".into(),
            TextRange::new(0, 5),
        ));

        assert!(parent.add_child(child).is_ok());
        assert_eq!(parent.text_info().bytes, 5);
        assert_eq!(
            parent
                .children()
                .expect("syntax node should have children")
                .len(),
            1
        );
    }

    #[test]
    fn test_split_token() {
        let token = AstNode::token(
            SyntaxKind::Text,
            "hello world".into(),
            TextRange::new(0, 11),
        );

        // Test valid split
        let result = token.split_token_at(5);
        assert!(result.is_ok());
        let (left, right) = result.expect("split should succeed");

        assert_eq!(left.token_text(), Some("hello"));
        assert_eq!(right.token_text(), Some(" world"));
        assert_eq!(left.range(), TextRange::new(0, 5));
        assert_eq!(right.range(), TextRange::new(5, 11));

        // Test invalid splits
        assert!(token.split_token_at(0).is_err());
        assert!(token.split_token_at(11).is_err());
    }

    #[test]
    fn test_merge_tokens() {
        let token1 = AstNode::token(SyntaxKind::Text, "hello".into(), TextRange::new(0, 5));

        let token2 = AstNode::token(SyntaxKind::Text, " world".into(), TextRange::new(5, 11));

        // Test valid merge
        let result = token1.try_merge_with(&token2);
        assert!(result.is_ok());
        let merged = result.expect("merge should succeed");

        assert_eq!(merged.token_text(), Some("hello world"));
        assert_eq!(merged.range(), TextRange::new(0, 11));

        // Test incompatible merge (different kinds)
        let token3 = AstNode::token(SyntaxKind::Whitespace, " ".into(), TextRange::new(11, 12));
        assert!(token2.try_merge_with(&token3).is_err());

        // Test non-adjacent nodes
        let token4 = AstNode::token(SyntaxKind::Text, "foo".into(), TextRange::new(20, 23));
        assert!(token1.try_merge_with(&token4).is_err());
    }

    #[test]
    fn test_merge_syntax_nodes() {
        // Create first syntax node with some children
        let token1 = Arc::new(AstNode::token(
            SyntaxKind::Text,
            "hello".into(),
            TextRange::new(0, 5),
        ));
        let token2 = Arc::new(AstNode::token(
            SyntaxKind::Whitespace,
            " ".into(),
            TextRange::new(5, 6),
        ));

        let mut syntax1 = AstNode::syntax(SyntaxKind::Paragraph, TextRange::new(0, 6));
        syntax1.add_child(token1).expect("should add child");
        syntax1.add_child(token2).expect("should add child");

        // Create second syntax node with children
        let token3 = Arc::new(AstNode::token(
            SyntaxKind::Text,
            "world".into(),
            TextRange::new(6, 11),
        ));
        let token4 = Arc::new(AstNode::token(
            SyntaxKind::Text,
            "!".into(),
            TextRange::new(11, 12),
        ));

        let mut syntax2 = AstNode::syntax(SyntaxKind::Paragraph, TextRange::new(6, 12));
        syntax2.add_child(token3).expect("should add child");
        syntax2.add_child(token4).expect("should add child");

        // Test valid merge
        let result = syntax1.try_merge_with(&syntax2);
        assert!(result.is_ok());
        let merged = result.expect("merge should succeed");

        // Verify merged node properties
        assert_eq!(merged.kind(), SyntaxKind::Paragraph);
        assert_eq!(merged.range(), TextRange::new(0, 12));

        let children = merged.children().expect("merged should have children");
        assert_eq!(children.len(), 4); // All 4 tokens

        // Verify text content
        let mut text = String::new();
        merged.collect_text(&mut text);
        assert_eq!(text, "hello world!");

        // Test incompatible merge (different kinds)
        let mut syntax3 = AstNode::syntax(SyntaxKind::Document, TextRange::new(12, 15));
        let token5 = Arc::new(AstNode::token(
            SyntaxKind::Text,
            "foo".into(),
            TextRange::new(12, 15),
        ));
        syntax3.add_child(token5).expect("should add child");

        assert!(syntax2.try_merge_with(&syntax3).is_err());

        // Test non-adjacent syntax nodes
        let mut syntax4 = AstNode::syntax(SyntaxKind::Paragraph, TextRange::new(20, 25));
        let token6 = Arc::new(AstNode::token(
            SyntaxKind::Text,
            "bar".into(),
            TextRange::new(20, 23),
        ));
        syntax4.add_child(token6).expect("should add child");

        assert!(syntax1.try_merge_with(&syntax4).is_err());
    }
}
