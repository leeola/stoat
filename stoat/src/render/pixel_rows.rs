//! The pixel form of a modal's result list, which sets each result row at the
//! boards' pixel sizes rather than on whole cells.
//!
//! A board sets a result row 20 px high and 1 px from the next, and a wrapped
//! result grows by 18 px a line, so no row stands on the cell grid. The rows
//! draw as text runs and bars in a resident pool's pages, each line on the
//! cell row that holds its first pixel row and nudged down by the rest, as a
//! terminal at [`PAGE_NUDGE_PROTOCOL`] composes them.
//!
//! The layout computes in physical px, with the cell of the winsize and each
//! board px times the display scale. It is exact at the scales 1 and 2, and
//! within one px at a fractional scale.
//!
//! [`PAGE_NUDGE_PROTOCOL`]: stoatty_protocol::PAGE_NUDGE_PROTOCOL

use crate::{
    render::{chrome, paint::style_rgb, picker::RowStart},
    theme::Theme,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::StatefulWidget,
};
use std::ops::Range;
use stoat_widgets::{
    bar::Bar,
    text_run::{self, TextRun},
    ApcScene,
};
use unicode_width::UnicodeWidthStr;

/// The logical px over and under the lines of a row.
pub(crate) const ROW_PAD: u16 = 1;

/// The logical px between two rows.
pub(crate) const ROW_GAP: u16 = 1;

/// The cell and the display scale that a pixel list lays its rows out by.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RowMetrics {
    /// The grid cell in physical px, width then height.
    pub(crate) cell: [u16; 2],
    /// The physical px of one logical px.
    pub(crate) scale: f32,
}

/// A bar of a pixel list, placed on the line that holds its top pixel row.
pub(crate) struct PixelBar {
    /// The left edge in sixteenths of a cell from the area's left.
    pub(crate) x: i16,
    /// The top in physical px from the area's top.
    pub(crate) y: u32,
    /// The width in sixteenths of a cell.
    pub(crate) width: u16,
    /// The height in sixteenths of a cell.
    pub(crate) height: u16,
    pub(crate) color: [u8; 3],
    /// The corner radius in logical px.
    pub(crate) radius: u8,
}

/// Where the text runs of a row of cells draw.
pub(crate) struct RunsAt {
    /// The sixteenths from `anchor`'s left edge to where the first of the cells
    /// draws.
    pub(crate) col: u16,
    /// The text scale, which sets how far each char advances.
    pub(crate) scale: u16,
    /// The one-row rect the runs anchor on.
    pub(crate) anchor: Rect,
    /// The color a cell whose foreground has no RGB value draws in.
    pub(crate) fallback: [u8; 3],
}

/// The nudge that a pixel list last sent, so a line sends one only where its
/// offset differs from the line before it.
#[derive(Default)]
pub(crate) struct Placer {
    nudge: i16,
}

impl RowMetrics {
    /// The metrics of a pixel list under `scene`, or `None` where a list keeps
    /// the cell form.
    ///
    /// The pixel form needs the plate look, whose Island the rows stand on, and
    /// a host that draws nudged page runs and has stated its cell and scale.
    pub(crate) fn of(theme: &Theme, scene: &ApcScene) -> Option<RowMetrics> {
        chrome::plate_colors(theme, scene)?;
        let (cell, scale) = scene.pixel_rows()?;
        Some(RowMetrics { cell, scale })
    }

    /// The physical px of `logical` board px, rounded.
    pub(crate) fn px(&self, logical: u16) -> u32 {
        (f32::from(logical) * self.scale).round() as u32
    }

    /// The physical px of the box of a row of `lines` lines, which holds the
    /// lines and the pad over and under them.
    pub(crate) fn box_height(&self, lines: u32) -> u32 {
        lines * self.line_height() + 2 * self.px(ROW_PAD)
    }

    /// The physical px from the top of a row of `lines` lines to the top of the
    /// row after it.
    pub(crate) fn pitch(&self, lines: u32) -> u32 {
        self.box_height(lines) + self.px(ROW_GAP)
    }

