//! Where a walkthrough stop's marks, card, and labels go, and when each draws.
//!
//! One stop becomes a [`Slide`]: a mark around its focus, a narration card, a
//! mark and a label box per annotation, and a table saying when each part
//! starts and how long it takes. [`layout`] is the whole of it, and it is pure.
//!
//! Placement is where the fiddly rules live. A label must not cover the code it
//! describes, the card must not sit over a long line, and a mark whose code
//! scrolled away must not draw a box around whatever took its place. Each rule
//! is one candidate test, and a pure function is what lets every one of them be
//! checked against an input literal with no editor, no terminal, and no clock.
//!
//! Nothing here reads a `Stoat`, a theme, or a config. The caller supplies the
//! geometry it already has and applies the colors it already resolved.

use crate::render::text::text_width;
use ratatui::layout::Rect;

/// Sixteenths in one cell, the unit a mark's geometry is stated in.
///
/// The sketch widgets place marks at sub-cell resolution so a stroke tracks
/// live font zoom, which is why the padding below is in sixteenths rather than
/// whole cells.
const CELL: i32 = 16;

/// A run of cells on one or more rows, as the caller measured them on screen.
///
/// Rows and columns are pane-relative. A range whose rows all scrolled out of
/// view is reported by an empty [`Self::rows`] rather than by an absent range,
/// so the caller does not have to decide what "off screen" means.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct CellRange {
    /// First and last row the range covers, inclusive, clamped to what is
    /// visible. Empty when none of it is on screen.
    pub(crate) rows: Vec<u16>,
    /// Column the range starts at on its first row.
    pub(crate) start_x: u16,
    /// Column the range ends at on its last row, inclusive.
    pub(crate) end_x: u16,
}

/// A rectangle in sixteenths of a cell, which is what a sketch widget takes.
///
/// Signed, because a mark pads outward from the cells it covers and a range at
/// the pane's left edge pads past it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct SixteenthRect {
    pub(crate) x: i16,
    pub(crate) y: i16,
    pub(crate) w: u16,
    pub(crate) h: u16,
}

/// The shape a mark takes around what it points at.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mark {
    /// A single row, circled. A ring says "this word" without covering it.
    Ellipse(SixteenthRect),
    /// Several rows, boxed. A ring around a block clears the corners only from
    /// far enough out to sit over the code beside it.
    Rect(SixteenthRect),
}

/// One annotation's mark, its label box, and whether a line joins them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Callout {
    /// The annotation's index in the stop, which is what matches a callout to
    /// the text it came from and to a marker color.
    pub(crate) key: usize,
    pub(crate) mark: Mark,
    /// The label box, in whole cells.
    pub(crate) label: Rect,
    /// Whether a connector joins the mark to the label. False when the label
    /// sits against the mark and a line between them has nowhere to go.
    pub(crate) link: bool,
}

/// A part of a slide, for the timing table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Part {
    Focus,
    FocusLink,
    Card,
    /// The `k`th annotation's mark, link, and label.
    Mark(usize),
    Link(usize),
    Label(usize),
}

/// How prominently one part draws.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Emphasis {
    /// Nothing is singled out, so everything draws alike.
    Plain,
    /// The part the reader is on.
    Current,
    /// Every other part, while one is current.
    Dimmed,
}

/// Everything a stop draws, and when.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Slide {
    pub(crate) focus: Option<Mark>,
    /// The narration card, in whole cells.
    pub(crate) card: Option<Rect>,
    /// Whether a connector joins the focus mark to the card.
    pub(crate) focus_link: bool,
    pub(crate) callouts: Vec<Callout>,
    /// When each part starts and how long it draws, in milliseconds from the
    /// stop's own zero.
    pub(crate) timing: Vec<(Part, u16, u16)>,
    /// Which annotation the reader is on, carried so [`Self::emphasis`]
    /// answers without the caller passing it back.
    current: Option<usize>,
}

impl Slide {
    /// How prominently annotation `key` draws.
    ///
    /// Everything is [`Emphasis::Plain`] until the reader walks into the
    /// annotations. From then on exactly one is current and the rest recede, so
    /// a stop with six marks still reads as being about one of them.
    pub(crate) fn emphasis(&self, key: usize) -> Emphasis {
        match self.current {
            None => Emphasis::Plain,
            Some(current) if current == key => Emphasis::Current,
            Some(_) => Emphasis::Dimmed,
        }
    }
}

