use crate::{
    bar::{Bar, BarBatch},
    cells,
    icon::{self, Icon},
    text_run::{self, TextRun, TextRunBatch},
    ApcScene,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style, widgets::StatefulWidget};
use stoatty_protocol::command::IconKind;

/// Sixteenths between the line number's right edge and the icon cell, and
/// between the structural lane and the separator hairline.
///
/// A quarter cell, so every gutter gap stays a clean division of the cell
/// regardless of digit count.
const NUMBER_GAP: u16 = 4;

/// Sixteenths between the separator hairline and the editor body text.
///
/// Matches [`NUMBER_GAP`] so the hairline reads centered in balanced gaps.
const TEXT_GAP: u16 = 4;

/// The mark a hint draws in the icon cell, since a hint has no icon of its own.
const HINT_DOT: &str = "\u{b7}";

/// An editor gutter composed of git lanes, sub-cell line numbers, diagnostic
/// icons, and a separator.
///
/// The widget is the showcase composite. It packs smaller-than-grid line
/// numbers (fractional [`TextRun`]s), thin git [`Bar`]s, diagnostic [`Icon`]s,
/// and a hairline separator into a few columns off the cell grid. For any other
/// terminal, it writes a degraded cell gutter (a right-aligned line number and
/// a one-column severity mark) into the buffer.
///
/// Left to right, the gutter holds the hunk and state lanes, the right-aligned
/// number, the icon cell, the structural lane, and the hairline. The icon cell
/// is a whole grid cell, because an icon anchors to a cell.
///
/// The geometry knobs are in **sixteenths of a cell** (16 = one cell), so the
/// gutter tracks live font zoom. The widget positions every element at an
/// absolute sixteenth offset to the render area.
///
/// Both halves clip to the area. Lines below its last row draw nothing, and a
/// line straddling the bottom keeps only the rows that fit, so a caller sizes
/// the area from its pane and passes however many lines it holds.
pub struct Gutter<'a> {
    /// The lines to draw, top to bottom.
    pub lines: &'a [GutterLine],
    /// How many git lanes sit left of the number, zero to two: the hunk lane,
    /// then the state lane.
    pub left_bars: u16,
    /// Each left lane's width, in sixteenths.
    pub left_bar_width: u16,
    /// How many structural lanes sit right of the icon cell, zero or one.
    pub diff_bars: u16,
    /// Each structural lane's width, in sixteenths.
    pub diff_bar_width: u16,
    /// Padding between adjacent lanes, in sixteenths.
    pub pad: u16,
    /// Line-number glyph size, in 256ths of a cell. A hint's dot draws at it
    /// too.
    pub number_scale: u16,
    /// Digit count of the widest line number, sizing the number column.
    pub width_digits: u16,
    pub number_fg: [u8; 3],
    pub separator: [u8; 3],
    /// Background the sub-cell runs composite over; must match the editor body.
    pub bg: [u8; 3],
    /// Whether the surface declares a line layout that binds the components.
    ///
    /// The protocol carries two ways to place a component under an inline
    /// expansion, and the surface picks one for every component on it. With a
    /// layout declared through [`crate::ApcScene::set_line_layout`], the widget
    /// emits each line at its logical row and the terminal folds in the
    /// expansions above it. Without one, the widget folds the heights itself
    /// and emits physical rows.
    ///
    /// [`Self::lines`] carries the heights either way. A bar's height is never
    /// resolved through the layout, so a state bar spanning an expanded line
    /// needs the emitter to measure it. The terminal folds no icon, so an icon
    /// takes its physical row in both models.
    pub bound_to_line_layout: bool,
}

