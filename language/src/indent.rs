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
/// the cursor's row opens an `@indent` region the cursor sits inside (so the new
/// line falls inside a freshly opened block). A query yielding no region leaves
/// it at a plain copy. This is Zed's indent-from-previous-row branch specialized
/// to an empty new line, so it needs no post-edit reparse.
///
/// A region opening later on the row does not count. Its delimiter goes down
/// with the new line rather than staying above it, so the line it lands on is
/// still outside the region and belongs at the enclosing level.
pub fn newline_indent(query: &Query, root: Node<'_>, rope: &Rope, cursor_offset: usize) -> String {
    let ranges = collect_indent_ranges(query, root, rope, newline_window(rope, cursor_offset));
    newline_indent_from(&ranges, rope, cursor_offset)
}

/// Bytes the query has to visit for [`newline_indent`] to answer.
///
/// The regions it reads open on the cursor's own row, before the cursor, so
/// their starts all sit here. Their ends can be anywhere below, which costs
/// nothing, since a match is returned whole once any of it falls in the window.
///
/// The outdents that can change one of those regions land here too. Truncating a
/// region to at or before the cursor is what changes the answer, and the
/// truncation puts the region's new end at the outdent's own position, which is
/// then between the region's start and the cursor.
fn newline_window(rope: &Rope, cursor_offset: usize) -> std::ops::Range<usize> {
    let row = rope.offset_to_point(cursor_offset).row;
    rope.point_to_offset(Point::new(row, 0))..cursor_offset + 1
}

/// Whether a region the cursor sits inside opened on its row, and the
/// whitespace that follows from it.
fn newline_indent_from(ranges: &[IndentRange], rope: &Rope, cursor_offset: usize) -> String {
    let row = rope.offset_to_point(cursor_offset).row;
    let base = line_leading_whitespace(rope, row);
    let opens = ranges
        .iter()
        .any(|r| r.start_row == row && r.start_byte < cursor_offset && r.end_byte > cursor_offset);
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
    suggested_indent_scanning(query, root, rope, row, true)
}

/// [`suggested_indent`], with the query restriction made optional so a test can
/// compare the restricted answer against the whole-prefix one.
fn suggested_indent_scanning(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    row: u32,
    restrict: bool,
) -> Option<String> {
    let window = if restrict {
        suggested_window(query, rope, row)
    } else {
        0..row_indent_end(rope, row) + 1
    };
    let ranges = collect_indent_ranges(query, root, rope, window);
    suggested_indent_from(&ranges, rope, row)
}

/// Bytes the query has to visit for [`suggested_indent`] to answer.
///
/// Both comparisons the decision makes put a byte of their region between the
/// previous row's start and just past this row's indentation. The indent test
/// wants a region starting on the previous row, and the outdent test wants an
/// end between the two rows' indentation ends. A query cursor returns any match
/// meeting its range whole, so a region reaching in from far above still arrives
/// carrying the start row the decision reads off it.
///
/// An `@outdent` capture is what that argument does not survive, so a query
/// holding one gets the whole prefix instead. Truncation pulls a region's end
/// back to an outdent's own position, and an outdent above this band is one the
/// narrow window never sees, leaving an end long enough to answer a test the
/// whole-prefix scan would have shortened it out of.
///
/// No `indents.scm` in the tree captures one, so the narrow window is what every
/// current language runs at. The wide branch is there so adding a query that
/// does captures it stays correct rather than silently mis-indenting.
fn suggested_window(query: &Query, rope: &Rope, row: u32) -> std::ops::Range<usize> {
    let end = row_indent_end(rope, row) + 1;
    if query.capture_index_for_name("outdent").is_some() {
        return 0..end;
    }
    let prev_row = row.saturating_sub(1);
    rope.point_to_offset(Point::new(prev_row, 0))..end
}