/// One annotation as the caller measured it.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct AnnotationCells {
    pub(crate) key: usize,
    pub(crate) range: CellRange,
    /// The label's lines, already wrapped by the caller.
    ///
    /// A wrap needs the font and the text. Placement needs only the extent, so
    /// the split there keeps this function pure and the wrap testable alone.
    pub(crate) label_lines: Vec<String>,
}

/// The screen geometry one stop is laid out against.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct SlideInput {
    /// The editor's content area, minus the minimap strip. Marks and boxes are
    /// clamped into it, so nothing draws over the gutter or the strip.
    pub(crate) pane: Rect,
    pub(crate) focus: Option<CellRange>,
    pub(crate) annotations: Vec<AnnotationCells>,
    /// Where the text ends on each visible row, so a box lands past the text
    /// rather than over it.
    pub(crate) line_ends: Vec<(u16, u16)>,
    /// The card's size in whole cells, or `None` when the stop has no
    /// narration.
    pub(crate) card: Option<(u16, u16)>,
    pub(crate) current: Option<usize>,
    /// Whether the reader has dismissed the card. A hidden card takes no space
    /// and no connector points at it.
    pub(crate) card_hidden: bool,
    /// Milliseconds every part waits before its own start.
    ///
    /// A jump to a stop off-screen glides there first, and a slide drawn during
    /// that glide lands against the rows the pane leaves behind.
    pub(crate) start_offset_ms: u16,
}

/// Lay out one stop.
pub(crate) fn layout(input: &SlideInput) -> Slide {
    let focus = input
        .focus
        .as_ref()
        .and_then(|range| focus_mark(range, input));

    let card = match (input.card, input.card_hidden) {
        (Some((width, height)), false) => place_card(input, width, height),
        _ => None,
    };

    let callouts = place_callouts(input, card);
    let focus_link = focus.is_some() && card.is_some();
    let timing = choreograph(input.start_offset_ms, focus, focus_link, &callouts);

    Slide {
        focus,
        card,
        focus_link,
        callouts,
        timing,
        current: input.current,
    }
}

/// The mark around a stop's focus, or `None` when its code is off screen.
///
/// A range on one row is circled and one spanning rows is boxed. Both pad
/// outward from the cells they cover, the ring more than the box: a ring's
/// widest point is its middle, so it needs more room beside the word than a box
/// needs beside a block.
fn focus_mark(range: &CellRange, input: &SlideInput) -> Option<Mark> {
    let (&first, &last) = (range.rows.first()?, range.rows.last()?);

    if first == last {
        return Some(Mark::Ellipse(pad_cells(
            input.pane,
            range.start_x,
            range.end_x + 1,
            first,
            first + 1,
            ELLIPSE_PAD_X,
            ELLIPSE_PAD_Y,
        )));
    }

    // A block's box reaches the longest of the rows it covers, so it encloses
    // the code rather than cutting through the line that sticks out furthest.
    let widest = range
        .rows
        .iter()
        .filter_map(|row| line_end(input, *row))
        .max()
        .unwrap_or(range.end_x);

    Some(Mark::Rect(pad_cells(
        input.pane,
        range.start_x,
        widest + 1,
        first,
        last + 1,
        RECT_PAD_X,
        RECT_PAD_Y,
    )))
}

/// Where the text ends on `row`, if the caller measured it.
fn line_end(input: &SlideInput, row: u16) -> Option<u16> {
    input
        .line_ends
        .iter()
        .find(|(at, _)| *at == row)
        .map(|(_, end)| *end)
}

/// Sixteenths a circled word is padded by on each axis.
///
/// A ring's widest point is its middle, so it clears the word only by bulging
/// past it. Less on the vertical, where a row's glyphs do not fill the cell.
const ELLIPSE_PAD_X: i32 = 10;
const ELLIPSE_PAD_Y: i32 = 4;

/// Sixteenths a boxed block is padded by. Tighter than a ring's, since a box
/// clears its contents at every point rather than only at the middle.
const RECT_PAD_X: i32 = 8;
const RECT_PAD_Y: i32 = 4;

