use crate::{
    bar::Bar,
    cells,
    panel::Panel,
    text_run::{self, TextRun},
    ApcScene,
};
use ratatui::{buffer::Buffer, layout::Rect, widgets::StatefulWidget};
use std::ops::Range;

/// The width of a dot segment's square, in sixteenths of a cell.
///
/// A cell is half as wide as it is tall, so 6 sixteenths across and
/// [`DOT_HEIGHT`] down draw a square.
const DOT_WIDTH: u16 = 6;

/// The height of a dot segment's square, in sixteenths of a cell.
const DOT_HEIGHT: u16 = 3;

/// How far down the row a dot segment's square starts, in sixteenths of a
/// cell, which puts its center where a middle dot's glyph sits.
const DOT_TOP: i16 = 7;

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
///
/// A [`SegmentBox`] gathers a run of left segments into one box of whole
/// cells, as an editor's mode chip and a tab bar's tabs are.
pub struct StatusBar<'a> {
    /// Segments packed left-to-right from the left edge.
    pub left: &'a [StatusSegment<'a>],
    /// Runs of [`Self::left`] that each draw in one box of whole cells, in
    /// order and apart.
    pub boxes: &'a [SegmentBox],
    /// The indices of the [`Self::left`] segments that draw as a small square
    /// in their foreground in place of their text, centered across the text's
    /// advance.
    pub dots: &'a [usize],
    /// The [`Self::left`] segments that draw at their own glyph size, each an
    /// index and a size in 256ths of the cell size, in place of
    /// [`Self::scale`].
    pub scales: &'a [(usize, u16)],
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
    /// Whether the text draws in the bold face.
    pub bold: bool,
}

/// A run of a [`StatusBar`]'s left segments that draws in one box of whole
/// cells, such as a tab or an editor's mode chip.
///
/// The box starts on the first whole cell the run reaches, spans the fewest
/// whole cells that hold the run, and centers the run in it. A [`Panel`] takes
/// whole cells, so whole cells are what lets a panel draw the box exactly
/// where the run sits.
pub struct SegmentBox {
    /// The run, as indices into [`StatusBar::left`].
    pub segments: Range<usize>,
    /// The panel that draws the box behind the run, or `None` for a box that
    /// only aligns the run. The bar places the panel on the box's cells.
    ///
    /// A box with a panel draws no background bar for its segments, because
    /// the panel is their background.
    pub panel: Option<Panel>,
}

/// One step of a left run's layout, in the order the bar draws it.
enum LeftItem<'t> {
    /// Box `index` of [`StatusBar::boxes`] covers `cells`, counted from the
    /// bar's left edge.
    Box { index: usize, cells: Range<u16> },
    /// Left segment `index` draws `text`, the part of its text that fits, from
    /// `x` sixteenths.
    Segment { index: usize, x: u16, text: &'t str },
}

