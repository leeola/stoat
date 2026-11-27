//! The Job List's Overlay, the session finder's plate over the rows of a
//! [`JobList`].
//!
//! A row names the state of its Cmd, its line, its time, and where it
//! stands, so a user finds a Cmd by any of them and acts on it from here.
//!
//! A host that lays rows out by pixels gets the rows at the board's px, 20 px
//! high and 1 px apart under a 22 px column header, in an Island that ends
//! 4 px under the last row.

use crate::{
    fuzzy,
    job_list::{JobList, JobPlace, JobScope},
    jobs::{self, JobState},
    render::{
        self,
        chrome::{
            self, IslandFit, ISLAND_BOTTOM, ISLAND_SIDE, ISLAND_TOP, PLATE_EDGES,
            SELECTED_ROW_RADIUS,
        },
        command_palette,
        editor::ResolvedChrome,
        paint::style_rgb,
        picker::{self, FilterHeader, Footer, FILTER_PROMPT},
        pixel_rows::{self, PixelBar, Placer, RowMetrics, ROW_GAP, ROW_PAD},
        text::{write_str, write_str_clipped},
        workspace_picker, TypeScale,
    },
    theme::{scope, Theme, TypeRole, FULL_CELL_SCALE},
    workspace::Workspace,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style, widgets::StatefulWidget};
