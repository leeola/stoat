use crate::{bar::Bar, text_run::TextRun, ApcScene};
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

/// A status bar composed of left- and right-anchored scaled text segments and a
/// top hairline separator that reads unbroken across the segment backgrounds.
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

/// A single segment of a [`StatusBar`], drawn as a full-row background bar with
/// a box-less scaled text run over it.
///
/// The text carries its own surrounding padding (a segment reads ` label `),
/// and that padded width sizes the background bar. The run stays box-less so the
/// bar carries the background and the top hairline reads unbroken above it.
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
            let advance = self.segment_advance(seg.text);
            self.draw_segment(cursor, advance, seg, area, buf, scene);
            cursor += advance;
        }

        let mut anchor = area.width * 16;
        for seg in self.right {
            let advance = self.segment_advance(seg.text);
            let start = anchor.saturating_sub(advance);
            if start < cursor {
                continue;
            }
            self.draw_segment(start, advance, seg, area, buf, scene);
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

    /// Draw one segment as a full-row background bar with a box-less text run
    /// over it.
    ///
    /// The bar carries the segment background so the top hairline, emitted after
    /// every segment bar, overwrites its top sliver and reads unbroken. A box on
    /// the run instead would paint from the later text pass and bury the line.
    fn draw_segment(
        &self,
        x: u16,
        advance: u16,
        seg: &StatusSegment<'_>,
        area: Rect,
        buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        Bar {
            x,
            y: 0,
            width: advance,
            height: 16,
            color: seg.bg,
        }
        .render(area, buf, scene);
        TextRun {
            col: x,
            row: 0,
            scale: self.scale,
            color: seg.fg,
            bg: None,
            text: seg.text,
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

        // advance("ab") = 2 * 160 / 16 = 20, advance("c") = 1 * 160 / 16 = 10.
        let first_bar = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 20,
            height: 16,
            color: [4, 5, 6],
        });
        let first = encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: None,
            text: "ab".to_owned(),
        });
        let second_bar = encode_bar(&BarCommand {
            x: 20,
            y: 0,
            width: 10,
            height: 16,
            color: [10, 11, 12],
        });
        let second = encode_text_run(&TextRunCommand {
            col: 20,
            row: 0,
            scale: 160,
            color: [7, 8, 9],
            bg: None,
            text: "c".to_owned(),
        });
        assert!(
            contains(scene.buffer(), &first_bar),
            "first segment background bar at col 0"
        );
        assert!(
            contains(scene.buffer(), &first),
            "first box-less run at col 0"
        );
        assert!(
            contains(scene.buffer(), &second_bar),
            "second segment background bar at the first's advance"
        );
        assert!(
            contains(scene.buffer(), &second),
            "second box-less run at the first's advance"
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
        let bar = encode_bar(&BarCommand {
            x: 300,
            y: 0,
            width: 20,
            height: 16,
            color: [4, 5, 6],
        });
        let run = encode_text_run(&TextRunCommand {
            col: 300,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: None,
            text: "xy".to_owned(),
        });
        assert!(
            contains(scene.buffer(), &bar),
            "right segment background bar at width*16 - advance"
        );
        assert!(
            contains(scene.buffer(), &run),
            "right box-less run at width*16 - advance"
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
        let dropped_run = encode_text_run(&TextRunCommand {
            col: 38,
            row: 0,
            scale: 160,
            color: [3, 3, 3],
            bg: None,
            text: "R".to_owned(),
        });
        let dropped_bar = encode_bar(&BarCommand {
            x: 38,
            y: 0,
            width: 10,
            height: 16,
            color: [4, 4, 4],
        });
        assert!(
            !contains(scene.buffer(), &dropped_run),
            "the colliding right segment emits no run"
        );
        assert!(
            !contains(scene.buffer(), &dropped_bar),
            "the colliding right segment emits no bar"
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
