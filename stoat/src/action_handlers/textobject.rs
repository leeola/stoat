//! Helix-parity textobject selection: `m a <type>` and `m i <type>`.
//!
//! Pattern mirrors `surround`: the action arms a pending state; the
//! next char keypress is intercepted by [`crate::app::Stoat::handle_key`]
//! and dispatched to [`execute_select_textobject`]. Type chars follow
//! Helix's defaults: `f` (function), `t` (class / type), `p` (paragraph),
//! `a` (parameter), `c` (comment), `T` (test), `e` (entry), `w` (word),
//! `W` (WORD), `m` (closest surrounding pair), and any non-alphanumeric
//! char as its own literal pair (e.g. `(`, `"`).
//!
//! Tree-sitter-driven types use the language's `textobjects_query`
//! (compiled from `textobjects.scm`), then pick the smallest capture
//! containing the cursor. Languages without a textobjects query
//! (json, markdown) no-op for those types. Paragraph is line-based
//! and does not require tree-sitter.

use crate::{
    action_handlers::movement::BlankRows,
    app::{Stoat, UpdateEffect},
    pane::View,
};
use std::collections::HashMap;
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
    let count = super::arming_count(stoat);
    stoat.pending_textobject_select = Some((TextobjectMode::Around, count));
    UpdateEffect::Redraw
}

pub(super) fn select_textobject_inner(stoat: &mut Stoat) -> UpdateEffect {
    let count = super::arming_count(stoat);
    stoat.pending_textobject_select = Some((TextobjectMode::Inner, count));
    UpdateEffect::Redraw
}

/// Select the textobject the type-char + mode chord names around every cursor.
///
/// Each selection resolves its own object from its own cursor, so cursors in
/// different words or pairs each take theirs. A selection whose object does not
/// resolve keeps the range it had. The chord is a no-op when the type char names
/// no textobject, or when no cursor is inside one.
///
/// `count` reaches the pair types alone, where it names the Nth enclosing pair.
/// The word and tree-sitter types have no meaning to give it.
///
/// Cursors resolving to the same object end as one selection, the collection
/// merging selections that overlap.
pub(crate) fn execute_select_textobject(
    stoat: &mut Stoat,
    mode: TextobjectMode,
    ch: char,
    count: usize,
) -> UpdateEffect {
    stoat.last_motion = Some(crate::action_handlers::LastMotion::TextObject { mode, ch, count });
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, cursors, change_hunks) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let cursors: Vec<(usize, usize)> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
                let head_off = buffer_snapshot.resolve_anchor(&sel.head());
                let cursor = stoat_text::cursor_offset(buffer_snapshot.rope(), tail_off, head_off);
                (sel.id, cursor)
            })
            .collect();

        // Resolving every hunk's anchors is the work the gutter does per frame,
        // so only the one chord that reads hunks pays for it. None here means
        // the buffer has no diff at all, which the change object refuses over.
        let change_hunks = (ch == 'g').then(|| {
            display_snapshot.diff_map().map(|diff_map| {
                diff_map
                    .live_hunks(buffer_snapshot)
                    .in_range(0..u32::MAX)
                    .map(|(_, rows)| rows)
                    .collect::<Vec<_>>()
            })
        });
        (buffer_id, cursors, change_hunks)
    };

    let change_hunks = match change_hunks {
        Some(None) => {
            stoat.set_status("no diff for this buffer");
            return UpdateEffect::None;
        },
        Some(Some(hunks)) => hunks,
        None => Vec::new(),
    };
    let ws = stoat.active_workspace_mut();

    let targets: HashMap<usize, std::ops::Range<usize>> = cursors
        .into_iter()
        .filter_map(|(id, cursor)| {
            find_textobject(ws, buffer_id, cursor, mode, ch, count, &change_hunks)
                .map(|range| (id, range))
        })
        .collect();

    if targets.is_empty() {
        return UpdateEffect::None;
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| {
        let Some(range) = targets.get(&sel.id) else {
            return sel.clone();
        };
        let mut new = sel.clone();
        new.start = new_buf.anchor_at(range.start, Bias::Right);
        new.end = new_buf.anchor_at(range.end, Bias::Left);
        new.reversed = false;
        new.goal = SelectionGoal::None;
        new
    });
    UpdateEffect::Redraw
}

