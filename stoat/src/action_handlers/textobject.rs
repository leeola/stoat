//! Helix-parity textobject selection: `m a <type>` and `m i <type>`.
//!
//! Pattern mirrors `surround`: the action arms a pending state; the
//! next char keypress is intercepted by [`crate::app::Stoat::handle_key`]
//! and dispatched to [`execute_select_textobject`]. Type chars follow
//! Helix's defaults: `f` (function), `t` (class / type), `p` (paragraph),
//! `a` (parameter), `c` (comment), `w` (word), `W` (WORD), `m` (closest
//! surrounding pair), and any non-alphanumeric char as its own literal
//! pair (e.g. `(`, `"`).
//!
//! Tree-sitter-driven types use the language's `textobjects_query`
//! (compiled from `textobjects.scm`), then pick the smallest capture
//! containing the cursor. Languages without a textobjects query
//! (json, markdown) no-op for those types. Paragraph is line-based
//! and does not require tree-sitter.

use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
};
use stoat_text::{Bias, CharCategory, Point, Rope, SelectionGoal};

/// Around / inside selection mode for the active textobject chord.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextobjectMode {
    Around,
    Inner,
}

impl TextobjectMode {
    fn capture_suffix(self) -> &'static str {
        match self {
            TextobjectMode::Around => "around",
            TextobjectMode::Inner => "inside",
        }
    }
}

pub(super) fn select_textobject_around(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_textobject_select = Some(TextobjectMode::Around);
    UpdateEffect::Redraw
}

pub(super) fn select_textobject_inner(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_textobject_select = Some(TextobjectMode::Inner);
    UpdateEffect::Redraw
}

/// Resolve the type-char + mode chord into a target byte range and
/// install it as the focused editor's primary selection. Unknown
/// type chars and ranges that cannot be resolved are no-ops.
pub(crate) fn execute_select_textobject(
    stoat: &mut Stoat,
    mode: TextobjectMode,
    ch: char,
) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, cursor) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        let cursor = buffer_snapshot.resolve_anchor(&head);
        (buffer_id, cursor)
    };

    let target = match ch {
        'p' => {
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            find_textobject_paragraph(guard.rope(), cursor, mode)
        },
        'f' | 't' | 'a' | 'c' => {
            let kind = match ch {
                'f' => "function",
                't' => "class",
                'a' => "parameter",
                'c' => "comment",
                _ => unreachable!(),
            };
            find_textobject_treesitter(ws, buffer_id, cursor, kind, mode)
        },
        'm' => {
            let rope = {
                let buffer = ws.buffers.get(buffer_id).expect("buffer");
                buffer.read().expect("poisoned").rope().clone()
            };
            let snapshot = ws.buffers.syntax_map(buffer_id).map(|m| m.snapshot());
            let tree = super::surround::deepest_tree_at(snapshot, cursor);
            super::surround::closest_surround_pair(&rope, cursor, tree).map(
                |(open, close, open_off, close_off)| {
                    pair_to_range(open, close, open_off, close_off, mode)
                },
            )
        },
        'w' | 'W' => {
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            find_textobject_word(guard.rope(), cursor, mode, ch == 'W')
        },
        pair if !pair.is_ascii_alphanumeric() => {
            let (open, close) = super::surround::surround_pair_for(pair);
            super::surround::surround_pair_at(ws, buffer_id, cursor, open, close)
                .map(|(open_off, close_off)| pair_to_range(open, close, open_off, close_off, mode))
        },
        _ => None,
    };

    let Some(range) = target else {
        return UpdateEffect::None;
    };

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    let new_start = new_buf.anchor_at(range.start, Bias::Right);
    let new_end = new_buf.anchor_at(range.end, Bias::Left);
    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        new.start = new_start;
        new.end = new_end;
        new.reversed = false;
        new.goal = SelectionGoal::None;
        new
    });
    UpdateEffect::Redraw
}

/// Byte range for a resolved surround pair, given the delimiter chars
/// and their offsets. Inner excludes both delimiters; Around spans from
/// the open delimiter through the close.
fn pair_to_range(
    open: char,
    close: char,
    open_off: usize,
    close_off: usize,
    mode: TextobjectMode,
) -> std::ops::Range<usize> {
    match mode {
        TextobjectMode::Inner => open_off + open.len_utf8()..close_off,
        TextobjectMode::Around => open_off..close_off + close.len_utf8(),
    }
}