/// Turn a cell rectangle into a padded sixteenths one, clamped to `pane`.
///
/// The column and row bounds are half-open, so `x1` and `y1` name the cell past
/// the last covered one.
fn pad_cells(
    pane: Rect,
    x0: u16,
    x1: u16,
    y0: u16,
    y1: u16,
    pad_x: i32,
    pad_y: i32,
) -> SixteenthRect {
    let bound = |value: i32, low: i32, high: i32| value.clamp(low, high);
    let (pane_x0, pane_x1) = (
        i32::from(pane.x) * CELL,
        i32::from(pane.x + pane.width) * CELL,
    );
    let (pane_y0, pane_y1) = (
        i32::from(pane.y) * CELL,
        i32::from(pane.y + pane.height) * CELL,
    );

    let left = bound(i32::from(x0) * CELL - pad_x, pane_x0, pane_x1);
    let right = bound(i32::from(x1) * CELL + pad_x, pane_x0, pane_x1);
    let top = bound(i32::from(y0) * CELL - pad_y, pane_y0, pane_y1);
    let bottom = bound(i32::from(y1) * CELL + pad_y, pane_y0, pane_y1);

    SixteenthRect {
        x: left as i16,
        y: top as i16,
        w: (right - left).max(0) as u16,
        h: (bottom - top).max(0) as u16,
    }
}

/// Cells the card keeps clear of the pane's right edge.
const CARD_MARGIN: u16 = 1;

/// Cells a line must stay clear of the card's left edge for the right margin to
/// be free.
///
/// Without the gap the card's stroke lands against the last character of the
/// longest line, which reads as covering it even where it does not.
const CARD_CLEARANCE: u16 = 3;

/// Rows between the focus block and a card placed under or over it.
const CARD_GAP: u16 = 2;

/// Place the narration card, or `None` when it fits nowhere in the pane.
///
/// The right margin comes first, because a card beside the code leaves every
/// line of it readable. It is taken only when no line among the focus rows
/// reaches into it; otherwise the card sits below the focus block, then above
/// it, and finally in the right margin anyway.
///
/// The last candidate is what makes this total. A pane with no room anywhere
/// still has to put the narration somewhere, and a card over one long line is
/// better than a stop with no narration at all.
fn place_card(input: &SlideInput, width: u16, height: u16) -> Option<Rect> {
    if width > input.pane.width || height > input.pane.height {
        return None;
    }

    let margin_x = input.pane.x + input.pane.width.saturating_sub(width + CARD_MARGIN);
    let rows = input.focus.as_ref().map(|focus| focus.rows.clone());
    let (top, bottom, left) = match rows.as_deref() {
        Some([first, .., last]) => (*first, *last, focus_left(input)),
        Some([only]) => (*only, *only, focus_left(input)),
        _ => {
            // With no focus on screen there is nothing for the card to avoid,
            // so the margin is free by definition.
            return Some(clamp_rect(
                input.pane,
                margin_x,
                input.pane.y,
                width,
                height,
            ));
        },
    };

    let margin_free = rows
        .iter()
        .flatten()
        .filter_map(|row| line_end(input, *row))
        .all(|end| end + CARD_CLEARANCE < margin_x);

    let candidates = [
        margin_free.then_some((margin_x, top)),
        Some((left, bottom + 1 + CARD_GAP)),
        top.checked_sub(height + CARD_GAP).map(|y| (left, y)),
        Some((margin_x, top)),
    ];

    let placed = candidates
        .into_iter()
        .flatten()
        .find(|&(x, y)| fits(input.pane, x, y, width, height))
        .unwrap_or((margin_x, input.pane.y));

    Some(clamp_rect(input.pane, placed.0, placed.1, width, height))
}

/// The leftmost column the focus covers on any of its visible rows.
fn focus_left(input: &SlideInput) -> u16 {
    input
        .focus
        .as_ref()
        .map_or(input.pane.x, |focus| focus.start_x)
}

/// Whether a box of `width` by `height` at `(x, y)` lies wholly inside `pane`.
fn fits(pane: Rect, x: u16, y: u16, width: u16, height: u16) -> bool {
    x >= pane.x
        && y >= pane.y
        && x + width <= pane.x + pane.width
        && y + height <= pane.y + pane.height
}

