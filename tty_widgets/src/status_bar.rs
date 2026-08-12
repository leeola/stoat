use crate::{bar::Bar, cells, text_run::TextRun, ApcScene};
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};

/// A status bar composed of left- and right-anchored scaled text segments and a
/// top hairline separator that reads across the background between them.
///
/// A segment paints a full-row background box only where its background differs
/// from [`Self::bg`], and that box breaks the hairline where it sits. The
/// separator is emitted first so it can. A hairline painted over a segment
/// instead clips its top sliver, which reads as the segment starting below the
/// bar's own top edge.
///
/// A segment on the bar's own background paints no box, so the hairline reads
/// through it. The cell grid already carries that color under the whole row, so
/// such a box would be invisible apart from erasing the hairline over exactly
/// the segment's text.
///
/// Components-only, like [`TextRun`] and [`Bar`]: it emits off-grid APC frames
/// and writes no cell fallback, so the caller paints its own degraded cells for
/// any other terminal. [`Self::scale`] is the glyph size in 256ths of a cell
/// (256 = grid size), and every position is in sixteenths of a cell (16 = one
/// cell), so the bar tracks live font zoom.
///
/// Left segments pack rightward from the left edge, and the bar cuts the last
/// one to the glyphs that fit before the right edge. Right segments pack
/// leftward from the right edge, and a segment that overlaps the left run is
/// dropped. Neither run draws past the bar.
///
/// The left run is cut rather than dropped because the segments that outgrow a
/// bar are the informative ones, paths and branch names, and a caller's cell
/// fallback clips them the same way.
pub struct StatusBar<'a> {
    /// Segments packed left-to-right from the left edge.
    pub left: &'a [StatusSegment<'a>],
    /// Segments packed right-to-left from the right edge, in slice order.
    pub right: &'a [StatusSegment<'a>],
    /// Glyph size in 256ths of the cell size.
    pub scale: u16,
    /// Hairline separator color, drawn along the row's top edge.
    pub separator: [u8; 3],
    /// The row's own background, matching what the cell grid paints beneath it.
    /// A segment carrying this same background paints no box of its own.
    pub bg: [u8; 3],
}

