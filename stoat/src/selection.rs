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

    pub(crate) fn rotate_primary(&mut self, forward: bool) {
        if self.disjoint.len() < 2 {
            return;
        }
        let primary_id = self.newest_anchor().id;
        let primary_idx = self
            .disjoint
            .iter()
            .position(|s| s.id == primary_id)
            .expect("primary id must be in disjoint");
        let len = self.disjoint.len();
        let new_idx = if forward {
            (primary_idx + 1) % len
        } else {
            (primary_idx + len - 1) % len
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
    fn rotate_primary_single_selection_is_noop() {
        let multi = singleton("abcdef");
        let _snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        let before_id = collection.newest_anchor().id;
        collection.rotate_primary(true);
        assert_eq!(collection.newest_anchor().id, before_id);
        collection.rotate_primary(false);
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
        collection.rotate_primary(true);
        assert_eq!(primary_offset(&collection), 0);
        collection.rotate_primary(true);
        assert_eq!(primary_offset(&collection), 3);
        collection.rotate_primary(true);
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
        collection.rotate_primary(false);
        assert_eq!(primary_offset(&collection), 3);
        collection.rotate_primary(false);
        assert_eq!(primary_offset(&collection), 0);
        collection.rotate_primary(false);
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
    fn snapshot_shift_c_adds_selection_below_styled() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("sample.txt", "abcd\nefgh\nijkl\n");

        h.open_file(&path);
        h.type_keys("shift-C");
        h.assert_snapshot("shift_c_adds_selection_below");
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
}
