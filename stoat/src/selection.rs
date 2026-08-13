use crate::multi_buffer::MultiBufferSnapshot;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use stoat_text::{
    next_char_boundaries_batch, prev_char_boundary, Anchor, Bias, Selection, SelectionGoal,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct SelectionsCollection {
    next_selection_id: usize,
    /// The selections, shared rather than owned.
    ///
    /// Every dispatched action captures this set for its undo group, so a set
    /// that could only be copied made each keystroke cost a list the length of
    /// the cursor count. Shared, a capture is a refcount bump.
    ///
    /// Nothing mutates through the handle. Each mutator builds a new list and
    /// installs it, which is what the set-replacing ones did anyway.
    disjoint: Arc<[Selection<Anchor>]>,
    /// Where in [`Self::disjoint`] the highest-id selection sits.
    ///
    /// The primary is asked for several times per keystroke and the set changes
    /// about once, so finding it is recorded when the set is installed rather
    /// than repeated on every read. Only [`Self::install`] writes it, which is
    /// what keeps it true.
    newest: usize,
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
            disjoint: Arc::from([default]),
            newest: 0,
        }
    }

    /// Install `disjoint` as the selection set, recording which entry is the
    /// primary.
    ///
    /// The one way the set changes. Every mutator builds its list and hands it
    /// here, so the recorded primary cannot fall out of step with the list it
    /// indexes.
    fn install(&mut self, disjoint: Arc<[Selection<Anchor>]>) {
        self.newest = newest_index(&disjoint);
        self.disjoint = disjoint;
    }

    pub(crate) fn all_anchors(&self) -> &[Selection<Anchor>] {
        &self.disjoint
    }

    /// The selection set as a handle that outlives the borrow.
    ///
    /// For a caller that has to keep the set rather than read it, such as an
    /// undo group capturing what to restore. Costs a refcount bump, where
    /// copying out of [`Self::all_anchors`] would cost the whole list.
    pub(crate) fn shared_anchors(&self) -> Arc<[Selection<Anchor>]> {
        Arc::clone(&self.disjoint)
    }

    pub(crate) fn newest_anchor(&self) -> &Selection<Anchor> {
        debug_assert_eq!(
            self.newest,
            newest_index(&self.disjoint),
            "the recorded primary drifted from the set it indexes"
        );

        self.disjoint
            .get(self.newest)
            .expect("SelectionsCollection invariant: at least one selection")
    }

    /// Rewrite every selection endpoint through `remap`.
    ///
    /// Moves a selection set onto a buffer state whose fragment tree was
    /// rebuilt rather than edited. The anchors name insertions that no longer
    /// exist there, so carrying them across would resolve to unrelated
    /// positions. Ordinary edits need nothing of the sort, since anchors are
    /// built to ride those.
    ///
    /// The caller decides what an endpoint maps to, so nothing here checks that
    /// the result stays ordered or deduplicated. Feeding it a
    /// position-preserving map keeps both.
    pub(crate) fn reanchor(&mut self, mut remap: impl FnMut(&Anchor) -> Anchor) {
        let remapped = self
            .disjoint
            .iter()
            .map(|selection| Selection {
                start: remap(&selection.start),
                end: remap(&selection.end),
                ..selection.clone()
            })
            .collect();
        self.install(remapped);
    }

    pub(crate) fn insert_cursor(
        &mut self,
        head: Anchor,
        goal: SelectionGoal,
        snapshot: &MultiBufferSnapshot,
    ) {
        let new_offset = snapshot.resolve_anchor(&head);

        // Widen to the 1-wide block before deduping. At the rope end the block
        // widens backward, so its start is not `new_offset`. Deduping on the
        // widened span keeps a clamped multi-cursor from stacking identical
        // cursors on the last cell.
        let widened = Selection {
            id: 0usize,
            start: new_offset,
            end: new_offset,
            reversed: false,
            goal,
        }
        .min_width_1(snapshot.rope());

        let pos = self
            .disjoint
            .binary_search_by(|s| snapshot.resolve_anchor(&s.start).cmp(&widened.start))
            .unwrap_or_else(|p| p);

        if let Some(existing) = self.disjoint.get(pos)
            && snapshot.resolve_anchor(&existing.start) == widened.start
            && snapshot.resolve_anchor(&existing.end) == widened.end
        {
            return;
        }

        let id = self.next_selection_id;
        self.next_selection_id += 1;
        let selection = Selection {
            id,
            start: snapshot.anchor_at(widened.start, Bias::Right),
            end: snapshot.anchor_at(widened.end, Bias::Right),
            reversed: false,
            goal,
        };
        self.install(insert_at(&self.disjoint, pos, selection));
    }

    /// Replace the collection with a single 1-wide block cursor over the first
    /// character, widened against `snapshot`.
    ///
    /// Seeds a freshly opened editor so its initial cursor covers a character
    /// like Helix, rather than the zero-width placeholder [`Self::new`] holds
    /// before a rope is available. An empty buffer leaves a zero-width cursor.
    pub(crate) fn seed_cursor(&mut self, snapshot: &MultiBufferSnapshot) {
        self.install(Arc::from([block_cursor_at(
            0,
            SelectionGoal::None,
            0,
            snapshot,
        )]));
        self.next_selection_id = 1;
    }

    pub(crate) fn set_single_range(&mut self, start: Anchor, end: Anchor, goal: SelectionGoal) {
        let id = self.next_selection_id;
        self.next_selection_id += 1;
        self.install(Arc::from([Selection {
            id,
            start,
            end,
            reversed: false,
            goal,
        }]));
    }

    /// Replace the collection with a single 1-wide block cursor over the
    /// character at `offset`, widened against `snapshot`.
    ///
    /// Mouse clicks land here so the cursor covers a character like the keyboard
    /// landing paths, rather than the zero-width selection that would render on
    /// the trailing phantom row when a click clips to the rope end.
    pub(crate) fn set_block_cursor(&mut self, offset: usize, snapshot: &MultiBufferSnapshot) {
        let id = self.next_selection_id;
        self.next_selection_id += 1;
        self.install(Arc::from([block_cursor_at(
            offset,
            SelectionGoal::None,
            id,
            snapshot,
        )]));
    }

    /// Replace the selection set with a saved `snapshot`, e.g. the selections an
    /// undo group captured before its edits. Bumps the id allocator past the
    /// snapshot so later insertions never reuse a restored id. An empty snapshot
    /// is ignored, keeping the invariant of at least one selection.
    pub(crate) fn restore(&mut self, snapshot: Arc<[Selection<Anchor>]>) {
        if snapshot.is_empty() {
            return;
        }
        if let Some(max_id) = snapshot.iter().map(|s| s.id).max() {
            self.next_selection_id = self.next_selection_id.max(max_id + 1);
        }
        self.install(snapshot);
    }

    pub(crate) fn keep_primary(&mut self) {
        let primary = self.newest_anchor().clone();
        self.install(Arc::from([primary]));
    }

    pub(crate) fn remove_primary(&mut self) {
        if self.disjoint.len() < 2 {
            return;
        }
        let primary_id = self.newest_anchor().id;
        let kept = self
            .disjoint
            .iter()
            .filter(|s| s.id != primary_id)
            .cloned()
            .collect();
        self.install(kept);
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
        let rotated = self
            .disjoint
            .iter()
            .enumerate()
            .map(|(idx, selection)| match idx == new_idx {
                true => Selection {
                    id: new_id,
                    ..selection.clone()
                },
                false => selection.clone(),
            })
            .collect();
        self.install(rotated);
    }

    pub(crate) fn transform<F>(&mut self, snapshot: &MultiBufferSnapshot, mut f: F)
    where
        F: FnMut(&Selection<Anchor>) -> Selection<Anchor>,
    {
        let transformed: Vec<Selection<Anchor>> = self.disjoint.iter().map(&mut f).collect();
        self.replace_with(transformed, snapshot);
    }

    /// [`Self::transform`] with each selection's endpoints already resolved.
    ///
    /// The closure receives its selection alongside the head and tail offsets,
    /// in that order. Resolving an anchor descends the fragment tree from the
    /// root, so a closure resolving its own turns a few hundred cursors into a
    /// few hundred pairs of descents per keystroke. Here every endpoint is
    /// resolved in one batch, which is two sorted walks for the whole set.
    ///
    /// Head and tail rather than start and end, that being what a motion asks
    /// for, and the two differing by whether the selection is reversed.
    pub(crate) fn transform_resolved<F>(&mut self, snapshot: &MultiBufferSnapshot, mut f: F)
    where
        F: FnMut(&Selection<Anchor>, usize, usize) -> Selection<Anchor>,
    {
        let offsets = {
            let anchors: Vec<Anchor> = self
                .disjoint
                .iter()
                .flat_map(|sel| [sel.head(), sel.tail()])
                .collect();
            snapshot.resolve_anchors_batch(&anchors)
        };

        let transformed: Vec<Selection<Anchor>> = self
            .disjoint
            .iter()
            .zip(offsets.chunks_exact(2))
            .map(|(sel, ends)| f(sel, ends[0], ends[1]))
            .collect();
        self.replace_with(transformed, snapshot);
    }

    /// Flat-map each selection into zero or more replacement pieces, given as
    /// `(start, end)` offsets.
    ///
    /// An empty vec keeps the original selection unchanged. A non-empty one
    /// replaces it with the pieces, each receiving a fresh id from this
    /// collection's allocator.
    ///
    /// Pieces arrive as offsets rather than anchored so every endpoint the
    /// split produces is minted in one walk, under `bias`. A splitter anchoring
    /// its own would pay a root descent each, and a regex over a large
    /// selection produces thousands.
    pub(crate) fn split_each<F>(&mut self, snapshot: &MultiBufferSnapshot, bias: Bias, mut split: F)
    where
        F: FnMut(&Selection<Anchor>) -> Vec<(usize, usize)>,
    {
        // Which selections were split and into what, kept apart from the
        // anchoring so the whole set's endpoints go through one batch.
        let split_into: Vec<Vec<(usize, usize)>> = self.disjoint.iter().map(&mut split).collect();

        let anchors = {
            let flat: Vec<usize> = split_into
                .iter()
                .flatten()
                .flat_map(|&(start, end)| [start, end])
                .collect();
            snapshot.anchors_at_batch(&flat, bias)
        };

        let mut anchors = anchors.chunks_exact(2);
        let mut new_disjoint: Vec<Selection<Anchor>> = Vec::with_capacity(self.disjoint.len());
        for (sel, pieces) in self.disjoint.iter().zip(&split_into) {
            if pieces.is_empty() {
                new_disjoint.push(sel.clone());
                continue;
            }
            for _ in pieces {
                let span = anchors.next().expect("two anchors per piece");
                new_disjoint.push(Selection {
                    id: self.next_selection_id,
                    start: span[0],
                    end: span[1],
                    reversed: false,
                    goal: SelectionGoal::None,
                });
                self.next_selection_id += 1;
            }
        }
        self.replace_with(new_disjoint, snapshot);
    }

    /// Replace selections with `new_disjoint`, giving each one a fresh id.
    ///
    /// For a producer building a set of selections that carry no identity yet,
    /// which is every producer that would otherwise leave them all at the
    /// default id. The collection tells selections apart by id and by nothing
    /// else. The primary is the highest-id one, removing it retains everything
    /// whose id differs, and switch-case, increment and paste all build maps
    /// keyed on it. Selections sharing an id are therefore one selection to all
    /// of them, and removing the primary of a set that shares one removes the
    /// whole set.
    ///
    /// [`Self::replace_with`] is for callers whose ids already mean something,
    /// such as a motion carrying each selection's identity forward.
    pub(crate) fn replace_with_fresh_ids(
        &mut self,
        mut new_disjoint: Vec<Selection<Anchor>>,
        snapshot: &MultiBufferSnapshot,
    ) {
        for selection in &mut new_disjoint {
            selection.id = self.next_selection_id;
            self.next_selection_id += 1;
        }
        self.replace_with(new_disjoint, snapshot);
    }

    /// Add `added` to the set, each taking a fresh id.
    ///
    /// Ids ascend in the order given, so the last one becomes the primary, as
    /// it would had each been inserted in turn.
    ///
    /// Adding one at a time cost a binary search whose comparator resolved an
    /// anchor per probe and an O(N) shift, per addition, where this is one sort
    /// and one pass. Overlaps merge rather than surviving side by side, which
    /// is the disjointness the collection is named for.
    pub(crate) fn extend_with_fresh_ids(
        &mut self,
        added: Vec<Selection<Anchor>>,
        snapshot: &MultiBufferSnapshot,
    ) {
        if added.is_empty() {
            return;
        }
        let mut new_disjoint = self.disjoint.to_vec();
        new_disjoint.extend(added.into_iter().map(|mut selection| {
            selection.id = self.next_selection_id;
            self.next_selection_id += 1;
            selection
        }));
        self.replace_with(new_disjoint, snapshot);
    }

    /// [`Self::replace_with_fresh_ids`] from `(start, end)` offsets, minting
    /// every endpoint under `bias` in one batched walk.
    ///
    /// For a producer that found its spans by scanning text rather than by
    /// moving selections around, and so holds offsets. Anchoring them itself
    /// would be a root descent per endpoint, which a regex over a large
    /// selection turns into thousands.
    pub(crate) fn replace_with_fresh_ids_from_offsets(
        &mut self,
        spans: &[(usize, usize)],
        bias: Bias,
        snapshot: &MultiBufferSnapshot,
    ) {
        let anchors = {
            let flat: Vec<usize> = spans
                .iter()
                .flat_map(|&(start, end)| [start, end])
                .collect();
            snapshot.anchors_at_batch(&flat, bias)
        };

        let new_disjoint: Vec<Selection<Anchor>> = anchors
            .chunks_exact(2)
            .map(|span| {
                let id = self.next_selection_id;
                self.next_selection_id += 1;
                Selection {
                    id,
                    start: span[0],
                    end: span[1],
                    reversed: false,
                    goal: SelectionGoal::None,
                }
            })
            .collect();
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
        /// A selection alongside the offsets its anchors resolve to, so the
        /// sort and the dedupe read a span already in hand rather than seeking
        /// the fragment tree per comparison.
        struct Resolved {
            start: usize,
            end: usize,
            selection: Selection<Anchor>,
        }

        // Every endpoint in one pass. Resolving anchors one at a time descends
        // from the root each time, which a few hundred cursors turn into
        // thousands of descents on a keystroke. The batch is two sorted walks
        // for the whole set, and returns results in input order.
        let offsets = {
            let anchors: Vec<Anchor> = new_disjoint
                .iter()
                .flat_map(|sel| [sel.start, sel.end])
                .collect();
            snapshot.resolve_anchors_batch(&anchors)
        };

        // A motion walks codepoints, and a combining mark is a codepoint that
        // categorizes apart from the letter it sits on, so a word motion stops
        // between the two. Almost every producer of selections arrives here, so
        // this is where the rule is applied rather than in each of them.
        //
        // The exceptions write into `disjoint` directly and each carry the rule
        // themselves. [`Self::insert_cursor`] widens through
        // `Selection::min_width_1`, which lands on whole clusters.
        // [`Self::set_block_cursor`] and [`Self::seed_cursor`] go through
        // [`block_cursor_at`], which clamps down before widening because
        // widening alone would start from wherever a click landed.
        // [`Self::restore`] replays anchors an earlier pass already clipped.
        //
        // Starts clamp down and ends clamp up, so a selection only ever grows
        // out to the character it was cutting, and start stays at or below end
        // without asking. An endpoint already on a boundary keeps the anchor it
        // came with rather than being rebuilt into an equivalent one.
        let rope = snapshot.rope();
        let snapped = {
            let requests: Vec<(usize, Bias)> = offsets
                .chunks_exact(2)
                .flat_map(|span| [(span[0], Bias::Left), (span[1], Bias::Right)])
                .collect();
            rope.clip_to_grapheme_boundaries_batch(&requests)
        };

        let mut indexed: Vec<Resolved> = new_disjoint
            .into_iter()
            .zip(offsets.chunks_exact(2).zip(snapped.chunks_exact(2)))
            .map(|(mut selection, (span, snapped))| {
                let (start, end) = (snapped[0], snapped[1]);
                if start != span[0] {
                    selection.start = snapshot.anchor_at(start, Bias::Left);
                }
                if end != span[1] {
                    selection.end = snapshot.anchor_at(end, Bias::Right);
                }
                Resolved {
                    start,
                    end,
                    selection,
                }
            })
            .collect();
        indexed.sort_by_key(|r| (r.start, r.selection.id));

        // Merge selections that overlap into the span their union covers,
        // keeping the highest-id survivor. Two selections over the same text
        // are one place the buffer is being edited, and letting both through
        // means editing the shared part twice, which consumes as much text
        // again beyond it. Sorted by start offset, anything overlapping is
        // adjacent to what it overlaps.
        //
        // Equal starts merge whatever the ends do, which is what collapses
        // duplicate cursors. Under the min-width-1 model those are identical
        // 1-wide ranges rather than empty points, as a multi-cursor page motion
        // landing every cursor on one row produces. Spans that merely touch
        // stay apart, covering disjoint text.
        let mut deduped: Vec<Resolved> = Vec::with_capacity(indexed.len());
        for entry in indexed {
            if let Some(prev) = deduped.last_mut()
                && (entry.start == prev.start || entry.start < prev.end)
            {
                let start_anchor = prev.selection.start;
                let (end, end_anchor) = if entry.end > prev.end {
                    (entry.end, entry.selection.end)
                } else {
                    (prev.end, prev.selection.end)
                };

                // Which way the merged span faces is a property of what went
                // into it, not of which input was minted later. `prev` already
                // carries the answer for everything merged into it so far.
                let reversed = prev.selection.reversed && entry.selection.reversed;
                if entry.selection.id > prev.selection.id {
                    prev.selection = entry.selection;
                }

                prev.end = end;
                prev.selection.reversed = reversed;
                prev.selection.start = start_anchor;
                prev.selection.end = end_anchor;
                continue;
            }
            deduped.push(entry);
        }
        self.install(deduped.into_iter().map(|r| r.selection).collect());
    }

    /// Every selection's id, cursor endpoints and vertical goal, resolved in one
    /// batch.
    ///
    /// [`Self::transform_resolved`]'s prologue without the transform, for a
    /// motion that works out where each cursor lands and hands the offsets to
    /// [`Self::land_block_cursors`] rather than building selections of its own.
    /// Resolving per selection instead would be two root descents each.
    ///
    /// Head and tail rather than start and end, matching `transform_resolved`
    /// and what a motion asks for.
    pub(crate) fn resolved_reads(&self, snapshot: &MultiBufferSnapshot) -> Vec<ResolvedRead> {
        let offsets = {
            let anchors: Vec<Anchor> = self
                .disjoint
                .iter()
                .flat_map(|sel| [sel.head(), sel.tail()])
                .collect();
            snapshot.resolve_anchors_batch(&anchors)
        };

        self.disjoint
            .iter()
            .zip(offsets.chunks_exact(2))
            .map(|(sel, ends)| ResolvedRead {
                id: sel.id,
                head: ends[0],
                tail: ends[1],
                goal: sel.goal,
                reversed: sel.reversed,
            })
            .collect()
    }

    /// Land a forward block cursor per selection, from offsets already in hand.
    ///
    /// `landings` carries a selection id, the offset its cursor lands on, and
    /// the vertical goal to keep, sorted by id. A selection the list does not
    /// name keeps the span, the goal, and the anchors it has. `end_cell` decides
    /// what a landing on the very end of the rope covers, where there is no next
    /// character to widen over.
    ///
    /// The clip, sort and merge are [`Self::replace_with`]'s, and the anchors
    /// come out the same. What it saves is the round trip: a caller that already
    /// knows where each cursor landed would otherwise mint two anchors per
    /// cursor, at a root descent each, only for `replace_with` to resolve them
    /// straight back to the offsets it was handed.
    pub(crate) fn land_block_cursors(
        &mut self,
        landings: &[(usize, usize, SelectionGoal)],
        end_cell: EndCell,
        snapshot: &MultiBufferSnapshot,
    ) {
        let rope = snapshot.rope();
        let forwards = {
            let starts: Vec<usize> = landings.iter().map(|&(_, start, _)| start).collect();
            next_char_boundaries_batch(rope, &starts)
        };

        self.land_from_offsets(snapshot, |sel| {
            let found = landings
                .binary_search_by_key(&sel.id, |(id, _, _)| *id)
                .ok()?;
            let (_, start, goal) = landings[found];
            let forward = forwards[found];
            // Nothing after the landing means it is on the end of the rope,
            // where `end_cell` decides between the character before it and no
            // character at all.
            let (start, end) = match (forward > start, end_cell) {
                (true, _) => (start, forward),
                (false, EndCell::Previous) => (prev_char_boundary(rope, start), start),
                (false, EndCell::Empty) => (start, start),
            };
            Some(SpanLanding {
                id: sel.id,
                start,
                end,
                reversed: false,
                goal,
            })
        });
    }

    /// Replace named selections with the spans given as offsets.
    ///
    /// `landings` carries a selection id, the span it lands on, which way it
    /// faces, and the goal to keep. A selection the list does not name keeps
    /// the span, the goal, and the anchors it has. Order does not matter, the
    /// lookup being by id.
    ///
    /// The counterpart to [`Self::land_block_cursors`] for a caller landing a
    /// span rather than a cursor. What it saves is the same round trip: every
    /// anchor is minted in one batched walk instead of two root descents per
    /// selection.
    pub(crate) fn replace_from_offsets(
        &mut self,
        landings: &[SpanLanding],
        snapshot: &MultiBufferSnapshot,
    ) {
        self.land_from_offsets(snapshot, |sel| {
            landings
                .iter()
                .find(|landing| landing.id == sel.id)
                .copied()
        });
    }

    /// Rebuild the collection from spans in offsets, one per selection.
    ///
    /// `landing_for` answers with where a selection lands, or [`None`] for one
    /// the caller is not moving, whose span is read out of its anchors instead.
    ///
    /// From there both landing paths run the same pipeline. Every endpoint is
    /// clipped to a grapheme boundary, the set is ordered by start, whatever
    /// overlaps is merged, and every anchor that moved is minted in one batched
    /// walk per bias.
    fn land_from_offsets<F>(&mut self, snapshot: &MultiBufferSnapshot, landing_for: F)
    where
        F: Fn(&Selection<Anchor>) -> Option<SpanLanding>,
    {
        assert!(
            !self.disjoint.is_empty(),
            "SelectionsCollection invariant: at least one selection"
        );
        let wanted: Vec<Option<SpanLanding>> = self.disjoint.iter().map(landing_for).collect();

        // Only the selections no landing names have to be read out of their
        // anchors. A landed one already knows where it is.
        let carried = {
            let anchors: Vec<Anchor> = self
                .disjoint
                .iter()
                .zip(&wanted)
                .filter(|(_, landing)| landing.is_none())
                .flat_map(|(sel, _)| [sel.start, sel.end])
                .collect();
            snapshot.resolve_anchors_batch(&anchors)
        };

        let mut carried = carried.chunks_exact(2);
        let mut entries: Vec<Landing> = self
            .disjoint
            .iter()
            .zip(wanted)
            .map(|(sel, landing)| match landing {
                Some(landing) => Landing {
                    start: landing.start,
                    end: landing.end,
                    id: landing.id,
                    reversed: landing.reversed,
                    goal: landing.goal,
                    start_clipped: false,
                    keep: None,
                },
                None => {
                    let span = carried.next().expect("one span per unnamed selection");
                    Landing {
                        start: span[0],
                        end: span[1],
                        id: sel.id,
                        reversed: sel.reversed,
                        goal: sel.goal,
                        start_clipped: false,
                        keep: Some((sel.start, sel.end)),
                    }
                },
            })
            .collect();

        let snapped = {
            let requests: Vec<(usize, Bias)> = entries
                .iter()
                .flat_map(|entry| [(entry.start, Bias::Left), (entry.end, Bias::Right)])
                .collect();
            snapshot.rope().clip_to_grapheme_boundaries_batch(&requests)
        };

        for (entry, snapped) in entries.iter_mut().zip(snapped.chunks_exact(2)) {
            let (start, end) = (snapped[0], snapped[1]);
            // A clipped endpoint is not where its anchor said, so it gets a new
            // one. An unclipped one keeps whatever it arrived with.
            entry.keep = entry
                .keep
                .filter(|_| start == entry.start && end == entry.end);
            entry.start_clipped = start != entry.start;
            entry.start = start;
            entry.end = end;
        }
        entries.sort_by_key(|e| (e.start, e.id));

        let mut deduped: Vec<Landing> = Vec::with_capacity(entries.len());
        for entry in entries {
            if let Some(prev) = deduped.last_mut()
                && (entry.start == prev.start || entry.start < prev.end)
            {
                let start = prev.start;
                let start_clipped = prev.start_clipped;
                let start_keep = prev.keep;
                let (end, end_keep) = match entry.end > prev.end {
                    true => (entry.end, entry.keep),
                    false => (prev.end, prev.keep),
                };

                // As in `replace_with`'s merge, the direction follows the inputs
                // rather than whichever is newest, so the same overlap lands the
                // cursor on the same end however the ids fall.
                let reversed = prev.reversed && entry.reversed;
                if entry.id > prev.id {
                    prev.id = entry.id;
                    prev.goal = entry.goal;
                }
                prev.reversed = reversed;

                prev.start = start;
                prev.start_clipped = start_clipped;
                prev.end = end;
                prev.keep = match (start_keep, end_keep) {
                    (Some((sa, _)), Some((_, ea))) => Some((sa, ea)),
                    _ => None,
                };
                continue;
            }
            deduped.push(entry);
        }

        // One walk for every endpoint that needs an anchor, split by the bias it
        // takes. The left-biased call is empty unless something was clipped.
        let right: Vec<usize> = deduped
            .iter()
            .filter(|e| e.keep.is_none())
            .flat_map(|e| match e.start_clipped {
                true => vec![e.end],
                false => vec![e.start, e.end],
            })
            .collect();
        let left: Vec<usize> = deduped
            .iter()
            .filter(|e| e.keep.is_none() && e.start_clipped)
            .map(|e| e.start)
            .collect();
        let mut right = snapshot.anchors_at_batch(&right, Bias::Right).into_iter();
        let mut left = snapshot.anchors_at_batch(&left, Bias::Left).into_iter();

        let landed = deduped
            .into_iter()
            .map(|entry| {
                let (start, end) = match entry.keep {
                    Some((start, end)) => (start, end),
                    None => {
                        let start = match entry.start_clipped {
                            true => left.next().expect("one per clipped start"),
                            false => right.next().expect("one per unclipped start"),
                        };
                        (start, right.next().expect("one per end"))
                    },
                };
                Selection {
                    id: entry.id,
                    start,
                    end,
                    reversed: entry.reversed,
                    goal: entry.goal,
                }
            })
            .collect();
        self.install(landed);
    }
}

