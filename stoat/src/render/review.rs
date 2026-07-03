use crate::{
    display_map::{BlockRowKind, DisplaySnapshot},
    editor_state::EditorState,
    review::{MoveProvenance, ReviewRow},
    review_session::{ChunkStatus, ReviewViewState},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    widgets::StatefulWidget,
};
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
    let Some(view) = editor.review_view.as_ref() else {
        return;
    };
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
                bg: rg.colors.bg,
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

/// Paint a `<- {rel_path}:{line+1}` chip after the rendered side text
/// to surface that the moved hunk's source lives in a different file
/// of the same review session. `text_cols` is the column count
/// already consumed by [`render_side_text`]; the chip starts two
/// columns later (so the gap is visually obvious) and truncates if
/// fewer columns remain. No-op when `text_cols + 2 >= max_cols`.
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
    let chip = format!("<- {}:{}", prov.rel_path, prov.line + 1);
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

    fn buffer_text(buf: &Buffer, y: u16) -> String {
        (buf.area.x..buf.area.x + buf.area.width)
            .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    #[test]
    fn move_chip_paints_text_after_two_col_gap() {
        let area = Rect::new(0, 0, 50, 1);
        let mut buf = Buffer::empty(area);
        let prov = MoveProvenance {
            rel_path: "a.rs".to_string(),
            line: 0,
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
        };
        render_move_chip(&mut buf, 0, 0, 0, 30, &prov, Style::default());
        let text = buffer_text(&buf, 0);
        assert!(
            text.contains("<- x.rs:42"),
            "chip prints 1-based line number; got {text:?}"
        );
    }
}
