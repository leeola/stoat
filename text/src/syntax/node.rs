//! Syntax nodes with rope offsets

use crate::{TextSize, range::TextRange, syntax::unified_kind::SyntaxKind};
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use std::sync::{Arc, Weak};

/// A syntax node in the AST
pub struct SyntaxNode {
    data: Arc<SyntaxNodeData>,
}

struct SyntaxNodeData {
    /// The kind of this node
    kind: SyntaxKind,
    /// Byte range in the rope
    range: TextRange,
    /// Cached text content
    text: OnceCell<String>,
    /// Parent node (if any)
    parent: RwLock<Option<Weak<SyntaxNodeData>>>,
    /// Children (lazily parsed)
    children: OnceCell<Vec<SyntaxElement>>,
}

/// Either a node or a token
pub enum SyntaxElement {
    Node(SyntaxNode),
    Token(SyntaxToken),
}

/// A syntax token (leaf node)
pub struct SyntaxToken {
    kind: SyntaxKind,
    range: TextRange,
    text: Arc<str>,
}

impl SyntaxNode {
    /// Create a new syntax node (internal use)
    pub(crate) fn new(kind: SyntaxKind, range: TextRange) -> Self {
        Self {
            data: Arc::new(SyntaxNodeData {
                kind,
                range,
                text: OnceCell::new(),
                parent: RwLock::new(None),
                children: OnceCell::new(),
            }),
        }
    }

    /// Create a new syntax node with text (for testing)
    #[cfg(test)]
    pub(crate) fn new_with_text(kind: SyntaxKind, range: TextRange, text: String) -> Self {
        let node = Self::new(kind, range);
        let _ = node.data.text.set(text);
        node
    }

    /// Create a new syntax node with children
    pub(crate) fn new_with_children(
        kind: SyntaxKind,
        range: TextRange,
        children: Vec<SyntaxElement>,
    ) -> Self {
        let node = Self::new(kind, range);

        // Set parent pointers for child nodes
        let weak_parent = Arc::downgrade(&node.data);
        for child in &children {
            if let SyntaxElement::Node(child_node) = child {
                *child_node.data.parent.write() = Some(weak_parent.clone());
            }
        }

        let _ = node.data.children.set(children);
        node
    }

    /// Get the kind of this node
    pub fn kind(&self) -> SyntaxKind {
        self.data.kind
    }

    /// Get the text range of this node
    pub fn text_range(&self) -> TextRange {
        self.data.range
    }

    /// Get the text content of this node
    pub fn text(&self) -> &str {
        self.data.text.get_or_init(|| {
            // Build text from children
            let mut text = String::new();
            for child in self.children() {
                match child {
                    SyntaxElement::Token(token) => text.push_str(token.text()),
                    SyntaxElement::Node(node) => text.push_str(node.text()),
                }
            }
            text
        })
    }

    /// Get the parent node
    pub fn parent(&self) -> Option<SyntaxNode> {
        self.data
            .parent
            .read()
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .map(|data| SyntaxNode { data })
    }

    /// Get child nodes (triggers lazy parsing if needed)
    pub fn children(&self) -> &[SyntaxElement] {
        self.data.children.get_or_init(Vec::new)
    }

    /// Get the first child
    pub fn first_child(&self) -> Option<SyntaxNode> {
        self.children().iter().find_map(|child| match child {
            SyntaxElement::Node(node) => Some(node.clone()),
            SyntaxElement::Token(_) => None,
        })
    }

    /// Get child at index
    pub fn child(&self, index: usize) -> Option<SyntaxNode> {
        self.children()
            .iter()
            .filter_map(|child| match child {
                SyntaxElement::Node(node) => Some(node.clone()),
                SyntaxElement::Token(_) => None,
            })
            .nth(index)
    }

    /// Find a descendant matching the predicate
    pub fn find_descendant(&self, predicate: impl Fn(&SyntaxNode) -> bool) -> Option<SyntaxNode> {
        if predicate(self) {
            return Some(self.clone());
        }

        for child in self.children() {
            if let SyntaxElement::Node(node) = child {
                if let Some(found) = node.find_descendant(&predicate) {
                    return Some(found);
                }
            }
        }

        None
    }

    /// Get all tokens in this subtree
    pub fn tokens(&self) -> Vec<SyntaxToken> {
        let mut tokens = Vec::new();
        self.collect_tokens(&mut tokens);
        tokens
    }

