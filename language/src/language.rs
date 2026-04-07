use crate::grammar;
use std::{path::Path, sync::Arc};
use tree_sitter::{Language as TsLanguage, Query};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenStyle {
    Keyword,
    KeywordControl,
    String,
    StringEscape,
    Comment,
    CommentDoc,
    Function,
    FunctionMethod,
    FunctionSpecial,
    Type,
    TypeBuiltin,
    TypeInterface,
    Constant,
    ConstantBuiltin,
    Boolean,
    Number,
    Operator,
    PunctuationBracket,
    PunctuationDelimiter,
    Property,
    Attribute,
    Variable,
    VariableParameter,
    VariableSpecial,
    Lifetime,
    Title,
    LinkText,
    LinkUri,
}

impl TokenStyle {
    pub const ALL: &'static [TokenStyle] = &[
        TokenStyle::Keyword,
        TokenStyle::KeywordControl,
        TokenStyle::String,
        TokenStyle::StringEscape,
        TokenStyle::Comment,
        TokenStyle::CommentDoc,
        TokenStyle::Function,
        TokenStyle::FunctionMethod,
        TokenStyle::FunctionSpecial,
        TokenStyle::Type,
        TokenStyle::TypeBuiltin,
        TokenStyle::TypeInterface,
        TokenStyle::Constant,
        TokenStyle::ConstantBuiltin,
        TokenStyle::Boolean,
        TokenStyle::Number,
        TokenStyle::Operator,
        TokenStyle::PunctuationBracket,
        TokenStyle::PunctuationDelimiter,
        TokenStyle::Property,
        TokenStyle::Attribute,
        TokenStyle::Variable,
        TokenStyle::VariableParameter,
        TokenStyle::VariableSpecial,
        TokenStyle::Lifetime,
        TokenStyle::Title,
        TokenStyle::LinkText,
        TokenStyle::LinkUri,
    ];

    pub fn from_capture_name(name: &str) -> Option<TokenStyle> {
        // Match longest-prefix first; tree-sitter capture names are dotted.
        match name {
            "keyword" => Some(TokenStyle::Keyword),
            "keyword.control" => Some(TokenStyle::KeywordControl),
            "string" | "string.special" => Some(TokenStyle::String),
            "string.escape" => Some(TokenStyle::StringEscape),
            "comment" => Some(TokenStyle::Comment),
            "comment.doc" => Some(TokenStyle::CommentDoc),
            "function" | "function.definition" => Some(TokenStyle::Function),
            "function.method" => Some(TokenStyle::FunctionMethod),
            "function.special" | "function.special.definition" => Some(TokenStyle::FunctionSpecial),
            "type" => Some(TokenStyle::Type),
            "type.builtin" => Some(TokenStyle::TypeBuiltin),
            "type.interface" => Some(TokenStyle::TypeInterface),
            "constant" => Some(TokenStyle::Constant),
            "constant.builtin" => Some(TokenStyle::ConstantBuiltin),
            "boolean" | "constant.builtin.boolean" => Some(TokenStyle::Boolean),
            "number" | "constant.numeric.integer" | "constant.numeric.float" => {
                Some(TokenStyle::Number)
            },
            "operator" => Some(TokenStyle::Operator),
            "punctuation.bracket" => Some(TokenStyle::PunctuationBracket),
            "punctuation.delimiter"
            | "punctuation.special"
            | "punctuation.markup"
            | "punctuation.list_marker.markup"
            | "punctuation.embedded.markup" => Some(TokenStyle::PunctuationDelimiter),
            "property" | "property.json_key" | "variable.other.member" => {
                Some(TokenStyle::Property)
            },
            "attribute" => Some(TokenStyle::Attribute),
            "variable" => Some(TokenStyle::Variable),
            "variable.parameter" => Some(TokenStyle::VariableParameter),
            "variable.special" => Some(TokenStyle::VariableSpecial),
            "lifetime" => Some(TokenStyle::Lifetime),
            "title.markup" => Some(TokenStyle::Title),
            "link_text.markup" => Some(TokenStyle::LinkText),
            "link_uri.markup" => Some(TokenStyle::LinkUri),
            _ => None,
        }
    }
}

pub struct Language {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub grammar: TsLanguage,
    pub highlight_query: Query,
    /// Indexed by tree-sitter capture index. None means the capture
    /// name is unrecognized and spans for it are skipped.
    pub capture_styles: Vec<Option<TokenStyle>>,
}

