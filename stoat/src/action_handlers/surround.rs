use crate::{
    action_handlers::movement::{window_around, PairScan, BRACKET_PAIRS},
    app::{Stoat, UpdateEffect},
    buffer::TextBuffer,
    pane::View,
};
use std::{cmp::Reverse, ops::Range};
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
    stoat.pending_surround_count = super::arming_count(stoat);
    stoat.pending_surround_replace = SurroundReplaceStage::AwaitFrom;
    UpdateEffect::Redraw
}

pub(super) fn surround_delete(stoat: &mut Stoat) -> UpdateEffect {
    stoat.pending_surround_count = super::arming_count(stoat);
    stoat.pending_surround_delete = true;
    UpdateEffect::Redraw
}

/// Wrap every non-empty selection in the focused editor with the pair
/// [`surround_pair_for`] names, skipping collapsed ones.
pub(crate) fn execute_surround_add(stoat: &mut Stoat, ch: char) -> UpdateEffect {
    let (open, close) = surround_pair_for(ch);
    execute_surround_add_pair(stoat, &open.to_string(), &close.to_string())
}

/// Wrap every non-empty selection with `open` and `close`, skipping collapsed
/// ones.
///
/// Takes text rather than the pair's two chars, because a line ending is a
/// string and CRLF is two of them.
///
/// Each affected selection comes out covering the whole pair, opener through
/// closer, with the direction it had. The pair is what the operation just made,
/// so it is what a follow-up edit or a second wrap acts on. Select mode ends
/// here rather than at a binding. The char completing the chord is consumed
/// before any keymap lookup runs, so no binding sees it.
pub(crate) fn execute_surround_add_pair(
    stoat: &mut Stoat,
    open: &str,
    close: &str,
) -> UpdateEffect {
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

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        // The close sits past the open within a selection and the selections
        // are visited back to front, so flattening the pair per selection
        // leaves the whole sequence descending.
        let batch: Vec<(Range<usize>, &str)> = entries
            .iter()
            .rev()
            .flat_map(|(_, s, e, _)| [(*e..*e, close), (*s..*s, open)])
            .collect();
        guard.edit_batch(&batch);
    }

    let open_len = open.len();
    let close_len = close.len();
    let mut id_to_range: std::collections::HashMap<usize, (usize, usize, bool)> =
        std::collections::HashMap::with_capacity(entries.len());
    let mut shift: i64 = 0;
    for (id, s, e, reversed) in entries.iter() {
        let new_start = (*s as i64 + shift) as usize;
        let new_end = (*e as i64 + shift) as usize + open_len + close_len;
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
    stoat.set_focused_mode("normal".to_string());
    UpdateEffect::Redraw
}

/// The pair `ch` names, whichever of its two ends the user typed.
///
/// A character that opens or closes a bracket names that bracket. Anything else
/// names itself at both ends, which is what makes the quotes work and lets a
/// user wrap a selection in any character at all.
pub(crate) fn surround_pair_for(ch: char) -> (char, char) {
    BRACKET_PAIRS
        .into_iter()
        .find(|&(open, close)| ch == open || ch == close)
        .unwrap_or((ch, ch))
}

