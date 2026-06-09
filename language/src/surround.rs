//! Geometry for the surround text object: map a delimiter char to its
//! canonical pair, and find the pair enclosing a cursor over a
//! [`Rope`], skipping delimiters inside string/comment syntax when a
//! parse [`Tree`] is available.

use crate::{bracket, Tree};
use stoat_text::Rope;

/// Canonical open/close pair for a surround character. Bracket
/// characters map to their pair; any other character (quotes, custom
/// delimiters) pairs with itself.
pub fn surround_pair_for(ch: char) -> (char, char) {
    match ch {
        '(' | ')' => ('(', ')'),
        '[' | ']' => ('[', ']'),
        '{' | '}' => ('{', '}'),
        '<' | '>' => ('<', '>'),
        other => (other, other),
    }
}

/// Byte offsets of the open and close delimiters of the `open`/`close`
/// pair enclosing `cursor`. Symmetric pairs (quotes) are matched by
/// scanning outward, with a tree-aware disambiguation when the cursor
/// sits on a quote; asymmetric pairs (brackets) walk left for the open
/// and right for the close, tracking nesting depth. Delimiters inside
/// string/comment nodes are skipped when `tree` is provided. Returns
/// `None` when no enclosing pair exists.
pub fn find_surround_pair(
    rope: &Rope,
    cursor: usize,
    open: char,
    close: char,
    tree: Option<&Tree>,
) -> Option<(usize, usize)> {
    if open == close {
        if rope.chars_at(cursor).next() == Some(open) {
            if let Some(pair) = enclosing_string_pair(rope, tree, cursor, open) {
                return Some(pair);
            }
            return None;
        }
        let open_pos = walk_left_for_symmetric(rope, cursor, open, tree)?;
        let close_pos = walk_right_for_symmetric(rope, cursor, open, tree)?;
        Some((open_pos, close_pos))
    } else {
        let open_pos = walk_left_for_open(rope, cursor, open, close, tree)?;
        let close_pos = walk_right_for_close(rope, cursor, open, close, tree)?;
        Some((open_pos, close_pos))
    }
}

fn in_skip_zone(tree: Option<&Tree>, offset: usize) -> bool {
    match tree {
        Some(t) => bracket::is_in_string_or_comment(t, offset),
        None => false,
    }
}

/// Walk the tree-sitter ancestor chain at `offset` looking for the
/// deepest node whose `kind()` mentions `"string"`. Returns the
/// node's byte range (half-open: `start..end_byte`). Used to
/// disambiguate cursor-on-quote surround lookups; the calling site
/// translates `range.end - 1` into the closing quote's byte offset.
fn find_enclosing_string_node(tree: &Tree, offset: usize) -> Option<std::ops::Range<usize>> {
    let mut node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    loop {
        if node.kind().contains("string") {
            return Some(node.byte_range());
        }
        match node.parent() {
            Some(p) => node = p,
            None => return None,
        }
    }
}

/// Translate `find_enclosing_string_node` into a surround pair when
/// the located string node opens with `open`. Returns `None` when
/// the buffer has no tree, no string ancestor exists at the cursor,
/// or the located node does not start with `open` (e.g. a rust raw
/// string `r"..."` whose first byte is `r`).
fn enclosing_string_pair(
    rope: &Rope,
    tree: Option<&Tree>,
    cursor: usize,
    open: char,
) -> Option<(usize, usize)> {
    let tree = tree?;
    let range = find_enclosing_string_node(tree, cursor)?;
    if range.start >= range.end {
        return None;
    }
    if rope.chars_at(range.start).next() != Some(open) {
        return None;
    }
    Some((range.start, range.end - open.len_utf8()))
}

fn walk_right_for_close(
    rope: &Rope,
    cursor: usize,
    open: char,
    close: char,
    tree: Option<&Tree>,
) -> Option<usize> {
    let mut chars = rope.chars_at(cursor);
    let mut pos = cursor;
    let first = chars.next()?;
    if first == close && !in_skip_zone(tree, pos) {
        return Some(pos);
    }
    pos += first.len_utf8();
    let mut step_over: usize = 0;
    for c in chars {
        let skip = in_skip_zone(tree, pos);
        if !skip {
            if c == open {
                step_over += 1;
            } else if c == close {
                if step_over == 0 {
                    return Some(pos);
                }
                step_over -= 1;
            }
        }
        pos += c.len_utf8();
    }
    None
}

