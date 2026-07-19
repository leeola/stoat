use super::TEXT_SCALE_COMPACT;
use crate::{
    diff_map::ChangeKind,
    display_map::{
        highlights::{HighlightEndpoint, HighlightStyle},
        BlockRowKind, DisplaySnapshot,
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
use std::sync::Arc;
use stoat_text::{cursor_offset, Point};
use stoatty_widgets::{bar::Bar, text_run::TextRun, ApcScene};

/// Fraction a changed line's background wash blends toward the editor
/// background, leaving 10% of the diff color. Light enough that code stays
/// readable through the wash that marks the whole line.
const LINE_TINT: f32 = 0.90;

/// Fraction an intraline change span's wash blends toward the background,
/// leaving 28% of the diff color. Stronger than [`LINE_TINT`] so the exact
/// changed chars stand out within an already-washed line.
const SPAN_TINT: f32 = 0.72;

/// Line wash for content already applied to the git index, leaving 5% of the
/// diff color. One step past [`LINE_TINT`] toward the background so staged
/// changes read as receding while unstaged ones stay vivid.
const STAGED_LINE_TINT: f32 = 0.95;

/// Span wash for content already applied to the git index, leaving 14% of the
/// diff color. The staged counterpart to [`SPAN_TINT`], receding a step further
/// like [`STAGED_LINE_TINT`].
const STAGED_SPAN_TINT: f32 = 0.86;

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
    render_review_cursor(editor, &snapshot, inner, theme, buf, stoatty);
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
/// Reuses [`render_review_rows`]'s two-column geometry and the ASCII gutter
/// path. The rich sub-cell gutter is not engaged here.
pub(crate) fn render_diff_view(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    stoatty: bool,
) {
    let snapshot = editor.display_map.snapshot();
    paint_diff_rows(
        &snapshot,
        editor.scroll_row,
        inner,
        fallback_style,
        theme,
        buf,
    );
    render_review_cursor(editor, &snapshot, inner, theme, buf, stoatty);
}

/// Paint the two-column diff body for the rows visible from `scroll_row`, base
/// text left and buffer text right.
///
/// Shared by the live [`render_diff_view`] and the off-loop smooth-scroll page
/// so both paint an identical grid. It takes owned parts and paints no cursor,
/// letting a pooled page render it on a blocking worker.
pub(crate) fn paint_diff_rows(
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let total_rows = snapshot.line_count();
    let visible = inner.height as u32;
    let end_row = (scroll_row + visible).min(total_rows);
    if end_row <= scroll_row {
        return;
    }

    let full_w = inner.width as usize;
    let status_w: usize = 2;
    let num_w: usize = 5;
    let gutter_w = status_w + num_w;
    let sep: usize = 1;
    let half_w = (full_w.saturating_sub(sep)) / 2;
    let left_content_w = half_w.saturating_sub(gutter_w);
    let right_start = inner.x + half_w as u16 + sep as u16;
    let right_content_w = (full_w - half_w - sep).saturating_sub(gutter_w);

    let left_num_x = inner.x + status_w as u16;
    let left_text_x = left_num_x + num_w as u16;
    let right_num_x = right_start + status_w as u16;
    let right_text_x = right_num_x + num_w as u16;

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
    let row_endpoints = snapshot.highlighted_endpoints(scroll_row..end_row);
    let mut line_buf = String::new();

    for display_row in scroll_row..end_row {
        let y = inner.y + (display_row - scroll_row) as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let sep_x = inner.x + half_w as u16;
        if sep_x < inner.x + inner.width {
            buf[(sep_x, y)].set_char('│').set_style(dim_style);
        }

        match snapshot.classify_row(display_row) {
            BlockRowKind::Block { .. } => {
                line_buf.clear();
                snapshot.write_display_line(&mut line_buf, display_row);
                render_side_num(buf, left_num_x, y, base_line + 1, dim_style);
                let token_spans = snapshot
                    .diff_map()
                    .and_then(|dm| dm.base_highlights_for_line(base_line))
                    .unwrap_or(&[]);
                let changes = base_changes
                    .get(&base_line)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                let staged = snapshot
                    .diff_map()
                    .and_then(|dm| dm.base_line_staged(base_line));
                let side = tints.as_ref().map(|t| t.side(staged.unwrap_or(false)));
                if let Some(side) = side {
                    let line_tint = if changes.iter().any(|(_, k)| matches!(k, ChangeKind::Moved)) {
                        side.moved_line
                    } else {
                        side.removed_line
                    };
                    fill_line_tint(buf, left_text_x, y, left_content_w, line_tint);
                }
                paint_base_row(
                    buf,
                    left_text_x,
                    y,
                    &line_buf,
                    left_content_w,
                    token_spans,
                    del_style,
                    changes,
                    side.map(|c| c.removed_span),
                    side.map(|c| c.moved_span),
                );
                if let Some(staged) = staged {
                    let change_scope =
                        if changes.iter().any(|(_, k)| matches!(k, ChangeKind::Moved)) {
                            s::DIFF_MOVED
                        } else {
                            s::DIFF_DELETED
                        };
                    paint_status_bars(buf, inner.x, y, change_scope, staged, theme);
                }
                base_line += 1;
            },
            BlockRowKind::BufferRow { buffer_row } => {
                render_side_num(buf, right_num_x, y, buffer_row + 1, dim_style);
                let changes = buffer_row_change_spans(snapshot, buffer_row);
                let staged = snapshot
                    .diff_map()
                    .and_then(|dm| dm.staged_for_line(buffer_row));
                let side = tints.as_ref().map(|t| t.side(staged.unwrap_or(false)));
                if let Some(side) = side {
                    let line_tint = match snapshot.line_diff_status(buffer_row) {
                        DiffStatus::Added | DiffStatus::Modified => Some(side.added_line),
                        DiffStatus::Moved => Some(side.moved_line),
                        DiffStatus::Unchanged => None,
                    };
                    if let Some(tint) = line_tint {
                        fill_line_tint(buf, right_text_x, y, right_content_w, tint);
                    }
                }
                paint_highlighted_row(
                    snapshot,
                    display_row,
                    right_text_x,
                    y,
                    right_content_w,
                    buf,
                    fallback_style,
                    inlay_style,
                    &changes,
                    side.map(|c| c.added_span),
                    side.map(|c| c.moved_span),
                    &row_endpoints,
                );
                if snapshot.line_diff_status(buffer_row) == DiffStatus::Moved
                    && let Some((path, line)) = move_chip_source(snapshot, buffer_row)
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
                    let change_scope = match snapshot.line_diff_status(buffer_row) {
                        DiffStatus::Added => s::DIFF_ADDED,
                        DiffStatus::Modified => s::DIFF_MODIFIED,
                        DiffStatus::Moved => s::DIFF_MOVED,
                        DiffStatus::Unchanged => s::DIFF_CONTEXT,
                    };
                    paint_status_bars(buf, right_start, y, change_scope, staged, theme);
                }
                if snapshot.line_diff_status(buffer_row) == DiffStatus::Unchanged {
                    line_buf.clear();
                    snapshot.write_display_line(&mut line_buf, display_row);
                    render_side_num(buf, left_num_x, y, base_line + 1, dim_style);
                    let token_spans = snapshot
                        .diff_map()
                        .and_then(|dm| dm.base_highlights_for_line(base_line))
                        .unwrap_or(&[]);
                    paint_base_row(
                        buf,
                        left_text_x,
                        y,
                        &line_buf,
                        left_content_w,
                        token_spans,
                        dim_style,
                        &[],
                        None,
                        None,
                    );
                    base_line += 1;
                }
            },
        }
    }
}

/// Background washes marking diff changes across [`paint_diff_rows`]' two
/// columns, resolved once per paint from the theme's diff colors blended toward
/// the editor background.
///
/// The washes come in a staged set and an unstaged set. Staged content recedes
/// an extra step toward the background so it reads as fading into the index
/// while unstaged changes stay vivid. Select a set for a row with [`Self::side`].
struct DiffTints {
    unstaged: ChangeTints,
    staged: ChangeTints,
}

impl DiffTints {
    fn side(&self, staged: bool) -> &ChangeTints {
        if staged {
            &self.staged
        } else {
            &self.unstaged
        }
    }
}

/// The six change washes for one staged state.
///
/// A line-level wash ([`LINE_TINT`]) fills a changed line and a stronger
/// span-level wash ([`SPAN_TINT`]) marks the exact changed chars within it.
/// `added` and `removed` key the buffer (right) and base (left) sides. `moved`
/// keys relocated content on either side.
struct ChangeTints {
    added_line: [u8; 3],
    removed_line: [u8; 3],
    moved_line: [u8; 3],
    added_span: [u8; 3],
    removed_span: [u8; 3],
    moved_span: [u8; 3],
}

/// Resolve the staged and unstaged diff-change washes from the theme, or `None`
/// when the background or any diff color is not an RGB color.
///
/// A `None` disables tinting for the whole frame, so the diff view falls back to
/// [`Modifier::UNDERLINED`] on change spans and skips line washes, keeping
/// indexed-color themes marking changes.
fn resolve_diff_tints(theme: &crate::theme::Theme) -> Option<DiffTints> {
    use crate::theme::scope as s;
    let bg = style_rgb(theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg))?;
    let added = style_rgb(theme.get(s::DIFF_ADDED).fg)?;
    let removed = style_rgb(theme.get(s::DIFF_DELETED).fg)?;
    let moved = style_rgb(theme.get(s::DIFF_MOVED).fg)?;
    let set = |line: f32, span: f32| ChangeTints {
        added_line: dim_rgb(added, bg, line),
        removed_line: dim_rgb(removed, bg, line),
        moved_line: dim_rgb(moved, bg, line),
        added_span: dim_rgb(added, bg, span),
        removed_span: dim_rgb(removed, bg, span),
        moved_span: dim_rgb(moved, bg, span),
    };
    Some(DiffTints {
        unstaged: set(LINE_TINT, SPAN_TINT),
        staged: set(STAGED_LINE_TINT, STAGED_SPAN_TINT),
    })
}

