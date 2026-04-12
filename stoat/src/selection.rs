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
}
