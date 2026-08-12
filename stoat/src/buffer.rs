use crate::diff_map::DiffMap;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::HashMap, ops::Range, sync::Arc};
pub use stoat_text::BufferId;
use stoat_text::{
    patch::{Edit, Patch},
    Anchor, Bias, Cursor, Dimensions, Edit as TreeEdit, Fragment, IndentStyle, InsertionFragment,
    InsertionFragmentKey, KeyedItem, Locator, Point, Rope, RopeCursor, Selection, SumTree, UndoMap,
    UndoOperation,
};

/// Op-log length past which [`TextBuffer::history`] persists a compacted seed
/// rather than the whole log.
///
/// The log is unbounded in the live buffer, so without a ceiling a long session
/// writes every keystroke it ever took into `state.ron` and replays them all on
/// reopen. Compaction buys that bound by giving up undo past the threshold,
/// which is the trade helix and zed already make by persisting no log at all.
pub(crate) const OPS_COMPACT_THRESHOLD: usize = 4096;

/// Timestamp a compacted history's seed edit takes when replayed. Replay starts
/// from a fresh [`TextBuffer`] and assigns timestamps sequentially from one, so
/// the seed is always the first.
const SEED_TIMESTAMP: u64 = 1;

pub struct TextBuffer {
    pub snapshot: TextBufferSnapshot,
    pub dirty: bool,
    /// Edit-frontier timestamp (the `edit_history` top) captured at the last
    /// clean point, whether a save or the seeded/pristine baseline. `None` is
    /// the pristine empty state. Reaching this frontier again via undo/redo
    /// reads as clean; [`Self::saved_text`] adds the content-based path that
    /// also clears [`Self::dirty`] when an edit restores the saved bytes off a
    /// diverged frontier.
    saved_marker: Option<u64>,
    /// Visible text captured at the last [`Self::mark_clean`], or `None` before
    /// the first clean point. Lets [`Self::recompute_dirty`] clear
    /// [`Self::dirty`] whenever content returns to the saved bytes even though
    /// the edit frontier has moved past [`Self::saved_marker`] -- the
    /// type-a-char-then-delete-it case the frontier comparison alone misses.
    saved_text: Option<Rope>,
    /// Snapshot version [`Self::saved_text`] was captured at, so the comparison
    /// against it can ask which spans have changed since.
    ///
    /// Distinct from [`Self::saved_marker`], which names the last edit rather
    /// than the snapshot. An undo carries the snapshot onto a timestamp of its
    /// own while leaving the marker on the last surviving edit, and it is the
    /// snapshot's text that was saved, so a diff taken from the marker would
    /// describe a text that was never captured.
    ///
    /// Set and cleared with [`Self::saved_text`], and like it never persisted,
    /// so a restored buffer answers from bytes it holds rather than a version
    /// whose fragments are gone.
    saved_version: Option<u64>,
    pub diff_map: Option<DiffMap>,
    next_timestamp: u64,
    buffer_id: BufferId,
    /// Stack of edit groups eligible to be the target of the next `undo()`.
    /// One group is one logical undo step -- a whole dispatched action or a
    /// whole insert-mode session. Extended by `edit()`, popped by `undo()`.
    /// Independent of [`Self::ops`], which records every edit and undo for replay.
    edit_history: Vec<UndoGroup>,
    /// Stack of edit groups undone and eligible for the next `redo()`. Pushed on
    /// `undo()`, popped on `redo()`, cleared on any new `edit()`.
    redo_history: Vec<UndoGroup>,
    /// Count of leading [`Self::edit_history`] groups that seeded the buffer's
    /// initial content rather than being user edits. [`Self::undo`] refuses to
    /// pop below this floor, so undoing a freshly loaded file is a no-op instead
    /// of reverting the whole load. Zero for a buffer created empty via
    /// [`Self::new`], since it has no seed to protect.
    undo_floor: usize,
    /// Whether [`Self::begin_group`] opened a group. While open, edits collapse
    /// into one logical undo step. The group is materialized lazily on its first
    /// edit, so a group that never edits leaves `edit_history` untouched -- which
    /// keeps a wrapped-but-non-editing action (including `undo`/`redo` itself)
    /// from stacking an empty step.
    open_group: bool,
    /// Whether the open group has taken at least one edit and been pushed onto
    /// `edit_history`, distinguishing appending to it from starting it.
    open_group_started: bool,
    /// Editor selections captured at [`Self::begin_group`], moved into the group
    /// when it materializes and restored when the group is undone.
    open_group_before: Arc<[Selection<Anchor>]>,
    /// Chronological log of user-driven mutations. Replaying this on a fresh
    /// [`TextBuffer`] reconstructs an identical fragment tree, anchors, and
    /// undo map, which is how workspace save/restore preserves selections and
    /// undo stack across sessions.
    ///
    /// Grows without bound in a live session. [`Self::history`] is where the
    /// ceiling is applied, so what persists past [`OPS_COMPACT_THRESHOLD`] is a
    /// seed rather than this log.
    ///
    /// The seed edit a loaded file opens with carries no text. [`Self::seed_text`]
    /// holds it instead, and [`Self::history`] puts it back.
    ops: Vec<BufferOp>,
    /// Text [`Self::with_text`] loaded the buffer with, standing in for the
    /// first entry of [`Self::ops`].
    ///
    /// Recording that entry the way every other edit is recorded would keep a
    /// heap copy of the whole file for as long as the buffer is open, next to
    /// the rope built from the same bytes. A rope costs nothing at load, since
    /// it shares its chunks with the live one, and diverges only as far as the
    /// edits since have rewritten them.
    ///
    /// `Some` exactly when the first op is that emptied seed edit, which is
    /// what lets [`Self::history`] restore it by position. A buffer loaded
    /// empty pushed no op and leaves this `None`, as does one replayed by
    /// [`Self::from_history`], whose ops are all genuine.
    seed_text: Option<Rope>,
    next_checkpoint_id: u32,
    /// Named markers on the op log placed by `commit_undo_checkpoint`. Read by
    /// checkpoint-navigation actions; never mutated by `edit` / `undo` / `redo`.
    checkpoints: Vec<Checkpoint>,
    /// Indentation unit this buffer uses, detected from its content at load and
    /// falling back to [`IndentStyle::default`] when the content carries no
    /// evidence. Cached rather than re-detected per edit.
    indent_style: IndentStyle,
}

/// A single replayable mutation on a [`TextBuffer`]. Edits record the `(range,
/// text)` inputs; undos target the top of the edit history the same way
/// interactive `u` does; redos target the top of the redo history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BufferOp {
    Edit { old: Range<usize>, text: String },
    Undo,
    Redo,
}

/// A single logical undo step covering the edits made by one dispatched action
/// or a whole insert-mode session, plus the editor selections to restore when
/// the group is undone or redone.
///
/// Grouping is an in-session overlay on the flat [`BufferOp`] log, which still
/// records each edit and undo individually, so it is not persisted -- a
/// restored buffer replays every edit as its own singleton group.
struct UndoGroup {
    /// Edit timestamps in application order. Undo toggles them in reverse.
    edits: Vec<u64>,
    /// Editor selections captured when the group opened, restored on undo.
    selections_before: Arc<[Selection<Anchor>]>,
    /// Editor selections captured when the group sealed, restored on redo.
    selections_after: Arc<[Selection<Anchor>]>,
}

/// Serializable buffer state for persistence. Holds the op log plus the
/// last-clean edit frontier, replayed via [`TextBuffer::from_history`].
///
/// The log is either the buffer's own or, past [`OPS_COMPACT_THRESHOLD`], a
/// single seed edit standing in for it. A seed reproduces the text but not the
/// fragment tree, so anchors taken against the live buffer do not survive it.
/// [`Self::compacted`] is how a holder of such anchors learns it has to
/// re-express them.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BufferHistory {
    pub ops: Vec<BufferOp>,
    /// Persisted [`TextBuffer::saved_marker`], the last-clean edit frontier.
    /// Deterministic replay reassigns identical timestamps, so it identifies
    /// the same frontier on restore. `#[serde(default)]` reads an older state
    /// file (which stored a `dirty` bool) as `None`, so a clean buffer restores
    /// dirty once and self-heals on the next save.
    #[serde(default)]
    pub saved_marker: Option<u64>,
    /// Persisted [`TextBuffer::undo_floor`], the count of leading seed groups
    /// protected from undo. `#[serde(default)]` reads an older state file as 0,
    /// so a restored buffer allows undoing its seed once and self-heals when the
    /// file is next reopened via [`TextBuffer::with_text`].
    #[serde(default)]
    pub undo_floor: usize,
    /// Whether [`Self::ops`] is a compacted seed rather than the buffer's own
    /// log. This describes how the value was produced rather than what it
    /// holds, so it is not part of the on-disk format. A history read back from
    /// disk is just a log to replay, and reads `false`.
    #[serde(skip)]
    pub compacted: bool,
}

/// Stable identifier for a [`Checkpoint`] within a single [`TextBuffer`].
/// Monotonically increasing per buffer; not unique across buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CheckpointId(pub u32);

/// Named marker on a [`TextBuffer`]'s op log. `op_index` is the value of
/// `ops.len()` at the time the checkpoint was placed, so checkpoints partition
/// the linear undo timeline into reachable navigation targets.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Checkpoint {
    pub id: CheckpointId,
    pub op_index: usize,
    pub label: Option<String>,
}

#[derive(Clone)]
pub struct TextBufferSnapshot {
    pub visible_text: Rope,
    pub(crate) deleted_text: Rope,
    fragments: SumTree<Fragment>,
    insertions: SumTree<InsertionFragment>,
    undo_map: UndoMap,
    pub version: u64,
    /// Which buffer's fragment tree this snapshot is, so a caller holding an
    /// [`Anchor`] can tell whether it names a position here at all. Resolving a
    /// foreign anchor yields an offset rather than an error.
    pub buffer_id: BufferId,
}

impl TextBuffer {
    pub fn new(buffer_id: BufferId) -> Self {
        let cx = &None;
        let mut fragments = SumTree::new(cx);
        let insertions = SumTree::new(());

        fragments.push(
            Fragment {
                id: Locator::min(),
                timestamp: 0,
                insertion_offset: 0,
                len: 0,
                visible: false,
                deletions: Default::default(),
                max_undos: 0,
            },
            cx,
        );

        Self {
            snapshot: TextBufferSnapshot {
                visible_text: Rope::new(),
                deleted_text: Rope::new(),
                fragments,
                insertions,
                undo_map: UndoMap::new(),
                version: 0,
                buffer_id,
            },
            dirty: false,
            saved_marker: None,
            saved_text: None,
            saved_version: None,
            diff_map: None,
            next_timestamp: 1,
            buffer_id,
            edit_history: Vec::new(),
            redo_history: Vec::new(),
            undo_floor: 0,
            open_group: false,
            open_group_started: false,
            open_group_before: Arc::from([]),
            ops: Vec::new(),
            seed_text: None,
            next_checkpoint_id: 0,
            checkpoints: Vec::new(),
            indent_style: IndentStyle::default(),
        }
    }

    pub fn with_text(buffer_id: BufferId, text: &str) -> Self {
        let mut buf = Self::new(buffer_id);
        if !text.is_empty() {
            buf.edit(0..0, text);

            // Seeded through `edit` so the fragment tree, timestamps and dirty
            // state are built exactly as any other edit builds them, then the
            // recorded copy of the file is handed to the rope that already
            // holds those bytes.
            buf.seed_text = Some(buf.snapshot.visible_text.clone());
            let Some(BufferOp::Edit { text, .. }) = buf.ops.first_mut() else {
                unreachable!("the edit above is the log's first op")
            };
            *text = String::new();
        }
        // Empty content is a baseline too. Skipping this leaves `saved_text`
        // unset, and the content comparison that clears a round-trip edit
        // answers false whenever there is nothing to compare against, so the
        // buffer would read modified while holding exactly the saved bytes.
        buf.mark_clean();
        buf.undo_floor = buf.edit_history.len();
        buf.detect_indent_style();
        buf
    }

    /// The indentation unit this buffer uses, detected from its content.
    pub fn indent_style(&self) -> IndentStyle {
        self.indent_style
    }

    /// Re-detect and cache the buffer's indentation style from its current
    /// content, falling back to the default when the content shows no evidence.
    fn detect_indent_style(&mut self) {
        self.indent_style = stoat_text::detect_indent_style(self.rope()).unwrap_or_default();
    }

    /// Apply several disjoint replacements in two passes over the fragment
    /// tree, however many there are, rather than one rebuild each.
    ///
    /// `edits` are pre-edit coordinates sorted descending by start, which is what
    /// applying them right to left needs and what every caller already builds.
    /// Ranges must not overlap. Touching ranges and repeated empty ranges at one
    /// offset are fine.
    ///
    /// An empty range sharing an offset with a deleting range's start is out of
    /// contract. Sequentially, whichever ran first decides whether
    /// the inserted text lands before or after the run the other deletes, and a
    /// single forward pass builds the tree left to right, so it cannot place a
    /// later fragment ahead of one it has already pushed. Both orders are
    /// defensible and the callers never produce the shape, so it is rejected
    /// rather than silently resolved.
    ///
    /// The result is indistinguishable from calling [`Self::edit`] on each in the
    /// given order. The recorded ops, their timestamps, and the undo grouping all
    /// come out the same, because the pass reads the old tree, against which every
    /// range's coordinates are correct whichever order they are visited in. What
    /// it saves is the N-1 extra tree rebuilds, rope rebuilds, and dirty
    /// recomputations that N separate calls pay for one keystroke.
    pub fn edit_batch(&mut self, edits: &[(Range<usize>, &str)]) {
        if edits.is_empty() {
            return;
        }
        if edits.len() == 1 {
            let (range, text) = &edits[0];
            self.edit(range.clone(), text);
            return;
        }

        self.redo_history.clear();

        // Ops and timestamps follow the caller's descending order, so the
        // recorded history is byte-identical to the sequential calls.
        let base = self.next_timestamp;
        for (k, (range, text)) in edits.iter().enumerate() {
            self.ops.push(BufferOp::Edit {
                old: range.clone(),
                text: (*text).to_owned(),
            });
            self.record_edit(base + k as u64);
        }
        self.next_timestamp = base + edits.len() as u64;

        // The walk reads the old tree, so it runs ascending while the timestamps
        // stay attached to the range they were assigned to.
        //
        // Reversed, so two ranges at one offset are applied last-first. That is
        // what puts the later insert ahead of the earlier, matching where
        // inserting at an offset that already holds new text lands.
        let ascending: Vec<(usize, &(Range<usize>, &str))> =
            edits.iter().enumerate().rev().collect();
        debug_assert!(
            ascending
                .windows(2)
                .all(|w| w[0].1 .0.end <= w[1].1 .0.start),
            "edit_batch takes disjoint ranges sorted descending by start",
        );
        debug_assert!(
            ascending
                .windows(2)
                .all(|w| w[0].1 .0.start != w[1].1 .0.start
                    || w[0].1 .0.is_empty() == w[1].1 .0.is_empty()),
            "an empty range sharing a start with a deleting one is out of contract",
        );

        let cx = &None;
        let mut new_insertions = Vec::new();

        let boundaries = {
            let mut all: Vec<usize> = ascending
                .iter()
                .flat_map(|(_, (range, _))| [range.start, range.end])
                .collect();
            all.sort_unstable();
            all.dedup();
            all
        };

        let old_fragments = split_at_boundaries(
            std::mem::replace(&mut self.snapshot.fragments, SumTree::new(cx)),
            &boundaries,
            &mut new_insertions,
        );

        let mut new_fragments = SumTree::new(cx);
        let mut cursor = old_fragments.cursor::<usize>(cx);
        let mut deleted_rope = DeletedRebuild::new(
            &self.snapshot.visible_text,
            &self.snapshot.deleted_text,
            ascending[0].1 .0.start,
        );

        for (k, (range, text)) in &ascending {
            deleted_rope.skip_to(range.start);
            splice_one_range(
                SpliceRange {
                    range: range.clone(),
                    text,
                    timestamp: base + *k as u64,
                },
                &mut SpliceState {
                    cursor: &mut cursor,
                    new_fragments: &mut new_fragments,
                    new_insertions: &mut new_insertions,
                    deleted_rope: &mut deleted_rope,
                },
            );
        }

        let suffix = cursor.suffix();
        deleted_rope.carry(suffix.summary().text.deleted);
        new_fragments.append(suffix, cx);

        let mut all_insertions = self.snapshot.insertions.clone();
        all_insertions.edit(insertion_edits(new_insertions), ());

        let visible_text = {
            let mut out = Rope::new();
            let mut reader = RopeCursor::new(&self.snapshot.visible_text, 0);
            for (_, (range, text)) in &ascending {
                out.append(reader.slice(range.start));
                out.push(text);
                reader.seek_forward(range.end);
            }
            out.append(reader.suffix());
            out
        };

        let deleted_text = deleted_rope.into_text();

        self.snapshot.visible_text = visible_text;
        self.snapshot.deleted_text = deleted_text;
        self.snapshot.fragments = new_fragments;
        self.snapshot.insertions = all_insertions;
        self.snapshot.version = self.next_timestamp - 1;
        self.recompute_dirty();
    }

    pub fn edit(&mut self, range: Range<usize>, text: &str) {
        self.redo_history.clear();
        self.ops.push(BufferOp::Edit {
            old: range.clone(),
            text: text.to_owned(),
        });
        let timestamp = self.next_timestamp;
        self.next_timestamp += 1;

        let cx = &None;
        let mut new_fragments = SumTree::new(cx);
        let mut new_insertions = Vec::new();
        let old_fragments = std::mem::replace(&mut self.snapshot.fragments, SumTree::new(cx));
        let mut cursor = old_fragments.cursor::<usize>(cx);
        let mut deleted_rope = DeletedRebuild::new(
            &self.snapshot.visible_text,
            &self.snapshot.deleted_text,
            range.start,
        );

        splice_one_range(
            SpliceRange {
                range: range.clone(),
                text,
                timestamp,
            },
            &mut SpliceState {
                cursor: &mut cursor,
                new_fragments: &mut new_fragments,
                new_insertions: &mut new_insertions,
                deleted_rope: &mut deleted_rope,
            },
        );

        // Copy remaining fragments
        let suffix = cursor.suffix();
        deleted_rope.carry(suffix.summary().text.deleted);
        new_fragments.append(suffix, cx);

        // Update insertions tree
        let mut all_insertions = self.snapshot.insertions.clone();
        all_insertions.edit(insertion_edits(new_insertions), ());

        let deleted_text = deleted_rope.into_text();

        // Update the rope
        self.snapshot.visible_text.replace(range, text);

        // Store new state
        self.snapshot.deleted_text = deleted_text;
        self.snapshot.fragments = new_fragments;
        self.snapshot.insertions = all_insertions;
        self.snapshot.version = timestamp;
        self.record_edit(timestamp);
        self.recompute_dirty();
    }
}