/// Fill `cols` content cells from `start_x` with a background wash, leaving each
/// symbol untouched so text painted afterward keeps the wash. Ratatui's
/// `set_style` patches only the fields a style sets, and token styles set no
/// background.
fn fill_line_tint(buf: &mut Buffer, start_x: u16, y: u16, cols: usize, tint: [u8; 3]) {
    let color = Color::Rgb(tint[0], tint[1], tint[2]);
    for i in 0..cols {
        let x = start_x + i as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }
        buf[(x, y)].set_bg(color);
    }
}

/// Wash a cell `style` for a change span of `kind`. A moved span takes
/// `moved_span_tint` and any other kind takes the side wash `side_span_tint`. A
/// `None` tint falls back to [`Modifier::UNDERLINED`] so non-RGB themes still
/// mark the span.
fn apply_span_tint(
    style: Style,
    kind: &ChangeKind,
    side_span_tint: Option<[u8; 3]>,
    moved_span_tint: Option<[u8; 3]>,
) -> Style {
    let tint = match kind {
        ChangeKind::Moved => moved_span_tint,
        _ => side_span_tint,
    };
    match tint {
        Some([r, g, b]) => style.bg(Color::Rgb(r, g, b)),
        None => style.add_modifier(Modifier::UNDERLINED),
    }
}

