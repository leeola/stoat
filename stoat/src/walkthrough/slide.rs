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
use std::ops::{Range, RangeInclusive};

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

/// One annotation's label box, and where a line joins it to the code.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct Callout {
    /// The annotation's index in the stop, which is what matches a callout to
    /// the text it came from and to a marker color.
    pub(crate) key: usize,
    /// The label box, in whole cells.
    pub(crate) label: Rect,
    /// The connector from the code to the label, or `None` when the label sits
    /// on the annotation's own row, which already says what it names.
    ///
    /// The layout plans the whole line because only it knows what lies between
    /// the code and the box. The text on each row, the card, and the other
    /// labels all lie there.
    pub(crate) link: Option<Link>,
}

/// The line a connector draws from an annotation's code to its label.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Link {
    /// Where the line leaves the code, in sixteenths of a cell.
    ///
    /// A line that leaves beside text on the annotation's row runs through
    /// that text on its way to the label, so the point depends on where each
    /// row's text ends.
    pub(crate) from: (i16, i16),
    /// Where the line meets the label, in sixteenths of a cell, just left of
    /// the label's left edge.
    pub(crate) to: (i16, i16),
    /// How far the line bows off its chord, in 64ths of the chord's length as
    /// the protocol states it, the sign picking the side. Zero draws it
    /// straight.
    pub(crate) bend: i8,
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
    /// The size of one cell in pixels, width then height, or `None` when the
    /// terminal reports none.
    ///
    /// The terminal bows a connector in pixels, so the bend that takes a line
    /// around a box depends on the cell's shape. An unknown size counts as a
    /// square cell.
    pub(crate) cell_pixels: Option<(u16, u16)>,
    /// The card's size in whole cells, or `None` when the stop has no
    /// narration.
    pub(crate) card: Option<(u16, u16)>,
    /// The key of the annotation the reader is on, or `None` while the reader
    /// is on the focus.
    ///
    /// Annotations past it are not yet reached and draw no callout. Their
    /// labels still hold their place in the plan.
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

/// Place the label of every annotation, and return the callouts the reader has
/// reached.
///
/// Every annotation is placed whether or not the reader has reached it. A
/// reveal therefore moves nothing, and a label already up was planned around
/// the labels still to come.
///
/// A label never covers code. The labels form one column past the text,
/// planned as one stack centered on the rows it names. Each box sits
/// [`LABEL_GAP`] past the longest line among the rows the box spans and the
/// rows its connector crosses. A card splits the column, as [`plan_labels`]
/// describes.
///
/// A connector arrives on its label's left side, and it bows only to get
/// around something in its way. The card, the other labels, and the text on
/// the rows it crosses are all in its way.
///
/// A label that fits nowhere draws nothing. A label over code hides what the
/// stop is about, which costs the reader more than one missing label.
///
/// An annotation whose code is off screen contributes no callout, for the same
/// reason a focus does. A connector to clamped cells points at whatever
/// scrolled into their place.
fn place_callouts(input: &SlideInput, card: Option<Rect>) -> Vec<Callout> {
    let sizes: Vec<(usize, u16, u16, u16)> = input
        .annotations
        .iter()
        .enumerate()
        .filter_map(|(index, annotation)| {
            let (width, height) = label_size(&annotation.label_lines)?;
            let first = *annotation.range.rows.first()?;
            Some((index, first, width, height))
        })
        .collect();
    let labels = plan_labels(input, card, &sizes);

    let aspect = input
        .cell_pixels
        .filter(|&(width, height)| width > 0 && height > 0)
        .map_or(1.0, |(width, height)| f32::from(height) / f32::from(width));
    let reached = input.current.map_or(0, |at| at + 1);

    let mut callouts = Vec::new();
    for (slot, (&(index, first, _, height), label)) in sizes.iter().zip(&labels).enumerate() {
        let annotation = &input.annotations[index];
        let (Some(label), Some(&last)) = (*label, annotation.range.rows.last()) else {
            continue;
        };
        if annotation.key >= reached {
            continue;
        }

        let link = link_point(input, &annotation.range, label.y).map(|from| {
            let to = link_end(from, label);
            let crossed = first.min(label.y)..=last.max(label.y + height - 1);
            let others: Vec<Rect> = labels
                .iter()
                .enumerate()
                .filter(|&(other, _)| other != slot)
                .filter_map(|(_, other)| *other)
                .collect();
            let obstacles = link_obstacles(input, card, &others, crossed);
            Link {
                from,
                to,
                bend: link_bend(from, to, &obstacles, aspect),
            }
        });
        callouts.push(Callout {
            key: annotation.key,
            label,
            link,
        });
    }

    callouts
}

