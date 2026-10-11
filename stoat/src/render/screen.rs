//! Where buffer text sits on screen.
//!
//! A screen position is read back from what the last render painted rather
//! than recomputed, so no caller re-derives the gutter or the diff column. The
//! paint records the rect it put buffer text into as
//! [`EditorState::text_rect`], and every answer here starts from that record.

use crate::editor_state::EditorState;
use std::ops::RangeInclusive;

/// The cells a buffer range covers on screen, in absolute screen cells.
pub(crate) struct Span {
    pub(crate) rows: RangeInclusive<u16>,
    /// The first cell on the first row.
    pub(crate) start_x: u16,
    /// The last cell on the last row, inclusive.
    pub(crate) end_x: u16,
}

/// The screen cell that shows buffer `offset`, as absolute `(x, y)`.
///
/// The answer comes from the last render, so a caller that draws over a pane
/// asks after that pane paints in the same frame. `None` before the first
/// render, for an offset past the end of the buffer, and for a cell outside
/// the painted text, such as a row scrolled out of view or a column past the
/// right edge.
pub(crate) fn cell(editor: &mut EditorState, offset: usize) -> Option<(u16, u16)> {
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
pub(crate) fn visible_span(editor: &mut EditorState, range: RangeInclusive<usize>) -> Option<Span> {
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

#[cfg(test)]
mod tests {
    use super::{cell, visible_span};
    use crate::{
        action_handlers,
        editor_state::EditorState,
        render::{layout::split_pane_status, review},
        test_fixture::open_scratch_file,
        test_harness::{self, TestHarness},
    };
    use ratatui::layout::Rect;
    use stoat_config::LineNumbers;

    fn focused(h: &mut TestHarness) -> &mut EditorState {
        action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor")
    }

    /// The focused pane's content area, gutter included.
    fn content(h: &TestHarness) -> Rect {
        let ws = h.stoat.active_workspace();
        split_pane_status(ws.panes.pane(ws.panes.focus()).area).0
    }

    #[test]
    fn a_cell_sits_on_the_painted_glyph() {
        let mut h = TestHarness::default();
        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Absolute);
        open_scratch_file(&mut h, "alpha\nbravo\n");
        h.snapshot();

        let buf = h.rendered_buffer();
        let x = (0..buf.area.width)
            .find(|&x| buf[(x, 0)].symbol() == "a")
            .expect("the a of alpha is painted");
        let content = content(&h);
        let editor = focused(&mut h);
        assert!(editor.gutter_width > 0, "the line numbers take a gutter");
        assert_eq!(
            x,
            content.x + editor.gutter_width,
            "the text starts past the gutter"
        );
        assert_eq!(cell(editor, 0), Some((x, 0)));
    }

    #[test]
    fn a_visible_span_cuts_a_range_at_the_view_edges() {
        let mut h = TestHarness::default();
        let text: String = (0..100).map(|line| format!("line {line:02}\n")).collect();
        open_scratch_file(&mut h, &text);
        h.snapshot();
        let editor = focused(&mut h);
        editor.scroll_row = 30;
        let area = editor.text_rect.expect("the render records the text rect");
        // Every line is `line NN` and its newline, eight bytes.
        let line = |row: usize| row * 8;

        let spans = [
            visible_span(editor, line(10)..=line(90)),
            visible_span(editor, line(10)..=line(31) + 2),
            visible_span(editor, line(10)..=line(20)),
        ]
        .map(|span| span.map(|span| (span.rows, span.start_x, span.end_x)));
        assert_eq!(
            spans,
            [
                Some((area.y..=area.bottom() - 1, area.x, area.right() - 1)),
                Some((area.y..=area.y + 1, area.x, area.x + 2)),
                None,
            ],
            "both edges cut, the top edge cut, and a range wholly above the view"
        );
    }

    #[test]
    fn no_render_answers_no_cell() {
        let mut stoat = test_harness::stoat();
        test_harness::editor::seed_focused_buffer(&mut stoat, "alpha\n");
        let editor = action_handlers::focused_editor_mut(&mut stoat).expect("focused editor");
        assert_eq!(cell(editor, 0), None);
    }

    /// A diff view paints the buffer in its right column, so a popup anchors
    /// there rather than at the pane's left edge.
    #[test]
    fn diff_view_anchors_lsp_popups_to_the_right_text_column() {
        // Wide enough for the two-column diff layout.
        let mut h = TestHarness::with_size(120, 12);
        open_scratch_file(&mut h, "keep\nnew\ntail\n");
        h.seed_current_diff_map("keep\nold\ntail\n");

        let off = cell_with_diff_view(&mut h, false);
        let text = focused(&mut h)
            .text_rect
            .expect("the render records the text rect");
        assert_eq!(
            off,
            Some((text.x + 2, text.y)),
            "with diff off the popup anchors at the text column past the gutter"
        );

        let on = cell_with_diff_view(&mut h, true);
        let content = content(&h);
        assert_eq!(
            on,
            Some((review::right_text_x(content) + 2, content.y)),
            "with diff on the popup anchors at the right diff text column"
        );
    }

    /// The cell of offset 2, column 2 of the first line, after a render with
    /// the diff view set to `on`.
    fn cell_with_diff_view(h: &mut TestHarness, on: bool) -> Option<(u16, u16)> {
        focused(h).set_diff_view(on);
        h.stoat.render();
        cell(focused(h), 2)
    }
}