    /// The physical px that a list `rows` cells tall gives its rows.
    ///
    /// The gap after the last row stands past the list, so the budget counts
    /// one gap more than the cells hold.
    pub(crate) fn extent(&self, rows: u16) -> u32 {
        u32::from(rows) * self.line_height() + self.px(ROW_GAP)
    }

    /// The physical px from the list's top to the top of result `result`, from
    /// `tops`, the line that each result starts on as [`RowTops`] sums them.
    ///
    /// [`RowTops`]: crate::picker::RowTops
    pub(crate) fn top(&self, tops: &[u32], result: usize) -> u32 {
        let spacing = 2 * self.px(ROW_PAD) + self.px(ROW_GAP);
        tops[result] * self.line_height() + result as u32 * spacing
    }

    /// Where a paint whose first physical px row is `at` starts, from `tops` as
    /// [`Self::top`] reads them, or `None` at or past the top of the entry
    /// after the last measured result.
    ///
    /// The paint starts on the last result whose top is at most `at`, with the
    /// px of that result above `at` skipped.
    pub(crate) fn start(&self, tops: &[u32], at: u32) -> Option<RowStart> {
        let end = tops.len().checked_sub(1)?;
        if at >= self.top(tops, end) {
            return None;
        }
        let (mut low, mut high) = (0, end - 1);
        while low < high {
            let middle = (low + high).div_ceil(2);
            match self.top(tops, middle) <= at {
                true => low = middle,
                false => high = middle - 1,
            }
        }
        Some(RowStart {
            result: low,
            skip: (at - self.top(tops, low)) as u16,
        })
    }

    /// The cell row that holds physical px row `y`, and the nudge in logical
    /// px that sets a line's top on `y` from that row's top.
    pub(crate) fn place(&self, y: u32) -> (u16, i16) {
        let height = self.line_height();
        let offset = (y % height) as f32 / self.scale;
        ((y / height) as u16, offset.round() as i16)
    }

    /// The sixteenths of a cell's width nearest `logical` board px.
    pub(crate) fn cols(&self, logical: u16) -> u16 {
        let width = u32::from(self.cell[0]);
        ((self.px(logical) * 16 + width / 2) / width) as u16
    }

    /// The sixteenths of a cell's height nearest `logical` board px.
    pub(crate) fn rows16(&self, logical: u16) -> u16 {
        let height = self.line_height();
        ((self.px(logical) * 16 + height / 2) / height) as u16
    }

    /// The sixteenths of a cell's height that the box of a row of `lines`
    /// lines takes.
    pub(crate) fn box16(&self, lines: u16) -> u16 {
        lines * 16 + self.rows16(2 * ROW_PAD)
    }

    fn line_height(&self) -> u32 {
        u32::from(self.cell[1])
    }
}

impl Placer {
    /// The one-row rect of `area` that holds physical px row `y`, after the
    /// nudge that sets a line's top on `y`.
    pub(crate) fn line(
        &mut self,
        metrics: &RowMetrics,
        area: Rect,
        y: u32,
        scene: &mut ApcScene,
    ) -> Rect {
        let (row, dy) = metrics.place(y);
        if dy != self.nudge {
            scene.nudge([0, dy]);
            self.nudge = dy;
        }
        Rect::new(area.x, area.y + row, area.width, 1)
    }

    /// Return the decorations after the list to no nudge.
    pub(crate) fn end(&mut self, scene: &mut ApcScene) {
        if self.nudge != 0 {
            scene.nudge([0, 0]);
            self.nudge = 0;
        }
    }
}

