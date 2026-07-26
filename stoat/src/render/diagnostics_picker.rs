use crate::{
    diagnostics_picker::{DiagnosticsPicker, PickerScope},
    render::text::write_str,
};
use lsp_types::DiagnosticSeverity;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Clear, Widget},
};
use std::path::Path;

/// Rows of diagnostics the modal shows at once. A longer list scrolls under the
/// selection rather than growing the box past a readable height.
const MAX_ENTRY_ROWS: u16 = 12;

/// Lay the diagnostics picker's modal out within `area`, returning its outer
/// box and the inner rect holding the diagnostic rows, or [`None`] when `area`
/// is too small to host it or there is nothing to list.
fn diagnostics_picker_layout(area: Rect, entries_len: usize) -> Option<(Rect, Rect)> {
    if entries_len == 0 {
        return None;
    }
    let entry_rows = (entries_len as u16).min(MAX_ENTRY_ROWS);
    let modal = crate::render::chrome::modal_box(area, (0, 2 + entry_rows), (80, 3), (50, 3), 0)?;
    Some((modal, Block::default().borders(Borders::ALL).inner(modal)))
}

pub(crate) fn render_diagnostics_picker(
    picker: &mut DiagnosticsPicker,
    git_root: &Path,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let Some((modal_area, inner)) = diagnostics_picker_layout(area, picker.entries().len()) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    let title = match picker.scope() {
        PickerScope::Local => " diagnostics ",
        PickerScope::Workspace => " diagnostics (workspace) ",
    };
    Clear.render(modal_area, buf);
    crate::render::chrome::modal_frame(buf, modal_area, Some(title), modal_style, theme, scene);

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let muted_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let workspace_scope = picker.scope() == PickerScope::Workspace;
    let path_w: u16 = if workspace_scope { 28 } else { 0 };
    let pos_w: u16 = 12;
    let sev_w: u16 = 2;

    let path_x = inner.x + 1;
    let pos_x = if workspace_scope {
        path_x + path_w + 1
    } else {
        inner.x + 1
    };
    let sev_x = pos_x + pos_w + 1;
    let msg_x = sev_x + sev_w + 1;
    let msg_w = inner.width.saturating_sub(msg_x - inner.x);

    let rows = inner.height as usize;
    picker.viewport_rows = Some(rows);
    let selected = picker.selected();
    let start = crate::render::picker::window_start(selected, rows);
    for (i, entry) in picker.entries().iter().enumerate().skip(start).take(rows) {
        let row = inner.y + (i - start) as u16;
        let is_selected = i == selected;
        let base_style = if is_selected {
            selected_style
        } else {
            row_style
        };
        for col in inner.x..inner.x + inner.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
        }

        if workspace_scope {
            let path_text = entry
                .path
                .as_deref()
                .map(|p| display_path(p, git_root, path_w as usize))
                .unwrap_or_default();
            let path_style = if is_selected { base_style } else { muted_style };
            write_str(buf, path_x, row, &path_text, path_style);
        }

        let pos = format!("{:>4}:{:<3}", entry.line, entry.column);
        let pos: String = pos.chars().take(pos_w as usize).collect();
        write_str(buf, pos_x, row, &pos, base_style);

        let sev_glyph = severity_glyph(entry.severity);
        write_str(buf, sev_x, row, sev_glyph, base_style);

        let msg: String = entry.message.chars().take(msg_w as usize).collect();
        write_str(buf, msg_x, row, &msg, base_style);
    }
}

