use crate::diff_map::DiffMap;
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, ops::Range, sync::Arc};
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
    /// the pristine empty state. [`Self::dirty`] caches
    /// `edit_history.last() != saved_marker`, so undo/redo back to this
    /// frontier reads as clean again while moving off it reads as modified.
    saved_marker: Option<u64>,
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
        let mut new_text_inserted = false;

        // Copy all fragments before the edit start
        new_fragments.append(cursor.slice(&range.start, Bias::Right), cx);

        let mut delete_remaining = range.end - range.start;

        if let Some(fragment) = cursor.item() {
            let fragment_start = *cursor.start();
            let overshoot = range.start - fragment_start;

            if overshoot > 0 && fragment.visible {
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
                    delete_remaining -= to_delete_here;
                }

                let suffix_len = remaining_in_fragment.saturating_sub(to_delete_here);
                if suffix_len > 0 && delete_remaining == 0 {
                    let suffix_insertion_offset =
                        fragment.insertion_offset + overshoot as u32 + to_delete_here as u32;

                    if !text.is_empty() {
                        let next_id = cursor
                            .next_item()
                            .map(|f| &f.id)
                            .unwrap_or(Locator::max_ref());
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
                        new_text_inserted = true;
                    }

                    let next_id = cursor
                        .next_item()
                        .map(|f| &f.id)
                        .unwrap_or(Locator::max_ref());
                    let suffix_id = Locator::between(last_id(&new_fragments, cx), next_id);
                    let suffix = Fragment {
                        id: suffix_id.clone(),
                        timestamp: fragment.timestamp,
                        insertion_offset: suffix_insertion_offset,
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
                    cursor.next();
                } else {
                    cursor.next();
                }
            } else {
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

                        if !text.is_empty() {
                            let new_frag_id =
                                Locator::between(last_id(&new_fragments, cx), &fragment.id);
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
                            new_text_inserted = true;
                        }

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
                    new_fragments.push(fragment.clone(), cx);
                    cursor.next();
                },
                None => break,
            }
        }

        // Insert new text if not yet inserted (pure insertion case)
        if !text.is_empty() && !new_text_inserted {
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

        // Copy remaining fragments
        new_fragments.append(cursor.suffix(), cx);

        // Update insertions tree
        let mut all_insertions = self.snapshot.insertions.clone();
        for ins in new_insertions {
            all_insertions.insert_or_replace(ins, ());
        }

        // Capture deleted text before mutating the visible rope
        if range.start < range.end {
            let deleted_bytes = self.snapshot.visible_text.slice(range.clone());
            self.snapshot.deleted_text.append(deleted_bytes);
        }

        // Update the rope
        self.snapshot.visible_text.replace(range, text);

        // Store new state
        self.snapshot.fragments = new_fragments;
        self.snapshot.insertions = all_insertions;
        self.snapshot.version = timestamp;
        self.dirty = true;
        self.record_edit(timestamp);
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

    /// Record the current edit frontier as the clean baseline, marking the
    /// buffer unmodified. Call at every clean point (save, seeded content) so a
    /// later undo/redo back to this frontier clears [`Self::dirty`] again.
    pub(crate) fn mark_clean(&mut self) {
        self.saved_marker = self.frontier();
        self.dirty = false;
    }

    fn recompute_dirty(&mut self) {
        self.dirty = self.frontier() != self.saved_marker;
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
        for &edit_timestamp in group.edits.iter().rev() {
            self.apply_undo_toggle(edit_timestamp, BufferOp::Undo);
        }
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
        for &edit_timestamp in &group.edits {
            self.apply_undo_toggle(edit_timestamp, BufferOp::Redo);
        }
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

    fn apply_undo_toggle(&mut self, edit_timestamp: u64, op: BufferOp) {
        self.ops.push(op);
        let undo_timestamp = self.next_timestamp;
        self.next_timestamp += 1;

        let new_count = self.snapshot.undo_map.undo_count(edit_timestamp) + 1;

        self.snapshot.undo_map.insert(&UndoOperation {
            timestamp: undo_timestamp,
            counts: HashMap::from([(edit_timestamp, new_count)]),
        });

        let cx = &None;
        let old_fragments = std::mem::replace(&mut self.snapshot.fragments, SumTree::new(cx));
        let mut new_fragments = SumTree::new(cx);
        let mut new_visible = Rope::new();
        let mut new_deleted = Rope::new();

        let mut visible_cursor_offset = 0usize;
        let mut deleted_cursor_offset = 0usize;

        let frag_cursor = old_fragments.cursor::<()>(cx);
        for fragment in frag_cursor {
            let len = fragment.len as usize;
            let was_visible = fragment.visible;
            let is_visible = fragment.is_visible_with_undos(&self.snapshot.undo_map);

            let mut new_frag = fragment.clone();
            new_frag.visible = is_visible;
            if was_visible != is_visible {
                new_frag.max_undos = undo_timestamp;
            }

            if was_visible {
                let text_slice = self
                    .snapshot
                    .visible_text
                    .slice(visible_cursor_offset..(visible_cursor_offset + len));
                if is_visible {
                    new_visible.append(text_slice);
                } else {
                    new_deleted.append(text_slice);
                }
                visible_cursor_offset += len;
            } else {
                let text_slice = self
                    .snapshot
                    .deleted_text
                    .slice(deleted_cursor_offset..(deleted_cursor_offset + len));
                if is_visible {
                    new_visible.append(text_slice);
                } else {
                    new_deleted.append(text_slice);
                }
                deleted_cursor_offset += len;
            }

            new_fragments.push(new_frag, cx);
        }

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

    fn find_fragment_for_anchor(&self, anchor: &Anchor) -> (Option<&Fragment>, usize) {
        let key = InsertionFragmentKey {
            timestamp: anchor.timestamp,
            split_offset: anchor.offset,
        };

        let (_start, _end, result) =
            self.insertions
                .find_with_prev::<InsertionFragmentKey, _>((), &key, anchor.bias);

        let fragment_id = match result {
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
        };

        let cx = &None;
        let target = Some(fragment_id.clone());
        let (start, _end, item) = self
            .fragments
            .find::<Dimensions<Option<Locator>, usize>, _>(cx, &target, Bias::Left);

        (item, start.1)
    }

    pub fn resolve_anchors_batch(&self, anchors: &[Anchor]) -> Vec<usize> {
        anchors.iter().map(|a| self.resolve_anchor(a)).collect()
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
    use std::ops::Range;
    use stoat_text::{Bias, BufferId, IndentStyle, Point, Selection, SelectionGoal};

    fn buf(content: &str) -> TextBuffer {
        TextBuffer::with_text(BufferId::new(0), content)
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
        let min = stoat_text::Anchor::min();
        let max = stoat_text::Anchor::max();
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
}
