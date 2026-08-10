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

/// Sorted, deduplicated start byte offsets of every match's
/// `capture_name` union range, over the matches within `bytes`.
///
/// Used by goto-next/prev navigation (`] f` / `[ f` / `] t` / `[ t`)
/// to land on the keyword that opens each function or class. A caller
/// seeking one direction passes only the bytes that direction can
/// answer from. The range bounds which matches are visited, and each
/// still reports its own extents. Returns an empty vector when
/// `capture_name` is unknown to `query` or no match yields a
/// capture under that name.
pub fn collect_capture_starts(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    capture_name: &str,
    bytes: Range<usize>,
) -> Vec<usize> {
    let mut out = Vec::new();
    let Some(cap_idx) = query.capture_index_for_name(capture_name) else {
        return out;
    };
    let provider = RopeTextProvider { rope };
    let mut cursor_h = QueryCursorHandle::new();
    cursor_h.set_byte_range(bytes);
    let mut matches = cursor_h.matches(query, root, provider);
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
        if let Some(u) = union {
            out.push(u.start);
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Smallest byte range under `capture_name` in `query` that contains
/// `cursor`. Returns `None` if `capture_name` is unknown to the query
/// or no matching capture brackets `cursor`.
///
/// Ties break toward the innermost match by capture length, which is
/// what a textobject selection wants. `rope` is needed for query
/// predicates (`#eq?`, `#match?`) that read node text.
pub fn find_smallest_capture_at(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    capture_name: &str,
    cursor: usize,
) -> Option<Range<usize>> {
    find_smallest_capture_scanning(query, root, rope, capture_name, cursor, true)
}

/// [`find_smallest_capture_at`], with the query restriction made optional so a
/// test can compare the restricted answer against the whole-file one.
fn find_smallest_capture_scanning(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    capture_name: &str,
    cursor: usize,
    restrict: bool,
) -> Option<Range<usize>> {
    let cap_idx = query.capture_index_for_name(capture_name)?;
    let provider = RopeTextProvider { rope };
    let mut cursor_h = QueryCursorHandle::new();
    if restrict {
        // Only a union bracketing the cursor can win, and every capture in a
        // match descends from the node the pattern matched, so that node covers
        // the union and the cursor with it. Matches elsewhere cannot answer and
        // need not be visited, even where the union spans the cursor while no
        // single capture node does.
        cursor_h.set_byte_range(cursor..cursor + 1);
    }
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

#[cfg(test)]
mod tests {
    use super::{collect_capture_starts, find_smallest_capture_scanning};
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

    /// Functions inside impl blocks and beside them, so a cursor in any one of
    /// them has matches above it, below it, and enclosing it. The restriction
    /// has to keep the last kind while skipping the others.
    fn nested_source() -> String {
        let mut src = String::from("struct A;\n\n");
        for block in 0..4 {
            src.push_str(&format!("impl A{block} {{\n"));
            for f in 0..4 {
                src.push_str(&format!(
                    "    fn m{block}_{f}(&self) -> u32 {{\n        let x = {f};\n        x + 1\n    }}\n"
                ));
            }
            src.push_str("}\n\n");
            src.push_str(&format!("fn free{block}() -> u32 {{\n    {block}\n}}\n\n"));
        }
        src
    }

    /// Both entry points now ask the query about a slice rather than the file,
    /// which is only sound if every match that can answer still turns up.
    #[test]
    fn restricting_the_query_finds_what_scanning_the_file_found() {
        let lang = lang("rust");
        let src = nested_source();
        let tree = parse(&lang, &src);
        let rope = Rope::from(src.as_str());
        let query = lang.textobjects_query().expect("textobjects query");
        let root = tree.root_node();
        let whole = 0..src.len();

        let mut answered = 0;
        let mut pruned = 0;
        for cursor in 0..src.len() {
            if !src.is_char_boundary(cursor) {
                continue;
            }

            for name in ["function.around", "function.inside", "class.around"] {
                let restricted =
                    find_smallest_capture_scanning(query, root, &rope, name, cursor, true);
                let whole_file =
                    find_smallest_capture_scanning(query, root, &rope, name, cursor, false);
                assert_eq!(restricted, whole_file, "{name} at offset {cursor}");
                answered += usize::from(whole_file.is_some());
            }

            // What the caller keeps out of each direction's window, against what
            // it kept when the starts came from the whole file.
            let full = collect_capture_starts(query, root, &rope, "function.around", whole.clone());
            let forward =
                collect_capture_starts(query, root, &rope, "function.around", cursor..src.len());
            let backward =
                collect_capture_starts(query, root, &rope, "function.around", 0..cursor + 1);
            assert_eq!(
                forward.iter().copied().find(|&s| s > cursor),
                full.iter().copied().find(|&s| s > cursor),
                "next function from offset {cursor}"
            );
            assert_eq!(
                backward.iter().copied().rev().find(|&s| s < cursor),
                full.iter().copied().rev().find(|&s| s < cursor),
                "prev function from offset {cursor}"
            );
            pruned += usize::from(forward.len() < full.len() || backward.len() < full.len());
        }

        assert!(
            answered > 100,
            "the fixture has to put the cursor inside plenty of captures, or the \
             comparison above was None against None: {answered}"
        );
        assert!(
            pruned > 100,
            "and the windows have to actually drop matches, or they were the whole \
             file and nothing was restricted: {pruned}"
        );
    }
}
