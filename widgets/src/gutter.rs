use crate::{
    bar::Bar,
    cells,
    text_run::{self, TextRun},
    ApcScene,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style, widgets::StatefulWidget};

/// Sixteenths between the line number's right edge and the separator hairline.
///
/// A quarter cell, twice the inter-bar pad, so every gutter gap stays a clean
/// division of the cell regardless of digit count.
const NUMBER_GAP: u16 = 4;

/// Sixteenths between the separator hairline and the editor body text.
///
/// Matches [`NUMBER_GAP`] so the hairline reads centered in balanced gaps.
const TEXT_GAP: u16 = 4;

/// An editor gutter composed of sub-cell line numbers, status bars, and a
/// separator.
///
/// The showcase composite: it packs smaller-than-grid line numbers (fractional
/// [`TextRun`]s), thin git and diagnostic [`Bar`]s, and a hairline separator into
/// a few columns off the cell grid, and writes a degraded cell gutter (a
/// right-aligned line number and a one-column severity mark) into the buffer for
/// any other terminal.
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
    /// Diagnostic bar width, in sixteenths.
    pub bar_width: u16,
    /// How many diff bar slots sit right of the number, one to three.
    pub diff_bars: u16,
    /// Each diff bar's width, in sixteenths.
    pub diff_bar_width: u16,
    /// Inter-element padding, in sixteenths.
    pub pad: u16,
    /// Line-number glyph size, in 256ths of a cell.
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
    /// resolved through the layout, so a diagnostic spanning an expanded line
    /// needs the emitter to measure it.
    pub bound_to_line_layout: bool,
}

/// A line's diff marks, one bar color per slot right of the line number, and
/// whether the change is a deletion seam.
///
/// The gutter reads the first [`Gutter::diff_bars`] slots. Slots 0 and 2 carry
/// change kinds. A seam draws each of those as a short top-aligned bar, since a
/// deletion occupies no line of its own and marks the row that now sits below
/// the removed content, while a normal change fills the code row. Slot 1
/// carries a state and always spans the line height.
#[derive(Clone, Copy)]
pub struct GitMark {
    /// The bar color per slot, left to right, `None` for a slot that paints
    /// nothing on this row.
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
    /// Diagnostic severity bar spanning the line's full [`Self::height`], or
    /// `None` for a line without one.
    pub diagnostic: Option<Diagnostic>,
}

/// A line's diagnostic: the bar color and the cell-fallback severity letter.
#[derive(Clone, Copy)]
pub struct Diagnostic {
    pub color: [u8; 3],
    pub mark: char,
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
    /// Sized to fit the diagnostic bar, the number column, the diff bars right
    /// of the number, and a quarter-cell gap on each side of the hairline.
    /// [`Self::separator_x`] and [`Self::number_right_edge`] derive backward
    /// from this, so the rounding slack lands in the blank field left of the
    /// right-aligned numbers rather than around the bars.
    pub fn cell_width(&self) -> u16 {
        (self.bar_width
            + self.diff_bars_span()
            + self.number_advance(usize::from(self.width_digits))
            + 2 * NUMBER_GAP
            + 1
            + TEXT_GAP)
            .div_ceil(16)
    }

    /// Diff bar `slot`'s left edge, in sixteenths from the gutter's left edge.
    pub fn bar_x(&self, slot: u16) -> u16 {
        self.number_right_edge() + NUMBER_GAP + slot * (self.diff_bar_width + self.pad)
    }

    /// Sixteenths the diff bars and the pads between them span.
    fn diff_bars_span(&self) -> u16 {
        self.diff_bars * self.diff_bar_width + self.diff_bars.saturating_sub(1) * self.pad
    }

    /// Sixteenths a run of `digits` numerals advances at [`Self::number_scale`].
    fn number_advance(&self, digits: usize) -> u16 {
        text_run::advance_sixteenths(digits, self.number_scale)
    }

    /// Where a right-aligned line number ends, in sixteenths from the gutter's left edge.
    pub fn number_right_edge(&self) -> u16 {
        self.separator_x() - 2 * NUMBER_GAP - self.diff_bars_span()
    }

    /// The separator hairline's left edge, in sixteenths from the gutter's left edge.
    pub fn separator_x(&self) -> u16 {
        self.cell_width() * 16 - 1 - TEXT_GAP
    }

    fn total_rows(&self) -> u16 {
        self.lines.iter().map(|line| line.height).sum()
    }

