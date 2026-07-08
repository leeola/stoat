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

/// Byte offset of the bracket matching the cursor at `offset`, resolved from the
/// `brackets.scm` query's `@open`/`@close` captures.
///
/// On an `@open` token, returns the paired `@close` token's start. On a
/// `@close`, returns the `@open`'s start. When `offset` sits strictly between a
/// pair's delimiters and on neither, returns the innermost enclosing pair's
/// `@close` start, so the cursor jumps out to its bracket.
///
/// Returns `None` when no captured pair covers or encloses `offset`. A bracket
/// inside a string, char, or comment literal is never captured, so it matches
/// nothing. The quote delimiters themselves are captured pairs, so a cursor
/// inside a string resolves to its closing quote.
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

    // Innermost pair whose delimiters enclose `offset`, as (open.start,
    // close.start). Kept for the from-within case, resolved only if no delimiter
    // is hit directly.
    let mut enclosing: Option<(usize, usize)> = None;

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
        if open.end <= offset && offset < close.start {
            let tighter = enclosing.is_none_or(|(best_open, _)| open.start > best_open);
            if tighter {
                enclosing = Some((open.start, close.start));
            }
        }
    }

    enclosing.map(|(_, close_start)| close_start)
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
    fn rust_char_literal_bracket_is_not_a_delimiter() {
        // `fn a() { let c = '('; }`: the `(` at 18 sits inside a char literal, so
        // it is not a captured bracket token and cannot pair with the code
        // `)`/`}`. From within the fn body it resolves to the enclosing `}` at 22
        // (offset 18 is between `{` at 7 and `}` at 22), not to a false mate.
        assert_eq!(match_at("rust", "fn a() { let c = '('; }\n", 18), Some(22));
    }

    #[test]
    fn rust_from_inside_returns_enclosing_close() {
        // `fn a() { let x = 1; }`: the `{` is at 7 and `}` at 20. A cursor at 9
        // (on `let`, no delimiter) resolves to the enclosing block's `}`.
        assert_eq!(match_at("rust", "fn a() { let x = 1; }\n", 9), Some(20));
    }

    #[test]
    fn rust_from_inside_picks_innermost_pair() {
        // `fn a() { { } }`: the inner block `{` is at 9 and `}` at 11, nested in
        // the fn body `{` at 7 / `}` at 13. A cursor at 10 picks the innermost.
        assert_eq!(match_at("rust", "fn a() { { } }\n", 10), Some(11));
    }

    #[test]
    fn rust_from_inside_string_returns_closing_quote() {
        // `fn a() { let s = "hi"; }`: the opening `"` is at 17 and closing at 20.
        // A cursor at 18 (on `h`, inside the string) resolves to the close quote,
        // since the quote delimiters are a captured pair.
        assert_eq!(
            match_at("rust", "fn a() { let s = \"hi\"; }\n", 18),
            Some(20)
        );
    }
}
