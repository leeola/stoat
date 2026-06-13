use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
    register::{register_for_char, Register},
};
use stoat_text::{Bias, SelectionGoal};

/// Copy every non-collapsed selection's content into the
/// caller-selected register (or unnamed when none is set),
/// joined with newlines in start-offset order so a later paste
/// can split back per-line.
///
/// Routes by register variant: `Clipboard` writes to
/// [`crate::host::ClipboardHost::set`]; `Blackhole` swallows the
/// content silently; `Search`, `SelectionIndex`, and `LastInsert`
/// are read-only and short-circuit; named/unnamed registers go
/// through the in-memory store. No-op when every selection is
/// collapsed or the focused pane is not an editor.
pub(super) fn yank(stoat: &mut Stoat) -> UpdateEffect {
    let target = stoat.consume_selected_register();
    if matches!(
        target,
        Register::Search | Register::SelectionIndex | Register::LastInsert
    ) {
        return UpdateEffect::None;
    }
    let Some(content) = selections_joined_text(stoat) else {
        return UpdateEffect::None;
    };
    if content.is_empty() {
        return UpdateEffect::None;
    }
    match target {
        Register::Clipboard => {
            if let Err(err) = stoat.clipboard_host().set(&content) {
                tracing::warn!(target: "stoat::yank", ?err, "clipboard set failed");
            }
        },
        Register::Blackhole => {},
        Register::Unnamed | Register::Named(_) => {
            stoat.registers.write(target, content);
        },
        Register::Search | Register::SelectionIndex | Register::LastInsert => {
            // Short-circuited above. Branch retained so the match
            // stays exhaustive without a wildcard arm that would
            // hide future variants.
        },
    }
    UpdateEffect::None
}

pub(super) fn select_register(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_register_select = true;
    UpdateEffect::Redraw
}

pub(super) fn insert_register(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_insert_register = true;
    UpdateEffect::Redraw
}

/// Apply the consumed-char keypress to the pending
/// [`crate::app::Stoat::pending_register_select`] chord. Maps the
/// char through [`register_for_char`] -- ASCII letters select a
/// named register, `"` selects the unnamed register, and the
/// helix special chars (`*`/`+`/`/`/`_`/`#`/`.`) select the
/// matching special register variant. Any other char clears the
/// pending state without selecting a register.
pub(crate) fn execute_select_register(stoat: &mut Stoat, ch: char) {
    stoat.selected_register = register_for_char(ch);
}

pub(super) fn paste_after(stoat: &mut Stoat) -> UpdateEffect {
    paste(stoat, PasteSide::After)
}

pub(super) fn paste_before(stoat: &mut Stoat) -> UpdateEffect {
    paste(stoat, PasteSide::Before)
}

/// Write every non-collapsed selection's content (joined by
/// newlines, in start-offset order) to the system clipboard via
/// the active [`crate::host::ClipboardHost`]. No-op when every
/// selection is collapsed.
pub(super) fn yank_to_clipboard(stoat: &mut Stoat) -> UpdateEffect {
    let Some(content) = selections_joined_text(stoat) else {
        return UpdateEffect::None;
    };
    if content.is_empty() {
        return UpdateEffect::None;
    }
    if let Err(err) = stoat.clipboard_host().set(&content) {
        tracing::warn!(target: "stoat::yank", ?err, "clipboard set failed");
    }
    UpdateEffect::None
}

/// Write only the primary selection's content to the system
/// clipboard. No-op when the primary selection is collapsed.
pub(super) fn yank_main_to_clipboard(stoat: &mut Stoat) -> UpdateEffect {
    let Some(content) = primary_selection_text(stoat) else {
        return UpdateEffect::None;
    };
    if content.is_empty() {
        return UpdateEffect::None;
    }
    if let Err(err) = stoat.clipboard_host().set(&content) {
        tracing::warn!(target: "stoat::yank", ?err, "clipboard set failed");
    }
    UpdateEffect::None
}

pub(super) fn paste_clipboard_after(stoat: &mut Stoat) -> UpdateEffect {
    paste_clipboard(stoat, PasteSide::After)
}

pub(super) fn paste_clipboard_before(stoat: &mut Stoat) -> UpdateEffect {
    paste_clipboard(stoat, PasteSide::Before)
}

fn paste_clipboard(stoat: &mut Stoat, side: PasteSide) -> UpdateEffect {
    let content = match stoat.clipboard_host().get() {
        Ok(Some(text)) => text,
        Ok(None) => return UpdateEffect::None,
        Err(err) => {
            tracing::warn!(target: "stoat::yank", ?err, "clipboard read failed");
            return UpdateEffect::None;
        },
    };
    paste_text(stoat, &content, side)
}

/// Extract the focused editor's primary selection content as a
/// `String`. Returns `None` when the focused pane is not an
/// editor or the primary selection is collapsed.
fn primary_selection_text(stoat: &mut Stoat) -> Option<String> {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return None,
    };
    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let primary = editor.selections.newest_anchor();
    let start = buf_snap.resolve_anchor(&primary.start);
    let end = buf_snap.resolve_anchor(&primary.end);
    let (lo, hi) = if start <= end {
        (start, end)
    } else {
        (end, start)
    };
    if lo == hi {
        return None;
    }
    Some(buf_snap.rope().slice(lo..hi).to_string())
}

#[derive(Clone, Copy)]
enum PasteSide {
    After,
    Before,
}

