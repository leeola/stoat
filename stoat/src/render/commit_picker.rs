use crate::{
    commit_picker::{CommitPicker, CommitPickerRole},
    render::{file_finder::file_finder_layout, text::write_str_clipped},
    theme::{scope, Theme},
    workspace::Workspace,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Clear, Widget},
};

pub(crate) fn render_commit_picker(
    picker: &mut CommitPicker,
    ws: &mut Workspace,
    theme: &Theme,
    area: Rect,
    zoom: i8,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    // A commit row plus its diff preview both want room, and the history behind
    // them is arbitrarily long, so the picker asks for the whole area rather
    // than measuring a list it would only ever outgrow.
    let Some(layout) = file_finder_layout(area, (u16::MAX, u16::MAX), zoom) else {
        return;
    };

    // The same rows serve both roles, so the title is what tells the user
    // whether picking one starts a review or just closes the listing.
    let title = match picker.role {
        CommitPickerRole::PickBase => " review from commit ",
        CommitPickerRole::Browse => " git log ",
    };
    let modal_style = theme.get(scope::UI_MODAL_PALETTE);
    Clear.render(layout.modal, buf);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(title),
        modal_style,
        theme,
        &mut *scene,
    );

    let inner = layout.inner;
    let separator_style = theme.get(scope::UI_BORDER_INACTIVE);

    crate::render::text::write_str(buf, inner.x, inner.y, ">", theme.get(scope::UI_PROMPT));
    let input_area = Rect::new(inner.x + 2, inner.y, inner.width.saturating_sub(2), 1);
    picker.input.render(
        &mut ws.editors,
        input_area,
        true,
        "prompt",
        theme,
        &std::collections::BTreeMap::new(),
        buf,
    );

    crate::render::chrome::hline(
        buf,
        inner.x,
        inner.y + 1,
        inner.width,
        separator_style,
        Some(&mut *scene),
    );

    if let Some(preview_rect) = layout.preview {
        crate::render::chrome::vline(
            buf,
            layout.list.x + layout.list.width,
            layout.list.y,
            layout.list.height,
            separator_style,
            scene,
        );
        render_preview(picker, preview_rect, theme, buf, scene);
    }

    picker.viewport_rows = Some(layout.list.height as usize);
    paint_commit_picker_rows(picker, layout.list, theme, buf);
}

/// Paint the commit rows into `area`, following the selection so the selected
/// row stays visible.
///
/// A row is the picker's own row text, painted as one string so a fuzzy match
/// offset lands on the character it matched. The sha and branch segments are
/// recolored in place afterwards rather than written separately, which is what
/// keeps the columns and the highlights consistent.
fn paint_commit_picker_rows(picker: &CommitPicker, area: Rect, theme: &Theme, buf: &mut Buffer) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let start_row = picker.selected.saturating_sub(rows.saturating_sub(1));

    let row_style = theme.get(scope::UI_TEXT);
    let selected_style = theme.get(scope::UI_SELECTION);
    let match_style = theme.get(scope::UI_SEARCH_MATCH);
    let sha_style = theme.get(scope::VCS_COMMIT_SHA);
    let branch_style = theme.get(scope::VCS_COMMIT_METADATA);

    let end_x = area.x + area.width;
    let text_x = area.x + 1;

    for (row_idx, (&idx, indices)) in picker
        .filtered
        .iter()
        .zip(picker.match_indices.iter())
        .skip(start_row)
        .take(rows)
        .enumerate()
    {
        let y = area.y + row_idx as u16;
        let is_selected = start_row + row_idx == picker.selected;
        let style = if is_selected {
            selected_style
        } else {
            row_style
        };
        for col in area.x..end_x {
            buf[(col, y)].set_char(' ').set_style(style);
        }

        let row = picker.row(idx);
        write_str_clipped(buf, text_x, y, &row.text, style, end_x);

        if !is_selected {
            let sha_end = text_x + row.sha_chars as u16;
            recolor(buf, y, text_x, sha_end.min(end_x), sha_style);

            let branch_start = sha_end + 1;
            let branch_end = branch_start + row.branch_chars as u16;
            recolor(buf, y, branch_start, branch_end.min(end_x), branch_style);
        }

        for &offset in indices {
            let col = text_x + offset as u16;
            if col >= end_x {
                break;
            }
            buf[(col, y)].set_style(match_style);
        }
    }
}

fn recolor(buf: &mut Buffer, y: u16, from_x: u16, to_x: u16, style: ratatui::style::Style) {
    for col in from_x..to_x {
        buf[(col, y)].set_style(style);
    }
}

/// Draw the selected commit's cached diff, or a placeholder while its
/// background build is still running.
fn render_preview(
    picker: &CommitPicker,
    area: Rect,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let session = picker
        .selected_commit()
        .and_then(|commit| picker.preview_sessions.get(&commit.sha));
    match session {
        Some(session) => {
            crate::render::commits::render_commit_preview(session, theme, area, buf, scene)
        },
        None => {
            write_str_clipped(
                buf,
                area.x,
                area.y,
                "loading diff...",
                theme.get(scope::UI_TEXT_MUTED),
                area.x + area.width,
            );
        },
    }
}