/// The label box for each entry of `sizes`, or `None` for a label that fits
/// nowhere.
///
/// Each entry is an annotation's index in `input.annotations`, its first row,
/// and its label's width and height. The whole pane is one column at first.
///
/// When a box of that column lands on the card, the card splits the column,
/// since a label over the card hides the narration. The segments over and
/// under the card leave out its rows outright, so no box lands on it whatever
/// its column.
///
/// The labels whose code sits beside the card fill the room under it from the
/// last label up, after the labels whose code sits under the card. The first
/// label that does not fit goes over the card, and so does every label beside
/// the card before it. A run too tall for the rows under the card thus splits
/// around the card in order, and the rows over the card take the labels that
/// do not fit under it.
fn plan_labels(
    input: &SlideInput,
    card: Option<Rect>,
    sizes: &[(usize, u16, u16, u16)],
) -> Vec<Option<Rect>> {
    let pane_end = input.pane.y + input.pane.height;
    let ideals: Vec<(u16, u16)> = sizes
        .iter()
        .map(|&(_, first, _, height)| (first, height))
        .collect();

    let whole = label_boxes(input, sizes, &stack_rows(&ideals, input.pane.y, pane_end));
    let Some(card) =
        card.filter(|&card| whole.iter().flatten().any(|&label| overlaps(label, card)))
    else {
        return whole;
    };

    let under_card = card.y + card.height;
    let mut room_under = ideals
        .iter()
        .filter(|&&(first, _)| first >= under_card)
        .fold(pane_end.saturating_sub(under_card), |room, &(_, height)| {
            room.saturating_sub(height)
        });
    let mut goes_under = vec![false; ideals.len()];
    let mut filling = true;
    for (at, &(first, height)) in ideals.iter().enumerate().rev() {
        if first >= under_card {
            goes_under[at] = true;
        } else if first >= card.y && filling {
            filling = height <= room_under;
            if filling {
                room_under -= height;
            }
            goes_under[at] = filling;
        }
    }

    let mut rows = vec![None; ideals.len()];
    for (under, lo, hi) in [(false, input.pane.y, card.y), (true, under_card, pane_end)] {
        let members: Vec<usize> = (0..ideals.len())
            .filter(|&at| goes_under[at] == under)
            .collect();
        let segment: Vec<(u16, u16)> = members.iter().map(|&at| ideals[at]).collect();
        for (at, row) in members.into_iter().zip(stack_rows(&segment, lo, hi)) {
            rows[at] = row;
        }
    }

    label_boxes(input, sizes, &rows)
}