/// A box pushed into `pane`, so a candidate that hangs over an edge still draws
/// somewhere sensible rather than off screen.
fn clamp_rect(pane: Rect, x: u16, y: u16, width: u16, height: u16) -> Rect {
    let width = width.min(pane.width);
    let height = height.min(pane.height);
    Rect {
        x: x.clamp(pane.x, pane.x + pane.width - width),
        y: y.clamp(pane.y, pane.y + pane.height - height),
        width,
        height,
    }
}

/// Cells between an annotation's line end and its label box.
const LABEL_GAP: u16 = 4;

/// Rows a label candidate is tried at, relative to the annotation's first row,
/// in the order they are tried.
///
/// The annotation's own row first, so a label reads as belonging to the line it
/// names. Then one row out either way, then two, which keeps a label near its
/// mark rather than sliding to wherever there happens to be room.
const LABEL_ROW_OFFSETS: [i32; 5] = [0, 1, -1, 2, -2];

/// Place each annotation's mark and label box.
///
/// A label never covers the code it describes. Every candidate is rejected if
/// it overlaps the card, an earlier label, the focus rows, or the annotation's
/// own rows, and the first that clears all four is taken.
///
/// An annotation whose code is off screen contributes no callout, for the same
/// reason a focus does: a clamped mark points at whatever scrolled into its
/// place.
fn place_callouts(input: &SlideInput, card: Option<Rect>) -> Vec<Callout> {
    let focus_rows: Vec<u16> = input
        .focus
        .as_ref()
        .map(|focus| focus.rows.clone())
        .unwrap_or_default();

    let mut placed: Vec<Rect> = Vec::new();
    let mut callouts = Vec::new();

    for annotation in &input.annotations {
        let Some(mark) = focus_mark(&annotation.range, input) else {
            continue;
        };
        let Some((width, height)) = label_size(&annotation.label_lines) else {
            continue;
        };
        let Some(&first) = annotation.range.rows.first() else {
            continue;
        };

        let x = line_end(input, first).unwrap_or(annotation.range.end_x) + LABEL_GAP;
        let below_focus = focus_rows.last().map(|last| last + 1 + CARD_GAP);

        let label = LABEL_ROW_OFFSETS
            .iter()
            .filter_map(|offset| offset_row(first, *offset).map(|y| (x, y)))
            .chain(below_focus.map(|y| (input.pane.x, y)))
            .map(|(x, y)| Rect {
                x,
                y,
                width,
                height,
            })
            .find(|box_| {
                fits(input.pane, box_.x, box_.y, width, height)
                    && card.is_none_or(|card| !overlaps(*box_, card))
                    && placed.iter().all(|earlier| !overlaps(*box_, *earlier))
                    && !covers_code(*box_, &focus_rows, input)
                    && !covers_code(*box_, &annotation.range.rows, input)
            });

        let Some(label) = label else {
            continue;
        };
        placed.push(label);
        callouts.push(Callout {
            key: annotation.key,
            mark,
            label,
            // A label sitting on its own row against its mark needs no line to
            // say which mark it belongs to. One that moved does.
            link: label.y != first,
        });
    }

    callouts
}

/// The label box's size in cells: the widest line plus a one-cell border on
/// each side, and one row per line plus the same.
///
/// `None` for a label with no lines, which draws nothing rather than an empty
/// box.
fn label_size(lines: &[String]) -> Option<(u16, u16)> {
    let widest = lines.iter().map(|line| text_width(line)).max()?;
    Some((widest as u16 + 2, lines.len() as u16 + 2))
}

/// `row` moved by `offset`, or `None` when that lands above the screen.
fn offset_row(row: u16, offset: i32) -> Option<u16> {
    let moved = i32::from(row) + offset;
    u16::try_from(moved).ok()
}

/// Whether two boxes share any cell.
fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
}

/// Whether `box_` covers the text on any of `rows`.
///
/// Sharing a row is not enough. A label placed past where that row's text ends
/// sits beside the code rather than over it, which is exactly where a label
/// belongs. Rejecting the whole row would push every label off the line it
/// names.
fn covers_code(box_: Rect, rows: &[u16], input: &SlideInput) -> bool {
    rows.iter().any(|row| {
        let shares_row = *row >= box_.y && *row < box_.y + box_.height;
        shares_row && box_.x <= line_end(input, *row).unwrap_or(input.pane.width)
    })
}

