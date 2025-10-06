//! Semantic metadata support - copied from stoat_rope

use std::fmt;

/// A unique identifier for semantic information attached to a node
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SemanticId(pub u64);

impl SemanticId {
    /// Create a new semantic ID
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Get the raw ID value
    pub const fn value(&self) -> u64 {
        self.0
    }
}

impl fmt::Display for SemanticId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sem:{}", self.0)
    }
}

/// Types of semantic relationships between nodes
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticKind {
    /// This node defines a symbol
    Definition,
    /// This node references a symbol defined elsewhere
    Reference,
    /// This node is a type annotation
    TypeAnnotation,
    /// This node is a function/method call
    Call,
    /// This node is an import/use statement
    Import,
    /// This node is a declaration (without body)
    Declaration,
    /// Custom semantic kind for language-specific needs
    Custom(u16),
}

impl SemanticKind {
    /// Get a human-readable name for this semantic kind
    pub fn as_str(&self) -> &'static str {
        match self {
            SemanticKind::Definition => "definition",
            SemanticKind::Reference => "reference",
            SemanticKind::TypeAnnotation => "type_annotation",
            SemanticKind::Call => "call",
            SemanticKind::Import => "import",
            SemanticKind::Declaration => "declaration",
            SemanticKind::Custom(_) => "custom",
        }
    }
}

impl fmt::Display for SemanticKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SemanticKind::Custom(id) => write!(f, "custom:{id}"),
            _ => write!(f, "{}", self.as_str()),
        }
    }
}

/// Semantic metadata that can be attached to a node
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SemanticInfo {
    /// The semantic ID for external data lookup
    pub id: SemanticId,
    /// The kind of semantic relationship
    pub kind: SemanticKind,
}

impl SemanticInfo {
    /// Create new semantic info
    pub const fn new(id: SemanticId, kind: SemanticKind) -> Self {
        Self { id, kind }
    }

    /// Create semantic info for a definition
    pub const fn definition(id: SemanticId) -> Self {
        Self::new(id, SemanticKind::Definition)
    }

    /// Create semantic info for a reference
    pub const fn reference(id: SemanticId) -> Self {
        Self::new(id, SemanticKind::Reference)
    }

    /// Create semantic info for a type annotation
    pub const fn type_annotation(id: SemanticId) -> Self {
        Self::new(id, SemanticKind::TypeAnnotation)
    }

    /// Create semantic info for a function call
    pub const fn call(id: SemanticId) -> Self {
        Self::new(id, SemanticKind::Call)
    }
}