/// The top row of each label in one column over rows `lo..hi`, or `None` for
/// a label that hangs past `hi`.
///
/// `labels` holds each label's ideal top row and height, in annotation order,
/// and the column keeps that order. The labels sit past the text in one
/// column, so a stack reads as one list.
///
/// Labels that overlap at their ideal rows form a block, and the block's top
/// is the mean, over its members, of each ideal top less the heights stacked
/// above it in the block. The mean puts a block's labels as near their rows as
/// the order allows. Five equal boxes on consecutive rows put the middle one
/// on its own row, two above it, and two below. The mean rounds half away from
/// zero.
///
/// A block that crosses an end of the segment slides back inside it. A block
/// taller than the segment starts at `lo`, and its labels past `hi` drop out.
fn stack_rows(labels: &[(u16, u16)], lo: u16, hi: u16) -> Vec<Option<u16>> {
    let (lo, hi) = (i32::from(lo), i32::from(hi));
    let top_and_height = |members: Range<usize>| {
        let count = members.len() as f32;
        let (sum, height) =
            labels[members]
                .iter()
                .fold((0, 0), |(sum, offset), &(ideal, height)| {
                    (sum + i32::from(ideal) - offset, offset + i32::from(height))
                });

        let mean = (sum as f32 / count).round() as i32;
        (mean.clamp(lo, (hi - height).max(lo)), height)
    };

    // This loop is the pool-adjacent-violators pass. A block that overlaps the
    // one above absorbs it, and the merged block sits no lower than the block
    // it absorbed, so the loop then checks it against the next block up.
    let mut blocks: Vec<(Range<usize>, i32, i32)> = Vec::new();
    for at in 0..labels.len() {
        let mut members = at..at + 1;
        loop {
            let (top, height) = top_and_height(members.clone());
            match blocks.last() {
                Some((above, above_top, above_height)) if above_top + above_height > top => {
                    members = above.start..members.end;
                    blocks.pop();
                },
                _ => {
                    blocks.push((members, top, height));
                    break;
                },
            }
        }
    }

    let mut rows = Vec::with_capacity(labels.len());
    for (members, top, _) in blocks {
        let mut row = top;
        for &(_, height) in &labels[members] {
            let height = i32::from(height);
            rows.push((row + height <= hi).then_some(row as u16));
            row += height;
        }
    }
    rows
}

/// The box for each label of `sizes` at its row in `rows`, or `None` where the
/// row is `None` or the box does not fit the pane.
fn label_boxes(
    input: &SlideInput,
    sizes: &[(usize, u16, u16, u16)],
    rows: &[Option<u16>],
) -> Vec<Option<Rect>> {
    sizes
        .iter()
        .zip(rows)
        .map(|(&(index, first, width, height), &row)| {
            let y = row?;
            // The connector crosses every row between the annotation's and the
            // box's, so with the box's own rows they form one run.
            let band = first.min(y)..=first.max(y + height - 1);
            let x = widest_end(input, band, input.annotations[index].range.end_x) + LABEL_GAP;
            fits(input.pane, x, y, width, height).then_some(Rect {
                x,
                y,
                width,
                height,
            })
        })
        .collect()
}

