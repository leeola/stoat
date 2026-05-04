use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
    register::Register,
};
use stoat_text::{Bias, SelectionGoal};

/// Copy the focused editor's primary selection content into the
/// unnamed register. Selections are not modified. No-op when the
/// primary selection is collapsed (empty range) or the focused
/// pane is not an editor.
pub(super) fn yank(stoat: &mut Stoat) -> UpdateEffect {
    let Some(content) = primary_selection_text(stoat) else {
        return UpdateEffect::None;
    };
    if content.is_empty() {
        return UpdateEffect::None;
    }
    stoat.registers.write(Register::Unnamed, content);
    UpdateEffect::None
}

pub(super) fn paste_after(stoat: &mut Stoat) -> UpdateEffect {
    paste(stoat, PasteSide::After)
}

pub(super) fn paste_before(stoat: &mut Stoat) -> UpdateEffect {
    paste(stoat, PasteSide::Before)
}

#[derive(Clone, Copy)]
enum PasteSide {
    After,
    Before,
}

/// Extract the focused editor's primary selection content as a
/// `String`. Returns `None` when the focused pane is not an
/// editor.
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
    Some(buf_snap.rope().slice(lo..hi).to_string())
}

/// Insert the unnamed register's content at every selection,
/// either at each selection's `start` (Before) or `end` (After).
/// After the edit, every affected selection collapses to a cursor
/// at the end of the inserted text. No-op when the register is
/// empty or unset, or when the focused pane is not an editor.
fn paste(stoat: &mut Stoat, side: PasteSide) -> UpdateEffect {
    let Some(content) = stoat.registers.read(Register::Unnamed).map(str::to_owned) else {
        return UpdateEffect::None;
    };
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

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        for (_, off) in entries.iter().rev() {
            guard.edit(*off..*off, &content);
        }
    }

    let content_len = content.len();
    let mut id_to_caret: std::collections::HashMap<usize, usize> =
        std::collections::HashMap::with_capacity(entries.len());
    let mut shift: i64 = 0;
    for (id, off) in entries.iter() {
        let caret = (*off as i64 + shift) as usize + content_len;
        id_to_caret.insert(*id, caret);
        shift += content_len as i64;
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
    use crate::test_harness::TestHarness;
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
}
