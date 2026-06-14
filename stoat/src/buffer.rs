use crate::diff_map::DiffMap;
use encoding_rs::{GBK, SHIFT_JIS, UTF_16BE, UTF_16LE, UTF_8, WINDOWS_1252};
use serde::{Deserialize, Serialize};
use std::{cmp::Ordering, collections::HashMap, ops::Range, sync::Arc};
pub use stoat_text::BufferId;
use stoat_text::{
    patch::{Edit, Patch},
    Anchor, Bias, Dimensions, Fragment, InsertionFragment, InsertionFragmentKey, Locator, Point,
    Rope, SumTree, UndoMap, UndoOperation,
};

/// The dominant line terminator a [`TextBuffer`] uses. The rope stores
/// line endings verbatim, so this is detected from and applied to its
/// content rather than tracked as separate metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum LineEnding {
    #[default]
    Lf,
    Crlf,
    Cr,
}

impl LineEnding {
    /// Short status-bar tag: `LF`, `CRLF`, or `CR`.
    pub fn as_str(self) -> &'static str {
        match self {
            LineEnding::Lf => "LF",
            LineEnding::Crlf => "CRLF",
            LineEnding::Cr => "CR",
        }
    }

    fn terminator(self) -> &'static str {
        match self {
            LineEnding::Lf => "\n",
            LineEnding::Crlf => "\r\n",
            LineEnding::Cr => "\r",
        }
    }
}

/// The character encoding a [`TextBuffer`]'s bytes were decoded from.
/// Unlike [`LineEnding`], the decoded rope is always UTF-8 and cannot
/// reveal the original encoding, so this is tracked as metadata.
/// Re-selecting an encoding re-decodes the file's bytes via [`decode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Encoding {
    #[default]
    Utf8,
    Utf8Bom,
    Utf16Le,
    Utf16Be,
    Latin1,
    ShiftJis,
    Gbk,
}

impl Encoding {
    /// Status-bar label, e.g. `UTF-8`, `Shift-JIS`.
    pub fn as_str(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf8Bom => "UTF-8 BOM",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Latin1 => "Latin-1",
            Encoding::ShiftJis => "Shift-JIS",
            Encoding::Gbk => "GBK",
        }
    }
}