/// Paint base text with per-token syntax styles for the diff view's left
/// column.
///
/// A byte inside a token span takes that token's color. Bytes outside every
/// span fall back to `fallback` (the deletion or context color), so the diff
/// tint still fills the gaps between tokens.
///
/// `change_spans` mark the changed chars of a modified or moved base line, as
/// line-local base byte ranges tagged by [`ChangeKind`]. A byte inside one takes
/// the span wash as its background. A replaced span takes `side_span_tint` (the
/// removed color on this base side) and a moved span takes `moved_span_tint`. A
/// `None` tint (a non-RGB theme) falls back to [`Modifier::UNDERLINED`].
#[allow(clippy::too_many_arguments)]
fn paint_base_row(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text: &str,
    max_cols: usize,
    token_spans: &[(std::ops::Range<usize>, HighlightStyle)],
    fallback: Style,
    change_spans: &[(std::ops::Range<usize>, ChangeKind)],
    side_span_tint: Option<[u8; 3]>,
    moved_span_tint: Option<[u8; 3]>,
) {
    debug_assert!(
        token_spans.is_sorted_by_key(|(range, _)| range.start),
        "token_spans must be start-sorted for the monotonic cursor"
    );
    debug_assert!(
        change_spans.is_sorted_by_key(|(range, _)| range.start),
        "change_spans must be start-sorted for the monotonic cursor"
    );

    let mut token_cursor = 0;
    let mut span_cursor = 0;
    for (col, (byte_idx, ch)) in text.char_indices().enumerate() {
        if col >= max_cols {
            break;
        }
        let x = start_x + col as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }

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

        while change_spans
            .get(span_cursor)
            .is_some_and(|(r, _)| r.end <= byte_idx)
        {
            span_cursor += 1;
        }
        if let Some((range, kind)) = change_spans.get(span_cursor)
            && range.start <= byte_idx
        {
            style = apply_span_tint(style, kind, side_span_tint, moved_span_tint);
        }

        buf[(x, y)].set_char(ch).set_style(style);
    }
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

/// Base-file line number at the top of the viewport (display row `scroll_row`).
///
/// Every display row above `scroll_row` is base-present (a deleted-base block
/// row or an unchanged buffer row) except changed buffer rows, which have no
/// base line. So the base line count is `scroll_row` minus the changed buffer
/// rows above, which the diff map answers in one seek rather than a per-row
/// walk from the document start.
fn base_line_at(snapshot: &DisplaySnapshot, scroll_row: u32) -> u32 {
    let buffer_rows_above = snapshot.buffer_rows_above(scroll_row);
    let changed = snapshot
        .diff_map()
        .map_or(0, |dm| dm.changed_rows_before(buffer_rows_above));
    scroll_row.saturating_sub(changed)
}

/// Paint one display row's syntax-highlighted chunks into a column starting at
/// `start_x`, clamped to `max_cols` and the buffer's right edge.
///
/// `change_spans` mark the changed chars of a modified or moved row, as
/// display-column ranges tagged by [`ChangeKind`]. A cell whose column falls in
/// one takes the span wash as its background. A replaced span takes
/// `side_span_tint` (the added color on this buffer side) and a moved span takes
/// `moved_span_tint`; a `None` tint (a non-RGB theme) falls back to
/// [`Modifier::UNDERLINED`]. Columns, not byte offsets, are used because the
/// chunks expand tabs, so the counter tracks display cells.
#[allow(clippy::too_many_arguments)]
fn paint_highlighted_row(
    snapshot: &DisplaySnapshot,
    display_row: u32,
    start_x: u16,
    y: u16,
    max_cols: usize,
    buf: &mut Buffer,
    fallback_style: Style,
    inlay_style: Style,
    change_spans: &[(std::ops::Range<usize>, ChangeKind)],
    side_span_tint: Option<[u8; 3]>,
    moved_span_tint: Option<[u8; 3]>,
    endpoints: &Arc<[HighlightEndpoint]>,
) {
    let mut col = 0usize;
    for chunk in
        snapshot.highlighted_chunks_with_endpoints(display_row..display_row + 1, endpoints.clone())
    {
        let style = if chunk.is_inlay {
            inlay_style
        } else {
            chunk
                .highlight_style
                .as_ref()
                .map(|hs| hs.to_ratatui_style())
                .unwrap_or(fallback_style)
        };
        for ch in chunk.text.chars() {
            if ch == '\n' || col >= max_cols {
                return;
            }
            let x = start_x + col as u16;
            if x >= buf.area.x + buf.area.width {
                return;
            }
            let cell_style = match change_spans.iter().find(|(range, _)| range.contains(&col)) {
                Some((_, kind)) => apply_span_tint(style, kind, side_span_tint, moved_span_tint),
                None => style,
            };
            buf[(x, y)].set_char(ch).set_style(cell_style);
            col += 1;
        }
    }
}

