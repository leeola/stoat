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

/// Share of the body the commit list takes, the rest going to the diff below
/// it. A percentage rather than a row count so the split holds at any modal
/// size.
const LIST_BODY_PERCENT: u16 = 40;

/// Rows the list keeps however small the body gets, so a short modal still
/// shows enough history to choose from.
const MIN_LIST_ROWS: u16 = 5;

/// The on-screen rectangles of the commit picker modal.
///
/// The diff preview sits *below* the list rather than beside it, so each side
/// of a changed line gets the modal's full width instead of half of it.
///
/// Shared by the renderer and the wheel hit-test so a scroll lands on the
/// region it appears over.
pub(crate) struct CommitPickerLayout {
    /// The bordered modal box.
    pub(crate) modal: Rect,
    /// Inside the border: input row, separator, header, list, then preview.
    pub(crate) inner: Rect,
    /// The commit table's column labels, above the rows they name. Reserved
    /// here and painted when the rows become a table, so the rects below it
    /// settle once rather than shifting a row later.
    #[allow(dead_code)]
    pub(crate) header: Rect,
    /// The commit rows.
    pub(crate) list: Rect,
    /// The selected commit's diff, at the modal's full width. `None` when the
    /// body has no rows left for it.
    pub(crate) preview: Option<Rect>,
}

/// Lay out the commit picker within `area`, or `None` when `area` is too small
/// to host it.
///
/// Takes its box from [`file_finder_layout`] so the picker sizes and centers
/// like the rest of the modal family, then splits the body horizontally rather
/// than using that layout's side-by-side split.
pub(crate) fn commit_picker_layout(area: Rect, zoom: i8) -> Option<CommitPickerLayout> {
    // A commit row plus its diff preview both want room, and the history behind
    // them is arbitrarily long, so the picker asks for the whole area rather
    // than measuring a list it would only ever outgrow.
    let box_layout = file_finder_layout(area, (u16::MAX, u16::MAX), zoom)?;
    let inner = box_layout.inner;

    let header = Rect::new(inner.x, inner.y + 2, inner.width, 1);
    let body_top = header.y + 1;
    let body_height = (inner.y + inner.height).saturating_sub(body_top);

    let list_height = (body_height * LIST_BODY_PERCENT / 100)
        .max(MIN_LIST_ROWS)
        .min(body_height);
    let list = Rect::new(inner.x, body_top, inner.width, list_height);

    // One row goes to the separator between the two, so a body with nothing
    // left over shows the list alone.
    let preview_height = body_height.saturating_sub(list_height + 1);
    let preview = (preview_height > 0).then(|| {
        Rect::new(
            inner.x,
            list.y + list_height + 1,
            inner.width,
            preview_height,
        )
    });

    Some(CommitPickerLayout {
        modal: box_layout.modal,
        inner,
        header,
        list,
        preview,
    })
}

pub(crate) fn render_commit_picker(
    picker: &mut CommitPicker,
    ws: &mut Workspace,
    theme: &Theme,
    area: Rect,
    zoom: i8,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let Some(layout) = commit_picker_layout(area, zoom) else {
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
        crate::render::chrome::hline(
            buf,
            layout.list.x,
            layout.list.y + layout.list.height,
            layout.list.width,
            separator_style,
            Some(&mut *scene),
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