/// A single segment of a [`StatusBar`], drawn as a box-less scaled text run over
/// a full-row background bar.
///
/// The text carries its own surrounding padding (a segment reads ` label `),
/// and that padded width sizes the background bar. The run stays box-less so the
/// background comes from the bar, which spans the row's full height and so
/// covers the hairline emitted before it.
///
/// [`Self::bg`] matching [`StatusBar::bg`] drops the bar, leaving the run over
/// the row's own background.
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
        let limit = cells::span_sixteenths(area.width);

        Bar {
            x: 0,
            y: 0,
            width: limit,
            height: 1,
            color: self.separator,
        }
        .render(area, buf, scene);

        let mut cursor = 0u16;
        for seg in self.left {
            let text = self.clip_to_room(seg.text, limit - cursor);
            if text.is_empty() && !seg.text.is_empty() {
                break;
            }

            let advance = self.segment_advance(text);
            let fitted = StatusSegment {
                text,
                fg: seg.fg,
                bg: seg.bg,
            };
            self.draw_segment(cursor, advance, &fitted, area, buf, scene);
            cursor += advance;

            if text.len() < seg.text.len() {
                break;
            }
        }

        let mut anchor = limit;
        for seg in self.right {
            let advance = self.segment_advance(seg.text);
            let start = anchor.saturating_sub(advance);
            if start < cursor {
                continue;
            }
            self.draw_segment(start, advance, seg, area, buf, scene);
            anchor = start;
        }
    }

    /// Draw one segment as a box-less text run, over a full-row background bar
    /// when the segment's background differs from the bar's own.
    ///
    /// The bar carries the segment background and spans the row's full height,
    /// so it covers the top hairline emitted before it and the segment reads
    /// flush with the bar's top edge. A box on the run instead would paint from
    /// the later text pass and bury the segment's own background.
    ///
    /// A segment matching [`StatusBar::bg`] emits no bar. The cell grid already
    /// paints that color under the whole row, so the only thing such a bar
    /// would do is erase the hairline over the segment's text.
    fn draw_segment(
        &self,
        x: u16,
        advance: u16,
        seg: &StatusSegment<'_>,
        area: Rect,
        buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        let x = cells::signed_sixteenths(x);

        if seg.bg != self.bg {
            Bar {
                x,
                y: 0,
                width: advance,
                height: 16,
                color: seg.bg,
            }
            .render(area, buf, scene);
        }
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
        cells::advance_sixteenths(text.chars().count(), self.scale)
    }

    /// The prefix of `text` that advances no further than `room` sixteenths.
    ///
    /// Inverts [`Self::segment_advance`], so the prefix ends on the last glyph
    /// that starts and finishes inside the room rather than one past it. A zero
    /// scale advances nothing, so every segment fits.
    fn clip_to_room<'t>(&self, text: &'t str, room: u16) -> &'t str {
        if self.scale == 0 {
            return text;
        }

        let fits = (u32::from(room) * 16 / u32::from(self.scale)) as usize;
        match text.char_indices().nth(fits) {
            Some((byte, _)) => &text[..byte],
            None => text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{StatusBar, StatusSegment};
    use crate::ApcScene;
    use ratatui::{buffer::Buffer, layout::Rect};
    use stoatty_protocol::command::{encode_bar, encode_text_run, BarCommand, TextRunCommand};

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        position(haystack, needle).is_some()
    }

    fn position(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
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
            bg: [0, 0, 0],
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
            bg: [0, 0, 0],
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

    /// Paths and branch names grow without bound, so a left run outgrowing a
    /// narrow bar is ordinary. Past the edge it draws over the neighboring
    /// pane, not over its own right segments.
    #[test]
    fn the_left_run_stops_at_the_right_edge() {
        let left = [
            StatusSegment {
                text: "ab",
                fg: [1, 2, 3],
                bg: [4, 5, 6],
            },
            StatusSegment {
                text: "LONGISH",
                fg: [7, 8, 9],
                bg: [10, 11, 12],
            },
        ];
        let status = StatusBar {
            left: &left,
            right: &[],
            scale: 160,
            separator: [60, 66, 77],
            bg: [0, 0, 0],
        };
        let area = Rect::new(0, 0, 3, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        status.draw_components(area, &mut buf, &mut scene);

        // width*16 = 48; advance("ab") = 20 leaves 28 sixteenths, which fits
        // two more glyphs at 10 each. The whole of "LONGISH" advances 70.
        let fits = encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: None,
            text: "ab".to_owned(),
        });
        let cut = encode_text_run(&TextRunCommand {
            col: 20,
            row: 0,
            scale: 160,
            color: [7, 8, 9],
            bg: None,
            text: "LO".to_owned(),
        });
        let cut_bar = encode_bar(&BarCommand {
            x: 20,
            y: 0,
            width: 20,
            height: 16,
            color: [10, 11, 12],
        });
        let overruns = encode_text_run(&TextRunCommand {
            col: 20,
            row: 0,
            scale: 160,
            color: [7, 8, 9],
            bg: None,
            text: "LONGISH".to_owned(),
        });
        let overrun_bar = encode_bar(&BarCommand {
            x: 20,
            y: 0,
            width: 70,
            height: 16,
            color: [10, 11, 12],
        });
        assert!(
            contains(scene.buffer(), &fits),
            "the segment that fits draws"
        );
        assert!(
            contains(scene.buffer(), &cut),
            "the overrunning segment keeps the glyphs that fit"
        );
        assert!(
            contains(scene.buffer(), &cut_bar),
            "and its background bar stops with them"
        );
        assert!(
            !contains(scene.buffer(), &overruns),
            "the segment past the edge emits no whole run"
        );
        assert!(
            !contains(scene.buffer(), &overrun_bar),
            "and no background bar"
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
            bg: [0, 0, 0],
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
            bg: [0, 0, 0],
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

    #[test]
    fn the_hairline_is_emitted_before_the_segments_that_cover_it() {
        let left = [StatusSegment {
            text: " NOR ",
            fg: [1, 2, 3],
            bg: [4, 5, 6],
        }];
        let right = [StatusSegment {
            text: " 1:1 ",
            fg: [7, 8, 9],
            bg: [10, 11, 12],
        }];
        let status = StatusBar {
            left: &left,
            right: &right,
            scale: 160,
            separator: [60, 66, 77],
            bg: [0, 0, 0],
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
        // advance(" NOR ") = 5 * 160 / 16 = 50; right start = 320 - 50 = 270.
        let left_bar = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 50,
            height: 16,
            color: [4, 5, 6],
        });
        let right_bar = encode_bar(&BarCommand {
            x: 270,
            y: 0,
            width: 50,
            height: 16,
            color: [10, 11, 12],
        });

        let frames = scene.buffer();
        let separator_at = position(frames, &separator).expect("hairline emitted");
        let left_at = position(frames, &left_bar).expect("left segment bar emitted");
        let right_at = position(frames, &right_bar).expect("right segment bar emitted");

        assert!(
            separator_at < left_at,
            "the hairline precedes the left segment bar that covers it"
        );
        assert!(
            separator_at < right_at,
            "the hairline precedes the right segment bar that covers it"
        );
    }

    /// A bar whose segments all sit on its own background carries no visible
    /// boxes, so a covering bar per segment would show up only as gaps chewed
    /// out of the hairline over exactly the segment texts.
    #[test]
    fn a_segment_on_the_bar_background_emits_no_covering_bar() {
        let bar_bg = [30, 31, 32];
        let left = [StatusSegment {
            text: "ab",
            fg: [1, 2, 3],
            bg: bar_bg,
        }];
        let right = [StatusSegment {
            text: "xy",
            fg: [7, 8, 9],
            bg: [10, 11, 12],
        }];
        let status = StatusBar {
            left: &left,
            right: &right,
            scale: 160,
            separator: [60, 66, 77],
            bg: bar_bg,
        };
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        status.draw_components(area, &mut buf, &mut scene);

        // advance("ab") = advance("xy") = 20; right start = 320 - 20 = 300.
        let base_bar = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 20,
            height: 16,
            color: bar_bg,
        });
        let base_run = encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: None,
            text: "ab".to_owned(),
        });
        let separator = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 320,
            height: 1,
            color: [60, 66, 77],
        });
        let distinct_bar = encode_bar(&BarCommand {
            x: 300,
            y: 0,
            width: 20,
            height: 16,
            color: [10, 11, 12],
        });

        let frames = scene.buffer();
        assert!(
            !contains(frames, &base_bar),
            "a segment on the bar background emits no covering bar"
        );
        assert!(contains(frames, &base_run), "but still emits its text run");
        assert!(
            contains(frames, &separator),
            "so the hairline still reads across it"
        );
        assert!(
            contains(frames, &distinct_bar),
            "a distinct-background segment keeps covering the hairline"
        );
    }
}
