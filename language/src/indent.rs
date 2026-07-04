//! Tree-sitter-query-driven auto-indent, following Zed's `indents.scm` dialect.
//!
//! The `indents.scm` query marks `@indent` nodes whose interior indents, with
//! `@end` overriding where the region terminates (the closing token), `@start`
//! overriding where it begins, and `@outdent` truncating the innermost enclosing
//! region. A suggestion is a basis row plus a single-unit delta, never a
//! multi-level count. The absolute depth comes entirely from the basis row's own
//! leading whitespace, so nested indentation emerges from indenting relative to
//! an already-indented row.
//!
//! stoat indents with one tab per level, so the functions here return the
//! leading-whitespace string to append after a newline (or that a row should
//! carry), preserving a tab or space basis rather than a bare column count.

use crate::highlight::{QueryCursorHandle, RopeTextProvider};
use stoat_text::{Point, Rope};
use tree_sitter::{Node, Query, StreamingIterator};

/// One indent region resolved from the query, in byte offsets plus the rows its
/// endpoints land on.
struct IndentRange {
    start_byte: usize,
    start_row: u32,
    end_byte: usize,
}

/// Leading whitespace for a new empty line inserted at `cursor_offset`.
///
/// The new line copies the cursor row's leading whitespace, plus one tab when
/// the cursor's row opens an `@indent` region whose end lies past the cursor (so
/// the new line falls inside a freshly opened block). A query yielding no region
/// leaves it at a plain copy. This is Zed's indent-from-previous-row branch
/// specialized to an empty new line, so it needs no post-edit reparse.
pub fn newline_indent(query: &Query, root: Node<'_>, rope: &Rope, cursor_offset: usize) -> String {
    let row = rope.offset_to_point(cursor_offset).row;
    let base = line_leading_whitespace(rope, row);
    let opens = collect_indent_ranges(query, root, rope)
        .iter()
        .any(|r| r.start_row == row && r.end_byte > cursor_offset);
    if opens {
        format!("{base}\t")
    } else {
        base
    }
}

/// The leading whitespace `row` should carry, following Zed's per-row decision.
///
/// Returns `None` when the query offers no suggestion, meaning the caller keeps
/// the row's current indentation. A body row inside a block that opened on the
/// previous row indents one level. A closing-token row aligns to its opener's
/// row. Otherwise the previous row's indentation is copied.
pub fn suggested_indent(query: &Query, root: Node<'_>, rope: &Rope, row: u32) -> Option<String> {
    let ranges = collect_indent_ranges(query, root, rope);

    let prev_row = row.saturating_sub(1);
    let prev_start_byte = row_indent_end(rope, prev_row);
    let row_start_byte = row_indent_end(rope, row);

    let mut indent_from_prev = false;
    let mut outdent_to_row = u32::MAX;
    for r in &ranges {
        if r.start_row >= row {
            continue;
        }
        if r.start_row == prev_row && r.end_byte > row_start_byte {
            indent_from_prev = true;
        }
        if r.end_byte > prev_start_byte && r.end_byte <= row_start_byte {
            outdent_to_row = outdent_to_row.min(r.start_row);
        }
    }

    let (basis_row, indent) = if outdent_to_row == prev_row {
        (prev_row, false)
    } else if indent_from_prev {
        (prev_row, true)
    } else if outdent_to_row < prev_row {
        (outdent_to_row, false)
    } else if row == 0 || !is_line_blank(rope, prev_row) {
        (prev_row, false)
    } else {
        return None;
    };

    let base = line_leading_whitespace(rope, basis_row);
    Some(if indent { format!("{base}\t") } else { base })
}

/// The leading run of spaces and tabs on `row`, as a string.
pub fn line_leading_whitespace(rope: &Rope, row: u32) -> String {
    let start = rope.point_to_offset(Point::new(row, 0));
    rope.chars_at(start)
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect()
}

/// Byte offset of the first non-whitespace character on `row` (or the row's end
/// when it is blank).
fn row_indent_end(rope: &Rope, row: u32) -> usize {
    let col = line_leading_whitespace(rope, row).chars().count() as u32;
    rope.point_to_offset(Point::new(row, col))
}

/// True when `row` is empty or contains only whitespace.
fn is_line_blank(rope: &Rope, row: u32) -> bool {
    let start = rope.point_to_offset(Point::new(row, 0));
    let len = rope.line_len(row);
    rope.chars_at(start)
        .take(len as usize)
        .all(|c| c == ' ' || c == '\t')
}