/// Draw the cells `cells` of row `row` of `painted` as text runs, as `at`
/// places them.
///
/// Each longest stretch of cells with one foreground, weight, and slant is one
/// run with no background, which starts at `at.col` plus the advance of the
/// cells before it. The blank cells at the ends of a stretch draw nothing, and
/// a stretch of blanks draws no run. A wide cell ends its run, since a run
/// advances a char a cell.
pub(crate) fn emit_cells(
    painted: &Buffer,
    row: u16,
    cells: Range<u16>,
    at: &RunsAt,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let face = |modifier: Modifier| modifier & (Modifier::BOLD | Modifier::ITALIC);
    let mut text = String::new();
    let mut x = cells.start;
    while x < cells.end {
        let head = &painted[(x, row)];
        let (fg, modifier) = (head.fg, face(head.modifier));
        let mut first = None;
        let mut kept = 0;
        text.clear();
        while x < cells.end {
            let cell = &painted[(x, row)];
            if cell.fg != fg || face(cell.modifier) != modifier {
                break;
            }
            x += 1;
            // The cell after a wide char holds no symbol of its own, and a
            // blank before the first char starts no run.
            let symbol = cell.symbol();
            if symbol.is_empty() || (symbol == " " && first.is_none()) {
                continue;
            }
            first.get_or_insert(x - 1);
            text.push_str(symbol);
            if symbol != " " {
                kept = text.len();
            }
            if symbol.width() > 1 {
                break;
            }
        }
        text.truncate(kept);
        let Some(first) = first.filter(|_| !text.is_empty()) else {
            continue;
        };

        let offset = text_run::advance_sixteenths(usize::from(first - cells.start), at.scale);
        TextRun {
            col: (at.col + offset) as i16,
            row: 0,
            scale: at.scale,
            color: style_rgb(Some(fg)).unwrap_or(at.fallback),
            bg: None,
            bold: modifier.contains(Modifier::BOLD),
            italic: modifier.contains(Modifier::ITALIC),
            follow: 0,
            anchor: None,
            text: &text,
        }
        .render(at.anchor, buf, scene);
    }
}

/// Draw `spans` as text runs on the line `anchor` from `col` sixteenths at
/// `scale`, and answer the sixteenth that the last span's advance ends at.
///
/// Each span is one run with no background, which starts at `col` plus the
/// advance of the chars before it, in its style's foreground, weight, and
/// slant. A foreground with no RGB value takes `fallback`, and a blank span
/// draws no run.
pub(crate) fn emit_spans<S: AsRef<str>>(
    spans: &[(S, Style)],
    col: u16,
    scale: u16,
    anchor: Rect,
    fallback: [u8; 3],
    buf: &mut Buffer,
    scene: &mut ApcScene,
) -> u16 {
    let mut chars = 0;
    for (text, style) in spans {
        let text = text.as_ref();
        if !text.trim().is_empty() {
            let start = col.saturating_add(text_run::advance_sixteenths(chars, scale));
            TextRun {
                col: i16::try_from(start).unwrap_or(i16::MAX),
                row: 0,
                scale,
                color: style_rgb(style.fg).unwrap_or(fallback),
                bg: None,
                bold: style.add_modifier.contains(Modifier::BOLD),
                italic: style.add_modifier.contains(Modifier::ITALIC),
                follow: 0,
                anchor: None,
                text,
            }
            .render(anchor, buf, scene);
        }
        chars += text.chars().count();
    }
    col.saturating_add(text_run::advance_sixteenths(chars, scale))
}

/// Draw `bar` on the line of `area` that holds its top pixel row, under the
/// nudge that sets its top there.
pub(crate) fn bar(
    placer: &mut Placer,
    metrics: &RowMetrics,
    area: Rect,
    bar: &PixelBar,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let line = placer.line(metrics, area, bar.y, scene);
    Bar {
        x: bar.x,
        y: 0,
        width: bar.width,
        height: bar.height,
        color: bar.color,
        radius: bar.radius,
        corners: 0,
    }
    .render(line, buf, scene);
}

