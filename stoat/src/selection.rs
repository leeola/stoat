use crate::multi_buffer::MultiBufferSnapshot;
use serde::{Deserialize, Serialize};
use stoat_text::{Anchor, Bias, Selection, SelectionGoal};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SelectionsCollection {
    next_selection_id: usize,
    disjoint: Vec<Selection<Anchor>>,
}

impl Default for SelectionsCollection {
    fn default() -> Self {
        Self::new()
    }
}

impl SelectionsCollection {
    pub fn new() -> Self {
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

    pub fn all_anchors(&self) -> &[Selection<Anchor>] {
        &self.disjoint
    }

    pub fn newest_anchor(&self) -> &Selection<Anchor> {
        self.disjoint
            .iter()
            .max_by_key(|s| s.id)
            .expect("SelectionsCollection invariant: at least one selection")
    }

    pub fn insert_cursor(
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

    pub fn set_single_range(&mut self, start: Anchor, end: Anchor, goal: SelectionGoal) {
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

    pub fn keep_primary(&mut self) {
        let primary = self.newest_anchor().clone();
        self.disjoint = vec![primary];
    }

    pub fn remove_primary(&mut self) {
        if self.disjoint.len() < 2 {
            return;
        }
        let primary_id = self.newest_anchor().id;
        self.disjoint.retain(|s| s.id != primary_id);
    }

    pub fn rotate_primary_by(&mut self, forward: bool, count: u32) {
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

    pub fn transform<F>(&mut self, snapshot: &MultiBufferSnapshot, mut f: F)
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
    pub fn split_each<F>(&mut self, snapshot: &MultiBufferSnapshot, mut split: F)
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
    pub fn replace_with(
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

    /// Replace selections from `(start, end, cursor_at_start)` byte-offset
    /// spans, assigning sequential ids and advancing the id counter past
    /// them so later cursor insertions stay monotonic. A span with
    /// `start == end` becomes a bare cursor; `cursor_at_start` maps to
    /// [`Selection::reversed`]. Anchors bind with [`Bias::Left`]. Panics
    /// when `spans` is empty (the collection must keep at least one
    /// selection). Test-support seeding entry point.
    pub fn set_from_offsets(
        &mut self,
        spans: &[(usize, usize, bool)],
        snapshot: &MultiBufferSnapshot,
    ) {
        let selections: Vec<Selection<Anchor>> = spans
            .iter()
            .enumerate()
            .map(|(id, &(start, end, reversed))| Selection {
                id,
                start: snapshot.anchor_at(start, Bias::Left),
                end: snapshot.anchor_at(end, Bias::Left),
                reversed,
                goal: SelectionGoal::None,
            })
            .collect();
        self.next_selection_id = selections.len();
        self.replace_with(selections, snapshot);
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
    fn set_from_offsets_seeds_spans_and_advances_id_counter() {
        let multi = singleton("abcdef");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        collection.set_from_offsets(&[(0, 0, false), (2, 4, true)], &snapshot);
        let sels = collection.all_anchors();
        assert_eq!(sels.len(), 2);
        // Sorted by offset: bare cursor at 0, then the 2..4 selection.
        assert_eq!(snapshot.resolve_anchor(&sels[0].start), 0);
        assert!(sels[0].is_empty());
        assert_eq!(snapshot.resolve_anchor(&sels[1].start), 2);
        assert_eq!(snapshot.resolve_anchor(&sels[1].end), 4);
        assert!(sels[1].reversed);

        // The id counter advanced past the seeded ids, so a fresh cursor
        // becomes the new max-id primary rather than colliding.
        collection.insert_cursor(
            snapshot.anchor_at(5, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );
        assert_eq!(collection.newest_anchor().id, 2);
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
    fn select_mode_status_label_is_sel() {
        let theme = crate::theme::Theme::empty();
        let badges = std::collections::BTreeMap::new();
        let (label, _) = crate::render::pane::mode_segment("select", &theme, &badges);
        assert_eq!(label, "SEL");
    }

    #[test]
    fn config_badge_overrides_hardcoded_label() {
        let theme = crate::theme::Theme::empty();
        let mut badges = std::collections::BTreeMap::new();
        badges.insert("select".to_string(), "VIS".to_string());
        let (label, _) = crate::render::pane::mode_segment("select", &theme, &badges);
        assert_eq!(label, "VIS");
    }

    #[test]
    fn config_badge_supplies_label_for_user_defined_mode() {
        let theme = crate::theme::Theme::empty();
        let mut badges = std::collections::BTreeMap::new();
        badges.insert("custom".to_string(), "CUS".to_string());
        let (label, _) = crate::render::pane::mode_segment("custom", &theme, &badges);
        assert_eq!(label, "CUS");
        // No badge entry -> hardcoded fallback for unknown mode is "---".
        let empty = std::collections::BTreeMap::new();
        let (label, _) = crate::render::pane::mode_segment("custom", &theme, &empty);
        assert_eq!(label, "---");
    }

    fn theme_from_src(src: &str) -> crate::theme::Theme {
        let (config, errors) = stoat_config::parse(src);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let config = config.expect("expected successful parse");
        crate::theme::Theme::from_config(&config, "t").expect("theme load failed")
    }

    #[test]
    fn theme_per_mode_color_overrides_default() {
        let theme = theme_from_src(r#"theme t { ui.statusline.normal.fg = red; }"#);
        let badges = std::collections::BTreeMap::new();
        let (_, color) = crate::render::pane::mode_segment("normal", &theme, &badges);
        assert_eq!(color, ratatui::style::Color::Red);
    }

    #[test]
    fn theme_per_mode_color_for_user_defined_mode() {
        let theme = theme_from_src(r#"theme t { ui.statusline.custom.fg = magenta; }"#);
        let badges = std::collections::BTreeMap::new();
        let (_, color) = crate::render::pane::mode_segment("custom", &theme, &badges);
        assert_eq!(color, ratatui::style::Color::Magenta);
    }

    #[test]
    fn legacy_submode_scope_still_colors_all_submodes() {
        let theme = theme_from_src(r#"theme t { ui.statusline.submode.fg = cyan; }"#);
        let badges = std::collections::BTreeMap::new();
        for mode in [
            "goto",
            "z",
            "match",
            "space",
            "space_workspace",
            "space_pane_nav",
            "claude",
        ] {
            let (_, color) = crate::render::pane::mode_segment(mode, &theme, &badges);
            assert_eq!(
                color,
                ratatui::style::Color::Cyan,
                "submode `{mode}` should inherit the legacy submode color",
            );
        }
    }

    #[test]
    fn theme_per_mode_color_wins_over_legacy_submode_scope() {
        let theme = theme_from_src(
            r#"theme t {
                ui.statusline.submode.fg = cyan;
                ui.statusline.goto.fg = red;
            }"#,
        );
        let badges = std::collections::BTreeMap::new();
        let (_, goto_color) = crate::render::pane::mode_segment("goto", &theme, &badges);
        let (_, space_color) = crate::render::pane::mode_segment("space", &theme, &badges);
        assert_eq!(goto_color, ratatui::style::Color::Red);
        assert_eq!(space_color, ratatui::style::Color::Cyan);
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
        let badges = std::collections::BTreeMap::new();
        for (mode, expected) in cases {
            let (label, _) = crate::render::pane::mode_segment(mode, &theme, &badges);
            assert_eq!(label, expected, "label for mode {mode:?}");
        }
    }
}