/// Display-column ranges, each tagged with its [`ChangeKind`], to wash on buffer
/// `buffer_row` in the diff view's right column, from the buffer spans of any
/// hunk covering the row.
///
/// The token detail's byte ranges are absolute buffer offsets. Each is clamped
/// to the row and mapped through [`DisplaySnapshot::buffer_to_display`], so tab
/// expansion in the painted chunks stays aligned. Empty when no hunk refines the
/// row.
fn buffer_row_change_spans(
    snapshot: &DisplaySnapshot,
    buffer_row: u32,
) -> Vec<(std::ops::Range<usize>, ChangeKind)> {
    let Some(diff_map) = snapshot.diff_map() else {
        return Vec::new();
    };
    let hunks = diff_map.hunks_in_range(buffer_row..buffer_row + 1);
    if hunks.is_empty() {
        return Vec::new();
    }

    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    let line_start = rope.point_to_offset(Point::new(buffer_row, 0));
    let line_end = line_start + rope.line_len(buffer_row) as usize;

    let mut ranges = Vec::new();
    for hunk in hunks {
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
            ranges.push((start_col..end_col, span.kind.clone()));
        }
    }
    ranges
}

/// The origin of a moved buffer row, for the diff view's move chip.
///
/// Scans the hunks covering `buffer_row` for the first move span and returns its
/// first counterpart source as `(file name, 0-based line)`. The file name is
/// `None` for an intra-file move (no counterpart buffer), so the chip omits the
/// path. Returns `None` when the row is not part of a move.
fn move_chip_source(snapshot: &DisplaySnapshot, buffer_row: u32) -> Option<(Option<String>, u32)> {
    let diff_map = snapshot.diff_map()?;
    for hunk in diff_map.hunks_in_range(buffer_row..buffer_row + 1) {
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

/// X column where the right pane's text begins. Mirrors the right-pane layout
/// in [`render_review_rows`]: a status glyph then a line-number column precede
/// the text on each side.
pub(crate) fn right_text_x(inner: Rect) -> u16 {
    let full_w = inner.width as usize;
    let sep: usize = 1;
    let half_w = (full_w.saturating_sub(sep)) / 2;
    let right_start = inner.x + half_w as u16 + sep as u16;
    right_start + 1 + 5
}

/// Paint the primary selection's cursor over the right pane's text, or set the
/// stoatty hardware cursor there. Skips a row scrolled out of view.
fn render_review_cursor(
    editor: &mut EditorState,
    snapshot: &DisplaySnapshot,
    inner: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    stoatty: bool,
) {
    let cursor_style = theme.get(crate::theme::scope::UI_CURSOR);
    let text_x = right_text_x(inner);

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

    let full_w = inner.width as usize;
    let status_w: usize = 1;
    let num_w: usize = 5;
    let gutter_w: usize = status_w + num_w;
    let sep: usize = 1;
    let half_w = (full_w.saturating_sub(sep)) / 2;
    let left_content_w = half_w.saturating_sub(gutter_w);
    let right_start = inner.x + half_w as u16 + sep as u16;
    let right_content_w = (full_w - half_w - sep).saturating_sub(gutter_w);

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

        let sep_x = inner.x + half_w as u16;
        if rich.is_none() && sep_x < inner.x + inner.width {
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
                        inner.x,
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
                        right_start,
                        y,
                        status,
                        is_current,
                        current_style,
                        theme,
                    );
                }
                let left_num_x = inner.x + status_w as u16;
                let right_num_x = right_start + status_w as u16;
                let left_text_x = left_num_x + num_w as u16;
                let right_text_x = right_num_x + num_w as u16;
                match row {
                    ReviewRow::Context { left, right } => {
                        draw_side_num(
                            &mut rich,
                            buf,
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
                                &mut rich, buf, inner, left_num_x, y, l.line_num, dim_style,
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
    if let Some(rg) = rich.as_mut() {
        let sep_x = inner.x + half_w as u16;
        Bar {
            x: (sep_x - inner.x) * 16 + 8,
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
                    x: (col - inner.x) * 16,
                    y: (y - inner.y) * 16,
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
fn draw_side_num(
    rich: &mut Option<RichGutter<'_>>,
    buf: &mut Buffer,
    inner: Rect,
    num_x: u16,
    y: u16,
    num: u32,
    dim_style: Style,
) {
    match rich {
        Some(rg) => {
            let text = num.to_string();
            let digits = text.len() as u16;
            let right_edge = (num_x - inner.x + 4) * 16;
            TextRun {
                col: right_edge.saturating_sub(digits * TEXT_SCALE_COMPACT / 16),
                row: (y - inner.y) * 16,
                scale: TEXT_SCALE_COMPACT,
                color: rg.colors.dim,
                bg: Some(rg.colors.bg),
                text: &text,
            }
            .render(inner, buf, &mut *rg.scene);
        },
        None => render_side_num(buf, num_x, y, num, dim_style),
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

pub(crate) fn style_rgb(color: Option<Color>) -> Option<[u8; 3]> {
    match color {
        Some(Color::Rgb(r, g, b)) => Some([r, g, b]),
        _ => None,
    }
}

/// Blend `fg` toward `bg` by `amount`, where `0.0` returns `fg` unchanged and
/// `1.0` returns `bg`.
///
/// Dims an unfocused pane's colors toward the theme background by a configurable
/// fraction. `amount` is clamped to `0.0..=1.0`.
pub(crate) fn dim_rgb(fg: [u8; 3], bg: [u8; 3], amount: f32) -> [u8; 3] {
    let amount = amount.clamp(0.0, 1.0);
    let blend = |f: u8, b: u8| (f as f32 * (1.0 - amount) + b as f32 * amount).round() as u8;
    [
        blend(fg[0], bg[0]),
        blend(fg[1], bg[1]),
        blend(fg[2], bg[2]),
    ]
}

pub(crate) fn render_side_num(buf: &mut Buffer, x: u16, y: u16, num: u32, style: Style) {
    let s = format!("{num:>4} ");
    for (i, ch) in s.chars().enumerate() {
        let col = x + i as u16;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        buf[(col, y)].set_char(ch).set_style(style);
    }
}

pub(crate) fn paint_status_gutter(
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

pub(crate) fn render_empty_num(buf: &mut Buffer, x: u16, y: u16, style: Style) {
    for i in 0..5u16 {
        let col = x + i;
        if col >= buf.area.x + buf.area.width {
            break;
        }
        buf[(col, y)].set_char('.').set_style(style);
    }
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
pub(crate) fn render_move_chip(
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

/// Render text with sub-line change span highlighting. Characters
/// within any `spans` range get `highlight_style`; characters within
/// any `moved_spans` range get the diff theme's move color (cyan)
/// regardless of which side they live on. The rest get `base_style`.
///
/// Move highlighting takes precedence over change highlighting: if a
/// byte falls in both a change span and a moved span, the move color
/// wins so users see at a glance that the token relocated rather than
/// was replaced.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_side_text(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text: &str,
    max_cols: usize,
    base_style: Style,
    spans: &[std::ops::Range<usize>],
    highlight_style: Style,
    moved_spans: &[std::ops::Range<usize>],
    moved_style: Style,
) {
    debug_assert!(
        moved_spans.is_sorted_by_key(|s| s.start),
        "moved_spans must be start-sorted for the monotonic cursor"
    );
    debug_assert!(
        spans.is_sorted_by_key(|s| s.start),
        "spans must be start-sorted for the monotonic cursor"
    );

    let mut moved_cursor = 0;
    let mut span_cursor = 0;
    for (col, (byte_idx, ch)) in text.char_indices().enumerate() {
        if col >= max_cols {
            break;
        }
        let x = start_x + col as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }

        while moved_spans
            .get(moved_cursor)
            .is_some_and(|s| s.end <= byte_idx)
        {
            moved_cursor += 1;
        }
        let in_moved = matches!(moved_spans.get(moved_cursor), Some(s) if s.start <= byte_idx);

        while spans.get(span_cursor).is_some_and(|s| s.end <= byte_idx) {
            span_cursor += 1;
        }
        let in_span = matches!(spans.get(span_cursor), Some(s) if s.start <= byte_idx);

        let style = if in_moved {
            moved_style
        } else if in_span {
            highlight_style
        } else {
            base_style
        };
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

    #[test]
    fn base_line_at_matches_the_reference_walk() {
        // The original per-row walk, kept as the correctness oracle.
        fn reference(snapshot: &DisplaySnapshot, scroll_row: u32) -> u32 {
            let mut base_line = 0;
            for row in 0..scroll_row {
                match snapshot.classify_row(row) {
                    BlockRowKind::Block { .. } => base_line += 1,
                    BlockRowKind::BufferRow { buffer_row } => {
                        if snapshot.line_diff_status(buffer_row) == DiffStatus::Unchanged {
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
        let mut saw_blocks = false;
        for (base, text) in fixtures {
            let mut editor = diff_editor(base, text);
            let snapshot = editor.display_map.snapshot();
            let total = snapshot.line_count();
            saw_blocks |= total > snapshot.buffer_line_count();
            for row in 0..total {
                assert_eq!(
                    base_line_at(&snapshot, row),
                    reference(&snapshot, row),
                    "base_line_at disagrees with the walk at row {row}/{total} for {base:?}->{text:?}"
                );
            }
        }
        assert!(
            saw_blocks,
            "fixtures must splice deleted-base block rows to exercise the block case"
        );
    }

    /// A diff-view editor over `text`, diffed against `base`, with the view and
    /// its deleted-block splicing enabled.
    fn diff_editor(base: &str, text: &str) -> EditorState {
        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let mut tb = TextBuffer::with_text(BufferId::new(0), text);
        tb.diff_map = Some(DiffMap::from_structural_changes(
            structural_diff::diff(base, text),
            base,
            text,
        ));
        let shared = Arc::new(RwLock::new(tb));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor);
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
        let mut editor = EditorState::new(BufferId::new(0), shared, executor);
        editor.set_diff_view(true);
        editor
    }

    /// A minimal theme whose diff colors resolve to RGB, so the change washes
    /// engage when rendering a hand-built diff editor off the harness.
    fn rgb_diff_theme() -> Theme {
        let src = r##"theme rgbtest {
            diff.context.fg  = "#808080";
            diff.added.fg    = "#00ff00";
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
        let index_changed: Vec<std::ops::Range<u32>> =
            DiffMap::from_structural_changes(structural_diff::diff(index, text), index, text)
                .hunks_in_range(0..u32::MAX)
                .iter()
                .map(|h| h.buffer_line_range.clone())
                .collect();
        tb.diff_map = Some(DiffMap::from_structural_changes_staged(
            structural_diff::diff(base, text),
            base,
            text,
            &index_changed,
        ));
        let shared = Arc::new(RwLock::new(tb));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor);
        editor.set_diff_view(true);
        editor
    }

    #[test]
    fn diff_view_marks_staged_and_unstaged_hunks_in_the_status_column() {
        use crate::theme::scope as sc;

        // HEAD a/b/c/d; buffer changes line 1 (B) and line 3 (D); the index
        // holds only the line-1 change, so line 1 is staged, line 3 is not.
        let mut editor = diff_editor_staged("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n");
        let area = Rect::new(0, 0, 40, 8);
        let mut buf = Buffer::empty(area);
        let theme = rgb_diff_theme();
        render_diff_view(&mut editor, area, Style::default(), &theme, &mut buf, false);

        // The right (buffer) status column: change bar at right_start, staged
        // bar in the cell after it.
        let change_col = ((40 - 1) / 2 + 1) as u16;
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

    #[test]
    fn diff_view_lays_out_base_left_buffer_right() {
        let mut editor = diff_editor("keep\nold\ntail\n", "keep\nnew\ntail\n");
        let area = Rect::new(0, 0, 40, 8);
        let mut buf = Buffer::empty(area);
        render_diff_view(
            &mut editor,
            area,
            Style::default(),
            &Theme::empty(),
            &mut buf,
            false,
        );

        // For width 40, left text spans cols 7..19, the separator sits at col 19,
        // and right text spans cols 27..40. The status column takes two cells.
        let left = |y| line_text(&buf, y, 7..19);
        let right = |y| line_text(&buf, y, 27..40);

        assert!(
            left(0).contains("keep"),
            "row0 left mirrors context: {:?}",
            left(0)
        );
        assert!(
            right(0).contains("keep"),
            "row0 right shows buffer: {:?}",
            right(0)
        );

        assert!(
            left(1).contains("old"),
            "row1 left shows deleted base: {:?}",
            left(1)
        );
        assert_eq!(
            right(1).trim(),
            "",
            "row1 right blank for a deletion: {:?}",
            right(1)
        );

        assert!(
            right(2).contains("new"),
            "row2 right shows the new line: {:?}",
            right(2)
        );
        assert_eq!(
            left(2).trim(),
            "",
            "row2 left blank for a modified line: {:?}",
            left(2)
        );

        assert!(
            left(3).contains("tail") && right(3).contains("tail"),
            "row3 context mirrors both sides: left={:?} right={:?}",
            left(3),
            right(3)
        );

        assert_eq!(
            buf[(19, 0)].symbol(),
            "│",
            "the two columns are split by a separator"
        );
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

        let mut h = TestHarness::with_size(60, 10);
        let path = h.write_file("a.rs", "fn main() {}\n");
        h.open_file(&path);
        h.stoat.drive_background();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        h.snapshot();

        // For width 60 the right text begins at col 36. A syntax-highlighted right
        // column paints more than one foreground color across the row's tokens.
        let buf = h.rendered_buffer();
        let mut colors = std::collections::HashSet::new();
        for x in 36..60 {
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

        let mut h = TestHarness::with_size(60, 10);
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

        // Left text spans cols 6..29 (1-col status + 5-col number, half width 29).
        let buf = h.rendered_buffer();
        let mut colors = std::collections::HashSet::new();
        for y in 0..buf.area.height {
            for x in 6..29 {
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

    /// A width-60 diff-view harness over one `.rs` file diffed `base` -> `buffer`,
    /// rendered once. The default theme resolves diff colors to RGB, so the change
    /// washes engage. Column layout: left content cols 6..29, separator col 29,
    /// right content cols 36..60.
    fn diff_harness(base: &str, buffer: &str) -> crate::test_harness::TestHarness {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(60, 10);
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

    /// The RGB wash a diff `scope` blends to at `amount`, as the cell background
    /// the render paints.
    fn tint(theme: &Theme, scope: &str, amount: f32) -> Color {
        let bg = style_rgb(
            theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
        .expect("rgb background");
        let [r, g, b] = dim_rgb(
            style_rgb(theme.get(scope).fg).expect("rgb diff color"),
            bg,
            amount,
        );
        Color::Rgb(r, g, b)
    }

    /// Non-blank glyphs in `cols` whose cell background is exactly `bg`, in row
    /// order.
    fn chars_with_bg(buf: &Buffer, bg: Color, cols: std::ops::Range<u16>) -> String {
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in cols.clone() {
                let cell = &buf[(x, y)];
                if cell.bg == bg && !cell.symbol().trim().is_empty() {
                    out.push_str(cell.symbol());
                }
            }
        }
        out
    }

    /// The glyphs of row `y` across `cols`, for locating a rendered line.
    fn line_text(buf: &Buffer, y: u16, cols: std::ops::Range<u16>) -> String {
        cols.map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn diff_view_washes_only_the_changed_word() {
        // `main` becomes `other`, so only that one word changed on the line.
        let h = diff_harness("fn main() {}\n", "fn other() {}\n");

        use crate::theme::scope as sc;
        let added_span = tint(&h.stoat.theme, sc::DIFF_ADDED, SPAN_TINT);
        let removed_span = tint(&h.stoat.theme, sc::DIFF_DELETED, SPAN_TINT);
        let buf = h.rendered_buffer();

        assert_eq!(
            chars_with_bg(buf, added_span, 30..buf.area.width),
            "other",
            "the right column washes only the changed word with the added span tint"
        );
        assert_eq!(
            chars_with_bg(buf, removed_span, 0..30),
            "main",
            "the left column washes only the changed base word with the removed span tint"
        );
    }

    #[test]
    fn diff_view_added_line_carries_the_added_wash() {
        // The second line is a pure insertion, so nothing is refined.
        let h = diff_harness("fn a() {}\n", "fn a() {}\nfn b() {}\n");

        let added_line = tint(&h.stoat.theme, crate::theme::scope::DIFF_ADDED, LINE_TINT);
        let buf = h.rendered_buffer();

        let underlined = (0..buf.area.height).any(|y| {
            (0..buf.area.width).any(|x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED))
        });
        assert!(!underlined, "a pure added line underlines nothing");

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 37..60).contains("fn b"))
            .expect("added line rendered on the right");
        assert!(
            (37..60).all(|x| buf[(x, row)].bg == added_line),
            "the added line's right-column cells all carry the added line wash"
        );
    }

    #[test]
    fn diff_view_deleted_line_carries_the_removed_wash() {
        // `old` is deleted, so it renders as a base-only block row on the left.
        let h = diff_harness("keep\nold\ntail\n", "keep\ntail\n");

        let removed_line = tint(&h.stoat.theme, crate::theme::scope::DIFF_DELETED, LINE_TINT);
        let buf = h.rendered_buffer();

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 7..29).contains("old"))
            .expect("deleted base line rendered on the left");
        assert!(
            (7..29).all(|x| buf[(x, row)].bg == removed_line),
            "the deleted base line's left-column cells all carry the removed line wash"
        );
    }

    #[test]
    fn diff_view_unchanged_mirrored_base_row_stays_untinted() {
        // `keep` is unchanged and mirrored into the left column untinted.
        let h = diff_harness("keep\nold\ntail\n", "keep\ntail\n");

        use crate::theme::scope as sc;
        let removed_line = tint(&h.stoat.theme, sc::DIFF_DELETED, LINE_TINT);
        let removed_span = tint(&h.stoat.theme, sc::DIFF_DELETED, SPAN_TINT);
        let buf = h.rendered_buffer();

        let row = (0..buf.area.height)
            .find(|&y| line_text(buf, y, 7..29).contains("keep"))
            .expect("unchanged base line mirrored on the left");
        assert!(
            (7..29).all(|x| buf[(x, row)].bg != removed_line && buf[(x, row)].bg != removed_span),
            "an unchanged mirrored base row carries no removed wash"
        );
    }

    #[test]
    fn diff_view_moved_line_carries_the_moved_wash() {
        // A Moved hunk covering buffer line 1 ("bb", bytes 3..5); no base bytes,
        // so the line stays in place on the right rather than splicing a block.
        let dm = {
            let detail = Arc::new(TokenDetail {
                buffer_spans: vec![ChangeSpan {
                    byte_range: 3..5,
                    kind: ChangeKind::Moved,
                    move_metadata: None,
                }],
                base_spans: Vec::new(),
            });
            DiffMap::from_hunks(
                [DiffHunk {
                    status: DiffHunkStatus::Moved,
                    unstaged_lines: std::iter::once(1..2).collect(),
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
        render_diff_view(&mut editor, area, fallback, &theme, &mut buf, false);

        use crate::theme::scope as sc;
        let moved_line = tint(&theme, sc::DIFF_MOVED, LINE_TINT);
        let moved_span = tint(&theme, sc::DIFF_MOVED, SPAN_TINT);

        // "bb" sits on display row 1. The diff view's status column is one cell
        // wider than the review session's, so the right text starts one past
        // right_text_x.
        let rx = right_text_x(area) + 1;
        assert_eq!(
            buf[(rx, 1)].symbol(),
            "b",
            "the moved line renders on the right"
        );
        assert_eq!(
            buf[(rx, 1)].bg,
            moved_span,
            "moved word takes the moved span wash"
        );
        assert_eq!(
            buf[(rx + 1, 1)].bg,
            moved_span,
            "moved word takes the moved span wash"
        );
        assert_eq!(
            buf[(rx + 3, 1)].bg,
            moved_line,
            "the rest of the moved line takes the moved line wash"
        );
    }

    #[test]
    fn diff_view_staged_line_wash_recedes_further_than_unstaged() {
        use crate::theme::scope as sc;

        // The base is a/b. The buffer inserts S before a and U before b. The
        // index holds S but not U, so S is a staged add and U an unstaged one.
        let mut editor = diff_editor_staged("a\nb\n", "S\na\nb\n", "S\na\nU\nb\n");
        let area = Rect::new(0, 0, 40, 8);
        let mut buf = Buffer::empty(area);
        let theme = rgb_diff_theme();
        render_diff_view(&mut editor, area, Style::default(), &theme, &mut buf, false);

        let staged_line = tint(&theme, sc::DIFF_ADDED, STAGED_LINE_TINT);
        let unstaged_line = tint(&theme, sc::DIFF_ADDED, LINE_TINT);
        assert_ne!(
            staged_line, unstaged_line,
            "the staged wash must differ from the unstaged one"
        );

        let rx = right_text_x(area) + 1;
        let row_of = |ch: char| {
            (0..area.height)
                .find(|&y| buf[(rx, y)].symbol().starts_with(ch))
                .unwrap_or_else(|| panic!("no right-column row starts with {ch}"))
        };
        assert_eq!(
            buf[(rx, row_of('S'))].bg,
            staged_line,
            "the staged added line takes the receding staged wash"
        );
        assert_eq!(
            buf[(rx, row_of('U'))].bg,
            unstaged_line,
            "the unstaged added line takes the vivid unstaged wash"
        );
        let context = buf[(rx, row_of('a'))].bg;
        assert!(
            context != staged_line && context != unstaged_line,
            "an unchanged context row carries no line wash"
        );
    }

    #[test]
    fn diff_view_staged_span_takes_the_dimmer_span_wash() {
        use crate::theme::scope as sc;

        // A staged Modified hunk on buffer line 1 with a change span over "bb"
        // (bytes 3..5). Empty unstaged_lines marks the whole hunk staged.
        let dm = {
            let detail = Arc::new(TokenDetail {
                buffer_spans: vec![ChangeSpan {
                    byte_range: 3..5,
                    kind: ChangeKind::Replaced,
                    move_metadata: None,
                }],
                base_spans: Vec::new(),
            });
            DiffMap::from_hunks(
                [DiffHunk {
                    status: DiffHunkStatus::Modified,
                    unstaged_lines: Vec::new(),
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
        render_diff_view(&mut editor, area, Style::default(), &theme, &mut buf, false);

        let staged_span = tint(&theme, sc::DIFF_ADDED, STAGED_SPAN_TINT);
        let unstaged_span = tint(&theme, sc::DIFF_ADDED, SPAN_TINT);
        assert_ne!(
            staged_span, unstaged_span,
            "the staged span wash must differ from the unstaged one"
        );

        let rx = right_text_x(area) + 1;
        assert_eq!(
            buf[(rx, 1)].bg,
            staged_span,
            "a span on a staged line takes the dimmer staged span wash"
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
            }],
            base_spans: Vec::new(),
        });
        let dm = DiffMap::from_hunks(
            [DiffHunk {
                status: DiffHunkStatus::Moved,
                unstaged_lines: std::iter::once(1..2).collect(),
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
            false,
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
            false,
        );

        let row = buffer_text(&buf, 1);
        assert!(
            row.contains("<- 5") && !row.contains(':'),
            "an intra-file moved row shows a path-less chip; got {row:?}"
        );
    }

    #[test]
    fn paint_base_row_washes_replaced_and_moved_spans_by_kind() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let change_spans = vec![(0..3, ChangeKind::Replaced), (3..6, ChangeKind::Moved)];
        paint_base_row(
            &mut buf,
            0,
            0,
            "abcdefgh",
            8,
            &[],
            Style::default(),
            &change_spans,
            Some([10, 20, 30]),
            Some([40, 50, 60]),
        );

        for x in 0..3 {
            assert_eq!(
                buf[(x, 0)].bg,
                Color::Rgb(10, 20, 30),
                "replaced span takes the side wash"
            );
        }
        for x in 3..6 {
            assert_eq!(
                buf[(x, 0)].bg,
                Color::Rgb(40, 50, 60),
                "moved span takes the moved wash"
            );
        }
        assert_eq!(
            buf[(6, 0)].bg,
            Color::Reset,
            "chars outside every span keep the default bg"
        );
        assert!(
            !buf[(0, 0)].modifier.contains(Modifier::UNDERLINED),
            "a washed cell is not also underlined"
        );
    }

    #[test]
    fn paint_base_row_underlines_change_spans_without_tints() {
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 1));
        let change_spans = vec![(0..3, ChangeKind::Replaced), (3..6, ChangeKind::Moved)];
        paint_base_row(
            &mut buf,
            0,
            0,
            "abcdefgh",
            8,
            &[],
            Style::default(),
            &change_spans,
            None,
            None,
        );

        for x in 0..6 {
            assert!(
                buf[(x, 0)].modifier.contains(Modifier::UNDERLINED),
                "col {x} underlines when tints are absent"
            );
            assert_eq!(
                buf[(x, 0)].bg,
                Color::Reset,
                "no wash bg when tints are absent"
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

    #[test]
    fn dim_rgb_blends_between_fg_and_bg() {
        let fg = [200, 100, 40];
        assert_eq!(dim_rgb(fg, [0, 0, 0], 0.0), fg, "amount 0 keeps fg");
        assert_eq!(
            dim_rgb(fg, [0, 0, 0], 1.0),
            [0, 0, 0],
            "amount 1 reaches bg"
        );
        assert_eq!(
            dim_rgb(fg, [50, 50, 50], 0.5),
            [125, 75, 45],
            "midpoint blend"
        );
        assert_eq!(
            dim_rgb(fg, [0, 0, 0], 2.0),
            [0, 0, 0],
            "amount clamps above 1"
        );
    }
}
