use crate::{bar::Bar, cells, text_run::TextRun, ApcScene};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::StatefulWidget,
};

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

/// One gutter line: its number, the rows it occupies, and its status marks.
#[derive(Clone, Copy)]
pub struct GutterLine {
    pub number: u32,
    /// Rows the line occupies: one, plus any inline-expansion rows beneath it.
    pub height: u16,
    /// Git-status bar color, or `None` for an unchanged line.
    pub git: Option<[u8; 3]>,
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
    pub fn cell_width(&self) -> u16 {
        (self.separator_x() + 1).div_ceil(16)
    }

    fn git_x(&self) -> u16 {
        self.bar_width + self.pad
    }

    /// Sixteenths a run of `digits` numerals advances at [`Self::number_scale`].
    fn number_advance(&self, digits: u16) -> u16 {
        digits * self.number_scale / 16
    }

    fn number_right_edge(&self) -> u16 {
        2 * self.bar_width + 2 * self.pad + self.number_advance(self.width_digits)
    }

    fn separator_x(&self) -> u16 {
        self.number_right_edge() + self.pad
    }

    fn total_rows(&self) -> u16 {
        self.lines.iter().map(|line| line.height).sum()
    }

    fn draw_components(&self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        let number_right = self.number_right_edge();
        let git_x = self.git_x();

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
                bg: self.bg,
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
                    height: 16,
                    color: git,
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

    fn draw_fallback(&self, area: Rect, buf: &mut Buffer) {
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
    use super::{Diagnostic, Gutter, GutterLine};
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
        // number_advance(2) = 2*160/16 = 20; right_edge = 10 + 4 + 20 = 34;
        // separator_x = 36; cell_width = ceil(37/16) = 3.
        assert_eq!(config(&[]).cell_width(), 3);
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
        assert_eq!(buf.cell((2u16, 0u16)).expect("cell").symbol(), "7");
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
            x: 36,
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
            git: Some([152, 195, 121]),
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
            x: 7,
            y: 0,
            width: 5,
            height: 16,
            color: [152, 195, 121],
        });
        assert!(
            contains(scene.buffer(), &diag_bar),
            "diagnostic bar spans the two-row line"
        );
        assert!(
            contains(scene.buffer(), &git_bar),
            "git bar marks only the code row"
        );
    }
}
