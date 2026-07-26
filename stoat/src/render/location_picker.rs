use crate::{
    location_picker::LocationPicker,
    render::table::{self, Column, Width},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Clear, Widget},
};
use std::path::Path;

/// Rows of candidates the modal shows at once. A longer candidate list scrolls
/// under the selection rather than growing the box past a readable height.
const MAX_ENTRY_ROWS: u16 = 12;

/// The candidate's file, its line and column, and the line's text. Headerless,
/// since three columns of obvious content need no labels over them.
const COLUMNS: [Column; 3] = [
    Column {
        label: "path",
        width: Width::Fixed(32),
    },
    Column {
        label: "position",
        width: Width::Fixed(12),
    },
    Column {
        label: "text",
        width: Width::Fill,
    },
];

/// Lay the location picker's modal out within `area`, returning its outer box
/// and the inner rect holding the candidate rows, or [`None`] when `area` is
/// too small to host it or there is nothing to list.
///
/// Painting and hit-testing both go through this, so a clicked row cannot
/// disagree with the row drawn there.
pub(crate) fn location_picker_layout(area: Rect, entries_len: usize) -> Option<(Rect, Rect)> {
    if entries_len == 0 {
        return None;
    }
    let entry_rows = (entries_len as u16).min(MAX_ENTRY_ROWS);
    let modal = crate::render::chrome::modal_box(area, (0, 2 + entry_rows), (80, 3), (50, 3), 0)?;
    Some((modal, Block::default().borders(Borders::ALL).inner(modal)))
}

pub(crate) fn render_location_picker(
    picker: &mut LocationPicker,
    git_root: &Path,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let Some((modal_area, inner)) = location_picker_layout(area, picker.entries().len()) else {
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

    // The table sits a column in from the border, and the path column carries
    // the whole width it asks for so a long path truncates rather than pushing
    // the position column around.
    let table_x = inner.x + 1;
    let table_width = inner.width.saturating_sub(1);
    let widths = table::resolve_widths(&COLUMNS, &[], table_width);

    let rows = inner.height as usize;
    picker.viewport_rows = Some(rows);
    let selected = picker.selected();
    let start = crate::render::picker::window_start(selected, rows);

    for (i, entry) in picker.entries().iter().skip(start).take(rows).enumerate() {
        let row = inner.y + i as u16;
        let is_selected = start + i == selected;
        let base_style = if is_selected {
            selected_style
        } else {
            row_style
        };
        for col in inner.x..inner.x + inner.width {
            buf[(col, row)].set_char(' ').set_style(base_style);
        }

        let path_text = table::display_path(&entry.path, git_root, widths[0] as usize);
        let position = format!("{:>4}:{:<3}", entry.line, entry.column);
        let cells = [path_text.as_str(), position.as_str(), entry.text.as_str()];

        table::paint_row(
            buf,
            Rect::new(table_x, row, table_width, 1),
            &cells,
            &widths,
            // The path is context rather than the candidate itself, so it reads
            // dimmer. A selected row keeps one style throughout, since the
            // highlight is what carries it.
            |column| match column {
                0 if !is_selected => muted_style,
                _ => base_style,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{location_picker_layout, render_location_picker, MAX_ENTRY_ROWS};
    use crate::{
        location_picker::{LocationEntry, LocationPicker},
        theme::Theme,
    };
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::path::{Path, PathBuf};

    #[test]
    fn paging_moves_by_half_the_rendered_rows_and_stops_at_the_ends() {
        let entries = (0..20)
            .map(|i| LocationEntry {
                path: PathBuf::from(format!("/r/file{i:02}.rs")),
                offset: 0,
                line: i + 1,
                column: 1,
                text: format!("candidate {i}"),
            })
            .collect();
        let mut picker = LocationPicker::new(entries);

        let area = Rect::new(0, 0, 100, 30);
        render_location_picker(
            &mut picker,
            Path::new("/r"),
            &Theme::empty(),
            area,
            &mut Buffer::empty(area),
            &mut stoatty_widgets::ApcScene::new(),
        );

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
}
