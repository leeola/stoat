use crate::{
    action_handlers::movement::MAX_PAIR_SCAN,
    app::{Stoat, UpdateEffect},
    buffer::TextBuffer,
    pane::View,
};
use std::ops::Range;
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
        // The close sits past the open within a selection and the selections
        // are visited back to front, so flattening the pair per selection
        // leaves the whole sequence descending.
        let batch: Vec<(Range<usize>, &str)> = entries
            .iter()
            .rev()
            .flat_map(|(_, s, e, _)| [(*e..*e, close_str.as_str()), (*s..*s, open_str.as_str())])
            .collect();
        guard.edit_batch(&batch);
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

pub(crate) fn surround_pair_for(ch: char) -> (char, char) {
    match ch {
        '(' | ')' => ('(', ')'),
        '[' | ']' => ('[', ']'),
        '{' | '}' => ('{', '}'),
        '<' | '>' => ('<', '>'),
        other => (other, other),
    }
}

/// Apply the consumed-char keypress to the pending surround_delete
/// chord. For every selection's primary cursor, find the nearest
/// enclosing surround pair and remove its open / close. `ch` names the
/// pair type, except `m`, which means the nearest pair of any type (so a
/// literal `m...m` pair is unreachable). Selections whose cursor is not
/// enclosed by a matching pair are skipped. Pairs are deduped before
/// edits run, so two cursors inside the same pair produce one edit.
pub(crate) fn execute_surround_delete(stoat: &mut Stoat, ch: char) -> UpdateEffect {
    let pair = (ch != 'm').then(|| surround_pair_for(ch));
    let pairs = match collect_surround_pairs(stoat, pair) {
        Some(p) if !p.is_empty() => p,
        _ => return UpdateEffect::None,
    };

    let buffer_id = focused_buffer_id(stoat).expect("checked by collect_surround_pairs");
    let ws = stoat.active_workspace_mut();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let mut guard = buffer.write().expect("poisoned");

    let edits = pairs
        .iter()
        .flat_map(|(open_off, close_off, open, close)| {
            [
                (*open_off, open.len_utf8(), ""),
                (*close_off, close.len_utf8(), ""),
            ]
        })
        .collect();
    edit_delimiters(&mut guard, edits);

    UpdateEffect::Redraw
}

/// Apply the consumed two-char keypresses to the pending
/// surround_replace chord. For every selection's primary cursor, find
/// the nearest enclosing surround pair and replace its open / close with
/// the canonical pair for `to`. `from` names the pair type, except `m`,
/// which means the nearest pair of any type. `to` is always a literal
/// pair char. Selections whose cursor is not enclosed by a matching pair
/// are skipped. Pairs are deduped before edits run.
pub(crate) fn execute_surround_replace(stoat: &mut Stoat, from: char, to: char) -> UpdateEffect {
    let from_pair = (from != 'm').then(|| surround_pair_for(from));
    let (new_open, new_close) = surround_pair_for(to);
    let pairs = match collect_surround_pairs(stoat, from_pair) {
        Some(p) if !p.is_empty() => p,
        _ => return UpdateEffect::None,
    };

    let buffer_id = focused_buffer_id(stoat).expect("checked by collect_surround_pairs");
    let ws = stoat.active_workspace_mut();
    let buffer = ws.buffers.get(buffer_id).expect("buffer");
    let mut guard = buffer.write().expect("poisoned");
    let new_open_str = new_open.to_string();
    let new_close_str = new_close.to_string();

    let edits = pairs
        .iter()
        .flat_map(|(open_off, close_off, old_open, old_close)| {
            [
                (*open_off, old_open.len_utf8(), new_open_str.as_str()),
                (*close_off, old_close.len_utf8(), new_close_str.as_str()),
            ]
        })
        .collect();
    edit_delimiters(&mut guard, edits);

    UpdateEffect::Redraw
}

