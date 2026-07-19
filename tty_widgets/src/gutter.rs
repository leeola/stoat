use crate::{bar::Bar, cells, text_run::TextRun, ApcScene};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::StatefulWidget,
};

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
/// absolute sixteenth offset to the render area and folds each line's row
/// `height` in itself, so it declares no surface line layout.
pub struct Gutter<'a> {
    /// The lines to draw, top to bottom.
    pub lines: &'a [GutterLine],
    /// Color-bar width, in sixteenths.
    pub bar_width: u16,
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
}

/// A line's git-diff marks: the change-kind bar color, the staged-state bar
/// color, and whether the change is a deletion seam.
///
/// Two bars sit right of the line number. The change-kind bar takes [`Self::color`];
/// a seam renders it as a short top-aligned bar, since a deletion occupies no
/// line of its own and marks the row that now sits below the removed content,
/// while a normal change fills the row height. The staged-state bar takes
/// [`Self::staged_color`] and always fills the row height.
#[derive(Clone, Copy)]
pub struct GitMark {
    pub color: [u8; 3],
    pub staged_color: [u8; 3],
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
    /// Sized to fit the diagnostic bar, the number column, the change-kind and
    /// staged-state bars right of the number, and a quarter-cell gap on each
    /// side of the hairline. [`Self::separator_x`] and [`Self::number_right_edge`]
    /// derive backward from this, so the rounding slack lands in the blank field
    /// left of the right-aligned numbers rather than around the bars.
    pub fn cell_width(&self) -> u16 {
        (3 * self.bar_width
            + self.pad
            + self.number_advance(self.width_digits)
            + 2 * NUMBER_GAP
            + 1
            + TEXT_GAP)
            .div_ceil(16)
    }

    /// The change-kind bar's left edge, a gap right of the line number.
    fn git_x(&self) -> u16 {
        self.number_right_edge() + NUMBER_GAP
    }

    /// The staged-state bar's left edge, a pad right of the change-kind bar.
    fn staged_x(&self) -> u16 {
        self.git_x() + self.bar_width + self.pad
    }

    /// Sixteenths a run of `digits` numerals advances at [`Self::number_scale`].
    fn number_advance(&self, digits: u16) -> u16 {
        digits * self.number_scale / 16
    }

    fn number_right_edge(&self) -> u16 {
        self.separator_x() - 2 * NUMBER_GAP - 2 * self.bar_width - self.pad
    }

    fn separator_x(&self) -> u16 {
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
        let git_x = self.git_x();
        let staged_x = self.staged_x();

        let mut top = 0u16;
        for line in self.lines {
            let y = top * 16;

            let mut digits = [0u8; 10];
            let text = format_u32(&mut digits, line.number);
            let col = number_right.saturating_sub(self.number_advance(text.len() as u16));
            TextRun {
                col,
                row: y,
                scale: self.number_scale,
                color: self.number_fg,
                bg: Some(self.bg),
                text,
            }
            .render(area, buf, scene);

            if let Some(diag) = line.diagnostic {
                Bar {
                    x: 0,
                    y,
                    width: self.bar_width,
                    height: line.height * 16,
                    color: diag.color,
                }
                .render(area, buf, scene);
            }
            if let Some(git) = line.git {
                Bar {
                    x: git_x,
                    y,
                    width: self.bar_width,
                    height: if git.seam { 6 } else { 16 },
                    color: git.color,
                }
                .render(area, buf, scene);
                Bar {
                    x: staged_x,
                    y,
                    width: self.bar_width,
                    height: line.height * 16,
                    color: git.staged_color,
                }
                .render(area, buf, scene);
            }

            top += line.height;
        }

        Bar {
            x: self.separator_x(),
            y: 0,
            width: 1,
            height: self.total_rows() * 16,
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
            let y = area.y + top;

            if let Some(diag) = line.diagnostic {
                let mut mark = [0u8; 4];
                cells::put(
                    buf,
                    area.x,
                    y,
                    diag.mark.encode_utf8(&mut mark),
                    Style::default().fg(rgb(diag.color)),
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
                Style::default().fg(rgb(self.number_fg)).bg(rgb(self.bg)),
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

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::Rgb(r, g, b)
}

#[cfg(test)]
mod tests {
    use super::{Diagnostic, GitMark, Gutter, GutterLine};
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
    use stoatty_protocol::command::{encode_bar, BarCommand};

    fn config(lines: &[GutterLine]) -> Gutter<'_> {
        Gutter {
            lines,
            bar_width: 5,
            pad: 2,
            number_scale: 160,
            width_digits: 2,
            number_fg: [99, 109, 131],
            separator: [60, 66, 77],
            bg: [40, 44, 52],
        }
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn cell_width_derives_from_the_sixteenth_layout() {
        // number_advance(2) = 2*160/16 = 20; content = 15 + 2 + 20 + 8 + 1 + 4 = 50
        // (3*bar + pad + number + 2*NUMBER_GAP + separator + TEXT_GAP);
        // cell_width = ceil(50/16) = 4.
        assert_eq!(config(&[]).cell_width(), 4);
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
                color: [152, 195, 121],
                staged_color: [80, 90, 100],
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
            x: 43,
            y: 0,
            width: 5,
            height: 16,
            color: [152, 195, 121],
        });
        let staged_bar = encode_bar(&BarCommand {
            x: 50,
            y: 0,
            width: 5,
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

    #[test]
    fn seam_git_mark_draws_a_short_top_aligned_bar() {
        let lines = [GutterLine {
            number: 4,
            height: 1,
            git: Some(GitMark {
                color: [224, 108, 117],
                staged_color: [80, 90, 100],
                seam: true,
            }),
            diagnostic: None,
        }];
        let gutter = config(&lines);
        let area = Rect::new(0, 0, 10, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        gutter.render(area, &mut buf, &mut scene);

        let seam_bar = encode_bar(&BarCommand {
            x: 43,
            y: 0,
            width: 5,
            height: 6,
            color: [224, 108, 117],
        });
        assert!(
            contains(scene.buffer(), &seam_bar),
            "a deletion seam is a short top-aligned bar"
        );
    }
}
