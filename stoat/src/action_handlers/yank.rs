use crate::{
    app::{Stoat, UpdateEffect},
    pane::View,
    register::Register,
};
use std::ops::Range;
use stoat_text::{Bias, LineEnding, Point, SelectionGoal};

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
    let Some(fragments) = selection_fragments(stoat) else {
        return UpdateEffect::None;
    };
    // A collapsed selection yields an empty fragment rather than no fragment,
    // so a set that is only collapsed arrives here as a vector of empty
    // strings. Copying that would overwrite the register with nothing.
    if fragments.iter().all(|f| f.is_empty()) {
        return UpdateEffect::None;
    }
    let count = fragments.len();
    write_fragments_to_register(stoat, target, fragments);
    stoat.set_status(format!("yanked {count} selection(s)"));
    UpdateEffect::Redraw
}

/// Write per-selection `fragments` to `target`. The clipboard receives the
/// fragments joined with newlines. Blackhole and the read-only registers
/// (search, selection index, last insert) drop them.
///
/// Shared by yank and by delete, which yanks the removed text before deleting
/// it.
pub(crate) fn write_fragments_to_register(
    stoat: &mut Stoat,
    target: Register,
    fragments: Vec<String>,
) {
    match target {
        Register::Clipboard => {
            crate::host::clipboard_copy(
                stoat.clipboard_host().as_ref(),
                stoat.env_host().as_ref(),
                &fragments.join("\n"),
            );
        },
        Register::Blackhole => {},
        Register::Unnamed | Register::Named(_) => {
            stoat.registers.write(target, fragments);
        },
        Register::Search | Register::SelectionIndex | Register::LastInsert => {},
    }
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

/// Replace every non-empty selection with the caller-selected register's
/// content (or the unnamed register when none is set), following Helix's
/// `replace_with_yanked`.
///
/// Fragments distribute across the selections in start-offset order, the last
/// fragment repeating when the selections outnumber them, and the pending
/// count repeats each fragment. Each replaced selection re-covers the text it
/// received. Empty selections are left untouched and consume no fragment.
///
/// No-op when the register is empty or the focused pane is not an editor.
pub(super) fn replace_with_yanked(stoat: &mut Stoat) -> UpdateEffect {
    let count = stoat.take_pending_count().unwrap_or(1).max(1) as usize;
    let source = stoat.consume_selected_register();
    let Some(fragments) = read_register_fragments(stoat, source) else {
        return UpdateEffect::None;
    };
    if fragments.is_empty() {
        return UpdateEffect::None;
    }

    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let editor_id = match ws.panes.pane(focused).view {
        View::Editor(id) => id,
        _ => return UpdateEffect::None,
    };

    let (buffer_id, entries) = {
        let editor = ws.editors.get_mut(editor_id).expect("editor");
        let buffer_id = editor.buffer_id;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let mut entries: Vec<(usize, usize, usize)> = editor
            .selections
            .all_anchors()
            .iter()
            .map(|sel| {
                let s = buf_snap.resolve_anchor(&sel.start);
                let e = buf_snap.resolve_anchor(&sel.end);
                let (lo, hi) = if s <= e { (s, e) } else { (e, s) };
                (sel.id, lo, hi)
            })
            // Dropped before the payloads are cut so a selection covering
            // nothing consumes no fragment, rather than merely being edited
            // with one. Editing it would insert the fragment outright, since
            // there is no text in the range for it to replace.
            .filter(|(_, lo, hi)| lo < hi)
            .collect();
        entries.sort_by_key(|(_, start, _)| *start);
        (buffer_id, entries)
    };

    if entries.is_empty() {
        return UpdateEffect::None;
    }

    let payloads: Vec<String> = (0..entries.len())
        .map(|idx| fragments[idx.min(fragments.len() - 1)].repeat(count))
        .collect();

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> = entries
            .iter()
            .enumerate()
            .rev()
            .map(|(i, (_, start, end))| (*start..*end, payloads[i].as_str()))
            .collect();
        guard.edit_batch(&batch);
    }

    // Each selection re-covers its replacement. A running length delta shifts
    // later ranges when a fragment differs in length from the text it replaced.
    let mut new_ranges: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new();
    let mut shift = 0isize;
    for (i, (id, start, end)) in entries.iter().enumerate() {
        let new_start = (*start as isize + shift) as usize;
        let new_end = new_start + payloads[i].len();
        new_ranges.insert(*id, (new_start, new_end));
        shift += payloads[i].len() as isize - (*end - *start) as isize;
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if let Some(&(start, end)) = new_ranges.get(&sel.id) {
            new.start = new_buf.anchor_at(start, Bias::Right);
            new.end = new_buf.anchor_at(end, Bias::Right);
            new.reversed = false;
            new.goal = SelectionGoal::None;
        }
        new
    });
    UpdateEffect::Redraw
}

