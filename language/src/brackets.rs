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
    matching_bracket_scanning(query, root, rope, offset, true)
}

/// [`matching_bracket`], with the query restriction made optional so a test can
/// compare the restricted answer against the whole-file one.
fn matching_bracket_scanning(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    offset: usize,
    restrict: bool,
) -> Option<usize> {
    let open_ix = query.capture_index_for_name("open")?;
    let close_ix = query.capture_index_for_name("close")?;

    let provider = RopeTextProvider { rope };
    let mut cursor = QueryCursorHandle::new();
    if restrict {
        // Every match that can answer contains the cursor. A direct hit has it
        // inside a delimiter and the enclosing case has it between them, so
        // matches elsewhere in the file cannot win and need not be visited.
        cursor.set_byte_range(offset..offset + 1);
    }
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

/// Delimiter pairs the query-free walk recognizes.
///
/// Wider than the plaintext set by the three quotes and `|`, since a tree names
/// which side of a symmetric delimiter is which where a character scan sees only
/// the character.
/// Rust writes closure parameters as `|a, b|`, whose node holds both bars as its
/// first and last children, so the walk tells them from a bitwise or.
const TREE_PAIRS: [(char, char); 13] = [
    ('(', ')'),
    ('{', '}'),
    ('[', ']'),
    ('<', '>'),
    ('\u{2018}', '\u{2019}'),
    ('\u{201c}', '\u{201d}'),
    ('\u{ab}', '\u{bb}'),
    ('\u{300c}', '\u{300d}'),
    ('\u{ff08}', '\u{ff09}'),
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
    ('|', '|'),
];

/// Sibling nodes either sibling walk visits before giving up.
///
/// A node's sibling list runs as long as its parent's children, which for a
/// source file's top level is every item in it. Walking that whole list to
/// conclude a press matched nothing is the cost this bounds.
const SIBLING_LIMIT: usize = 16;

/// Byte offset of the bracket matching the cursor at `offset`, read from the
/// syntax tree without a `brackets.scm` query.
///
/// Backs the languages that ship a grammar but no query, which is most of them.
/// The tree names the construct the cursor is in, so this matches from inside
/// one and not only from a delimiter.
///
/// Returns `None` when no construct around the cursor is delimited by a pair,
/// which leaves a caller free to fall back to a character scan.
///
/// Only a single-byte node counts as a delimiter. A multi-byte one occupies a
/// node whose range is longer, and reading its first byte as the whole
/// character is what that restriction avoids.
///
/// See also:
/// - [`matching_bracket`] for the query path, which a language shipping a `brackets.scm` takes
///   instead.
pub fn matching_bracket_from_tree(root: Node<'_>, rope: &Rope, offset: usize) -> Option<usize> {
    let mut node = root.descendant_for_byte_range(offset, offset)?;

    loop {
        if let Some(found) = pair_within(&node, rope, offset) {
            return Some(found);
        }
        if let Some(found) = pair_across_siblings(&node, rope) {
            return Some(found);
        }
        if let Some(found) = enclosing_close_ahead(&node, rope) {
            return Some(found);
        }
        node = node.parent()?;
    }
}

/// The delimiter answering a cursor inside a node whose first and last children
/// are a pair.
///
/// The cursor sitting on the closing one answers with the opening one, and
/// anywhere else inside answers with the closing one, so a repeat leaves the
/// construct rather than bouncing between its ends.
fn pair_within(node: &Node<'_>, rope: &Rope, offset: usize) -> Option<usize> {
    if !node.is_named() || node.child_count() < 2 {
        return None;
    }
    let (open_at, open) = single_char(&node.child(0)?, rope)?;
    let (close_at, close) = single_char(&node.child((node.child_count() - 1) as u32)?, rope)?;
    if !TREE_PAIRS.contains(&(open, close)) || offset < open_at || offset > close_at {
        return None;
    }
    Some(if close_at == offset {
        open_at
    } else {
        close_at
    })
}

/// The partner of a node that is itself a delimiter, found among its siblings.
fn pair_across_siblings(node: &Node<'_>, rope: &Rope) -> Option<usize> {
    let (_, ch) = single_char(node, rope)?;

    if let Some(&(open, _)) = TREE_PAIRS.iter().find(|&&(_, close)| close == ch)
        && let Some(found) = walk_siblings(node.prev_sibling(), rope, ch, open, false)
    {
        return Some(found);
    }
    if let Some(&(_, close)) = TREE_PAIRS.iter().find(|&&(open, _)| open == ch)
        && let Some(found) = walk_siblings(node.next_sibling(), rope, ch, close, true)
    {
        return Some(found);
    }
    None
}

/// The closing delimiter of a construct the node sits inside, found by looking
/// ahead for one whose own opening delimiter lies behind it.
///
/// This is what answers a cursor between two delimiters that are siblings of it
/// rather than children of a node enclosing it.
fn enclosing_close_ahead(node: &Node<'_>, rope: &Rope) -> Option<usize> {
    let mut sibling = node.next_sibling();
    for _ in 0..SIBLING_LIMIT {
        let current = sibling?;
        if let Some((_, ch)) = single_char(&current, rope)
            && let Some(&(open, _)) = TREE_PAIRS.iter().find(|&&(_, close)| close == ch)
            && walk_siblings(current.prev_sibling(), rope, ch, open, false).is_some()
        {
            return Some(current.start_byte());
        }
        sibling = current.next_sibling();
    }
    None
}

/// Offset of the first `wanted` delimiter among the siblings from `start`, with
/// nested `nested` delimiters consumed on the way.
fn walk_siblings(
    start: Option<Node<'_>>,
    rope: &Rope,
    nested: char,
    wanted: char,
    forward: bool,
) -> Option<usize> {
    let mut depth = 0usize;
    let mut node = start;
    for _ in 0..SIBLING_LIMIT {
        let current = node?;
        if let Some((at, ch)) = single_char(&current, rope) {
            if ch == wanted {
                if depth == 0 {
                    return Some(at);
                }
                depth -= 1;
            } else if ch == nested {
                depth += 1;
            }
        }
        node = if forward {
            current.next_sibling()
        } else {
            current.prev_sibling()
        };
    }
    None
}

/// The character a node holds, when it holds exactly one byte of it.
fn single_char(node: &Node<'_>, rope: &Rope) -> Option<(usize, char)> {
    let range = node.byte_range();
    if range.len() != 1 {
        return None;
    }
    rope.chars_at(range.start)
        .next()
        .map(|ch| (range.start, ch))
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
            lang.bracket_query().expect("bracket query"),
            tree.root_node(),
            &rope,
            offset,
        )
    }

    /// Nested and sibling pairs of several kinds, deep enough that a cursor in
    /// the middle has many pairs enclosing it and many more nowhere near it.
    fn nested_source() -> String {
        let mut src = String::from("fn main() {\n");
        for depth in 0..12 {
            let pad = "    ".repeat(depth + 1);
            src.push_str(&format!("{pad}if a[{depth}] == (b + {depth}) {{\n"));
        }
        src.push_str(&"    ".repeat(13));
        src.push_str("let s = \"text (paren) here\";\n");
        for depth in (0..12).rev() {
            src.push_str(&"    ".repeat(depth + 1));
            src.push_str("}\n");
        }
        src.push_str("}\n");
        src
    }

    #[test]
    fn restricting_the_query_finds_what_scanning_the_file_found() {
        let lang = lang("rust");
        let src = nested_source();
        let tree = parse(&lang, &src);
        let rope = Rope::from(src.as_str());
        let query = lang.bracket_query().expect("bracket query");

        let mut answered = 0;
        for offset in 0..src.len() {
            if !src.is_char_boundary(offset) {
                continue;
            }
            let restricted =
                super::matching_bracket_scanning(query, tree.root_node(), &rope, offset, true);
            let whole_file =
                super::matching_bracket_scanning(query, tree.root_node(), &rope, offset, false);

            assert_eq!(
                restricted,
                whole_file,
                "offset {offset} in {:?}",
                &src[offset.saturating_sub(20)..(offset + 20).min(src.len())]
            );
            // The restriction could plausibly break where the cursor sits
            // between a pair's delimiters, touching neither, so that only the
            // pattern as a whole covers it. Counted so this cannot go vacuous.
            let on_delimiter = src[offset..].starts_with(['(', ')', '[', ']', '{', '}', '"']);
            answered += usize::from(whole_file.is_some() && !on_delimiter);
        }

        assert!(
            answered > 100,
            "the fixture has to put the cursor inside plenty of pairs rather than on them, \
             or nothing here tested the case that could break: {answered}"
        );
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
