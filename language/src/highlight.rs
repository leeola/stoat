use crate::language::{Language, TokenStyle};
use std::ops::Range;
use tree_sitter::{Parser, QueryCursor, StreamingIterator, Tree};

pub struct SyntaxState {
    pub tree: Tree,
    pub version: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub byte_range: Range<usize>,
    pub style: TokenStyle,
}

pub fn parse(language: &Language, text: &str, old_tree: Option<&Tree>) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&language.grammar).ok()?;
    parser.parse(text, old_tree)
}

// Tree-sitter highlight query convention: later patterns in the .scm file
// are more specific and should override earlier ones. The downstream merger
// keys all syntax tokens under a single HighlightKey, so "last write wins".
// Sorting spans by (start, pattern_index) ensures spans for the same byte
// range emit in pattern order, giving the latest pattern priority in the
// merger.
pub fn extract_highlights(language: &Language, tree: &Tree, text: &str) -> Vec<HighlightSpan> {
    let mut raw: Vec<(Range<usize>, usize, TokenStyle)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&language.highlight_query, tree.root_node(), text.as_bytes());

    while let Some(m) = matches.next() {
        let pattern = m.pattern_index;
        for capture in m.captures {
            let style = match language.capture_styles.get(capture.index as usize) {
                Some(Some(s)) => *s,
                _ => continue,
            };
            let start = capture.node.start_byte();
            let end = capture.node.end_byte();
            if start == end {
                continue;
            }
            raw.push((start..end, pattern, style));
        }
    }

    raw.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(a.1.cmp(&b.1)));

    raw.into_iter()
        .map(|(byte_range, _, style)| HighlightSpan { byte_range, style })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{extract_highlights, parse, HighlightSpan};
    use crate::language::{LanguageRegistry, TokenStyle};

    fn rust() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .unwrap()
    }

    fn json() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.json"))
            .unwrap()
    }

    fn toml() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.toml"))
            .unwrap()
    }

    fn markdown() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.md"))
            .unwrap()
    }

    /// Returns the last span matching the byte range of `fragment` in `text`.
    /// Last is the one the downstream merger picks for overlapping spans.
    fn span_for_text<'a>(
        spans: &'a [HighlightSpan],
        text: &str,
        fragment: &str,
    ) -> Option<&'a HighlightSpan> {
        let start = text.find(fragment)?;
        let end = start + fragment.len();
        spans
            .iter()
            .rev()
            .find(|s| s.byte_range.start == start && s.byte_range.end == end)
    }

    #[test]
    fn parse_rust_smoke() {
        let lang = rust();
        let tree = parse(&lang, "fn main() {}", None).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[test]
    fn parse_json_smoke() {
        let lang = json();
        let tree = parse(&lang, "{}", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
    }

    #[test]
    fn parse_toml_smoke() {
        let lang = toml();
        let tree = parse(&lang, "a = 1\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
    }

    #[test]
    fn parse_markdown_smoke() {
        let lang = markdown();
        let tree = parse(&lang, "# Title\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
    }

    #[test]
    fn rust_keyword_captured() {
        let text = "fn main() {}";
        let lang = rust();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let span = span_for_text(&spans, text, "fn").expect("fn span");
        assert_eq!(span.style, TokenStyle::Keyword);
    }

    #[test]
    fn rust_string_captured() {
        let text = r#"fn main() { let _ = "hi"; }"#;
        let lang = rust();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let span = span_for_text(&spans, text, "\"hi\"").expect("string span");
        assert_eq!(span.style, TokenStyle::String);
    }

    #[test]
    fn rust_function_definition_captured() {
        let text = "fn main() {}";
        let lang = rust();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let span = span_for_text(&spans, text, "main").expect("main span");
        assert_eq!(span.style, TokenStyle::Function);
    }

    #[test]
    fn json_property_and_string_captured() {
        let text = r#"{"a":"b"}"#;
        let lang = json();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let key = span_for_text(&spans, text, "\"a\"").expect("key span");
        let value = span_for_text(&spans, text, "\"b\"").expect("value span");
        assert_eq!(key.style, TokenStyle::Property);
        assert_eq!(value.style, TokenStyle::String);
    }

    #[test]
    fn markdown_title_captured() {
        let text = "# Title\n";
        let lang = markdown();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        assert!(
            spans.iter().any(|s| s.style == TokenStyle::Title),
            "expected at least one Title span, got {spans:?}"
        );
    }

    #[test]
    fn spans_sorted_by_start() {
        let text = "fn a() { fn b() {} }";
        let lang = rust();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        for w in spans.windows(2) {
            assert!(w[0].byte_range.start <= w[1].byte_range.start);
        }
    }
}
