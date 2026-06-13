use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
};
use stoat_language::surround::{find_surround_pair, surround_pair_for, SurroundReplaceStage};
use stoat_text::{Bias, SelectionGoal};

pub(super) fn surround_add(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_surround_add = true;
    UpdateEffect::Redraw
}

pub(super) fn surround_replace(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_surround_replace = SurroundReplaceStage::AwaitFrom;
    UpdateEffect::Redraw
}

pub(super) fn surround_delete(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_surround_delete = true;
    UpdateEffect::Redraw
}

/// Apply the consumed-char keypress to the pending surround_add chord:
/// wrap every non-empty selection in the focused editor with the pair
/// returned by [`surround_pair_for`]. Empty (collapsed) selections are
/// skipped. After the wrap, each affected selection's range covers the
/// original content (between the inserted open and close), preserving
/// the original `reversed` direction.
pub(crate) fn execute_surround_add(stoat: &mut Stoat, ch: char) -> UpdateEffect {
    let (open, close) = surround_pair_for(ch);
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, mut entries) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let display_snapshot = editor.display_map.snapshot();
        let buffer_snapshot = display_snapshot.buffer_snapshot();
        let entries: Vec<(usize, usize, usize, bool)> = editor
            .selections
            .all_anchors()
            .iter()
            .filter_map(|sel| {
                let s = buffer_snapshot.resolve_anchor(&sel.start);
                let e = buffer_snapshot.resolve_anchor(&sel.end);
                if s == e {
                    return None;
                }
                Some((sel.id, s, e, sel.reversed))
            })
            .collect();
        (buffer_id, entries)
    };

    if entries.is_empty() {
        return UpdateEffect::None;
    }

    entries.sort_by_key(|(_, s, _, _)| *s);

    let open_str = open.to_string();
    let close_str = close.to_string();

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for (_, s, e, _) in entries.iter().rev() {
            guard.edit(*e..*e, &close_str);
            guard.edit(*s..*s, &open_str);
        }
    }

    let open_len = open.len_utf8();
    let close_len = close.len_utf8();
    let mut id_to_range: std::collections::HashMap<usize, (usize, usize, bool)> =
        std::collections::HashMap::with_capacity(entries.len());
    let mut shift: i64 = 0;
    for (id, s, e, reversed) in entries.iter() {
        let new_start = (*s as i64 + shift) as usize + open_len;
        let new_end = (*e as i64 + shift) as usize + open_len;
        id_to_range.insert(*id, (new_start, new_end, *reversed));
        shift += (open_len + close_len) as i64;
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();

    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if let Some(&(start_off, end_off, reversed)) = id_to_range.get(&sel.id) {
            new.start = new_buf.anchor_at(start_off, Bias::Left);
            new.end = new_buf.anchor_at(end_off, Bias::Right);
            new.reversed = reversed;
            new.goal = SelectionGoal::None;
        }
        new
    });
    UpdateEffect::Redraw
}

/// Apply the consumed-char keypress to the pending surround_delete
/// chord: for every selection's primary cursor, find the nearest
/// enclosing surround pair for `ch` via [`find_surround_pair`]
/// and remove its open / close. Selections whose cursor is not
/// enclosed by a pair are skipped. Pairs are deduped before edits
/// run, so two cursors inside the same pair produce one edit.
pub(crate) fn execute_surround_delete(stoat: &mut Stoat, ch: char) -> UpdateEffect {
    let (open, close) = surround_pair_for(ch);
    let pairs = match collect_surround_pairs(stoat, open, close) {
        Some(p) if !p.is_empty() => p,
        _ => return UpdateEffect::None,
    };

    let buffer_id = focused_buffer_id(stoat).expect("checked by collect_surround_pairs");
    let ws = stoat.active_workspace_mut();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let mut guard = buffer.write().expect("poisoned");
    let open_len = open.len_utf8();
    let close_len = close.len_utf8();
    for (open_off, close_off) in pairs.iter().rev() {
        guard.edit(*close_off..*close_off + close_len, "");
        guard.edit(*open_off..*open_off + open_len, "");
    }
    UpdateEffect::Redraw
}

/// Apply the consumed two-char keypresses to the pending
/// surround_replace chord: for every selection's primary cursor,
/// find the nearest enclosing surround pair for `from` via
/// [`find_surround_pair`] and replace its open / close with
/// the canonical pair for `to`. Selections whose cursor is not
/// enclosed by a `from` pair are skipped. Pairs are deduped before
/// edits run.
pub(crate) fn execute_surround_replace(stoat: &mut Stoat, from: char, to: char) -> UpdateEffect {
    let (old_open, old_close) = surround_pair_for(from);
    let (new_open, new_close) = surround_pair_for(to);
    let pairs = match collect_surround_pairs(stoat, old_open, old_close) {
        Some(p) if !p.is_empty() => p,
        _ => return UpdateEffect::None,
    };

    let buffer_id = focused_buffer_id(stoat).expect("checked by collect_surround_pairs");
    let ws = stoat.active_workspace_mut();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let mut guard = buffer.write().expect("poisoned");
    let old_open_len = old_open.len_utf8();
    let old_close_len = old_close.len_utf8();
    let new_open_str = new_open.to_string();
    let new_close_str = new_close.to_string();
    for (open_off, close_off) in pairs.iter().rev() {
        guard.edit(*close_off..*close_off + old_close_len, &new_close_str);
        guard.edit(*open_off..*open_off + old_open_len, &new_open_str);
    }
    UpdateEffect::Redraw
}

/// Walk every selection's primary cursor in the focused editor and
/// gather the enclosing surround pair for `(open, close)` per cursor.
/// Returns the deduped pair offsets sorted ascending by `open_off`,
/// or `None` when the focused pane is not an editor. When the buffer
/// has a syntax map, the per-cursor pair lookup runs through the
/// deepest covering layer's tree so brackets inside string / comment
/// nodes are skipped; buffers without a syntax map fall back to the
/// plain non-tree-sitter walk.
fn collect_surround_pairs(
    stoat: &mut Stoat,
    open: char,
    close: char,
) -> Option<Vec<(usize, usize)>> {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return None,
    };
    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let buffer_id = editor.buffer_id;
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope().clone();
    let cursors: Vec<usize> = editor
        .selections
        .all_anchors()
        .iter()
        .map(|sel| buffer_snapshot.resolve_anchor(&sel.head()))
        .collect();
    drop(snapshot);

    let syntax_map = ws.buffers.syntax_map(buffer_id);
    let snapshot = syntax_map.map(|m| m.snapshot());
    let mut pairs: Vec<(usize, usize)> = cursors
        .into_iter()
        .filter_map(|head| {
            let tree = snapshot.as_ref().and_then(|s| {
                s.iter_layers()
                    .fold(None::<&stoat_language::SyntaxLayer>, |acc, layer| {
                        let lstart = layer.start_offset as usize;
                        let lend = layer.end_offset as usize;
                        if lstart <= head && lend >= head {
                            match acc {
                                Some(prev) if prev.depth >= layer.depth => acc,
                                _ => Some(layer),
                            }
                        } else {
                            acc
                        }
                    })
                    .map(|layer| &layer.tree)
            });
            find_surround_pair(&rope, head, open, close, tree)
        })
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    Some(pairs)
}

fn focused_buffer_id(stoat: &Stoat) -> Option<crate::buffer::BufferId> {
    let ws = stoat.active_workspace();
    let focused = ws.panes.focus();
    match ws.panes.pane(focused).view {
        View::Editor(id) => Some(ws.editors.get(id).expect("editor").buffer_id),
        _ => None,
    }
}