/// The whitespace `row` should carry, given the regions around it.
fn suggested_indent_from(ranges: &[IndentRange], rope: &Rope, row: u32) -> Option<String> {
    let prev_row = row.saturating_sub(1);
    let prev_start_byte = row_indent_end(rope, prev_row);
    let row_start_byte = row_indent_end(rope, row);

    let mut indent_from_prev = false;
    let mut outdent_to_row = u32::MAX;
    for r in ranges {
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

/// Every multi-row indent region the query finds within `bytes`, with the
/// outdents inside `bytes` applied.
///
/// The range bounds the *matches visited*, not what they report. A match whose
/// nodes intersect it comes back with its full extents, so a region reaching far
/// below the window still carries its real end. Each caller passes the bytes its
/// own decision can read, argued at the window it builds.
fn collect_indent_ranges(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    bytes: std::ops::Range<usize>,
) -> Vec<IndentRange> {
    let Some(indent_ix) = query.capture_index_for_name("indent") else {
        return Vec::new();
    };
    let start_ix = query.capture_index_for_name("start");
    let end_ix = query.capture_index_for_name("end");
    let outdent_ix = query.capture_index_for_name("outdent");

    let provider = RopeTextProvider { rope };
    let mut cursor = QueryCursorHandle::new();
    cursor.set_byte_range(bytes);
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
    use super::{
        collect_indent_ranges, newline_indent, newline_indent_from, newline_window,
        suggested_indent, suggested_indent_from, suggested_indent_scanning, suggested_window,
    };
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
    fn rust_newline_before_the_open_brace_does_not_indent() {
        // The new line carries `fn a() {` down, and that opener has not opened
        // anything above the line it lands on.
        assert_eq!(newline_at("rust", "fn a() {\n}\n", 0), "");
        // Between the parens, still ahead of the brace.
        assert_eq!(newline_at("rust", "fn a() {\n}\n", 5), "");
        // Directly before it, where the brace goes down with the new line.
        assert_eq!(newline_at("rust", "fn a() {\n}\n", 7), "");
    }

    #[test]
    fn json_newline_after_open_brace_indents() {
        // Cursor after `{` at offset 1.
        assert_eq!(newline_at("json", "{\n}\n", 1), "\t");
    }

    /// A window that is too wide costs time and answers correctly, so only
    /// comparing it against the whole file can say it is not too narrow.
    ///
    /// What this catches is a window that misses the decision point, which is
    /// the mistake available to a reader shortening one. It cannot speak to how
    /// much further each window reaches. The regions a decision reads all span
    /// the byte it asks about, so they come back however tight the window is,
    /// and the one thing that would not is an `@outdent`, which no
    /// `indents.scm` in the tree captures. The reach is what keeps the answer
    /// right if one ever does.
    #[test]
    fn every_window_answers_what_the_whole_file_answers() {
        let src = "fn a() {\n\tif b {\n\t\twhile c {\n\t\t\tx;\n\t\t}\n\t\tif d { y; }\n\t\tz;\n\t}\n\tw;\n}\n";
        let lang = lang("rust");
        let tree = parse(&lang, src);
        let rope = Rope::from(src);
        let query = lang.indent_query.as_ref().expect("indent query");
        let root = tree.root_node();
        let whole = 0..src.len();

        for offset in 0..=src.len() {
            let windowed = collect_indent_ranges(query, root, &rope, newline_window(&rope, offset));
            let full = collect_indent_ranges(query, root, &rope, whole.clone());
            assert_eq!(
                newline_indent_from(&windowed, &rope, offset),
                newline_indent_from(&full, &rope, offset),
                "newline_indent disagrees at offset {offset}"
            );
        }

        for row in 0..=rope.max_point().row {
            let windowed =
                collect_indent_ranges(query, root, &rope, suggested_window(query, &rope, row));
            let full = collect_indent_ranges(query, root, &rope, whole.clone());
            assert_eq!(
                suggested_indent_from(&windowed, &rope, row),
                suggested_indent_from(&full, &rope, row),
                "suggested_indent disagrees at row {row}"
            );
        }
    }

    /// The narrowed window answers what the whole prefix answers, across every
    /// shape the other tests here rely on, and through the entry point rather
    /// than the range collection alone.
    ///
    /// Like the comparison above, this cannot pin where the window's start
    /// belongs. Under the captures these queries use, every region a decision
    /// reads runs from its opener down to at least the row being asked about, so
    /// it meets the range and comes back whole however tight the window is.
    /// Starting at the previous row is a margin, not a measured boundary, and
    /// moving the start to the row itself passes this too.
    ///
    /// What the start does carry is the `@outdent` branch in
    /// [`suggested_window`], where an outdent above the window is one the narrow
    /// scan never sees. No query here captures one, so that branch is what these
    /// fixtures cannot reach rather than something they confirm.
    #[test]
    fn the_narrow_window_answers_what_the_whole_prefix_answers() {
        let fixtures: [(&str, &str); 6] = [
            ("rust", "fn a() {\n\tx;\n}\n"),
            ("rust", "fn a() {\n\tif b {\n\t\tx;\n\t}\n}\n"),
            (
                "rust",
                "fn a() {\n\tif b {\n\t\twhile c {\n\t\t\tx;\n\t\t}\n\t}\n}\n",
            ),
            // Several levels closing on consecutive rows, so a row's decision
            // reads a region that opened well above it.
            (
                "rust",
                "fn a() {\n\tif b {\n\t\tif c {\n\t\t\tif d {\n\t\t\t\tx;\n\t\t\t}\n\t\t}\n\t}\n\ty;\n}\n",
            ),
            ("json", "{\n\t\"a\": {\n\t\t\"b\": 1\n\t}\n}\n"),
            ("json", "{\n}\n"),
        ];

        for (name, src) in fixtures {
            let lang = lang(name);
            let tree = parse(&lang, src);
            let rope = Rope::from(src);
            let query = lang.indent_query.as_ref().expect("indent query");
            let root = tree.root_node();

            for row in 0..=rope.max_point().row {
                assert_eq!(
                    suggested_indent_scanning(query, root, &rope, row, true),
                    suggested_indent_scanning(query, root, &rope, row, false),
                    "{name} row {row} of {src:?}",
                );
            }
        }
    }
}
