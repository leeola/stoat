use super::{
    paint::{
        dim_rgb, paint_style_runs, render_empty_num, render_side_num, render_side_text, style_rgb,
    },
    TEXT_SCALE_COMPACT,
};
use crate::{
    diff_map::{ChangeKind, DiffHunk, DiffHunkStatus},
    display_map::{
        highlights::HighlightStyle, BlockRowKind, CachedHighlightEndpoints, DisplaySnapshot,
        RowHighlightCursor,
    },
    editor_state::EditorState,
    host::DiffStatus,
    review::ReviewRow,
    review_session::{ChunkStatus, ReviewViewState},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::StatefulWidget,
};
use std::{
    fmt::Write,
    hash::{DefaultHasher, Hash, Hasher},
};
use stoat_text::{cursor_offset, Point};
use stoat_widgets::{
    bar::Bar,
    text_run::{self, TextRun},
    ApcScene,
};

/// Fraction an unchanged row's token foregrounds blend toward the editor
/// background, leaving 60% of the syntax color.
///
/// Context receding is what makes a changed row stand out, the changed row
/// itself painting its syntax colors at full strength. Nothing washes a whole
/// line, so the palette carries the emphasis and the eye lands on the rows
/// still at full strength.
pub(crate) const CONTEXT_SOFTEN: f32 = 0.40;

/// Fraction the unchanged chars of a token-refined row blend toward the editor
/// background, leaving 75% of the syntax color.
///
/// Lighter than [`CONTEXT_SOFTEN`] on purpose. The row is changed content, so
/// it must still read as ahead of the context around it, while the chars the
/// refinement marks pop hardest inside their own line.
pub(crate) const MODIFIED_ROW_SOFTEN: f32 = 0.25;

/// Furthest the diff view's soften may be turned down. Lands the scale on zero,
/// which disables softening outright and restores the paint from before it.
pub(crate) const DIFF_SOFTEN_MIN: i8 = -4;

/// Furthest the soften turns up, landing the context blend on [`SOFTEN_CAP`].
///
/// The largest level that still moves a context row. One step further scales
/// past the cap, where the trim takes the excess back and the row paints
/// exactly as it did a step earlier.
pub(crate) const DIFF_SOFTEN_MAX: i8 = 6;

/// Ceiling on a scaled soften fraction, leaving a twentieth of the syntax color.
///
/// Where the top level lands, and the readability floor the dial stops at. A
/// fraction of 1.0 blends a foreground into the background exactly, which
/// leaves text nobody reads.
const SOFTEN_CAP: f32 = 0.95;

/// Multiplier `level` applies to [`CONTEXT_SOFTEN`] and [`MODIFIED_ROW_SOFTEN`].
///
/// Level 0 returns 1.0, so an untouched session paints the shipped fractions
/// exactly. Each step is a quarter, so four steps down turn softening off and
/// six steps up reach the readability floor. The ends sit at different
/// distances because the scale starts at 1.0, which is nearer to zero than to
/// the 2.5 the floor needs.
///
/// A caller must skip [`soften_style`] entirely on a zero scale rather than pass
/// it a zero amount, because that call drops bold whatever amount it is given.
pub(crate) fn diff_soften_scale(level: i8) -> f32 {
    1.0 + 0.25 * f32::from(level.clamp(DIFF_SOFTEN_MIN, DIFF_SOFTEN_MAX))
}

pub(crate) fn render_review(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
) {
    let snapshot = editor.display_map.snapshot();
    let stoatty = scene.is_some();
    let Some(view) = editor.review_view.as_ref() else {
        return;
    };
    if view.rows.is_empty() {
        render_review_empty(view.watching, inner, theme, buf);
        return;
    }
    render_review_rows(
        &snapshot,
        view,
        editor.scroll_row,
        inner,
        fallback_style,
        theme,
        buf,
        scene,
    );
    render_review_cursor(
        editor,
        &snapshot,
        inner,
        review_cursor_text_x(inner),
        theme,
        buf,
        stoatty,
    );
}

/// Paint an editor as a side-by-side diff, with base (HEAD) text on the left and
/// the live syntax-highlighted buffer on the right, row-aligned through the
/// display map's deleted-block splicing.
///
/// The right column runs the same highlighted pipeline as a plain editor, so the
/// buffer stays fully editable and colored. The left column shows removed and
/// modified base lines (as spliced block rows) in the diff-deleted style and
/// mirrors unchanged lines dimmed. Added and modified new lines leave it blank.
/// Line numbers are base-file lines on the left and buffer lines on the right.
///
/// Lays its columns out per [`DiffLayout::DIFF_VIEW`]. When a `scene` is
/// threaded (a stoatty terminal) the gutter paints with the rich sub-cell
/// components, otherwise it falls back to the ASCII gutter.
pub(crate) fn render_diff_view(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
    soften_scale: f32,
) {
    let stoatty = scene.is_some();
    // The base rows pair with the live ones only where there are two columns to
    // pair across, so the width decides it, and it has to be settled before the
    // snapshot because it changes which blocks the display map splices.
    editor
        .display_map
        .set_pair_modified_hunks(inner.width >= DIFF_TWO_COLUMN_MIN);
    let snapshot = editor.display_map.snapshot();
    paint_diff_rows(
        &snapshot,
        editor.scroll_row,
        inner,
        fallback_style,
        theme,
        buf,
        scene,
        0.0,
        soften_scale,
        Some(&mut editor.highlight_endpoint_cache),
        Some(&mut editor.diff_row_cache),
    );
    render_review_cursor(
        editor,
        &snapshot,
        inner,
        right_text_x(inner),
        theme,
        buf,
        stoatty,
    );
}

/// Minimum inner width for the two-column diff layout. Below this each text
/// half falls under about 43 columns after its gutters, too cramped to read
/// code, so the view collapses to a single unified column.
const DIFF_TWO_COLUMN_MIN: u16 = 100;

/// What one display row is, without borrowing the snapshot it came from.
///
/// [`BlockRowKind`] holds a reference to the block a row belongs to, which is
/// why it cannot be kept across frames. Neither the diff body nor the conflict
/// body reads that block, so only the discriminant and the buffer row survive
/// here.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum DiffRowKind {
    Block,
    BufferRow { buffer_row: u32 },
}

/// One display row's derived state, as the diff and conflict bodies read it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct DiffRowState {
    pub(crate) kind: DiffRowKind,
    pub(crate) status: DiffStatus,
    pub(crate) staged: Option<bool>,
    /// Whether a base row sits beside this one in the left column.
    ///
    /// A modified hunk pairs base row i with live row i as far as its base text
    /// reaches, so its first rows carry one and its last do not. False for
    /// every other status: an unchanged row mirrors its own base line by a
    /// different route, and an added one has no base row at all.
    pub(crate) paired: bool,
    /// Display-column ranges to mark, each with its kind and its
    /// [`crate::diff_map::ChangeSpan::prose`] flag. Empty for a row no hunk
    /// refines.
    pub(crate) change_spans: Vec<(std::ops::Range<usize>, ChangeKind, bool)>,
}

/// The visible rows' derived state, held across repaints.
///
/// Every entry costs a block-tree descent to classify the row and a hunk-tree
/// seek for its status, its staged flag and its change spans, and the diff and
/// conflict bodies asked for all of that per row per frame. None of it moves
/// while the buffer, the display map, the diff and the painted window stand
/// still, which is the ordinary case between two frames.
pub(crate) struct DiffRowCache {
    key: u64,
    rows: Vec<DiffRowState>,
}

/// Hash what the cached rows are derived from.
///
/// A change to any of these misses and rebuilds. The theme, the tints and the
/// column geometry are deliberately absent, none of them reaching the state
/// being cached.
fn diff_row_key(
    scroll_row: u32,
    visible: u32,
    buffer_version: u64,
    map_version: usize,
    diff_version: usize,
    paired: bool,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    scroll_row.hash(&mut hasher);
    visible.hash(&mut hasher);
    buffer_version.hash(&mut hasher);
    map_version.hash(&mut hasher);
    diff_version.hash(&mut hasher);
    paired.hash(&mut hasher);
    hasher.finish()
}

/// The derived state of `rows`, rebuilding into `cache` only when its key moved.
///
/// The rows are borrowed from the cache rather than handed back by value, a
/// copy per frame being the cost this exists to avoid. A caller with no cache of
/// its own passes a local one, which builds once and drops with the paint.
pub(crate) fn diff_row_states<'c>(
    snapshot: &DisplaySnapshot,
    rows: std::ops::Range<u32>,
    cache: &'c mut Option<DiffRowCache>,
) -> &'c [DiffRowState] {
    let key = diff_row_key(
        rows.start,
        rows.end - rows.start,
        snapshot.buffer_snapshot().version(),
        snapshot.version(),
        snapshot.diff_map().map_or(0, |dm| dm.version()),
        snapshot.pairs_modified_hunks(),
    );
    if cache.as_ref().is_none_or(|c| c.key != key) {
        *cache = Some(DiffRowCache {
            key,
            rows: build_diff_row_states(snapshot, rows),
        });
    }
    &cache.as_ref().expect("filled just above").rows
}

fn build_diff_row_states(
    snapshot: &DisplaySnapshot,
    rows: std::ops::Range<u32>,
) -> Vec<DiffRowState> {
    let mut hunk_scratch: Vec<&DiffHunk> = Vec::new();
    rows.map(|display_row| {
        let kind = match snapshot.classify_row(display_row) {
            BlockRowKind::Block { .. } => DiffRowKind::Block,
            BlockRowKind::BufferRow { buffer_row } => DiffRowKind::BufferRow { buffer_row },
        };
        let DiffRowKind::BufferRow { buffer_row } = kind else {
            return DiffRowState {
                kind,
                status: DiffStatus::Unchanged,
                staged: None,
                paired: false,
                change_spans: Vec::new(),
            };
        };
        let mut change_spans = Vec::new();
        // A refined hunk narrows the change to the rows its runs name, so a row
        // inside one that names none has nothing to bar.
        let marked = write_buffer_row_change_spans(
            snapshot,
            buffer_row,
            &mut hunk_scratch,
            &mut change_spans,
        );
        let status = snapshot.line_diff_status(buffer_row);
        DiffRowState {
            kind,
            status,
            paired: status == DiffStatus::Modified
                && paired_with_base(snapshot, buffer_row, &mut hunk_scratch),
            staged: marked
                .then(|| {
                    snapshot
                        .diff_map()
                        .and_then(|dm| dm.staged_for_line(buffer_row))
                })
                .flatten(),
            change_spans,
        }
    })
    .collect()
}

/// Whether the modified row `buffer_row` has a base row beside it.
///
/// A modified hunk's base rows pair with its live rows one for one from the
/// hunk's start, so a row is paired while its offset into the hunk is inside the
/// base text the hunk removed. Past that the live side outruns the base and the
/// left column is blank; the base rows past the live side block after the hunk
/// instead.
fn paired_with_base<'a>(
    snapshot: &'a DisplaySnapshot,
    buffer_row: u32,
    hunk_scratch: &mut Vec<&'a DiffHunk>,
) -> bool {
    if !snapshot.pairs_modified_hunks() {
        return false;
    }
    let Some(dm) = snapshot.diff_map() else {
        return false;
    };
    dm.hunks_in_range_into(buffer_row..buffer_row + 1, hunk_scratch);
    hunk_scratch.iter().any(|hunk| {
        hunk.status == DiffHunkStatus::Modified
            && buffer_row.saturating_sub(hunk.buffer_start_line) < dm.base_line_count(hunk)
    })
}

/// One diff body's gutter shape and the narrowest rect it still splits in two.
///
/// The two bodies that draw a diff want different gutters. The diff view reads
/// as an editor with a base column bolted on, so it leads with the line number
/// and rules the gutter off from the code. The review screen and the commit
/// preview lead with the chunk-status glyph, because a reviewer scans statuses
/// down the left edge, and they run the text straight after the number to buy
/// back a column.
///
/// A layout exists so both bodies resolve their columns through
/// [`DiffColumns`]. Click mapping reads the same geometry the paint does (see
/// [`right_text_x`]), so a second formula misplaces clicks with nothing
/// failing.
#[derive(Clone, Copy)]
pub(crate) struct DiffLayout {
    num_w: u16,
    status_w: u16,
    /// Width of the rule between the gutter and the text. Zero for a layout
    /// that runs the text straight after the gutter.
    sep_w: u16,
    /// The status column precedes the line number rather than following it.
    status_first: bool,
    /// Inner width below which both sides alias onto one unified column. Zero
    /// for a layout that always splits.
    two_column_min: u16,
}

impl DiffLayout {
    /// The diff view's gutter: line number, change/staged status, then a rule.
    pub(crate) const DIFF_VIEW: Self = Self {
        num_w: 5,
        status_w: 2,
        sep_w: 1,
        status_first: false,
        two_column_min: DIFF_TWO_COLUMN_MIN,
    };

    /// The review screen's and commit preview's gutter: a one-cell chunk-status
    /// glyph, then the line number, then the text.
    pub(crate) const REVIEW: Self = Self {
        num_w: 5,
        status_w: 1,
        sep_w: 0,
        status_first: true,
        two_column_min: 0,
    };

    /// Offsets of the number, status, and rule columns from a side's start.
    fn offsets(&self) -> (u16, u16, Option<u16>) {
        let (num, status) = if self.status_first {
            (self.status_w, 0)
        } else {
            (0, self.num_w)
        };
        let sep = (self.sep_w > 0).then_some(self.num_w + self.status_w);
        (num, status, sep)
    }
}

/// Column geometry for one diff body, resolved once from the inner rect.
///
/// A wide rect splits into a base column on the left and a buffer column on the
/// right, each laid out per its [`DiffLayout`]. A rect narrower than the
/// layout's threshold aliases both sides onto one full-width column with no
/// mid-divider, so Block and buffer rows land in the same place and read as a
/// unified diff.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct DiffColumns {
    pub(crate) left_num_x: u16,
    pub(crate) status_left_x: u16,
    /// Absolute x of the rule between the left gutter and its text, absent for
    /// a layout with no rule.
    pub(crate) left_sep_x: Option<u16>,
    pub(crate) left_text_x: u16,
    pub(crate) left_content_w: usize,
    pub(crate) right_num_x: u16,
    pub(crate) status_right_x: u16,
    pub(crate) right_sep_x: Option<u16>,
    pub(crate) right_text_x: u16,
    pub(crate) right_content_w: usize,
    /// Absolute x of the mid-divider, absent in the unified layout.
    pub(crate) sep_x: Option<u16>,
}

