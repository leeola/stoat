use crate::{
    commit_picker::{CommitColumn, CommitPicker, CommitPickerRole, CommitRow, COMMIT_COLUMNS},
    render::{
        file_finder::file_finder_layout,
        table::{self, Column, Width},
        text::write_str_clipped,
    },
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
    paint_commit_picker_rows(picker, layout.header, layout.list, theme, buf);
}

/// The commit table's columns, in [`CommitColumn`] order.
///
/// The title fills, since it is the column worth the most room and the only one
/// whose useful length has no bound. The rest size to what they hold: a sha
/// stops at the abbreviation git prints, a branch or author gets enough to
/// recognise without crowding out the title, and a relative age never needs
/// more than a few characters.
const COLUMNS: [Column; COMMIT_COLUMNS] = [
    Column {
        label: "Commit",
        width: Width::Fit { min: 7, max: 12 },
    },
    Column {
        label: "Branch",
        width: Width::Fit { min: 6, max: 20 },
    },
    Column {
        label: "Title",
        width: Width::Fill,
    },
    Column {
        label: "Author",
        width: Width::Fit { min: 6, max: 16 },
    },
    Column {
        label: "Date",
        // A relative age never exceeds four characters ("12mo" is the longest
        // any bucket produces), and the label is four too.
        width: Width::Fixed(4),
    },
];

/// The columns in display order, so a cell index resolves to the column it
/// belongs to. Parallel to [`COLUMNS`].
const COLUMN_ORDER: [CommitColumn; COMMIT_COLUMNS] = [
    CommitColumn::Commit,
    CommitColumn::Branch,
    CommitColumn::Title,
    CommitColumn::Author,
    CommitColumn::Date,
];

/// Paint the commit table into `list`, with its column labels in `header`,
/// following the selection so the selected row stays visible.
///
/// Widths resolve over the rows actually about to be painted, so the columns
/// fit what is on screen rather than the whole history. Match highlights are
/// translated out of each row's joined haystack onto the column showing them,
/// and one whose cell truncated it away is dropped.
fn paint_commit_picker_rows(
    picker: &CommitPicker,
    header: Rect,
    area: Rect,
    theme: &Theme,
    buf: &mut Buffer,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let start_row = picker.selected.saturating_sub(rows.saturating_sub(1));

    let visible: Vec<CommitRow> = picker
        .filtered
        .iter()
        .skip(start_row)
        .take(rows)
        .map(|&idx| picker.row(idx))
        .collect();

    let text_x = area.x + 1;
    let table_width = area.width.saturating_sub(1);
    let widest: Vec<u16> = (0..COMMIT_COLUMNS)
        .map(|column| {
            visible
                .iter()
                .map(|row| row.cells[column].text.chars().count() as u16)
                .max()
                .unwrap_or(0)
        })
        .collect();
    let widths = table::resolve_widths(&COLUMNS, &widest, table_width);
    let starts = table::column_starts(&widths);

    let header_area = Rect::new(text_x, header.y, table_width, header.height);
    table::paint_header(
        buf,
        header_area,
        &COLUMNS,
        &widths,
        theme.get(scope::UI_TEXT_MUTED),
    );

    let row_style = theme.get(scope::UI_TEXT);
    let selected_style = theme.get(scope::UI_SELECTION);
    let match_style = theme.get(scope::UI_SEARCH_MATCH);
    let sha_style = theme.get(scope::VCS_COMMIT_SHA);
    let branch_style = theme.get(scope::VCS_COMMIT_METADATA);
    let end_x = area.x + area.width;

    for (row_idx, (row, indices)) in visible
        .iter()
        .zip(picker.match_indices.iter().skip(start_row))
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

        let bounds = Rect::new(text_x, y, table_width, 1);
        for (column, (&width, &start)) in widths.iter().zip(starts.iter()).enumerate() {
            // The selection's own background carries the row, so leaving the
            // metadata colors off it keeps the highlight readable.
            let cell_style = if is_selected {
                style
            } else {
                match COLUMN_ORDER[column] {
                    CommitColumn::Commit => sha_style,
                    CommitColumn::Branch | CommitColumn::Author | CommitColumn::Date => {
                        branch_style
                    },
                    CommitColumn::Title => style,
                }
            };
            table::paint_cell(
                buf,
                text_x + start,
                y,
                &row.cells[column].text,
                width,
                cell_style,
                bounds,
            );
        }

        let cell_starts: Vec<usize> = row.cells.iter().map(|cell| cell.start).collect();
        let cell_lens: Vec<usize> = row
            .cells
            .iter()
            .map(|cell| cell.text.chars().count())
            .collect();
        for &offset in indices {
            let Some(col) =
                table::cell_column(offset as usize, &cell_starts, &cell_lens, &widths, &starts)
            else {
                continue;
            };
            let col = text_x + col;
            if col < end_x {
                buf[(col, y)].set_style(match_style);
            }
        }
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
