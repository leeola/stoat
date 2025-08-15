//! Semantic syntax layer for cross-language AST representation.
//!
//! This module defines a universal semantic token system that maps language-specific
//! syntax elements to common programming concepts. Instead of storing Rust-specific
//! tokens like `Fn` or Python-specific `def`, we use semantic kinds like [`SemanticKind::Function`]
//! that work across all languages.
//!
//! # Architecture
//!
//! The SST (Stoat Syntax Tree) uses a two-layer approach:
//!
//! 1. **Semantic Layer** ([`SemanticKind`]) - Universal programming concepts
//! 2. **Syntax Layer** (stored as strings) - Original tree-sitter node types
//!
//! This enables cross-language operations:
//! - "Find all functions" works in Rust, Python, JavaScript, etc.
//! - "Extract parameter" refactoring can share logic across languages
//! - Navigation by intent rather than syntax
//!
//! # Language Mapping
//!
//! Each language parser provides a mapping from its tree-sitter node types to semantic kinds:
//!
//! ```text
//! Rust:       "function_item"       -> SemanticKind::Function
//! Python:     "function_definition" -> SemanticKind::Function  
//! JavaScript: "function_declaration" -> SemanticKind::Function
//! Go:         "function_declaration" -> SemanticKind::Function
//! ```
//!
//! # Example Usage
//!
//! ```ignore
//! // Find all variable declarations regardless of language
//! let vars = tree.find_by_kind(SemanticKind::VariableDecl);
//!
//! // Check if cursor is inside any function
//! let in_function = tree.is_in_context(cursor_node, SemanticKind::Function);
//!
//! // Works the same whether the code is Rust, Python, or JavaScript!
//! ```

/// Semantic kinds representing universal programming concepts.
///
/// These kinds abstract over language-specific syntax to enable cross-language
/// queries and operations. Each kind represents a concept that exists across
/// most programming languages, though the specific syntax varies.
///
/// # Design Principles
///
/// - **Universal**: Each kind should exist in most languages
/// - **Semantic**: Represents meaning/purpose, not syntax
/// - **Composable**: Complex structures built from simple kinds
/// - **Extensible**: New kinds can be added as needed
///
/// # Fallback Strategy
///
/// When a tree-sitter node doesn't map to any semantic kind, it receives
/// [`SemanticKind::Unknown`]. The original node type is preserved in the
/// `syntax_kind` field for language-specific operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticKind {
    /// Root node of a file or module.
    ///
    /// Maps to the top-level container in each language:
    /// - Rust: `source_file`
    /// - Python: `module`
    /// - JavaScript: `program`
    Module,

    /// Function or method definition.
    ///
    /// Covers all callable code units:
    /// - Regular functions
    /// - Methods (instance/static)
    /// - Lambdas/closures
    /// - Arrow functions
    ///
    /// Language mappings:
    /// - Rust: `function_item`, `closure_expression`
    /// - Python: `function_definition`, `lambda`
    /// - JavaScript: `function_declaration`, `arrow_function`, `method_definition`
    Function,

    /// Code block or scope.
    ///
    /// Any bracketed region that creates a new scope:
    /// - Function bodies
    /// - Loop bodies
    /// - Conditional branches
    /// - Anonymous blocks
    ///
    /// Language mappings:
    /// - Rust: `block`
    /// - Python: `block` (indented)
    /// - JavaScript: `statement_block`
    Block,

    /// Variable declaration or definition.
    ///
    /// Includes all forms of variable binding:
    /// - Immutable bindings (`let`, `const`)
    /// - Mutable bindings (`let mut`, `var`)
    /// - Pattern bindings
    ///
    /// Language mappings:
    /// - Rust: `let_declaration`
    /// - Python: `assignment` (when creating new binding)
    /// - JavaScript: `variable_declaration`
    VariableDecl,

    /// Function parameter or argument declaration.
    ///
    /// Formal parameters in function signatures:
    /// - Positional parameters
    /// - Named/keyword parameters
    /// - Rest/variadic parameters
    ///
    /// Language mappings:
    /// - Rust: `parameter`, `self_parameter`
    /// - Python: `parameter`, `typed_parameter`
    /// - JavaScript: `formal_parameters`
    Parameter,

    /// Assignment expression or statement.
    ///
    /// Value assignment to existing bindings:
    /// - Simple assignment (`x = 1`)
    /// - Compound assignment (`x += 1`)
    /// - Destructuring assignment
    ///
    /// Language mappings:
    /// - Rust: `assignment_expression`
    /// - Python: `assignment`
    /// - JavaScript: `assignment_expression`
    Assignment,

    /// Function or method call.
    ///
    /// Any invocation of callable code:
    /// - Function calls
    /// - Method calls
    /// - Constructor calls
    ///
    /// Language mappings:
    /// - Rust: `call_expression`
    /// - Python: `call`
    /// - JavaScript: `call_expression`
    Call,

    /// Identifier reference.
    ///
    /// Names that reference values:
    /// - Variable names
    /// - Function names
    /// - Type names
    /// - Module names
    ///
    /// Language mappings:
    /// - Rust: `identifier`
    /// - Python: `identifier`
    /// - JavaScript: `identifier`
    Identifier,

    /// Literal value.
    ///
    /// Compile-time constant values:
    /// - Numbers (`42`, `3.14`)
    /// - Strings (`"hello"`)
    /// - Booleans (`true`, `false`)
    /// - Characters (`'a'`)
    ///
    /// Language mappings:
    /// - Rust: `integer_literal`, `string_literal`, `boolean_literal`
    /// - Python: `integer`, `string`, `true`, `false`
    /// - JavaScript: `number`, `string`, `true`, `false`
    Literal,

    /// Whitespace between tokens.
    ///
    /// Preserved for:
    /// - Maintaining correct text offsets
    /// - Formatting preservation
    /// - Whitespace-sensitive languages
    Whitespace,

    /// Single-line comment.
    ///
    /// Regular comments for code documentation:
    /// - Rust: `//`, `/* */`
    /// - Python: `#`
    /// - JavaScript: `//`, `/* */`
    Comment,

    /// Documentation comment.
    ///
    /// Special comments that generate documentation:
    /// - Rust: `///`, `//!`
    /// - Python: `"""docstring"""`
    /// - JavaScript: `/** JSDoc */`
    ///
    /// These often trigger embedded language parsing (Markdown).
    DocComment,

    /// Unknown or unmapped syntax.
    ///
    /// Fallback for tree-sitter nodes without semantic mapping.
    /// The original node type is preserved in `syntax_kind`.
    Unknown,
}

/// Language identifier for multi-language support.
///
/// Identifies which language a syntax tree or node belongs to,
/// enabling language-specific behavior while maintaining a
/// universal semantic layer.
///
/// # Adding New Languages
///
/// To add a new language:
/// 1. Add a variant to this enum
/// 2. Implement tree-sitter -> [`SemanticKind`] mapping
/// 3. Register language-specific handlers
///
/// For experimental or rare languages, use [`LanguageId::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageId {
    /// Rust programming language
    Rust,
    /// Python programming language
    Python,
    /// JavaScript programming language
    JavaScript,
    /// TypeScript programming language
    TypeScript,
    /// Go programming language
    Go,
    /// Other languages identified by name.
    ///
    /// Use this for languages without dedicated variants.
    /// The string should be a standard identifier like "ruby" or "c++".
    Other(&'static str),
}
