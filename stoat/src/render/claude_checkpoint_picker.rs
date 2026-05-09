use crate::{claude_checkpoint_picker::CheckpointPicker, render::text::write_str};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Widget},
};

pub(crate) fn render_claude_checkpoint_picker(
    picker: &CheckpointPicker,
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
        .title(" claude restore ")
        .title_style(modal_style);
    let inner = block.inner(modal_area);
    block.render(modal_area, buf);

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);

    let selected = picker.selected();
    let label_x = inner.x + 2;
    let marker_x = inner.x;
    let label_w = inner.width.saturating_sub(label_x - inner.x);

    for (i, entry) in entries.iter().take(max_entries as usize).enumerate() {
        let row = inner.y + i as u16;
        let is_selected = i == selected;
        let base_style = if is_selected {
            selected_style
        } else {
            row_style
        };

        for col in inner.x..inner.x + inner.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
        }

        let marker = if is_selected { ">" } else { " " };
        write_str(buf, marker_x, row, marker, base_style);
        let label: String = entry.label.chars().take(label_w as usize).collect();
        write_str(buf, label_x, row, &label, base_style);
    }
}
