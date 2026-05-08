//! Helpers for textobject queries.
//!
//! `select_textobject_around` / `select_textobject_inner` need to find
//! the smallest tree-sitter capture (under a given name like
//! `function.around`) that contains the cursor. This module wraps the
//! query-cursor + rope-text-provider plumbing into a single function
//! so handlers in the `stoat` crate do not have to construct a
//! `QueryCursor` and `TextProvider` themselves.
//!
//! Pure tree-sitter logic only -- paragraph (line-based) textobjects
//! are handled in the `stoat` crate alongside the action handler.

use crate::highlight::{QueryCursorHandle, RopeTextProvider};
use std::ops::Range;
use stoat_text::Rope;
use tree_sitter::{Node, Query, StreamingIterator};

/// Smallest byte range under `capture_name` in `query` that contains
/// `cursor`. Returns `None` if `capture_name` is unknown to the query
/// or no matching capture brackets `cursor`.
///
/// Helix's textobject selection picks the innermost match by capture
/// length; this function follows the same rule. `rope` is needed for
/// query predicates (`#eq?`, `#match?`) that read node text.
pub fn find_smallest_capture_at(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    capture_name: &str,
    cursor: usize,
) -> Option<Range<usize>> {
    let cap_idx = query.capture_index_for_name(capture_name)?;
    let provider = RopeTextProvider { rope };
    let mut cursor_h = QueryCursorHandle::new();
    let mut matches = cursor_h.matches(query, root, provider);
    let mut best: Option<Range<usize>> = None;
    while let Some(m) = matches.next() {
        let mut union: Option<Range<usize>> = None;
        for cap in m.captures {
            if cap.index != cap_idx {
                continue;
            }
            let r = cap.node.byte_range();
            union = Some(match union {
                None => r,
                Some(u) => u.start.min(r.start)..u.end.max(r.end),
            });
        }
        let Some(u) = union else { continue };
        if !(u.start <= cursor && cursor < u.end) {
            continue;
        }
        let len = u.end - u.start;
        match &best {
            Some(b) if (b.end - b.start) <= len => {},
            _ => best = Some(u),
        }
    }
    best
}