/// Run the focused buffer's [`textobjects_query`](stoat_language::Language::textobjects_query)
/// over the deepest syntax layer covering `cursor`, looking for the
/// smallest capture named `<kind>.{around|inside}`. Returns the
/// matching byte range or `None` when the language has no textobjects
/// query, the cursor is outside any capture, or the capture name is
/// absent from the query (e.g. a language whose textobjects.scm has
/// no `class.around`).
fn find_textobject_treesitter(
    ws: &crate::workspace::Workspace,
    buffer_id: crate::buffer::BufferId,
    cursor: usize,
    kind: &str,
    mode: TextobjectMode,
) -> Option<std::ops::Range<usize>> {
    let syntax_map = ws.buffers.syntax_map(buffer_id)?;
    let snapshot = syntax_map.snapshot();
    let layer =
        snapshot
            .iter_layers()
            .fold(None::<&stoat_language::SyntaxLayer>, |acc, layer| {
                let start = layer.start_offset as usize;
                let end = layer.end_offset as usize;
                if start <= cursor && end >= cursor {
                    match acc {
                        Some(prev) if prev.depth >= layer.depth => acc,
                        _ => Some(layer),
                    }
                } else {
                    acc
                }
            })?;
    let query = layer.language.textobjects_query.as_ref()?;
    let buffer = ws.buffers.get(buffer_id)?;
    let guard = buffer.read().ok()?;
    let capture_name = format!("{kind}.{}", mode.capture_suffix());
    stoat_language::find_smallest_capture_at(
        query,
        layer.tree.root_node(),
        guard.rope(),
        &capture_name,
        cursor,
    )
}

/// Line-based paragraph textobject. Walks lines around `cursor`
/// finding the run of non-blank lines (a "paragraph"). Around mode
/// includes the trailing blank-line run; Inner mode trims trailing
/// blanks. A blank line is one whose [`Rope::line_len`] is zero.
///
/// Returns `None` when `cursor` sits on a blank line and no
/// surrounding paragraph extends across it (i.e. the buffer has no
/// non-blank line at all, or only blank lines around the cursor).
fn find_textobject_paragraph(
    rope: &Rope,
    cursor: usize,
    mode: TextobjectMode,
) -> Option<std::ops::Range<usize>> {
    let max_row = rope.max_point().row;
    let cursor_row = rope.offset_to_point(cursor).row;
    if rope.is_empty() {
        return None;
    }

    if rope.line_len(cursor_row) == 0 {
        let mut probe = cursor_row;
        let mut found = None;
        while probe > 0 {
            probe -= 1;
            if rope.line_len(probe) > 0 {
                found = Some(probe);
                break;
            }
        }
        if found.is_none() {
            let mut probe = cursor_row;
            while probe < max_row {
                probe += 1;
                if rope.line_len(probe) > 0 {
                    found = Some(probe);
                    break;
                }
            }
        }
        let anchor_row = found?;
        return paragraph_range_starting_from(rope, anchor_row, mode, max_row);
    }

    let mut start_row = cursor_row;
    while start_row > 0 && rope.line_len(start_row - 1) > 0 {
        start_row -= 1;
    }
    let mut end_row = cursor_row;
    while end_row < max_row && rope.line_len(end_row + 1) > 0 {
        end_row += 1;
    }

    let start = rope.point_to_offset(Point::new(start_row, 0));
    let inner_end = end_of_line_offset(rope, end_row);
    match mode {
        TextobjectMode::Inner => Some(start..inner_end),
        TextobjectMode::Around => {
            let mut tail_row = end_row;
            while tail_row < max_row && rope.line_len(tail_row + 1) == 0 {
                tail_row += 1;
            }
            let around_end = if tail_row == end_row {
                inner_end
            } else {
                end_of_line_offset(rope, tail_row)
            };
            Some(start..around_end)
        },
    }
}

fn paragraph_range_starting_from(
    rope: &Rope,
    anchor_row: u32,
    mode: TextobjectMode,
    max_row: u32,
) -> Option<std::ops::Range<usize>> {
    let mut start_row = anchor_row;
    while start_row > 0 && rope.line_len(start_row - 1) > 0 {
        start_row -= 1;
    }
    let mut end_row = anchor_row;
    while end_row < max_row && rope.line_len(end_row + 1) > 0 {
        end_row += 1;
    }
    let start = rope.point_to_offset(Point::new(start_row, 0));
    let inner_end = end_of_line_offset(rope, end_row);
    match mode {
        TextobjectMode::Inner => Some(start..inner_end),
        TextobjectMode::Around => {
            let mut tail_row = end_row;
            while tail_row < max_row && rope.line_len(tail_row + 1) == 0 {
                tail_row += 1;
            }
            let around_end = if tail_row == end_row {
                inner_end
            } else {
                end_of_line_offset(rope, tail_row)
            };
            Some(start..around_end)
        },
    }
}

