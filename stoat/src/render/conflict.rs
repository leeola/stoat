use crate::{
    pane::Pane,
    rebase::{ActiveRebase, RebasePause},
    render::{
        layout::split_pane_status,
        pane::render_overlay_status,
        text::{truncate_to_cols, write_str},
        FrameCtx,
    },
};
use ratatui::{buffer::Buffer, style::Style};

pub(crate) fn render_conflict(
    pane: &Pane,
    is_focused: bool,
    active: &ActiveRebase,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
) {
    use crate::rebase::ConflictResolution;

    let theme = frame.theme;
    let workspace_root = frame.workspace_root;
    let (inner, status_area) = split_pane_status(pane.area);
    render_overlay_status(status_area, is_focused, frame, "conflict", buf);
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
    let add_hl = theme.get(s::DIFF_ADDED);
    let del_hl = theme.get(s::DIFF_DELETED);

    for y in inner.y..inner.y + inner.height {
        buf[(sep_x, y)].set_char('│').set_style(dim);
    }

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

        draw_line(&mut y, "<<<<<<< ours", del_hl, buf);
        for line in file.ours.as_deref().unwrap_or("").lines() {
            draw_line(&mut y, line, file_style, buf);
        }
        draw_line(&mut y, "=======", dim, buf);
        for line in file.theirs.as_deref().unwrap_or("").lines() {
            draw_line(&mut y, line, file_style, buf);
        }
        draw_line(&mut y, ">>>>>>> theirs", add_hl, buf);
        if let Some(ancestor) = &file.ancestor {
            y += 1;
            draw_line(&mut y, "--- ancestor ---", dim, buf);
            for line in ancestor.lines() {
                draw_line(&mut y, line, dim, buf);
            }
        }
    }
}
