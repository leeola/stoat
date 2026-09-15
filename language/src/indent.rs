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
//! The functions here return the leading-whitespace string to append after a
//! newline (or that a row should carry), preserving a tab or space basis rather
//! than a bare column count. One level of it is whatever `indent_unit` the
//! caller passes, since which unit a buffer is written in is the caller's to
//! know and not this crate's.

use crate::highlight::{QueryCursorHandle, RopeTextProvider};
use stoat_text::{Point, Rope};
use tree_sitter::{CaptureQuantifier, Language as TsLanguage, Node, Query, StreamingIterator};

/// A language's indent query, split by where a new line's indent starts walking
/// the tree.
///
/// The decision reads regions whose `@indent` node opens on the cursor's row and
/// holds the cursor. A walk from the root visits every node around that row, and
/// a pattern such as `(_ "{" "}" @end) @indent` starts at the `{` of each
/// enclosing list and walks the list's children up to its `}`. Inside a long
/// list, that costs a keypress far more than the row it asks about.
///
/// A pattern that captures `@indent` on its own outermost node keeps its whole
/// match inside that node, so it walks from the highest node that starts on the
/// row. Every other pattern walks from the root. Markdown's
/// `(list (list_item) @indent)` and rust's `((where_clause) _ @end)` sequence are
/// two of them, because their matches reach above or beside the captured node.
///
/// See also:
/// - [`newline_indent`], which reads both parts.
pub struct IndentQueries {
    /// Patterns that walk from the highest node starting on the cursor's row.
    from_row: Option<Query>,
    /// Patterns that walk from the root.
    from_root: Option<Query>,
}

impl IndentQueries {
    /// Splits `query`, compiled from `src`, between the row's node and the root.
    ///
    /// A query that captures `@start` or `@outdent` walks whole from the root. A
    /// `@start` capture moves a region's start off its node, and an `@outdent`
    /// truncates regions that the row's node does not hold. Markdown's
    /// `@start.list_item` is a capture of another name and changes nothing here.
    ///
    /// A part that fails to compile is left out, on the best-effort contract of
    /// the query it came from.
    pub(crate) fn split(grammar: &TsLanguage, query: &Query, src: &str) -> IndentQueries {
        let Some(indent_ix) = query.capture_index_for_name("indent") else {
            return IndentQueries {
                from_row: None,
                from_root: None,
            };
        };
        if query.capture_index_for_name("start").is_some()
            || query.capture_index_for_name("outdent").is_some()
        {
            return IndentQueries {
                from_row: None,
                from_root: compile(grammar, src),
            };
        }

        let mut from_row = String::new();
        let mut from_root = String::new();
        for pattern in 0..query.pattern_count() {
            // A match with no `@indent` capture has no start, so it forms no
            // region for either part to find.
            if query.capture_quantifiers(pattern)[indent_ix as usize] == CaptureQuantifier::Zero {
                continue;
            }
            let text =
                &src[query.start_byte_for_pattern(pattern)..query.end_byte_for_pattern(pattern)];
            let part = if indent_on_outer_node(text) {
                &mut from_row
            } else {
                &mut from_root
            };
            part.push_str(text);
            part.push('\n');
        }

        IndentQueries {
            from_row: compile(grammar, &from_row),
            from_root: compile(grammar, &from_root),
        }
    }
}

/// One indent region resolved from the query, in byte offsets plus the rows its
/// endpoints land on.
struct IndentRange {
    start_byte: usize,
    start_row: u32,
    end_byte: usize,
}

