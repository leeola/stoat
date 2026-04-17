use crate::multi_buffer::MultiBufferSnapshot;
use stoat_text::{Anchor, Selection, SelectionGoal};

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

    pub(crate) fn transform<F>(&mut self, snapshot: &MultiBufferSnapshot, mut f: F)
    where
        F: FnMut(&Selection<Anchor>) -> Selection<Anchor>,
    {
        let transformed: Vec<Selection<Anchor>> = self.disjoint.iter().map(&mut f).collect();
        let mut indexed: Vec<(usize, Selection<Anchor>)> = transformed
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
        let path = crate::test_harness::write_file(&h, "sample.txt", "abcd\nefgh\nijkl\n");

        h.open_file(&path);
        h.type_keys("C");
        h.assert_snapshot("add_selection_below");
    }

    #[test]
    fn snapshot_shift_c_adds_selection_below_styled() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = crate::test_harness::write_file(&h, "sample.txt", "abcd\nefgh\nijkl\n");

        h.open_file(&path);
        h.type_keys("shift-C");
        h.assert_snapshot("shift_c_adds_selection_below");
    }

    #[test]
    fn snapshot_move_right() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = crate::test_harness::write_file(&h, "s.txt", "hello world\n");
        h.open_file(&path);
        h.type_keys("l l l");
        h.assert_snapshot("snapshot_move_right");
    }

    #[test]
    fn snapshot_move_down() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = crate::test_harness::write_file(&h, "s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("j j");
        h.assert_snapshot("snapshot_move_down");
    }

    #[test]
    fn snapshot_word_forward() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = crate::test_harness::write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w");
        h.assert_snapshot("snapshot_word_forward");
    }

    #[test]
    fn snapshot_word_end() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = crate::test_harness::write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("e");
        h.assert_snapshot("snapshot_word_end");
    }

    #[test]
    fn snapshot_word_backward() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = crate::test_harness::write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("l l l l l l l");
        h.type_keys("b");
        h.assert_snapshot("snapshot_word_backward");
    }

    #[test]
    fn snapshot_word_forward_repeated() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = crate::test_harness::write_file(&h, "s.txt", "foo bar baz\n");
        h.open_file(&path);
        h.type_keys("w w");
        h.assert_snapshot("snapshot_word_forward_repeated");
    }

    #[test]
    fn snapshot_multi_cursor_move_right() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = crate::test_harness::write_file(&h, "s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("C l l");
        h.assert_snapshot("snapshot_multi_cursor_move_right");
    }
}