fn end_of_line_offset(rope: &Rope, row: u32) -> usize {
    let max = rope.max_point();
    if row >= max.row {
        rope.len()
    } else {
        rope.point_to_offset(Point::new(row + 1, 0))
    }
}

/// Word textobject over the char at `cursor`. Inner spans the run of
/// chars sharing the cursor char's category; Around also swallows the
/// trailing whitespace run, or the leading run when there is no trailing
/// whitespace. `long` (the `W` object) splits only on whitespace and
/// line endings, so a token like `foo.bar` stays whole.
///
/// Returns `None` when the cursor sits on whitespace or a line ending,
/// where there is no word to select.
fn find_textobject_word(
    rope: &Rope,
    cursor: usize,
    mode: TextobjectMode,
    long: bool,
) -> Option<std::ops::Range<usize>> {
    let word_start = find_word_boundary(rope, cursor, false, long);
    let word_end = match rope.chars_at(cursor).next() {
        Some(c)
            if !matches!(
                stoat_text::categorize_char(c),
                CharCategory::Whitespace | CharCategory::Eol
            ) =>
        {
            find_word_boundary(rope, cursor + c.len_utf8(), true, long)
        },
        _ => cursor,
    };

    if word_start == word_end {
        return None;
    }

    match mode {
        TextobjectMode::Inner => Some(word_start..word_end),
        TextobjectMode::Around => {
            let trailing: usize = rope
                .chars_at(word_end)
                .take_while(|c| stoat_text::categorize_char(*c) == CharCategory::Whitespace)
                .map(char::len_utf8)
                .sum();
            if trailing > 0 {
                Some(word_start..word_end + trailing)
            } else {
                let leading: usize = rope
                    .reversed_chars_at(word_start)
                    .take_while(|c| stoat_text::categorize_char(*c) == CharCategory::Whitespace)
                    .map(char::len_utf8)
                    .sum();
                Some(word_start - leading..word_end)
            }
        },
    }
}

