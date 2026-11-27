use crate::{
    action_handlers::view,
    conflict_session::{ConflictDerived, ConflictFrame},
    display_map::{CachedHighlightEndpoints, DisplayPoint, DisplaySnapshot},
    editor_state::EditorState,
    merge_view::{AlignRow, ChunkState, MergeDoc, Side},
    render::{
        editor::wrap_mark_style,
        paint::{
            dim_rgb, fill_line_tint, render_empty_num, render_side_num, render_side_text, style_rgb,
        },
        review::{paint_highlighted_row, DiffDials},
        TypeScale,
    },
    review::ReviewSide,
    theme::TypeRole,
};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::Style,
};
use std::{ops::Range, sync::Arc};
use stoat_action::ActionKind;
use stoat_text::cursor_offset;
use stoat_widgets::ApcScene;

pub mod grid;

/// At or above this inner width the ours and theirs columns keep a line-number
/// gutter. Below it they drop it so their text has room to read.
const CONFLICT_SIDE_NUM_MIN: u16 = 120;

/// Blend fraction toward the background for an unresolved chunk's tint wash.
const BAND_TINT: f32 = 0.85;

/// A MergeGrid paint's scene, with what its rows draw beyond the document.
pub struct GridPaint<'a> {
    pub scene: &'a mut ApcScene,
    /// The display row the cursor sits on, whose merged number the grid lifts.
    /// `None` for a pool page, which never carries the cursor.
    pub cursor_row: Option<u32>,
    /// The chunk whose chip shows its pick buttons, as the view state holds
    /// it.
    pub chip_menu: Option<usize>,
    /// Where the paint records the cells of the chips and the pick buttons it
    /// draws, so a press finds them. `None` for a pool page, which no press
    /// reads.
    pub layout: Option<&'a mut ConflictLayout>,
    /// The side the pointer rests over, which the rows ring. `None` for a
    /// pool page, since a glide shows no hover.
    pub hover: Option<ConflictHover>,
}

/// Where the MergeGrid's columns, rows, and chips fell in the frame that
/// painted it, in the frame's absolute cells.
///
/// The paint records it on the editor, so a reader outside the painter places
/// itself against the columns the frame drew rather than laying out columns
/// of its own. The status row splits its runs at the merged column's edges,
/// and a press finds the chip or the pick button under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictLayout {
    pub ours: Range<u16>,
    pub merged: Range<u16>,
    pub theirs: Range<u16>,
    /// The rows under the header, across all three columns.
    pub body: Rect,
    /// The cells of each chunk's chip on screen, with the chunk it names.
    pub chips: Vec<(Rect, usize)>,
    /// The cells of each pick button of the open chip menu, with the pick it
    /// takes.
    pub buttons: Vec<(Rect, ActionKind)>,
}

/// What a press on the conflict view's MergeGrid lands on, as
/// [`conflict_target`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConflictTarget {
    /// A pick button of the open chip menu, with the pick it takes.
    Button(ActionKind),
    /// A chunk's chip, with the state the chunk stands at.
    Chip { chunk: usize, state: ChunkState },
    /// The ours or theirs side of a chunk that still takes picks.
    Side {
        chunk: usize,
        side: Side,
        /// The merge row of the side's line under the press, or `None` on a
        /// row of the chunk's band past that side's lines.
        merge_row: Option<usize>,
    },
}

/// The open chunk's side the pointer rests over, which the MergeGrid rings and
/// names the pick of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ConflictHover {
    pub chunk: usize,
    pub side: Side,
    /// The merge row of the one line an Alt press takes, set while Alt is
    /// held. `None` rings the whole side, which a plain press takes.
    pub merge_row: Option<usize>,
}

/// Column geometry for the conflict view's cell layout, the one a terminal with
/// no live scene gets, resolved once per frame.
///
/// Three equal columns (ours, center, theirs) separated by two one-cell rules.
/// The center always keeps a one-cell state gutter, a five-cell number gutter,
/// and its text. The side columns keep a five-cell number gutter only at or
/// above [`CONFLICT_SIDE_NUM_MIN`]. Below it they drop it.
pub struct ConflictColumns {
    ours_num_x: u16,
    ours_text_x: u16,
    center_gutter_x: u16,
    center_num_x: u16,
    /// Absolute x of the center column's editable text.
    pub center_text_x: u16,
    theirs_num_x: u16,
    theirs_text_x: u16,
    sep1_x: u16,
    /// Absolute x of the rule after the center column, the exclusive end of the
    /// center text region.
    pub sep2_x: u16,
    side_w: usize,
    center_w: usize,
    side_nums: bool,
}