/// Apply delimiter edits given as `(offset, len, replacement)`, from the end of
/// the buffer backwards.
///
/// The offsets were read off the text as it stood before any of them ran, and
/// nested pairs interleave their four delimiters, so the outer close sits past
/// the inner pair and moves when the inner pair is edited. Taking the whole set
/// from the end means every edit only ever disturbs text already dealt with.
///
/// Delimiters at one offset collapse to a single edit. A quote can close one
/// pair and open the next, as the middle quote of `"a"b"` does for cursors on
/// either side of it, and editing that offset twice would consume the character
/// after it.
fn edit_delimiters(buffer: &mut TextBuffer, mut edits: Vec<(usize, usize, &str)>) {
    edits.sort_unstable_by_key(|&(offset, _, _)| offset);
    edits.dedup_by_key(|&mut (offset, _, _)| offset);

    let batch: Vec<(Range<usize>, &str)> = edits
        .iter()
        .rev()
        .map(|(offset, len, replacement)| (*offset..*offset + *len, *replacement))
        .collect();
    buffer.edit_batch(&batch);
}

/// Walk every selection's primary cursor in the focused editor and
/// gather the enclosing surround pair per cursor, each carrying its own
/// delimiter chars as `(open_off, close_off, open, close)`. Returns the
/// deduped pairs sorted ascending, or `None` when the focused pane is
/// not an editor.
///
/// The `pair` argument chooses how each cursor resolves.
/// `Some((open, close))` finds the enclosing pair of that one type via
/// [`surround_pair_at`]. `None` (the `m` case) finds the innermost
/// enclosing pair of any type via [`closest_pair_at`], which may resolve
/// different types across cursors. Either way brackets inside string /
/// comment nodes are skipped when the buffer has a syntax map.
fn collect_surround_pairs(
    stoat: &mut Stoat,
    pair: Option<(char, char)>,
) -> Option<Vec<(usize, usize, char, char)>> {
    let (buffer_id, cursors) = {
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
        let cursors: Vec<usize> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
                let head_off = buffer_snapshot.resolve_anchor(&sel.head());
                stoat_text::cursor_offset(buffer_snapshot.rope(), tail_off, head_off)
            })
            .collect();
        (buffer_id, cursors)
    };

    let ws = stoat.active_workspace();
    let mut pairs: Vec<(usize, usize, char, char)> = cursors
        .into_iter()
        .filter_map(|head| match pair {
            Some((open, close)) => surround_pair_at(ws, buffer_id, head, open, close)
                .map(|(open_off, close_off)| (open_off, close_off, open, close)),
            None => closest_pair_at(ws, buffer_id, head),
        })
        .collect();
    pairs.sort_unstable();
    pairs.dedup();
    Some(pairs)
}

/// The nearest enclosing `(open, close)` pair around `cursor` in the
/// buffer's live rope, resolved against the deepest syntax layer
/// covering the cursor so brackets inside string / comment nodes are
/// skipped. `None` when no enclosing pair exists. Shared by the
/// surround delete/replace collection and the `m i`/`m a` pair-char
/// textobjects.
pub(crate) fn surround_pair_at(
    ws: &crate::workspace::Workspace,
    buffer_id: crate::buffer::BufferId,
    cursor: usize,
    open: char,
    close: char,
) -> Option<(usize, usize)> {
    let buffer = ws.buffers.get(buffer_id)?;
    let rope = buffer.read().expect("poisoned").rope().clone();
    let snapshot = ws.buffers.syntax_map(buffer_id).map(|m| m.snapshot());
    let tree = deepest_tree_at(snapshot, cursor);
    find_surround_pair(&rope, cursor, open, close, tree)
}

/// The innermost enclosing pair of any type around `cursor`, as
/// `(open_off, close_off, open, close)`. The `None`-pair variant of
/// [`surround_pair_at`], driving `m d m` / `m r m`. Delegates to
/// [`closest_surround_pair`] over the deepest covering syntax layer.
fn closest_pair_at(
    ws: &crate::workspace::Workspace,
    buffer_id: crate::buffer::BufferId,
    cursor: usize,
) -> Option<(usize, usize, char, char)> {
    let buffer = ws.buffers.get(buffer_id)?;
    let rope = buffer.read().expect("poisoned").rope().clone();
    let snapshot = ws.buffers.syntax_map(buffer_id).map(|m| m.snapshot());
    let tree = deepest_tree_at(snapshot, cursor);
    closest_surround_pair(&rope, cursor, tree)
        .map(|(open, close, open_off, close_off)| (open_off, close_off, open, close))
}

