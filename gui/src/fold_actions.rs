//! Tree-sitter container detection for the fold actions. Produces the
//! buffer `Point` ranges that the fold actions pass to
//! [`crate::display_map::DisplayMap::fold`] / `unfold`.
//!
//! Reuses the breadcrumbs layer walk
//! ([`crate::breadcrumbs::shallowest_containing_layer`]) and the same
//! container kinds, plus `block` so generic `{ }` blocks fold too.

use crate::{breadcrumbs::shallowest_containing_layer, editor::Editor};
use gpui::App;
use std::{collections::BTreeMap, ops::Range};
use stoat_language::Node;
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

/// Map each foldable multi-line container whose declaration starts in
/// `[start_row, end_row)` to its fold range. Drives the fold gutter
/// chevrons (which rows get one) and resolves a chevron click to the
/// container at its row. When a row begins more than one foldable node
/// (e.g. a function and its block), the outermost wins.
pub(crate) fn foldable_container_ranges_in_range(
    editor: &Editor,
    start_row: u32,
    end_row: u32,
    cx: &App,
) -> BTreeMap<u32, Range<Point>> {
    let mut out = BTreeMap::new();
    let multi = editor.multi_buffer().read(cx);
    let Some(buffer) = multi.as_singleton() else {
        return out;
    };
    let snapshot = multi.snapshot();
    let rope = snapshot.rope();
    let buffer = buffer.read(cx);
    let Some(syntax_map) = buffer.syntax_map() else {
        return out;
    };
    let Some(layer) = shallowest_containing_layer(syntax_map.snapshot(), 0) else {
        return out;
    };

    let root = layer.tree.root_node();
    let start_byte = rope.point_to_offset(Point::new(start_row, 0));
    let end_byte = rope.point_to_offset(Point::new(end_row, 0));
    let seed = root
        .descendant_for_byte_range(start_byte, end_byte)
        .unwrap_or(root);

    // A container whose declaration sits at the viewport top spans the
    // seed, so it is an ancestor rather than part of its subtree. Record
    // those outermost-first to preserve the outermost-wins ordering.
    let mut ancestors = Vec::new();
    let mut ancestor = seed.parent();
    while let Some(node) = ancestor {
        ancestors.push(node);
        ancestor = node.parent();
    }
    for node in ancestors.into_iter().rev() {
        record_foldable(node, rope, start_row, end_row, &mut out);
    }

    collect_foldable_starts(
        seed, rope, start_byte, end_byte, start_row, end_row, &mut out,
    );
    out
}

/// Collapse a container spanning `start_row..=end_row`: fold from the
/// end of the first line to the start of the last line, keeping the
/// declaration and closing lines visible.
fn fold_range(rope: &Rope, start_row: u32, end_row: u32) -> Range<Point> {
    Point::new(start_row, rope.line_len(start_row))..Point::new(end_row, 0)
}

/// Record `node` as its start row's fold range when it is a multi-line
/// foldable container declared in `[start_row, end_row)`. [`BTreeMap::
/// entry`] keeps the outermost range when containers share a start row.
fn record_foldable(
    node: Node<'_>,
    rope: &Rope,
    start_row: u32,
    end_row: u32,
    out: &mut BTreeMap<u32, Range<Point>>,
) {
    if !FOLDABLE_KINDS.contains(&node.kind()) {
        return;
    }
    let node_start = rope.offset_to_point(node.byte_range().start).row;
    let node_end = rope.offset_to_point(node.byte_range().end).row;
    if node_end > node_start && (start_row..end_row).contains(&node_start) {
        out.entry(node_start)
            .or_insert_with(|| fold_range(rope, node_start, node_end));
    }
}

/// Recursively collect foldable multi-line containers starting in
/// `[start_row, end_row)`, pruning subtrees outside the visible byte
/// range `[start_byte, end_byte)` so node byte ranges are compared
/// directly instead of converted to rows. The parent of a same-row pair
/// is visited first, so [`BTreeMap::entry`] keeps the outermost per row.
fn collect_foldable_starts(
    node: Node<'_>,
    rope: &Rope,
    start_byte: usize,
    end_byte: usize,
    start_row: u32,
    end_row: u32,
    out: &mut BTreeMap<u32, Range<Point>>,
) {
    let range = node.byte_range();
    if range.end < start_byte || range.start >= end_byte {
        return;
    }
    record_foldable(node, rope, start_row, end_row, out);
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i as u32) {
            collect_foldable_starts(child, rope, start_byte, end_byte, start_row, end_row, out);
        }
    }
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

    const NESTED: &str = "mod foo {\n    impl Bar {\n        fn baz() {\n            let x = 1;\n        }\n    }\n}\n";

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

    #[test]
    fn foldable_ranges_in_range_maps_each_nested_start_row() {
        let mut cx = TestAppContext::single();
        let editor = new_rust_editor(&mut cx, NESTED);
        let ranges = editor.read_with(&cx, |ed, cx| {
            foldable_container_ranges_in_range(ed, 0, 7, cx)
        });
        assert_eq!(
            ranges.into_iter().collect::<Vec<_>>(),
            vec![
                (0, Point::new(0, 9)..Point::new(6, 0)),
                (1, Point::new(1, 14)..Point::new(5, 0)),
                (2, Point::new(2, 18)..Point::new(4, 0)),
            ]
        );
    }

    #[test]
    fn foldable_ranges_in_range_prunes_outside_rows() {
        let mut cx = TestAppContext::single();
        let editor = new_rust_editor(&mut cx, NESTED);
        let ranges = editor.read_with(&cx, |ed, cx| {
            foldable_container_ranges_in_range(ed, 2, 3, cx)
        });
        assert_eq!(
            ranges.into_iter().collect::<Vec<_>>(),
            vec![(2, Point::new(2, 18)..Point::new(4, 0))]
        );
    }

    #[test]
    fn foldable_ranges_in_range_finds_viewport_top_container() {
        let mut cx = TestAppContext::single();
        let editor = new_rust_editor(&mut cx, NESTED);
        // Viewport rows 0..2: the `mod` and `impl` containers start in it
        // and span past it; the deeper `fn` at row 2 is excluded.
        let ranges = editor.read_with(&cx, |ed, cx| {
            foldable_container_ranges_in_range(ed, 0, 2, cx)
        });
        assert_eq!(
            ranges.into_iter().collect::<Vec<_>>(),
            vec![
                (0, Point::new(0, 9)..Point::new(6, 0)),
                (1, Point::new(1, 14)..Point::new(5, 0)),
            ]
        );
    }
}