fn collect_indent_ranges(query: &Query, root: Node<'_>, rope: &Rope) -> Vec<IndentRange> {
    let Some(indent_ix) = query.capture_index_for_name("indent") else {
        return Vec::new();
    };
    let start_ix = query.capture_index_for_name("start");
    let end_ix = query.capture_index_for_name("end");
    let outdent_ix = query.capture_index_for_name("outdent");

    let provider = RopeTextProvider { rope };
    let mut cursor = QueryCursorHandle::new();
    let mut matches = cursor.matches(query, root, provider);

    let mut ranges: Vec<IndentRange> = Vec::new();
    let mut outdents: Vec<usize> = Vec::new();
    while let Some(m) = matches.next() {
        let mut start: Option<usize> = None;
        let mut end: Option<usize> = None;
        for cap in m.captures {
            if cap.index == indent_ix {
                start.get_or_insert(cap.node.start_byte());
                end.get_or_insert(cap.node.end_byte());
            } else if Some(cap.index) == start_ix {
                start = Some(cap.node.end_byte());
            } else if Some(cap.index) == end_ix {
                end = Some(cap.node.start_byte());
            } else if Some(cap.index) == outdent_ix {
                outdents.push(cap.node.start_byte());
            }
        }

        let (Some(s), Some(e)) = (start, end) else {
            continue;
        };
        let start_row = rope.offset_to_point(s).row;
        if start_row == rope.offset_to_point(e).row {
            continue;
        }
        match ranges.iter_mut().find(|r| r.start_byte == s) {
            Some(existing) => existing.end_byte = existing.end_byte.max(e),
            None => ranges.push(IndentRange {
                start_byte: s,
                start_row,
                end_byte: e,
            }),
        }
    }

    outdents.sort_unstable();
    for pos in outdents {
        if let Some(r) = ranges
            .iter_mut()
            .rev()
            .find(|r| r.start_byte <= pos && pos <= r.end_byte)
        {
            r.end_byte = pos;
        }
    }
    ranges
}

#[cfg(test)]
mod tests {
    use super::{newline_indent, suggested_indent};
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

    fn newline_at(name: &str, src: &str, cursor: usize) -> String {
        let lang = lang(name);
        let tree = parse(&lang, src);
        let rope = Rope::from(src);
        newline_indent(
            lang.indent_query.as_ref().expect("indent query"),
            tree.root_node(),
            &rope,
            cursor,
        )
    }

    fn suggested(name: &str, src: &str, row: u32) -> Option<String> {
        let lang = lang(name);
        let tree = parse(&lang, src);
        let rope = Rope::from(src);
        suggested_indent(
            lang.indent_query.as_ref().expect("indent query"),
            tree.root_node(),
            &rope,
            row,
        )
    }

    #[test]
    fn rust_newline_after_open_brace_indents() {
        // Cursor at the end of `fn a() {` (offset 8, before the newline).
        assert_eq!(newline_at("rust", "fn a() {\n}\n", 8), "\t");
    }

    #[test]
    fn rust_newline_on_plain_line_copies_indent() {
        // Cursor at the end of the already-indented body line. Nothing opens.
        assert_eq!(newline_at("rust", "fn a() {\n\tlet x = 1;\n}\n", 20), "\t");
    }

    #[test]
    fn rust_body_indents_closer_outdents() {
        let src = "fn a() {\n\tx;\n}\n";
        assert_eq!(suggested("rust", src, 1).as_deref(), Some("\t"));
        assert_eq!(suggested("rust", src, 2).as_deref(), Some(""));
    }

    #[test]
    fn rust_nested_blocks_stack() {
        let src = "fn a() {\n\tif b {\n\t\tx;\n\t}\n}\n";
        assert_eq!(suggested("rust", src, 2).as_deref(), Some("\t\t"));
        assert_eq!(suggested("rust", src, 3).as_deref(), Some("\t"));
        assert_eq!(suggested("rust", src, 4).as_deref(), Some(""));
    }

    #[test]
    fn json_newline_after_open_brace_indents() {
        // Cursor after `{` at offset 1.
        assert_eq!(newline_at("json", "{\n}\n", 1), "\t");
    }
}