/// When each part of a slide starts and how long it draws.
///
/// One table rather than constants beside the placement code, because a stop's
/// pacing is a single design decision. Spread out, they drift apart the moment
/// one is tuned and the stop stops reading as one motion.
///
/// The negative offsets are deliberate. A part that started only after the one
/// before it finished would read as a sequence of separate drawings; starting
/// each slightly early makes them read as one hand moving.
mod choreography {
    /// A circled word draws in one stroke, so its duration is fixed.
    pub(super) const FOCUS_MS: u16 = 260;

    /// A boxed block scales with its perimeter, so a large box does not draw at
    /// the same speed a small one does and look rushed.
    pub(super) const FOCUS_PER_CELL_MS: u16 = 6;
    pub(super) const FOCUS_MIN_MS: u16 = 260;
    pub(super) const FOCUS_MAX_MS: u16 = 480;

    /// The connector leaves the focus mark just after it closes.
    pub(super) const FOCUS_LINK_DELAY_MS: i32 = 40;
    pub(super) const FOCUS_LINK_MS: u16 = 180;

    /// The card opens before the connector quite reaches it, so the line
    /// arrives at a box already there rather than at nothing.
    pub(super) const CARD_DELAY_MS: i32 = -60;
    pub(super) const CARD_MS: u16 = 200;

    /// Annotations follow the card, one after another, far enough apart to read
    /// as separate points.
    pub(super) const ANNOTATION_DELAY_MS: i32 = 60;
    pub(super) const ANNOTATION_STRIDE_MS: u16 = 140;
    pub(super) const ANNOTATION_MARK_MS: u16 = 220;

    /// A callout's own connector and label overlap their mark, so one
    /// annotation reads as a single gesture rather than three.
    pub(super) const ANNOTATION_LINK_DELAY_MS: i32 = -40;
    pub(super) const ANNOTATION_LINK_MS: u16 = 150;
    pub(super) const ANNOTATION_LABEL_DELAY_MS: i32 = -40;
    pub(super) const ANNOTATION_LABEL_MS: u16 = 150;
}

/// Build the timing table for one slide.
///
/// Every start is measured from the stop's own zero, with `start_offset` added
/// to all of them so a slide waiting out a glide shifts whole rather than
/// compressing.
fn choreograph(
    start_offset: u16,
    focus: Option<Mark>,
    focus_link: bool,
    callouts: &[Callout],
) -> Vec<(Part, u16, u16)> {
    use choreography as c;

    let mut timing = Vec::new();
    let mut after = start_offset;

    if let Some(mark) = focus {
        let duration = focus_duration(mark);
        timing.push((Part::Focus, start_offset, duration));
        after = start_offset + duration;
    }

    if focus_link {
        let link_at = shift(after, c::FOCUS_LINK_DELAY_MS);
        timing.push((Part::FocusLink, link_at, c::FOCUS_LINK_MS));

        let link_end = link_at + c::FOCUS_LINK_MS;
        let card_at = shift(link_end, c::CARD_DELAY_MS);
        timing.push((Part::Card, card_at, c::CARD_MS));
        after = card_at + c::CARD_MS;
    }

    for (index, callout) in callouts.iter().enumerate() {
        let mark_at = shift(after, c::ANNOTATION_DELAY_MS)
            + c::ANNOTATION_STRIDE_MS.saturating_mul(index as u16);
        timing.push((Part::Mark(callout.key), mark_at, c::ANNOTATION_MARK_MS));

        let mark_end = mark_at + c::ANNOTATION_MARK_MS;
        let link_at = shift(mark_end, c::ANNOTATION_LINK_DELAY_MS);
        if callout.link {
            timing.push((Part::Link(callout.key), link_at, c::ANNOTATION_LINK_MS));
        }

        let label_at = shift(
            link_at + c::ANNOTATION_LINK_MS,
            c::ANNOTATION_LABEL_DELAY_MS,
        );
        timing.push((Part::Label(callout.key), label_at, c::ANNOTATION_LABEL_MS));
    }

    // One annotation's parts overlap the next one's start by design, so build
    // order is not start order. Sorting here lets a caller read the table as
    // the sequence it describes rather than sorting it again.
    timing.sort_by_key(|(_, start, _)| *start);
    timing
}