/// Write every non-collapsed selection's content (joined by
/// newlines, in start-offset order) to the system clipboard via
/// the active [`crate::host::ClipboardHost`]. No-op when every
/// selection is collapsed.
pub(super) fn yank_to_clipboard(stoat: &mut Stoat) -> UpdateEffect {
    let Some(fragments) = selection_fragments(stoat) else {
        return UpdateEffect::None;
    };
    if fragments.is_empty() {
        return UpdateEffect::None;
    }
    crate::host::clipboard_copy(
        stoat.clipboard_host().as_ref(),
        stoat.env_host().as_ref(),
        &fragments.join("\n"),
    );
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
    crate::host::clipboard_copy(
        stoat.clipboard_host().as_ref(),
        stoat.env_host().as_ref(),
        &content,
    );
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
    // The clipboard is outside the editor and can hold any line ending, while a
    // buffer holds LF whatever its file uses. Normalizing here rather than at
    // each paste site keeps the invariant a property of the read.
    let content = LineEnding::normalize(&content).into_owned();
    paste_text(stoat, &[content], side)
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

/// Walk every selection in the focused editor in start-offset order and
/// slice each range out of the rope, one fragment per selection like Helix. A
/// collapsed selection yields an empty fragment. Returns `None` when the
/// focused pane is not an editor.
pub(super) fn selection_fragments(stoat: &mut Stoat) -> Option<Vec<String>> {
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
        .map(|sel| {
            let start = buf_snap.resolve_anchor(&sel.start);
            let end = buf_snap.resolve_anchor(&sel.end);
            let (lo, hi) = if start <= end {
                (start, end)
            } else {
                (end, start)
            };
            (lo, hi)
        })
        .collect();
    ranges.sort_unstable();
    let pieces: Vec<String> = ranges
        .into_iter()
        .map(|(lo, hi)| rope.slice(lo..hi).to_string())
        .collect();
    Some(pieces)
}

/// Insert the caller-selected register's content (or the
/// unnamed register when no selection is active) at every
/// selection, either at each selection's `start` (Before) or
/// `end` (After).
fn paste(stoat: &mut Stoat, side: PasteSide) -> UpdateEffect {
    let source = stoat.consume_selected_register();
    let Some(fragments) = read_register_fragments(stoat, source) else {
        return UpdateEffect::None;
    };
    paste_text(stoat, &fragments, side)
}

/// Resolve `register` to its per-selection fragments. Named and unnamed
/// registers read from the in-memory store. Clipboard, search, and
/// last-insert come from host services and each hold a single value, so
/// they resolve to a one-element vec. `SelectionIndex` reads the active
/// selection set, one index per selection.
///
/// Returns `None` for blackhole, for read-only registers whose backing
/// is empty, and for `SelectionIndex` when the focused pane has no
/// selections.
pub(crate) fn read_register_fragments(
    stoat: &mut Stoat,
    register: Register,
) -> Option<Vec<String>> {
    match register {
        Register::Unnamed | Register::Named(_) => {
            stoat.registers.read(register).map(<[String]>::to_vec)
        },
        // Normalized for the same reason as [`paste_clipboard`]: this is the
        // other place clipboard text enters, and it feeds the insert-mode
        // register insert and `replace_with_yanked`. The in-memory registers
        // below hold buffer-sourced text, which is already LF.
        Register::Clipboard => match stoat.clipboard_host().get() {
            Ok(text) => text.map(|t| vec![LineEnding::normalize(&t).into_owned()]),
            Err(err) => {
                tracing::warn!(target: "stoat::yank", ?err, "clipboard read failed");
                None
            },
        },
        Register::Search => stoat.last_search.as_ref().map(|s| vec![s.query.clone()]),
        Register::Blackhole => None,
        Register::LastInsert => stoat.last_insert_text.clone().map(|t| vec![t]),
        Register::SelectionIndex => selection_index_fragments(stoat),
    }
}

/// Build one index fragment per selection ("1", "2", ..., "N") for the
/// focused editor, so paste distributes one index to each selection.
/// Returns `None` when the focused pane is not an editor or has no
/// selections.
fn selection_index_fragments(stoat: &mut Stoat) -> Option<Vec<String>> {
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
    Some((1..=count).map(|i| i.to_string()).collect())
}

/// Insert each fragment at every selection and leave the inserted text
/// selected.
///
/// Each fragment lands at its selection's `start` (Before) or `end` (After),
/// repeated by the pending count. Fragments distribute across selections in
/// start-offset order, and the last fragment repeats when the selections
/// outnumber them, so a single fragment lands at every selection. Each
/// affected selection ends as a forward range over the text it inserted.
///
/// No-op when every fragment is empty or the focused pane is not an editor.
fn paste_text(stoat: &mut Stoat, fragments: &[String], side: PasteSide) -> UpdateEffect {
    if fragments.iter().all(String::is_empty) {
        return UpdateEffect::None;
    }

    // Line-shaped register content (any fragment ends with a line ending)
    // pastes as a line rather than splicing mid-line. After puts it below the
    // line, Before at the line start. One line-shaped fragment settles it for
    // the whole paste, so fragments carrying no line ending land at a line
    // start too.
    let linewise = fragments.iter().any(|f| f.ends_with('\n'));

    let count = stoat.take_pending_count().unwrap_or(1).max(1) as usize;

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
        let max_row = rope.max_point().row;
        let rope_len = rope.len();
        // A buffer with no final line ending has no row after its last one, so
        // the offset that opens a fresh line in a terminated buffer is the end
        // of this one's last line instead. The entry landing there carries the
        // separator its line needs.
        let unterminated = rope_len > 0 && !rope.ends_with("\n");
        let mut open_line = false;
        let entries: Vec<(usize, usize, bool)> = editor
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
                open_line = false;
                let insert_at = match (side, linewise) {
                    (PasteSide::Before, true) => {
                        let row = rope.offset_to_point(lo).row;
                        rope.point_to_offset(Point::new(row, 0))
                    },
                    (PasteSide::After, true) => {
                        // The line below the range's last content line. A range
                        // ending at column 0 consumed the previous line's ending,
                        // so its last content line is one above.
                        let hi_point = rope.offset_to_point(hi);
                        let last_line = if hi > lo && hi_point.column == 0 {
                            hi_point.row.saturating_sub(1)
                        } else {
                            hi_point.row
                        };
                        let next = last_line + 1;
                        if next > max_row {
                            open_line = unterminated;
                            rope_len
                        } else {
                            rope.point_to_offset(Point::new(next, 0))
                        }
                    },
                    (PasteSide::Before, false) => lo,
                    (PasteSide::After, false) => {
                        if lo == hi {
                            rope.next_grapheme_boundary(hi)
                        } else {
                            hi
                        }
                    },
                };
                (sel.id, insert_at, open_line)
            })
            .collect();
        (buffer_id, entries)
    };

    if entries.is_empty() {
        return UpdateEffect::None;
    }

    entries.sort_by_key(|(_, off, _)| *off);

    // Each selection receives its fragment in start-offset order, repeated by
    // the pending count. Selections beyond the fragment count reuse the last
    // fragment. An entry opening a line at an unterminated end carries the
    // separator on its payload rather than as an edit of its own, which would
    // shift the offsets the entries before it were computed against.
    let payloads: Vec<String> = entries
        .iter()
        .enumerate()
        .map(|(idx, (_, _, open_line))| {
            let text = fragments[idx.min(fragments.len() - 1)].repeat(count);
            match open_line {
                true => format!("\n{text}"),
                false => text,
            }
        })
        .collect();

    {
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        let batch: Vec<(Range<usize>, &str)> = entries
            .iter()
            .enumerate()
            .rev()
            .map(|(idx, (_, off, _))| (*off..*off, payloads[idx].as_str()))
            .collect();
        guard.edit_batch(&batch);
    }

    let mut id_to_range: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::with_capacity(entries.len());
    let mut shift: i64 = 0;
    for (idx, (id, off, open_line)) in entries.iter().enumerate() {
        let payload_len = payloads[idx].len();
        // The separator belongs to the previous line's ending rather than to
        // what was pasted, so the selection starts past it.
        let opened = usize::from(*open_line);
        let start = (*off as i64 + shift) as usize + opened;
        id_to_range.insert(*id, (start, start + payload_len - opened));
        shift += payload_len as i64;
    }

    let editor = ws.editors.get_mut(editor_id).expect("editor still exists");
    let new_display = editor.display_map.snapshot();
    let new_buf = new_display.buffer_snapshot();
    editor.selections.transform(new_buf, |sel| {
        let mut new = sel.clone();
        if let Some(&(start, end)) = id_to_range.get(&sel.id) {
            new.start = new_buf.anchor_at(start, Bias::Left);
            new.end = new_buf.anchor_at(end, Bias::Right);
            new.reversed = false;
            new.goal = SelectionGoal::None;
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
        let sel = editor.selections.newest_anchor();
        stoat_text::cursor_offset(
            buf_snap.rope(),
            buf_snap.resolve_anchor(&sel.tail()),
            buf_snap.resolve_anchor(&sel.head()),
        )
    }

    /// Delete yanks what it removes, so deleting nothing must not reach the
    /// register at all. On an empty buffer the sole selection is collapsed and
    /// yields one empty fragment, which is not the same as having something to
    /// store.
    #[test]
    fn deleting_a_collapsed_selection_leaves_the_register_alone() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["kept".to_string()]);
        h.type_keys("escape");

        crate::action_handlers::dispatch(&mut h.stoat, &action::DeleteSelection);
        assert_eq!(
            h.stoat
                .registers
                .read(crate::register::Register::Unnamed)
                .map(<[String]>::to_vec),
            Some(vec!["kept".to_string()]),
            "a delete that removed nothing does not overwrite the register",
        );
    }

    /// Yanking a selection that covers nothing stores nothing and claims
    /// nothing.
    ///
    /// A collapsed selection still yields a fragment, just an empty one, so a
    /// guard that only rejects a fragmentless set lets it overwrite whatever
    /// the register held. The delete side is pinned by
    /// [`deleting_a_collapsed_selection_leaves_the_register_alone`].
    #[test]
    fn yanking_a_collapsed_selection_leaves_the_register_alone() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["kept".to_string()]);
        h.type_keys("escape");

        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        assert_eq!(
            h.stoat
                .registers
                .read(crate::register::Register::Unnamed)
                .map(<[String]>::to_vec),
            Some(vec!["kept".to_string()]),
            "a yank that copied nothing does not overwrite the register",
        );
        assert_eq!(
            h.stoat.pending_message, None,
            "and does not report a yank that did not happen",
        );
    }

    #[test]
    fn yank_stores_primary_selection_in_unnamed() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
        assert_eq!(stored, Some("abc".to_string()));
    }

    #[test]
    fn yank_bare_cursor_yanks_char() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
        assert_eq!(stored, Some("a".to_string()));
    }

    #[test]
    fn paste_after_inserts_at_selection_end() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        h.type_keys("escape");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abcabc\n");
        assert_eq!(cursor_offset(&mut h), 5);
    }

    #[test]
    fn paste_before_inserts_at_selection_start() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        h.type_keys("escape");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteBefore);
        assert_eq!(buffer_text(&h, &path), "abcabc\n");
        assert_eq!(cursor_offset(&mut h), 2);
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
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        h.type_keys("escape");
        h.type_keys("h");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "ababcc\n");
    }

    #[test]
    fn paste_after_selects_inserted_text() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        h.type_keys("escape");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abcabc\n");
        assert_eq!(h.selection_spans(), vec![(3, 6, false)]);
    }

    #[test]
    fn paste_after_honors_count() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        h.type_keys("escape");
        h.stoat.pending_count = Some(3);
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abcabcabcabc\n");
        assert_eq!(h.selection_spans(), vec![(3, 12, false)]);
    }

    #[test]
    fn yank_via_y_binding() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l");
        h.type_keys("escape");
        h.type_keys("y");
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
        assert_eq!(stored, Some("abc".to_string()));
    }

    #[test]
    fn paste_after_via_p_binding() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        h.type_keys("y");
        h.type_keys("escape");
        h.type_keys("p");
        assert_eq!(buffer_text(&h, &path), "abcabc\n");
    }

    #[test]
    fn paste_before_via_capital_p_binding() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
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
            .map(|f| f.join("\n"));
        assert_eq!(stored, Some("abc\ndef".to_string()));
    }

    #[test]
    fn yank_stores_a_fragment_per_selection() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(<[String]>::to_vec);
        assert_eq!(stored, Some(vec!["abc".to_string(), "def".to_string()]));
        assert_eq!(
            h.stoat.pending_message,
            Some("yanked 2 selection(s)".to_string())
        );
    }

    #[test]
    fn delete_yanks_the_removed_text_and_paste_restores_it() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::DeleteSelection);
        assert_eq!(buffer_text(&h, &path), "\n");
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
        assert_eq!(stored, Some("abc".to_string()));

        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteBefore);
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    #[test]
    fn blackhole_prefixed_delete_leaves_registers_untouched() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["keep".to_string()]);
        h.type_keys("v l l");
        h.type_keys("escape");
        h.type_keys("\" _");
        crate::action_handlers::dispatch(&mut h.stoat, &action::DeleteSelection);
        assert_eq!(buffer_text(&h, &path), "\n");
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
        assert_eq!(stored, Some("keep".to_string()));
    }

    #[test]
    fn delete_no_yank_deletes_but_leaves_registers_untouched() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["keep".to_string()]);
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::DeleteSelectionNoYank);
        assert_eq!(buffer_text(&h, &path), "\n");
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
        assert_eq!(stored, Some("keep".to_string()));
    }

    #[test]
    fn change_whole_line_opens_a_fresh_indented_line() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "    a\n    b\n    c\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::SelectLineBelow);
        crate::action_handlers::dispatch(&mut h.stoat, &action::ChangeSelection);
        assert_eq!(buffer_text(&h, &path), "    a\n    \n    c\n");
    }

    #[test]
    fn change_partial_line_deletes_inline() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abcdef\n");
        h.type_keys("v l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::ChangeSelection);
        assert_eq!(buffer_text(&h, &path), "ef\n");
    }

    #[test]
    fn select_mode_delete_exits_to_normal() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abcdef\n");
        h.type_keys("v l l d");
        assert_eq!(buffer_text(&h, &path), "def\n");
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    /// A block cursor left on the rope end rests past the last character and
    /// covers nothing, so insert types at the end and append does too.
    ///
    /// The end is a cursor position of its own rather than an alias for the
    /// cell before it, which is what keeps the position past the final
    /// character reachable at all. A change here shows up as text landing on
    /// the wrong side of the cursor at the end of a file and nowhere else,
    /// which is why the two keys are pinned together.
    #[test]
    fn delete_at_buffer_end_leaves_the_cursor_past_the_last_character() {
        let typed_after = |keys: &str| {
            let mut h = TestHarness::with_size(40, 10);
            let path = seed(&mut h, "abc");
            h.type_keys("l v l d");
            assert_eq!(
                buffer_text(&h, &path),
                "a",
                "the delete empties the line's tail"
            );
            assert_eq!(cursor_offset(&mut h), 1, "the cursor rests on the end");

            h.type_keys(keys);
            buffer_text(&h, &path)
        };

        assert_eq!(
            typed_after("i x escape"),
            "ax",
            "insert goes at the end, where the cursor sits",
        );
        assert_eq!(
            typed_after("a x escape"),
            "ax",
            "and append reaches the same insert point",
        );
    }

    #[test]
    fn linewise_paste_after_inserts_the_line_below() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "X\nY\nZ\n");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["X\n".to_string()]);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "X\nY\nZ\nX\n");
    }

    /// Pasting a line below the last line of a buffer that has no final newline
    /// opens a line rather than splicing onto the one that is there.
    ///
    /// The rope reports a row after a trailing line ending and none without
    /// one, so the offset that means "the start of a fresh line" for a
    /// newline-terminated buffer means "the end of the last line's text" for
    /// this one. Files opened without a final newline are ordinary.
    #[test]
    fn linewise_paste_after_opens_a_line_at_an_unterminated_end() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\ndef");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["X\n".to_string()]);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abc\ndef\nX\n");
    }

    /// A buffer that does end in a newline already has the row to paste into,
    /// so pasting past its last line adds no separator of its own.
    ///
    /// Reaching that branch needs the cursor on the row after the final line
    /// ending, which only a terminated buffer has. Without this the opening
    /// newline could be added unconditionally and nothing would notice.
    #[test]
    fn linewise_paste_past_the_last_line_adds_no_blank_line() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["X\n".to_string()]);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abc\nX\n");
    }

    /// The probe for a missing final line ending reads the buffer's end
    /// safely, whatever character sits there.
    ///
    /// Rope offsets are byte offsets, so a multibyte final character puts the
    /// last byte inside it. A probe that reads a character at that offset
    /// slices mid-character and panics. The crash reaches every paste in such
    /// a buffer, linewise or not, because the probe runs before the paste
    /// shape decides anything.
    #[test]
    fn paste_into_a_buffer_that_ends_in_a_multibyte_character() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "café");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["x".to_string()]);
        for _ in 0..3 {
            crate::action_handlers::dispatch(&mut h.stoat, &action::MoveRight);
        }
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "caféx");
    }

    #[test]
    fn linewise_paste_before_inserts_the_line_above() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "X\nY\nZ\n");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["X\n".to_string()]);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::MoveDown);
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteBefore);
        assert_eq!(buffer_text(&h, &path), "X\nY\nX\nZ\n");
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
        h.type_keys("v l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        crate::action_handlers::dispatch(&mut h.stoat, &action::AddSelectionBelow);
        crate::action_handlers::dispatch(&mut h.stoat, &action::AddSelectionBelow);
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abab\ncdab\nefab\n");
    }

    #[test]
    fn paste_distributes_one_fragment_per_selection() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        h.stoat.registers.write(
            crate::register::Register::Unnamed,
            vec!["A".to_string(), "B".to_string()],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abcA\ndefB\n");
    }

    #[test]
    fn paste_repeats_last_fragment_across_extra_selections() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["A".to_string()]);
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abcA\ndefA\n");
    }

    #[test]
    fn paste_keeps_a_newline_bearing_fragment_intact() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        h.stoat.registers.write(
            crate::register::Register::Unnamed,
            vec!["x\ny".to_string(), "z".to_string()],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(buffer_text(&h, &path), "abcx\ny\ndefz\n");
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
    fn yank_to_clipboard_bare_cursor_yanks_char() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &action::YankToClipboard);
        assert_eq!(h.fake_clipboard().writes(), vec!["a".to_string()]);
    }

    #[test]
    fn yank_to_clipboard_emits_osc52_over_ssh() {
        let mut h = TestHarness::with_size(40, 10);
        h.fake_env().set("SSH_TTY", "/dev/pts/0");
        seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &action::YankToClipboard);
        assert_eq!(h.fake_clipboard().writes(), vec!["abc\ndef".to_string()]);
        assert_eq!(
            h.fake_clipboard().osc52_emits(),
            vec!["abc\ndef".to_string()],
            "a keyboard yank forwards to the local clipboard over SSH"
        );
    }

    #[test]
    fn yank_to_clipboard_skips_osc52_locally() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &action::YankToClipboard);
        assert_eq!(h.fake_clipboard().writes(), vec!["abc\ndef".to_string()]);
        assert!(
            h.fake_clipboard().osc52_emits().is_empty(),
            "no OSC 52 forwarding outside an SSH session"
        );
    }

    #[test]
    fn clipboard_register_yank_emits_osc52_over_ssh() {
        let mut h = TestHarness::with_size(40, 10);
        h.fake_env().set("SSH_TTY", "/dev/pts/0");
        seed(&mut h, "abc\n");
        h.type_keys("v l l");
        super::execute_select_register(&mut h.stoat, '+');
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        assert_eq!(h.fake_clipboard().writes(), vec!["abc".to_string()]);
        assert_eq!(
            h.fake_clipboard().osc52_emits(),
            vec!["abc".to_string()],
            "the `\"+y` clipboard-register yank forwards over SSH too"
        );
    }

    #[test]
    fn paste_clipboard_after_inserts_clipboard_content() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.fake_clipboard().set("xyz").unwrap();
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteClipboardAfter);
        assert_eq!(buffer_text(&h, &path), "abcxyz\n");
    }

    /// Clipboard text arrives normalized, so a CRLF clipboard cannot put a
    /// carriage return into the buffer.
    ///
    /// A buffer holds LF whatever its file uses, and everything downstream is
    /// built on that. Saving a CRLF-detected file rewrites each `\n` back into
    /// `\r\n`, so a `\r` that slipped in becomes `\r\r\n`, and row-end logic
    /// lands between the `\r` and the `\n` because the rope counts `\r` as
    /// content while `\r\n` is a single grapheme.
    #[test]
    fn pasting_crlf_clipboard_text_normalizes_it() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.fake_clipboard().set("x\r\ny\r\n").unwrap();
        h.type_keys("escape");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteClipboardAfter);
        assert!(
            !buffer_text(&h, &path).contains('\r'),
            "no carriage return reaches the buffer, got {:?}",
            buffer_text(&h, &path),
        );
    }

    /// The same for the clipboard register, which is a separate read.
    #[test]
    fn inserting_the_clipboard_register_normalizes_crlf() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.fake_clipboard().set("x\r\ny\r\n").unwrap();
        h.type_keys("escape");
        h.type_keys("\" * p");
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert!(
            !buffer_text(&h, &path).contains('\r'),
            "no carriage return reaches the buffer, got {:?}",
            buffer_text(&h, &path),
        );
    }

    /// A single line-shaped fragment makes the whole paste linewise.
    ///
    /// A mixed register is what an ordinary multi-cursor yank produces when one
    /// selection covers a line ending and another does not. The classification
    /// is a property of the register rather than of each fragment, so the
    /// fragment carrying no line ending opens a line as well instead of
    /// splicing into the one it was pasted onto. This follows the reference
    /// implementation, which asks the same question with `any`.
    #[test]
    fn a_mixed_register_pastes_linewise() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        h.stoat.registers.write(
            crate::register::Register::Unnamed,
            vec!["X\n".to_string(), "Y".to_string()],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &action::PasteAfter);
        assert_eq!(
            buffer_text(&h, &path),
            "abc\nX\ndef\nY",
            "both fragments land at a line start, including the one with no newline",
        );
    }

    #[test]
    fn paste_clipboard_before_inserts_clipboard_content() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.fake_clipboard().set("xyz").unwrap();
        h.type_keys("v l l");
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
        h.type_keys("v l l");
        h.type_keys("escape");
        h.type_keys("space \" y");
        assert_eq!(h.fake_clipboard().writes(), vec!["abc".to_string()]);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn select_register_then_yank_writes_to_named() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l");
        h.type_keys("escape");
        h.type_keys("\" a y");
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Named('a'))
            .map(|f| f.join("\n"));
        assert_eq!(stored, Some("abc".to_string()));
        let unnamed = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
        assert_eq!(unnamed, None);
    }

    #[test]
    fn select_register_consumed_by_one_op() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l");
        h.type_keys("escape");
        h.type_keys("\" a y");
        assert!(h.stoat.selected_register.is_none());
        crate::action_handlers::dispatch(&mut h.stoat, &action::Yank);
        let stored_a = h
            .stoat
            .registers
            .read(crate::register::Register::Named('a'))
            .map(|f| f.join("\n"));
        assert_eq!(stored_a, Some("abc".to_string()));
        let unnamed = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
        assert_eq!(unnamed, Some("abc".to_string()));
    }

    #[test]
    fn paste_from_named_register() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat.registers.write(
            crate::register::Register::Named('a'),
            vec!["xyz".to_string()],
        );
        h.type_keys("v l l");
        h.type_keys("escape");
        h.type_keys("\" a p");
        assert_eq!(buffer_text(&h, &path), "abcxyz\n");
    }

    #[test]
    fn select_register_dquote_selects_unnamed() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "abc\n");
        h.type_keys("v l l");
        h.type_keys("escape");
        h.type_keys("\" \" y");
        let stored = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
        assert_eq!(stored, Some("abc".to_string()));
    }

    #[test]
    fn insert_register_inserts_named_at_cursor() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat.registers.write(
            crate::register::Register::Named('a'),
            vec!["xyz".to_string()],
        );
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
            .write(crate::register::Register::Unnamed, vec!["xyz".to_string()]);
        h.type_keys("a");
        h.type_keys("Ctrl-r");
        h.type_keys("\"");
        assert_eq!(buffer_text(&h, &path), "axyzbc\n");
    }

    #[test]
    fn insert_register_gives_each_cursor_its_own_fragment() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "xy\nzw\n");
        h.stoat.registers.write(
            crate::register::Register::Unnamed,
            vec!["A".to_string(), "B".to_string()],
        );
        h.type_keys("C");
        h.type_keys("i");
        h.type_keys("Ctrl-r");
        h.type_keys("\"");
        assert_eq!(
            buffer_text(&h, &path),
            "Axy\nBzw\n",
            "every cursor received the whole register joined together"
        );
    }

    #[test]
    fn insert_register_repeats_the_last_fragment() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "xy\nzw\nuv\n");
        h.stoat.registers.write(
            crate::register::Register::Unnamed,
            vec!["A".to_string(), "B".to_string()],
        );
        h.type_keys("2 C");
        h.type_keys("i");
        h.type_keys("Ctrl-r");
        h.type_keys("\"");
        assert_eq!(buffer_text(&h, &path), "Axy\nBzw\nBuv\n");
    }

    #[test]
    fn insert_register_records_the_newest_cursor_for_repeat() {
        let mut h = TestHarness::with_size(40, 10);
        seed(&mut h, "xy\nzw\n");
        h.stoat.registers.write(
            crate::register::Register::Unnamed,
            vec!["A".to_string(), "B".to_string()],
        );
        h.type_keys("C");
        h.type_keys("i");
        h.type_keys("Ctrl-r");
        h.type_keys("\"");
        h.type_keys("escape");
        assert_eq!(
            h.stoat.last_insert_text.as_deref(),
            Some("B"),
            "repeat replays one string, so it is one a cursor actually received"
        );
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
        h.stoat.registers.write(
            crate::register::Register::Named('a'),
            vec!["xyz".to_string()],
        );
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
        h.type_keys("v l l");
        h.type_keys("escape");
        h.type_keys("\" * y");
        assert_eq!(h.fake_clipboard().writes(), vec!["abc".to_string()]);
        let unnamed = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
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
        h.type_keys("v l l");
        h.type_keys("escape");
        h.type_keys("\" _ y");
        let unnamed = h
            .stoat
            .registers
            .read(crate::register::Register::Unnamed)
            .map(|f| f.join("\n"));
        assert_eq!(unnamed, None);
        assert_eq!(h.fake_clipboard().writes(), Vec::<String>::new());
    }

    #[test]
    fn paste_search_register_pastes_last_search_query() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat.last_search = Some(crate::action_handlers::search::LastSearch::new(
            "needle".into(),
            crate::action_handlers::search::SearchDirection::Forward,
        ));
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

    #[test]
    fn replace_with_yanked_replaces_selection_and_selects_it() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["xyz".to_string()]);
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::ReplaceWithYanked);
        assert_eq!(buffer_text(&h, &path), "xyz\n");
        assert_eq!(h.selection_spans(), vec![(0, 3, false)]);
    }

    #[test]
    fn replace_with_yanked_empty_register_is_noop() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.type_keys("v l l");
        crate::action_handlers::dispatch(&mut h.stoat, &action::ReplaceWithYanked);
        assert_eq!(buffer_text(&h, &path), "abc\n");
    }

    /// A replace covering no text has nothing to replace, so it must not insert
    /// the fragment instead. An empty buffer holds one collapsed selection,
    /// which is the whole selection set here.
    #[test]
    fn replace_with_yanked_over_a_collapsed_selection_is_a_noop() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["X".to_string()]);
        h.type_keys("escape");

        crate::action_handlers::dispatch(&mut h.stoat, &action::ReplaceWithYanked);
        assert_eq!(buffer_text(&h, &path), "", "nothing was covered to replace");
    }

    /// A collapsed selection consumes no fragment, so the ones after it are not
    /// pushed onto the wrong fragment.
    ///
    /// Replacing with an empty fragment leaves that selection covering nothing,
    /// and no widening runs afterwards. The next replace therefore sees a
    /// collapsed selection beside a live one on a buffer that is not empty, so
    /// the state under test is reachable rather than hypothetical.
    #[test]
    fn replace_with_yanked_gives_the_first_fragment_to_the_first_live_selection() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        h.stoat.registers.write(
            crate::register::Register::Unnamed,
            vec![String::new(), "X".to_string()],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &action::ReplaceWithYanked);
        assert_eq!(
            buffer_text(&h, &path),
            "\nX\n",
            "the first selection is emptied"
        );
        assert_eq!(
            h.selection_spans(),
            vec![(0, 0, false), (1, 2, false)],
            "the emptied selection is left collapsed",
        );

        h.stoat.registers.write(
            crate::register::Register::Unnamed,
            vec!["P".to_string(), "Q".to_string()],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &action::ReplaceWithYanked);
        assert_eq!(
            buffer_text(&h, &path),
            "\nP\n",
            "the live selection takes the first fragment and the collapsed one takes none",
        );
    }

    #[test]
    fn replace_with_yanked_distributes_one_fragment_per_selection() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        h.stoat.registers.write(
            crate::register::Register::Unnamed,
            vec!["A".to_string(), "B".to_string()],
        );
        crate::action_handlers::dispatch(&mut h.stoat, &action::ReplaceWithYanked);
        assert_eq!(buffer_text(&h, &path), "A\nB\n");
    }

    #[test]
    fn replace_with_yanked_repeats_last_fragment_across_extra_selections() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\ndef\n");
        make_two_selections(&mut h);
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["A".to_string()]);
        crate::action_handlers::dispatch(&mut h.stoat, &action::ReplaceWithYanked);
        assert_eq!(buffer_text(&h, &path), "A\nA\n");
    }

    #[test]
    fn replace_with_yanked_honors_count() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["x".to_string()]);
        h.type_keys("v l l");
        h.stoat.pending_count = Some(3);
        crate::action_handlers::dispatch(&mut h.stoat, &action::ReplaceWithYanked);
        assert_eq!(buffer_text(&h, &path), "xxx\n");
    }

    #[test]
    fn replace_with_yanked_via_r_binding() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["xyz".to_string()]);
        h.type_keys("R");
        assert_eq!(buffer_text(&h, &path), "xyzbc\n");
    }

    #[test]
    fn replace_with_yanked_select_mode_exits_to_normal() {
        let mut h = TestHarness::with_size(40, 10);
        let path = seed(&mut h, "abc\n");
        h.stoat
            .registers
            .write(crate::register::Register::Unnamed, vec!["xyz".to_string()]);
        h.type_keys("v l l");
        h.type_keys("R");
        assert_eq!(buffer_text(&h, &path), "xyz\n");
        assert_eq!(h.stoat.focused_mode(), "normal");
    }
}