/// Apply the consumed-char keypress to the pending surround_delete
/// chord. For every selection's primary cursor, find the nearest
/// enclosing surround pair and remove its open / close. `ch` names the
/// pair type, except `m`, which means the nearest pair of any type (so a
/// literal `m...m` pair is unreachable). A count typed in front of the
/// chord reaches that far out, so `2 m d (` takes the parens around the
/// nearest ones. Nothing is edited unless every cursor names a pair of
/// its own, per [`collect_surround_pairs`].
pub(crate) fn execute_surround_delete(stoat: &mut Stoat, ch: char) -> UpdateEffect {
    let pair = (ch != 'm').then(|| surround_pair_for(ch));
    let skip = stoat.pending_surround_count.saturating_sub(1);
    let pairs = match collect_surround_pairs(stoat, pair, skip) {
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
/// pair char. A count typed in front of the chord reaches that far out,
/// so `2 m r ( [` rewrites the parens around the nearest ones. Nothing is
/// edited unless every cursor names a pair of its own, per
/// [`collect_surround_pairs`].
pub(crate) fn execute_surround_replace(stoat: &mut Stoat, from: char, to: char) -> UpdateEffect {
    let from_pair = (from != 'm').then(|| surround_pair_for(from));
    let (new_open, new_close) = surround_pair_for(to);
    let skip = stoat.pending_surround_count.saturating_sub(1);
    let pairs = match collect_surround_pairs(stoat, from_pair, skip) {
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
/// pairs sorted ascending, or `None` when the focused pane is not an
/// editor.
///
/// `skip` reaches past that many pairs around every cursor, so a count of
/// two on the chord passes one.
///
/// Two cases return `None` with a status set, leaving the caller nothing
/// to edit. A cursor with no pair around it aborts the whole operation,
/// and so does a pair two cursors both name.
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
    skip: usize,
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
        let cursors: Vec<(usize, Range<usize>)> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
                let head_off = buffer_snapshot.resolve_anchor(&sel.head());
                let cursor = stoat_text::cursor_offset(buffer_snapshot.rope(), tail_off, head_off);
                let span = buffer_snapshot.resolve_anchor(&sel.start)
                    ..buffer_snapshot.resolve_anchor(&sel.end);
                (cursor, span)
            })
            .collect();
        (buffer_id, cursors)
    };

    let ws = stoat.active_workspace();
    let buffer = ws.buffers.get(buffer_id)?;
    let rope = buffer.read().expect("poisoned").rope().clone();
    let snapshot = ws.buffers.syntax_map(buffer_id).map(|m| m.snapshot());

    // One window covering every cursor's reach, so cursors in the same layer
    // collect their zones once between them rather than once each. A window
    // wider than any single cursor needs only adds zones nothing asks about.
    // Only the closest-pair walk reads zones, so only it pays for them.
    let window = {
        let first = cursors.iter().map(|(head, _)| *head).min().unwrap_or(0);
        let last = cursors.iter().map(|(head, _)| *head).max().unwrap_or(0);
        window_around(first).start..window_around(last).end
    };

    // Cursors can sit in different layers, so the zones are keyed on the tree
    // they came from rather than shared outright.
    let mut scans: Vec<(*const stoat_language::Tree, PairScan<'_>)> = Vec::new();

    let mut pairs: Vec<(usize, usize, char, char)> = Vec::with_capacity(cursors.len());
    let mut claimed: Vec<usize> = Vec::with_capacity(cursors.len() * 2);
    for (head, span) in cursors {
        let tree = deepest_tree_at(snapshot, head);

        let found = match pair {
            Some((open, close)) => {
                let scan = PairScan::plaintext(tree);
                find_surround_pair(&rope, head, open, close, &scan, skip)
                    .map(|(open_off, close_off)| (open_off, close_off, open, close))
            },
            None => {
                let key = tree.map_or(std::ptr::null(), |t| t as *const _);
                let idx = match scans.iter().position(|(seen, _)| *seen == key) {
                    Some(idx) => idx,
                    None => {
                        scans.push((key, PairScan::over(tree, window.clone())));
                        scans.len() - 1
                    },
                };
                closest_surround_pair(&rope, head, &scans[idx].1, skip, span)
                    .map(|(open, close, open_off, close_off)| (open_off, close_off, open, close))
            },
        };
        // Both misses abort the whole operation rather than dropping the
        // cursor. A partial edit across cursors is the one outcome the user has
        // no way to undo by eye, since which cursors landed is invisible once
        // the text has changed.
        let Some(found) = found else {
            stoat.set_status("no surround pair around one of the cursors");
            return None;
        };
        if claimed.contains(&found.0) || claimed.contains(&found.1) {
            stoat.set_status("two cursors share a surround pair");
            return None;
        }
        claimed.extend([found.0, found.1]);
        pairs.push(found);
    }

    pairs.sort_unstable();
    Some(pairs)
}

/// The nearest enclosing `(open, close)` pair around `cursor` in the
/// buffer's live rope, resolved against the deepest syntax layer
/// covering the cursor so brackets inside string / comment nodes are
/// skipped. `None` when no enclosing pair exists. Shared by the
/// surround delete/replace collection and the `m i`/`m a` pair-char
/// textobjects.
///
/// `skip` reaches past that many enclosing pairs, so `1` gives the pair
/// around the nearest one.
pub(crate) fn surround_pair_at(
    ws: &crate::workspace::Workspace,
    buffer_id: crate::buffer::BufferId,
    cursor: usize,
    open: char,
    close: char,
    skip: usize,
) -> Option<(usize, usize)> {
    let buffer = ws.buffers.get(buffer_id)?;
    let rope = buffer.read().expect("poisoned").rope().clone();
    let snapshot = ws.buffers.syntax_map(buffer_id).map(|m| m.snapshot());
    let tree = deepest_tree_at(snapshot, cursor);
    find_surround_pair(&rope, cursor, open, close, &PairScan::plaintext(tree), skip)
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
/// The brackets come from [`BRACKET_PAIRS`], which a bracket match walks too, so
/// the two stay one list. Four more pairs sit on top of them. Three are the
/// quotes, whose two ends are the same character. The fourth is the bars a rust
/// closure takes its parameters between.
///
/// Their delimiter characters are disjoint, so a character is a delimiter for
/// at most one entry here. That is what lets one walk serve all of them.
///
/// The bars are a known wrong answer waiting to happen. Nothing here tells a
/// closure's bars from a bitwise or, so `a | b | c` reads as a pair around `b`.
/// The syntax tree does know the difference, and consulting it is what the
/// entry needs to be right.
const SURROUND_PAIRS: [(char, char); BRACKET_PAIRS.len() + 4] = {
    let mut pairs = [(' ', ' '); BRACKET_PAIRS.len() + 4];
    let mut i = 0;
    while i < BRACKET_PAIRS.len() {
        pairs[i] = BRACKET_PAIRS[i];
        i += 1;
    }
    pairs[i] = ('"', '"');
    pairs[i + 1] = ('\'', '\'');
    pairs[i + 2] = ('`', '`');
    pairs[i + 3] = ('|', '|');
    pairs
};

/// The innermost enclosing pair of any surround type around `cursor`,
/// as `(open, close, open_off, close_off)`. Runs [`find_surround_pair`]
/// for each bracket and quote pair and keeps the one with the greatest
/// `open_off` (deepest enclosing). `None` when no pair type encloses
/// the cursor. Drives the `m i m` / `m a m` closest-pair textobject.
///
/// `skip` reaches past that many enclosing pairs, and it counts them
/// across types. A cursor in `([here])` resolves to the parens with `1`.
///
/// `span` is what the selection covers now. The answer has to cover it and be
/// more than it, so a selection already running from an opener through its
/// closer reaches the pair outside instead of naming its own again. That is
/// what makes a second `m a m` grow. A selection between the delimiters is not
/// the pair itself, so a second `m i m` keeps it.
pub(crate) fn closest_surround_pair(
    rope: &Rope,
    cursor: usize,
    scan: &PairScan<'_>,
    skip: usize,
    span: Range<usize>,
) -> Option<(char, char, usize, usize)> {
    let at_cursor = rope.chars_at(cursor).next();

    // A quote under the cursor cannot say which of its sides opens, so its type
    // is answered by the syntax tree rather than by walking. Deciding that here
    // keeps those types out of both walks entirely.
    let mut string_pair: [Option<(usize, usize)>; SURROUND_PAIRS.len()] = Default::default();
    let mut settled = [false; SURROUND_PAIRS.len()];
    for (i, (open, close)) in SURROUND_PAIRS.into_iter().enumerate() {
        if open == close && at_cursor == Some(open) {
            settled[i] = true;
            string_pair[i] = enclosing_delimited_pair(rope, scan.tree, cursor, open);
        }
    }

    // A pair with n others inside it has at most n of its own type inside it,
    // so the nth pair overall is within the first n of the type it belongs to.
    // Running every type that far out therefore collects every candidate in
    // contention, and ordering them by opener says which one wins.
    //
    // One level further than that, because the span filter below drops the pair
    // the selection already holds and there is only ever one of those. The extra
    // level only adds candidates that sort after the ones already collected, so
    // it changes nothing where the filter drops none.
    let mut candidates: Vec<(char, char, usize, usize)> = Vec::with_capacity(SURROUND_PAIRS.len());
    for reach in 0..=skip + 1 {
        let opens = scan_left_for_opens(rope, cursor, at_cursor, &settled, scan, reach);
        let closes = scan_right_for_closes(rope, cursor, at_cursor, &settled, scan, reach);
        for (i, (open, close)) in SURROUND_PAIRS.into_iter().enumerate() {
            // A string node has no second occurrence of its type outside it, so
            // it stands only as the innermost candidate.
            let resolved = match settled[i] {
                true if reach == 0 => string_pair[i],
                true => None,
                false => opens[i].zip(closes[i]),
            };
            if let Some((open_off, close_off)) = resolved {
                candidates.push((open, close, open_off, close_off));
            }
        }
    }

    candidates.sort_unstable_by_key(|&(_, _, open_off, _)| Reverse(open_off));
    candidates
        .into_iter()
        .filter(|&(_, close, open_off, close_off)| {
            let around_end = close_off + close.len_utf8();
            let covers = open_off <= span.start && span.end <= around_end;
            let already = open_off == span.start && span.end == around_end;
            covers && !already
        })
        .nth(skip)
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
///
/// Every type applies `skip` to itself alone. It passes over that many of its
/// own opens before it takes an answer.
fn scan_left_for_opens(
    rope: &Rope,
    cursor: usize,
    at_cursor: Option<char>,
    settled: &[bool; SURROUND_PAIRS.len()],
    scan: &PairScan<'_>,
    skip: usize,
) -> [Option<usize>; SURROUND_PAIRS.len()] {
    let mut found: [Option<usize>; SURROUND_PAIRS.len()] = Default::default();
    let mut step_over = [0usize; SURROUND_PAIRS.len()];
    let mut remaining = [skip; SURROUND_PAIRS.len()];
    let mut done = *settled;

    // An open under the cursor is that type's open, without walking anywhere.
    for (i, (open, close)) in SURROUND_PAIRS.into_iter().enumerate() {
        if !done[i] && open != close && at_cursor == Some(open) && !scan.skips(cursor) {
            take_or_skip(cursor, i, &mut found, &mut done, &mut remaining);
        }
    }

    let mut pos = cursor;
    for c in rope.reversed_chars_at(cursor).take(scan.reach) {
        let Some(next) = pos.checked_sub(c.len_utf8()) else {
            break;
        };
        pos = next;

        let Some(i) = pair_index(c) else { continue };
        if done[i] || scan.skips(pos) {
            continue;
        }

        let (open, close) = SURROUND_PAIRS[i];
        if open == close || c == open && step_over[i] == 0 {
            take_or_skip(pos, i, &mut found, &mut done, &mut remaining);
        } else if c == close {
            step_over[i] += 1;
        } else {
            step_over[i] -= 1;
        }
    }
    found
}

/// Answer pair type `i` with the delimiter at `pos`, or spend one of the
/// type's remaining skips on it.
fn take_or_skip(
    pos: usize,
    i: usize,
    found: &mut [Option<usize>; SURROUND_PAIRS.len()],
    done: &mut [bool; SURROUND_PAIRS.len()],
    remaining: &mut [usize; SURROUND_PAIRS.len()],
) {
    if remaining[i] == 0 {
        found[i] = Some(pos);
        done[i] = true;
    } else {
        remaining[i] -= 1;
    }
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
    scan: &PairScan<'_>,
    skip: usize,
) -> [Option<usize>; SURROUND_PAIRS.len()] {
    let mut found: [Option<usize>; SURROUND_PAIRS.len()] = Default::default();
    let mut step_over = [0usize; SURROUND_PAIRS.len()];
    let mut remaining = [skip; SURROUND_PAIRS.len()];
    let mut done = *settled;

    for (i, (open, close)) in SURROUND_PAIRS.into_iter().enumerate() {
        if !done[i] && open != close && at_cursor == Some(close) && !scan.skips(cursor) {
            take_or_skip(cursor, i, &mut found, &mut done, &mut remaining);
        }
    }

    let mut pos = cursor + at_cursor.map_or(0, char::len_utf8);
    for c in rope.chars_at(pos).take(scan.reach) {
        if let Some(i) = pair_index(c)
            && !done[i]
            && !scan.skips(pos)
        {
            let (open, close) = SURROUND_PAIRS[i];
            if open == close || c == close && step_over[i] == 0 {
                take_or_skip(pos, i, &mut found, &mut done, &mut remaining);
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

/// The nearest enclosing `(open, close)` pair around `cursor`, as the byte
/// offset of each delimiter, or `None` when none encloses it.
///
/// Asymmetric pairs use depth tracking so nested pairs do not confuse the
/// search. Symmetric pairs (`open == close`) take the nearest occurrence in
/// each direction. A cursor sitting exactly on a symmetric char has no answer
/// in the text, since neither side says which one opens, so it is read off the
/// string node the tree puts there and gives up when there is none.
///
/// `scan` decides how far the walk reaches and which delimiters it steps over.
/// The chords that name a pair type hand it
/// [`PairScan::plaintext`](crate::action_handlers::movement::PairScan::plaintext),
/// which is what makes `m d (` reach a paren inside a string literal, and what
/// Helix's own `find_nth_open_pair` does.
///
/// `skip` reaches past that many enclosing pairs, so `1` gives the
/// pair around the nearest one. A quote resolved from a string node
/// has nothing outside it of its own type, so any `skip` above zero
/// finds nothing there.
///
/// A cursor with no character boundary after it finds nothing, whatever
/// encloses it. That refuses a real pair whose closer is the buffer's last
/// character, and it is what Helix refuses too.
pub(crate) fn find_surround_pair(
    rope: &Rope,
    cursor: usize,
    open: char,
    close: char,
    scan: &PairScan<'_>,
    skip: usize,
) -> Option<(usize, usize)> {
    // A selection reaching the end of the text is refused before anything is
    // looked for, so a pair closing the buffer's last character is out of reach.
    // The cursor stands in for the selection's end, which is one boundary past
    // it for a cursor and for a forward selection alike.
    let after_cursor = cursor + rope.chars_at(cursor).next().map_or(0, char::len_utf8);
    if after_cursor >= rope.len() {
        return None;
    }

    if open == close {
        if rope.chars_at(cursor).next() == Some(open) {
            if skip > 0 {
                return None;
            }
            return enclosing_delimited_pair(rope, scan.tree, cursor, open);
        }
        let open_pos = walk_left_for_symmetric(rope, cursor, open, scan, skip)?;
        let close_pos = walk_right_for_symmetric(rope, cursor, open, scan, skip)?;
        Some((open_pos, close_pos))
    } else {
        let open_pos = walk_left_for_open(rope, cursor, open, close, scan, skip)?;
        let close_pos = walk_right_for_close(rope, cursor, open, close, scan, skip)?;
        Some((open_pos, close_pos))
    }
}

/// Offsets of the two `open` delimiters around `cursor`, or `None` where the
/// buffer has no tree and where no node around the cursor carries that pair.
///
/// A character that both opens and closes leaves a cursor on it with no side
/// to search from, so the tree answers instead. Any node the character closes
/// is a node it delimits, whatever the grammar calls it: a string, a char
/// literal, a prefixed literal whose own first byte is the prefix, or a markup
/// attribute's quoted value.
///
/// Answers the deepest such node, so a literal nested in another resolves to
/// its own delimiters.
fn enclosing_delimited_pair(
    rope: &Rope,
    tree: Option<&stoat_language::Tree>,
    cursor: usize,
    open: char,
) -> Option<(usize, usize)> {
    let mut node = tree?
        .root_node()
        .descendant_for_byte_range(cursor, cursor)?;
    loop {
        if let Some(pair) = delimiter_pair(rope, node.byte_range(), open) {
            return Some(pair);
        }
        node = node.parent()?;
    }
}

/// Offsets of the `open` closing `range` and of the first one inside it, where
/// `range` carries both.
///
/// A node ending in the delimiter is a node the delimiter closes, and the pair
/// opens at the first one inside it. That admits a prefix, so `r"..."` and
/// `b"..."` resolve to their quotes rather than to the letter they start with.
///
/// The end test is what keeps the climb honest. A node merely containing two of
/// the character, which most of the file does, closes with something else.
fn delimiter_pair(rope: &Rope, range: Range<usize>, open: char) -> Option<(usize, usize)> {
    if rope.reversed_chars_at(range.end).next() != Some(open) {
        return None;
    }
    let last = range.end.checked_sub(open.len_utf8())?;

    // The caller reaches here only with the cursor on one of the pair, so the
    // scan meets an `open` at or before it and needs no bound of its own.
    let mut pos = range.start;
    for ch in rope.chars_at(range.start) {
        if pos >= last {
            return None;
        }
        if ch == open {
            return Some((pos, last));
        }
        pos += ch.len_utf8();
    }
    None
}

fn walk_right_for_close(
    rope: &Rope,
    cursor: usize,
    open: char,
    close: char,
    scan: &PairScan<'_>,
    mut skip: usize,
) -> Option<usize> {
    let mut chars = rope.chars_at(cursor);
    let mut pos = cursor;
    let first = chars.next()?;
    if first == close && !scan.skips(pos) {
        if skip == 0 {
            return Some(pos);
        }
        skip -= 1;
    }
    pos += first.len_utf8();
    let mut step_over: usize = 0;
    // The tree is asked only where the answer is used. It is a descent from the
    // root plus an ancestor walk, and every character that is not a delimiter
    // discarded it.
    for c in chars.take(scan.reach) {
        if c == open && !scan.skips(pos) {
            step_over += 1;
        } else if c == close && !scan.skips(pos) {
            if step_over > 0 {
                step_over -= 1;
            } else if skip == 0 {
                return Some(pos);
            } else {
                skip -= 1;
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
    scan: &PairScan<'_>,
    mut skip: usize,
) -> Option<usize> {
    if rope.chars_at(cursor).next() == Some(open) && !scan.skips(cursor) {
        if skip == 0 {
            return Some(cursor);
        }
        skip -= 1;
    }
    let mut pos = cursor;
    let mut step_over: usize = 0;
    for c in rope.reversed_chars_at(cursor).take(scan.reach) {
        pos = pos.checked_sub(c.len_utf8())?;
        if c == close && !scan.skips(pos) {
            step_over += 1;
        } else if c == open && !scan.skips(pos) {
            if step_over > 0 {
                step_over -= 1;
            } else if skip == 0 {
                return Some(pos);
            } else {
                skip -= 1;
            }
        }
    }
    None
}

fn walk_right_for_symmetric(
    rope: &Rope,
    cursor: usize,
    ch: char,
    scan: &PairScan<'_>,
    mut skip: usize,
) -> Option<usize> {
    let mut pos = cursor;
    for c in rope.chars_at(cursor).take(scan.reach) {
        if c == ch && !scan.skips(pos) {
            if skip == 0 {
                return Some(pos);
            }
            skip -= 1;
        }
        pos += c.len_utf8();
    }
    None
}

fn walk_left_for_symmetric(
    rope: &Rope,
    cursor: usize,
    ch: char,
    scan: &PairScan<'_>,
    mut skip: usize,
) -> Option<usize> {
    let mut pos = cursor;
    for c in rope.reversed_chars_at(cursor).take(scan.reach) {
        pos = pos.checked_sub(c.len_utf8())?;
        if c == ch && !scan.skips(pos) {
            if skip == 0 {
                return Some(pos);
            }
            skip -= 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        action_handlers::{focused_editor_mut, movement::MAX_PAIR_SCAN},
        test_harness::TestHarness,
    };
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
            walk_right_for_close(&rope, 1, '(', ')', &PairScan::around(None, 0), 0),
            None,
            "the close is past where the walk gives up"
        );
        assert_eq!(
            walk_left_for_open(&rope, len - 1, '(', ')', &PairScan::around(None, 0), 0),
            None,
            "and so is the open, walking the other way"
        );
    }

    /// The cap belongs to the windowed scan, whose zones are only collected for
    /// the window it stops inside. A plaintext walk has no window to stay in.
    #[test]
    fn a_pair_further_apart_than_the_cap_is_found_by_a_plaintext_walk() {
        let (rope, len) = spaced_pair(MAX_PAIR_SCAN + 100);

        assert_eq!(
            walk_right_for_close(&rope, 1, '(', ')', &PairScan::plaintext(None), 0),
            Some(len - 1),
        );
        assert_eq!(
            walk_left_for_open(&rope, len - 1, '(', ')', &PairScan::plaintext(None), 0),
            Some(0),
        );
    }

    #[test]
    fn a_pair_within_the_cap_is_still_found() {
        let filler = MAX_PAIR_SCAN - 100;
        let (rope, len) = spaced_pair(filler);

        assert_eq!(
            walk_right_for_close(&rope, 1, '(', ')', &PairScan::around(None, 0), 0),
            Some(1 + filler),
            "a close inside the cap is where it always was"
        );
        assert_eq!(
            walk_left_for_open(&rope, len - 1, '(', ')', &PairScan::around(None, 0), 0),
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
                find_surround_pair(
                    rope,
                    cursor,
                    open,
                    close,
                    &PairScan::around(tree, cursor),
                    0,
                )
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

            // The per-type walk refuses a cursor with nothing after it and the
            // closest walk does not, which is the split Helix has, so the two
            // only answer alike short of the last character.
            for cursor in 0..rope.len().saturating_sub(1) {
                if !text.is_char_boundary(cursor) {
                    continue;
                }
                assert_eq!(
                    closest_surround_pair(
                        &rope,
                        cursor,
                        &PairScan::around(None, cursor),
                        0,
                        cursor..cursor + 1
                    ),
                    closest_by_type(&rope, cursor, None),
                    "seed {seed}, cursor {cursor}, in {text:?}"
                );
            }
        }
    }

    /// The collected zones answer exactly what asking the tree per character
    /// answered.
    ///
    /// This is the whole of what the change to a collected list rests on. A
    /// node's range contains an offset only when that node is an ancestor of the
    /// smallest node there, so the two should agree everywhere, but a boundary
    /// offset is one node's end and the next one's start and the two ways of
    /// asking could split on it. Every offset of a fixture holding strings,
    /// comments, and brackets inside both is what settles that.
    #[test]
    fn collected_zones_answer_what_asking_the_tree_per_character_did() {
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

        let mut zoned = 0usize;
        for cursor in 0..=rope.len() {
            let tree = deepest_tree_at(snapshot, cursor).expect("a covering layer");
            let scan = PairScan::around(Some(tree), cursor);
            let expected = crate::action_handlers::movement::is_in_string_or_comment(tree, cursor);
            zoned += usize::from(expected);
            assert_eq!(scan.skips(cursor), expected, "offset {cursor}");
        }

        assert!(
            zoned > 0,
            "the fixture has to put some offsets inside a zone for this to say anything",
        );
    }

    /// The zones cover as far as the scan reaches, not just around the cursor.
    ///
    /// A scan walks thousands of characters, so a window sized to the cursor's
    /// neighbourhood would leave a distant string unclassified and its brackets
    /// counted as code. The decoy here sits thousands of bytes from the cursor,
    /// which is where a too-small window stops covering and starts answering
    /// with the wrong pair.
    #[test]
    fn a_decoy_bracket_far_from_the_cursor_is_still_inside_its_string() {
        let mut h = TestHarness::with_size(60, 10);
        let padding = "        1,\n".repeat(500);
        let src = format!("fn f() {{\n    let a = (\n        \"decoy ( unclosed\",\n{padding}        x\n    );\n}}\n");
        let path = seed_rs(&mut h, &src);

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

        let real_open = src.find("let a = (").expect("the tuple opens") + "let a = ".len();
        let decoy = src[real_open + 1..]
            .find('(')
            .map(|i| real_open + 1 + i)
            .expect("the string holds a decoy");
        let cursor = src.rfind('x').expect("the fixture ends with x");
        assert!(
            cursor - decoy > 4_000,
            "the decoy has to be far enough that a small window would miss it",
        );

        let tree = deepest_tree_at(snapshot, cursor);
        let found = closest_surround_pair(
            &rope,
            cursor,
            &PairScan::around(tree, cursor),
            0,
            cursor..cursor + 1,
        );
        assert_eq!(
            found.map(|(open, _, open_off, _)| (open, open_off)),
            Some(('(', real_open)),
            "the pair is the real one, not the bracket inside the string",
        );
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

        // Short of the last character, for the reason the property test above
        // gives.
        for cursor in 0..rope.len().saturating_sub(1) {
            let tree = deepest_tree_at(snapshot, cursor);
            assert!(tree.is_some(), "a covering layer at {cursor}");
            assert_eq!(
                closest_surround_pair(
                    &rope,
                    cursor,
                    &PairScan::around(tree, cursor),
                    0,
                    cursor..cursor + 1
                ),
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
        assert_eq!(
            cursor_offset(&mut h),
            2,
            "the selection covers the pair, so the cursor rests on the closer",
        );
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
        assert_eq!(
            find_surround_pair(&r, 2, '(', ')', &PairScan::around(None, 0), 0),
            Some((0, 4))
        );
    }

    #[test]
    fn find_pair_paren_cursor_on_open() {
        let r = rope("(abc)");
        assert_eq!(
            find_surround_pair(&r, 0, '(', ')', &PairScan::around(None, 0), 0),
            Some((0, 4))
        );
    }

    #[test]
    fn find_pair_paren_cursor_on_close() {
        // Trailing text so the close is not the last character, which the
        // buffer-end guard refuses on its own.
        let r = rope("(abc);");
        assert_eq!(
            find_surround_pair(&r, 4, '(', ')', &PairScan::around(None, 0), 0),
            Some((0, 4))
        );
    }

    /// A pair closing the buffer's last character is out of reach, which costs
    /// a real object to keep the behavior Helix has.
    #[test]
    fn a_cursor_with_nothing_after_it_finds_no_pair() {
        let r = rope("(abc)");
        assert_eq!(
            find_surround_pair(&r, 4, '(', ')', &PairScan::around(None, 0), 0),
            None,
            "the cursor sits on the last character"
        );
        assert_eq!(
            find_surround_pair(&r, 2, '(', ')', &PairScan::around(None, 0), 0),
            Some((0, 4)),
            "a cursor with a character after it still resolves the same pair"
        );
    }

    #[test]
    fn find_pair_paren_no_match_returns_none() {
        let r = rope("abc");
        assert_eq!(
            find_surround_pair(&r, 1, '(', ')', &PairScan::around(None, 0), 0),
            None
        );
    }

    #[test]
    fn find_pair_nested_paren_finds_innermost() {
        let r = rope("((abc))");
        assert_eq!(
            find_surround_pair(&r, 3, '(', ')', &PairScan::around(None, 0), 0),
            Some((1, 5))
        );
    }

    #[test]
    fn find_pair_unbalanced_paren_returns_none() {
        let r = rope("(abc");
        assert_eq!(
            find_surround_pair(&r, 1, '(', ')', &PairScan::around(None, 0), 0),
            None
        );
    }

    #[test]
    fn find_pair_quote_cursor_inside() {
        let r = rope("\"abc\"");
        assert_eq!(
            find_surround_pair(&r, 2, '"', '"', &PairScan::around(None, 2), 0),
            Some((0, 4))
        );
    }

    #[test]
    fn find_pair_quote_cursor_on_quote_is_ambiguous() {
        let r = rope("\"abc\"");
        assert_eq!(
            find_surround_pair(&r, 0, '"', '"', &PairScan::around(None, 0), 0),
            None
        );
        assert_eq!(
            find_surround_pair(&r, 4, '"', '"', &PairScan::around(None, 4), 0),
            None
        );
    }

    #[test]
    fn find_pair_quote_no_match_returns_none() {
        let r = rope("abc");
        assert_eq!(
            find_surround_pair(&r, 1, '"', '"', &PairScan::around(None, 1), 0),
            None
        );
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
    fn count_prefix_surround_delete_takes_outer_pair() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "((abc))\n");
        h.type_keys("l l l");

        h.type_keys("2 m d (");
        assert_eq!(
            buffer_text(&h, &path),
            "(abc)\n",
            "the count reached past the nearest pair"
        );
    }

    #[test]
    fn count_prefix_surround_replace_takes_outer_pair() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "((abc))\n");
        h.type_keys("l l l");

        h.type_keys("2 m r ( [");
        assert_eq!(buffer_text(&h, &path), "[(abc)]\n");
    }

    /// The nearest pair and the one around it are different types, so the count
    /// has to order candidates across types rather than per type.
    #[test]
    fn count_prefix_closest_pair_crosses_types() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "([abc])\n");
        h.type_keys("l l l");

        h.type_keys("2 m d m");
        assert_eq!(
            buffer_text(&h, &path),
            "[abc]\n",
            "the parens went, the brackets stayed"
        );
    }

    #[test]
    fn count_past_the_outermost_pair_edits_nothing() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(abc)\n");
        h.type_keys("l l");

        h.type_keys("3 m d (");
        assert_eq!(buffer_text(&h, &path), "(abc)\n");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no surround pair around one of the cursors"),
        );
    }

    /// A chord armed without a count reaches the nearest pair, whatever the
    /// count the last chord captured.
    #[test]
    fn count_does_not_carry_to_the_next_chord() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "((abc))\n");
        h.type_keys("l l l");

        h.type_keys("2 m d (");
        h.type_keys("m d (");
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn surround_replace_nested_pairs_replaces_both() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(a(b)c)\n");
        nested_pairs_with_a_cursor_in_each(&mut h);

        h.type_keys("m r ( [");
        assert_eq!(buffer_text(&h, &path), "[a[b]c]\n");
    }

    /// Either end of a bracket names the whole bracket, so the closing quote
    /// answers to the opening one the user typed.
    #[test]
    fn surround_delete_a_curly_quote_pair() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "\u{2018}abc\u{2019}\n");
        h.type_keys("l l");

        h.type_keys("m d \u{2018}");
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn closest_pair_resolves_closure_bars() {
        let r = rope("|x|");
        assert_eq!(
            closest_surround_pair(&r, 1, &PairScan::around(None, 1), 0, 1..2),
            Some(('|', '|', 0, 2)),
        );
    }

    #[test]
    fn surround_replace_nested_pairs_with_a_wider_delimiter() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(a(b)c)\n");
        nested_pairs_with_a_cursor_in_each(&mut h);

        // Two bytes replacing one, so every offset after the first edit moves.
        h.type_keys("m r ( \u{ab}");
        assert_eq!(buffer_text(&h, &path), "\u{ab}a\u{ab}b\u{bb}c\u{bb}\n");
    }

    #[test]
    fn surround_delete_pairs_sharing_a_quote_aborts() {
        let mut h = TestHarness::with_size(40, 10);
        // The middle quote closes the first pair and opens the second, so the
        // two cursors name overlapping pairs.
        let path = seed(&mut h, "\"a\"b\"\n");
        nested_pairs_with_a_cursor_in_each(&mut h);

        h.type_keys("m d \"");
        assert_eq!(
            buffer_text(&h, &path),
            "\"a\"b\"\n",
            "neither cursor edits, since the pairs overlap",
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("two cursors share a surround pair"),
        );
    }

    /// One cursor without a pair stops the whole operation. Editing the
    /// cursors that matched leaves the user unable to tell which those were.
    #[test]
    fn surround_delete_aborts_when_one_cursor_has_no_pair() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(a)\nbcd\n");
        h.type_keys("l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::AddSelectionBelow);

        h.type_keys("m d (");
        assert_eq!(
            buffer_text(&h, &path),
            "(a)\nbcd\n",
            "the first cursor's pair survives because the second has none",
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no surround pair around one of the cursors"),
        );
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

    /// The key that cancels the chord reaches nothing else.
    ///
    /// The mode shows it for the object chords, but not for this one. The
    /// match menu's surround arms hand back normal mode as they arm, so a
    /// chord armed from select mode has no select mode left to lose. What
    /// normal mode binds Escape to shows it instead.
    #[test]
    fn cancelling_surround_delete_leaves_the_key_hints_alone() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "(abc)\n");
        h.stoat.key_hints_visible = true;

        h.type_keys("m d");
        assert!(h.stoat.pending_surround_delete, "chord armed");

        h.type_keys("escape");
        assert!(!h.stoat.pending_surround_delete, "chord dropped");
        assert!(
            h.stoat.key_hints_visible,
            "normal mode binds Escape to dismissing the hints, which this press never reached"
        );
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
    fn surround_delete_counts_a_balanced_pair_inside_a_string() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "let _ = (\"outer (inner)\");\n";
        let path = seed_rs(&mut h, src);
        let cursor = src.find("outer").expect("cursor target");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);
        h.type_keys("m d (");
        assert_eq!(buffer_text(&h, &path), "let _ = \"outer (inner)\";\n");
    }

    /// The chord that names a pair type reads the text alone, so a delimiter
    /// inside a string literal is a delimiter. Helix answers the same, and its
    /// `m d m` counterpart is the one that consults the tree.
    #[test]
    fn surround_delete_reaches_a_bracket_inside_a_string() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "let s = \"a (inner) b\";\n";
        let path = seed_rs(&mut h, src);
        let cursor = src.find("inner").expect("cursor target");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);

        h.type_keys("m d (");
        assert_eq!(buffer_text(&h, &path), "let s = \"a inner b\";\n");
    }

    #[test]
    fn surround_replace_counts_a_balanced_pair_inside_a_comment() {
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

    /// A char literal is quoted the same way a string is, so a cursor on its
    /// quote resolves the same way.
    ///
    /// The grammar calls it a `char_literal`, which no test for the word
    /// "string" matches. What makes it a quoted node is that the quote closes
    /// it.
    #[test]
    fn surround_delete_quote_on_a_char_literal() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "let c = 'x';\n";
        let path = seed_rs(&mut h, src);
        let cursor = src.find('\'').expect("opening quote present");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);
        h.type_keys("m d '");
        assert_eq!(buffer_text(&h, &path), "let c = x;\n");
    }

    /// A quote with no literal around it resolves to nothing, rather than to
    /// whichever node happens to contain it.
    ///
    /// The apostrophe here sits in a comment, so no node closes on one. Without
    /// that test the climb settles on the comment and pairs the apostrophe with
    /// the comment's own last character.
    #[test]
    fn surround_delete_quote_in_a_comment_finds_no_pair() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "// it's fine\nlet x = 1;\n";
        let path = seed_rs(&mut h, src);
        let cursor = src.find('\'').expect("apostrophe present");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);
        h.type_keys("m d '");
        assert_eq!(buffer_text(&h, &path), src, "nothing is deleted");
    }

    /// A prefixed literal opens with its prefix, not its quote, so a rule
    /// reading the node's first byte refuses it.
    #[test]
    fn surround_delete_quote_on_a_raw_string() {
        let mut h = TestHarness::with_size(60, 10);
        let src = "let s = r\"abc\";\n";
        let path = seed_rs(&mut h, src);
        let cursor = src.find('"').expect("opening quote present");
        crate::action_handlers::movement::jump_to_offset(&mut h.stoat, cursor);
        h.type_keys("m d \"");
        assert_eq!(buffer_text(&h, &path), "let s = rabc;\n");
    }

    #[test]
    fn surround_pair_on_quote_no_tree_returns_none() {
        let r = rope("\"abc\"");
        assert_eq!(
            find_surround_pair(&r, 0, '"', '"', &PairScan::around(None, 0), 0),
            None
        );
        assert_eq!(
            find_surround_pair(&r, 4, '"', '"', &PairScan::around(None, 4), 0),
            None
        );
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
        assert_eq!(
            primary_range(&mut h),
            (0, 5),
            "the pair is what the operation made, so it is what stays selected",
        );
    }

    /// Enter names a line ending, which puts the selection on a line of its
    /// own. LF whatever the file uses, since that is what the buffer holds.
    #[test]
    fn surround_add_enter_wraps_in_line_endings() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("enter");

        assert_eq!(buffer_text(&h, &path), "\nabc\n\n");
        assert!(!h.stoat.pending_surround_add, "the chord is spent");
    }

    /// The wrap ends in normal mode, so the next motion moves rather than
    /// extends. The chord's last key never reaches the keymap, which leaves
    /// the handler as the only place to do it.
    #[test]
    fn surround_add_exits_select_mode() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        assert_eq!(h.stoat.focused_mode(), "select");

        crate::action_handlers::dispatch(&mut h.stoat, &action::SurroundAdd);
        h.type_keys("(");
        assert_eq!(buffer_text(&h, &path), "(abc)\n");
        assert_eq!(h.stoat.focused_mode(), "normal");
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