impl DiffColumns {
    pub(crate) fn compute(inner: Rect, layout: DiffLayout) -> Self {
        let gutter_w = (layout.num_w + layout.status_w + layout.sep_w) as usize;
        let (num_off, status_off, sep_off) = layout.offsets();

        if inner.width < layout.two_column_min {
            let num_x = inner.x + num_off;
            let status_x = inner.x + status_off;
            let sep_x = sep_off.map(|off| inner.x + off);
            let text_x = inner.x + gutter_w as u16;
            let content_w = (inner.width as usize).saturating_sub(gutter_w);
            return Self {
                left_num_x: num_x,
                status_left_x: status_x,
                left_sep_x: sep_x,
                left_text_x: text_x,
                left_content_w: content_w,
                right_num_x: num_x,
                status_right_x: status_x,
                right_sep_x: sep_x,
                right_text_x: text_x,
                right_content_w: content_w,
                sep_x: None,
            };
        }

        let full_w = inner.width as usize;
        let sep: usize = 1;
        let half_w = (full_w.saturating_sub(sep)) / 2;
        let left_content_w = half_w.saturating_sub(gutter_w);
        let right_start = inner.x + half_w as u16 + sep as u16;
        let right_content_w = (full_w - half_w - sep).saturating_sub(gutter_w);
        Self {
            left_num_x: inner.x + num_off,
            status_left_x: inner.x + status_off,
            left_sep_x: sep_off.map(|off| inner.x + off),
            left_text_x: inner.x + gutter_w as u16,
            left_content_w,
            right_num_x: right_start + num_off,
            status_right_x: right_start + status_off,
            right_sep_x: sep_off.map(|off| right_start + off),
            right_text_x: right_start + gutter_w as u16,
            right_content_w,
            sep_x: Some(inner.x + half_w as u16),
        }
    }
}

/// Paint the diff body for the rows visible from `scroll_row`.
///
/// A wide inner rect lays out base text left and buffer text right. A narrow one
/// collapses to a single unified column. See [`DiffColumns`] for the geometry.
///
/// Shared by the live [`render_diff_view`] and the off-loop smooth-scroll page
/// so both paint an identical grid. It takes owned parts and paints no cursor,
/// letting a pooled page render it on a blocking worker.
///
/// `endpoint_cache` and `row_cache` are the editor's, when this paint has one
/// behind it. A pooled page has no editor at all and passes `None` for both,
/// resolving its endpoints and row state fresh.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_diff_rows(
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
    dim: f32,
    soften_scale: f32,
    endpoint_cache: Option<&mut Option<CachedHighlightEndpoints>>,
    row_cache: Option<&mut Option<DiffRowCache>>,
) {
    let total_rows = snapshot.line_count();
    let visible = inner.height as u32;
    let end_row = (scroll_row + visible).min(total_rows);
    if end_row <= scroll_row {
        return;
    }

    // One buffer for every number this loop paints, rather than one per row.
    let mut num_text = String::new();

    // Rich mode replaces the ASCII gutter (status glyphs, line numbers, and the
    // separators) with sub-cell APC components. It engages only when a scene is
    // threaded and every gutter color resolves to RGB, so the two paths never
    // mix within one frame. `dim` fades the bars toward the background for a
    // pooled unfocused page, matching the cell dimming.
    let mut rich = scene.and_then(|scene| {
        resolve_diff_rich_colors(theme, fallback_style, dim)
            .map(|colors| DiffRichGutter { scene, colors })
    });

    let DiffColumns {
        left_num_x,
        status_left_x,
        left_sep_x,
        left_text_x,
        left_content_w,
        right_num_x,
        status_right_x,
        right_sep_x,
        right_text_x,
        right_content_w,
        sep_x,
    } = DiffColumns::compute(inner, DiffLayout::DIFF_VIEW);

    use crate::theme::scope as s;
    let dim_style = theme.get(s::DIFF_CONTEXT);
    let del_style = theme.get(s::DIFF_DELETED);
    let inlay_style = fallback_style.patch(theme.get(s::UI_VIRTUAL_INLAY));

    let tints = resolve_diff_tints(theme);
    let base_changes = snapshot
        .diff_map()
        .map(|dm| dm.base_change_spans())
        .unwrap_or_default();

    let mut base_line = base_line_at(snapshot, scroll_row);
    let row_endpoints = match endpoint_cache {
        Some(cache) => snapshot.highlighted_endpoints_cached(scroll_row..end_row, cache),
        None => snapshot.highlighted_endpoints(scroll_row..end_row),
    };
    // One replay for the loop. The rows below ascend, and each opens its own
    // stream because the paint puts a gutter, a number and a status glyph
    // between them, so without this each row re-walks the endpoints above it.
    let mut row_cursor = snapshot.row_highlight_cursor(row_endpoints);
    let mut local_row_cache = None;
    let row_states = diff_row_states(
        snapshot,
        scroll_row..end_row,
        row_cache.unwrap_or(&mut local_row_cache),
    );
    let mut line_buf = String::new();
    // Reused across rows. No row keeps its hunks past itself, so the buffer
    // only ever grows to the widest row rather than being rebuilt for each one.
    let mut hunk_scratch: Vec<&DiffHunk> = Vec::new();

    for display_row in scroll_row..end_row {
        let y = inner.y + (display_row - scroll_row) as u16;
        if y >= inner.y + inner.height {
            break;
        }

        if rich.is_none() {
            if let Some(sep_x) = sep_x {
                buf[(sep_x, y)].set_char('│').set_style(dim_style);
            }
            for gutter_sep in [left_sep_x, right_sep_x].into_iter().flatten() {
                if gutter_sep < inner.x + inner.width {
                    buf[(gutter_sep, y)].set_char('│').set_style(dim_style);
                }
            }
        }

        let row_state = &row_states[(display_row - scroll_row) as usize];
        match row_state.kind {
            DiffRowKind::Block => {
                line_buf.clear();
                snapshot.write_display_line(&mut line_buf, display_row);
                paint_base_side(
                    snapshot,
                    &mut rich,
                    buf,
                    &mut num_text,
                    inner,
                    (left_num_x, status_left_x, left_text_x, left_content_w),
                    y,
                    base_line,
                    &line_buf,
                    &base_changes,
                    tints.as_ref(),
                    (del_style, dim_style),
                    theme,
                    soften_scale,
                );
                base_line += 1;
            },
            DiffRowKind::BufferRow { buffer_row } => {
                draw_diff_num(
                    &mut rich,
                    buf,
                    &mut num_text,
                    inner,
                    right_num_x,
                    y,
                    buffer_row + 1,
                    dim_style,
                );
                let changes = &row_state.change_spans;
                let staged = row_state.staged;
                let status = row_state.status;
                let soften_row = match status {
                    DiffStatus::Unchanged => tints.as_ref().map(|t| t.bg),
                    _ => None,
                };
                let soften_gaps = match status {
                    DiffStatus::Modified | DiffStatus::Moved => tints.as_ref().map(|t| t.bg),
                    _ => None,
                };
                paint_highlighted_row(
                    snapshot,
                    display_row,
                    right_text_x,
                    y,
                    right_content_w,
                    buf,
                    fallback_style,
                    inlay_style,
                    changes,
                    tints.is_some(),
                    soften_row,
                    soften_gaps,
                    soften_scale,
                    &mut row_cursor,
                );
                if status == DiffStatus::Moved
                    && let Some((path, line)) =
                        move_chip_source(snapshot, buffer_row, &mut hunk_scratch)
                {
                    line_buf.clear();
                    snapshot.write_display_line(&mut line_buf, display_row);
                    render_move_chip(
                        buf,
                        right_text_x,
                        y,
                        line_buf.chars().count(),
                        right_content_w,
                        path.as_deref(),
                        line,
                        theme.get(s::DIFF_MOVED).add_modifier(Modifier::ITALIC),
                    );
                }
                if let Some(staged) = staged {
                    let change_scope = match status {
                        DiffStatus::Added => s::DIFF_ADDED,
                        DiffStatus::Modified => s::DIFF_MODIFIED,
                        DiffStatus::Moved => s::DIFF_MOVED,
                        DiffStatus::Unchanged => s::DIFF_CONTEXT,
                    };
                    draw_diff_status(
                        &mut rich,
                        buf,
                        inner,
                        status_right_x,
                        y,
                        change_scope,
                        staged,
                        theme,
                    );
                }
                // An unchanged row mirrors its own line, which is the same text
                // on both sides. A paired modified row shows the base line it
                // replaced, painted as a removed row rather than a mirror. A
                // row past its hunk's base rows has nothing on the left.
                if status == DiffStatus::Unchanged {
                    line_buf.clear();
                    snapshot.write_display_line(&mut line_buf, display_row);
                    draw_diff_num(
                        &mut rich,
                        buf,
                        &mut num_text,
                        inner,
                        left_num_x,
                        y,
                        base_line + 1,
                        dim_style,
                    );
                    let token_spans = base_token_spans(snapshot, base_line);
                    paint_base_row(
                        buf,
                        left_text_x,
                        y,
                        &line_buf,
                        left_content_w,
                        token_spans,
                        dim_style,
                        &[],
                        tints.is_some(),
                        soften_row,
                        None,
                        soften_scale,
                    );
                    base_line += 1;
                } else if row_state.paired {
                    let text = snapshot
                        .diff_map()
                        .and_then(|dm| dm.base_line_text(base_line))
                        .unwrap_or("");
                    paint_base_side(
                        snapshot,
                        &mut rich,
                        buf,
                        &mut num_text,
                        inner,
                        (left_num_x, status_left_x, left_text_x, left_content_w),
                        y,
                        base_line,
                        text,
                        &base_changes,
                        tints.as_ref(),
                        (del_style, dim_style),
                        theme,
                        soften_scale,
                    );
                    base_line += 1;
                }
            },
        }
    }

    // One hairline separator per gutter spanning the visible rows, replacing the
    // per-row glyph. Centered in its cell via the +8 sixteenths offset.
    if let Some(rg) = rich.as_mut() {
        let height = (end_row - scroll_row) as u16 * 16;
        for sep in [left_sep_x, right_sep_x, sep_x].into_iter().flatten() {
            if sep < inner.x + inner.width {
                Bar {
                    x: ((sep - inner.x) * 16 + 8) as i16,
                    y: 0,
                    width: 1,
                    height,
                    color: rg.colors.dim,
                }
                .render(inner, buf, &mut *rg.scene);
            }
        }
    }
}

/// The editor background the diff view's softening blends toward, present only
/// on an RGB theme.
///
/// Resolved once per paint. A theme whose background is not an RGB color has no
/// channels to blend, so it softens nothing and marks its change spans with
/// [`Modifier::UNDERLINED`] instead, which is what an absent value means to
/// every caller.
pub(crate) struct DiffTints {
    pub(crate) bg: [u8; 3],
}

/// Resolve the background the softening blends toward, or `None` when the
/// theme's is not an RGB color.
///
/// A `None` turns off softening for the whole frame and puts the diff view on
/// the underline fallback for change spans, which is how an indexed-color theme
/// still marks what changed.
pub(crate) fn resolve_diff_tints(theme: &crate::theme::Theme) -> Option<DiffTints> {
    use crate::theme::scope as s;
    let bg = style_rgb(theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg))?;
    Some(DiffTints { bg })
}

/// Mark a cell `style` as inside a change span.
///
/// An RGB theme leaves a code span as it is. The chars outside the spans recede
/// instead, so a changed char leads its line by being the one thing at full
/// strength, and the diff paints no background at all.
///
/// A `prose` replacement bolds on top of that. The receding alone is too weak
/// to find a changed char inside a string or a comment, where the whole literal
/// carries one color and no token boundary falls beside the edit. Only a
/// replacement bolds, because that is the case the reader compares char by
/// char.
///
/// A theme that cannot blend has no receding to lead against, so it underlines
/// the span, which is the only mark left to it.
fn mark_span(style: Style, kind: &ChangeKind, prose: bool, rgb: bool) -> Style {
    let style = match rgb {
        true => style,
        false => style.add_modifier(Modifier::UNDERLINED),
    };
    match prose && matches!(kind, ChangeKind::Replaced) {
        true => style.add_modifier(Modifier::BOLD),
        false => style,
    }
}

/// Recede `style` by blending its foreground toward `bg` by `amount` and
/// dropping bold.
///
/// A non-RGB foreground comes back unblended, because there is no channel to
/// interpolate. Bold goes regardless. Weight pulls the eye as hard as color, so
/// a softened run that keeps its bold still competes with the changed content
/// it sits behind.
fn soften_style(style: Style, bg: [u8; 3], amount: f32) -> Style {
    let style = match style_rgb(style.fg) {
        Some(fg) => {
            let [r, g, b] = dim_rgb(fg, bg, amount);
            style.fg(Color::Rgb(r, g, b))
        },
        None => style,
    };
    style.remove_modifier(Modifier::BOLD)
}