/// The boxes a connector goes around, with `crossed` the rows from its code to
/// the far edge of its label.
///
/// The card and the other labels of the stop grow by a quarter cell on each
/// side, which is the room the rough pass jitters a stroke by. The text on the
/// crossed rows is not grown. The line starts on the edge of its own row's
/// text, and a grown row puts that start inside it, where every line runs into
/// it.
fn link_obstacles(
    input: &SlideInput,
    card: Option<Rect>,
    others: &[Rect],
    crossed: RangeInclusive<u16>,
) -> Vec<SixteenthRect> {
    let room = CELL / 4;
    let grown = |rect: Rect| SixteenthRect {
        x: (i32::from(rect.x) * CELL - room) as i16,
        y: (i32::from(rect.y) * CELL - room) as i16,
        w: (i32::from(rect.width) * CELL + 2 * room) as u16,
        h: (i32::from(rect.height) * CELL + 2 * room) as u16,
    };

    let pane_x = input.pane.x;
    let text = crossed.filter_map(|row| {
        let end = line_end(input, row).filter(|&end| end > pane_x)?;
        Some(SixteenthRect {
            x: (i32::from(pane_x) * CELL) as i16,
            y: (i32::from(row) * CELL) as i16,
            w: (i32::from(end - pane_x) * CELL) as u16,
            h: CELL as u16,
        })
    });

    card.into_iter()
        .chain(others.iter().copied())
        .map(grown)
        .chain(text)
        .collect()
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

/// Cells after a range that its connector steps over rather than starts under.
///
/// Two cover a comma, or a closing bracket and its comma.
const SHORT_TAIL: u16 = 2;

/// Where the connector from `range` to a label at row `label_y` leaves the
/// code, in sixteenths, or `None` when the label sits on the range's first row.
///
/// A range that ends its last row, or that at most [`SHORT_TAIL`] cells
/// follow, takes its line from a quarter cell past the row's end, level with
/// it. A line that starts under a word to dodge one comma reads as pointing at
/// the row below.
///
/// When more text follows the range, a sideways start runs through that text.
/// The line then leaves from under the last cell toward a label below the
/// range, and from over the first row toward any other label.
///
/// Over the first row means over the last cell of a one-row range. A longer
/// range's last cell sits under rows of the range itself, so the line leaves
/// from over its first cell instead.
fn link_point(input: &SlideInput, range: &CellRange, label_y: u16) -> Option<(i16, i16)> {
    let (&first, &last) = (range.rows.first()?, range.rows.last()?);
    if label_y == first {
        return None;
    }

    let sixteenths = |cells: u16, offset: i32| (i32::from(cells) * CELL + offset) as i16;
    let end = line_end(input, last);
    let tail = end.is_some_and(|end| end > range.end_x + 1 + SHORT_TAIL);

    Some(match (tail, label_y > last) {
        (false, _) => (
            sixteenths(
                end.map_or(range.end_x + 1, |end| end.max(range.end_x + 1)),
                CELL / 4,
            ),
            sixteenths(last, CELL / 2),
        ),
        (true, true) => (sixteenths(range.end_x, CELL / 2), sixteenths(last + 1, 0)),
        (true, false) => {
            let over = match first == last {
                true => range.end_x,
                false => range.start_x,
            };
            (sixteenths(over, CELL / 2), sixteenths(first, 0))
        },
    })
}

/// Where a connector from `from` meets `label`, in sixteenths.
///
/// The point is a quarter cell left of the box, level with `from` where the
/// side allows. The height is held to the middle three fifths of the side, so
/// the line meets the side rather than a rounded corner.
///
/// A label always sits past the text its connector comes from, so its left
/// side is the one side no other label or line is against. A line that names
/// the box and lets the terminal pick the side facing the code meets a stacked
/// label on the edge it shares with its neighbor.
fn link_end(from: (i16, i16), label: Rect) -> (i16, i16) {
    let top = i32::from(label.y) * CELL;
    let height = i32::from(label.height) * CELL;
    let inset = height / 5;
    (
        (i32::from(label.x) * CELL - CELL / 4) as i16,
        i32::from(from.1).clamp(top + inset, top + height - inset) as i16,
    )
}

/// The bends a connector tries, straight first, then each side in turn at a
/// growing bow.
const LINK_BENDS: [i8; 7] = [0, -12, 12, -24, 24, -32, 32];

/// The bend that takes a connector from `from` to `to` around `obstacles`, on
/// a cell `aspect` times as tall as it is wide.
///
/// A straight line that clears everything stays straight, because a bend with
/// nothing to explain it reads as a flourish. A line that no bend gets clear
/// also stays straight. A pointer that crosses something still says which code
/// the label names, and a missing one says nothing.
fn link_bend(from: (i16, i16), to: (i16, i16), obstacles: &[SixteenthRect], aspect: f32) -> i8 {
    LINK_BENDS
        .into_iter()
        .find(|&bend| {
            link_samples(from, to, bend, aspect)
                .into_iter()
                .skip(1)
                .all(|point| obstacles.iter().all(|&rect| !strictly_inside(point, rect)))
        })
        .unwrap_or(0)
}

/// Points sampled along each segment of a connector, both ends included.
const LINK_SAMPLES: usize = 32;

/// Points along the connector from `from` to `to` at `bend`, in sixteenths,
/// on a cell `aspect` times as tall as it is wide.
///
/// The points follow the terminal's curve before its rough jitter. A straight
/// line is sampled along its chord. A bowed line is the Catmull-Rom spline
/// through the bowed midpoint, with both ends doubled so the curve reaches
/// them, which the terminal draws as two cubics.
///
/// The terminal bows the line in pixels, so the bow follows the cell's shape.
/// A level line on a tall cell bows fewer rows than on a square one. A scale
/// of either axis moves the spline with its points, so the midpoint is the
/// only point that the cell's shape changes.
fn link_samples(from: (i16, i16), to: (i16, i16), bend: i8, aspect: f32) -> Vec<(f32, f32)> {
    let from = (f32::from(from.0), f32::from(from.1));
    let to = (f32::from(to.0), f32::from(to.1));
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);

    if bend == 0 {
        return sample_ts()
            .map(|t| (from.0 + dx * t, from.1 + dy * t))
            .collect();
    }

    let bow = f32::from(bend) / 64.0;
    let mid = (
        (from.0 + to.0) / 2.0 - dy * aspect * bow,
        (from.1 + to.1) / 2.0 + dx / aspect * bow,
    );

    let plus_sixth = |at: (f32, f32), tail: (f32, f32), head: (f32, f32)| {
        (
            at.0 + (head.0 - tail.0) / 6.0,
            at.1 + (head.1 - tail.1) / 6.0,
        )
    };
    let first = [
        from,
        plus_sixth(from, from, mid),
        plus_sixth(mid, to, from),
        mid,
    ];
    let second = [mid, plus_sixth(mid, from, to), plus_sixth(to, to, mid), to];
    bezier(first).chain(bezier(second)).collect()
}

