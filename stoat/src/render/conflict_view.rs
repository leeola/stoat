use crate::{
    conflict_session::ConflictDerived,
    display_map::{CachedHighlightEndpoints, DisplaySnapshot},
    editor_state::EditorState,
    merge_view::{AlignRow, ChunkState, MergeDoc},
    render::{
        paint::{
            dim_rgb, fill_line_tint, render_empty_num, render_side_num, render_side_text, style_rgb,
        },
        review::{paint_highlighted_row, render_review_cursor},
    },
    review::ReviewSide,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use std::sync::Arc;

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
pub(crate) struct ConflictColumns {
    ours_num_x: u16,
    ours_text_x: u16,
    center_gutter_x: u16,
    center_num_x: u16,
    /// Absolute x of the center column's editable text, where a click maps to a
    /// center-buffer offset.
    pub(crate) center_text_x: u16,
    theirs_num_x: u16,
    theirs_text_x: u16,
    sep1_x: u16,
    /// Absolute x of the rule after the center column, the exclusive end of the
    /// center text region.
    pub(crate) sep2_x: u16,
    side_w: usize,
    center_w: usize,
    side_nums: bool,
}

impl ConflictColumns {
    pub(crate) fn compute(inner: Rect) -> Self {
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

    // Held aside for the paint, since the view state below borrows the editor
    // and a sibling field cannot be borrowed alongside it.
    let mut endpoint_cache = editor.highlight_endpoint_cache.take();
    let mut row_cache = editor.diff_row_cache.take();
    if let Some(state) = editor.conflict_view.as_mut() {
        let (doc, derived) = Arc::make_mut(state).derived(&snapshot);
        render_conflict_rows(
            &snapshot,
            doc,
            derived,
            scroll_row,
            inner,
            fallback_style,
            theme,
            buf,
            Some(&mut endpoint_cache),
            Some(&mut row_cache),
        );
    }
    editor.highlight_endpoint_cache = endpoint_cache;
    editor.diff_row_cache = row_cache;

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
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_conflict_rows(
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
) {
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
                );
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
                    None,
                    &mut row_cursor,
                );
            },
            crate::render::review::DiffRowKind::Block => {
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
                render_side_num(buf, num_text, num_x, y, side.line_num, dim);
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
    use crate::test_harness::TestHarness;
    use ratatui::layout::Rect;
    use std::{path::PathBuf, sync::Arc};

    /// Open the conflict view on one three-stage file in a `width`-wide harness.
    fn conflict_harness(width: u16, ancestor: &str, ours: &str, theirs: &str) -> TestHarness {
        let mut h = TestHarness::with_size(width, 24);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .conflicted_file("f.txt", Some(ancestor), Some(ours), Some(theirs));
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Conflict);
        h.settle();
        h
    }

    /// Split each rendered row on the two column rules into its trimmed ours,
    /// center, and theirs text.
    ///
    /// Reading each column on its own is what makes these tests meaningful. A
    /// whole-frame substring check passes on a painting center even when both
    /// sides are blank, which is exactly the failure being pinned. Rows are read
    /// off the rules rather than off [`ConflictColumns`] so the assertions
    /// describe what reached the screen, not what the geometry intended.
    fn split_columns(frame: &str) -> Vec<(String, String, String)> {
        frame
            .lines()
            .map(|line| {
                let mut parts = line.split('│');
                let ours = parts.next().unwrap_or_default().trim().to_string();
                let center = parts.next().unwrap_or_default().trim().to_string();
                let theirs = parts.next().unwrap_or_default().trim().to_string();
                (ours, center, theirs)
            })
            .collect()
    }

    /// The focused editor's conflict state paired with a snapshot of its center
    /// buffer, which is what `derived` is keyed on.
    fn view_and_snapshot(
        h: &mut TestHarness,
    ) -> (
        crate::conflict_session::ConflictViewState,
        crate::display_map::DisplaySnapshot,
    ) {
        let editor =
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("center editor");
        let view = editor.conflict_view.clone().expect("conflict view");
        (
            Arc::try_unwrap(view).unwrap_or_else(|shared| (*shared).clone()),
            editor.display_map.snapshot(),
        )
    }

    /// A rebuild allocates a fresh plan, so a stable pointer is proof the cached
    /// one was handed back rather than recomputed.
    fn plan_ptr(
        state: &mut crate::conflict_session::ConflictViewState,
        snap: &crate::display_map::DisplaySnapshot,
    ) -> *const crate::merge_view::AlignRow {
        state.derived(snap).1.plan.as_ptr()
    }

    #[test]
    fn derived_layout_is_reused_while_the_buffer_holds() {
        let mut h = conflict_harness(150, "a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n");
        let (mut state, snap) = view_and_snapshot(&mut h);

        let first = plan_ptr(&mut state, &snap);
        assert_eq!(
            plan_ptr(&mut state, &snap),
            first,
            "a second paint against the same snapshot reuses the derived layout",
        );
    }

    #[test]
    fn an_edit_to_the_center_forces_a_recompute() {
        let mut h = conflict_harness(150, "a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n");
        let (mut state, before) = view_and_snapshot(&mut h);
        let stale = plan_ptr(&mut state, &before);

        h.type_keys("i");
        h.type_text("edited");
        h.type_keys("escape");

        let (_, after) = view_and_snapshot(&mut h);
        assert_ne!(
            plan_ptr(&mut state, &after),
            stale,
            "a changed buffer version rebuilds rather than reusing",
        );

        let mut scratch = state.clone();
        scratch.invalidate_derived();
        assert_eq!(
            state.derived(&after).1.states,
            scratch.derived(&after).1.states,
            "the rebuilt states match a from-scratch classification",
        );
    }

    #[test]
    fn a_pick_forces_a_recompute() {
        let mut h = conflict_harness(150, "a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n");
        let (mut state, snap) = view_and_snapshot(&mut h);
        let before = state.derived(&snap).1.states.clone();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ConflictPickOurs);
        h.settle();

        let (mut picked, after) = view_and_snapshot(&mut h);
        assert_ne!(
            picked.derived(&after).1.states,
            before,
            "the pick's new chunk state reaches the painter rather than a stale one",
        );
    }

    #[test]
    fn side_columns_carry_the_ours_and_theirs_content() {
        let mut h = conflict_harness(150, "a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n");
        let rows = split_columns(&h.snapshot().content);

        assert_eq!(
            rows[0],
            ("1 a".into(), "1 a".into(), "1 a".into()),
            "context above the conflict reads across all three columns, each \
             numbered from its own stage",
        );
        assert_eq!(
            (rows[1].0.as_str(), rows[1].2.as_str()),
            ("2 OURS", "2 THEIRS"),
            "each side's stage content sits on the band's first row",
        );
        assert_eq!(
            rows[1].1, "?   2 <<<<<<< ours",
            "and lines up with the center's marker band",
        );
    }

    #[test]
    fn side_columns_stay_aligned_while_scrolled() {
        let stage = |edit: &str| -> String {
            (0..200)
                .map(|i| match i {
                    120 => format!("{edit}\n"),
                    _ => format!("line {i:03}\n"),
                })
                .collect()
        };
        let base: String = (0..200).map(|i| format!("line {i:03}\n")).collect();

        let mut h = conflict_harness(150, &base, &stage("OURS EDIT"), &stage("THEIRS EDIT"));
        // Park the marker band one row below the top so it renders clear of the
        // conflict hint box, which the view always overlays bottom-right.
        crate::action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .scroll_row = 119;

        let rows = split_columns(&h.snapshot().content);

        assert_eq!(
            (rows[0].0.as_str(), rows[0].2.as_str()),
            ("120 line 119", "120 line 119"),
            "the sides start at the scrolled row, so the align plan is indexed \
             by absolute display row rather than by screen row",
        );
        assert_eq!(
            (rows[1].0.as_str(), rows[1].2.as_str()),
            ("121 OURS EDIT", "121 THEIRS EDIT"),
            "and the scrolled conflict band still carries both sides",
        );
    }

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
