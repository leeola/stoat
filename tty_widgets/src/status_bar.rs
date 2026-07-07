use crate::{bar::Bar, text_run::TextRun, ApcScene};
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

/// A status bar composed of left- and right-anchored scaled text segments and a
/// top hairline separator.
///
/// Components-only, like [`TextRun`] and [`Bar`]: it emits off-grid APC frames
/// and writes no cell fallback, so the caller paints its own degraded cells for
/// any other terminal. [`Self::scale`] is the glyph size in 256ths of a cell
/// (256 = grid size), and every position is in sixteenths of a cell (16 = one
/// cell), so the bar tracks live font zoom.
///
/// Left segments pack rightward from the left edge. Right segments pack leftward
/// from the right edge and are dropped when they would overlap the left run, so
/// the two runs never collide.
pub struct StatusBar<'a> {
    /// Segments packed left-to-right from the left edge.
    pub left: &'a [StatusSegment<'a>],
    /// Segments packed right-to-left from the right edge, in slice order.
    pub right: &'a [StatusSegment<'a>],
    /// Glyph size in 256ths of the cell size.
    pub scale: u16,
    /// Hairline separator color, drawn along the row's top edge.
    pub separator: [u8; 3],
}

/// A single segment of a [`StatusBar`], drawn as one scaled text run.
///
/// The text carries its own surrounding padding (a segment reads ` label `).
/// Because a scaled run's background paints as a full-row-height rect, those
/// padding spaces carry the segment background with no extra bars.
pub struct StatusSegment<'a> {
    pub text: &'a str,
    pub fg: [u8; 3],
    pub bg: [u8; 3],
}

impl StatusBar<'_> {
    /// Draw the scaled segments and the top hairline as off-grid components.
    ///
    /// The caller passes the on-screen status [`Rect`]. [`TextRun`] and [`Bar`]
    /// offset by the area, so positions here are area-relative sixteenths.
    pub fn draw_components(&self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        let mut cursor = 0u16;
        for seg in self.left {
            TextRun {
                col: cursor,
                row: 0,
                scale: self.scale,
                color: seg.fg,
                bg: seg.bg,
                text: seg.text,
            }
            .render(area, buf, scene);
            cursor += self.segment_advance(seg.text);
        }

        let mut anchor = area.width * 16;
        for seg in self.right {
            let start = anchor.saturating_sub(self.segment_advance(seg.text));
            if start < cursor {
                continue;
            }
            TextRun {
                col: start,
                row: 0,
                scale: self.scale,
                color: seg.fg,
                bg: seg.bg,
                text: seg.text,
            }
            .render(area, buf, scene);
            anchor = start;
        }

        Bar {
            x: 0,
            y: 0,
            width: area.width * 16,
            height: 1,
            color: self.separator,
        }
        .render(area, buf, scene);
    }

    /// Sixteenths a segment's `text` advances at [`Self::scale`].
    fn segment_advance(&self, text: &str) -> u16 {
        text.chars().count() as u16 * self.scale / 16
    }
}

#[cfg(test)]
mod tests {
    use super::{StatusBar, StatusSegment};
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect};
    use stoatty_protocol::command::{encode_bar, encode_text_run, BarCommand, TextRunCommand};

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack
            .windows(needle.len())
            .any(|window| window == needle)
    }

    #[test]
    fn left_segments_pack_from_the_left_edge() {
        let left = [
            StatusSegment {
                text: "ab",
                fg: [1, 2, 3],
                bg: [4, 5, 6],
            },
            StatusSegment {
                text: "c",
                fg: [7, 8, 9],
                bg: [10, 11, 12],
            },
        ];
        let status = StatusBar {
            left: &left,
            right: &[],
            scale: 160,
            separator: [60, 66, 77],
        };
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        status.draw_components(area, &mut buf, &mut scene);

        let first = encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: [4, 5, 6],
            text: "ab".to_owned(),
        });
        // advance("ab") = 2 * 160 / 16 = 20
        let second = encode_text_run(&TextRunCommand {
            col: 20,
            row: 0,
            scale: 160,
            color: [7, 8, 9],
            bg: [10, 11, 12],
            text: "c".to_owned(),
        });
        assert!(contains(scene.buffer(), &first), "first segment at col 0");
        assert!(
            contains(scene.buffer(), &second),
            "second segment at the first's advance"
        );
    }

    #[test]
    fn a_right_segment_anchors_to_the_right_edge() {
        let right = [StatusSegment {
            text: "xy",
            fg: [1, 2, 3],
            bg: [4, 5, 6],
        }];
        let status = StatusBar {
            left: &[],
            right: &right,
            scale: 160,
            separator: [60, 66, 77],
        };
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        status.draw_components(area, &mut buf, &mut scene);

        // width*16 = 320; advance("xy") = 20; start = 300
        let run = encode_text_run(&TextRunCommand {
            col: 300,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: [4, 5, 6],
            text: "xy".to_owned(),
        });
        assert!(
            contains(scene.buffer(), &run),
            "right segment lands at width*16 - advance"
        );
    }

    #[test]
    fn a_colliding_right_segment_is_dropped() {
        let left = [StatusSegment {
            text: "LEFT",
            fg: [1, 1, 1],
            bg: [2, 2, 2],
        }];
        let right = [StatusSegment {
            text: "R",
            fg: [3, 3, 3],
            bg: [4, 4, 4],
        }];
        let status = StatusBar {
            left: &left,
            right: &right,
            scale: 160,
            separator: [60, 66, 77],
        };
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        status.draw_components(area, &mut buf, &mut scene);

        // cursor after "LEFT" = 4 * 160 / 16 = 40; width*16 = 48; advance("R") = 10;
        // start = 38 < 40, so the right segment is skipped.
        let dropped = encode_text_run(&TextRunCommand {
            col: 38,
            row: 0,
            scale: 160,
            color: [3, 3, 3],
            bg: [4, 4, 4],
            text: "R".to_owned(),
        });
        assert!(
            !contains(scene.buffer(), &dropped),
            "the colliding right segment emits nothing"
        );
    }

    #[test]
    fn the_top_hairline_separator_is_emitted() {
        let status = StatusBar {
            left: &[],
            right: &[],
            scale: 160,
            separator: [60, 66, 77],
        };
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        status.draw_components(area, &mut buf, &mut scene);

        let separator = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 320,
            height: 1,
            color: [60, 66, 77],
        });
        assert!(
            contains(scene.buffer(), &separator),
            "top hairline separator bar frame"
        );
    }
}