pub struct LanguageRegistry {
    languages: Vec<Arc<Language>>,
}

impl LanguageRegistry {
    pub fn standard() -> Self {
        Self {
            languages: vec![
                Arc::new(make_rust()),
                Arc::new(make_json()),
                Arc::new(make_toml()),
                Arc::new(make_markdown()),
            ],
        }
    }

    pub fn for_path(&self, path: &Path) -> Option<Arc<Language>> {
        let ext = path.extension()?.to_str()?;
        self.languages
            .iter()
            .find(|l| l.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
            .cloned()
    }

    pub fn languages(&self) -> &[Arc<Language>] {
        &self.languages
    }
}

fn make_language(
    name: &'static str,
    extensions: &'static [&'static str],
    grammar: TsLanguage,
    highlight_src: &str,
) -> Language {
    let highlight_query = Query::new(&grammar, highlight_src)
        .unwrap_or_else(|e| panic!("highlight query for {name} failed to compile: {e}"));
    let capture_styles: Vec<Option<TokenStyle>> = highlight_query
        .capture_names()
        .iter()
        .map(|n| TokenStyle::from_capture_name(n))
        .collect();
    Language {
        name,
        extensions,
        grammar,
        highlight_query,
        capture_styles,
    }
}

fn make_rust() -> Language {
    make_language(
        "rust",
        &["rs"],
        grammar::rust(),
        include_str!("../../vendor/zed/crates/languages/src/rust/highlights.scm"),
    )
}

fn make_json() -> Language {
    make_language(
        "json",
        &["json"],
        grammar::json(),
        include_str!("../../vendor/zed/crates/languages/src/json/highlights.scm"),
    )
}

fn make_toml() -> Language {
    make_language(
        "toml",
        &["toml"],
        grammar::toml(),
        include_str!("../../vendor/helix/runtime/queries/toml/highlights.scm"),
    )
}

fn make_markdown() -> Language {
    make_language(
        "markdown",
        &["md", "markdown"],
        grammar::markdown(),
        include_str!("../../vendor/zed/crates/languages/src/markdown/highlights.scm"),
    )
}

#[cfg(test)]
mod tests {
    use super::{LanguageRegistry, TokenStyle};
    use std::path::Path;

    #[test]
    fn for_path_resolves_extensions() {
        let reg = LanguageRegistry::standard();
        assert_eq!(reg.for_path(Path::new("a.rs")).unwrap().name, "rust");
        assert_eq!(reg.for_path(Path::new("a.json")).unwrap().name, "json");
        assert_eq!(reg.for_path(Path::new("a.toml")).unwrap().name, "toml");
        assert_eq!(reg.for_path(Path::new("a.md")).unwrap().name, "markdown");
        assert_eq!(
            reg.for_path(Path::new("a.markdown")).unwrap().name,
            "markdown"
        );
        assert_eq!(reg.for_path(Path::new("a.RS")).unwrap().name, "rust");
        assert!(reg.for_path(Path::new("a.txt")).is_none());
        assert!(reg.for_path(Path::new("noext")).is_none());
    }

    #[test]
    fn standard_compiles_all_queries() {
        // Constructor unwraps query compile errors; this test triggers
        // those panics in CI to catch query/runtime mismatches early.
        let _reg = LanguageRegistry::standard();
    }

    #[test]
    fn capture_styles_populated() {
        let reg = LanguageRegistry::standard();
        for lang in reg.languages() {
            assert_eq!(
                lang.capture_styles.len(),
                lang.highlight_query.capture_names().len(),
                "{} capture_styles length must match capture_names",
                lang.name
            );
            // At least one capture should map to a known TokenStyle.
            assert!(
                lang.capture_styles.iter().any(|s| s.is_some()),
                "{} has no recognized captures",
                lang.name
            );
        }
    }

    #[test]
    fn from_capture_name_known_and_unknown() {
        assert_eq!(
            TokenStyle::from_capture_name("keyword"),
            Some(TokenStyle::Keyword)
        );
        assert_eq!(
            TokenStyle::from_capture_name("function.method"),
            Some(TokenStyle::FunctionMethod)
        );
        assert_eq!(
            TokenStyle::from_capture_name("punctuation.embedded.markup"),
            Some(TokenStyle::PunctuationDelimiter)
        );
        assert_eq!(TokenStyle::from_capture_name("nope.unknown"), None);
        assert_eq!(TokenStyle::from_capture_name("none"), None);
    }
}
