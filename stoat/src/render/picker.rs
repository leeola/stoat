//! Shared painters for a [`crate::picker::PathPicker`]'s result list and
//! preview.
//!
//! The standalone file finder and the palette's inline argument picker paint
//! the same list of repo-relative rows beside the same preview pane. Both drive
//! a `PathPicker`, so both render through these functions and cannot drift.

use crate::{
    picker::{row_display, PickList, Preview},
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
    let home = crate::paths::home_dir();
    let home = home.as_deref();

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
        let label = format!(
            "{prefix}{}",
            row_display(
                &picklist.base[idx],
                git_root,
                picklist.display_roots.as_deref(),
                home
            )
        );
        let width = end_x.saturating_sub(label_x) as usize;
        let label_len = label.chars().count();

        // A row wider than its column is start-truncated helix-style so the file
        // name at the tail stays on screen. An ellipsis replaces the `dropped`
        // leading chars in one cell, leaving `width - 1` cells for the tail.
        let (dropped, text_x) = if label_len > width && width > 1 {
            let dropped = label_len - (width - 1);
            buf[(label_x, row)].set_char('\u{2026}').set_style(style);
            let tail_start = label
                .char_indices()
                .nth(dropped)
                .map_or(label.len(), |(byte, _)| byte);
            write_str_clipped(buf, label_x + 1, row, &label[tail_start..], style, end_x);
            (dropped, label_x + 1)
        } else {
            write_str_clipped(buf, label_x, row, &label, style, end_x);
            (0, label_x)
        };

        for (label_col, _) in label.chars().enumerate().skip(dropped) {
            let col = text_x + (label_col - dropped) as u16;
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Color;
    use std::path::PathBuf;

    fn row_text(buf: &Buffer, row: u16, area: Rect) -> String {
        (area.x..area.x + area.width)
            .map(|c| buf[(c, row)].symbol().chars().next().unwrap_or(' '))
            .collect()
    }

    /// A picklist over `base` with every row filtered in and no row selected, so
    /// the painter uses the plain row style.
    fn list_of(base: Vec<PathBuf>, match_indices: Vec<Vec<u32>>) -> PickList {
        PickList {
            filtered: (0..base.len()).collect(),
            base: base.into(),
            match_indices,
            selected: usize::MAX,
            ..PickList::default()
        }
    }

    fn match_theme() -> Theme {
        let src = r##"theme t { ui.search.match.fg = "#ff0000"; }"##;
        let (config, _) = stoat_config::parse(src);
        Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads")
    }

    #[test]
    fn long_path_start_truncates_keeping_the_file_name() {
        let git_root = Path::new("/r");
        let list = list_of(
            vec![PathBuf::from(
                "/r/very/deeply/nested/dir/module/file_name.rs",
            )],
            vec![vec![]],
        );
        // Column width 20 leaves a 19-cell label area after the one-cell pad.
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(&list, git_root, "", area, 0, &Theme::empty(), &mut buf);

        let text = row_text(&buf, 0, area);
        assert!(
            text.trim_start().starts_with('\u{2026}'),
            "an ellipsis stands in for the dropped head: {text:?}"
        );
        assert!(
            text.ends_with("file_name.rs"),
            "the file name at the tail stays visible: {text:?}"
        );
    }

    #[test]
    fn short_path_renders_in_full_without_an_ellipsis() {
        let git_root = Path::new("/r");
        let list = list_of(vec![PathBuf::from("/r/a.rs")], vec![vec![]]);
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(&list, git_root, "", area, 0, &Theme::empty(), &mut buf);

        let text = row_text(&buf, 0, area);
        assert!(
            !text.contains('\u{2026}'),
            "no ellipsis for a path that fits: {text:?}"
        );
        assert_eq!(text.trim(), "a.rs", "the whole path renders: {text:?}");
    }

    #[test]
    fn match_highlight_follows_the_truncated_tail() {
        let git_root = Path::new("/r");
        // The 42-char label loses its leading 24 chars in a 19-cell area, so the
        // tail begins at char 24. Match offset 0 lands in the dropped head and
        // offset 30 (the `f` of `file_name`) in the visible tail.
        let list = list_of(
            vec![PathBuf::from(
                "/r/very/deeply/nested/dir/module/file_name.rs",
            )],
            vec![vec![0, 30]],
        );
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        paint_path_rows(&list, git_root, "", area, 0, &match_theme(), &mut buf);

        let match_fg = Color::Rgb(255, 0, 0);
        let highlighted: Vec<u16> = (area.x..area.x + area.width)
            .filter(|&c| buf[(c, 0)].fg == match_fg)
            .collect();
        assert_eq!(
            highlighted,
            vec![8],
            "only the tail match highlights at its shifted column; the dropped-head match paints nothing"
        );
    }
}