/// One range's replacement, as [`splice_one_range`] consumes it.
struct SpliceRange<'a> {
    range: Range<usize>,
    text: &'a str,
    timestamp: u64,
}

/// The fragment-tree surgery in progress, shared across every range of one pass.
///
/// The cursor walks the old tree once, so a batch hands the same state to each
/// range in turn rather than starting over.
struct SpliceState<'a, 'b, 'c, 'd> {
    cursor: &'a mut Cursor<'b, 'c, Fragment, usize>,
    new_fragments: &'a mut SumTree<Fragment>,
    new_insertions: &'a mut Vec<InsertionFragment>,
    deleted_rope: &'a mut DeletedRebuild<'d>,
}

/// Splice one range's replacement into the fragment tree being built, advancing
/// the cursor past what it consumed.
///
/// The cursor must sit at or before `range.start` in the old tree, and the
/// coordinates are the old tree's, which is what lets a caller apply several
/// disjoint ranges against one walk without adjusting any of them.
///
/// The old visible and deleted ropes are reached through the rebuild in
/// `state`, which reads both forward across the whole pass.
fn splice_one_range(edit: SpliceRange<'_>, state: &mut SpliceState<'_, '_, '_, '_>) {
    let SpliceRange {
        range,
        text,
        timestamp,
    } = edit;
    let cx = &None;
    let SpliceState {
        cursor,
        new_fragments,
        new_insertions,
        deleted_rope,
    } = state;

    // Copy all fragments before the edit start
    let prefix = cursor.slice(&range.start, Bias::Right);
    deleted_rope.carry(prefix.summary().text.deleted);
    new_fragments.append(prefix, cx);

    let mut delete_remaining = range.end - range.start;
    let overshoot = cursor.item().map_or(0, |_| range.start - *cursor.start());

    if let Some(fragment) = cursor.item().filter(|f| overshoot > 0 && f.visible) {
        let prefix = Fragment {
            id: Locator::between(last_id(new_fragments, cx), &fragment.id),
            timestamp: fragment.timestamp,
            insertion_offset: fragment.insertion_offset,
            len: overshoot as u32,
            visible: true,
            deletions: fragment.deletions.clone(),
            max_undos: fragment.max_undos,
        };
        push_insertion(new_insertions, &prefix);
        new_fragments.push(prefix, cx);
    }

    // The new text goes ahead of everything this edit deletes, so an anchor
    // inside the replaced range resolves past the replacement rather than
    // before it. Fragments already deleted at this position fall behind it
    // for the same reason. They stand for text that is gone, and the new
    // text takes its place.
    if !text.is_empty() {
        let next_id = cursor.item().map(|f| &f.id).unwrap_or(Locator::max_ref());
        let new_frag_id = Locator::between(last_id(new_fragments, cx), next_id);
        let new_frag = Fragment {
            id: new_frag_id.clone(),
            timestamp,
            insertion_offset: 0,
            len: text.len() as u32,
            visible: true,
            deletions: Default::default(),
            max_undos: 0,
        };
        new_insertions.push(InsertionFragment {
            timestamp,
            split_offset: 0,
            fragment_id: new_frag_id,
        });
        new_fragments.push(new_frag, cx);
    }

    if let Some(fragment) = cursor.item() {
        // A pure insert landing on a fragment's start consumes none of it, so it
        // stays where it is rather than being re-emitted verbatim under a fresh
        // id. Its own id keeps its insertions entry valid, and the new text's id
        // was already chosen to sort ahead of it. Leaving the cursor on it is
        // what lets a later range starting inside it seek forward.
        if fragment.visible && overshoot == 0 && delete_remaining == 0 {
        } else if fragment.visible {
            let fragment_visible_len = fragment.len as usize;
            let remaining_in_fragment = fragment_visible_len - overshoot;
            let to_delete_here = delete_remaining.min(remaining_in_fragment);

            if to_delete_here > 0 {
                let next_id = cursor
                    .next_item()
                    .map(|f| &f.id)
                    .unwrap_or(Locator::max_ref());
                let mut deleted = fragment.clone();
                deleted.id = Locator::between(last_id(new_fragments, cx), next_id);
                deleted.insertion_offset = fragment.insertion_offset + overshoot as u32;
                deleted.len = to_delete_here as u32;
                deleted.visible = false;
                deleted.deletions.push(timestamp);
                push_insertion(new_insertions, &deleted);
                new_fragments.push(deleted, cx);
                deleted_rope.take(to_delete_here);
                delete_remaining -= to_delete_here;
            }

            let suffix_len = remaining_in_fragment.saturating_sub(to_delete_here);
            if suffix_len > 0 && delete_remaining == 0 {
                let next_id = cursor
                    .next_item()
                    .map(|f| &f.id)
                    .unwrap_or(Locator::max_ref());
                let suffix_id = Locator::between(last_id(new_fragments, cx), next_id);
                let suffix = Fragment {
                    id: suffix_id.clone(),
                    timestamp: fragment.timestamp,
                    insertion_offset: fragment.insertion_offset
                        + overshoot as u32
                        + to_delete_here as u32,
                    len: suffix_len as u32,
                    visible: true,
                    deletions: fragment.deletions.clone(),
                    max_undos: fragment.max_undos,
                };
                new_insertions.push(InsertionFragment {
                    timestamp: suffix.timestamp,
                    split_offset: suffix.insertion_offset,
                    fragment_id: suffix_id,
                });
                new_fragments.push(suffix, cx);
            }

            cursor.next();
        } else {
            deleted_rope.carry(fragment.len as usize);
            new_fragments.push(fragment.clone(), cx);
            cursor.next();
        }
    }

    // Continue deleting through subsequent fragments
    while delete_remaining > 0 {
        match cursor.item() {
            Some(fragment) if fragment.visible => {
                let frag_len = fragment.len as usize;
                if frag_len <= delete_remaining {
                    let mut deleted = fragment.clone();
                    deleted.visible = false;
                    deleted.deletions.push(timestamp);
                    new_fragments.push(deleted, cx);
                    deleted_rope.take(frag_len);
                    delete_remaining -= frag_len;
                    cursor.next();
                } else {
                    let mut deleted_part = fragment.clone();
                    deleted_part.id = Locator::between(last_id(new_fragments, cx), &fragment.id);
                    deleted_part.len = delete_remaining as u32;
                    deleted_part.visible = false;
                    deleted_part.deletions.push(timestamp);
                    push_insertion(new_insertions, &deleted_part);
                    new_fragments.push(deleted_part, cx);
                    deleted_rope.take(delete_remaining);

                    let next_id = cursor
                        .next_item()
                        .map(|f| &f.id)
                        .unwrap_or(Locator::max_ref());
                    let remaining_id = Locator::between(last_id(new_fragments, cx), next_id);
                    let remaining = Fragment {
                        id: remaining_id.clone(),
                        timestamp: fragment.timestamp,
                        insertion_offset: fragment.insertion_offset + delete_remaining as u32,
                        len: (frag_len - delete_remaining) as u32,
                        visible: true,
                        deletions: fragment.deletions.clone(),
                        max_undos: fragment.max_undos,
                    };
                    new_insertions.push(InsertionFragment {
                        timestamp: remaining.timestamp,
                        split_offset: remaining.insertion_offset,
                        fragment_id: remaining_id,
                    });
                    new_fragments.push(remaining, cx);

                    delete_remaining = 0;
                    cursor.next();
                }
            },
            Some(fragment) => {
                deleted_rope.carry(fragment.len as usize);
                new_fragments.push(fragment.clone(), cx);
                cursor.next();
            },
            None => break,
        }
    }
}

/// Rebuild `old` so that no visible offset in `boundaries` falls strictly
/// inside a fragment.
///
/// [`splice_one_range`] consumes whole fragments, emitting an untouched tail as
/// a new fragment and advancing past it. That is fine for one range and fatal
/// for several sharing a walk, because a later range landing in that tail would
/// need the cursor to seek backward. Cutting every boundary ahead of time means each
/// range starts and ends on a fragment edge, so the tail branches never fire and
/// every seek is forward.
///
/// Splitting a fragment leaves the right half its original id and gives the left
/// half a fresh one ordered before it, so document order is preserved and the id
/// an anchor already resolved to still names text at the same place. Both halves
/// re-record their insertions entry, since the key is `(timestamp, split_offset)`
/// and the left half's offset is unchanged while the right half's has moved.
///
/// Only visible fragments can hold a boundary strictly inside, since boundaries
/// are visible offsets, so the deleted rope and the visible rope are untouched.
fn split_at_boundaries(
    old: SumTree<Fragment>,
    boundaries: &[usize],
    new_insertions: &mut Vec<InsertionFragment>,
) -> SumTree<Fragment> {
    let cx = &None;
    let mut new_fragments = SumTree::new(cx);
    let mut cursor = old.cursor::<usize>(cx);

    // A fragment can hold several boundaries, which is the ordinary case for
    // carets in a freshly opened buffer, so the right half of a split is held
    // back rather than pushed. The cursor has already moved past it, so only
    // this can offer it to the next boundary.
    let mut pending: Option<(Fragment, usize)> = None;

    for &boundary in boundaries {
        if let Some((held, start)) = pending.take() {
            let end = start + held.len as usize;
            if boundary > start && boundary < end {
                let left_len = boundary - start;
                let mut right = held.clone();
                right.insertion_offset += left_len as u32;
                right.len -= left_len as u32;

                let mut left = held;
                left.id = Locator::between(last_id(&new_fragments, cx), &right.id);
                left.len = left_len as u32;
                push_insertion(new_insertions, &left);
                new_fragments.push(left, cx);

                pending = Some((right, boundary));
                continue;
            }

            push_insertion(new_insertions, &held);
            new_fragments.push(held, cx);
        }

        new_fragments.append(cursor.slice(&boundary, Bias::Right), cx);

        let Some(fragment) = cursor.item() else {
            continue;
        };
        let start = *cursor.start();
        if start >= boundary || !fragment.visible {
            continue;
        }

        let left_len = boundary - start;
        let mut right = fragment.clone();
        right.insertion_offset += left_len as u32;
        right.len -= left_len as u32;

        let mut left = fragment.clone();
        left.id = Locator::between(last_id(&new_fragments, cx), &right.id);
        left.len = left_len as u32;
        push_insertion(new_insertions, &left);
        new_fragments.push(left, cx);

        pending = Some((right, boundary));
        cursor.next();
    }

    if let Some((held, _)) = pending {
        push_insertion(new_insertions, &held);
        new_fragments.push(held, cx);
    }

    new_fragments.append(cursor.suffix(), cx);
    new_fragments
}

impl TextBuffer {
    /// Record `timestamp` in the open group, or as its own singleton group when
    /// no group is open (the from_history replay and any unwrapped edit).
    fn record_edit(&mut self, timestamp: u64) {
        if self.open_group {
            if self.open_group_started
                && let Some(group) = self.edit_history.last_mut()
            {
                group.edits.push(timestamp);
                return;
            }
            self.edit_history.push(UndoGroup {
                edits: vec![timestamp],
                selections_before: std::mem::take(&mut self.open_group_before),
                selections_after: Arc::from([]),
            });
            self.open_group_started = true;
        } else {
            self.edit_history.push(UndoGroup {
                edits: vec![timestamp],
                selections_before: Arc::from([]),
                selections_after: Arc::from([]),
            });
        }
    }

    /// Open an undo group so the following [`Self::edit`] calls collapse into one
    /// logical step. `selections_before` is the editor selection set to restore
    /// when the group is later undone.
    ///
    /// The group is not materialized until its first edit, so opening one around
    /// a non-editing action costs nothing and leaves the undo history unchanged.
    pub(crate) fn begin_group(&mut self, selections_before: Arc<[Selection<Anchor>]>) {
        if self.open_group {
            self.seal_group(Arc::from([]));
        }
        self.open_group = true;
        self.open_group_started = false;
        self.open_group_before = selections_before;
    }

    /// Open an undo group only when none is already open, returning whether it
    /// did.
    ///
    /// Unlike [`Self::begin_group`], an already-open group is left untouched
    /// rather than sealed, so a mid-session action's edits join the enclosing
    /// insert session's single undo step instead of splitting it. Returns
    /// `false` when a group was already open, `true` when a fresh one opened.
    ///
    /// `selections_before` is called only when a group actually opens. Refusing
    /// is the common case during an insert session, where every typed character
    /// asks, so a caller that gathered the selections first would be building a
    /// list per keystroke for nobody.
    pub(crate) fn try_begin_group(
        &mut self,
        selections_before: impl FnOnce() -> Arc<[Selection<Anchor>]>,
    ) -> bool {
        if self.open_group {
            return false;
        }
        self.open_group = true;
        self.open_group_started = false;
        self.open_group_before = selections_before();
        true
    }

    /// Whether an undo group is open, so a caller that seals one can tell
    /// whether there was anything to seal.
    pub(crate) fn group_open(&self) -> bool {
        self.open_group
    }

    /// Whether the open group has taken an edit yet.
    ///
    /// A group that has not is discarded on sealing, so the selections a caller
    /// would pass to [`Self::seal_group`] are never read. Gathering them costs a
    /// copy of the whole selection set, and most actions edit nothing, so a
    /// caller asks this first.
    pub(crate) fn group_started(&self) -> bool {
        self.open_group_started
    }

    /// Close the open undo group, recording `selections_after` to restore on
    /// redo. A group that took no edits was never materialized, so a non-editing
    /// action leaves no undo step behind.
    pub(crate) fn seal_group(&mut self, selections_after: Arc<[Selection<Anchor>]>) {
        if !self.open_group {
            return;
        }
        self.open_group = false;
        self.open_group_before = Arc::from([]);
        if self.open_group_started {
            self.open_group_started = false;
            if let Some(group) = self.edit_history.last_mut() {
                group.selections_after = selections_after;
            }
        }
    }

    /// Timestamp of the most recent edit, skipping a transiently empty open
    /// group. `None` when nothing has been edited.
    fn frontier(&self) -> Option<u64> {
        self.edit_history
            .iter()
            .rev()
            .find_map(|group| group.edits.last())
            .copied()
    }

    /// Record the current edit frontier and visible text as the clean baseline,
    /// marking the buffer unmodified. Call at every clean point (save, seeded
    /// content) so a later undo/redo back to this frontier -- or any edit that
    /// restores the saved bytes -- clears [`Self::dirty`] again.
    pub(crate) fn mark_clean(&mut self) {
        // A marker taken mid-group names a frontier undo can never land on,
        // since undo pops whole groups, so the nearest undo steps past the
        // saved state to before the group began. Sealing makes the save point a
        // boundary, which a live buffer only needs for that reason and a
        // restored one needs to read clean at all, having no saved bytes to
        // compare against.
        //
        // Sealed with no selections, as `undo`, `redo` and `begin_group` all do
        // at this level. The selections a group restores come from the caller,
        // and a save has none to offer.
        self.seal_group(Arc::from([]));
        self.saved_marker = self.frontier();
        self.saved_text = Some(self.snapshot.visible_text.clone());
        // The version the saved bytes belong to, which is not the marker above.
        // That names the last edit, while an undo carries the snapshot past it
        // onto a timestamp of its own, and it is the snapshot's text being
        // captured here.
        self.saved_version = Some(self.snapshot.version);
        self.dirty = false;
    }

    /// Modified when the edit frontier has moved off [`Self::saved_marker`] and
    /// the visible text differs from the saved bytes. The content check clears
    /// dirty for a round-trip edit -- type a char then delete it, or undo to a
    /// diverged frontier -- that the frontier comparison alone reports modified.
    fn recompute_dirty(&mut self) {
        self.dirty = self.frontier() != self.saved_marker && !self.matches_saved_text();
    }

    /// Whether the visible text is byte-identical to the content captured at the
    /// last [`Self::mark_clean`]. Always false before the first clean point,
    /// when no saved text exists.
    ///
    /// Compares each rope's counts first, which is O(1) and carries the line,
    /// character, and UTF-16 counts alongside the byte length. Two texts that
    /// differ almost always differ in one of those, and that is enough to
    /// answer, which matters because this runs after every edit and the walk
    /// below is O(file).
    ///
    /// Only the counts, not the whole summary. A summary also names the longest
    /// row, which is a fact about the text but is computed by whichever of two
    /// paths the chunking selects, so a field there is one tie-break away from
    /// calling identical text different. Wrongly reporting a saved buffer dirty
    /// is not recoverable by the reader, and the counts alone already reject
    /// almost everything.
    ///
    /// Equal counts do not mean equal text, so a byte comparison still decides
    /// those. The counts agree on exactly the edits that preserve them, a case
    /// toggle or a replacement of like for like, which is why that comparison
    /// reads only the spans that changed rather than both ropes end to end.
    ///
    /// Which spans those are comes from the edits recorded since the version the
    /// saved text was captured at. Everything outside them is unchanged since
    /// then, so it already matches, and with the lengths equal from the counts
    /// above the changed spans settle the rest.
    fn matches_saved_text(&self) -> bool {
        let Some(saved) = &self.saved_text else {
            return false;
        };
        let current = &self.snapshot.visible_text;
        let (saved_summary, current_summary) = (saved.summary(), current.summary());
        if saved_summary.len != current_summary.len
            || saved_summary.len_utf16 != current_summary.len_utf16
            || saved_summary.lines != current_summary.lines
            || saved_summary.chars != current_summary.chars
        {
            return false;
        }

        let Some(saved_version) = self.saved_version else {
            return chunk_streams_match(saved.chunks(), current.chunks());
        };

        let patch = self.snapshot.edits_since(saved_version);
        let edits = patch.edits();
        let (Some(first), Some(last)) = (edits.first(), edits.last()) else {
            return true;
        };

        // Each edit against its own counterpart, while every one of them holds
        // its length. Then each pair lines up on its own and the walk reads
        // only what moved.
        if edits.iter().all(|edit| edit.old.len() == edit.new.len()) {
            return edits.iter().all(|edit| {
                chunk_streams_match(
                    saved.chunks_in_range(edit.old.clone()),
                    current.chunks_in_range(edit.new.clone()),
                )
            });
        }

        // An edit that shifts lengths moves everything after it, so its
        // neighbours no longer sit at the same offsets in both texts and cannot
        // be paired off. One window spanning the whole set restores the
        // alignment. The patch bounds every change, so the text before the
        // first edit is untouched and the two windows start together. The text
        // after the last is untouched too, and the counts above already made
        // the totals equal, so they end together as well.
        chunk_streams_match(
            saved.chunks_in_range(first.old.start..last.old.end),
            current.chunks_in_range(first.new.start..last.new.end),
        )
    }