impl ConflictColumns {
    pub fn compute(inner: Rect) -> Self {
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
///
/// Under a live `scene` the view paints as the MergeGrid, with a header row
/// above the columns, when the theme gives the grid RGB colors. Otherwise it
/// keeps the cell layout, its rules and state gutter drawn in cells.
///
/// `delegates_cursor` hands the cursor to the terminal rather than painting it,
/// which only a live scene does.
pub fn render_conflict_view(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
    delegates_cursor: bool,
) {
    let snapshot = editor.display_map.snapshot();
    let delegates_cursor = delegates_cursor && scene.is_some();
    // The grid needs a row under its header to draw anything.
    let live = match (scene, editor.conflict_view.as_deref()) {
        (Some(scene), Some(state)) if inner.height > grid::HEADER_ROWS => {
            grid::Grid::resolve(inner, &snapshot, &state.doc, theme).map(|grid| (scene, grid))
        },
        _ => None,
    };

    let (body, text) = match &live {
        Some((_, grid)) => (
            Rect {
                y: inner.y + grid::HEADER_ROWS,
                height: inner.height.saturating_sub(grid::HEADER_ROWS),
                ..inner
            },
            grid.cols.merged.text_x..grid.cols.merged.right(),
        ),
        None => {
            let cols = ConflictColumns::compute(inner);
            (inner, cols.center_text_x..cols.sep2_x)
        },
    };
    editor.viewport_rows = Some(u32::from(body.height));
    view::apply_held_screen_row(editor);
    editor.text_rect = Some(Rect {
        x: text.start,
        y: body.y,
        width: text.end.saturating_sub(text.start),
        height: body.height,
    });
    let mut layout = live.as_ref().map(|(_, grid)| ConflictLayout {
        ours: grid.cols.ours.x..grid.cols.ours.right(),
        merged: grid.cols.merged.x..grid.cols.merged.right(),
        theirs: grid.cols.theirs.x..grid.cols.theirs.right(),
        body,
        chips: Vec::new(),
        buttons: Vec::new(),
    });
    let scroll_row = editor.scroll_row;
    let cursor = cursor_display(editor, &snapshot);

    // Held aside for the paint, since the view state below borrows the editor
    // and a sibling field cannot be borrowed alongside it.
    let mut endpoint_cache = editor.highlight_endpoint_cache.take();
    let mut row_cache = editor.diff_row_cache.take();
    let ConflictFrame { position, hover } = editor.conflict_frame;
    if let Some(state) = editor.conflict_view.as_mut() {
        let state = Arc::make_mut(state);
        let chip_menu = state.chip_menu;
        let paint = live.map(|(scene, grid)| {
            grid::paint_header(
                Rect { height: 1, ..inner },
                &grid,
                &state.labels,
                theme.type_scale(TypeRole::Ui),
                theme.type_scale(TypeRole::Caption),
                buf,
                scene,
            );
            grid::paint_file_header(
                Rect {
                    y: inner.y + 1,
                    height: 1,
                    ..inner
                },
                &grid,
                grid::FileFacts {
                    path: &state.rel_path,
                    position,
                    counts: state.line_counts,
                },
                theme,
                buf,
                scene,
            );
            GridPaint {
                scene,
                cursor_row: Some(cursor.row),
                chip_menu,
                layout: layout.as_mut(),
                hover,
            }
        });
        let (doc, derived) = state.derived(&snapshot);
        render_conflict_rows(
            &snapshot,
            doc,
            derived,
            scroll_row,
            body,
            fallback_style,
            theme,
            buf,
            Some(&mut endpoint_cache),
            Some(&mut row_cache),
            paint,
        );
    }
    editor.conflict_layout = layout;
    editor.highlight_endpoint_cache = endpoint_cache;
    editor.diff_row_cache = row_cache;

    render_conflict_cursor(editor, cursor, body, text, theme, buf, delegates_cursor);
}

/// What a press on cell `column`, `row` of the frame lands on in the conflict
/// view that `editor` shows, or `None` for a press the editor's own pointer
/// path keeps.
///
/// A press on a pick button of the open chip menu is a
/// [`ConflictTarget::Button`], and a press on a chunk's chip is a
/// [`ConflictTarget::Chip`] whatever the chunk's state. A press in the ours or
/// theirs column of a chunk that still shows its markers or a per-row mix is
/// a [`ConflictTarget::Side`]. The cells are the ones the last paint recorded,
/// so a frame with no MergeGrid has no target. A chunk that a whole-side pick,
/// an auto-resolution, or a hand edit settled is no side target, so a press on
/// its side places the cursor as on any row.
pub(crate) fn conflict_target(
    editor: &mut EditorState,
    column: u16,
    row: u16,
) -> Option<ConflictTarget> {
    let at = Position::new(column, row);
    let (body, chip, side) = {
        let layout = editor.conflict_layout.as_ref()?;
        if let Some(&(_, kind)) = layout.buttons.iter().find(|(cells, _)| cells.contains(at)) {
            return Some(ConflictTarget::Button(kind));
        }
        let chip = layout
            .chips
            .iter()
            .find(|(cells, _)| cells.contains(at))
            .map(|&(_, chunk)| chunk);
        let side = if layout.ours.contains(&column) {
            Some(Side::Ours)
        } else if layout.theirs.contains(&column) {
            Some(Side::Theirs)
        } else {
            None
        };
        (layout.body, chip, side)
    };
    if !body.contains(at) || (chip.is_none() && side.is_none()) {
        return None;
    }
    // At an unaligned rest the composite draws a row in the cell its offset
    // rounds to, as the editor's own press path reads it.
    let display_row =
        editor.scroll_row + u32::from(editor.scroll_frac >= 0.5) + u32::from(row - body.y);

    let snapshot = editor.display_map.snapshot();
    let state = Arc::make_mut(editor.conflict_view.as_mut()?);
    let (_, derived) = state.derived(&snapshot);
    if let Some(chunk) = chip {
        let state = *derived.states.get(chunk)?;
        return Some(ConflictTarget::Chip { chunk, state });
    }

    let side = side?;
    let align = derived.plan.get(display_row as usize)?;
    let chunk = align.chunk?;
    if !matches!(
        derived.states.get(chunk)?,
        ChunkState::Unresolved | ChunkState::Picked
    ) {
        return None;
    }

    let merge_row = match side {
        Side::Ours => align.ours,
        Side::Theirs => align.theirs,
    };
    Some(ConflictTarget::Side {
        chunk,
        side,
        merge_row,
    })
}

/// The display cell of the primary selection's cursor in `snapshot`.
fn cursor_display(editor: &EditorState, snapshot: &DisplaySnapshot) -> DisplayPoint {
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let sel = editor.selections.newest_anchor();
    let cursor = cursor_offset(
        rope,
        buffer_snapshot.resolve_anchor(&sel.tail()),
        buffer_snapshot.resolve_anchor(&sel.head()),
    );
    snapshot.buffer_to_display(rope.offset_to_point(cursor))
}

/// Paint the cursor at `display` over the merged center's text, or set the
/// stoatty hardware cursor there. Skips a row scrolled out of `body`.
///
/// One cell for the newest cursor, and no selection wash as a plain pane paints,
/// because the reader picks chunks in the center column rather than selecting
/// text there.
///
/// `text` is the center's text columns. The center has no horizontal scroll, so
/// a cursor past its last column is hidden, as a row out of view is, rather
/// than clamped onto a cell it is not on.
fn render_conflict_cursor(
    editor: &mut EditorState,
    display: DisplayPoint,
    body: Rect,
    text: Range<u16>,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    stoatty: bool,
) {
    let cursor_style = theme.cursor_style();

    let visible = body.height as u32;
    if display.row < editor.scroll_row || display.row >= editor.scroll_row + visible {
        return;
    }
    let y = body.y + (display.row - editor.scroll_row) as u16;
    let x = u32::from(text.start) + display.column;
    if x >= u32::from(text.end) || y >= body.y + body.height {
        return;
    }
    let x = x as u16;

    if stoatty {
        editor.cursor_screen_cell = Some((x, y));
    } else {
        let cell = &mut buf[(x, y)];
        let existing = cell.symbol().chars().next().unwrap_or(' ');
        cell.set_char(if existing == '\0' { ' ' } else { existing });
        cell.set_style(cursor_style);
    }
}

/// Paint the three-column body -- ours, the merged center, and theirs -- for the
/// `inner`-sized window starting at display row `scroll_row`.
///
/// The single seam the live grid and the pooled smooth-scroll pages share, so a
/// glide cannot drift from the frame it settles into. Everything downstream of
/// the snapshot is derived here rather than passed in, which lets a pooled page
/// render off the run loop from an owned [`ConflictViewState`] clone.
///
/// Paints rows only. The cursor belongs to the live grid alone, since a pooled
/// page never carries one.
///
/// `endpoint_cache` is the editor's, when this paint has one behind it. A pooled
/// page has no editor at all and passes `None`, resolving its endpoints fresh.
///
/// Under `paint` the rows paint as the MergeGrid, with their sub-cell marks in
/// its scene. With no `paint`, or a theme that gives the grid no RGB surface,
/// they keep the cell layout.
#[allow(clippy::too_many_arguments)]
pub fn render_conflict_rows(
    snapshot: &DisplaySnapshot,
    doc: &MergeDoc,
    derived: &ConflictDerived,
    scroll_row: u32,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    endpoint_cache: Option<&mut Option<CachedHighlightEndpoints>>,
    row_cache: Option<&mut Option<crate::render::review::DiffRowCache>>,
    paint: Option<GridPaint<'_>>,
) {
    if let Some(paint) = paint
        && let Some(grid) = grid::Grid::resolve(inner, snapshot, doc, theme)
    {
        grid::paint_rows(
            snapshot,
            scroll_row,
            inner,
            &grid,
            doc,
            derived,
            fallback_style,
            theme,
            buf,
            endpoint_cache,
            row_cache,
            paint,
        );
        return;
    }

    let cols = ConflictColumns::compute(inner);
    paint_conflict_rows(
        snapshot,
        scroll_row,
        inner,
        &cols,
        doc,
        &derived.states,
        &derived.plan,
        fallback_style,
        theme,
        buf,
        endpoint_cache,
        row_cache,
    );
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
    chunk_states: &[ChunkState],
    plan: &[AlignRow],
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    endpoint_cache: Option<&mut Option<CachedHighlightEndpoints>>,
    row_cache: Option<&mut Option<crate::render::review::DiffRowCache>>,
) {
    use crate::theme::scope as s;
    let dim = theme.get(s::UI_TEXT_MUTED);
    let header_style = theme.get(s::VCS_CONFLICT_HEADER);
    let ours_style = theme.get(s::VCS_CONFLICT_OURS);
    let theirs_style = theme.get(s::VCS_CONFLICT_THEIRS);
    let inlay_style = fallback_style.patch(theme.get(s::UI_VIRTUAL_INLAY));
    let wrap_style = fallback_style.patch(wrap_mark_style(theme));

    let unresolved_tint = {
        let bg = style_rgb(theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg));
        let header = style_rgb(theme.get(s::VCS_CONFLICT_HEADER).fg);
        match (bg, header) {
            (Some(bg), Some(header)) => Some(dim_rgb(header, bg, BAND_TINT)),
            _ => None,
        }
    };

    let total = snapshot.line_count();
    let end_row = (scroll_row + inner.height as u32).min(total);
    if end_row <= scroll_row {
        return;
    }
    let endpoints = match endpoint_cache {
        Some(cache) => snapshot.highlighted_endpoints_cached(scroll_row..end_row, cache),
        None => snapshot.highlighted_endpoints(scroll_row..end_row),
    };
    // One replay for the loop, since the rows ascend and each opens its own
    // stream around the separators and numbers painted between them.
    let mut row_cursor = snapshot.row_highlight_cursor(endpoints);
    let mut local_row_cache = None;
    let row_states = crate::render::review::diff_row_states(
        snapshot,
        scroll_row..end_row,
        row_cache.unwrap_or(&mut local_row_cache),
    );

    // One buffer for every number this loop paints, rather than one per row.
    let mut num_text = String::new();

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

        match row_states[(display_row - scroll_row) as usize].kind {
            crate::render::review::DiffRowKind::BufferRow { buffer_row } => {
                render_side_num(
                    buf,
                    &mut num_text,
                    cols.center_num_x,
                    y,
                    buffer_row + 1,
                    dim,
                    4,
                );
                paint_highlighted_row(
                    snapshot,
                    display_row,
                    cols.center_text_x,
                    y,
                    cols.center_w,
                    buf,
                    fallback_style,
                    fallback_style,
                    fallback_style,
                    inlay_style,
                    wrap_style,
                    &[],
                    // No change span reaches a conflict row, so the span mark
                    // and the tint that rides it never engage.
                    None,
                    None,
                    None,
                    // This row softens and tints nothing, so the dials it takes
                    // never apply.
                    DiffDials::shipped(),
                    None,
                    &mut row_cursor,
                );
            },
            crate::render::review::DiffRowKind::Block { .. } => {
                render_empty_num(buf, cols.center_num_x, y, dim);
            },
        }

        if let Some(row) = plan_row {
            paint_side(
                buf,
                &mut num_text,
                cols.ours_num_x,
                cols.ours_text_x,
                y,
                row.ours.and_then(|i| doc.rows[i].ours.as_ref()),
                cols.side_w,
                cols.side_nums,
                fallback_style,
                ours_style,
                dim,
            );
            paint_side(
                buf,
                &mut num_text,
                cols.theirs_num_x,
                cols.theirs_text_x,
                y,
                row.theirs.and_then(|i| doc.rows[i].theirs.as_ref()),
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
    num_text: &mut String,
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
                render_side_num(buf, num_text, num_x, y, side.line_num, dim, 4);
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
