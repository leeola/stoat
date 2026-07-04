//! Bracket-pair matching from the `brackets.scm` query.
//!
//! The query captures matched delimiters as `@open`/`@close` pairs in the zed
//! dialect. Only structural bracket tokens are captured. A bracket character
//! inside a string, char, or comment literal is never an `@open`/`@close` node,
//! so it resolves to no match. The query is therefore a grammar-accurate
//! replacement for scanning text and guessing which brackets are real.

use crate::highlight::{QueryCursorHandle, RopeTextProvider};
use std::ops::Range;
use stoat_text::Rope;
use tree_sitter::{Node, Query, StreamingIterator};

/// Byte offset of the bracket matching the delimiter at `offset`, resolved from
/// the `brackets.scm` query's `@open`/`@close` captures.
///
/// When `offset` falls on an `@open` token, returns the paired `@close` token's
/// start. On a `@close`, returns the `@open`'s start. Returns `None` when no
/// captured pair covers `offset`, including any bracket character inside a
/// string, char, or comment literal, which the query never captures.
pub fn matching_bracket(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    offset: usize,
) -> Option<usize> {
    let open_ix = query.capture_index_for_name("open")?;
    let close_ix = query.capture_index_for_name("close")?;

    let provider = RopeTextProvider { rope };
    let mut cursor = QueryCursorHandle::new();
    let mut matches = cursor.matches(query, root, provider);

    while let Some(m) = matches.next() {
        let mut open: Option<Range<usize>> = None;
        let mut close: Option<Range<usize>> = None;
        for cap in m.captures {
            if cap.index == open_ix {
                open.get_or_insert_with(|| cap.node.start_byte()..cap.node.end_byte());
            } else if cap.index == close_ix {
                close.get_or_insert_with(|| cap.node.start_byte()..cap.node.end_byte());
            }
        }

        let (Some(open), Some(close)) = (open, close) else {
            continue;
        };
        if open.contains(&offset) {
            return Some(close.start);
        }
        if close.contains(&offset) {
            return Some(open.start);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::matching_bracket;
    use crate::{Language, LanguageRegistry};
    use std::sync::Arc;
    use stoat_text::Rope;
    use tree_sitter::{Parser, Tree};

    fn lang(name: &str) -> Arc<Language> {
        LanguageRegistry::standard()
            .languages()
            .iter()
            .find(|l| l.name == name)
            .cloned()
            .expect("language registered")
    }

    fn parse(lang: &Language, src: &str) -> Tree {
        let mut parser = Parser::new();
        parser.set_language(&lang.grammar).expect("grammar");
        parser.parse(src, None).expect("parse")
    }

    fn match_at(name: &str, src: &str, offset: usize) -> Option<usize> {
        let lang = lang(name);
        let tree = parse(&lang, src);
        let rope = Rope::from(src);
        matching_bracket(
            lang.bracket_query.as_ref().expect("bracket query"),
            tree.root_node(),
            &rope,
            offset,
        )
    }

    #[test]
    fn rust_open_paren_matches_close() {
        // `fn a() {}`: `(` at 4 pairs with `)` at 5.
        assert_eq!(match_at("rust", "fn a() {}\n", 4), Some(5));
    }

    #[test]
    fn rust_close_paren_matches_open() {
        assert_eq!(match_at("rust", "fn a() {}\n", 5), Some(4));
    }

    #[test]
    fn rust_bracket_in_char_literal_has_no_match() {
        // `fn a() { let c = '('; }`: the `(` at 18 sits inside a char literal, so
        // it is not a bracket token and must not pair with the code `)`/`}`.
        assert_eq!(match_at("rust", "fn a() { let c = '('; }\n", 18), None);
    }
}
