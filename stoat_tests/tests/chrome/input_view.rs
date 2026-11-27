use stoat::input_view::{InputView, SubmitTarget};
use stoat_text::{Bias, SelectionGoal};

/// The caret counts the chars, not the bytes, before the cell the cursor
/// covers. A cursor at the text's end covers the cell past its last char.
#[test]
fn the_caret_counts_the_chars_before_the_cursor() {
    let mut h = stoat::Stoat::test();
    let executor = h.stoat.executor.clone();
    let ws = h.stoat.active_workspace_mut();
    let text = "a\u{00E9}b";
    let input = InputView::create(ws, executor, SubmitTarget::FileFinder, text, "insert");
    let at_end = input.caret(ws);

    let editor = ws
        .editors
        .get_mut(input.editor_id)
        .expect("the input's editor");
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    let after_accent = buffer.anchor_at(3, Bias::Right);
    editor.selections.transform(buffer, |selection| {
        let mut moved = selection.clone();
        moved.collapse_to(after_accent, SelectionGoal::None);
        moved
    });

    assert_eq!(
        (at_end, input.caret(ws)),
        ((text.to_owned(), 3), (text.to_owned(), 2)),
    );
}