fn walk_left_for_open(
    rope: &Rope,
    cursor: usize,
    open: char,
    close: char,
    tree: Option<&Tree>,
) -> Option<usize> {
    if rope.chars_at(cursor).next() == Some(open) && !in_skip_zone(tree, cursor) {
        return Some(cursor);
    }
    let mut pos = cursor;
    let mut step_over: usize = 0;
    for c in rope.reversed_chars_at(cursor) {
        pos = pos.checked_sub(c.len_utf8())?;
        let skip = in_skip_zone(tree, pos);
        if !skip {
            if c == close {
                step_over += 1;
            } else if c == open {
                if step_over == 0 {
                    return Some(pos);
                }
                step_over -= 1;
            }
        }
    }
    None
}

fn walk_right_for_symmetric(
    rope: &Rope,
    cursor: usize,
    ch: char,
    tree: Option<&Tree>,
) -> Option<usize> {
    let mut pos = cursor;
    for c in rope.chars_at(cursor) {
        if c == ch && !in_skip_zone(tree, pos) {
            return Some(pos);
        }
        pos += c.len_utf8();
    }
    None
}

fn walk_left_for_symmetric(
    rope: &Rope,
    cursor: usize,
    ch: char,
    tree: Option<&Tree>,
) -> Option<usize> {
    let mut pos = cursor;
    for c in rope.reversed_chars_at(cursor) {
        pos = pos.checked_sub(c.len_utf8())?;
        if c == ch && !in_skip_zone(tree, pos) {
            return Some(pos);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rope(s: &str) -> Rope {
        let mut r = Rope::new();
        r.push(s);
        r
    }

    #[test]
    fn pair_for_brackets() {
        assert_eq!(surround_pair_for('('), ('(', ')'));
        assert_eq!(surround_pair_for(')'), ('(', ')'));
        assert_eq!(surround_pair_for('['), ('[', ']'));
        assert_eq!(surround_pair_for(']'), ('[', ']'));
        assert_eq!(surround_pair_for('{'), ('{', '}'));
        assert_eq!(surround_pair_for('}'), ('{', '}'));
        assert_eq!(surround_pair_for('<'), ('<', '>'));
        assert_eq!(surround_pair_for('>'), ('<', '>'));
    }

    #[test]
    fn pair_for_quotes_doubles_char() {
        assert_eq!(surround_pair_for('"'), ('"', '"'));
        assert_eq!(surround_pair_for('\''), ('\'', '\''));
        assert_eq!(surround_pair_for('`'), ('`', '`'));
    }

    #[test]
    fn pair_for_arbitrary_char_doubles() {
        assert_eq!(surround_pair_for('*'), ('*', '*'));
        assert_eq!(surround_pair_for('|'), ('|', '|'));
    }

    #[test]
    fn find_pair_paren_cursor_inside() {
        let r = rope("(abc)");
        assert_eq!(find_surround_pair(&r, 2, '(', ')', None), Some((0, 4)));
    }

    #[test]
    fn find_pair_paren_cursor_on_open() {
        let r = rope("(abc)");
        assert_eq!(find_surround_pair(&r, 0, '(', ')', None), Some((0, 4)));
    }

    #[test]
    fn find_pair_paren_cursor_on_close() {
        let r = rope("(abc)");
        assert_eq!(find_surround_pair(&r, 4, '(', ')', None), Some((0, 4)));
    }

    #[test]
    fn find_pair_paren_no_match_returns_none() {
        let r = rope("abc");
        assert_eq!(find_surround_pair(&r, 1, '(', ')', None), None);
    }

    #[test]
    fn find_pair_nested_paren_finds_innermost() {
        let r = rope("((abc))");
        assert_eq!(find_surround_pair(&r, 3, '(', ')', None), Some((1, 5)));
    }

    #[test]
    fn find_pair_unbalanced_paren_returns_none() {
        let r = rope("(abc");
        assert_eq!(find_surround_pair(&r, 1, '(', ')', None), None);
    }

    #[test]
    fn find_pair_quote_cursor_inside() {
        let r = rope("\"abc\"");
        assert_eq!(find_surround_pair(&r, 2, '"', '"', None), Some((0, 4)));
    }

    #[test]
    fn find_pair_quote_cursor_on_quote_is_ambiguous() {
        let r = rope("\"abc\"");
        assert_eq!(find_surround_pair(&r, 0, '"', '"', None), None);
        assert_eq!(find_surround_pair(&r, 4, '"', '"', None), None);
    }

    #[test]
    fn find_pair_quote_no_match_returns_none() {
        let r = rope("abc");
        assert_eq!(find_surround_pair(&r, 1, '"', '"', None), None);
    }
}
