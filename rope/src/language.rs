//! Language context for AST nodes.
//!
//! This module defines the language context that can be associated with
//! AST nodes to indicate what language rules and keymaps should apply.

/// Language context for a node in the AST.
///
/// Each node can optionally have a language context that indicates
/// what language-specific behavior should apply at that position.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    /// Plain text with no special language rules
    PlainText,

    /// Markdown formatted text
    Markdown,

    /// Rust programming language
    Rust,
    // Future languages can be added here:
    // Python,
    // JavaScript,
    // TypeScript,
    // Go,
    // etc.
}

impl Language {
    /// Returns a human-readable name for the language.
    pub fn name(&self) -> &'static str {
        match self {
            Language::PlainText => "Plain Text",
            Language::Markdown => "Markdown",
            Language::Rust => "Rust",
        }
    }

    /// Returns a short identifier for the language.
    pub fn id(&self) -> &'static str {
        match self {
            Language::PlainText => "plain",
            Language::Markdown => "markdown",
            Language::Rust => "rust",
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.name())
    }
}
