use crate::{diagnostics_picker::DiagnosticsPicker, render::text::write_str};
use lsp_types::DiagnosticSeverity;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Widget},
};

pub(crate) fn render_diagnostics_picker(
    picker: &DiagnosticsPicker,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 50 || area.height < 6 {
        return;
    }

    let entries = picker.entries();
    if entries.is_empty() {
        return;
    }
    let max_entries = 12u16;
    let entry_rows = (entries.len() as u16).min(max_entries);

    let box_width = 80u16.min(area.width.saturating_sub(4));
    if box_width < 50 {
        return;
    }
    let box_height = 2 + entry_rows;
    if box_height > area.height {
        return;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal_area = Rect::new(x, y, box_width, box_height);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(modal_style)
        .title(" diagnostics ")
        .title_style(modal_style);
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);

    let pos_w = 12u16;
    let sev_w = 2u16;
    let pos_x = inner.x + 1;
    let sev_x = pos_x + pos_w + 1;
    let msg_x = sev_x + sev_w + 1;
    let msg_w = inner.width.saturating_sub(msg_x - inner.x);

    for (i, entry) in entries.iter().take(max_entries as usize).enumerate() {
        let row = inner.y + i as u16;
        let is_selected = i == picker.selected();
        let base_style = if is_selected {
            selected_style
        } else {
            row_style
        };
        for col in inner.x..inner.x + inner.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
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

fn severity_glyph(severity: Option<DiagnosticSeverity>) -> &'static str {
    match severity {
        Some(DiagnosticSeverity::ERROR) => "E",
        Some(DiagnosticSeverity::WARNING) => "W",
        Some(DiagnosticSeverity::INFORMATION) => "I",
        Some(DiagnosticSeverity::HINT) => "H",
        _ => " ",
    }
}