    /// Draw only the off-grid components (sub-cell numbers, bars, separator).
    ///
    /// An app that composites rich chrome itself calls this instead of the
    /// [`StatefulWidget`] render, which also lays down the degraded cell gutter
    /// and would double under the components inside a rich terminal.
    pub fn draw_components(&self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        let number_right = self.number_right_edge();

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
            TextRun {
                col,
                row,
                scale: self.number_scale,
                color: self.number_fg,
                bg: Some(self.bg),
                text,
                follow: 0,
                anchor: None,
            }
            .render(area, buf, scene);

            if let Some(diag) = line.diagnostic {
                Bar {
                    x: 0,
                    y: row,
                    width: self.bar_width,
                    height: cells::span_sixteenths(line.height).min(remaining),
                    color: diag.color,
                }
                .render(area, buf, scene);
            }
            if let Some(git) = line.git {
                let slots = (0u16..).zip(git.slots).take(usize::from(self.diff_bars));
                for (slot, color) in slots {
                    let Some(color) = color else {
                        continue;
                    };
                    let height = match slot {
                        1 => cells::span_sixteenths(line.height),
                        _ if git.seam => 6,
                        _ => 16,
                    };
                    Bar {
                        x: cells::signed_sixteenths(self.bar_x(slot)),
                        y: row,
                        width: self.diff_bar_width,
                        height: height.min(remaining),
                        color,
                    }
                    .render(area, buf, scene);
                }
            }

            top += line.height;
            logical = logical.saturating_add(1);
        }