/// Paint base text with per-token syntax styles for the diff view's left
/// column.
///
/// A byte inside a token span takes that token's color. Bytes outside every
/// span fall back to `fallback`, the deletion or context color, so the gaps
/// between tokens still read as part of the diff.
///
/// `change_spans` mark the changed chars of a modified or moved base line, as
/// line-local base byte ranges tagged by [`ChangeKind`]. On an RGB theme
/// (`rgb`) a byte inside one is left as it is, at full strength, and the
/// softening of everything around it is what marks it. A theme that cannot
/// blend underlines the span instead, per [`mark_span`].
///
/// `soften_row` recedes the whole row behind the changed rows around it, by
/// blending every foreground toward the given background per [`soften_style`].
/// Pass the editor background for an unchanged row and `None` for a changed one.
///
/// `soften_gaps` recedes only the chars outside every change span, at the
/// lighter [`MODIFIED_ROW_SOFTEN`], so a refined row's changed chars lead their
/// own line. An empty `change_spans` no-ops it, which keeps a row the
/// refinement never reached at full strength.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_base_row(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text: &str,
    max_cols: usize,
    token_spans: &[(std::ops::Range<usize>, HighlightStyle)],
    fallback: Style,
    change_spans: &[(std::ops::Range<usize>, ChangeKind, bool)],
    rgb: bool,
    soften_row: Option<[u8; 3]>,
    soften_gaps: Option<[u8; 3]>,
    soften_scale: f32,
) {
    debug_assert!(
        token_spans.is_sorted_by_key(|(range, _)| range.start),
        "token_spans must be start-sorted for the monotonic cursor"
    );
    debug_assert!(
        change_spans.is_sorted_by_key(|(range, ..)| range.start),
        "change_spans must be start-sorted for the monotonic cursor"
    );

    let soften_gaps = soften_gaps.filter(|_| !change_spans.is_empty());
    let mut token_cursor = 0;
    let mut span_cursor = 0;
    paint_style_runs(buf, start_x, y, text, max_cols, |byte_idx| {
        while token_spans
            .get(token_cursor)
            .is_some_and(|(r, _)| r.end <= byte_idx)
        {
            token_cursor += 1;
        }
        let mut style = match token_spans.get(token_cursor) {
            Some((range, hs)) if range.start <= byte_idx => hs.to_ratatui_style(),
            _ => fallback,
        };
        if let Some(bg) = soften_row.filter(|_| soften_scale > 0.0) {
            style = soften_style(style, bg, (CONTEXT_SOFTEN * soften_scale).min(SOFTEN_CAP));
        }

        while change_spans
            .get(span_cursor)
            .is_some_and(|(r, ..)| r.end <= byte_idx)
        {
            span_cursor += 1;
        }
        match change_spans.get(span_cursor) {
            Some((range, kind, prose)) if range.start <= byte_idx => {
                style = mark_span(style, kind, *prose, rgb);
            },
            _ => {
                if let Some(bg) = soften_gaps.filter(|_| soften_scale > 0.0) {
                    style = soften_style(
                        style,
                        bg,
                        (MODIFIED_ROW_SOFTEN * soften_scale).min(SOFTEN_CAP),
                    );
                }
            },
        }

        style
    });
}

/// The base line's syntax spans, empty where the editor paints no syntax color.
///
/// The left column's tokens come from the diff map rather than from the display
/// map, so the snapshot cannot withhold them the way it withholds the right
/// column's. Reading its flag here is what keeps the two columns agreeing.
fn base_token_spans(
    snapshot: &DisplaySnapshot,
    base_line: u32,
) -> &[(std::ops::Range<usize>, HighlightStyle)] {
    if !snapshot.syntax_highlighting() {
        return &[];
    }
    snapshot
        .diff_map()
        .and_then(|dm| dm.base_highlights_for_line(base_line))
        .unwrap_or(&[])
}

/// Paint the diff view's two-cell status column for a hunk row.
///
/// The first cell carries the change-kind bar in `change_scope`. The second
/// carries a staged-state bar scoped `diff.staged` when `staged` else
/// `diff.unstaged`. Both use the `▎` bar, mirroring the editor gutter's two
/// bars. The staged cell is skipped when it would fall outside the buffer.
fn paint_status_bars(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    change_scope: &str,
    staged: bool,
    theme: &crate::theme::Theme,
) {
    use crate::theme::scope as s;
    if x >= buf.area.x + buf.area.width {
        return;
    }
    buf[(x, y)].set_char('▎').set_style(theme.get(change_scope));
    if x + 1 < buf.area.x + buf.area.width {
        let staged_scope = if staged {
            s::DIFF_STAGED
        } else {
            s::DIFF_UNSTAGED
        };
        buf[(x + 1, y)]
            .set_char('▎')
            .set_style(theme.get(staged_scope));
    }
}

/// Paint one base line into the left column: its number, its text under the
/// removed style with the base change-span washes, and its staged status.
///
/// Both sides of the diff reach here. A block row is a base line with no live
/// row beside it, and a paired modified row is one that has both, so the two
/// paint the left column identically and differ only in where the text comes
/// from.
///
/// `columns` is `(number, status, text, content width)` for the left side, and
/// `styles` is `(removed, dim)`.
#[allow(clippy::too_many_arguments)]
fn paint_base_side(
    snapshot: &DisplaySnapshot,
    rich: &mut Option<DiffRichGutter<'_>>,
    buf: &mut Buffer,
    num_text: &mut String,
    inner: Rect,
    columns: (u16, u16, u16, usize),
    y: u16,
    base_line: u32,
    text: &str,
    base_changes: &crate::diff_map::BaseChangeSpans,
    tints: Option<&DiffTints>,
    styles: (Style, Style),
    theme: &crate::theme::Theme,
    soften_scale: f32,
) {
    use crate::theme::scope as s;
    let (num_x, status_x, text_x, content_w) = columns;
    let (del_style, dim_style) = styles;

    draw_diff_num(
        rich,
        buf,
        num_text,
        inner,
        num_x,
        y,
        base_line + 1,
        dim_style,
    );

    let token_spans = base_token_spans(snapshot, base_line);
    let changes = base_changes
        .get(&base_line)
        .map(Vec::as_slice)
        .unwrap_or(&[]);
    let staged = snapshot
        .diff_map()
        .and_then(|dm| dm.base_line_staged(base_line));
    paint_base_row(
        buf,
        text_x,
        y,
        text,
        content_w,
        token_spans,
        del_style,
        changes,
        tints.is_some(),
        None,
        tints.map(|t| t.bg),
        soften_scale,
    );

    if let Some(staged) = staged {
        let change_scope = if changes
            .iter()
            .any(|(_, k, _)| matches!(k, ChangeKind::Moved))
        {
            s::DIFF_MOVED
        } else {
            s::DIFF_DELETED
        };
        draw_diff_status(rich, buf, inner, status_x, y, change_scope, staged, theme);
    }
}

/// Base-file line number at the top of the viewport (display row `scroll_row`).
///
/// Every display row above `scroll_row` is base-present (a deleted-base block
/// row or an unchanged buffer row) except changed buffer rows, which have no
/// base line. So the base line count is `scroll_row` minus the changed buffer
/// rows above, which the diff map answers in one seek rather than a per-row
/// walk from the document start.
fn base_line_at(snapshot: &DisplaySnapshot, scroll_row: u32) -> u32 {
    let buffer_rows_above = snapshot.buffer_rows_above(scroll_row);
    let changed = snapshot.diff_map().map_or(0, |dm| {
        dm.rows_without_base_before(buffer_rows_above, snapshot.pairs_modified_hunks())
    });
    scroll_row.saturating_sub(changed)
}

/// Paint one display row's syntax-highlighted chunks into a column starting at
/// `start_x`, clamped to `max_cols` and the buffer's right edge.
///
/// `change_spans` mark the changed chars of a modified or moved row, as
/// display-column ranges tagged by [`ChangeKind`]. On an RGB theme (`rgb`) a
/// cell inside one is left as it is, at full strength, and the softening of
/// everything around it is what marks it. A theme that cannot blend underlines
/// the span instead, per [`mark_span`]. Columns, not byte offsets, are used
/// because the chunks expand tabs, so the counter tracks display cells.
///
/// `soften_row` recedes the whole row behind the changed rows around it, by
/// blending every foreground toward the given background per [`soften_style`].
/// Pass the editor background for an unchanged row and `None` for a changed one.
///
/// `soften_gaps` recedes only the cells outside every change span, at the
/// lighter [`MODIFIED_ROW_SOFTEN`], so a refined row's changed cells lead their
/// own line. An empty `change_spans` no-ops it, which keeps a row the
/// refinement never reached at full strength.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_highlighted_row(
    snapshot: &DisplaySnapshot,
    display_row: u32,
    start_x: u16,
    y: u16,
    max_cols: usize,
    buf: &mut Buffer,
    fallback_style: Style,
    inlay_style: Style,
    change_spans: &[(std::ops::Range<usize>, ChangeKind, bool)],
    rgb: bool,
    soften_row: Option<[u8; 3]>,
    soften_gaps: Option<[u8; 3]>,
    soften_scale: f32,
    row_cursor: &mut RowHighlightCursor,
) {
    debug_assert!(
        change_spans.is_sorted_by_key(|(range, ..)| range.start),
        "change_spans must be start-sorted for the monotonic cursor"
    );

    let soften_gaps = soften_gaps.filter(|_| !change_spans.is_empty());
    let mut col = 0usize;
    let mut span_cursor = 0;
    for chunk in snapshot.row_chunks(display_row, row_cursor) {
        let style = if chunk.is_inlay {
            inlay_style
        } else {
            chunk
                .highlight_style
                .as_ref()
                .map(|hs| hs.to_ratatui_style())
                .unwrap_or(fallback_style)
        };
        let style = match soften_row.filter(|_| soften_scale > 0.0) {
            Some(bg) => soften_style(style, bg, (CONTEXT_SOFTEN * soften_scale).min(SOFTEN_CAP)),
            None => style,
        };
        // Both variants resolve per chunk rather than per cell, because a
        // chunk's cells differ only in which side of a change span they fall on.
        let gap_style = match soften_gaps.filter(|_| soften_scale > 0.0) {
            Some(bg) => soften_style(
                style,
                bg,
                (MODIFIED_ROW_SOFTEN * soften_scale).min(SOFTEN_CAP),
            ),
            None => style,
        };

        for ch in chunk.text.chars() {
            if ch == '\n' || col >= max_cols {
                return;
            }
            let x = start_x + col as u16;
            if x >= buf.area.x + buf.area.width {
                return;
            }
            while change_spans
                .get(span_cursor)
                .is_some_and(|(r, ..)| r.end <= col)
            {
                span_cursor += 1;
            }
            let cell_style = match change_spans.get(span_cursor) {
                Some((range, kind, prose)) if range.start <= col => {
                    mark_span(style, kind, *prose, rgb)
                },
                _ => gap_style,
            };
            buf[(x, y)].set_char(ch).set_style(cell_style);
            col += 1;
        }
    }
}

/// Write into `out` the display-column ranges to wash on buffer `buffer_row` in
/// the diff view's right column, each tagged with its [`ChangeKind`], taken from
/// the buffer spans of any hunk covering the row.
///
/// The token detail's byte ranges are absolute buffer offsets. Each is clamped
/// to the row and mapped through [`DisplaySnapshot::buffer_to_display`], so tab
/// expansion in the painted chunks stays aligned.
///
/// Both vectors belong to the caller and are reused across the rows of one
/// paint, so each is cleared before being filled. A row no hunk refines leaves
/// `out` empty rather than carrying the previous row's spans.
///
/// Returns whether the row is marked, by the same rule the gutter applies: a
/// covering hunk the tree pass never narrowed marks every row it spans, and a
/// narrowed one marks only the rows its runs name. A caller reads this rather
/// than `out` being non-empty, so the bars and the gutter cannot disagree.
fn write_buffer_row_change_spans<'a>(
    snapshot: &'a DisplaySnapshot,
    buffer_row: u32,
    hunks: &mut Vec<&'a DiffHunk>,
    out: &mut Vec<(std::ops::Range<usize>, ChangeKind, bool)>,
) -> bool {
    out.clear();
    hunks.clear();

    let Some(diff_map) = snapshot.diff_map() else {
        return false;
    };
    diff_map.hunks_in_range_into(buffer_row..buffer_row + 1, hunks);
    if hunks.is_empty() {
        return false;
    }
    let marked = hunks.iter().any(|hunk| {
        !hunk.refined() || hunk.marked_rows.iter().any(|run| run.contains(&buffer_row))
    });

    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let line_start = rope.point_to_offset(Point::new(buffer_row, 0));
    let line_end = line_start + rope.line_len(buffer_row) as usize;

    for hunk in hunks.iter() {
        let Some(detail) = &hunk.token_detail else {
            continue;
        };
        for span in &detail.buffer_spans {
            let start = span.byte_range.start.max(line_start);
            let end = span.byte_range.end.min(line_end);
            if start >= end {
                continue;
            }
            let start_col = snapshot
                .buffer_to_display(rope.offset_to_point(start))
                .column as usize;
            let end_col = snapshot.buffer_to_display(rope.offset_to_point(end)).column as usize;
            out.push((start_col..end_col, span.kind.clone(), span.prose));
        }
    }
    // Spans arrive per hunk and are not otherwise ordered. Start-sorting them
    // makes the painter's monotonic span cursor correct.
    out.sort_by_key(|(range, ..)| range.start);
    marked
}

/// The origin of a moved buffer row, for the diff view's move chip.
///
/// Scans the hunks covering `buffer_row` for the first move span and returns its
/// first counterpart source as `(file name, 0-based line)`. The file name is
/// `None` for an intra-file move (no counterpart buffer), so the chip omits the
/// path. Returns `None` when the row is not part of a move.
fn move_chip_source<'a>(
    snapshot: &'a DisplaySnapshot,
    buffer_row: u32,
    hunks: &mut Vec<&'a DiffHunk>,
) -> Option<(Option<String>, u32)> {
    hunks.clear();
    let diff_map = snapshot.diff_map()?;
    diff_map.hunks_in_range_into(buffer_row..buffer_row + 1, hunks);
    for hunk in hunks.iter() {
        let Some(detail) = &hunk.token_detail else {
            continue;
        };
        for span in &detail.buffer_spans {
            let Some(meta) = &span.move_metadata else {
                continue;
            };
            let Some(source) = meta.sources.first() else {
                continue;
            };
            let path = source.buffer.as_ref().map(|b| {
                b.path
                    .file_name()
                    .unwrap_or(b.path.as_os_str())
                    .to_string_lossy()
                    .into_owned()
            });
            return Some((path, source.line_range.start));
        }
    }
    None
}

