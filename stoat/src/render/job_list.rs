//! The Job List's Overlay, the session finder's plate over the rows of a
//! [`JobList`].
//!
//! A row names the state of its Cmd, its line, its time, and where it
//! stands, so a user finds a Cmd by any of them and acts on it from here.

use crate::{
    fuzzy,
    job_list::{JobList, JobPlace, JobScope},
    jobs::{self, JobState},
    render::{
        chrome::{self, ISLAND_BOTTOM, ISLAND_SIDE, ISLAND_TOP},
        editor::ResolvedChrome,
        paint::style_rgb,
        picker::{self, FilterHeader, Footer, FILTER_PROMPT},
        text::{write_str, write_str_clipped},
        workspace_picker, TypeScale,
    },
    theme::{scope, Theme, TypeRole},
    workspace::Workspace,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style, widgets::StatefulWidget};
use stoat_widgets::{
    text_run::{self, TextRun},
    ApcScene,
};

/// The key line's hints, each a bound key and the word for what it does.
const JOB_LIST_HINTS: [(&str, &str); 6] = [
    ("Enter", "open"),
    ("C-r", "run again"),
    ("C-c", "interrupt"),
    ("C-x", "dismiss"),
    ("S-Tab", "scope"),
    ("Esc", "close"),
];

/// Cells of the place column, which `run pane` fills.
const PLACE_COLS: u16 = 8;

/// Cells of the state column.
const STATE_COLS: u16 = 16;

/// The cells of a row that the header row and each row read.
struct Columns {
    mark: u16,
    line: u16,
    /// The cell where the line clips, 2 cells before the state text.
    line_end: u16,
    state: u16,
    /// The first cell of the place column, whose text is right-aligned.
    place: u16,
}

impl Columns {
    /// The columns inside the modal's `inner` rect.
    ///
    /// The place column ends one cell before the rect's right edge, and the
    /// state column ends 2 cells left of it.
    fn of(inner: Rect) -> Self {
        let place = inner.right().saturating_sub(1 + PLACE_COLS);
        let state = place.saturating_sub(2 + STATE_COLS);
        Self {
            mark: inner.x + 1,
            line: inner.x + 3,
            line_end: state.saturating_sub(2),
            state,
            place,
        }
    }
}

/// Lay the Job List out within `area` over `rows` rows, returning its outer
/// box and the inner rect, as the session finder lays its box out.
///
/// An empty list keeps one row, which reads `no jobs`.
pub(crate) fn job_list_layout(area: Rect, rows: usize) -> Option<(Rect, Rect)> {
    workspace_picker::workspace_picker_layout(area, rows.max(1))
}

/// The rows the list paints in, inside the modal's `inner` rect.
///
/// The paint and the pointer's hit test both read it, so a press lands on the
/// row drawn under it.
pub(crate) fn job_list_rows(inner: Rect) -> Rect {
    workspace_picker::workspace_picker_list(inner)
}

