use crate::diff_map::DiffMap;
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::HashMap, ops::Range, sync::Arc};
pub use stoat_text::BufferId;
use stoat_text::{
    patch::{Edit, Patch},
    Anchor, Bias, Dimensions, Fragment, IndentStyle, InsertionFragment, InsertionFragmentKey,
    Locator, Point, Rope, Selection, SumTree, UndoMap, UndoOperation,
};

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
    open_group_before: Vec<Selection<Anchor>>,
    /// Chronological log of user-driven mutations. Replaying this on a fresh
    /// [`TextBuffer`] reconstructs an identical fragment tree, anchors, and
    /// undo map, which is how workspace save/restore preserves selections and
    /// undo stack across sessions.
    ops: Vec<BufferOp>,
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
#[derive(Clone, Debug, Serialize, Deserialize)]
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
    selections_before: Vec<Selection<Anchor>>,
    /// Editor selections captured when the group sealed, restored on redo.
    selections_after: Vec<Selection<Anchor>>,
}

/// Serializable buffer state for persistence. Holds the op log plus the
/// last-clean edit frontier, replayed via [`TextBuffer::from_history`].
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
    buffer_id: BufferId,
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
            diff_map: None,
            next_timestamp: 1,
            buffer_id,
            edit_history: Vec::new(),
            redo_history: Vec::new(),
            undo_floor: 0,
            open_group: false,
            open_group_started: false,
            open_group_before: Vec::new(),
            ops: Vec::new(),
            next_checkpoint_id: 0,
            checkpoints: Vec::new(),
            indent_style: IndentStyle::default(),
        }
    }

    pub fn with_text(buffer_id: BufferId, text: &str) -> Self {
        let mut buf = Self::new(buffer_id);
        if !text.is_empty() {
            buf.edit(0..0, text);
            buf.mark_clean();
            buf.undo_floor = buf.edit_history.len();
        }
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
        let mut deleted_rope = DeletedRebuild::new(range.start);

        // Copy all fragments before the edit start
        let prefix = cursor.slice(&range.start, Bias::Right);
        deleted_rope.carry(&self.snapshot.deleted_text, prefix.summary().text.deleted);
        new_fragments.append(prefix, cx);

        let mut delete_remaining = range.end - range.start;
        let overshoot = cursor.item().map_or(0, |_| range.start - *cursor.start());

        if let Some(fragment) = cursor.item().filter(|f| overshoot > 0 && f.visible) {
            let prefix = Fragment {
                id: Locator::between(last_id(&new_fragments, cx), &fragment.id),
                timestamp: fragment.timestamp,
                insertion_offset: fragment.insertion_offset,
                len: overshoot as u32,
                visible: true,
                deletions: fragment.deletions.clone(),
                max_undos: fragment.max_undos,
            };
            push_insertion(&mut new_insertions, &prefix);
            new_fragments.push(prefix, cx);
        }

        // The new text goes ahead of everything this edit deletes, so an anchor
        // inside the replaced range resolves past the replacement rather than
        // before it. Fragments already deleted at this position fall behind it
        // for the same reason. They stand for text that is gone, and the new
        // text takes its place.
        if !text.is_empty() {
            let next_id = cursor.item().map(|f| &f.id).unwrap_or(Locator::max_ref());
            let new_frag_id = Locator::between(last_id(&new_fragments, cx), next_id);
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
            if fragment.visible {
                let fragment_visible_len = fragment.len as usize;
                let remaining_in_fragment = fragment_visible_len - overshoot;
                let to_delete_here = delete_remaining.min(remaining_in_fragment);

                if to_delete_here > 0 {
                    let next_id = cursor
                        .next_item()
                        .map(|f| &f.id)
                        .unwrap_or(Locator::max_ref());
                    let mut deleted = fragment.clone();
                    deleted.id = Locator::between(last_id(&new_fragments, cx), next_id);
                    deleted.insertion_offset = fragment.insertion_offset + overshoot as u32;
                    deleted.len = to_delete_here as u32;
                    deleted.visible = false;
                    deleted.deletions.push(timestamp);
                    push_insertion(&mut new_insertions, &deleted);
                    new_fragments.push(deleted, cx);
                    deleted_rope.take(&self.snapshot.visible_text, to_delete_here);
                    delete_remaining -= to_delete_here;
                }

                let suffix_len = remaining_in_fragment.saturating_sub(to_delete_here);
                if suffix_len > 0 && delete_remaining == 0 {
                    let next_id = cursor
                        .next_item()
                        .map(|f| &f.id)
                        .unwrap_or(Locator::max_ref());
                    let suffix_id = Locator::between(last_id(&new_fragments, cx), next_id);
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
                deleted_rope.carry(&self.snapshot.deleted_text, fragment.len as usize);
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
                        deleted_rope.take(&self.snapshot.visible_text, frag_len);
                        delete_remaining -= frag_len;
                        cursor.next();
                    } else {
                        let mut deleted_part = fragment.clone();
                        deleted_part.id =
                            Locator::between(last_id(&new_fragments, cx), &fragment.id);
                        deleted_part.len = delete_remaining as u32;
                        deleted_part.visible = false;
                        deleted_part.deletions.push(timestamp);
                        push_insertion(&mut new_insertions, &deleted_part);
                        new_fragments.push(deleted_part, cx);
                        deleted_rope.take(&self.snapshot.visible_text, delete_remaining);

                        let next_id = cursor
                            .next_item()
                            .map(|f| &f.id)
                            .unwrap_or(Locator::max_ref());
                        let remaining_id = Locator::between(last_id(&new_fragments, cx), next_id);
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
                    deleted_rope.carry(&self.snapshot.deleted_text, fragment.len as usize);
                    new_fragments.push(fragment.clone(), cx);
                    cursor.next();
                },
                None => break,
            }
        }

        // Copy remaining fragments
        let suffix = cursor.suffix();
        deleted_rope.carry(&self.snapshot.deleted_text, suffix.summary().text.deleted);
        new_fragments.append(suffix, cx);

        // Update insertions tree
        let mut all_insertions = self.snapshot.insertions.clone();
        for ins in new_insertions {
            all_insertions.insert_or_replace(ins, ());
        }

        // Update the rope
        self.snapshot.visible_text.replace(range, text);

        // Store new state
        self.snapshot.deleted_text = deleted_rope.text;
        self.snapshot.fragments = new_fragments;
        self.snapshot.insertions = all_insertions;
        self.snapshot.version = timestamp;
        self.record_edit(timestamp);
        self.recompute_dirty();
    }

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
                selections_after: Vec::new(),
            });
            self.open_group_started = true;
        } else {
            self.edit_history.push(UndoGroup {
                edits: vec![timestamp],
                selections_before: Vec::new(),
                selections_after: Vec::new(),
            });
        }
    }

    /// Open an undo group so the following [`Self::edit`] calls collapse into one
    /// logical step. `selections_before` is the editor selection set to restore
    /// when the group is later undone.
    ///
    /// The group is not materialized until its first edit, so opening one around
    /// a non-editing action costs nothing and leaves the undo history unchanged.
    pub(crate) fn begin_group(&mut self, selections_before: Vec<Selection<Anchor>>) {
        if self.open_group {
            self.seal_group(Vec::new());
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
    pub(crate) fn try_begin_group(&mut self, selections_before: Vec<Selection<Anchor>>) -> bool {
        if self.open_group {
            return false;
        }
        self.open_group = true;
        self.open_group_started = false;
        self.open_group_before = selections_before;
        true
    }

    /// Close the open undo group, recording `selections_after` to restore on
    /// redo. A group that took no edits was never materialized, so a non-editing
    /// action leaves no undo step behind.
    pub(crate) fn seal_group(&mut self, selections_after: Vec<Selection<Anchor>>) {
        if !self.open_group {
            return;
        }
        self.open_group = false;
        self.open_group_before = Vec::new();
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
        self.saved_marker = self.frontier();
        self.saved_text = Some(self.snapshot.visible_text.clone());
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
    /// Equal counts do not mean equal text, so the byte walk still decides
    /// those. It is what a length-preserving edit costs, a case toggle or a
    /// replacement of like for like.
    fn matches_saved_text(&self) -> bool {
        let Some(saved) = &self.saved_text else {
            return false;
        };
        let current = &self.snapshot.visible_text;
        let (saved_summary, current_summary) = (saved.summary(), current.summary());
        saved_summary.len == current_summary.len
            && saved_summary.len_utf16 == current_summary.len_utf16
            && saved_summary.lines == current_summary.lines
            && saved_summary.chars == current_summary.chars
            && saved
                .chunks()
                .flat_map(str::bytes)
                .eq(current.chunks().flat_map(str::bytes))
    }

    /// Undo the top edit group, reverting all of its edits as one step. Returns
    /// the editor selections captured when the group opened, to restore the
    /// cursor to edit time, or `None` when there is nothing to undo.
    ///
    /// The content a buffer was loaded or seeded with is not an undo target, so
    /// undoing a freshly opened file with no user edits returns `None` and
    /// leaves the file intact rather than emptying it.
    pub fn undo(&mut self) -> Option<Vec<Selection<Anchor>>> {
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
    pub fn redo(&mut self) -> Option<Vec<Selection<Anchor>>> {
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

        let mut copied_visible = 0usize;
        let mut copied_deleted = 0usize;

        let mut untouched = old_fragments.cursor::<Option<Locator>>(cx);
        let mut reachable =
            old_fragments.filter::<_, ()>(cx, |summary| summary.max_version >= oldest_toggled);
        reachable.next();

        while let Some(fragment) = reachable.item() {
            let run = untouched.slice(&Some(&fragment.id), Bias::Left);
            let run_visible = run.summary().text.visible;
            let run_deleted = run.summary().text.deleted;

            new_visible.append(
                self.snapshot
                    .visible_text
                    .slice(copied_visible..copied_visible + run_visible),
            );
            new_deleted.append(
                self.snapshot
                    .deleted_text
                    .slice(copied_deleted..copied_deleted + run_deleted),
            );
            copied_visible += run_visible;
            copied_deleted += run_deleted;
            new_fragments.append(run, cx);

            let len = fragment.len as usize;
            let was_visible = fragment.visible;
            let is_visible = fragment.is_visible_with_undos(&self.snapshot.undo_map);

            let text = if was_visible {
                let slice = self
                    .snapshot
                    .visible_text
                    .slice(copied_visible..copied_visible + len);
                copied_visible += len;
                slice
            } else {
                let slice = self
                    .snapshot
                    .deleted_text
                    .slice(copied_deleted..copied_deleted + len);
                copied_deleted += len;
                slice
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

        new_visible.append(
            self.snapshot
                .visible_text
                .slice(copied_visible..self.snapshot.visible_text.len()),
        );
        new_deleted.append(
            self.snapshot
                .deleted_text
                .slice(copied_deleted..self.snapshot.deleted_text.len()),
        );
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
    /// the result with [`Self::from_history`] to reconstruct an identical
    /// buffer.
    pub fn history(&self) -> BufferHistory {
        BufferHistory {
            ops: self.ops.clone(),
            saved_marker: self.saved_marker,
            undo_floor: self.undo_floor,
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

fn last_id<'a>(tree: &'a SumTree<Fragment>, _cx: &Option<u64>) -> &'a Locator {
    tree.last().map(|f| &f.id).unwrap_or(Locator::min_ref())
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
/// each side needs only a running offset. Text already deleted comes from the
/// old deleted rope, and text this edit deletes is still in the visible one.
struct DeletedRebuild {
    text: Rope,
    already_deleted: usize,
    being_deleted: usize,
}

impl DeletedRebuild {
    /// `range_start` is where in the visible rope this edit starts deleting.
    fn new(range_start: usize) -> Self {
        Self {
            text: Rope::new(),
            already_deleted: 0,
            being_deleted: range_start,
        }
    }

    /// Carry over `len` bytes of text that was already deleted and stays so.
    fn carry(&mut self, old_deleted: &Rope, len: usize) {
        let end = self.already_deleted + len;
        self.text
            .append(old_deleted.slice(self.already_deleted..end));
        self.already_deleted = end;
    }

    /// Take `len` bytes that this edit is deleting out of the visible rope.
    fn take(&mut self, old_visible: &Rope, len: usize) {
        let end = self.being_deleted + len;
        self.text.append(old_visible.slice(self.being_deleted..end));
        self.being_deleted = end;
    }
}

fn push_insertion(insertions: &mut Vec<InsertionFragment>, fragment: &Fragment) {
    insertions.push(InsertionFragment {
        timestamp: fragment.timestamp,
        split_offset: fragment.insertion_offset,
        fragment_id: fragment.id.clone(),
    });
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
        let mut indexed: Vec<(usize, usize)> =
            offsets.iter().map(|&o| o.min(len)).enumerate().collect();
        indexed.sort_unstable_by_key(|&(_, o)| o);

        let mut results = vec![Anchor::min_for_buffer(self.buffer_id); offsets.len()];
        let cx = &None;
        let mut cursor = self.fragments.cursor::<usize>(cx);
        for (original_idx, offset) in indexed {
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
        let fragment_id = self.insertion_fragment_id(anchor);

        let cx = &None;
        let target = Some(fragment_id.clone());
        let (start, _end, item) = self
            .fragments
            .find::<Dimensions<Option<Locator>, usize>, _>(cx, &target, Bias::Left);

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

        let mut fragment_ids: Vec<(usize, Locator)> = Vec::with_capacity(pending.len());
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
                                Some(p) => p.fragment_id.clone(),
                                None => Locator::min_ref().clone(),
                            }
                        } else {
                            insertion.fragment_id.clone()
                        }
                    },
                    None => match self.insertions.last() {
                        Some(ins) => ins.fragment_id.clone(),
                        None => Locator::min_ref().clone(),
                    },
                };
                fragment_ids.push((i, fragment_id));
            }
        }

        fragment_ids.sort_by(|a, b| a.1.cmp(&b.1));

        let cx = &None;
        let mut cursor = self
            .fragments
            .cursor::<Dimensions<Option<Locator>, usize>>(cx);
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
    use super::TextBuffer;
    use std::{cmp::Ordering, mem, ops::Range};
    use stoat_text::{Anchor, Bias, BufferId, IndentStyle, Point, Selection, SelectionGoal};

    fn buf(content: &str) -> TextBuffer {
        TextBuffer::with_text(BufferId::new(0), content)
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
        let offsets: Vec<usize> = (0..=len).collect();
        for bias in [Bias::Left, Bias::Right] {
            let batch = snap.anchors_at_batch(&offsets, bias);
            for (i, &off) in offsets.iter().enumerate() {
                assert_eq!(
                    batch[i],
                    snap.anchor_at(off, bias),
                    "anchors_at_batch disagrees at offset {off} bias {bias:?}"
                );
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
        for _ in 0..15 {
            let len = b.snapshot.visible_text.len();
            let at = (lcg(&mut seed) as usize) % (len + 1);
            if at < len {
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

    #[test]
    fn begin_group_collapses_edits_into_one_undo_step() {
        let mut b = buf("");
        b.begin_group(Vec::new());
        b.edit(0..0, "a");
        b.edit(1..1, "b");
        b.edit(2..2, "c");
        b.seal_group(Vec::new());
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
        grouped.begin_group(Vec::new());
        for (range, text) in &edits {
            grouped.edit(range.clone(), text);
        }
        grouped.seal_group(Vec::new());

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
    fn try_begin_group_leaves_an_open_group_untouched() {
        let mut b = buf("");
        b.begin_group(Vec::new());
        b.edit(0..0, "a");
        assert!(
            !b.try_begin_group(Vec::new()),
            "an already-open group is not reopened"
        );
        b.edit(1..1, "b");
        b.seal_group(Vec::new());
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
            b.try_begin_group(Vec::new()),
            "a fresh group opens when none is active"
        );
        b.edit(0..0, "a");
        b.edit(1..1, "b");
        b.seal_group(Vec::new());
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
        b.begin_group(Vec::new());
        b.seal_group(Vec::new());
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

    #[test]
    fn undo_returns_the_groups_before_selections() {
        let mut b = buf("hello");
        let anchor = b.anchor_at(2, Bias::Right);
        let before = vec![Selection {
            id: 7,
            start: anchor,
            end: anchor,
            reversed: false,
            goal: SelectionGoal::None,
        }];
        b.begin_group(before);
        b.edit(5..5, " world");
        b.seal_group(Vec::new());
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
                        b.begin_group(Vec::new());

                        for _ in 0..1 + rng.below(3) {
                            let text = b.snapshot.visible_text.to_string();
                            let mut bounds: Vec<usize> =
                                text.char_indices().map(|(i, _)| i).collect();
                            bounds.push(text.len());

                            let a = bounds[rng.below(bounds.len())];
                            let z = bounds[rng.below(bounds.len())];
                            b.edit(a.min(z)..a.max(z), INSERTS[rng.below(INSERTS.len())]);
                        }

                        b.seal_group(Vec::new());
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
                    0..=5 => {
                        b.begin_group(Vec::new());
                        model.begin_group();

                        for _ in 0..1 + rng.below(3) {
                            let text = b.snapshot.visible_text.to_string();
                            let a = random_boundary(&mut rng, &text);
                            let z = random_boundary(&mut rng, &text);
                            let insert = INSERTS[rng.below(INSERTS.len())];

                            b.edit(a.min(z)..a.max(z), insert);
                            model.edit(a.min(z)..a.max(z), insert);
                        }

                        b.seal_group(Vec::new());
                    },
                    6..=8 => {
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
            }
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

    /// One random edit, undo, or redo, weighted so a buffer accumulates
    /// history faster than it unwinds it.
    fn random_step(rng: &mut Lcg, b: &mut TextBuffer) {
        const INSERTS: [&str; 5] = ["", "z", "hello ", "\u{65e5}\u{672c}", "\n  "];

        match rng.below(10) {
            0..=5 => {
                let text = b.snapshot.visible_text.to_string();
                let a = random_boundary(rng, &text);
                let z = random_boundary(rng, &text);
                b.edit(a.min(z)..a.max(z), INSERTS[rng.below(INSERTS.len())]);
            },
            6..=8 => {
                b.undo();
            },
            _ => {
                b.redo();
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