/// Where a selection lands, worked out in offsets by whoever moved it.
///
/// See [`SelectionsCollection::replace_from_offsets`], which takes these, and
/// [`SelectionsCollection::land_block_cursors`], which builds them for the
/// narrower case of a 1-wide cursor.
#[derive(Clone, Copy)]
pub(crate) struct SpanLanding {
    pub(crate) id: usize,
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) reversed: bool,
    pub(crate) goal: SelectionGoal,
}

/// One selection read out of its anchors, as a motion needs it.
///
/// See [`SelectionsCollection::resolved_reads`], which produces these.
pub(crate) struct ResolvedRead {
    pub(crate) id: usize,
    pub(crate) head: usize,
    pub(crate) tail: usize,
    pub(crate) goal: SelectionGoal,
    pub(crate) reversed: bool,
}

/// What a block cursor landing on the end of the rope covers, there being no
/// next character to widen over.
///
/// The two answers belong to different callers rather than to different
/// cursors, so a batch picks one. A normal-mode motion covers a cell wherever
/// it lands, including past the last character. An insert cursor sits at its
/// insertion point and must not step back, which would put the point before the
/// character just typed and corrupt what follows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum EndCell {
    Previous,
    Empty,
}

/// A selection on its way through [`SelectionsCollection::land_block_cursors`],
/// held as offsets so the pipeline never resolves an anchor it just made.
///
/// `keep` carries the anchors of a selection that was not landed, which stand
/// unless an endpoint moved under the grapheme clip.
struct Landing {
    start: usize,
    end: usize,
    start_clipped: bool,
    id: usize,
    reversed: bool,
    goal: SelectionGoal,
    keep: Option<(Anchor, Anchor)>,
}