/// How long a focus mark takes to draw.
///
/// A ring is one stroke at a fixed pace. A box scales with its perimeter, so a
/// block spanning ten rows does not draw as fast as one spanning two.
fn focus_duration(mark: Mark) -> u16 {
    use choreography as c;

    match mark {
        Mark::Ellipse(_) => c::FOCUS_MS,
        Mark::Rect(rect) => {
            let cells = (u32::from(rect.w) + u32::from(rect.h)) * 2 / CELL as u32;
            let scaled = cells.saturating_mul(u32::from(c::FOCUS_PER_CELL_MS));
            (scaled as u16).clamp(c::FOCUS_MIN_MS, c::FOCUS_MAX_MS)
        },
    }
}

/// A start time moved by a signed offset, floored at zero.
///
/// A negative offset larger than what precedes it means the part starts with
/// the slide rather than before it, which is the only sensible reading.
fn shift(at: u16, offset: i32) -> u16 {
    (i32::from(at) + offset).max(0) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pane wide enough for a card in the right margin, with the code well
    /// clear of it.
    fn pane() -> Rect {
        Rect {
            x: 0,
            y: 0,
            width: 100,
            height: 30,
        }
    }

    fn range(rows: &[u16], start_x: u16, end_x: u16) -> CellRange {
        CellRange {
            rows: rows.to_vec(),
            start_x,
            end_x,
        }
    }

    fn input(pane: Rect, focus: Option<CellRange>) -> SlideInput {
        SlideInput {
            pane,
            focus,
            annotations: Vec::new(),
            // Every visible row ends at column 40, so the right margin of the
            // wide pane is clear and a narrow pane's is not.
            line_ends: (0..30).map(|row| (row, 40)).collect(),
            card: Some((30, 6)),
            current: None,
            card_hidden: false,
            start_offset_ms: 0,
        }
    }

    fn annotation(
        key: usize,
        rows: &[u16],
        start_x: u16,
        end_x: u16,
        lines: &[&str],
    ) -> AnnotationCells {
        AnnotationCells {
            key,
            range: range(rows, start_x, end_x),
            label_lines: lines.iter().map(|line| (*line).to_owned()).collect(),
        }
    }

    /// A word gets a ring rather than a box, and the ring bulges past the word
    /// on every side. A ring drawn tight to the glyphs reads as underlining
    /// them.
    #[test]
    fn a_one_row_focus_is_a_ring_padded_past_the_word() {
        let slide = layout(&input(pane(), Some(range(&[4], 8, 11))));

        assert_eq!(
            slide.focus,
            Some(Mark::Ellipse(SixteenthRect {
                x: 8 * 16 - 10,
                y: 4 * 16 - 4,
                w: (12 - 8) * 16 + 20,
                h: 16 + 8,
            })),
            "four cells wide, padded on both axes",
        );
    }

    /// A block gets a box, and the box reaches the longest of the rows it
    /// covers. Sized to the range's own end column it would cut through
    /// whichever line sticks out furthest.
    #[test]
    fn a_multi_row_focus_is_a_box_reaching_its_longest_row() {
        let mut input = input(pane(), Some(range(&[4, 5, 6], 4, 12)));
        input.line_ends = vec![(4, 20), (5, 55), (6, 30)];

        let Some(Mark::Rect(rect)) = layout(&input).focus else {
            panic!("three rows box");
        };
        assert_eq!(rect.x, 4 * 16 - 8, "the block's own left edge");
        assert_eq!(
            i32::from(rect.x) + i32::from(rect.w),
            (55 + 1) * 16 + 8,
            "and the longest row's right one",
        );
        assert_eq!(rect.y, 4 * 16 - 4);
        assert_eq!(i32::from(rect.y) + i32::from(rect.h), 7 * 16 + 4);
    }

    /// A range that scrolled out of view has no rows to measure, and a mark
    /// clamped into the pane would circle whatever took its place.
    #[test]
    fn an_off_screen_focus_draws_no_mark() {
        let slide = layout(&input(pane(), Some(range(&[], 8, 11))));

        assert_eq!(slide.focus, None);
        assert!(!slide.focus_link, "and nothing points at the card");
    }

    /// A card beside the code leaves every line of it readable, so the right
    /// margin comes first when the code stays clear of it.
    #[test]
    fn a_wide_pane_puts_the_card_in_the_right_margin() {
        let slide = layout(&input(pane(), Some(range(&[4], 8, 11))));

        assert_eq!(
            slide.card,
            Some(Rect {
                x: 100 - 30 - 1,
                y: 4,
                width: 30,
                height: 6,
            }),
            "against the right edge, level with the focus",
        );
        assert!(slide.focus_link, "and a connector reaches it");
    }

    /// A card in the right margin of a narrow pane sits over the code, so the
    /// space under the focus block is taken instead.
    #[test]
    fn a_narrow_pane_puts_the_card_below_the_focus() {
        let narrow = Rect {
            x: 0,
            y: 0,
            width: 50,
            height: 30,
        };
        let slide = layout(&input(narrow, Some(range(&[4], 8, 11))));

        assert_eq!(
            slide.card,
            Some(Rect {
                x: 8,
                y: 4 + 1 + 2,
                width: 30,
                height: 6,
            }),
            "under the focus, at its left edge",
        );
    }

    /// A dismissed card takes no space, and a connector pointing at nothing is
    /// worse than no connector.
    #[test]
    fn a_hidden_card_leaves_no_card_and_no_link() {
        let mut input = input(pane(), Some(range(&[4], 8, 11)));
        input.card_hidden = true;

        let slide = layout(&input);
        assert_eq!(slide.card, None);
        assert!(!slide.focus_link);
        assert!(
            !slide
                .timing
                .iter()
                .any(|(part, ..)| matches!(part, Part::Card | Part::FocusLink)),
            "and neither is scheduled",
        );
    }

    /// Two labels on adjacent rows cannot both sit on their own row, so the
    /// second moves. Left overlapping, one would be unreadable.
    #[test]
    fn two_adjacent_annotations_get_labels_that_do_not_overlap() {
        let mut input = input(pane(), None);
        input.card = None;
        input.annotations = vec![
            annotation(0, &[10], 4, 8, &["first note"]),
            annotation(1, &[11], 4, 8, &["second note"]),
        ];

        let slide = layout(&input);
        let [first, second] = slide.callouts.as_slice() else {
            panic!(
                "two annotations make two callouts, got {}",
                slide.callouts.len()
            );
        };

        assert!(
            !overlaps(first.label, second.label),
            "{:?} and {:?} share cells",
            first.label,
            second.label,
        );
        assert_eq!(first.label.y, 10, "the first sits on its own row");
        assert_ne!(second.label.y, 11, "the second had to move");
    }

    /// A label over the focus text hides the code the whole stop is about.
    ///
    /// Sharing a row is fine, and is where a label belongs: past where that
    /// row's text ends. Only reaching back over the text is wrong.
    #[test]
    fn a_label_never_covers_the_focus_text() {
        let mut input = input(pane(), Some(range(&[10, 11, 12], 4, 60)));
        // The annotation sits on a short line just above a long focus block, so
        // a tall label starting beside it hangs down over the focus text.
        input.line_ends = (0..30)
            .map(|row| (row, if (10..=12).contains(&row) { 60 } else { 20 }))
            .collect();
        input.card = None;
        input.annotations = vec![annotation(0, &[8], 4, 8, &["one", "two", "three"])];

        let slide = layout(&input);
        let [callout] = slide.callouts.as_slice() else {
            panic!(
                "one annotation makes one callout, got {}",
                slide.callouts.len()
            );
        };

        assert!(
            !covers_code(callout.label, &[10, 11, 12], &input),
            "the label at {:?} hangs over the focus text",
            callout.label,
        );
        assert!(
            callout.label.y > 12,
            "so it moved clear of the block entirely, to {:?}",
            callout.label,
        );
    }

    /// A stop reads as one hand moving, which needs every part to start after
    /// the one before it.
    #[test]
    fn every_part_starts_after_the_one_before_it() {
        let mut input = input(pane(), Some(range(&[4], 8, 11)));
        input.annotations = vec![
            annotation(0, &[10], 4, 8, &["one"]),
            annotation(1, &[13], 4, 8, &["two"]),
            annotation(2, &[16], 4, 8, &["three"]),
        ];

        let timing = layout(&input).timing;
        let starts: Vec<u16> = timing.iter().map(|(_, start, _)| *start).collect();
        assert!(
            starts.windows(2).all(|pair| pair[0] <= pair[1]),
            "starts run forward, got {starts:?}",
        );

        let mark_at = |key: usize| {
            timing
                .iter()
                .find(|(part, ..)| *part == Part::Mark(key))
                .map(|(_, start, _)| *start)
                .expect("every annotation has a mark")
        };
        assert_eq!(
            mark_at(2) - mark_at(0),
            2 * choreography::ANNOTATION_STRIDE_MS,
            "annotations are evenly spaced",
        );
    }

    /// A large box drawn at a small one's pace looks rushed, so its duration
    /// scales with its perimeter.
    #[test]
    fn a_larger_focus_box_takes_longer_to_draw() {
        let duration = |rows: &[u16]| {
            // Short lines, so a two-row box lands inside the band the scaling
            // works in rather than at its ceiling.
            let mut small_pane = input(pane(), Some(range(rows, 4, 12)));
            small_pane.line_ends = (0..30).map(|row| (row, 12)).collect();
            let slide = layout(&small_pane);
            slide
                .timing
                .iter()
                .find(|(part, ..)| *part == Part::Focus)
                .map(|(_, _, duration)| *duration)
                .expect("a focus draws")
        };

        let small = duration(&[4, 5]);
        let large = duration(&(4..20).collect::<Vec<_>>());
        assert!(large > small, "{large} against {small}");
        assert!(large <= choreography::FOCUS_MAX_MS, "and stays capped");
    }

    /// A glide moves the pane under the slide, so a slide drawn during it lands
    /// against rows that are leaving. The whole thing shifts rather than
    /// compressing into what is left.
    #[test]
    fn a_start_offset_shifts_every_part_by_the_same_amount() {
        let mut base = input(pane(), Some(range(&[4], 8, 11)));
        base.annotations = vec![annotation(0, &[10], 4, 8, &["one"])];

        let mut delayed = base.clone();
        delayed.start_offset_ms = 500;

        let starts = |input: &SlideInput| -> Vec<u16> {
            layout(input).timing.iter().map(|(_, at, _)| *at).collect()
        };
        let shifted: Vec<u16> = starts(&base).iter().map(|at| at + 500).collect();

        assert_eq!(starts(&delayed), shifted, "every part moved together");
    }

    /// Until the reader walks into the annotations nothing is singled out. From
    /// then on exactly one is current, so a stop with six marks still reads as
    /// being about one of them.
    #[test]
    fn one_current_annotation_dims_the_rest() {
        let mut input = input(pane(), None);
        input.annotations = vec![
            annotation(0, &[10], 4, 8, &["one"]),
            annotation(1, &[14], 4, 8, &["two"]),
        ];

        let plain = layout(&input);
        assert_eq!(plain.emphasis(0), Emphasis::Plain);
        assert_eq!(plain.emphasis(1), Emphasis::Plain);

        input.current = Some(1);
        let walked = layout(&input);
        assert_eq!(walked.emphasis(1), Emphasis::Current);
        assert_eq!(walked.emphasis(0), Emphasis::Dimmed);
    }

    /// The card carries the narration the whole stop is about, so a label over
    /// it hides more than the label says.
    #[test]
    fn a_label_never_covers_the_card() {
        let mut input = input(pane(), Some(range(&[4], 8, 11)));
        // Long enough that the label beside it reaches into the right margin
        // the card takes.
        input.annotations = vec![annotation(0, &[5], 4, 8, &["a label wide enough to reach"])];

        let slide = layout(&input);
        let card = slide.card.expect("the card is placed");
        let [callout] = slide.callouts.as_slice() else {
            panic!(
                "one annotation makes one callout, got {}",
                slide.callouts.len()
            );
        };

        assert!(
            !overlaps(callout.label, card),
            "the label at {:?} sits on the card at {card:?}",
            callout.label,
        );
        assert_ne!(
            callout.label.y, 5,
            "so it left the row beside its mark, landing at {:?}",
            callout.label,
        );
    }
}
