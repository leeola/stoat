//! Shared painters for a [`crate::picker::PathPicker`]'s result list and
//! preview.
//!
//! The standalone file finder and the palette's inline argument picker paint
//! the same list of repo-relative rows beside the same preview pane. Both drive
//! a `PathPicker`, so both render through these functions and cannot drift.

use crate::{
    file_finder::display_row,
    picker::{PickList, Preview},
    render::{editor::render_editor, text::write_str_clipped},
    theme::{scope, Theme},
    workspace::Workspace,
};
use ratatui::{buffer::Buffer, layout::Rect};
use std::path::Path;

/// Paint a picker's result rows into `area`, one repo-relative path per row,
/// with the selected row and fuzzy-match characters highlighted.
///
/// `start_row` is the first filtered index to show, so the live list can derive
/// it from the selection while the smooth-scroll pool paints absolute pages, and
/// both render identical rows. Rows are read from `picklist.base`, which every
/// caller keeps in sync with its display set on refilter.
pub(crate) fn paint_path_rows(
    picklist: &PickList,
    git_root: &Path,
    prefix: &str,
    area: Rect,
    start_row: usize,
    theme: &Theme,
    buf: &mut Buffer,
) {
    let rows = area.height as usize;
    if rows == 0 {
        return;
    }
    let row_style = theme.get(scope::UI_TEXT);
    let selected_style = theme.get(scope::UI_SELECTION);
    let match_style = theme.get(scope::UI_SEARCH_MATCH);

    let end_x = area.x + area.width;
    let label_x = area.x + 1;
    let prefix_len = prefix.chars().count() as u32;

    for (row_idx, (&idx, indices)) in picklist
        .filtered
        .iter()
        .zip(picklist.match_indices.iter())
        .skip(start_row)
        .take(rows)
        .enumerate()
    {
        let row = area.y + row_idx as u16;
        let is_selected = start_row + row_idx == picklist.selected;
        let style = if is_selected {
            selected_style
        } else {
            row_style
        };
        for col in area.x..end_x {
            buf[(col, row)].set_char(' ').set_style(style);
        }
        let label = format!("{prefix}{}", display_row(&picklist.base[idx], git_root));
        write_str_clipped(buf, label_x, row, &label, style, end_x);
        for (label_col, _) in label.chars().enumerate() {
            let col = label_x + label_col as u16;
            if col >= end_x {
                break;
            }
            let label_col = label_col as u32;
            // The literal `prefix` is never matched. The picklist's indices are
            // offsets into the part after it, so shift them past the prefix.
            if label_col >= prefix_len && indices.binary_search(&(label_col - prefix_len)).is_ok() {
                buf[(col, row)].set_style(match_style);
            }
        }
    }
}

/// Paint the picker's preview pane by rendering its scratch editor into `area`.
/// A no-op for an empty rect.
pub(crate) fn render_picker_preview(
    preview: &Preview,
    area: Rect,
    theme: &Theme,
    ws: &mut Workspace,
    buf: &mut Buffer,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let fallback = theme.get(scope::UI_TEXT);
    if let Some(editor) = ws.editors.get_mut(preview.editor) {
        render_editor(editor, area, fallback, theme, buf, false);
    }
}

/// Split a picker body rect into a result list and an optional preview pane.
///
/// The preview appears only when `width >= wide_threshold`, where the list
/// takes 40% (floored at `min_list`) and the preview the rest past a one-cell
/// separator. Below the threshold the list takes the full width and there is no
/// preview.
pub(crate) fn split_list_preview(
    x: u16,
    y: u16,
    width: u16,
    height: u16,
    wide_threshold: u16,
    min_list: u16,
) -> (Rect, Option<Rect>) {
    if width >= wide_threshold {
        let list_width = (width * 40 / 100).max(min_list);
        let preview_width = width.saturating_sub(list_width + 1);
        (
            Rect::new(x, y, list_width, height),
            Some(Rect::new(x + list_width + 1, y, preview_width, height)),
        )
    } else {
        (Rect::new(x, y, width, height), None)
    }
}