    fn collect_tokens(&self, tokens: &mut Vec<SyntaxToken>) {
        for child in self.children() {
            match child {
                SyntaxElement::Token(token) => tokens.push(token.clone()),
                SyntaxElement::Node(node) => node.collect_tokens(tokens),
            }
        }
    }

    /// Find the token at the given offset
    pub fn find_token_at_offset(&self, offset: TextSize) -> Option<SyntaxToken> {
        // First check if offset is within our range
        if !self.text_range().contains(offset) {
            return None;
        }

        // Search through children
        for child in self.children() {
            match child {
                SyntaxElement::Token(token) => {
                    if token.text_range().contains(offset) {
                        return Some(token.clone());
                    }
                },
                SyntaxElement::Node(node) => {
                    if let Some(token) = node.find_token_at_offset(offset) {
                        return Some(token);
                    }
                },
            }
        }

        None
    }

    /// Find the deepest node containing the given offset
    pub fn find_node_at_offset(&self, offset: TextSize) -> Option<SyntaxNode> {
        // First check if offset is within our range
        if !self.text_range().contains(offset) {
            return None;
        }

        // Try to find a child node containing the offset
        for child in self.children() {
            if let SyntaxElement::Node(node) = child {
                if node.text_range().contains(offset) {
                    // Recursively find the deepest node
                    if let Some(deeper) = node.find_node_at_offset(offset) {
                        return Some(deeper);
                    }
                    return Some(node.clone());
                }
            }
        }

        // No child contains it, so we're the deepest
        Some(self.clone())
    }

    /// Get the next sibling of this node
    pub fn next_sibling(&self) -> Option<SyntaxNode> {
        let parent = self.parent()?;
        let siblings = parent.children();

        // Find our position among siblings
        let mut found_self = false;
        for child in siblings {
            if found_self {
                if let SyntaxElement::Node(sibling) = child {
                    return Some(sibling.clone());
                }
            } else if let SyntaxElement::Node(node) = child {
                // Check if this is us by comparing ranges (since we don't have direct equality)
                if node.text_range() == self.text_range() && node.kind() == self.kind() {
                    found_self = true;
                }
            }
        }

        None
    }

    /// Get the previous sibling of this node
    pub fn prev_sibling(&self) -> Option<SyntaxNode> {
        let parent = self.parent()?;
        let siblings = parent.children();

        let mut prev_node = None;
        for child in siblings {
            if let SyntaxElement::Node(node) = child {
                // Check if this is us
                if node.text_range() == self.text_range() && node.kind() == self.kind() {
                    return prev_node;
                }
                prev_node = Some(node.clone());
            }
        }

        None
    }
}

impl Clone for SyntaxNode {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
        }
    }
}

impl SyntaxToken {
    /// Create a new syntax token
    pub fn new(kind: SyntaxKind, range: TextRange, text: Arc<str>) -> Self {
        Self { kind, range, text }
    }

    /// Get the kind of this token
    pub fn kind(&self) -> SyntaxKind {
        self.kind
    }

    /// Get the text range of this token
    pub fn text_range(&self) -> TextRange {
        self.range
    }

    /// Get the text content of this token
    pub fn text(&self) -> &str {
        &self.text
    }
}

impl Clone for SyntaxToken {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind,
            range: self.range,
            text: self.text.clone(),
        }
    }
}

impl SyntaxElement {
    /// Get the kind of this element
    pub fn kind(&self) -> SyntaxKind {
        match self {
            SyntaxElement::Node(node) => node.kind(),
            SyntaxElement::Token(token) => token.kind(),
        }
    }

    /// Get the text range of this element
    pub fn text_range(&self) -> TextRange {
        match self {
            SyntaxElement::Node(node) => node.text_range(),
            SyntaxElement::Token(token) => token.text_range(),
        }
    }

    /// Get the text content of this element
    pub fn text(&self) -> &str {
        match self {
            SyntaxElement::Node(node) => node.text(),
            SyntaxElement::Token(token) => token.text(),
        }
    }
}

impl Clone for SyntaxElement {
    fn clone(&self) -> Self {
        match self {
            SyntaxElement::Node(node) => SyntaxElement::Node(node.clone()),
            SyntaxElement::Token(token) => SyntaxElement::Token(token.clone()),
        }
    }
}
