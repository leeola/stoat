use crate::multi_buffer::MultiBufferSnapshot;
use serde::{Deserialize, Serialize};
use stoat_text::{Anchor, Selection, SelectionGoal};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SelectionsCollection {
    next_selection_id: usize,
    disjoint: Vec<Selection<Anchor>>,
}

impl SelectionsCollection {
    pub(crate) fn new() -> Self {
        let default = Selection {
            id: 0,
            start: Anchor::min(),
            end: Anchor::min(),
            reversed: false,
            goal: SelectionGoal::None,
        };
        Self {
            next_selection_id: 1,
            disjoint: vec![default],
        }
    }

    pub(crate) fn all_anchors(&self) -> &[Selection<Anchor>] {
        &self.disjoint
    }

    pub(crate) fn newest_anchor(&self) -> &Selection<Anchor> {
        self.disjoint
            .iter()
            .max_by_key(|s| s.id)
            .expect("SelectionsCollection invariant: at least one selection")
    }

    pub(crate) fn insert_cursor(
        &mut self,
        head: Anchor,
        goal: SelectionGoal,
        snapshot: &MultiBufferSnapshot,
    ) {
        let new_offset = snapshot.resolve_anchor(&head);

        let pos = self
            .disjoint
            .binary_search_by(|s| snapshot.resolve_anchor(&s.start).cmp(&new_offset))
            .unwrap_or_else(|p| p);

        if let Some(existing) = self.disjoint.get(pos) {
            if existing.is_empty() && snapshot.resolve_anchor(&existing.start) == new_offset {
                return;
            }
        }

        let id = self.next_selection_id;
        self.next_selection_id += 1;
        let selection = Selection {
            id,
            start: head,
            end: head,
            reversed: false,
            goal,
        };
        self.disjoint.insert(pos, selection);
    }

    pub(crate) fn set_single_range(&mut self, start: Anchor, end: Anchor, goal: SelectionGoal) {
        let id = self.next_selection_id;
        self.next_selection_id += 1;
        self.disjoint = vec![Selection {
            id,
            start,
            end,
            reversed: false,
            goal,
        }];
    }

    pub(crate) fn keep_primary(&mut self) {
        let primary = self.newest_anchor().clone();
        self.disjoint = vec![primary];
    }

    pub(crate) fn remove_primary(&mut self) {
        if self.disjoint.len() < 2 {
            return;
        }
        let primary_id = self.newest_anchor().id;
        self.disjoint.retain(|s| s.id != primary_id);
    }

    pub(crate) fn rotate_primary_by(&mut self, forward: bool, count: u32) {
        if self.disjoint.len() < 2 || count == 0 {
            return;
        }
        let primary_id = self.newest_anchor().id;
        let primary_idx = self
            .disjoint
            .iter()
            .position(|s| s.id == primary_id)
            .expect("primary id must be in disjoint");
        let len = self.disjoint.len();
        let offset = (count as usize) % len;
        if offset == 0 {
            return;
        }
        let new_idx = if forward {
            (primary_idx + offset) % len
        } else {
            (primary_idx + len - offset) % len
        };
        let new_id = self.next_selection_id;
        self.next_selection_id += 1;
        self.disjoint[new_idx].id = new_id;
    }

    pub(crate) fn transform<F>(&mut self, snapshot: &MultiBufferSnapshot, mut f: F)
    where
        F: FnMut(&Selection<Anchor>) -> Selection<Anchor>,
    {
        let transformed: Vec<Selection<Anchor>> = self.disjoint.iter().map(&mut f).collect();
        self.replace_with(transformed, snapshot);
    }

    /// Flat-map each selection into zero or more replacement pieces. Returning
    /// an empty vec keeps the original selection unchanged; returning a
    /// non-empty vec replaces it with the pieces, each receiving a fresh id
    /// from this collection's allocator.
    pub(crate) fn split_each<F>(&mut self, snapshot: &MultiBufferSnapshot, mut split: F)
    where
        F: FnMut(&Selection<Anchor>) -> Vec<Selection<Anchor>>,
    {
        let mut new_disjoint: Vec<Selection<Anchor>> = Vec::with_capacity(self.disjoint.len());
        for sel in &self.disjoint {
            let pieces = split(sel);
            if pieces.is_empty() {
                new_disjoint.push(sel.clone());
                continue;
            }
            for mut piece in pieces {
                piece.id = self.next_selection_id;
                self.next_selection_id += 1;
                new_disjoint.push(piece);
            }
        }
        self.replace_with(new_disjoint, snapshot);
    }

    /// Replace selections with `new_disjoint`, sorting by offset and deduping
    /// empty collisions at the same offset (keeping the highest-id survivor).
    /// Asserts non-empty: callers must ensure at least one selection.
    pub(crate) fn replace_with(
        &mut self,
        new_disjoint: Vec<Selection<Anchor>>,
        snapshot: &MultiBufferSnapshot,
    ) {
        assert!(
            !new_disjoint.is_empty(),
            "SelectionsCollection invariant: at least one selection"
        );
        let mut indexed: Vec<(usize, Selection<Anchor>)> = new_disjoint
            .into_iter()
            .map(|s| (snapshot.resolve_anchor(&s.start), s))
            .collect();
        indexed.sort_by_key(|(offset, sel)| (*offset, sel.id));

        let mut deduped: Vec<Selection<Anchor>> = Vec::with_capacity(indexed.len());
        let mut last_empty_offset: Option<usize> = None;
        for (offset, sel) in indexed {
            if sel.is_empty() {
                if last_empty_offset == Some(offset) {
                    let prev = deduped.last_mut().expect("empty collision without prior");
                    if sel.id > prev.id {
                        *prev = sel;
                    }
                    continue;
                }
                last_empty_offset = Some(offset);
            } else {
                last_empty_offset = None;
            }
            deduped.push(sel);
        }
        self.disjoint = deduped;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::{BufferId, TextBuffer},
        multi_buffer::MultiBuffer,
    };
    use std::sync::{Arc, RwLock};
    use stoat_text::Bias;

    fn singleton(content: &str) -> MultiBuffer {
        let id = BufferId::new(0);
        let buffer = TextBuffer::with_text(id, content);
        MultiBuffer::singleton(id, Arc::new(RwLock::new(buffer)))
    }

    #[test]
    fn new_collection_has_one_cursor_at_zero() {
        let collection = SelectionsCollection::new();
        assert_eq!(collection.all_anchors().len(), 1);
        let sel = &collection.all_anchors()[0];
        assert_eq!(sel.id, 0);
        assert!(sel.is_empty());
        assert_eq!(sel.goal, SelectionGoal::None);
        assert!(sel.start.is_min());
    }

