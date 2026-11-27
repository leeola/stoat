//! Where buffer text sits on screen.
//!
//! A screen position is read back from what the last render painted rather
//! than recomputed, so no caller re-derives the gutter or the diff column. The
//! paint records the rect it put buffer text into as
//! [`EditorState::text_rect`], and every answer here starts from that record.

use crate::editor_state::EditorState;
use std::ops::RangeInclusive;

/// The cells a buffer range covers on screen, in absolute screen cells.
pub struct Span {
    pub rows: RangeInclusive<u16>,
    /// The first cell on the first row.
    pub start_x: u16,
    /// The last cell on the last row, inclusive.
    pub end_x: u16,
}

/// The screen cell that shows buffer `offset`, as absolute `(x, y)`.
///
/// The answer comes from the last render, so a caller that draws over a pane
/// asks after that pane paints in the same frame. `None` before the first
/// render, for an offset past the end of the buffer, and for a cell outside
/// the painted text, such as a row scrolled out of view or a column past the
/// right edge.
pub fn cell(editor: &mut EditorState, offset: usize) -> Option<(u16, u16)> {
    let area = editor.text_rect?;
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    if offset > rope.len() {
        return None;
    }

    let display = snapshot.buffer_to_display(rope.offset_to_point(offset));
    let row = display.row.checked_sub(editor.scroll_row)?;
    if row >= u32::from(area.height) || display.column >= u32::from(area.width) {
        return None;
    }
    Some((area.x + display.column as u16, area.y + row as u16))
}

/// The cells of `range` that show on screen, from the cell of its first offset
/// to the cell of its last.
///
/// An end that lies past the view is cut at the view's edge. A range that
/// starts above the view starts at the first cell of the top row, and one that
/// ends below it ends at the last cell of the bottom row, so a range taller
/// than the view keeps its visible part. `None` when no row of the range shows.
/// A range is never moved into view, because the moved range marks whatever
/// scrolled into its place.
pub fn visible_span(editor: &mut EditorState, range: RangeInclusive<usize>) -> Option<Span> {
    let area = editor.text_rect.filter(|area| !area.is_empty())?;
    let (first_row, last_row) = {
        let snapshot = editor.display_map.snapshot();
        let rope = snapshot.buffer_snapshot().rope();
        let row_of = |offset: usize| {
            let point = rope.offset_to_point(offset.min(rope.len()));
            snapshot.buffer_to_display(point).row
        };
        (row_of(*range.start()), row_of(*range.end()))
    };
    let view = editor.scroll_row..editor.scroll_row + u32::from(area.height);

    // An end inside the view's rows takes its own cell, and `cell` answers
    // `None` for one wholly above or below the view.
    let (start_x, first) = match first_row < view.start {
        true => (area.x, area.y),
        false => cell(editor, *range.start())?,
    };
    let (end_x, last) = match last_row >= view.end {
        true => (area.right() - 1, area.bottom() - 1),
        false => cell(editor, *range.end())?,
    };
    Some(Span {
        rows: first..=last,
        start_x,
        end_x,
    })
}