/// The tree of the deepest syntax layer whose byte span covers
/// `offset`, or `None` when there is no syntax map or no covering
/// layer. The pair search skips brackets inside string / comment nodes
/// of this tree.
pub(crate) fn deepest_tree_at(
    snapshot: Option<&stoat_language::SyntaxSnapshot>,
    offset: usize,
) -> Option<&stoat_language::Tree> {
    snapshot?
        .iter_layers()
        .fold(None::<&stoat_language::SyntaxLayer>, |acc, layer| {
            let lstart = layer.start_offset as usize;
            let lend = layer.end_offset as usize;
            if lstart <= offset && lend >= offset {
                match acc {
                    Some(prev) if prev.depth >= layer.depth => acc,
                    _ => Some(layer),
                }
            } else {
                acc
            }
        })
        .map(|layer| &layer.tree)
}

/// Every pair type the closest-pair textobject considers.
///
/// Their delimiter characters are disjoint, so a character is a delimiter for
/// at most one entry here. That is what lets one walk serve all of them.
const SURROUND_PAIRS: [(char, char); 7] = [
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('<', '>'),
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
];

/// The innermost enclosing pair of any surround type around `cursor`,
/// as `(open, close, open_off, close_off)`. Runs [`find_surround_pair`]
/// for each bracket and quote pair and keeps the one with the greatest
/// `open_off` (deepest enclosing). `None` when no pair type encloses
/// the cursor. Drives the `m i m` / `m a m` closest-pair textobject.
pub(crate) fn closest_surround_pair(
    rope: &Rope,
    cursor: usize,
    tree: Option<&stoat_language::Tree>,
) -> Option<(char, char, usize, usize)> {
    let at_cursor = rope.chars_at(cursor).next();

    // A quote under the cursor cannot say which of its sides opens, so its type
    // is answered by the syntax tree rather than by walking. Deciding that here
    // keeps those types out of both walks entirely.
    let mut resolved: [Option<(usize, usize)>; SURROUND_PAIRS.len()] = Default::default();
    let mut settled = [false; SURROUND_PAIRS.len()];
    for (i, (open, close)) in SURROUND_PAIRS.into_iter().enumerate() {
        if open == close && at_cursor == Some(open) {
            settled[i] = true;
            resolved[i] = enclosing_string_pair(rope, tree, cursor, open);
        }
    }

    let opens = scan_left_for_opens(rope, cursor, at_cursor, &settled, tree);
    let closes = scan_right_for_closes(rope, cursor, at_cursor, &settled, tree);
    for i in 0..SURROUND_PAIRS.len() {
        if !settled[i] {
            resolved[i] = opens[i].zip(closes[i]);
        }
    }

    SURROUND_PAIRS
        .into_iter()
        .enumerate()
        .filter_map(|(i, (open, close))| {
            resolved[i].map(|(open_off, close_off)| (open, close, open_off, close_off))
        })
        .max_by_key(|&(_, _, open_off, _)| open_off)
}

/// Which pair type `c` is a delimiter of, if any.
fn pair_index(c: char) -> Option<usize> {
    SURROUND_PAIRS
        .iter()
        .position(|&(open, close)| c == open || c == close)
}

