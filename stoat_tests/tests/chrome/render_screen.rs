use ratatui::layout::Rect;
use stoat::{
    action_handlers,
    editor_state::EditorState,
    render::{
        review,
        screen::{cell, visible_span},
    },
    test_fixture::open_scratch_file,
    test_harness,
    test_harness::TestHarness,
};
use stoat_config::LineNumbers;

fn focused(h: &mut TestHarness) -> &mut EditorState {
    action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor")
}

/// The focused pane's content area, gutter included.
fn content(h: &TestHarness) -> Rect {
    let ws = h.stoat.active_workspace();
    ws.panes.pane(ws.panes.focus()).area
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
    let text = focused(&mut h)
        .text_rect
        .expect("the render records the text rect");
    let divider = review::diff_divider_x(content).expect("a two-column diff");
    assert_eq!(
        (on, text.x > divider),
        (Some((text.x + 2, content.y)), true),
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