use stoat_widgets::{
    bar::Bar,
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

/// The logical px of the column header at the board's pixels, the boards'
/// `height: 22px`.
const HEADER_PX: u16 = 22;

/// The logical px of the rule under the column header, the boards'
/// `border-bottom: 1px`.
const RULE_PX: u16 = 1;

/// The logical px of the list's pad over its first row and under its last,
/// the boards' `padding: 4px`.
const LIST_PAD_PX: u16 = 4;

/// The logical px from either side of the Island to a row's content, the
/// list's 4 px pad and the row's `padding: 0 8px`.
const ROW_INSET_PX: u16 = 12;

/// The logical px of the mark's column, the first of the boards'
/// `grid-template-columns`.
const MARK_COLUMN_PX: u16 = 14;

/// The logical px across a running Cmd's dot.
const DOT_PX: u16 = 6;

/// The logical px between two columns, the boards' `column-gap: 8px`.
const COLUMN_GAP_PX: u16 = 8;

/// The logical px that an Island's top stands under the top of the row above
/// its first row, as [`ISLAND_TOP`] sets it, so the top stands `h - 14` px
/// over the first row.
const ISLAND_TOP_PX: u16 = 14;

/// The logical px that an Island's side stands in from the far side of the
/// column beside its cells, as [`ISLAND_SIDE`] sets it.
const ISLAND_SIDE_PX: u16 = 6;

/// The logical px that an Island ends under its last row, as
/// [`ISLAND_BOTTOM`] sets it.
const ISLAND_BOTTOM_PX: u16 = 4;

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

/// Where the Job List's box and its rows stand, as [`job_list_layout`] lays
/// them out.
pub struct JobListLayout {
    /// The outer box.
    pub(crate) modal: Rect,
    /// The box less its border, which holds the filter header, the Island,
    /// and the key line.
    pub inner: Rect,
    /// The cell rows under the column header that the rows draw in, which a
    /// press reads.
    pub rows: Rect,
    /// How the rows stand at the board's pixels, or `None` for rows on whole
    /// cells.
    pub(crate) pixels: Option<JobListPixels>,
}

/// How a Job List lays its rows out at the board's pixels.
pub(crate) struct JobListPixels {
    pub(crate) metrics: RowMetrics,
    /// The Island's cell rows from the column header's row down, its bottom
    /// edge, and the key line and the plate's bottom under it.
    pub(crate) fit: IslandFit,
    /// The rows on show.
    pub(crate) shown: usize,
}

/// Where the parts of a pixel row of the Job List draw, and the metrics they
/// lay out by.
///
/// Each column is in sixteenths from the inner rect's left edge. The boards
/// measure the columns from the Island's left edge, which stands 3 px left of
/// that edge at the 9 px cell, where [`ISLAND_SIDE`] reaches.
struct RowLayout {
    m: RowMetrics,
    /// The color of a run whose foreground has no RGB value.
    fallback: [u8; 3],
    /// The start and the width of the mark's 14 px column.
    mark: (i32, i32),
    /// The left of a running Cmd's dot, centered in the mark's column.
    dot: i32,
    line: i32,
    /// Where the line stops, 8 px before the state text.
    line_end: i32,
    state: i32,
    /// Where the place ends, 12 px inside the Island's right edge.
    place_end: i32,
    /// The ends of a row's box, 4 px inside the Island.
    row_box: (i32, i32),
    /// The ends of the Island, which the header's rule spans.
    island: (i32, i32),
}

impl RowLayout {
    /// The layout of the rows over `inner` under `m`, whose runs fall back to
    /// `fallback`.
    ///
    /// The place keeps its 8 chars and the state text its 16 at the cell size,
    /// as the cell form does.
    fn of(inner: Rect, m: RowMetrics, fallback: [u8; 3]) -> RowLayout {
        let cell = i64::from(m.cell[0].max(1));
        let px = |logical: u16| i64::from(m.px(logical));
        let sixteenths = |px: i64| (px * 16 + cell / 2).div_euclid(cell) as i32;
        let left = px(ISLAND_SIDE_PX) - cell;
        let right = i64::from(inner.width) * cell - left;

        let mark = left + px(ROW_INSET_PX);
        let place_end = right - px(ROW_INSET_PX);
        let state_end = place_end - i64::from(PLACE_COLS) * cell - px(COLUMN_GAP_PX);
        let state = state_end - i64::from(STATE_COLS) * cell;
        RowLayout {
            m,
            fallback,
            mark: (
                sixteenths(mark),
                sixteenths(mark + px(MARK_COLUMN_PX)) - sixteenths(mark),
            ),
            dot: sixteenths(mark + (px(MARK_COLUMN_PX) - px(DOT_PX)) / 2),
            line: sixteenths(mark + px(MARK_COLUMN_PX) + px(COLUMN_GAP_PX)),
            line_end: sixteenths(state - px(COLUMN_GAP_PX)),
            state: sixteenths(state),
            place_end: sixteenths(place_end),
            row_box: (
                sixteenths(left + px(LIST_PAD_PX)),
                sixteenths(right - px(LIST_PAD_PX)),
            ),
            island: (sixteenths(left), sixteenths(right)),
        }
    }

    /// Draw `spans` as runs at `scale` on the line `anchor` from `col`
    /// sixteenths, as [`pixel_rows::emit_spans`] draws them.
    fn runs<S: AsRef<str>>(
        &self,
        spans: &[(S, Style)],
        col: i32,
        scale: u16,
        anchor: Rect,
        buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        let col = u16::try_from(col.max(0)).unwrap_or(u16::MAX);
        pixel_rows::emit_spans(spans, col, scale, anchor, self.fallback, buf, scene);
    }

    /// Draw the mark of `state` on the line `anchor` in the foreground of
    /// `style`, centered in the mark's column.
    ///
    /// A running Cmd with an RGB color draws the boards' 6 px dot, centered
    /// in the row's box. Every other mark draws its glyph at the cell size.
    fn mark(
        &self,
        state: JobState,
        style: Style,
        anchor: Rect,
        buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        let m = &self.m;
        if let Some(color) = style_rgb(style.fg).filter(|_| state == JobState::Running) {
            let top =
                (m.box_height(1).saturating_sub(m.px(DOT_PX)) / 2).saturating_sub(m.px(ROW_PAD));
            let y = m.px_rows16(top);
            Bar {
                x: self.dot as i16,
                y: y as i16,
                width: m.cols(DOT_PX),
                height: m.px_rows16(top + m.px(DOT_PX)) - y,
                color,
                radius: 3,
                corners: 0,
            }
            .render(anchor, buf, scene);
            return;
        }
        let glyph = [(state.mark().to_string(), style)];
        let col = self.mark.0 + (self.mark.1 - 16) / 2;
        self.runs(&glyph, col, FULL_CELL_SCALE, anchor, buf, scene);
    }
}

/// Lay the Job List out within `area` over `rows` rows, as the session finder
/// lays its box out.
///
/// An empty list keeps one row, which reads `no jobs`.
///
/// With `pixels` the rows stand at the board's pixels, 21 px apart at the
/// 18 px cell. A list whose rows fit the Island of the box at its most list
/// rows sizes the box to them, the Island ending 4 px under its last row, as
/// [`chrome::island_fit`] fits it. A longer list keeps the box of the cells
/// and shows the rows that its Island holds.
pub fn job_list_layout(
    area: Rect,
    rows: usize,
    pixels: Option<RowMetrics>,
) -> Option<JobListLayout> {
    let rows = rows.max(1);
    let Some(m) = pixels else {
        let (modal, inner) = workspace_picker::workspace_picker_layout(area, rows)?;
        return Some(JobListLayout {
            modal,
            inner,
            rows: job_list_rows(inner),
            pixels: None,
        });
    };

    let h = ((f32::from(m.cell[1]) / m.scale).round() as u16).max(1);
    let count = u16::try_from(rows).unwrap_or(u16::MAX);
    let most = workspace_picker::max_list_rows(area);
    let (shown, below) = match count <= rows_held(h, most) {
        true => (count, island_below(h, count)),
        false => {
            let list_rows = count.min(most);
            (rows_held(h, list_rows), cell_island_below(h, list_rows))
        },
    };
    let fit = chrome::island_fit(h, below);
    let (modal, inner) =
        workspace_picker::picker_box(area, 5 + fit.rows + u16::from(fit.extra_row))?;
    Some(JobListLayout {
        modal,
        inner,
        rows: Rect::new(
            inner.x,
            inner.y + 3,
            inner.width,
            fit.rows.saturating_sub(1),
        ),
        pixels: Some(JobListPixels {
            metrics: m,
            fit,
            shown: usize::from(shown),
        }),
    })
}

/// The rows the list paints in, inside the modal's `inner` rect.
///
/// The paint and the pointer's hit test both read it, so a press lands on the
/// row drawn under it.
pub fn job_list_rows(inner: Rect) -> Rect {
    workspace_picker::workspace_picker_list(inner)
}

/// Paint the Job List over `area`.
///
/// The box keeps the height of every row while the filter narrows them, as
/// the session finder's box does.
///
/// Under [`RowMetrics::of`] the rows stand at the board's pixels, as
/// [`job_list_layout`] lays them out and [`paint_pixel_rows`] draws them, and
/// the plate and the key line follow the Island's bottom edge. The metrics
/// stay on the list for a press to read.
pub(crate) fn render_job_list(
    list: &mut JobList,
    ws: &mut Workspace,
    theme: &Theme,
    resolved: &ResolvedChrome,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    list.row_metrics = RowMetrics::of(theme, scene);
    let Some(layout) = job_list_layout(area, list.entries().len(), list.row_metrics) else {
        return;
    };
    let JobListLayout {
        modal,
        inner,
        rows: rows_rect,
        pixels,
    } = layout;
    let modal_style = theme.get(scope::UI_MODAL_PICKER);
    let plate = chrome::plate_colors(theme, scene);
    match pixels.as_ref().zip(plate) {
        Some((pixels, plate)) => {
            let [top, right, _, left] = PLATE_EDGES;
            let edges = [top, right, pixels.fit.plate_bottom, left];
            chrome::modal_plate(buf, inner, edges, plate, theme, scene);
        },
        None => {
            chrome::list_plate(buf, modal, modal_style, theme, &mut *scene);
        },
    }

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

    let footer = picker::footer_rect(modal);
    if let Some(plate) = plate {
        let body_top = inner.y + 2;
        let (rows, bottom) = match &pixels {
            Some(pixels) => (pixels.fit.rows, pixels.fit.bottom),
            None => (footer.y.saturating_sub(body_top), ISLAND_BOTTOM),
        };
        let body = Rect::new(inner.x, body_top, inner.width, rows);
        let edges = [ISLAND_TOP, ISLAND_SIDE, bottom, ISLAND_SIDE];
        chrome::modal_island(body, edges, plate, theme, buf, scene);
    }

    let parts = Footer {
        hints: &JOB_LIST_HINTS,
        ..Footer::default()
    };
    if let Some(pixels) = &pixels {
        paint_pixel_rows(list, ws, inner, pixels, theme, buf, scene);
        let key_row = inner.y + 2 + pixels.fit.rows;
        let rect = Rect::new(modal.x + 1, key_row, modal.width.saturating_sub(2), 1);
        let down = pixels.fit.footer_down;
        picker::modal_footer_at(buf, rect, &parts, plate, down, theme, scene);
        return;
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

    picker::modal_footer(buf, footer, &parts, plate, theme, scene);
}

/// The row on show, counted from the first, whose box or the gap under it
/// holds the physical px row `pixel` of a pixel Job List under `m`, counted
/// from the top of the list's first cell row under the column header, or
/// `None` for a pixel over the first row's box.
///
/// A press reads it, so it lands on the row drawn under it.
pub(crate) fn pixel_row_at(m: RowMetrics, pixel: u32) -> Option<usize> {
    let down = i64::from(pixel) + i64::from(m.cell[1]) - row_px(&m, 0);
    (down >= 0).then(|| (down / i64::from(m.pitch(1).max(1))) as usize)
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

/// Paint the column header, its rule, and the rows of `list` over the Island
/// in `inner` at the board's pixels, as `pixels` lays them out.
///
/// No cell is written. The header's labels draw at the Small size in the
/// middle of its 22 px, and its rule on the pixel row under it, across the
/// Island. Each row's box stands one row pitch under the one before it, the
/// first 5 px under the rule's row at the 18 px cell, with the selected row's
/// box drawn first, and its text draws at the cell size on its line 1 px
/// under the box's top. The paint ends with no nudge in force.
fn paint_pixel_rows(
    list: &mut JobList,
    ws: &Workspace,
    inner: Rect,
    pixels: &JobListPixels,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let m = pixels.metrics;
    let fallback = style_rgb(Some(render::themed_fg(theme))).unwrap_or([0xff; 3]);
    let layout = RowLayout::of(inner, m, fallback);
    let body = Rect::new(inner.x, inner.y + 2, inner.width, pixels.fit.rows);
    let header = Rect { height: 1, ..body };
    let muted = theme.get(scope::UI_TEXT_MUTED);

    let small = theme.type_scale(TypeRole::Small);
    let where_col = layout.place_end - i32::from(text_run::advance_sixteenths(5, small));
    scene.nudge([0, label_nudge(&m)]);
    for (label, col) in [
        ("command", layout.line),
        ("state", layout.state),
        ("where", where_col),
    ] {
        layout.runs(&[(label, muted)], col, small, header, buf, scene);
    }
    scene.nudge([0, 0]);

    let mut placer = Placer::default();
    let rule = theme
        .try_get(scope::UI_DIVIDER)
        .filter(|style| style.fg.is_some())
        .unwrap_or_else(|| theme.get(scope::UI_BORDER_INACTIVE));
    if let Some(color) = style_rgb(rule.fg) {
        let (left, right) = layout.island;
        let bar = PixelBar {
            x: left as i16,
            y: down(rule_px(&m)),
            width: (right - left) as u16,
            height: m.rows16(RULE_PX).max(1),
            color,
            radius: 0,
        };
        pixel_rows::bar(&mut placer, &m, body, &bar, buf, scene);
    }

    let shown = pixels.shown;
    list.viewport_rows = Some(shown);
    let row_style = theme.get(scope::UI_TEXT);
    let selected_style = theme.list_selected(scope::UI_SELECTION);
    let match_style = theme.list_match();
    let line_chars = usize::try_from((layout.line_end - layout.line).max(0) / 16).unwrap_or(0);
    if list.filtered().is_empty() {
        let anchor = placer.line(&m, body, down(row_px(&m, 0)) + m.px(ROW_PAD), scene);
        let text: String = "no jobs".chars().take(line_chars).collect();
        let disabled = theme.get(scope::UI_TEXT_DISABLED);
        layout.runs(
            &[(text, disabled)],
            layout.line,
            FULL_CELL_SCALE,
            anchor,
            buf,
            scene,
        );
    }

    let mut derived = Vec::new();
    let mut matching = fuzzy::Scratch::default();
    let selected = list.selected();
    let start = picker::window_start(selected, shown);
    for (i, &idx) in list.filtered().iter().enumerate().skip(start).take(shown) {
        let entry = &list.entries()[idx];
        let Some(cmd) = ws.cmds.get(entry.cmd) else {
            continue;
        };
        let top = down(row_px(&m, i - start));
        let base = match i == selected {
            true => {
                if let Some(color) = style_rgb(selected_style.bg) {
                    let (left, right) = layout.row_box;
                    let bar = PixelBar {
                        x: left as i16,
                        y: top,
                        width: (right - left) as u16,
                        height: m.box16(1),
                        color,
                        radius: SELECTED_ROW_RADIUS,
                    };
                    pixel_rows::bar(&mut placer, &m, body, &bar, buf, scene);
                }
                Style {
                    bg: row_style.bg,
                    ..selected_style
                }
            },
            false => row_style,
        };
        let anchor = placer.line(&m, body, top + m.px(ROW_PAD), scene);

        let state = jobs::state_of(cmd);
        let state_style = base.patch(jobs::state_style(theme, state));
        layout.mark(state, state_style, anchor, buf, scene);

        let line: String = entry.line.chars().take(line_chars).collect();
        let indices = list.row_indices(i, &mut derived, &mut matching);
        let spans = command_palette::match_spans(&line, 0, indices, base, match_style);
        layout.runs(&spans, layout.line, FULL_CELL_SCALE, anchor, buf, scene);

        let word = state.word();
        let detail = match (state, cmd.progress()) {
            (JobState::Running, Some((done, total))) => format!("{done}/{total}"),
            _ => jobs::time_text(cmd),
        };
        let room = usize::from(STATE_COLS).saturating_sub(word.chars().count() + 1);
        let detail: String = detail.chars().take(room).collect();
        let status = [
            (word, state_style),
            (" ", base),
            (detail.as_str(), base.patch(muted)),
        ];
        layout.runs(&status, layout.state, FULL_CELL_SCALE, anchor, buf, scene);

        let place = match entry.place {
            JobPlace::Tray => "tray",
            JobPlace::RunPane(_) => "run pane",
        };
        let width = text_run::advance_sixteenths(place.chars().count(), FULL_CELL_SCALE);
        let col = layout.place_end - i32::from(width);
        layout.runs(
            &[(place, base.patch(muted))],
            col,
            FULL_CELL_SCALE,
            anchor,
            buf,
            scene,
        );
    }
    placer.end(scene);
}

/// The logical px from the top of the column header's row down to the bottom
/// of an Island that holds `rows` rows on cells `h` logical px high, the
/// list's pad under the last row's box.
fn island_below(h: u16, rows: u16) -> u16 {
    let rows = u32::from(rows);
    let boxes = rows * (u32::from(h) + 2 * u32::from(ROW_PAD))
        + rows.saturating_sub(1) * u32::from(ROW_GAP);
    let head = u32::from(ISLAND_TOP_PX + HEADER_PX + RULE_PX + 2 * LIST_PAD_PX);
    saturate((head + boxes).saturating_sub(u32::from(h)))
}

/// The logical px from the top of the column header's row down to the bottom
/// of the Island of the cells' box of `list_rows` list rows on cells `h`
/// logical px high.
fn cell_island_below(h: u16, list_rows: u16) -> u16 {
    saturate(u32::from(list_rows + 1) * u32::from(h) + u32::from(ISLAND_BOTTOM_PX))
}

/// The most rows, at least one, whose Island fits the Island of the cells'
/// box of `list_rows` list rows on cells `h` logical px high.
fn rows_held(h: u16, list_rows: u16) -> u16 {
    let first = u32::from(island_below(h, 1));
    let pitch = u32::from(h) + 2 * u32::from(ROW_PAD) + u32::from(ROW_GAP);
    match u32::from(cell_island_below(h, list_rows)).checked_sub(first) {
        Some(room) => saturate(room / pitch + 1),
        None => 1,
    }
}

/// The physical px from the top of the column header's row down to the rule
/// under the header, the pixel row under its 22 px.
///
/// The header starts at the Island's top, `h - 14` px over the row.
fn rule_px(m: &RowMetrics) -> i64 {
    i64::from(m.px(ISLAND_TOP_PX) + m.px(HEADER_PX)) - i64::from(m.cell[1])
}

/// The physical px from the top of the column header's row down to the top
/// of the box of row `row` on show, the rule and the list's pad under the
/// header and one row pitch under the row before it.
fn row_px(m: &RowMetrics, row: usize) -> i64 {
    let first = rule_px(m) + i64::from(m.px(RULE_PX) + m.px(LIST_PAD_PX));
    first + row as i64 * i64::from(m.pitch(1))
}

/// The nudge in logical px that centers the column header's labels in its
/// 22 px, from the top of its row.
fn label_nudge(m: &RowMetrics) -> i16 {
    let cell = i64::from(m.cell[1]);
    let top = i64::from(m.px(ISLAND_TOP_PX)) - cell + (i64::from(m.px(HEADER_PX)) - cell) / 2;
    (top as f32 / m.scale).round() as i16
}

/// `px` as a count of physical px down an area, held at 0.
fn down(px: i64) -> u32 {
    u32::try_from(px.max(0)).unwrap_or(u32::MAX)
}

/// `value` held at [`u16::MAX`].
fn saturate(value: u32) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests {
    use super::{job_list_layout, job_list_rows, render_job_list, Columns, STATE_COLS};
    use crate::{
        app::Stoat,
        jobs::{self, JobState},
        render::{
            chrome::{IslandFit, ISLAND_SIDE, ISLAND_TOP, PLATE_EDGES},
            editor::ResolvedChrome,
            paint::style_rgb,
            pixel_rows::RowMetrics,
        },
        test_harness::TestHarness,
        theme::{scope, Theme},
    };
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};
    use std::{sync::Arc, time::Duration};
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{self, Command, Edge};

    /// The board's 9 by 18 px cell at the scale 1.
    const M1: RowMetrics = RowMetrics {
        cell: [9, 18],
        scale: 1.0,
    };

    /// The board's cell at the scale 2, 18 by 36 physical px.
    const M2: RowMetrics = RowMetrics {
        cell: [18, 36],
        scale: 2.0,
    };

    /// A bar as its radius, the nudge over it, its y and x in sixteenths from
    /// the inner rect's top-left, its width, and its height.
    type BarAt = (u8, [i16; 2], i16, i16, u16, u16);

    /// A run as its text, the nudge over it, its row and col in sixteenths
    /// from the inner rect's top-left, and its color.
    type RunAt = (String, [i16; 2], i16, i16, [u8; 3]);

    /// The commands that the open Job List of `h` paints at the board's
    /// pixels under `m` into the harness's area, with the box's inner rect and
    /// the Island's cell rows.
    fn pixel_commands(h: &mut TestHarness, m: RowMetrics) -> (Vec<Command>, Rect, u16) {
        let mut list = h.stoat.job_list.take().expect("the list is open");
        let theme = h.stoat.theme.clone();
        let chrome = ResolvedChrome::resolve(&theme);
        let area = h.stoat.size();
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_nudge_protocol(true);
        scene.set_pixel_rows(Some((m.cell, m.scale)));
        let ws = h.stoat.active_workspace_mut();
        render_job_list(&mut list, ws, &theme, &chrome, area, &mut buf, &mut scene);
        let layout = job_list_layout(area, list.entries().len(), Some(m)).expect("the list fits");
        h.stoat.job_list = Some(list);

        let island = layout.pixels.map_or(0, |pixels| pixels.fit.rows);
        (command::decode_stream(scene.bytes()), layout.inner, island)
    }

    /// The bars and the runs of the Island's rows that the open Job List of
    /// `h` paints at the board's pixels under `m`, as [`pixel_commands`] gives
    /// them.
    fn pixel_paint(h: &mut TestHarness, m: RowMetrics) -> (Vec<BarAt>, Vec<RunAt>) {
        let (commands, inner, island) = pixel_commands(h, m);
        let (left, top) = (inner.x as i16 * 16, inner.y as i16 * 16);
        let rows = (top + 32)..(top + 32 + island as i16 * 16);
        let mut nudge = [0, 0];
        let (mut bars, mut runs) = (Vec::new(), Vec::new());
        for command in commands {
            match command {
                Command::Nudge(offset) => nudge = offset,
                Command::Bar(bar) if rows.contains(&(bar.y - bar.y.rem_euclid(16))) => {
                    let at = (bar.y - top, bar.x - left, bar.width, bar.height);
                    bars.push((bar.radius, nudge, at.0, at.1, at.2, at.3));
                },
                Command::TextRun(run) if rows.contains(&run.row) => {
                    runs.push((run.text, nudge, run.row - top, run.col - left, run.color))
                },
                _ => {},
            }
        }
        (bars, runs)
    }

    /// The RGB foreground of `scope` under the theme of `h`.
    fn fg(h: &TestHarness, scope: &str) -> [u8; 3] {
        style_rgb(h.stoat.theme.get(scope).fg).expect("an RGB foreground")
    }

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
        let inner = job_list_layout(h.stoat.size(), 2, None)
            .expect("the list fits")
            .inner;
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

        let inner = job_list_layout(h.stoat.size(), 1, None)
            .expect("the list fits")
            .inner;
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

        let inner = job_list_layout(h.stoat.size(), 1, None)
            .expect("the list fits")
            .inner;
        let (_, colors) = row_of(h.rendered_buffer(), inner, job_list_rows(inner).y);
        let at = usize::from(Columns::of(inner).line - inner.x);
        let matched = h.stoat.theme.list_match().fg;
        let marks: Vec<bool> = colors[at..at + 4]
            .iter()
            .map(|&fg| Some(fg) == matched)
            .collect();
        assert_eq!(marks, [true, true, true, false]);
    }

    /// A pixel list's Island ends 4 px under its last row, 26 px and 21 px a
    /// row under the top of the column header's row at the 18 px cell, and
    /// `island_fit` sets its rows, its bottom edge, the key line, and the
    /// plate's bottom by that. One row's 47 px take 2 rows and 11 px more, so
    /// the box is 7 rows high. Three rows' 89 px take 4 rows and 17 px, with a
    /// blank row under the key line for the plate, at either scale.
    #[test]
    fn a_pixel_job_list_ends_four_pixels_under_its_last_row() {
        let fit = |rows: usize, m: RowMetrics| {
            let layout =
                job_list_layout(Rect::new(0, 0, 80, 24), rows, Some(m)).expect("the list fits");
            let pixels = layout.pixels.expect("the rows stand at pixels");
            (pixels.fit, pixels.shown, layout.modal.height)
        };

        let one = IslandFit {
            rows: 2,
            bottom: Edge::Outset(11),
            footer_down: 14,
            plate_bottom: Edge::Outset(17),
            extra_row: false,
        };
        let three = IslandFit {
            rows: 4,
            bottom: Edge::Outset(17),
            footer_down: 20,
            plate_bottom: Edge::Outset(5),
            extra_row: true,
        };
        assert_eq!(
            [fit(1, M1), fit(3, M1), fit(3, M2)],
            [(one, 1, 7), (three, 3, 10), (three, 3, 10)],
        );
    }

    /// At the board's pixels the column header's labels center in its 22 px,
    /// 2 px over their row, and its rule is one bar 1 high on the first pixel
    /// row of the first list row, across the Island from 3 px left of the
    /// inner rect. The first row's box stands 5 px into that row, 1 px inside
    /// each end of the inner rect, and its line 6 px. The dot of a running Cmd
    /// centers in the 14 px column from 12 px, at col 23, the line starts at
    /// 34 px, col 55, and the place ends 12 px inside the Island with the
    /// state text 8 px before it. The scale 2 sets them on the same
    /// sixteenths.
    #[test]
    fn a_pixel_job_row_stands_five_pixels_under_its_header() {
        let mut h = Stoat::test();
        h.push_job("sleep 30");
        h.type_keys("escape space J j");
        h.snapshot();

        let muted = fg(&h, scope::UI_TEXT_MUTED);
        let selected = style_rgb(h.stoat.theme.list_selected(scope::UI_SELECTION).fg)
            .expect("an RGB selected foreground");
        let running = style_rgb(jobs::state_style(&h.stoat.theme, JobState::Running).fg)
            .expect("an RGB running color");
        let run = |text: &str, nudge: i16, row: i16, col: i16, color: [u8; 3]| {
            (text.to_owned(), [0, nudge], row, col, color)
        };
        let painted = (
            vec![
                (0, [0, 0], 48, -5, 1194, 1),
                (4, [0, 5], 48, 2, 1180, 18),
                (3, [0, 6], 53, 23, 11, 6),
            ],
            vec![
                run("command", -2, 32, 55, muted),
                run("state", -2, 32, 770, muted),
                run("where", -2, 32, 1088, muted),
                run("sleep 30", 6, 48, 55, selected),
                run("running", 6, 48, 770, running),
                run("0:00", 6, 48, 898, muted),
                run("tray", 6, 48, 1104, muted),
            ],
        );
        assert_eq!(
            [pixel_paint(&mut h, M1), pixel_paint(&mut h, M2)],
            [painted.clone(), painted],
        );
    }

    /// The selected pixel row draws its line in the selected foreground over
    /// its box, and a row that is not the selected one in the text color,
    /// under a theme that gives the two apart.
    #[test]
    fn a_selected_pixel_row_reads_in_the_selected_foreground() {
        let mut h = Stoat::test();
        h.push_job("sleep 30");
        h.push_job("sleep 31");
        h.type_keys("escape space J j");
        h.snapshot();
        let src = r##"theme t {
            ui.surface.ground.bg = "#0c0e11";
            ui.background.bg = "#17191e";
            ui.text.fg = "#010101";
            ui.list.selected = { fg: "#020202", bg: "#7f7f7f" };
        }"##;
        let (config, _) = stoat_config::parse(src);
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
        h.stoat.theme = Arc::new(theme);

        let (_, runs) = pixel_paint(&mut h, M1);
        let lines: Vec<(String, [u8; 3])> = runs
            .into_iter()
            .filter(|run| run.2 >= 48 && run.3 == 55)
            .map(|run| (run.0, run.4))
            .collect();
        assert_eq!(
            lines,
            [
                ("sleep 30".to_owned(), [2, 2, 2]),
                ("sleep 31".to_owned(), [1, 1, 1])
            ],
        );
    }

    /// A second pixel row stands one row pitch, 21 px, under the first, so its
    /// line draws 9 px into the second list row, in the text color for a row
    /// that is not the selected one. An ended Cmd's mark is its glyph at the
    /// cell size, centered in the mark's 14 px column at col 20.
    #[test]
    fn a_second_pixel_row_stands_one_pitch_under_the_first() {
        let mut h = Stoat::test();
        h.open_run();
        let failed = h.submit_run("make");
        h.inject_cmd_done(failed.cmd, 1);
        h.advance_clock(Duration::from_secs(1));
        h.push_job("sleep 30");
        h.type_keys("escape space J j");
        h.snapshot();

        let (muted, text) = (fg(&h, scope::UI_TEXT_MUTED), fg(&h, scope::UI_TEXT));
        let red = style_rgb(jobs::state_style(&h.stoat.theme, JobState::Failed).fg)
            .expect("an RGB failed color");
        let (_, runs) = pixel_paint(&mut h, M1);
        let second: Vec<_> = runs.into_iter().filter(|run| run.2 == 64).collect();
        let run = |text: &str, col: i16, color: [u8; 3]| (text.to_owned(), [0, 9], 64, col, color);
        assert_eq!(
            second,
            [
                run("\u{2717}", 20, red),
                run("make", 55, text),
                run("failed", 770, red),
                run("exit 1", 882, muted),
                run("run pane", 1040, muted),
            ],
        );
    }

    /// A pixel row cuts its line at the 43 chars that fit before the 8 px
    /// ahead of the state text on 74 columns, and its state text at its 16
    /// chars.
    #[test]
    fn a_pixel_row_cuts_its_line_and_its_state_text_at_their_columns() {
        let mut h = Stoat::test();
        let line = format!("cargo test {}", "x".repeat(50));
        let job = h.push_job(&line);
        let results = "test a ... ok\r\n".repeat(12);
        h.inject_cmd_output(
            job.cmd,
            format!("running 1234567 tests\r\n{results}").as_bytes(),
        );
        h.type_keys("escape space J j");
        h.snapshot();

        let (_, runs) = pixel_paint(&mut h, M1);
        let texts: Vec<String> = runs
            .into_iter()
            .filter(|run| run.2 == 48)
            .map(|run| run.0)
            .collect();
        let cut: String = line.chars().take(43).collect();
        assert_eq!(texts, [cut.as_str(), "running", "12/12345", "tray"]);
    }

    /// At the board's pixels the Island's bottom edge, the key line, and the
    /// plate's bottom follow the fit. One row's Island takes the header's row
    /// and the row under it and ends 11 px past them, the key line draws 14 px
    /// down the row under the Island, and the plate ends 17 px under it.
    #[test]
    fn a_pixel_job_list_ends_its_island_its_key_line_and_its_plate_by_its_fit() {
        let mut h = Stoat::test();
        h.push_job("sleep 30");
        h.type_keys("escape space J j");
        h.snapshot();

        let (commands, inner, _) = pixel_commands(&mut h, M1);
        let mut nudge = [0, 0];
        let (mut panels, mut key) = (Vec::new(), None);
        for command in commands {
            match command {
                Command::Nudge(offset) => nudge = offset,
                Command::Panel(panel) if panel.width == inner.width && panel.fill.is_some() => {
                    panels.push((panel.top, panel.height, panel.edges))
                },
                Command::TextRun(run) if run.text == "Enter" => {
                    key = key.or(Some((nudge, run.row)))
                },
                _ => {},
            }
        }

        let [top, right, _, left] = PLATE_EDGES;
        let island = [ISLAND_TOP, ISLAND_SIDE, Edge::Outset(11), ISLAND_SIDE];
        assert_eq!(
            (panels, key),
            (
                vec![
                    (inner.y, inner.height, [top, right, Edge::Outset(17), left]),
                    (inner.y + 2, 2, island),
                ],
                Some(([1, 14], (inner.y as i16 + 4) * 16)),
            ),
        );
    }

    /// A pixel list shows the rows that the Island of its cells' box holds at
    /// the most list rows, 4 on 24 rows, where 6 list rows' Island ends 130 px
    /// under the header's row. Four rows size the box to them, and six keep
    /// the box and the rows of the cells with 4 rows on show. On 12 rows the
    /// cells' one list row holds no pixel row whole, and the list still shows
    /// one.
    #[test]
    fn a_long_pixel_job_list_keeps_the_box_of_its_cells() {
        let layout = |height: u16, rows: usize, m| {
            job_list_layout(Rect::new(0, 0, 80, height), rows, m).expect("the list fits")
        };
        let (four, six, cells) = (
            layout(24, 4, Some(M1)),
            layout(24, 6, Some(M1)),
            layout(24, 6, None),
        );
        let shown = |layout: &super::JobListLayout| {
            let pixels = layout.pixels.as_ref()?;
            Some((pixels.shown, pixels.fit.rows, pixels.fit.bottom))
        };

        assert_eq!(
            (
                shown(&four),
                four.modal.height,
                shown(&six),
                six.modal,
                six.rows,
                shown(&layout(12, 1, Some(M1))),
            ),
            (
                Some((4, 6, Edge::Outset(2))),
                11,
                Some((4, 7, Edge::Outset(4))),
                cells.modal,
                cells.rows,
                Some((1, 2, Edge::Outset(11))),
            ),
        );
    }
}