pub struct TextBuffer {
    pub snapshot: TextBufferSnapshot,
    pub dirty: bool,
    /// Character encoding the file's bytes were decoded from. Metadata
    /// only; the rope content is always UTF-8.
    encoding: Encoding,
    pub diff_map: Option<DiffMap>,
    next_timestamp: u64,
    buffer_id: BufferId,
    /// Stack of edit timestamps eligible to be the target of the next `undo()`.
    /// Pushed on `edit()`, popped on `undo()`. Independent of [`Self::ops`],
    /// which records both edits and undos for replay.
    edit_history: Vec<u64>,
    /// Stack of edit timestamps that have been undone and are eligible to be
    /// the target of the next `redo()`. Pushed on `undo()`, popped on
    /// `redo()`, cleared on any new `edit()` per standard undo/redo semantics.
    redo_history: Vec<u64>,
    /// Chronological log of user-driven mutations. Replaying this on a fresh
    /// [`TextBuffer`] reconstructs an identical fragment tree, anchors, and
    /// undo map, which is how workspace save/restore preserves selections and
    /// undo stack across sessions.
    ops: Vec<BufferOp>,
    /// Initial file content installed below the undo floor by
    /// [`Self::with_text`] / [`Self::from_history`]. Reproduced by the fragment
    /// tree but absent from [`Self::ops`] and [`Self::edit_history`], so `undo`
    /// cannot revert the file load. Serialized so restore reinstalls it.
    base_text: String,
    next_checkpoint_id: u32,
    /// Named markers on the op log placed by `commit_undo_checkpoint`. Read by
    /// checkpoint-navigation actions; never mutated by `edit` / `undo` / `redo`.
    checkpoints: Vec<Checkpoint>,
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

/// Serializable buffer state: the base file content, the op log, and the
/// `dirty` flag. Replay via [`TextBuffer::from_history`].
///
/// `base_text` is the initial file load, installed below the undo floor so
/// `undo` cannot revert it. It defaults to empty for snapshots written before
/// the field existed, which recorded the load as the first entry in `ops`
/// instead; those still replay correctly.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BufferHistory {
    #[serde(default)]
    pub base_text: String,
    pub ops: Vec<BufferOp>,
    pub dirty: bool,
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
            encoding: Encoding::Utf8,
            diff_map: None,
            next_timestamp: 1,
            buffer_id,
            edit_history: Vec::new(),
            redo_history: Vec::new(),
            ops: Vec::new(),
            base_text: String::new(),
            next_checkpoint_id: 0,
            checkpoints: Vec::new(),
        }
    }

    pub fn with_text(buffer_id: BufferId, text: &str) -> Self {
        let mut buf = Self::new(buffer_id);
        buf.install_base_text(text);
        buf
    }

    pub fn edit(&mut self, range: Range<usize>, text: &str) {
        self.redo_history.clear();
        self.ops.push(BufferOp::Edit {
            old: range.clone(),
            text: text.to_owned(),
        });
        let timestamp = self.next_timestamp;
        self.next_timestamp += 1;
        self.apply_edit(range, text, timestamp);
        self.dirty = true;
        self.edit_history.push(timestamp);
    }

    /// Apply a `(range, text)` mutation to the fragment tree at `timestamp`,
    /// updating the visible/deleted ropes, insertions, and version. Records no
    /// history -- callers own `ops`, `edit_history`, and `dirty`. Separating
    /// this from [`Self::edit`] lets the initial file load install as base
    /// text below the undo floor via [`Self::install_base_text`].
    fn apply_edit(&mut self, range: Range<usize>, text: &str, timestamp: u64) {
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
    }

    /// Install `text` as the buffer's base content at the first timestamp,
    /// below the undo floor: it is reproduced by the fragment tree but recorded
    /// in neither `ops` nor `edit_history`, so `undo` cannot revert it and it
    /// is not treated as a user edit. Empty text is a no-op, leaving the buffer
    /// genuinely empty.
    fn install_base_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        let timestamp = self.next_timestamp;
        self.next_timestamp += 1;
        self.apply_edit(0..0, text, timestamp);
        self.base_text = text.to_owned();
    }

    pub fn undo(&mut self) -> bool {
        let Some(edit_timestamp) = self.edit_history.pop() else {
            return false;
        };
        self.apply_undo_toggle(edit_timestamp, BufferOp::Undo);
        self.redo_history.push(edit_timestamp);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(edit_timestamp) = self.redo_history.pop() else {
            return false;
        };
        self.apply_undo_toggle(edit_timestamp, BufferOp::Redo);
        self.edit_history.push(edit_timestamp);
        true
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

    /// The buffer's dominant line ending, detected from its first
    /// terminator. Empty and single-line buffers report [`LineEnding::Lf`].
    pub fn line_ending(&self) -> LineEnding {
        detect_line_ending(self.rope())
    }

    /// Rewrite every line ending to `target`, replacing the whole buffer
    /// text. A no-op when the text is already uniform at `target`.
    pub fn set_line_ending(&mut self, target: LineEnding) {
        let current = self.rope().to_string();
        let rewritten = normalize_line_endings(&current, target);
        if rewritten != current {
            self.edit(0..current.len(), &rewritten);
        }
    }

    pub fn encoding(&self) -> Encoding {
        self.encoding
    }

    /// Record the encoding the content corresponds to. Metadata only:
    /// decoding the file's bytes and replacing the text is the caller's
    /// job, since it requires IO this type does not perform.
    pub fn set_encoding(&mut self, encoding: Encoding) {
        self.encoding = encoding;
    }

    pub fn version(&self) -> u64 {
        self.snapshot.version
    }

    pub fn buffer_id(&self) -> BufferId {
        self.buffer_id
    }

    /// Snapshot the op log and dirty flag for persistence. Replay the result
    /// with [`Self::from_history`] to reconstruct an identical buffer.
    pub fn history(&self) -> BufferHistory {
        BufferHistory {
            base_text: self.base_text.clone(),
            ops: self.ops.clone(),
            dirty: self.dirty,
        }
    }

    /// Reconstruct a [`TextBuffer`] by replaying `history` on a fresh buffer.
    /// The base file content is installed first (below the undo floor), then
    /// the op log replays. Sequential timestamp assignment means anchors from
    /// the original buffer resolve to identical byte offsets in the
    /// reconstructed one. Pre-`base_text` snapshots default the base to empty
    /// and carry the load as the first op, so they replay unchanged.
    pub fn from_history(buffer_id: BufferId, history: &BufferHistory) -> Self {
        let mut buf = Self::new(buffer_id);
        buf.install_base_text(&history.base_text);
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
        buf.dirty = history.dirty;
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

    /// Document-order [`Locator`] of the fragment an anchor sits in.
    ///
    /// This is the structural half of resolution: it locates the anchor's
    /// fragment in the insertions tree but skips the fragment-tree seek that
    /// [`TextBufferSnapshot::resolve_anchor`] performs to turn the fragment into
    /// a byte offset. It underlies [`TextBufferSnapshot::cmp_anchors`], which
    /// orders anchors without resolving them. The min/max sentinels map to
    /// [`Locator::min`]/[`Locator::max`] so they sort first/last.
    pub fn fragment_id_for_anchor(&self, anchor: &Anchor) -> &Locator {
        if anchor.is_min() {
            return Locator::min_ref();
        }
        if anchor.is_max() {
            return Locator::max_ref();
        }

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

    /// Order two anchors in document order without resolving either to a byte
    /// offset, comparing their fragment [`Locator`]s instead.
    ///
    /// Agrees with the offset order of [`Anchor::cmp`] for any anchor produced
    /// by [`TextBufferSnapshot::anchor_at`]: at a fragment boundary a `Left`
    /// anchor lands in the earlier fragment and a `Right` anchor in the later
    /// one, so fragment order and the offset+bias tie-break coincide.
    pub fn cmp_anchors(&self, a: &Anchor, b: &Anchor) -> Ordering {
        a.cmp_structural(b, &|anchor| self.fragment_id_for_anchor(anchor))
    }

    fn find_fragment_for_anchor(&self, anchor: &Anchor) -> (Option<&Fragment>, usize) {
        let target = Some(self.fragment_id_for_anchor(anchor).clone());
        let cx = &None;
        let (start, _end, item) = self
            .fragments
            .find::<Dimensions<Option<Locator>, usize>, _>(cx, &target, Bias::Left);

        (item, start.1)
    }

    /// Resolve many anchors to byte offsets, returning offsets in the same
    /// order as `anchors`.
    ///
    /// Equivalent to mapping [`Self::resolve_anchor`] over the slice, but the
    /// `fragments`-tree half of resolution is served by one forward cursor walk
    /// instead of an independent descent per anchor. Anchors are visited in
    /// document order internally and scattered back, so the input need not be
    /// sorted; the per-anchor [`Self::fragment_id_for_anchor`] lookup is the
    /// only remaining per-anchor descent.
    pub fn resolve_anchors_batch(&self, anchors: &[Anchor]) -> Vec<usize> {
        let text_len = self.visible_text.len();
        let locators: Vec<&Locator> = anchors
            .iter()
            .map(|anchor| self.fragment_id_for_anchor(anchor))
            .collect();

        let mut order: Vec<usize> = (0..anchors.len()).collect();
        order.sort_by(|&i, &j| locators[i].cmp(locators[j]));

        let mut offsets = vec![0usize; anchors.len()];
        let mut cursor = self
            .fragments
            .cursor::<Dimensions<Option<Locator>, usize>>(&None);

        for &i in &order {
            let anchor = &anchors[i];
            if anchor.is_min() {
                offsets[i] = 0;
                continue;
            }
            if anchor.is_max() {
                offsets[i] = text_len;
                continue;
            }

            cursor.seek_forward(&Some(locators[i].clone()), Bias::Left);
            let base_offset = cursor.start().1;
            offsets[i] = match cursor.item() {
                Some(fragment) if fragment.visible => {
                    base_offset + anchor.offset.saturating_sub(fragment.insertion_offset) as usize
                },
                _ => base_offset,
            };
        }

        offsets
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
        let mut result = Patch::empty();

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
            let was_visible = fragment.timestamp <= since_version
                && !fragment.deletions.iter().any(|&d| d <= since_version);

            if fragment.visible && !was_visible {
                let edit = Edit {
                    old: old_offset..old_offset,
                    new: new_offset..(new_offset + len),
                };
                result.push(edit);
                new_offset += len;
            } else if !fragment.visible && was_visible {
                let edit = Edit {
                    old: old_offset..(old_offset + len),
                    new: new_offset..new_offset,
                };
                result.push(edit);
                old_offset += len;
            } else if fragment.visible {
                old_offset += len;
                new_offset += len;
            }

            new_offset_from_skipped = *cursor.start() + fragment.visible_len();
            cursor.next();
        }

        result
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

fn detect_line_ending(rope: &Rope) -> LineEnding {
    let mut chars = rope.chars();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                return if chars.next() == Some('\n') {
                    LineEnding::Crlf
                } else {
                    LineEnding::Cr
                };
            },
            '\n' => return LineEnding::Lf,
            _ => {},
        }
    }
    LineEnding::Lf
}

