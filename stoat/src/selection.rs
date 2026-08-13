use crate::multi_buffer::MultiBufferSnapshot;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use stoat_text::{
    next_char_boundaries_batch, next_char_boundary, Anchor, Bias, Rope, Selection, SelectionGoal,
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
        self.install(Arc::from([land_block_cursor(
            0,
            0,
            SelectionGoal::None,
            snapshot.rope(),
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
        self.install(Arc::from([land_block_cursor(
            id,
            offset,
            SelectionGoal::None,
            snapshot.rope(),
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
        // [`land_block_cursor`], which clamps down before widening because
        // widening alone starts from wherever a click landed.
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
    /// name keeps the span, the goal, and the anchors it has. A landing on the
    /// very end of the rope stays zero-width, there being no next character to
    /// widen over.
    ///
    /// The clip, sort and merge are [`Self::replace_with`]'s, and the anchors
    /// come out the same. What it saves is the round trip: a caller that already
    /// knows where each cursor landed would otherwise mint two anchors per
    /// cursor, at a root descent each, only for `replace_with` to resolve them
    /// straight back to the offsets it was handed.
    pub(crate) fn land_block_cursors(
        &mut self,
        landings: &[(usize, usize, SelectionGoal)],
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
            // Nothing after the landing means it sits on the end of the rope,
            // which is a cursor position of its own and stays zero-width rather
            // than reaching back over the last character.
            let end = if forward > start { forward } else { start };
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

/// Land a forward 1-wide block cursor on the cell at `target`, preserving
/// `goal`.
///
/// This is the min-width-1 replacement for a bare `collapse_to`. The block
/// cursor sits on `target` and the selection covers that one cell rather than
/// collapsing to a zero-width point. The rope end is the exception: no next
/// character exists to widen over, and the position after the last character is
/// a cursor position of its own, so the landing stays zero-width there.
///
/// `target` is a position rather than a boundary. An offset inside a grapheme
/// cluster lands on the whole cluster.
pub(crate) fn land_block_cursor(
    id: usize,
    target: usize,
    goal: SelectionGoal,
    rope: &Rope,
    buffer: &MultiBufferSnapshot,
) -> Selection<Anchor> {
    // A mouse click resolves through a display clip that walks codepoints, so
    // it names an offset inside a cluster. Widening from there jumps to the
    // cluster's end and covers only its tail. Clamping down first is
    // `replace_with`'s start rule, and it is what a click means: the cell
    // belongs to the cluster it is drawn inside. A caller landing a motion
    // result is already on a boundary, so the clamp costs it nothing.
    let target = rope.clip_to_grapheme_boundary(target, Bias::Left);
    let widened = Selection {
        id,
        start: target,
        end: target,
        reversed: false,
        goal,
    }
    .min_width_1(rope);
    anchor_selection(widened, buffer)
}

/// Land a forward 1-wide block cursor covering the character after `target`,
/// or a zero-width cursor at the rope end.
///
/// This is the insert-mode counterpart to [`land_block_cursor`]. An insert
/// cursor sits at the insertion point, so it widens forward and never steps
/// back at the buffer end. A backward step there moves the insertion point
/// before the last inserted character and corrupts further typing.
pub(crate) fn forward_block_cursor(
    id: usize,
    target: usize,
    goal: SelectionGoal,
    rope: &Rope,
    buffer: &MultiBufferSnapshot,
) -> Selection<Anchor> {
    let end = next_char_boundary(rope, target);
    Selection {
        id,
        start: buffer.anchor_at(target, Bias::Right),
        end: buffer.anchor_at(end, Bias::Right),
        reversed: false,
        goal,
    }
}

/// Re-anchor an offset-based selection produced by the block-cursor helpers.
pub(crate) fn anchor_selection(
    landed: Selection<usize>,
    buffer: &MultiBufferSnapshot,
) -> Selection<Anchor> {
    Selection {
        id: landed.id,
        start: buffer.anchor_at(landed.start, Bias::Right),
        end: buffer.anchor_at(landed.end, Bias::Right),
        reversed: landed.reversed,
        goal: landed.goal,
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
                c.land_block_cursors(&[(0, 2, SelectionGoal::None)], s)
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

        collection.land_block_cursors(&[(1, 3, SelectionGoal::None)], &snapshot);

        let merged = collection.all_anchors();
        assert_eq!(merged.len(), 1, "the landing sits inside the other span");
        assert!(
            !merged[0].reversed,
            "one input faced forward, so the merge does",
        );
    }

    /// One selection landed on `offset`, read back as `(start, end, goal)`.
    fn land_one(text: &str, offset: usize, goal: SelectionGoal) -> (usize, usize, SelectionGoal) {
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

        collection.land_block_cursors(&[(1, offset, goal)], &snapshot);

        let landed = &collection.all_anchors()[0];
        (
            snapshot.resolve_anchor(&landed.start),
            snapshot.resolve_anchor(&landed.end),
            landed.goal,
        )
    }

    /// A cursor landing on the rope's end has no character after it and covers
    /// none, staying on the position it was sent to.
    ///
    /// Reaching back over the last character instead makes that position
    /// indistinguishable from the one before it, which leaves nothing able to
    /// put a cursor past the final character. Every caller wants the same
    /// answer here. A motion lands where it landed, and an insert cursor must
    /// not step back before the character just typed.
    #[test]
    fn a_landing_on_the_rope_end_stays_zero_width() {
        let text = "ab";
        let end = text.len();

        assert_eq!(
            land_one(text, end, SelectionGoal::None),
            (end, end, SelectionGoal::None),
        );
        assert_eq!(
            land_one(text, 1, SelectionGoal::None),
            (1, 2, SelectionGoal::None),
            "a landing with a character after it still covers that cell",
        );
    }

    /// Vertical motion carries a goal column across rows, so a landing has to
    /// arrive with the one it was given rather than a cleared one.
    #[test]
    fn a_landing_keeps_the_goal_it_was_given() {
        let (start, end, goal) = land_one("abc", 1, SelectionGoal::Column(7));
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

        through_offsets.land_block_cursors(&landings, &snapshot);

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
}
