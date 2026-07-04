use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
    register::Register,
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

/// Resolve [`Register`] from the consumed-char keypress for the
/// pending [`crate::app::Stoat::pending_insert_register`] chord
/// and the `SelectRegister` chord. `"` -> `Unnamed`; ASCII
/// letter -> `Named`; helix special chars route to the matching
/// special variant; any other char returns `None`.
pub(crate) fn register_for_char(ch: char) -> Option<Register> {
    match ch {
        '"' => Some(Register::Unnamed),
        '*' | '+' => Some(Register::Clipboard),
        '/' => Some(Register::Search),
        '_' => Some(Register::Blackhole),
        '#' => Some(Register::SelectionIndex),
        '.' => Some(Register::LastInsert),
        _ if ch.is_ascii_alphabetic() => Some(Register::Named(ch)),
        _ => None,
    }
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

#[cfg(test)]
mod tests {
    use crate::{host::ClipboardHost, test_harness::TestHarness};
    use std::path::PathBuf;
    use stoat_action::{self as action, OpenFile};

    fn seed(h: &mut TestHarness, contents: &str) -> PathBuf {
        let root = PathBuf::from("/yank-test");
        let path = root.join("buf.txt");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        path
    }

    fn buffer_text(h: &TestHarness, path: &std::path::Path) -> String {
        let ws = h.stoat.active_workspace();
        let id = ws.buffers.id_for_path(path).expect("buffer registered");
        let buf = ws.buffers.get(id).expect("buffer present");
        let guard = buf.read().expect("buffer lock");
        guard.rope().to_string()
    }

    fn cursor_offset(h: &mut TestHarness) -> usize {
        let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        buf_snap.resolve_anchor(&head)
    }

    #[test]
    fn yank_stores_primary_selection_in_unnamed() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(str::to_owned);
        assert_eq!(stored, Some("abc".to_string()));
    }

    #[test]
    fn yank_collapsed_selection_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(str::to_owned);
        assert_eq!(stored, None);
    }

    #[test]
    fn paste_after_inserts_at_selection_end() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        h.type_keys("escape");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abcabc\n");
        assert_eq!(cursor_offset(&mut h), 6);
    }

    #[test]
    fn paste_before_inserts_at_selection_start() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        h.type_keys("escape");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteBefore);
        assert_eq!(buffer_text(&h, &path), "abcabc\n");
        assert_eq!(cursor_offset(&mut h), 3);
    }

    #[test]
    fn paste_with_empty_register_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn paste_after_with_collapsed_cursor_inserts_at_cursor() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        h.type_keys("escape");
        h.type_keys("h");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abcabc\n");
    }

    #[test]
    fn yank_via_y_binding() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        h.type_keys("escape");
        h.type_keys("y");
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(str::to_owned);
        assert_eq!(stored, Some("abc".to_string()));
    }

    #[test]
    fn paste_after_via_p_binding() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        h.type_keys("y");
        h.type_keys("escape");
        h.type_keys("p");
        assert_eq!(buffer_text(&h, &path), "abcabc\n");
    }

    #[test]
    fn paste_before_via_capital_p_binding() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        h.type_keys("y");
        h.type_keys("escape");
        h.type_keys("P");
        assert_eq!(buffer_text(&h, &path), "abcabc\n");
    }

    fn make_two_selections(h: &mut TestHarness) {
        crate::action_handlers::dispatch(&mut h.stoat, &action::AddSelectionBelow);
        h.stoat.set_focused_mode("select".into());
        crate::action_handlers::dispatch(&mut h.stoat, &action::ExtendRight);
        crate::action_handlers::dispatch(&mut h.stoat, &action::ExtendRight);
        crate::action_handlers::dispatch(&mut h.stoat, &action::ExtendRight);
        h.stoat.set_focused_mode("normal".into());
    }

    #[test]
    fn yank_joins_multi_selection_with_newlines() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        assert_eq!(h.selection_spans(), vec![(0, 3, false), (4, 7, false)]);
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(str::to_owned);
        assert_eq!(stored, Some("abc\ndef".to_string()));
    }

    #[test]
    fn paste_after_with_line_match_pastes_line_per_selection() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abcabc\ndefdef\n");
    }

    #[test]
    fn paste_after_with_line_count_mismatch_pastes_full_at_each() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "ab\ncd\nef\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        crate::action_handlers::dispatch(&mut h.stoat, &action::AddSelectionBelow);
        crate::action_handlers::dispatch(&mut h.stoat, &action::AddSelectionBelow);
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abab\ncd\nabef\nab");
    }

    #[test]
    fn yank_to_clipboard_writes_joined_selections() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &action::YankToClipboard);
        assert_eq!(h.fake_clipboard().writes(), vec!["abc\ndef".to_string()]);
    }

    #[test]
    fn yank_main_to_clipboard_writes_only_primary() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &action::YankMainToClipboard);
        assert_eq!(h.fake_clipboard().writes(), vec!["def".to_string()]);
    }

    #[test]
    fn yank_to_clipboard_with_collapsed_selection_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::YankToClipboard);
        assert_eq!(h.fake_clipboard().writes(), Vec::<String>::new());
    }

    #[test]
    fn paste_clipboard_after_inserts_clipboard_content() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.fake_clipboard().set("xyz").unwrap();
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteClipboardAfter);
        assert_eq!(buffer_text(&h, &path), "abcxyz\n");
    }

    #[test]
    fn paste_clipboard_before_inserts_clipboard_content() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.fake_clipboard().set("xyz").unwrap();
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteClipboardBefore);
        assert_eq!(buffer_text(&h, &path), "xyzabc\n");
    }

    #[test]
    fn paste_clipboard_with_empty_clipboard_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteClipboardAfter);
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn yank_to_clipboard_via_space_dquote_y_binding() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        h.type_keys("escape");
        h.type_keys("space \" y");
        assert_eq!(h.fake_clipboard().writes(), vec!["abc".to_string()]);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn select_register_then_yank_writes_to_named() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        h.type_keys("escape");
        h.type_keys("\" a y");
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Named('a'))
            .map(str::to_owned);
        assert_eq!(stored, Some("abc".to_string()));
        let unnamed = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(str::to_owned);
        assert_eq!(unnamed, None);
    }

    #[test]
    fn select_register_consumed_by_one_op() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        h.type_keys("escape");
        h.type_keys("\" a y");
        assert!(h.stoat.selected_register.is_none());
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        let stored_a = h
            .stoat
            .registers
            .read(crate::register::Register::Named('a'))
            .map(str::to_owned);
        assert_eq!(stored_a, Some("abc".to_string()));
        let unnamed = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(str::to_owned);
        assert_eq!(unnamed, Some("abc".to_string()));
    }

    #[test]
    fn paste_from_named_register() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Named('a'), "xyz".to_string());
        h.type_keys("v l l l");
        h.type_keys("escape");
        h.type_keys("\" a p");
        assert_eq!(buffer_text(&h, &path), "abcxyz\n");
    }

    #[test]
    fn select_register_dquote_selects_unnamed() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        h.type_keys("escape");
        h.type_keys("\" \" y");
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(str::to_owned);
        assert_eq!(stored, Some("abc".to_string()));
    }

    #[test]
    fn insert_register_inserts_named_at_cursor() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Named('a'), "xyz".to_string());
        h.type_keys("a");
        h.type_keys("Ctrl-r");
        h.type_keys("a");
        assert_eq!(buffer_text(&h, &path), "axyzbc\n");
    }

    #[test]
    fn insert_register_inserts_unnamed_via_dquote() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, "xyz".to_string());
        h.type_keys("a");
        h.type_keys("Ctrl-r");
        h.type_keys("\"");
        assert_eq!(buffer_text(&h, &path), "axyzbc\n");
    }

    #[test]
    fn insert_register_with_empty_register_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("a");
        h.type_keys("Ctrl-r");
        h.type_keys("a");
        assert_eq!(buffer_text(&h, &path), "abc\n");
        assert!(!h.stoat.pending_insert_register);
    }

    #[test]
    fn insert_register_escape_cancels() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Named('a'), "xyz".to_string());
        h.type_keys("a");
        h.type_keys("Ctrl-r");
        h.type_keys("escape");
        assert!(!h.stoat.pending_insert_register);
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn yank_clipboard_register_writes_to_clipboard_host() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        h.type_keys("escape");
        h.type_keys("\" * y");
        assert_eq!(h.fake_clipboard().writes(), vec!["abc".to_string()]);
        let unnamed = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(str::to_owned);
        assert_eq!(unnamed, None);
    }

    #[test]
    fn paste_clipboard_register_reads_from_clipboard_host() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.fake_clipboard().set("xyz").unwrap();
        h.type_keys("escape");
        h.type_keys("\" * p");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "axyzbc\n");
    }

    #[test]
    fn yank_blackhole_register_swallows_content() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        h.type_keys("escape");
        h.type_keys("\" _ y");
        let unnamed = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(str::to_owned);
        assert_eq!(unnamed, None);
        assert_eq!(h.fake_clipboard().writes(), Vec::<String>::new());
    }

    #[test]
    fn paste_search_register_pastes_last_search_query() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat.last_search = Some(crate::action_handlers::search::LastSearch {
            query: "needle".into(),
            direction: crate::action_handlers::search::SearchDirection::Forward,
        });
        h.type_keys("escape");
        h.type_keys("\" / p");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "aneedlebc\n");
    }

    #[test]
    fn paste_search_register_no_op_when_no_search() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat.last_search = None;
        h.type_keys("escape");
        h.type_keys("\" / p");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn paste_last_insert_register_pastes_recent_insert() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("a");
        h.type_text("hi");
        h.type_keys("escape");
        h.type_keys("\" . p");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert!(buffer_text(&h, &path).contains("hi"));
        assert_eq!(h.stoat.last_insert_text.as_deref(), Some("hi"));
    }

    #[test]
    fn paste_selection_index_pastes_one_per_selection() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "ab\ncd\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::AddSelectionBelow);
        h.type_keys("\" # p");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        let text = buffer_text(&h, &path);
        assert!(text.contains('1'), "expected '1' in {text:?}");
        assert!(text.contains('2'), "expected '2' in {text:?}");
    }
}
