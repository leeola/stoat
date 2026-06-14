//! Tree-aware bracket matching over a [`Rope`].
//!
//! Finds the bracket matching the one under the cursor, skipping
//! brackets that sit inside string or comment syntax nodes when a
//! parse [`Tree`] is available.

use crate::Tree;
use stoat_text::Rope;

/// Find the matching bracket for the character at `head_offset` in
/// `rope`. Returns the byte offset of the matched bracket, or `None`
/// when the char at `head_offset` is not a bracket, the cursor sits
/// inside a string/comment node (when `tree` is provided), or no
/// matching bracket exists in the requested direction. Bracket
/// characters inside string/comment nodes are skipped during the scan
/// when `tree` is provided.
pub fn match_bracket_target(rope: &Rope, head_offset: usize, tree: Option<&Tree>) -> Option<usize> {
    let ch = rope.chars_at(head_offset).next()?;
    let (open, close, forward) = bracket_pair(ch)?;
    if let Some(t) = tree
        && is_in_string_or_comment(t, head_offset)
    {
        return None;
    }
    scan_bracket_match(rope, head_offset, ch, open, close, forward, tree)
}

/// Map a bracket character to its `(open, close, search-forward)`
/// triple, or `None` when `ch` is not a bracket.
pub fn bracket_pair(ch: char) -> Option<(char, char, bool)> {
    match ch {
        '(' => Some(('(', ')', true)),
        ')' => Some(('(', ')', false)),
        '[' => Some(('[', ']', true)),
        ']' => Some(('[', ']', false)),
        '{' => Some(('{', '}', true)),
        '}' => Some(('{', '}', false)),
        _ => None,
    }
}

/// True when `offset` falls inside a string or comment node of `tree`.
pub fn is_in_string_or_comment(tree: &Tree, offset: usize) -> bool {
    let Some(mut node) = tree.root_node().descendant_for_byte_range(offset, offset) else {
        return false;
    };
    loop {
        let kind = node.kind();
        if kind.contains("string") || kind.contains("comment") {
            return true;
        }
        match node.parent() {
            Some(p) => node = p,
            None => return false,
        }
    }
}

/// Scan from `start` for the bracket matching `start_ch`, tracking
/// nesting depth and skipping brackets inside string/comment nodes
/// when `tree` is provided.
pub fn scan_bracket_match(
    rope: &Rope,
    start: usize,
    start_ch: char,
    open: char,
    close: char,
    forward: bool,
    tree: Option<&Tree>,
) -> Option<usize> {
    let mut depth: u32 = 1;
    let in_skip_zone = |offset: usize| match tree {
        Some(t) => is_in_string_or_comment(t, offset),
        None => false,
    };
    if forward {
        let mut cur = start + start_ch.len_utf8();
        for c in rope.chars_at(cur) {
            if (c == open || c == close) && !in_skip_zone(cur) {
                if c == open {
                    depth += 1;
                } else {
                    depth -= 1;
                    if depth == 0 {
                        return Some(cur);
                    }
                }
            }
            cur += c.len_utf8();
        }
        None
    } else {
        let mut cur = start;
        for c in rope.reversed_chars_at(start) {
            cur -= c.len_utf8();
            if (c == open || c == close) && !in_skip_zone(cur) {
                if c == close {
                    depth += 1;
                } else {
                    depth -= 1;
                    if depth == 0 {
                        return Some(cur);
                    }
                }
            }
        }
        None
    }
}