/// Leading whitespace for a new empty line inserted at `cursor_offset`.
///
/// The new line copies the cursor row's leading whitespace, plus one
/// `indent_unit` when the cursor's row opens an `@indent` region the cursor
/// sits inside (so the new line falls inside a freshly opened block). A query
/// yielding no region leaves it at a plain copy. This is Zed's
/// indent-from-previous-row branch specialized to an empty new line, so it
/// needs no post-edit reparse.
///
/// A region opening later on the row does not count. Its delimiter goes down
/// with the new line rather than staying above it, so the line it lands on is
/// still outside the region and belongs at the enclosing level.
///
/// A call costs the row's highest node rather than the nodes around it, apart
/// from the patterns [`IndentQueries`] keeps on the root. A row that holds the
/// whole document, such as a JSON array written on one line, still costs the
/// whole document.
pub fn newline_indent(
    queries: &IndentQueries,
    root: Node<'_>,
    rope: &Rope,
    cursor_offset: usize,
    indent_unit: &str,
) -> String {
    let window = newline_window(rope, cursor_offset);
    let mut ranges = Vec::new();
    if let (Some(query), Some(anchor)) = (
        &queries.from_row,
        row_anchor(root, window.start, cursor_offset),
    ) {
        ranges.extend(collect_indent_ranges(query, anchor, rope, window.clone()));
    }
    if let Some(query) = &queries.from_root {
        ranges.extend(collect_indent_ranges(query, root, rope, window));
    }
    newline_indent_from(&ranges, rope, cursor_offset, indent_unit)
}

