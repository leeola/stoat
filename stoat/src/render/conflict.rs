use crate::{
    merge_view,
    pane::Pane,
    rebase::{ActiveRebase, RebasePause},
    render::{
        layout::split_pane_status,
        pane::render_overlay_status,
        review::{render_empty_num, render_side_num, render_side_text},
        text::{truncate_to_cols, write_str},
        FrameCtx,
    },
    review::ReviewSide,
};
use ratatui::{buffer::Buffer, style::Style};

pub(crate) fn render_conflict(
    pane: &Pane,
    is_focused: bool,
    active: &ActiveRebase,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    use crate::rebase::ConflictResolution;

    let theme = frame.theme;
    let workspace_root = frame.workspace_root;
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(status_area, is_focused, frame, buf, &mut *scene);
    if inner.width < 20 || inner.height < 4 {
        return;
    }

    let (source_sha, files, selected, resolutions) = match active.pause.as_ref() {
        Some(RebasePause::Conflict {
            source_sha,
            files,
            selected,
            resolutions,
        }) => (source_sha, files, *selected, resolutions),
        _ => return,
    };

    let left_w = (inner.width / 3).max(20);
    let left_w = left_w.min(inner.width.saturating_sub(20));
    let sep_x = inner.x + left_w;
    let right_x = sep_x + 1;
    let right_w = inner.width.saturating_sub(left_w + 1);

    use crate::theme::scope as s;
    let dim = theme.get(s::UI_TEXT_MUTED);
    let header_style = theme.get(s::VCS_CONFLICT_HEADER);
    let sel_style = theme.get(crate::theme::scope::UI_SELECTION_REVERSED);
    let ours_style = theme.get(s::VCS_CONFLICT_OURS);
    let theirs_style = theme.get(s::VCS_CONFLICT_THEIRS);
    let file_style = theme.get(s::UI_TEXT);

    crate::render::chrome::vline(buf, sep_x, inner.y, inner.height, dim, scene);

    let short = source_sha.chars().take(7).collect::<String>();
    write_str(
        buf,
        inner.x,
        inner.y,
        &truncate_to_cols(&format!("conflict picking {short}"), inner.width as usize),
        header_style,
    );
    write_str(
        buf,
        inner.x,
        inner.y + 1,
        &truncate_to_cols(
            "o take ours  t take theirs  s skip entry  Enter commit  a abort",
            inner.width as usize,
        ),
        dim,
    );

    let list_top = inner.y + 3;
    for (i, file) in files.iter().enumerate() {
        let y = list_top + i as u16;
        if y >= inner.y + inner.height {
            break;
        }
        let is_selected = i == selected;
        let row_style = if is_selected { sel_style } else { file_style };
        if is_selected {
            for x in inner.x..inner.x + left_w {
                buf[(x, y)].set_style(sel_style);
            }
        }
        let marker = match resolutions.get(&file.path).copied() {
            Some(ConflictResolution::TakeOurs) => 'O',
            Some(ConflictResolution::TakeTheirs) => 'T',
            Some(ConflictResolution::SkipEntry) => 'S',
            None => '?',
        };
        let marker_style = match resolutions.get(&file.path).copied() {
            Some(ConflictResolution::TakeOurs) => ours_style,
            Some(ConflictResolution::TakeTheirs) => theirs_style,
            _ => dim,
        };
        write_str(
            buf,
            inner.x,
            y,
            &format!("{marker} "),
            if is_selected { sel_style } else { marker_style },
        );
        let path_x = inner.x + 2;
        let path_max = (left_w as usize).saturating_sub(2);
        write_str(
            buf,
            path_x,
            y,
            &truncate_to_cols(
                &crate::paths::display_relative(&file.path, workspace_root),
                path_max,
            ),
            row_style,
        );
    }

    if let Some(file) = files.get(selected) {
        let mut y = inner.y + 3;
        let max_y = inner.y + inner.height;
        let max_w = right_w as usize;

        let draw_line = |y: &mut u16, text: &str, style: Style, buf: &mut Buffer| {
            if *y >= max_y {
                return;
            }
            write_str(buf, right_x, *y, &truncate_to_cols(text, max_w), style);
            *y += 1;
        };

        let header = match resolutions.get(&file.path).copied() {
            Some(ConflictResolution::TakeOurs) => ("will take OURS", ours_style),
            Some(ConflictResolution::TakeTheirs) => ("will take THEIRS", theirs_style),
            Some(ConflictResolution::SkipEntry) => ("entry skipped", dim),
            None => ("unresolved (defaults to theirs)", dim),
        };
        draw_line(&mut y, header.0, header.1, buf);
        y += 1;

        let col_w = right_w / 3;
        let ours_x = right_x;
        let base_x = right_x + col_w;
        let theirs_x = right_x + 2 * col_w;
        let text_cols = (col_w as usize).saturating_sub(6);

        if y < max_y {
            write_str(buf, ours_x, y, "ours", ours_style);
            write_str(buf, base_x, y, "ancestor", dim);
            write_str(buf, theirs_x, y, "theirs", theirs_style);
            y += 1;
        }

        let ancestor = file.ancestor.as_deref().unwrap_or("");
        let ours = file.ours.as_deref().unwrap_or("");
        let theirs = file.theirs.as_deref().unwrap_or("");
        for row in merge_view::build_merge_rows(ancestor, ours, theirs, None) {
            if y >= max_y {
                break;
            }
            // A conflict row (both sides changed the ancestor line) tints with
            // the conflict header so the divergence stands out across columns.
            let base_style = if row.conflict {
                header_style
            } else {
                file_style
            };
            paint_merge_side(
                buf,
                ours_x,
                y,
                row.ours.as_ref(),
                text_cols,
                base_style,
                ours_style,
                dim,
            );
            paint_merge_side(
                buf,
                base_x,
                y,
                row.base.as_ref(),
                text_cols,
                base_style,
                base_style,
                dim,
            );
            paint_merge_side(
                buf,
                theirs_x,
                y,
                row.theirs.as_ref(),
                text_cols,
                base_style,
                theirs_style,
                dim,
            );
            y += 1;
        }
    }
}

/// Paint one column of a merge row. A present side renders a muted line number
/// and its text with change spans highlighted. An absent side renders a
/// placeholder gutter, which happens for a deletion or a one-sided insertion the
/// other column carries.
#[allow(clippy::too_many_arguments)]
fn paint_merge_side(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    side: Option<&ReviewSide>,
    text_cols: usize,
    base_style: Style,
    highlight_style: Style,
    dim: Style,
) {
    match side {
        Some(side) => {
            render_side_num(buf, x, y, side.line_num, dim);
            render_side_text(
                buf,
                x + 5,
                y,
                &side.text,
                text_cols,
                base_style,
                &side.change_spans,
                highlight_style,
                &side.moved_spans,
                base_style,
            );
        },
        None => render_empty_num(buf, x, y, dim),
    }
}
