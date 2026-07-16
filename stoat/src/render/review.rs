use crate::{
    diff_map::ChangeKind,
    display_map::{highlights::HighlightStyle, BlockRowKind, DisplaySnapshot},
    editor_state::EditorState,
    host::DiffStatus,
    review::{MoveProvenance, ReviewRow},
    review_session::{ChunkStatus, ReviewViewState},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::StatefulWidget,
};
use stoat_text::{cursor_offset, Point};
use stoatty_widgets::{bar::Bar, text_run::TextRun, ApcScene};

/// Line-number glyph size in 256ths of a cell, so the number reads smaller than
/// the body text. Matches the gutter demo's `NUMBER_SCALE`.
const NUMBER_SCALE: u16 = 160;

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
    let status_w: usize = 1;
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

    let base_underlines = snapshot
        .diff_map()
        .map(|dm| dm.base_underline_spans())
        .unwrap_or_default();

    let mut base_line = base_line_at(snapshot, scroll_row);

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
                let text = snapshot.display_line(display_row);
                render_side_num(buf, left_num_x, y, base_line + 1, dim_style);
                let token_spans = snapshot
                    .diff_map()
                    .and_then(|dm| dm.base_highlights_for_line(base_line))
                    .unwrap_or(&[]);
                let underlines = base_underlines
                    .get(&base_line)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                paint_base_row(
                    buf,
                    left_text_x,
                    y,
                    &text,
                    left_content_w,
                    token_spans,
                    del_style,
                    underlines,
                );
                base_line += 1;
            },
            BlockRowKind::BufferRow { buffer_row } => {
                render_side_num(buf, right_num_x, y, buffer_row + 1, dim_style);
                let underlines = buffer_row_underlines(snapshot, buffer_row);
                paint_highlighted_row(
                    snapshot,
                    display_row,
                    right_text_x,
                    y,
                    right_content_w,
                    buf,
                    fallback_style,
                    &underlines,
                );
                if let Some(staged) = snapshot
                    .diff_map()
                    .and_then(|dm| dm.staged_for_line(buffer_row))
                {
                    paint_staged_glyph(buf, right_start, y, staged, theme);
                }
                if snapshot.line_diff_status(buffer_row) == DiffStatus::Unchanged {
                    let text = snapshot.display_line(display_row);
                    render_side_num(buf, left_num_x, y, base_line + 1, dim_style);
                    let token_spans = snapshot
                        .diff_map()
                        .and_then(|dm| dm.base_highlights_for_line(base_line))
                        .unwrap_or(&[]);
                    paint_base_row(
                        buf,
                        left_text_x,
                        y,
                        &text,
                        left_content_w,
                        token_spans,
                        dim_style,
                        &[],
                    );
                    base_line += 1;
                }
            },
        }
    }
}

/// Paint base text with per-token syntax styles for the diff view's left
/// column.
///
/// A byte inside a token span takes that token's color. Bytes outside every
/// span fall back to `fallback` (the deletion or context color), so the diff
/// tint still fills the gaps between tokens.
///
/// `underlines` mark the changed chars of a modified base line, as line-local
/// base byte ranges. A byte inside one gains [`Modifier::UNDERLINED`] over its
/// color.
#[allow(clippy::too_many_arguments)]
fn paint_base_row(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text: &str,
    max_cols: usize,
    token_spans: &[(std::ops::Range<usize>, HighlightStyle)],
    fallback: Style,
    underlines: &[std::ops::Range<usize>],
) {
    for (col, (byte_idx, ch)) in text.char_indices().enumerate() {
        if col >= max_cols {
            break;
        }
        let x = start_x + col as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }
        let mut style = token_spans
            .iter()
            .find(|(range, _)| range.contains(&byte_idx))
            .map(|(_, hs)| hs.to_ratatui_style())
            .unwrap_or(fallback);
        if underlines.iter().any(|range| range.contains(&byte_idx)) {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        buf[(x, y)].set_char(ch).set_style(style);
    }
}

/// Paint the git-index staged glyph for a hunk row into a 1-cell status
/// column. Staged hunks show `+` in the add color, unstaged `-` in the delete
/// color, matching [`paint_status_gutter`]'s convention.
fn paint_staged_glyph(buf: &mut Buffer, x: u16, y: u16, staged: bool, theme: &crate::theme::Theme) {
    use crate::theme::scope as s;
    if x >= buf.area.x + buf.area.width {
        return;
    }
    let (ch, style) = if staged {
        ('+', theme.get(s::DIFF_ADDED))
    } else {
        ('-', theme.get(s::DIFF_DELETED))
    };
    buf[(x, y)].set_char(ch).set_style(style);
}