    #[test]
    fn insert_cursor_assigns_monotonic_id() {
        let multi = singleton("abcdef");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        collection.insert_cursor(
            snapshot.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        collection.insert_cursor(
            snapshot.anchor_at(5, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );

        let ids: Vec<usize> = collection.all_anchors().iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![0, 1, 2]);
    }

    #[test]
    fn newest_anchor_returns_max_id() {
        let multi = singleton("abcdef");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        collection.insert_cursor(
            snapshot.anchor_at(4, Bias::Right),
            SelectionGoal::Column(4),
            &snapshot,
        );
        assert_eq!(collection.newest_anchor().id, 1);
        assert_eq!(collection.newest_anchor().goal, SelectionGoal::Column(4));
    }

    #[test]
    fn keep_primary_retains_only_newest() {
        let multi = singleton("abcdef");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        collection.insert_cursor(
            snapshot.anchor_at(2, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        collection.insert_cursor(
            snapshot.anchor_at(4, Bias::Right),
            SelectionGoal::Column(4),
            &snapshot,
        );
        assert_eq!(collection.all_anchors().len(), 3);

        collection.keep_primary();

        let remaining = collection.all_anchors();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, 2);
        assert_eq!(remaining[0].goal, SelectionGoal::Column(4));
    }

    #[test]
    fn remove_primary_drops_newest() {
        let multi = singleton("abcdef");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        collection.insert_cursor(
            snapshot.anchor_at(2, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        collection.insert_cursor(
            snapshot.anchor_at(4, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        assert_eq!(collection.all_anchors().len(), 3);
        let dropped_id = collection.newest_anchor().id;

        collection.remove_primary();

        let remaining_ids: Vec<usize> = collection.all_anchors().iter().map(|s| s.id).collect();
        assert_eq!(remaining_ids, vec![0, 1]);
        assert!(!remaining_ids.contains(&dropped_id));
    }

    #[test]
    fn remove_primary_singleton_is_noop() {
        let multi = singleton("abcdef");
        let _snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        let before_id = collection.newest_anchor().id;
        collection.remove_primary();
        assert_eq!(collection.all_anchors().len(), 1);
        assert_eq!(collection.newest_anchor().id, before_id);
    }

    #[test]
    fn rotate_primary_single_selection_is_noop() {
        let multi = singleton("abcdef");
        let _snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        let before_id = collection.newest_anchor().id;
        collection.rotate_primary_by(true, 1);
        assert_eq!(collection.newest_anchor().id, before_id);
        collection.rotate_primary_by(false, 1);
        assert_eq!(collection.newest_anchor().id, before_id);
    }

    #[test]
    fn rotate_primary_forward_wraps() {
        let multi = singleton("abcdefghij");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();
        collection.insert_cursor(
            snapshot.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        collection.insert_cursor(
            snapshot.anchor_at(6, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );

        let primary_offset = |c: &SelectionsCollection| -> usize {
            snapshot.resolve_anchor(&c.newest_anchor().start)
        };

        assert_eq!(primary_offset(&collection), 6);
        collection.rotate_primary_by(true, 1);
        assert_eq!(primary_offset(&collection), 0);
        collection.rotate_primary_by(true, 1);
        assert_eq!(primary_offset(&collection), 3);
        collection.rotate_primary_by(true, 1);
        assert_eq!(primary_offset(&collection), 6);
    }

    #[test]
    fn rotate_primary_backward_wraps() {
        let multi = singleton("abcdefghij");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();
        collection.insert_cursor(
            snapshot.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        collection.insert_cursor(
            snapshot.anchor_at(6, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );

        let primary_offset = |c: &SelectionsCollection| -> usize {
            snapshot.resolve_anchor(&c.newest_anchor().start)
        };

        assert_eq!(primary_offset(&collection), 6);
        collection.rotate_primary_by(false, 1);
        assert_eq!(primary_offset(&collection), 3);
        collection.rotate_primary_by(false, 1);
        assert_eq!(primary_offset(&collection), 0);
        collection.rotate_primary_by(false, 1);
        assert_eq!(primary_offset(&collection), 6);
    }

    #[test]
    fn insert_cursor_sorts_by_offset() {
        let multi = singleton("abcdefghij");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        collection.insert_cursor(
            snapshot.anchor_at(7, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        collection.insert_cursor(
            snapshot.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        collection.insert_cursor(
            snapshot.anchor_at(5, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );

        let offsets: Vec<usize> = collection
            .all_anchors()
            .iter()
            .map(|s| snapshot.resolve_anchor(&s.start))
            .collect();
        assert_eq!(offsets, vec![0, 3, 5, 7]);
    }

    #[test]
    fn transform_advances_each_cursor() {
        let multi = singleton("abcdef");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();
        collection.insert_cursor(
            snapshot.anchor_at(2, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        collection.insert_cursor(
            snapshot.anchor_at(4, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );

        collection.transform(&snapshot, |sel| {
            let offset = snapshot.resolve_anchor(&sel.head());
            let anchor = snapshot.anchor_at(offset + 1, Bias::Right);
            let mut new = sel.clone();
            new.collapse_to(anchor, SelectionGoal::None);
            new
        });

        let offsets: Vec<usize> = collection
            .all_anchors()
            .iter()
            .map(|s| snapshot.resolve_anchor(&s.start))
            .collect();
        assert_eq!(offsets, vec![1, 3, 5]);
    }

    #[test]
    fn transform_dedupes_empty_collisions() {
        let multi = singleton("abcdef");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();
        collection.insert_cursor(
            snapshot.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        collection.insert_cursor(
            snapshot.anchor_at(4, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        assert_eq!(collection.all_anchors().len(), 3);

        collection.transform(&snapshot, |sel| {
            let mut new = sel.clone();
            let target = snapshot.anchor_at(5, Bias::Right);
            new.collapse_to(target, SelectionGoal::None);
            new
        });

        let offsets: Vec<usize> = collection
            .all_anchors()
            .iter()
            .map(|s| snapshot.resolve_anchor(&s.start))
            .collect();
        assert_eq!(offsets, vec![5]);
    }

    #[test]
    fn transform_resorts_after_swap() {
        let multi = singleton("abcdefghij");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();
        collection.insert_cursor(
            snapshot.anchor_at(2, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        collection.insert_cursor(
            snapshot.anchor_at(7, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );

        collection.transform(&snapshot, |sel| {
            let offset = snapshot.resolve_anchor(&sel.head());
            let new_offset = if offset == 0 { 0 } else { 9 - offset };
            let mut new = sel.clone();
            new.collapse_to(
                snapshot.anchor_at(new_offset, Bias::Right),
                SelectionGoal::None,
            );
            new
        });

        let offsets: Vec<usize> = collection
            .all_anchors()
            .iter()
            .map(|s| snapshot.resolve_anchor(&s.start))
            .collect();
        assert_eq!(offsets, vec![0, 2, 7]);
    }

    #[test]
    fn split_each_keeps_original_when_closure_returns_empty() {
        let multi = singleton("abcdef");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();
        collection.insert_cursor(
            snapshot.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        let before_ids: Vec<usize> = collection.all_anchors().iter().map(|s| s.id).collect();

        collection.split_each(&snapshot, |_| Vec::new());

        let after_ids: Vec<usize> = collection.all_anchors().iter().map(|s| s.id).collect();
        assert_eq!(after_ids, before_ids);
    }

    #[test]
    fn split_each_replaces_with_pieces_and_allocates_fresh_ids() {
        let multi = singleton("abcdefghij");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();
        collection.set_single_range(
            snapshot.anchor_at(0, Bias::Right),
            snapshot.anchor_at(10, Bias::Right),
            SelectionGoal::None,
        );
        let before_ids: Vec<usize> = collection.all_anchors().iter().map(|s| s.id).collect();

        collection.split_each(&snapshot, |_| {
            vec![
                Selection {
                    id: 0,
                    start: snapshot.anchor_at(0, Bias::Right),
                    end: snapshot.anchor_at(3, Bias::Right),
                    reversed: false,
                    goal: SelectionGoal::None,
                },
                Selection {
                    id: 0,
                    start: snapshot.anchor_at(5, Bias::Right),
                    end: snapshot.anchor_at(8, Bias::Right),
                    reversed: false,
                    goal: SelectionGoal::None,
                },
            ]
        });

        let after: Vec<(usize, usize)> = collection
            .all_anchors()
            .iter()
            .map(|s| {
                (
                    snapshot.resolve_anchor(&s.start),
                    snapshot.resolve_anchor(&s.end),
                )
            })
            .collect();
        assert_eq!(after, vec![(0, 3), (5, 8)]);
        let after_ids: Vec<usize> = collection.all_anchors().iter().map(|s| s.id).collect();
        assert!(after_ids.iter().all(|id| !before_ids.contains(id)));
    }

    #[test]
    fn transform_preserves_ids() {
        let multi = singleton("abcdefghij");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();
        collection.insert_cursor(
            snapshot.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        let original_ids: Vec<usize> = collection.all_anchors().iter().map(|s| s.id).collect();

        collection.transform(&snapshot, |sel| {
            let offset = snapshot.resolve_anchor(&sel.head());
            let mut new = sel.clone();
            new.collapse_to(
                snapshot.anchor_at(offset + 1, Bias::Right),
                SelectionGoal::None,
            );
            new
        });

        let new_ids: Vec<usize> = collection.all_anchors().iter().map(|s| s.id).collect();
        assert_eq!(new_ids, original_ids);
    }

    #[test]
    fn insert_cursor_dedupes_same_offset_empty() {
        let multi = singleton("abcdef");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        collection.insert_cursor(
            snapshot.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        let after_first = collection.all_anchors().len();
        collection.insert_cursor(
            snapshot.anchor_at(3, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        assert_eq!(collection.all_anchors().len(), after_first);
    }

    #[test]
    fn snapshot_add_selection_below() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("sample.txt", "abcd\nefgh\nijkl\n");

        h.open_file(&path);
        h.type_keys("C");
        h.assert_snapshot("add_selection_below");
    }

    #[test]
    fn snapshot_split_selection_on_newline() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("sample.txt", "abc\ndef\nghi\n");

        h.open_file(&path);
        h.type_keys("% alt-s");
        h.assert_snapshot("split_selection_on_newline");
    }

    #[test]
    fn snapshot_shift_c_adds_selection_below_styled() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("sample.txt", "abcd\nefgh\nijkl\n");

        h.open_file(&path);
        h.type_keys("shift-C");
        h.assert_snapshot("shift_c_adds_selection_below");
    }

    #[test]
    fn count_prefix_add_selection_below_inserts_n_cursors() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
        h.open_file(&path);
        h.type_keys("3 shift-C");
        let spans = h.selection_spans();
        assert_eq!(
            spans.len(),
            4,
            "3C from 1 cursor should leave 4 cursors total (got {spans:?})"
        );
    }

    #[test]
    fn count_prefix_add_selection_above_inserts_n_cursors() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
        h.open_file(&path);
        h.type_keys("4 j");
        h.type_keys("3 alt-shift-C");
        let spans = h.selection_spans();
        assert_eq!(
            spans.len(),
            4,
            "3 Alt-C from 1 cursor should leave 4 cursors total (got {spans:?})"
        );
    }

    #[test]
    fn count_prefix_add_selection_below_clamps_at_buffer_end() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        h.type_keys("9 9 shift-C");
        let spans = h.selection_spans();
        assert!(
            spans.len() <= 4,
            "huge count should clamp at buffer end (3 lines means at most 3 cursors below the start, got {spans:?})"
        );
        assert!(
            spans.len() > 1,
            "should have added at least one cursor below (got {spans:?})"
        );
    }

    #[test]
    fn snapshot_move_right() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "hello world\n");
        h.open_file(&path);
        h.type_keys("l l l");
        h.assert_snapshot("snapshot_move_right");
    }

    #[test]
    fn snapshot_move_down() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("j j");
        h.assert_snapshot("snapshot_move_down");
    }

    #[test]
    fn snapshot_word_forward() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w");
        h.assert_snapshot("snapshot_word_forward");
    }

    #[test]
    fn snapshot_word_end() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("e");
        h.assert_snapshot("snapshot_word_end");
    }

    #[test]
    fn snapshot_word_backward() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l");
        h.type_keys("b");
        h.assert_snapshot("snapshot_word_backward");
    }

    #[test]
    fn snapshot_word_forward_repeated() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w w");
        h.assert_snapshot("snapshot_word_forward_repeated");
    }

    #[test]
    fn snapshot_multi_cursor_move_right() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("C l l");
        h.assert_snapshot("snapshot_multi_cursor_move_right");
    }

    #[test]
    fn snapshot_goto_line_start() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w w");
        h.type_keys("home");
        h.assert_snapshot("snapshot_goto_line_start");
    }

    #[test]
    fn snapshot_goto_line_end() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("end");
        h.assert_snapshot("snapshot_goto_line_end");
    }

    #[test]
    fn snapshot_goto_line_end_empty_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n\nxyz\n");
        h.open_file(&path);
        h.type_keys("j");
        h.type_keys("end");
        h.assert_snapshot("snapshot_goto_line_end_empty_line");
    }

    #[test]
    fn snapshot_goto_file_start() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("j j l l");
        h.type_keys("g k");
        h.assert_snapshot("snapshot_goto_file_start");
    }

    #[test]
    fn snapshot_goto_last_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("g j");
        h.assert_snapshot("snapshot_goto_last_line");
    }

    #[test]
    fn snapshot_goto_first_nonwhitespace() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "    foo bar\n");
        h.open_file(&path);
        h.type_keys("g i");
        h.assert_snapshot("snapshot_goto_first_nonwhitespace");
    }

    #[test]
    fn snapshot_goto_first_nonwhitespace_empty_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n\nxyz\n");
        h.open_file(&path);
        h.type_keys("j");
        h.type_keys("g i");
        h.assert_snapshot("snapshot_goto_first_nonwhitespace_empty_line");
    }

    #[test]
    fn snapshot_extend_to_line_start() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w w");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendToLineStart);
        h.assert_snapshot("snapshot_extend_to_line_start");
    }

    #[test]
    fn snapshot_extend_to_line_end() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar baz\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendToLineEnd);
        h.assert_snapshot("snapshot_extend_to_line_end");
    }

    #[test]
    fn snapshot_extend_to_file_start() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("j j l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendToFileStart);
        h.assert_snapshot("snapshot_extend_to_file_start");
    }

    #[test]
    fn snapshot_extend_to_last_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendToLastLine);
        h.assert_snapshot("snapshot_extend_to_last_line");
    }

    #[test]
    fn snapshot_collapse_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w w");
        h.type_keys(";");
        h.assert_snapshot("snapshot_collapse_selection");
    }

