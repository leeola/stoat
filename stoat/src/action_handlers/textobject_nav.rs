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