    /// Undo the top edit group, reverting all of its edits as one step. Returns
    /// the editor selections captured when the group opened, to restore the
    /// cursor to edit time, or `None` when there is nothing to undo.
    ///
    /// The content a buffer was loaded or seeded with is not an undo target, so
    /// undoing a freshly opened file with no user edits returns `None` and
    /// leaves the file intact rather than emptying it.
    pub fn undo(&mut self) -> Option<Arc<[Selection<Anchor>]>> {
        // An open group names the top of the history, and undoing moves that
        // top. Leaving it open would hand the next edit to whichever group the
        // pop exposed, so the group is closed against the history it was
        // opened over. The selections are the caller's to supply on redo, and
        // undo answers with the group's own `selections_before`, so an empty
        // set here is not a value anything reads back.
        self.seal_group(Arc::from([]));

        if self.edit_history.len() <= self.undo_floor {
            return None;
        }
        let group = self.edit_history.pop()?;
        self.apply_undo_toggles(group.edits.iter().rev().copied(), BufferOp::Undo);
        let selections = group.selections_before.clone();
        self.redo_history.push(group);
        self.recompute_dirty();
        Some(selections)
    }

    /// Redo the top undone group, reapplying all of its edits as one step.
    /// Returns the editor selections captured when the group sealed, or `None`
    /// when there is nothing to redo.
    pub fn redo(&mut self) -> Option<Arc<[Selection<Anchor>]>> {
        // Symmetry with [`Self::undo`], and unreachable rather than load-bearing:
        // every edit path clears the redo history first, so a group that has
        // taken an edit and a group waiting to be redone cannot both exist. The
        // seal costs nothing and keeps the two entry points from having to be
        // reasoned about separately.
        self.seal_group(Arc::from([]));

        let group = self.redo_history.pop()?;
        self.apply_undo_toggles(group.edits.iter().copied(), BufferOp::Redo);
        let selections = group.selections_after.clone();
        self.edit_history.push(group);
        self.recompute_dirty();
        Some(selections)
    }

    /// Place a named marker at the current op-log position. The returned
    /// [`CheckpointId`] is the navigation target consumed by checkpoint
    /// navigation actions; pass [`None`] for `label` for unlabeled markers
    /// (the default `commit_undo_checkpoint` behavior).
    pub fn checkpoint(&mut self, label: Option<String>) -> CheckpointId {
        let id = CheckpointId(self.next_checkpoint_id);
        self.next_checkpoint_id += 1;
        self.checkpoints.push(Checkpoint {
            id,
            op_index: self.ops.len(),
            label,
        });
        id
    }

    pub fn checkpoints(&self) -> &[Checkpoint] {
        &self.checkpoints
    }

    /// Toggle every edit in `timestamps` between undone and applied, rebuilding
    /// the buffer once for the whole batch.
    ///
    /// Each timestamp still gets its own op-log entry, its own undo timestamp,
    /// and its own [`UndoMap`] entry. [`Self::from_history`] replays each
    /// [`BufferOp::Undo`] as a singleton-group undo, so the op stream has to
    /// stay one entry per edit for a restored buffer to match, and the frontier
    /// and dirty tracking read the timestamp stream. The expensive half is what
    /// stops repeating. The fragment walk and the two rope rebuilds run once
    /// against the finished undo map instead of once per edit, so undoing a
    /// typing run is one pass over the buffer rather than one per typed
    /// character.
    ///
    /// That pass then only visits what the batch can reach. A fragment's
    /// visibility turns on whether its own insertion or one of its deletions was
    /// toggled, and a subtree's `max_version` is the newest of exactly those
    /// timestamps, so a subtree older than everything the batch touched holds
    /// nothing that can change and is copied through whole. The runs between
    /// what does change keep their `visible` flags, so their bytes stay in the
    /// rope already holding them and move across as one slice per side rather
    /// than a fragment at a time. Undoing recent edits therefore costs the spans
    /// they cover. Undoing the oldest edit in the buffer still costs the whole
    /// buffer, which is the honest bound on the pruning.
    ///
    /// A fragment the batch toggled is stamped with the batch's last undo
    /// timestamp rather than the one that flipped it. The stamp only has to
    /// exceed any `since_version` a caller can hold, and no snapshot is taken
    /// between the toggles of one call, so no version inside the batch is
    /// observable. A fragment that flips and flips back within the batch, which
    /// one group inserting and then deleting the same text produces, ends
    /// unstamped because its net visibility never changed.
    fn apply_undo_toggles(&mut self, timestamps: impl Iterator<Item = u64>, op: BufferOp) {
        let mut last_undo_timestamp = None;
        let mut oldest_toggled = u64::MAX;
        for edit_timestamp in timestamps {
            self.ops.push(op.clone());
            let undo_timestamp = self.next_timestamp;
            self.next_timestamp += 1;
            last_undo_timestamp = Some(undo_timestamp);
            oldest_toggled = oldest_toggled.min(edit_timestamp);

            let new_count = self.snapshot.undo_map.undo_count(edit_timestamp) + 1;
            self.snapshot.undo_map.insert(&UndoOperation {
                timestamp: undo_timestamp,
                counts: HashMap::from([(edit_timestamp, new_count)]),
            });
        }

        // An empty group never materialized, so there is nothing to rebuild and
        // no timestamp to stamp the result with.
        let Some(undo_timestamp) = last_undo_timestamp else {
            return;
        };

        let cx = &None;
        let old_fragments = std::mem::replace(&mut self.snapshot.fragments, SumTree::new(cx));
        let mut new_fragments = SumTree::new(cx);
        let mut new_visible = Rope::new();
        let mut new_deleted = Rope::new();

        // Both ropes are read strictly forward. The walk crosses the old
        // fragments in order, and each fragment claims the next stretch of
        // whichever rope it currently lives in, so each rope gets one reader
        // rather than a descent from the root per fragment and per run between
        // them.
        let mut old_visible = RopeCursor::new(&self.snapshot.visible_text, 0);
        let mut old_deleted = RopeCursor::new(&self.snapshot.deleted_text, 0);

        let mut untouched = old_fragments.cursor::<Option<&Locator>>(cx);
        let mut reachable =
            old_fragments.filter::<_, ()>(cx, |summary| summary.max_version >= oldest_toggled);
        reachable.next();

        while let Some(fragment) = reachable.item() {
            let run = untouched.slice(&Some(&fragment.id), Bias::Left);
            let run_visible = run.summary().text.visible;
            let run_deleted = run.summary().text.deleted;

            let carried_visible = old_visible.offset() + run_visible;
            new_visible.append(old_visible.slice(carried_visible));
            let carried_deleted = old_deleted.offset() + run_deleted;
            new_deleted.append(old_deleted.slice(carried_deleted));
            new_fragments.append(run, cx);

            let len = fragment.len as usize;
            let was_visible = fragment.visible;
            let is_visible = fragment.is_visible_with_undos(&self.snapshot.undo_map);

            let text = if was_visible {
                let end = old_visible.offset() + len;
                old_visible.slice(end)
            } else {
                let end = old_deleted.offset() + len;
                old_deleted.slice(end)
            };

            if is_visible {
                new_visible.append(text);
            } else {
                new_deleted.append(text);
            }

            let mut new_frag = fragment.clone();
            new_frag.visible = is_visible;
            if was_visible != is_visible {
                new_frag.max_undos = undo_timestamp;
            }
            new_fragments.push(new_frag, cx);

            untouched.next();
            reachable.next();
        }

        // Consuming both readers here is also what ends their borrows, which
        // the writes below could not happen under.
        new_visible.append(old_visible.suffix());
        new_deleted.append(old_deleted.suffix());
        new_fragments.append(untouched.suffix(), cx);

        self.snapshot.fragments = new_fragments;
        self.snapshot.visible_text = new_visible;
        self.snapshot.deleted_text = new_deleted;
        self.snapshot.version = undo_timestamp;
    }

    pub fn anchor_at(&self, offset: usize, bias: Bias) -> Anchor {
        self.snapshot.anchor_at(offset, bias)
    }

    pub fn resolve_anchor(&self, anchor: &Anchor) -> usize {
        self.snapshot.resolve_anchor(anchor)
    }

    pub fn point_for_anchor(&self, anchor: &Anchor) -> Point {
        self.snapshot.point_for_anchor(anchor)
    }

    pub fn line_count(&self) -> u32 {
        self.snapshot.visible_text.max_point().row + 1
    }

    pub fn rope(&self) -> &Rope {
        &self.snapshot.visible_text
    }

    pub fn version(&self) -> u64 {
        self.snapshot.version
    }

    pub fn buffer_id(&self) -> BufferId {
        self.buffer_id
    }

    /// Snapshot the op log and clean-frontier marker for persistence. Replay
    /// the result with [`Self::from_history`] to reconstruct the buffer.
    ///
    /// A log longer than [`OPS_COMPACT_THRESHOLD`] is replaced by a single seed
    /// edit carrying the current text, which bounds what a long session writes
    /// to disk and replays on reopen. The reconstruction is then faithful in
    /// text and dirty state but not in history. The seed is the oldest state
    /// reachable, so undo stops there and redo has nothing to offer.
    ///
    /// A seed also rebuilds the fragment tree from scratch, which invalidates
    /// anchors taken against this buffer. Callers holding any must re-express
    /// them, and [`BufferHistory::compacted`] reports when that is needed.
    pub fn history(&self) -> BufferHistory {
        if self.ops.len() > OPS_COMPACT_THRESHOLD {
            return self.compacted_history();
        }

        let mut ops = self.ops.clone();
        if let Some(seed) = &self.seed_text {
            // Materialized only here, so what persists is the same log as ever
            // and a restored buffer replays the load the way it was written.
            let Some(BufferOp::Edit { text, .. }) = ops.first_mut() else {
                unreachable!("a seeded log opens with the edit that loaded it")
            };
            *text = seed.to_string();
        }

        BufferHistory {
            ops,
            saved_marker: self.saved_marker,
            undo_floor: self.undo_floor,
            compacted: false,
        }
    }

    /// The current text as a one-edit log, protected from undo by the same
    /// floor [`Self::with_text`] gives a freshly loaded file.
    ///
    /// Replay reaches the seed's frontier at [`SEED_TIMESTAMP`], so naming that
    /// as the clean marker restores a clean buffer clean. A buffer with unsaved
    /// changes gets no marker at all, leaving the restored frontier diverged and
    /// the buffer modified, so the changes still prompt on quit.
    fn compacted_history(&self) -> BufferHistory {
        BufferHistory {
            ops: vec![BufferOp::Edit {
                old: 0..0,
                text: self.snapshot.visible_text.to_string(),
            }],
            saved_marker: (!self.dirty).then_some(SEED_TIMESTAMP),
            undo_floor: 1,
            compacted: true,
        }
    }

    /// Reconstruct a [`TextBuffer`] by replaying `history` on a fresh buffer.
    /// Sequential timestamp assignment means anchors from the original buffer
    /// resolve to identical byte offsets in the reconstructed one.
    pub fn from_history(buffer_id: BufferId, history: &BufferHistory) -> Self {
        let mut buf = Self::new(buffer_id);
        for op in &history.ops {
            match op {
                BufferOp::Edit { old, text } => buf.edit(old.clone(), text),
                BufferOp::Undo => {
                    buf.undo();
                },
                BufferOp::Redo => {
                    buf.redo();
                },
            }
        }
        buf.saved_marker = history.saved_marker;
        buf.undo_floor = history.undo_floor;
        buf.recompute_dirty();
        buf.detect_indent_style();
        buf
    }
}

impl Default for TextBuffer {
    fn default() -> Self {
        Self::new(BufferId::new(0))
    }
}

/// The id of the tree's last fragment, or [`Locator::min`] when it holds none.
///
/// Read off the root summary rather than walked down to. `FragmentSummary`
/// assigns `max_id` as summaries fold in rather than taking a maximum of them,
/// so it holds the last fragment's id, and an empty tree carries the `zero`
/// summary's `Locator::min`. Every emitted fragment asks for this to place the
/// next one, so a descent here would double the descents a splice makes.
fn last_id<'a>(tree: &'a SumTree<Fragment>, _cx: &Option<u64>) -> &'a Locator {
    &tree.summary().max_id
}

/// Builds the deleted rope for one edit, in document order.
///
/// The deleted rope holds the bytes of every non-visible fragment concatenated
/// in the order those fragments appear in the tree, which is what lets undo
/// find a fragment's bytes by counting the deleted lengths ahead of it. So an
/// edit cannot simply append what it deletes. Bytes deleted later can belong
/// earlier in the document than bytes deleted before them, so appending would
/// hand undo some other deletion's bytes.
///
/// Bytes reach the new rope from two places, each read in ascending order, so
/// each side needs only a reader that moves forward. Text already deleted comes
/// from the old deleted rope, and text this edit deletes is still in the
/// visible one.
struct DeletedRebuild<'a> {
    text: Rope,
    /// Reader over the text that was deleted before this edit and stays so.
    already_deleted: RopeCursor<'a>,
    /// Reader over the visible text this edit is taking out.
    ///
    /// Both readers only ever move forward, which is what lets them be cursors
    /// rather than a pair of offsets sliced from the root. The walk crosses the
    /// old fragments in order, and each piece of either rope is claimed by the
    /// one fragment covering it.
    being_deleted: RopeCursor<'a>,
}

impl<'a> DeletedRebuild<'a> {
    /// `range_start` is where in the visible rope this edit starts deleting.
    fn new(old_visible: &'a Rope, old_deleted: &'a Rope, range_start: usize) -> Self {
        Self {
            text: Rope::new(),
            already_deleted: RopeCursor::new(old_deleted, 0),
            being_deleted: RopeCursor::new(old_visible, range_start),
        }
    }

    /// Carry over `len` bytes of text that was already deleted and stays so.
    fn carry(&mut self, len: usize) {
        let end = self.already_deleted.offset() + len;
        let carried = self.already_deleted.slice(end);
        self.text.append(carried);
    }

    /// Take `len` bytes that this edit is deleting out of the visible rope.
    fn take(&mut self, len: usize) {
        let end = self.being_deleted.offset() + len;
        let taken = self.being_deleted.slice(end);
        self.text.append(taken);
    }

    /// Move the read position to where the next range starts deleting.
    ///
    /// One pass over several disjoint ranges shares a rebuild, and the bytes
    /// between two of them are never deleted, so nothing reads them. Only a
    /// forward move is meaningful. The ranges arrive ascending, so a backward
    /// one would mean they overlap.
    fn skip_to(&mut self, offset: usize) {
        debug_assert!(
            offset >= self.being_deleted.offset(),
            "ranges are applied ascending, so the delete position only moves forward",
        );
        self.being_deleted.seek_forward(offset);
    }

    /// The rebuilt deleted rope, releasing the borrows on the old ones.
    ///
    /// A caller writes the result back over the ropes this read from, which the
    /// readers cannot be alive for.
    fn into_text(self) -> Rope {
        self.text
    }
}

/// One edit's insertion records, reduced to the last under each key and wrapped
/// for [`SumTree::edit`].
///
/// Applying the records one at a time replaced an earlier record with a later
/// one under the same key, and the batch has to land on the same tree.
/// [`SumTree::edit`] replaces a same-key entry already in the tree but buffers
/// and pushes every record inside one batch, so a duplicate left in would put
/// two entries under one key. Nothing downstream would report that. The lookups
/// answer from whichever entry the walk meets first, and which one that is
/// depends on where the tree happened to split.
fn insertion_edits(mut records: Vec<InsertionFragment>) -> Vec<TreeEdit<InsertionFragment>> {
    // Stable, so records under one key stay in the order they were emitted and
    // the last of a run is the one that was applied last.
    records.sort_by_key(|record| record.key());

    let mut edits = Vec::with_capacity(records.len());
    let mut records = records.into_iter().peekable();
    while let Some(record) = records.next() {
        if records
            .peek()
            .is_some_and(|next| next.key() == record.key())
        {
            continue;
        }
        edits.push(TreeEdit::Insert(record));
    }
    edits
}

fn push_insertion(insertions: &mut Vec<InsertionFragment>, fragment: &Fragment) {
    insertions.push(InsertionFragment {
        timestamp: fragment.timestamp,
        split_offset: fragment.insertion_offset,
        fragment_id: fragment.id.clone(),
    });
}

/// Whether two chunk streams spell out the same bytes.
///
/// Two ropes holding one text need not be chunked alike, so the chunks cannot
/// be paired off. The walk compares the overlapping prefix of whichever two are
/// current and advances the one that runs out, which hands each comparison to
/// memcmp. Reading a byte at a time instead pushes every byte of the file
/// through two nested iterator state machines.
fn chunk_streams_match<'a>(
    mut left: impl Iterator<Item = &'a str>,
    mut right: impl Iterator<Item = &'a str>,
) -> bool {
    let mut l: &[u8] = &[];
    let mut r: &[u8] = &[];

    loop {
        if l.is_empty() {
            match left.next() {
                Some(chunk) => l = chunk.as_bytes(),
                None => break,
            }
            continue;
        }
        if r.is_empty() {
            match right.next() {
                Some(chunk) => r = chunk.as_bytes(),
                None => break,
            }
            continue;
        }

        let common = l.len().min(r.len());
        if l[..common] != r[..common] {
            return false;
        }
        l = &l[common..];
        r = &r[common..];
    }

    // One stream is spent, so the texts are equal only if the other is too.
    // Counted rather than probed for a next chunk, since a stream may yield
    // empty ones.
    let left_rest = l.len() + left.map(str::len).sum::<usize>();
    let right_rest = r.len() + right.map(str::len).sum::<usize>();
    left_rest == right_rest
}