/// Render `path` relative to `git_root` when possible, falling
/// back to the absolute path. Truncates from the left so the
/// basename stays visible when the result exceeds `max_chars`,
/// using a leading ellipsis to mark the truncation.
fn display_path(path: &Path, git_root: &Path, max_chars: usize) -> String {
    let relative = path
        .strip_prefix(git_root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned();
    if relative.chars().count() <= max_chars {
        return relative;
    }
    let ellipsis = "...";
    let keep = max_chars.saturating_sub(ellipsis.chars().count());
    let tail: String = relative
        .chars()
        .rev()
        .take(keep)
        .collect::<Vec<char>>()
        .into_iter()
        .rev()
        .collect();
    format!("{ellipsis}{tail}")
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

#[cfg(test)]
mod tests {
    use super::{diagnostics_picker_layout, render_diagnostics_picker, MAX_ENTRY_ROWS};
    use crate::{
        buffer::{BufferId, TextBuffer},
        diagnostics_picker::DiagnosticsPicker,
        host::OffsetEncoding,
        render::picker::test_support::{row_text, selected_rows, selection_theme},
    };
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::path::Path;

    /// A picker over `count` single-line diagnostics, one per line, each
    /// carrying its own index in the message so a painted row is identifiable.
    fn picker_over(count: u32) -> DiagnosticsPicker {
        let text: String = (0..count).map(|i| format!("line {i}\n")).collect();
        let buffer = TextBuffer::with_text(BufferId::new(1), &text);
        let diagnostics: Vec<(OffsetEncoding, Diagnostic)> = (0..count)
            .map(|line| {
                let position = Position { line, character: 0 };
                let diagnostic = Diagnostic {
                    range: Range {
                        start: position,
                        end: position,
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    message: format!("diagnostic {line}"),
                    ..Diagnostic::default()
                };
                (OffsetEncoding::Utf16, diagnostic)
            })
            .collect();
        DiagnosticsPicker::new(&diagnostics, &buffer)
    }

    fn render(picker: &mut DiagnosticsPicker, buf: &mut Buffer, area: Rect) {
        render_diagnostics_picker(
            picker,
            Path::new("/r"),
            &selection_theme(),
            area,
            buf,
            &mut stoatty_widgets::ApcScene::new(),
        );
    }

    #[test]
    fn layout_holds_one_row_per_diagnostic() {
        let (modal, inner) = diagnostics_picker_layout(Rect::new(0, 0, 100, 40), 3)
            .expect("the area hosts the modal");
        assert_eq!(modal.width, 80, "the box holds at its recommended width");
        assert_eq!(inner.height, 3, "one row per diagnostic");
    }

    #[test]
    fn layout_caps_the_rows_it_shows() {
        let (_, inner) = diagnostics_picker_layout(Rect::new(0, 0, 100, 40), 30)
            .expect("the area hosts the modal");
        assert_eq!(inner.height, MAX_ENTRY_ROWS);
    }

    #[test]
    fn layout_none_when_too_small_or_empty() {
        assert_eq!(diagnostics_picker_layout(Rect::new(0, 0, 40, 24), 3), None);
        assert_eq!(diagnostics_picker_layout(Rect::new(0, 0, 80, 4), 3), None);
        assert_eq!(diagnostics_picker_layout(Rect::new(0, 0, 80, 24), 0), None);
    }

    #[test]
    fn paging_moves_by_half_the_rendered_rows_and_stops_at_the_ends() {
        let mut picker = picker_over(20);
        let area = Rect::new(0, 0, 100, 30);
        render(&mut picker, &mut Buffer::empty(area), area);

        let half = picker.viewport_rows.expect("the render stamped a viewport") / 2;
        assert!(
            half > 1,
            "a meaningful page needs more than one row: {half}"
        );

        picker.page(1);
        assert_eq!(picker.selected(), half, "a page down covers half a screen");
        picker.page(-1);
        assert_eq!(picker.selected(), 0, "and a page up returns");

        for _ in 0..20 {
            picker.page(1);
        }
        assert_eq!(picker.selected(), 19, "paging past the end stops on it");
        for _ in 0..20 {
            picker.page(-1);
        }
        assert_eq!(picker.selected(), 0, "and past the start stops there");
    }

    #[test]
    fn the_last_of_more_diagnostics_than_fit_paints_as_selected() {
        let mut picker = picker_over(20);
        for _ in 1..picker.entries().len() {
            picker.select_next();
        }
        assert_eq!(picker.selected(), 19, "the last entry is selected");

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut buf, area);

        let rows = selected_rows(&buf);
        assert_eq!(rows.len(), 1, "the selection is on screen exactly once");
        let text = row_text(&buf, rows[0]);
        assert!(
            text.contains("diagnostic 19"),
            "and it is the selected entry that paints there: {text:?}"
        );
    }
}
