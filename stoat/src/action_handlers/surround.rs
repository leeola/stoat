use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
};
use stoat_text::{Bias, SelectionGoal};

pub(super) fn surround_add(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_surround_add = true;
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

fn surround_pair_for(ch: char) -> (char, char) {
    match ch {
        '(' | ')' => ('(', ')'),
        '[' | ']' => ('[', ']'),
        '{' | '}' => ('{', '}'),
        '<' | '>' => ('<', '>'),
        other => (other, other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{action_handlers::focused_editor_mut, test_harness::TestHarness};
    use std::path::PathBuf;
    use stoat_action::{self as action, OpenFile};

    fn seed(h: &mut TestHarness, contents: &str) -> PathBuf {
        let root = PathBuf::from("/surround-test");
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
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let head = editor.selections.newest_anchor().head();
        buf_snap.resolve_anchor(&head)
    }

    #[test]
    fn pair_for_brackets() {
        assert_eq!(surround_pair_for('('), ('(', ')'));
        assert_eq!(surround_pair_for(')'), ('(', ')'));
        assert_eq!(surround_pair_for('['), ('[', ']'));
        assert_eq!(surround_pair_for(']'), ('[', ']'));
        assert_eq!(surround_pair_for('{'), ('{', '}'));
        assert_eq!(surround_pair_for('}'), ('{', '}'));
        assert_eq!(surround_pair_for('<'), ('<', '>'));
        assert_eq!(surround_pair_for('>'), ('<', '>'));
    }

    #[test]
    fn pair_for_quotes_doubles_char() {
        assert_eq!(surround_pair_for('"'), ('"', '"'));
        assert_eq!(surround_pair_for('\''), ('\'', '\''));
        assert_eq!(surround_pair_for('`'), ('`', '`'));
    }

    #[test]
    fn pair_for_arbitrary_char_doubles() {
        assert_eq!(surround_pair_for('*'), ('*', '*'));
        assert_eq!(surround_pair_for('|'), ('|', '|'));
    }

    #[test]
    fn surround_add_wraps_selection_with_paren() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("(");
        assert_eq!(buffer_text(&h, &path), "(abc)\n");
        assert!(!h.stoat.pending_surround_add);
    }

    #[test]
    fn surround_add_close_char_wraps_with_canonical_pair() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys(">");
        assert_eq!(buffer_text(&h, &path), "<abc>\n");
    }

    #[test]
    fn surround_add_quote_wraps_with_same_char() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("\"");
        assert_eq!(buffer_text(&h, &path), "\"abc\"\n");
    }

    #[test]
    fn surround_add_arbitrary_char_doubles() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("*");
        assert_eq!(buffer_text(&h, &path), "*abc*\n");
    }

    #[test]
    fn surround_add_collapsed_selection_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        let before = cursor_offset(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("(");
        assert_eq!(buffer_text(&h, &path), "abc\n");
        assert_eq!(cursor_offset(&mut h), before);
        assert!(!h.stoat.pending_surround_add);
    }

    #[test]
    fn surround_add_pending_clears_on_non_char() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        assert!(h.stoat.pending_surround_add);
        h.type_keys("escape");
        assert!(!h.stoat.pending_surround_add);
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn surround_add_via_match_mode_binding() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l l");
        h.type_keys("escape");
        h.type_keys("m s [");
        assert_eq!(buffer_text(&h, &path), "[abc]\n");
        assert_eq!(h.stoat.mode, "normal");
    }
}
