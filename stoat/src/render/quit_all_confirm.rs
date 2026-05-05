use crate::{quit_all_confirm::QuitAllConfirm, render::text::write_str};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Widget},
};

pub(crate) fn render_quit_all_confirm(
    modal: &QuitAllConfirm,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
) {
    if area.width < 50 || area.height < 6 {
        return;
    }

    let entries = modal.entries();
    let max_entries = 10u16;
    let entry_rows = (entries.len() as u16).min(max_entries);

    let box_width = 70u16.min(area.width.saturating_sub(4));
    if box_width < 50 {
        return;
    }
    let box_height = 4 + entry_rows;
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
        .title(" unsaved buffers ")
        .title_style(modal_style);
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);

    let prompt_row = inner.y;
    let prompt = if entries.len() == 1 {
        "1 buffer has unsaved changes:".to_string()
    } else {
        format!("{} buffers have unsaved changes:", entries.len())
    };
    write_str(buf, inner.x, prompt_row, &prompt, prompt_style);

    let entries_top = inner.y + 2;
    let max_display = (inner.width as usize).saturating_sub(3);
    for (i, entry) in entries.iter().take(max_entries as usize).enumerate() {
        let row = entries_top + i as u16;
        let trimmed: String = entry.display.chars().take(max_display).collect();
        write_str(buf, inner.x, row, " * ", row_style);
        write_str(buf, inner.x + 3, row, &trimmed, row_style);
    }
}
