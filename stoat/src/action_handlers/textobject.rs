//! Helix-parity textobject selection: `m a <type>` and `m i <type>`.
//!
//! Pattern mirrors `surround`: the action arms a pending state; the
//! next char keypress is intercepted by [`crate::app::Stoat::handle_key`]
//! and dispatched to [`execute_select_textobject`]. Type chars follow
//! Helix's defaults: `f` (function), `t` (class / type), `p` (paragraph),
//! `a` (parameter), `c` (comment).
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
use stoat_text::{Bias, Point, Rope, SelectionGoal};

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
