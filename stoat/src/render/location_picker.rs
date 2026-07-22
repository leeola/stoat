use crate::{location_picker::LocationPicker, render::text::write_str};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Clear, Widget},
};
use std::path::Path;

/// Rows of candidates the modal shows at once. A longer candidate list scrolls
/// under the selection rather than growing the box past a readable height.
const MAX_ENTRY_ROWS: u16 = 12;

/// Lay the location picker's modal out within `area`, returning its outer box
/// and the inner rect holding the candidate rows, or [`None`] when `area` is
/// too small to host it or there is nothing to list.
///
/// Painting and hit-testing both go through this, so a clicked row cannot
/// disagree with the row drawn there.
pub(crate) fn location_picker_layout(area: Rect, entries_len: usize) -> Option<(Rect, Rect)> {
    if area.width < 50 || area.height < 6 || entries_len == 0 {
        return None;
    }
    let entry_rows = (entries_len as u16).min(MAX_ENTRY_ROWS);

    let box_width = 80u16.min(area.width.saturating_sub(4));
    if box_width < 50 {
        return None;
    }
    let box_height = 2 + entry_rows;
    if box_height > area.height {
        return None;
    }

    let x = area.x + (area.width.saturating_sub(box_width)) / 2;
    let y = area.y + (area.height.saturating_sub(box_height)) / 2;
    let modal = Rect::new(x, y, box_width, box_height);
    Some((modal, Block::default().borders(Borders::ALL).inner(modal)))
}

/// First candidate visible when `selected` is showing, given `rows` on screen.
///
/// Keeps the selection on the last visible row once it passes the window, which
/// is the same derivation the finder and palette lists use, so the picker's hit
/// test and its paint agree on which entry a row holds.
pub(crate) fn location_picker_window_start(selected: usize, rows: usize) -> usize {
    selected.saturating_sub(rows.saturating_sub(1))
}

pub(crate) fn render_location_picker(
    picker: &LocationPicker,
    git_root: &Path,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let entries = picker.entries();
    let Some((modal_area, inner)) = location_picker_layout(area, entries.len()) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    Clear.render(modal_area, buf);
    crate::render::chrome::modal_frame(
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

    let rows = inner.height as usize;
    let start = location_picker_window_start(picker.selected(), rows);

    for (i, entry) in entries.iter().skip(start).take(rows).enumerate() {
        let row = inner.y + i as u16;
        let is_selected = start + i == picker.selected();
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

#[cfg(test)]
mod tests {
    use super::{location_picker_layout, location_picker_window_start, MAX_ENTRY_ROWS};
    use ratatui::layout::Rect;

    #[test]
    fn layout_inner_holds_one_row_per_entry() {
        let (modal, inner) = location_picker_layout(Rect::new(0, 0, 80, 24), 3).expect("layout");
        assert_eq!(inner.height, 3, "one row per entry");
        assert_eq!(modal.height, 5, "plus the border rows");
        assert!(modal.width >= 50 && modal.width <= 80);
    }

    #[test]
    fn layout_caps_the_rows_it_shows() {
        let (_, inner) = location_picker_layout(Rect::new(0, 0, 100, 40), 30).expect("layout");
        assert_eq!(inner.height, MAX_ENTRY_ROWS);
    }

    #[test]
    fn layout_none_when_too_small_or_empty() {
        assert_eq!(location_picker_layout(Rect::new(0, 0, 40, 24), 3), None);
        assert_eq!(location_picker_layout(Rect::new(0, 0, 80, 4), 3), None);
        assert_eq!(location_picker_layout(Rect::new(0, 0, 80, 24), 0), None);
    }

    #[test]
    fn window_holds_the_selection_on_the_last_row() {
        assert_eq!(location_picker_window_start(3, 12), 0, "still on screen");
        assert_eq!(location_picker_window_start(14, 12), 3, "scrolled by 3");
    }
}
