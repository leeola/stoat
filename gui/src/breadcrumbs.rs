//! Per-pane breadcrumbs bar: a horizontal strip above the editor body
//! showing the tree-sitter syntactic ancestry at the primary cursor
//! (e.g. `mod foo > impl Bar > fn baz`).
//!
//! Mirrors the [`crate::status_bar::cursor_position::CursorPosition`]
//! item -- a weak editor handle plus an [`EditorEvent::Changed`]
//! subscription that refreshes a cached value -- but is rendered by the
//! owning [`crate::pane::Pane`] rather than the status bar. The pane
//! rebinds it to the active editor via [`Breadcrumbs::set_editor`].

use crate::{
    editor::{Editor, EditorEvent},
    theme::ActiveTheme,
};
use gpui::{
    div, px, AnyElement, App, Context, Entity, IntoElement, ParentElement, Render, SharedString,
    Styled, Subscription, WeakEntity, Window,
};
use stoat_language::{Node, SyntaxLayer, SyntaxSnapshot};
use stoat_text::Rope;

/// Tree-sitter node kinds whose name is surfaced as a breadcrumb
/// segment, across the languages stoat bundles. The Rust grammar names
/// the relevant containers `*_item`; Python/JavaScript use
/// `*_definition`.
pub(crate) const CONTAINER_KINDS: &[&str] = &[
    "function_item",
    "impl_item",
    "trait_item",
    "mod_item",
    "struct_item",
    "enum_item",
    "class_definition",
    "function_definition",
    "method_definition",
];

/// Cached syntactic context for the active editor's primary cursor,
/// rendered as a breadcrumbs bar. Segments run outermost-first.
pub struct Breadcrumbs {
    segments: Vec<SharedString>,
    editor: Option<WeakEntity<Editor>>,
    _editor_subscription: Option<Subscription>,
}

impl Default for Breadcrumbs {
    fn default() -> Self {
        Self::new()
    }
}

impl Breadcrumbs {
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
            editor: None,
            _editor_subscription: None,
        }
    }

    /// Bind to `editor` (or clear when `None`). Rebinding to the
    /// already-bound editor is a no-op, so the pane can call this on
    /// every render without churning the subscription.
    pub fn set_editor(&mut self, editor: Option<Entity<Editor>>, cx: &mut Context<'_, Self>) {
        let already_bound = match (&self.editor, &editor) {
            (Some(current), Some(next)) => current.entity_id() == next.entity_id(),
            (None, None) => true,
            _ => false,
        };
        if already_bound {
            return;
        }

        match editor {
            Some(editor) => {
                self.editor = Some(editor.downgrade());
                self._editor_subscription = Some(cx.subscribe(
                    &editor,
                    |this, editor, _event: &EditorEvent, cx| {
                        this.refresh_from_editor(&editor, cx);
                    },
                ));
                self.refresh_from_editor(&editor, cx);
            },
            None => {
                self.editor = None;
                self._editor_subscription = None;
                if !self.segments.is_empty() {
                    self.segments.clear();
                    cx.notify();
                }
            },
        }
    }

    fn refresh_from_editor(&mut self, editor: &Entity<Editor>, cx: &mut Context<'_, Self>) {
        let next = context_segments(editor.read(cx), cx);
        if self.segments != next {
            self.segments = next;
            cx.notify();
        }
    }
}

impl Render for Breadcrumbs {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) -> impl IntoElement {
        let theme = cx.theme();
        let separator = theme.breadcrumb_separator;
        let text = theme.breadcrumb_text;

        let mut children: Vec<AnyElement> = Vec::new();
        for (ix, segment) in self.segments.iter().enumerate() {
            if ix > 0 {
                children.push(div().text_color(separator).child(">").into_any_element());
            }
            children.push(
                div()
                    .text_color(text)
                    .child(segment.clone())
                    .into_any_element(),
            );
        }

        div()
            .flex()
            .items_center()
            .h(px(20.0))
            .px_2()
            .gap_1()
            .children(children)
    }
}

/// Walk the tree-sitter ancestry at the editor's primary cursor and
/// collect the names of the enclosing [`CONTAINER_KINDS`] nodes,
/// outermost first. Empty when the editor is not a single buffer, has
/// no parse tree, or the cursor sits at file scope.
fn context_segments(editor: &Editor, cx: &App) -> Vec<SharedString> {
    let multi = editor.multi_buffer().read(cx);
    let Some(buffer) = multi.as_singleton() else {
        return Vec::new();
    };
    let snapshot = multi.snapshot();
    let offset = {
        let head = editor.selections().newest_anchor().head();
        snapshot.resolve_anchor(&head)
    };

    let buffer = buffer.read(cx);
    let Some(syntax_map) = buffer.syntax_map() else {
        return Vec::new();
    };
    let layers = syntax_map.snapshot();
    let Some(layer) = shallowest_containing_layer(layers, offset) else {
        return Vec::new();
    };
    let Some(mut node) = layer
        .tree
        .root_node()
        .descendant_for_byte_range(offset, offset)
    else {
        return Vec::new();
    };

    let rope = snapshot.rope();
    let mut segments = Vec::new();
    loop {
        if CONTAINER_KINDS.contains(&node.kind())
            && let Some(name) = node_name(node, rope)
        {
            segments.push(name);
        }
        match node.parent() {
            Some(parent) => node = parent,
            None => break,
        }
    }
    segments.reverse();
    segments
}