/// Paint the clean-tree empty state as a centered dim line, so the diff view
/// reads as intentionally open and waiting rather than broken. The watching
/// clause is dropped when `review_follow` will not auto-refresh the view.
fn render_review_empty(watching: bool, inner: Rect, theme: &crate::theme::Theme, buf: &mut Buffer) {
    let message = if watching {
        "working tree clean, watching for changes"
    } else {
        "working tree clean"
    };
    let chars: Vec<char> = message.chars().collect();
    let width = chars.len() as u16;
    if width > inner.width || inner.height == 0 {
        return;
    }
    let style = theme.get(crate::theme::scope::UI_TEXT_DIM);
    let start_x = inner.x + (inner.width - width) / 2;
    let y = inner.y + inner.height / 2;
    for (i, ch) in chars.into_iter().enumerate() {
        buf[(start_x + i as u16, y)].set_char(ch).set_style(style);
    }
}

/// X column where the diff body's buffer text begins, for placing the cursor
/// and mapping clicks onto buffer offsets.
///
/// In the two-column layout this is the right pane's text column. In the narrow
/// unified layout it is the single shared text column. See [`DiffColumns`].
pub(crate) fn right_text_x(inner: Rect) -> u16 {
    DiffColumns::compute(inner, DiffLayout::DIFF_VIEW).right_text_x
}

/// X column where the review session's buffer text begins, for placing its
/// cursor.
///
/// The review screen lays its gutter out per [`DiffLayout::REVIEW`], which puts
/// its text a column left of the diff view's and never collapses to a unified
/// column, so its cursor lands elsewhere than [`right_text_x`].
fn review_cursor_text_x(inner: Rect) -> u16 {
    DiffColumns::compute(inner, DiffLayout::REVIEW).right_text_x
}

/// Paint the primary selection's cursor over the right pane's text, or set the
/// stoatty hardware cursor there. Skips a row scrolled out of view.
pub(crate) fn render_review_cursor(
    editor: &mut EditorState,
    snapshot: &DisplaySnapshot,
    inner: Rect,
    text_x: u16,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    stoatty: bool,
) {
    let cursor_style = theme.cursor_style();

    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let sel = editor.selections.newest_anchor();
    let cursor = cursor_offset(
        rope,
        buffer_snapshot.resolve_anchor(&sel.tail()),
        buffer_snapshot.resolve_anchor(&sel.head()),
    );
    let display = snapshot.buffer_to_display(rope.offset_to_point(cursor));

    let visible = inner.height as u32;
    if display.row < editor.scroll_row || display.row >= editor.scroll_row + visible {
        return;
    }
    let y = inner.y + (display.row - editor.scroll_row) as u16;
    let x = text_x + display.column as u16;
    if x >= inner.x + inner.width || y >= inner.y + inner.height {
        return;
    }

    if stoatty {
        editor.cursor_screen_cell = Some((x, y));
    } else {
        let cell = &mut buf[(x, y)];
        let existing = cell.symbol().chars().next().unwrap_or(' ');
        cell.set_char(if existing == '\0' { ' ' } else { existing });
        cell.set_style(cursor_style);
    }
}

/// Paint the review pane rows from owned, `Send` parts rather than an
/// [`EditorState`], so a pooled review page can render off the run loop the way
/// [`render_page_from_snapshot`](crate::smooth_scroll::render_page_from_snapshot)
/// does for editors. `scroll_row` is the display row at the top of `inner`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_review_rows(
    snapshot: &DisplaySnapshot,
    view: &ReviewViewState,
    scroll_row: u32,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
) {
    let rows = &view.rows;
    let total_rows = snapshot.line_count();
    let visible = inner.height as u32;
    let end_row = (scroll_row + visible).min(total_rows);
    if end_row <= scroll_row {
        return;
    }

    // One buffer for every number this loop paints, rather than one per row.
    let mut num_text = String::new();

    let DiffColumns {
        left_num_x,
        status_left_x,
        left_text_x,
        left_content_w,
        right_num_x,
        status_right_x,
        right_text_x,
        right_content_w,
        sep_x,
        ..
    } = DiffColumns::compute(inner, DiffLayout::REVIEW);

    use crate::theme::scope as s;
    let dim_style = theme.get(s::DIFF_CONTEXT);
    let del_hl = theme.get(s::DIFF_DELETED);
    let add_hl = theme.get(s::DIFF_ADDED);
    let move_hl = theme.get(s::DIFF_MOVED).add_modifier(Modifier::ITALIC);
    let current_style = theme.get(s::DIFF_CURRENT_HUNK);

    // Rich mode replaces the ASCII gutter (status glyph, line number, gap dots,
    // separator) with sub-cell APC components. It engages only when a scene is
    // threaded and every gutter color resolves to RGB, so the two paths never
    // mix within one frame.
    let mut rich = scene.and_then(|scene| {
        resolve_rich_colors(theme, fallback_style).map(|colors| RichGutter { scene, colors })
    });

    for display_row in scroll_row..end_row {
        let y = inner.y + (display_row - scroll_row) as u16;
        if y >= inner.y + inner.height {
            break;
        }

        if let Some(sep_x) = sep_x
            && rich.is_none()
            && sep_x < inner.x + inner.width
        {
            buf[(sep_x, y)].set_char('│').set_style(dim_style);
        }

        match snapshot.classify_row(display_row) {
            BlockRowKind::BufferRow { buffer_row } => {
                let Some(row) = rows.get(buffer_row as usize) else {
                    continue;
                };
                if let Some((chunk_id, status)) = view.chunk_and_status_at_row(buffer_row) {
                    let is_current = Some(chunk_id) == view.current_chunk;
                    draw_status_gutter(
                        &mut rich,
                        buf,
                        inner,
                        status_left_x,
                        y,
                        status,
                        is_current,
                        current_style,
                        theme,
                    );
                    draw_status_gutter(
                        &mut rich,
                        buf,
                        inner,
                        status_right_x,
                        y,
                        status,
                        is_current,
                        current_style,
                        theme,
                    );
                }
                match row {
                    ReviewRow::Context { left, right } => {
                        draw_side_num(
                            &mut rich,
                            buf,
                            &mut num_text,
                            inner,
                            left_num_x,
                            y,
                            left.line_num,
                            dim_style,
                        );
                        render_side_text(
                            buf,
                            left_text_x,
                            y,
                            &left.text,
                            left_content_w,
                            fallback_style,
                            &[],
                            fallback_style,
                            &[],
                            move_hl,
                        );
                        draw_side_num(
                            &mut rich,
                            buf,
                            &mut num_text,
                            inner,
                            right_num_x,
                            y,
                            right.line_num,
                            dim_style,
                        );
                        render_side_text(
                            buf,
                            right_text_x,
                            y,
                            &right.text,
                            right_content_w,
                            fallback_style,
                            &[],
                            fallback_style,
                            &[],
                            move_hl,
                        );
                    },
                    ReviewRow::Changed { left, right } => {
                        if let Some(l) = left {
                            draw_side_num(
                                &mut rich,
                                buf,
                                &mut num_text,
                                inner,
                                left_num_x,
                                y,
                                l.line_num,
                                dim_style,
                            );
                            render_side_text(
                                buf,
                                left_text_x,
                                y,
                                &l.text,
                                left_content_w,
                                fallback_style,
                                &l.change_spans,
                                del_hl,
                                &l.moved_spans,
                                move_hl,
                            );
                            if let Some(prov) = l.move_provenance.as_ref() {
                                render_move_chip(
                                    buf,
                                    left_text_x,
                                    y,
                                    l.text.chars().count(),
                                    left_content_w,
                                    (!prov.intra_file).then_some(prov.rel_path.as_str()),
                                    prov.line,
                                    move_hl,
                                );
                            }
                        } else {
                            draw_empty_num(&rich, buf, left_num_x, y, dim_style);
                        }
                        if let Some(r) = right {
                            draw_side_num(
                                &mut rich,
                                buf,
                                &mut num_text,
                                inner,
                                right_num_x,
                                y,
                                r.line_num,
                                dim_style,
                            );
                            render_side_text(
                                buf,
                                right_text_x,
                                y,
                                &r.text,
                                right_content_w,
                                fallback_style,
                                &r.change_spans,
                                add_hl,
                                &r.moved_spans,
                                move_hl,
                            );
                            if let Some(prov) = r.move_provenance.as_ref() {
                                render_move_chip(
                                    buf,
                                    right_text_x,
                                    y,
                                    r.text.chars().count(),
                                    right_content_w,
                                    (!prov.intra_file).then_some(prov.rel_path.as_str()),
                                    prov.line,
                                    move_hl,
                                );
                            }
                        } else {
                            draw_empty_num(&rich, buf, right_num_x, y, dim_style);
                        }
                    },
                }
            },
            BlockRowKind::Block { block, line_index } => {
                let line = block.get_line(line_index);
                let block_style = theme.get(crate::theme::scope::UI_PROMPT);
                for (i, ch) in line.chars().enumerate() {
                    let x = inner.x + i as u16;
                    if x >= inner.x + inner.width {
                        break;
                    }
                    buf[(x, y)].set_char(ch).set_style(block_style);
                }
            },
        }
    }

    // One hairline separator spanning the visible rows, replacing the per-row
    // glyph. Centered in its cell via the +8 sixteenths offset.
    if let (Some(rg), Some(sep_x)) = (rich.as_mut(), sep_x) {
        Bar {
            x: ((sep_x - inner.x) * 16 + 8) as i16,
            y: 0,
            width: 1,
            height: (end_row - scroll_row) as u16 * 16,
            color: rg.colors.dim,
        }
        .render(inner, buf, &mut *rg.scene);
    }
}

/// Emit a chunk-status bar (rich) or paint the ASCII status glyph.
///
/// A [`ChunkStatus::Pending`] chunk has no bar in rich mode, matching the blank
/// glyph the ASCII path draws for it.
#[allow(clippy::too_many_arguments)]
fn draw_status_gutter(
    rich: &mut Option<RichGutter<'_>>,
    buf: &mut Buffer,
    inner: Rect,
    col: u16,
    y: u16,
    status: ChunkStatus,
    is_current: bool,
    current_style: Style,
    theme: &crate::theme::Theme,
) {
    match rich {
        Some(rg) => {
            if let Some(color) = status_bar_color(status, is_current, &rg.colors) {
                Bar {
                    x: ((col - inner.x) * 16) as i16,
                    y: ((y - inner.y) * 16) as i16,
                    width: 6,
                    height: 16,
                    color,
                }
                .render(inner, buf, &mut *rg.scene);
            }
        },
        None => paint_status_gutter(buf, col, y, status, is_current, current_style, theme),
    }
}

/// Emit a right-aligned line number as a sub-cell run (rich) or paint the ASCII
/// number.
#[allow(clippy::too_many_arguments)]
fn draw_side_num(
    rich: &mut Option<RichGutter<'_>>,
    buf: &mut Buffer,
    scratch: &mut String,
    inner: Rect,
    num_x: u16,
    y: u16,
    num: u32,
    dim_style: Style,
) {
    scratch.clear();
    write!(scratch, "{num}").expect("writing to a String is infallible");
    match rich {
        Some(rg) => {
            let advance = text_run::advance_sixteenths(scratch.len(), TEXT_SCALE_COMPACT);
            let right_edge = (num_x - inner.x + 4) * 16;
            TextRun {
                col: right_edge.saturating_sub(advance) as i16,
                row: ((y - inner.y) * 16) as i16,
                scale: TEXT_SCALE_COMPACT,
                color: rg.colors.dim,
                bg: Some(rg.colors.bg),
                text: scratch,
            }
            .render(inner, buf, &mut *rg.scene);
        },
        None => render_side_num(buf, scratch, num_x, y, num, dim_style),
    }
}

/// Paint the ASCII gap marker (`.....`) for a side with no line on this row. In
/// rich mode the gap is simply the absence of a run, so this is a no-op.
fn draw_empty_num(
    rich: &Option<RichGutter<'_>>,
    buf: &mut Buffer,
    num_x: u16,
    y: u16,
    dim_style: Style,
) {
    if rich.is_none() {
        render_empty_num(buf, num_x, y, dim_style);
    }
}

/// The RGB gutter colors extracted from the theme, plus the reused scene the
/// sub-cell components append into.
struct RichGutter<'a> {
    scene: &'a mut ApcScene,
    colors: RichColors,
}

struct RichColors {
    /// Line-number and separator color (`diff.context` fg).
    dim: [u8; 3],
    /// Background the line-number runs composite over.
    bg: [u8; 3],
    staged: [u8; 3],
    unstaged: [u8; 3],
    skipped: [u8; 3],
    current: [u8; 3],
}

/// Extract every gutter color as RGB, or `None` if any is missing or not an RGB
/// color. A `None` here disables rich mode for the whole frame, so the gutter
/// falls back to ASCII rather than mixing the two.
fn resolve_rich_colors(theme: &crate::theme::Theme, fallback_style: Style) -> Option<RichColors> {
    use crate::theme::scope as s;
    let bg = fallback_style
        .bg
        .or_else(|| theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg));
    Some(RichColors {
        dim: style_rgb(theme.get(s::DIFF_CONTEXT).fg)?,
        bg: style_rgb(bg)?,
        staged: style_rgb(theme.get(s::DIFF_ADDED).fg)?,
        unstaged: style_rgb(theme.get(s::DIFF_DELETED).fg)?,
        skipped: style_rgb(theme.get(s::UI_TEXT_MUTED).fg)?,
        current: style_rgb(theme.get(s::DIFF_CURRENT_HUNK).fg)?,
    })
}

/// The bar color for a chunk status, or `None` when the status draws no bar. A
/// current chunk always takes the current-hunk color, mirroring the ASCII glyph.
fn status_bar_color(status: ChunkStatus, is_current: bool, colors: &RichColors) -> Option<[u8; 3]> {
    if is_current {
        return Some(colors.current);
    }
    match status {
        ChunkStatus::Staged => Some(colors.staged),
        ChunkStatus::Unstaged => Some(colors.unstaged),
        ChunkStatus::Skipped => Some(colors.skipped),
        ChunkStatus::Pending => None,
    }
}

/// The RGB gutter colors the diff view's rich sub-cell components composite
/// with, plus the reused scene they append into.
struct DiffRichGutter<'a> {
    scene: &'a mut ApcScene,
    colors: DiffRichColors,
}

