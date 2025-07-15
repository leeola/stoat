//! Syntax nodes with rope offsets

use crate::{range::TextRange, syntax::kind::Syntax};
use once_cell::sync::OnceCell;
use parking_lot::RwLock;
use std::sync::{Arc, Weak};

/// A syntax node in the AST
pub struct SyntaxNode<S: Syntax> {
    data: Arc<SyntaxNodeData<S>>,
}

struct SyntaxNodeData<S: Syntax> {
    /// The kind of this node
    kind: S::Kind,
    /// Byte range in the rope
    range: TextRange,
    /// Cached text content
    text: OnceCell<String>,
    /// Parent node (if any)
    parent: RwLock<Option<Weak<SyntaxNodeData<S>>>>,
    /// Children (lazily parsed)
    children: OnceCell<Vec<SyntaxElement<S>>>,
}

/// Either a node or a token
pub enum SyntaxElement<S: Syntax> {
    Node(SyntaxNode<S>),
    Token(SyntaxToken<S>),
}

/// A syntax token (leaf node)
pub struct SyntaxToken<S: Syntax> {
    kind: S::Kind,
    range: TextRange,
    text: Arc<str>,
}

impl<S: Syntax> SyntaxNode<S> {
    /// Create a new syntax node (internal use)
    pub(crate) fn new(kind: S::Kind, range: TextRange) -> Self {
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
    pub(crate) fn new_with_text(kind: S::Kind, range: TextRange, text: String) -> Self {
        let node = Self::new(kind, range);
        let _ = node.data.text.set(text);
        node
    }

    /// Get the kind of this node
    pub fn kind(&self) -> S::Kind {
        self.data.kind
    }

    /// Get the text range of this node
    pub fn text_range(&self) -> TextRange {
        self.data.range
    }

    /// Get the text content of this node
    pub fn text(&self) -> &str {
        self.data.text.get_or_init(|| {
            // TODO: Extract text from rope via buffer
            String::new()
        })
    }

    /// Get the parent node
    pub fn parent(&self) -> Option<SyntaxNode<S>> {
        self.data
            .parent
            .read()
            .as_ref()
            .and_then(|weak| weak.upgrade())
            .map(|data| SyntaxNode { data })
    }

    /// Get child nodes (triggers lazy parsing if needed)
    pub fn children(&self) -> &[SyntaxElement<S>] {
        self.data.children.get_or_init(|| {
            // TODO: Lazy parse children
            Vec::new()
        })
    }

    /// Get the first child
    pub fn first_child(&self) -> Option<SyntaxNode<S>> {
        self.children().iter().find_map(|child| match child {
            SyntaxElement::Node(node) => Some(node.clone()),
            SyntaxElement::Token(_) => None,
        })
    }

    /// Get child at index
    pub fn child(&self, index: usize) -> Option<SyntaxNode<S>> {
        self.children()
            .iter()
            .filter_map(|child| match child {
                SyntaxElement::Node(node) => Some(node.clone()),
                SyntaxElement::Token(_) => None,
            })
            .nth(index)
    }

    /// Find a descendant matching the predicate
    pub fn find_descendant(
        &self,
        predicate: impl Fn(&SyntaxNode<S>) -> bool,
    ) -> Option<SyntaxNode<S>> {
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
    pub fn tokens(&self) -> Vec<SyntaxToken<S>> {
        let mut tokens = Vec::new();
        self.collect_tokens(&mut tokens);
        tokens
    }

    fn collect_tokens(&self, tokens: &mut Vec<SyntaxToken<S>>) {
        for child in self.children() {
            match child {
                SyntaxElement::Token(token) => tokens.push(token.clone()),
                SyntaxElement::Node(node) => node.collect_tokens(tokens),
            }
        }
    }
}

impl<S: Syntax> Clone for SyntaxNode<S> {
    fn clone(&self) -> Self {
        Self {
            data: self.data.clone(),
        }
    }
}

impl<S: Syntax> SyntaxToken<S> {
    /// Create a new syntax token
    pub fn new(kind: S::Kind, range: TextRange, text: Arc<str>) -> Self {
        Self { kind, range, text }
    }

    /// Get the kind of this token
    pub fn kind(&self) -> S::Kind {
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

impl<S: Syntax> Clone for SyntaxToken<S> {
    fn clone(&self) -> Self {
        Self {
            kind: self.kind,
            range: self.range,
            text: self.text.clone(),
        }
    }
}

impl<S: Syntax> SyntaxElement<S> {
    /// Get the kind of this element
    pub fn kind(&self) -> S::Kind {
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

impl<S: Syntax> Clone for SyntaxElement<S> {
    fn clone(&self) -> Self {
        match self {
            SyntaxElement::Node(node) => SyntaxElement::Node(node.clone()),
            SyntaxElement::Token(token) => SyntaxElement::Token(token.clone()),
        }
    }
}