impl StatusBar<'_> {
    /// Draw the scaled segments and the top hairline as off-grid components.
    ///
    /// The caller passes the on-screen status [`Rect`]. [`TextRun`] and [`Bar`]
    /// offset by the area, so positions here are area-relative sixteenths.
    pub fn draw_components(&self, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        self.draw_components_within(area, area.width, buf, scene);
    }

    /// Draw the bar across `area` with its segments inside the row's first
    /// `segments_width` cells.
    ///
    /// The hairline still runs the whole row. The cells past `segments_width`
    /// belong to something drawn over the bar, such as a badge box, and a
    /// segment placed there sits under it. A width past the row's counts as
    /// the row's.
    pub fn draw_components_within(
        &self,
        area: Rect,
        segments_width: u16,
        buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        let limit = cells::span_sixteenths(segments_width.min(area.width));
        let layout = layout_left(
            self.left.len(),
            |index| self.left[index].text,
            self.boxes
                .iter()
                .map(|segment_box| segment_box.segments.clone()),
            |index| segment_scale(self.scales, self.scale, index),
            limit,
        );

        // A panel hides a component of lower seq only half a pixel inside its
        // box, so one whole hairline still crosses the box's top edge. The
        // hairline breaks over each box with a panel, as a segment's background
        // bar breaks it.
        let mut from = 0;
        for item in &layout {
            if let LeftItem::Box { index, cells } = item
                && self.boxes[*index].panel.is_some()
            {
                self.draw_hairline(from..cells.start, area, buf, scene);
                from = cells.end;
            }
        }
        self.draw_hairline(from..area.width, area, buf, scene);

        let mut cursor = 0u16;
        let mut paneled = 0..0;
        for item in layout {
            match item {
                LeftItem::Box { index, cells } => {
                    let segment_box = &self.boxes[index];
                    paneled = 0..0;
                    if let Some(panel) = &segment_box.panel {
                        let rect = Rect::new(area.x + cells.start, area.y, cells.len() as u16, 1);
                        panel.draw_components(rect, scene);
                        paneled = segment_box.segments.clone();
                    }
                    cursor = cursor.max(cells::span_sixteenths(cells.end));
                },
                LeftItem::Segment { index, x, text } => {
                    let seg = &self.left[index];
                    let scale = segment_scale(self.scales, self.scale, index);
                    let advance = advance(text, scale);
                    if !paneled.contains(&index) {
                        self.draw_backing(x, advance, seg.bg, area, buf, scene);
                    }
                    let fitted = StatusSegment {
                        text,
                        fg: seg.fg,
                        bg: seg.bg,
                        bold: seg.bold,
                    };
                    match self.dots.contains(&index) {
                        true => self.draw_dot(x, advance, seg.fg, area, buf, scene),
                        false => self.draw_run(x, &fitted, scale, area, buf, scene),
                    }
                    cursor = cursor.max(x + advance);
                },
            }
        }

        let mut anchor = limit;
        for seg in self.right {
            let advance = advance(seg.text, self.scale);
            let start = anchor.saturating_sub(advance);
            if start < cursor {
                continue;
            }
            self.draw_backing(start, advance, seg.bg, area, buf, scene);
            self.draw_run(start, seg, self.scale, area, buf, scene);
            anchor = start;
        }
    }

    /// Draw the hairline along the top of `cells`, counted from the bar's left
    /// edge, or nothing for an empty range.
    fn draw_hairline(&self, cells: Range<u16>, area: Rect, buf: &mut Buffer, scene: &mut ApcScene) {
        if cells.is_empty() {
            return;
        }

        Bar {
            x: cells::signed_sixteenths(cells::span_sixteenths(cells.start)),
            y: 0,
            width: cells::span_sixteenths(cells.end - cells.start),
            height: 1,
            color: self.separator,
        }
        .render(area, buf, scene);
    }

    /// Draw a full-row bar in `bg` behind the segment `advance` sixteenths wide
    /// at `x`, when `bg` differs from the bar's own.
    ///
    /// The bar carries the segment background and spans the row's full height,
    /// so it covers the top hairline emitted before it and the segment reads
    /// flush with the bar's top edge. A box on the run paints from the later
    /// text pass instead, and it buries the segment's own background.
    ///
    /// A segment matching [`StatusBar::bg`] emits no bar. The cell grid already
    /// paints that color under the whole row, so such a bar only erases the
    /// hairline over the segment's text.
    fn draw_backing(
        &self,
        x: u16,
        advance: u16,
        bg: [u8; 3],
        area: Rect,
        buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        if bg == self.bg {
            return;
        }

        Bar {
            x: cells::signed_sixteenths(x),
            y: 0,
            width: advance,
            height: 16,
            color: bg,
        }
        .render(area, buf, scene);
    }

    /// Draw `seg`'s text as a box-less run in its foreground at `x`, its glyphs
    /// at `scale`.
    fn draw_run(
        &self,
        x: u16,
        seg: &StatusSegment<'_>,
        scale: u16,
        area: Rect,
        buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        TextRun {
            col: cells::signed_sixteenths(x),
            row: 0,
            scale,
            color: seg.fg,
            bg: None,
            bold: seg.bold,
            text: seg.text,
            follow: 0,
            anchor: None,
        }
        .render(area, buf, scene);
    }

    /// Draw a dot segment's square in `color` in place of its glyph, centered
    /// across the `advance` sixteenths at `x`.
    fn draw_dot(
        &self,
        x: u16,
        advance: u16,
        color: [u8; 3],
        area: Rect,
        buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        Bar {
            x: cells::signed_sixteenths(x + advance.saturating_sub(DOT_WIDTH) / 2),
            y: DOT_TOP,
            width: DOT_WIDTH,
            height: DOT_HEIGHT,
            color,
        }
        .render(area, buf, scene);
    }
}