/// Paint the Job List over `area`.
///
/// The box keeps the height of every row while the filter narrows them, as
/// the session finder's box does.
pub(crate) fn render_job_list(
    list: &mut JobList,
    ws: &mut Workspace,
    theme: &Theme,
    resolved: &ResolvedChrome,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let Some((modal, inner)) = job_list_layout(area, list.entries().len()) else {
        return;
    };
    let modal_style = theme.get(scope::UI_MODAL_PICKER);
    chrome::list_plate(buf, modal, modal_style, theme, &mut *scene);

    let all = list.scope() == JobScope::All;
    picker::filter_header(
        buf,
        inner,
        &FilterHeader {
            label: "Jobs",
            prompt: FILTER_PROMPT,
            count: Some((list.filtered().len(), list.entries().len())),
            chips: &[("all", all), ("running", !all)],
        },
        &list.input,
        ws,
        theme,
        resolved,
        scene,
    );

    let plate = chrome::plate_colors(theme, scene);
    let footer = picker::footer_rect(modal);
    if let Some(plate) = plate {
        let body_top = inner.y + 2;
        let body = Rect::new(
            inner.x,
            body_top,
            inner.width,
            footer.y.saturating_sub(body_top),
        );
        let edges = [ISLAND_TOP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE];
        chrome::modal_island(body, edges, plate, theme, buf, scene);
    }

    let columns = Columns::of(inner);
    let muted = theme.get(scope::UI_TEXT_MUTED);
    let header_row = inner.y + 2;
    if plate.is_some() {
        let rule = theme
            .try_get(scope::UI_DIVIDER)
            .filter(|style| style.fg.is_some())
            .unwrap_or_else(|| theme.get(scope::UI_BORDER_INACTIVE));
        chrome::hline_edge(buf, inner.x, header_row, inner.width, rule, scene);
    }
    write_str(buf, columns.line, header_row, "command", muted);
    write_str(buf, columns.state, header_row, "state", muted);
    write_str(buf, columns.place, header_row, &place_text("where"), muted);

    let rows_rect = job_list_rows(inner);
    let rows = usize::from(rows_rect.height);
    list.viewport_rows = Some(rows);
    if list.filtered().is_empty() && rows > 0 {
        let disabled = theme.get(scope::UI_TEXT_DISABLED);
        write_str_clipped(
            buf,
            columns.line,
            rows_rect.y,
            "no jobs",
            disabled,
            columns.line_end,
        );
    }

    let row_style = theme.get(scope::UI_TEXT);
    let match_style = theme.list_match();
    // Reused across the painted rows rather than allocated per row, since a
    // row past the indexed block derives its offsets here.
    let mut derived = Vec::new();
    let mut matching = fuzzy::Scratch::default();
    let selected = list.selected();
    let start = picker::window_start(selected, rows);
    for (i, &idx) in list.filtered().iter().enumerate().skip(start).take(rows) {
        let entry = &list.entries()[idx];
        let Some(cmd) = ws.cmds.get(entry.cmd) else {
            continue;
        };
        let y = rows_rect.y + (i - start) as u16;
        let rect = Rect::new(inner.x, y, inner.width, 1);
        let base = match i == selected {
            true => chrome::selected_row(rect, row_style, true, theme, buf, scene),
            false => {
                for x in rect.left()..rect.right() {
                    buf[(x, y)].set_char(' ').set_style(row_style);
                }
                row_style
            },
        };

        let state = jobs::state_of(cmd);
        let state_style = base.patch(jobs::state_style(theme, state));
        let mark = Rect::new(columns.mark, y, 1, 1);
        paint_mark(state, mark, state_style, theme, buf, scene);

        write_str_clipped(buf, columns.line, y, &entry.line, base, columns.line_end);
        let indices = list.row_indices(i, &mut derived, &mut matching);
        let shown = usize::from(columns.line_end.saturating_sub(columns.line));
        for j in (0..entry.line.chars().count().min(shown)).map(|j| j as u32) {
            if indices.binary_search(&j).is_ok() {
                buf[(columns.line + j as u16, y)].set_style(match_style);
            }
        }

        let word = state.word();
        let detail = match (state, cmd.progress()) {
            (JobState::Running, Some((done, total))) => format!("{done}/{total}"),
            _ => jobs::time_text(cmd),
        };
        let state_end = columns.state + STATE_COLS;
        write_str_clipped(buf, columns.state, y, word, state_style, state_end);
        let detail_x = columns.state + word.chars().count() as u16 + 1;
        write_str_clipped(buf, detail_x, y, &detail, base.patch(muted), state_end);

        let place = match entry.place {
            JobPlace::Tray => "tray",
            JobPlace::RunPane(_) => "run pane",
        };
        write_str(buf, columns.place, y, &place_text(place), base.patch(muted));
    }

    let parts = Footer {
        hints: &JOB_LIST_HINTS,
        ..Footer::default()
    };
    picker::modal_footer(buf, footer, &parts, plate, theme, scene);
}

/// `text` right-aligned in the place column.
fn place_text(text: &str) -> String {
    format!("{text:>w$}", w = usize::from(PLACE_COLS))
}

/// Paint the mark of `state` into `cell` in the foreground of `style`.
///
/// Under a live scene a running Cmd's mark is the round dot of the session
/// finder, at the small type scale and centered in the cell. Every other mark,
/// and each mark with no live scene or no RGB color, is the glyph of the
/// state, as [`JobState::mark`] gives it.
fn paint_mark(
    state: JobState,
    cell: Rect,
    style: Style,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let dot = style_rgb(style.fg).filter(|_| scene.live() && state == JobState::Running);
    if let Some(color) = dot {
        let scale = theme.type_scale(TypeRole::Small);
        TextRun {
            col: ((16 - text_run::advance_sixteenths(1, scale).min(16)) / 2) as i16,
            row: 0,
            scale,
            color,
            bg: None,
            text: "\u{25cf}",
            bold: false,
            italic: false,
            follow: 0,
            anchor: None,
        }
        .render(cell, buf, scene);
        return;
    }
    write_str(buf, cell.x, cell.y, &state.mark().to_string(), style);
}