/// Byte offset of the word boundary reached by scanning from `pos` in
/// one direction. The scan stops at whitespace and line endings. For
/// short words (`long` false) it also stops where the char category
/// changes (e.g. word to punctuation). `forward` scans toward the end
/// of the buffer, otherwise toward the start.
fn find_word_boundary(rope: &Rope, mut pos: usize, forward: bool, long: bool) -> usize {
    let len = rope.len();
    let boundary = |category: CharCategory, prev: CharCategory, at: usize| {
        !long && category != prev && at != 0 && at != len
    };

    if forward {
        let mut prev = rope
            .reversed_chars_at(pos)
            .next()
            .map_or(CharCategory::Whitespace, stoat_text::categorize_char);
        for ch in rope.chars_at(pos) {
            match stoat_text::categorize_char(ch) {
                CharCategory::Eol | CharCategory::Whitespace => return pos,
                category => {
                    if boundary(category, prev, pos) {
                        return pos;
                    }
                    pos += ch.len_utf8();
                    prev = category;
                },
            }
        }
    } else {
        let mut prev = rope
            .chars_at(pos)
            .next()
            .map_or(CharCategory::Whitespace, stoat_text::categorize_char);
        for ch in rope.reversed_chars_at(pos) {
            match stoat_text::categorize_char(ch) {
                CharCategory::Eol | CharCategory::Whitespace => return pos,
                category => {
                    if boundary(category, prev, pos) {
                        return pos;
                    }
                    pos = pos.saturating_sub(ch.len_utf8());
                    prev = category;
                },
            }
        }
    }
    pos
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};
    use std::path::PathBuf;
    use stoat_action::{self as action, OpenFile};

    fn seed(h: &mut TestHarness, name: &str, contents: &str) -> PathBuf {
        let root = PathBuf::from("/textobject-test");
        let path = root.join(name);
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
    }

    fn primary_range(h: &mut TestHarness) -> (usize, usize) {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        (start, end)
    }

    fn jump(h: &mut TestHarness, offset: usize) {
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, offset);
    }

    fn rope_of(s: &str) -> Rope {
        let mut r = Rope::new();
        r.push(s);
        r
    }

    #[test]
    fn paragraph_inner_selects_run_of_nonblank_lines() {
        let r = rope_of("alpha\nbeta\n\ngamma\n");
        let range =
            find_textobject_paragraph(&r, 2, TextobjectMode::Inner).expect("paragraph found");
        assert_eq!(range, 0..11);
    }

    #[test]
    fn paragraph_around_includes_trailing_blank() {
        let r = rope_of("alpha\nbeta\n\ngamma\n");
        let range =
            find_textobject_paragraph(&r, 2, TextobjectMode::Around).expect("paragraph found");
        assert_eq!(range, 0..12);
    }

    #[test]
    fn paragraph_cursor_on_blank_line_finds_neighbour() {
        let r = rope_of("alpha\n\nbeta\n");
        let range = find_textobject_paragraph(&r, 6, TextobjectMode::Inner)
            .expect("neighbour paragraph found");
        assert_eq!(range, 0..6);
    }

    #[test]
    fn paragraph_empty_buffer_is_none() {
        let r = rope_of("");
        assert_eq!(
            find_textobject_paragraph(&r, 0, TextobjectMode::Inner),
            None
        );
    }

    #[test]
    fn paragraph_no_blank_lines_selects_whole_buffer() {
        let r = rope_of("alpha\nbeta\ngamma\n");
        let range =
            find_textobject_paragraph(&r, 7, TextobjectMode::Inner).expect("paragraph found");
        assert_eq!(range, 0..17);
    }

    #[test]
    fn paragraph_via_chord_in_match_mode() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "buf.txt", "alpha\nbeta\n\ngamma\n");
        jump(&mut h, 2);
        h.type_keys("m i p");
        assert_eq!(primary_range(&mut h), (0, 11));
        assert_eq!(h.stoat.focused_mode(), "normal");
        let _ = path;
    }

    #[test]
    fn paragraph_around_via_chord() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "alpha\nbeta\n\ngamma\n");
        jump(&mut h, 2);
        h.type_keys("m a p");
        assert_eq!(primary_range(&mut h), (0, 12));
    }

    #[test]
    fn pending_clears_on_non_char_keypress() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "alpha\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SelectTextobjectInner);
        assert!(h.stoat.pending_textobject_select.is_some());
        h.type_keys("escape");
        assert!(h.stoat.pending_textobject_select.is_none());
    }

    #[test]
    fn unknown_type_char_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "alpha beta\n");
        jump(&mut h, 3);
        let before = primary_range(&mut h);
        h.type_keys("m i z");
        assert_eq!(primary_range(&mut h), before);
    }

    #[test]
    fn function_inner_selects_body_of_rust_fn() {
        let mut h = TestHarness::with_size(60, 20);
        let src = "fn alpha() {\n    let x = 1;\n}\n";
        seed(&mut h, "main.rs", src);
        h.settle();
        let body_off = src.find("let").expect("body present");
        jump(&mut h, body_off);
        h.type_keys("m i f");
        let (start, end) = primary_range(&mut h);
        let span = &src[start..end];
        assert!(span.starts_with("{"), "got span {span:?}");
        assert!(span.contains("let x = 1;"));
        assert!(span.ends_with("}"));
    }

    #[test]
    fn function_around_selects_full_rust_fn() {
        let mut h = TestHarness::with_size(60, 20);
        let src = "fn alpha() {\n    let x = 1;\n}\n";
        seed(&mut h, "main.rs", src);
        h.settle();
        let body_off = src.find("let").expect("body present");
        jump(&mut h, body_off);
        h.type_keys("m a f");
        let (start, end) = primary_range(&mut h);
        let span = &src[start..end];
        assert!(
            span.contains("fn alpha"),
            "around should cover signature, got {span:?}"
        );
        assert!(span.contains("let x = 1;"));
    }

    #[test]
    fn class_inner_selects_struct_body() {
        let mut h = TestHarness::with_size(60, 20);
        let src = "struct Foo {\n    field: u32,\n}\n";
        seed(&mut h, "main.rs", src);
        h.settle();
        let body_off = src.find("field").expect("body present");
        jump(&mut h, body_off);
        h.type_keys("m i t");
        let (start, end) = primary_range(&mut h);
        let span = &src[start..end];
        assert!(
            span.contains("field"),
            "class body should include field, got {span:?}"
        );
    }

    #[test]
    fn parameter_inner_selects_single_argument() {
        let mut h = TestHarness::with_size(60, 20);
        let src = "fn foo(a: u32, b: u32) {}\n";
        seed(&mut h, "main.rs", src);
        h.settle();
        let body_off = src.find("a:").expect("first param");
        jump(&mut h, body_off);
        h.type_keys("m i a");
        let (start, end) = primary_range(&mut h);
        let span = &src[start..end];
        assert!(span.contains("a: u32"), "parameter span {span:?}");
        assert!(
            !span.contains("b:"),
            "inner should not include sibling, got {span:?}"
        );
    }

    #[test]
    fn comment_around_selects_block_of_line_comments() {
        let mut h = TestHarness::with_size(60, 20);
        let src = "// first line\n// second line\nfn foo() {}\n";
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 4);
        h.type_keys("m a c");
        let (start, end) = primary_range(&mut h);
        let span = &src[start..end];
        assert!(span.contains("first line"));
        assert!(span.contains("second line"));
        assert!(!span.contains("fn foo"));
    }

    #[test]
    fn no_textobjects_query_for_json_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        let src = "{\"a\": 1}\n";
        seed(&mut h, "data.json", src);
        h.settle();
        jump(&mut h, 5);
        let before = primary_range(&mut h);
        h.type_keys("m i f");
        assert_eq!(
            primary_range(&mut h),
            before,
            "json has no textobjects.scm; function lookup should leave selection unchanged",
        );
    }

    #[test]
    fn pair_char_inner_selects_content() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "(abc)\n");
        jump(&mut h, 2);
        h.type_keys("m i (");
        assert_eq!(primary_range(&mut h), (1, 4));
    }

    #[test]
    fn pair_char_around_includes_delimiters() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "(abc)\n");
        jump(&mut h, 2);
        h.type_keys("m a (");
        assert_eq!(primary_range(&mut h), (0, 5));
    }

    #[test]
    fn pair_char_quote_inner_selects_content() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "\"abc\"\n");
        jump(&mut h, 2);
        h.type_keys("m i \"");
        assert_eq!(primary_range(&mut h), (1, 4));
    }

    #[test]
    fn closest_pair_inner_picks_innermost() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "fn f() { let y = ([x]); }\n";
        seed(&mut h, "main.rs", src);
        h.settle();
        let x_off = src.find('x').expect("target present");
        jump(&mut h, x_off);
        h.type_keys("m i m");
        let (start, end) = primary_range(&mut h);
        assert_eq!(&src[start..end], "x", "innermost pair inner is just x");
    }

    #[test]
    fn pair_char_no_enclosing_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "abc\n");
        jump(&mut h, 1);
        let before = primary_range(&mut h);
        h.type_keys("m i (");
        assert_eq!(primary_range(&mut h), before);
    }

    #[test]
    fn word_inner_selects_word() {
        let r = rope_of("hello world\n");
        assert_eq!(
            find_textobject_word(&r, 2, TextobjectMode::Inner, false),
            Some(0..5)
        );
    }

    #[test]
    fn word_around_includes_trailing_space() {
        let r = rope_of("hello world\n");
        assert_eq!(
            find_textobject_word(&r, 2, TextobjectMode::Around, false),
            Some(0..6)
        );
    }

    #[test]
    fn word_inner_selects_punctuation_run() {
        let r = rope_of("a::b\n");
        assert_eq!(
            find_textobject_word(&r, 1, TextobjectMode::Inner, false),
            Some(1..3)
        );
    }

    #[test]
    fn long_word_spans_punctuation() {
        let r = rope_of("foo.bar\n");
        assert_eq!(
            find_textobject_word(&r, 2, TextobjectMode::Inner, true),
            Some(0..7)
        );
    }

    #[test]
    fn word_on_whitespace_is_none() {
        let r = rope_of("a b\n");
        assert_eq!(
            find_textobject_word(&r, 1, TextobjectMode::Inner, false),
            None
        );
    }

    #[test]
    fn word_around_uses_leading_when_no_trailing() {
        let r = rope_of("a bb\n");
        assert_eq!(
            find_textobject_word(&r, 3, TextobjectMode::Around, false),
            Some(1..4)
        );
    }

    #[test]
    fn word_inner_via_chord() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "hello world\n");
        jump(&mut h, 2);
        h.type_keys("m i w");
        assert_eq!(primary_range(&mut h), (0, 5));
    }

    #[test]
    fn long_word_via_chord() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "foo.bar baz\n");
        jump(&mut h, 2);
        h.type_keys("m i W");
        assert_eq!(primary_range(&mut h), (0, 7));
    }
}