fn normalize_line_endings(text: &str, target: LineEnding) -> String {
    let terminator = target.terminator();
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str(terminator);
            },
            '\n' => out.push_str(terminator),
            other => out.push(other),
        }
    }
    out
}

/// Decode `bytes` as `encoding`, returning the UTF-8 text and whether
/// the decode was lossy (malformed sequences replaced with U+FFFD).
pub fn decode(bytes: &[u8], encoding: Encoding) -> (String, bool) {
    let (text, had_errors) = match encoding {
        Encoding::Utf8 => UTF_8.decode_without_bom_handling(bytes),
        Encoding::Utf8Bom => UTF_8.decode_with_bom_removal(bytes),
        Encoding::Utf16Le => UTF_16LE.decode_without_bom_handling(bytes),
        Encoding::Utf16Be => UTF_16BE.decode_without_bom_handling(bytes),
        Encoding::Latin1 => WINDOWS_1252.decode_without_bom_handling(bytes),
        Encoding::ShiftJis => SHIFT_JIS.decode_without_bom_handling(bytes),
        Encoding::Gbk => GBK.decode_without_bom_handling(bytes),
    };
    (text.into_owned(), had_errors)
}

#[cfg(test)]
mod tests {
    use super::{BufferHistory, BufferOp, Encoding, LineEnding, TextBuffer};
    use std::cmp::Ordering;
    use stoat_text::{Anchor, Bias, BufferId, Point};