/// The nearest enclosing open left of `cursor`, per pair type, in one walk.
///
/// Each asymmetric type carries the depth counter its own walk carried, and
/// each symmetric type takes the first occurrence it meets. A type that has its
/// answer drops out, so the walk only continues for those still looking.
fn scan_left_for_opens(
    rope: &Rope,
    cursor: usize,
    at_cursor: Option<char>,
    settled: &[bool; SURROUND_PAIRS.len()],
    tree: Option<&stoat_language::Tree>,
) -> [Option<usize>; SURROUND_PAIRS.len()] {
    let mut found: [Option<usize>; SURROUND_PAIRS.len()] = Default::default();
    let mut step_over = [0usize; SURROUND_PAIRS.len()];
    let mut done = *settled;

    // An open under the cursor is that type's open, without walking anywhere.
    for (i, (open, close)) in SURROUND_PAIRS.into_iter().enumerate() {
        if !done[i] && open != close && at_cursor == Some(open) && !in_skip_zone(tree, cursor) {
            found[i] = Some(cursor);
            done[i] = true;
        }
    }

    let mut pos = cursor;
    for c in rope.reversed_chars_at(cursor).take(MAX_PAIR_SCAN) {
        let Some(next) = pos.checked_sub(c.len_utf8()) else {
            break;
        };
        pos = next;

        let Some(i) = pair_index(c) else { continue };
        if done[i] || in_skip_zone(tree, pos) {
            continue;
        }

        let (open, close) = SURROUND_PAIRS[i];
        if open == close || c == open && step_over[i] == 0 {
            found[i] = Some(pos);
            done[i] = true;
        } else if c == close {
            step_over[i] += 1;
        } else {
            step_over[i] -= 1;
        }
    }
    found
}

