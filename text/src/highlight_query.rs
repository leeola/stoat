use crate::language::Language;
use std::ops::Range;
use tree_sitter::{Query, QueryCursor};

#[derive(Clone, Debug)]
pub struct HighlightCapture {
    pub byte_range: Range<usize>,
    pub capture_index: u32,
}

pub struct HighlightQuery {
    query: Query,
}

impl HighlightQuery {
    pub fn new(language: Language) -> Option<Self> {
        let ts_language = match language {
            Language::Rust => tree_sitter_rust::language(),
            Language::Markdown => tree_sitter_md::language(),
            Language::Json => tree_sitter_json::language(),
            Language::Toml => tree_sitter_toml::language(),
            Language::PlainText => return None,
        };

        let source = match language {
            Language::Rust => include_str!("../queries/rust.scm"),
            Language::Json => include_str!("../queries/json.scm"),
            Language::Toml => include_str!("../queries/toml.scm"),
            Language::Markdown => include_str!("../queries/markdown.scm"),
            Language::PlainText => return None,
        };

        let query = Query::new(ts_language, source).ok()?;
        Some(Self { query })
    }

    pub fn capture_names(&self) -> &[String] {
        self.query.capture_names()
    }

    pub fn captures(
        &self,
        tree: &tree_sitter::Tree,
        source: &[u8],
        range: Range<usize>,
    ) -> Vec<HighlightCapture> {
        let mut cursor = QueryCursor::new();
        cursor.set_byte_range(range);

        let mut result = Vec::new();
        for (match_, capture_index) in cursor.captures(&self.query, tree.root_node(), source) {
            let capture = &match_.captures[capture_index];
            let node = capture.node;
            result.push(HighlightCapture {
                byte_range: node.start_byte()..node.end_byte(),
                capture_index: capture.index,
            });
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_query_compiles() {
        let query = HighlightQuery::new(Language::Rust);
        assert!(query.is_some());
        let query = query.unwrap();
        let names = query.capture_names();
        assert!(names.contains(&"keyword".to_string()));
        assert!(names.contains(&"string".to_string()));
        assert!(names.contains(&"function".to_string()));
    }

    #[test]
    fn json_query_compiles() {
        assert!(HighlightQuery::new(Language::Json).is_some());
    }

    #[test]
    fn toml_query_compiles() {
        assert!(HighlightQuery::new(Language::Toml).is_some());
    }

    #[test]
    fn markdown_query_compiles() {
        assert!(HighlightQuery::new(Language::Markdown).is_some());
    }

    #[test]
    fn plain_text_returns_none() {
        assert!(HighlightQuery::new(Language::PlainText).is_none());
    }

    #[test]
    fn rust_captures_keywords() {
        let query = HighlightQuery::new(Language::Rust).unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(tree_sitter_rust::language()).unwrap();

        let source = b"fn main() { let x = 42; }";
        let tree = parser.parse(source, None).unwrap();

        let captures = query.captures(&tree, source, 0..source.len());
        assert!(!captures.is_empty());

        let capture_names = query.capture_names();
        let keyword_captures: Vec<_> = captures
            .iter()
            .filter(|c| capture_names[c.capture_index as usize] == "keyword")
            .collect();

        assert!(keyword_captures.len() >= 2, "should capture 'fn' and 'let'");

        let first_keyword = &source[keyword_captures[0].byte_range.clone()];
        assert_eq!(first_keyword, b"fn");
    }

    #[test]
    fn rust_captures_with_range() {
        let query = HighlightQuery::new(Language::Rust).unwrap();
        let mut parser = tree_sitter::Parser::new();
        parser.set_language(tree_sitter_rust::language()).unwrap();

        let source = b"fn foo() {}\nfn bar() {}";
        let tree = parser.parse(source, None).unwrap();

        let all = query.captures(&tree, source, 0..source.len());
        let second_line_only = query.captures(&tree, source, 12..source.len());

        assert!(all.len() > second_line_only.len());
    }
}
