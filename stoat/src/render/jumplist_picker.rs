use crate::{
    jumplist_picker::JumplistPicker,
    render::table::{self, Column, Width},
};
use ratatui::{buffer::Buffer, layout::Rect};

/// A one-column marker for the walk cursor, then the file, the position, and
/// the line's text. Headerless, since the marker column has nothing to label
/// and the rest are obvious.
///
/// The sized columns stay narrow because the list shares its body with a
/// preview. A file and a position wide enough to read whole would leave the
/// snippet, which is what a query names, nothing at all.
const COLUMNS: [Column; 4] = [
    Column {
        label: "",
        width: Width::Fixed(1),
    },
    Column {
        label: "file",
        width: Width::Fixed(14),
    },
    Column {
        label: "position",
        width: Width::Fixed(8),
    },
    Column {
        label: "snippet",
        width: Width::Fill,
    },
];

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_jumplist_picker(
    picker: &mut JumplistPicker,
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
        Some(" jumplist "),
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

    paint_jump_rows(picker, layout.list, theme, buf);
}

/// Paint the surviving jumps into `area`, following the selection.
///
/// The row the walk cursor sits on carries its own style, so a filtered list
/// still says where a plain backward jump would land. Matched characters of
/// the snippet carry the search-match style.
fn paint_jump_rows(
    picker: &mut JumplistPicker,
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

    let cursor_idx = picker.cursor_idx();
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
        let is_current = entry_idx == cursor_idx;
        let base_style = match (is_selected, is_current) {
            (true, _) => selected_style,
            (false, true) => prompt_style,
            (false, false) => row_style,
        };

        for col in area.x..area.x + area.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
        }

        let marker = match is_current {
            true => ">",
            false => " ",
        };
        let position = format!("{:>4}:{:<3}", entry.line, entry.column);
        let cells = [
            marker,
            entry.filename.as_str(),
            position.as_str(),
            entry.snippet.as_str(),
        ];

        // The whole row already carries the style that says which entry the
        // walk cursor is on, so no cell needs one of its own.
        table::paint_row(
            buf,
            Rect::new(area.x, row, area.width, 1),
            &cells,
            &widths,
            |_| base_style,
        );

        // The haystack joins the location to the snippet, so its offsets index
        // that joined string rather than any one column. Only offsets landing
        // in the snippet are painted, which is the part a query usually names.
        let indices = picker
            .picker
            .row_indices(row_idx, &mut derived, &mut matching);
        if indices.is_empty() {
            continue;
        }
        let snippet_x = area.x + table::column_starts(&widths)[3];
        let prefix_len = crate::jumplist_picker::haystack_prefix_len(entry);
        let end_x = area.x + area.width;
        for &index in indices {
            let Some(col) = index.checked_sub(prefix_len) else {
                continue;
            };
            let x = snippet_x + col as u16;
            if x < end_x {
                buf[(x, row)].set_style(match_style);
            }
        }
    }
}