        Bar {
            x: cells::signed_sixteenths(self.separator_x()),
            y: 0,
            width: 1,
            height: cells::span_sixteenths(self.total_rows().min(area.height)),
            color: self.separator,
        }
        .render(area, buf, scene);
    }

    /// Draw only the degraded cell gutter (a right-aligned number and a
    /// one-column severity mark) into the buffer, for a terminal without the
    /// off-grid components.
    pub fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
        let width = self.cell_width();
        if width == 0 {
            return;
        }

        let mut top = 0u16;
        for line in self.lines {
            if top >= area.height {
                break;
            }
            let y = area.y + top;

            if let Some(diag) = line.diagnostic {
                let mut mark = [0u8; 4];
                cells::put(
                    buf,
                    area.x,
                    y,
                    diag.mark.encode_utf8(&mut mark),
                    Style::default().fg(cells::rgb(diag.color)),
                );
            }

            let mut digits = [0u8; 10];
            let text = format_u32(&mut digits, line.number);
            let start = (area.x + width)
                .saturating_sub(text.len() as u16)
                .max(area.x + 1);
            let max = (area.x + width).saturating_sub(start) as usize;
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
    use stoatty_protocol::command::{encode_bar, encode_text_run, BarCommand, TextRunCommand};

    fn config(lines: &[GutterLine]) -> Gutter<'_> {
        Gutter {
            lines,
            bar_width: 5,
            diff_bars: 3,
            diff_bar_width: 3,
            pad: 2,
            number_scale: 160,
            width_digits: 2,
            number_fg: [99, 109, 131],
            separator: [60, 66, 77],
            bg: [40, 44, 52],
            bound_to_line_layout: false,
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn cell_width_derives_from_the_sixteenth_layout() {
        // number_advance(2) = 2*160/16 = 20; content = 5 + 9 + 4 + 20 + 8 + 1 + 4 = 51
        // (bar + 3 diff bars + 2 pads + number + 2*NUMBER_GAP + separator + TEXT_GAP);
        // cell_width = ceil(51/16) = 4.
        assert_eq!(config(&[]).cell_width(), 4);
    }

    #[test]
    fn the_offsets_derive_backward_from_the_cell_width() {
        // separator_x = 4*16 - 1 - TEXT_GAP = 59;
        // number_right_edge = 59 - 2*NUMBER_GAP - (3 diff bars + 2 pads) = 59 - 8 - 13 = 38;
        // bar_x(slot) = 38 + NUMBER_GAP + slot * (diff bar + pad) = 42, 47, 52.
        let gutter = config(&[]);
        assert_eq!(
            (
                gutter.number_right_edge(),
                [0, 1, 2].map(|slot| gutter.bar_x(slot)),
                gutter.separator_x(),
            ),
            (38, [42, 47, 52], 59),
        );
    }

    /// The plain editor's gutter draws one diff bar, so the number moves right
    /// to sit a gap from it, and a slot past the bar count paints nothing.
    #[test]
    fn a_one_bar_gutter_packs_the_number_against_its_single_slot() {
        // content = 5 + 5 + 20 + 8 + 1 + 4 = 43, so cell_width = ceil(43/16) = 3;
        // separator_x = 3*16 - 1 - TEXT_GAP = 43; bar_x(0) = 43 - 8 - 5 + NUMBER_GAP = 34.
        let lines = [GutterLine {
            number: 1,
            height: 1,
            git: Some(GitMark {
                slots: [Some([152, 195, 121]), Some([80, 90, 100]), None],
                seam: false,
            }),
            diagnostic: None,
        }];
        let gutter = Gutter {
            diff_bars: 1,
            diff_bar_width: 5,
            ..config(&lines)
        };
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        gutter.draw_components(area, &mut buf, &mut scene);

        let bar_at = |x, color| {
            encode_bar(&BarCommand {
                x,
                y: 0,
                width: 5,
                height: 16,
                color,
            })
        };
        assert_eq!(
            (
                (gutter.cell_width(), gutter.separator_x(), gutter.bar_x(0)),
                contains(scene.buffer(), &bar_at(34, [152, 195, 121])),
                contains(scene.buffer(), &bar_at(41, [80, 90, 100])),
            ),
            ((3, 43, 34), true, false),
            "the width, the hairline and the bar, then slot 0 drawn and slot 1 not",
        );
    }

    #[test]
    fn fallback_draws_mark_and_right_aligned_number() {
        let lines = [GutterLine {
            number: 7,
            height: 1,
            git: None,
            diagnostic: Some(Diagnostic {
                color: [224, 108, 117],
                mark: 'E',
            }),
        }];
        let gutter = config(&lines);
        let area = Rect::new(0, 0, gutter.cell_width(), 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        gutter.render(area, &mut buf, &mut scene);

        assert_eq!(buf.cell((0u16, 0u16)).expect("cell").symbol(), "E");
        assert_eq!(buf.cell((3u16, 0u16)).expect("cell").symbol(), "7");
    }

    #[test]
    fn components_emit_separator_and_diagnostic_bar() {
        let lines = [GutterLine {
            number: 1,
            height: 1,
            git: None,
            diagnostic: Some(Diagnostic {
                color: [224, 108, 117],
                mark: 'E',
            }),
        }];
        let gutter = config(&lines);
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        gutter.render(area, &mut buf, &mut scene);

        let separator = encode_bar(&BarCommand {
            x: 59,
            y: 0,
            width: 1,
            height: 16,
            color: [60, 66, 77],
        });
        let diag_bar = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 5,
            height: 16,
            color: [224, 108, 117],
        });
        assert!(contains(scene.buffer(), &separator), "separator bar frame");
        assert!(contains(scene.buffer(), &diag_bar), "diagnostic bar frame");
    }

    #[test]
    fn diagnostic_bar_spans_the_line_height() {
        let lines = [GutterLine {
            number: 1,
            height: 2,
            git: Some(GitMark {
                slots: [Some([152, 195, 121]), Some([80, 90, 100]), None],
                seam: false,
            }),
            diagnostic: Some(Diagnostic {
                color: [224, 108, 117],
                mark: 'E',
            }),
        }];
        let gutter = config(&lines);
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        gutter.render(area, &mut buf, &mut scene);

        let diag_bar = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 5,
            height: 32,
            color: [224, 108, 117],
        });
        let git_bar = encode_bar(&BarCommand {
            x: 42,
            y: 0,
            width: 3,
            height: 16,
            color: [152, 195, 121],
        });
        let staged_bar = encode_bar(&BarCommand {
            x: 47,
            y: 0,
            width: 3,
            height: 32,
            color: [80, 90, 100],
        });
        assert!(
            contains(scene.buffer(), &diag_bar),
            "diagnostic bar spans the two-row line"
        );
        assert!(
            contains(scene.buffer(), &git_bar),
            "change-kind bar marks only the code row"
        );
        assert!(
            contains(scene.buffer(), &staged_bar),
            "staged-state bar spans the full line height"
        );
    }

    /// The surface picks one placement model for every component on it, so the
    /// two models must not both fold the same expansion.
    #[test]
    fn a_bound_gutter_emits_logical_rows() {
        let lines = [
            GutterLine {
                number: 1,
                height: 2,
                git: None,
                diagnostic: None,
            },
            GutterLine {
                number: 2,
                height: 1,
                git: None,
                diagnostic: Some(Diagnostic {
                    color: [224, 108, 117],
                    mark: 'E',
                }),
            },
        ];
        let area = Rect::new(0, 0, 10, 3);

        let bar_at = |y| {
            encode_bar(&BarCommand {
                x: 0,
                y,
                width: 5,
                height: 16,
                color: [224, 108, 117],
            })
        };
        let emit = |bound| {
            let mut gutter = config(&lines);
            gutter.bound_to_line_layout = bound;
            let mut buf = Buffer::empty(area);
            let mut scene = ApcScene::new();

            gutter.draw_components(area, &mut buf, &mut scene);
            scene.buffer().to_vec()
        };

        assert!(
            contains(&emit(false), &bar_at(32)),
            "unbound, the widget folds the expansion into a physical row"
        );
        assert!(
            contains(&emit(true), &bar_at(16)),
            "bound, the terminal folds it, so the widget emits the logical row"
        );
    }

    /// A caller sizes the gutter area from its pane, not from its line count,
    /// so more lines than rows is ordinary. ratatui panics outright on an
    /// out-of-buffer row, and the components clip nowhere at all.
    #[test]
    fn lines_below_the_area_are_dropped() {
        let lines = [1, 2, 3].map(|number| GutterLine {
            number,
            height: 1,
            git: None,
            diagnostic: None,
        });
        let gutter = config(&lines);
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        gutter.render(area, &mut buf, &mut scene);

        let third_number = encode_text_run(&TextRunCommand {
            col: 28,
            row: 32,
            scale: 160,
            color: [99, 109, 131],
            bg: Some([40, 44, 52]),
            follow: 0,
            anchor: None,
            text: "3".to_owned(),
        });
        let second_number = encode_text_run(&TextRunCommand {
            col: 28,
            row: 16,
            scale: 160,
            color: [99, 109, 131],
            bg: Some([40, 44, 52]),
            follow: 0,
            anchor: None,
            text: "2".to_owned(),
        });
        assert!(
            contains(scene.buffer(), &second_number),
            "the last line inside the area still draws"
        );
        assert!(
            !contains(scene.buffer(), &third_number),
            "the line past the bottom draws nothing"
        );

        let separator = encode_bar(&BarCommand {
            x: 59,
            y: 0,
            width: 1,
            height: 32,
            color: [60, 66, 77],
        });
        assert!(
            contains(scene.buffer(), &separator),
            "the separator spans the area, not the line total"
        );
    }

    /// Dropping only fully-below lines leaves the same overrun one row down. A
    /// tall line entered at the last row still spans its full height.
    #[test]
    fn a_line_straddling_the_bottom_clamps_its_bars() {
        let lines = [
            GutterLine {
                number: 1,
                height: 1,
                git: None,
                diagnostic: None,
            },
            GutterLine {
                number: 2,
                height: 2,
                git: Some(GitMark {
                    slots: [Some([152, 195, 121]), Some([80, 90, 100]), None],
                    seam: false,
                }),
                diagnostic: Some(Diagnostic {
                    color: [224, 108, 117],
                    mark: 'E',
                }),
            },
        ];
        let gutter = config(&lines);
        let area = Rect::new(0, 0, 10, 2);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        gutter.render(area, &mut buf, &mut scene);

        let diag_bar = encode_bar(&BarCommand {
            x: 0,
            y: 16,
            width: 5,
            height: 16,
            color: [224, 108, 117],
        });
        let staged_bar = encode_bar(&BarCommand {
            x: 47,
            y: 16,
            width: 3,
            height: 16,
            color: [80, 90, 100],
        });
        assert!(
            contains(scene.buffer(), &diag_bar),
            "the diagnostic bar stops at the area bottom"
        );
        assert!(
            contains(scene.buffer(), &staged_bar),
            "and so does the staged-state bar"
        );
    }

    /// The two change-kind slots draw a seam short, and the state slot between
    /// them keeps its full height.
    #[test]
    fn seam_git_mark_draws_a_short_top_aligned_bar() {
        let lines = [GutterLine {
            number: 4,
            height: 1,
            git: Some(GitMark {
                slots: [
                    Some([224, 108, 117]),
                    Some([80, 90, 100]),
                    Some([97, 175, 239]),
                ],
                seam: true,
            }),
            diagnostic: None,
        }];
        let gutter = config(&lines);
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        gutter.render(area, &mut buf, &mut scene);

        let bar = |x, height, color| {
            contains(
                scene.bytes(),
                &encode_bar(&BarCommand {
                    x,
                    y: 0,
                    width: 3,
                    height,
                    color,
                }),
            )
        };
        assert_eq!(
            [
                bar(42, 6, [224, 108, 117]),
                bar(47, 16, [80, 90, 100]),
                bar(52, 6, [97, 175, 239]),
            ],
            [true; 3],
            "slots 0 and 2 draw the short seam bar, and slot 1 fills the row"
        );
    }
}