#[cfg(test)]
mod tests {
    use super::{bar, emit_cells, emit_spans, PixelBar, Placer, RowMetrics, RunsAt};
    use crate::{
        render::picker::RowStart,
        theme::{Theme, FULL_CELL_SCALE},
    };
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier, Style},
    };
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{self, BarCommand, Command, TextRunCommand};

    const M1: RowMetrics = RowMetrics {
        cell: [9, 18],
        scale: 1.0,
    };
    const M2: RowMetrics = RowMetrics {
        cell: [18, 36],
        scale: 2.0,
    };

    /// The commands that `scene` holds, without the ends of its text runs.
    fn commands(scene: &ApcScene) -> Vec<Command> {
        command::decode_stream(scene.bytes())
            .into_iter()
            .filter(|command| *command != Command::TextRunEnd)
            .collect()
    }

    fn run(col: i16, text: &str, color: [u8; 3], bold: bool) -> Command {
        Command::TextRun(TextRunCommand {
            col,
            row: 80,
            scale: FULL_CELL_SCALE,
            color,
            bg: None,
            bold,
            italic: false,
            follow: 0,
            anchor: None,
            text: text.to_owned(),
        })
    }

    /// The runs of the first `rows` rows of `painted`, anchored on the cell row
    /// 5 from cell 3, with the fallback color 9, 9, 9.
    fn runs_of(painted: &Buffer, rows: u16) -> Vec<Command> {
        let at = RunsAt {
            col: 0,
            scale: FULL_CELL_SCALE,
            anchor: Rect::new(3, 5, 8, 1),
            fallback: [9, 9, 9],
        };
        let mut scene = ApcScene::new();
        for row in 0..rows {
            emit_cells(
                painted,
                row,
                0..8,
                &at,
                &mut Buffer::empty(Rect::default()),
                &mut scene,
            );
        }
        commands(&scene)
    }

    /// A list lays its rows out by pixels only under the plate look, which a
    /// scene that reads nudges and a theme with a ground and a surface give,
    /// and only once the host has stated its cell and scale.
    #[test]
    fn a_list_lays_out_by_pixels_only_under_the_plate_look() {
        let src = r##"theme t {
            ui.surface.ground.bg = "#0c0e11";
            ui.background.bg = "#17191e";
        }"##;
        let (config, _) = stoat_config::parse(src);
        let theme = Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
        let mut scene = ApcScene::new();
        scene.set_pixel_rows(Some(([9, 18], 1.0)));
        let flat = RowMetrics::of(&theme, &scene);
        scene.set_nudge_protocol(true);
        let plate = RowMetrics::of(&theme, &scene);
        scene.set_pixel_rows(None);

        assert_eq!(
            [flat, plate, RowMetrics::of(&theme, &scene)],
            [None, Some(M1), None]
        );
    }

    #[test]
    fn a_row_is_two_pixels_higher_than_its_lines_and_one_pixel_from_the_next() {
        let sizes = |m: RowMetrics| [m.box_height(1), m.pitch(1), m.pitch(2), m.extent(27)];

        assert_eq!(
            [sizes(M1), sizes(M2)],
            [[20, 21, 39, 487], [40, 42, 78, 974]]
        );
    }

    #[test]
    fn a_pixel_offset_splits_into_a_cell_row_and_a_nudge() {
        assert_eq!(
            [M1.place(0), M1.place(22), M1.place(275), M2.place(42)],
            [(0, 0), (1, 4), (15, 5), (1, 3)]
        );
    }

    #[test]
    fn tops_in_pixels_add_a_pad_and_a_gap_for_each_result() {
        let tops = [0, 1, 3, 4];
        let start = |result, skip| Some(RowStart { result, skip });

        assert_eq!(
            (
                [0, 1, 2, 3].map(|result| M1.top(&tops, result)),
                [30, 80, 81].map(|at| M1.start(&tops, at)),
            ),
            ([0, 21, 60, 81], [start(1, 9), start(2, 20), None])
        );
    }

    #[test]
    fn a_pixel_size_rounds_to_sixteenths_of_the_cell() {
        assert_eq!(
            (
                [1, 3, 8, 11].map(|px| M1.cols(px)),
                [2, 6, 15, 16, 20].map(|px| M1.rows16(px)),
            ),
            ([2, 5, 14, 20], [2, 5, 13, 14, 18])
        );
    }

    /// `ab` from cell 1 and `cd` from cell 4 draw 4 and 7 cells right of the
    /// anchor's left sixteenth 48, and the blank second row draws nothing.
    #[test]
    fn cells_of_one_style_draw_as_one_run() {
        let mut painted = Buffer::empty(Rect::new(0, 0, 8, 2));
        let bold = Style::default()
            .fg(Color::Rgb(1, 2, 3))
            .add_modifier(Modifier::BOLD);
        painted.set_string(1, 0, "ab", bold);
        painted.set_string(4, 0, "cd", Style::default().fg(Color::Rgb(4, 5, 6)));

        assert_eq!(
            runs_of(&painted, 2),
            [
                run(64, "ab", [1, 2, 3], true),
                run(112, "cd", [4, 5, 6], false)
            ]
        );
    }

    /// The cell after a wide char takes its style and starts no text of the
    /// run, so `x` draws from its own cell 3, and the `y` of a plain foreground
    /// draws in the fallback.
    #[test]
    fn a_wide_cell_ends_its_run_and_a_plain_foreground_takes_the_fallback() {
        let mut painted = Buffer::empty(Rect::new(0, 0, 8, 1));
        let style = Style::default().fg(Color::Rgb(1, 2, 3));
        painted.set_string(1, 0, "\u{5b57}x", style);
        painted[(2, 0)].set_style(style);
        painted.set_string(5, 0, "y", Style::default());

        assert_eq!(
            runs_of(&painted, 1),
            [
                run(64, "\u{5b57}", [1, 2, 3], false),
                run(96, "x", [1, 2, 3], false),
                run(128, "y", [9, 9, 9], false),
            ]
        );
    }

    /// Each span draws one run from the advance of the chars before it, in its
    /// own colors, and a blank span draws none. At the scale of 232 a char
    /// advances 14.5 sixteenths, so the runs after `ab` start where the
    /// rounded advance of 3 and 5 chars ends. A foreground with no RGB value
    /// takes the fallback, and the answer is where the advance of all 6 chars
    /// ends.
    #[test]
    fn spans_draw_one_run_each_after_the_chars_before_them() {
        let bold = Style::default()
            .fg(Color::Rgb(1, 2, 3))
            .add_modifier(Modifier::BOLD);
        let spans = [
            ("ab", bold),
            (" ", Style::default()),
            ("cd", Style::default().fg(Color::Rgb(4, 5, 6))),
            ("e", Style::default()),
        ];
        let mut scene = ApcScene::new();
        let end = emit_spans(
            &spans,
            8,
            232,
            Rect::new(0, 5, 8, 1),
            [9, 9, 9],
            &mut Buffer::empty(Rect::default()),
            &mut scene,
        );
        let at_232 = |col, text, color, bold| match run(col, text, color, bold) {
            Command::TextRun(run) => Command::TextRun(TextRunCommand { scale: 232, ..run }),
            command => command,
        };

        assert_eq!(
            (commands(&scene), end),
            (
                vec![
                    at_232(8, "ab", [1, 2, 3], true),
                    at_232(52, "cd", [4, 5, 6], false),
                    at_232(81, "e", [9, 9, 9], false),
                ],
                95,
            )
        );
    }

    /// Pixel row 43 is 7 px into cell row 2 of the area, so the bar draws on
    /// row 7 under a nudge of 7.
    #[test]
    fn a_bar_draws_under_the_nudge_of_its_pixel_row() {
        let mut scene = ApcScene::new();
        let spec = PixelBar {
            x: 2,
            y: 43,
            width: 20,
            height: 18,
            color: [7, 8, 9],
            radius: 4,
        };
        let area = Rect::new(3, 5, 8, 4);
        let mut buf = Buffer::empty(Rect::default());
        bar(
            &mut Placer::default(),
            &M1,
            area,
            &spec,
            &mut buf,
            &mut scene,
        );

        assert_eq!(
            commands(&scene),
            [
                Command::Nudge([0, 7]),
                Command::Bar(BarCommand {
                    x: 50,
                    y: 112,
                    width: 20,
                    height: 18,
                    color: [7, 8, 9],
                    occludes: false,
                    hatch: None,
                    radius: 4,
                    corners: 0,
                }),
            ]
        );
    }
}