/// A line's git marks, one bar color per lane, and whether the change is a
/// deletion seam.
///
/// Each slot has a fixed lane. Slot 0 is the hunk kind and slot 1 the state,
/// both left of the number. Slot 2 is the structural change, right of the icon
/// cell. A gutter draws a slot only where it reserves the slot's lane, per
/// [`Gutter::left_bars`] and [`Gutter::diff_bars`].
///
/// A seam draws the two change slots as short top-aligned bars, since a
/// deletion occupies no line of its own and marks the row that now sits below
/// the removed content, while a normal change fills the code row. The state
/// slot always spans the line height.
#[derive(Clone, Copy)]
pub struct GitMark {
    /// The bar color per slot, `None` for a slot that paints nothing on this
    /// row.
    pub slots: [Option<[u8; 3]>; 3],
    pub seam: bool,
}

/// One gutter line: its number, the rows it occupies, and its status marks.
#[derive(Clone, Copy)]
pub struct GutterLine {
    pub number: u32,
    /// Rows the line occupies: one, plus any inline-expansion rows beneath it.
    pub height: u16,
    /// Git-diff mark, or `None` for an unchanged line.
    pub git: Option<GitMark>,
    /// The diagnostic drawn in the icon cell of the line's first row, or
    /// `None` for a line without one.
    pub diagnostic: Option<Diagnostic>,
}

/// A line's diagnostic: the mark color and the icon kind.
///
/// `kind` is `None` for a hint, which has no icon and draws a small dot.
#[derive(Clone, Copy)]
pub struct Diagnostic {
    pub color: [u8; 3],
    pub kind: Option<IconKind>,
}

impl StatefulWidget for Gutter<'_> {
    type State = ApcScene;

    fn render(self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        self.draw_fallback(area, buf);
        self.draw_components(area, buf, scene);
    }
}