impl TextBufferSnapshot {
    pub fn empty() -> Self {
        TextBuffer::new(BufferId::new(0)).snapshot
    }

    pub fn anchor_at(&self, offset: usize, bias: Bias) -> Anchor {
        let offset = offset.min(self.visible_text.len());

        if bias == Bias::Left && offset == 0 {
            return Anchor::min_for_buffer(self.buffer_id);
        }
        if bias == Bias::Right && offset == self.visible_text.len() {
            return Anchor::max_for_buffer(self.buffer_id);
        }

        let cx = &None;
        let (start, _end, item) = self.fragments.find::<usize, _>(cx, &offset, bias);

        match item {
            Some(fragment) if fragment.visible => {
                let overshoot = offset - start;
                Anchor {
                    timestamp: fragment.timestamp,
                    offset: fragment.insertion_offset + overshoot as u32,
                    bias,
                    buffer_id: Some(self.buffer_id),
                }
            },
            _ => {
                if bias == Bias::Left {
                    Anchor::min_for_buffer(self.buffer_id)
                } else {
                    Anchor::max_for_buffer(self.buffer_id)
                }
            },
        }
    }

    /// Anchor a batch of byte offsets in one forward pass over the fragment tree.
    ///
    /// Equivalent to [`Self::anchor_at`] on each offset with the same `bias`, but
    /// advances a single cursor over the offsets sorted ascending rather than
    /// seeking from the root per offset, so a whole token stream costs O(file)
    /// instead of O(tokens * log n). Results are returned in the input order.
    /// Callers pass one bias per batch (token starts use [`Bias::Right`], ends
    /// [`Bias::Left`]).
    pub fn anchors_at_batch(&self, offsets: &[usize], bias: Bias) -> Vec<Anchor> {
        let len = self.visible_text.len();
        // Input already in that order is walked where it stands, which is what
        // every caller anchoring a sorted set hands over and what a batch of one
        // is for free. Only input that is actually out of order pays for a
        // permutation. Reading `is_sorted` off the offsets rather than the
        // clamped ones is enough, since clamping to the length is monotonic.
        let ascending_order: Option<Vec<usize>> = (!offsets.is_sorted()).then(|| {
            let mut order: Vec<usize> = (0..offsets.len()).collect();
            order.sort_unstable_by_key(|&i| offsets[i]);
            order
        });

        let mut results = vec![Anchor::min_for_buffer(self.buffer_id); offsets.len()];
        let cx = &None;
        let mut cursor = self.fragments.cursor::<usize>(cx);
        for step in 0..offsets.len() {
            let original_idx = match &ascending_order {
                Some(order) => order[step],
                None => step,
            };
            let offset = offsets[original_idx].min(len);
            results[original_idx] = if bias == Bias::Left && offset == 0 {
                Anchor::min_for_buffer(self.buffer_id)
            } else if bias == Bias::Right && offset == len {
                Anchor::max_for_buffer(self.buffer_id)
            } else {
                cursor.seek_forward(&offset, bias);
                let start = *cursor.start();
                match cursor.item() {
                    Some(fragment) if fragment.visible => Anchor {
                        timestamp: fragment.timestamp,
                        offset: fragment.insertion_offset + (offset - start) as u32,
                        bias,
                        buffer_id: Some(self.buffer_id),
                    },
                    _ if bias == Bias::Left => Anchor::min_for_buffer(self.buffer_id),
                    _ => Anchor::max_for_buffer(self.buffer_id),
                }
            };
        }
        results
    }

    pub fn resolve_anchor(&self, anchor: &Anchor) -> usize {
        if anchor.is_min() {
            return 0;
        }
        if anchor.is_max() {
            return self.visible_text.len();
        }

        let (fragment, base_offset) = self.find_fragment_for_anchor(anchor);
        match fragment {
            Some(f) if f.visible => {
                let overshoot = anchor.offset.saturating_sub(f.insertion_offset);
                base_offset + overshoot as usize
            },
            _ => base_offset,
        }
    }

    /// Order two anchors by position, permanently.
    ///
    /// Anchors compare by the identity of the fragment holding them rather than
    /// by the offset they currently resolve to. A deletion collapses every
    /// anchor in the deleted span onto one offset, so an offset comparison
    /// reports them equal and lets the bias tiebreak invert a range that was
    /// well formed. Fragment locators order by document position and are never
    /// renumbered, so the order they give outlives the text going invisible.
    ///
    /// Anchors sharing an insertion skip the locator entirely. An insertion's
    /// bytes keep their relative order however the fragment tree later splits
    /// them, so the offset within it already decides.
    pub fn cmp_anchors(&self, a: &Anchor, b: &Anchor) -> Ordering {
        let fragments = if a.timestamp == b.timestamp {
            Ordering::Equal
        } else {
            self.fragment_id_for_anchor(a)
                .cmp(self.fragment_id_for_anchor(b))
        };

        fragments
            .then_with(|| a.offset.cmp(&b.offset))
            .then_with(|| a.bias.cmp(&b.bias))
    }

    /// The locator of the fragment an anchor sits in, which is its position in
    /// the document expressed so that edits elsewhere cannot renumber it.
    ///
    /// Only the max sentinel needs answering here. Its key runs off the end of
    /// the insertion tree, where the fallback is the newest insertion, and the
    /// newest insertion is not the last fragment in the document. The min
    /// sentinel needs no branch of its own. Its key sorts below every insertion,
    /// so the lookup finds no predecessor and already answers
    /// [`Locator::min_ref`].
    fn fragment_id_for_anchor(&self, anchor: &Anchor) -> &Locator {
        if anchor.is_max() {
            return Locator::max_ref();
        }
        self.insertion_fragment_id(anchor)
    }

    /// The fragment an anchor's insertion offset falls in, looked up through the
    /// insertion tree. Sentinel anchors have no insertion, so callers reaching
    /// this directly answer for them first.
    fn insertion_fragment_id(&self, anchor: &Anchor) -> &Locator {
        let key = InsertionFragmentKey {
            timestamp: anchor.timestamp,
            split_offset: anchor.offset,
        };

        let (_start, _end, result) =
            self.insertions
                .find_with_prev::<InsertionFragmentKey, _>((), &key, anchor.bias);

        match result {
            Some((prev, insertion)) => {
                let ins_key = InsertionFragmentKey {
                    timestamp: insertion.timestamp,
                    split_offset: insertion.split_offset,
                };
                if ins_key > key
                    || (anchor.bias == Bias::Left && ins_key == key && anchor.offset > 0)
                {
                    match prev {
                        Some(p) => &p.fragment_id,
                        None => Locator::min_ref(),
                    }
                } else {
                    &insertion.fragment_id
                }
            },
            None => match self.insertions.last() {
                Some(ins) => &ins.fragment_id,
                None => Locator::min_ref(),
            },
        }
    }

    fn find_fragment_for_anchor(&self, anchor: &Anchor) -> (Option<&Fragment>, usize) {
        let cx = &None;
        let target = Some(self.insertion_fragment_id(anchor));
        let (start, _end, item) = self
            .fragments
            .find::<Dimensions<Option<&Locator>, usize>, _>(cx, &target, Bias::Left);

        (item, start.1)
    }

    /// Resolve a batch of anchors to byte offsets in two forward passes.
    ///
    /// Equivalent to [`Self::resolve_anchor`] on each, but advances one cursor
    /// per tree over the anchors sorted into that tree's key order rather than
    /// seeking from the root twice per anchor. Results come back in input order.
    ///
    /// The two passes are separate because the trees are keyed differently. An
    /// anchor's insertion key says nothing about where its fragment sorts, so
    /// the whole set has to be resolved to fragment ids before the second walk
    /// can visit them in Locator order.
    pub fn resolve_anchors_batch(&self, anchors: &[Anchor]) -> Vec<usize> {
        let mut results = vec![0usize; anchors.len()];

        // Anchors needing a real lookup, paired with their input index. The
        // sentinels answer without touching either tree.
        let mut pending: Vec<usize> = Vec::with_capacity(anchors.len());
        for (i, anchor) in anchors.iter().enumerate() {
            if anchor.is_min() {
                results[i] = 0;
            } else if anchor.is_max() {
                results[i] = self.visible_text.len();
            } else {
                pending.push(i);
            }
        }
        if pending.is_empty() {
            return results;
        }

        // Left before Right at an equal key, so the cursor only ever advances.
        pending.sort_unstable_by_key(|&i| {
            let a = &anchors[i];
            (a.timestamp, a.offset, a.bias)
        });

        let mut fragment_ids: Vec<(usize, &Locator)> = Vec::with_capacity(pending.len());
        {
            let mut cursor = self.insertions.cursor::<InsertionFragmentKey>(());
            for &i in &pending {
                let anchor = &anchors[i];
                let key = InsertionFragmentKey {
                    timestamp: anchor.timestamp,
                    split_offset: anchor.offset,
                };
                cursor.seek_forward(&key, anchor.bias);

                let fragment_id = match cursor.item() {
                    Some(insertion) => {
                        let ins_key = InsertionFragmentKey {
                            timestamp: insertion.timestamp,
                            split_offset: insertion.split_offset,
                        };
                        if ins_key > key
                            || (anchor.bias == Bias::Left && ins_key == key && anchor.offset > 0)
                        {
                            match cursor.prev_item() {
                                Some(p) => &p.fragment_id,
                                None => Locator::min_ref(),
                            }
                        } else {
                            &insertion.fragment_id
                        }
                    },
                    None => match self.insertions.last() {
                        Some(ins) => &ins.fragment_id,
                        None => Locator::min_ref(),
                    },
                };
                fragment_ids.push((i, fragment_id));
            }
        }

        fragment_ids.sort_by(|a, b| a.1.cmp(b.1));

        let cx = &None;
        let mut cursor = self
            .fragments
            .cursor::<Dimensions<Option<&Locator>, usize>>(cx);
        for (i, fragment_id) in fragment_ids {
            let target = Some(fragment_id);
            cursor.seek_forward(&target, Bias::Left);
            let base_offset = cursor.start().1;
            let anchor = &anchors[i];
            results[i] = match cursor.item() {
                Some(f) if f.visible => {
                    base_offset + anchor.offset.saturating_sub(f.insertion_offset) as usize
                },
                _ => base_offset,
            };
        }

        results
    }

    pub fn point_for_anchor(&self, anchor: &Anchor) -> Point {
        self.visible_text
            .offset_to_point(self.resolve_anchor(anchor))
    }

    pub fn points_for_anchors_batch(&self, anchors: &[Anchor]) -> Vec<Point> {
        let offsets = self.resolve_anchors_batch(anchors);
        self.visible_text.offsets_to_points_batch(&offsets)
    }

    pub fn is_anchor_valid(&self, anchor: &Anchor) -> bool {
        if anchor.is_min() || anchor.is_max() {
            return true;
        }
        if anchor.timestamp > self.version {
            return false;
        }
        if let Some(id) = anchor.buffer_id
            && id != self.buffer_id
        {
            return false;
        }
        let (fragment, _) = self.find_fragment_for_anchor(anchor);
        fragment.is_some_and(|f| f.visible)
    }

    pub fn edits_since(&self, since_version: u64) -> Patch<usize> {
        if since_version >= self.version {
            return Patch::empty();
        }

        let cx = &None;
        let mut old_offset = 0usize;
        let mut new_offset = 0usize;
        let mut new_offset_from_skipped = 0usize;
        let mut edits: Vec<Edit<usize>> = Vec::new();

        let mut cursor = self
            .fragments
            .filter::<_, usize>(cx, |summary| summary.max_version > since_version);

        cursor.next();
        while let Some(fragment) = cursor.item() {
            // cursor.start() = cumulative visible bytes of all items
            // (including skipped unchanged ones) before this fragment.
            // The difference from our last tracked new_offset is how many
            // unchanged visible bytes were skipped.
            let skipped_visible = *cursor.start() - new_offset_from_skipped;
            old_offset += skipped_visible;
            new_offset += skipped_visible;

            let len = fragment.len as usize;
            let was_visible = fragment.was_visible(since_version, &self.undo_map);

            if fragment.visible && !was_visible {
                let edit = Edit {
                    old: old_offset..old_offset,
                    new: new_offset..(new_offset + len),
                };
                edits.push(edit);
                new_offset += len;
            } else if !fragment.visible && was_visible {
                let edit = Edit {
                    old: old_offset..(old_offset + len),
                    new: new_offset..new_offset,
                };
                edits.push(edit);
                old_offset += len;
            } else if fragment.visible {
                old_offset += len;
                new_offset += len;
            }

            new_offset_from_skipped = *cursor.start() + fragment.visible_len();
            cursor.next();
        }

        // The per-fragment edits are already sorted and monotonic in a single
        // old->new coordinate space, so composing them onto an empty patch
        // consolidates adjacent edits without ever taking compose's overlap
        // branch. Composing them one at a time instead would feed each edit's
        // absolute-old range into the running result's shifted new space,
        // mis-sequencing them and underflowing Edit::old_len.
        Patch::empty().compose(edits)
    }

    pub fn len(&self) -> usize {
        self.visible_text.len()
    }

    pub fn is_empty(&self) -> bool {
        self.visible_text.len() == 0
    }

    pub fn max_point(&self) -> Point {
        self.visible_text.max_point()
    }

    pub fn line_count(&self) -> u32 {
        self.visible_text.max_point().row + 1
    }
}

pub type SharedBuffer = Arc<std::sync::RwLock<TextBuffer>>;

#[cfg(test)]
mod tests {
    use super::{insertion_edits, BufferOp, TextBuffer, TreeEdit, OPS_COMPACT_THRESHOLD};
    use std::{cmp::Ordering, mem, ops::Range, sync::Arc};
    use stoat_text::{
        Anchor, Bias, BufferId, IndentStyle, InsertionFragment, Locator, Point, Selection,
        SelectionGoal,
    };

    fn buf(content: &str) -> TextBuffer {
        TextBuffer::with_text(BufferId::new(0), content)
    }

    /// A save during an insert session lands its marker on the last edit, and
    /// undo pops whole groups, so without a boundary there the nearest undo
    /// steps straight past the saved state to before the session began.
    ///
    /// A live buffer hides that behind the text comparison. A restored one
    /// cannot, because the history carries the marker across but not the saved
    /// bytes, leaving the frontier the only thing left to answer with.
    #[test]
    fn a_save_inside_an_insert_session_is_undoable_back_to() {
        let mut b = buf("start\n");
        b.begin_group(Arc::from([]));
        b.edit(6..6, "one\n");
        b.mark_clean();
        let saved = b.snapshot.visible_text.to_string();

        b.edit(10..10, "two\n");
        b.seal_group(Arc::from([]));
        assert!(b.dirty, "the edit after the save leaves it modified");

        b.undo();
        assert_eq!(
            b.snapshot.visible_text.to_string(),
            saved,
            "the undo lands on the saved state rather than stepping past it",
        );
        assert!(!b.dirty, "and reads clean there");

        let restored = TextBuffer::from_history(BufferId::new(0), &b.history());
        assert_eq!(restored.snapshot.visible_text.to_string(), saved);
        assert!(
            !restored.dirty,
            "the restored buffer agrees, with no saved bytes to fall back on",
        );
    }

    /// Everything an `edit_batch` must reproduce, read off a buffer.
    ///
    /// Comparing final text alone would pass a batch that corrupted the fragment
    /// tree, since the rope is built separately from it. The anchors and the
    /// `edits_since` patch are what read the tree back out.
    fn observable(b: &TextBuffer, pre_version: u64) -> String {
        let snap = &b.snapshot;
        let len = snap.visible_text.len();
        let anchors: Vec<usize> = (0..=len)
            .filter(|o| snap.visible_text.is_char_boundary(*o))
            .flat_map(|o| [(o, Bias::Left), (o, Bias::Right)])
            .map(|(o, bias)| snap.resolve_anchor(&snap.anchor_at(o, bias)))
            .collect();
        let patch: Vec<(Range<usize>, Range<usize>)> = snap
            .edits_since(pre_version)
            .edits()
            .iter()
            .map(|e| (e.old.clone(), e.new.clone()))
            .collect();

        format!(
            "text={:?}\ndeleted={:?}\nversion={}\nops={:?}\nanchors={:?}\npatch={:?}",
            snap.visible_text.to_string(),
            snap.deleted_text.to_string(),
            snap.version,
            b.ops,
            anchors,
            patch,
        )
    }

    /// An anchor at every character boundary, in both biases, for a buffer that
    /// has not been edited yet.
    ///
    /// [`observable`] builds its anchors on the buffer it then reads, which
    /// makes that an identity round trip. An anchor made at an offset resolves
    /// back to it by construction. These are the readers `split_at_boundaries`
    /// promises to keep valid, made against the tree as it stood before the
    /// re-keying.
    fn anchors_before_edits(b: &TextBuffer) -> Vec<Anchor> {
        let snap = &b.snapshot;
        let len = snap.visible_text.len();
        (0..=len)
            .filter(|o| snap.visible_text.is_char_boundary(*o))
            .flat_map(|o| [(o, Bias::Left), (o, Bias::Right)])
            .map(|(o, bias)| snap.anchor_at(o, bias))
            .collect()
    }

    /// Where carried anchors land, and how they order against each other.
    ///
    /// Resolution alone would miss a re-keying that moved a run of anchors
    /// together, since they would go on agreeing with each other while all
    /// being wrong, so the pairwise ordering is read too.
    fn carried(b: &TextBuffer, anchors: &[Anchor]) -> String {
        let snap = &b.snapshot;
        let resolved: Vec<usize> = anchors.iter().map(|a| snap.resolve_anchor(a)).collect();
        let order: Vec<Ordering> = anchors
            .windows(2)
            .map(|pair| snap.cmp_anchors(&pair[0], &pair[1]))
            .collect();
        format!("resolved={resolved:?}\norder={order:?}")
    }

