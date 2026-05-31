//! Tree-sitter container detection for the fold actions. Produces the
//! buffer `Point` ranges that the fold actions pass to
//! [`crate::display_map::DisplayMap::fold`] / `unfold`.
//!
//! Reuses the breadcrumbs layer walk
//! ([`crate::breadcrumbs::shallowest_containing_layer`]) and the same
//! container kinds, plus `block` so generic `{ }` blocks fold too.

use crate::{breadcrumbs::shallowest_containing_layer, editor::Editor};
use gpui::App;
use std::ops::Range;
use stoat_text::{Point, Rope};

/// Tree-sitter node kinds that fold as a unit: the breadcrumb container
/// kinds plus `block` (a generic `{ }` block).
const FOLDABLE_KINDS: &[&str] = &[
    "function_item",
    "impl_item",
    "trait_item",
    "mod_item",
    "struct_item",
    "enum_item",
    "class_definition",
    "function_definition",
    "method_definition",
    "block",
];

/// The fold range for the smallest foldable container enclosing the
/// cursor whose declaration starts strictly above the cursor's row.
/// `None` when the editor is not a single parsed buffer or no such
/// multi-line container encloses the cursor.
pub(crate) fn fold_container_at(editor: &Editor, cx: &App) -> Option<Range<Point>> {
    let multi = editor.multi_buffer().read(cx);
    let buffer = multi.as_singleton()?;
    let snapshot = multi.snapshot();
    let rope = snapshot.rope();
    let cursor_offset = {
        let head = editor.selections().newest_anchor().head();
        snapshot.resolve_anchor(&head)
    };
    let cursor_row = rope.offset_to_point(cursor_offset).row;

    let buffer = buffer.read(cx);
    let syntax_map = buffer.syntax_map()?;
    let layer = shallowest_containing_layer(syntax_map.snapshot(), cursor_offset)?;
    let mut node = layer
        .tree
        .root_node()
        .descendant_for_byte_range(cursor_offset, cursor_offset)?;

    loop {
        if FOLDABLE_KINDS.contains(&node.kind()) {
            let start_row = rope.offset_to_point(node.byte_range().start).row;
            let end_row = rope.offset_to_point(node.byte_range().end).row;
            if start_row < cursor_row && end_row > start_row {
                return Some(fold_range(rope, start_row, end_row));
            }
        }
        node = node.parent()?;
    }
}

/// Fold ranges for every top-level foldable container (direct children
/// of the syntax root) that spans more than one row. Empty when the
/// editor is not a single parsed buffer.
pub(crate) fn top_level_fold_ranges(editor: &Editor, cx: &App) -> Vec<Range<Point>> {
    let multi = editor.multi_buffer().read(cx);
    let Some(buffer) = multi.as_singleton() else {
        return Vec::new();
    };
    let snapshot = multi.snapshot();
    let rope = snapshot.rope();
    let buffer = buffer.read(cx);
    let Some(syntax_map) = buffer.syntax_map() else {
        return Vec::new();
    };
    let Some(layer) = shallowest_containing_layer(syntax_map.snapshot(), 0) else {
        return Vec::new();
    };
    let root = layer.tree.root_node();

    let mut ranges = Vec::new();
    for i in 0..root.child_count() {
        let Some(child) = root.child(i as u32) else {
            continue;
        };
        if !FOLDABLE_KINDS.contains(&child.kind()) {
            continue;
        }
        let start_row = rope.offset_to_point(child.byte_range().start).row;
        let end_row = rope.offset_to_point(child.byte_range().end).row;
        if end_row > start_row {
            ranges.push(fold_range(rope, start_row, end_row));
        }
    }
    ranges
}

/// The whole-buffer `Point` range, used by the unfold-all action to
/// clear every fold (`unfold` removes folds overlapping the range).
pub(crate) fn whole_buffer_range(editor: &Editor, cx: &App) -> Range<Point> {
    let snapshot = editor.multi_buffer().read(cx).snapshot();
    let rope = snapshot.rope();
    Point::new(0, 0)..rope.offset_to_point(rope.len())
}

/// Collapse a container spanning `start_row..=end_row`: fold from the
/// end of the first line to the start of the last line, keeping the
/// declaration and closing lines visible.
fn fold_range(rope: &Rope, start_row: u32, end_row: u32) -> Range<Point> {
    Point::new(start_row, rope.line_len(start_row))..Point::new(end_row, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, Entity, TestAppContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_language::{Language, LanguageRegistry, SyntaxMap};
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::{Bias, Selection, SelectionGoal};

    const NESTED: &str =
        "mod foo {\n    impl Bar {\n        fn baz() {\n            let x = 1;\n        }\n    }\n}\n";

    fn rust_language() -> Arc<Language> {
        LanguageRegistry::standard()
            .find_by_name("rust")
            .expect("rust grammar")
    }

    fn new_rust_editor(cx: &mut TestAppContext, text: &str) -> Entity<Editor> {
        let buffer = cx.update(|cx| cx.new(|_| Buffer::with_text(BufferId::new(0), text)));
        let map = {
            let rope = Rope::from(text);
            let mut map = SyntaxMap::new();
            map.reparse(&rope, rust_language(), 1).expect("reparse");
            map
        };
        buffer.update(cx, |b, cx| b.set_syntax_map(Some(map), cx));

        let executor = Executor::new(Arc::new(TestScheduler::new()));
        let multi_buffer = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| MultiBuffer::singleton(buffer, cx)))
        };
        let display_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DisplayMap::new(buffer, executor, cx)))
        };
        let diff_map = {
            let buffer = buffer.clone();
            cx.update(|cx| cx.new(|cx| DiffMap::new(buffer, cx)))
        };
        cx.update(|cx| {
            cx.new(|cx| Editor::new(multi_buffer, display_map, diff_map, EditorMode::full(), cx))
        })
    }

    fn seed_cursor(editor: &Entity<Editor>, cx: &mut TestAppContext, offset: usize) {
        editor.update(cx, |ed, cx| {
            let snapshot = ed.multi_buffer().read(cx).snapshot();
            let anchor = snapshot.anchor_at(offset, Bias::Left);
            ed.selections_mut().replace_with(
                vec![Selection {
                    id: 1,
                    start: anchor,
                    end: anchor,
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                &snapshot,
            );
        });
    }

    #[test]
    fn fold_container_at_cursor_folds_enclosing_block() {
        let mut cx = TestAppContext::single();
        let editor = new_rust_editor(&mut cx, NESTED);
        seed_cursor(&editor, &mut cx, NESTED.find("let x").expect("anchor"));
        let range = editor.read_with(&cx, fold_container_at);
        // The fn baz block spans rows 2..4; fold collapses its body.
        assert_eq!(range, Some(Point::new(2, 18)..Point::new(4, 0)));
    }

    #[test]
    fn fold_container_at_file_scope_is_none() {
        let mut cx = TestAppContext::single();
        let editor = new_rust_editor(&mut cx, "let x = 1;\n");
        seed_cursor(&editor, &mut cx, 0);
        let range = editor.read_with(&cx, fold_container_at);
        assert_eq!(range, None);
    }

    #[test]
    fn top_level_fold_ranges_lists_root_containers() {
        let mut cx = TestAppContext::single();
        let editor = new_rust_editor(&mut cx, NESTED);
        let ranges = editor.read_with(&cx, top_level_fold_ranges);
        // The single top-level item is `mod foo` spanning rows 0..6.
        assert_eq!(ranges, vec![Point::new(0, 9)..Point::new(6, 0)]);
    }
}
