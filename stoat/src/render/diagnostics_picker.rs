use crate::{
    diagnostics_picker::{DiagnosticsPicker, PickerScope},
    render::table::{self, Column, Width},
};
use lsp_types::DiagnosticSeverity;
use ratatui::{buffer::Buffer, layout::Rect};
use std::path::Path;

/// Position, severity, and message, for the list of one buffer's diagnostics.
/// Both tables are headerless, since a severity glyph and a `line:column` need
/// no labels over them.
///
/// The sized columns stay narrow because the list shares its body with a
/// preview. A position and a path wide enough to read whole would leave the
/// message, which is what a query names, nothing at all.
const LOCAL_COLUMNS: [Column; 3] = [
    Column {
        label: "position",
        width: Width::Fixed(8),
    },
    Column {
        label: "severity",
        width: Width::Fixed(2),
    },
    Column {
        label: "message",
        width: Width::Fill,
    },
];

/// The same, behind a path column, for the list spanning every buffer. A
/// workspace diagnostic is meaningless without the file it came from, which is
/// why this is a different table rather than the local one with a blank column.
const WORKSPACE_COLUMNS: [Column; 4] = [
    Column {
        label: "path",
        width: Width::Fixed(18),
    },
    LOCAL_COLUMNS[0],
    LOCAL_COLUMNS[1],
    LOCAL_COLUMNS[2],
];

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_diagnostics_picker(
    picker: &mut DiagnosticsPicker,
    ws: &mut crate::workspace::Workspace,
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
    let (modal_area, inner) = (layout.modal, layout.inner);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    let title = match picker.scope() {
        PickerScope::Local(_) => " diagnostics ",
        PickerScope::Workspace => " diagnostics (workspace) ",
    };
    crate::render::clear_themed(modal_area, buf, theme);
    crate::render::chrome::modal_frame(
        buf,
        modal_area,
        Some(title),
        modal_style,
        theme,
        &mut *scene,
    );

    crate::render::picker::filter_header(
        buf,
        inner,
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

    paint_diagnostic_rows(picker, git_root, layout.list, theme, buf);
}

/// Paint the surviving diagnostics into `area`, following the selection.
///
/// Matched characters of the message carry the search-match style, so a
/// filtered list shows why each row survived.
fn paint_diagnostic_rows(
    picker: &mut DiagnosticsPicker,
    git_root: &Path,
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
    let muted_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);
    let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);

    let workspace_scope = matches!(picker.scope(), PickerScope::Workspace);
    let columns: &[Column] = match workspace_scope {
        true => &WORKSPACE_COLUMNS,
        false => &LOCAL_COLUMNS,
    };
    let table_x = area.x + 1;
    let table_width = area.width.saturating_sub(1);
    let widths = table::resolve_widths(columns, &[], table_width);

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

        let path_text = match workspace_scope {
            true => entry
                .path
                .as_deref()
                .map(|p| table::display_path(p, git_root, widths[0] as usize))
                .unwrap_or_default(),
            false => String::new(),
        };
        let position = format!("{:>4}:{:<3}", entry.line, entry.column);

        let mut cells: Vec<&str> = Vec::with_capacity(columns.len());
        if workspace_scope {
            cells.push(&path_text);
        }
        cells.extend([
            position.as_str(),
            severity_glyph(entry.severity),
            entry.message.as_str(),
        ]);

        table::paint_row(
            buf,
            Rect::new(table_x, row, table_width, 1),
            &cells,
            &widths,
            // Only the workspace list has a path column, and it reads dimmer
            // than the diagnostic itself. A selected row keeps one style
            // throughout, since the highlight is what carries it.
            |column| match column {
                0 if workspace_scope && !is_selected => muted_style,
                _ => base_style,
            },
        );

        // The haystack joins the location to the message, so its offsets index
        // that joined string rather than any one column. Only offsets landing
        // in the message are painted, which is the part a query usually names.
        let indices = picker
            .picker
            .row_indices(row_idx, &mut derived, &mut matching);
        if indices.is_empty() {
            continue;
        }
        let message_x = table_x + table::column_starts(&widths)[columns.len() - 1];
        let prefix_len = crate::diagnostics_picker::haystack_prefix_len(entry);
        let end_x = area.x + area.width;
        for &index in indices {
            let Some(col) = index.checked_sub(prefix_len) else {
                continue;
            };
            let x = message_x + col as u16;
            if x < end_x {
                buf[(x, row)].set_style(match_style);
            }
        }
    }
}

fn severity_glyph(severity: Option<DiagnosticSeverity>) -> &'static str {
    match severity {
        Some(DiagnosticSeverity::ERROR) => "E",
        Some(DiagnosticSeverity::WARNING) => "W",
        Some(DiagnosticSeverity::INFORMATION) => "I",
        Some(DiagnosticSeverity::HINT) => "H",
        _ => " ",
    }
}
