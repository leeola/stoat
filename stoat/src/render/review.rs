use crate::{display_map::BlockRowKind, editor_state::EditorState, review::ReviewRow};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};

pub(crate) fn render_review(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let snapshot = editor.display_map.snapshot();
    let view = match editor.review_view.as_ref() {
        Some(v) => v,
        None => return,
    };
    let rows = &view.rows;
    let total_rows = snapshot.line_count();
    let visible = inner.height as u32;
    let end_row = (editor.scroll_row + visible).min(total_rows);
    if end_row <= editor.scroll_row {
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

    for display_row in editor.scroll_row..end_row {
        let y = inner.y + (display_row - editor.scroll_row) as u16;
        if y >= inner.y + inner.height {
            break;
        }

        let sep_x = inner.x + half_w as u16;
        if sep_x < inner.x + inner.width {
            buf[(sep_x, y)].set_char('│').set_style(dim_style);
        }

        match snapshot.classify_row(display_row) {
            BlockRowKind::BufferRow { buffer_row } => {
                let Some(row) = rows.get(buffer_row as usize) else {
                    continue;
                };
                if let Some((chunk_id, status)) = view.chunk_and_status_at_row(buffer_row) {
                    let is_current = Some(chunk_id) == view.current_chunk;
                    paint_status_gutter(buf, inner.x, y, status, is_current, current_style, theme);
                    paint_status_gutter(
                        buf,
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
                        render_side_num(buf, left_num_x, y, left.line_num, dim_style);
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
                        render_side_num(buf, right_num_x, y, right.line_num, dim_style);
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
                            render_side_num(buf, left_num_x, y, l.line_num, dim_style);
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
                        } else {
                            render_empty_num(buf, left_num_x, y, dim_style);
                        }
                        if let Some(r) = right {
                            render_side_num(buf, right_num_x, y, r.line_num, dim_style);
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
                        } else {
                            render_empty_num(buf, right_num_x, y, dim_style);
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
    status: crate::review_session::ChunkStatus,
    is_current: bool,
    current_style: Style,
    theme: &crate::theme::Theme,
) {
    use crate::{review_session::ChunkStatus, theme::scope as s};

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
