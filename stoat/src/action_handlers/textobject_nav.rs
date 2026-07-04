//! Helix-parity goto-next/prev navigation for tree-sitter textobjects.
//!
//! Bound to `] f` / `[ f` (function) and `] t` / `[ t` (class) in the
//! `bracket_next` / `bracket_prev` modes. Mirrors the
//! [`crate::action_handlers::lsp::goto_diagnostic`] shape: collect
//! candidate offsets from the active buffer's textobjects query,
//! filter by direction relative to the cursor, and `jump_to_offset`
//! to the first match.
//!
//! Selection (`m a` / `m i`) lives in the sibling
//! [`crate::action_handlers::textobject`] module; this file is the
//! directional cousin (jump rather than expand-around).

use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
};

#[derive(Debug, Clone, Copy)]
pub(crate) enum NavKind {
    Function,
    Class,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum NavDirection {
    Next,
    Prev,
}

impl NavKind {
    fn capture_name(self) -> &'static str {
        match self {
            NavKind::Function => "function.around",
            NavKind::Class => "class.around",
        }
    }
}

pub(crate) fn goto_textobject(
    stoat: &mut Stoat,
    kind: NavKind,
    direction: NavDirection,
) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, cursor) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        let cursor = buffer_snapshot.resolve_anchor(&head);
        (buffer_id, cursor)
    };

    let starts = collect_capture_starts_for_buffer(ws, buffer_id, cursor, kind.capture_name());
    let target = match direction {
        NavDirection::Next => starts.into_iter().find(|&s| s > cursor),
        NavDirection::Prev => starts.into_iter().rev().find(|&s| s < cursor),
    };

    let Some(target) = target else {
        return UpdateEffect::None;
    };
    crate::action_handlers::movement::jump_to_offset(stoat, target)
}

fn collect_capture_starts_for_buffer(
    ws: &crate::workspace::Workspace,
    buffer_id: crate::buffer::BufferId,
    cursor: usize,
    capture_name: &str,
) -> Vec<usize> {
    let Some(syntax_map) = ws.buffers.syntax_map(buffer_id) else {
        return Vec::new();
    };
    let snapshot = syntax_map.snapshot();
    let layer = snapshot
        .iter_layers()
        .fold(None::<&stoat_language::SyntaxLayer>, |acc, layer| {
            let start = layer.start_offset as usize;
            let end = layer.end_offset as usize;
            if start <= cursor && end >= cursor {
                match acc {
                    Some(prev) if prev.depth >= layer.depth => acc,
                    _ => Some(layer),
                }
            } else {
                acc
            }
        });
    let Some(layer) = layer else {
        return Vec::new();
    };
    let Some(query) = layer.language.textobjects_query.as_ref() else {
        return Vec::new();
    };
    let Some(buffer) = ws.buffers.get(buffer_id) else {
        return Vec::new();
    };
    let Ok(guard) = buffer.read() else {
        return Vec::new();
    };
    stoat_language::collect_capture_starts(
        query,
        layer.tree.root_node(),
        guard.rope(),
        capture_name,
    )
}

#[cfg(test)]
mod tests {
    use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};
    use std::path::PathBuf;
    use stoat_action::{
        GotoNextClass, GotoNextFunction, GotoPrevClass, GotoPrevFunction, OpenFile,
    };

    fn seed(h: &mut TestHarness, name: &str, contents: &str) -> PathBuf {
        let root = PathBuf::from("/textobject-nav-test");
        let path = root.join(name);
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.stoat.drive_background();
        let _ = h.stoat.render();
        h.settle();
        h.stoat.drive_background();
        let _ = h.stoat.render();
        h.settle();
        path
    }

    fn cursor_offset(h: &mut TestHarness) -> usize {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        buf_snap.resolve_anchor(&head)
    }

    fn jump(h: &mut TestHarness, offset: usize) {
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, offset);
    }

    #[test]
    fn next_function_jumps_to_first_then_second() {
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        let first = cursor_offset(&mut h);
        assert_eq!(&src[first..first + 8], "fn beta(");
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        let second = cursor_offset(&mut h);
        assert_eq!(&src[second..second + 9], "fn gamma(");
    }

    #[test]
    fn next_function_no_op_after_last() {
        let src = "fn only() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        let after_last = src.len() - 1;
        jump(&mut h, after_last);
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn prev_function_jumps_backward() {
        let src = "fn alpha() {}\nfn beta() {}\nfn gamma() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, src.len());
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevFunction);
        let last = cursor_offset(&mut h);
        assert_eq!(&src[last..last + 9], "fn gamma(");
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevFunction);
        let mid = cursor_offset(&mut h);
        assert_eq!(&src[mid..mid + 8], "fn beta(");
    }

    #[test]
    fn prev_function_no_op_before_first() {
        let src = "fn only() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevFunction);
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn next_class_finds_struct_enum_trait_impl() {
        let src = "struct Foo {}\nenum Bar { A }\ntrait Baz {}\nimpl Foo {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextClass);
        let first = cursor_offset(&mut h);
        assert!(
            src[first..].starts_with("enum Bar"),
            "expected enum Bar; got {:?}",
            &src[first..first + 10.min(src.len() - first)],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextClass);
        let second = cursor_offset(&mut h);
        assert!(
            src[second..].starts_with("trait Baz"),
            "expected trait Baz; got {:?}",
            &src[second..second + 10.min(src.len() - second)],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextClass);
        let third = cursor_offset(&mut h);
        assert!(
            src[third..].starts_with("impl Foo"),
            "expected impl Foo; got {:?}",
            &src[third..third + 10.min(src.len() - third)],
        );
    }

    #[test]
    fn prev_class_jumps_backward_through_definitions() {
        let src = "struct Foo {}\nenum Bar { A }\nimpl Foo {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, src.len());
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevClass);
        let last = cursor_offset(&mut h);
        assert!(src[last..].starts_with("impl Foo"), "{}", &src[last..]);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevClass);
        let mid = cursor_offset(&mut h);
        assert!(src[mid..].starts_with("enum Bar"), "{}", &src[mid..]);
    }

    #[test]
    fn json_buffer_with_no_textobjects_query_is_noop() {
        let src = "{\"a\": 1, \"b\": 2}\n";
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "data.json", src);
        h.settle();
        jump(&mut h, 0);
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoNextFunction);
        assert_eq!(cursor_offset(&mut h), before);
        crate::action_handlers::dispatch(&mut h.stoat, &GotoPrevClass);
        assert_eq!(cursor_offset(&mut h), before);
    }

    #[test]
    fn next_function_via_bracket_chord() {
        let src = "fn alpha() {}\nfn beta() {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, 0);
        h.type_keys("] f");
        let first = cursor_offset(&mut h);
        assert_eq!(&src[first..first + 8], "fn beta(");
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn prev_class_via_bracket_chord() {
        let src = "struct Foo {}\nstruct Bar {}\n";
        let mut h = TestHarness::with_size(60, 20);
        seed(&mut h, "main.rs", src);
        h.settle();
        jump(&mut h, src.len());
        h.type_keys("[ t");
        let off = cursor_offset(&mut h);
        assert!(src[off..].starts_with("struct Bar"), "{}", &src[off..]);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }
}
