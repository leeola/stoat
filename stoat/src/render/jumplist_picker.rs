use crate::{jumplist_picker::JumplistPicker, render::text::write_str};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Clear, Widget},
};

pub(crate) fn render_jumplist_picker(
    picker: &JumplistPicker,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
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
    Clear.render(modal_area, buf);
    let inner = crate::render::chrome::modal_frame(
        buf,
        modal_area,
        Some(" jumplist "),
        modal_style,
        theme,
        scene,
    );

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let prompt_style = theme.get(crate::theme::scope::UI_PROMPT);

    let cursor_idx = picker.cursor_idx();
    let selected = picker.selected();

    let filename_w = 18u16;
    let position_w = 9u16;
    let marker_x = inner.x;
    let name_x = marker_x + 2;
    let pos_x = name_x + filename_w + 1;
    let snippet_x = pos_x + position_w + 1;
    let snippet_w = inner.width.saturating_sub(snippet_x - inner.x);

    for (i, entry) in entries.iter().take(max_entries as usize).enumerate() {
        let row = inner.y + i as u16;
        let is_selected = i == selected;
        let is_current = i == cursor_idx;
        let base_style = if is_selected {
            selected_style
        } else if is_current {
            prompt_style
        } else {
            row_style
        };

        for col in inner.x..inner.x + inner.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
        }

        let marker = if is_current { ">" } else { " " };
        write_str(buf, marker_x, row, marker, base_style);
        let name: String = entry.filename.chars().take(filename_w as usize).collect();
        write_str(buf, name_x, row, &name, base_style);
        let pos = format!("{:>4}:{:<3}", entry.line, entry.column);
        let pos: String = pos.chars().take(position_w as usize).collect();
        write_str(buf, pos_x, row, &pos, base_style);
        let snippet: String = entry.snippet.chars().take(snippet_w as usize).collect();
        write_str(buf, snippet_x, row, &snippet, base_style);
    }
}