#[cfg(test)]
mod tests {
    use super::{job_list_layout, job_list_rows, Columns, STATE_COLS};
    use crate::app::Stoat;
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};
    use std::time::Duration;

    /// Row `y` of `buf` across `inner`, as the cells' text, one char a cell,
    /// and the foreground of each cell.
    fn row_of(buf: &Buffer, inner: Rect, y: u16) -> (String, Vec<Color>) {
        let cells = (inner.left()..inner.right()).map(|x| &buf[(x, y)]);
        let text = cells.clone().map(|cell| cell.symbol()).collect::<String>();
        (text, cells.map(|cell| cell.fg).collect())
    }

    /// The cell of each of `parts` in `row`, the first one after the part
    /// before it.
    fn cells_of(row: &str, parts: &[&str]) -> Vec<usize> {
        let chars: Vec<char> = row.chars().collect();
        let mut from = 0;
        parts
            .iter()
            .map(|part| {
                let part: Vec<char> = part.chars().collect();
                let at = (from..chars.len())
                    .find(|&at| chars[at..].starts_with(&part))
                    .expect("the part follows the one before it");
                from = at + part.len();
                at
            })
            .collect()
    }

    /// With no live scene the row of a Job reads its mark, its line, its state
    /// and time, and its place, the mark and the word in the state color and
    /// the place muted. A failed Cmd of a Run pane reads its exit code.
    #[test]
    fn a_job_row_reads_its_mark_line_state_and_place() {
        let mut h = Stoat::test();
        h.stoat.stoatty = false;
        h.open_run();
        let failed = h.submit_run("make");
        h.inject_cmd_done(failed.cmd, 101);
        h.advance_clock(Duration::from_secs(1));
        h.push_job("sleep 30");
        h.type_keys("escape space J j");
        h.snapshot();
        let (_, inner) = job_list_layout(h.stoat.size(), 2).expect("the list fits");
        let rows = job_list_rows(inner);
        let buf = h.rendered_buffer();

        let (job, job_colors) = row_of(buf, inner, rows.y);
        let job_at = cells_of(&job, &["\u{25cf}", "sleep 30", "running 0:00", "tray"]);
        let (make, make_colors) = row_of(buf, inner, rows.y + 1);
        let make_at = cells_of(&make, &["\u{2717}", "make", "failed exit 101"]);
        let fg = |colors: &[Color], at: &[usize], parts: &[usize]| {
            parts
                .iter()
                .map(|&part| colors[at[part]])
                .collect::<Vec<_>>()
        };
        let (running, muted, red) = (
            Color::Rgb(0xc5, 0x9a, 0xf0),
            Color::Rgb(0x83, 0x8a, 0x98),
            Color::Rgb(0xf2, 0x77, 0x7f),
        );
        assert_eq!(
            (
                fg(&job_colors, &job_at, &[0, 2, 3]),
                fg(&make_colors, &make_at, &[0, 2]),
                make.trim_end().ends_with("run pane"),
            ),
            (vec![running, running, muted], vec![red, red], true),
            "{job:?} {make:?}",
        );
    }

    /// A running Cmd whose report counts its progress reads the counts in
    /// place of its time.
    #[test]
    fn a_running_cmd_with_progress_reads_its_counts() {
        let mut h = Stoat::test();
        h.stoat.stoatty = false;
        let job = h.push_job("cargo test");
        h.inject_cmd_output(
            job.cmd,
            b"running 4 tests\r\ntest a ... ok\r\ntest b ... ok\r\n",
        );
        h.type_keys("escape space J j");
        h.snapshot();

        let (_, inner) = job_list_layout(h.stoat.size(), 1).expect("the list fits");
        let (row, _) = row_of(h.rendered_buffer(), inner, job_list_rows(inner).y);
        let at = usize::from(Columns::of(inner).state - inner.x);
        let state: String = row.chars().skip(at).take(usize::from(STATE_COLS)).collect();
        assert_eq!(state.trim_end(), "running 2/4");
    }

    /// The characters of a line that the filter matched draw in the list's
    /// match style, and the others keep the row's.
    #[test]
    fn a_filtered_line_marks_its_matched_characters() {
        let mut h = Stoat::test();
        h.stoat.stoatty = false;
        h.open_run();
        let echo = h.submit_run("echo a");
        h.inject_cmd_done(echo.cmd, 0);
        h.type_keys("escape space J j");
        h.type_text("ech");
        h.settle();
        h.snapshot();

        let (_, inner) = job_list_layout(h.stoat.size(), 1).expect("the list fits");
        let (_, colors) = row_of(h.rendered_buffer(), inner, job_list_rows(inner).y);
        let at = usize::from(Columns::of(inner).line - inner.x);
        let matched = h.stoat.theme.list_match().fg;
        let marks: Vec<bool> = colors[at..at + 4]
            .iter()
            .map(|&fg| Some(fg) == matched)
            .collect();
        assert_eq!(marks, [true, true, true, false]);
    }
}