/// Walk every selection in the focused editor in start-offset
/// order, slice each non-collapsed range out of the rope, and
/// join with `\n`. Returns `None` when the focused pane is not
/// an editor.
fn selections_joined_text(stoat: &mut Stoat) -> Option<String> {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return None,
    };
    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let rope = buf_snap.rope();
    let mut ranges: Vec<(usize, usize)> = editor
        .selections
        .all_anchors()
        .iter()
        .filter_map(|sel| {
            let start = buf_snap.resolve_anchor(&sel.start);
            let end = buf_snap.resolve_anchor(&sel.end);
            let (lo, hi) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            (lo != hi).then_some((lo, hi))
        })
        .collect();
    ranges.sort_unstable();
    let pieces: Vec<String> = ranges
        .into_iter()
        .map(|(lo, hi)| rope.slice(lo..hi).to_string())
        .collect();
    Some(pieces.join("\n"))
}

/// Insert the caller-selected register's content (or the
/// unnamed register when no selection is active) at every
/// selection, either at each selection's `start` (Before) or
/// `end` (After).
fn paste(stoat: &mut Stoat, side: PasteSide) -> UpdateEffect {
    let source = stoat.consume_selected_register();
    let Some(content) = read_register_content(stoat, source) else {
        return UpdateEffect::None;
    };
    paste_text(stoat, &content, side)
}

/// Resolve the textual content of `register` from its backing
/// store: in-memory for named/unnamed, host services for
/// clipboard/search/last-insert, the active selection set for
/// `SelectionIndex`. Returns `None` for blackhole, for read-only
/// registers whose backing is empty, and for `SelectionIndex`
/// when the focused pane has no selections.
pub(crate) fn read_register_content(stoat: &mut Stoat, register: Register) -> Option<String> {
    match register {
        Register::Unnamed | Register::Named(_) => stoat.registers.read(register).map(str::to_owned),
        Register::Clipboard => match stoat.clipboard_host().get() {
            Ok(text) => text,
            Err(err) => {
                tracing::warn!(target: "stoat::yank", ?err, "clipboard read failed");
                None
            },
        },
        Register::Search => stoat.last_search.as_ref().map(|s| s.query.clone()),
        Register::Blackhole => None,
        Register::LastInsert => stoat.last_insert_text.clone(),
        Register::SelectionIndex => selection_index_content(stoat),
    }
}

/// Build a newline-joined "1\n2\n...\nN" for the focused
/// editor's selection count. The string drops into
/// [`paste_text`]'s `line_aware` branch so each selection
/// receives its own index. Returns `None` when the focused pane
/// is not an editor or has no selections.
fn selection_index_content(stoat: &mut Stoat) -> Option<String> {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return None,
    };
    let editor = ws.editors.get_mut(editor_id).expect("editor");
    let count = editor.selections.all_anchors().len();
    if count == 0 {
        return None;
    }
    let pieces: Vec<String> = (1..=count).map(|i| i.to_string()).collect();
    Some(pieces.join("\n"))
}

/// Insert `content` at every selection, either at each
/// selection's `start` (Before) or `end` (After). When `content`
/// has exactly one line per selection (and more than one
/// selection), each selection receives the matching line in
/// start-offset order; otherwise every selection receives the
/// full content. After the edit, every affected selection
/// collapses to a cursor at the end of its inserted text. No-op
/// when `content` is empty or the focused pane is not an editor.
fn paste_text(stoat: &mut Stoat, content: &str, side: PasteSide) -> UpdateEffect {
    if content.is_empty() {
        return UpdateEffect::None;
    }

    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, mut entries) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let rope = buf_snap.rope();
        let entries: Vec<(usize, usize)> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let start = buf_snap.resolve_anchor(&sel.start);
                let end = buf_snap.resolve_anchor(&sel.end);
                let (lo, hi) = if start <= end {
                    (start, end)
                } else {
                    (end, start)
                };
                let insert_at = match side {
                    PasteSide::Before => lo,
                    PasteSide::After => {
                        if lo == hi {
                            rope.chars_at(hi).next().map_or(hi, |c| hi + c.len_utf8())
                        } else {
                            hi
                        }
                    },
                };
                (sel.id, insert_at)
            })
            .collect();
        (buffer_id, entries)
    };

    if entries.is_empty() {
        return UpdateEffect::None;
    }

    entries.sort_by_key(|(_, off)| *off);

    let lines: Vec<&str> = content.split('\n').collect();
    let line_aware = entries.len() > 1 && lines.len() == entries.len();
    let payload_for = |idx: usize| -> &str {
        if line_aware {
            lines[idx]
        } else {
            content
        }
    };

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for (idx, (_, off)) in entries.iter().enumerate().rev() {
            guard.edit(*off..*off, payload_for(idx));
        }
    }

    let mut id_to_caret: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::with_capacity(entries.len());
    let mut shift: i64 = 0;
    for (idx, (id, off)) in entries.iter().enumerate() {
        let payload_len = payload_for(idx).len();
        let caret = (*off as i64 + shift) as usize + payload_len;
        id_to_caret.insert(*id, caret);
        shift += payload_len as i64;
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if let Some(&caret) = id_to_caret.get(&sel.id) {
            let anchor = new_buf.anchor_at(caret, Bias::Left);
            new.collapse_to(anchor, SelectionGoal::None);
        }
        new
    });
    UpdateEffect::Redraw
}