/// The highest node holding `cursor_offset` that starts at or after
/// `row_start`, or `None` when no node holding it starts there.
///
/// A region that answers [`newline_indent`] opens on the row before the cursor
/// and ends past it, so its `@indent` node holds the cursor's leaf and starts at
/// or after the row start. Every node between that leaf and the region's node
/// starts no earlier, so the node this returns holds the region's node too.
///
/// With no such node, no region from the row's patterns answers. That is the
/// cursor between two elements of a list that opened on an earlier row, where
/// the smallest node holding the cursor is the list itself.
fn row_anchor(root: Node<'_>, row_start: usize, cursor_offset: usize) -> Option<Node<'_>> {
    let leaf = root.descendant_for_byte_range(cursor_offset, cursor_offset)?;
    let mut node = root;
    while node.start_byte() < row_start {
        node = node
            .child_with_descendant(leaf)
            .filter(|child| *child != node)?;
    }
    Some(node)
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
fn newline_indent_from(
    ranges: &[IndentRange],
    rope: &Rope,
    cursor_offset: usize,
    indent_unit: &str,
) -> String {
    let row = rope.offset_to_point(cursor_offset).row;
    let base = line_leading_whitespace(rope, row);
    let opens = ranges
        .iter()
        .any(|r| r.start_row == row && r.start_byte < cursor_offset && r.end_byte > cursor_offset);
    if opens {
        format!("{base}{indent_unit}")
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
pub fn suggested_indent(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    row: u32,
    indent_unit: &str,
) -> Option<String> {
    suggested_indent_scanning(query, root, rope, row, indent_unit, true)
}

/// [`suggested_indent`], with the query restriction made optional so a test can
/// compare the restricted answer against the whole-prefix one.
fn suggested_indent_scanning(
    query: &Query,
    root: Node<'_>,
    rope: &Rope,
    row: u32,
    indent_unit: &str,
    restrict: bool,
) -> Option<String> {
    let window = if restrict {
        suggested_window(query, rope, row)
    } else {
        0..row_indent_end(rope, row) + 1
    };
    let ranges = collect_indent_ranges(query, root, rope, window);
    suggested_indent_from(&ranges, rope, row, indent_unit)
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
fn suggested_indent_from(
    ranges: &[IndentRange],
    rope: &Rope,
    row: u32,
    indent_unit: &str,
) -> Option<String> {
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
    Some(if indent {
        format!("{base}{indent_unit}")
    } else {
        base
    })
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

/// Whether a pattern's text puts its only `@indent` capture on its outermost
/// node.
///
/// The text has to be one node pattern, `(name ...)`, closed right before the
/// capture. A grouping, an alternation, a quantifier, or a second `@indent`
/// fails the test. Such a pattern walks from the root, which costs time and
/// never loses a match that reaches outside the captured node.
fn indent_on_outer_node(text: &str) -> bool {
    let Some(node) = text.trim_end().strip_suffix("@indent") else {
        return false;
    };
    let node = node.trim();
    let opens_a_node = node
        .strip_prefix('(')
        .and_then(|inner| inner.trim_start().chars().next())
        .is_some_and(|first| first.is_alphabetic() || first == '_');
    opens_a_node && node.ends_with(')') && text.matches("@indent").count() == 1
}

/// The query `src` compiles to, or `None` when it holds no pattern or fails to
/// compile.
fn compile(grammar: &TsLanguage, src: &str) -> Option<Query> {
    if src.trim().is_empty() {
        return None;
    }
    Query::new(grammar, src).ok()
}

#[cfg(test)]
mod tests {
    use super::{
        collect_indent_ranges, indent_on_outer_node, newline_indent, newline_indent_from,
        suggested_indent, suggested_indent_from, suggested_indent_scanning, suggested_window,
        IndentQueries,
    };
    use crate::{Language, LanguageRegistry};
    use std::sync::Arc;
    use stoat_text::Rope;
    use tree_sitter::{Parser, Query, Tree};

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
        newline_at_unit(name, src, cursor, "\t")
    }

    fn newline_at_unit(name: &str, src: &str, cursor: usize, indent_unit: &str) -> String {
        let lang = lang(name);
        let tree = parse(&lang, src);
        let rope = Rope::from(src);
        newline_indent(
            lang.newline_indent_queries().expect("indent queries"),
            tree.root_node(),
            &rope,
            cursor,
            indent_unit,
        )
    }

    fn suggested(name: &str, src: &str, row: u32) -> Option<String> {
        suggested_unit(name, src, row, "\t")
    }

    fn suggested_unit(name: &str, src: &str, row: u32, indent_unit: &str) -> Option<String> {
        let lang = lang(name);
        let tree = parse(&lang, src);
        let rope = Rope::from(src);
        suggested_indent(
            lang.indent_query().expect("indent query"),
            tree.root_node(),
            &rope,
            row,
            indent_unit,
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

    /// The unit is appended to the basis row's own whitespace, so a two-space
    /// buffer indents by two spaces from a two-space base rather than by a tab.
    ///
    /// The base is copied from the text and the delta comes from the caller, so
    /// a buffer whose rows are already spaced would otherwise get a tab glued
    /// onto spaces and drift a level deeper than it reads.
    #[test]
    fn the_indent_unit_is_whatever_the_caller_passes() {
        assert_eq!(newline_at_unit("json", "{\n}\n", 1, "  "), "  ");
        assert_eq!(
            newline_at_unit("json", "{\n  \"a\": {\n  }\n}\n", 10, "  "),
            "    ",
            "an opener on an already-indented row indents from that row",
        );

        let src = "{\n  \"a\": {\n    \"b\": 1\n  }\n}\n";
        assert_eq!(suggested_unit("json", src, 1, "  ").as_deref(), Some("  "));
        assert_eq!(
            suggested_unit("json", src, 2, "  ").as_deref(),
            Some("    ")
        );
        assert_eq!(suggested_unit("json", src, 3, "  ").as_deref(), Some("  "));
    }

    /// A window that is too wide costs time and answers correctly, so only a
    /// comparison against the whole file shows that it is not too narrow.
    ///
    /// What this catches is a window that misses the decision point, which is
    /// the mistake available to a reader shortening one. It says nothing about
    /// how much further each window reaches. The regions a decision reads all
    /// span the byte it asks about, so they come back however tight the window
    /// is, and the one thing that does not come back is an `@outdent`, which no
    /// `indents.scm` in the tree captures. The reach is what keeps the answer
    /// right if one ever does.
    ///
    /// A new line also starts most patterns at the row's highest node rather
    /// than the root. The rows that go on past their opener put the cursor in a
    /// node below the region it asks about. The `where` clause on its own row
    /// and the markdown list item that runs onto a second row are the shapes
    /// whose patterns reach outside that node, and the long json array is the
    /// list that walking from the root pays for.
    #[test]
    fn every_window_answers_what_the_whole_file_answers() {
        let src = "fn a() {\n\tif b {\n\t\twhile c {\n\t\t\tx;\n\t\t}\n\t\tif d { y; }\n\t\tz;\n\t}\n\tw;\n}\n";
        let json_array = format!(
            "[\n{}\n]\n",
            (0..1000)
                .map(|i| format!("\t{i}"))
                .collect::<Vec<_>>()
                .join(",\n")
        );
        let newline_fixtures = [
            ("rust", src.to_owned()),
            ("rust", "fn a() { let x = 1;\n\tlet y = 2;\n}\n".to_owned()),
            ("json", "[ 1, [ 2,\n\t3 ],\n\t4\n]\n".to_owned()),
            ("rust", "fn f<T>()\nwhere\n    T: Clone,\n{\n}\n".to_owned()),
            ("markdown", "- one\n- two\n  more\n- three\n".to_owned()),
            ("json", json_array),
        ];
        for (name, text) in &newline_fixtures {
            let lang = lang(name);
            let tree = parse(&lang, text);
            let rope = Rope::from(text.as_str());
            let root = tree.root_node();
            let query = lang.indent_query().expect("indent query");
            let full = collect_indent_ranges(query, root, &rope, 0..text.len());
            let queries = lang.newline_indent_queries().expect("indent queries");
            for offset in 0..=text.len() {
                assert_eq!(
                    newline_indent(queries, root, &rope, offset, "\t"),
                    newline_indent_from(&full, &rope, offset, "\t"),
                    "{name} newline_indent disagrees at offset {offset}"
                );
            }
        }

        let lang = lang("rust");
        let tree = parse(&lang, src);
        let rope = Rope::from(src);
        let query = lang.indent_query().expect("indent query");
        let root = tree.root_node();
        let whole = 0..src.len();
        for row in 0..=rope.max_point().row {
            let windowed =
                collect_indent_ranges(query, root, &rope, suggested_window(query, &rope, row));
            let full = collect_indent_ranges(query, root, &rope, whole.clone());
            assert_eq!(
                suggested_indent_from(&windowed, &rope, row, "\t"),
                suggested_indent_from(&full, &rope, row, "\t"),
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
            let query = lang.indent_query().expect("indent query");
            let root = tree.root_node();

            for row in 0..=rope.max_point().row {
                assert_eq!(
                    suggested_indent_scanning(query, root, &rope, row, "\t", true),
                    suggested_indent_scanning(query, root, &rope, row, "\t", false),
                    "{name} row {row} of {src:?}",
                );
            }
        }
    }

    /// The two window arguments, checked against a query that captures
    /// `@outdent`, which is the case neither test above can reach.
    ///
    /// No shipped `indents.scm` captures one, so the truncation branch and the
    /// widened window have to be reached through a query written here. A line
    /// comment stands in for the outdent marker because it can be placed on any
    /// row, including well above the band a narrow window would read.
    ///
    /// The row loop is what puts [`suggested_window`]'s widening under test.
    /// Truncation pulls a region's end back to the outdent's own position, and
    /// an outdent above the narrow band is one that scan never sees, leaving an
    /// end long enough to answer a test the whole-prefix scan shortens it out
    /// of. Removing the widening makes six of these fixtures disagree.
    ///
    /// The offset loop is the counterpart for [`newline_window`], which has no
    /// widening and needs none. That decision only reads regions starting on
    /// the cursor's own row, and truncating one needs an outdent at or after
    /// its start, so every outdent that can change the answer already sits
    /// inside the narrow window. What the loop pins is that argument: narrowing
    /// the window further makes it disagree.
    #[test]
    fn an_outdent_query_answers_what_the_whole_file_answers() {
        let outdent_fixtures = [
            "fn a() {\n\t// out\n\tx;\n}\n",
            "fn a() {\n\tif b {\n\t\t// out\n\t\tx;\n\t}\n\ty;\n}\n",
            "fn a() { // out\n\tx;\n}\n",
            "fn a() {\n\t// o1\n\tif b {\n\t\t// o2\n\t\tx;\n\t}\n}\n",
            "fn a() {\n\tif b { // out\n\t\tx;\n\t}\n}\n",
            "fn a() {\n\t// out\n\tif b {\n\t\tx;\n\t}\n}\n",
        ];

        let lang = lang("rust");
        let source = "(_ \"{\" \"}\" @end) @indent\n(line_comment) @outdent\n";
        let query = Query::new(&lang.grammar, source).expect("the synthetic query builds");
        assert!(
            query.capture_index_for_name("outdent").is_some(),
            "the fixtures only reach the branch if the query carries an outdent",
        );
        let queries = IndentQueries::split(&lang.grammar, &query, source);
        assert!(
            queries.from_row.is_none(),
            "an outdent keeps every pattern on the root"
        );

        for src in outdent_fixtures {
            let tree = parse(&lang, src);
            let rope = Rope::from(src);
            let root = tree.root_node();
            let whole = 0..src.len();

            for row in 0..=rope.max_point().row {
                assert_eq!(
                    suggested_indent_scanning(&query, root, &rope, row, "\t", true),
                    suggested_indent_scanning(&query, root, &rope, row, "\t", false),
                    "suggested_indent row {row} of {src:?}",
                );
                assert_eq!(
                    suggested_window(&query, &rope, row).start,
                    0,
                    "and reads the whole prefix to do it, row {row} of {src:?}",
                );
            }

            let full = collect_indent_ranges(&query, root, &rope, whole.clone());
            for offset in 0..=src.len() {
                assert_eq!(
                    newline_indent(&queries, root, &rope, offset, "\t"),
                    newline_indent_from(&full, &rope, offset, "\t"),
                    "newline_indent offset {offset} of {src:?}",
                );
            }
        }
    }

    /// A pattern walks from the row's highest node only when its match stays
    /// inside the node it captures, and from the root otherwise.
    ///
    /// Rust's bracket patterns and json's two capture their outermost node.
    /// Rust's alternation holds the `where` sequence, which reaches a sibling,
    /// and markdown's list pattern captures a child of its root. A shape the
    /// test does not recognize stays on the root.
    #[test]
    fn the_split_sends_each_pattern_where_its_match_lies() {
        let pattern_counts = |name: &str| {
            let language = lang(name);
            let queries = language.newline_indent_queries().expect("indent queries");
            (
                queries.from_row.as_ref().map(Query::pattern_count),
                queries.from_root.as_ref().map(Query::pattern_count),
            )
        };
        assert_eq!(
            [
                pattern_counts("rust"),
                pattern_counts("json"),
                pattern_counts("markdown")
            ],
            [(Some(4), Some(1)), (Some(2), None), (None, Some(1))],
        );

        let shapes = [
            "(_ \"{\" \"}\" @end) @indent",
            "(array\n  \"]\" @end) @indent",
            "(list\n  (list_item) @indent)",
            "[(a) (b)] @indent",
            "((where_clause) _ @end) @indent",
            "(a)* @indent",
            "(a (#eq? @indent \"x\")) @indent",
            "\"{\" @indent",
        ];
        assert_eq!(
            shapes.map(indent_on_outer_node),
            [true, true, false, false, false, false, false, false],
        );

        let rust = lang("rust");
        let source = "(_ \"{\" @start \"}\" @end) @indent\n";
        let query = Query::new(&rust.grammar, source).expect("the query builds");
        let queries = IndentQueries::split(&rust.grammar, &query, source);
        assert!(
            queries.from_row.is_none() && queries.from_root.is_some(),
            "a @start capture keeps the whole query on the root",
        );
    }
}