/// Count the base-present display rows above `scroll_row` to get the base-file
/// line number at the top of the viewport.
///
/// Context buffer rows and deleted/modified base block rows each map to one base
/// line. Added and modified new-line buffer rows do not, and are skipped. Walks
/// from row 0, so a deep scroll costs one classify per row above the viewport.
fn base_line_at(snapshot: &DisplaySnapshot, scroll_row: u32) -> u32 {
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

/// Paint one display row's syntax-highlighted chunks into a column starting at
/// `start_x`, clamped to `max_cols` and the buffer's right edge.
///
/// `underlines` mark the changed chars of a modified row, as display-column
/// ranges. A cell whose column falls in one gains [`Modifier::UNDERLINED`] over
/// its token style. Columns, not byte offsets, are used because the chunks
/// expand tabs, so the counter tracks display cells.
#[allow(clippy::too_many_arguments)]
fn paint_highlighted_row(
    snapshot: &DisplaySnapshot,
    display_row: u32,
    start_x: u16,
    y: u16,
    max_cols: usize,
    buf: &mut Buffer,
    fallback_style: Style,
    underlines: &[std::ops::Range<usize>],
) {
    let mut col = 0usize;
    for chunk in snapshot.highlighted_chunks(display_row..display_row + 1) {
        let style = chunk
            .highlight_style
            .as_ref()
            .map(|hs| hs.to_ratatui_style())
            .unwrap_or(fallback_style);
        for ch in chunk.text.chars() {
            if ch == '\n' || col >= max_cols {
                return;
            }
            let x = start_x + col as u16;
            if x >= buf.area.x + buf.area.width {
                return;
            }
            let cell_style = if underlines.iter().any(|range| range.contains(&col)) {
                style.add_modifier(Modifier::UNDERLINED)
            } else {
                style
            };
            buf[(x, y)].set_char(ch).set_style(cell_style);
            col += 1;
        }
    }
}

/// Display-column ranges to underline on buffer `buffer_row` in the diff view's
/// right column, from the [`ChangeKind::Replaced`] buffer spans of any hunk
/// covering the row.
///
/// The token detail's byte ranges are absolute buffer offsets. Each is clamped
/// to the row and mapped through [`DisplaySnapshot::buffer_to_display`], so tab
/// expansion in the painted chunks stays aligned. Empty when no hunk refines the
/// row.
fn buffer_row_underlines(
    snapshot: &DisplaySnapshot,
    buffer_row: u32,
) -> Vec<std::ops::Range<usize>> {
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
            if span.kind != ChangeKind::Replaced {
                continue;
            }
            let start = span.byte_range.start.max(line_start);
            let end = span.byte_range.end.min(line_end);
            if start >= end {
                continue;
            }
            let start_col = snapshot
                .buffer_to_display(rope.offset_to_point(start))
                .column as usize;
            let end_col = snapshot.buffer_to_display(rope.offset_to_point(end)).column as usize;
            ranges.push(start_col..end_col);
        }
    }
    ranges
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
                                    prov,
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
                                    prov,
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
                col: right_edge.saturating_sub(digits * NUMBER_SCALE / 16),
                row: (y - inner.y) * 16,
                scale: NUMBER_SCALE,
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
/// A cross-file move paints `<- {rel_path}:{line+1}`. An intra-file move
/// paints a path-less `<- {line+1}`, since repeating the row's own file name
/// is noise. `text_cols` is the column count already consumed by
/// [`render_side_text`]; the chip starts two columns later (so the gap is
/// visually obvious) and truncates if fewer columns remain. No-op when
/// `text_cols + 2 >= max_cols`.
pub(crate) fn render_move_chip(
    buf: &mut Buffer,
    start_x: u16,
    y: u16,
    text_cols: usize,
    max_cols: usize,
    prov: &MoveProvenance,
    style: Style,
) {
    let chip_start_col = text_cols.saturating_add(2);
    if chip_start_col >= max_cols {
        return;
    }
    let chip = if prov.intra_file {
        format!("<- {}", prov.line + 1)
    } else {
        format!("<- {}:{}", prov.rel_path, prov.line + 1)
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
    for (col, (byte_idx, ch)) in text.char_indices().enumerate() {
        if col >= max_cols {
            break;
        }
        let x = start_x + col as u16;
        if x >= buf.area.x + buf.area.width {
            break;
        }
        let in_moved = moved_spans
            .iter()
            .any(|s| byte_idx >= s.start && byte_idx < s.end);
        let in_span = spans
            .iter()
            .any(|s| byte_idx >= s.start && byte_idx < s.end);
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
        diff_map::DiffMap,
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
        // HEAD a/b/c/d; buffer changes line 1 (B) and line 3 (D); the index
        // holds only the line-1 change, so line 1 is staged, line 3 is not.
        let mut editor = diff_editor_staged("a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n");
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

        // The right (buffer) status column sits at right_start = (width-1)/2 + 1.
        let status_col = (40 - 1) / 2 + 1;
        let glyphs: String = (0..area.height)
            .map(|y| buf[(status_col, y)].symbol().chars().next().unwrap_or(' '))
            .collect();
        assert!(
            glyphs.contains('+'),
            "the staged hunk shows + in the status column: {glyphs:?}"
        );
        assert!(
            glyphs.contains('-'),
            "the unstaged hunk shows - in the status column: {glyphs:?}"
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

        // For width 40, left text spans cols 6..19, the separator sits at col 19,
        // and right text spans cols 26..40.
        let rows: Vec<String> = (0..4).map(|y| buffer_text(&buf, y)).collect();

        assert!(
            rows[0][6..19].contains("keep"),
            "row0 left mirrors context: {:?}",
            rows[0]
        );
        assert!(
            rows[0][26..40].contains("keep"),
            "row0 right shows buffer: {:?}",
            rows[0]
        );

        assert!(
            rows[1][6..19].contains("old"),
            "row1 left shows deleted base: {:?}",
            rows[1]
        );
        assert_eq!(
            rows[1][26..40].trim(),
            "",
            "row1 right blank for a deletion: {:?}",
            rows[1]
        );

        assert!(
            rows[2][26..40].contains("new"),
            "row2 right shows the new line: {:?}",
            rows[2]
        );
        assert_eq!(
            rows[2][6..19].trim(),
            "",
            "row2 left blank for a modified line: {:?}",
            rows[2]
        );

        assert!(
            rows[3][6..19].contains("tail") && rows[3][26..40].contains("tail"),
            "row3 context mirrors both sides: {:?}",
            rows[3]
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

    #[test]
    fn diff_view_underlines_only_the_changed_word() {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(60, 10);
        // `main` becomes `other`, so only that one word changed on the line.
        h.stage_review_scenario("/repo", &[("a.rs", "fn main() {}\n", "fn other() {}\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(std::path::Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        h.snapshot();

        // Collect underlined glyphs per column half (the separator sits at col 29).
        let buf = h.rendered_buffer();
        let mut left = String::new();
        let mut right = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                let cell = &buf[(x, y)];
                if cell.modifier.contains(Modifier::UNDERLINED) && !cell.symbol().trim().is_empty()
                {
                    if x < 30 {
                        left.push_str(cell.symbol());
                    } else {
                        right.push_str(cell.symbol());
                    }
                }
            }
        }
        assert_eq!(
            right, "other",
            "right column underlines only the changed word"
        );
        assert_eq!(
            left, "main",
            "left column underlines only the changed base word"
        );
    }

    #[test]
    fn diff_view_added_line_carries_no_underline() {
        use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};

        let mut h = TestHarness::with_size(60, 10);
        // The second line is a pure insertion, so nothing is refined.
        h.stage_review_scenario(
            "/repo",
            &[("a.rs", "fn a() {}\n", "fn a() {}\nfn b() {}\n")],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(std::path::Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        h.snapshot();

        let buf = h.rendered_buffer();
        let underlined = (0..buf.area.height).any(|y| {
            (0..buf.area.width).any(|x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED))
        });
        assert!(!underlined, "a pure added line underlines nothing");
    }

    #[test]
    fn move_chip_paints_text_after_two_col_gap() {
        let area = Rect::new(0, 0, 50, 1);
        let mut buf = Buffer::empty(area);
        let prov = MoveProvenance {
            rel_path: "a.rs".to_string(),
            line: 0,
            intra_file: false,
        };
        render_move_chip(&mut buf, 0, 0, 5, 50, &prov, Style::default());
        let text = buffer_text(&buf, 0);
        assert_eq!(&text[..7], "       ", "5-col text + 2-col gap before chip");
        assert_eq!(&text[7..16], "<- a.rs:1", "chip text follows the gap");
    }

    #[test]
    fn move_chip_no_op_when_text_fills_max_cols() {
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let prov = MoveProvenance {
            rel_path: "long_name.rs".to_string(),
            line: 100,
            intra_file: false,
        };
        render_move_chip(&mut buf, 0, 0, 19, 20, &prov, Style::default());
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
        let prov = MoveProvenance {
            rel_path: "long_name.rs".to_string(),
            line: 99,
            intra_file: false,
        };
        render_move_chip(&mut buf, 0, 0, 5, 20, &prov, Style::default());
        let text = buffer_text(&buf, 0);
        // text_cols=5 + 2-col gap = chip starts at col 7; max_cols=20 leaves 13 cols.
        // "<- long_name.rs:100" is 19 chars; truncated to 13: "<- long_name.".
        assert_eq!(&text[7..20], "<- long_name.", "chip truncates to fit");
    }

    #[test]
    fn move_chip_uses_one_based_line_number() {
        let area = Rect::new(0, 0, 30, 1);
        let mut buf = Buffer::empty(area);
        let prov = MoveProvenance {
            rel_path: "x.rs".to_string(),
            line: 41,
            intra_file: false,
        };
        render_move_chip(&mut buf, 0, 0, 0, 30, &prov, Style::default());
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
        let prov = MoveProvenance {
            rel_path: String::new(),
            line: 41,
            intra_file: true,
        };
        render_move_chip(&mut buf, 0, 0, 0, 30, &prov, Style::default());
        let text = buffer_text(&buf, 0);
        assert!(
            text.contains("<- 42") && !text.contains(':'),
            "intra-file chip shows the 1-based line without a path; got {text:?}"
        );
    }
}
