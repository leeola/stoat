use crate::{
    pane_finder::PaneFinder,
    render::table::{self, Column, Width},
};
use ratatui::{buffer::Buffer, layout::Rect};

/// The view's kind, its title, and where it shows. Headerless, since the three
/// columns read plainly.
///
/// The kind and the place stay narrow because the list shares its body with a
/// preview, and the title is what a reader knows a view by.
const COLUMNS: [Column; 3] = [
    Column {
        label: "kind",
        width: Width::Fixed(8),
    },
    Column {
        label: "title",
        width: Width::Fill,
    },
    Column {
        label: "place",
        width: Width::Fixed(8),
    },
];

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_pane_finder(
    finder: &mut PaneFinder,
    ws: &mut crate::workspace::Workspace,
    theme: &crate::theme::Theme,
    chrome: &crate::render::editor::ResolvedChrome,
    area: Rect,
    zoom: i8,
    list_percent: u16,
    buf: &mut Buffer,
    scene: &mut stoat_widgets::ApcScene,
) {
    let Some(layout) = crate::render::picker::target_picker_layout(area, zoom, list_percent) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    crate::render::clear_themed(layout.modal, buf, theme);
    crate::render::chrome::modal_frame(
        buf,
        layout.modal,
        Some(" panes "),
        modal_style,
        theme,
        &mut *scene,
    );

    crate::render::picker::filter_header(
        buf,
        layout.inner,
        ">",
        &finder.picker.input,
        ws,
        theme,
        chrome,
        &mut *scene,
    );

    if let Some(preview_rect) = layout.preview {
        crate::render::chrome::vline(
            buf,
            layout.list.x + layout.list.width,
            layout.list.y,
            layout.list.height,
            theme.get(crate::theme::scope::UI_BORDER_INACTIVE),
            scene,
        );
        finder.picker.preview_rows = Some(preview_rect.height as usize);
        crate::render::picker::render_picker_preview(
            &finder.picker.preview,
            preview_rect,
            theme,
            chrome,
            ws,
            buf,
        );
    }

    paint_view_rows(finder, layout.list, theme, buf);
}

/// Paint the surviving views into `area`, following the selection.
///
/// Matched characters of the title carry the search-match style.
fn paint_view_rows(
    finder: &mut PaneFinder,
    area: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    finder.picker.viewport_rows = Some(rows);

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    let selected = finder.selected();

    // The table sits a column in from the border, as in the pickers with no
    // marker column.
    let table_x = area.x + 1;
    let table_width = area.width.saturating_sub(1);
    let widths = table::resolve_widths(&COLUMNS, &[], table_width);
    let start = crate::render::picker::window_start(selected, rows);

    let mut derived = Vec::new();
    let mut matching = crate::fuzzy::Scratch::default();

    for offset in 0..rows.min(finder.filtered().len().saturating_sub(start)) {
        let row_idx = start + offset;
        let Some(entry) = finder
            .filtered()
            .get(row_idx)
            .and_then(|&idx| finder.entries().get(idx))
        else {
            continue;
        };
        let row = area.y + offset as u16;
        let base_style = match row_idx == selected {
            true => selected_style,
            false => row_style,
        };

        for col in area.x..area.x + area.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
        }

        let place = entry.place.label();
        let cells = [entry.kind, entry.title.as_str(), place.as_str()];
        table::paint_row(
            buf,
            Rect::new(table_x, row, table_width, 1),
            &cells,
            &widths,
            |_| base_style,
        );

        // The haystack joins the kind to the title, so its offsets index that
        // joined string rather than either column. Only offsets landing in the
        // title are painted, which is what a reader names a view by. The place
        // column follows the title, so the paint stops at the title's end.
        let indices = finder
            .picker
            .row_indices(row_idx, &mut derived, &mut matching);
        if indices.is_empty() {
            continue;
        }
        let title_x = table_x + table::column_starts(&widths)[1];
        let title_end = title_x + widths[1];
        let prefix_len = crate::pane_finder::haystack_prefix_len(entry);
        for &index in indices {
            let Some(col) = index.checked_sub(prefix_len) else {
                continue;
            };
            let x = title_x + col as u16;
            if x < title_end {
                buf[(x, row)].set_style(match_style);
            }
        }
    }
}
