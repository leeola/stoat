use crate::{
    commit_list::CommitListState,
    host::{CommitFileChange, CommitFileChangeKind},
    pane::Pane,
    render::{
        layout::split_pane_status,
        pane::render_overlay_status,
        review::{render_empty_num, render_side_num, render_side_text},
        text::{truncate_to_cols, write_str},
        FrameCtx,
    },
    review::ReviewRow,
    review_session::ReviewSession,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use std::path::Path;

pub(crate) fn render_commits(
    pane: &Pane,
    is_focused: bool,
    state: &mut CommitListState,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    mut scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    let theme = frame.theme;
    let workspace_root = frame.workspace_root;
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(status_area, is_focused, frame, buf);

    let Some(left_area) = commits_list_rect(pane.area) else {
        return;
    };
    let sep_x = left_area.x + left_area.width;
    let right_x = sep_x + 1;
    let right_w = inner.width.saturating_sub(left_area.width + 1);

    let sep_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    crate::render::chrome::vline(
        buf,
        sep_x,
        inner.y,
        inner.height,
        sep_style,
        scene.as_deref_mut(),
    );

    state.viewport_rows = left_area.height as usize;
    state.ensure_selected_visible(state.viewport_rows);
    render_commit_list_pane(state, theme, left_area, buf);

    if right_w > 0 {
        let right_area = Rect::new(right_x, inner.y, right_w, inner.height);
        render_commit_detail_pane(state, workspace_root, theme, right_area, buf, scene);
    }
}

/// The commit list's rectangle within an overlay pane, or `None` when the pane
/// is too small. Shared by the renderer and the smooth-scroll emit so the pooled
/// region matches the painted list.
pub(crate) fn commits_list_rect(pane_area: Rect) -> Option<Rect> {
    let (inner, _) = split_pane_status(pane_area);
    if inner.width < 10 || inner.height == 0 {
        return None;
    }
    let left_w = commit_list_width(inner.width);
    Some(Rect::new(inner.x, inner.y, left_w, inner.height))
}

fn commit_list_width(total: u16) -> u16 {
    let target = (total as u32 * 2 / 5) as u16;
    target.clamp(22, 48).min(total.saturating_sub(12))
}

fn render_commit_list_pane(
    state: &CommitListState,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    let top = state.scroll_top.min(state.commits.len().saturating_sub(1));
    paint_commit_rows(state, area, top, theme, buf);
}

/// Paint the commit list into `area` starting at row `start_row`, with the
/// selected row highlighted and a trailing load/end marker.
///
/// Shared by the live list, which derives `start_row` from `scroll_top`, and the
/// smooth-scroll pool, which paints absolute pages, so both render identical
/// rows.
pub(crate) fn paint_commit_rows(
    state: &CommitListState,
    area: Rect,
    start_row: usize,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    use crate::theme::scope as s;
    let dim = theme.get(s::VCS_COMMIT_METADATA);
    if state.commits.is_empty() {
        let msg = if state.pending_load.is_some() {
            "loading commits..."
        } else {
            "no commits"
        };
        write_str(buf, area.x, area.y, msg, dim);
        return;
    }

    let sel_style = theme.get(crate::theme::scope::UI_SELECTION_REVERSED);
    let sha_style = theme.get(s::VCS_COMMIT_SHA);
    let summary_style = theme.get(s::VCS_COMMIT_SUMMARY);

    let top = start_row.min(state.commits.len());
    let rows_visible = area.height as usize;
    let end = (top + rows_visible).min(state.commits.len());

    for (i, commit) in state.commits[top..end].iter().enumerate() {
        let y = area.y + i as u16;
        let is_selected = top + i == state.selected;
        let row_style = if is_selected {
            sel_style
        } else {
            summary_style
        };

        if is_selected && area.width > 0 {
            for x in area.x..area.x + area.width {
                buf[(x, y)].set_style(sel_style);
            }
        }

        let sha_x = area.x;
        let sha = &commit.short_sha;
        let sha_len = sha.len().min(area.width as usize);
        write_str(
            buf,
            sha_x,
            y,
            &sha[..sha_len],
            if is_selected { sel_style } else { sha_style },
        );

        let summary_x = sha_x + sha_len as u16 + 1;
        let remaining = (area.x + area.width).saturating_sub(summary_x);
        if remaining > 0 {
            let summary = truncate_to_cols(&commit.summary, remaining as usize);
            write_str(buf, summary_x, y, &summary, row_style);
        }
    }

    if state.pending_load.is_some() && end == state.commits.len() && end - top < rows_visible {
        let y = area.y + (end - top) as u16;
        write_str(buf, area.x, y, "loading more...", dim);
    } else if state.reached_end && end == state.commits.len() && end - top < rows_visible {
        let y = area.y + (end - top) as u16;
        write_str(buf, area.x, y, "(end of history)", dim);
    }
}

fn render_commit_detail_pane(
    state: &CommitListState,
    workspace_root: &Path,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    let dim = theme.get(crate::theme::scope::VCS_COMMIT_METADATA);
    let Some(sha) = state.selected_sha() else {
        write_str(buf, area.x, area.y, "no selection", dim);
        return;
    };

    let summary_rows = match state.summaries.get(sha) {
        Some(changes) => render_commit_summary(changes, workspace_root, theme, area, buf),
        None => {
            write_str(buf, area.x, area.y, "loading summary...", dim);
            1
        },
    };

    let preview_y = area.y + summary_rows as u16 + 1;
    if preview_y >= area.y + area.height {
        return;
    }
    let preview_area = Rect::new(
        area.x,
        preview_y,
        area.width,
        area.y + area.height - preview_y,
    );
    match state.preview_sessions.get(sha) {
        Some(session) => render_commit_preview(session, theme, preview_area, buf, scene),
        None => {
            if preview_area.height > 0 {
                write_str(
                    buf,
                    preview_area.x,
                    preview_area.y,
                    "loading preview...",
                    dim,
                );
            }
        },
    }
}

fn render_commit_summary(
    changes: &[CommitFileChange],
    workspace_root: &Path,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) -> usize {
    use crate::theme::scope as s;
    let header_style = theme.get(s::UI_TEXT).add_modifier(Modifier::BOLD);
    let path_style = theme.get(s::UI_TEXT);
    let add_style = theme.get(s::DIFF_ADDED);
    let del_style = theme.get(s::DIFF_DELETED);

    let total_add: u32 = changes.iter().map(|c| c.additions).sum();
    let total_del: u32 = changes.iter().map(|c| c.deletions).sum();
    let header = format!(
        "{} file{}, +{total_add} -{total_del}",
        changes.len(),
        if changes.len() == 1 { "" } else { "s" }
    );
    write_str(buf, area.x, area.y, &header, header_style);

    let mut rows_used = 1;
    let max_rows = (area.height as usize).saturating_sub(1);
    for (i, change) in changes.iter().take(max_rows).enumerate() {
        let y = area.y + 1 + i as u16;
        let kind_char = match change.kind {
            CommitFileChangeKind::Added => 'A',
            CommitFileChangeKind::Modified => 'M',
            CommitFileChangeKind::Deleted => 'D',
            CommitFileChangeKind::Renamed => 'R',
            CommitFileChangeKind::TypeChange => 'T',
        };
        write_str(buf, area.x, y, &format!("{kind_char} "), path_style);
        let rel = crate::paths::display_relative(&change.rel_path, workspace_root);
        let path_width = area.width.saturating_sub(2 + 12) as usize;
        let rel = truncate_to_cols(&rel, path_width);
        write_str(buf, area.x + 2, y, &rel, path_style);

        let stats = format!(" +{} -{}", change.additions, change.deletions);
        let stats_x = area.x + area.width.saturating_sub(stats.len() as u16);
        let split = stats.find('-').unwrap_or(stats.len());
        write_str(buf, stats_x, y, &stats[..split], add_style);
        write_str(buf, stats_x + split as u16, y, &stats[split..], del_style);
        rows_used += 1;
    }
    rows_used
}

/// Render a compact preview of a [`ReviewSession`]: each chunk's rows
/// painted sequentially with a yellow file/chunk header, top-to-bottom
/// within `area`. Does not rely on editor machinery; used by the
/// commits view's right pane.
fn render_commit_preview(
    session: &ReviewSession,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    mut scene: Option<&mut stoatty_widgets::ApcScene>,
) {
    if area.height == 0 || area.width == 0 {
        return;
    }
    use crate::theme::scope as s;
    let dim = theme.get(s::UI_TEXT_MUTED);
    let header_style = theme.get(s::VCS_COMMIT_SHA);
    let del_hl = theme.get(s::DIFF_DELETED);
    let add_hl = theme.get(s::DIFF_ADDED);
    let move_hl = theme.get(s::DIFF_MOVED).add_modifier(Modifier::ITALIC);
    let fallback_style = Style::default();

    let full_w = area.width as usize;
    let status_w: usize = 1;
    let num_w: usize = 5;
    let gutter_w = status_w + num_w;
    let sep: usize = 1;
    let half_w = (full_w.saturating_sub(sep)) / 2;
    let left_content_w = half_w.saturating_sub(gutter_w);
    let right_start = area.x + half_w as u16 + sep as u16;
    let right_content_w = (full_w - half_w - sep).saturating_sub(gutter_w);
    let sep_x = area.x + half_w as u16;

    let mut y = area.y;
    let end_y = area.y + area.height;

    for file in &session.files {
        for chunk_id in &file.chunks {
            let Some(chunk) = session.chunks.get(chunk_id) else {
                continue;
            };
            if y >= end_y {
                return;
            }
            let file_total = file.chunks.len();
            let lang_str = file
                .language
                .as_ref()
                .map(|l| l.name.to_string())
                .unwrap_or_default();
            let label = format!(
                "{} --- {}/{} --- {}",
                file.rel_path,
                chunk.chunk_index_in_file + 1,
                file_total,
                lang_str
            );
            let label_trunc = truncate_to_cols(&label, area.width as usize);
            write_str(buf, area.x, y, &label_trunc, header_style);
            y += 1;

            for row in &chunk.hunk.rows {
                if y >= end_y {
                    return;
                }
                if sep_x < area.x + area.width {
                    crate::render::chrome::vline(buf, sep_x, y, 1, dim, scene.as_deref_mut());
                }
                let left_num_x = area.x + status_w as u16;
                let right_num_x = right_start + status_w as u16;
                let left_text_x = left_num_x + num_w as u16;
                let right_text_x = right_num_x + num_w as u16;
                match row {
                    ReviewRow::Context { left, right } => {
                        render_side_num(buf, left_num_x, y, left.line_num, dim);
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
                        render_side_num(buf, right_num_x, y, right.line_num, dim);
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
                            render_side_num(buf, left_num_x, y, l.line_num, dim);
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
                            render_empty_num(buf, left_num_x, y, dim);
                        }
                        if let Some(r) = right {
                            render_side_num(buf, right_num_x, y, r.line_num, dim);
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
                            render_empty_num(buf, right_num_x, y, dim);
                        }
                    },
                }
                y += 1;
            }
        }
    }
}
