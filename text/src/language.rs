//! Language identification and detection

/// Supported languages for tree-sitter parsing
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    /// Rust programming language
    Rust,
    /// Markdown formatted text
    Markdown,
    /// Markdown inline content (emphasis, links, code spans, etc.)
    MarkdownInline,
    /// JSON data format
    Json,
    /// TOML configuration format
    Toml,
    /// Plain text (no tree-sitter parsing)
    PlainText,
}

impl Language {
    /// Detect language from name (e.g. code fence tags like "rust", "json")
    pub fn from_name(name: &str) -> Self {
        match name.to_lowercase().as_str() {
            "rust" | "rs" => Language::Rust,
            "markdown" | "md" => Language::Markdown,
            "json" => Language::Json,
            "toml" => Language::Toml,
            _ => Language::PlainText,
        }
    }

    /// Detect language from file extension
    pub fn from_extension(ext: &str) -> Self {
        match ext.to_lowercase().as_str() {
            "rs" => Language::Rust,
            "md" | "markdown" | "mdown" | "mkdn" | "mkd" => Language::Markdown,
            "json" => Language::Json,
            "toml" => Language::Toml,
            _ => Language::PlainText,
        }
    }

    /// Get file extensions for this language
    pub fn extensions(&self) -> &'static [&'static str] {
        match self {
            Language::Rust => &["rs"],
            Language::Markdown => &["md", "markdown"],
            Language::MarkdownInline => &[],
            Language::Json => &["json"],
            Language::Toml => &["toml"],
            Language::PlainText => &["txt"],
        }
    }

    /// Get human-readable name
    pub fn name(&self) -> &'static str {
        match self {
            Language::Rust => "Rust",
            Language::Markdown => "Markdown",
            Language::MarkdownInline => "Markdown Inline",
            Language::Json => "JSON",
            Language::Toml => "TOML",
            Language::PlainText => "Plain Text",
        }
    }
}