    #[test]
    fn snapshot_flip_selections() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w");
        h.type_keys("alt-;");
        h.assert_snapshot("snapshot_flip_selections");
    }

    #[test]
    fn snapshot_select_all() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\ndef\n");
        h.open_file(&path);
        h.type_keys("%");
        h.assert_snapshot("snapshot_select_all");
    }

    #[test]
    fn snapshot_select_line_below_snaps_to_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("x");
        h.assert_snapshot("snapshot_select_line_below_snaps_to_line");
    }

    #[test]
    fn snapshot_select_line_below_extends_on_repeat() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("x x");
        h.assert_snapshot("snapshot_select_line_below_extends_on_repeat");
    }

    #[test]
    fn snapshot_keep_primary_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("C");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::KeepPrimarySelection);
        h.assert_snapshot("snapshot_keep_primary_selection");
    }

    #[test]
    fn rotate_selections_forward_cycles_primary() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("C C");
        assert_eq!(h.head_offsets(), vec![0, 4, 8]);
        assert_eq!(h.primary_head_offset(), 8);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionsForward);
        assert_eq!(h.primary_head_offset(), 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionsForward);
        assert_eq!(h.primary_head_offset(), 4);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionsForward);
        assert_eq!(h.primary_head_offset(), 8);
    }

    #[test]
    fn rotate_selections_backward_cycles_primary() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("C C");
        assert_eq!(h.primary_head_offset(), 8);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionsBackward);
        assert_eq!(h.primary_head_offset(), 4);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionsBackward);
        assert_eq!(h.primary_head_offset(), 0);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionsBackward);
        assert_eq!(h.primary_head_offset(), 8);
    }

    #[test]
    fn rotate_single_selection_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\ndef\n");
        h.open_file(&path);
        let before = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionsForward);
        assert_eq!(h.primary_head_offset(), before);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RotateSelectionsBackward);
        assert_eq!(h.primary_head_offset(), before);
    }

    #[test]
    fn count_prefix_rotate_forward_cycles_n_positions() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\njkl\n");
        h.open_file(&path);
        h.type_keys("C C C");
        assert_eq!(h.head_offsets(), vec![0, 4, 8, 12]);
        assert_eq!(h.primary_head_offset(), 12);
        h.type_keys("2 )");
        assert_eq!(
            h.primary_head_offset(),
            4,
            "2 ) from primary at offset 12 should land on offset 4 (wraps from 12 -> 0 -> 4)"
        );
    }

    #[test]
    fn count_prefix_rotate_backward_cycles_n_positions() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\njkl\n");
        h.open_file(&path);
        h.type_keys("C C C");
        assert_eq!(h.primary_head_offset(), 12);
        h.type_keys("2 (");
        assert_eq!(
            h.primary_head_offset(),
            4,
            "2 ( from primary at offset 12 should land on offset 4"
        );
    }

    #[test]
    fn count_prefix_rotate_full_cycle_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\ndef\nghi\njkl\n");
        h.open_file(&path);
        h.type_keys("C C C");
        let before = h.primary_head_offset();
        h.type_keys("4 )");
        assert_eq!(
            h.primary_head_offset(),
            before,
            "rotating by len cycles should leave the primary at the same offset"
        );
    }

    #[test]
    fn snapshot_trim_selections_strips_whitespace() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "  hello  \n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::TrimSelections);
        h.assert_snapshot("snapshot_trim_selections_strips_whitespace");
    }

    #[test]
    fn snapshot_trim_selections_all_whitespace_collapses_to_primary() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "   \n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::TrimSelections);
        h.assert_snapshot("snapshot_trim_selections_all_whitespace_collapses_to_primary");
    }

    fn page_scratch_content() -> String {
        (0..30).map(|i| format!("line{i:02}\n")).collect()
    }

    #[test]
    fn snapshot_page_down_scrolls_and_moves_cursor() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        h.assert_snapshot("snapshot_page_down_scrolls_and_moves_cursor");
    }

    #[test]
    fn snapshot_page_up_after_page_down_returns_to_top() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f ctrl-b");
        h.assert_snapshot("snapshot_page_up_after_page_down_returns_to_top");
    }

    #[test]
    fn snapshot_half_page_down() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-d");
        h.assert_snapshot("snapshot_half_page_down");
    }

    #[test]
    fn snapshot_half_page_up_from_bottom() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f ctrl-f ctrl-u");
        h.assert_snapshot("snapshot_half_page_up_from_bottom");
    }

    #[test]
    fn snapshot_page_down_clamps_at_last_line() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        h.type_keys("ctrl-f");
        h.assert_snapshot("snapshot_page_down_clamps_at_last_line");
    }

    #[test]
    fn snapshot_page_up_at_top_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-b");
        h.assert_snapshot("snapshot_page_up_at_top_is_noop");
    }

    #[test]
    fn goto_window_top_after_scroll_lands_at_scroll_row() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let scroll_before = h.editor_scroll_rows();
        let scroll_row = scroll_before[0];
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoWindowTop);
        let positions = h.cursor_display_positions();
        assert_eq!(positions, vec![(scroll_row, 0)]);
        assert_eq!(h.editor_scroll_rows(), scroll_before);
    }

    #[test]
    fn goto_window_center_lands_at_viewport_midpoint() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let scroll_before = h.editor_scroll_rows();
        let scroll_row = scroll_before[0];
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoWindowCenter);
        let positions = h.cursor_display_positions();
        assert!(positions[0].0 > scroll_row);
        assert_eq!(h.editor_scroll_rows(), scroll_before);
    }

    #[test]
    fn goto_window_bottom_lands_at_last_visible_row() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let scroll_before = h.editor_scroll_rows();
        let scroll_row = scroll_before[0];
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoWindowBottom);
        let positions = h.cursor_display_positions();
        assert!(
            positions[0].0 > scroll_row,
            "bottom row {} must be below scroll_row {}",
            positions[0].0,
            scroll_row
        );
        assert_eq!(h.editor_scroll_rows(), scroll_before);
    }

    #[test]
    fn goto_window_clamps_to_buffer_end() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoWindowBottom);
        let positions = h.cursor_display_positions();
        assert!(
            positions[0].0 <= 3,
            "cursor must clamp to last buffer row, got {}",
            positions[0].0
        );
    }

    #[test]
    fn align_view_top_scrolls_so_cursor_at_top() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let head_before = h.cursor_display_positions();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AlignViewTop);
        let scroll = h.editor_scroll_rows()[0];
        let head_after = h.cursor_display_positions();
        assert_eq!(
            scroll, head_before[0].0,
            "scroll_row should equal cursor row"
        );
        assert_eq!(head_after, head_before, "cursor row must not move");
    }

    #[test]
    fn align_view_center_puts_cursor_at_midpoint() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let head_before = h.cursor_display_positions();
        let cursor_row = head_before[0].0;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AlignViewCenter);
        let scroll = h.editor_scroll_rows()[0];
        let head_after = h.cursor_display_positions();
        assert!(
            scroll < cursor_row,
            "scroll {scroll} should be above cursor {cursor_row}"
        );
        assert!(
            cursor_row - scroll <= 5,
            "cursor at row {cursor_row}, scroll {scroll}: viewport midpoint should be roughly half a viewport up"
        );
        assert_eq!(head_after, head_before, "cursor row must not move");
    }

    #[test]
    fn align_view_bottom_puts_cursor_at_last_visible_row() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let head_before = h.cursor_display_positions();
        let cursor_row = head_before[0].0;
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AlignViewBottom);
        let scroll = h.editor_scroll_rows()[0];
        let head_after = h.cursor_display_positions();
        assert!(
            scroll <= cursor_row,
            "scroll {scroll} should be at or above cursor {cursor_row}"
        );
        assert_eq!(head_after, head_before, "cursor row must not move");
    }

    #[test]
    fn align_view_clamps_to_max_scroll() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AlignViewBottom);
        let scroll = h.editor_scroll_rows()[0];
        assert_eq!(
            scroll, 0,
            "buffer shorter than viewport must clamp scroll_row to 0"
        );
    }

    #[test]
    fn scroll_down_increments_scroll_row() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        let head_before = h.cursor_display_positions();
        let scroll_before = h.editor_scroll_rows()[0];
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ScrollDown);
        assert_eq!(h.editor_scroll_rows()[0], scroll_before + 1);
        assert_eq!(
            h.cursor_display_positions(),
            head_before,
            "cursor must not move"
        );
    }

    #[test]
    fn scroll_up_decrements_scroll_row() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let scroll_before = h.editor_scroll_rows()[0];
        assert!(scroll_before > 0);
        let head_before = h.cursor_display_positions();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ScrollUp);
        assert_eq!(h.editor_scroll_rows()[0], scroll_before - 1);
        assert_eq!(
            h.cursor_display_positions(),
            head_before,
            "cursor must not move"
        );
    }

    #[test]
    fn scroll_up_at_top_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ScrollUp);
        assert_eq!(h.editor_scroll_rows()[0], 0);
    }

    #[test]
    fn scroll_down_clamps_at_max_scroll() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        for _ in 0..5 {
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ScrollDown);
        }
        assert_eq!(
            h.editor_scroll_rows()[0],
            0,
            "buffer shorter than viewport keeps scroll_row at 0"
        );
    }

    #[test]
    fn count_prefix_scroll_down_advances_n_rows() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        let scroll_before = h.editor_scroll_rows()[0];
        h.type_keys("3 z j");
        assert_eq!(h.editor_scroll_rows()[0], scroll_before + 3);
    }

    #[test]
    fn count_prefix_scroll_up_walks_back_n_rows() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("ctrl-f");
        let scroll_before = h.editor_scroll_rows()[0];
        assert!(scroll_before >= 3);
        h.type_keys("3 z k");
        assert_eq!(h.editor_scroll_rows()[0], scroll_before - 3);
    }

    #[test]
    fn count_prefix_scroll_down_clamps_at_max_scroll() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", &page_scratch_content());
        h.open_file(&path);
        h.type_keys("9 9 z j");
        let scroll = h.editor_scroll_rows()[0];
        let saturating = h.editor_scroll_rows()[0];
        h.type_keys("z j");
        assert_eq!(
            h.editor_scroll_rows()[0],
            saturating,
            "scroll_row should be at max_scroll after huge count; further scroll-down is a no-op (got {scroll} -> {})",
            h.editor_scroll_rows()[0]
        );
    }

    fn focused_buffer_text(h: &mut crate::test_harness::TestHarness) -> String {
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            crate::pane::View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let guard = buffer.read().expect("poisoned");
        guard.rope().to_string()
    }

    #[test]
    fn switch_case_uppercases_lowercase_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "hello\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        assert_eq!(focused_buffer_text(&mut h), "HELLO\n");
    }

    #[test]
    fn switch_case_lowercases_uppercase_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "HELLO\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        assert_eq!(focused_buffer_text(&mut h), "hello\n");
    }

    #[test]
    fn switch_case_toggles_mixed_case() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "Hello World\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        assert_eq!(focused_buffer_text(&mut h), "hELLO wORLD\n");
    }

    #[test]
    fn switch_case_passes_through_non_letters() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc 123!\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        assert_eq!(focused_buffer_text(&mut h), "ABC 123!\n");
    }

    #[test]
    fn increment_seeks_forward_to_next_digit_on_line() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 42\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Increment);
        assert_eq!(focused_buffer_text(&mut h), "let x = 43\n");
    }

    #[test]
    fn increment_no_op_when_line_has_no_digit() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\n42\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Increment);
        assert_eq!(
            focused_buffer_text(&mut h),
            "abc\n42\n",
            "seek should not cross newline"
        );
    }

    #[test]
    fn increment_hex_preserves_lowercase_and_width() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0x0f\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Increment);
        assert_eq!(focused_buffer_text(&mut h), "let x = 0x10\n");
    }

    #[test]
    fn increment_hex_grows_width_on_overflow() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0xff\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Increment);
        assert_eq!(focused_buffer_text(&mut h), "let x = 0x100\n");
    }

    #[test]
    fn increment_hex_uses_uppercase_when_input_was_uppercase() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0xFE\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Increment);
        assert_eq!(focused_buffer_text(&mut h), "let x = 0xFF\n");
    }

    #[test]
    fn decrement_binary_preserves_width() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0b1010\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Decrement);
        assert_eq!(focused_buffer_text(&mut h), "let x = 0b1001\n");
    }

    #[test]
    fn increment_octal_preserves_width() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0o17\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Increment);
        assert_eq!(focused_buffer_text(&mut h), "let x = 0o20\n");
    }

    #[test]
    fn decrement_hex_saturates_at_zero() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0x00\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Decrement);
        assert_eq!(focused_buffer_text(&mut h), "let x = 0x00\n");
    }

    #[test]
    fn increment_hex_underscored_no_width_change() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0xab_cd\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Increment);
        assert_eq!(focused_buffer_text(&mut h), "let x = 0xab_ce\n");
    }

    #[test]
    fn increment_hex_underscored_overflow_regroups_right() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0xff_ff\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Increment);
        assert_eq!(focused_buffer_text(&mut h), "let x = 0x1_00_00\n");
    }

    #[test]
    fn decrement_binary_underscored_preserves_width() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0b1010_1010\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Decrement);
        assert_eq!(focused_buffer_text(&mut h), "let x = 0b1010_1001\n");
    }

    #[test]
    fn decrement_hex_underscored_borrow_pads_left() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0x10_00_00_00\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Decrement);
        assert_eq!(focused_buffer_text(&mut h), "let x = 0x0f_ff_ff_ff\n");
    }

    #[test]
    fn count_prefix_increment_adds_count() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 10\n");
        h.open_file(&path);
        h.type_keys("5 ctrl-a");
        assert_eq!(focused_buffer_text(&mut h), "let x = 15\n");
    }

    #[test]
    fn count_prefix_decrement_subtracts_count() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 10\n");
        h.open_file(&path);
        h.type_keys("3 ctrl-x");
        assert_eq!(focused_buffer_text(&mut h), "let x = 7\n");
    }

    #[test]
    fn count_prefix_increment_hex_uses_count() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0x10\n");
        h.open_file(&path);
        h.type_keys("4 ctrl-a");
        assert_eq!(focused_buffer_text(&mut h), "let x = 0x14\n");
    }

    #[test]
    fn select_mode_v_enters_then_h_extends_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("3 l");
        let before = h.selection_spans();
        h.type_keys("v");
        assert_eq!(h.stoat.mode, "select");
        h.type_keys("h h");
        let after = h.selection_spans();
        assert_ne!(after, before, "selection should have extended");
        assert_eq!(after[0].0, 1, "tail of extended selection at byte 1");
        assert_eq!(after[0].1, 3, "head of extended selection back at byte 3");
    }

    #[test]
    fn count_prefix_in_select_mode_extends_n_lines() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\n");
        h.open_file(&path);
        h.type_keys("v");
        assert_eq!(h.stoat.mode, "select");
        h.type_keys("3 j");
        let spans = h.selection_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].0, 0,
            "anchor stays at byte 0 while head extends down"
        );
        assert_eq!(
            spans[0].1, 6,
            "3 j in select mode should extend the head three lines down"
        );
    }

    #[test]
    fn select_mode_v_exits_back_to_normal() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("v");
        assert_eq!(h.stoat.mode, "select");
        h.type_keys("v");
        assert_eq!(h.stoat.mode, "normal");
    }

    #[test]
    fn select_mode_escape_exits_back_to_normal() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("v");
        assert_eq!(h.stoat.mode, "select");
        h.type_keys("Escape");
        assert_eq!(h.stoat.mode, "normal");
    }

    #[test]
    fn select_mode_status_label_is_sel() {
        let theme = crate::theme::Theme::empty();
        let (label, _) = crate::render::pane::mode_segment("select", &theme);
        assert_eq!(label, "SEL");
    }

    #[test]
    fn select_mode_semicolon_collapses_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v l l l");
        let before = h.selection_spans()[0];
        assert!(before.1 > before.0, "selection should be non-empty");
        h.type_keys(";");
        let after = h.selection_spans()[0];
        assert_eq!(after.0, after.1, "; should collapse to a cursor");
    }

    #[test]
    fn select_mode_alt_semicolon_flips_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v l l l");
        let before = h.selection_spans()[0];
        h.type_keys("Alt-;");
        let after = h.selection_spans()[0];
        assert_eq!(after.0, before.0, "tail/head ranges remain the same");
        assert_eq!(after.1, before.1);
        assert_ne!(after.2, before.2, "reversed flag flipped");
    }

    #[test]
    fn select_mode_indent_indents_selection_lines() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\ndef\n");
        h.open_file(&path);
        h.type_keys("v j l");
        h.type_keys(">");
        assert_eq!(focused_buffer_text(&mut h), "\tabc\n\tdef\n");
    }

    #[test]
    fn select_mode_delete_removes_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v l l l");
        h.type_keys("d");
        assert_eq!(focused_buffer_text(&mut h), "def\n");
    }

    #[test]
    fn select_mode_tilde_switches_case() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v l l l");
        h.type_keys("~");
        assert_eq!(focused_buffer_text(&mut h), "ABCdef\n");
    }

    #[test]
    fn select_mode_undo_reverts_prior_edit() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v l l l");
        h.type_keys("d");
        assert_eq!(focused_buffer_text(&mut h), "def\n");
        h.type_keys("u");
        assert_eq!(focused_buffer_text(&mut h), "abcdef\n");
    }

    #[test]
    fn select_mode_alt_o_expands_selection_to_enclosing_node() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {}\n");
        h.open_file(&path);
        h.type_keys("l l l v");
        let before = h.selection_spans()[0];
        h.type_keys("Alt-o");
        let after = h.selection_spans()[0];
        assert_eq!(h.stoat.mode, "select", "Alt-o stays in select mode");
        assert!(
            after.0 <= before.0 && after.1 > before.1,
            "expansion should cover and exceed the prior selection ({before:?} -> {after:?})"
        );
    }

    #[test]
    fn select_mode_alt_i_shrinks_back_after_expand() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {}\n");
        h.open_file(&path);
        h.type_keys("l l l v");
        let before = h.selection_spans();
        h.type_keys("Alt-o");
        assert_ne!(h.selection_spans(), before, "Alt-o should grow selection");
        h.type_keys("Alt-i");
        assert_eq!(h.stoat.mode, "select", "Alt-i stays in select mode");
        assert_eq!(
            h.selection_spans(),
            before,
            "Alt-i should restore pre-expand selection"
        );
    }

    #[test]
    fn submode_status_labels() {
        let theme = crate::theme::Theme::empty();
        let cases = [
            ("goto", "GTO"),
            ("z", "VWA"),
            ("bracket_next", "BNX"),
            ("bracket_prev", "BPV"),
            ("match", "MAT"),
            ("select_goto", "SLG"),
            ("space", "SPC"),
            ("space_workspace", "SWS"),
            ("space_pane_nav", "SPN"),
            ("space_pane_nav_new", "SNN"),
            ("claude", "CLA"),
        ];
        for (mode, expected) in cases {
            let (label, _) = crate::render::pane::mode_segment(mode, &theme);
            assert_eq!(label, expected, "label for mode {mode:?}");
        }
    }

    #[test]
    fn select_mode_f_extends_forward_to_target_char() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v");
        h.type_keys("f e");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!((start, end, reversed), (0, 4, false));
    }

    #[test]
    fn select_mode_capital_f_extends_backward() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("4 l v");
        h.type_keys("F b");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!((start, end, reversed), (1, 4, true));
    }

    #[test]
    fn select_mode_t_extends_till_next_char() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v");
        h.type_keys("t e");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!((start, end, reversed), (0, 3, false));
    }

    #[test]
    fn select_mode_capital_t_extends_till_prev_char() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("4 l v");
        h.type_keys("T b");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!((start, end, reversed), (2, 4, true));
    }

    #[test]
    fn normal_mode_f_still_collapses_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("f e");
        let (start, end, _) = h.selection_spans()[0];
        assert_eq!(
            (start, end),
            (4, 4),
            "normal-mode find collapses to cursor at the 'e'"
        );
    }

    #[test]
    fn select_mode_alt_n_extends_to_next_sibling() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn a() {} fn b() {}\n");
        h.open_file(&path);
        h.type_keys("l l v");
        let before_offset = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendSelectNextSibling);
        let head_after = h.primary_head_offset();
        let (start, end, _reversed) = h.selection_spans()[0];
        assert!(
            head_after > before_offset,
            "head should have moved forward across siblings"
        );
        assert!(end > start, "selection has non-empty range");
    }

    #[test]
    fn select_mode_alt_p_extends_to_prev_sibling() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn a() {} fn b() {}\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l l l l l l v");
        let before_offset = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendSelectPrevSibling);
        let head_after = h.primary_head_offset();
        let (start, end, _reversed) = h.selection_spans()[0];
        assert!(
            head_after < before_offset,
            "head should have moved backward across siblings"
        );
        assert!(end > start, "selection has non-empty range");
    }

    #[test]
    fn normal_mode_alt_n_still_collapses() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn a() {} fn b() {}\n");
        h.open_file(&path);
        h.type_keys("l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
        let (start, end, _) = h.selection_spans()[0];
        assert!(end > start, "normal-mode sibling jump produces a range");
    }

    #[test]
    fn select_mode_alt_b_extends_to_parent_node_start() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() { let x = 1; }\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l l l l l l l l l l v");
        let before_offset = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendMoveParentNodeStart);
        let head_after = h.primary_head_offset();
        let (start, end, reversed) = h.selection_spans()[0];
        assert!(
            head_after < before_offset,
            "head should have moved earlier in the buffer"
        );
        assert!(reversed, "head ahead of tail means selection is reversed");
        assert!(end > start, "selection has non-empty range");
    }

    #[test]
    fn select_mode_alt_e_extends_to_parent_node_end() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() { let x = 1; }\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l l l l l l l l l l v");
        let before_offset = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendMoveParentNodeEnd);
        let head_after = h.primary_head_offset();
        let (start, end, reversed) = h.selection_spans()[0];
        assert!(
            head_after > before_offset,
            "head should have moved forward in the buffer"
        );
        assert!(!reversed, "head ahead of tail means selection is forward");
        assert!(end > start, "selection has non-empty range");
    }

    #[test]
    fn normal_mode_alt_b_still_collapses() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() { let x = 1; }\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l l l l l l l l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveParentNodeStart);
        let (start, end, _) = h.selection_spans()[0];
        assert_eq!(start, end, "normal-mode parent jump collapses to cursor");
    }

    #[test]
    fn select_mode_g_pipe_extends_to_column() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdefgh\n");
        h.open_file(&path);
        h.type_keys("v 5 g |");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!(
            (start, end, reversed),
            (0, 4, false),
            "head extended to column 5 (offset 4) while tail stays at 0"
        );
        assert_eq!(h.stoat.mode, "select", "back to select after the chord");
    }

    #[test]
    fn normal_mode_g_pipe_still_collapses() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdefgh\n");
        h.open_file(&path);
        h.type_keys("5 g |");
        assert_eq!(h.cursor_display_positions(), vec![(0, 4)]);
        assert_eq!(h.stoat.mode, "normal");
    }

    #[test]
    fn select_mode_g_i_extends_to_first_nonwhitespace() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "    hello\n");
        h.open_file(&path);
        h.type_keys("8 l v");
        h.type_keys("g i");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!((start, end, reversed), (4, 8, true));
        assert_eq!(h.stoat.mode, "select");
    }

    #[test]
    fn select_mode_g_j_extends_to_last_line() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\n");
        h.open_file(&path);
        h.type_keys("v");
        h.type_keys("g j");
        let head = h.primary_head_offset();
        assert_eq!(head, 6, "head extended to start of last content line");
        assert_eq!(h.stoat.mode, "select");
    }

    #[test]
    fn select_mode_g_k_extends_to_file_start() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\n");
        h.open_file(&path);
        h.type_keys("j j j v");
        h.type_keys("g k");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!((start, end, reversed), (0, 6, true));
        assert_eq!(h.stoat.mode, "select");
    }

    #[test]
    fn select_mode_g_t_extends_to_window_top() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
        h.open_file(&path);
        h.type_keys("j j v");
        h.type_keys("g t");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!(
            (start, end, reversed),
            (0, 4, true),
            "head extended to row 0; tail at the original cursor row 2"
        );
        assert_eq!(h.stoat.mode, "select");
    }

    #[test]
    fn normal_mode_g_i_still_collapses() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "    hello\n");
        h.open_file(&path);
        h.type_keys("8 l");
        h.type_keys("g i");
        let (start, end, _) = h.selection_spans()[0];
        assert_eq!((start, end), (4, 4));
        assert_eq!(h.stoat.mode, "normal");
    }

    #[test]
    fn select_goto_escape_returns_to_select() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("v g");
        assert_eq!(h.stoat.mode, "select_goto");
        h.type_keys("Escape");
        assert_eq!(h.stoat.mode, "select");
    }

    #[test]
    fn repeat_last_motion_extends_in_select_mode() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "ababab\n");
        h.open_file(&path);
        h.type_keys("v");
        h.type_keys("f a");
        let after_first = h.selection_spans()[0];
        h.type_keys("Alt-.");
        let after_repeat = h.selection_spans()[0];
        assert!(
            after_repeat.1 > after_first.1,
            "Alt-. should extend further forward, got {after_first:?} -> {after_repeat:?}"
        );
        assert_eq!(after_repeat.0, 0, "tail still anchored at the start");
    }

    #[test]
    fn switch_case_empty_selection_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        let before = focused_buffer_text(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        assert_eq!(focused_buffer_text(&mut h), before);
    }

    #[test]
    fn switch_to_uppercase_lower_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "hello\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchToUppercase);
        assert_eq!(focused_buffer_text(&mut h), "HELLO\n");
    }

    #[test]
    fn switch_to_uppercase_mixed_selection_is_idempotent_for_uppers() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "Hello World!\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchToUppercase);
        assert_eq!(focused_buffer_text(&mut h), "HELLO WORLD!\n");
    }

    #[test]
    fn switch_to_lowercase_upper_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "HELLO\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchToLowercase);
        assert_eq!(focused_buffer_text(&mut h), "hello\n");
    }

    #[test]
    fn switch_to_lowercase_mixed_selection_is_idempotent_for_lowers() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "Hello World!\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchToLowercase);
        assert_eq!(focused_buffer_text(&mut h), "hello world!\n");
    }

    #[test]
    fn delete_selection_removes_full_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "hello world\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
        assert_eq!(focused_buffer_text(&mut h), "");
    }

    #[test]
    fn delete_selection_empty_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "hello\n");
        h.open_file(&path);
        let before = focused_buffer_text(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
        assert_eq!(focused_buffer_text(&mut h), before);
    }

    #[test]
    fn delete_selection_removes_each_split_cursor_range() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("% alt-s");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
        assert_eq!(focused_buffer_text(&mut h), "\n\n\n");
    }

    #[test]
    fn toggle_comments_rust_single_line_inserts_prefix() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "let x = 42;\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(focused_buffer_text(&mut h), "// let x = 42;\n");
    }

    #[test]
    fn toggle_comments_rust_round_trip() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "let x = 42;\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(focused_buffer_text(&mut h), "let x = 42;\n");
    }

    #[test]
    fn toggle_comments_rust_multi_line_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {\n    let x = 42;\n}\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "// fn main() {\n    // let x = 42;\n// }\n",
            "prefix added at first non-whitespace on each line"
        );
    }

    #[test]
    fn toggle_comments_rust_skips_whitespace_only_lines() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "abc\n   \nxyz\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "// abc\n   \n// xyz\n",
            "blank line in the middle stays uncommented"
        );
    }

    #[test]
    fn toggle_comments_rust_independent_per_line_when_mixed() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "// abc\nxyz\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "abc\n// xyz\n",
            "each line toggles independently"
        );
    }

    #[test]
    fn toggle_comments_toml_uses_hash() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.toml", "key = 1\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(focused_buffer_text(&mut h), "# key = 1\n");
    }

    #[test]
    fn toggle_comments_json_no_op() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.json", "{}\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "{}\n",
            "json has no line_comment, action no-ops"
        );
    }

    #[test]
    fn indent_selection_inserts_tab_at_cursor_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::IndentSelection);
        assert_eq!(focused_buffer_text(&mut h), "\tabc\n");
    }

    #[test]
    fn indent_selection_indents_every_covered_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::IndentSelection);
        assert_eq!(focused_buffer_text(&mut h), "\tabc\n\tdef\n\tghi\n");
    }

    #[test]
    fn unindent_selection_removes_leading_tab() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "\tabc\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnindentSelection);
        assert_eq!(focused_buffer_text(&mut h), "abc\n");
    }

    #[test]
    fn unindent_selection_removes_up_to_four_leading_spaces() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "      abc\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnindentSelection);
        assert_eq!(focused_buffer_text(&mut h), "  abc\n");
    }

    #[test]
    fn unindent_selection_no_leading_whitespace_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnindentSelection);
        assert_eq!(focused_buffer_text(&mut h), "abc\n");
    }

    #[test]
    fn count_prefix_indent_inserts_n_tabs() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("3 >");
        assert_eq!(focused_buffer_text(&mut h), "\t\t\tabc\n");
    }

    #[test]
    fn count_prefix_unindent_removes_n_tabs() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "\t\t\tabc\n");
        h.open_file(&path);
        h.type_keys("2 <");
        assert_eq!(focused_buffer_text(&mut h), "\tabc\n");
    }

    #[test]
    fn count_prefix_unindent_removes_n_space_groups() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "        abc\n");
        h.open_file(&path);
        h.type_keys("2 <");
        assert_eq!(focused_buffer_text(&mut h), "abc\n");
    }

    #[test]
    fn count_prefix_unindent_clamps_at_available_indent() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "\tabc\n");
        h.open_file(&path);
        h.type_keys("9 <");
        assert_eq!(focused_buffer_text(&mut h), "abc\n");
    }

    #[test]
    fn indent_selection_dedupes_lines_across_multi_cursors() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("% alt-s");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::IndentSelection);
        assert_eq!(focused_buffer_text(&mut h), "\tabc\n\tdef\n\tghi\n");
    }

    #[test]
    fn align_selections_pads_shorter_lines_to_match_longest_head() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\ndefgh\nij\n");
        h.open_file(&path);
        h.type_keys("% alt-s");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AlignSelections);
        assert_eq!(focused_buffer_text(&mut h), "  abc\ndefgh\n   ij\n");
    }

    #[test]
    fn align_selections_already_aligned_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("% alt-s");
        let before = focused_buffer_text(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AlignSelections);
        assert_eq!(focused_buffer_text(&mut h), before);
    }

    #[test]
    fn align_selections_single_selection_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "hello\n");
        h.open_file(&path);
        h.type_keys("%");
        let before = focused_buffer_text(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AlignSelections);
        assert_eq!(focused_buffer_text(&mut h), before);
    }

    #[test]
    fn align_selections_skips_multi_line_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\ndefgh\nij\n");
        h.open_file(&path);
        h.type_keys("%");
        let before = focused_buffer_text(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AlignSelections);
        assert_eq!(focused_buffer_text(&mut h), before);
    }

    #[test]
    fn undo_after_single_edit_restores_text() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "hello\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
        assert_eq!(focused_buffer_text(&mut h), "");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        assert_eq!(focused_buffer_text(&mut h), "hello\n");
    }

    #[test]
    fn undo_consecutive_walks_history_back_to_origin() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        assert_eq!(focused_buffer_text(&mut h), "abc\n");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        assert_eq!(focused_buffer_text(&mut h), "ABC\n");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        assert_eq!(focused_buffer_text(&mut h), "abc\n");
    }

    #[test]
    fn undo_past_end_of_history_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "stays\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        let after_initial_undo = focused_buffer_text(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        assert_eq!(focused_buffer_text(&mut h), after_initial_undo);
    }

    #[test]
    fn redo_after_undo_restores_edit() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "hello\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
        assert_eq!(focused_buffer_text(&mut h), "");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        assert_eq!(focused_buffer_text(&mut h), "hello\n");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Redo);
        assert_eq!(focused_buffer_text(&mut h), "");
    }

    #[test]
    fn redo_with_empty_redo_stack_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "hello\n");
        h.open_file(&path);
        let before = focused_buffer_text(&mut h);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Redo);
        assert_eq!(focused_buffer_text(&mut h), before);
    }

    #[test]
    fn count_prefix_undo_walks_back_n_steps() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        assert_eq!(focused_buffer_text(&mut h), "ABC\n");
        h.type_keys("3 u");
        assert_eq!(focused_buffer_text(&mut h), "abc\n");
    }

    #[test]
    fn count_prefix_redo_walks_forward_n_steps() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        h.type_keys("3 u");
        assert_eq!(focused_buffer_text(&mut h), "abc\n");
        h.type_keys("3 U");
        assert_eq!(focused_buffer_text(&mut h), "ABC\n");
    }

    #[test]
    fn count_prefix_undo_redo_round_trip_with_huge_count() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        let after_edit = focused_buffer_text(&mut h);
        h.type_keys("9 9 u");
        h.type_keys("9 9 U");
        assert_eq!(
            focused_buffer_text(&mut h),
            after_edit,
            "huge undo + huge redo should round-trip back to post-edit state"
        );
    }

    fn install_diff_hunks(h: &mut crate::test_harness::TestHarness, line_starts: &[u32]) {
        use crate::diff_map::{DiffHunk, DiffHunkStatus, DiffMap};
        let hunks: Vec<DiffHunk> = line_starts
            .iter()
            .map(|&start| DiffHunk {
                status: DiffHunkStatus::Added,
                buffer_start_line: start,
                buffer_line_range: start..(start + 1),
                base_byte_range: 0..0,
                anchor_range: None,
                token_detail: None,
            })
            .collect();
        let dm = DiffMap::from_hunks(hunks, None);
        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let editor_id = match ws.panes.pane(focused).view {
            crate::pane::View::Editor(id) => id,
            _ => panic!("focused pane is not an editor"),
        };
        let buffer_id = ws.editors[editor_id].buffer_id;
        let buffer = ws.buffers.get(buffer_id).expect("buffer");
        let mut guard = buffer.write().expect("poisoned");
        guard.diff_map = Some(dm);
    }

    #[test]
    fn goto_next_change_jumps_forward() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
        h.open_file(&path);
        install_diff_hunks(&mut h, &[2, 5]);

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
        assert_eq!(h.primary_head_offset(), 4);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
        assert_eq!(h.primary_head_offset(), 10);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
        assert_eq!(h.primary_head_offset(), 10);
    }

    #[test]
    fn goto_prev_change_jumps_backward() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
        h.open_file(&path);
        install_diff_hunks(&mut h, &[2, 5]);
        h.type_keys("g j");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevChange);
        assert_eq!(h.primary_head_offset(), 10);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevChange);
        assert_eq!(h.primary_head_offset(), 4);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevChange);
        assert_eq!(h.primary_head_offset(), 4);
    }

    #[test]
    fn count_prefix_goto_next_change_jumps_n_changes() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 15);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
        h.open_file(&path);
        install_diff_hunks(&mut h, &[2, 5, 8]);
        h.type_keys("2 ] g");
        assert_eq!(h.primary_head_offset(), 10);
    }

    #[test]
    fn count_prefix_goto_prev_change_jumps_back_n_changes() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 15);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
        h.open_file(&path);
        install_diff_hunks(&mut h, &[2, 5, 8]);
        h.type_keys("g j");
        h.type_keys("2 [ g");
        assert_eq!(h.primary_head_offset(), 10);
    }

    #[test]
    fn count_prefix_goto_next_change_clamps_at_last() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 15);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
        h.open_file(&path);
        install_diff_hunks(&mut h, &[2, 5, 8]);
        h.type_keys("9 ] g");
        assert_eq!(h.primary_head_offset(), 16);
    }

    #[test]
    fn expand_selection_grows_from_cursor_to_token() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let spans = h.selection_spans();
        assert_eq!(spans, [(3, 7, false)]);
    }

    #[test]
    fn expand_selection_walks_to_parent_when_already_on_node() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let first = h.selection_spans()[0];
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let second = h.selection_spans()[0];
        assert!(
            second.0 <= first.0 && second.1 >= first.1 && second != first,
            "second expansion should cover at least the first ({first:?} -> {second:?})"
        );
    }

    #[test]
    fn expand_selection_dives_into_injection_layer() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 10);
        let path = h.write_file("s.md", "# Title\n\nSome **bold** text\n");
        h.open_file(&path);
        h.type_keys("j j 7 l");
        assert_eq!(
            h.primary_head_offset(),
            16,
            "test setup: cursor should be on 'b' in 'bold'"
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let snippet = "# Title\n\nSome **bold** text\n";
        let (start, end, _) = h.selection_spans()[0];
        assert!(end > start, "expansion produced empty range");
        let selected = &snippet[start..end];
        let inline_text = "Some **bold** text";
        assert!(
            selected.contains("bold") && selected.len() < inline_text.len(),
            "expected inner-grammar node containing 'bold' but tighter than the inline node \"{inline_text}\" ({}..{}), got {start}..{end} = {selected:?}",
            snippet.find(inline_text).unwrap(),
            snippet.find(inline_text).unwrap() + inline_text.len(),
        );
    }

    #[test]
    fn expand_selection_no_op_without_syntax_map() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "plain text content\n");
        h.open_file(&path);
        let before = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        assert_eq!(h.selection_spans(), before);
    }

    #[test]
    fn shrink_selection_restores_previous_after_expand() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        let before = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        assert_ne!(h.selection_spans(), before);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
        assert_eq!(h.selection_spans(), before);
    }

    #[test]
    fn shrink_walks_full_expansion_chain() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        let step0 = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let step1 = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let step2 = h.selection_spans();
        assert_ne!(step1, step0);
        assert_ne!(step2, step1);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
        assert_eq!(h.selection_spans(), step1);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
        assert_eq!(h.selection_spans(), step0);
    }

    #[test]
    fn shrink_with_no_history_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {}\n");
        h.open_file(&path);
        let before = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
        assert_eq!(h.selection_spans(), before);
    }

    #[test]
    fn count_prefix_expand_selection_walks_n_levels() {
        let mut h_count = crate::test_harness::TestHarness::with_size(40, 5);
        let path1 = h_count.write_file("s.rs", "fn main() {}\n");
        h_count.open_file(&path1);
        h_count.type_keys("l l l");
        h_count.type_keys("3 alt-o");
        let count_result = h_count.selection_spans();

        let mut h_loop = crate::test_harness::TestHarness::with_size(40, 5);
        let path2 = h_loop.write_file("s.rs", "fn main() {}\n");
        h_loop.open_file(&path2);
        h_loop.type_keys("l l l");
        for _ in 0..3 {
            crate::action_handlers::dispatch(&mut h_loop.stoat, &stoat_action::ExpandSelection);
        }
        let loop_result = h_loop.selection_spans();

        assert_eq!(
            count_result, loop_result,
            "count-prefix expand should match repeated single expand"
        );
    }

    #[test]
    fn count_prefix_shrink_selection_walks_back_n_levels() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        let before = h.selection_spans();
        for _ in 0..3 {
            crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        }
        assert_ne!(h.selection_spans(), before);
        h.type_keys("3 alt-i");
        assert_eq!(
            h.selection_spans(),
            before,
            "3 alt-i should rewind 3 expansions to the original selection"
        );
    }

    #[test]
    fn count_prefix_expand_selection_clamps_at_root() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn x() {}\n");
        h.open_file(&path);
        h.type_keys("l");
        h.type_keys("9 9 alt-o");
        let after_huge = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        assert_eq!(
            h.selection_spans(),
            after_huge,
            "additional expand at root should be a no-op"
        );
    }

    #[test]
    fn select_next_sibling_jumps_to_next_named_node() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let on_first_fn = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
        let on_second_fn = h.selection_spans();
        assert_ne!(on_second_fn, on_first_fn);
        assert!(
            on_second_fn[0].0 >= on_first_fn[0].1,
            "next sibling should start at or after first sibling end ({on_first_fn:?} -> {on_second_fn:?})"
        );
    }

    #[test]
    fn select_prev_sibling_walks_back() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let on_first_fn = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectPrevSibling);
        assert_eq!(h.selection_spans(), on_first_fn);
    }

    #[test]
    fn select_sibling_no_op_without_syntax_map() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "alpha beta gamma\n");
        h.open_file(&path);
        let before = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
        assert_eq!(h.selection_spans(), before);
    }

    #[test]
    fn count_prefix_select_sibling_walks_n_siblings() {
        let mut h_count = crate::test_harness::TestHarness::with_size(40, 5);
        let path1 = h_count.write_file("s.rs", "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n");
        h_count.open_file(&path1);
        h_count.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h_count.stoat, &stoat_action::ExpandSelection);
        crate::action_handlers::dispatch(&mut h_count.stoat, &stoat_action::ExpandSelection);
        h_count.type_keys("3 alt-n");
        let count_result = h_count.selection_spans();

        let mut h_loop = crate::test_harness::TestHarness::with_size(40, 5);
        let path2 = h_loop.write_file("s.rs", "fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\n");
        h_loop.open_file(&path2);
        h_loop.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h_loop.stoat, &stoat_action::ExpandSelection);
        crate::action_handlers::dispatch(&mut h_loop.stoat, &stoat_action::ExpandSelection);
        for _ in 0..3 {
            crate::action_handlers::dispatch(&mut h_loop.stoat, &stoat_action::SelectNextSibling);
        }
        let loop_result = h_loop.selection_spans();

        assert_eq!(
            count_result, loop_result,
            "count-prefix select_sibling should match repeated single dispatch"
        );
    }

    #[test]
    fn count_prefix_select_sibling_clamps_at_chain_end() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        h.type_keys("9 alt-n");
        let after_huge = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
        assert_eq!(
            h.selection_spans(),
            after_huge,
            "next sibling at end-of-chain after huge count should be a no-op"
        );
    }

    #[test]
    fn count_prefix_move_to_parent_walks_higher_than_single_step() {
        let mut h_single = crate::test_harness::TestHarness::with_size(40, 5);
        let p1 = h_single.write_file("s.rs", "fn main() { let x = (1 + 2); }\n");
        h_single.open_file(&p1);
        h_single.type_keys("l l l l l l l l l l l l l l l l l l l l l l");
        let starting = h_single.primary_head_offset();
        crate::action_handlers::dispatch(&mut h_single.stoat, &stoat_action::MoveParentNodeStart);
        let single_offset = h_single.primary_head_offset();
        assert!(
            single_offset < starting,
            "1 Alt-b should move backward from {starting} (got {single_offset})"
        );

        let mut h_count = crate::test_harness::TestHarness::with_size(40, 5);
        let p2 = h_count.write_file("s.rs", "fn main() { let x = (1 + 2); }\n");
        h_count.open_file(&p2);
        h_count.type_keys("l l l l l l l l l l l l l l l l l l l l l l");
        h_count.type_keys("3 alt-b");
        let count_offset = h_count.primary_head_offset();
        assert!(
            count_offset < single_offset,
            "3 Alt-b should walk further up than 1 Alt-b ({single_offset} -> {count_offset})"
        );
    }

    #[test]
    fn select_sibling_no_op_at_tree_edge() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn only() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let before = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
        assert_eq!(h.selection_spans(), before);
    }

    #[test]
    fn move_parent_node_start_collapses_to_parent_start() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() { let x = 1; }\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l l l l l l l l l l");
        let before_offset = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveParentNodeStart);
        let after_offset = h.primary_head_offset();
        assert!(
            after_offset < before_offset,
            "MoveParentNodeStart should move cursor left from {before_offset} to a smaller offset (got {after_offset})"
        );
        let spans = h.selection_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, spans[0].1, "selection collapsed to cursor");
    }

    #[test]
    fn move_parent_node_end_collapses_to_parent_end() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() { let x = 1; }\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l l l l l l l l l l");
        let before_offset = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveParentNodeEnd);
        let after_offset = h.primary_head_offset();
        assert!(
            after_offset > before_offset,
            "MoveParentNodeEnd should move cursor right from {before_offset} to a larger offset (got {after_offset})"
        );
        let spans = h.selection_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(spans[0].0, spans[0].1, "selection collapsed to cursor");
    }

    #[test]
    fn jump_backward_restores_saved_position() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "abcdefghij\n");
        h.open_file(&path);
        h.type_keys("l l l");
        let saved = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l l");
        assert_ne!(h.primary_head_offset(), saved);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(h.primary_head_offset(), saved);
    }

    #[test]
    fn jump_forward_walks_back_after_jump_backward() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "abcdefghij\n");
        h.open_file(&path);
        h.type_keys("l l");
        let a = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l l");
        let b = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(h.primary_head_offset(), b);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(h.primary_head_offset(), a);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpForward);
        assert_eq!(h.primary_head_offset(), b);
    }

    #[test]
    fn jump_with_empty_jumplist_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "hello\n");
        h.open_file(&path);
        let before = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(h.primary_head_offset(), before);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpForward);
        assert_eq!(h.primary_head_offset(), before);
    }

    #[test]
    fn count_prefix_jump_backward_walks_n_entries() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "abcdefghij\n");
        h.open_file(&path);
        h.type_keys("l");
        let a = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        h.stoat.pending_count = Some(3);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(
            h.primary_head_offset(),
            a,
            "3 jumps back from the third saved position should land on the first save"
        );
    }

    #[test]
    fn count_prefix_jump_forward_walks_n_entries() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "abcdefghij\n");
        h.open_file(&path);
        h.type_keys("l");
        let _a = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        let _b = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        let c = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        h.stoat.pending_count = Some(3);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        h.stoat.pending_count = Some(2);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpForward);
        assert_eq!(
            h.primary_head_offset(),
            c,
            "2 jumps forward from oldest should reach the third save"
        );
    }

    #[test]
    fn count_prefix_jump_backward_clamps_at_history_start() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "abcdefghij\n");
        h.open_file(&path);
        h.type_keys("l");
        let a = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        h.stoat.pending_count = Some(99);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(
            h.primary_head_offset(),
            a,
            "huge count should clamp at the oldest jumplist entry"
        );
    }

    #[test]
    fn count_prefix_repeats_move_down() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
        h.open_file(&path);
        h.type_keys("4 j");
        let positions = h.cursor_display_positions();
        assert_eq!(positions, vec![(4, 0)]);
    }

    #[test]
    fn count_prefix_resets_after_motion() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
        h.open_file(&path);
        h.type_keys("4 j");
        let after_count = h.cursor_display_positions();
        assert_eq!(after_count, vec![(4, 0)]);
        h.type_keys("j");
        let after_plain = h.cursor_display_positions();
        assert_eq!(after_plain, vec![(5, 0)]);
    }

    #[test]
    fn find_next_char_jumps_forward() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcdefg\n");
        h.open_file(&path);
        h.type_keys("f c");
        assert_eq!(h.primary_head_offset(), 2);
    }

    #[test]
    fn find_next_char_no_match_keeps_cursor() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcdefg\n");
        h.open_file(&path);
        let before = h.primary_head_offset();
        h.type_keys("f z");
        assert_eq!(h.primary_head_offset(), before);
    }

    #[test]
    fn find_prev_char_jumps_backward() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcdefg\n");
        h.open_file(&path);
        h.type_keys("l l l l l l");
        h.type_keys("F b");
        assert_eq!(h.primary_head_offset(), 1);
    }

    #[test]
    fn till_next_char_lands_one_before() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcdefg\n");
        h.open_file(&path);
        h.type_keys("t c");
        assert_eq!(h.primary_head_offset(), 1);
    }

    #[test]
    fn till_prev_char_lands_one_after() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcdefg\n");
        h.open_file(&path);
        h.type_keys("l l l l l l");
        h.type_keys("T b");
        assert_eq!(h.primary_head_offset(), 2);
    }

    #[test]
    fn repeat_last_motion_replays_find_next_char() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcabcabc\n");
        h.open_file(&path);
        h.type_keys("f c");
        assert_eq!(h.primary_head_offset(), 2);
        h.type_keys("alt-.");
        assert_eq!(h.primary_head_offset(), 5);
        h.type_keys("alt-.");
        assert_eq!(h.primary_head_offset(), 8);
    }

    #[test]
    fn repeat_last_motion_with_no_history_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "hello\n");
        h.open_file(&path);
        let before = h.primary_head_offset();
        h.type_keys("alt-.");
        assert_eq!(h.primary_head_offset(), before);
    }

    #[test]
    fn repeat_last_motion_uses_most_recent_find_kind() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcabcabc\n");
        h.open_file(&path);
        h.type_keys("f c");
        assert_eq!(h.primary_head_offset(), 2);
        h.type_keys("F a");
        assert_eq!(h.primary_head_offset(), 0);
        h.type_keys("l l l l");
        assert_eq!(h.primary_head_offset(), 4);
        h.type_keys("alt-.");
        assert_eq!(h.primary_head_offset(), 3);
    }

    #[test]
    fn find_aborts_on_escape() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcdefg\n");
        h.open_file(&path);
        let before = h.primary_head_offset();
        h.type_keys("f");
        h.type_keys("Escape");
        h.type_keys("c");
        assert_eq!(h.primary_head_offset(), before);
    }

    #[test]
    fn count_prefix_find_next_char_jumps_to_nth_match() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcabcabc\n");
        h.open_file(&path);
        h.type_keys("3 f c");
        assert_eq!(h.primary_head_offset(), 8);
    }

    #[test]
    fn count_prefix_till_next_char_lands_one_before_nth_match() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcabcabc\n");
        h.open_file(&path);
        h.type_keys("2 t c");
        assert_eq!(h.primary_head_offset(), 4);
    }

    #[test]
    fn count_prefix_find_prev_char_walks_back_n_matches() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcabcabc\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l l l");
        assert_eq!(h.primary_head_offset(), 9);
        h.type_keys("3 F a");
        assert_eq!(h.primary_head_offset(), 0);
    }

    #[test]
    fn count_prefix_till_prev_char_lands_one_after_nth_match() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcabcabc\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l l l");
        h.type_keys("2 T a");
        assert_eq!(h.primary_head_offset(), 4);
    }

    #[test]
    fn count_prefix_find_no_op_when_fewer_than_count_matches() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcabc\n");
        h.open_file(&path);
        let before = h.primary_head_offset();
        h.type_keys("9 f c");
        assert_eq!(h.primary_head_offset(), before);
    }

    #[test]
    fn count_prefix_repeat_last_motion_advances_n_matches() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcabcabcabc\n");
        h.open_file(&path);
        h.type_keys("f c");
        assert_eq!(h.primary_head_offset(), 2);
        h.type_keys("3 alt-.");
        assert_eq!(h.primary_head_offset(), 11);
    }

    #[test]
    fn snapshot_pending_count_appears_in_status_bar() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("4");
        h.assert_snapshot("snapshot_pending_count_appears_in_status_bar");
    }

    #[test]
    fn bare_zero_jumps_to_line_start() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc def\n");
        h.open_file(&path);
        h.type_keys("l l l l");
        assert_eq!(h.primary_head_offset(), 4);
        h.type_keys("0");
        assert_eq!(h.primary_head_offset(), 0);
    }

    #[test]
    fn zero_accumulates_into_pending_count() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 50);
        let body: String = (0..50).map(|i| format!("line{i}\n")).collect();
        let path = h.write_file("s.txt", &body);
        h.open_file(&path);
        h.type_keys("4 0 j");
        let positions = h.cursor_display_positions();
        assert_eq!(positions[0].0, 40);
    }

    #[test]
    fn count_prefix_repeats_move_right() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdefghij\n");
        h.open_file(&path);
        h.type_keys("4 l");
        assert_eq!(h.primary_head_offset(), 4);
    }

    #[test]
    fn count_prefix_repeats_move_left() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdefghij\n");
        h.open_file(&path);
        h.type_keys("5 l");
        assert_eq!(h.primary_head_offset(), 5);
        h.type_keys("3 h");
        assert_eq!(h.primary_head_offset(), 2);
    }

    #[test]
    fn count_prefix_repeats_next_word_start() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "alpha beta gamma delta\n");
        h.open_file(&path);
        h.type_keys("3 w");
        // "alpha beta gamma " is 16 bytes; "delta" starts at offset 17.
        // After three next_word_start jumps, head sits at the end of
        // the third word. With shift_to_prev_char, head lands on the
        // last char before "delta" -- which is the space at offset 16.
        let positions = h.cursor_display_positions();
        assert_eq!(positions, vec![(0, 16)]);
    }

    #[test]
    fn count_prefix_repeats_prev_word_start() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "alpha beta gamma delta\n");
        h.open_file(&path);
        h.type_keys("g j");
        h.type_keys("3 b");
        let positions = h.cursor_display_positions();
        assert_eq!(positions[0].0, 0, "should be back on row 0");
        assert!(
            positions[0].1 < 16,
            "3b from end should land before delta (got col {})",
            positions[0].1
        );
    }

    #[test]
    fn count_prefix_repeats_next_long_word_start() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "foo.bar baz qux quux\n");
        h.open_file(&path);
        h.stoat.pending_count = Some(3);
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::defs::editor::MoveNextLongWordStart,
        );
        assert_eq!(
            h.primary_head_offset(),
            15,
            "long-word treats `foo.bar` as one word, so 3W from offset 0 \
             advances past `baz qux ` to the space before `quux`"
        );
    }

    #[test]
    fn goto_line_number_jumps_to_count_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\n");
        h.open_file(&path);
        h.type_keys("5 G");
        let positions = h.cursor_display_positions();
        assert_eq!(positions, vec![(4, 0)]);
    }

    #[test]
    fn goto_line_number_clamps_at_last_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\n");
        h.open_file(&path);
        h.type_keys("9 9 G");
        let positions = h.cursor_display_positions();
        assert_eq!(positions, vec![(3, 0)]);
    }

    #[test]
    fn goto_line_number_without_count_jumps_to_last_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
        h.open_file(&path);
        h.type_keys("G");
        let with_g = h.cursor_display_positions();
        let mut h2 = crate::test_harness::TestHarness::with_size(20, 10);
        let path2 = h2.write_file("s.txt", "a\nb\nc\nd\ne\n");
        h2.open_file(&path2);
        h2.type_keys("g j");
        let with_gj = h2.cursor_display_positions();
        assert_eq!(with_g, with_gj);
    }

    #[test]
    fn goto_column_jumps_to_count_column() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcdefgh\n");
        h.open_file(&path);
        h.type_keys("5 g |");
        assert_eq!(h.cursor_display_positions(), vec![(0, 4)]);
    }

    #[test]
    fn goto_column_without_count_lands_at_line_start() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcdefgh\n");
        h.open_file(&path);
        h.type_keys("l l l l g |");
        assert_eq!(h.cursor_display_positions(), vec![(0, 0)]);
    }

    #[test]
    fn goto_column_clamps_to_line_end() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("9 9 g |");
        assert_eq!(h.cursor_display_positions(), vec![(0, 3)]);
    }

    #[test]
    fn goto_column_stays_on_current_row() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcdef\nghijkl\nmnopqr\n");
        h.open_file(&path);
        h.type_keys("j 4 g |");
        assert_eq!(h.cursor_display_positions(), vec![(1, 3)]);
    }

    #[test]
    fn goto_column_walks_chars_not_bytes() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "αβγδε\n");
        h.open_file(&path);
        h.type_keys("3 g |");
        let offset = h.primary_head_offset();
        assert_eq!(
            offset, 4,
            "third column on a 2-byte-per-char line is byte 4"
        );
    }

    #[test]
    fn count_survives_setmode_chord() {
        let mut split = crate::test_harness::TestHarness::with_size(20, 5);
        let split_path = split.write_file("s.txt", "abcdefgh\n");
        split.open_file(&split_path);
        split.type_keys("5 g");
        split.type_keys("|");
        let mut chord = crate::test_harness::TestHarness::with_size(20, 5);
        let chord_path = chord.write_file("s.txt", "abcdefgh\n");
        chord.open_file(&chord_path);
        chord.type_keys("5 g |");
        assert_eq!(
            split.cursor_display_positions(),
            chord.cursor_display_positions()
        );
    }

    #[test]
    fn goto_next_paragraph_jumps_from_paragraph_start() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "alpha\nbeta\n\ngamma\ndelta\n");
        h.open_file(&path);
        h.type_keys("] p");
        assert_eq!(h.cursor_display_positions(), vec![(3, 0)]);
    }

    #[test]
    fn goto_next_paragraph_jumps_from_middle_of_paragraph() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "alpha\nbeta\n\ngamma\ndelta\n");
        h.open_file(&path);
        h.type_keys("j ] p");
        assert_eq!(h.cursor_display_positions(), vec![(3, 0)]);
    }

    #[test]
    fn goto_next_paragraph_no_op_at_buffer_end() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "alpha\nbeta\n");
        h.open_file(&path);
        h.type_keys("j ] p");
        assert_eq!(h.cursor_display_positions(), vec![(1, 0)]);
    }

    #[test]
    fn goto_next_paragraph_walks_through_multiple_blanks() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "alpha\n\n\n\nbeta\n");
        h.open_file(&path);
        h.type_keys("] p");
        assert_eq!(h.cursor_display_positions(), vec![(4, 0)]);
    }

    #[test]
    fn goto_prev_paragraph_jumps_from_paragraph_start() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "alpha\nbeta\n\ngamma\ndelta\n");
        h.open_file(&path);
        h.type_keys("j j j [ p");
        assert_eq!(h.cursor_display_positions(), vec![(0, 0)]);
    }

    #[test]
    fn goto_prev_paragraph_jumps_from_middle_of_paragraph() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "alpha\nbeta\n\ngamma\ndelta\n");
        h.open_file(&path);
        h.type_keys("j j j j [ p");
        assert_eq!(h.cursor_display_positions(), vec![(3, 0)]);
    }

    #[test]
    fn goto_prev_paragraph_no_op_at_buffer_start() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "alpha\nbeta\n\ngamma\n");
        h.open_file(&path);
        h.type_keys("[ p");
        assert_eq!(h.cursor_display_positions(), vec![(0, 0)]);
    }

    #[test]
    fn goto_next_paragraph_from_empty_line_lands_on_following_paragraph() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "alpha\n\nbeta\n");
        h.open_file(&path);
        h.type_keys("j ] p");
        assert_eq!(h.cursor_display_positions(), vec![(2, 0)]);
    }

    #[test]
    fn count_prefix_goto_next_paragraph_jumps_n_paragraphs() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\n\nb\n\nc\n\nd\n");
        h.open_file(&path);
        h.type_keys("3 ] p");
        assert_eq!(h.cursor_display_positions(), vec![(6, 0)]);
    }

    #[test]
    fn count_prefix_goto_prev_paragraph_jumps_back_n_paragraphs() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\n\nb\n\nc\n\nd\n");
        h.open_file(&path);
        h.type_keys("6 j");
        assert_eq!(h.cursor_display_positions(), vec![(6, 0)]);
        h.type_keys("3 [ p");
        assert_eq!(h.cursor_display_positions(), vec![(0, 0)]);
    }

    #[test]
    fn count_prefix_goto_next_paragraph_clamps_at_last_paragraph() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\n\nb\n");
        h.open_file(&path);
        h.type_keys("9 ] p");
        assert_eq!(h.cursor_display_positions(), vec![(2, 0)]);
    }

    #[test]
    fn match_brackets_jumps_open_to_close() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "(abc)\n");
        h.open_file(&path);
        h.type_keys("m m");
        assert_eq!(h.primary_head_offset(), 4);
    }

    #[test]
    fn match_brackets_jumps_close_to_open() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "(abc)\n");
        h.open_file(&path);
        h.type_keys("4 l m m");
        assert_eq!(h.primary_head_offset(), 0);
    }

    #[test]
    fn match_brackets_handles_nesting() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "((a)(b))\n");
        h.open_file(&path);
        h.type_keys("m m");
        assert_eq!(h.primary_head_offset(), 7);
    }

    #[test]
    fn match_brackets_handles_inner_close_to_inner_open() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "((a)(b))\n");
        h.open_file(&path);
        h.type_keys("3 l m m");
        assert_eq!(h.primary_head_offset(), 1);
    }

    #[test]
    fn match_brackets_supports_brackets_and_braces() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "[x]{y}\n");
        h.open_file(&path);
        h.type_keys("m m");
        assert_eq!(h.primary_head_offset(), 2);
        h.type_keys("l m m");
        assert_eq!(h.primary_head_offset(), 5);
    }

    #[test]
    fn match_brackets_no_op_off_bracket() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "(abc)\n");
        h.open_file(&path);
        h.type_keys("l m m");
        assert_eq!(h.primary_head_offset(), 1);
    }

    #[test]
    fn match_brackets_no_op_unbalanced() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "(abc\n");
        h.open_file(&path);
        h.type_keys("m m");
        assert_eq!(h.primary_head_offset(), 0);
    }

    #[test]
    fn match_brackets_with_multibyte_inside() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "(αβγ)\n");
        h.open_file(&path);
        h.type_keys("m m");
        assert_eq!(h.primary_head_offset(), 7);
    }

    #[test]
    fn match_brackets_skips_brace_in_string() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 5);
        let path = h.write_file("s.rs", "fn f() { \"}\" ; }\n");
        h.open_file(&path);
        h.type_keys("7 l");
        assert_eq!(h.primary_head_offset(), 7, "cursor on the opening `{{`");
        h.type_keys("m m");
        assert_eq!(
            h.primary_head_offset(),
            15,
            "naive scan would land on the `}}` inside the string at offset 10"
        );
    }

    #[test]
    fn match_brackets_skips_brace_in_block_comment() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 5);
        let path = h.write_file("s.rs", "fn f() { /* } */ }\n");
        h.open_file(&path);
        h.type_keys("7 l");
        assert_eq!(h.primary_head_offset(), 7, "cursor on the opening `{{`");
        h.type_keys("m m");
        assert_eq!(
            h.primary_head_offset(),
            17,
            "naive scan would land on the `}}` inside the block comment at offset 12"
        );
    }

    #[test]
    fn match_brackets_no_op_when_cursor_in_string() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 5);
        let path = h.write_file("s.rs", "fn f() { let s = \"()\"; }\n");
        h.open_file(&path);
        h.type_keys("1 8 l");
        assert_eq!(
            h.primary_head_offset(),
            18,
            "cursor on the `(` inside the string"
        );
        h.type_keys("m m");
        assert_eq!(
            h.primary_head_offset(),
            18,
            "cursor in string should not match brackets"
        );
    }

    #[test]
    fn count_prefix_word_clamps_at_buffer_edge() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("9 9 w");
        let offset = h.primary_head_offset();
        assert!(
            offset <= 4,
            "huge word count should clamp at buffer end (got {offset})"
        );
    }

    #[test]
    fn count_prefix_clamps_at_end_of_buffer() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("9 9 l");
        let offset = h.primary_head_offset();
        assert!(
            offset <= 4,
            "move_right with huge count should clamp at buffer end (got {offset})"
        );
    }

    #[test]
    fn count_prefix_no_op_when_binding_exists() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\n");
        h.open_file(&path);
        h.type_keys("j");
        let positions = h.cursor_display_positions();
        assert_eq!(positions, vec![(1, 0)]);
    }

    #[test]
    fn count_prefix_extends_select_line_below() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
        h.open_file(&path);
        h.type_keys("3 x");
        let spans = h.selection_spans();
        assert_eq!(spans.len(), 1);
        let three_lines_len = "a\nb\nc\n".len();
        assert_eq!(
            (spans[0].0, spans[0].1),
            (0, three_lines_len),
            "3x from line 0 should select three lines"
        );
    }

    #[test]
    fn count_prefix_extends_already_line_shaped_select_line_below() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\n");
        h.open_file(&path);
        h.type_keys("x");
        h.type_keys("2 x");
        let spans = h.selection_spans();
        assert_eq!(spans.len(), 1);
        let three_lines_len = "a\nb\nc\n".len();
        assert_eq!(
            (spans[0].0, spans[0].1),
            (0, three_lines_len),
            "x then 2x should grow to three lines total"
        );
    }

    #[test]
    fn count_prefix_select_line_below_clamps_at_buffer_end() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\n");
        h.open_file(&path);
        h.type_keys("9 9 x");
        let spans = h.selection_spans();
        assert_eq!(spans.len(), 1);
        let buffer_len = "a\nb\n".len();
        assert_eq!(
            (spans[0].0, spans[0].1),
            (0, buffer_len),
            "huge count should clamp at buffer end"
        );
    }

    #[test]
    fn save_selection_truncates_forward_history() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "abcdefghij\n");
        h.open_file(&path);
        h.type_keys("l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        let after_save = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpForward);
        assert_eq!(
            h.primary_head_offset(),
            after_save,
            "JumpForward after a fresh save should be a no-op (forward history was truncated)"
        );
    }

    #[test]
    fn move_to_parent_bound_no_op_without_syntax_map() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "alpha beta gamma\n");
        h.open_file(&path);
        let before = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveParentNodeStart);
        assert_eq!(h.selection_spans(), before);
    }

    #[test]
    fn shrink_after_cursor_move_does_not_restore_old_chain() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        h.type_keys("l l");
        let after_move = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        assert_ne!(h.selection_spans(), after_move);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
        assert_eq!(h.selection_spans(), after_move);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ShrinkSelection);
        assert_eq!(h.selection_spans(), after_move);
    }

    #[test]
    fn goto_next_change_no_op_without_diff_map() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        let before = h.primary_head_offset();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
        assert_eq!(h.primary_head_offset(), before);
    }
}