    fn buf(content: &str) -> TextBuffer {
        TextBuffer::with_text(BufferId::new(0), content)
    }

    #[test]
    fn cmp_anchors_matches_offset_order_across_fragments() {
        let mut b = buf("hello world");
        b.edit(5..5, " BIG"); // splits the original insertion around a new one
        b.edit(0..0, "X"); // a leading fragment
        let snap = &b.snapshot;

        let mut anchors = Vec::new();
        for off in 0..=snap.len() {
            anchors.push(snap.anchor_at(off, Bias::Left));
            anchors.push(snap.anchor_at(off, Bias::Right));
        }

        let mut by_structural = anchors.clone();
        by_structural.sort_by(|a, c| snap.cmp_anchors(a, c));
        let mut by_offset = anchors.clone();
        by_offset.sort_by(|a, c| a.cmp(c, &|x| snap.resolve_anchor(x)));

        assert_eq!(by_structural, by_offset);
    }

    #[test]
    fn cmp_anchors_orders_sentinels_first_and_last() {
        let mut b = buf("abc");
        b.edit(1..1, "Z");
        let snap = &b.snapshot;
        let mid = snap.anchor_at(2, Bias::Left);
        let min = Anchor::min_for_buffer(BufferId::new(0));
        let max = Anchor::max_for_buffer(BufferId::new(0));

        assert_eq!(snap.cmp_anchors(&min, &mid), Ordering::Less);
        assert_eq!(snap.cmp_anchors(&mid, &max), Ordering::Less);
        assert_eq!(snap.cmp_anchors(&min, &max), Ordering::Less);
        assert_eq!(snap.cmp_anchors(&min, &min), Ordering::Equal);
    }

