//! Where a walkthrough stop's marks, card, and labels go, and when each draws.
//!
//! One stop becomes a [`Slide`]: a mark around its focus, a narration card, a
//! label box per annotation the reader has reached, and a table saying when
//! each part starts and how long it takes. [`layout`] is the whole of it, and it
//! is pure.
//!
//! An annotation draws no mark of its own. The caller shows its code by color
//! instead, so a stop with several annotations is not a stack of strokes over
//! the lines they name. Only the focus is enclosed, because that one outline is
//! what the reader orients by.
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
use std::ops::RangeInclusive;

/// Sixteenths in one cell, the unit a mark's geometry is stated in.
///
/// The sketch widgets place marks at sub-cell resolution so a stroke tracks
/// live font zoom, which is why the padding below is in sixteenths rather than
/// whole cells.
const CELL: i32 = 16;

/// A run of cells on one or more rows, as the caller measured them on screen.
///
/// Rows and columns are absolute screen cells, not pane-relative. A range whose
/// rows all scrolled out of view is reported by an empty [`Self::rows`] rather
/// than by an absent range, so the caller does not have to decide what "off
/// screen" means.
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

/// The shape the focus mark takes around the stop's subject.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Mark {
    /// A single row, circled. A ring says "this word" without covering it.
    Ellipse(SixteenthRect),
    /// Several rows, boxed. A ring around a block clears the corners only from
    /// far enough out to sit over the code beside it.
    Rect(SixteenthRect),
}

/// One annotation's code, its label box, and whether a line joins them.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Callout {
    /// The annotation's index in the stop, which is what matches a callout to
    /// the text it came from and to a marker color.
    pub(crate) key: usize,
    /// The annotation's cells as measured, where the connector starts.
    ///
    /// Its rows are never empty, since an annotation off screen places no
    /// callout.
    pub(crate) range: CellRange,
    /// The label box, in whole cells.
    pub(crate) label: Rect,
    /// Whether a connector joins the code to the label. False when the label
    /// sits on the annotation's own row, which already says what it names.
    pub(crate) link: bool,
}

/// A part of a slide, for the timing table.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Part {
    Focus,
    FocusLink,
    Card,
    /// The `k`th annotation's connector and label.
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
    /// One callout per annotation the reader has reached.
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
    /// a stop with six labels still reads as being about one of them.
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
    /// The cells the pane paints buffer text into, which is its content area
    /// minus the gutter and the minimap strip. Marks and boxes are clamped into
    /// it, so nothing draws over either.
    pub(crate) pane: Rect,
    pub(crate) focus: Option<CellRange>,
    pub(crate) annotations: Vec<AnnotationCells>,
    /// Where the text ends on each visible row, so a box lands past the text
    /// rather than over it.
    pub(crate) line_ends: Vec<(u16, u16)>,
    /// The card's size in whole cells, or `None` when the stop has no
    /// narration.
    pub(crate) card: Option<(u16, u16)>,
    /// The key of the annotation the reader is on, or `None` while the reader
    /// is on the focus.
    ///
    /// Annotations past it are not yet reached and place no callout.
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
    let timing = choreograph(
        input.start_offset_ms,
        focus,
        focus_link,
        card.is_some(),
        &callouts,
    );

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
/// A word is circled and everything else is boxed: a block of rows, and a
/// single row too long for a ring to clear. Both pad outward from the cells
/// they cover, the ring more than the box: a ring's widest point is its middle,
/// so it needs more room beside the word than a box needs beside a block.
fn focus_mark(range: &CellRange, input: &SlideInput) -> Option<Mark> {
    let (&first, &last) = (range.rows.first()?, range.rows.last()?);
    let one_row = first == last;
    let cells = range.end_x.saturating_sub(range.start_x) + 1;
    let circled = one_row && cells <= RING_MAX_CELLS;

    // A block's box reaches the longest of the rows it covers, so it encloses
    // the code rather than cutting through the line that sticks out furthest. A
    // single row is a substring of its own line, so it reaches its own end
    // rather than boxing code the stop does not name.
    let right = match one_row {
        true => range.end_x,
        false => range
            .rows
            .iter()
            .filter_map(|row| line_end(input, *row))
            .max()
            .unwrap_or(range.end_x),
    };

    let (pad_x, pad_y) = match circled {
        true => (ELLIPSE_PAD_X, ELLIPSE_PAD_Y),
        false => (RECT_PAD_X, RECT_PAD_Y),
    };
    let rect = pad_cells(
        input.pane,
        range.start_x,
        right + 1,
        first,
        last + 1,
        pad_x,
        pad_y,
    );

    Some(match circled {
        true => Mark::Ellipse(rect),
        false => Mark::Rect(rect),
    })
}