/// The byte range of the textobject `ch` names around one cursor, or `None`
/// when the chord names no type or nothing of that type encloses the cursor.
///
/// Taking a single cursor is what lets every selection resolve its own object.
/// The tree-sitter types therefore run their query once per cursor, which is
/// the cost of the objects being per cursor at all.
///
/// `change_hunks` carries the live diff rows the `g` type reads, and is empty
/// for every other type. The caller resolves them once for the whole chord.
fn find_textobject(
    ws: &crate::workspace::Workspace,
    buffer_id: crate::buffer::BufferId,
    cursor: usize,
    mode: TextobjectMode,
    ch: char,
    count: usize,
    change_hunks: &[std::ops::Range<u32>],
) -> Option<std::ops::Range<usize>> {
    let skip = count.saturating_sub(1);
    match ch {
        'p' => {
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            find_textobject_paragraph(guard.rope(), cursor, mode)
        },
        'f' | 't' | 'a' | 'c' | 'T' | 'e' | 'x' => {
            let kind = match ch {
                'f' => "function",
                't' => "class",
                'a' => "parameter",
                'c' => "comment",
                'T' => "test",
                'e' => "entry",
                'x' => "xml-element",
                _ => unreachable!(),
            };
            find_textobject_treesitter(ws, buffer_id, cursor, kind, mode)
        },
        'g' => {
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            let guard = buffer.read().expect("poisoned");
            find_textobject_change(guard.rope(), cursor, change_hunks)
        },
        'm' => {
            let rope = {
                let buffer = ws.buffers.get(buffer_id).expect("buffer");
                buffer.read().expect("poisoned").rope().clone()
            };
            let snapshot = ws.buffers.syntax_map(buffer_id).map(|m| m.snapshot());
            let tree = super::surround::deepest_tree_at(snapshot, cursor);
            let scan = crate::action_handlers::movement::PairScan::around(tree, cursor);
            super::surround::closest_surround_pair(&rope, cursor, &scan, skip).map(
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
            super::surround::surround_pair_at(ws, buffer_id, cursor, open, close, skip)
                .map(|(open_off, close_off)| pair_to_range(open, close, open_off, close_off, mode))
        },
        _ => None,
    }
}

/// The byte range of the diff hunk `cursor` sits on, as whole lines.
///
/// The range runs from the start of the hunk's first row to the start of the
/// row after its last, so it carries the final row's line ending the way a
/// linewise selection does.
///
/// A deletion or move seam covers no rows of the buffer, and answers `None`
/// rather than a cursor-width range at the seam. Around and inside resolve
/// alike, since a hunk has no delimiters to sit outside of.
fn find_textobject_change(
    rope: &Rope,
    cursor: usize,
    change_hunks: &[std::ops::Range<u32>],
) -> Option<std::ops::Range<usize>> {
    let row = rope.offset_to_point(cursor).row;
    let rows = change_hunks
        .iter()
        .find(|rows| rows.start <= row && row < rows.end)?;

    let start = rope.point_to_offset(Point::new(rows.start, 0));
    let end = rope.point_to_offset(Point::new(rows.end, 0));
    Some(start..end)
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
    let query = layer.language.textobjects_query()?;
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

    // One lookup across every scan below, so the windows it reads carry from
    // the outward search to the paragraph's ends.
    let mut blanks = BlankRows::new(rope);

    if blanks.is_blank(cursor_row) {
        let mut probe = cursor_row;
        let mut found = None;
        while probe > 0 {
            probe -= 1;
            if !blanks.is_blank(probe) {
                found = Some(probe);
                break;
            }
        }
        if found.is_none() {
            let mut probe = cursor_row;
            while probe < max_row {
                probe += 1;
                if !blanks.is_blank(probe) {
                    found = Some(probe);
                    break;
                }
            }
        }
        let anchor_row = found?;
        return paragraph_range_starting_from(rope, anchor_row, mode, max_row, &mut blanks);
    }

    paragraph_range_starting_from(rope, cursor_row, mode, max_row, &mut blanks)
}

fn paragraph_range_starting_from(
    rope: &Rope,
    anchor_row: u32,
    mode: TextobjectMode,
    max_row: u32,
    blanks: &mut BlankRows<'_>,
) -> Option<std::ops::Range<usize>> {
    let mut start_row = anchor_row;
    while start_row > 0 && !blanks.is_blank(start_row - 1) {
        start_row -= 1;
    }
    let mut end_row = anchor_row;
    while end_row < max_row && !blanks.is_blank(end_row + 1) {
        end_row += 1;
    }
    let start = rope.point_to_offset(Point::new(start_row, 0));
    let inner_end = end_of_line_offset(rope, end_row);
    match mode {
        TextobjectMode::Inner => Some(start..inner_end),
        TextobjectMode::Around => {
            let mut tail_row = end_row;
            while tail_row < max_row && blanks.is_blank(tail_row + 1) {
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

    /// Alt-. after the chord runs the chord again, without reading its type
    /// char a second time.
    ///
    /// The mode and the char both come from keypresses the replay never sees.
    /// A recording that dropped them leaves the chord unrepeatable.
    #[test]
    fn repeat_last_motion_replays_the_textobject_chord() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "alpha\nbeta\n\ngamma\n");
        jump(&mut h, 2);

        h.type_keys("m i p");
        assert_eq!(primary_range(&mut h), (0, 11));

        jump(&mut h, 14);
        crate::action_handlers::dispatch(&mut h.stoat, &action::RepeatLastMotion);
        assert_eq!(
            primary_range(&mut h),
            (12, 18),
            "the chord runs again on the paragraph the cursor moved to",
        );
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

    /// Open `name` in a repo where its HEAD text differs from the buffer's, and
    /// install the diff map on this turn so the hunks are there to select.
    fn seed_with_diff(h: &mut TestHarness, name: &str, head: &str, working: &str) -> PathBuf {
        let root = PathBuf::from("/textobject-diff");
        h.stage_review_scenario(root.clone(), &[(name, head, working)]);
        let path = root.join(name);
        h.open_file(&path);
        h.settle();

        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        let git_host = h.stoat.git_host.clone();
        let language_registry = h.stoat.language_registry.clone();
        let syntax_styles = h.stoat.syntax_styles.clone();
        let base_cache = h.stoat.base_highlights_cache.clone();
        h.stoat.active_workspace_mut().install_diff_map_now(
            &git_host,
            &language_registry,
            &syntax_styles,
            &base_cache,
            buffer_id,
        );
        path
    }

    #[test]
    fn change_object_selects_the_hunk_rows() {
        let mut h = TestHarness::with_size(40, 10);
        seed_with_diff(&mut h, "a.txt", "a\nb\nc\n", "a\nX\nc\n");
        jump(&mut h, 2);

        h.type_keys("m i g");
        assert_eq!(
            primary_range(&mut h),
            (2, 4),
            "the modified row, through its line ending"
        );
    }

    /// A hunk has no delimiters to sit outside of, so both modes take the rows.
    #[test]
    fn change_object_around_matches_inside() {
        let mut h = TestHarness::with_size(40, 10);
        seed_with_diff(&mut h, "a.txt", "a\nb\nc\n", "a\nX\nc\n");
        jump(&mut h, 2);

        h.type_keys("m a g");
        assert_eq!(primary_range(&mut h), (2, 4));
    }

    #[test]
    fn change_object_off_a_hunk_keeps_the_selection() {
        let mut h = TestHarness::with_size(40, 10);
        seed_with_diff(&mut h, "a.txt", "a\nb\nc\n", "a\nX\nc\n");
        jump(&mut h, 0);
        let before = primary_range(&mut h);

        h.type_keys("m i g");
        assert_eq!(primary_range(&mut h), before);
        assert_eq!(
            h.stoat.pending_message, None,
            "an unchanged row is no error"
        );
    }

    #[test]
    fn change_object_without_a_diff_map_reports_it() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "alpha\nbeta\n");
        jump(&mut h, 2);
        let before = primary_range(&mut h);

        h.type_keys("m i g");
        assert_eq!(primary_range(&mut h), before);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no diff for this buffer"),
        );
    }

    /// No grammar in this tree captures xml elements. The arm only puts `x` on
    /// the tree-sitter path, where rust answers with nothing. This pin catches
    /// the arm without its capture name, which panics the kind match.
    #[test]
    fn xml_element_object_no_ops_without_the_capture() {
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", "fn alpha() {\n    let x = 1;\n}\n");
        h.settle();
        jump(&mut h, 17);
        let before = primary_range(&mut h);

        h.type_keys("m i x");
        assert_eq!(primary_range(&mut h), before);
    }

    #[test]
    fn count_prefix_pair_object_takes_the_outer_pair() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "((abc))\n");
        jump(&mut h, 3);

        h.type_keys("2 m i (");
        assert_eq!(primary_range(&mut h), (1, 6));
    }

    #[test]
    fn count_prefix_closest_pair_object_takes_the_outer_pair() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "([abc])\n");
        jump(&mut h, 3);

        h.type_keys("2 m a m");
        assert_eq!(
            primary_range(&mut h),
            (0, 7),
            "the parens, brackets and all"
        );
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
    fn each_cursor_selects_its_own_word() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "foo baz\nbar quux\n");
        h.type_keys("shift-C");
        assert_eq!(h.selection_spans(), vec![(0, 1, false), (8, 9, false)]);

        h.type_keys("m i w");
        assert_eq!(
            h.selection_spans(),
            vec![(0, 3, false), (8, 11, false)],
            "one cursor's word was imposed on the other"
        );
    }

    #[test]
    fn each_cursor_selects_its_own_pair() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "(ab) (cd)\n");
        h.type_keys("%");
        h.type_keys("s");
        h.type_text("[ac]");
        h.type_keys("Enter");
        assert_eq!(h.selection_spans(), vec![(1, 2, false), (6, 7, false)]);

        h.type_keys("m i (");
        assert_eq!(h.selection_spans(), vec![(1, 3, false), (6, 8, false)]);
    }

    #[test]
    fn a_cursor_with_no_object_stays_where_it_is() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "(ab) cd\n");
        h.type_keys("%");
        h.type_keys("s");
        h.type_text("[ad]");
        h.type_keys("Enter");
        assert_eq!(h.selection_spans(), vec![(1, 2, false), (6, 7, false)]);

        // The second cursor is in no pair at all, so it has nothing to select.
        h.type_keys("m i (");
        assert_eq!(
            h.selection_spans(),
            vec![(1, 3, false), (6, 7, false)],
            "a cursor with no object was moved onto another cursor's"
        );
    }

    #[test]
    fn cursors_in_one_word_become_one_selection() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "buf.txt", "hello world\n");
        h.type_keys("%");
        h.type_keys("s");
        h.type_text("[he]");
        h.type_keys("Enter");
        assert_eq!(h.selection_spans(), vec![(0, 1, false), (1, 2, false)]);

        h.type_keys("m i w");
        assert_eq!(
            h.selection_spans(),
            vec![(0, 5, false)],
            "both resolve to the same word, so the collection merges them"
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

    #[test]
    fn test_around_selects_attributed_function() {
        let mut h = TestHarness::with_size(60, 20);
        let src = "#[test]\nfn checks() {\n    assert!(true);\n}\n";
        seed(&mut h, "main.rs", src);
        h.settle();
        let body_off = src.find("assert").expect("body present");
        jump(&mut h, body_off);
        h.type_keys("m a T");
        let (start, end) = primary_range(&mut h);
        let span = &src[start..end];
        assert!(
            span.contains("fn checks"),
            "around should cover the fn, got {span:?}"
        );
        assert!(span.contains("assert!(true)"));
    }

    #[test]
    fn entry_inner_selects_field_value() {
        let mut h = TestHarness::with_size(60, 20);
        let src = "struct S { x: u32 }\nfn f() -> S { S { x: 42 } }\n";
        seed(&mut h, "main.rs", src);
        h.settle();
        let val_off = src.rfind("42").expect("value present");
        jump(&mut h, val_off);
        h.type_keys("m i e");
        let (start, end) = primary_range(&mut h);
        assert_eq!(&src[start..end], "42");
    }

    #[test]
    fn entry_around_selects_array_element() {
        let mut h = TestHarness::with_size(60, 20);
        let src = "fn f() {\n    let a = [1, 2, 3];\n}\n";
        seed(&mut h, "main.rs", src);
        h.settle();
        let elem_off = src.find('2').expect("element present");
        jump(&mut h, elem_off);
        h.type_keys("m a e");
        let (start, end) = primary_range(&mut h);
        assert_eq!(&src[start..end], "2");
    }

    #[test]
    fn entry_inner_on_array_element_is_noop() {
        let mut h = TestHarness::with_size(60, 20);
        let src = "fn f() {\n    let a = [1, 2, 3];\n}\n";
        seed(&mut h, "main.rs", src);
        h.settle();
        let elem_off = src.find('2').expect("element present");
        jump(&mut h, elem_off);
        let before = primary_range(&mut h);
        h.type_keys("m i e");
        assert_eq!(
            primary_range(&mut h),
            before,
            "array elements have only entry.around"
        );
    }
}