impl Gutter<'_> {
    /// The whole-cell columns the gutter reserves, from its sixteenth layout.
    ///
    /// The cells before the icon cell fit the left lanes, the number column,
    /// and a quarter-cell gap. The cells after it fit the structural lanes and
    /// a quarter-cell gap on each side of the hairline. Each part rounds up on
    /// its own, so the slack lands left of the right-aligned numbers and left
    /// of the structural lanes rather than around the hairline.
    pub fn cell_width(&self) -> u16 {
        let tail = self.diff_bars_span() + NUMBER_GAP + 1 + TEXT_GAP;
        self.icon_x() / 16 + 1 + tail.div_ceil(16)
    }

    /// The icon cell's left edge, in sixteenths from the gutter's left edge.
    ///
    /// Always a whole cell, because an icon anchors to a grid cell.
    pub fn icon_x(&self) -> u16 {
        let lead = self.left_bars_span()
            + self.number_advance(usize::from(self.width_digits))
            + NUMBER_GAP;
        lead.div_ceil(16) * 16
    }

    /// Slot `slot`'s left edge, in sixteenths from the gutter's left edge.
    ///
    /// Slots 0 and 1 are the hunk and state lanes at the gutter's left edge.
    /// Slot 2 is the structural lane, right of the icon cell.
    pub fn bar_x(&self, slot: u16) -> u16 {
        match slot {
            0 | 1 => slot * (self.left_bar_width + self.pad),
            _ => self.separator_x() - NUMBER_GAP - self.diff_bars_span(),
        }
    }

    /// Where a right-aligned line number ends, in sixteenths from the gutter's left edge.
    pub fn number_right_edge(&self) -> u16 {
        self.icon_x() - NUMBER_GAP
    }

    /// The separator hairline's left edge, in sixteenths from the gutter's left edge.
    pub fn separator_x(&self) -> u16 {
        self.cell_width() * 16 - 1 - TEXT_GAP
    }

    /// Whether the gutter reserves slot `slot`'s lane.
    fn reserves(&self, slot: u16) -> bool {
        match slot {
            0 | 1 => slot < self.left_bars,
            _ => self.diff_bars > 0,
        }
    }

    /// Slot `slot`'s bar width, in sixteenths.
    fn slot_width(&self, slot: u16) -> u16 {
        match slot {
            0 | 1 => self.left_bar_width,
            _ => self.diff_bar_width,
        }
    }

    /// Sixteenths the left lanes and the pad between them span.
    fn left_bars_span(&self) -> u16 {
        self.left_bars * self.left_bar_width + self.left_bars.saturating_sub(1) * self.pad
    }

    /// Sixteenths the structural lanes and the pads between them span.
    fn diff_bars_span(&self) -> u16 {
        self.diff_bars * self.diff_bar_width + self.diff_bars.saturating_sub(1) * self.pad
    }

    /// Sixteenths a run of `digits` numerals advances at [`Self::number_scale`].
    fn number_advance(&self, digits: usize) -> u16 {
        text_run::advance_sixteenths(digits, self.number_scale)
    }

    fn total_rows(&self) -> u16 {
        self.lines.iter().map(|line| line.height).sum()
    }

    /// Draw only the off-grid components (sub-cell numbers, bars, icons, and
    /// the separator).
    ///
    /// An app that composites rich chrome itself calls this instead of the
    /// [`StatefulWidget`] render, which also lays down the degraded cell
    /// gutter. Inside a rich terminal, that gutter doubles under the
    /// components.
    pub fn draw_components(&self, area: Rect, _buf: &mut Buffer, scene: &mut ApcScene) {
        let number_right = self.number_right_edge();
        // The lanes sit left of the numbers, the icon cell right of them, and
        // the structural lane and the separator right of that. Nothing
        // overlaps, so a batch that groups by style leaves what shows as it is.
        let mut numbers = TextRunBatch::default();
        let mut bars = BarBatch::default();

        let limit = cells::span_sixteenths(area.height);

        let mut top = 0u16;
        let mut logical = 0u16;
        for line in self.lines {
            // Clipping is physical in both models, because the area holds the
            // rows the expansions actually consume.
            let physical = cells::span_sixteenths(top);
            if physical >= limit {
                break;
            }
            let remaining = limit - physical;

            let row = cells::signed_sixteenths(match self.bound_to_line_layout {
                true => cells::span_sixteenths(logical),
                false => physical,
            });

            let mut digits = [0u8; 10];
            let text = format_u32(&mut digits, line.number);
            let col = cells::signed_sixteenths(
                number_right.saturating_sub(self.number_advance(text.len())),
            );
            numbers.push(
                area,
                TextRun {
                    col,
                    row,
                    scale: self.number_scale,
                    color: self.number_fg,
                    bg: Some(self.bg),
                    text,
                    follow: 0,
                    anchor: None,
                },
            );

            if let Some(diag) = line.diagnostic {
                self.draw_diagnostic(diag, area, (top, row), &mut numbers, scene);
            }
            if let Some(git) = line.git {
                for (slot, color) in (0u16..).zip(git.slots) {
                    let Some(color) = color.filter(|_| self.reserves(slot)) else {
                        continue;
                    };
                    let height = match slot {
                        1 => cells::span_sixteenths(line.height),
                        _ if git.seam => 6,
                        _ => 16,
                    };
                    bars.push(
                        area,
                        Bar {
                            x: cells::signed_sixteenths(self.bar_x(slot)),
                            y: row,
                            width: self.slot_width(slot),
                            height: height.min(remaining),
                            color,
                        },
                    );
                }
            }

            top += line.height;
            logical = logical.saturating_add(1);
        }

        bars.push(
            area,
            Bar {
                x: cells::signed_sixteenths(self.separator_x()),
                y: 0,
                width: 1,
                height: cells::span_sixteenths(self.total_rows().min(area.height)),
                color: self.separator,
            },
        );
        numbers.flush(scene);
        bars.flush(scene);
    }

    /// Draw `diag` in the icon cell of the line whose first row sits `top`
    /// rows into `area`, and whose runs draw at sixteenth row `row`.
    ///
    /// An icon command names an absolute grid cell, so the icon takes the
    /// physical row. A hint's dot is a run, so it takes `row` as the numbers
    /// do.
    fn draw_diagnostic(
        &self,
        diag: Diagnostic,
        area: Rect,
        (top, row): (u16, i16),
        numbers: &mut TextRunBatch,
        scene: &mut ApcScene,
    ) {
        let icon_x = self.icon_x();
        match diag.kind {
            Some(kind) => Icon {
                kind,
                color: diag.color,
                size: 1,
                offset: [0, 0],
            }
            .draw_components(
                Rect {
                    x: area.x + icon_x / 16,
                    y: area.y + top,
                    width: 1,
                    height: 1,
                },
                scene,
            ),
            None => {
                let advance = text_run::advance_sixteenths(1, self.number_scale);
                numbers.push(
                    area,
                    TextRun {
                        col: cells::signed_sixteenths(icon_x + 16u16.saturating_sub(advance) / 2),
                        row,
                        scale: self.number_scale,
                        color: diag.color,
                        bg: Some(self.bg),
                        text: HINT_DOT,
                        follow: 0,
                        anchor: None,
                    },
                );
            },
        }
    }

    /// Draw only the degraded cell gutter (a right-aligned number and a
    /// one-column severity mark) into the buffer, for a terminal without the
    /// off-grid components.
    ///
    /// The mark takes the icon cell, and the number ends against it.
    pub fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
        let icon_col = area.x + self.icon_x() / 16;

        let mut top = 0u16;
        for line in self.lines {
            if top >= area.height {
                break;
            }
            let y = area.y + top;

            if let Some(diag) = line.diagnostic {
                cells::put(
                    buf,
                    icon_col,
                    y,
                    diag.kind.map_or("H", icon::sigil),
                    Style::default().fg(cells::rgb(diag.color)),
                );
            }

            let mut digits = [0u8; 10];
            let text = format_u32(&mut digits, line.number);
            let start = icon_col.saturating_sub(text.len() as u16).max(area.x);
            let max = icon_col.saturating_sub(start) as usize;
            buf.set_stringn(
                start,
                y,
                text,
                max,
                Style::default()
                    .fg(cells::rgb(self.number_fg))
                    .bg(cells::rgb(self.bg)),
            );

            top += line.height;
        }
    }
}

