use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
};
use stoat_text::{Bias, Rope, SelectionGoal};

/// Two-step capture state for [`surround_replace`]: arms after the action
/// fires, transitions to [`SurroundReplaceStage::AwaitTo`] once the user
/// types the from-char, then back to [`SurroundReplaceStage::Idle`] after
/// the to-char applies the edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub(crate) enum SurroundReplaceStage {
    #[default]
    Idle,
    AwaitFrom,
    AwaitTo(char),
}

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

fn surround_pair_for(ch: char) -> (char, char) {
    match ch {
        '(' | ')' => ('(', ')'),
        '[' | ']' => ('[', ']'),
        '{' | '}' => ('{', '}'),
        '<' | '>' => ('<', '>'),
        other => (other, other),
    }
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

/// Plain (non-tree-sitter) variant of Helix's `find_nth_pairs_pos`.
/// Walks the rope outward from `cursor` (a byte offset) to find the
/// nearest enclosing pair for `(open, close)`. Asymmetric pairs use
/// depth tracking so nested pairs do not confuse the search;
/// symmetric pairs (`open == close`) take the nearest occurrence in
/// each direction. When the cursor sits exactly on a symmetric char
/// the search bails because there is no way to know which side of
/// the cursor is the open. Returns `(open_byte, close_byte)` --
/// each is the byte offset of the corresponding pair char in the
/// rope -- or `None` when no enclosing pair exists.
/// When `tree` is `Some`, candidate brackets / quotes whose offset
/// lies inside a string or comment node are skipped during the walk;
/// the pair-depth counter does not advance for skipped chars. `None`
/// keeps the plain non-tree-sitter behaviour for buffers without a
/// syntax map. Used by `execute_surround_replace` and
/// `execute_surround_delete` so `m r ( )` and `m d (` ignore
/// brackets that happen to live inside string literals.
pub(crate) fn find_surround_pair(
    rope: &Rope,
    cursor: usize,
    open: char,
    close: char,
    tree: Option<&stoat_language::Tree>,
) -> Option<(usize, usize)> {
    if open == close {
        if rope.chars_at(cursor).next() == Some(open) {
            return None;
        }
        let open_pos = walk_left_for_symmetric(rope, cursor, open, tree)?;
        let close_pos = walk_right_for_symmetric(rope, cursor, open, tree)?;
        Some((open_pos, close_pos))
    } else {
        let open_pos = walk_left_for_open(rope, cursor, open, close, tree)?;
        let close_pos = walk_right_for_close(rope, cursor, open, close, tree)?;
        Some((open_pos, close_pos))
    }
}

fn in_skip_zone(tree: Option<&stoat_language::Tree>, offset: usize) -> bool {
    match tree {
        Some(t) => super::movement::is_in_string_or_comment(t, offset),
        None => false,
    }
}

fn walk_right_for_close(
    rope: &Rope,
    cursor: usize,
    open: char,
    close: char,
    tree: Option<&stoat_language::Tree>,
) -> Option<usize> {
    let mut chars = rope.chars_at(cursor);
    let mut pos = cursor;
    let first = chars.next()?;
    if first == close && !in_skip_zone(tree, pos) {
        return Some(pos);
    }
    pos += first.len_utf8();
    let mut step_over: usize = 0;
    for c in chars {
        let skip = in_skip_zone(tree, pos);
        if !skip {
            if c == open {
                step_over += 1;
            } else if c == close {
                if step_over == 0 {
                    return Some(pos);
                }
                step_over -= 1;
            }
        }
        pos += c.len_utf8();
    }
    None
}

fn walk_left_for_open(
    rope: &Rope,
    cursor: usize,
    open: char,
    close: char,
    tree: Option<&stoat_language::Tree>,
) -> Option<usize> {
    if rope.chars_at(cursor).next() == Some(open) && !in_skip_zone(tree, cursor) {
        return Some(cursor);
    }
    let mut pos = cursor;
    let mut step_over: usize = 0;
    for c in rope.reversed_chars_at(cursor) {
        pos = pos.checked_sub(c.len_utf8())?;
        let skip = in_skip_zone(tree, pos);
        if !skip {
            if c == close {
                step_over += 1;
            } else if c == open {
                if step_over == 0 {
                    return Some(pos);
                }
                step_over -= 1;
            }
        }
    }
    None
}

fn walk_right_for_symmetric(
    rope: &Rope,
    cursor: usize,
    ch: char,
    tree: Option<&stoat_language::Tree>,
) -> Option<usize> {
    let mut pos = cursor;
    for c in rope.chars_at(cursor) {
        if c == ch && !in_skip_zone(tree, pos) {
            return Some(pos);
        }
        pos += c.len_utf8();
    }
    None
}

fn walk_left_for_symmetric(
    rope: &Rope,
    cursor: usize,
    ch: char,
    tree: Option<&stoat_language::Tree>,
) -> Option<usize> {
    let mut pos = cursor;
    for c in rope.reversed_chars_at(cursor) {
        pos = pos.checked_sub(c.len_utf8())?;
        if c == ch && !in_skip_zone(tree, pos) {
            return Some(pos);
        }
    }
    None
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

    fn rope(s: &str) -> Rope {
        let mut r = Rope::new();
        r.push(s);
        r
    }

    #[test]
    fn find_pair_paren_cursor_inside() {
        let r = rope("(abc)");
        assert_eq!(find_surround_pair(&r, 2, '(', ')', None), Some((0, 4)));
    }

    #[test]
    fn find_pair_paren_cursor_on_open() {
        let r = rope("(abc)");
        assert_eq!(find_surround_pair(&r, 0, '(', ')', None), Some((0, 4)));
    }

    #[test]
    fn find_pair_paren_cursor_on_close() {
        let r = rope("(abc)");
        assert_eq!(find_surround_pair(&r, 4, '(', ')', None), Some((0, 4)));
    }

    #[test]
    fn find_pair_paren_no_match_returns_none() {
        let r = rope("abc");
        assert_eq!(find_surround_pair(&r, 1, '(', ')', None), None);
    }

    #[test]
    fn find_pair_nested_paren_finds_innermost() {
        let r = rope("((abc))");
        assert_eq!(find_surround_pair(&r, 3, '(', ')', None), Some((1, 5)));
    }

    #[test]
    fn find_pair_unbalanced_paren_returns_none() {
        let r = rope("(abc");
        assert_eq!(find_surround_pair(&r, 1, '(', ')', None), None);
    }

    #[test]
    fn find_pair_quote_cursor_inside() {
        let r = rope("\"abc\"");
        assert_eq!(find_surround_pair(&r, 2, '"', '"', None), Some((0, 4)));
    }

    #[test]
    fn find_pair_quote_cursor_on_quote_is_ambiguous() {
        let r = rope("\"abc\"");
        assert_eq!(find_surround_pair(&r, 0, '"', '"', None), None);
        assert_eq!(find_surround_pair(&r, 4, '"', '"', None), None);
    }

    #[test]
    fn find_pair_quote_no_match_returns_none() {
        let r = rope("abc");
        assert_eq!(find_surround_pair(&r, 1, '"', '"', None), None);
    }

    #[test]
    fn surround_replace_paren_with_bracket() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(abc)\n");
        h.type_keys("l l");
        h.type_keys("m r ( [");
        assert_eq!(buffer_text(&h, &path), "[abc]\n");
        assert_eq!(h.stoat.mode, "normal");
        assert_eq!(h.stoat.pending_surround_replace, SurroundReplaceStage::Idle,);
    }

    #[test]
    fn surround_replace_quote_with_quote() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "\"abc\"\n");
        h.type_keys("l l");
        h.type_keys("m r \" '");
        assert_eq!(buffer_text(&h, &path), "'abc'\n");
    }

    #[test]
    fn surround_replace_no_enclosing_pair_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("l");
        h.type_keys("m r ( [");
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn surround_delete_paren() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(abc)\n");
        h.type_keys("l l");
        h.type_keys("m d (");
        assert_eq!(buffer_text(&h, &path), "abc\n");
        assert!(!h.stoat.pending_surround_delete);
    }

    #[test]
    fn surround_delete_quote() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "\"abc\"\n");
        h.type_keys("l l");
        h.type_keys("m d \"");
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn surround_delete_no_enclosing_pair_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("m d (");
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn surround_replace_pending_clears_on_non_char_in_await_from() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(abc)\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundReplace);
        assert_eq!(
            h.stoat.pending_surround_replace,
            SurroundReplaceStage::AwaitFrom,
        );
        h.type_keys("escape");
        assert_eq!(h.stoat.pending_surround_replace, SurroundReplaceStage::Idle,);
        assert_eq!(buffer_text(&h, &path), "(abc)\n");
    }

    #[test]
    fn surround_delete_pending_clears_on_non_char() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(abc)\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundDelete);
        assert!(h.stoat.pending_surround_delete);
        h.type_keys("escape");
        assert!(!h.stoat.pending_surround_delete);
        assert_eq!(buffer_text(&h, &path), "(abc)\n");
    }

    fn seed_rs(h: &mut TestHarness, contents: &str) -> PathBuf {
        let root = PathBuf::from("/surround-test");
        let path = root.join("main.rs");
        h.fake_fs()
            .insert_files(std::iter::once((path.clone(), contents.as_bytes())));
        h.stoat.active_workspace_mut().git_root = root;
        crate::action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        let _ = h.stoat.render();
        h.settle();
        let _ = h.stoat.render();
        h.settle();
        path
    }

    #[test]
    fn surround_delete_skips_brackets_inside_string() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "let _ = (\"outer (inner)\");\n";
        let path = seed_rs(&mut h, src);
        let cursor = src.find("outer").expect("cursor target");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);
        h.type_keys("m d (");
        assert_eq!(buffer_text(&h, &path), "let _ = \"outer (inner)\";\n");
    }

    #[test]
    fn surround_replace_skips_brackets_inside_comment() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "fn f() { /* (foo) */ let x = (bar); }\n";
        let path = seed_rs(&mut h, src);
        let cursor = src.find("bar").expect("cursor target");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);
        h.type_keys("m r ( [");
        assert_eq!(
            buffer_text(&h, &path),
            "fn f() { /* (foo) */ let x = [bar]; }\n",
        );
    }
}
