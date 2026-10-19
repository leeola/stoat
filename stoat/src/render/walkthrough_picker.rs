use crate::{
    render::table::{self, Column, Width},
    walkthrough_picker::WalkthroughPicker,
};
use ratatui::{buffer::Buffer, layout::Rect};

/// A one-column marker for the tour that is open, then the slug, the stop
/// count, and the title. Headerless, since the marker column has nothing to
/// label and the rest read plainly.
///
/// The slug and the count stay narrow because the list shares its body with a
/// preview, and the title is what a reader recognizes a tour by.
const COLUMNS: [Column; 4] = [
    Column {
        label: "",
        width: Width::Fixed(1),
    },
    Column {
        label: "slug",
        width: Width::Fixed(20),
    },
    Column {
        label: "stops",
        width: Width::Fixed(5),
    },
    Column {
        label: "title",
        width: Width::Fill,
    },
];

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_walkthrough_picker(
    picker: &mut WalkthroughPicker,
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
        Some(" walkthroughs "),
        modal_style,
        theme,
        &mut *scene,
    );

    crate::render::picker::filter_header(
        buf,
        layout.inner,
        ">",
        &picker.picker.input,
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
        picker.picker.preview_rows = Some(preview_rect.height as usize);
        crate::render::picker::render_picker_preview(
            &picker.picker.preview,
            preview_rect,
            theme,
            chrome,
            ws,
            buf,
        );
    }

    paint_tour_rows(picker, layout.list, theme, buf);
}

/// Paint the surviving tours into `area`, following the selection.
///
/// The row of the tour that is open carries its own style and a `>` marker, so
/// a filtered list still says which tour plays. Matched characters of the
/// title carry the search-match style.
fn paint_tour_rows(
    picker: &mut WalkthroughPicker,
    area: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    picker.picker.viewport_rows = Some(rows);

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);
    let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    let playing = picker.playing_idx();
    let selected = picker.selected();

    // The marker column is flush against the border, so its own gap supplies
    // the pad the other pickers put in front of their table.
    let widths = table::resolve_widths(&COLUMNS, &[], area.width);
    let start = crate::render::picker::window_start(selected, rows);

    let mut derived = Vec::new();
    let mut matching = crate::fuzzy::Scratch::default();

    for offset in 0..rows.min(picker.filtered().len().saturating_sub(start)) {
        let row_idx = start + offset;
        let Some(&entry_idx) = picker.filtered().get(row_idx) else {
            continue;
        };
        let Some(entry) = picker.entries().get(entry_idx) else {
            continue;
        };
        let row = area.y + offset as u16;
        let is_selected = row_idx == selected;
        let is_playing = playing == Some(entry_idx);
        let base_style = match (is_selected, is_playing) {
            (true, _) => selected_style,
            (false, true) => prompt_style,
            (false, false) => row_style,
        };

        for col in area.x..area.x + area.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
        }

        let marker = match is_playing {
            true => ">",
            false => " ",
        };
        let stops = entry.stops.to_string();
        let cells = [
            marker,
            entry.slug.as_str(),
            stops.as_str(),
            entry.title.as_str(),
        ];

        // The whole row already carries the style that says whether its tour
        // is open, so no cell needs one of its own.
        table::paint_row(
            buf,
            Rect::new(area.x, row, area.width, 1),
            &cells,
            &widths,
            |_| base_style,
        );

        // The haystack joins the slug to the title, so its offsets index that
        // joined string rather than either column. Only offsets landing in the
        // title are painted, which is what a reader names a tour by.
        let indices = picker
            .picker
            .row_indices(row_idx, &mut derived, &mut matching);
        if indices.is_empty() {
            continue;
        }
        let title_x = area.x + table::column_starts(&widths)[3];
        let prefix_len = crate::walkthrough_picker::haystack_prefix_len(entry);
        let end_x = area.x + area.width;
        for &index in indices {
            let Some(col) = index.checked_sub(prefix_len) else {
                continue;
            };
            let x = title_x + col as u16;
            if x < end_x {
                buf[(x, row)].set_style(match_style);
            }
        }
    }
}
