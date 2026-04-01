use crate::diff_map::DiffMap;
use std::{collections::HashMap, sync::Arc};
pub use stoat_text::BufferId;
use stoat_text::{
    patch::{Edit, Patch},
    Anchor, Bias, Dimensions, Fragment, InsertionFragment, InsertionFragmentKey, Locator, Point,
    Rope, SumTree, UndoMap, UndoOperation,
};

pub struct TextBuffer {
    pub snapshot: TextBufferSnapshot,
    pub dirty: bool,
    pub diff_map: Option<DiffMap>,
    next_timestamp: u64,
    buffer_id: BufferId,
    edit_history: Vec<u64>,
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
            diff_map: None,
            next_timestamp: 1,
            buffer_id,
            edit_history: Vec::new(),
        }
    }

    pub fn with_text(buffer_id: BufferId, text: &str) -> Self {
        let mut buf = Self::new(buffer_id);
        if !text.is_empty() {
            buf.edit(0..0, text);
            buf.dirty = false;
        }
        buf
    }

    pub fn edit(&mut self, range: std::ops::Range<usize>, text: &str) {
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
        self.dirty = true;
        self.edit_history.push(timestamp);
    }

    pub fn undo(&mut self) -> bool {
        let Some(&edit_timestamp) = self.edit_history.last() else {
            return false;
        };

        let undo_timestamp = self.next_timestamp;
        self.next_timestamp += 1;

        let current_count = if self.snapshot.undo_map.is_undone(edit_timestamp) {
            2
        } else {
            1
        };

        self.snapshot.undo_map.insert(&UndoOperation {
            timestamp: undo_timestamp,
            counts: HashMap::from([(edit_timestamp, current_count)]),
        });

        // Rebuild fragment visibility and ropes
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

        if current_count % 2 == 1 {
            self.edit_history.pop();
        }

        true
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
        if let Some(id) = anchor.buffer_id {
            if id != self.buffer_id {
                return false;
            }
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
                result = result.compose([edit]);
                new_offset += len;
            } else if !fragment.visible && was_visible {
                let edit = Edit {
                    old: old_offset..(old_offset + len),
                    new: new_offset..new_offset,
                };
                result = result.compose([edit]);
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
    use stoat_text::{Bias, BufferId, Point};

    fn buf(content: &str) -> TextBuffer {
        TextBuffer::with_text(BufferId::new(0), content)
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
    fn undo_preserves_anchors() {
        let mut b = buf("hello world");
        let a = b.anchor_at(8, Bias::Right);
        b.edit(5..11, "");
        assert!(!b.snapshot.is_anchor_valid(&a));
        b.undo();
        assert!(b.snapshot.is_anchor_valid(&a));
        assert_eq!(b.resolve_anchor(&a), 8);
    }
}
