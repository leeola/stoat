use crate::{
    display_map::{BlockRowKind, DisplaySnapshot},
    editor_state::EditorState,
    merge_view::{ChunkState, MergeDoc, RowPick},
    render::review::{
        dim_rgb, fill_line_tint, paint_highlighted_row, render_empty_num, render_review_cursor,
        render_side_num, render_side_text, style_rgb,
    },
    review::ReviewSide,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use stoat_text::Anchor;

/// At or above this inner width the ours and theirs columns keep a line-number
/// gutter. Below it they drop it so their text has room to read.
const CONFLICT_SIDE_NUM_MIN: u16 = 120;

/// Blend fraction toward the background for an unresolved chunk's tint wash.
const BAND_TINT: f32 = 0.85;

/// Column geometry for the three-way conflict view, resolved once per frame.
///
/// Three equal columns (ours, center, theirs) separated by two one-cell rules.
/// The center always keeps a one-cell state gutter, a five-cell number gutter,
/// and its text. The side columns keep a five-cell number gutter only at or
/// above [`CONFLICT_SIDE_NUM_MIN`]. Below it they drop it.
struct ConflictColumns {
    ours_num_x: u16,
    ours_text_x: u16,
    center_gutter_x: u16,
    center_num_x: u16,
    center_text_x: u16,
    theirs_num_x: u16,
    theirs_text_x: u16,
    sep1_x: u16,
    sep2_x: u16,
    side_w: usize,
    center_w: usize,
    side_nums: bool,
}

impl ConflictColumns {
    fn compute(inner: Rect) -> Self {
        let num_w: u16 = 5;
        let sep: u16 = 1;
        let side_nums = inner.width >= CONFLICT_SIDE_NUM_MIN;

        let col_w = inner.width.saturating_sub(2 * sep) / 3;
        let ours_x = inner.x;
        let sep1_x = ours_x + col_w;
        let center_x = sep1_x + sep;
        let sep2_x = center_x + col_w;
        let theirs_x = sep2_x + sep;

        let center_gutter_x = center_x;
        let center_num_x = center_x + 1;
        let center_text_x = center_num_x + num_w;
        let center_w = (col_w as usize).saturating_sub((1 + num_w) as usize);

        let (ours_num_x, ours_text_x, theirs_num_x, theirs_text_x, side_w) = if side_nums {
            (
                ours_x,
                ours_x + num_w,
                theirs_x,
                theirs_x + num_w,
                (col_w as usize).saturating_sub(num_w as usize),
            )
        } else {
            (ours_x, ours_x, theirs_x, theirs_x, col_w as usize)
        };

        Self {
            ours_num_x,
            ours_text_x,
            center_gutter_x,
            center_num_x,
            center_text_x,
            theirs_num_x,
            theirs_text_x,
            sep1_x,
            sep2_x,
            side_w,
            center_w,
            side_nums,
        }
    }
}

/// Paint the three-column conflict view. The editable merged center is flanked
/// by the ours and theirs sides, aligned per
/// [`crate::merge_view::MergeDoc::align`].
pub(crate) fn render_conflict_view(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    stoatty: bool,
) {
    let cols = ConflictColumns::compute(inner);
    let snapshot = editor.display_map.snapshot();
    let scroll_row = editor.scroll_row;

    if let Some(state) = editor.conflict_view.as_ref() {
        let chunk_center_rows = chunk_center_rows(&snapshot, &state.chunk_anchors);
        let chunk_states = chunk_states(&snapshot, &state.doc, &state.chunk_anchors, &state.picks);
        paint_conflict_rows(
            &snapshot,
            scroll_row,
            inner,
            &cols,
            &state.doc,
            &chunk_center_rows,
            &chunk_states,
            fallback_style,
            theme,
            buf,
        );
    }

    render_review_cursor(
        editor,
        &snapshot,
        inner,
        cols.center_text_x,
        theme,
        buf,
        stoatty,
    );
}

/// Resolve each chunk's anchors to the row span it currently occupies in the
/// center buffer.
///
/// A pick reassembles a chunk's marker block into its chosen resolution, which
/// is usually fewer rows, so the span shrinks. Sizing the side band to this live
/// span rather than the original marker height keeps the ours and theirs columns
/// aligned after a pick. Feeds [`crate::merge_view::MergeDoc::align`].
fn chunk_center_rows(snapshot: &DisplaySnapshot, chunk_anchors: &[(Anchor, Anchor)]) -> Vec<usize> {
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    chunk_anchors
        .iter()
        .map(|(start, end)| {
            let start_row = rope
                .offset_to_point(buffer_snapshot.resolve_anchor(start))
                .row;
            let end_row = rope
                .offset_to_point(buffer_snapshot.resolve_anchor(end))
                .row;
            end_row.saturating_sub(start_row) as usize
        })
        .collect()
}

/// Classify each chunk against its live center-region text so the gutter glyph
/// and unresolved wash follow picks and hand edits.
///
/// The state is derived, never stored, so a buffer edit or undo can never leave
/// the glyph disagreeing with the text. Feeds [`state_glyph`].
fn chunk_states(
    snapshot: &DisplaySnapshot,
    doc: &MergeDoc,
    chunk_anchors: &[(Anchor, Anchor)],
    picks: &[Vec<RowPick>],
) -> Vec<ChunkState> {
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    chunk_anchors
        .iter()
        .enumerate()
        .map(|(i, (start, end))| {
            let start = buffer_snapshot.resolve_anchor(start);
            let end = buffer_snapshot.resolve_anchor(end);
            let region_text: String = rope.chunks_in_range(start..end).collect();
            doc.chunks[i].classify(&doc.rows, &picks[i], &region_text)
        })
        .collect()
}

/// The one-cell gutter marker for a chunk's resolution state.
fn state_glyph(state: ChunkState) -> char {
    match state {
        ChunkState::Unresolved => '?',
        ChunkState::Ours => 'O',
        ChunkState::Theirs => 'T',
        ChunkState::Both => 'B',
        ChunkState::Picked => '~',
        ChunkState::AutoIndent => 'I',
        ChunkState::Manual => 'M',
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_conflict_rows(
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    inner: Rect,
    cols: &ConflictColumns,
    doc: &MergeDoc,
    chunk_center_rows: &[usize],
    chunk_states: &[ChunkState],
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    use crate::theme::scope as s;
    let dim = theme.get(s::UI_TEXT_MUTED);
    let header_style = theme.get(s::VCS_CONFLICT_HEADER);
    let ours_style = theme.get(s::VCS_CONFLICT_OURS);
    let theirs_style = theme.get(s::VCS_CONFLICT_THEIRS);
    let inlay_style = fallback_style.patch(theme.get(s::UI_VIRTUAL_INLAY));

    let unresolved_tint = {
        let bg = style_rgb(theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg));
        let header = style_rgb(theme.get(s::VCS_CONFLICT_HEADER).fg);
        match (bg, header) {
            (Some(bg), Some(header)) => Some(dim_rgb(header, bg, BAND_TINT)),
            _ => None,
        }
    };

    let plan = doc.align(chunk_center_rows);
    let total = snapshot.line_count();
    let end_row = (scroll_row + inner.height as u32).min(total);
    if end_row <= scroll_row {
        return;
    }
    let endpoints = snapshot.highlighted_endpoints(scroll_row..end_row);

    for display_row in scroll_row..end_row {
        let y = inner.y + (display_row - scroll_row) as u16;

        buf[(cols.sep1_x, y)].set_char('│').set_style(dim);
        buf[(cols.sep2_x, y)].set_char('│').set_style(dim);

        let plan_row = plan.get(display_row as usize);

        if let Some(chunk_idx) = plan_row.and_then(|r| r.chunk) {
            let state = chunk_states[chunk_idx];
            buf[(cols.center_gutter_x, y)]
                .set_char(state_glyph(state))
                .set_style(header_style);
            if state == ChunkState::Unresolved
                && let Some(tint) = unresolved_tint
            {
                fill_line_tint(buf, cols.center_text_x, y, cols.center_w, tint);
            }
        }

        match snapshot.classify_row(display_row) {
            BlockRowKind::BufferRow { buffer_row } => {
                render_side_num(buf, cols.center_num_x, y, buffer_row + 1, dim);
                paint_highlighted_row(
                    snapshot,
                    display_row,
                    cols.center_text_x,
                    y,
                    cols.center_w,
                    buf,
                    fallback_style,
                    inlay_style,
                    &[],
                    None,
                    None,
                    &endpoints,
                );
            },
            BlockRowKind::Block { .. } => {
                render_empty_num(buf, cols.center_num_x, y, dim);
            },
        }

        if let Some(row) = plan_row {
            paint_side(
                buf,
                cols.ours_num_x,
                cols.ours_text_x,
                y,
                row.ours,
                cols.side_w,
                cols.side_nums,
                fallback_style,
                ours_style,
                dim,
            );
            paint_side(
                buf,
                cols.theirs_num_x,
                cols.theirs_text_x,
                y,
                row.theirs,
                cols.side_w,
                cols.side_nums,
                fallback_style,
                theirs_style,
                dim,
            );
        }
    }
}

/// Paint one side column of a merge row. A present side renders a muted line
/// number (when the column keeps its gutter) and its text with change-span
/// highlights. An absent side renders a placeholder gutter for a deletion or
/// one-sided line.
#[allow(clippy::too_many_arguments)]
fn paint_side(
    buf: &mut Buffer,
    num_x: u16,
    text_x: u16,
    y: u16,
    side: Option<&ReviewSide>,
    text_w: usize,
    side_nums: bool,
    base_style: Style,
    highlight_style: Style,
    dim: Style,
) {
    match side {
        Some(side) => {
            if side_nums {
                render_side_num(buf, num_x, y, side.line_num, dim);
            }
            render_side_text(
                buf,
                text_x,
                y,
                &side.text,
                text_w,
                base_style,
                &side.change_spans,
                highlight_style,
                &side.moved_spans,
                base_style,
            );
        },
        None => {
            if side_nums {
                render_empty_num(buf, num_x, y, dim);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::ConflictColumns;
    use ratatui::layout::Rect;

    #[test]
    fn wide_layout_lays_out_three_columns_with_side_gutters() {
        let cols = ConflictColumns::compute(Rect::new(0, 0, 150, 40));
        assert!(cols.side_nums, "side gutters kept at 150 cols");
        // col_w = (150 - 2) / 3 = 49.
        assert_eq!((cols.ours_num_x, cols.ours_text_x), (0, 5));
        assert_eq!(cols.sep1_x, 49);
        assert_eq!(
            (cols.center_gutter_x, cols.center_num_x, cols.center_text_x),
            (50, 51, 56)
        );
        assert_eq!(cols.sep2_x, 99);
        assert_eq!((cols.theirs_num_x, cols.theirs_text_x), (100, 105));
        assert_eq!((cols.side_w, cols.center_w), (44, 43));
    }

    #[test]
    fn narrow_layout_drops_side_gutters() {
        let cols = ConflictColumns::compute(Rect::new(0, 0, 90, 40));
        assert!(!cols.side_nums, "side gutters dropped below 120 cols");
        assert_eq!(cols.ours_num_x, cols.ours_text_x, "no ours gutter");
        assert_eq!(cols.theirs_num_x, cols.theirs_text_x, "no theirs gutter");
        // col_w = (90 - 2) / 3 = 29; center keeps its 1 + 5 gutter.
        assert_eq!((cols.side_w, cols.center_w), (29, 23));
    }
}