/// Line-number, separator, and status-bar colors for the diff rich gutter.
///
/// The per-row change bar color is read from the row's change scope, so the
/// resolver validates every change scope is RGB up front and this stores only
/// the dim, background, and staged/unstaged colors used directly. `dim_amount`
/// fades every bar toward the background on a pooled unfocused page.
#[derive(Clone, Copy)]
struct DiffRichColors {
    dim: [u8; 3],
    bg: [u8; 3],
    staged: [u8; 3],
    unstaged: [u8; 3],
    dim_amount: f32,
}

impl DiffRichColors {
    /// Fade `color` toward the background by the pooled-page dim amount.
    fn faded(&self, color: [u8; 3]) -> [u8; 3] {
        dim_rgb(color, self.bg, self.dim_amount)
    }
}

/// Extract every diff gutter color as RGB, or `None` when any is missing or not
/// an RGB color, which disables rich mode for the frame so the ASCII and rich
/// paths never mix.
fn resolve_diff_rich_colors(
    theme: &crate::theme::Theme,
    fallback_style: Style,
    dim_amount: f32,
) -> Option<DiffRichColors> {
    use crate::theme::scope as s;
    let bg = fallback_style
        .bg
        .or_else(|| theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg));
    for scope in [
        s::DIFF_ADDED,
        s::DIFF_MODIFIED,
        s::DIFF_MOVED,
        s::DIFF_DELETED,
        s::DIFF_CONTEXT,
    ] {
        style_rgb(theme.get(scope).fg)?;
    }
    // Pre-fade the directly-stored colors so the line numbers and separators
    // need no per-use dimming. The per-row change color fades at its call site.
    let bg = style_rgb(bg)?;
    Some(DiffRichColors {
        dim: dim_rgb(style_rgb(theme.get(s::DIFF_CONTEXT).fg)?, bg, dim_amount),
        bg,
        staged: dim_rgb(style_rgb(theme.get(s::DIFF_STAGED).fg)?, bg, dim_amount),
        unstaged: dim_rgb(style_rgb(theme.get(s::DIFF_UNSTAGED).fg)?, bg, dim_amount),
        dim_amount,
    })
}

/// Emit a right-aligned diff line number as a sub-cell run (rich) or paint the
/// ASCII number.
#[allow(clippy::too_many_arguments)]
fn draw_diff_num(
    rich: &mut Option<DiffRichGutter<'_>>,
    buf: &mut Buffer,
    scratch: &mut String,
    inner: Rect,
    num_x: u16,
    y: u16,
    num: u32,
    dim_style: Style,
) {
    scratch.clear();
    write!(scratch, "{num}").expect("writing to a String is infallible");
    match rich {
        Some(rg) => {
            let advance = text_run::advance_sixteenths(scratch.len(), TEXT_SCALE_COMPACT);
            let right_edge = (num_x - inner.x + 4) * 16;
            TextRun {
                col: right_edge.saturating_sub(advance) as i16,
                row: ((y - inner.y) * 16) as i16,
                scale: TEXT_SCALE_COMPACT,
                color: rg.colors.dim,
                bg: Some(rg.colors.bg),
                text: scratch,
            }
            .render(inner, buf, &mut *rg.scene);
        },
        None => render_side_num(buf, scratch, num_x, y, num, dim_style),
    }
}

/// Emit the change and staged bars (rich) or paint the ASCII status glyphs.
///
/// The bars follow the editor's rich gutter spacing, a five-sixteenth change bar
/// at the status cell, then a five-sixteenth staged bar seven sixteenths later.
#[allow(clippy::too_many_arguments)]
fn draw_diff_status(
    rich: &mut Option<DiffRichGutter<'_>>,
    buf: &mut Buffer,
    inner: Rect,
    status_x: u16,
    y: u16,
    change_scope: &str,
    staged: bool,
    theme: &crate::theme::Theme,
) {
    match rich {
        Some(rg) => {
            let change = rg
                .colors
                .faded(style_rgb(theme.get(change_scope).fg).unwrap_or(rg.colors.dim));
            let staged_color = if staged {
                rg.colors.staged
            } else {
                rg.colors.unstaged
            };
            let x0 = ((status_x - inner.x) * 16) as i16;
            let y0 = ((y - inner.y) * 16) as i16;
            Bar {
                x: x0,
                y: y0,
                width: 5,
                height: 16,
                color: change,
            }
            .render(inner, buf, &mut *rg.scene);
            Bar {
                x: x0 + 7,
                y: y0,
                width: 5,
                height: 16,
                color: staged_color,
            }
            .render(inner, buf, &mut *rg.scene);
        },
        None => paint_status_bars(buf, status_x, y, change_scope, staged, theme),
    }
}

fn paint_status_gutter(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    status: ChunkStatus,
    is_current: bool,
    current_style: Style,
    theme: &crate::theme::Theme,
) {
    use crate::theme::scope as s;

    if x >= buf.area.x + buf.area.width {
        return;
    }
    if is_current {
        buf[(x, y)].set_char('│').set_style(current_style);
        return;
    }
    let (ch, style) = match status {
        ChunkStatus::Pending => (' ', theme.get(s::UI_TEXT_MUTED)),
        ChunkStatus::Staged => ('+', theme.get(s::DIFF_ADDED)),
        ChunkStatus::Unstaged => ('-', theme.get(s::DIFF_DELETED)),
        ChunkStatus::Skipped => ('~', theme.get(s::UI_TEXT_MUTED)),
    };
    buf[(x, y)].set_char(ch).set_style(style);
}