/// Format `n` into `buf` and return the decimal string, avoiding a per-call
/// allocation in the per-row render path.
fn format_u32(buf: &mut [u8; 10], mut n: u32) -> &str {
    let mut start = buf.len();
    loop {
        start -= 1;
        buf[start] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    std::str::from_utf8(&buf[start..]).expect("ascii digits are valid utf-8")
}

#[cfg(test)]
mod tests {
    use super::{Diagnostic, GitMark, Gutter, GutterLine};
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{
        decode_stream, encode_bar, encode_icon, encode_text_run, BarCommand, Command, IconCommand,
        IconKind, TextRunCommand,
    };

    const RED: [u8; 3] = [224, 108, 117];
    const GREEN: [u8; 3] = [152, 195, 121];
    const GRAY: [u8; 3] = [80, 90, 100];
    const BLUE: [u8; 3] = [97, 175, 239];

    /// The full lane set at scale 160 and two digits. The icon cell lands at
    /// cell 2 (the lanes 7, the number 20, and the gap 4 round up to 32
    /// sixteenths), so the number ends at 28. The tail (one 3/16 lane, two
    /// gaps, and the hairline) rounds up to one cell, so the gutter is four
    /// cells wide, the hairline sits at 59, and the structural lane at 52.
    fn config(lines: &[GutterLine]) -> Gutter<'_> {
        Gutter {
            lines,
            left_bars: 2,
            left_bar_width: 3,
            diff_bars: 1,
            diff_bar_width: 3,
            pad: 1,
            number_scale: 160,
            width_digits: 2,
            number_fg: [99, 109, 131],
            separator: [60, 66, 77],
            bg: [40, 44, 52],
            bound_to_line_layout: false,
        }
    }

    fn line(number: u32, git: Option<GitMark>, diagnostic: Option<Diagnostic>) -> GutterLine {
        GutterLine {
            number,
            height: 1,
            git,
            diagnostic,
        }
    }

    fn marks(slots: [Option<[u8; 3]>; 3], seam: bool) -> Option<GitMark> {
        Some(GitMark { slots, seam })
    }

    fn error() -> Option<Diagnostic> {
        Some(Diagnostic {
            color: RED,
            kind: Some(IconKind::Error),
        })
    }

    fn bar(x: i16, y: i16, width: u16, height: u16, color: [u8; 3]) -> Vec<u8> {
        encode_bar(&BarCommand {
            x,
            y,
            width,
            height,
            color,
        })
    }

    fn icon_at(top: u16, left: u16, kind: IconKind, color: [u8; 3]) -> Vec<u8> {
        encode_icon(&IconCommand {
            top,
            left,
            kind,
            color,
            size: 1,
            offset: [0, 0],
        })
    }

    /// The scene `gutter` emits into an `area` of `rows` rows.
    fn emit(gutter: &Gutter<'_>, rows: u16) -> Vec<u8> {
        let area = Rect::new(0, 0, 10, rows);
        let mut scene = ApcScene::new();
        gutter.draw_components(area, &mut Buffer::empty(area), &mut scene);
        scene.buffer().to_vec()
    }

    /// Whether every run and bar `needle` encodes is among those `haystack`
    /// draws, a batch read as the runs or bars it stands for.
    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        let drawn = singles(haystack);
        singles(needle)
            .iter()
            .all(|command| drawn.contains(command))
    }

    /// The bars `bytes` draw, in emit order.
    fn bars(bytes: &[u8]) -> Vec<BarCommand> {
        singles(bytes)
            .into_iter()
            .filter_map(|command| match command {
                Command::Bar(bar) => Some(bar),
                _ => None,
            })
            .collect()
    }

    /// The commands `bytes` decode to, each batch expanded into the single
    /// runs or bars it stands for and the close markers dropped.
    fn singles(bytes: &[u8]) -> Vec<Command> {
        decode_stream(bytes)
            .into_iter()
            .flat_map(|command| match command {
                Command::TextRuns(batch) => batch
                    .runs
                    .into_iter()
                    .map(|(col, row, text)| {
                        Command::TextRun(TextRunCommand {
                            col,
                            row,
                            scale: batch.scale,
                            color: batch.color,
                            bg: batch.bg,
                            follow: batch.follow,
                            anchor: batch.anchor,
                            text,
                        })
                    })
                    .collect(),
                Command::Bars(batch) => batch
                    .bars
                    .into_iter()
                    .map(|(x, y, width, height)| {
                        Command::Bar(BarCommand {
                            x,
                            y,
                            width,
                            height,
                            color: batch.color,
                        })
                    })
                    .collect(),
                Command::TextRunEnd => Vec::new(),
                other => vec![other],
            })
            .collect()
    }

    /// Under the batch protocol the gutter sends one `text_runs` frame for its
    /// numbers and one `bars` frame per bar color, and they draw what the
    /// per-run frames draw.
    #[test]
    fn a_batching_host_takes_one_frame_per_style() {
        let lines =
            [1, 2].map(|number| line(number, marks([Some(GREEN), None, None], false), None));
        let emit = |batches: bool| {
            let area = Rect::new(0, 0, 10, 2);
            let mut scene = ApcScene::new();
            scene.set_batch_protocol(batches);
            config(&lines).draw_components(area, &mut Buffer::empty(area), &mut scene);
            scene.buffer().to_vec()
        };

        let batched = emit(true);
        let frames: Vec<&str> = decode_stream(&batched)
            .iter()
            .map(|command| match command {
                Command::TextRuns(_) => "text_runs",
                Command::Bars(_) => "bars",
                _ => "other",
            })
            .collect();
        assert_eq!(
            frames,
            ["text_runs", "bars", "bars"],
            "both numbers in one frame, then the hunk bars and the separator by color",
        );
        assert_eq!(singles(&batched), singles(&emit(false)));
    }

    #[test]
    fn cell_width_counts_the_lead_the_icon_cell_and_the_tail() {
        let gutter = config(&[]);
        assert_eq!(
            [
                gutter.cell_width(),
                Gutter {
                    width_digits: 4,
                    ..config(&[])
                }
                .cell_width(),
            ],
            [4, 6],
            "a 31-sixteenth lead fits two cells, and four digits (a 51-sixteenth lead) take four",
        );
    }

    #[test]
    fn the_offsets_derive_from_the_icon_cell_and_the_cell_width() {
        let gutter = config(&[]);
        assert_eq!(
            (
                gutter.icon_x(),
                gutter.number_right_edge(),
                [0, 1, 2].map(|slot| gutter.bar_x(slot)),
                gutter.separator_x(),
            ),
            (32, 28, [0, 4, 52], 59),
        );
    }

    /// The plain editor reserves no structural lane, so slot 2 paints nothing
    /// and the tail keeps one cell.
    #[test]
    fn a_gutter_without_a_structural_lane_draws_only_the_left_lanes() {
        let lines = [line(
            1,
            marks([Some(GREEN), Some(GRAY), Some(BLUE)], false),
            None,
        )];
        let gutter = Gutter {
            diff_bars: 0,
            ..config(&lines)
        };

        assert_eq!(
            (
                (gutter.cell_width(), gutter.separator_x()),
                bars(&emit(&gutter, 1)),
            ),
            (
                (4, 59),
                vec![
                    BarCommand {
                        x: 0,
                        y: 0,
                        width: 3,
                        height: 16,
                        color: GREEN,
                    },
                    BarCommand {
                        x: 4,
                        y: 0,
                        width: 3,
                        height: 16,
                        color: GRAY,
                    },
                    BarCommand {
                        x: 59,
                        y: 0,
                        width: 1,
                        height: 16,
                        color: [60, 66, 77],
                    },
                ],
            ),
        );
    }

    #[test]
    fn fallback_draws_the_mark_in_the_icon_column_after_the_number() {
        let lines = [
            line(7, None, error()),
            line(
                12,
                None,
                Some(Diagnostic {
                    color: BLUE,
                    kind: None,
                }),
            ),
        ];
        let gutter = config(&lines);
        let area = Rect::new(0, 0, gutter.cell_width(), 2);
        let mut buf = Buffer::empty(area);

        gutter.render(area, &mut buf, &mut ApcScene::new());

        let row = |y: u16| -> String {
            (0..area.width)
                .map(|x| buf.cell((x, y)).expect("cell").symbol())
                .collect()
        };
        assert_eq!([row(0), row(1)], [" 7E ", "12H "]);
    }

    #[test]
    fn components_emit_the_separator_and_the_diagnostic_icon() {
        let lines = [line(1, None, error())];
        let scene = emit(&config(&lines), 1);

        assert!(
            contains(&scene, &bar(59, 0, 1, 16, [60, 66, 77])),
            "separator bar frame"
        );
        assert!(
            contains(&scene, &icon_at(0, 2, IconKind::Error, RED)),
            "an error icon in the icon cell"
        );
    }

    /// A hint has no icon, so it draws a dot run centered in the icon cell.
    #[test]
    fn a_hint_draws_a_dot_in_the_icon_cell() {
        let lines = [line(
            1,
            None,
            Some(Diagnostic {
                color: BLUE,
                kind: None,
            }),
        )];
        let scene = emit(&config(&lines), 1);

        // The dot advances 10 sixteenths at scale 160, so it starts 3 into the
        // cell at 32.
        let dot = encode_text_run(&TextRunCommand {
            col: 35,
            row: 0,
            scale: 160,
            color: BLUE,
            bg: Some([40, 44, 52]),
            follow: 0,
            anchor: None,
            text: "\u{b7}".to_owned(),
        });
        assert_eq!(
            (
                contains(&scene, &dot),
                singles(&scene)
                    .iter()
                    .any(|command| matches!(command, Command::Icon(_))),
            ),
            (true, false),
            "the dot draws, and no icon does",
        );
    }

    #[test]
    fn the_state_lane_spans_the_line_height() {
        let lines = [GutterLine {
            height: 2,
            ..line(1, marks([Some(GREEN), Some(GRAY), None], false), error())
        }];
        let scene = emit(&config(&lines), 2);

        assert!(
            contains(&scene, &bar(0, 0, 3, 16, GREEN)),
            "the hunk bar marks only the code row"
        );
        assert!(
            contains(&scene, &bar(4, 0, 3, 32, GRAY)),
            "the state bar spans the full line height"
        );
        assert!(
            contains(&scene, &icon_at(0, 2, IconKind::Error, RED)),
            "the icon marks the line's first row"
        );
    }

    /// The surface picks one placement model for every component on it, so the
    /// two models must not both fold the same expansion.
    #[test]
    fn a_bound_gutter_emits_logical_rows() {
        let lines = [
            GutterLine {
                height: 2,
                ..line(1, None, None)
            },
            line(2, marks([Some(RED), None, None], false), error()),
        ];
        let emit = |bound| {
            let mut gutter = config(&lines);
            gutter.bound_to_line_layout = bound;
            emit(&gutter, 3)
        };
        let icon = icon_at(2, 2, IconKind::Error, RED);

        assert!(
            contains(
                &emit(false),
                &[bar(0, 32, 3, 16, RED), icon.clone()].concat()
            ),
            "unbound, the widget folds the expansion into a physical row"
        );
        assert!(
            contains(&emit(true), &[bar(0, 16, 3, 16, RED), icon].concat()),
            "bound, the terminal folds the bar, and the icon keeps its physical cell"
        );
    }

    /// A caller sizes the gutter area from its pane, not from its line count,
    /// so more lines than rows is ordinary. ratatui panics outright on an
    /// out-of-buffer row, and the components clip nowhere at all.
    #[test]
    fn lines_below_the_area_are_dropped() {
        let lines = [1, 2, 3].map(|number| line(number, None, None));
        let scene = emit(&config(&lines), 2);

        let number_at = |row, text: &str| {
            encode_text_run(&TextRunCommand {
                col: 18,
                row,
                scale: 160,
                color: [99, 109, 131],
                bg: Some([40, 44, 52]),
                follow: 0,
                anchor: None,
                text: text.to_owned(),
            })
        };
        assert!(
            contains(&scene, &number_at(16, "2")),
            "the last line inside the area still draws"
        );
        assert!(
            !contains(&scene, &number_at(32, "3")),
            "the line past the bottom draws nothing"
        );
        assert!(
            contains(&scene, &bar(59, 0, 1, 32, [60, 66, 77])),
            "the separator spans the area, not the line total"
        );
    }

    /// Dropping only fully-below lines leaves the same overrun one row down. A
    /// tall line entered at the last row still spans its full height.
    #[test]
    fn a_line_straddling_the_bottom_clamps_its_bars() {
        let lines = [
            line(1, None, None),
            GutterLine {
                height: 2,
                ..line(2, marks([Some(GREEN), Some(GRAY), None], false), error())
            },
        ];
        let scene = emit(&config(&lines), 2);

        assert!(
            contains(&scene, &bar(4, 16, 3, 16, GRAY)),
            "the state bar stops at the area bottom"
        );
        assert!(
            contains(&scene, &icon_at(1, 2, IconKind::Error, RED)),
            "the icon still marks the line's first row"
        );
    }

    /// The two change-kind slots draw a seam short, and the state slot between
    /// them keeps its full height.
    #[test]
    fn seam_git_mark_draws_a_short_top_aligned_bar() {
        let lines = [line(
            4,
            marks([Some(RED), Some(GRAY), Some(BLUE)], true),
            None,
        )];
        let scene = emit(&config(&lines), 1);

        assert_eq!(
            [
                contains(&scene, &bar(0, 0, 3, 6, RED)),
                contains(&scene, &bar(4, 0, 3, 16, GRAY)),
                contains(&scene, &bar(52, 0, 3, 6, BLUE)),
            ],
            [true; 3],
            "slots 0 and 2 draw the short seam bar, and slot 1 fills the row"
        );
    }
}