    #[test]
    fn line_ending_detects_lf_crlf_cr_and_defaults() {
        assert_eq!(buf("a\nb\n").line_ending(), LineEnding::Lf);
        assert_eq!(buf("a\r\nb\r\n").line_ending(), LineEnding::Crlf);
        assert_eq!(buf("a\rb").line_ending(), LineEnding::Cr);
        assert_eq!(buf("abc").line_ending(), LineEnding::Lf);
    }

    #[test]
    fn set_line_ending_rewrites_terminators() {
        let mut b = buf("a\nb\nc");
        b.set_line_ending(LineEnding::Crlf);
        assert_eq!(b.rope().to_string(), "a\r\nb\r\nc");
        b.set_line_ending(LineEnding::Lf);
        assert_eq!(b.rope().to_string(), "a\nb\nc");
    }

    #[test]
    fn encoding_defaults_to_utf8_and_set_updates_it() {
        let mut b = buf("x");
        assert_eq!(b.encoding(), Encoding::Utf8);
        b.set_encoding(Encoding::ShiftJis);
        assert_eq!(b.encoding(), Encoding::ShiftJis);
    }

    #[test]
    fn decode_maps_encodings_and_flags_lossy() {
        assert_eq!(
            super::decode("héllo".as_bytes(), Encoding::Utf8),
            ("héllo".to_string(), false)
        );
        assert_eq!(
            super::decode(&[0xE9], Encoding::Latin1),
            ("é".to_string(), false)
        );
        assert_eq!(
            super::decode(&[0x93, 0xFA, 0x96, 0x7B], Encoding::ShiftJis),
            ("日本".to_string(), false)
        );

        let (text, lossy) = super::decode(&[0xFF, 0xFE], Encoding::Utf8);
        assert!(lossy, "invalid utf-8 should be lossy");
        assert!(text.contains('\u{FFFD}'));
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
    fn batch_resolve_unsorted_matches_per_anchor() {
        let mut b = buf("hello world");
        b.edit(5..5, " BIG"); // splits the original insertion around a new one
        b.edit(0..0, "X"); // a leading fragment
        let snap = &b.snapshot;
        let id = BufferId::new(0);

        let mut anchors = Vec::new();
        for off in 0..=snap.len() {
            anchors.push(snap.anchor_at(off, Bias::Right));
            anchors.push(snap.anchor_at(off, Bias::Left));
        }
        anchors.reverse(); // descending input exercises the sort + scatter path
        anchors.insert(anchors.len() / 2, Anchor::min_for_buffer(id));
        anchors.insert(anchors.len() / 3, Anchor::max_for_buffer(id));

        let expected: Vec<usize> = anchors.iter().map(|a| snap.resolve_anchor(a)).collect();
        assert_eq!(snap.resolve_anchors_batch(&anchors), expected);
    }

    #[test]
    fn batch_resolve_min_max_interspersed() {
        let mut b = buf("hello");
        let a1 = b.anchor_at(1, Bias::Right);
        let a3 = b.anchor_at(3, Bias::Right);
        b.edit(0..0, "XX"); // shifts both anchors right by two
        let snap = &b.snapshot;
        let id = BufferId::new(0);

        let anchors = [
            a3,
            Anchor::min_for_buffer(id),
            a1,
            Anchor::max_for_buffer(id),
        ];
        assert_eq!(snap.resolve_anchors_batch(&anchors), vec![5, 0, 3, 7]);
    }

    #[test]
    fn batch_resolve_empty() {
        let b = buf("hello");
        assert_eq!(b.snapshot.resolve_anchors_batch(&[]), Vec::<usize>::new());
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
    fn edits_since_three_interspersed_inserts() {
        let mut b = buf("ab\ncd\nef\n");
        let v0 = b.version();
        b.edit(9..9, "ab");
        b.edit(6..6, "ab");
        b.edit(2..2, "ab");
        let new = b.snapshot.visible_text.to_string();

        let patch = b.snapshot.edits_since(v0);
        let edits = patch.edits();
        assert_eq!(
            edits
                .iter()
                .map(|e| (e.old.clone(), e.new.clone()))
                .collect::<Vec<_>>(),
            [(2..2, 2..4), (6..6, 8..10), (9..9, 13..15)],
        );

        // The patch must reconstruct the new text from the old text;
        // accumulating the edits incorrectly merges interspersed inserts.
        let old = "ab\ncd\nef\n";
        let mut rebuilt = String::new();
        let mut pos = 0;
        for e in edits {
            rebuilt.push_str(&old[pos..e.old.start]);
            rebuilt.push_str(&new[e.new.start..e.new.end]);
            pos = e.old.end;
        }
        rebuilt.push_str(&old[pos..]);
        assert_eq!(rebuilt, new);
    }

    #[test]
    fn edits_since_no_changes() {
        let b = buf("hello");
        let patch = b.snapshot.edits_since(b.version());
        assert!(patch.is_empty());
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
        assert!(!b.undo());
        assert_eq!(b.snapshot.visible_text.to_string(), "");
    }

    #[test]
    fn undo_does_not_revert_initial_load() {
        let mut b = buf("hello");
        assert!(!b.undo(), "undo with no user edits is a no-op");
        assert_eq!(b.snapshot.visible_text.to_string(), "hello");
        assert!(!b.dirty, "a freshly loaded buffer is not dirty");
    }

    #[test]
    fn history_keeps_load_below_undo_floor() {
        let mut b = buf("hello world");
        let anchor = b.anchor_at(6, Bias::Right);
        b.edit(11..11, "!");

        let history = b.history();
        assert_eq!(history.base_text, "hello world");
        assert_eq!(history.ops.len(), 1, "only the user edit is logged");

        let restored = TextBuffer::from_history(BufferId::new(0), &history);
        assert_eq!(restored.snapshot.visible_text.to_string(), "hello world!");
        assert_eq!(
            restored.resolve_anchor(&anchor),
            6,
            "anchors survive restore"
        );
    }

    #[test]
    fn old_format_history_round_trips() {
        // Snapshots predating `base_text` recorded the file load as the first
        // op with an empty base; they must still reconstruct the same text.
        let history = BufferHistory {
            base_text: String::new(),
            ops: vec![
                BufferOp::Edit {
                    old: 0..0,
                    text: "hello".to_string(),
                },
                BufferOp::Edit {
                    old: 5..5,
                    text: " world".to_string(),
                },
            ],
            dirty: true,
        };
        let b = TextBuffer::from_history(BufferId::new(0), &history);
        assert_eq!(b.snapshot.visible_text.to_string(), "hello world");
        assert!(b.dirty);
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
        assert!(b.undo());
        assert_eq!(b.snapshot.visible_text.to_string(), "hello");
        assert!(b.redo());
        assert_eq!(b.snapshot.visible_text.to_string(), "hello world");
    }

    #[test]
    fn redo_walks_back_through_a_full_cycle() {
        let mut b = buf("a");
        b.edit(1..1, "b");
        b.edit(2..2, "c");
        assert_eq!(b.snapshot.visible_text.to_string(), "abc");
        assert!(b.undo());
        assert!(b.undo());
        assert_eq!(b.snapshot.visible_text.to_string(), "a");
        assert!(b.redo());
        assert_eq!(b.snapshot.visible_text.to_string(), "ab");
        assert!(b.redo());
        assert_eq!(b.snapshot.visible_text.to_string(), "abc");
    }

    #[test]
    fn new_edit_clears_redo_stack() {
        let mut b = buf("a");
        b.edit(1..1, "b");
        assert_eq!(b.snapshot.visible_text.to_string(), "ab");
        assert!(b.undo());
        assert_eq!(b.snapshot.visible_text.to_string(), "a");
        b.edit(1..1, "X");
        assert_eq!(b.snapshot.visible_text.to_string(), "aX");
        assert!(!b.redo(), "redo stack cleared by new edit");
        assert_eq!(b.snapshot.visible_text.to_string(), "aX");
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
        // The initial load is base text, not an op, so only the two edits count.
        assert_eq!(b.checkpoints()[0].op_index, 2);
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