/// Where the text ends on `row`, if the caller measured it.
fn line_end(input: &SlideInput, row: u16) -> Option<u16> {
    input
        .line_ends
        .iter()
        .find(|(at, _)| *at == row)
        .map(|(_, end)| *end)
}

/// Widest single-row range a ring still clears, in cells.
///
/// A ring crosses the glyph band, about 7 px either side of a 20 px row's
/// center, at 88 percent of its half-width. Past this the stroke runs through
/// the characters at the range's ends: half a glyph each end at 20 cells, two
/// glyphs each end at 40. A box clears its contents at every point, so a longer
/// line takes one.
const RING_MAX_CELLS: u16 = 12;

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

/// Cells between the longest line beside a label and its box.
const LABEL_GAP: u16 = 4;

/// Place the label box of each annotation the reader has reached.
///
/// A label never covers code. Its rows are searched outward from the
/// annotation's first row, and each candidate sits [`LABEL_GAP`] past the
/// longest line among the rows the box spans and the rows its connector
/// crosses. The first candidate that fits the pane and clears the card and
/// every earlier label is taken.
///
/// A label that fits nowhere draws nothing. A label over code hides what the
/// stop is about, which costs the reader more than one missing label.
///
/// An annotation whose code is off screen contributes no callout, for the same
/// reason a focus does. A connector to clamped cells points at whatever
/// scrolled into their place.
fn place_callouts(input: &SlideInput, card: Option<Rect>) -> Vec<Callout> {
    let mut placed: Vec<Rect> = Vec::new();
    let mut callouts = Vec::new();

    let reached = input.current.map_or(0, |at| at + 1);
    for annotation in input
        .annotations
        .iter()
        .filter(|annotation| annotation.key < reached)
    {
        let Some((width, height)) = label_size(&annotation.label_lines) else {
            continue;
        };
        let Some(&first) = annotation.range.rows.first() else {
            continue;
        };

        let label = label_rows(first, input.pane)
            .map(|y| {
                // The connector crosses every row between the annotation's and
                // the box's, so with the box's own rows they form one run.
                let band = first.min(y)..=first.max(y + height - 1);
                Rect {
                    x: widest_end(input, band, annotation.range.end_x) + LABEL_GAP,
                    y,
                    width,
                    height,
                }
            })
            .find(|box_| {
                fits(input.pane, box_.x, box_.y, width, height)
                    && card.is_none_or(|card| !overlaps(*box_, card))
                    && placed.iter().all(|earlier| !overlaps(*box_, *earlier))
            });

        let Some(label) = label else {
            continue;
        };
        placed.push(label);
        callouts.push(Callout {
            key: annotation.key,
            range: annotation.range.clone(),
            label,
            // A label sitting on its annotation's own row needs no line to say
            // which code it belongs to. One that moved does.
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

/// The pane rows a label is tried at, by distance from `first`, below before
/// above at each distance.
///
/// The annotation's own row leads, so a label reads as belonging to the line it
/// names. Growing one row at a time keeps a label near its code, and running
/// to the pane's edges lets a crowded stop stack its labels rather than drop
/// them.
fn label_rows(first: u16, pane: Rect) -> impl Iterator<Item = u16> {
    let rows = pane.y..pane.y + pane.height;
    (0..=pane.height)
        .flat_map(move |distance| {
            let below = first.checked_add(distance);
            let above = first.checked_sub(distance).filter(|_| distance > 0);
            [below, above]
        })
        .flatten()
        .filter(move |row| rows.contains(row))
}

/// Where the longest line among `rows` ends.
///
/// A row the caller did not measure counts as ending at `unmeasured`.
fn widest_end(input: &SlideInput, rows: RangeInclusive<u16>, unmeasured: u16) -> u16 {
    rows.map(|row| line_end(input, row).unwrap_or(unmeasured))
        .max()
        .unwrap_or(unmeasured)
}

/// Whether two boxes share any cell.
fn overlaps(a: Rect, b: Rect) -> bool {
    a.x < b.x + b.width && b.x < a.x + a.width && a.y < b.y + b.height && b.y < a.y + a.height
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

    /// A callout's label opens before its connector quite reaches it, so one
    /// annotation reads as a single gesture rather than two.
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
    card: bool,
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
    } else if card {
        // A card with no link to wait for opens with the slide. It is still in
        // the table, so it takes the offset a retiring slide adds rather than
        // drawing over what is on its way out.
        timing.push((Part::Card, start_offset, c::CARD_MS));
    }

    for (index, callout) in callouts.iter().enumerate() {
        let link_at = shift(after, c::ANNOTATION_DELAY_MS)
            + c::ANNOTATION_STRIDE_MS.saturating_mul(index as u16);
        let label_at = match callout.link {
            true => {
                timing.push((Part::Link(callout.key), link_at, c::ANNOTATION_LINK_MS));
                shift(
                    link_at + c::ANNOTATION_LINK_MS,
                    c::ANNOTATION_LABEL_DELAY_MS,
                )
            },
            false => link_at,
        };
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

    /// Each callout's label box and whether a connector joins it to its code.
    fn placed(callouts: &[Callout]) -> Vec<(Rect, bool)> {
        callouts
            .iter()
            .map(|callout| (callout.label, callout.link))
            .collect()
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

    /// A ring clears a word at the widest point of its own curve, which is what
    /// bounds how long a range it can circle.
    #[test]
    fn a_twelve_cell_range_is_still_circled() {
        let start = 8;
        let end = start + RING_MAX_CELLS - 1;
        let slide = layout(&input(pane(), Some(range(&[4], start, end))));

        let Some(Mark::Ellipse(rect)) = slide.focus else {
            panic!("a range the ring clears is circled, got {:?}", slide.focus);
        };
        assert_eq!(
            (rect.x, i32::from(rect.x) + i32::from(rect.w)),
            (
                start as i16 * 16 - ELLIPSE_PAD_X as i16,
                i32::from(end + 1) * 16 + ELLIPSE_PAD_X,
            ),
            "padded past the word it circles",
        );
    }

    /// Past that the ring's stroke crosses the glyph band inside the range, so
    /// it strikes through the characters at either end. A box clears them.
    #[test]
    fn a_wider_single_row_range_is_boxed_to_its_own_end() {
        let (start, end) = (4, 33);
        let mut input = input(pane(), Some(range(&[4], start, end)));
        // A line running well past the range, which a block's box would reach
        // and a single row's must not.
        input.line_ends = vec![(4, 60)];

        let Some(Mark::Rect(rect)) = layout(&input).focus else {
            panic!("a range longer than a ring clears is boxed");
        };
        assert_eq!(
            (rect.x, i32::from(rect.x) + i32::from(rect.w)),
            (
                start as i16 * 16 - RECT_PAD_X as i16,
                i32::from(end + 1) * 16 + RECT_PAD_X,
            ),
            "boxed to the range's own end, not the line's",
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
        input.current = Some(1);

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
    /// A tall label on a short line hangs down beside the focus block, so it
    /// starts past the block's longest line rather than past its own.
    #[test]
    fn a_label_never_covers_the_focus_text() {
        let mut input = input(pane(), Some(range(&[10, 11, 12], 4, 60)));
        // The annotation sits on a short line just above a long focus block, so
        // a tall label on its own row reaches down beside the focus rows.
        input.line_ends = (0..30)
            .map(|row| (row, if (10..=12).contains(&row) { 60 } else { 20 }))
            .collect();
        input.card = None;
        input.annotations = vec![annotation(0, &[8], 4, 8, &["one", "two", "three"])];
        input.current = Some(0);

        let slide = layout(&input);
        let [callout] = slide.callouts.as_slice() else {
            panic!(
                "one annotation makes one callout, got {}",
                slide.callouts.len()
            );
        };

        assert_eq!(
            callout.label,
            Rect {
                x: 60 + LABEL_GAP,
                y: 8,
                width: 7,
                height: 5,
            },
            "the label keeps its own row and starts past the focus text it hangs beside",
        );
    }

    /// Only one label fits on a row that two annotations share, so the second
    /// stacks directly under the first, in the same column past the text.
    #[test]
    fn a_second_label_on_the_same_row_stacks_under_the_first() {
        let mut input = input(pane(), None);
        input.card = None;
        input.annotations = vec![
            annotation(0, &[10], 4, 8, &["first note"]),
            annotation(1, &[10], 12, 16, &["second note"]),
        ];
        input.current = Some(1);

        assert_eq!(
            placed(&layout(&input).callouts),
            [
                (
                    Rect {
                        x: 44,
                        y: 10,
                        width: 12,
                        height: 3,
                    },
                    false,
                ),
                (
                    Rect {
                        x: 44,
                        y: 13,
                        width: 13,
                        height: 3,
                    },
                    true,
                ),
            ],
            "the first on its own row, the second under it with a connector back",
        );
    }

    /// The labels placed for one annotation on `row` when every line ends at
    /// column 20 but `long_row`'s, which ends at 60, and `card` holds part of
    /// the pane.
    fn placed_beside_long_row(row: u16, long_row: u16, card: Rect) -> Vec<(Rect, bool)> {
        let mut input = input(pane(), None);
        input.line_ends = (0..30)
            .map(|at| (at, if at == long_row { 60 } else { 20 }))
            .collect();
        input.annotations = vec![annotation(0, &[row], 4, 8, &["note"])];
        input.current = Some(0);
        placed(&place_callouts(&input, Some(card)))
    }

    /// A label pushed off its row starts past the longest line it spans. A
    /// column measured from the label's own row alone puts the box over the
    /// longer line below.
    #[test]
    fn a_label_clears_the_longest_row_it_spans() {
        // Over the rest of the annotation's row, so the label has to leave it.
        let card = Rect {
            x: 24,
            y: 11,
            width: pane().width - 24,
            height: 1,
        };

        assert_eq!(
            placed_beside_long_row(11, 12, card),
            [(
                Rect {
                    x: 60 + LABEL_GAP,
                    y: 12,
                    width: 6,
                    height: 3,
                },
                true,
            )],
            "one row down, past the long line it spans",
        );
    }

    /// A label also starts past the longest line its connector crosses, so a
    /// label that leaves its row stays right of the code between it and its
    /// annotation.
    #[test]
    fn a_label_clears_the_rows_its_connector_crosses() {
        // Over rows 12 to 16, so the nearest free rows are under the long one.
        let card = Rect {
            x: 24,
            y: 12,
            width: pane().width - 24,
            height: 5,
        };

        assert_eq!(
            placed_beside_long_row(14, 15, card),
            [(
                Rect {
                    x: 60 + LABEL_GAP,
                    y: 17,
                    width: 6,
                    height: 3,
                },
                true,
            )],
            "under the long line, and past it, since the connector crosses it",
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
        input.current = Some(2);

        let timing = layout(&input).timing;
        let starts: Vec<u16> = timing.iter().map(|(_, start, _)| *start).collect();
        assert!(
            starts.windows(2).all(|pair| pair[0] <= pair[1]),
            "starts run forward, got {starts:?}",
        );

        let label_at = |key: usize| {
            timing
                .iter()
                .find(|(part, ..)| *part == Part::Label(key))
                .map(|(_, start, _)| *start)
                .expect("every annotation has a label")
        };
        assert_eq!(
            label_at(2) - label_at(0),
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
        base.current = Some(0);

        let mut delayed = base.clone();
        delayed.start_offset_ms = 500;

        let starts = |input: &SlideInput| -> Vec<u16> {
            layout(input).timing.iter().map(|(_, at, _)| *at).collect()
        };
        let shifted: Vec<u16> = starts(&base).iter().map(|at| at + 500).collect();

        assert_eq!(starts(&delayed), shifted, "every part moved together");
    }

    /// A stop with no focus has nothing for its card to be linked from, so the
    /// card opens with the slide. It is still one of the slide's parts, which
    /// is what makes it wait out a retiring slide rather than drawing over it.
    #[test]
    fn a_card_without_a_link_still_takes_the_slides_offset() {
        let mut input = input(pane(), None);
        input.start_offset_ms = 500;

        let slide = layout(&input);

        assert!(!slide.focus_link, "nothing links a card with no focus");
        assert_eq!(
            slide
                .timing
                .iter()
                .find(|(part, ..)| *part == Part::Card)
                .map(|(_, at, duration)| (*at, *duration)),
            Some((500, choreography::CARD_MS)),
            "the card is scheduled at the offset the slide starts from",
        );
    }

    /// Until the reader walks into the annotations no callout is up. From then
    /// on exactly one is current, so a stop with six labels still reads as being
    /// about one of them.
    #[test]
    fn one_current_annotation_dims_the_rest() {
        let mut input = input(pane(), None);
        input.annotations = vec![
            annotation(0, &[10], 4, 8, &["one"]),
            annotation(1, &[14], 4, 8, &["two"]),
        ];

        let plain = layout(&input);
        assert!(
            plain.callouts.is_empty(),
            "no annotation is reached yet, got {:?}",
            plain.callouts,
        );

        input.current = Some(1);
        let walked = layout(&input);
        assert_eq!(walked.emphasis(1), Emphasis::Current);
        assert_eq!(walked.emphasis(0), Emphasis::Dimmed);
    }

    /// A stop opens on its focus alone, and each step onto an annotation adds
    /// that annotation's callout. A label already up keeps its place when the
    /// next one arrives, so a reveal never moves what the reader just read.
    #[test]
    fn an_annotation_not_yet_reached_places_no_callout() {
        let mut base = input(pane(), None);
        // Adjacent rows, so the second label has to move around the first.
        base.annotations = vec![
            annotation(0, &[10], 4, 8, &["one"]),
            annotation(1, &[11], 4, 8, &["two"]),
        ];
        let labels = |current: Option<usize>| -> Vec<(usize, Rect)> {
            let slide = layout(&SlideInput {
                current,
                ..base.clone()
            });
            slide
                .callouts
                .iter()
                .map(|callout| (callout.key, callout.label))
                .collect()
        };

        let both = labels(Some(1));
        assert_eq!(labels(None), Vec::new(), "the focus comes alone");
        assert_eq!(
            labels(Some(0)),
            both[..1],
            "the first label lands where it stays"
        );
    }

    /// The card carries the narration the whole stop is about, so a label over
    /// it hides more than the label says.
    #[test]
    fn a_label_never_covers_the_card() {
        let mut input = input(pane(), Some(range(&[4], 8, 11)));
        // Long enough that the label beside it reaches into the right margin
        // the card takes.
        input.annotations = vec![annotation(0, &[5], 4, 8, &["a label wide enough to reach"])];
        input.current = Some(0);

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
            "so it left the row beside its code, landing at {:?}",
            callout.label,
        );
    }
}