    /// `edit_batch` is indistinguishable from the sequential calls it replaces.
    ///
    /// The fixtures are the shapes that break a shared cursor walk. Several carets
    /// sit in a buffer that is still one fragment, ranges touch end to start,
    /// inserts repeat at one offset, and a tree arrives already fragmented by
    /// prior edits and an undo.
    fn assert_batch_matches_sequential(content: &str, edits: &[(Range<usize>, &str)]) {
        let mut batched = buf(content);
        let mut sequential = buf(content);
        let pre_version = batched.snapshot.version;

        // Each buffer supplies its own, so the comparison never rests on
        // anchors from one resolving in the other. What is being asked is that
        // an anchor made the same way in each survives the edits the same way.
        let batched_anchors = anchors_before_edits(&batched);
        let sequential_anchors = anchors_before_edits(&sequential);

        batched.edit_batch(edits);
        for (range, text) in edits {
            sequential.edit(range.clone(), text);
        }

        assert_eq!(
            observable(&batched, pre_version),
            observable(&sequential, pre_version),
            "batch diverged from sequential on {content:?} with {edits:?}",
        );
        assert_eq!(
            carried(&batched, &batched_anchors),
            carried(&sequential, &sequential_anchors),
            "anchors predating the batch diverged on {content:?} with {edits:?}",
        );
        check_invariants(&batched, "after the batch");
        check_invariants(&sequential, "after the sequential edits");

        // Undo and redo read the op log and the fragment tree together, so a
        // tree that merely looks right at the end still fails here. Undo also
        // restores fragments the batch removed, which is where an anchor into
        // deleted text is most likely to come back somewhere else.
        for step in 0..3 {
            batched.undo();
            sequential.undo();
            assert_eq!(
                observable(&batched, pre_version),
                observable(&sequential, pre_version),
                "undo {step} diverged on {content:?}",
            );
            assert_eq!(
                carried(&batched, &batched_anchors),
                carried(&sequential, &sequential_anchors),
                "undo {step} moved a carried anchor on {content:?}",
            );
            check_invariants(&batched, &format!("batched, after undo {step}"));
            check_invariants(&sequential, &format!("sequential, after undo {step}"));
        }
        for step in 0..3 {
            batched.redo();
            sequential.redo();
            assert_eq!(
                observable(&batched, pre_version),
                observable(&sequential, pre_version),
                "redo {step} diverged on {content:?}",
            );
            assert_eq!(
                carried(&batched, &batched_anchors),
                carried(&sequential, &sequential_anchors),
                "redo {step} moved a carried anchor on {content:?}",
            );
            check_invariants(&batched, &format!("batched, after redo {step}"));
            check_invariants(&sequential, &format!("sequential, after redo {step}"));
        }
    }

    #[test]
    fn batch_matches_sequential_for_carets_in_one_fragment() {
        // This is the case the shared-cursor attempt panicked on. A fresh buffer
        // is one fragment, so every caret sits inside it.
        assert_batch_matches_sequential(
            "hello world here",
            &[(12..12, "X"), (6..6, "X"), (0..0, "X")],
        );
    }

    #[test]
    fn batch_matches_sequential_for_repeated_offsets_and_touching_ranges() {
        assert_batch_matches_sequential("abcdefghij", &[(4..4, "Y"), (4..4, "X")]);
        assert_batch_matches_sequential("abcdefghij", &[(5..7, "Z"), (3..5, "Y")]);
    }

    #[test]
    fn insertion_records_collapse_to_the_last_under_one_key() {
        // What applying the records one at a time did, and what the batch has to
        // reproduce. `SumTree::edit` would keep both same-key records instead,
        // and no buffer path emits a pair today, so the contract is pinned here
        // rather than through an edit.
        let record = |split_offset: u32, fragment_id: Locator| InsertionFragment {
            timestamp: 7,
            split_offset,
            fragment_id,
        };
        let (first, last) = (Locator::min(), Locator::max());

        let edits = insertion_edits(vec![
            record(4, first.clone()),
            record(0, first.clone()),
            record(4, last.clone()),
        ]);

        let kept: Vec<(u32, &Locator)> = edits
            .iter()
            .map(|edit| match edit {
                TreeEdit::Insert(record) => (record.split_offset, &record.fragment_id),
                TreeEdit::Remove(_) => unreachable!("insertion_edits only inserts"),
            })
            .collect();
        assert_eq!(
            kept,
            vec![(0, &first), (4, &last)],
            "the repeated key keeps the record emitted last",
        );
    }

    #[test]
    fn batch_matches_sequential_over_a_fragmented_tree() {
        let mut fragmented = buf("one two three four five");
        fragmented.edit(8..8, "AA");
        fragmented.edit(0..0, "BB");
        fragmented.edit(4..6, "");
        fragmented.undo();
        let content = fragmented.snapshot.visible_text.to_string();

        assert_batch_matches_sequential(&content, &[(18..20, "Q"), (10..12, ""), (2..2, "P")]);
    }

    #[test]
    fn batch_matches_sequential_over_random_edits() {
        // A cheap deterministic generator avoids a rand dependency, and the fixed
        // seed keeps a failure reproducible.
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        let mut next = |bound: usize| -> usize {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            if bound == 0 {
                0
            } else {
                state as usize % bound
            }
        };

        // Multibyte, so a batch boundary can sit against a character several
        // bytes wide rather than always between two ASCII ones.
        let content =
            "the quick \u{65e5}\u{672c} fox jumps\ne\u{301} over the lazy dog\nand back again\n";
        for _ in 0..200 {
            let count = 2 + next(3);
            let mut cuts: Vec<usize> = (0..count * 2)
                .map(|_| next(content.len() + 1))
                .filter(|o| content.is_char_boundary(*o))
                .collect();
            cuts.sort_unstable();
            cuts.dedup();
            if cuts.len() < 2 {
                continue;
            }

            let texts = ["", "X", "hello", "\n"];
            let mut edits: Vec<(Range<usize>, &str)> = cuts
                .chunks_exact(2)
                .map(|w| (w[0]..w[1], texts[next(texts.len())]))
                .collect();
            edits.reverse();

            assert_batch_matches_sequential(content, &edits);
        }
    }

    /// The pre-split rewrites fragment ids and insertion offsets, so it has to
    /// leave everything a reader can see untouched.
    #[test]
    fn splitting_at_boundaries_changes_nothing_observable() {
        let mut b = buf("one two three four five six seven");
        b.edit(8..8, "AA");
        b.edit(0..0, "BB");
        b.edit(4..6, "");
        let pre_version = b.snapshot.version;
        let before = observable(&b, pre_version);

        // Made against the tree the split is about to re-key, which is what its
        // doc promises to keep valid.
        let anchors = anchors_before_edits(&b);
        let carried_before = carried(&b, &anchors);

        let len = b.snapshot.visible_text.len();
        let boundaries: Vec<usize> = (0..=len).step_by(3).collect();
        let mut new_insertions = Vec::new();
        let split = super::split_at_boundaries(
            mem::replace(&mut b.snapshot.fragments, super::SumTree::new(&None)),
            &boundaries,
            &mut new_insertions,
        );
        b.snapshot.fragments = split;
        for ins in new_insertions {
            b.snapshot.insertions.insert_or_replace(ins, ());
        }

        assert_eq!(
            before,
            observable(&b, pre_version),
            "the split is invisible"
        );
        assert_eq!(
            carried_before,
            carried(&b, &anchors),
            "the split moved an anchor that predates it",
        );
    }

    #[test]
    fn anchors_at_batch_matches_anchor_at() {
        let mut b = buf("hello world\nsecond line\nthird line\n");
        // Fragment the tree so anchors resolve across several fragments.
        b.edit(6..6, "brave ");
        b.edit(0..0, "prefix ");
        b.edit(10..14, "");
        let snap = b.snapshot.clone();

        let len = snap.visible_text.len();
        let ascending: Vec<usize> = (0..=len).collect();
        // The walk takes ascending offsets where they stand and permutes any
        // other order, so both orders have to answer what `anchor_at` does.
        // Odds before evens leaves no run of the input in order.
        let shuffled: Vec<usize> = (0..=len)
            .filter(|o| o % 2 == 1)
            .chain((0..=len).filter(|o| o % 2 == 0))
            .collect();

        for offsets in [&ascending, &shuffled] {
            for bias in [Bias::Left, Bias::Right] {
                let batch = snap.anchors_at_batch(offsets, bias);
                for (i, &off) in offsets.iter().enumerate() {
                    assert_eq!(
                        batch[i],
                        snap.anchor_at(off, bias),
                        "anchors_at_batch disagrees at offset {off} bias {bias:?}"
                    );
                }
            }
        }
    }

