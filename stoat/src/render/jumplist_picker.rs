use crate::{
    jumplist_picker::JumplistPicker,
    render::table::{self, Column, Width},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    widgets::{Block, Borders, Clear, Widget},
};

/// Rows of jumps the modal shows at once. A longer list scrolls under the
/// selection rather than growing the box past a readable height.
const MAX_ENTRY_ROWS: u16 = 12;

/// A one-column marker for the walk cursor, then the file, the position, and
/// the line's text. Headerless, since the marker column has nothing to label
/// and the rest are obvious.
const COLUMNS: [Column; 4] = [
    Column {
        label: "",
        width: Width::Fixed(1),
    },
    Column {
        label: "file",
        width: Width::Fixed(18),
    },
    Column {
        label: "position",
        width: Width::Fixed(9),
    },
    Column {
        label: "snippet",
        width: Width::Fill,
    },
];

/// Lay the jumplist picker's modal out within `area`, returning its outer box
/// and the inner rect holding the jump rows, or [`None`] when `area` is too
/// small to host it or there is nothing to list.
fn jumplist_picker_layout(area: Rect, entries_len: usize) -> Option<(Rect, Rect)> {
    if entries_len == 0 {
        return None;
    }
    let entry_rows = (entries_len as u16).min(MAX_ENTRY_ROWS);
    let modal = crate::render::chrome::modal_box(area, (0, 2 + entry_rows), (80, 3), (50, 3), 0)?;
    Some((modal, Block::default().borders(Borders::ALL).inner(modal)))
}

pub(crate) fn render_jumplist_picker(
    picker: &mut JumplistPicker,
    theme: &crate::theme::Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut stoatty_widgets::ApcScene,
) {
    let Some((modal_area, inner)) = jumplist_picker_layout(area, picker.entries().len()) else {
        return;
    };

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_PICKER);
    Clear.render(modal_area, buf);
    crate::render::chrome::modal_frame(
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

    // The marker column is flush against the border, so its own gap supplies
    // the pad the other pickers put in front of their table.
    let widths = table::resolve_widths(&COLUMNS, &[], inner.width);

    let rows = inner.height as usize;
    picker.viewport_rows = Some(rows);
    let start = crate::render::picker::window_start(selected, rows);
    for (i, entry) in picker.entries().iter().enumerate().skip(start).take(rows) {
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
        let position = format!("{:>4}:{:<3}", entry.line, entry.column);
        let cells = [
            marker,
            entry.filename.as_str(),
            position.as_str(),
            entry.snippet.as_str(),
        ];

        // The whole row already carries the style that says which entry the
        // walk cursor is on, so no cell needs one of its own.
        table::paint_row(
            buf,
            Rect::new(inner.x, row, inner.width, 1),
            &cells,
            &widths,
            |_| base_style,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{jumplist_picker_layout, render_jumplist_picker, MAX_ENTRY_ROWS};
    use crate::{
        buffer_registry::BufferRegistry,
        jumplist::{JumpEntry, JumpList},
        jumplist_picker::JumplistPicker,
        render::picker::test_support::{row_text, selected_rows, selection_theme},
    };
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::path::Path;
    use stoat_text::{Bias, Selection, SelectionGoal};

    /// A picker over `count` jumps into one buffer, each on its own line, whose
    /// snippet carries its index so a painted row is identifiable.
    fn picker_over(count: usize) -> JumplistPicker {
        // Every line is the same width, so entry `i` starts at `i * LINE_LEN`.
        const LINE_LEN: usize = "entry 00\n".len();
        let mut buffers = BufferRegistry::new();
        let text: String = (0..count).map(|i| format!("entry {i:02}\n")).collect();
        let (buffer_id, _) = buffers.open(Path::new("/dir/file.rs"), &text);

        let mut jumplist = JumpList::default();
        for i in 0..count {
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
        JumplistPicker::new(&jumplist, &buffers)
    }

    fn render(picker: &mut JumplistPicker, buf: &mut Buffer, area: Rect) {
        render_jumplist_picker(
            picker,
            &selection_theme(),
            area,
            buf,
            &mut stoatty_widgets::ApcScene::new(),
        );
    }

    #[test]
    fn layout_holds_one_row_per_jump() {
        let (modal, inner) =
            jumplist_picker_layout(Rect::new(0, 0, 100, 40), 3).expect("the area hosts the modal");
        assert_eq!(modal.width, 80, "the box holds at its recommended width");
        assert_eq!(inner.height, 3, "one row per jump");
    }

    #[test]
    fn layout_caps_the_rows_it_shows() {
        let (_, inner) =
            jumplist_picker_layout(Rect::new(0, 0, 100, 40), 30).expect("the area hosts the modal");
        assert_eq!(inner.height, MAX_ENTRY_ROWS);
    }

    /// An area too short for the whole list gives back a shorter box rather
    /// than none, where the old hand-rolled sizing refused outright. The rows
    /// have to follow the box down, or the surplus paints through the bottom
    /// border and onto the editor behind.
    #[test]
    fn a_box_shortened_to_fit_paints_nothing_outside_itself() {
        let mut picker = picker_over(12);
        let area = Rect::new(0, 0, 100, 12);
        let (modal, inner) =
            jumplist_picker_layout(area, 12).expect("a short area still hosts the modal");
        assert!(
            inner.height < 12,
            "the area forces a box shorter than the list: {inner:?}"
        );

        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut buf, area);

        let pristine = Buffer::empty(area);
        let outside: Vec<(u16, u16)> = (area.y..area.y + area.height)
            .flat_map(|y| (area.x..area.x + area.width).map(move |x| (x, y)))
            .filter(|&(x, y)| !modal.contains((x, y).into()))
            .filter(|&(x, y)| buf[(x, y)] != pristine[(x, y)])
            .collect();
        assert_eq!(outside, Vec::new(), "every painted cell is inside the box");
    }

    #[test]
    fn layout_none_when_too_small_or_empty() {
        assert_eq!(jumplist_picker_layout(Rect::new(0, 0, 40, 24), 3), None);
        assert_eq!(jumplist_picker_layout(Rect::new(0, 0, 80, 4), 3), None);
        assert_eq!(jumplist_picker_layout(Rect::new(0, 0, 80, 24), 0), None);
    }

    #[test]
    fn paging_moves_by_half_the_rendered_rows_and_stops_at_the_ends() {
        let mut picker = picker_over(20);
        let area = Rect::new(0, 0, 100, 30);
        render(&mut picker, &mut Buffer::empty(area), area);

        let half = picker.viewport_rows.expect("the render stamped a viewport") / 2;
        assert!(
            half > 1,
            "a meaningful page needs more than one row: {half}"
        );

        // The picker opens on the walk cursor, so paging starts from a known row.
        while picker.selected() > 0 {
            picker.select_prev();
        }
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
    fn the_last_of_more_jumps_than_fit_paints_as_selected() {
        let mut picker = picker_over(20);
        while picker.selected() + 1 < picker.entries().len() {
            picker.select_next();
        }
        assert_eq!(picker.selected(), 19, "the last entry is selected");

        let area = Rect::new(0, 0, 100, 30);
        let mut buf = Buffer::empty(area);
        render(&mut picker, &mut buf, area);

        let rows = selected_rows(&buf);
        assert_eq!(rows.len(), 1, "the selection is on screen exactly once");
        let text = row_text(&buf, rows[0]);
        assert!(
            text.contains("entry 19"),
            "and it is the selected entry that paints there: {text:?}"
        );
    }
}