/// The shallowest syntax layer whose byte span contains `offset` -- the
/// whole-file tree -- so injected sub-layers do not hide the outer
/// structural context.
pub(crate) fn shallowest_containing_layer(
    snapshot: &SyntaxSnapshot,
    offset: usize,
) -> Option<&SyntaxLayer> {
    snapshot.iter_layers().fold(None, |acc, layer| {
        let start = layer.start_offset as usize;
        let end = layer.end_offset as usize;
        if start <= offset && offset <= end {
            match acc {
                Some(prev) if prev.depth <= layer.depth => acc,
                _ => Some(layer),
            }
        } else {
            acc
        }
    })
}

/// The declared name of a container node: its `name` field, falling
/// back to `type` for `impl_item`, which has no name field. `None` when
/// neither field is present or the slice is empty.
///
/// The range is bounded against `rope.len()` because the parse tree can
/// briefly lag the rope after an edit, leaving node offsets past the
/// current end; an unbounded [`Rope::slice`] would panic there.
fn node_name(node: Node<'_>, rope: &Rope) -> Option<SharedString> {
    let name = node
        .child_by_field_name("name")
        .or_else(|| node.child_by_field_name("type"))?;
    let range = name.byte_range();
    if range.start >= range.end || range.end > rope.len() {
        return None;
    }
    let text = rope.slice(range.start..range.end).to_string();
    (!text.is_empty()).then(|| SharedString::from(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::Buffer, diff_map::DiffMap, display_map::DisplayMap, editor::EditorMode,
        multi_buffer::MultiBuffer,
    };
    use gpui::{AppContext, TestAppContext};
    use std::sync::Arc;
    use stoat::buffer::BufferId;
    use stoat_language::{Language, LanguageRegistry, SyntaxMap};
    use stoat_scheduler::{Executor, TestScheduler};
    use stoat_text::{Bias, Selection, SelectionGoal};

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
    fn segments_report_nested_rust_context() {
        let mut cx = TestAppContext::single();
        let text = "mod foo {\n    impl Bar {\n        fn baz() {\n            let x = 1;\n        }\n    }\n}\n";
        let editor = new_rust_editor(&mut cx, text);
        seed_cursor(&editor, &mut cx, text.find("let x").expect("cursor anchor"));

        let breadcrumbs = cx.update(|cx| cx.new(|_| Breadcrumbs::new()));
        breadcrumbs.update(&mut cx, |b, cx| b.set_editor(Some(editor.clone()), cx));

        breadcrumbs.read_with(&cx, |b, _| {
            assert_eq!(
                b.segments,
                vec![
                    SharedString::from("foo"),
                    SharedString::from("Bar"),
                    SharedString::from("baz"),
                ]
            );
        });
    }

    #[test]
    fn segments_empty_at_file_scope() {
        let mut cx = TestAppContext::single();
        let text = "// outside\nfn baz() {}\n";
        let editor = new_rust_editor(&mut cx, text);
        seed_cursor(
            &editor,
            &mut cx,
            text.find("outside").expect("cursor anchor"),
        );

        let breadcrumbs = cx.update(|cx| cx.new(|_| Breadcrumbs::new()));
        breadcrumbs.update(&mut cx, |b, cx| b.set_editor(Some(editor.clone()), cx));

        breadcrumbs.read_with(&cx, |b, _| assert!(b.segments.is_empty()));
    }

    #[test]
    fn clearing_editor_drops_segments() {
        let mut cx = TestAppContext::single();
        let text = "mod foo {\n    fn baz() {\n        let x = 1;\n    }\n}\n";
        let editor = new_rust_editor(&mut cx, text);
        seed_cursor(&editor, &mut cx, text.find("let x").expect("cursor anchor"));

        let breadcrumbs = cx.update(|cx| cx.new(|_| Breadcrumbs::new()));
        breadcrumbs.update(&mut cx, |b, cx| b.set_editor(Some(editor.clone()), cx));
        breadcrumbs.read_with(&cx, |b, _| assert!(!b.segments.is_empty()));

        breadcrumbs.update(&mut cx, |b, cx| b.set_editor(None, cx));
        breadcrumbs.read_with(&cx, |b, _| assert!(b.segments.is_empty()));
    }
}