    /// A deterministic pseudo-random walk, so the fixture below fragments the
    /// tree in a shape nobody hand-picked while staying reproducible.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 33
    }

    /// `resolve_anchors_batch` must agree with `resolve_anchor` on every anchor,
    /// including ones left inside regions a later edit deleted, since the two
    /// walk the fragment trees by different routes.
    #[test]
    fn resolve_anchors_batch_matches_resolve_anchor() {
        // A spread of seeds, since one tree shape can easily miss a boundary
        // case in a cursor walk.
        for seed in [0x5eed_1234, 0x0bad_c0de, 0x1357_9bdf, 7, 0xffff_0001] {
            check_batch_matches_per_anchor(seed);
        }
    }

    fn check_batch_matches_per_anchor(seed: u64) {
        let mut b = buf("hello world\nsecond line\nthird line\nfourth line\n");
        let mut seed = seed;

        // Fragment the tree through a few dozen interleaved edits.
        for _ in 0..30 {
            let len = b.snapshot.visible_text.len();
            let at = (lcg(&mut seed) as usize) % (len + 1);
            if lcg(&mut seed).is_multiple_of(3) && at < len {
                let end = (at + 1 + (lcg(&mut seed) as usize) % 5).min(len);
                b.edit(at..end, "");
            } else {
                b.edit(at..at, "xy\n");
            }
        }

        let mid = b.snapshot.clone();
        let len = mid.visible_text.len();
        let mut anchors = vec![
            Anchor::min_for_buffer(BufferId::new(0)),
            Anchor::max_for_buffer(BufferId::new(0)),
        ];
        for _ in 0..50 {
            let off = (lcg(&mut seed) as usize) % (len + 1);
            let bias = if lcg(&mut seed).is_multiple_of(2) {
                Bias::Left
            } else {
                Bias::Right
            };
            anchors.push(mid.anchor_at(off, bias));
        }

        // More edits after anchoring, so some anchors now sit in deleted text.
        // Some of that deleting is done by a batch, since a batch reaches the
        // fragment tree by its own path and an anchor it strands has to resolve
        // the same way.
        for round in 0..15 {
            let len = b.snapshot.visible_text.len();
            let at = (lcg(&mut seed) as usize) % (len + 1);

            if round % 4 == 3 && len > 12 {
                let second = (lcg(&mut seed) as usize) % (len / 2);
                let first = len / 2 + (lcg(&mut seed) as usize) % (len - len / 2);
                let first_end = (first + 1 + (lcg(&mut seed) as usize) % 4).min(len);
                let second_end = (second + 1 + (lcg(&mut seed) as usize) % 4).min(len / 2);
                b.edit_batch(&[(first..first_end, "Q"), (second..second_end, "")]);
            } else if at < len {
                let end = (at + 1 + (lcg(&mut seed) as usize) % 7).min(len);
                b.edit(at..end, "z");
            } else {
                b.edit(at..at, "tail\n");
            }
        }

        let snap = b.snapshot.clone();
        let expected: Vec<usize> = anchors.iter().map(|a| snap.resolve_anchor(a)).collect();
        assert_eq!(
            snap.resolve_anchors_batch(&anchors),
            expected,
            "the batch walk must land every anchor where the per-anchor seek does",
        );

        // A single-anchor batch and an empty one take the same routes.
        assert!(snap.resolve_anchors_batch(&[]).is_empty());
        for anchor in &anchors {
            assert_eq!(
                snap.resolve_anchors_batch(std::slice::from_ref(anchor)),
                vec![snap.resolve_anchor(anchor)],
            );
        }
    }

    #[test]
    fn indent_style_detected_from_tabs() {
        let b = buf("fn a() {\n\tlet x = 1;\n\tif x {\n\t\tx;\n\t}\n}\n");
        assert_eq!(b.indent_style(), IndentStyle::Tabs);
    }

    #[test]
    fn indent_style_detected_from_spaces() {
        let b = buf("fn a() {\n  let x = 1;\n  if x {\n    x;\n  }\n}\n");
        assert_eq!(b.indent_style(), IndentStyle::Spaces(2));
    }

    #[test]
    fn indent_style_defaults_without_evidence() {
        assert_eq!(buf("alpha\nbravo\n").indent_style(), IndentStyle::default());
        assert_eq!(
            TextBuffer::new(BufferId::new(0)).indent_style(),
            IndentStyle::default()
        );
    }

    #[test]
    fn anchor_insert_before() {
        let mut b = buf("hello");
        let a = b.anchor_at(3, Bias::Right);
        b.edit(0..0, "XX");
        assert_eq!(b.resolve_anchor(&a), 5);
    }

    #[test]
    fn anchor_insert_after() {
        let mut b = buf("hello");
        let a = b.anchor_at(2, Bias::Right);
        b.edit(4..4, "XX");
        assert_eq!(b.resolve_anchor(&a), 2);
    }

    #[test]
    fn anchor_delete_before() {
        let mut b = buf("hello");
        let a = b.anchor_at(4, Bias::Right);
        b.edit(0..2, "");
        assert_eq!(b.resolve_anchor(&a), 2);
    }

    #[test]
    fn anchor_bias_left_at_insertion() {
        let mut b = buf("hello");
        let a = b.anchor_at(3, Bias::Left);
        b.edit(3..3, "XX");
        assert_eq!(b.resolve_anchor(&a), 3);
    }

    #[test]
    fn anchor_bias_right_at_insertion() {
        let mut b = buf("hello");
        let a = b.anchor_at(3, Bias::Right);
        b.edit(3..3, "XX");
        assert_eq!(b.resolve_anchor(&a), 5);
    }

    #[test]
    fn anchor_within_deleted_range_left() {
        let mut b = buf("hello world");
        let a = b.anchor_at(7, Bias::Left);
        b.edit(5..11, "");
        assert_eq!(b.resolve_anchor(&a), 5);
    }

    #[test]
    fn anchor_within_deleted_range_right() {
        let mut b = buf("hello world");
        let a = b.anchor_at(7, Bias::Right);
        b.edit(5..11, "");
        assert_eq!(b.resolve_anchor(&a), 5);
    }

    #[test]
    fn an_anchor_in_replaced_text_lands_after_the_replacement() {
        for bias in [Bias::Left, Bias::Right] {
            let mut b = buf("hello world");
            let a = b.anchor_at(7, bias);

            b.edit(5..11, "XYZ");
            assert_eq!(b.snapshot.visible_text.to_string(), "helloXYZ");
            assert_eq!(
                b.resolve_anchor(&a),
                8,
                "{bias:?} anchor resolved before the text that replaced it"
            );
        }
    }

    #[test]
    fn a_replace_spanning_fragments_lands_anchors_after_it() {
        let mut b = buf("abcdefghij");
        b.edit(3..3, "XX");
        assert_eq!(b.snapshot.visible_text.to_string(), "abcXXdefghij");

        // Partway into the first fragment, across the second, partway into the
        // third, so every branch of the walk sees the range.
        let inside = b.anchor_at(6, Bias::Right);
        let after = b.anchor_at(8, Bias::Right);

        b.edit(2..8, "QQ");
        assert_eq!(b.snapshot.visible_text.to_string(), "abQQghij");
        assert_eq!(b.resolve_anchor(&inside), 4);
        assert_eq!(b.resolve_anchor(&after), 4);
    }

    #[test]
    fn replacing_only_part_of_a_fragment_keeps_the_rest_in_place() {
        let mut b = buf("hello world");
        let inside = b.anchor_at(7, Bias::Right);
        let after = b.anchor_at(9, Bias::Right);

        b.edit(5..8, "XYZ");
        assert_eq!(b.snapshot.visible_text.to_string(), "helloXYZrld");
        assert_eq!(b.resolve_anchor(&inside), 8);
        assert_eq!(b.resolve_anchor(&after), 9);
    }

    #[test]
    fn a_range_spanning_deleted_text_stays_well_formed() {
        let mut b = buf("hello world");

        // Start biased right and end biased left, the convention a token range
        // is built with so typing at either edge falls outside it.
        let start = b.anchor_at(6, Bias::Right);
        let end = b.anchor_at(9, Bias::Left);
        assert_eq!(b.snapshot.cmp_anchors(&start, &end), Ordering::Less);

        b.edit(5..11, "");
        assert_eq!(
            b.snapshot.cmp_anchors(&start, &end),
            Ordering::Less,
            "the deletion inverted a range that was well formed before it"
        );
    }

    #[test]
    fn deleted_anchors_keep_their_order_and_stay_distinct() {
        let mut b = buf("hello world");

        let inside: Vec<Anchor> = (6..10).map(|o| b.anchor_at(o, Bias::Right)).collect();
        b.edit(5..11, "");

        for pair in inside.windows(2) {
            assert_eq!(
                b.snapshot.cmp_anchors(&pair[0], &pair[1]),
                Ordering::Less,
                "distinct anchors in one deleted region tied or swapped"
            );
        }

        for (i, a) in inside.iter().enumerate() {
            assert_eq!(b.snapshot.cmp_anchors(a, a), Ordering::Equal, "anchor {i}");
        }
    }

    #[test]
    fn the_document_sentinels_bound_every_anchor() {
        let mut b = buf("hello world");
        let min = Anchor::min_for_buffer(b.snapshot.buffer_id);
        let max = Anchor::max_for_buffer(b.snapshot.buffer_id);

        // A second insertion, so the newest fragment by timestamp is not the
        // last one in the document and max cannot rely on being either.
        b.edit(0..0, "first ");

        for offset in [1, 6, 11, 16] {
            let a = b.anchor_at(offset, Bias::Right);
            assert_eq!(b.snapshot.cmp_anchors(&min, &a), Ordering::Less, "{offset}");
            assert_eq!(
                b.snapshot.cmp_anchors(&max, &a),
                Ordering::Greater,
                "{offset}"
            );
        }

        assert_eq!(b.snapshot.cmp_anchors(&min, &max), Ordering::Less);
    }

    #[test]
    fn anchor_multiple_edits() {
        let mut b = buf("abcdef");
        let a = b.anchor_at(4, Bias::Right);
        b.edit(0..0, "XX");
        b.edit(3..5, "Y");
        assert_eq!(b.resolve_anchor(&a), 5);
    }

    #[test]
    fn anchor_min_max() {
        let mut b = buf("hello");
        let min = Anchor::min();
        let max = Anchor::max();
        assert_eq!(b.resolve_anchor(&min), 0);
        assert_eq!(b.resolve_anchor(&max), 5);
        b.edit(5..5, " world");
        assert_eq!(b.resolve_anchor(&min), 0);
        assert_eq!(b.resolve_anchor(&max), 11);
    }

    #[test]
    fn batch_resolve() {
        let mut b = buf("hello");
        let a1 = b.anchor_at(1, Bias::Right);
        let a2 = b.anchor_at(3, Bias::Right);
        b.edit(0..0, "XX");
        let offsets = b.snapshot.resolve_anchors_batch(&[a1, a2]);
        assert_eq!(offsets, vec![3, 5]);
    }

    #[test]
    fn point_for_anchor_multiline() {
        let mut b = buf("hello\nworld");
        let a = b.anchor_at(8, Bias::Right);
        b.edit(0..0, "XX");
        let point = b.point_for_anchor(&a);
        assert_eq!(point, Point::new(1, 2));
    }

    #[test]
    fn resolve_skips_early_records() {
        let mut b = buf("hello");
        for _ in 0..100 {
            b.edit(0..0, "X");
        }
        let a = b.anchor_at(50, Bias::Right);
        b.edit(0..0, "Y");
        assert_eq!(b.resolve_anchor(&a), 51);
    }

    #[test]
    fn edits_since_single_insert() {
        let mut b = buf("hello");
        let v0 = b.version();
        b.edit(5..5, " world");
        let patch = b.snapshot.edits_since(v0);
        let edits = patch.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old, 5..5);
        assert_eq!(edits[0].new, 5..11);
    }

    #[test]
    fn edits_since_single_delete() {
        let mut b = buf("hello world");
        let v0 = b.version();
        b.edit(5..11, "");
        let patch = b.snapshot.edits_since(v0);
        let edits = patch.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old, 5..11);
        assert_eq!(edits[0].new, 5..5);
    }

    #[test]
    fn edits_since_no_changes() {
        let b = buf("hello");
        let patch = b.snapshot.edits_since(b.version());
        assert!(patch.is_empty());
    }

    #[test]
    fn edits_since_reflects_an_undone_insert() {
        let mut b = buf("hello");
        b.edit(5..5, " world");
        let v_after_insert = b.version();
        b.undo();
        let patch = b.snapshot.edits_since(v_after_insert);
        let edits = patch.edits();
        assert_eq!(edits.len(), 1, "the reverted insert appears as a deletion");
        assert_eq!(edits[0].old, 5..11);
        assert_eq!(edits[0].new, 5..5);
    }

    #[test]
    fn edits_since_reflects_an_undone_delete() {
        let mut b = buf("hello world");
        b.edit(5..11, "");
        let v_after_delete = b.version();
        b.undo();
        let patch = b.snapshot.edits_since(v_after_delete);
        let edits = patch.edits();
        assert_eq!(
            edits.len(),
            1,
            "the reverted delete appears as an insertion"
        );
        assert_eq!(edits[0].old, 5..5);
        assert_eq!(edits[0].new, 5..11);
    }

    #[test]
    fn edits_since_after_undo_then_redo_is_empty() {
        let mut b = buf("hello");
        b.edit(5..5, " world");
        let v_after_insert = b.version();
        b.undo();
        b.redo();
        let patch = b.snapshot.edits_since(v_after_insert);
        assert!(
            patch.is_empty(),
            "undo then redo returns to the same content: {:?}",
            patch.edits()
        );
    }

    #[test]
    fn edits_since_spans_an_undo_then_a_splitting_edit() {
        // Undoing the delete restores the "AB" fragment. Inserting inside it
        // splits it, and the split halves must inherit the restored fragment's
        // undo version, or edits_since from the post-delete version filters them
        // out and drops the restored text.
        let mut b = buf("AB");
        b.edit(0..2, "");
        let v_after_delete = b.version();
        b.undo();
        b.edit(1..1, "X");
        let new_text = b.snapshot.visible_text.to_string();
        assert_eq!(new_text, "AXB");

        let patch = b.snapshot.edits_since(v_after_delete);
        let mut reconstructed = String::new();
        for edit in patch.edits().iter().rev() {
            reconstructed.replace_range(edit.old.clone(), &new_text[edit.new.clone()]);
        }
        assert_eq!(
            reconstructed, new_text,
            "patch from the post-delete version rebuilds the split content",
        );
    }

    #[test]
    fn text_roundtrip() {
        let b = buf("hello world");
        assert_eq!(b.snapshot.visible_text.to_string(), "hello world");
    }

    #[test]
    fn edit_replace() {
        let mut b = buf("hello world");
        b.edit(5..11, " there");
        assert_eq!(b.snapshot.visible_text.to_string(), "hello there");
    }

    #[test]
    fn empty_buffer_anchor() {
        let b = TextBuffer::new(BufferId::new(0));
        let a = b.anchor_at(0, Bias::Left);
        assert_eq!(b.resolve_anchor(&a), 0);
    }

    #[test]
    fn edits_since_many_fragments_few_changes() {
        let mut b = buf("abcdefghij");
        for i in 0..50 {
            b.edit(i..i, "X");
        }
        let v_mid = b.version();
        b.edit(0..0, "NEW");
        let patch = b.snapshot.edits_since(v_mid);
        let edits = patch.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old, 0..0);
        assert_eq!(edits[0].new, 0..3);
    }

    #[test]
    fn edits_since_replace() {
        let mut b = buf("hello world");
        let v0 = b.version();
        b.edit(5..11, " there");
        let patch = b.snapshot.edits_since(v0);
        let edits = patch.edits();
        assert_eq!(edits.len(), 1);
        assert_eq!(edits[0].old, 5..11);
        assert_eq!(edits[0].new, 5..11);
    }

    /// An edit-sequence test case pairing the initial text with the ordered
    /// `(range, replacement)` edits applied to it in turn.
    type EditCase<'a> = (&'a str, &'a [(Range<usize>, &'a str)]);

    #[test]
    fn edits_since_reconstructs_multi_region_changes() {
        // Applying the patch in reverse to the pre-edit text must reproduce the
        // post-edit text. These multi-region edits shift new-coordinates past a
        // later change's absolute-old offset, which the accumulation must keep
        // monotonic rather than compose edits across the shifted region -- the
        // last case otherwise underflows Edit::old_len.
        let cases: &[EditCase<'_>] = &[
            ("0123456789", &[(2..2, "ABCDEFGHIJKLMNOPQR"), (23..26, "")]),
            ("0123456789", &[(1..1, "ABCDEFGHIJ"), (13..17, "")]),
            (
                "aaaa bbbb cccc dddd",
                &[(0..0, "X"), (5..5, "Y"), (10..10, "Z")],
            ),
            (
                "abcdefghijklmnopqrstuvwxyz",
                &[
                    (22..22, "ABCDEFGHIJKLMN"),
                    (18..20, "ABCDEFGHIJKLMNOPQRS"),
                    (1..6, "ABCDEFGHIJKLM"),
                ],
            ),
        ];
        for (initial, edits) in cases {
            let mut b = buf(initial);
            let old_text = b.snapshot.visible_text.to_string();
            let v0 = b.version();
            for (range, text) in *edits {
                b.edit(range.clone(), text);
            }
            let new_text = b.snapshot.visible_text.to_string();
            let patch = b.snapshot.edits_since(v0);
            let mut reconstructed = old_text;
            for edit in patch.edits().iter().rev() {
                reconstructed.replace_range(edit.old.clone(), &new_text[edit.new.clone()]);
            }
            assert_eq!(
                reconstructed,
                new_text,
                "edits={edits:?} patch={:?}",
                patch.edits()
            );
        }
    }

    #[test]
    fn anchor_invalid_after_deletion() {
        let mut b = buf("hello world");
        let a = b.anchor_at(7, Bias::Right);
        assert!(b.snapshot.is_anchor_valid(&a));
        b.edit(5..11, "");
        assert!(!b.snapshot.is_anchor_valid(&a));
    }

    #[test]
    fn anchor_valid_in_visible_text() {
        let mut b = buf("hello world");
        let a = b.anchor_at(2, Bias::Right);
        b.edit(5..11, "");
        assert!(b.snapshot.is_anchor_valid(&a));
    }

    #[test]
    fn anchor_invalid_wrong_buffer() {
        let b = buf("hello");
        let a = b.anchor_at(2, Bias::Right);
        let other = TextBuffer::with_text(BufferId::new(99), "other");
        assert!(!other.snapshot.is_anchor_valid(&a));
    }

    #[test]
    fn undo_insertion() {
        let mut b = buf("hello");
        b.edit(5..5, " world");
        assert_eq!(b.snapshot.visible_text.to_string(), "hello world");
        b.undo();
        assert_eq!(b.snapshot.visible_text.to_string(), "hello");
    }

    #[test]
    fn undo_deletion() {
        let mut b = buf("hello world");
        b.edit(5..11, "");
        assert_eq!(b.snapshot.visible_text.to_string(), "hello");
        b.undo();
        assert_eq!(b.snapshot.visible_text.to_string(), "hello world");
    }

    #[test]
    fn undo_replace() {
        let mut b = buf("hello world");
        b.edit(6..11, "there");
        assert_eq!(b.snapshot.visible_text.to_string(), "hello there");
        b.undo();
        assert_eq!(b.snapshot.visible_text.to_string(), "hello world");
    }

    #[test]
    fn undo_empty_history() {
        let mut b = TextBuffer::new(BufferId::new(0));
        assert!(b.undo().is_none());
        assert_eq!(b.snapshot.visible_text.to_string(), "");
    }

    #[test]
    fn undo_preserves_anchors() {
        let mut b = buf("hello world");
        let a = b.anchor_at(8, Bias::Right);
        b.edit(5..11, "");
        assert!(!b.snapshot.is_anchor_valid(&a));
        b.undo();
        assert!(b.snapshot.is_anchor_valid(&a));
        assert_eq!(b.resolve_anchor(&a), 8);
    }

    #[test]
    fn redo_after_undo_restores_edit() {
        let mut b = buf("hello");
        b.edit(5..5, " world");
        assert_eq!(b.snapshot.visible_text.to_string(), "hello world");
        assert!(b.undo().is_some());
        assert_eq!(b.snapshot.visible_text.to_string(), "hello");
        assert!(b.redo().is_some());
        assert_eq!(b.snapshot.visible_text.to_string(), "hello world");
    }

    #[test]
    fn redo_walks_back_through_a_full_cycle() {
        let mut b = buf("a");
        b.edit(1..1, "b");
        b.edit(2..2, "c");
        assert_eq!(b.snapshot.visible_text.to_string(), "abc");
        assert!(b.undo().is_some());
        assert!(b.undo().is_some());
        assert_eq!(b.snapshot.visible_text.to_string(), "a");
        assert!(b.redo().is_some());
        assert_eq!(b.snapshot.visible_text.to_string(), "ab");
        assert!(b.redo().is_some());
        assert_eq!(b.snapshot.visible_text.to_string(), "abc");
    }

    /// An edit made after an undo is its own step, not an addition to whatever
    /// group the undo exposed.
    ///
    /// An open group names the top of the history. Undoing moves that top, so a
    /// group left open afterwards points at a group the caller never opened, and
    /// the next edit joins it. On a fresh buffer the exposed group is the seed
    /// the undo floor protects, which would make the edit un-undoable entirely.
    #[test]
    fn an_edit_after_an_undo_undoes_on_its_own() {
        let mut b = buf("seed");
        b.begin_group(Arc::from([]));
        b.edit(4..4, "X");
        assert_eq!(b.snapshot.visible_text.to_string(), "seedX");

        assert!(b.undo().is_some(), "the open group is undoable");
        assert_eq!(b.snapshot.visible_text.to_string(), "seed");

        b.edit(4..4, "Y");
        assert_eq!(b.snapshot.visible_text.to_string(), "seedY");
        assert!(b.undo().is_some(), "the post-undo edit is its own step");
        assert_eq!(
            b.snapshot.visible_text.to_string(),
            "seed",
            "and undoing it leaves the seed content",
        );
        assert!(
            b.undo().is_none(),
            "nothing below the floor is undoable, so the seed survives",
        );
        assert_eq!(b.snapshot.visible_text.to_string(), "seed");
    }

    #[test]
    fn begin_group_collapses_edits_into_one_undo_step() {
        let mut b = buf("");
        b.begin_group(Arc::from([]));
        b.edit(0..0, "a");
        b.edit(1..1, "b");
        b.edit(2..2, "c");
        b.seal_group(Arc::from([]));
        assert_eq!(b.snapshot.visible_text.to_string(), "abc");
        assert!(b.undo().is_some());
        assert_eq!(
            b.snapshot.visible_text.to_string(),
            "",
            "one undo reverts the whole group"
        );
        assert!(b.redo().is_some());
        assert_eq!(
            b.snapshot.visible_text.to_string(),
            "abc",
            "one redo restores the whole group"
        );
    }

    /// Undoing a group rebuilds the buffer once rather than once per edit in
    /// it, so what one pass produces has to be what the per-edit passes
    /// produced. Text alone is not enough of a check: the op stream and
    /// timestamps are what a restored buffer replays, and the patch is what
    /// every consumer diffing across the undo reads.
    ///
    /// The third edit deletes text the first one inserted, which is the case
    /// the collapse changes most. Per-edit passes flip that fragment visible
    /// and back again. One pass sees only that it ended where it started.
    #[test]
    fn a_group_undo_lands_where_edit_by_edit_undos_land() {
        let edits: [(Range<usize>, &str); 3] = [(0..0, "abc"), (3..3, "def"), (1..2, "")];

        let mut grouped = buf("");
        grouped.begin_group(Arc::from([]));
        for (range, text) in &edits {
            grouped.edit(range.clone(), text);
        }
        grouped.seal_group(Arc::from([]));

        let mut separate = buf("");
        for (range, text) in &edits {
            separate.edit(range.clone(), text);
        }

        let before_undo = grouped.version();
        assert_eq!(
            before_undo,
            separate.version(),
            "the two fixtures must reach the same state to be comparable",
        );

        grouped.undo();
        for _ in 0..edits.len() {
            separate.undo();
        }

        assert_eq!(
            grouped.snapshot.visible_text.to_string(),
            separate.snapshot.visible_text.to_string(),
            "one rebuild must restore the text three rebuilds restore",
        );
        assert_eq!(
            grouped.snapshot.deleted_text.to_string(),
            separate.snapshot.deleted_text.to_string(),
            "and park the same bytes as deleted",
        );
        assert_eq!(
            grouped.version(),
            separate.version(),
            "the batch consumes one timestamp per edit, same as the loop",
        );
        assert_eq!(
            format!("{:?}", grouped.history().ops),
            format!("{:?}", separate.history().ops),
            "from_history replays the op stream, so it must stay one op per edit",
        );
        assert_eq!(
            grouped.snapshot.edits_since(before_undo).edits(),
            separate.snapshot.edits_since(before_undo).edits(),
            "consumers diffing across the undo must see the same patch",
        );
    }

    #[test]
    fn a_refused_group_never_asks_for_the_selections() {
        // Refusing is the normal case mid-insert-session, once per typed
        // character, and gathering the selections is what the caller would pay
        // for each time.
        let mut b = buf("");
        b.begin_group(Arc::from([]));

        let mut asked = 0;
        assert!(
            !b.try_begin_group(|| {
                asked += 1;
                Arc::from([])
            }),
            "a group was already open",
        );
        assert_eq!(asked, 0, "so nothing was gathered");

        b.seal_group(Arc::from([]));
        assert!(
            b.try_begin_group(|| {
                asked += 1;
                Arc::from([])
            }),
            "and now one opens",
        );
        assert_eq!(asked, 1, "which does gather them");
    }

    #[test]
    fn try_begin_group_leaves_an_open_group_untouched() {
        let mut b = buf("");
        b.begin_group(Arc::from([]));
        b.edit(0..0, "a");
        assert!(
            !b.try_begin_group(|| Arc::from([])),
            "an already-open group is not reopened"
        );
        b.edit(1..1, "b");
        b.seal_group(Arc::from([]));
        assert!(b.undo().is_some());
        assert_eq!(
            b.snapshot.visible_text.to_string(),
            "",
            "the edit after try_begin_group joined the original group"
        );
    }

    #[test]
    fn try_begin_group_opens_a_group_on_an_idle_buffer() {
        let mut b = buf("");
        assert!(
            b.try_begin_group(|| Arc::from([])),
            "a fresh group opens when none is active"
        );
        b.edit(0..0, "a");
        b.edit(1..1, "b");
        b.seal_group(Arc::from([]));
        assert!(b.undo().is_some());
        assert_eq!(
            b.snapshot.visible_text.to_string(),
            "",
            "try_begin_group collapsed the edits into one step like begin_group"
        );
    }

    #[test]
    fn empty_group_leaves_no_undo_step() {
        let mut b = buf("hi");
        b.edit(2..2, "!");
        b.begin_group(Arc::from([]));
        b.seal_group(Arc::from([]));
        assert!(b.undo().is_some());
        assert_eq!(
            b.snapshot.visible_text.to_string(),
            "hi",
            "a sealed group that took no edits is not its own undo step"
        );
    }

    #[test]
    fn ungrouped_edits_undo_individually() {
        let mut b = buf("");
        b.edit(0..0, "a");
        b.edit(1..1, "b");
        assert!(b.undo().is_some());
        assert_eq!(
            b.snapshot.visible_text.to_string(),
            "a",
            "an edit outside a group is its own step"
        );
    }

    /// Every dispatched action opens a group, so the capture happens per
    /// keystroke and copying the set would cost the cursor count each time.
    /// Storing the handle makes it a refcount bump, and pointer identity is the
    /// only way to tell the two apart from outside.
    #[test]
    fn opening_a_group_shares_the_selection_set_rather_than_copying_it() {
        let mut b = buf("hello");
        let anchor = b.anchor_at(2, Bias::Right);
        let selections: Arc<[Selection<Anchor>]> = Arc::from([Selection {
            id: 7,
            start: anchor,
            end: anchor,
            reversed: false,
            goal: SelectionGoal::None,
        }]);

        b.begin_group(Arc::clone(&selections));

        assert!(
            Arc::ptr_eq(&selections, &b.open_group_before),
            "the group holds the caller's list, not a copy of it"
        );
    }

    #[test]
    fn undo_returns_the_groups_before_selections() {
        let mut b = buf("hello");
        let anchor = b.anchor_at(2, Bias::Right);
        let before: Arc<[Selection<Anchor>]> = Arc::from([Selection {
            id: 7,
            start: anchor,
            end: anchor,
            reversed: false,
            goal: SelectionGoal::None,
        }]);
        b.begin_group(before);
        b.edit(5..5, " world");
        b.seal_group(Arc::from([]));
        let restored = b.undo().expect("undo returns the group's selections");
        assert_eq!(restored.len(), 1);
        assert_eq!(restored[0].id, 7);
        assert_eq!(
            b.resolve_anchor(&restored[0].start),
            2,
            "the restored anchor tracks the pre-edit offset"
        );
    }

    #[test]
    fn new_edit_clears_redo_stack() {
        let mut b = buf("a");
        b.edit(1..1, "b");
        assert_eq!(b.snapshot.visible_text.to_string(), "ab");
        assert!(b.undo().is_some());
        assert_eq!(b.snapshot.visible_text.to_string(), "a");
        b.edit(1..1, "X");
        assert_eq!(b.snapshot.visible_text.to_string(), "aX");
        assert!(b.redo().is_none(), "redo stack cleared by new edit");
        assert_eq!(b.snapshot.visible_text.to_string(), "aX");
    }

    #[test]
    fn undo_back_to_saved_clears_dirty() {
        let mut b = buf("hello");
        assert!(!b.dirty);
        b.edit(5..5, " world");
        assert!(b.dirty);
        b.undo();
        assert!(!b.dirty, "undo back to saved content clears dirty");
        b.redo();
        assert!(b.dirty, "redo away from saved content sets dirty");
    }

    #[test]
    fn deleting_a_typed_char_reads_clean() {
        let mut b = buf("hello");
        b.edit(5..5, "x");
        assert!(b.dirty);
        b.edit(5..6, "");
        assert!(
            !b.dirty,
            "content back on the saved bytes reads clean despite a moved frontier"
        );
    }

    /// The two ropes are chunked however their edits left them, so the walk
    /// has to pair bytes rather than chunks. Long enough to span many chunks,
    /// with the difference in the last byte so nothing short-circuits early.
    #[test]
    fn the_chunk_walk_pairs_bytes_across_unlike_layouts() {
        let text = "the quick brown fox jumps over the lazy dog\n".repeat(40);

        let one_push = stoat_text::Rope::from(text.as_str());
        let in_pieces = {
            let mut rope = stoat_text::Rope::new();
            for piece in text.as_bytes().chunks(7) {
                rope.push(std::str::from_utf8(piece).expect("ascii fixture"));
            }
            rope
        };
        assert!(
            super::chunk_streams_match(one_push.chunks(), in_pieces.chunks()),
            "the same bytes match however each rope was built",
        );

        let mut differs = text.clone();
        differs.pop();
        differs.push('X');
        let differs = stoat_text::Rope::from(differs.as_str());
        assert!(
            !super::chunk_streams_match(one_push.chunks(), differs.chunks()),
            "a difference in the last byte is still a difference",
        );

        assert!(
            !super::chunk_streams_match(one_push.chunks(), stoat_text::Rope::new().chunks()),
            "and a spent stream does not match a full one",
        );
    }

    /// A buffer opened from an empty file reads clean once its content is back
    /// to the empty bytes on disk.
    ///
    /// The round trip leaves the frontier moved, so only the content check can
    /// clear it, and that check answers false whenever no baseline was ever
    /// recorded. A non-empty file records one and heals. An empty one has to as
    /// well, or it reports modified while holding exactly what was saved.
    #[test]
    fn round_trip_edit_on_an_empty_file_reads_clean() {
        let mut b = buf("");
        b.edit(0..0, "a");
        assert!(
            b.dirty,
            "inserting a character diverges from the empty file"
        );

        b.edit(0..1, "");
        assert_eq!(b.snapshot.visible_text.to_string(), "");
        assert!(!b.dirty, "the content is the empty bytes on disk again");
    }

    #[test]
    fn edit_leaving_changed_content_reads_dirty() {
        let mut b = buf("hello");
        b.edit(5..5, "x");
        b.edit(0..1, "");
        assert_eq!(b.snapshot.visible_text.to_string(), "ellox");
        assert!(b.dirty, "same length but different bytes stays modified");
    }

    #[test]
    fn restored_buffer_stays_dirty_until_mark_clean() {
        let mut b = buf("hello");
        b.edit(5..5, "x");
        b.edit(5..6, "");
        assert!(!b.dirty, "the live buffer is clean via the content match");

        let mut restored = TextBuffer::from_history(BufferId::new(0), &b.history());
        assert!(
            restored.dirty,
            "a restored buffer has no saved text so the diverged frontier reads dirty"
        );
        restored.mark_clean();
        assert!(
            !restored.dirty,
            "mark_clean recaptures saved text and clears dirty"
        );
    }

    #[test]
    fn undo_on_a_freshly_loaded_file_is_a_noop() {
        let mut b = buf("hello");
        assert!(b.undo().is_none(), "the seeded load is not an undo target");
        assert_eq!(b.snapshot.visible_text.to_string(), "hello");
    }

    #[test]
    fn undo_reverts_user_edits_then_stops_at_the_seed() {
        let mut b = buf("hello");
        b.edit(5..5, "!");
        assert!(b.undo().is_some(), "the user edit undoes");
        assert_eq!(b.snapshot.visible_text.to_string(), "hello");
        assert!(b.undo().is_none(), "undo stops at the seeded baseline");
        assert_eq!(b.snapshot.visible_text.to_string(), "hello");
    }

    /// A loaded file lives in the rope, so the op log holds no second copy of
    /// it, while what persists still carries the text a restore replays.
    ///
    /// Emptiness alone would not say the memory went. A string emptied without
    /// releasing its buffer reads empty and still holds the file, so capacity
    /// is what says nothing was kept.
    #[test]
    fn the_load_lives_in_the_rope_and_persists_from_it() {
        let content = "seeded contents\nsecond line\n";
        let mut b = buf(content);
        b.edit(0..0, "user edit\n");

        let BufferOp::Edit { old, text } = &b.ops[0] else {
            panic!("the log opens with the load")
        };
        assert_eq!(*old, 0..0, "the load covers the empty buffer");
        assert_eq!(
            text.capacity(),
            0,
            "the load keeps no allocation of its own"
        );

        let history = b.history();
        let BufferOp::Edit { old, text } = &history.ops[0] else {
            panic!("the persisted log opens with the load")
        };
        assert_eq!((old.clone(), text.as_str()), (0..0, content));

        let restored = TextBuffer::from_history(BufferId::new(0), &history);
        assert_eq!(
            restored.snapshot.visible_text.to_string(),
            b.snapshot.visible_text.to_string(),
        );
        assert!(
            restored.seed_text.is_none(),
            "a replayed log has no load to stand in for",
        );
    }

    #[test]
    fn undo_floor_survives_a_history_round_trip() {
        let history = buf("hello").history();
        let mut restored = TextBuffer::from_history(BufferId::new(0), &history);
        assert!(
            restored.undo().is_none(),
            "the restored seed stays protected"
        );
        assert_eq!(restored.snapshot.visible_text.to_string(), "hello");
    }

    /// The threshold is a boundary, so a log that merely reaches it must still
    /// persist whole. Asserting only the compacted side would pass against a
    /// buffer that compacts every history it hands out.
    #[test]
    fn an_op_log_past_the_threshold_persists_as_one_seed_edit() {
        let mut b = buf("");
        while b.ops.len() < OPS_COMPACT_THRESHOLD {
            let end = b.snapshot.visible_text.len();
            b.edit(end..end, "x");
        }

        let full = b.history();
        assert!(!full.compacted, "a log at the threshold persists whole");
        assert_eq!(full.ops.len(), OPS_COMPACT_THRESHOLD);

        let end = b.snapshot.visible_text.len();
        b.edit(end..end, "y");

        let compacted = b.history();
        assert!(compacted.compacted, "one op past the threshold compacts");
        assert_eq!(compacted.ops.len(), 1, "the log persists as a single op");

        let mut restored = TextBuffer::from_history(BufferId::new(0), &compacted);
        assert_eq!(
            restored.snapshot.visible_text.to_string(),
            b.snapshot.visible_text.to_string(),
            "the seed restores the text the log built"
        );
        assert!(
            restored.undo().is_none(),
            "the seed is the oldest reachable state"
        );
    }

    /// A clean buffer must not reopen with unsaved changes it does not have,
    /// and a dirty one must still prompt.
    #[test]
    fn a_compacted_history_carries_the_dirty_state() {
        let mut b = buf("seed\n");
        while b.ops.len() <= OPS_COMPACT_THRESHOLD {
            let end = b.snapshot.visible_text.len();
            b.edit(end..end, "x");
        }

        let dirty = TextBuffer::from_history(BufferId::new(0), &b.history());
        assert!(dirty.dirty, "unsaved edits restore modified");

        b.mark_clean();
        let clean = TextBuffer::from_history(BufferId::new(0), &b.history());
        assert!(!clean.dirty, "a saved buffer restores unmodified");
    }

    #[test]
    fn undo_to_pristine_empty_clears_dirty() {
        let mut b = TextBuffer::new(BufferId::new(0));
        b.edit(0..0, "x");
        assert!(b.dirty);
        b.undo();
        assert!(
            !b.dirty,
            "undo back to the pristine empty state clears dirty"
        );
    }

    #[test]
    fn mark_clean_rebaselines_dirty() {
        let mut b = buf("a");
        b.edit(1..1, "b");
        b.mark_clean();
        assert!(!b.dirty);
        b.edit(2..2, "c");
        assert!(b.dirty);
        b.undo();
        assert!(!b.dirty, "undo to the marked frontier is clean");
        b.undo();
        assert!(b.dirty, "undo past the marked frontier is dirty");
    }

    #[test]
    fn checkpoint_records_initial_op_index() {
        let mut b = TextBuffer::new(BufferId::new(0));
        let id = b.checkpoint(None);
        let cps = b.checkpoints();
        assert_eq!(cps.len(), 1);
        assert_eq!(cps[0].id, id);
        assert_eq!(cps[0].op_index, 0);
        assert_eq!(cps[0].label, None);
    }

    #[test]
    fn checkpoint_records_op_index_after_edits() {
        let mut b = buf("hi");
        b.edit(2..2, "!");
        b.edit(0..0, "X");
        b.checkpoint(None);
        assert_eq!(b.checkpoints()[0].op_index, 3);
    }

    #[test]
    fn checkpoint_ids_are_monotonic() {
        let mut b = buf("hello");
        let a = b.checkpoint(None);
        b.edit(0..0, "X");
        let c = b.checkpoint(None);
        b.edit(0..0, "Y");
        let d = b.checkpoint(None);
        let ids: Vec<_> = b.checkpoints().iter().map(|cp| cp.id).collect();
        assert_eq!(ids, vec![a, c, d]);
        assert!(a.0 < c.0 && c.0 < d.0);
    }

    #[test]
    fn checkpoint_preserves_label() {
        let mut b = buf("hello");
        b.checkpoint(Some("before refactor".to_string()));
        b.checkpoint(None);
        let cps = b.checkpoints();
        assert_eq!(cps[0].label.as_deref(), Some("before refactor"));
        assert_eq!(cps[1].label, None);
    }

    #[test]
    fn undoing_a_delete_restores_that_deletes_own_bytes() {
        let mut b = buf("abcdef");

        b.edit(4..5, "");
        b.edit(1..2, "");
        assert_eq!(b.snapshot.visible_text.to_string(), "acdf");

        b.undo();
        assert_eq!(
            b.snapshot.visible_text.to_string(),
            "abcdf",
            "undo restored another deletion's bytes"
        );
    }

    struct Lcg(u64);

    impl Lcg {
        fn below(&mut self, n: usize) -> usize {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            ((self.0 >> 33) % n as u64) as usize
        }
    }

    /// Each fragment in tree order as its insertion timestamp, its offset into
    /// that insertion, whether it is visible, and its bytes read from whichever
    /// rope the flag claims holds them.
    ///
    /// Asserts on the way that neither rope carries bytes past the fragments
    /// claiming them, which is what a mis-copied span leaves behind.
    fn fragment_spans(b: &TextBuffer) -> Vec<(u64, u32, bool, String)> {
        let cx = &None;
        let mut visible_offset = 0usize;
        let mut deleted_offset = 0usize;
        let mut spans = Vec::new();

        for fragment in b.snapshot.fragments.cursor::<()>(cx) {
            let len = fragment.len as usize;
            let text = if fragment.visible {
                let slice = b
                    .snapshot
                    .visible_text
                    .slice(visible_offset..visible_offset + len);
                visible_offset += len;
                slice.to_string()
            } else {
                let slice = b
                    .snapshot
                    .deleted_text
                    .slice(deleted_offset..deleted_offset + len);
                deleted_offset += len;
                slice.to_string()
            };

            spans.push((
                fragment.timestamp,
                fragment.insertion_offset,
                fragment.visible,
                text,
            ));
        }

        assert_eq!(
            visible_offset,
            b.snapshot.visible_text.len(),
            "visible rope holds bytes no fragment claims"
        );
        assert_eq!(
            deleted_offset,
            b.snapshot.deleted_text.len(),
            "deleted rope holds bytes no fragment claims"
        );

        spans
    }

    /// The structural invariants of the fragment and insertion trees.
    ///
    /// [`fragment_spans`] reads each fragment's bytes out of the two ropes, so
    /// it proves the ropes and the fragment lengths agree and nothing more.
    /// Nothing reads the insertions tree at all. A split that re-records an
    /// entry against the wrong fragment leaves the text correct and surfaces
    /// only later, through whichever anchor path consults the broken entry.
    ///
    /// Both directions of the fragment-to-insertion correspondence are read.
    /// Either alone passes for a tree whose entries were duplicated rather than
    /// moved, because the surviving copy still answers the lookup.
    fn check_invariants(b: &TextBuffer, context: &str) {
        use std::collections::HashMap;
        use stoat_text::{InsertionFragmentKey, KeyedItem, Locator};

        let cx = &None;
        let snap = &b.snapshot;

        let fragments: Vec<&super::Fragment> = snap.fragments.cursor::<()>(cx).collect();

        // The tree is ordered by locator, so a duplicate or out-of-order id
        // breaks every seek without necessarily changing the text.
        for pair in fragments.windows(2) {
            assert!(
                pair[0].id < pair[1].id,
                "{context}: fragment ids are not ascending, {:?} then {:?}",
                pair[0].id,
                pair[1].id,
            );
        }

        let by_id: HashMap<&Locator, &super::Fragment> =
            fragments.iter().map(|f| (&f.id, *f)).collect();

        for fragment in &fragments {
            // The tree opens with an empty sentinel that stands for the
            // position before all text. It comes from no insertion, so the
            // insertions tree deliberately holds no entry for it.
            if fragment.id == Locator::min() {
                continue;
            }

            let key = InsertionFragmentKey {
                timestamp: fragment.timestamp,
                split_offset: fragment.insertion_offset,
            };
            let mut cursor = snap.insertions.cursor::<InsertionFragmentKey>(());
            cursor.seek(&key, Bias::Left);
            let entry = cursor
                .item()
                .filter(|entry| entry.key() == key)
                .unwrap_or_else(|| panic!("{context}: no insertion entry for {key:?}"));
            assert_eq!(
                entry.fragment_id, fragment.id,
                "{context}: insertion entry for {key:?} names another fragment",
            );
        }

        for entry in snap.insertions.cursor::<()>(()) {
            let fragment = by_id
                .get(&entry.fragment_id)
                .unwrap_or_else(|| panic!("{context}: insertion entry names a missing fragment"));
            assert_eq!(
                fragment.insertion_offset, entry.split_offset,
                "{context}: insertion entry and fragment disagree on the offset",
            );
        }

        // Maintained incrementally as the tree is rebuilt, so it can drift from
        // the text it describes.
        let summary = snap.fragments.summary();
        assert_eq!(
            summary.text.visible,
            snap.visible_text.len(),
            "{context}: summary disagrees with the visible rope",
        );
        assert_eq!(
            summary.text.deleted,
            snap.deleted_text.len(),
            "{context}: summary disagrees with the deleted rope",
        );
    }

    /// Assert the full contract of an undo or redo rebuild.
    ///
    /// Every fragment survives in order carrying the same bytes, each one's
    /// visibility agrees with the undo map, and the ones that flipped are
    /// stamped with the resulting version.
    fn assert_toggle_preserved(before: &[(u64, u32, bool, String)], b: &TextBuffer) {
        let after = fragment_spans(b);
        assert_eq!(before.len(), after.len(), "fragment count changed");

        for (i, (was, now)) in before.iter().zip(&after).enumerate() {
            assert_eq!(
                (was.0, was.1, &was.3),
                (now.0, now.1, &now.3),
                "fragment {i} lost its identity or its bytes"
            );
        }

        // The head sentinel carries no bytes, so which rope it claims is not
        // observable and no rebuild has to agree with the undo map about it.
        let cx = &None;
        for (i, fragment) in b
            .snapshot
            .fragments
            .cursor::<()>(cx)
            .enumerate()
            .filter(|(_, fragment)| fragment.len > 0)
        {
            assert_eq!(
                fragment.visible,
                fragment.is_visible_with_undos(&b.snapshot.undo_map),
                "fragment {i} visibility disagrees with the undo map"
            );

            if before[i].2 != fragment.visible {
                assert_eq!(
                    fragment.max_undos,
                    b.version(),
                    "fragment {i} flipped without being stamped"
                );
            }
        }
    }

    #[test]
    fn undo_and_redo_preserve_every_fragment_and_its_bytes() {
        const INSERTS: [&str; 6] = ["", "x", "hello ", "\n", "\u{65e5}\u{672c}", "  \n  "];

        let mut rng = Lcg(0x9E37_79B9_7F4A_7C15);

        for _ in 0..64 {
            let mut b = buf("the quick brown fox\njumps over the lazy dog\nsphinx of quartz\n");

            for _ in 0..30 {
                match rng.below(10) {
                    0..=5 => {
                        b.begin_group(Arc::from([]));

                        for _ in 0..1 + rng.below(3) {
                            let text = b.snapshot.visible_text.to_string();
                            let mut bounds: Vec<usize> =
                                text.char_indices().map(|(i, _)| i).collect();
                            bounds.push(text.len());

                            let a = bounds[rng.below(bounds.len())];
                            let z = bounds[rng.below(bounds.len())];
                            b.edit(a.min(z)..a.max(z), INSERTS[rng.below(INSERTS.len())]);
                        }

                        b.seal_group(Arc::from([]));
                    },
                    6..=8 => {
                        let before = fragment_spans(&b);
                        b.undo();
                        assert_toggle_preserved(&before, &b);
                    },
                    _ => {
                        let before = fragment_spans(&b);
                        b.redo();
                        assert_toggle_preserved(&before, &b);
                    },
                }

                check_invariants(&b, "after a random operation");
            }
        }
    }

    /// The text a buffer should be showing, modeled as a string and two stacks
    /// of strings that know nothing of fragments, ropes, or timestamps.
    ///
    /// The fragment fuzz above checks the ropes against the fragments, so it
    /// still holds for a buffer that is wrong about its text in a way its own
    /// bookkeeping agrees with. Agreeing with this model is a claim about the
    /// text itself, which is what the deleted rope's order decides.
    struct TextStack {
        current: String,
        undone: Vec<String>,
        redone: Vec<String>,
    }

    impl TextStack {
        /// Seed text is not an undo target, matching `with_text` flooring its
        /// history above the edit that seeded it.
        fn new(text: &str) -> Self {
            Self {
                current: text.to_owned(),
                undone: Vec::new(),
                redone: Vec::new(),
            }
        }

        /// Open an undo step over the edits that follow, whose bytes the buffer
        /// restores in one batch.
        fn begin_group(&mut self) {
            self.redone.clear();
            self.undone.push(self.current.clone());
        }

        fn edit(&mut self, range: Range<usize>, text: &str) {
            self.current.replace_range(range, text);
        }

        fn undo(&mut self) {
            if let Some(previous) = self.undone.pop() {
                self.redone.push(mem::replace(&mut self.current, previous));
            }
        }

        fn redo(&mut self) {
            if let Some(next) = self.redone.pop() {
                self.undone.push(mem::replace(&mut self.current, next));
            }
        }
    }

    /// A char boundary of `text` chosen by `rng`, its end included, so a random
    /// range never splits a character.
    fn random_boundary(rng: &mut Lcg, text: &str) -> usize {
        let mut bounds: Vec<usize> = text.char_indices().map(|(i, _)| i).collect();
        bounds.push(text.len());
        bounds[rng.below(bounds.len())]
    }

    /// Two or three ranges legal for [`TextBuffer::edit_batch`], paired with the
    /// text to put in each.
    ///
    /// Cut points are deduped before pairing, so no two ranges share an offset
    /// and they are strictly disjoint. Reversing gives the descending order the
    /// batch takes its ranges in.
    ///
    /// Some pairs collapse to an empty range, which is the multi-caret shape.
    /// That stays in contract. What `edit_batch` rejects is an empty range
    /// sharing an offset with a deleting range's start, and distinct cut points
    /// rule that out.
    fn random_batch(
        rng: &mut Lcg,
        text: &str,
        inserts: &[&'static str],
    ) -> Vec<(Range<usize>, &'static str)> {
        let mut cuts: Vec<usize> = (0..(2 + rng.below(2)) * 2)
            .map(|_| random_boundary(rng, text))
            .collect();
        cuts.sort_unstable();
        cuts.dedup();

        let mut edits: Vec<(Range<usize>, &'static str)> = cuts
            .chunks_exact(2)
            .map(|pair| {
                let end = if rng.below(3) == 0 { pair[0] } else { pair[1] };
                (pair[0]..end, inserts[rng.below(inserts.len())])
            })
            .collect();

        edits.reverse();
        edits
    }

    #[test]
    fn edits_undos_and_redos_show_the_text_they_should() {
        const INSERTS: [&str; 6] = ["", "z", "hello ", "\n", "\u{65e5}\u{672c}", "  \n  "];
        const SEED: &str = "the quick brown fox\njumps over the lazy dog\nsphinx of quartz\n";

        let mut rng = Lcg(0x243F_6A88_85A3_08D3);

        for _ in 0..64 {
            let mut b = buf(SEED);
            let mut model = TextStack::new(SEED);

            for step in 0..30 {
                match rng.below(10) {
                    0..=4 => {
                        b.begin_group(Arc::from([]));
                        model.begin_group();

                        for _ in 0..1 + rng.below(3) {
                            let text = b.snapshot.visible_text.to_string();
                            let a = random_boundary(&mut rng, &text);
                            let z = random_boundary(&mut rng, &text);
                            let insert = INSERTS[rng.below(INSERTS.len())];

                            b.edit(a.min(z)..a.max(z), insert);
                            model.edit(a.min(z)..a.max(z), insert);
                        }

                        b.seal_group(Arc::from([]));
                    },
                    5..=6 => {
                        // The batch arrives on whatever tree the steps before it
                        // left, which is what puts it over toggled fragments and
                        // in among undos.
                        let text = b.snapshot.visible_text.to_string();
                        let edits = random_batch(&mut rng, &text, &INSERTS);

                        // A short buffer can leave too few distinct cut points to
                        // pair, and an empty group is not the shape under test.
                        if !edits.is_empty() {
                            b.begin_group(Arc::from([]));
                            model.begin_group();

                            b.edit_batch(&edits);
                            // Right to left, the order the batch takes them in
                            // and the order it is documented to match.
                            for (range, insert) in &edits {
                                model.edit(range.clone(), insert);
                            }

                            b.seal_group(Arc::from([]));
                        }
                    },
                    7..=8 => {
                        b.undo();
                        model.undo();
                    },
                    _ => {
                        b.redo();
                        model.redo();
                    },
                }

                assert_eq!(
                    b.snapshot.visible_text.to_string(),
                    model.current,
                    "step {step} left the buffer showing text no edit history explains"
                );
                check_invariants(&b, &format!("step {step}"));
            }
        }
    }

    /// A replay hands back one undo step per edit, not one per session, and the
    /// floor that protects the seed content still lands in the right place.
    ///
    /// Grouping is a session overlay that is never persisted, so the same
    /// history that undid an insert in one step undoes it in as many steps as
    /// it had edits. The floor is the sharper half: it counts groups, and the
    /// replay changes how many groups the same edits make, so a floor off by
    /// one would either undo the seed content away or refuse an undo that
    /// should have been allowed.
    #[test]
    fn a_replayed_buffer_undoes_edit_by_edit_and_keeps_its_floor() {
        let mut b = buf("seed\n");

        b.begin_group(Arc::from([]));
        b.edit(5..5, "one\n");
        b.edit(9..9, "two\n");
        b.edit(13..13, "three\n");
        b.seal_group(Arc::from([]));
        let edited = b.snapshot.visible_text.to_string();
        assert_eq!(edited, "seed\none\ntwo\nthree\n");

        assert!(b.undo().is_some(), "the session's three edits undo as one");
        assert_eq!(b.snapshot.visible_text.to_string(), "seed\n");
        assert!(
            b.undo().is_none(),
            "and the floor refuses to undo the seed content away",
        );

        // Back to the edited state, so the history the replay reads is the one
        // the session saved rather than one already unwound.
        b.redo();
        assert_eq!(b.snapshot.visible_text.to_string(), edited);

        let mut restored = TextBuffer::from_history(BufferId::new(0), &b.history());
        assert_eq!(restored.snapshot.visible_text.to_string(), edited);

        for want in ["seed\none\ntwo\n", "seed\none\n", "seed\n"] {
            assert!(
                restored.undo().is_some(),
                "the replay still has {want:?} to reach"
            );
            assert_eq!(
                restored.snapshot.visible_text.to_string(),
                want,
                "the replay steps through the states the session skipped",
            );
        }
        assert!(
            restored.undo().is_none(),
            "the floor carried across the regrouping and still holds the seed",
        );
    }

    /// A replayed buffer resolves the original's anchors to the same offsets.
    ///
    /// That is what a restored session's cursors rest on, and text equality
    /// does not imply it. An anchor names a fragment and an offset inside it,
    /// so two trees holding the same bytes split into different fragments
    /// satisfy every round-trip check there is and still place a restored
    /// cursor somewhere else. The live tree reaches its shape through
    /// `edit_batch` and `split_at_boundaries`, which the sequential replay
    /// never runs, so only the timestamps line the two up.
    ///
    /// Anchors are taken twice, so the set includes ones made against a tree
    /// that later edits went on to move, and at both biases, since bias is what
    /// decides the side of an insertion an anchor falls on.
    #[test]
    fn a_replayed_buffer_resolves_the_anchors_the_original_made() {
        const INSERTS: [&str; 6] = ["", "z", "hello ", "\n", "\u{65e5}\u{672c}", "  \n  "];
        const SEED: &str = "the quick brown fox\njumps over the lazy dog\nsphinx of quartz\n";

        let mut rng = Lcg(0x5851_F42D_4C95_7F2D);

        let sample = |rng: &mut Lcg, b: &TextBuffer| -> Vec<Anchor> {
            let text = b.snapshot.visible_text.to_string();
            (0..6)
                .flat_map(|_| {
                    let offset = random_boundary(rng, &text);
                    [
                        b.anchor_at(offset, Bias::Left),
                        b.anchor_at(offset, Bias::Right),
                    ]
                })
                .collect()
        };

        for round in 0..48 {
            let mut b = buf(SEED);
            let mut early: Vec<Anchor> = Vec::new();

            for step in 0..20 {
                match rng.below(10) {
                    0..=4 => {
                        let text = b.snapshot.visible_text.to_string();
                        let a = random_boundary(&mut rng, &text);
                        let z = random_boundary(&mut rng, &text);
                        b.edit(a.min(z)..a.max(z), INSERTS[rng.below(INSERTS.len())]);
                    },
                    5..=6 => {
                        let text = b.snapshot.visible_text.to_string();
                        let edits = random_batch(&mut rng, &text, &INSERTS);
                        if !edits.is_empty() {
                            b.edit_batch(&edits);
                        }
                    },
                    7..=8 => {
                        b.undo();
                    },
                    _ => {
                        b.redo();
                    },
                }

                if step == 10 {
                    early = sample(&mut rng, &b);
                }
            }

            let late = sample(&mut rng, &b);
            let restored = TextBuffer::from_history(BufferId::new(0), &b.history());

            // The anchors first. Text and version agreeing is the weaker claim,
            // and asserting them ahead would report a shifted replay as a text
            // or version difference rather than as the moved cursors it is.
            for (which, anchors) in [("mid-run", &early), ("final", &late)] {
                let want: Vec<usize> = anchors.iter().map(|a| b.resolve_anchor(a)).collect();
                let got: Vec<usize> = anchors.iter().map(|a| restored.resolve_anchor(a)).collect();
                assert_eq!(
                    got, want,
                    "round {round}: {which} anchors move in the replay"
                );
            }

            assert_eq!(
                restored.snapshot.visible_text.to_string(),
                b.snapshot.visible_text.to_string(),
                "round {round}: the replay shows different text",
            );
            assert_eq!(
                restored.version(),
                b.version(),
                "round {round}: the replay ends on a different version",
            );

            let mut b = b;
            let mut restored = restored;
            assert_eq!(
                restored.redo().is_some(),
                b.redo().is_some(),
                "round {round}: the replay disagrees about there being a redo",
            );
        }
    }

    #[test]
    fn undoing_a_delete_that_spans_multibyte_text_restores_it() {
        // Deleting the shorter run first, later in the document, then the
        // longer multibyte run before it. Read back in the order the deletions
        // happened rather than the document's, the first fragment's span ends
        // inside a character.
        let mut b = buf("ab\u{65e5}\u{672c}cd");

        b.edit(8..9, "");
        b.edit(2..8, "");
        assert_eq!(b.snapshot.visible_text.to_string(), "abd");

        b.undo();
        assert_eq!(
            b.snapshot.visible_text.to_string(),
            "ab\u{65e5}\u{672c}d",
            "undo read the deleted rope at a mid-character offset"
        );

        b.undo();
        assert_eq!(b.snapshot.visible_text.to_string(), "ab\u{65e5}\u{672c}cd");
    }

    /// The dirty verdict agrees with comparing every byte, across random edits,
    /// undos and redos after a save.
    ///
    /// The comparison reads only the spans recorded as changed since the saved
    /// version, so a span it fails to name reads clean over a buffer that
    /// differs, and the reader closes it without saving. Reasoning about which
    /// spans those are is what this replaces, over sequences that put the save
    /// point before, between and after the edits that follow it.
    #[test]
    fn the_dirty_verdict_agrees_with_comparing_every_byte() {
        let mut rng = Lcg(0x1234_5678_9abc_def0);

        for round in 0..64 {
            let mut b = buf("alpha beta\ngamma delta\nepsilon zeta\n");

            // Somewhere in the run, so the save lands on a buffer that already
            // has history behind it as often as not.
            let save_at = rng.below(8);

            for step in 0..12 {
                if step == save_at {
                    b.mark_clean();
                }
                random_step(&mut rng, &mut b);

                let saved = b.saved_text.clone();
                let by_bytes = saved
                    .is_some_and(|saved| saved.to_string() == b.snapshot.visible_text.to_string());
                assert_eq!(
                    b.matches_saved_text(),
                    by_bytes,
                    "round {round} step {step}: saved {:?} against {:?}",
                    b.saved_text.as_ref().map(|s| s.to_string()),
                    b.snapshot.visible_text.to_string(),
                );
            }
        }
    }

    /// Two edits that cancel each other out land on the saved bytes, and
    /// nothing joins them into one because they do not touch. Reading them one
    /// at a time finds a delete and an insert of unequal length and calls the
    /// buffer dirty, which is the move-a-line-down-and-back case and costs the
    /// reader a save prompt on a file they did not change.
    #[test]
    fn edits_that_cancel_out_read_clean() {
        let mut b = buf("xyxy");
        b.mark_clean();

        b.edit(0..2, "");
        b.edit(2..2, "xy");

        assert_eq!(b.snapshot.visible_text.to_string(), "xyxy");
        assert!(
            b.matches_saved_text(),
            "the text is the saved text, byte for byte",
        );
    }

    /// A move that swaps two lines keeps every count the summary carries, so it
    /// is the shape the cheap guard cannot answer and the span comparison has to.
    #[test]
    fn swapping_two_lines_reads_dirty() {
        let mut b = buf("alpha\nbravo\n");
        b.mark_clean();
        assert!(!b.dirty, "the save is the baseline");

        b.edit(0..12, "bravo\nalpha\n");

        let before = b.snapshot.visible_text.summary().clone();
        assert_eq!(
            (before.len, before.lines, before.chars),
            (12, Point::new(2, 0), 12),
            "the swap has to leave the counts alone or it proves nothing",
        );
        assert!(b.dirty, "the same bytes in another order is a change");

        b.edit(0..12, "alpha\nbravo\n");
        assert!(!b.dirty, "and swapping them back is not");
    }

    /// One random edit, undo, or redo, weighted so a buffer accumulates
    /// history faster than it unwinds it.
    fn random_step(rng: &mut Lcg, b: &mut TextBuffer) {
        const INSERTS: [&str; 5] = ["", "z", "hello ", "\u{65e5}\u{672c}", "\n  "];

        match rng.below(11) {
            0..=4 => {
                let text = b.snapshot.visible_text.to_string();
                let a = random_boundary(rng, &text);
                let z = random_boundary(rng, &text);
                b.edit(a.min(z)..a.max(z), INSERTS[rng.below(INSERTS.len())]);
            },
            5..=6 => {
                let text = b.snapshot.visible_text.to_string();
                b.edit_batch(&random_batch(rng, &text, &INSERTS));
            },
            7..=8 => {
                b.undo();
            },
            9 => {
                b.redo();
            },
            // Back onto the saved bytes without undoing to them. Undo is the
            // only other way a run reaches them, and it rewinds the recorded
            // edits along with the text, so a walk over those edits is never
            // asked to answer for a buffer that arrived by editing.
            _ => {
                if let Some(saved) = b.saved_text.as_ref().map(|s| s.to_string()) {
                    let len = b.snapshot.visible_text.len();
                    b.edit(0..len, &saved);
                }
            },
        }
    }

    #[test]
    fn edits_since_reconstructs_across_undo_and_redo() {
        const SEED: &str = "aaaa bbbb cccc\ndddd eeee ffff\ngggg hhhh iiii\n";

        let mut rng = Lcg(0xB7E1_5162_8AED_2A6B);

        for round in 0..64 {
            let mut b = buf(SEED);

            // The version a caller holds is taken mid-history, not at a
            // pristine buffer. Only then can the span it asks about contain an
            // undo of its own, which is what makes a fragment's visibility at
            // that version a question the undo map has to answer.
            for _ in 0..8 {
                random_step(&mut rng, &mut b);
            }

            let old_text = b.snapshot.visible_text.to_string();
            let v0 = b.version();

            for _ in 0..12 {
                random_step(&mut rng, &mut b);
            }

            let new_text = b.snapshot.visible_text.to_string();
            let patch = b.snapshot.edits_since(v0);

            let mut reconstructed = old_text.clone();
            for edit in patch.edits().iter().rev() {
                reconstructed.replace_range(edit.old.clone(), &new_text[edit.new.clone()]);
            }

            assert_eq!(
                reconstructed,
                new_text,
                "round {round}: the patch does not carry the old text to the new one, patch={:?}",
                patch.edits()
            );
        }
    }
}