/// The extent of each box of `boxes` that a bar of left segments `texts`
/// draws in `segments_width` cells, in sixteenths from the bar's left edge.
///
/// A segment advances at its entry in `scales`, as [`StatusBar::scales`]
/// gives it, or at `scale`. These are the boxes
/// [`StatusBar::draw_components_within`] draws, so a press hit-tests against
/// what is on screen. A box past the bar's edge has no entry, and one that the
/// edge cuts ends there.
pub fn box_spans(
    texts: &[&str],
    boxes: &[Range<usize>],
    scales: &[(usize, u16)],
    scale: u16,
    segments_width: u16,
) -> Vec<Range<u16>> {
    layout_left(
        texts.len(),
        |index| texts[index],
        boxes.iter().cloned(),
        |index| segment_scale(scales, scale, index),
        cells::span_sixteenths(segments_width),
    )
    .into_iter()
    .filter_map(|item| match item {
        LeftItem::Box { cells, .. } => {
            Some(cells::span_sixteenths(cells.start)..cells::span_sixteenths(cells.end))
        },
        LeftItem::Segment { .. } => None,
    })
    .collect()
}

/// Lay out a left run of `count` segments in `limit` sixteenths, with each run
/// of `boxes` in one box of whole cells.
///
/// `text` gives each segment's text and `scale` the glyph size it advances at.
/// A plain segment packs at the cursor. A box starts on the next whole cell,
/// spans the fewest whole cells that hold its run, and centers the run. A
/// segment or a box that crosses `limit` is cut there and ends the layout.
fn layout_left<'t>(
    count: usize,
    text: impl Fn(usize) -> &'t str,
    boxes: impl Iterator<Item = Range<usize>>,
    scale: impl Fn(usize) -> u16,
    limit: u16,
) -> Vec<LeftItem<'t>> {
    let mut items = Vec::new();
    let mut boxes = boxes.enumerate().peekable();
    let mut cursor = 0u16;
    let mut index = 0;
    while index < count {
        let Some((box_index, run)) = boxes.next_if(|(_, run)| run.start == index) else {
            let full = text(index);
            let fitted = clip_to_room(full, limit - cursor, scale(index));
            if fitted.is_empty() && !full.is_empty() {
                break;
            }
            items.push(LeftItem::Segment {
                index,
                x: cursor,
                text: fitted,
            });
            cursor += advance(fitted, scale(index));
            if fitted.len() < full.len() {
                break;
            }
            index += 1;
            continue;
        };

        let start = cursor.div_ceil(16).saturating_mul(16);
        if start >= limit {
            break;
        }
        let natural = run.clone().fold(0u16, |sum, segment| {
            sum.saturating_add(advance(text(segment), scale(segment)))
        });
        let span = natural.div_ceil(16).saturating_mul(16);
        let end = start.saturating_add(span).min(limit);
        items.push(LeftItem::Box {
            index: box_index,
            cells: start / 16..end / 16,
        });

        let mut x = start + (span - natural) / 2;
        for segment in run.clone() {
            let full = text(segment);
            let fitted = clip_to_room(full, limit.saturating_sub(x), scale(segment));
            if fitted.is_empty() && !full.is_empty() {
                return items;
            }
            items.push(LeftItem::Segment {
                index: segment,
                x,
                text: fitted,
            });
            x += advance(fitted, scale(segment));
            if fitted.len() < full.len() {
                return items;
            }
        }
        cursor = end;
        index = run.end;
    }
    items
}

/// The glyph size of left segment `index`, which is its entry in `scales`, or
/// `scale` for a segment with none.
fn segment_scale(scales: &[(usize, u16)], scale: u16, index: usize) -> u16 {
    scales
        .iter()
        .find(|&&(segment, _)| segment == index)
        .map_or(scale, |&(_, own)| own)
}

/// Sixteenths a segment's `text` advances at `scale`.
fn advance(text: &str, scale: u16) -> u16 {
    text_run::advance_sixteenths(text.chars().count(), scale)
}

/// The prefix of `text` that advances no further than `room` sixteenths at
/// `scale`.
///
/// Inverts [`advance`], so the prefix ends on the last glyph that starts and
/// finishes inside the room rather than one past it. A zero scale advances
/// nothing, so every segment fits.
fn clip_to_room(text: &str, room: u16, scale: u16) -> &str {
    if scale == 0 {
        return text;
    }

    let fits = (u32::from(room) * 16 / u32::from(scale)) as usize;
    match text.char_indices().nth(fits) {
        Some((byte, _)) => &text[..byte],
        None => text,
    }
}

