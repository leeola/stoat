use crate::{location_picker::LocationPicker, render::text::write_str};
use ratatui::{buffer::Buffer, layout::Rect};
use std::path::Path;

pub(crate) fn render_location_picker(
    picker: &LocationPicker,
    git_root: &Path,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: Option<&mut stoatty_widgets::ApcScene>,
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
    let inner = crate::render::chrome::modal_frame(
        buf,
        modal_area,
        Some(" locations "),
        modal_style,
        theme,
        scene,
    );

    let row_style = theme.get(crate::theme::scope::UI_TEXT);
    let selected_style = theme.get(crate::theme::scope::UI_SELECTION);
    let muted_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let path_w: u16 = 32;
    let pos_w: u16 = 12;

    let path_x = inner.x + 1;
    let pos_x = path_x + path_w + 1;
    let text_x = pos_x + pos_w + 1;
    let text_w = inner.width.saturating_sub(text_x - inner.x);

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

        let path_text = display_path(&entry.path, git_root, path_w as usize);
        let path_style = if is_selected { base_style } else { muted_style };
        write_str(buf, path_x, row, &path_text, path_style);

        let pos = format!("{:>4}:{:<3}", entry.line, entry.column);
        let pos: String = pos.chars().take(pos_w as usize).collect();
        write_str(buf, pos_x, row, &pos, base_style);

        let text: String = entry.text.chars().take(text_w as usize).collect();
        write_str(buf, text_x, row, &text, base_style);
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