/// The curve parameters a segment is sampled at, both ends included.
fn sample_ts() -> impl Iterator<Item = f32> {
    (0..LINK_SAMPLES).map(|i| i as f32 / (LINK_SAMPLES - 1) as f32)
}

/// Points along the cubic Bezier curve with control points `points`.
fn bezier(points: [(f32, f32); 4]) -> impl Iterator<Item = (f32, f32)> {
    sample_ts().map(move |t| {
        let u = 1.0 - t;
        let weights = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
        weights
            .iter()
            .zip(points)
            .fold((0.0, 0.0), |(x, y), (weight, point)| {
                (x + weight * point.0, y + weight * point.1)
            })
    })
}

/// Whether `point` lies inside `rect` and on none of its edges.
///
/// A connector starts on the edge of its own row's text and ends on the edge
/// of its label's jitter room, so a point on an edge is not in the way.
fn strictly_inside(point: (f32, f32), rect: SixteenthRect) -> bool {
    let (left, top) = (f32::from(rect.x), f32::from(rect.y));
    let (right, bottom) = (left + f32::from(rect.w), top + f32::from(rect.h));
    point.0 > left && point.0 < right && point.1 > top && point.1 < bottom
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
        let label_at = match callout.link.is_some() {
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
            cell_pixels: None,
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
            .map(|callout| (callout.label, callout.link.is_some()))
            .collect()
    }

    /// A card from column 24 to the pane's right edge, over `height` rows from
    /// row `y`.
    ///
    /// A label beside lines that end at column 20 starts at column 24, so the
    /// card blocks that column on each row it covers.
    fn card_over_rows(y: u16, height: u16) -> Rect {
        Rect {
            x: 24,
            y,
            width: pane().width - 24,
            height,
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

    /// Two labels on adjacent rows do not both fit on their own rows, so the
    /// pair straddles the two rows. One box starts a row above the first row,
    /// and the other starts a row under the second. Left to overlap, one label
    /// hides the other.
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
        assert_eq!(
            [first.label.y, second.label.y],
            [9, 12],
            "one box a row above the pair and one a row under it",
        );
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

    /// Only one label fits on a row that two annotations share, so the pair
    /// straddles the row. The first box starts a row above it, and the second
    /// sits directly under the first, in the same column past the text. Neither
    /// box starts on the row, so both carry a connector.
    #[test]
    fn two_labels_on_one_row_straddle_it() {
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
                        y: 9,
                        width: 12,
                        height: 3,
                    },
                    true,
                ),
                (
                    Rect {
                        x: 44,
                        y: 12,
                        width: 13,
                        height: 3,
                    },
                    true,
                ),
            ],
            "the first a row above the shared row, the second under the first",
        );
    }

    /// The row of each label placed for one annotation on each of `rows`, with
    /// `card` holding part of the pane. Every label has two lines, so every box
    /// is four rows tall.
    fn stacked_label_rows(rows: Range<u16>, card: Option<Rect>) -> Vec<u16> {
        let mut input = input(pane(), None);
        input.annotations = rows
            .enumerate()
            .map(|(key, row)| annotation(key, &[row], 4, 8, &["a", "b"]))
            .collect();
        input.current = Some(input.annotations.len() - 1);

        place_callouts(&input, card)
            .iter()
            .map(|callout| callout.label.y)
            .collect()
    }

    /// The layout centers a stack on the rows it names. A stack that hangs
    /// from its first label puts the last labels far from their code, with no
    /// room planned for them near it.
    #[test]
    fn a_stack_centers_on_the_rows_it_names() {
        assert_eq!(
            stacked_label_rows(10..15, None),
            [4, 8, 12, 16, 20],
            "the middle label on its own row, two above it and two below",
        );
    }

    /// A centered stack that runs past the pane's bottom slides up as one
    /// until its last box fits, so the labels keep their order.
    #[test]
    fn a_stack_past_the_pane_bottom_slides_up() {
        assert_eq!(
            stacked_label_rows(24..29, None),
            [10, 14, 18, 22, 26],
            "the last box ends on the pane's last row",
        );
    }

    /// A centered stack that runs past the pane's top slides down as one until
    /// its first box fits.
    #[test]
    fn a_stack_past_the_pane_top_slides_down() {
        assert_eq!(
            stacked_label_rows(2..7, None),
            [0, 4, 8, 12, 16],
            "the first box starts on the pane's first row",
        );
    }

    /// Eight four-row boxes need 32 rows, and the pane has 30. The stack starts
    /// at the pane's top, and the box that hangs past its bottom draws nothing.
    #[test]
    fn a_stack_taller_than_the_pane_drops_its_last_labels() {
        assert_eq!(
            stacked_label_rows(0..8, None),
            [0, 4, 8, 12, 16, 20, 24],
            "seven boxes from the top, and no eighth",
        );
    }

    /// Five four-row labels beside a card have eight free rows under it and
    /// twelve over it. The last two fill the rows under the card and the first
    /// three go over it, so the stack keeps its order and drops no label that
    /// fits.
    #[test]
    fn a_stack_too_tall_for_the_rows_under_the_card_splits_around_it() {
        assert_eq!(
            stacked_label_rows(12..17, Some(card_over_rows(12, 10))),
            [0, 4, 8, 22, 26],
            "three over the card and two under it",
        );
    }

    /// A card over the middle of a stack splits it. A label over the card hides
    /// the narration, so the two labels above the card move up off it and the
    /// two below it start under it.
    #[test]
    fn a_card_splits_the_stack_around_it() {
        let mut input = input(pane(), None);
        input.annotations = [10, 11, 16, 17]
            .into_iter()
            .enumerate()
            .map(|(key, row)| annotation(key, &[row], 4, 8, &["note"]))
            .collect();
        input.current = Some(3);

        let rows: Vec<u16> = place_callouts(&input, Some(card_over_rows(12, 4)))
            .iter()
            .map(|callout| callout.label.y)
            .collect();
        assert_eq!(rows, [6, 9, 16, 19], "two over the card and two under it");
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
        assert_eq!(
            placed_beside_long_row(11, 12, card_over_rows(11, 1)),
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
        assert_eq!(
            placed_beside_long_row(14, 15, card_over_rows(12, 5)),
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

    /// Where the connector of each callout placed for one annotation leaves
    /// the code. The annotation runs over `rows`, from column 4 on the first to
    /// column 8 on the last, and `card` holds part of the pane. The last row's
    /// text ends at `last_end` and every other row's at 20.
    fn links_from(rows: &[u16], last_end: u16, card: Rect) -> Vec<Option<(i16, i16)>> {
        let last = *rows.last().expect("an annotation covers a row");
        let mut input = input(pane(), None);
        input.line_ends = (0..30)
            .map(|row| (row, if row == last { last_end } else { 20 }))
            .collect();
        input.annotations = vec![annotation(0, rows, 4, 8, &["note"])];
        input.current = Some(0);
        place_callouts(&input, Some(card))
            .iter()
            .map(|callout| callout.link.map(|link| link.from))
            .collect()
    }

    /// A line that leaves beside a word runs through the text after it, so a
    /// label below takes its line from under the word's last cell.
    #[test]
    fn a_connector_leaves_from_under_a_word_that_text_follows() {
        assert_eq!(
            links_from(&[10], 20, card_over_rows(10, 1)),
            [Some((8 * 16 + 8, 11 * 16))],
            "the bottom middle of cell 8",
        );
    }

    /// A word that ends its row has nothing after it to cross, so its line
    /// leaves just past its last cell, level with it.
    #[test]
    fn a_connector_leaves_past_a_word_that_ends_its_row() {
        assert_eq!(
            links_from(&[10], 9, card_over_rows(10, 1)),
            [Some((9 * 16 + 4, 10 * 16 + 8))],
            "a quarter cell past cell 8, halfway down row 10",
        );
    }

    /// A match arm's comma is not text worth dodging. A line that starts under
    /// the arm to miss it reads as pointing at the row below, so the line steps
    /// over two tail cells and leaves past the row's end, level with the code.
    #[test]
    fn a_connector_steps_over_a_short_tail() {
        assert_eq!(
            links_from(&[10], 11, card_over_rows(10, 1)),
            [Some((11 * 16 + 4, 10 * 16 + 8))],
            "a quarter cell past the row's end at cell 11, halfway down row 10",
        );
    }

    /// Three cells after the word are more than a comma, so a sideways start
    /// runs through text and the line leaves from under the word instead.
    #[test]
    fn a_connector_leaves_from_under_a_word_a_longer_tail_follows() {
        assert_eq!(
            links_from(&[10], 12, card_over_rows(10, 1)),
            [Some((8 * 16 + 8, 11 * 16))],
            "the bottom middle of cell 8",
        );
    }

    /// A label above takes its line from over the word's last cell, the end
    /// nearest the label, so the line crosses the least of the code above.
    #[test]
    fn a_connector_leaves_from_over_a_word_when_its_label_sits_above() {
        assert_eq!(
            links_from(&[10], 20, card_over_rows(10, 20)),
            [Some((8 * 16 + 8, 10 * 16))],
            "the top middle of cell 8",
        );
    }

    /// A block's last cell sits under rows of the block itself, so the line to
    /// a label above leaves from over the block's first cell.
    #[test]
    fn a_block_connector_to_a_label_above_leaves_over_its_first_cell() {
        assert_eq!(
            links_from(&[10, 11], 20, card_over_rows(10, 20)),
            [Some((4 * 16 + 8, 10 * 16))],
            "the top middle of cell 4 on row 10",
        );
    }

    /// A block that ends its last row leaves from just past that row's last
    /// cell. Level with its first row, the line starts inside that row's text.
    #[test]
    fn a_block_that_ends_its_row_connects_from_past_its_last_cell() {
        assert_eq!(
            links_from(&[10, 11], 9, card_over_rows(10, 1)),
            [Some((9 * 16 + 4, 11 * 16 + 8))],
            "a quarter cell past cell 8, halfway down row 11",
        );
    }

    /// A label sits past the text its connector comes from, so its left side
    /// faces the code and no other box is against it. The line meets that side
    /// level with the code where the side allows, clear of the rounded corners.
    #[test]
    fn a_connector_arrives_on_the_label_side_facing_the_code() {
        let label = Rect {
            x: 24,
            y: 13,
            width: 12,
            height: 3,
        };

        assert_eq!(
            link_end((8 * 16 + 8, 11 * 16), label),
            (24 * 16 - 4, 13 * 16 + 9),
            "a quarter cell left of the box, as high up its side as the corner allows",
        );
    }

    /// A bend with nothing to explain it reads as a flourish.
    #[test]
    fn a_straight_connector_that_clears_everything_stays_straight() {
        assert_eq!(link_bend((136, 192), (380, 217), &[], 1.0), 0);
    }

    /// A label between the code and the box its connector points at bends the
    /// line around it, so the line does not run through the label.
    #[test]
    fn a_connector_bows_around_a_label_in_its_way() {
        let (from, to) = ((136, 192), (700, 217));
        let label = SixteenthRect {
            x: 384,
            y: 192,
            w: 96,
            h: 48,
        };

        let bend = link_bend(from, to, &[label], 1.0);
        assert_ne!(bend, 0, "the straight line runs through the label");
        assert!(
            link_samples(from, to, bend, 1.0)
                .iter()
                .all(|&point| !strictly_inside(point, label)),
            "the line bowed by {bend} clears the label",
        );
    }

    /// A pointer that crosses something still says which code its label names,
    /// where a missing one says nothing.
    #[test]
    fn a_connector_with_no_clear_path_stays_straight() {
        let corridor = SixteenthRect {
            x: 200,
            y: -1000,
            w: 400,
            h: 2000,
        };

        assert_eq!(link_bend((136, 192), (700, 217), &[corridor], 1.0), 0);
    }

    /// The terminal bows a line in pixels, so a level line on a cell twice as
    /// tall as it is wide bows half as many rows as on a square cell.
    #[test]
    fn a_tall_cell_bows_a_level_connector_less() {
        let peak = |aspect| {
            link_samples((0, 0), (640, 0), -32, aspect)
                .into_iter()
                .map(|(_, y)| y)
                .fold(f32::INFINITY, f32::min)
        };

        assert_eq!(
            (peak(1.0), peak(2.0)),
            (-320.0, -160.0),
            "the midpoint is the peak, and it rises half as far on the tall cell",
        );
    }

    /// A bend that clears a label on a square cell falls short of it on a tall
    /// cell, so the tall cell takes the next bend that clears it.
    #[test]
    fn a_tall_cell_takes_a_deeper_bend_around_a_label() {
        let label = SixteenthRect {
            x: 280,
            y: -90,
            w: 80,
            h: 180,
        };
        let bend = |aspect| link_bend((0, 0), (640, 0), &[label], aspect);

        assert_eq!(
            (bend(1.0), bend(2.0)),
            (-12, -24),
            "a bow of 12 rises only 60 sixteenths on the tall cell, inside the label",
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
    /// that annotation's callout. The layout plans every label of the stop
    /// before any is revealed, so a step moves no label and no connector the
    /// reader already sees.
    #[test]
    fn every_label_is_planned_before_any_is_revealed() {
        let mut base = input(pane(), None);
        // Adjacent rows, so the first label makes room for the second.
        base.annotations = vec![
            annotation(0, &[10], 4, 8, &["one"]),
            annotation(1, &[11], 4, 8, &["two"]),
        ];
        let callouts = |current: Option<usize>| {
            layout(&SlideInput {
                current,
                ..base.clone()
            })
            .callouts
        };

        let both = callouts(Some(1));
        assert_eq!(callouts(None), Vec::new(), "the focus comes alone");
        assert_eq!(
            callouts(Some(0)),
            both[..1],
            "the first callout lands where it stays",
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
