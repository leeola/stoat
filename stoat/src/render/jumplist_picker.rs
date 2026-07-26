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

    let rows = max_entries as usize;
    let start = crate::render::picker::window_start(selected, rows);
    for (i, entry) in entries.iter().enumerate().skip(start).take(rows) {
        let row = inner.y + (i - start) as u16;
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

#[cfg(test)]
mod tests {
    use super::render_jumplist_picker;
    use crate::{
        buffer_registry::BufferRegistry,
        jumplist::{JumpEntry, JumpList},
        jumplist_picker::JumplistPicker,
        render::picker::test_support::{row_text, selected_rows, selection_theme},
    };
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::path::Path;
    use stoat_text::{Bias, Selection, SelectionGoal};

    #[test]
    fn the_last_of_more_jumps_than_fit_paints_as_selected() {
        let mut buffers = BufferRegistry::new();
        // Every line is the same width, so entry `i` starts at `i * LINE_LEN`.
        const LINE_LEN: usize = "entry 00\n".len();
        let text: String = (0..20).map(|i| format!("entry {i:02}\n")).collect();
        let (buffer_id, _) = buffers.open(Path::new("/dir/file.rs"), &text);

        let mut jumplist = JumpList::default();
        for i in 0..20 {
            let anchor = {
                let buffer = buffers.get(buffer_id).expect("buffer open");
                let guard = buffer.read().expect("buffer readable");
                guard.anchor_at(i * LINE_LEN, Bias::Right)
            };
            let entry = JumpEntry {
                buffer_id,
                selections: vec![Selection {
                    id: 0,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
            };
            jumplist.push(entry, &buffers);
        }

        let mut picker = JumplistPicker::new(&jumplist, &buffers);
        while picker.selected() + 1 < picker.entries().len() {
            picker.select_next();
        }
        assert_eq!(picker.selected(), 19, "the last entry is selected");

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render_jumplist_picker(
            &picker,
            &selection_theme(),
            area,
            &mut buf,
            &mut stoatty_widgets::ApcScene::new(),
        );

        let rows = selected_rows(&buf);
        assert_eq!(rows.len(), 1, "the selection is on screen exactly once");
        let text = row_text(&buf, rows[0]);
        assert!(
            text.contains("entry 19"),
            "and it is the selected entry that paints there: {text:?}"
        );
    }
}