#[cfg(test)]
mod tests {
    use super::{box_spans, SegmentBox, StatusBar, StatusSegment};
    use crate::{panel::Panel, ApcScene};
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::ops::Range;
    use stoatty_protocol::command::{
        encode_bar, encode_panel, encode_text_run, BarCommand, BorderStyle, PanelCommand,
        PanelShadow, TextRunCommand,
    };

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
                bold: false,
            },
            StatusSegment {
                text: "c",
                fg: [7, 8, 9],
                bg: [10, 11, 12],
                bold: true,
            },
        ];
        let status = StatusBar {
            left: &left,
            boxes: &[],
            dots: &[],
            scales: &[],
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
            occludes: false,
            hatch: None,
        });
        let first = encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: None,
            bold: false,
            follow: 0,
            anchor: None,
            text: "ab".to_owned(),
        });
        let second_bar = encode_bar(&BarCommand {
            x: 20,
            y: 0,
            width: 10,
            height: 16,
            color: [10, 11, 12],
            occludes: false,
            hatch: None,
        });
        let second = encode_text_run(&TextRunCommand {
            col: 20,
            row: 0,
            scale: 160,
            color: [7, 8, 9],
            bg: None,
            bold: true,
            follow: 0,
            anchor: None,
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
            "second box-less run at the first's advance, in bold"
        );
    }

    #[test]
    fn a_right_segment_anchors_to_the_right_edge() {
        let right = [StatusSegment {
            text: "xy",
            fg: [1, 2, 3],
            bg: [4, 5, 6],
            bold: false,
        }];
        let status = StatusBar {
            left: &[],
            boxes: &[],
            dots: &[],
            scales: &[],
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
            occludes: false,
            hatch: None,
        });
        let run = encode_text_run(&TextRunCommand {
            col: 300,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: None,
            bold: false,
            follow: 0,
            anchor: None,
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

    /// A badge over the bar's right end owns the cells under it, so the right
    /// run ends where the badge starts while the hairline still runs the row.
    #[test]
    fn right_segments_anchor_at_the_given_width() {
        let right = [StatusSegment {
            text: "xy",
            fg: [1, 2, 3],
            bg: [4, 5, 6],
            bold: false,
        }];
        let status = StatusBar {
            left: &[],
            boxes: &[],
            dots: &[],
            scales: &[],
            right: &right,
            scale: 160,
            separator: [60, 66, 77],
            bg: [0, 0, 0],
        };
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        status.draw_components_within(area, 8, &mut buf, &mut scene);

        // The hairline spans 20 * 16 = 320. The segments end at 8 * 16 = 128,
        // and advance("xy") = 20 starts the run at 108.
        let hairline = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 320,
            height: 1,
            color: [60, 66, 77],
            occludes: false,
            hatch: None,
        });
        let run = encode_text_run(&TextRunCommand {
            col: 108,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: None,
            bold: false,
            follow: 0,
            anchor: None,
            text: "xy".to_owned(),
        });
        assert!(
            contains(scene.buffer(), &hairline),
            "the hairline spans the whole row"
        );
        assert!(
            contains(scene.buffer(), &run),
            "the right run ends at the segments width"
        );
    }

    /// The left run stops where the badge starts too, rather than at the row's
    /// end under the badge.
    #[test]
    fn the_left_run_stops_at_the_given_width() {
        let left = [StatusSegment {
            text: "ABCDEFGHIJKLMN",
            fg: [1, 2, 3],
            bg: [4, 5, 6],
            bold: false,
        }];
        let status = StatusBar {
            left: &left,
            boxes: &[],
            dots: &[],
            scales: &[],
            right: &[],
            scale: 160,
            separator: [60, 66, 77],
            bg: [0, 0, 0],
        };
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();

        status.draw_components_within(area, 8, &mut buf, &mut scene);

        // 8 * 16 = 128 sixteenths fit 12 glyphs at 10 each, so the run is cut
        // there. The row's 320 holds all 14.
        let cut = encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: None,
            bold: false,
            follow: 0,
            anchor: None,
            text: "ABCDEFGHIJKL".to_owned(),
        });
        assert!(
            contains(scene.buffer(), &cut),
            "the left run is cut at the segments width"
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
                bold: false,
            },
            StatusSegment {
                text: "LONGISH",
                fg: [7, 8, 9],
                bg: [10, 11, 12],
                bold: false,
            },
        ];
        let status = StatusBar {
            left: &left,
            boxes: &[],
            dots: &[],
            scales: &[],
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
            bold: false,
            follow: 0,
            anchor: None,
            text: "ab".to_owned(),
        });
        let cut = encode_text_run(&TextRunCommand {
            col: 20,
            row: 0,
            scale: 160,
            color: [7, 8, 9],
            bg: None,
            bold: false,
            follow: 0,
            anchor: None,
            text: "LO".to_owned(),
        });
        let cut_bar = encode_bar(&BarCommand {
            x: 20,
            y: 0,
            width: 20,
            height: 16,
            color: [10, 11, 12],
            occludes: false,
            hatch: None,
        });
        let overruns = encode_text_run(&TextRunCommand {
            col: 20,
            row: 0,
            scale: 160,
            color: [7, 8, 9],
            bg: None,
            bold: false,
            follow: 0,
            anchor: None,
            text: "LONGISH".to_owned(),
        });
        let overrun_bar = encode_bar(&BarCommand {
            x: 20,
            y: 0,
            width: 70,
            height: 16,
            color: [10, 11, 12],
            occludes: false,
            hatch: None,
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
            bold: false,
        }];
        let right = [StatusSegment {
            text: "R",
            fg: [3, 3, 3],
            bg: [4, 4, 4],
            bold: false,
        }];
        let status = StatusBar {
            left: &left,
            boxes: &[],
            dots: &[],
            scales: &[],
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
            bold: false,
            follow: 0,
            anchor: None,
            text: "R".to_owned(),
        });
        let dropped_bar = encode_bar(&BarCommand {
            x: 38,
            y: 0,
            width: 10,
            height: 16,
            color: [4, 4, 4],
            occludes: false,
            hatch: None,
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
            boxes: &[],
            dots: &[],
            scales: &[],
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
            occludes: false,
            hatch: None,
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
            bold: false,
        }];
        let right = [StatusSegment {
            text: " 1:1 ",
            fg: [7, 8, 9],
            bg: [10, 11, 12],
            bold: false,
        }];
        let status = StatusBar {
            left: &left,
            boxes: &[],
            dots: &[],
            scales: &[],
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
            occludes: false,
            hatch: None,
        });
        // advance(" NOR ") = 5 * 160 / 16 = 50; right start = 320 - 50 = 270.
        let left_bar = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 50,
            height: 16,
            color: [4, 5, 6],
            occludes: false,
            hatch: None,
        });
        let right_bar = encode_bar(&BarCommand {
            x: 270,
            y: 0,
            width: 50,
            height: 16,
            color: [10, 11, 12],
            occludes: false,
            hatch: None,
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
            bold: false,
        }];
        let right = [StatusSegment {
            text: "xy",
            fg: [7, 8, 9],
            bg: [10, 11, 12],
            bold: false,
        }];
        let status = StatusBar {
            left: &left,
            boxes: &[],
            dots: &[],
            scales: &[],
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
            occludes: false,
            hatch: None,
        });
        let base_run = encode_text_run(&TextRunCommand {
            col: 0,
            row: 0,
            scale: 160,
            color: [1, 2, 3],
            bg: None,
            bold: false,
            follow: 0,
            anchor: None,
            text: "ab".to_owned(),
        });
        let separator = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 320,
            height: 1,
            color: [60, 66, 77],
            occludes: false,
            hatch: None,
        });
        let distinct_bar = encode_bar(&BarCommand {
            x: 300,
            y: 0,
            width: 20,
            height: 16,
            color: [10, 11, 12],
            occludes: false,
            hatch: None,
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

    /// A box takes the fewest whole cells that hold its run and centers the run
    /// in them. Its panel breaks the hairline and comes before its run, and the
    /// left run packs on from the box's end. A bar too narrow for the box cuts
    /// it at the edge.
    #[test]
    fn a_box_holds_its_run_in_whole_cells() {
        let left = [
            StatusSegment {
                text: " NOR ",
                fg: [1, 2, 3],
                bg: [4, 5, 6],
                bold: false,
            },
            StatusSegment {
                text: "ab",
                fg: [7, 8, 9],
                bg: [0, 0, 0],
                bold: false,
            },
        ];
        let boxes = [SegmentBox {
            segments: 0..1,
            panel: Some(Panel::chip([4, 5, 6])),
        }];
        let draw = |width: u16| {
            let status = StatusBar {
                left: &left,
                boxes: &boxes,
                dots: &[],
                scales: &[],
                right: &[],
                scale: 232,
                separator: [60, 66, 77],
                bg: [0, 0, 0],
            };
            let area = Rect::new(2, 3, width, 1);
            let mut buf = Buffer::empty(area);
            let mut scene = ApcScene::new();
            status.draw_components(area, &mut buf, &mut scene);
            scene.bytes().to_vec()
        };
        let hairline = |from: u16, to: u16| {
            encode_bar(&BarCommand {
                x: 32 + from as i16 * 16,
                y: 48,
                width: (to - from) * 16,
                height: 1,
                color: [60, 66, 77],
                occludes: false,
                hatch: None,
            })
        };
        let panel = |width: u16| {
            encode_panel(&PanelCommand {
                top: 3,
                left: 2,
                width,
                height: 1,
                style: BorderStyle::None_,
                border: [4, 5, 6],
                corner_radius: 1,
                fill: Some([4, 5, 6]),
                shadow: PanelShadow::None_,
                inset_x: 0,
                inset_y: 1,
                corners: 0,
                fillets: 0,
                hatch: None,
                above_pools: false,
                backdrop: false,
                anchor: None,
            })
        };
        let run = |col: i16, text: &str, color: [u8; 3]| {
            encode_text_run(&TextRunCommand {
                col,
                row: 48,
                scale: 232,
                color,
                bg: None,
                bold: false,
                follow: 0,
                anchor: None,
                text: text.to_owned(),
            })
        };

        // " NOR " advances 73 sixteenths at scale 232, so the box takes five
        // cells and its run sits (80 - 73) / 2 = 3 sixteenths in. The left run
        // goes on at the box's end, 32 + 80. Four cells hold the box's first
        // 61 sixteenths of text, four glyphs.
        assert_eq!(
            [draw(20), draw(4)],
            [
                [
                    hairline(5, 20),
                    panel(5),
                    run(35, " NOR ", [1, 2, 3]),
                    run(112, "ab", [7, 8, 9]),
                ]
                .concat(),
                [panel(4), run(35, " NOR", [1, 2, 3])].concat(),
            ],
        );
    }

    /// A box after a plain segment starts on the next whole cell and centers
    /// its whole run. A box with a panel breaks the hairline and draws no
    /// background bar for its segments, and a dot segment draws a square in
    /// place of its glyph.
    #[test]
    fn a_box_aligns_its_run_and_a_dot_draws_a_square() {
        let segment = |text, fg, bg| StatusSegment {
            text,
            fg,
            bg,
            bold: false,
        };
        let left = [
            segment("a", [1, 1, 1], [0, 0, 0]),
            segment("bc", [2, 2, 2], [9, 9, 9]),
            segment(".", [3, 3, 3], [9, 9, 9]),
            segment("d", [4, 4, 4], [9, 9, 9]),
        ];
        let boxes = [SegmentBox {
            segments: 1..4,
            panel: Some(Panel::chip([9, 9, 9])),
        }];
        let status = StatusBar {
            left: &left,
            boxes: &boxes,
            dots: &[2],
            scales: &[],
            right: &[],
            scale: 160,
            separator: [60, 66, 77],
            bg: [0, 0, 0],
        };
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        status.draw_components(area, &mut buf, &mut scene);

        let run = |col: i16, text: &str, color: [u8; 3]| {
            encode_text_run(&TextRunCommand {
                col,
                row: 0,
                scale: 160,
                color,
                bg: None,
                bold: false,
                follow: 0,
                anchor: None,
                text: text.to_owned(),
            })
        };
        let bar = |x: i16, y: i16, width: u16, height: u16, color: [u8; 3]| {
            encode_bar(&BarCommand {
                x,
                y,
                width,
                height,
                color,
                occludes: false,
                hatch: None,
            })
        };
        // "a" advances 10 sixteenths at scale 160, so the box starts on cell 1.
        // Its run advances 20 + 10 + 10 = 40, so it takes three cells and starts
        // 4 in: "bc" at 20, the dot's 10 from 40 with its square at 42, and "d"
        // at 50.
        let panel = encode_panel(&PanelCommand {
            top: 0,
            left: 1,
            width: 3,
            height: 1,
            style: BorderStyle::None_,
            border: [9, 9, 9],
            corner_radius: 1,
            fill: Some([9, 9, 9]),
            shadow: PanelShadow::None_,
            inset_x: 0,
            inset_y: 1,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        });
        assert_eq!(
            scene.bytes(),
            [
                bar(0, 0, 16, 1, [60, 66, 77]),
                bar(64, 0, 256, 1, [60, 66, 77]),
                run(0, "a", [1, 1, 1]),
                panel,
                run(20, "bc", [2, 2, 2]),
                bar(42, 7, 6, 3, [3, 3, 3]),
                run(50, "d", [4, 4, 4]),
            ]
            .concat(),
        );
    }

    /// The spans are the boxes the bar draws. A plain segment takes no span, a
    /// box rounds out to whole cells at any scale, and the bar's edge cuts the
    /// box it crosses and drops the boxes past it.
    #[test]
    fn box_spans_are_the_boxes_the_bar_draws() {
        let texts = ["ab", "cde", "fghij", "k"];
        let boxes = [1..3, 3..4];
        let spans = |scale: u16, width: u16| {
            box_spans(&texts, &boxes, &[], scale, width)
                .into_iter()
                .map(|span| (span.start, span.end))
                .collect::<Vec<_>>()
        };

        // At 160 "ab" advances 20, "cde" and "fghij" 30 and 50, and "k" 10. At
        // 256 each glyph takes a cell.
        assert_eq!(
            [spans(160, 20), spans(256, 20), spans(160, 6), spans(160, 2)],
            [
                vec![(32, 112), (112, 128)],
                vec![(32, 160), (160, 176)],
                vec![(32, 96)],
                vec![],
            ],
        );
    }

    /// A left segment with its own scale draws its run at that scale and
    /// advances by it, so the box around it, the segments after it, and the
    /// box's span all follow.
    #[test]
    fn a_segment_draws_and_advances_at_its_own_scale() {
        let segment = |text, fg| StatusSegment {
            text,
            fg,
            bg: [0, 0, 0],
            bold: false,
        };
        let left = [
            segment("ab", [1, 1, 1]),
            segment("cd", [2, 2, 2]),
            segment("e", [3, 3, 3]),
        ];
        let boxes = [SegmentBox {
            segments: 0..2,
            panel: None,
        }];
        let scales = [(1, 80)];
        let status = StatusBar {
            left: &left,
            boxes: &boxes,
            dots: &[],
            scales: &scales,
            right: &[],
            scale: 160,
            separator: [60, 66, 77],
            bg: [0, 0, 0],
        };
        let area = Rect::new(0, 0, 20, 1);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        status.draw_components(area, &mut buf, &mut scene);

        let run = |col: i16, text: &str, scale: u16, color: [u8; 3]| {
            encode_text_run(&TextRunCommand {
                col,
                row: 0,
                scale,
                color,
                bg: None,
                bold: false,
                follow: 0,
                anchor: None,
                text: text.to_owned(),
            })
        };
        let hairline = encode_bar(&BarCommand {
            x: 0,
            y: 0,
            width: 320,
            height: 1,
            color: [60, 66, 77],
            occludes: false,
            hatch: None,
        });
        let runs: Vec<Range<usize>> = boxes
            .iter()
            .map(|segment_box| segment_box.segments.clone())
            .collect();
        let spans: Vec<_> = box_spans(&["ab", "cd", "e"], &runs, &scales, 160, 20)
            .into_iter()
            .map(|span| (span.start, span.end))
            .collect();
        // "ab" advances 20 at scale 160 and "cd" 10 at 80, so the box takes two
        // cells and starts its run 1 in, with "ab" at 1 and "cd" at 21. "e"
        // follows the box at 32.
        assert_eq!(
            (scene.bytes(), spans),
            (
                &[
                    hairline,
                    run(1, "ab", 160, [1, 1, 1]),
                    run(21, "cd", 80, [2, 2, 2]),
                    run(32, "e", 160, [3, 3, 3]),
                ]
                .concat()[..],
                vec![(0, 32)],
            ),
        );
    }
}
