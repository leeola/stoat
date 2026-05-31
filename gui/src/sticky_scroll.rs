//! Sticky-scroll header detection: when a syntactic container's
//! declaration scrolls above the viewport, find that container so the
//! editor's render can pin its first line at the top.
//!
//! Reuses the breadcrumbs container detection
//! ([`crate::breadcrumbs::CONTAINER_KINDS`],
//! [`crate::breadcrumbs::shallowest_containing_layer`]) but anchors at
//! the first visible row rather than the cursor.

use crate::{
    breadcrumbs::{shallowest_containing_layer, CONTAINER_KINDS},
    editor::Editor,
};
use gpui::{App, SharedString};
use stoat_text::{Point, Rope};

/// Maximum characters of the sticky header line; longer signatures are
/// truncated with a trailing `...`.
const STICKY_HEADER_MAX_CHARS: usize = 160;

/// The sticky-scroll header for a viewport whose first visible buffer
/// row is `first_visible_buffer_row`: the first line of the innermost
/// [`CONTAINER_KINDS`] container whose declaration starts strictly above
/// that row. `None` when the editor is not a single parsed buffer or no
/// such container encloses the row (so its declaration is on screen or
/// the row is at file scope).
pub(crate) fn sticky_header(
    editor: &Editor,
    first_visible_buffer_row: u32,
    cx: &App,
) -> Option<SharedString> {
    let multi = editor.multi_buffer().read(cx);
    let buffer = multi.as_singleton()?;
    let snapshot = multi.snapshot();
    let rope = snapshot.rope();
    let offset = rope.point_to_offset(Point::new(first_visible_buffer_row, 0));

    let buffer = buffer.read(cx);
    let syntax_map = buffer.syntax_map()?;
    let layers = syntax_map.snapshot();
    let layer = shallowest_containing_layer(layers, offset)?;
    let mut node = layer
        .tree
        .root_node()
        .descendant_for_byte_range(offset, offset)?;

    loop {
        if CONTAINER_KINDS.contains(&node.kind()) {
            let start_row = rope.offset_to_point(node.byte_range().start).row;
            if start_row < first_visible_buffer_row {
                return Some(header_line(rope, start_row));
            }
        }
        node = node.parent()?;
    }
}

/// The trimmed, truncated text of buffer line `row` for the header.
fn header_line(rope: &Rope, row: u32) -> SharedString {
    let start = rope.point_to_offset(Point::new(row, 0));
    let end = rope.point_to_offset(Point::new(row, rope.line_len(row)));
    let line = rope.slice(start..end).to_string();
    SharedString::from(truncate(line.trim_start(), STICKY_HEADER_MAX_CHARS))
}

/// Truncate `s` to at most `max_chars` characters, ending with `...`
/// when truncation occurs. Operates on `char` boundaries.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max_chars.saturating_sub(3)).collect();
    out.push_str("...");
    out
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

    fn header_at(cx: &mut TestAppContext, editor: &Entity<Editor>, row: u32) -> Option<String> {
        editor.read_with(cx, |ed, cx| {
            sticky_header(ed, row, cx).map(|s| s.to_string())
        })
    }

    #[test]
    fn header_is_innermost_container_started_above() {
        let mut cx = TestAppContext::single();
        let editor = new_rust_editor(&mut cx, NESTED);
        // Row 3 ("let x = 1;") sits inside fn baz, which starts on row 2.
        assert_eq!(
            header_at(&mut cx, &editor, 3).as_deref(),
            Some("fn baz() {")
        );
    }

    #[test]
    fn header_skips_containers_whose_declaration_is_visible() {
        let mut cx = TestAppContext::single();
        let editor = new_rust_editor(&mut cx, NESTED);
        // Row 2 is fn baz's own declaration, so the header is the next
        // container up whose start is above: impl Bar (row 1).
        assert_eq!(
            header_at(&mut cx, &editor, 2).as_deref(),
            Some("impl Bar {")
        );
    }

    #[test]
    fn no_header_at_file_scope() {
        let mut cx = TestAppContext::single();
        let editor = new_rust_editor(&mut cx, NESTED);
        assert_eq!(header_at(&mut cx, &editor, 0), None);
    }

    #[test]
    fn truncate_appends_ellipsis_on_char_boundary() {
        assert_eq!(truncate("short", 160), "short");
        assert_eq!(truncate("Ångström rocks", 8), "Ångst...");
    }
}
