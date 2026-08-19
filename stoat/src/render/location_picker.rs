use crate::{
    location_picker::LocationPicker,
    render::table::{self, Column, Width},
    workspace::Workspace,
};
use ratatui::{buffer::Buffer, layout::Rect};
use std::path::Path;

/// The candidate's file, its line and column, and the line's text. Headerless,
/// since three columns of obvious content need no labels over them.
///
/// The sized columns stay narrow because the list shares its body with a
/// preview. A path and a position wide enough to read whole would leave the
/// candidate text, which is what a query names, nothing at all.
const COLUMNS: [Column; 3] = [
    Column {
        label: "path",
        width: Width::Fixed(18),
    },
    Column {
        label: "position",
        width: Width::Fixed(8),
    },
    Column {
        label: "text",
        width: Width::Fill,
    },
];

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_location_picker(
    picker: &mut LocationPicker,
    ws: &mut Workspace,
    git_root: &Path,
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
        Some(" locations "),
        modal_style,
        theme,
        &mut *scene,
    );

    crate::render::picker::filter_header(
        buf,
        layout.inner,
        ">",
        &picker.input,
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
        picker.preview_rows = Some(preview_rect.height as usize);
        crate::render::picker::render_picker_preview(
            &picker.preview,
            preview_rect,
            theme,
            chrome,
            ws,
            buf,
        );
    }

    paint_candidate_rows(picker, git_root, layout.list, theme, buf);
}

/// Paint the candidate rows into `area`, following the selection.
///
/// Each row shows the file dimmed as context, then the position it points at,
/// then the target line. Matched characters carry the search-match style, so a
/// filtered list shows why each row survived.
fn paint_candidate_rows(
    picker: &mut LocationPicker,
    git_root: &Path,
    area: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    picker.viewport_rows = Some(rows);

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let muted_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    // The table sits a column in from the border, and the path column carries
    // the whole width it asks for so a long path truncates rather than pushing
    // the position column around.
    let table_x = area.x + 1;
    let table_width = area.width.saturating_sub(1);
    let widths = table::resolve_widths(&COLUMNS, &[], table_width);

    let selected = picker.selected();
    let start = crate::render::picker::window_start(selected, rows);

    let mut derived = Vec::new();
    let mut matching = crate::fuzzy::Scratch::default();

    for offset in 0..rows.min(picker.filtered().len().saturating_sub(start)) {
        let row_idx = start + offset;
        let Some(entry) = picker
            .filtered()
            .get(row_idx)
            .and_then(|&idx| picker.entries().get(idx))
        else {
            continue;
        };
        let row = area.y + offset as u16;
        let is_selected = row_idx == selected;
        let base_style = match is_selected {
            true => selected_style,
            false => row_style,
        };
        for col in area.x..area.x + area.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
        }

        let path_text = table::display_path(&entry.path, git_root, widths[0] as usize);
        let position = format!("{:>4}:{:<3}", entry.line, entry.column);
        let cells = [path_text.as_str(), position.as_str(), entry.text.as_str()];

        table::paint_row(
            buf,
            Rect::new(table_x, row, table_width, 1),
            &cells,
            &widths,
            // The path is context rather than the candidate itself, so it reads
            // dimmer. A selected row keeps one style throughout, since the
            // highlight is what carries it.
            |column| match column {
                0 if !is_selected => muted_style,
                _ => base_style,
            },
        );

        // The haystack is `path:line text`, so its offsets index that joined
        // string rather than any one column. Only offsets landing in the text
        // column are painted, which is the part a query usually names.
        let indices = picker.row_indices(row_idx, &mut derived, &mut matching);
        if indices.is_empty() {
            continue;
        }
        let text_x = table_x + table::column_starts(&widths)[2];
        let prefix_len = format!("{}:{} ", entry.path.display(), entry.line)
            .chars()
            .count() as u32;
        let end_x = area.x + area.width;
        for &index in indices {
            let Some(col) = index.checked_sub(prefix_len) else {
                continue;
            };
            let x = text_x + col as u16;
            if x < end_x {
                buf[(x, row)].set_style(match_style);
            }
        }
    }
}