/// Paint a move-origin chip after the rendered side text to surface where
/// the moved hunk's counterpart lives.
///
/// A cross-file move (`path` is `Some`) paints `<- {path}:{line+1}`. An
/// intra-file move (`path` is `None`) paints a path-less `<- {line+1}`, since
/// repeating the row's own file name is noise. `text_cols` is the column count
/// already consumed by the row's text; the chip starts two columns later (so the
/// gap is visually obvious) and truncates if fewer columns remain. No-op when
/// `text_cols + 2 >= max_cols`.
#[allow(clippy::too_many_arguments)]
fn render_move_chip(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text_cols: usize,
    max_cols: usize,
    path: Option<&str>,
    line: u32,
    style: Style,
) {
    let chip_start_col = text_cols.saturating_add(2);
    if chip_start_col >= max_cols {
        return;
    }
    let chip = match path {
        Some(path) => format!("<- {}:{}", path, line + 1),
        None => format!("<- {}", line + 1),
    };
    let available = max_cols - chip_start_col;
    for (i, ch) in chip.chars().take(available).enumerate() {
        let x = start_x + (chip_start_col + i) as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }
        buf[(x, y)].set_char(ch).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::{BufferId, TextBuffer},
        diff_map::{ChangeSpan, DiffHunk, DiffHunkStatus, DiffMap, TokenDetail},
        theme::Theme,
    };
    use std::sync::{Arc, RwLock};
    use stoat_language::structural_diff;
    use stoat_scheduler::{Executor, TestScheduler};

    fn buffer_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.x + buf.area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// The review screen and the commit preview lay a side out as status glyph,
    /// line number, text, and split at every width.
    ///
    /// These are the values their two inline formulas produced before both
    /// folded into [`DiffColumns`]. The paint and the cursor read the same
    /// numbers, so a drift shows here rather than as a cursor off its column.
    #[test]
    fn the_review_layout_leads_with_the_status_glyph_and_never_unifies() {
        assert_eq!(
            DiffColumns::compute(Rect::new(3, 0, 121, 10), DiffLayout::REVIEW),
            DiffColumns {
                left_num_x: 4,
                status_left_x: 3,
                left_sep_x: None,
                left_text_x: 9,
                left_content_w: 54,
                right_num_x: 65,
                status_right_x: 64,
                right_sep_x: None,
                right_text_x: 70,
                right_content_w: 54,
                sep_x: Some(63),
            },
            "a wide review body splits either side of the mid-divider"
        );

        assert_eq!(
            DiffColumns::compute(Rect::new(0, 0, 40, 5), DiffLayout::REVIEW),
            DiffColumns {
                left_num_x: 1,
                status_left_x: 0,
                left_sep_x: None,
                left_text_x: 6,
                left_content_w: 13,
                right_num_x: 21,
                status_right_x: 20,
                right_sep_x: None,
                right_text_x: 26,
                right_content_w: 14,
                sep_x: Some(19),
            },
            "a review body stays two-column however narrow it gets"
        );
    }

    /// The diff view lays a side out as line number, status column, rule, text,
    /// and aliases both sides onto one column below [`DIFF_TWO_COLUMN_MIN`].
    #[test]
    fn the_diff_view_layout_rules_off_its_gutter_and_unifies_when_narrow() {
        assert_eq!(
            DiffColumns::compute(Rect::new(3, 0, 121, 10), DiffLayout::DIFF_VIEW),
            DiffColumns {
                left_num_x: 3,
                status_left_x: 8,
                left_sep_x: Some(10),
                left_text_x: 11,
                left_content_w: 52,
                right_num_x: 64,
                status_right_x: 69,
                right_sep_x: Some(71),
                right_text_x: 72,
                right_content_w: 52,
                sep_x: Some(63),
            },
            "a wide diff body puts base text left and buffer text right"
        );

        assert_eq!(
            DiffColumns::compute(Rect::new(0, 0, 40, 5), DiffLayout::DIFF_VIEW),
            DiffColumns {
                left_num_x: 0,
                status_left_x: 5,
                left_sep_x: Some(7),
                left_text_x: 8,
                left_content_w: 32,
                right_num_x: 0,
                status_right_x: 5,
                right_sep_x: Some(7),
                right_text_x: 8,
                right_content_w: 32,
                sep_x: None,
            },
            "a narrow diff body aliases both sides onto one column, no divider"
        );
    }

    /// A repaint that changed nothing reuses the derived rows, and a change to
    /// anything they were derived from rebuilds them.
    ///
    /// Reuse is what the cache is for, and every input in the key is there
    /// because the rows would otherwise be stale against it. A key missing one
    /// of them shows as a diff view that keeps painting the state before the
    /// edit, so each is moved on its own and checked to rebuild.
    #[test]
    fn row_states_are_reused_until_something_they_read_moves() {
        let mut editor = diff_editor("a\nb\nc\nd\n", "a\nB\nc\nD\n");
        let snapshot = editor.display_map.snapshot();
        let rows = 0..snapshot.line_count();

        let mut cache = None;
        diff_row_states(&snapshot, rows.clone(), &mut cache);
        let key = cache.as_ref().expect("built").key;

        // Marking the held entry with a status this text cannot produce for row
        // zero is what distinguishes reuse from a rebuild that lands on the same
        // answer. Comparing the rows alone would pass either way.
        cache.as_mut().expect("built").rows[0].status = DiffStatus::Moved;
        assert_eq!(
            diff_row_states(&snapshot, rows.clone(), &mut cache)[0].status,
            DiffStatus::Moved,
            "a repeat paint rebuilt the entry instead of reusing it",
        );
        assert_eq!(cache.as_ref().expect("held").key, key, "under the same key");

        // A narrower window is a different set of rows, so it must not be served
        // from the entry built for the wider one.
        diff_row_states(&snapshot, rows.start..rows.end - 1, &mut cache);
        assert_ne!(
            cache.as_ref().expect("held").key,
            key,
            "a shorter viewport rebuilds",
        );

        // The versions cannot be moved through the snapshot without rebuilding
        // one, so they are checked where the key is formed. Each is in it
        // because the rows go stale against it, and a key that dropped one would
        // keep painting the state from before that change.
        let base = diff_row_key(3, 20, 7, 11, 13, true);
        for moved in [
            diff_row_key(4, 20, 7, 11, 13, true),
            diff_row_key(3, 21, 7, 11, 13, true),
            diff_row_key(3, 20, 8, 11, 13, true),
            diff_row_key(3, 20, 7, 12, 13, true),
            diff_row_key(3, 20, 7, 11, 14, true),
        ] {
            assert_ne!(moved, base, "an input moved without changing the key");
        }
    }

    #[test]
    fn base_line_at_matches_the_reference_walk() {
        // The per-row walk, kept as the correctness oracle. A row carries a base
        // line when the left column paints one on it: every block row, every
        // unchanged row, and, where the layout pairs, a modified row still
        // inside its hunk's base text.
        fn reference(snapshot: &DisplaySnapshot, scroll_row: u32) -> u32 {
            let mut base_line = 0;
            for row in 0..scroll_row {
                match snapshot.classify_row(row) {
                    BlockRowKind::Block { .. } => base_line += 1,
                    BlockRowKind::BufferRow { buffer_row } => {
                        let status = snapshot.line_diff_status(buffer_row);
                        let paired = snapshot.pairs_modified_hunks()
                            && status == DiffStatus::Modified
                            && snapshot.diff_map().is_some_and(|dm| {
                                dm.hunks_in_range(buffer_row..buffer_row + 1)
                                    .iter()
                                    .any(|hunk| {
                                        hunk.status == DiffHunkStatus::Modified
                                            && buffer_row.saturating_sub(hunk.buffer_start_line)
                                                < dm.base_line_count(hunk)
                                    })
                            });
                        if status == DiffStatus::Unchanged || paired {
                            base_line += 1;
                        }
                    },
                }
            }
            base_line
        }

        // Each fixture is (base HEAD, buffer). Together they span a mid-file
        // modification with deletion, a deletion at the top, consecutive changes,
        // and a deletion at the tail, so the display carries added, deleted, and
        // modified hunks with deleted-base block rows spliced in.
        let fixtures = [
            ("a\nb\nc\nd\ne\nf\ng\nh\n", "a\nB\nc\nd\nINS\ng\nh\n"),
            ("x\ny\nz\nw\n", "z\nw\n"),
            ("1\n2\n3\n4\n5\n", "1\nTWO\nTHREE\n5\n"),
            ("p\nq\nr\ns\nt\n", "p\nq\nr\n"),
        ];
        // Both layouts, since pairing changes which rows carry a base line and
        // which of them block.
        let mut saw_blocks = false;
        let mut saw_pairs = false;
        for (base, text) in fixtures {
            let mut stacked_rows = 0;
            for pair in [false, true] {
                let mut editor = diff_editor(base, text);
                editor.display_map.set_pair_modified_hunks(pair);
                let snapshot = editor.display_map.snapshot();
                let total = snapshot.line_count();
                saw_blocks |= total > snapshot.buffer_line_count();
                match pair {
                    false => stacked_rows = total,
                    // Pairing puts a base row beside a live one rather than on a
                    // row of its own, so a fixture it reaches is shorter.
                    true => saw_pairs |= total < stacked_rows,
                }
                for row in 0..total {
                    assert_eq!(
                        base_line_at(&snapshot, row),
                        reference(&snapshot, row),
                        "base_line_at disagrees with the walk at row {row}/{total} \
                         for {base:?}->{text:?} paired={pair}"
                    );
                }
            }
        }
        assert!(
            saw_blocks,
            "fixtures must splice deleted-base block rows to exercise the block case"
        );
        assert!(saw_pairs, "and pair rows to exercise the paired case");
    }

    /// A diff-view editor over `text`, diffed against `base`, with the view and
    /// its deleted-block splicing enabled.
    fn diff_editor(base: &str, text: &str) -> EditorState {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        tb.diff_map = Some(DiffMap::from_structural_changes(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
        ));
        let shared = Arc::new(RwLock::new(tb));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
        editor.set_diff_view(true);
        editor
    }

    /// A diff-view editor over `text` with a hand-built diff map, for hunk shapes
    /// the structural differ will not synthesize from plain text (e.g. moves).
    fn diff_editor_with_map(text: &str, dm: DiffMap) -> EditorState {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        tb.diff_map = Some(dm);
        let shared = Arc::new(RwLock::new(tb));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
        editor.set_diff_view(true);
        editor
    }

    /// A minimal theme whose diff colors resolve to RGB, so the change washes
    /// engage when rendering a hand-built diff editor off the harness.
    fn rgb_diff_theme() -> Theme {
        let src = r##"theme rgbtest {
            diff.context.fg  = "#808080";
            diff.added.fg    = "#00ff00";
            diff.modified.fg = "#ffff00";
            diff.deleted.fg  = "#ff0000";
            diff.moved.fg    = "#0000ff";
            diff.staged.fg   = "#00ffff";
            diff.unstaged.fg = "#ff00ff";
            ui.background.bg = "#282c34";
        }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme config parses"), "rgbtest")
            .expect("rgb theme builds")
    }

    fn diff_editor_staged(base: &str, index: &str, text: &str) -> EditorState {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        let index_changed: Vec<std::ops::Range<u32>> = DiffMap::from_structural_changes(
            structural_diff::diff(index, text),
            Arc::new(index.to_string()),
            text,
        )
        .hunks_in_range(0..u32::MAX)
        .iter()
        .map(|h| h.buffer_line_range.clone())
        .collect();
        tb.diff_map = Some(DiffMap::from_structural_changes_staged(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
            &index_changed,
        ));
        let shared = Arc::new(RwLock::new(tb));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
        editor.set_diff_view(true);
        editor
    }

    #[test]
    fn diff_view_marks_staged_and_unstaged_hunks_in_the_status_column() {
        use crate::theme::scope as sc;

        // HEAD a/b/c/d; buffer changes line 1 (B) and line 3 (D); the index
        // holds only the line-1 change, so line 1 is staged, line 3 is not.
        let mut editor = diff_editor_staged("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n");
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        let theme = rgb_diff_theme();
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &theme,
            &mut buf,
            None,
            1.0,
        );

        // The right buffer status column follows its five-cell number gutter, so
        // it paints its change bar at right_start + 5 and its staged bar after.
        let change_col = ((120 - 1) / 2 + 1 + 5) as u16;
        let staged_col = change_col + 1;
        let staged_fg = theme.get(sc::DIFF_STAGED).fg.expect("staged fg");
        let unstaged_fg = theme.get(sc::DIFF_UNSTAGED).fg.expect("unstaged fg");

        let change_glyphs: String = (0..area.height)
            .map(|y| buf[(change_col, y)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            change_glyphs.contains('▎'),
            "changed rows show the change bar instead of +/-: {change_glyphs:?}"
        );

        let staged_colors: Vec<Color> = (0..area.height)
            .filter(|&y| buf[(staged_col, y)].symbol() == "▎")
            .map(|y| buf[(staged_col, y)].fg)
            .collect();
        assert!(
            staged_colors.contains(&staged_fg),
            "the staged hunk's bar uses the staged color: {staged_colors:?}"
        );
        assert!(
            staged_colors.contains(&unstaged_fg),
            "the unstaged hunk's bar uses the unstaged color: {staged_colors:?}"
        );
    }

    /// A refined hunk narrows the change to its span rows, and the status bars
    /// follow. Without this a reindent bars every row it moved, which is the
    /// clutter the refinement exists to remove.
    #[test]
    fn a_refined_hunk_bars_only_its_marked_rows() {
        // One hunk over the whole block, refined to its middle row alone.
        let text = "a\nb\nc\nd\n";
        let mut editor = {
            let executor = Executor::new(Arc::new(TestScheduler::new()));
            let mut tb = TextBuffer::with_text(BufferId::new(0), text);
            tb.diff_map = Some(DiffMap::from_hunks(
                [DiffHunk {
                    status: DiffHunkStatus::Modified,
                    buffer_start_line: 0,
                    buffer_line_range: 0..4,
                    base_byte_range: 0..8,
                    anchor_range: None,
                    token_detail: None,
                    unstaged_lines: std::iter::once(0..4).collect(),
                    marked_rows: std::iter::once(2..3).collect(),
                }],
                Some(Arc::new(text.to_string())),
            ));
            let shared = Arc::new(RwLock::new(tb));
            let mut editor =
                EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());
            editor.set_diff_view(true);
            editor
        };

        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        let theme = rgb_diff_theme();
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &theme,
            &mut buf,
            None,
            1.0,
        );

        let change_col = ((120 - 1) / 2 + 1 + 5) as u16;
        let barred: Vec<u16> = (0..area.height)
            .filter(|&y| buf[(change_col, y)].symbol() == "▎")
            .collect();
        // The diff view stacks base rows, so the screen row is found by what it
        // shows rather than assumed equal to the buffer row.
        let marked_screen_row = (0..area.height)
            .find(|&y| line_text(&buf, y, 68..120).trim() == "c")
            .expect("the marked buffer row is on screen");
        assert_eq!(
            barred,
            [marked_screen_row],
            "only the marked row bars, not every row the hunk spans"
        );
    }

    #[test]
    fn diff_view_rich_gutter_emits_bars_and_suppresses_ascii() {
        use stoatty_protocol::command::{encode_bar, BarCommand};

        fn contains(haystack: &[u8], needle: &[u8]) -> bool {
            haystack.windows(needle.len()).any(|w| w == needle)
        }

        let mut editor = diff_editor_staged("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n");
        let area = Rect::new(0, 0, 120, 8);
        let theme = rgb_diff_theme();

        let mut rich_buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &theme,
            &mut rich_buf,
            Some(&mut scene),
            1.0,
        );

        assert!(
            !scene.buffer().is_empty(),
            "rich mode emits sub-cell components"
        );

        let scroll = editor.scroll_row;
        let total = editor.display_map.snapshot().line_count();
        let rows = ((area.height as u32).min(total.saturating_sub(scroll))) as u16;
        let dim = [128, 128, 128];

        // Each side's gutter separator and the mid divider paint as hairlines.
        for sep_x in [7u16, 59, 67] {
            let frame = encode_bar(&BarCommand {
                x: (sep_x * 16 + 8) as i16,
                y: 0,
                width: 1,
                height: rows * 16,
                color: dim,
            });
            assert!(
                contains(scene.buffer(), &frame),
                "separator hairline at col {sep_x}"
            );
        }

        // A changed row emits a staged or unstaged bar in one of the two status
        // columns. Every row is searched rather than pinning the block layout.
        let staged = [[0, 255, 255], [255, 0, 255]];
        let staged_bar = (0..rows).any(|row| {
            [80u16 + 7, 65 * 16 + 7].iter().any(|&x| {
                staged.iter().any(|&color| {
                    contains(
                        scene.buffer(),
                        &encode_bar(&BarCommand {
                            x: x as i16,
                            y: (row * 16) as i16,
                            width: 5,
                            height: 16,
                            color,
                        }),
                    )
                })
            })
        });
        assert!(staged_bar, "a changed row emits a staged/unstaged bar");

        let has = |b: &Buffer, sym: &str| {
            (0..area.height).any(|y| (0..area.width).any(|x| b[(x, y)].symbol() == sym))
        };
        assert!(
            !has(&rich_buf, "▎"),
            "rich mode paints no ASCII status glyphs"
        );
        assert!(!has(&rich_buf, "│"), "rich mode paints no ASCII separators");

        let mut ascii_buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &theme,
            &mut ascii_buf,
            None,
            1.0,
        );
        assert!(has(&ascii_buf, "▎"), "the ASCII path paints status glyphs");
        assert!(has(&ascii_buf, "│"), "the ASCII path paints separators");
    }

    #[test]
    fn diff_rich_colors_fade_toward_the_background_with_the_dim_amount() {
        let theme = rgb_diff_theme();
        let full = resolve_diff_rich_colors(&theme, Style::default(), 0.0).expect("colors resolve");
        let faded =
            resolve_diff_rich_colors(&theme, Style::default(), 0.6).expect("colors resolve");

        assert_eq!(
            full.dim,
            [128, 128, 128],
            "no dim leaves the color unchanged"
        );
        assert_eq!(
            faded.dim,
            dim_rgb([128, 128, 128], full.bg, 0.6),
            "a pooled page fades the stored colors toward the background"
        );
        assert_eq!(faded.bg, full.bg, "the background itself is not faded");
    }

    #[test]
    fn diff_view_lays_out_base_left_buffer_right() {
        let mut editor = diff_editor("keep\nold\ntail\n", "keep\nnew\ntail\n");
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            None,
            1.0,
        );

        // Width 120 is wide enough for the two-column layout. Left text spans
        // cols 8..59, the mid-divider sits at col 59, right text spans 68..120.
        let left = |y| line_text(&buf, y, 8..59);
        let right = |y| line_text(&buf, y, 68..120);

        assert!(
            left(0).contains("keep") && right(0).contains("keep"),
            "row0 mirrors context: left={:?} right={:?}",
            left(0),
            right(0)
        );

        // The replaced line and its replacement share a row, which is what
        // makes the two sides read as aligned.
        assert!(
            left(1).contains("old") && right(1).contains("new"),
            "row1 pairs the base line with the one that replaced it: left={:?} right={:?}",
            left(1),
            right(1)
        );

        assert!(
            left(2).contains("tail") && right(2).contains("tail"),
            "row2 context mirrors both sides: left={:?} right={:?}",
            left(2),
            right(2)
        );

        assert_eq!(
            (left(3).trim(), right(3).trim()),
            ("", ""),
            "and the file ends there, with no row left over for the change",
        );

        assert_eq!(
            buf[(59, 0)].symbol(),
            "│",
            "the two columns are split by a separator"
        );
        assert_eq!(
            (buf[(7, 0)].symbol(), buf[(67, 0)].symbol()),
            ("│", "│"),
            "each side carries a gutter/code separator after its status column"
        );
    }

    /// The rows a length difference costs belong at the end of the hunk, not
    /// scattered through it, so the base rows pair as far as they can and only
    /// the excess takes rows of its own.
    #[test]
    fn a_longer_base_hunk_pairs_then_trails_its_excess() {
        let mut editor = diff_editor("keep\nold1\nold2\nold3\ntail\n", "keep\nnew1\ntail\n");
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            None,
            1.0,
        );

        let left = |y| line_text(&buf, y, 8..59);
        let right = |y| line_text(&buf, y, 68..120);

        assert!(
            left(1).contains("old1") && right(1).contains("new1"),
            "row1 pairs the first base row with the live one: left={:?} right={:?}",
            left(1),
            right(1)
        );
        assert!(
            left(2).contains("old2") && right(2).trim().is_empty(),
            "row2 trails the first excess base row: left={:?} right={:?}",
            left(2),
            right(2)
        );
        assert!(
            left(3).contains("old3") && right(3).trim().is_empty(),
            "row3 the second: left={:?} right={:?}",
            left(3),
            right(3)
        );
        assert!(
            left(4).contains("tail") && right(4).contains("tail"),
            "row4 is context again: left={:?} right={:?}",
            left(4),
            right(4)
        );

        // The left numbers count the base file straight down the column, which
        // is what a reader checks an alignment against. The status glyphs share
        // the gutter, so only the number itself is read.
        let number = |y| {
            line_text(&buf, y, 0..7)
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_owned()
        };
        assert_eq!(
            (0..5).map(number).collect::<Vec<_>>(),
            ["1", "2", "3", "4", "5"],
            "the base line numbers read 1..5 down the left column",
        );
    }

    /// The other way round: more live rows than base rows leaves the left column
    /// blank past the pairing, which the paint gives without a block.
    #[test]
    fn a_longer_live_hunk_blanks_its_trailing_left_rows() {
        let mut editor = diff_editor("keep\nold\ntail\n", "keep\nnew1\nnew2\ntail\n");
        let area = Rect::new(0, 0, 120, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            None,
            1.0,
        );

        let left = |y| line_text(&buf, y, 8..59);
        let right = |y| line_text(&buf, y, 68..120);

        assert!(
            left(1).contains("old") && right(1).contains("new1"),
            "row1 pairs the one base row with the first live one: left={:?} right={:?}",
            left(1),
            right(1)
        );
        assert!(
            left(2).trim().is_empty() && right(2).contains("new2"),
            "row2 has no base row to show: left={:?} right={:?}",
            left(2),
            right(2)
        );
        assert!(
            left(3).contains("tail") && right(3).contains("tail"),
            "row3 is context again: left={:?} right={:?}",
            left(3),
            right(3)
        );

        assert_eq!(
            line_text(&buf, 3, 0..7).trim(),
            "3",
            "and the base file has only three lines, so the context is its third",
        );
    }

    #[test]
    fn narrow_diff_renders_a_unified_single_column() {
        let mut editor = diff_editor("keep\nold\ntail\n", "keep\nnew\ntail\n");
        let area = Rect::new(0, 0, 40, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            None,
            1.0,
        );

        // The gutter/code separator is painted at col 7, but a unified view has
        // no two-column mid-divider, so no rule appears in the text region.
        let sep_col = area.x + 7;
        assert!(
            (0..area.height).any(|y| buf[(sep_col, y)].symbol() == "│"),
            "the gutter/code separator is painted in the unified view"
        );
        for y in 0..area.height {
            for x in (area.x + 8)..area.width {
                assert_ne!(
                    buf[(x, y)].symbol(),
                    "│",
                    "a unified diff paints no mid-divider past the gutter"
                );
            }
        }

        // The single text column sits past the number, status, and separator
        // gutter, and both sides render into it.
        let col = right_text_x(area);
        assert_eq!(col, area.x + 8, "unified text starts past the one gutter");

        let renders = |needle: &str| {
            (0..area.height).any(|y| {
                let row: String = (col..area.width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect();
                row.contains(needle)
            })
        };
        assert!(renders("old"), "the deleted base line renders inline");
        assert!(renders("new"), "the added buffer line renders inline");
    }

    #[test]
    fn typing_in_diff_view_edits_the_real_buffer() {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(40, 8);
        let path = h.write_file("a.txt", "abc\n");
        h.open_file(&path);
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);

        h.type_keys("i");
        h.type_text("X");

        let text = focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .display_map
            .snapshot()
            .buffer_snapshot()
            .rope()
            .to_string();
        assert!(
            text.starts_with('X'),
            "inserting in the diff view lands in the real buffer: {text:?}"
        );
    }

    #[test]
    fn diff_view_right_column_carries_syntax_colors() {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(120, 10);
        let path = h.write_file("a.rs", "fn main() {}\n");
        h.open_file(&path);
        h.stoat.drive_background();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        h.snapshot();

        // At width 120 the right text begins at col 67. A syntax-highlighted right
        // column paints more than one foreground color across the row's tokens.
        let buf = h.rendered_buffer();
        let mut colors = std::collections::HashSet::new();
        for x in 67..120 {
            let cell = &buf[(x, 0)];
            if cell.symbol().trim().is_empty() {
                continue;
            }
            colors.insert(format!("{:?}", cell.style().fg));
        }
        assert!(
            colors.len() >= 2,
            "the right column is syntax highlighted with distinct token colors: {colors:?}"
        );
    }

    #[test]
    fn diff_view_left_column_carries_base_token_colors() {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(120, 10);
        // The base carries rust keywords. The buffer differs, so the base line
        // renders as a deleted block in the left column.
        h.stage_review_scenario("/repo", &[("a.rs", "fn main() {}\n", "fn other() {}\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(std::path::Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        h.snapshot();

        // At width 120 left text spans cols 7..59 past the two-cell status and
        // five-cell line-number gutter.
        let buf = h.rendered_buffer();
        let mut colors = std::collections::HashSet::new();
        for y in 0..buf.area.height {
            for x in 7..59 {
                let cell = &buf[(x, y)];
                if cell.symbol().trim().is_empty() {
                    continue;
                }
                colors.insert(format!("{:?}", cell.style().fg));
            }
        }
        assert!(
            colors.len() >= 2,
            "the base column carries token colors plus the deletion fallback: {colors:?}"
        );
    }

    /// A width-120 diff-view harness over one `.rs` file diffed `base` ->
    /// `buffer`, rendered once. The default theme resolves diff colors to RGB, so
    /// the change washes engage.
    ///
    /// Width 120 stays above the two-column threshold. Left text spans cols 7..59,
    /// the separator sits at col 59, right text spans cols 67..120.
    fn diff_harness(base: &str, buffer: &str) -> crate::test_harness::TestHarness {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(120, 10);
        // The wash scans below address fixed side-by-side columns, so the pane must
        // span the full width. The single-minimap strip would reserve the right
        // edge and shift the columns.
        h.stoat.minimap_override = Some(false);
        h.stage_review_scenario("/repo", &[("a.rs", base, buffer)]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(std::path::Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        h.snapshot();
        h
    }

    /// The glyphs of row `y` across `cols`, for locating a rendered line.
    fn line_text(buf: &Buffer, y: u16, cols: std::ops::Range<u16>) -> String {
        cols.map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// The changed word is marked by everything around it receding, not by a
    /// color behind it, so a refined row is one flat stretch of the editor
    /// background from end to end. See
    /// `diff_view_softens_the_unchanged_gaps_of_a_refined_row` for the receding
    /// half of that.
    #[test]
    fn diff_view_paints_no_background_on_a_refined_row() {
        // `main` becomes `other`, so only that one word changed on the line.
        let h = diff_harness("fn main() {}\n", "fn other() {}\n");
        let canvas = h
            .stoat
            .theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg)
            .expect("an rgb editor background");
        let buf = h.rendered_buffer();

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 68..buf.area.width).contains("other"))
            .expect("the changed line rendered on the right");
        assert!(
            line_text(buf, row, 8..59).contains("main"),
            "and its base word beside it: {:?}",
            line_text(buf, row, 8..59)
        );

        let painted = (0..buf.area.width)
            .filter(|&x| buf[(x, row)].bg != canvas)
            .collect::<Vec<_>>();
        assert_eq!(
            painted,
            Vec::<u16>::new(),
            "no cell of the row carries a background of its own",
        );
    }

    #[test]
    fn diff_view_softens_the_unchanged_gaps_of_a_refined_row() {
        // `main` becomes `other` on the first line, so that line is refined.
        // The second is a pure insertion, which nothing refines, so its
        // identical `fn` keyword stays at full strength to compare against.
        let h = diff_harness("fn main() {}\n", "fn other() {}\nfn extra() {}\n");

        let bg = style_rgb(
            h.stoat
                .theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
        .expect("rgb background");
        let buf = h.rendered_buffer();

        let row_with = |text: &str| {
            (0..buf.area.height)
                .find(|&y| line_text(buf, y, 68..buf.area.width).contains(text))
                .unwrap_or_else(|| panic!("{text:?} rendered in the right column"))
        };
        let fg_of = |y: u16, glyph: &str| {
            let x = (68..buf.area.width)
                .find(|&x| buf[(x, y)].symbol() == glyph)
                .unwrap_or_else(|| panic!("{glyph:?} on row {y}"));
            buf[(x, y)].style().fg.expect("an fg")
        };

        let refined = row_with("fn other");
        let added = row_with("fn extra");

        let full_keyword = fg_of(added, "f");
        let [r, g, b] = dim_rgb(
            style_rgb(Some(full_keyword)).expect("rgb fg on the added row"),
            bg,
            MODIFIED_ROW_SOFTEN,
        );

        assert_eq!(
            (fg_of(refined, "f"), fg_of(refined, "o")),
            (Color::Rgb(r, g, b), fg_of(added, "e")),
            "the gaps blend toward the background while the changed word stays full"
        );
        assert_ne!(
            fg_of(refined, "f"),
            full_keyword,
            "the gap blend moves the color off full strength"
        );
    }

    /// Inside a string the whole literal carries one color, so the receding
    /// around a changed char is all that separates it. Bold gives the eye
    /// something to land on that the color cannot.
    #[test]
    fn diff_view_bolds_a_changed_char_inside_a_string() {
        // The two literals share most of their text, so the search reads them
        // as one edited string. Bold marks the case the reader compares char by
        // char, which is what a pair means and a delete beside an add does not.
        let h = diff_harness(
            "fn f() {\n    g(\"one aaa three\");\n}\n",
            "fn f() {\n    g(\"one zzz three\");\n}\n",
        );
        let buf = h.rendered_buffer();

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 68..buf.area.width).contains("zzz"))
            .expect("the changed line rendered on the right");
        let bold = |cols: std::ops::Range<u16>| {
            cols.filter(|&x| buf[(x, row)].modifier.contains(Modifier::BOLD))
                .map(|x| buf[(x, row)].symbol().to_string())
                .collect::<String>()
        };
        assert_eq!(
            (bold(68..buf.area.width), bold(8..59)),
            ("zzz".to_string(), "aaa".to_string()),
            "both columns bold their changed word and nothing else of the literal"
        );
    }

    /// The search reads two similar comments as a pair rather than as two
    /// unrelated runs, and the pair has to reach the reader. Without it the row
    /// arrives gutter-marked with nothing receding and nothing bold, saying
    /// only that the line changed somewhere.
    #[test]
    fn diff_view_marks_the_changed_word_of_a_similar_comment() {
        let h = diff_harness("// step aaa here\n", "// step zzz here\n");
        let buf = h.rendered_buffer();

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 68..buf.area.width).contains("zzz"))
            .expect("the edited comment rendered on the right");
        let bold = |cols: std::ops::Range<u16>| {
            cols.filter(|&x| buf[(x, row)].modifier.contains(Modifier::BOLD))
                .map(|x| buf[(x, row)].symbol().to_string())
                .collect::<String>()
        };
        assert_eq!(
            (bold(68..buf.area.width), bold(8..59)),
            ("zzz".to_string(), "aaa".to_string()),
            "each column marks its own changed word and leaves the rest of the comment alone"
        );
    }

    /// Bold says "these chars differ from the ones beside them in the other
    /// text". Added prose has no counterpart to differ from, so it stays plain
    /// however unstructured it is.
    #[test]
    fn diff_view_keeps_an_added_comment_unbolded() {
        // The buffer opens two changed runs against the base's one, so the
        // pairing pass claims `h` and leaves the comment with no counterpart.
        let h = diff_harness(
            "fn f() {\n    g();\n}\n",
            "fn f() {\n    h();\n    // note here\n}\n",
        );
        let buf = h.rendered_buffer();

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 68..buf.area.width).contains("note here"))
            .expect("the added comment rendered on the right");
        let bold = (68..buf.area.width)
            .filter(|&x| buf[(x, row)].modifier.contains(Modifier::BOLD))
            .map(|x| buf[(x, row)].symbol().to_string())
            .collect::<String>();
        assert_eq!(bold, "", "an added comment carries no bold");
    }

    /// A renamed identifier already has a token boundary and a color change
    /// beside it, so bolding it would mark what the reader can see.
    #[test]
    fn diff_view_keeps_a_renamed_identifier_unbolded() {
        let h = diff_harness("fn alpha() {}\n", "fn beta() {}\n");
        let buf = h.rendered_buffer();

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 68..buf.area.width).contains("beta"))
            .expect("the renamed line rendered on the right");
        let bold = (68..buf.area.width)
            .filter(|&x| buf[(x, row)].modifier.contains(Modifier::BOLD))
            .map(|x| buf[(x, row)].symbol().to_string())
            .collect::<String>();
        assert_eq!(bold, "", "nothing on a code row bolds");
    }

    /// A theme that cannot blend has no receding to lead a change with, so the
    /// view asks for the one mark left to it and underlines the changed chars.
    /// `paint_base_row_underlines_change_spans_on_a_theme_that_cannot_blend`
    /// pins the paint; this pins that the view asks for it.
    #[test]
    fn diff_view_underlines_a_change_span_on_a_theme_that_cannot_blend() {
        // `main` becomes `other`, so only that one word changed on the line.
        let mut h = diff_harness("fn main() {}\n", "fn other() {}\n");
        h.stoat.theme = Arc::new(Theme::empty());
        h.snapshot();
        let buf = h.rendered_buffer();

        let underlined = (0..buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .filter(|&pos| buf[pos].modifier.contains(Modifier::UNDERLINED))
            .map(|pos| buf[pos].symbol().to_string())
            .collect::<String>();
        assert_eq!(
            underlined, "mainother",
            "the changed word is underlined on both sides, and nothing else is"
        );
    }

    #[test]
    fn diff_view_softens_context_row_foregrounds() {
        // The first line is untouched, so it renders as a context row. The
        // second is a pure insertion, which nothing softens, so its identical
        // `fn` keyword is the full-strength reference.
        let h = diff_harness("fn keep() {}\n", "fn keep() {}\nfn add() {}\n");

        let bg = style_rgb(
            h.stoat
                .theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
        .expect("rgb background");
        let buf = h.rendered_buffer();

        let row_with = |text: &str| {
            (0..buf.area.height)
                .find(|&y| line_text(buf, y, 68..buf.area.width).contains(text))
                .unwrap_or_else(|| panic!("{text:?} rendered in the right column"))
        };
        let leading_glyph = |y: u16| {
            let x = (68..buf.area.width)
                .find(|&x| !buf[(x, y)].symbol().trim().is_empty())
                .expect("a glyph on the row");
            (buf[(x, y)].symbol().to_string(), buf[(x, y)].style().fg)
        };

        let (added_glyph, added_fg) = leading_glyph(row_with("fn add"));
        let (context_glyph, context_fg) = leading_glyph(row_with("fn keep"));
        let [r, g, b] = dim_rgb(
            style_rgb(added_fg).expect("rgb fg on the added row"),
            bg,
            CONTEXT_SOFTEN,
        );

        assert_eq!(
            (added_glyph.as_str(), context_glyph.as_str(), context_fg),
            ("f", "f", Some(Color::Rgb(r, g, b))),
            "the context row's keyword blends the added row's color toward the background"
        );
        assert_ne!(
            context_fg, added_fg,
            "the blend moves the context color off the full-strength one"
        );
    }

    /// The level reaches the paint through the whole live chain, which is what
    /// makes it a knob rather than a field.
    #[test]
    fn the_soften_level_scales_how_far_a_context_row_recedes() {
        let mut h = diff_harness("fn keep() {}\n", "fn keep() {}\nfn add() {}\n");

        let bg = style_rgb(
            h.stoat
                .theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
        .expect("rgb background");

        let context_fg = |h: &mut crate::test_harness::TestHarness| {
            h.snapshot();
            let buf = h.rendered_buffer();
            let y = (0..buf.area.height)
                .find(|&y| line_text(buf, y, 68..buf.area.width).contains("fn keep"))
                .expect("the context row renders");
            let x = (68..buf.area.width)
                .find(|&x| !buf[(x, y)].symbol().trim().is_empty())
                .expect("a glyph on the row");
            buf[(x, y)].style().fg
        };

        let shipped = context_fg(&mut h);

        h.stoat.diff_soften = DIFF_SOFTEN_MIN;
        let stopped = context_fg(&mut h);

        h.stoat.diff_soften = DIFF_SOFTEN_MAX;
        let deepest = context_fg(&mut h);

        let blend = |amount: f32| {
            let full = style_rgb(stopped).expect("rgb fg with no soften applied");
            let [r, g, b] = dim_rgb(full, bg, amount);
            Some(Color::Rgb(r, g, b))
        };

        assert_eq!(
            (shipped, deepest),
            (
                blend(CONTEXT_SOFTEN),
                blend((CONTEXT_SOFTEN * diff_soften_scale(DIFF_SOFTEN_MAX)).min(SOFTEN_CAP)),
            ),
            "level 0 paints the shipped fraction and the top level lands on the floor",
        );
        assert_ne!(
            stopped, shipped,
            "the bottom level stops the blend rather than shrinking it",
        );
    }

    /// The top level is chosen to be the last one that still moves a context
    /// row. Raising either the level or the fraction past this point buys a
    /// keypress that paints nothing, so both constants are pinned together.
    #[test]
    fn the_deepest_soften_level_is_the_last_one_that_moves_a_context_row() {
        let below = CONTEXT_SOFTEN * diff_soften_scale(DIFF_SOFTEN_MAX - 1);
        let deepest = CONTEXT_SOFTEN * diff_soften_scale(DIFF_SOFTEN_MAX);

        assert!(
            below < SOFTEN_CAP,
            "the step before the top still has room to move: {below} is not under {SOFTEN_CAP}",
        );
        assert!(
            deepest >= SOFTEN_CAP,
            "the top step reaches the floor: {deepest} is under {SOFTEN_CAP}",
        );
    }

    #[test]
    fn diff_view_added_line_takes_no_line_wash() {
        // The second line is a pure insertion, so nothing is refined.
        let h = diff_harness("fn a() {}\n", "fn a() {}\nfn b() {}\n");

        let buf = h.rendered_buffer();

        let underlined = (0..buf.area.height).any(|y| {
            (0..buf.area.width).any(|x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED))
        });
        assert!(!underlined, "a pure added line underlines nothing");

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 68..120).contains("fn b"))
            .expect("added line rendered on the right");
        let context = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 68..120).contains("fn a"))
            .expect("context line rendered on the right");
        assert!(
            (68..120).all(|x| buf[(x, row)].bg == buf[(x, context)].bg),
            "the added line's cells carry the same background as an unchanged row"
        );
    }

    #[test]
    fn diff_view_deleted_line_takes_no_line_wash() {
        // `old` is deleted, so it renders as a base-only block row on the left.
        let h = diff_harness("keep\nold\ntail\n", "keep\ntail\n");

        let buf = h.rendered_buffer();

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 8..59).contains("old"))
            .expect("deleted base line rendered on the left");
        let context = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 8..59).contains("keep"))
            .expect("unchanged base line mirrored on the left");
        assert!(
            (8..59).all(|x| buf[(x, row)].bg == buf[(x, context)].bg),
            "the deleted base line's cells carry the same background as a mirrored row"
        );
    }

    #[test]
    fn diff_view_unchanged_mirrored_base_row_carries_no_background() {
        // `keep` is unchanged and mirrored into the left column.
        let h = diff_harness("keep\nold\ntail\n", "keep\ntail\n");
        let canvas = h
            .stoat
            .theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg)
            .expect("an rgb editor background");
        let buf = h.rendered_buffer();

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 7..59).contains("keep"))
            .expect("unchanged base line mirrored on the left");
        assert!(
            (7..59).all(|x| buf[(x, row)].bg == canvas),
            "an unchanged mirrored base row paints no background of its own"
        );
    }

    /// A moved span is marked the same way a replaced one is, by the receding
    /// around it, so it paints no background either.
    #[test]
    fn diff_view_moved_line_carries_no_background() {
        // A Moved hunk covering buffer line 1 ("bb", bytes 3..5); no base bytes,
        // so the line stays in place on the right rather than splicing a block.
        let dm = {
            let detail = Arc::new(TokenDetail {
                buffer_spans: vec![ChangeSpan {
                    byte_range: 3..5,
                    kind: ChangeKind::Moved,
                    move_metadata: None,
                    prose: false,
                }],
                base_spans: Vec::new(),
            });
            DiffMap::from_hunks(
                [DiffHunk {
                    status: DiffHunkStatus::Moved,
                    unstaged_lines: std::iter::once(1..2).collect(),
                    marked_rows: Vec::new(),
                    buffer_start_line: 1,
                    buffer_line_range: 1..2,
                    base_byte_range: 0..0,
                    anchor_range: None,
                    token_detail: Some(detail),
                }],
                None,
            )
        };
        let mut editor = diff_editor_with_map("aa\nbb\ncc\n", dm);
        let theme = rgb_diff_theme();
        let area = Rect::new(0, 0, 40, 5);
        let mut buf = Buffer::empty(area);
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        render_diff_view(&mut editor, area, fallback, &theme, &mut buf, None, 1.0);

        // At this sub-threshold width the diff is unified, so the moved buffer
        // row "bb" paints at the single text column. It sits on display row 1.
        let rx = right_text_x(area);
        assert_eq!(
            buf[(rx, 1)].symbol(),
            "b",
            "the moved line renders on the right"
        );
        assert!(
            (rx..area.width).all(|x| buf[(x, 1)].bg == buf[(x, 0)].bg),
            "and every cell of it carries what an unchanged row carries",
        );
    }

    #[test]
    fn a_row_without_spans_clears_what_the_row_before_it_left() {
        let detail = Arc::new(TokenDetail {
            buffer_spans: vec![ChangeSpan {
                byte_range: 3..5,
                kind: ChangeKind::Replaced,
                move_metadata: None,
                prose: false,
            }],
            base_spans: Vec::new(),
        });
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Modified,
                unstaged_lines: std::iter::once(1..2).collect(),
                marked_rows: Vec::new(),
                buffer_start_line: 1,
                buffer_line_range: 1..2,
                base_byte_range: 0..0,
                anchor_range: None,
                token_detail: Some(detail),
            }],
            None,
        );
        let mut editor = diff_editor_with_map("aa\nbb\ncc\n", dm);
        let snapshot = editor.display_map.snapshot();

        let mut hunks = Vec::new();
        let mut spans = Vec::new();

        write_buffer_row_change_spans(&snapshot, 1, &mut hunks, &mut spans);
        assert_eq!(
            spans,
            vec![(0..2, ChangeKind::Replaced, false)],
            "the modified row reports the span covering its changed bytes",
        );

        write_buffer_row_change_spans(&snapshot, 2, &mut hunks, &mut spans);
        assert_eq!(
            spans,
            Vec::new(),
            "the unmodified row after it reports nothing of its own",
        );
    }

    /// A diff editor whose buffer line 1 ("bb") is a move whose counterpart is
    /// `source`, for exercising the move-origin chip.
    fn moved_editor(source: structural_diff::MoveSource) -> EditorState {
        let detail = Arc::new(TokenDetail {
            buffer_spans: vec![ChangeSpan {
                byte_range: 3..5,
                kind: ChangeKind::Moved,
                move_metadata: Some(Arc::new(structural_diff::MoveMetadata {
                    sources: vec![source],
                })),
                prose: false,
            }],
            base_spans: Vec::new(),
        });
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Moved,
                unstaged_lines: std::iter::once(1..2).collect(),
                marked_rows: Vec::new(),
                buffer_start_line: 1,
                buffer_line_range: 1..2,
                base_byte_range: 0..0,
                anchor_range: None,
                token_detail: Some(detail),
            }],
            None,
        );
        diff_editor_with_map("aa\nbb\ncc\n", dm)
    }

    #[test]
    fn diff_view_moved_row_shows_a_cross_file_origin_chip() {
        use structural_diff::{BufferRef, MoveSource, Side};

        let mut editor = moved_editor(MoveSource {
            buffer: Some(BufferRef {
                path: std::path::PathBuf::from("src/b.rs"),
                fingerprint: [0u8; 32],
            }),
            side: Side::Lhs,
            byte_range: 0..0,
            line_range: 3..4,
        });
        let area = Rect::new(0, 0, 60, 5);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            None,
            1.0,
        );

        let row = buffer_text(&buf, 1);
        assert!(
            row.contains("<- b.rs:4"),
            "a cross-file moved row shows the origin file:line chip; got {row:?}"
        );
    }

    #[test]
    fn diff_view_moved_row_shows_an_intra_file_origin_chip() {
        use structural_diff::{MoveSource, Side};

        let mut editor = moved_editor(MoveSource {
            buffer: None,
            side: Side::Lhs,
            byte_range: 0..0,
            line_range: 4..5,
        });
        let area = Rect::new(0, 0, 60, 5);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            None,
            1.0,
        );

        let row = buffer_text(&buf, 1);
        assert!(
            row.contains("<- 5") && !row.contains(':'),
            "an intra-file moved row shows a path-less chip; got {row:?}"
        );
    }

    /// A theme that can blend marks a change span by receding everything else,
    /// so the span itself is left exactly as it was: no background of its own,
    /// and no underline either.
    #[test]
    fn paint_base_row_leaves_change_spans_unwashed_on_an_rgb_theme() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let change_spans = vec![
            (0..3, ChangeKind::Replaced, false),
            (3..6, ChangeKind::Moved, false),
        ];
        paint_base_row(
            &mut buf,
            0,
            0,
            "abcdefgh",
            8,
            &[],
            Style::default(),
            &change_spans,
            true,
            None,
            None,
            1.0,
        );

        for x in 0..8 {
            assert_eq!(
                (buf[(x, 0)].bg, buf[(x, 0)].modifier),
                (Color::Reset, Modifier::empty()),
                "col {x} carries neither a background nor an underline",
            );
        }
    }

    /// A zero scale must skip the soften call rather than pass it a zero
    /// amount, because that call drops bold whatever amount it is given.
    #[test]
    fn a_zero_soften_scale_leaves_the_row_untouched() {
        let bold = Style::default()
            .fg(Color::Rgb(200, 100, 50))
            .add_modifier(Modifier::BOLD);
        let bg = [0, 0, 0];

        let paint = |scale: f32| {
            let mut buf = Buffer::empty(Rect::new(0, 0, 4, 1));
            paint_base_row(
                &mut buf,
                0,
                0,
                "word",
                4,
                &[],
                bold,
                &[],
                true,
                Some(bg),
                None,
                scale,
            );
            (buf[(0, 0)].style().fg, buf[(0, 0)].modifier)
        };

        assert_eq!(
            paint(0.0),
            (Some(Color::Rgb(200, 100, 50)), Modifier::BOLD),
            "a stopped soften keeps both the color and the weight",
        );

        let [r, g, b] = dim_rgb([200, 100, 50], bg, CONTEXT_SOFTEN * 2.0);
        assert_eq!(
            paint(2.0),
            (Some(Color::Rgb(r, g, b)), Modifier::empty()),
            "a doubled soften blends twice as far and drops the weight",
        );
    }

    #[test]
    fn paint_base_row_underlines_change_spans_on_a_theme_that_cannot_blend() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let change_spans = vec![
            (0..3, ChangeKind::Replaced, false),
            (3..6, ChangeKind::Moved, false),
        ];
        paint_base_row(
            &mut buf,
            0,
            0,
            "abcdefgh",
            8,
            &[],
            Style::default(),
            &change_spans,
            false,
            None,
            None,
            1.0,
        );

        for x in 0..6 {
            assert!(
                buf[(x, 0)].modifier.contains(Modifier::UNDERLINED),
                "col {x} underlines when the theme cannot blend"
            );
            assert_eq!(
                buf[(x, 0)].bg,
                Color::Reset,
                "and still paints no background"
            );
        }
        assert!(
            !buf[(6, 0)].modifier.contains(Modifier::UNDERLINED),
            "a char outside every span is not underlined"
        );
    }

    #[test]
    fn move_chip_paints_text_after_two_col_gap() {
        let area = Rect::new(0, 0, 50, 1);
        let mut buf = Buffer::empty(area);
        render_move_chip(&mut buf, 0, 0, 5, 50, Some("a.rs"), 0, Style::default());
        let text = buffer_text(&buf, 0);
        assert_eq!(&text[..7], "       ", "5-col text + 2-col gap before chip");
        assert_eq!(&text[7..16], "<- a.rs:1", "chip text follows the gap");
    }

    #[test]
    fn move_chip_no_op_when_text_fills_max_cols() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        render_move_chip(
            &mut buf,
            0,
            0,
            19,
            20,
            Some("long_name.rs"),
            100,
            Style::default(),
        );
        let text = buffer_text(&buf, 0);
        assert!(
            !text.contains("<-"),
            "chip must not paint when text fills max_cols; got {text:?}"
        );
    }

    #[test]
    fn move_chip_truncates_when_room_runs_out() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        render_move_chip(
            &mut buf,
            0,
            0,
            5,
            20,
            Some("long_name.rs"),
            99,
            Style::default(),
        );
        let text = buffer_text(&buf, 0);
        // text_cols=5 + 2-col gap = chip starts at col 7; max_cols=20 leaves 13 cols.
        // "<- long_name.rs:100" is 19 chars; truncated to 13: "<- long_name.".
        assert_eq!(&text[7..20], "<- long_name.", "chip truncates to fit");
    }

    #[test]
    fn move_chip_uses_one_based_line_number() {
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        render_move_chip(&mut buf, 0, 0, 0, 30, Some("x.rs"), 41, Style::default());
        let text = buffer_text(&buf, 0);
        assert!(
            text.contains("<- x.rs:42"),
            "chip prints 1-based line number; got {text:?}"
        );
    }

    #[test]
    fn move_chip_intra_file_omits_the_path() {
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        render_move_chip(&mut buf, 0, 0, 0, 30, None, 41, Style::default());
        let text = buffer_text(&buf, 0);
        assert!(
            text.contains("<- 42") && !text.contains(':'),
            "intra-file chip shows the 1-based line without a path; got {text:?}"
        );
    }
}