/// Collapse overlapping spans into the ranges their union covers, sorted by
/// start.
///
/// Editing selection by selection needs spans that do not overlap, or the
/// shared part is edited twice and the text after it is consumed a second
/// time. Spans that merely touch stay separate, `(0, 3)` and `(3, 6)` covering
/// disjoint text.
pub(crate) fn merge_overlapping_spans(mut spans: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    spans.sort_unstable();

    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(spans.len());
    for (start, end) in spans {
        match merged.last_mut() {
            Some(last) if start < last.1 => last.1 = last.1.max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

/// Build a forward 1-wide block cursor covering the character at `offset`,
/// widening backward at the rope end where there is no next character.
/// Where the highest-id selection sits in `selections`, or zero when there are
/// none.
///
/// Ties go to the last, which is what `max_by_key` answers and so what the
/// scan this replaces would have. Ids are minted from a counter that only
/// climbs, so a tie means one selection listed twice.
fn newest_index(selections: &[Selection<Anchor>]) -> usize {
    selections
        .iter()
        .enumerate()
        .max_by_key(|(_, selection)| selection.id)
        .map_or(0, |(index, _)| index)
}

/// `selections` with `selection` spliced in at `at`.
///
/// A shared list cannot be inserted into, so the whole list is rebuilt. The
/// callers already walked it to find `at`, so this adds a pass rather than a
/// complexity class.
fn insert_at(
    selections: &[Selection<Anchor>],
    at: usize,
    selection: Selection<Anchor>,
) -> Arc<[Selection<Anchor>]> {
    let mut rebuilt = Vec::with_capacity(selections.len() + 1);
    rebuilt.extend_from_slice(&selections[..at]);
    rebuilt.push(selection);
    rebuilt.extend_from_slice(&selections[at..]);
    rebuilt.into()
}

fn block_cursor_at(
    offset: usize,
    goal: SelectionGoal,
    id: usize,
    snapshot: &MultiBufferSnapshot,
) -> Selection<Anchor> {
    // A mouse click resolves through a display clip that walks codepoints, so
    // it can name an offset inside a cluster. Widening from there would jump to
    // the cluster's end and cover only its tail. Clamping down first is
    // `replace_with`'s start rule, and it is what a click means: the cell
    // belongs to the cluster it is drawn inside.
    let rope = snapshot.rope();
    let offset = rope.clip_to_grapheme_boundary(offset, Bias::Left);
    let widened = Selection {
        id,
        start: offset,
        end: offset,
        reversed: false,
        goal,
    }
    .min_width_1(rope);
    Selection {
        id,
        start: snapshot.anchor_at(widened.start, Bias::Right),
        end: snapshot.anchor_at(widened.end, Bias::Right),
        reversed: false,
        goal,
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
    use stoat_text::{next_char_boundary, Bias};

    /// The recorded primary is only right while every path that changes the set
    /// goes through the one place that records it. A mutator that assigned the
    /// list directly would leave it pointing at whatever used to be there, and
    /// the collection would go on answering with the wrong selection.
    ///
    /// Each case runs a mutator and asks who the primary is, against the scan
    /// the index replaced.
    #[test]
    fn every_mutator_leaves_the_primary_findable() {
        let multi = singleton("abcdefgh\nijklmnop\n");
        let snapshot = multi.snapshot();
        let at = |offset: usize| snapshot.anchor_at(offset, Bias::Right);

        /// A named mutation of a collection, for a table pinning what each
        /// one leaves behind.
        type NamedMutation = (
            &'static str,
            fn(&mut SelectionsCollection, &MultiBufferSnapshot),
        );

        let cases: [NamedMutation; 12] = [
            ("reanchor", |c, _| c.reanchor(|anchor| *anchor)),
            ("insert_cursor", |c, s| {
                c.insert_cursor(s.anchor_at(4, Bias::Right), SelectionGoal::None, s)
            }),
            ("extend_with_fresh_ids", |c, s| {
                c.extend_with_fresh_ids(
                    vec![Selection {
                        id: 0,
                        start: s.anchor_at(2, Bias::Right),
                        end: s.anchor_at(5, Bias::Right),
                        reversed: false,
                        goal: SelectionGoal::None,
                    }],
                    s,
                )
            }),
            ("seed_cursor", |c, s| c.seed_cursor(s)),
            ("set_single_range", |c, s| {
                c.set_single_range(
                    s.anchor_at(1, Bias::Right),
                    s.anchor_at(3, Bias::Right),
                    SelectionGoal::None,
                )
            }),
            ("set_block_cursor", |c, s| c.set_block_cursor(6, s)),
            ("restore", |c, s| {
                c.restore(Arc::from([Selection {
                    id: 41,
                    start: s.anchor_at(0, Bias::Right),
                    end: s.anchor_at(1, Bias::Right),
                    reversed: false,
                    goal: SelectionGoal::None,
                }]))
            }),
            ("keep_primary", |c, _| c.keep_primary()),
            ("remove_primary", |c, _| c.remove_primary()),
            ("rotate_primary_by", |c, _| c.rotate_primary_by(true, 1)),
            ("transform", |c, s| c.transform(s, |sel| sel.clone())),
            ("land_block_cursors", |c, s| {
                c.land_block_cursors(&[(0, 2, SelectionGoal::None)], EndCell::Previous, s)
            }),
        ];

        for (name, mutate) in cases {
            // Two selections to start, so a mutator that keeps or drops one has
            // something to choose between and the index can be wrong.
            let mut collection = SelectionsCollection::new();
            collection.insert_cursor(at(3), SelectionGoal::None, &snapshot);
            collection.insert_cursor(at(6), SelectionGoal::None, &snapshot);

            mutate(&mut collection, &snapshot);

            let scanned = collection
                .all_anchors()
                .iter()
                .max_by_key(|selection| selection.id)
                .expect("at least one selection");
            assert_eq!(
                collection.newest_anchor().id,
                scanned.id,
                "after {name} the recorded primary is not the highest id"
            );
        }
    }

    /// The undo group a keystroke opens keeps whatever this hands back, so it
    /// has to be the list itself. A copy would put the cursor count back into
    /// the cost of every action.
    #[test]
    fn sharing_the_selections_hands_back_the_same_list() {
        let collection = SelectionsCollection::new();

        assert!(
            Arc::ptr_eq(&collection.shared_anchors(), &collection.shared_anchors()),
            "two shares of one set name one list"
        );
    }

    /// A share outlives the set it came from, so a mutation has to build a new
    /// list rather than write through the one an undo group is holding.
    #[test]
    fn mutating_leaves_an_earlier_share_alone() {
        let multi = singleton("abcdefgh\n");
        let snapshot = multi.snapshot();

        let mut collection = SelectionsCollection::new();
        let held = collection.shared_anchors();
        collection.insert_cursor(
            snapshot.anchor_at(4, Bias::Right),
            SelectionGoal::None,
            &snapshot,
        );

        assert_eq!(held.len(), 1, "the held list still has what it had");
        assert!(
            !Arc::ptr_eq(&held, &collection.shared_anchors()),
            "and the collection moved on to a different one"
        );
    }

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

    /// A merged selection faces forward unless every input it merged faced
    /// backward.
    ///
    /// The direction decides which end the block cursor sits on, and taking it
    /// from whichever input happened to be minted later makes the merge depend
    /// on something that says nothing about direction. Swapping the ids must
    /// therefore leave the result alone, which the same assertion run both ways
    /// is what checks.
    #[test]
    fn merging_mixed_directions_faces_forward_whichever_is_newer() {
        let multi = singleton("abcdefgh\n");
        let snapshot = multi.snapshot();
        let span = |id: usize, start: usize, end: usize, reversed: bool| Selection {
            id,
            start: snapshot.anchor_at(start, Bias::Right),
            end: snapshot.anchor_at(end, Bias::Right),
            reversed,
            goal: SelectionGoal::None,
        };

        for (reversed_id, forward_id) in [(2, 1), (1, 2)] {
            let mut collection = SelectionsCollection::new();
            collection.replace_with(
                vec![span(reversed_id, 0, 6, true), span(forward_id, 3, 4, false)],
                &snapshot,
            );

            let merged = collection.all_anchors();
            assert_eq!(merged.len(), 1, "the two overlap, so they merge");
            assert_eq!(
                (
                    snapshot.resolve_anchor(&merged[0].start),
                    snapshot.resolve_anchor(&merged[0].end),
                    merged[0].reversed,
                ),
                (0, 6, false),
                "the union, facing forward, with the reversed id being {reversed_id}",
            );
        }
    }

    /// Landing cursors merges by the same direction rule.
    ///
    /// This path has its own merge loop, and a landing is always forward while
    /// a selection it does not name keeps the direction it had. So a landing
    /// inside a backward selection is the mixed case here, and the backward one
    /// carries the higher id, which is the arrangement the old rule got wrong.
    #[test]
    fn landing_a_cursor_inside_a_reversed_selection_faces_forward() {
        let multi = singleton("abcdefgh\n");
        let snapshot = multi.snapshot();

        let mut collection = SelectionsCollection::new();
        collection.restore(Arc::from([
            Selection {
                id: 1,
                start: snapshot.anchor_at(3, Bias::Right),
                end: snapshot.anchor_at(4, Bias::Right),
                reversed: false,
                goal: SelectionGoal::None,
            },
            Selection {
                id: 2,
                start: snapshot.anchor_at(0, Bias::Right),
                end: snapshot.anchor_at(6, Bias::Right),
                reversed: true,
                goal: SelectionGoal::None,
            },
        ]));

        collection.land_block_cursors(&[(1, 3, SelectionGoal::None)], EndCell::Previous, &snapshot);

        let merged = collection.all_anchors();
        assert_eq!(merged.len(), 1, "the landing sits inside the other span");
        assert!(
            !merged[0].reversed,
            "one input faced forward, so the merge does",
        );
    }

    /// One selection landed on `offset`, read back as `(start, end, goal)`.
    fn land_one(
        text: &str,
        offset: usize,
        goal: SelectionGoal,
        end_cell: EndCell,
    ) -> (usize, usize, SelectionGoal) {
        let multi = singleton(text);
        let snapshot = multi.snapshot();

        let mut collection = SelectionsCollection::new();
        collection.restore(Arc::from([Selection {
            id: 1,
            start: snapshot.anchor_at(0, Bias::Right),
            end: snapshot.anchor_at(1, Bias::Right),
            reversed: false,
            goal: SelectionGoal::None,
        }]));

        collection.land_block_cursors(&[(1, offset, goal)], end_cell, &snapshot);

        let landed = &collection.all_anchors()[0];
        (
            snapshot.resolve_anchor(&landed.start),
            snapshot.resolve_anchor(&landed.end),
            landed.goal,
        )
    }

    /// A cursor landing on the rope's end has no character after it, and what
    /// it covers is the caller's to say. A motion covers a cell wherever it
    /// lands. An insert cursor stays on its insertion point, since stepping back
    /// would put it before the character just typed.
    #[test]
    fn a_landing_on_the_rope_end_covers_what_the_caller_asked_for() {
        let text = "ab";
        let end = text.len();

        assert_eq!(
            land_one(text, end, SelectionGoal::None, EndCell::Previous),
            (1, 2, SelectionGoal::None),
            "the previous cell, so the cursor still covers a character",
        );
        assert_eq!(
            land_one(text, end, SelectionGoal::None, EndCell::Empty),
            (2, 2, SelectionGoal::None),
            "nothing, leaving the insertion point where it is",
        );
    }

    /// Vertical motion carries a goal column across rows, so a landing has to
    /// arrive with the one it was given rather than a cleared one.
    #[test]
    fn a_landing_keeps_the_goal_it_was_given() {
        let (start, end, goal) = land_one("abc", 1, SelectionGoal::Column(7), EndCell::Previous);
        assert_eq!((start, end), (1, 2), "a 1-wide cursor on the cell");
        assert_eq!(goal, SelectionGoal::Column(7));
    }

    /// Two backward selections merge into a backward one, so the rule is a
    /// property of the inputs rather than a blanket forward.
    #[test]
    fn merging_two_reversed_selections_stays_reversed() {
        let multi = singleton("abcdefgh\n");
        let snapshot = multi.snapshot();
        let span = |id: usize, start: usize, end: usize| Selection {
            id,
            start: snapshot.anchor_at(start, Bias::Right),
            end: snapshot.anchor_at(end, Bias::Right),
            reversed: true,
            goal: SelectionGoal::None,
        };

        let mut collection = SelectionsCollection::new();
        collection.replace_with(vec![span(1, 0, 6), span(2, 3, 8)], &snapshot);

        let merged = collection.all_anchors();
        assert_eq!(merged.len(), 1, "the two overlap, so they merge");
        assert!(merged[0].reversed, "both faced backward, so the merge does");
    }

    #[test]
    fn landing_from_offsets_matches_landing_through_anchors() {
        // The offsets path exists to skip minting anchors that replace_with
        // would resolve straight back, so it has to land on the same anchors,
        // not merely the same offsets. A combining mark and a regional-indicator
        // pair put clipping in the way, and cursors landing together exercise
        // the merge.
        let text: String = (0..40)
            .map(|i| match i % 3 {
                0 => format!("line {i} ascii only\n"),
                1 => format!("line {i} e\u{301}accented\n"),
                _ => format!("line {i} \u{1F1EC}\u{1F1E7} flag\n"),
            })
            .collect();
        let multi = singleton(&text);
        let snapshot = multi.snapshot();

        let seed = |collection: &mut SelectionsCollection| {
            for row in 0..40 {
                let at = text
                    .match_indices('\n')
                    .nth(row)
                    .map(|(i, _)| i)
                    .expect("the fixture has the rows");
                collection.insert_cursor(
                    snapshot.anchor_at(at.saturating_sub(row % 7), Bias::Right),
                    SelectionGoal::None,
                    &snapshot,
                );
            }
        };

        let mut through_offsets = SelectionsCollection::new();
        let mut through_anchors = SelectionsCollection::new();
        seed(&mut through_offsets);
        seed(&mut through_anchors);

        // Landings on and off grapheme boundaries, some of them colliding so
        // the dedupe runs.
        let mut landings: Vec<(usize, usize, SelectionGoal)> = through_offsets
            .all_anchors()
            .iter()
            .enumerate()
            .map(|(i, sel)| (sel.id, (i * 13) % text.len(), SelectionGoal::None))
            .collect();
        landings.sort_unstable_by_key(|(id, _, _)| *id);

        through_offsets.land_block_cursors(&landings, EndCell::Previous, &snapshot);

        let landed: Vec<Selection<Anchor>> = through_anchors
            .all_anchors()
            .iter()
            .map(|sel| {
                let found = landings
                    .binary_search_by_key(&sel.id, |(id, _, _)| *id)
                    .expect("every selection is named");
                let target = landings[found].1;
                let end = next_char_boundary(snapshot.rope(), target);
                Selection {
                    id: sel.id,
                    start: snapshot.anchor_at(target, Bias::Right),
                    end: snapshot.anchor_at(end, Bias::Right),
                    reversed: false,
                    goal: SelectionGoal::None,
                }
            })
            .collect();
        through_anchors.replace_with(landed, &snapshot);

        assert_eq!(
            through_offsets.all_anchors(),
            through_anchors.all_anchors(),
            "the two routes have to agree on the anchors, not just the offsets",
        );
    }

    #[test]
    fn replacing_from_offsets_matches_building_selections_one_at_a_time() {
        // The span path has the same job as the cursor one above, for a caller
        // landing a range rather than a cell. Spans of differing width, some
        // backward, some overlapping so the merge runs, and endpoints inside a
        // combining mark and a regional-indicator pair so the clip does.
        let text: String = (0..40)
            .map(|i| match i % 3 {
                0 => format!("line {i} ascii only\n"),
                1 => format!("line {i} e\u{301}accented\n"),
                _ => format!("line {i} \u{1F1EC}\u{1F1E7} flag\n"),
            })
            .collect();
        let multi = singleton(&text);
        let snapshot = multi.snapshot();

        let seed = |collection: &mut SelectionsCollection| {
            for row in 0..40 {
                let at = text
                    .match_indices('\n')
                    .nth(row)
                    .map(|(i, _)| i)
                    .expect("the fixture has the rows");
                collection.insert_cursor(
                    snapshot.anchor_at(at.saturating_sub(row % 7), Bias::Right),
                    SelectionGoal::None,
                    &snapshot,
                );
            }
        };

        let mut through_offsets = SelectionsCollection::new();
        let mut through_anchors = SelectionsCollection::new();
        seed(&mut through_offsets);
        seed(&mut through_anchors);

        let landings: Vec<SpanLanding> = through_offsets
            .all_anchors()
            .iter()
            .enumerate()
            .map(|(i, sel)| {
                let start = (i * 13) % text.len();
                SpanLanding {
                    id: sel.id,
                    start,
                    end: (start + i % 5).min(text.len()),
                    reversed: i % 2 == 0,
                    goal: match i % 4 {
                        0 => SelectionGoal::Column(i as u32),
                        _ => SelectionGoal::None,
                    },
                }
            })
            .collect();

        through_offsets.replace_from_offsets(&landings, &snapshot);

        let landed: Vec<Selection<Anchor>> = through_anchors
            .all_anchors()
            .iter()
            .map(|sel| {
                let landing = landings
                    .iter()
                    .find(|landing| landing.id == sel.id)
                    .expect("every selection is named");
                let rope = snapshot.rope();
                let start = rope.clip_to_grapheme_boundary(landing.start, Bias::Left);
                let end = rope.clip_to_grapheme_boundary(landing.end, Bias::Right);
                Selection {
                    id: sel.id,
                    // A start the clip moved is not where it was asked for, and
                    // takes the bias it was clipped toward. One that did not move
                    // is anchored forward like every other endpoint.
                    start: match start == landing.start {
                        true => snapshot.anchor_at(start, Bias::Right),
                        false => snapshot.anchor_at(start, Bias::Left),
                    },
                    end: snapshot.anchor_at(end, Bias::Right),
                    reversed: landing.reversed,
                    goal: landing.goal,
                }
            })
            .collect();
        through_anchors.replace_with(landed, &snapshot);

        assert_eq!(
            through_offsets.all_anchors(),
            through_anchors.all_anchors(),
            "the two routes have to agree on the anchors, not just the offsets",
        );
    }

    #[test]
    fn transform_resolved_hands_over_what_each_selection_would_resolve() {
        let text: String = (0..200).map(|i| format!("line {i} of text\n")).collect();
        let multi = singleton(&text);
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        // Many cursors, some reversed, so head and tail are not interchangeable
        // and the batch has to keep them the right way round.
        for row in 0..200 {
            let at = text
                .match_indices('\n')
                .nth(row)
                .map(|(i, _)| i)
                .expect("the fixture has the rows");
            collection.insert_cursor(
                snapshot.anchor_at(at.saturating_sub(row % 5), Bias::Right),
                SelectionGoal::None,
                &snapshot,
            );
        }
        collection.transform(&snapshot, |sel| {
            let mut flipped = sel.clone();
            flipped.reversed = sel.id % 2 == 0;
            flipped
        });

        let expected: Vec<(usize, usize)> = collection
            .all_anchors()
            .iter()
            .map(|sel| {
                (
                    snapshot.resolve_anchor(&sel.head()),
                    snapshot.resolve_anchor(&sel.tail()),
                )
            })
            .collect();
        assert!(expected.len() > 100, "the fixture has to have many cursors");

        let mut handed = Vec::new();
        collection.transform_resolved(&snapshot, |sel, head, tail| {
            handed.push((head, tail));
            sel.clone()
        });

        assert_eq!(
            handed, expected,
            "the batch gives each closure the offsets it would have resolved itself"
        );
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

        // insert_cursor now seeds 1-wide forward cursors, so the two inserted
        // heads sit one cell past their seed (offsets 3 and 5). The untouched
        // zero-width default at 0 advances to 1.
        let offsets: Vec<usize> = collection
            .all_anchors()
            .iter()
            .map(|s| snapshot.resolve_anchor(&s.start))
            .collect();
        assert_eq!(offsets, vec![1, 4, 6]);
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

    /// The collection is ordered by where each selection starts, and the merge
    /// depends on it, comparing each entry only against the one behind it.
    ///
    /// A selection enclosing another starts first but ends last, so the two
    /// endpoints disagree about the order. Ordered by end, the enclosing
    /// selection arrives second and the union is taken from the wrong start.
    #[test]
    fn replace_with_orders_by_start_not_end() {
        let multi = singleton("abcdefghij");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        let span = |id: usize, start: usize, end: usize| Selection {
            id,
            start: snapshot.anchor_at(start, Bias::Right),
            end: snapshot.anchor_at(end, Bias::Left),
            reversed: false,
            goal: SelectionGoal::None,
        };
        // Handed over end-first, so an unsorted result would keep that order.
        collection.replace_with(vec![span(1, 2, 4), span(2, 0, 8)], &snapshot);

        let spans: Vec<(usize, usize)> = collection
            .all_anchors()
            .iter()
            .map(|s| {
                (
                    snapshot.resolve_anchor(&s.start),
                    snapshot.resolve_anchor(&s.end),
                )
            })
            .collect();
        assert_eq!(
            spans,
            vec![(0, 8)],
            "the union runs from the enclosing selection's start",
        );
    }

    #[test]
    fn touching_selections_stay_apart() {
        let multi = singleton("abcdefghij");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        let span = |id: usize, start: usize, end: usize| Selection {
            id,
            start: snapshot.anchor_at(start, Bias::Right),
            end: snapshot.anchor_at(end, Bias::Left),
            reversed: false,
            goal: SelectionGoal::None,
        };
        collection.replace_with(vec![span(1, 0, 3), span(2, 3, 6)], &snapshot);

        let spans: Vec<(usize, usize)> = collection
            .all_anchors()
            .iter()
            .map(|s| {
                (
                    snapshot.resolve_anchor(&s.start),
                    snapshot.resolve_anchor(&s.end),
                )
            })
            .collect();
        assert_eq!(
            spans,
            vec![(0, 3), (3, 6)],
            "abutting selections cover disjoint text and are not one selection",
        );
    }

    #[test]
    fn merge_overlapping_spans_takes_the_union() {
        assert_eq!(
            merge_overlapping_spans(vec![(3, 7), (0, 4)]),
            vec![(0, 7)],
            "overlapping"
        );
        assert_eq!(
            merge_overlapping_spans(vec![(0, 3), (3, 6)]),
            vec![(0, 3), (3, 6)],
            "touching"
        );
        assert_eq!(
            merge_overlapping_spans(vec![(0, 9), (2, 4)]),
            vec![(0, 9)],
            "enclosed, so the union keeps the wider end"
        );
        assert_eq!(
            merge_overlapping_spans(vec![(0, 3), (1, 4), (2, 9)]),
            vec![(0, 9)],
            "a chain merges through"
        );
        assert_eq!(merge_overlapping_spans(vec![]), vec![], "empty");
    }

    /// The delete path hands this output straight to `edit_batch`, reversed,
    /// which takes disjoint ranges sorted descending and refuses an empty range
    /// sharing a start with a deleting one.
    ///
    /// Reversing gives descending only if the output ascends, disjointness is
    /// the batch's own requirement, and non-empty in giving non-empty out is
    /// what keeps every range in that batch a deleting one, so the shape the
    /// batch refuses cannot arise. All three are `debug_assert`s over there, so
    /// a release build leans on this end holding them.
    #[test]
    fn merged_spans_are_the_shape_a_delete_batch_needs() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for case in 0..512 {
            let spans: Vec<(usize, usize)> = (0..1 + (next() % 6))
                .map(|_| {
                    let start = (next() % 20) as usize;
                    // Non-empty, which is what the delete path filters for
                    // before it merges.
                    (start, start + 1 + (next() % 5) as usize)
                })
                .collect();

            let merged = merge_overlapping_spans(spans.clone());

            for window in merged.windows(2) {
                assert!(
                    window[0].1 <= window[1].0,
                    "case {case}: {merged:?} overlaps, from {spans:?}",
                );
                assert!(
                    window[0].0 < window[1].0,
                    "case {case}: {merged:?} does not ascend, from {spans:?}",
                );
            }
            for span in &merged {
                assert!(
                    span.0 < span.1,
                    "case {case}: {merged:?} holds an empty span, from {spans:?}",
                );
            }
        }
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

        // 1-wide insert_cursor puts the two heads at 3 and 8. The swap maps
        // them to 6 and 1, and the zero-width default stays at 0.
        let offsets: Vec<usize> = collection
            .all_anchors()
            .iter()
            .map(|s| snapshot.resolve_anchor(&s.start))
            .collect();
        assert_eq!(offsets, vec![0, 1, 6]);
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

        collection.split_each(&snapshot, Bias::Right, |_| Vec::new());

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

        collection.split_each(&snapshot, Bias::Right, |_| vec![(0, 3), (5, 8)]);

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
    fn add_selection_below_copies_selection_shape() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "foobar\nfoobar\n");
        h.open_file(&path);
        // Select `foo`, return to normal mode, then copy it downward.
        h.type_keys("v l l v");
        h.type_keys("shift-C");
        assert_eq!(h.selection_spans(), vec![(0, 3, false), (7, 10, false)]);
    }

    #[test]
    fn add_selection_below_skips_too_short_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "foobar\nx\nfoobar\n");
        h.open_file(&path);
        h.type_keys("v l l v");
        h.type_keys("shift-C");
        assert_eq!(h.selection_spans(), vec![(0, 3, false), (9, 12, false)]);
    }

    /// A copied cursor lands in the column a vertical motion would reach.
    ///
    /// Both answer "the same place, one line down", so they have to agree about
    /// what a column is. A tab is one byte and several cells, so a copy working
    /// in bytes lands near the start of the line below where the motion lands
    /// past the indent, and the two only diverge on lines that hold one.
    #[test]
    fn add_selection_below_lands_where_moving_down_lands() {
        let text = "\tfoo\nabcdefgh\n";

        let moved = {
            let mut h = crate::test_harness::TestHarness::with_size(20, 5);
            let path = h.write_file("s.txt", text);
            h.open_file(&path);
            h.type_keys("l j");
            h.selection_spans()
        };

        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", text);
        h.open_file(&path);
        h.type_keys("l");
        h.type_keys("shift-C");
        let copied = h.selection_spans();

        assert_eq!(copied.len(), 2, "the source and its copy, got {copied:?}",);
        assert_eq!(
            copied[1], moved[0],
            "the copy sits where moving down from the same cursor sits",
        );
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

    /// An added span that splits a cluster snaps out to cover it.
    ///
    /// Copying a selection onto another row works by column, and that row can
    /// hold different text, so a column that was a boundary on the source lands
    /// inside a cluster here. The production caller resolves its offsets through
    /// the display layer, which already snaps, so this exercises the contract
    /// directly rather than through a path that cannot violate it.
    #[test]
    fn an_added_span_splitting_a_cluster_snaps_out_to_cover_it() {
        let multi = singleton("ae\u{301}b\n");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        // Offset 2 is between the e and its acute. The cluster spans 1..4.
        let split = Selection {
            id: 0,
            start: snapshot.anchor_at(2, Bias::Right),
            end: snapshot.anchor_at(2, Bias::Right),
            reversed: false,
            goal: SelectionGoal::None,
        };
        collection.extend_with_fresh_ids(vec![split], &snapshot);

        let stored = collection
            .all_anchors()
            .iter()
            .map(|sel| {
                (
                    snapshot.resolve_anchor(&sel.start),
                    snapshot.resolve_anchor(&sel.end),
                )
            })
            .find(|&(start, _)| start != 0)
            .expect("the inserted range is stored alongside the default cursor");

        assert_eq!(
            stored,
            (1, 4),
            "the span grows out to the cluster it was splitting",
        );
    }

    /// A copied cursor's start holds its ground when text arrives on it.
    ///
    /// Which side of an insertion an endpoint lands on is the anchor's bias, and
    /// nothing about the offsets says which was used. Minting the copies' starts
    /// the other way would let text typed at a copy's own offset push it along,
    /// so the cursor would slide off the column it was copied to.
    #[test]
    fn a_copied_cursors_start_stays_before_text_inserted_at_it() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abcdef\nabcdef\n");
        h.open_file(&path);
        h.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AddSelectionBelow);
        assert_eq!(
            h.selection_spans(),
            vec![(3, 4, false), (10, 11, false)],
            "one cursor per row at column three",
        );

        // Onto the copy's own start, which is the offset whose bias decides
        // whether the cursor is pushed along or stays where it was put.
        {
            let ws = h.stoat.active_workspace();
            let editor_id = match ws.panes.pane(ws.panes.focus()).view {
                crate::pane::View::Editor(id) => id,
                _ => panic!("focused pane is not an editor"),
            };
            let buffer_id = ws.editors[editor_id].buffer_id;
            let buffer = ws.buffers.get(buffer_id).expect("buffer");
            buffer.write().expect("poisoned").edit(10..10, "XY");
        }

        assert_eq!(
            h.selection_spans(),
            vec![(3, 4, false), (10, 13, false)],
            "the copy's start stayed put and the insertion landed inside it",
        );
    }

    /// `set_block_cursor` snaps an offset landing inside a cluster.
    ///
    /// This is the mouse click's landing point, and the display clip chain
    /// ahead of it walks codepoints, so a click on an interior cell of a joined
    /// sequence arrives mid-cluster. Widening from there jumps to the cluster's
    /// end instead, leaving a span that covers only its tail, and deleting that
    /// would strand the codepoints before it.
    #[test]
    fn set_block_cursor_snaps_an_offset_inside_a_cluster() {
        // A man, woman, girl sequence joined by zero-width joiners: five
        // codepoints over eighteen bytes, whose only boundaries are 0 and 18.
        let multi = singleton("\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}b\n");
        let snapshot = multi.snapshot();
        let mut collection = SelectionsCollection::new();

        // Byte 7 is the woman codepoint, four bytes into the sequence.
        collection.set_block_cursor(7, &snapshot);

        let stored: Vec<(usize, usize)> = collection
            .all_anchors()
            .iter()
            .map(|sel| {
                (
                    snapshot.resolve_anchor(&sel.start),
                    snapshot.resolve_anchor(&sel.end),
                )
            })
            .collect();
        assert_eq!(
            stored,
            vec![(0, 18)],
            "the cursor covers the whole sequence the cell was drawn inside",
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
    fn snapshot_select_mode_forward_char_cursor() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v l l");
        h.assert_snapshot("select_mode_forward_char_cursor");
    }

    #[test]
    fn snapshot_select_mode_find_cursor() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v f e");
        h.assert_snapshot("select_mode_find_cursor");
    }

    #[test]
    fn snapshot_select_mode_vertical_cursor() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\nghijkl\n");
        h.open_file(&path);
        h.type_keys("l l v j");
        h.assert_snapshot("select_mode_vertical_cursor");
    }

    #[test]
    fn snapshot_select_mode_goto_first_nonws_cursor() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "  abc\n");
        h.open_file(&path);
        h.type_keys("v g i");
        h.assert_snapshot("select_mode_goto_first_nonws_cursor");
    }

    #[test]
    fn snapshot_select_mode_goto_window_cursor() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\ndef\nghi");
        h.open_file(&path);
        h.type_keys("v g b");
        h.assert_snapshot("select_mode_goto_window_cursor");
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
    fn goto_line_end_lands_on_last_visible_char() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("end");
        // The block cursor rests on the last visible char, not the newline.
        assert_eq!(h.selection_spans()[0], (2, 3, false));
    }

    #[test]
    fn goto_line_end_on_empty_line_stays_at_column_zero() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "x\n\ny\n");
        h.open_file(&path);
        h.type_keys("j");
        h.type_keys("end");
        // An empty line has no visible char, so it stays at the line start.
        assert_eq!(h.head_offsets(), vec![2]);
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
    fn goto_h_jumps_to_line_start() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "    abc\n");
        h.open_file(&path);
        h.type_keys("l l l l l l");
        h.type_keys("g h");
        assert_eq!(h.primary_head_offset(), 0);
    }

    #[test]
    fn goto_l_jumps_to_line_end() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc def\n");
        h.open_file(&path);
        h.type_keys("g l");
        // `gl` lands on the last visible char 'f', not the newline past it.
        assert_eq!(h.primary_head_offset(), 6);
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
    fn snapshot_select_line_cursor_on_line_end() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("x");
        h.assert_snapshot("snapshot_select_line_cursor_on_line_end");
    }

    #[test]
    fn snapshot_select_line_last_line_no_trailing_newline() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc");
        h.open_file(&path);
        h.type_keys("x");
        h.assert_snapshot("snapshot_select_line_last_line_no_trailing_newline");
    }

    #[test]
    fn snapshot_select_line_on_blank_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "abc\n\nxyz\n");
        h.open_file(&path);
        h.type_keys("j x");
        h.assert_snapshot("snapshot_select_line_on_blank_line");
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
        h.type_keys("3 z j");
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
        h.type_keys("5 plus");
        assert_eq!(focused_buffer_text(&mut h), "let x = 15\n");
    }

    #[test]
    fn count_prefix_decrement_subtracts_count() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 10\n");
        h.open_file(&path);
        h.type_keys("3 minus");
        assert_eq!(focused_buffer_text(&mut h), "let x = 7\n");
    }

    #[test]
    fn count_prefix_increment_hex_uses_count() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "let x = 0x10\n");
        h.open_file(&path);
        h.type_keys("4 plus");
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
        assert_eq!(h.stoat.focused_mode(), "select");
        h.type_keys("h h");
        let after = h.selection_spans();
        assert_ne!(after, before, "selection should have extended");
        // Extending left past the 1-wide anchor flips the range and steps the
        // tail one cell forward (Helix shrink-then-flip), so `d`'s cell stays
        // covered. `bcd` is selected (1..4) with the cursor on `b`.
        assert_eq!(after[0].0, 1, "tail of extended selection at byte 1");
        assert_eq!(
            after[0].1, 4,
            "end covers `d`, cursor (reversed head) on `b`"
        );
    }

    #[test]
    fn count_prefix_in_select_mode_extends_n_lines() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\n");
        h.open_file(&path);
        h.type_keys("v");
        assert_eq!(h.stoat.focused_mode(), "select");
        h.type_keys("3 j");
        let spans = h.selection_spans();
        assert_eq!(spans.len(), 1);
        assert_eq!(
            spans[0].0, 0,
            "anchor stays at byte 0 while head extends down"
        );
        assert_eq!(
            spans[0].1, 7,
            "3 j in select mode should extend the head three lines down"
        );
    }

    #[test]
    fn select_mode_v_exits_back_to_normal() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("v");
        assert_eq!(h.stoat.focused_mode(), "select");
        h.type_keys("v");
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn select_mode_escape_exits_back_to_normal() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("v");
        assert_eq!(h.stoat.focused_mode(), "select");
        h.type_keys("Escape");
        assert_eq!(h.stoat.focused_mode(), "normal");
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
    fn select_mode_semicolon_collapses_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v l l l");
        let before = h.selection_spans()[0];
        assert!(before.1 > before.0, "selection should be non-empty");
        h.type_keys(";");
        let after = h.selection_spans()[0];
        assert_eq!(
            (after.0, after.1),
            (3, 4),
            "; collapses to a 1-wide cursor on the last selected cell"
        );
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
        assert_eq!(focused_buffer_text(&mut h), "ef\n");
    }

    #[test]
    fn repeated_delete_walks_forward_through_line() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("d d");
        assert_eq!(focused_buffer_text(&mut h), "cdef\n");
    }

    #[test]
    fn delete_at_line_end_rewidens_backward() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc");
        h.open_file(&path);
        h.type_keys("l l d d");
        assert_eq!(focused_buffer_text(&mut h), "a");
    }

    #[test]
    fn multi_cursor_delete_rewidens_every_cursor() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\ndef\n");
        h.open_file(&path);
        h.type_keys("shift-C");
        h.type_keys("d");
        assert_eq!(focused_buffer_text(&mut h), "bc\nef\n");
        assert_eq!(h.selection_spans(), vec![(0, 1, false), (3, 4, false)]);
    }

    #[test]
    fn select_mode_tilde_switches_case() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v l l l");
        h.type_keys("~");
        assert_eq!(focused_buffer_text(&mut h), "ABCDef\n");
    }

    #[test]
    fn select_mode_undo_reverts_prior_edit() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v l l l");
        h.type_keys("d");
        assert_eq!(focused_buffer_text(&mut h), "ef\n");
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
        assert_eq!(
            h.stoat.focused_mode(),
            "select",
            "Alt-o stays in select mode"
        );
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
        assert_eq!(
            h.stoat.focused_mode(),
            "select",
            "Alt-i stays in select mode"
        );
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
        ];
        let badges = std::collections::BTreeMap::new();
        for (mode, expected) in cases {
            let (label, _) = crate::render::pane::mode_segment(mode, &theme, &badges);
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
        assert_eq!((start, end, reversed), (0, 5, false));
    }

    #[test]
    fn select_mode_capital_f_extends_backward() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("4 l v");
        h.type_keys("F b");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!(
            (start, end, reversed),
            (1, 5, true),
            "crossing the tail keeps the e the cursor was on covered",
        );
    }

    /// An extend that lands exactly on its own tail still covers a cell.
    ///
    /// Holding the tail and moving the head onto it would make the two
    /// endpoints equal, and nothing downstream widens an empty selection, so
    /// the block cursor would have no cell to paint at all.
    #[test]
    fn select_mode_extend_back_onto_the_tail_keeps_a_cell() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "ab cd ef\n");
        h.open_file(&path);
        h.type_keys("3 l v l l");
        assert_eq!(
            h.selection_spans()[0],
            (3, 6, false),
            "the tail sits at the c the backward word motion targets",
        );

        h.type_keys("b");
        assert_eq!(
            h.selection_spans()[0],
            (3, 4, false),
            "landing on the tail covers its cell rather than collapsing onto it",
        );
    }

    /// The same at a line start, where the target is the tail itself.
    #[test]
    fn select_mode_extend_to_line_start_at_column_zero_keeps_a_cell() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "foo bar\n");
        h.open_file(&path);
        h.type_keys("v");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendToLineStart);
        assert_eq!(
            h.selection_spans()[0],
            (0, 1, false),
            "already at column 0, so the cursor keeps its cell",
        );
    }

    /// A find is a horizontal move, so it clears the column a prior vertical
    /// move was holding.
    ///
    /// Carrying that column past the find makes the next vertical move return
    /// to where the cursor was before it, ignoring the column the find landed
    /// on. Here `j` holds column 0, `f x` moves to column 1, and the second `j`
    /// has to follow the find rather than snap back to column 0.
    #[test]
    fn select_mode_find_clears_the_vertical_goal_column() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "aaaa\nbxaa\ncccc\n");
        h.open_file(&path);
        h.type_keys("v j");
        h.type_keys("f x");
        assert_eq!(
            h.selection_spans()[0],
            (0, 7, false),
            "the find lands on the x at row 1 column 1",
        );

        h.type_keys("j");
        assert_eq!(
            h.selection_spans()[0],
            (0, 12, false),
            "the next row is entered at column 1, the column the find landed on",
        );
    }

    #[test]
    fn select_mode_t_extends_till_next_char() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("v");
        h.type_keys("t e");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!((start, end, reversed), (0, 4, false));
    }

    #[test]
    fn select_mode_capital_t_extends_till_prev_char() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("4 l v");
        h.type_keys("T b");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!(
            (start, end, reversed),
            (2, 5, true),
            "crossing the tail keeps the e the cursor was on covered",
        );
    }

    #[test]
    fn normal_mode_f_selects_to_target() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdef\n");
        h.open_file(&path);
        h.type_keys("f e");
        let (start, end, _) = h.selection_spans()[0];
        assert_eq!(
            (start, end),
            (0, 5),
            "normal-mode find selects from the cursor to the target inclusive"
        );
    }

    #[test]
    fn dfx_deletes_from_cursor_through_target() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "hello world\n");
        h.open_file(&path);
        h.type_keys("f o");
        h.type_keys("d");
        assert_eq!(focused_buffer_text(&mut h), " world\n");
    }

    #[test]
    fn dt_deletes_up_to_but_not_the_target() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "hello world\n");
        h.open_file(&path);
        h.type_keys("t o");
        h.type_keys("d");
        assert_eq!(focused_buffer_text(&mut h), "o world\n");
    }

    #[test]
    fn till_skips_an_adjacent_target() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "axbxc\n");
        h.open_file(&path);
        h.type_keys("t x");
        // The adjacent 'x' at 1 is skipped; till the second 'x' (at 3) selects
        // through 'b', ending before that 'x'.
        assert_eq!(h.selection_spans()[0], (0, 3, false));
    }

    #[test]
    fn find_lands_on_the_adjacent_target() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "axbxc\n");
        h.open_file(&path);
        h.type_keys("f x");
        // `f` is never skipped, so it lands on the first, adjacent 'x'.
        assert_eq!(h.selection_spans()[0], (0, 2, false));
    }

    /// A buffer whose second statement spans two rows.
    ///
    /// Both structural motions resolve their target from the node covering the
    /// whole selection, so a vertical step has to leave the selection inside one
    /// node that still has a parent or a sibling. Spanning two top-level items
    /// instead resolves to the root, which has neither, and the motion does
    /// nothing at all. A statement broken across two lines is the shape that
    /// keeps them moving.
    fn two_row_statement_source() -> &'static str {
        "fn m() {\n    let aaa =\n        1;\n    let bbbbbbbbbbbbbbbb = 2;\n    let ccccccccccccccccccccc = 3;\n}\n"
    }

    /// Moving to the next sibling is horizontal, so it drops the column a prior
    /// vertical move was holding.
    ///
    /// Here `j` holds column 8, the sibling extend moves the head to column 28,
    /// and the second `j` has to follow the sibling rather than snap back.
    #[test]
    fn select_mode_next_sibling_clears_the_vertical_goal_column() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 8);
        let path = h.write_file("s.rs", two_row_statement_source());
        h.open_file(&path);
        h.type_keys("j 8 l v j");
        assert_eq!(
            h.selection_spans()[0],
            (17, 32, false),
            "the head sits on row 2 column 8",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendSelectNextSibling);
        assert_eq!(
            h.selection_spans()[0],
            (17, 63, false),
            "the sibling extend moves the head to row 3 column 28",
        );

        h.type_keys("j");
        assert_eq!(
            h.selection_spans()[0],
            (17, 93, false),
            "the next row is entered at column 28, where the sibling extend left the head",
        );
    }

    /// The same for the move to a parent node's start, which goes backward.
    #[test]
    fn select_mode_parent_node_start_clears_the_vertical_goal_column() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 8);
        let path = h.write_file("s.rs", two_row_statement_source());
        h.open_file(&path);
        h.type_keys("j 8 l v j");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExtendMoveParentNodeStart);
        assert_eq!(
            h.selection_spans()[0],
            (7, 18, true),
            "the parent extend moves the head back to the block's brace on row 0",
        );

        h.type_keys("j");
        assert_eq!(
            h.selection_spans()[0],
            (16, 18, true),
            "the next row is entered at column 7, the brace's column, not the held column 8",
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
        assert_eq!(
            end,
            start + 1,
            "normal-mode parent jump collapses to a 1-wide cursor"
        );
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
            (0, 5, false),
            "head one past column 5 while tail stays at 0, cursor on offset 4"
        );
        assert_eq!(
            h.stoat.focused_mode(),
            "select",
            "back to select after the chord"
        );
    }

    #[test]
    fn normal_mode_g_pipe_still_collapses() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abcdefgh\n");
        h.open_file(&path);
        h.type_keys("5 g |");
        assert_eq!(h.cursor_display_positions(), vec![(0, 4)]);
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn select_mode_g_i_extends_to_first_nonwhitespace() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "    hello\n");
        h.open_file(&path);
        h.type_keys("8 l v");
        h.type_keys("g i");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!(
            (start, end, reversed),
            (4, 9, true),
            "crossing the tail keeps the o the cursor was on covered",
        );
        assert_eq!(h.stoat.focused_mode(), "select");
    }

    #[test]
    fn select_mode_g_j_extends_to_last_line() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\n");
        h.open_file(&path);
        h.type_keys("v");
        h.type_keys("g j");
        assert_eq!(
            h.primary_head_offset(),
            6,
            "block cursor lands on the last content line's char (`d`)"
        );
        assert_eq!(h.stoat.focused_mode(), "select");
    }

    #[test]
    fn select_mode_g_k_extends_to_file_start() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 10);
        let path = h.write_file("s.txt", "a\nb\nc\nd\n");
        h.open_file(&path);
        h.type_keys("j j j v");
        h.type_keys("g k");
        let (start, end, reversed) = h.selection_spans()[0];
        assert_eq!(
            (start, end, reversed),
            (0, 7, true),
            "crossing the tail keeps the cell the cursor was on covered"
        );
        assert_eq!(h.stoat.focused_mode(), "select");
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
            (0, 5, true),
            "head extended to row 0, and crossing the tail keeps the c on row 2 covered"
        );
        assert_eq!(h.stoat.focused_mode(), "select");
    }

    #[test]
    fn normal_mode_g_i_still_collapses() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "    hello\n");
        h.open_file(&path);
        h.type_keys("8 l");
        h.type_keys("g i");
        let (start, end, _) = h.selection_spans()[0];
        assert_eq!((start, end), (4, 5));
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn select_goto_escape_returns_to_select() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        h.type_keys("v g");
        assert_eq!(h.stoat.focused_mode(), "select_goto");
        h.type_keys("Escape");
        assert_eq!(h.stoat.focused_mode(), "select");
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
    fn switch_case_on_bare_cursor_toggles_char() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        assert_eq!(focused_buffer_text(&mut h), "Abc\n");
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
    fn switch_case_applies_to_each_split_cursor_range() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\ndef\nghi\n");
        h.open_file(&path);
        h.type_keys("% alt-s");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SwitchCase);
        assert_eq!(focused_buffer_text(&mut h), "ABC\nDEF\nGHI\n");
    }

    #[test]
    fn increment_applies_to_each_split_cursor_range() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "1\n2\n3\n");
        h.open_file(&path);
        h.type_keys("2 shift-C");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Increment);
        assert_eq!(focused_buffer_text(&mut h), "2\n3\n4\n");
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
    fn delete_selection_on_bare_cursor_deletes_char() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "hello\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
        assert_eq!(focused_buffer_text(&mut h), "ello\n");
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

    /// Every prefix lands at the shallowest row's indent, so the block's own
    /// indentation is preserved inside the comment rather than flattened.
    ///
    /// Removal still works off each row's own prefix, which is what lets the
    /// second toggle restore the original exactly.
    #[test]
    fn toggle_comments_rust_multi_line_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn main() {\n    let x = 42;\n}\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "// fn main() {\n//     let x = 42;\n// }\n",
            "prefix added at the shared column, indentation kept after it"
        );

        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "fn main() {\n    let x = 42;\n}\n",
            "the round trip restores the original indentation"
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

    /// A mixed block commits to being commented, then uncomments as a whole.
    ///
    /// Deciding per row would invert each one instead, leaving the block mixed
    /// after every toggle and swapping its two halves forever. One uncommented
    /// row is what makes the whole set count as uncommented.
    #[test]
    fn toggle_comments_rust_mixed_block_comments_all_then_uncomments_all() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "// abc\nxyz\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "// // abc\n// xyz\n",
            "the one uncommented row commits the set to being commented"
        );

        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "// abc\nxyz\n",
            "now that every row is commented the set uncomments"
        );
    }

    /// A blank row takes no edit in either direction, and it must not drag the
    /// shared column left either.
    #[test]
    fn toggle_comments_rust_uncomment_skips_whitespace_only_lines() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "// abc\n   \n// xyz\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "abc\n   \nxyz\n",
            "the blank line neither blocks the uncomment nor gets edited"
        );
    }

    /// Two selections reaching the same row comment it once.
    ///
    /// The rows are deduped before any edit is built, so a row two selections
    /// both reach costs one edit rather than one per selection. Without that,
    /// the second edit would land on the offset the first already used and
    /// stack a second prefix there.
    ///
    /// The selections are set directly rather than driven by keys. Two that
    /// share a row have to be disjoint in offsets to survive, since any pair
    /// that overlaps is merged into one, which is a single selection again and
    /// tests nothing.
    #[test]
    fn toggle_comments_rust_two_selections_reaching_a_row_comment_it_once() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "aaa\nbbb\nccc\n");
        h.open_file(&path);

        {
            let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            let snapshot = editor.display_map.snapshot();
            let buf_snap = snapshot.buffer_snapshot();
            editor.selections.set_single_range(
                buf_snap.anchor_at(0, Bias::Left),
                buf_snap.anchor_at(5, Bias::Right),
                SelectionGoal::None,
            );
            editor.selections.extend_with_fresh_ids(
                vec![Selection {
                    id: 0,
                    start: buf_snap.anchor_at(5, Bias::Left),
                    end: buf_snap.anchor_at(7, Bias::Right),
                    reversed: false,
                    goal: SelectionGoal::None,
                }],
                buf_snap,
            );
        }
        assert_eq!(
            h.selection_spans(),
            vec![(0, 5, false), (5, 7, false)],
            "row 1 is reached by both, row 2 by neither",
        );

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "// aaa\n// bbb\nccc\n",
            "the shared row takes one prefix, not one per selection"
        );
    }

    #[test]
    fn toggle_comments_rust_removes_prefix_without_trailing_space() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "//abc\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ToggleComments);
        assert_eq!(
            focused_buffer_text(&mut h),
            "abc\n",
            "no-space branch must strip only the prefix, not eat `a`"
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
            "json has no line_comments, action no-ops"
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
    fn indent_selection_uses_space_indent_style() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        // The 2-space indent of the second line makes the buffer space-styled.
        let path = h.write_file("s.txt", "a\n  b\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::IndentSelection);
        assert_eq!(focused_buffer_text(&mut h), "  a\n  b\n");
    }

    #[test]
    fn indent_selection_skips_blank_lines() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\n\ndef\n");
        h.open_file(&path);
        h.type_keys("%");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::IndentSelection);
        assert_eq!(focused_buffer_text(&mut h), "\tabc\n\n\tdef\n");
    }

    #[test]
    fn unindent_selection_removes_one_space_indent_width() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "  a\n  b\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::UnindentSelection);
        assert_eq!(focused_buffer_text(&mut h), "a\n  b\n");
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
    fn align_from_select_mode_returns_to_normal() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abc\ndefgh\nij\n");
        h.open_file(&path);
        h.type_keys("% alt-s");
        h.stoat.set_focused_mode("select".into());
        assert_eq!(h.stoat.focused_mode(), "select");
        h.type_keys("&");
        assert_eq!(
            h.stoat.focused_mode(),
            "normal",
            "align exits select mode like Helix"
        );
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
    fn extending_two_cursors_into_each_other_leaves_one_selection() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "aa\nbb\ncc\n");
        h.open_file(&path);

        // Cursors on the first two rows, each extended a row down, so the
        // second row belongs to both.
        h.type_keys("shift-C v j");
        assert_eq!(h.selection_spans(), vec![(0, 7, false)]);
    }

    #[test]
    fn deleting_overlapping_selections_spares_the_text_outside_them() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 6);
        let path = h.write_file("s.txt", "aa\nbb\ncc\n");
        h.open_file(&path);
        h.type_keys("shift-C v j");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
        assert_eq!(
            focused_buffer_text(&mut h),
            "c\n",
            "deleting the overlap twice consumed a character no selection covered"
        );
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

    /// An empty buffer holds one collapsed selection, because `min_width_1` has
    /// no grapheme to widen over in either direction. Deleting it covers no
    /// text, so the redo it would otherwise discard has to survive.
    #[test]
    fn deleting_a_collapsed_selection_keeps_the_redo() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "");
        h.open_file(&path);
        h.type_keys("i h e l l o escape");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        assert_eq!(focused_buffer_text(&mut h), "", "undo empties the buffer");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::DeleteSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Redo);
        assert_eq!(
            focused_buffer_text(&mut h),
            "hello",
            "the redo outlives the delete"
        );
    }

    /// Replacing a collapsed selection writes as many characters as it covers,
    /// which is none, so it must leave the redo alone for the same reason.
    #[test]
    fn replacing_a_collapsed_selection_keeps_the_redo() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "");
        h.open_file(&path);
        h.type_keys("i h e l l o escape");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Undo);
        assert_eq!(focused_buffer_text(&mut h), "", "undo empties the buffer");

        h.type_keys("r x");
        assert_eq!(focused_buffer_text(&mut h), "", "nothing to replace");

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Redo);
        assert_eq!(
            focused_buffer_text(&mut h),
            "hello",
            "the redo outlives the replace"
        );
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
                unstaged_lines: std::iter::once(start..(start + 1)).collect(),
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
    fn goto_next_change_uses_a_background_populated_diff_map() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        h.stage_review_scenario("/repo", &[("s.txt", "a\nb\nc\n", "a\nX\nc\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(std::path::Path::new("/repo/s.txt"));
        h.settle_diff_jobs();

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextChange);
        assert_eq!(
            h.primary_head_offset(),
            2,
            "the background-populated diff map drives GotoNextChange to the modified row",
        );
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
    fn helix_bracket_c_jumps_to_next_change() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 15);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
        h.open_file(&path);
        install_diff_hunks(&mut h, &[2, 5, 8]);
        h.type_keys("] c");
        assert_eq!(h.primary_head_offset(), 4);
    }

    #[test]
    fn helix_bracket_c_jumps_to_prev_change() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 15);
        let path = h.write_file("s.txt", "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\n");
        h.open_file(&path);
        install_diff_hunks(&mut h, &[2, 5, 8]);
        h.type_keys("g j");
        h.type_keys("[ c");
        assert_eq!(h.primary_head_offset(), 16);
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
    fn select_sibling_pivots_within_injection_layer() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 10);
        let path = h.write_file("s.md", "aaa **bbb** ccc *ddd* eee\n");
        h.open_file(&path);
        for _ in 0..6 {
            h.type_keys("l");
        }
        assert_eq!(
            h.primary_head_offset(),
            6,
            "test setup: cursor should be on first 'b' of 'bbb'"
        );
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let initial = h.selection_spans()[0];
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
        let after = h.selection_spans()[0];
        assert_ne!(
            after, initial,
            "next sibling inside the inline injection must shift the selection: \
             with the host markdown grammar the entire inline content is one leaf"
        );
        let line_end = 25;
        assert!(
            after.1 <= line_end,
            "next sibling should not escape past the line end {line_end}, got {after:?}"
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
        // A 1-wide cursor already sits on the `a` identifier node, so a single
        // expand reaches the enclosing function_item.
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
        // A 1-wide cursor already sits on the `a` identifier node, so a single
        // expand reaches the enclosing function_item.
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        let on_first_fn = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectNextSibling);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectPrevSibling);
        assert_eq!(h.selection_spans(), on_first_fn);
    }

    #[test]
    fn select_all_siblings_fans_to_each_named_sibling() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        // A 1-wide cursor already sits on the `a` identifier node, so a single
        // expand reaches the enclosing function_item (a zero-width cursor
        // needed two).
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectAllSiblings);
        let spans = h.selection_spans();
        assert_eq!(spans, vec![(0, 9, false), (10, 19, false), (20, 29, false)]);
    }

    #[test]
    fn select_all_children_fans_to_each_named_child() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        h.open_file(&path);
        h.type_keys("l l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ExpandSelection);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectAllChildren);
        let spans = h.selection_spans();
        assert_eq!(spans, vec![(0, 9, false), (10, 19, false), (20, 29, false)]);
    }

    #[test]
    fn select_all_siblings_no_op_without_syntax_map() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "alpha beta gamma\n");
        h.open_file(&path);
        let before = h.selection_spans();
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SelectAllSiblings);
        assert_eq!(h.selection_spans(), before);
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
        assert_eq!(
            spans[0].1,
            spans[0].0 + 1,
            "parent jump collapses to a 1-wide cursor"
        );
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
        assert_eq!(
            spans[0].1,
            spans[0].0 + 1,
            "parent jump collapses to a 1-wide cursor"
        );
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
    fn count_prefix_jump_backward_past_history_is_noop() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.txt", "abcdefghij\n");
        h.open_file(&path);
        h.type_keys("l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
        h.type_keys("l l");
        let before = h.primary_head_offset();
        h.stoat.pending_count = Some(99);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
        assert_eq!(
            h.primary_head_offset(),
            before,
            "a count past the history start is all-or-nothing, so nothing moves"
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

    /// Each cursor scans from itself, so a find keeps a multi-cursor set.
    ///
    /// Scanning once from the primary and stamping the result on every
    /// selection makes them all identical, and identical spans merge, so the
    /// set collapses to a single cursor on the primary's match.
    #[test]
    fn find_next_char_scans_from_each_cursor() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "ax\nax\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AddSelectionBelow);
        assert_eq!(
            h.selection_spans(),
            vec![(0, 1, false), (3, 4, false)],
            "one cursor on each row",
        );

        h.type_keys("f x");
        assert_eq!(
            h.selection_spans(),
            vec![(0, 2, false), (3, 5, false)],
            "each cursor covers up to the x on its own row",
        );
    }

    /// A cursor whose row holds no match stays where it is rather than being
    /// dragged onto another cursor's target.
    #[test]
    fn find_next_char_leaves_unmatched_cursors_alone() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abb\naxb\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AddSelectionBelow);

        h.type_keys("f x");
        assert_eq!(
            h.selection_spans(),
            vec![(0, 1, false), (4, 6, false)],
            "row 0 has no x so its cursor holds, row 1 covers up to its x",
        );
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
        // "alpha beta gamma " is 17 bytes; "delta" starts at offset 17. Three
        // threaded `w` jumps advance the anchor onto the third word start (11)
        // and the head onto "delta", so the span is (11, 17) and the block
        // cursor sits one cell back, on the space at offset 16.
        assert_eq!(h.selection_spans(), vec![(11, 17, false)]);
        assert_eq!(h.cursor_display_positions(), vec![(0, 16)]);
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
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::MoveNextLongWordStart);
        assert_eq!(
            h.primary_head_offset(),
            15,
            "long-word treats `foo.bar` as one word, so 3W from offset 0 \
             selects up to `quux` (head at 16); the block cursor sits one \
             cell back, on the space at 15"
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

    /// A column is a grapheme cluster, not a codepoint.
    ///
    /// A letter carrying a combining mark is two codepoints and one column, so
    /// counting codepoints walks into the middle of the cluster and stops a
    /// column early. Here the third column is the `y`, not the `x`.
    #[test]
    fn goto_column_counts_graphemes_not_codepoints() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "e\u{301}xyz\n");
        h.open_file(&path);
        h.type_keys("3 g |");
        assert_eq!(h.cursor_display_positions(), vec![(0, 2)]);
        assert_eq!(
            h.primary_head_offset(),
            4,
            "the y, one past the two-codepoint cluster and the x",
        );
    }

    /// Every cursor takes the column on its own row.
    ///
    /// Computing one target from the newest selection and stamping it on the
    /// rest makes every span identical, and identical spans merge, so the set
    /// collapses to a single cursor.
    #[test]
    fn goto_column_lands_each_cursor_on_its_own_row() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "abcdef\nghijkl\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AddSelectionBelow);
        assert_eq!(
            h.selection_spans(),
            vec![(0, 1, false), (7, 8, false)],
            "one cursor on each row",
        );

        h.type_keys("3 g |");
        assert_eq!(
            h.selection_spans(),
            vec![(2, 3, false), (9, 10, false)],
            "each cursor lands on column 3 of its own row",
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

    /// Every cursor jumps to the partner of its own bracket.
    ///
    /// Resolving one partner from the newest selection and landing it on the
    /// rest makes every span identical, and identical spans merge, so the set
    /// collapses to a single cursor.
    #[test]
    fn match_brackets_pairs_each_cursor_separately() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "(a)\n(b)\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AddSelectionBelow);
        assert_eq!(
            h.selection_spans(),
            vec![(0, 1, false), (4, 5, false)],
            "one cursor on each opening paren",
        );

        h.type_keys("m m");
        assert_eq!(
            h.selection_spans(),
            vec![(2, 3, false), (6, 7, false)],
            "each cursor lands on the closing paren of its own pair",
        );
    }

    /// A cursor on no bracket holds its place while the others jump.
    #[test]
    fn match_brackets_keeps_cursors_with_no_pair() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file("s.txt", "(a)\nxyz\n");
        h.open_file(&path);
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AddSelectionBelow);

        h.type_keys("m m");
        assert_eq!(
            h.selection_spans(),
            vec![(2, 3, false), (4, 5, false)],
            "row 0 jumps to its closing paren, row 1 has no bracket and stays",
        );
    }

    /// The same for a language shipping a brackets query, which resolves its
    /// partner through the syntax tree rather than the text scan.
    #[test]
    fn match_brackets_pairs_each_cursor_separately_with_a_query() {
        let mut h = crate::test_harness::TestHarness::with_size(30, 5);
        let path = h.write_file("s.rs", "fn a() {}\nfn b() {}\n");
        h.open_file(&path);
        h.type_keys("4 l");
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::AddSelectionBelow);
        assert_eq!(
            h.selection_spans(),
            vec![(4, 5, false), (14, 15, false)],
            "one cursor on each opening paren",
        );

        h.type_keys("m m");
        assert_eq!(
            h.selection_spans(),
            vec![(5, 6, false), (15, 16, false)],
            "each cursor lands on the closing paren of its own pair",
        );
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
    fn match_brackets_from_inside_jumps_to_enclosing() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 5);
        let path = h.write_file("s.rs", "fn a() { let x = 1; }\n");
        h.open_file(&path);
        // Cursor at offset 9 (`let`), between the braces but on no delimiter.
        h.type_keys("9 l m m");
        assert_eq!(
            h.primary_head_offset(),
            20,
            "from inside the braces, mm lands on the closing brace"
        );
        // Now on `}`, so mm returns to the opening brace.
        h.type_keys("m m");
        assert_eq!(
            h.primary_head_offset(),
            7,
            "on the closing brace, mm returns to the opening brace"
        );
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
    fn match_brackets_from_inside_string_jumps_to_quote() {
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
            20,
            "the string quotes are a captured pair, so mm jumps to the closing quote"
        );
    }

    #[test]
    fn match_brackets_char_literal_paren_resolves_to_enclosing() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 5);
        let path = h.write_file("s.rs", "fn f() { let c = '('; }\n");
        h.open_file(&path);
        h.type_keys("1 8 l");
        assert_eq!(
            h.primary_head_offset(),
            18,
            "cursor on the `(` inside the char literal"
        );
        h.type_keys("m m");
        assert_eq!(
            h.primary_head_offset(),
            22,
            "the char literal `(` is not a captured delimiter, so mm resolves to the enclosing brace"
        );
    }

    #[test]
    fn match_brackets_scanner_fallback_without_query() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 5);
        let path = h.write_file("t.toml", "[table]\n");
        h.open_file(&path);
        assert_eq!(h.primary_head_offset(), 0, "cursor starts on the `[`");
        h.type_keys("m m");
        assert_eq!(
            h.primary_head_offset(),
            6,
            "toml has no brackets query, so the scanner matches `[` to `]`"
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
    fn select_mode_x_selects_line_below() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 10);
        let path = h.write_file("s.txt", "a\nb\nc\n");
        h.open_file(&path);
        h.type_keys("v x");
        assert_eq!(
            h.selection_spans(),
            vec![(0, 2, false)],
            "vx selects the first line including its newline"
        );
        assert_eq!(h.stoat.focused_mode(), "select", "x stays in select mode");

        h.type_keys("x");
        assert_eq!(
            h.selection_spans(),
            vec![(0, 4, false)],
            "a second x extends to the line below"
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