/// The matching close right of `cursor`, per pair type, in one walk.
///
/// The mirror of [`scan_left_for_opens`], with one inherited quirk. The
/// character under the cursor counts as a close but not as an open, which is
/// what the per-type walk did by testing it before its loop began.
///
/// The cap is one budget shared by every type, where each type used to have its
/// own of the same size. Those budgets began a character apart for the two
/// families, so a symmetric type now reaches one character further than it did,
/// ten thousand characters out.
fn scan_right_for_closes(
    rope: &Rope,
    cursor: usize,
    at_cursor: Option<char>,
    settled: &[bool; SURROUND_PAIRS.len()],
    tree: Option<&stoat_language::Tree>,
) -> [Option<usize>; SURROUND_PAIRS.len()] {
    let mut found: [Option<usize>; SURROUND_PAIRS.len()] = Default::default();
    let mut step_over = [0usize; SURROUND_PAIRS.len()];
    let mut done = *settled;

    for (i, (open, close)) in SURROUND_PAIRS.into_iter().enumerate() {
        if !done[i] && open != close && at_cursor == Some(close) && !in_skip_zone(tree, cursor) {
            found[i] = Some(cursor);
            done[i] = true;
        }
    }

    let mut pos = cursor + at_cursor.map_or(0, char::len_utf8);
    for c in rope.chars_at(pos).take(MAX_PAIR_SCAN) {
        if let Some(i) = pair_index(c)
            && !done[i]
            && !in_skip_zone(tree, pos)
        {
            let (open, close) = SURROUND_PAIRS[i];
            if open == close || c == close && step_over[i] == 0 {
                found[i] = Some(pos);
                done[i] = true;
            } else if c == open {
                step_over[i] += 1;
            } else {
                step_over[i] -= 1;
            }
        }
        pos += c.len_utf8();
    }
    found
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
            if let Some(pair) = enclosing_string_pair(rope, tree, cursor, open) {
                return Some(pair);
            }
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

/// Walk the tree-sitter ancestor chain at `offset` looking for the
/// deepest node whose `kind()` mentions `"string"`. Returns the
/// node's byte range (half-open: `start..end_byte`). Used to
/// disambiguate cursor-on-quote surround lookups; the calling site
/// translates `range.end - 1` into the closing quote's byte offset.
fn find_enclosing_string_node(tree: &stoat_language::Tree, offset: usize) -> Option<Range<usize>> {
    let mut node = tree.root_node().descendant_for_byte_range(offset, offset)?;
    loop {
        if node.kind().contains("string") {
            return Some(node.byte_range());
        }
        match node.parent() {
            Some(p) => node = p,
            None => return None,
        }
    }
}

/// Translate `find_enclosing_string_node` into a surround pair when
/// the located string node opens with `open`. Returns `None` when
/// the buffer has no tree, no string ancestor exists at the cursor,
/// or the located node does not start with `open` (e.g. a rust raw
/// string `r"..."` whose first byte is `r`).
fn enclosing_string_pair(
    rope: &Rope,
    tree: Option<&stoat_language::Tree>,
    cursor: usize,
    open: char,
) -> Option<(usize, usize)> {
    let tree = tree?;
    let range = find_enclosing_string_node(tree, cursor)?;
    if range.start >= range.end {
        return None;
    }
    if rope.chars_at(range.start).next() != Some(open) {
        return None;
    }
    Some((range.start, range.end - open.len_utf8()))
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
    // The tree is asked only where the answer is used. It is a descent from the
    // root plus an ancestor walk, and every character that is not a delimiter
    // discarded it.
    for c in chars.take(MAX_PAIR_SCAN) {
        if c == open && !in_skip_zone(tree, pos) {
            step_over += 1;
        } else if c == close && !in_skip_zone(tree, pos) {
            if step_over == 0 {
                return Some(pos);
            }
            step_over -= 1;
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
    for c in rope.reversed_chars_at(cursor).take(MAX_PAIR_SCAN) {
        pos = pos.checked_sub(c.len_utf8())?;
        if c == close && !in_skip_zone(tree, pos) {
            step_over += 1;
        } else if c == open && !in_skip_zone(tree, pos) {
            if step_over == 0 {
                return Some(pos);
            }
            step_over -= 1;
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
    for c in rope.chars_at(cursor).take(MAX_PAIR_SCAN) {
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
    for c in rope.reversed_chars_at(cursor).take(MAX_PAIR_SCAN) {
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
        let sel = editor.selections.newest_anchor();
        stoat_text::cursor_offset(
            buf_snap.rope(),
            buf_snap.resolve_anchor(&sel.tail()),
            buf_snap.resolve_anchor(&sel.head()),
        )
    }

    /// `(` then `filler` then `)`, with the cursor just inside each delimiter,
    /// so each walk has the whole filler to cross before reaching its partner.
    fn spaced_pair(filler: usize) -> (Rope, usize) {
        let text = format!("({})", "x".repeat(filler));
        let len = text.len();
        (Rope::from(text.as_str()), len)
    }

    #[test]
    fn a_pair_further_apart_than_the_cap_is_not_found() {
        let (rope, len) = spaced_pair(MAX_PAIR_SCAN + 100);

        assert_eq!(
            walk_right_for_close(&rope, 1, '(', ')', None),
            None,
            "the close is past where the walk gives up"
        );
        assert_eq!(
            walk_left_for_open(&rope, len - 1, '(', ')', None),
            None,
            "and so is the open, walking the other way"
        );
    }

    #[test]
    fn a_pair_within_the_cap_is_still_found() {
        let filler = MAX_PAIR_SCAN - 100;
        let (rope, len) = spaced_pair(filler);

        assert_eq!(
            walk_right_for_close(&rope, 1, '(', ')', None),
            Some(1 + filler),
            "a close inside the cap is where it always was"
        );
        assert_eq!(
            walk_left_for_open(&rope, len - 1, '(', ')', None),
            Some(0),
            "and so is the open"
        );
    }

    /// The deepest of the per-type answers, which is what the closest pair used
    /// to be found by. Kept as the oracle for the merged walk.
    fn closest_by_type(
        rope: &Rope,
        cursor: usize,
        tree: Option<&stoat_language::Tree>,
    ) -> Option<(char, char, usize, usize)> {
        SURROUND_PAIRS
            .into_iter()
            .filter_map(|(open, close)| {
                find_surround_pair(rope, cursor, open, close, tree)
                    .map(|(open_off, close_off)| (open, close, open_off, close_off))
            })
            .max_by_key(|&(_, _, open_off, _)| open_off)
    }

    /// Text built from every delimiter set, nested and interleaved, so the
    /// per-type states have to be kept apart rather than sharing a counter.
    fn nested_fixture(seed: u64) -> String {
        const PIECES: [&str; 14] = [
            "(", ")", "[", "]", "{", "}", "<", ">", "\"", "'", "`", "ab ", "\n", "; ",
        ];
        let mut rng = seed;
        let mut text = String::new();
        for _ in 0..120 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            text.push_str(PIECES[((rng >> 33) % PIECES.len() as u64) as usize]);
        }
        text
    }

    #[test]
    fn one_walk_finds_what_asking_every_type_found() {
        for seed in 0..40u64 {
            let text = nested_fixture(seed);
            let rope = Rope::from(text.as_str());

            for cursor in 0..=rope.len() {
                if !text.is_char_boundary(cursor) {
                    continue;
                }
                assert_eq!(
                    closest_surround_pair(&rope, cursor, None),
                    closest_by_type(&rope, cursor, None),
                    "seed {seed}, cursor {cursor}, in {text:?}"
                );
            }
        }
    }

    /// The same comparison over a buffer with a real syntax tree, so the skip
    /// zones are live. Both walks consult them, and the property test above
    /// passes no tree, which would leave that half unchecked.
    #[test]
    fn one_walk_agrees_where_skip_zones_are_live() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "let a = (\"str (with) [brackets]\"); // {comment} 'x'\n\
                   let b = [c(d), e{f}, \"g'h\"];\n\
                   let c = `raw (x)` + 'y';\n";
        let path = seed_rs(&mut h, src);

        let ws = h.stoat.active_workspace();
        let buffer_id = ws.buffers.id_for_path(&path).expect("buffer is open");
        let rope = ws
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .read()
            .expect("poisoned")
            .rope()
            .clone();
        let snapshot = ws.buffers.syntax_map(buffer_id).map(|m| m.snapshot());
        assert!(snapshot.is_some(), "the fixture has to have parsed");

        for cursor in 0..=rope.len() {
            let tree = deepest_tree_at(snapshot, cursor);
            assert!(tree.is_some(), "a covering layer at {cursor}");
            assert_eq!(
                closest_surround_pair(&rope, cursor, tree),
                closest_by_type(&rope, cursor, tree),
                "cursor {cursor}"
            );
        }
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
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("(");
        assert_eq!(buffer_text(&h, &path), "(abc)\n");
        assert!(!h.stoat.pending_surround_add);
    }

    #[test]
    fn surround_add_close_char_wraps_with_canonical_pair() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys(">");
        assert_eq!(buffer_text(&h, &path), "<abc>\n");
    }

    #[test]
    fn surround_add_quote_wraps_with_same_char() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("\"");
        assert_eq!(buffer_text(&h, &path), "\"abc\"\n");
    }

    #[test]
    fn surround_add_arbitrary_char_doubles() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("*");
        assert_eq!(buffer_text(&h, &path), "*abc*\n");
    }

    #[test]
    fn surround_add_bare_cursor_wraps_char() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("(");
        assert_eq!(buffer_text(&h, &path), "(a)bc\n");
        assert_eq!(cursor_offset(&mut h), 1);
        assert!(!h.stoat.pending_surround_add);
    }

    #[test]
    fn surround_add_pending_clears_on_non_char() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
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
        h.type_keys("v l l");
        h.type_keys("escape");
        h.type_keys("m s [");
        assert_eq!(buffer_text(&h, &path), "[abc]\n");
        assert_eq!(h.stoat.focused_mode(), "normal");
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
        assert_eq!(h.stoat.focused_mode(), "normal");
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

    /// Cursors on the `a` and the `b`, so the two enclosing pairs nest.
    fn nested_pairs_with_a_cursor_in_each(h: &mut TestHarness) {
        h.type_keys("%");
        h.type_keys("s");
        h.type_text("[ab]");
        h.type_keys("Enter");
        assert_eq!(
            h.selection_spans(),
            vec![(1, 2, false), (3, 4, false)],
            "fixture needs one cursor inside each pair"
        );
    }

    #[test]
    fn surround_delete_nested_pairs_removes_both() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(a(b)c)\n");
        nested_pairs_with_a_cursor_in_each(&mut h);

        h.type_keys("m d (");
        assert_eq!(
            buffer_text(&h, &path),
            "abc\n",
            "the outer close was edited at an offset the inner delete had moved"
        );
    }

    #[test]
    fn surround_replace_nested_pairs_replaces_both() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(a(b)c)\n");
        nested_pairs_with_a_cursor_in_each(&mut h);

        h.type_keys("m r ( [");
        assert_eq!(buffer_text(&h, &path), "[a[b]c]\n");
    }

    #[test]
    fn surround_replace_nested_pairs_with_a_wider_delimiter() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(a(b)c)\n");
        nested_pairs_with_a_cursor_in_each(&mut h);

        // Two bytes replacing one, so every offset after the first edit moves.
        h.type_keys("m r ( \u{ab}");
        assert_eq!(buffer_text(&h, &path), "\u{ab}a\u{ab}b\u{ab}c\u{ab}\n");
    }

    #[test]
    fn surround_delete_pairs_sharing_a_quote_removes_it_once() {
        let mut h = TestHarness::with_size(40, 10);
        // The middle quote closes the first pair and opens the second, so it
        // reaches the edits twice.
        let path = seed(&mut h, "\"a\"b\"\n");
        nested_pairs_with_a_cursor_in_each(&mut h);

        h.type_keys("m d \"");
        assert_eq!(buffer_text(&h, &path), "ab\n");
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
        h.stoat.drive_background();
        let _ = h.stoat.render();
        h.settle();
        h.stoat.drive_background();
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

    #[test]
    fn surround_delete_quote_on_open_with_tree() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "let s = \"abc\";\n";
        let path = seed_rs(&mut h, src);
        let cursor = src.find('"').expect("opening quote present");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);
        h.type_keys("m d \"");
        assert_eq!(buffer_text(&h, &path), "let s = abc;\n");
    }

    #[test]
    fn surround_delete_quote_on_close_with_tree() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "let s = \"abc\";\n";
        let path = seed_rs(&mut h, src);
        let cursor = src.rfind('"').expect("closing quote present");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);
        h.type_keys("m d \"");
        assert_eq!(buffer_text(&h, &path), "let s = abc;\n");
    }

    #[test]
    fn surround_replace_quote_on_quote_with_tree() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "let s = \"abc\";\n";
        let path = seed_rs(&mut h, src);
        let cursor = src.find('"').expect("opening quote present");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);
        h.type_keys("m r \" '");
        assert_eq!(buffer_text(&h, &path), "let s = 'abc';\n");
    }

    #[test]
    fn surround_pair_on_quote_no_tree_returns_none() {
        let r = rope("\"abc\"");
        assert_eq!(find_surround_pair(&r, 0, '"', '"', None), None);
        assert_eq!(find_surround_pair(&r, 4, '"', '"', None), None);
    }

    fn primary_range(h: &mut TestHarness) -> (usize, usize) {
        let editor = focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let start = buf_snap.resolve_anchor(&sel.start);
        let end = buf_snap.resolve_anchor(&sel.end);
        (start, end)
    }

    #[test]
    fn cursor_offset_after_surround_add_collapsed_into_selection() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("(");
        assert_eq!(buffer_text(&h, &path), "(abc)\n");
        assert_eq!(primary_range(&mut h), (1, 4));
    }

    #[test]
    fn cursor_offset_after_surround_replace_in_paren() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(abc)\n");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 2);
        h.type_keys("m r ( [");
        assert_eq!(buffer_text(&h, &path), "[abc]\n");
        assert_eq!(cursor_offset(&mut h), 2);
    }

    #[test]
    fn cursor_offset_after_surround_delete_paren() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(abc)\n");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 2);
        h.type_keys("m d (");
        assert_eq!(buffer_text(&h, &path), "abc\n");
        assert_eq!(cursor_offset(&mut h), 1);
    }

    #[test]
    fn cursor_offset_after_surround_replace_quote() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "\"abc\"\n");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 2);
        h.type_keys("m r \" '");
        assert_eq!(buffer_text(&h, &path), "'abc'\n");
        assert_eq!(cursor_offset(&mut h), 2);
    }

    #[test]
    fn surround_delete_closest_removes_pair() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(abc)\n");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 2);
        h.type_keys("m d m");
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn surround_replace_closest_rewrites_innermost() {
        let mut h = TestHarness::with_size(40, 10);
        let src = "[(x)]\n";
        let path = seed(&mut h, src);
        let cursor = src.find('x').expect("target present");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);
        h.type_keys("m r m ]");
        assert_eq!(buffer_text(&h, &path), "[[x]]\n");
    }
}
