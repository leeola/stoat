//! Multi-layer syntax tree storage for languages with injections.
//!
//! A [`SumTree`] of [`SyntaxLayer`]s, each carrying one parsed
//! [`tree_sitter::Tree`] plus enough metadata to walk the layers in document
//! order, depth by depth. That lets a single capture iterator merge highlights
//! across the root grammar and every injection, at any nesting depth, rather
//! than looping over host nodes one level down.
//!
//! This is the path every highlight takes. [`crate::SyntaxState`] holds the
//! root tree alongside it for the consumers that want one tree rather than
//! merged captures, which are the indent queries, the code index, and the next
//! parse, since that edits the prior tree rather than building one.
//!
//! Pattern adapted from
//! `references/zed/crates/language/src/syntax_map.rs`. The pipeline is:
//!
//! ```text
//!   Buffer edit
//!     -> SyntaxMap::interpolate(edits)        // tree.edit() each layer (foreground, microseconds)
//!     -> SyntaxMap::reparse(rope, lang)       // background, multi-layer
//!         -> per-layer ParseStep queue
//!         -> get_injections() finds new layers via injections.scm
//!         -> Parser::set_included_ranges() supports combined injections
//!     -> SyntaxSnapshot::captures(range)      // merges QueryCaptures across layers
//!         -> BufferChunks emits styled chunks
//! ```

use crate::{
    highlight::{
        apply_input_edits, input_edits, parse_rope_inner, QueryCursorHandle, RopeTextProvider,
    },
    language::{InjectionGrouping, InjectionInner},
    language_for_fence_token, parse_rope_range, Language,
};
use std::{
    cmp::Reverse,
    collections::{HashMap, VecDeque},
    ops::Range,
    slice,
    sync::Arc,
    time::Instant,
};
use stoat_scheduler::Executor;
use stoat_text::{
    patch::Edit as PatchEdit, Bias, ContextLessSummary, Dimension, Item, Rope, SumTree,
};
use tree_sitter::{Node, Query, StreamingIterator, Tree};

/// How many captures [`SyntaxSnapshot::captures`] reserves room for.
///
/// A guess rather than a measurement, since the count is only known after the
/// walk. It is set where a whole-file walk of an ordinary source file lands, so
/// the common case allocates once instead of copying everything found so far at
/// each doubling.
const CAPTURE_CAPACITY: usize = 256;

/// One parsed tree at a particular nesting depth, anchored to a
/// `[start_offset, end_offset)` byte range in the host buffer.
///
/// Depth 0 is the root grammar. Each injection adds 1 to the depth of
/// the layer it lives inside. Multiple injections at the same depth
/// (e.g. all rust code fences in a markdown file) are stored as
/// separate layers and queried in document order.
#[derive(Clone)]
pub struct SyntaxLayer {
    pub depth: u32,
    pub start_offset: u32,
    pub end_offset: u32,
    pub language: Arc<Language>,
    pub tree: Tree,
}

impl std::fmt::Debug for SyntaxLayer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SyntaxLayer")
            .field("depth", &self.depth)
            .field("start_offset", &self.start_offset)
            .field("end_offset", &self.end_offset)
            .field("language", &self.language.name)
            .finish()
    }
}

/// SumTree summary for [`SyntaxLayer`]. Ordered by `(depth, start_offset)`
/// so layer iteration walks the tree shallowest-to-deepest, in document
/// order within each depth. Matches the iteration shape Zed's
/// `SyntaxMapCaptures` consumes.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct LayerKey {
    pub depth: u32,
    pub start_offset: u32,
}

/// What a subtree of layers reports to the one above it.
///
/// The key is the seek position, and the count is there because a sum-tree
/// carries no length of its own. Without it, asking how many layers a snapshot
/// holds means walking every one of them, which is what
/// [`SyntaxSnapshot::layer_count`] used to do on a path that runs per edit.
///
/// `max_end` is how far past a byte offset a subtree reaches. Layers order by
/// `(depth, start_offset)`, so start offsets restart at every depth and a seek
/// bounds no range query. The furthest end bounds one from below: a subtree
/// whose layers all end before a range starts holds nothing in that range.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LayerSummary {
    pub key: LayerKey,
    pub count: usize,
    pub max_end: u32,
}

impl ContextLessSummary for LayerSummary {
    fn add_summary(&mut self, other: &Self) {
        // The cumulative position of an ordered key is just the latest item's,
        // which is what a seek compares against. The count and the furthest end
        // are the two here that genuinely accumulate.
        self.key = other.key.clone();
        self.count += other.count;
        self.max_end = self.max_end.max(other.max_end);
    }
}

/// The key doubles as a cursor position, so a seek names a layer by
/// `(depth, start_offset)` and slices the run ahead of it.
impl<'a> Dimension<'a, LayerSummary> for LayerKey {
    fn zero(_: ()) -> Self {
        LayerKey::default()
    }

    fn add_summary(&mut self, summary: &'a LayerSummary, _: ()) {
        self.clone_from(&summary.key);
    }
}

impl Item for SyntaxLayer {
    type Summary = LayerSummary;
    fn summary(&self, _cx: ()) -> LayerSummary {
        LayerSummary {
            key: LayerKey {
                depth: self.depth,
                start_offset: self.start_offset,
            },
            count: 1,
            max_end: self.end_offset,
        }
    }
}

/// Immutable snapshot of every [`SyntaxLayer`] for one buffer version.
/// Cheap to clone (the inner [`SumTree`] is `Arc`-backed). Held by
/// [`SyntaxMap`]; threaded through the parse / highlight pipeline once
/// the migration lands.
#[derive(Clone, Default)]
pub struct SyntaxSnapshot {
    pub layers: SumTree<SyntaxLayer>,
    pub parsed_version: u64,
}

impl SyntaxSnapshot {
    pub fn is_empty(&self) -> bool {
        self.layers.is_empty()
    }

    pub fn layer_count(&self) -> usize {
        self.layers.summary().count
    }

    /// Iterate every layer in `(depth, start_offset)` order. The
    /// capture-merging iterator interleaves `QueryCaptures` from all
    /// matching layers in this order.
    pub fn iter_layers(&self) -> impl Iterator<Item = &SyntaxLayer> {
        self.layers.iter()
    }

    /// Materialize captures matching `select(layer.language)` across
    /// every layer, restricted to `byte_range`, sorted in document
    /// order. The per-layer query is selected by the `select`
    /// callback so callers can request highlights, brackets, indents,
    /// etc. without each layer needing to expose every query
    /// separately.
    ///
    /// Returns a [`Vec`] of [`SyntaxMapCapture`] entries sorted by
    /// `(start_byte, Reverse(end_byte), depth)`. Each capture carries
    /// the originating [`SyntaxLayer`]'s depth and language so
    /// consumers can resolve per-grammar style tables. Mirrors Zed's
    /// [`SyntaxMapCaptures`](references/zed/crates/language/src/syntax_map.rs:64-1209)
    /// in shape but materializes eagerly because tree-sitter's
    /// [`QueryCaptures`] borrows from a [`tree_sitter::QueryCursor`]
    /// whose lifetime is awkward to thread through a self-referential
    /// iterator. The eager Vec is fine for highlight extraction (a
    /// few hundred captures per render); a streaming variant can
    /// land later if profiling shows the allocation is hot.
    pub fn captures<'a>(
        &'a self,
        byte_range: Range<usize>,
        rope: &'a Rope,
        select: impl Fn(&'a Language) -> Option<&'a Query>,
    ) -> Vec<SyntaxMapCapture<'a>> {
        // A whole-file walk of a large buffer runs to thousands of captures,
        // and reaching that by growth costs a copy of everything found so far
        // at every doubling. A narrow range overshoots into an allocation it
        // was making anyway.
        let mut all: Vec<SyntaxMapCapture<'a>> = Vec::with_capacity(CAPTURE_CAPACITY);

        // One cursor for every layer. Taking one per layer is a pool
        // lock/unlock pair each way plus a range reset the next line
        // overwrites, and `set_byte_range` is sticky, so the range set here
        // covers the whole loop.
        let mut cursor = QueryCursorHandle::new();
        cursor.set_byte_range(byte_range.clone());

        // A file of rust doc comments carries a markdown layer per block and an
        // inline layer per inline node, so the tree runs to thousands. The
        // filter descends past a subtree whose layers all end before the range
        // starts, which is most of them for the narrow covers a recapture asks.
        let reaching = self
            .layers
            .filter::<_, ()>((), |s| s.max_end as usize > byte_range.start);

        for layer in reaching {
            // The summary bounds a subtree from below only. The range's own end
            // is still the layer's to answer, since start offsets restart at
            // every depth and so no summary orders them.
            if (layer.end_offset as usize) <= byte_range.start
                || (layer.start_offset as usize) >= byte_range.end
            {
                continue;
            }
            let Some(query) = select(layer.language.as_ref()) else {
                continue;
            };
            let provider = RopeTextProvider { rope };
            // QueryCursor::captures yields &(QueryMatch, capture_index)
            // tuples. The capture_index picks out which capture in the
            // match's `captures` array this iteration is yielding.
            //
            // The iterator holds the cursor's mutable borrow and is dropped
            // here at the end of the body, before the next layer asks for it.
            // The nodes it yields borrow the tree rather than the cursor, so
            // they outlive both.
            let mut iter = cursor.captures(query, layer.tree.root_node(), provider);
            while let Some(item) = iter.next() {
                let pattern_match = &item.0;
                let cap_index = item.1;
                let cap = pattern_match.captures[cap_index];
                all.push(SyntaxMapCapture {
                    node: cap.node,
                    index: cap.index,
                    depth: layer.depth,
                    language: layer.language.as_ref(),
                });
            }
        }
        // Document order is the primary key, and shallower layers come first
        // where two captures share a start and an end.
        //
        // The keys are cached because reading a node's range crosses into
        // tree-sitter twice. A plain comparison sort would ask both sides of
        // every comparison rather than each capture once.
        all.sort_by_cached_key(|c| {
            let r = c.node.byte_range();
            (r.start, Reverse(r.end), c.depth)
        });
        all
    }
}

/// One capture yielded by [`SyntaxSnapshot::captures`]. Carries the
/// originating layer's depth and language so consumers can route the
/// capture through the right per-grammar style table.
#[derive(Clone, Copy)]
pub struct SyntaxMapCapture<'a> {
    pub node: Node<'a>,
    pub index: u32,
    pub depth: u32,
    pub language: &'a Language,
}

/// Mutable container around [`SyntaxSnapshot`]. Held by the host
/// editor per buffer alongside [`crate::SyntaxState`] until callers
/// migrate from the single-tree highlight path.
///
/// Cloning costs a handful of refcount bumps, since the layer [`SumTree`]
/// is `Arc`-backed and cloning a [`Tree`] only retains its root subtree.
/// A caller whose reparse may fail can therefore clone the prior map, work
/// on the clone, and commit it only once the reparse succeeds.
#[derive(Clone, Default)]
pub struct SyntaxMap {
    snapshot: SyntaxSnapshot,
}

impl SyntaxMap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> &SyntaxSnapshot {
        &self.snapshot
    }

    /// Replace the layer set with a fresh sum-tree. Called by
    /// [`Self::reparse`] after each rebuild and by tests that want to
    /// install a known layer set in one go.
    ///
    /// Layers are sorted by `(depth, start_offset)` before insertion so
    /// iteration matches the layer-key order even if the caller provides
    /// them out of order. [`SumTree::from_iter`] preserves insertion
    /// order, so the sort is required for deterministic walks.
    pub fn install_layers(&mut self, layers: impl IntoIterator<Item = SyntaxLayer>, version: u64) {
        let mut sorted: Vec<SyntaxLayer> = layers.into_iter().collect();
        sorted.sort_by_key(|l| (l.depth, l.start_offset));
        self.snapshot = SyntaxSnapshot {
            layers: SumTree::from_iter(sorted, ()),
            parsed_version: version,
        };
    }

    /// Replay `edits` onto every layer so the map describes `new_rope`.
    ///
    /// Each layer's [`Tree`] replays `edits`, leaving it positioned to act as
    /// `old_tree` for an incremental [`reparse`](Self::reparse), and its
    /// `start_offset`/`end_offset` are moved to the same text's new
    /// position. Both halves matter: the tree
    /// alone would leave the layer record pointing at whatever text now
    /// occupies its old byte range, which
    /// [`SyntaxSnapshot::captures`] reads to decide whether a layer covers
    /// the requested range, and which
    /// [`Self::reparse_within_changed_ranges`] reads to decide which layers
    /// its filtered walk is responsible for re-discovering.
    ///
    /// A layer whose bounds fall inside replaced text collapses onto that
    /// replacement's start, so it becomes empty rather than spanning
    /// unrelated text. The following reparse then drops it, since a layer
    /// inside an edit is one the injection walk must re-find to keep.
    ///
    /// A layer lying entirely before every edit is left as it is. Nothing about
    /// it moves, so both halves of the work above would be writing back what is
    /// already there. That is a narrower test than the layer not intersecting
    /// an edit. One sitting after an edit intersects nothing and still has to be
    /// replayed, since its tree holds absolute offsets the edit shifts.
    pub fn interpolate(&mut self, edits: &[PatchEdit<usize>], old_rope: &Rope, new_rope: &Rope) {
        let Some(first_edit) = edits.first().map(|edit| edit.old.start) else {
            return;
        };
        let ts_edits = input_edits(edits, old_rope, new_rope);

        let replayed = |layer: &SyntaxLayer| {
            let mut next = layer.clone();
            if (layer.end_offset as usize) <= first_edit {
                return next;
            }

            apply_input_edits(&mut next.tree, &ts_edits);

            next.start_offset = translate_offset(edits, layer.start_offset as usize) as u32;
            next.end_offset =
                (translate_offset(edits, layer.end_offset as usize) as u32).max(next.start_offset);

            next
        };

        // Built rather than installed, because installing sorts. Depth does not
        // move here, and `translate_offset` is the identity below the first edit
        // and monotone above it, so no layer's key crosses another's and the
        // appended order is the order the tree wants.
        let carried = {
            // The summary says how far a subtree reaches, so this walk descends
            // past whole runs of layers the edit leaves alone and stops only at
            // the ones that need replaying.
            let reached = self
                .snapshot
                .layers
                .filter::<_, ()>((), |s| s.max_end as usize > first_edit);
            let mut cursor = self.snapshot.layers.cursor::<LayerKey>(());
            let mut built = SumTree::new(());

            for layer in reached {
                let key = LayerKey {
                    depth: layer.depth,
                    start_offset: layer.start_offset,
                };
                built.append(cursor.slice(&key, Bias::Left), ());

                // Two layers of one depth sharing a start offset stop the slice
                // at the first of them rather than at the one the filter named,
                // so every layer at this key answers for itself. A later filter
                // item at the same key then finds the cursor already past it.
                while let Some(at_key) = cursor
                    .item()
                    .filter(|l| l.depth == key.depth && l.start_offset == key.start_offset)
                {
                    built.push(replayed(at_key), ());
                    cursor.next();
                }
            }

            built.append(cursor.suffix(), ());
            built
        };

        self.snapshot.layers = carried;
    }

    /// Reparse `rope` against `language`, replacing every layer.
    ///
    /// Convenience wrapper around
    /// [`Self::reparse_within_changed_ranges`] that walks the entire
    /// tree (no changed-range filter).
    pub fn reparse(
        &mut self,
        rope: &Rope,
        language: Arc<Language>,
        version: u64,
        root: Option<&Tree>,
        deadline: Option<(Instant, &Executor)>,
    ) -> Option<Vec<Range<usize>>> {
        self.reparse_within_changed_ranges(rope, language, version, None, root, deadline)
    }

    /// Reparse `rope` against `language`, optionally filtering the
    /// injection query walk to a `changed_ranges` set so only
    /// recently-edited regions are re-queried for new injection host
    /// nodes.
    ///
    /// When `changed_ranges` is `Some`, each range is expanded by ±1
    /// row before being used as the query filter. The expansion
    /// catches comment-toggled injection boundaries (toggling a
    /// comment on a line doesn't change byte offsets but can flip
    /// whether a code fence on the previous or next line is part of
    /// the injection's host range).
    ///
    /// Mirrors `references/zed/crates/language/src/syntax_map.rs:806-822`.
    ///
    /// Recursive multi-level injection: walks the freshly-parsed
    /// root tree against the language's `injection_query`, parses
    /// each host node into a depth+1 [`SyntaxLayer`] via
    /// [`parse_rope_range`] (with `set_included_ranges` so the inner
    /// tree's nodes carry rope-absolute byte offsets), then queues
    /// the new layer for its own injection walk so nested injections
    /// (e.g. a regex inside a string inside a markdown code fence)
    /// are also discovered.
    ///
    /// Combined injections: multiple matches of the same inner
    /// language at the same depth are merged into a single tree via
    /// `set_included_ranges`, mirroring Zed's behavior.
    ///
    /// Prior trees from the same host range are reused as `old_tree`
    /// for incremental reparse.
    ///
    /// A caller holding a parse of this same rope passes it as `root` to
    /// skip the root parse, which is otherwise repeated work. The tree must
    /// be a parse of `rope` under `language`. It is installed as the depth-0
    /// layer verbatim, so one taken from other text or another grammar would
    /// publish layer offsets that do not describe the buffer.
    ///
    /// Every parse below this call honors `deadline`, so a caller on a latency
    /// budget cannot be stalled by a pathological injection layer. Passing it
    /// costs the whole reparse rather than one layer, since a map missing a
    /// layer would be indistinguishable from one whose injection genuinely
    /// went away. A `None` return means the deadline passed or the root parse
    /// failed, and leaves the map as the caller handed it over.
    ///
    /// Returns the byte ranges where the layer set moved, merged and in
    /// document order. An injection can restyle its whole span with the host
    /// tree identical throughout, so a caller narrowing its own work to the
    /// host tree's changed ranges would miss it. Adding these is what makes
    /// such a narrowing sound over an injected buffer.
    pub fn reparse_within_changed_ranges(
        &mut self,
        rope: &Rope,
        language: Arc<Language>,
        version: u64,
        changed_ranges: Option<&[Range<usize>]>,
        root: Option<&Tree>,
        deadline: Option<(Instant, &Executor)>,
    ) -> Option<Vec<Range<usize>>> {
        let expanded_ranges = changed_ranges.map(|ranges| expand_changed_rows(rope, ranges));
        // Continue with the body of the original `reparse`.
        self.reparse_inner(
            rope,
            language,
            version,
            expanded_ranges.as_deref(),
            root,
            deadline,
        )
    }

    fn reparse_inner(
        &mut self,
        rope: &Rope,
        language: Arc<Language>,
        version: u64,
        injection_filter_ranges: Option<&[Range<usize>]>,
        root: Option<&Tree>,
        deadline: Option<(Instant, &Executor)>,
    ) -> Option<Vec<Range<usize>>> {
        // Snapshot prior injection layers keyed by (host_range, language name)
        // so we can reuse them when the same host node still exists.
        let prior_injections: Vec<PriorInjection> = self
            .snapshot
            .layers
            .iter()
            .filter(|l| l.depth >= 1)
            .map(|l| PriorInjection {
                depth: l.depth,
                start_offset: l.start_offset,
                end_offset: l.end_offset,
                language: l.language.clone(),
                host_ranges: l.tree.included_ranges(),
                tree: l.tree.clone(),
            })
            .collect();

        // The walk asks after a prior once per host range it rediscovers, so
        // scanning the list would cost priors times discoveries. Two of the
        // three lookups want an exact host range, which is a key.
        let priors_by_host: HashMap<(u32, u32, &'static str), &PriorInjection> = prior_injections
            .iter()
            .map(|p| ((p.start_offset, p.end_offset, p.language.name), p))
            .collect();

        // The third wants whichever prior overlaps a range, which no exact key
        // answers, so it reads a start-ordered index of the same list.
        let prior_key = |p: &PriorInjection| IndexKey {
            depth: p.depth,
            language: p.language.name,
            span: p.start_offset as usize..p.end_offset as usize,
        };
        let priors_by_span = InjectionIndex::new(prior_injections.iter(), prior_key);

        let root_tree = match root {
            Some(tree) => tree.clone(),
            None => {
                let prior_root_tree = self
                    .snapshot
                    .layers
                    .iter()
                    .find(|l| l.depth == 0)
                    .map(|l| l.tree.clone());
                parse_rope_inner(&language, rope, prior_root_tree.as_ref(), None, deadline)?
            },
        };

        // An injection parse reports both "no tree here" and "out of time" as
        // `None`, and the walk's ordinary answer to the first is to move on
        // without a layer. Doing that after the budget ran out would install a
        // map that looks complete while missing the layers the abort cut off,
        // so the clock decides which of the two happened.
        let out_of_time = || deadline.is_some_and(|(dl, executor)| executor.now() >= dl);

        let injection_filter_ranges = injection_filter_ranges.filter(|ranges| !ranges.is_empty());

        // Where this walk moved the layer set, for a caller narrowing its own
        // work to the ranges a restyle could have reached.
        let mut layer_changes: Vec<Range<usize>> = Vec::new();

        // Queue of (depth, language, tree, parent host range) for the
        // BFS-like injection walk. Start with the root layer.
        let mut new_layers = vec![SyntaxLayer {
            depth: 0,
            start_offset: 0,
            end_offset: rope.len() as u32,
            language: language.clone(),
            tree: root_tree.clone(),
        }];

        // Process layers in FIFO order so each layer's children are
        // discovered after their parent. The queue grows as nested
        // injections are found.
        let mut queue: VecDeque<usize> = VecDeque::new();
        queue.push_back(0);
        while let Some(layer_idx) = queue.pop_front() {
            let parent = new_layers[layer_idx].clone();
            let parent_lang = parent.language.clone();
            let parent_tree = parent.tree.clone();
            let parent_depth = parent.depth;

            let Some(injection_query) = parent_lang.injection_query.as_ref() else {
                continue;
            };

            // Group injection matches by inner language, so a combined
            // language's host ranges are all in hand when its one tree is
            // parsed over them.
            //
            // Keyed by language rather than by language and combined-ness,
            // because two injections disagreeing on the flag would put a
            // merged layer and per-node layers over the same text, which is
            // the overlap [`InjectionIndex`] rules out.
            let mut grouped: HashMap<&'static str, InjectionGroup> = HashMap::new();

            let mut cursor = QueryCursorHandle::new();
            // Refilled per fence match rather than allocated per fence match.
            // An info string is a handful of bytes and every fence in the
            // buffer produces one.
            let mut fence_token = String::new();
            // When the caller has supplied changed-range filters,
            // restrict the injection query to the union of those
            // ranges. The cursor's `set_byte_range` only accepts a
            // single range, so we walk each filter range in turn and
            // collect the matches; for the common no-filter case we
            // walk the whole tree once.
            let whole_rope = 0..rope.len();
            let filter_ranges: &[Range<usize>] = match injection_filter_ranges {
                Some(ranges) if !ranges.is_empty() => ranges,
                _ => slice::from_ref(&whole_rope),
            };
            // A block layer is re-queried whole or carried whole, never patched,
            // so a filter reaching into one has to reach across all of it. This
            // is what lets the prior-range splice below be skipped for blocks:
            // the fresh matches already cover every block the edit touched.
            let mut widened: Vec<Range<usize>> = Vec::new();
            if injection_filter_ranges.is_some() {
                for injection in &parent_lang.injections {
                    if injection.grouping != InjectionGrouping::CombinedBlocks {
                        continue;
                    }
                    let InjectionInner::Fixed { language, .. } = &injection.inner else {
                        continue;
                    };
                    let Some(inner_lang) = language.get() else {
                        continue;
                    };
                    for filter in filter_ranges {
                        let mut span = filter.clone();
                        for prior in priors_by_span.overlapping(
                            parent_depth + 1,
                            inner_lang.name,
                            filter.clone(),
                            prior_key,
                        ) {
                            span.start = span.start.min(prior.start_offset as usize);
                            span.end = span.end.max(prior.end_offset as usize);
                        }
                        widened.push(span);
                    }
                }
            }
            if !widened.is_empty() {
                widened.sort_by_key(|r| r.start);
                merge_ranges(&mut widened);
            }
            let filter_ranges: &[Range<usize>] = match widened.is_empty() {
                true => filter_ranges,
                false => &widened,
            };

            for filter in filter_ranges {
                cursor.set_byte_range(filter.clone());
                let provider = RopeTextProvider { rope };
                let mut matches =
                    cursor.matches(injection_query, parent_tree.root_node(), provider);
                while let Some(m) = matches.next() {
                    let pattern_index = m.pattern_index;
                    let Some(injection) = parent_lang.injections.get(pattern_index) else {
                        continue;
                    };
                    match &injection.inner {
                        InjectionInner::Fixed { language, .. } => {
                            // Unfilled when the language was built outside the
                            // registry that wires these, leaving nothing to
                            // inject rather than a layer with no grammar.
                            let Some(inner_lang) = language.get() else {
                                continue;
                            };
                            for capture in m.captures {
                                // An empty range is kept here and dropped after
                                // grouping. A bare `///` line captures nothing,
                                // and dropping it now breaks the row adjacency
                                // that holds its block together, splitting one
                                // block in two at every blank doc line.
                                let inner_start = capture.node.start_byte();
                                let inner_end = capture.node.end_byte();
                                let group = grouped.entry(inner_lang.name).or_insert_with(|| {
                                    InjectionGroup {
                                        language: inner_lang.clone(),
                                        grouping: injection.grouping,
                                        ranges: Vec::new(),
                                    }
                                });
                                debug_assert_eq!(
                                    group.grouping, injection.grouping,
                                    "every injection naming {} must agree on grouping, or \
                                     its merged layer would overlap its per-node ones",
                                    inner_lang.name,
                                );
                                // The rows come from the node, which is in hand
                                // only here. Blocks are cut from them after the
                                // walk, where the order several filter ranges
                                // delivered matches in no longer matters.
                                group.ranges.push(HostRange {
                                    range: inner_start..inner_end,
                                    start_row: capture.node.start_position().row,
                                    end_row: capture.node.end_position().row,
                                });
                            }
                        },
                        InjectionInner::Fence => {
                            // A fence names its own language and is its own
                            // document, so resolve the info string and parse the
                            // content as a standalone layer per match rather than
                            // grouping it with the combined injections above.
                            let names = injection_query.capture_names();
                            let mut has_token = false;
                            let mut content: Option<Range<usize>> = None;
                            for capture in m.captures {
                                match names.get(capture.index as usize).copied() {
                                    Some("injection.language") => {
                                        fence_token.clear();
                                        fence_token.extend(
                                            rope.chunks_in_range(capture.node.byte_range()),
                                        );
                                        has_token = true;
                                    },
                                    Some("injection.content") => {
                                        let r = capture.node.byte_range();
                                        if r.end > r.start {
                                            content = Some(r);
                                        }
                                    },
                                    _ => {},
                                }
                            }
                            let (true, Some(content)) = (has_token, content) else {
                                continue;
                            };
                            let Some(candidates) = parent_lang.fence_candidates.get() else {
                                continue;
                            };
                            let Some(inner_lang) =
                                language_for_fence_token(&fence_token, candidates)
                            else {
                                continue;
                            };
                            let prior = priors_by_host
                                .get(&(content.start as u32, content.end as u32, inner_lang.name))
                                .copied();
                            let Some(inner_tree) = parse_rope_range(
                                &inner_lang,
                                rope,
                                content.clone(),
                                prior.map(|p| &p.tree),
                                deadline,
                            ) else {
                                if out_of_time() {
                                    return None;
                                }
                                continue;
                            };
                            record_layer_change(
                                &mut layer_changes,
                                &content,
                                prior.map(|p| &p.tree),
                                &inner_tree,
                            );
                            new_layers.push(SyntaxLayer {
                                depth: parent_depth + 1,
                                start_offset: content.start as u32,
                                end_offset: content.end as u32,
                                language: inner_lang,
                                tree: inner_tree,
                            });
                            queue.push_back(new_layers.len() - 1);
                        },
                    }
                }
            }
            drop(cursor);

            for (_, group) in grouped {
                let InjectionGroup {
                    language: inner_lang,
                    grouping,
                    ranges,
                } = group;

                // Blocks are cut before anything below reads the ranges, so the
                // rest of the loop works in plain byte ranges either way. Every
                // other grouping is one block holding all of them, which is what
                // makes the arms below read the same for all three.
                let mut blocks = match grouping {
                    InjectionGrouping::CombinedBlocks => block_ranges(ranges),
                    _ => vec![ranges
                        .into_iter()
                        .map(|host| host.range)
                        .filter(|r| !r.is_empty())
                        .collect()],
                };
                blocks.retain(|block: &Vec<Range<usize>>| !block.is_empty());

                for mut ranges in blocks {
                    // A filtered walk queried only the host nodes near the edit, so
                    // the ranges a combined layer needs are the whole layer only by
                    // accident. The rest are recovered from the prior layer's tree,
                    // whose included ranges `SyntaxMap::interpolate` has already
                    // shifted into current coordinates.
                    //
                    // A per-node language wants none of this. Its untouched ranges
                    // are whole prior layers, which `carry_unvisited_injections`
                    // restores, so splicing them in would reparse exactly the text
                    // the filter exists to skip.
                    //
                    // Blocks want none of it either. The filter was widened to every
                    // prior block it touched, so a touched block re-queried whole and
                    // an untouched one carries.
                    //
                    // Anything the filter reached was re-walked and the fresh answer
                    // stands there, so only untouched ranges carry over.
                    if let (InjectionGrouping::Combined, Some(filter)) =
                        (grouping, injection_filter_ranges)
                    {
                        let found_start = ranges.iter().map(|r| r.start).min().unwrap_or(0);
                        let found_end = ranges.iter().map(|r| r.end).max().unwrap_or(0);
                        let prior = priors_by_span
                            .overlapping(
                                parent_depth + 1,
                                inner_lang.name,
                                found_start..found_end,
                                prior_key,
                            )
                            .next();
                        if let Some(prior) = prior {
                            ranges.extend(
                                prior
                                    .host_ranges
                                    .iter()
                                    .map(|r| r.start_byte..r.end_byte)
                                    .filter(|r| {
                                        !r.is_empty()
                                            && !filter
                                                .iter()
                                                .any(|f| r.start < f.end && f.start < r.end)
                                    }),
                            );
                            ranges.sort_by_key(|r| r.start);
                            merge_ranges(&mut ranges);
                        }
                    }

                    // A combined injection over several host ranges is the one case
                    // that parses them together. Holding a single range it is
                    // indistinguishable from a per-node layer, so it falls through
                    // to the loop below and gets the same exact-bounds prior reuse.
                    if grouping.is_combined() && ranges.len() > 1 {
                        let mut sorted = ranges;
                        sorted.sort_by_key(|r| r.start);
                        let found_start = sorted.first().map(|r| r.start).unwrap_or(0);
                        let found_end = sorted.last().map(|r| r.end).unwrap_or(0);

                        // A combined layer's bounds move whenever a host range is
                        // added or dropped, so it is matched by overlap rather than
                        // by the exact bounds a single-range layer is looked up on.
                        // The layer's own text is what carries across, not its
                        // extent.
                        let prior = priors_by_span
                            .overlapping(
                                parent_depth + 1,
                                inner_lang.name,
                                found_start..found_end,
                                prior_key,
                            )
                            .next();

                        let merged_start = found_start;
                        let merged_end = found_end;
                        let Some(inner_tree) =
                            parse_rope_combined_ranges(&inner_lang, rope, &sorted, prior, deadline)
                        else {
                            if out_of_time() {
                                return None;
                            }
                            continue;
                        };
                        // Diffing against a prior matched by overlap only names
                        // where the two trees disagree, which misses text the layer
                        // no longer covers. The carry step below reports the whole
                        // span of any prior layer it drops, and a prior reached
                        // here was dropped there, so that half is already accounted
                        // for.
                        record_layer_change(
                            &mut layer_changes,
                            &(merged_start..merged_end),
                            prior.map(|p| &p.tree),
                            &inner_tree,
                        );
                        new_layers.push(SyntaxLayer {
                            depth: parent_depth + 1,
                            start_offset: merged_start as u32,
                            end_offset: merged_end as u32,
                            language: inner_lang.clone(),
                            tree: inner_tree,
                        });
                        queue.push_back(new_layers.len() - 1);
                        continue;
                    }

                    // The query runs once per filter range, and the filters
                    // themselves may overlap, so a host node near two edits is
                    // matched twice. Two layers over identical bounds is the
                    // overlap [`InjectionIndex`] rules out, so the duplicates go
                    // here rather than into the layer set.
                    ranges.sort_by_key(|r| r.start);
                    ranges.dedup();

                    for r in ranges {
                        let prior = priors_by_host
                            .get(&(r.start as u32, r.end as u32, inner_lang.name))
                            .copied();
                        let Some(inner_tree) = parse_rope_range(
                            &inner_lang,
                            rope,
                            r.clone(),
                            prior.map(|p| &p.tree),
                            deadline,
                        ) else {
                            if out_of_time() {
                                return None;
                            }
                            continue;
                        };
                        record_layer_change(
                            &mut layer_changes,
                            &r,
                            prior.map(|p| &p.tree),
                            &inner_tree,
                        );
                        new_layers.push(SyntaxLayer {
                            depth: parent_depth + 1,
                            start_offset: r.start as u32,
                            end_offset: r.end as u32,
                            language: inner_lang.clone(),
                            tree: inner_tree,
                        });
                        queue.push_back(new_layers.len() - 1);
                    }
                }
            }
        }

        if let Some(filter) = injection_filter_ranges.filter(|ranges| !ranges.is_empty()) {
            carry_unvisited_injections(
                &mut new_layers,
                &prior_injections,
                filter,
                &mut layer_changes,
            );
        }

        layer_changes.sort_unstable_by_key(|r| r.start);
        merge_ranges(&mut layer_changes);

        self.install_layers(new_layers, version);
        Some(layer_changes)
    }
}

/// The host ranges one injection walk found for a single inner language.
///
/// Collected before any of them is parsed, because a combined injection's
/// ranges are one document and the parse cannot start until the last of them
/// is in hand.
struct InjectionGroup {
    language: Arc<Language>,
    /// How these ranges divide into documents, from
    /// [`LanguageInjection::grouping`].
    grouping: InjectionGrouping,
    ranges: Vec<HostRange>,
}

/// One host node's byte range, with the rows it spans.
///
/// The rows ride along because [`InjectionGrouping::CombinedBlocks`] cuts its
/// documents at a break in them, and a node's position is readable only while
/// the node itself is in hand.
struct HostRange {
    range: Range<usize>,
    start_row: usize,
    end_row: usize,
}

/// Cut `ranges` into runs of host nodes on consecutive rows, dropping the empty
/// ranges within each run.
///
/// A run is one document for [`InjectionGrouping::CombinedBlocks`]. Sorting
/// first is what makes the cut independent of the order several filter ranges
/// delivered their matches in.
///
/// A run whose ranges are all empty yields nothing, since there is no text to
/// parse. A bare `///` line between two written ones still joins them, because
/// the empty range drops only after its row has held the run together.
fn block_ranges(mut ranges: Vec<HostRange>) -> Vec<Vec<Range<usize>>> {
    ranges.sort_by_key(|host| (host.start_row, host.range.start));

    let mut blocks: Vec<Vec<Range<usize>>> = Vec::new();
    let mut previous_end_row: Option<usize> = None;
    for host in ranges {
        let continues = previous_end_row.is_some_and(|end| host.start_row <= end + 1);
        if !continues {
            blocks.push(Vec::new());
        }
        previous_end_row = Some(host.end_row);
        if !host.range.is_empty() {
            blocks
                .last_mut()
                .expect("a block was pushed for the first range")
                .push(host.range);
        }
    }

    blocks.retain(|block| !block.is_empty());
    blocks
}

/// Per-host injection layer from the previous parse.
///
/// A reparse reuses prior work two ways. It hands the tree to tree-sitter
/// as `old_tree` when the same host range reappears in this parse, and it
/// keeps the whole layer when a filtered walk never visits the region and
/// so cannot re-find it.
#[derive(Clone)]
struct PriorInjection {
    depth: u32,
    start_offset: u32,
    end_offset: u32,
    language: Arc<Language>,
    /// The layer's included ranges, read off the tree once here rather than
    /// once a lookup.
    ///
    /// [`Tree::included_ranges`] builds a fresh list a call, and three sites ask
    /// a prior for these: the combined carry's splice, the walked test in
    /// [`carry_unvisited_injections`], and the reuse guard in
    /// [`parse_rope_combined_ranges`]. Each prior is asked once a host range the
    /// walk rediscovers, so the list is built once a layer instead.
    ///
    /// Held as [`tree_sitter::Range`] rather than byte pairs because the reuse
    /// guard compares against a freshly built list of those, points included. A
    /// byte-only comparison there accepts a prior whose points have drifted.
    host_ranges: Vec<tree_sitter::Range>,
    tree: Tree,
}

/// Injection layers grouped by the language and depth they sit at, so a lookup
/// answers from one group rather than from every layer in the file.
///
/// Within a group the layers are disjoint, so ordering them by start also
/// orders them by end and the ones overlapping any range form a contiguous
/// run. Both facts are what make [`Self::overlapping`] a search rather than a
/// scan, and a markdown file's hundreds of fences make that the difference
/// between a lookup and a walk of the file per lookup.
///
/// The disjointness holds because a language's injections agree on one
/// [`InjectionGrouping`]. Per-node gives each host node its own layer, and host
/// nodes do not overlap. Combined gives one layer covering every host range.
/// Blocks give one layer per run of host nodes on consecutive rows, and a run
/// ends where the next starts, so the runs are disjoint and start-ordered too.
struct InjectionIndex<'a, T> {
    groups: HashMap<(u32, &'static str), Vec<&'a T>>,
}

impl<'a, T> InjectionIndex<'a, T> {
    /// Index `items` by the `(depth, language name, start, end)` each reports.
    fn new(items: impl IntoIterator<Item = &'a T>, key: impl Fn(&T) -> IndexKey) -> Self {
        let mut groups: HashMap<(u32, &'static str), Vec<&'a T>> = HashMap::new();
        for item in items {
            let k = key(item);
            groups.entry((k.depth, k.language)).or_default().push(item);
        }
        for group in groups.values_mut() {
            group.sort_by_key(|item| key(item).span.start);

            debug_assert!(
                group
                    .windows(2)
                    .all(|w| key(w[0]).span.end <= key(w[1]).span.start),
                "layers at one depth and language must not overlap, or ordering \
                 by start would not order by end and the search below would \
                 miss the first overlapping layer",
            );
        }
        Self { groups }
    }

    /// The layers at `depth` in `language` whose span overlaps `span`, in start
    /// order.
    fn overlapping(
        &self,
        depth: u32,
        language: &'static str,
        span: Range<usize>,
        key: impl Fn(&T) -> IndexKey,
    ) -> impl Iterator<Item = &'a T> {
        let group = self.groups.get(&(depth, language));
        let start = group.map_or(0, |group| {
            group.partition_point(|item| key(item).span.end <= span.start)
        });

        group
            .into_iter()
            .flat_map(move |group| group[start..].iter().copied())
            .take_while(move |item| key(item).span.start < span.end)
    }
}

/// What [`InjectionIndex`] reads off a layer to place and find it.
struct IndexKey {
    depth: u32,
    language: &'static str,
    span: Range<usize>,
}

/// Note where a freshly parsed injection layer could have restyled text.
///
/// A layer whose prior tree was reused only differs from it where the two trees
/// do. Without one the layer is new text, or text whose bounds moved, and the
/// whole span is suspect.
///
/// The tree diff says nothing about text the layer used to cover and no longer
/// does. A caller passing a prior whose bounds may differ from `span` owes that
/// half of the answer separately.
fn record_layer_change(
    changes: &mut Vec<Range<usize>>,
    span: &Range<usize>,
    prior: Option<&Tree>,
    parsed: &Tree,
) {
    match prior {
        Some(prior) => changes.extend(
            prior
                .changed_ranges(parsed)
                .map(|r| r.start_byte..r.end_byte),
        ),
        None => changes.push(span.clone()),
    }
}

/// The byte ranges a filtered walk queries for injections, given what changed.
///
/// Each changed range grows by a row on each side, which catches an injection
/// boundary flipping. Uncommenting a line whose neighbour opened a fenced block
/// changes what that neighbour is, and the walk has to see it.
///
/// Growing pushes the ranges into each other, so the set comes back merged. Two
/// edits two rows apart reach the same rows, and the walk runs its injection
/// query once a filter, so an overlap left in the set is queried twice a layer
/// for one answer.
fn expand_changed_rows(rope: &Rope, ranges: &[Range<usize>]) -> Vec<Range<usize>> {
    let mut expanded: Vec<Range<usize>> = ranges
        .iter()
        .map(|r| {
            let start_point = rope.offset_to_point(r.start);
            let end_point = rope.offset_to_point(r.end);
            let start_row = start_point.row.saturating_sub(1);
            let end_row = end_point.row.saturating_add(2);
            let start_byte = rope
                .point_to_offset(stoat_text::Point::new(start_row, 0))
                .min(rope.len());
            let end_byte = rope
                .point_to_offset(stoat_text::Point::new(end_row, 0))
                .min(rope.len());
            start_byte..end_byte
        })
        .collect();
    expanded.sort_by_key(|r| r.start);
    merge_ranges(&mut expanded);
    expanded
}

/// Collapse a start-sorted range list in place so no two entries overlap or
/// touch.
fn merge_ranges(ranges: &mut Vec<Range<usize>>) {
    let mut write = 0;
    for read in 0..ranges.len() {
        if write > 0 && ranges[read].start <= ranges[write - 1].end {
            ranges[write - 1].end = ranges[write - 1].end.max(ranges[read].end);
            continue;
        }
        ranges[write] = ranges[read].clone();
        write += 1;
    }
    ranges.truncate(write);
}

/// Re-add the prior injection layers a filtered walk could not have found.
///
/// A filtered reparse only queries host nodes intersecting `filter`, so it
/// re-discovers the injections near the edit and nothing else. Left alone,
/// every other injection would disappear from the layer set and its text
/// would lose all highlighting until the next unfiltered parse. Prior
/// layers outside `filter` are therefore still live and are restored here.
///
/// A prior layer intersecting `filter` is deliberately not restored. That
/// region *was* re-walked, so the walk's answer is authoritative and its
/// absence means the injection is gone (a deleted code fence, a changed
/// info string). Restoring it would resurrect highlighting for text that
/// no longer holds that language.
///
/// What each dropped layer contributes to `layer_changes` differs by why it was
/// dropped. A layer a fresh one supersedes reports only where the two extents
/// disagree, since [`record_layer_change`] has already diffed the trees over the
/// extent they share, and that diff is blind only to text the layer no longer
/// covers. A layer with no successor is gone outright and reports its whole
/// span, which nothing else names.
///
/// The distinction is what keeps a keystroke cheap. A combined injection spans
/// its first host node to its last, so reporting that whole span would re-query
/// most of a markdown file, or everything between the outermost doc comments of
/// a Rust one, for a one-character edit. In steady-state typing the two extents
/// agree, because [`SyntaxMap::interpolate`] has already carried the prior
/// bounds onto the fresh ones, so a superseded layer reports nothing at all.
///
/// `prior` bounds must already be in `rope` coordinates, which
/// [`SyntaxMap::interpolate`] is responsible for.
fn carry_unvisited_injections(
    layers: &mut Vec<SyntaxLayer>,
    prior: &[PriorInjection],
    filter: &[Range<usize>],
    layer_changes: &mut Vec<Range<usize>>,
) {
    let overlaps = |a: Range<usize>, b: &Range<usize>| a.start < b.end && b.start < a.end;

    // Indexed once rather than scanned per prior layer. A markdown file with
    // hundreds of fences has as many layers on both sides, and the fold below
    // asks after every one of them.
    let fresh_key = |l: &SyntaxLayer| IndexKey {
        depth: l.depth,
        language: l.language.name,
        span: l.start_offset as usize..l.end_offset as usize,
    };
    let fresh_index = InjectionIndex::new(layers.iter(), fresh_key);

    let mut carried: Vec<SyntaxLayer> = Vec::new();
    for layer in prior {
        let span = layer.start_offset as usize..layer.end_offset as usize;

        // An empty span is a layer whose text the edit replaced outright,
        // collapsed by [`SyntaxMap::interpolate`]. It covers nothing to
        // highlight, and it would never intersect `filter`, so it has to be
        // dropped here rather than by the intersection test below.
        if span.is_empty() {
            continue;
        }

        // A combined injection merges several host ranges into one layer
        // spanning all of them, so a fresh layer can cover a prior one at a
        // different offset. Matching on overlap rather than equality keeps
        // that from producing two layers over the same text.
        let superseding = fresh_index
            .overlapping(layer.depth, layer.language.name, span.clone(), fresh_key)
            .fold(None::<Range<usize>>, |acc, l| {
                let extent = l.start_offset as usize..l.end_offset as usize;
                Some(match acc {
                    Some(a) => a.start.min(extent.start)..a.end.max(extent.end),
                    None => extent,
                })
            });

        if let Some(fresh) = superseding {
            for gap in [
                span.start.min(fresh.start)..span.start.max(fresh.start),
                span.end.min(fresh.end)..span.end.max(fresh.end),
            ] {
                if !gap.is_empty() {
                    layer_changes.push(gap);
                }
            }
            continue;
        }

        // A combined layer's extent runs from its first host node to its last,
        // with text between them that it does not cover. Testing the extent
        // would call it re-walked for an edit in one of those gaps, which the
        // walk never queried, so the host ranges themselves are what decide.
        // For a single-range layer they clamp back to the extent.
        let host_ranges: Vec<Range<usize>> = layer
            .host_ranges
            .iter()
            .map(|r| r.start_byte.max(span.start)..r.end_byte.min(span.end))
            .filter(|r| !r.is_empty())
            .collect();
        let walked = if host_ranges.is_empty() {
            filter.iter().any(|r| overlaps(span.clone(), r))
        } else {
            host_ranges
                .iter()
                .any(|h| filter.iter().any(|r| overlaps(h.clone(), r)))
        };
        if walked {
            layer_changes.push(span);
            continue;
        }

        // Held back rather than pushed, because the index above borrows
        // `layers` for the whole loop. Carried layers cannot supersede one
        // another anyway, so nothing later in the loop would have seen them.
        carried.push(SyntaxLayer {
            depth: layer.depth,
            start_offset: layer.start_offset,
            end_offset: layer.end_offset,
            language: layer.language.clone(),
            tree: layer.tree.clone(),
        });
    }

    drop(fresh_index);
    layers.append(&mut carried);
}

/// Map a byte offset from pre-edit into post-edit coordinates.
///
/// `edits` must be sorted by `old.start` and non-overlapping, the shape
/// [`stoat_text::patch::Patch`] maintains. An offset inside a replaced
/// region collapses onto that region's new start, matching
/// [`stoat_text::patch::Patch::old_to_new`] so carried offsets and patch
/// arithmetic elsewhere agree.
fn translate_offset(edits: &[PatchEdit<usize>], offset: usize) -> usize {
    let ix = match edits.binary_search_by(|probe| probe.old.start.cmp(&offset)) {
        Ok(ix) => ix,
        Err(0) => return offset,
        Err(ix) => ix - 1,
    };
    match edits.get(ix) {
        Some(edit) if offset >= edit.old.end => edit.new.end + (offset - edit.old.end),
        Some(edit) => edit.new.start,
        None => offset,
    }
}

/// Parse `rope` restricted to a list of byte ranges via
/// [`tree_sitter::Parser::set_included_ranges`]. The returned tree's
/// nodes carry rope-absolute byte offsets and span all the included
/// ranges as one logical document, which is how Zed handles "combined
/// injections" like multiple Rust code fences in a Markdown buffer.
fn parse_rope_combined_ranges(
    language: &Language,
    rope: &Rope,
    ranges: &[Range<usize>],
    prior: Option<&PriorInjection>,
    deadline: Option<(Instant, &Executor)>,
) -> Option<Tree> {
    if ranges.is_empty() {
        return None;
    }
    let ts_ranges: Vec<tree_sitter::Range> = ranges
        .iter()
        .map(|r| {
            let start_point = stoat_to_ts(rope.offset_to_point(r.start));
            let end_point = stoat_to_ts(rope.offset_to_point(r.end));
            tree_sitter::Range {
                start_byte: r.start,
                end_byte: r.end,
                start_point,
                end_point,
            }
        })
        .collect();
    // A combined tree is only reused when it was built over the very ranges
    // being asked for again. An edit inside an existing host range keeps the set
    // equal, because `SyntaxMap::interpolate` shifts the prior tree's ranges
    // exactly as the fresh query shifts the discovered ones, and that is the
    // case worth reusing for.
    //
    // Reuse across a moved set is not sound here, however cheap it would make
    // typing a new doc comment. A range set changes when a host node is added
    // or dropped, and no `Tree::edit` describes that, so tree-sitter is left
    // comparing the two range lists and reusing whatever subtree its external
    // scanner state matches at. For the markdown block grammar that state does
    // not distinguish the two layouts, and the reused blocks are the ones a
    // parse over the same ranges would not have produced.
    //
    // Splicing the ranges is not the missing piece. The walk above already
    // hands this the same set a from-scratch parse discovers, and dropping the
    // guard still lands a combined tree holding three paragraphs where a parse
    // over those very ranges holds two. That is what
    // `app::tests::a_carried_parse_tracks_a_fresh_parse_across_doc_comments`
    // catches, and it is the only fixture that does, doc comments being the
    // one combined injection left.
    //
    // The prior's own list is read off the record rather than off the tree,
    // which builds a fresh one a call.
    let old_tree = prior
        .filter(|p| p.host_ranges == ts_ranges)
        .map(|p| &p.tree);
    parse_rope_inner(language, rope, old_tree, Some(&ts_ranges), deadline)
}

fn stoat_to_ts(p: stoat_text::Point) -> tree_sitter::Point {
    tree_sitter::Point {
        row: p.row as usize,
        column: p.column as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{edit_tree, parse_rope, LanguageRegistry};
    use std::time::Duration;
    use stoat_scheduler::TestScheduler;

    fn rust_lang() -> Arc<Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .unwrap()
    }

    fn markdown_lang() -> Arc<Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.md"))
            .unwrap()
    }

    fn parse_rust(text: &str) -> Tree {
        let lang = rust_lang();
        crate::parse(&lang, text, None).expect("rust parse must succeed")
    }

    #[test]
    fn empty_snapshot() {
        let map = SyntaxMap::new();
        assert!(map.snapshot().is_empty());
        assert_eq!(map.snapshot().layer_count(), 0);
    }

    #[test]
    fn install_single_root_layer() {
        let lang = rust_lang();
        let tree = parse_rust("fn main() {}");
        let mut map = SyntaxMap::new();
        let layer = SyntaxLayer {
            depth: 0,
            start_offset: 0,
            end_offset: 12,
            language: lang.clone(),
            tree,
        };
        map.install_layers([layer], 1);
        assert_eq!(map.snapshot().layer_count(), 1);
        let first = map.snapshot().iter_layers().next().unwrap();
        assert_eq!(first.depth, 0);
        assert_eq!(first.language.name, "rust");
        assert_eq!(map.snapshot().parsed_version, 1);
    }

    #[test]
    fn reparse_installs_root_layer_for_empty_map() {
        let lang = rust_lang();
        let rope = Rope::from("fn main() {}");
        let mut map = SyntaxMap::new();
        assert!(map.reparse(&rope, lang.clone(), 1, None, None).is_some());

        assert_eq!(map.snapshot().layer_count(), 1);
        let root = map.snapshot().iter_layers().next().unwrap();
        assert_eq!(root.depth, 0);
        assert_eq!(root.start_offset, 0);
        assert_eq!(root.end_offset, rope.len() as u32);
        assert_eq!(root.language.name, "rust");
        assert_eq!(map.snapshot().parsed_version, 1);
    }

    #[test]
    fn reparse_reuses_prior_tree_when_available() {
        // After interpolate has positioned the prior tree against the
        // new rope, reparse can hand it to tree-sitter as `old_tree`
        // and reuse unchanged subtrees. The resulting tree must
        // reflect the new byte range and the layer's `end_offset` must
        // be updated.
        use stoat_text::patch::Edit as PatchEdit;
        let lang = rust_lang();
        let rope1 = Rope::from("fn main() {}");
        let mut map = SyntaxMap::new();
        map.reparse(&rope1, lang.clone(), 1, None, None).unwrap();
        assert_eq!(
            map.snapshot()
                .iter_layers()
                .next()
                .unwrap()
                .tree
                .root_node()
                .byte_range(),
            0..rope1.len()
        );

        // Insert " let x = 1;" before the closing brace.
        let original = "fn main() {}";
        let insert_pos = 11; // before final '}'
        let inserted = " let x = 1;";
        let mut new_text = String::new();
        new_text.push_str(&original[..insert_pos]);
        new_text.push_str(inserted);
        new_text.push_str(&original[insert_pos..]);
        let rope2 = Rope::from(new_text.as_str());
        let edits = vec![PatchEdit {
            old: insert_pos..insert_pos,
            new: insert_pos..(insert_pos + inserted.len()),
        }];

        map.interpolate(&edits, &rope1, &rope2);
        map.reparse(&rope2, lang.clone(), 2, None, None).unwrap();

        let layer = map.snapshot().iter_layers().next().unwrap();
        assert_eq!(layer.tree.root_node().byte_range(), 0..rope2.len());
        assert_eq!(layer.end_offset, rope2.len() as u32);
        assert_eq!(map.snapshot().layer_count(), 1);
        assert_eq!(map.snapshot().parsed_version, 2);
    }

    #[test]
    fn interpolate_then_reparse_matches_full_parse() {
        use stoat_text::patch::Edit as PatchEdit;
        let lang = rust_lang();
        let original = "fn main() { let x = 1; }";
        let old_rope = Rope::from(original);
        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();

        let insert_pos = 23;
        let inserted = "let y = 2; ";
        let mut new_text = String::new();
        new_text.push_str(&original[..insert_pos]);
        new_text.push_str(inserted);
        new_text.push_str(&original[insert_pos..]);
        let new_rope = Rope::from(new_text.as_str());

        let edits = vec![PatchEdit {
            old: insert_pos..insert_pos,
            new: insert_pos..(insert_pos + inserted.len()),
        }];
        map.interpolate(&edits, &old_rope, &new_rope);
        map.reparse(&new_rope, lang.clone(), 2, None, None).unwrap();

        let incremental = map
            .snapshot()
            .iter_layers()
            .next()
            .unwrap()
            .tree
            .root_node()
            .to_sexp();

        // Equivalence check: a fresh map parsing the new rope from
        // scratch must produce the same tree.
        let mut fresh = SyntaxMap::new();
        fresh.reparse(&new_rope, lang, 2, None, None).unwrap();
        let fresh_root = fresh
            .snapshot()
            .iter_layers()
            .next()
            .unwrap()
            .tree
            .root_node()
            .to_sexp();

        assert_eq!(incremental, fresh_root);
    }

    #[test]
    fn interpolate_then_reparse_matches_full_parse_over_many_fences() {
        // The edit lands in the last fence, so every layer above it is left
        // untouched by interpolate rather than replayed. Getting that skip
        // wrong shows up as a layer whose tree or bounds no longer describe the
        // text under it, which the comparison below catches.
        use stoat_text::patch::Edit as PatchEdit;
        let lang = markdown_lang();
        let original: String = (0..12)
            .map(|i| {
                format!("## Section {i}\n\nprose {i}\n\n```rust\nfn f{i}() {{ {i} }}\n```\n\n")
            })
            .collect();
        let old_rope = Rope::from(original.as_str());

        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();
        let layers = |map: &SyntaxMap| -> Vec<(u32, u32, u32, &'static str, String)> {
            map.snapshot()
                .iter_layers()
                .map(|l| {
                    (
                        l.depth,
                        l.start_offset,
                        l.end_offset,
                        l.language.name,
                        l.tree.root_node().to_sexp(),
                    )
                })
                .collect()
        };
        assert!(
            layers(&map).len() > 3,
            "the fixture must inject many layers, got {:?}",
            layers(&map).len()
        );

        let at = original.rfind("fn f11").expect("fixture has a last fence") + "fn f11".len();
        let inserted = "oo";
        let mut text = String::from(&original[..at]);
        text.push_str(inserted);
        text.push_str(&original[at..]);
        let new_rope = Rope::from(text.as_str());
        let edits = vec![PatchEdit {
            old: at..at,
            new: at..(at + inserted.len()),
        }];

        map.interpolate(&edits, &old_rope, &new_rope);
        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![at..(at + inserted.len())];
        map.reparse_within_changed_ranges(&new_rope, lang.clone(), 2, Some(&changed), None, None)
            .unwrap();

        let mut fresh = SyntaxMap::new();
        fresh.reparse(&new_rope, lang, 2, None, None).unwrap();
        assert_eq!(layers(&map), layers(&fresh));
    }

    /// A filtered reparse of a combined layer lands on the same layer set a
    /// from-scratch parse does.
    ///
    /// The walk queries only the edited region, so it re-finds one of the
    /// layer's host nodes and recovers the rest from the prior tree. Getting
    /// that splice wrong shows up either as a layer built over too few ranges,
    /// which the s-expression comparison catches, or as one whose bounds no
    /// longer describe the text under it.
    #[test]
    fn a_filtered_reparse_of_a_combined_layer_matches_a_full_parse() {
        use stoat_text::patch::Edit as PatchEdit;
        let lang = rust_lang();
        // Several doc lines a function, so one block really is several host
        // ranges. One line each gives one range a block, which is the per-node
        // case rather than the splice this exercises.
        let original: String = (0..8)
            .map(|i| {
                format!(
                    "/// doc {i} with **bold** text\n/// second line\n/// third line\n\
                     /// fourth line\n/// fifth line\n/// sixth line\nfn f{i}() {{ {i} }}\n\n"
                )
            })
            .collect();
        let old_rope = Rope::from(original.as_str());

        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();
        let layers = |map: &SyntaxMap| -> Vec<(u32, u32, u32, &'static str, String)> {
            map.snapshot()
                .iter_layers()
                .map(|l| {
                    (
                        l.depth,
                        l.start_offset,
                        l.end_offset,
                        l.language.name,
                        l.tree.root_node().to_sexp(),
                    )
                })
                .collect()
        };
        let combined_ranges = map
            .snapshot()
            .iter_layers()
            .filter(|l| l.depth == 1 && l.language.name == "markdown")
            .map(|l| l.tree.included_ranges().len())
            .max()
            .expect("a depth-1 markdown layer");
        assert!(
            combined_ranges > 4,
            "the fixture must merge many host ranges into one layer, got {combined_ranges}",
        );

        // An edit in the middle doc comment, so the splice has untouched ranges
        // to recover on both sides of the filter.
        let at = original.find("doc 4").expect("fixture has doc 4") + "doc ".len();
        let inserted = "number ";
        let mut text = String::from(&original[..at]);
        text.push_str(inserted);
        text.push_str(&original[at..]);
        let new_rope = Rope::from(text.as_str());

        map.interpolate(
            &[PatchEdit {
                old: at..at,
                new: at..(at + inserted.len()),
            }],
            &old_rope,
            &new_rope,
        );
        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![at..(at + inserted.len())];
        map.reparse_within_changed_ranges(&new_rope, lang.clone(), 2, Some(&changed), None, None)
            .unwrap();

        let mut fresh = SyntaxMap::new();
        fresh.reparse(&new_rope, lang, 2, None, None).unwrap();
        assert_eq!(layers(&map), layers(&fresh));
    }

    /// Typing a new host node into a combined layer still lands on the layer set
    /// a from-scratch parse produces.
    ///
    /// Adding or dropping a host node moves the layer's range set, which is the
    /// case the splice has to rebuild rather than carry, and the case
    /// `parse_rope_combined_ranges` declines to reuse a prior tree across. The
    /// steps run in sequence so each starts from the tree the last one left,
    /// which is where a layer whose ranges had drifted from its text would
    /// compound instead of washing out.
    #[test]
    fn a_new_host_node_typed_into_a_combined_layer_matches_a_full_parse() {
        use stoat_text::patch::Edit as PatchEdit;
        let lang = rust_lang();
        // Several doc lines a function, so the block a line joins is already
        // several host ranges rather than the one a per-node layer holds.
        let original: String = (0..6)
            .map(|i| {
                format!(
                    "/// doc {i} with **bold** text\n/// second line\n/// third line\n\
                     fn f{i}() {{ {i} }}\n\n"
                )
            })
            .collect();
        let old_rope = Rope::from(original.as_str());

        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();
        // Summed across the layers, because a doc line joins the block it sits
        // against and the count the assertion below reads is the file's.
        let ranges_of = |map: &SyntaxMap| -> usize {
            map.snapshot()
                .iter_layers()
                .filter(|l| l.depth == 1 && l.language.name == "markdown")
                .map(|l| l.tree.included_ranges().len())
                .sum()
        };
        let before = ranges_of(&map);

        let layers = |map: &SyntaxMap| -> Vec<(u32, u32, u32, &'static str, String)> {
            map.snapshot()
                .iter_layers()
                .map(|l| {
                    (
                        l.depth,
                        l.start_offset,
                        l.end_offset,
                        l.language.name,
                        l.tree.root_node().to_sexp(),
                    )
                })
                .collect()
        };

        // Whole doc comments rather than text inside one, so the layer gains and
        // loses host ranges rather than merely growing them. Each step reuses the
        // tree the previous step left, so a carried tree that had drifted from
        // the text would compound rather than wash out.
        let added = "/// a brand new *doc* line\n";
        let steps: Vec<(&str, Range<usize>, &str)> = vec![
            ("add before f3", 0..0, added),
            ("add before f1", 0..0, added),
            ("remove the first", 0..added.len(), ""),
        ];

        let mut rope = old_rope;
        let mut source = original.clone();
        let mut version = 1;
        for (step, _, replacement) in steps {
            let anchor = match step {
                "add before f3" => source.find("fn f3").expect("fixture has f3"),
                "add before f1" => source.find("fn f1").expect("fixture has f1"),
                _ => source
                    .find("/// a brand new")
                    .expect("an added line to drop"),
            };
            let removed = if replacement.is_empty() {
                added.len()
            } else {
                0
            };

            let mut text = String::from(&source[..anchor]);
            text.push_str(replacement);
            text.push_str(&source[anchor + removed..]);
            let new_rope = Rope::from(text.as_str());

            map.interpolate(
                &[PatchEdit {
                    old: anchor..(anchor + removed),
                    new: anchor..(anchor + replacement.len()),
                }],
                &rope,
                &new_rope,
            );
            version += 1;
            #[allow(clippy::single_range_in_vec_init)]
            let changed = vec![anchor..(anchor + replacement.len().max(1))];
            map.reparse_within_changed_ranges(
                &new_rope,
                lang.clone(),
                version,
                Some(&changed),
                None,
                None,
            )
            .unwrap();

            let mut fresh = SyntaxMap::new();
            fresh
                .reparse(&new_rope, lang.clone(), version, None, None)
                .unwrap();
            assert_eq!(layers(&map), layers(&fresh), "after {step}");

            source = text;
            rope = new_rope;
        }

        assert_eq!(
            ranges_of(&map),
            before + 1,
            "two doc lines added and one removed must leave one more host range",
        );
    }

    #[test]
    fn layers_iterate_in_depth_then_offset_order() {
        // Three layers: root at depth 0, two injections at depth 1
        // (out-of-order start offsets to verify the SumTree sorts).
        let lang = rust_lang();
        let mut map = SyntaxMap::new();
        let mk = |depth: u32, start: u32, end: u32| SyntaxLayer {
            depth,
            start_offset: start,
            end_offset: end,
            language: lang.clone(),
            tree: parse_rust(""),
        };
        map.install_layers(
            [
                mk(1, 50, 80), // injection later in document
                mk(0, 0, 100), // root
                mk(1, 10, 30), // injection earlier in document
            ],
            1,
        );
        let order: Vec<(u32, u32)> = map
            .snapshot()
            .iter_layers()
            .map(|l| (l.depth, l.start_offset))
            .collect();
        assert_eq!(order, vec![(0, 0), (1, 10), (1, 50)]);
    }

    #[test]
    fn reparse_markdown_produces_inline_injection_layer() {
        // Markdown with inline content should produce a depth-0
        // markdown root layer plus one or more depth-1 markdown-inline
        // layers covering the inline byte ranges.
        let lang = markdown_lang();
        let source = "# Title\n\nSome **bold** prose with `code`.\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1, None, None).unwrap();

        let layers: Vec<(u32, u32, &str)> = map
            .snapshot()
            .iter_layers()
            .map(|l| (l.depth, l.start_offset, l.language.name))
            .collect();

        // Root layer is the markdown grammar at depth 0.
        assert!(layers.iter().any(|&(d, _, n)| d == 0 && n == "markdown"));
        // At least one depth-1 markdown-inline layer for the inline node.
        assert!(
            layers
                .iter()
                .any(|&(d, _, n)| d == 1 && n == "markdown-inline"),
            "expected a depth-1 markdown-inline layer, got {layers:?}"
        );
    }

    #[test]
    fn reparse_rust_produces_no_injection_layers() {
        // Rust injects markdown into `doc_comment` nodes, but this rope has no
        // doc comment, so reparse stays at a single root layer.
        let lang = rust_lang();
        let rope = Rope::from("fn main() { let x = 1; }");
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1, None, None).unwrap();
        assert_eq!(map.snapshot().layer_count(), 1);
        assert_eq!(map.snapshot().iter_layers().next().unwrap().depth, 0);
    }

    #[test]
    fn reparse_rust_doc_comment_injects_markdown() {
        // A rust doc comment hosts a combined markdown injection, so `**bold**`
        // and a link inside `///` produce a depth-1 markdown layer over the
        // doc_comment range and a depth-2 markdown-inline layer inside it.
        let lang = rust_lang();
        let source = "/// **bold** [text](url)\nfn a() {}\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1, None, None).unwrap();

        let layers: Vec<(u32, &str)> = map
            .snapshot()
            .iter_layers()
            .map(|l| (l.depth, l.language.name))
            .collect();
        assert!(
            layers.iter().any(|&(d, n)| d == 0 && n == "rust"),
            "root rust layer, got {layers:?}"
        );
        assert!(
            layers.iter().any(|&(d, n)| d == 1 && n == "markdown"),
            "depth-1 markdown layer, got {layers:?}"
        );
        assert!(
            layers
                .iter()
                .any(|&(d, n)| d == 2 && n == "markdown-inline"),
            "depth-2 markdown-inline layer, got {layers:?}"
        );

        // The `**bold**` markup is captured by the markdown-inline layer inside
        // the doc comment.
        let bold_start = source.find("**bold**").unwrap();
        let bold_end = bold_start + "**bold**".len();
        let captures = map
            .snapshot()
            .captures(0..rope.len(), &rope, |l| Some(&l.highlight_query));
        assert!(
            captures.iter().any(|c| {
                let r = c.node.byte_range();
                r.start >= bold_start
                    && r.end <= bold_end
                    && c.language
                        .highlight_capture_names()
                        .get(c.index as usize)
                        .is_some_and(|n| n.contains("emphasis"))
            }),
            "expected an emphasis capture inside the doc comment's **bold**"
        );
    }

    #[test]
    fn reparse_markdown_fence_injects_named_language() {
        // A rust fence in a markdown buffer parses as rust. The SyntaxMap gains
        // a depth-1 rust layer over the fence content and captures its keyword.
        let lang = markdown_lang();
        let source = "```rust\nfn a() {}\n```\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1, None, None).unwrap();

        let layers: Vec<(u32, &str)> = map
            .snapshot()
            .iter_layers()
            .map(|l| (l.depth, l.language.name))
            .collect();
        assert!(
            layers.iter().any(|&(d, n)| d == 1 && n == "rust"),
            "depth-1 rust layer over the fence, got {layers:?}"
        );

        let fn_start = source.find("fn").unwrap();
        let captures = map
            .snapshot()
            .captures(0..rope.len(), &rope, |l| Some(&l.highlight_query));
        assert!(
            captures.iter().any(|c| {
                c.language.name == "rust"
                    && c.node.byte_range().start == fn_start
                    && c.language
                        .highlight_capture_names()
                        .get(c.index as usize)
                        .is_some_and(|n| n.contains("keyword"))
            }),
            "expected a rust keyword capture at the fence's fn"
        );
    }

    #[test]
    fn reparse_markdown_unknown_fence_token_adds_no_layer() {
        // A fence naming no registered language stays literal. Only markdown
        // layers appear, with no injected code language.
        let lang = markdown_lang();
        let rope = Rope::from("```cobol\nMOVE X TO Y\n```\n");
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1, None, None).unwrap();

        assert!(
            map.snapshot()
                .iter_layers()
                .all(|l| l.language.name.starts_with("markdown")),
            "unknown fence token must not inject a code language, got {:?}",
            map.snapshot()
                .iter_layers()
                .map(|l| (l.depth, l.language.name))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn reparse_rust_doc_comment_fence_nests_three_layers() {
        // A rust fence inside a rust doc comment's markdown nests three layers:
        // rust (0) -> markdown (1) -> rust (2).
        let lang = rust_lang();
        let source = "/// ```rust\n/// fn b() {}\n/// ```\nfn a() {}\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1, None, None).unwrap();

        let layers: Vec<(u32, &str)> = map
            .snapshot()
            .iter_layers()
            .map(|l| (l.depth, l.language.name))
            .collect();
        assert!(
            layers.iter().any(|&(d, n)| d == 0 && n == "rust"),
            "depth-0 rust, got {layers:?}"
        );
        assert!(
            layers.iter().any(|&(d, n)| d == 1 && n == "markdown"),
            "depth-1 markdown, got {layers:?}"
        );
        assert!(
            layers.iter().any(|&(d, n)| d == 2 && n == "rust"),
            "depth-2 rust fence, got {layers:?}"
        );
    }

    #[test]
    fn captures_merge_across_layers_in_document_order() {
        // Rust doc comments rather than a markdown file, because the layers
        // have to interleave for the ordering to mean anything. Layers are
        // walked deepest last, so where the deeper layer's text simply follows
        // the shallower one's, a list that was never sorted comes out ordered
        // anyway. Here the markdown starts inside the rust node holding it,
        // and each comment's injected captures fall between the next comment's
        // rust ones.
        let lang = rust_lang();
        let source = "/// one **bold**\nfn a() {}\n\
                      /// two **bold**\nfn b() {}\n\
                      /// three **bold**\nfn c() {}\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1, None, None).unwrap();

        let captures = map
            .snapshot()
            .captures(0..rope.len(), &rope, |l| Some(&l.highlight_query));

        assert!(
            !captures.is_empty(),
            "a buffer with injected content must produce captures"
        );

        // The whole sort key, since ties on start are what put a shallower
        // layer's enclosing node ahead of the text it encloses.
        let keys: Vec<(usize, Reverse<usize>, u32)> = captures
            .iter()
            .map(|c| {
                let r = c.node.byte_range();
                (r.start, Reverse(r.end), c.depth)
            })
            .collect();
        let mut ordered = keys.clone();
        ordered.sort();
        assert_eq!(keys, ordered, "captures must come back in document order");

        // From every layer, not just the host: the rust root, the combined
        // markdown over the doc comments, and the inline markdown inside it.
        let depths: std::collections::HashSet<u32> = captures.iter().map(|c| c.depth).collect();
        assert!(depths.contains(&0), "expected at least one depth-0 capture");
        assert!(depths.contains(&1), "expected at least one depth-1 capture");
        assert!(depths.contains(&2), "expected at least one depth-2 capture");

        // The injected captures land on the markup rather than merely
        // somewhere inside the comment.
        let bold = {
            let start = source.find("**bold**").expect("fixture");
            start..start + "**bold**".len()
        };
        assert!(
            captures.iter().any(|c| {
                c.depth >= 2 && c.node.start_byte() >= bold.start && c.node.end_byte() <= bold.end
            }),
            "expected an inline capture inside the first **bold**"
        );
    }

    #[test]
    fn captures_respect_byte_range_filter() {
        // A range filter should exclude captures that fall entirely
        // outside the requested byte range. Test by querying the
        // first half of a markdown buffer.
        let lang = markdown_lang();
        let source = "# Title\n\nSome **bold** text\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1, None, None).unwrap();

        let half = source.len() / 2;
        let captures = map
            .snapshot()
            .captures(0..half, &rope, |l| Some(&l.highlight_query));

        // Every capture must overlap [0..half).
        for c in &captures {
            let r = c.node.byte_range();
            assert!(
                r.start < half,
                "capture at {:?} should not start past the requested range end {}",
                r,
                half
            );
        }
    }

    /// The range filter reaches the layers queried after the first one.
    ///
    /// One cursor serves every layer, so a layer that came later would go
    /// unfiltered if the range stopped applying partway through. A combined
    /// layer is what reaches that case. It intersects a range covering one of
    /// its host nodes while holding captures in all the others, and the root
    /// grammar's layer is queried ahead of it.
    ///
    /// Ordering is left to `captures_merge_across_layers_in_document_order`. A
    /// range this narrow returns captures that are already ordered without any
    /// sort, so asserting it here would pin nothing.
    #[test]
    fn a_range_filter_reaches_a_layer_queried_after_the_first() {
        let lang = rust_lang();
        let source = "/// one **bold**\nfn a() {}\n\
                      /// two **bold**\nfn b() {}\n\
                      /// three **bold**\nfn c() {}\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1, None, None).unwrap();
        assert!(
            map.snapshot()
                .iter_layers()
                .any(|l| l.depth == 1 && l.tree.included_ranges().len() > 1),
            "the fixture must merge the doc comments into one layer",
        );

        let first = source.find("one").expect("fixture")..source.find("\nfn a").expect("fixture");
        let captures = map
            .snapshot()
            .captures(first.clone(), &rope, |l| Some(&l.highlight_query));

        assert!(
            captures.iter().any(|c| c.depth == 1),
            "the first comment must return captures from the combined layer",
        );
        let strays: Vec<Range<usize>> = captures
            .iter()
            .map(|c| c.node.byte_range())
            .filter(|r| r.start >= first.end || r.end <= first.start)
            .collect();
        assert_eq!(
            strays,
            Vec::<Range<usize>>::new(),
            "the other two comments' captures must not come back for a range \
             covering only the first",
        );
    }

    #[test]
    fn reparse_markdown_reuses_inline_tree_when_host_range_unchanged() {
        // First reparse populates the inline injection layer; the
        // second reparse against the same rope must reuse that tree
        // as the prior. Easiest verification: layer count stays the
        // same and the inline tree's root node's byte range still
        // matches.
        let lang = markdown_lang();
        let source = "Some **bold** text\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang.clone(), 1, None, None).unwrap();
        let first_inline_range = map
            .snapshot()
            .iter_layers()
            .find(|l| l.depth == 1)
            .map(|l| (l.start_offset, l.end_offset));
        assert!(
            first_inline_range.is_some(),
            "first reparse should produce an inline layer"
        );

        map.reparse(&rope, lang, 2, None, None).unwrap();
        let second_inline_range = map
            .snapshot()
            .iter_layers()
            .find(|l| l.depth == 1)
            .map(|l| (l.start_offset, l.end_offset));
        assert_eq!(first_inline_range, second_inline_range);
        assert_eq!(map.snapshot().parsed_version, 2);
    }

    #[test]
    fn reparse_within_changed_ranges_filters_injection_query() {
        // Markdown buffer with two inline regions on separate lines.
        // A changed-range filter restricted to the first line should
        // discover only the first inline injection layer (the +/- 1
        // row expansion still keeps it bounded to the affected area).
        let lang = markdown_lang();
        let source = "Some **bold** text\nMore *italic* text\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        // Filter to bytes covering only the first line.
        let first_newline = source.find('\n').unwrap();
        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![0..first_newline];
        map.reparse_within_changed_ranges(&rope, lang.clone(), 1, Some(&changed), None, None)
            .unwrap();

        // Should still produce at least one depth-1 inline layer (the
        // +/- 1 row expansion brings in the surrounding line, which
        // is enough for the inline injection on the first line to be
        // discovered).
        let inline_layers: Vec<(u32, u32)> = map
            .snapshot()
            .iter_layers()
            .filter(|l| l.depth == 1)
            .map(|l| (l.start_offset, l.end_offset))
            .collect();
        assert!(
            !inline_layers.is_empty(),
            "filtered reparse should still produce at least one inline layer, got {inline_layers:?}"
        );
    }

    /// Expanding two edits a couple of rows apart pushes their ranges into each
    /// other, and the walk runs its injection query once a filter, so an
    /// overlap left in the set is queried twice a layer for one answer.
    ///
    /// Paired with [`Self::two_filters_over_one_paragraph_leave_one_layer`]
    /// below, which drives the same fixture through the walk and pins the layer
    /// set against a fresh parse. A merge that swallowed a range shows up
    /// there.
    #[test]
    fn two_edits_two_rows_apart_expand_into_one_filter() {
        let source = "alpha *a*\nbravo *b*\ncharlie *c*\ndelta *d*\necho *e*\n";
        let rope = Rope::from(source);

        // Rows 1 and 3. Each expansion reaches row 2 from its own side, so the
        // two meet there.
        let changed = vec![
            source.find("bravo").expect("fixture")..source.find("*b*").expect("fixture"),
            source.find("delta").expect("fixture")..source.find("*d*").expect("fixture"),
        ];

        let expanded = expand_changed_rows(&rope, &changed);

        assert_eq!(
            expanded.len(),
            1,
            "the two expansions meet, so the walk queries them once: {expanded:?}"
        );
        assert_eq!(
            expanded[0],
            0..source.len(),
            "and the one filter reaches from the row above the first edit to past the last"
        );
    }

    /// Two edits whose filters meet still leave one layer over the host node
    /// they share.
    ///
    /// The expansions merge into one filter, and the injection query runs once
    /// per filter, so the paragraph reaching into both is matched once. It was
    /// matched twice before the merge, and a match is what a layer is made
    /// from, which the per-range dedupe is what caught.
    #[test]
    fn two_filters_over_one_paragraph_leave_one_layer() {
        let lang = markdown_lang();
        // One paragraph, since markdown breaks inline content only at a blank
        // line. Its single `inline` node is what both filters reach.
        let source = "alpha *a*\nbravo *b*\ncharlie *c*\ndelta *d*\necho *e*\n";
        let rope = Rope::from(source);

        let changed = vec![
            source.find("bravo").expect("fixture")..source.find("*b*").expect("fixture"),
            source.find("delta").expect("fixture")..source.find("*d*").expect("fixture"),
        ];

        let mut map = SyntaxMap::new();
        map.reparse_within_changed_ranges(&rope, lang.clone(), 1, Some(&changed), None, None)
            .unwrap();

        let mut fresh = SyntaxMap::new();
        fresh.reparse(&rope, lang, 1, None, None).unwrap();
        let inline = |map: &SyntaxMap| -> Vec<(u32, u32)> {
            map.snapshot()
                .iter_layers()
                .filter(|l| l.language.name == "markdown-inline")
                .map(|l| (l.start_offset, l.end_offset))
                .collect()
        };
        assert_eq!(
            inline(&fresh).len(),
            1,
            "the fixture must be one paragraph, or neither filter shares a host node"
        );
        assert_eq!(
            inline(&map),
            inline(&fresh),
            "a host node found by two filters must not become two layers"
        );
    }

    #[test]
    fn filtered_reparse_keeps_injections_outside_the_changed_range() {
        // A filtered reparse re-walks only the edited region, so an
        // injection far from the edit is never rediscovered by the
        // query. It must be carried over from the prior layer set
        // instead of vanishing, or every keystroke would strip
        // highlighting from the rest of the file.
        let lang = markdown_lang();
        let source = "```rust\nfn a() {}\n```\n\ntail text\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang.clone(), 1, None, None).unwrap();
        assert!(
            map.snapshot()
                .iter_layers()
                .any(|l| l.depth == 1 && l.language.name == "rust"),
            "fixture must start with a rust fence layer"
        );

        let tail = source.find("tail").unwrap();
        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![tail..source.len()];
        map.reparse_within_changed_ranges(&rope, lang.clone(), 2, Some(&changed), None, None)
            .unwrap();

        let fence_layers: Vec<(u32, u32)> = map
            .snapshot()
            .iter_layers()
            .filter(|l| l.language.name == "rust")
            .map(|l| (l.start_offset, l.end_offset))
            .collect();
        let fn_start = source.find("fn").unwrap() as u32;
        assert_eq!(
            fence_layers,
            vec![(fn_start, fn_start + "fn a() {}\n".len() as u32)],
            "the untouched rust fence must survive a filtered reparse exactly once"
        );
    }

    #[test]
    fn filtered_reparse_drops_an_injection_deleted_inside_the_changed_range() {
        // The carry-over must not resurrect an injection the edit
        // removed. A layer intersecting the filter is the walk's
        // responsibility, and the walk not finding it means it is gone.
        use stoat_text::patch::Edit as PatchEdit;
        let lang = markdown_lang();
        let old_source = "```rust\nfn a() {}\n```\n";
        let old_rope = Rope::from(old_source);
        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();
        assert!(
            map.snapshot()
                .iter_layers()
                .any(|l| l.language.name == "rust"),
            "fixture must start with a rust fence layer"
        );

        let new_source = "plain text\n";
        let new_rope = Rope::from(new_source);
        let edits = vec![PatchEdit {
            old: 0..old_source.len(),
            new: 0..new_source.len(),
        }];
        map.interpolate(&edits, &old_rope, &new_rope);
        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![0..new_source.len()];
        map.reparse_within_changed_ranges(&new_rope, lang.clone(), 2, Some(&changed), None, None)
            .unwrap();

        assert!(
            map.snapshot()
                .iter_layers()
                .all(|l| l.language.name != "rust"),
            "the deleted fence must not be carried over, got {:?}",
            map.snapshot()
                .iter_layers()
                .map(|l| (l.depth, l.language.name))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn interpolate_shifts_layer_offsets_through_the_edit() {
        // Layer bounds are read between interpolate and reparse (to
        // decide which layers a filtered reparse may carry over, and by
        // `captures` to skip layers outside the requested range), so
        // they must follow the text the way each layer's tree does.
        use stoat_text::patch::Edit as PatchEdit;
        let lang = markdown_lang();
        let old_source = "```rust\nfn a() {}\n```\n";
        let old_rope = Rope::from(old_source);
        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();
        let before: Vec<(u32, u32)> = map
            .snapshot()
            .iter_layers()
            .filter(|l| l.language.name == "rust")
            .map(|l| (l.start_offset, l.end_offset))
            .collect();

        let prefix = "# Heading\n";
        let new_source = format!("{prefix}{old_source}");
        let new_rope = Rope::from(new_source.as_str());
        let edits = vec![PatchEdit {
            old: 0..0,
            new: 0..prefix.len(),
        }];
        map.interpolate(&edits, &old_rope, &new_rope);

        let after: Vec<(u32, u32)> = map
            .snapshot()
            .iter_layers()
            .filter(|l| l.language.name == "rust")
            .map(|l| (l.start_offset, l.end_offset))
            .collect();
        let shift = prefix.len() as u32;
        assert_eq!(
            after,
            before
                .iter()
                .map(|&(s, e)| (s + shift, e + shift))
                .collect::<Vec<_>>(),
            "inserting before a layer must move its bounds by the insertion length"
        );
    }

    /// The summarized count matches the layers actually in the tree, and
    /// interpolation leaves them in the order the tree is seeked on.
    ///
    /// The count answers without a walk, so nothing else notices when it drifts
    /// from what the tree holds. Ordering is what lets interpolation skip the
    /// sort. Depth does not move and `translate_offset` cannot make one layer's
    /// start pass another's, so the list it builds is already ordered. That
    /// holds by construction rather than by the assertion below, which is here
    /// to catch a future edit that breaks the reasoning.
    #[test]
    fn a_layer_tree_counts_and_orders_itself_across_an_edit() {
        use stoat_text::patch::Edit as PatchEdit;
        let lang = markdown_lang();
        let old_source: String = (0..12)
            .map(|i| format!("para {i} *em*\n\n```rust\nfn f{i}() {{}}\n```\n\n"))
            .collect();
        let old_rope = Rope::from(old_source.as_str());
        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();

        let checked = |map: &SyntaxMap, when: &str| {
            let snapshot = map.snapshot();
            assert_eq!(
                snapshot.layer_count(),
                snapshot.iter_layers().count(),
                "the summarized count must match the tree {when}",
            );
            let keys: Vec<(u32, u32)> = snapshot
                .iter_layers()
                .map(|l| (l.depth, l.start_offset))
                .collect();
            let mut ordered = keys.clone();
            ordered.sort();
            assert_eq!(keys, ordered, "layers must stay key-ordered {when}");
            keys.len()
        };
        let before = checked(&map, "after a parse");
        assert!(
            before > 12,
            "the fixture must inject many layers, got {before}"
        );

        // Mid-file, so layers fall on both sides of it. The ones before are
        // left alone and the ones after have their offsets translated, and the
        // two halves have to stay in one order between them.
        let at = old_source.find("para 6").expect("fixture");
        let inserted = "# Heading\n\n";
        let new_source = format!("{}{inserted}{}", &old_source[..at], &old_source[at..]);
        let new_rope = Rope::from(new_source.as_str());
        map.interpolate(
            &[PatchEdit {
                old: at..at,
                new: at..(at + inserted.len()),
            }],
            &old_rope,
            &new_rope,
        );

        assert_eq!(
            checked(&map, "after an interpolation"),
            before,
            "interpolation moves layers without adding or dropping any",
        );
    }

    #[test]
    fn reparse_with_no_filter_matches_full_reparse() {
        // The convenience `reparse` and the underlying
        // `reparse_within_changed_ranges` with `None` should produce
        // identical layer sets.
        let lang = markdown_lang();
        let source = "# Heading\n\n**bold** text\n";
        let rope = Rope::from(source);

        let mut a = SyntaxMap::new();
        a.reparse(&rope, lang.clone(), 1, None, None).unwrap();

        let mut b = SyntaxMap::new();
        b.reparse_within_changed_ranges(&rope, lang.clone(), 1, None, None, None)
            .unwrap();

        let a_layers: Vec<(u32, u32, u32, &str)> = a
            .snapshot()
            .iter_layers()
            .map(|l| (l.depth, l.start_offset, l.end_offset, l.language.name))
            .collect();
        let b_layers: Vec<(u32, u32, u32, &str)> = b
            .snapshot()
            .iter_layers()
            .map(|l| (l.depth, l.start_offset, l.end_offset, l.language.name))
            .collect();
        assert_eq!(a_layers, b_layers);
    }

    /// Every layer's bounds, language, and tree shape, for comparing two
    /// maps that must agree on all three.
    ///
    /// Panics on a single-layer map, so a comparison of two maps cannot pass
    /// by agreeing that neither found any injection.
    fn layer_shapes(map: &SyntaxMap) -> Vec<(u32, u32, u32, &'static str, String)> {
        let shapes: Vec<_> = map
            .snapshot()
            .iter_layers()
            .map(|l| {
                (
                    l.depth,
                    l.start_offset,
                    l.end_offset,
                    l.language.name,
                    l.tree.root_node().to_sexp(),
                )
            })
            .collect();
        assert!(
            shapes.len() > 1,
            "fixture must produce injection layers, got {shapes:?}"
        );
        shapes
    }

    #[test]
    fn a_supplied_root_matches_the_self_parsing_reparse() {
        // The supplied tree stands in for the root parse, so the injection
        // walk that reads layer 0 must find the same fence and inline
        // layers it finds when reparse parses the root itself.
        let lang = markdown_lang();
        let source = "# Heading\n\n```rust\nfn main() {}\n```\n\n**bold** text\n";
        let rope = Rope::from(source);

        let mut self_parsed = SyntaxMap::new();
        self_parsed
            .reparse(&rope, lang.clone(), 1, None, None)
            .unwrap();

        let root = parse_rope(&lang, &rope, None).expect("markdown parse must succeed");
        let mut supplied = SyntaxMap::new();
        supplied
            .reparse(&rope, lang.clone(), 1, Some(&root), None)
            .unwrap();

        assert_eq!(layer_shapes(&self_parsed), layer_shapes(&supplied));
    }

    #[test]
    fn a_supplied_root_matches_the_self_parsing_filtered_reparse() {
        // This is the shape the parse pipeline runs per keystroke. Interpolate
        // carries the prior layers across the edit, then the filtered walk
        // re-discovers injections near it. Handing in a root parsed from the
        // same edited tree must not change which layers survive that walk.
        use stoat_text::patch::Edit as PatchEdit;
        let lang = markdown_lang();
        let old_source = "# Heading\n\n```rust\nfn main() {}\n```\n\n**bold** text\n";
        let old_rope = Rope::from(old_source);

        let insert_pos = old_source
            .find("fn main")
            .expect("fixture has a fence body");
        let inserted = "pub ";
        let mut new_text = String::new();
        new_text.push_str(&old_source[..insert_pos]);
        new_text.push_str(inserted);
        new_text.push_str(&old_source[insert_pos..]);
        let new_rope = Rope::from(new_text.as_str());
        let edits = vec![PatchEdit {
            old: insert_pos..insert_pos,
            new: insert_pos..(insert_pos + inserted.len()),
        }];
        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![insert_pos..(insert_pos + inserted.len())];

        let mut base = SyntaxMap::new();
        base.reparse(&old_rope, lang.clone(), 1, None, None)
            .unwrap();

        let mut self_parsed = base.clone();
        self_parsed.interpolate(&edits, &old_rope, &new_rope);
        self_parsed
            .reparse_within_changed_ranges(&new_rope, lang.clone(), 2, Some(&changed), None, None)
            .unwrap();

        // The root a caller already holds is the prior root advanced across
        // the edit, which is what `parse_buffer_step` parses before the map.
        let mut edited_root = base
            .snapshot()
            .iter_layers()
            .find(|l| l.depth == 0)
            .expect("a root layer")
            .tree
            .clone();
        edit_tree(&mut edited_root, &edits, &old_rope, &new_rope);
        let root = parse_rope(&lang, &new_rope, Some(&edited_root)).expect("reparse must succeed");

        let mut supplied = base;
        supplied.interpolate(&edits, &old_rope, &new_rope);
        supplied
            .reparse_within_changed_ranges(
                &new_rope,
                lang.clone(),
                2,
                Some(&changed),
                Some(&root),
                None,
            )
            .unwrap();

        assert_eq!(layer_shapes(&self_parsed), layer_shapes(&supplied));
    }

    /// Reparse `source` with the root handed in, once on a budget that cannot
    /// expire and once on one already spent.
    ///
    /// Asserts the spent budget aborts, and returns the budgeted map so the
    /// caller can pin which injection layers the abort cut off. Supplying the
    /// root is what makes the assertion mean something. It leaves the
    /// injection parses as the only parses, so nothing else could have
    /// aborted.
    ///
    /// Callers must give the injection they are testing a body big enough to
    /// matter. Tree-sitter consults the progress callback on its own schedule,
    /// so a parse of a few dozen bytes finishes without ever checking the
    /// clock, and a fixture built from those would abort nowhere and prove
    /// nothing.
    fn reparse_on_a_live_and_a_spent_budget(lang: &Arc<Language>, source: &str) -> SyntaxMap {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let rope = Rope::from(source);
        let root = parse_rope(lang, &rope, None).expect("the root parse must succeed");

        let mut budgeted = SyntaxMap::new();
        let unreachable_deadline = executor.now() + Duration::from_secs(60);
        budgeted
            .reparse(
                &rope,
                lang.clone(),
                1,
                Some(&root),
                Some((unreachable_deadline, &executor)),
            )
            .expect("a budget that cannot expire must not abort");

        let mut expired = SyntaxMap::new();
        assert_eq!(
            expired.reparse(
                &rope,
                lang.clone(),
                1,
                Some(&root),
                Some((executor.now(), &executor)),
            ),
            None,
            "an injection parse past the deadline must abort the whole reparse"
        );
        assert!(
            expired.snapshot().is_empty(),
            "and must install nothing, since a layer set missing its aborted layers reads as complete"
        );

        budgeted
    }

    /// A keystroke inside a combined injection reports what the edit touched,
    /// not the span the layer happens to reach across.
    ///
    /// A combined layer runs from its first host node to its last, so in a file
    /// with doc comments top and bottom that is nearly everything. Reporting it
    /// whole makes every keystroke re-query the file, re-mint every token
    /// anchor, and re-sort the full lists. The extent the fresh layer and the
    /// carried one share is already covered by the tree diff, so only their
    /// disagreement needs reporting, and in steady-state typing there is none.
    #[test]
    fn a_keystroke_in_a_combined_layer_reports_only_the_edit() {
        use stoat_text::patch::Edit as PatchEdit;
        let lang = rust_lang();
        let source = "/// one **bold**\nfn a() {}\n/// two **bold**\nfn b() {}\n\
                      /// three **bold**\nfn c() {}\n";
        let old_rope = Rope::from(source);

        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();

        let layer_span = map
            .snapshot()
            .iter_layers()
            .find(|l| l.depth == 1 && l.language.name == "markdown")
            .map(|l| l.start_offset as usize..l.end_offset as usize)
            .expect("a depth-1 markdown layer");
        assert!(
            layer_span.end - layer_span.start > source.len() / 2,
            "the fixture must have a layer spanning most of the file, got {layer_span:?}",
        );

        let at = source.find("**bold**").expect("fixture has bold") + "**".len();
        let inserted = "very ";
        let mut text = String::from(&source[..at]);
        text.push_str(inserted);
        text.push_str(&source[at..]);
        let new_rope = Rope::from(text.as_str());
        let edit = at..(at + inserted.len());

        map.interpolate(
            &[PatchEdit {
                old: at..at,
                new: edit.clone(),
            }],
            &old_rope,
            &new_rope,
        );

        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![edit.clone()];
        let layer_changes = map
            .reparse_within_changed_ranges(&new_rope, lang, 2, Some(&changed), None, None)
            .expect("the reparse must complete");

        // A quarter of the layer rather than merely less than it, so a report
        // that grew back part of the way still fails. The steady-state answer is
        // the edit itself, which is a fifteenth of this fixture's layer.
        let reported: usize = layer_changes.iter().map(|r| r.end - r.start).sum();
        let layer_len = layer_span.end - layer_span.start;
        assert!(
            reported * 4 < layer_len,
            "a one-word edit must report a small fraction of the combined span; \
             got {layer_changes:?} totalling {reported} against a layer of {layer_span:?}",
        );
    }

    #[test]
    fn an_edit_in_one_doc_comment_reuses_the_combined_tree() {
        use stoat_text::patch::Edit as PatchEdit;
        let lang = rust_lang();
        let source = "/// one **bold**\nfn a() {}\n/// two **bold**\nfn b() {}\n\
                      /// three **bold**\nfn c() {}\n";
        let old_rope = Rope::from(source);

        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();
        let combined = |map: &SyntaxMap| -> Vec<tree_sitter::Range> {
            map.snapshot()
                .iter_layers()
                .find(|l| l.depth == 1 && l.language.name == "markdown")
                .expect("a depth-1 markdown layer")
                .tree
                .included_ranges()
        };
        assert!(
            combined(&map).len() > 1,
            "the fixture must merge several host ranges into one layer"
        );

        let at = source.find("**bold**").expect("fixture has bold") + "**".len();
        let inserted = "very ";
        let mut text = String::from(&source[..at]);
        text.push_str(inserted);
        text.push_str(&source[at..]);
        let new_rope = Rope::from(text.as_str());
        let edits = vec![PatchEdit {
            old: at..at,
            new: at..(at + inserted.len()),
        }];

        map.interpolate(&edits, &old_rope, &new_rope);
        let carried = combined(&map);

        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![at..(at + inserted.len())];
        map.reparse_within_changed_ranges(&new_rope, lang.clone(), 2, Some(&changed), None, None)
            .unwrap();

        // Reuse itself is invisible from here, since an incremental parse and a
        // from-scratch one over the same text and ranges must agree. What can
        // be pinned is the condition reuse is gated on, which is the walk
        // rediscovering the same ranges the prior tree was built over. If that
        // stopped holding for an ordinary edit, reuse would quietly stop with
        // nothing else to show for it.
        assert_eq!(
            combined(&map),
            carried,
            "an edit inside one comment must leave the range set where interpolate put it"
        );

        let mut fresh = SyntaxMap::new();
        fresh.reparse(&new_rope, lang, 2, None, None).unwrap();
        let shapes = |map: &SyntaxMap| -> Vec<(u32, u32, u32, String)> {
            map.snapshot()
                .iter_layers()
                .map(|l| {
                    (
                        l.depth,
                        l.start_offset,
                        l.end_offset,
                        l.tree.root_node().to_sexp(),
                    )
                })
                .collect()
        };
        assert_eq!(
            shapes(&map),
            shapes(&fresh),
            "and must land on the same layers a from-scratch parse would"
        );
    }

    #[test]
    fn a_filtered_reparse_keeps_the_paragraphs_the_filter_never_reached() {
        // Markdown puts each paragraph's inline content in its own layer, and a
        // filter covering the last paragraph reaches only that one. Nothing in
        // the walk re-finds the first, so the carry pass is the only thing
        // keeping it, and without it the rest of the document loses its inline
        // highlighting.
        use stoat_text::patch::Edit as PatchEdit;
        let lang = markdown_lang();
        let source = "para one *em*\n\npara two *em*\n";
        let old_rope = Rope::from(source);

        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();
        let inline = |map: &SyntaxMap| -> Vec<Vec<Range<usize>>> {
            map.snapshot()
                .iter_layers()
                .filter(|l| l.language.name == "markdown-inline")
                .map(|l| {
                    l.tree
                        .included_ranges()
                        .iter()
                        .map(|r| r.start_byte..r.end_byte)
                        .collect()
                })
                .collect()
        };
        let first = source.find("para one").expect("fixture");
        assert_eq!(
            inline(&map),
            vec![vec![first..first + "para one *em*".len()], vec![15..28]],
            "the fixture must give each paragraph its own layer"
        );

        let at = source.find("para two").expect("fixture") + "para ".len();
        let inserted = "the ";
        let text = format!("{}{inserted}{}", &source[..at], &source[at..]);
        let new_rope = Rope::from(text.as_str());
        let edits = vec![PatchEdit {
            old: at..at,
            new: at..(at + inserted.len()),
        }];
        map.interpolate(&edits, &old_rope, &new_rope);

        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![at..(at + inserted.len())];
        map.reparse_within_changed_ranges(&new_rope, lang.clone(), 2, Some(&changed), None, None)
            .unwrap();

        let mut fresh = SyntaxMap::new();
        fresh.reparse(&new_rope, lang, 2, None, None).unwrap();
        assert_eq!(
            inline(&map),
            inline(&fresh),
            "the untouched first paragraph must keep its inline layer"
        );
    }

    /// Splitting one paragraph leaves every later paragraph's inline layer
    /// exactly where interpolate put it.
    ///
    /// Pressing Enter mid-paragraph adds a host node, which is the change no
    /// merged layer absorbs. One range set becomes another, the parse declines
    /// the prior tree, and every paragraph in the file is parsed again. With a
    /// layer per paragraph the addition is local, and the layers coming through
    /// untouched is the condition their reuse rests on.
    #[test]
    fn splitting_a_paragraph_leaves_the_later_inline_layers_alone() {
        use stoat_text::patch::Edit as PatchEdit;
        let lang = markdown_lang();
        let source: String = (0..6)
            .map(|i| format!("para {i} with *em* text\n\n"))
            .collect();
        let old_rope = Rope::from(source.as_str());

        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();
        let inline = |map: &SyntaxMap| -> Vec<Range<usize>> {
            map.snapshot()
                .iter_layers()
                .filter(|l| l.language.name == "markdown-inline")
                .map(|l| l.start_offset as usize..l.end_offset as usize)
                .collect()
        };
        assert_eq!(
            inline(&map).len(),
            6,
            "the fixture must put each paragraph in its own layer"
        );

        let at = source.find("para 1 ").expect("fixture") + "para 1 ".len();
        let inserted = "\n\n";
        let text = format!("{}{inserted}{}", &source[..at], &source[at..]);
        let new_rope = Rope::from(text.as_str());
        map.interpolate(
            &[PatchEdit {
                old: at..at,
                new: at..(at + inserted.len()),
            }],
            &old_rope,
            &new_rope,
        );

        let later = text.find("para 2").expect("fixture");
        let untouched = |map: &SyntaxMap| -> Vec<Range<usize>> {
            inline(map)
                .into_iter()
                .filter(|l| l.start >= later)
                .collect()
        };
        let carried = untouched(&map);
        assert_eq!(
            carried.len(),
            4,
            "paragraphs 2 through 5 sit after the edit"
        );

        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![at..(at + inserted.len())];
        map.reparse_within_changed_ranges(&new_rope, lang.clone(), 2, Some(&changed), None, None)
            .unwrap();

        assert_eq!(
            untouched(&map),
            carried,
            "a split in one paragraph must not move the layers after it"
        );

        let mut fresh = SyntaxMap::new();
        fresh.reparse(&new_rope, lang, 2, None, None).unwrap();
        assert_eq!(
            inline(&map),
            inline(&fresh),
            "and the split paragraph must end up as the two layers a fresh parse finds"
        );
    }

    /// Two doc comments with code between them are two documents, and editing
    /// one leaves the other's tree where it was.
    ///
    /// This is the whole point of cutting blocks at a break in the rows. One
    /// layer over every doc comment in the file has to reparse whole whenever a
    /// `///` line is added or dropped, because no incremental parse survives a
    /// change to its range set.
    #[test]
    fn an_edit_in_one_doc_block_leaves_the_other_block_carried() {
        use stoat_text::patch::Edit as PatchEdit;
        let lang = rust_lang();
        let source =
            "/// one **b**\n/// one more\nfn a() {}\n\n/// two **b**\n/// two more\nfn b() {}\n";
        let old_rope = Rope::from(source);

        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();
        let blocks = |map: &SyntaxMap| -> Vec<(u32, u32, usize)> {
            map.snapshot()
                .iter_layers()
                .filter(|l| l.depth == 1 && l.language.name == "markdown")
                .map(|l| (l.start_offset, l.end_offset, l.tree.included_ranges().len()))
                .collect()
        };

        let before = blocks(&map);
        assert_eq!(
            before.len(),
            2,
            "each run of doc lines is its own document: {before:?}"
        );
        assert!(
            before.iter().all(|(_, _, ranges)| *ranges == 2),
            "and holds the two lines of its run: {before:?}"
        );

        // A whole new doc line in the second block, so its range set changes and
        // it parses fresh rather than carrying. The first block's must not move.
        let at = source
            .find("/// two more")
            .expect("fixture has the second run");
        let added = "/// two even more\n";
        let text = format!("{}{}{}", &source[..at], added, &source[at..]);
        let new_rope = Rope::from(text.as_str());

        map.interpolate(
            &[PatchEdit {
                old: at..at,
                new: at..at + added.len(),
            }],
            &old_rope,
            &new_rope,
        );
        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![at..at + added.len()];
        let reported = map
            .reparse_within_changed_ranges(&new_rope, lang.clone(), 2, Some(&changed), None, None)
            .expect("the reparse completes");

        // What the reparse reports changed is what it re-parsed or dropped, so a
        // block it never reports is one it carried whole. One layer over every
        // doc comment reports the span of all of them for this same edit.
        let first_block_end = before[0].1 as usize;
        assert!(
            reported.iter().all(|r| r.start >= first_block_end),
            "the untouched block is outside everything the reparse reports: {reported:?}"
        );

        let mut fresh = SyntaxMap::new();
        fresh.reparse(&new_rope, lang, 2, None, None).unwrap();
        assert_eq!(
            blocks(&map),
            blocks(&fresh),
            "and the edited block lands where a fresh parse puts it"
        );
    }

    #[test]
    fn a_filtered_reparse_keeps_doc_comments_the_edit_sat_between() {
        // Code between two doc comments ends one block and starts another, so
        // neither layer covers the body the edit lands in. A walk that reaches
        // toward a layer and re-finds no markdown there drops it, and that
        // comment stops being highlighted.
        use stoat_text::patch::Edit as PatchEdit;
        let lang = rust_lang();
        let source = "/// one **b**\nfn a() {\n    let x = 1;\n}\n/// two **b**\nfn b() {}\n";
        let old_rope = Rope::from(source);

        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang.clone(), 1, None, None).unwrap();
        let comments = |map: &SyntaxMap| -> Vec<Range<usize>> {
            let mut ranges: Vec<Range<usize>> = map
                .snapshot()
                .iter_layers()
                .filter(|l| l.depth == 1 && l.language.name == "markdown")
                .flat_map(|l| {
                    l.tree
                        .included_ranges()
                        .into_iter()
                        .map(|r| r.start_byte..r.end_byte)
                        .collect::<Vec<_>>()
                })
                .collect();
            ranges.sort_by_key(|r| r.start);
            ranges
        };
        assert_eq!(
            comments(&map).len(),
            2,
            "the fixture must inject both doc comments"
        );

        let at = source.find("let x = 1").expect("fixture") + "let x = ".len();
        let text = format!("{}42{}", &source[..at], &source[at + 1..]);
        let new_rope = Rope::from(text.as_str());
        let edits = vec![PatchEdit {
            old: at..at + 1,
            new: at..at + 2,
        }];
        map.interpolate(&edits, &old_rope, &new_rope);

        #[allow(clippy::single_range_in_vec_init)]
        let changed = vec![at..at + 2];
        map.reparse_within_changed_ranges(&new_rope, lang.clone(), 2, Some(&changed), None, None)
            .unwrap();

        let mut fresh = SyntaxMap::new();
        fresh.reparse(&new_rope, lang, 2, None, None).unwrap();
        assert_eq!(
            comments(&map),
            comments(&fresh),
            "an edit between the comments must leave both of them injected"
        );
    }

    #[test]
    fn an_edit_in_code_leaves_a_doc_comment_layer_alone() {
        // Reporting layer changes only pays off if the common edit reports
        // nothing. A rust file with one doc comment is the case that matters,
        // since otherwise every keystroke anywhere in it would drag the
        // comment's markdown layer back through the caller's re-query.
        use stoat_text::patch::Edit as PatchEdit;
        let lang = rust_lang();
        let source = "/// A **doc** comment\nfn a() {}\n\nfn b() {}\n\nfn c() {}\n";
        let rope = Rope::from(source);

        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang.clone(), 1, None, None).unwrap();
        let markdown_layers = |map: &SyntaxMap| -> Vec<(u32, u32)> {
            map.snapshot()
                .iter_layers()
                .filter(|l| l.language.name == "markdown")
                .map(|l| (l.start_offset, l.end_offset))
                .collect()
        };
        let before = markdown_layers(&map);
        assert_eq!(
            before.len(),
            1,
            "the fixture must inject one markdown layer"
        );

        let edit_at = source.rfind("fn c() {").expect("fixture has a third fn") + "fn c() {".len();
        let reparse = |map: &mut SyntaxMap, at: usize| -> Vec<Range<usize>> {
            let inserted = " let x = 1; ";
            let mut text = String::from(&source[..at]);
            text.push_str(inserted);
            text.push_str(&source[at..]);
            let new_rope = Rope::from(text.as_str());
            let edits = vec![PatchEdit {
                old: at..at,
                new: at..(at + inserted.len()),
            }];
            map.interpolate(&edits, &rope, &new_rope);
            #[allow(clippy::single_range_in_vec_init)]
            let changed = vec![at..(at + inserted.len())];
            map.reparse_within_changed_ranges(
                &new_rope,
                lang.clone(),
                2,
                Some(&changed),
                None,
                None,
            )
            .expect("reparse must succeed")
        };

        let mut untouched = map.clone();
        assert_eq!(
            reparse(&mut untouched, edit_at),
            Vec::<Range<usize>>::new(),
            "an edit in code rows away from the comment moves no layer"
        );
        assert_eq!(
            markdown_layers(&untouched),
            before,
            "and leaves the comment's layer exactly where it was"
        );

        // The contrast is what shows the empty report above is a real answer
        // rather than the reporting never firing.
        let mut touched = map;
        assert!(
            !reparse(
                &mut touched,
                source.find("**doc**").expect("fixture has bold")
            )
            .is_empty(),
            "an edit inside the comment does move the layer set"
        );
    }

    #[test]
    fn an_expired_deadline_aborts_a_ranged_injection_parse() {
        // A fence and no prose, so the fence body is the only injection parse
        // the walk runs and the abort can have come from nowhere else.
        let source = format!("```rust\n{}```\n", "fn main() { let x = 1; }\n".repeat(400));
        let budgeted = reparse_on_a_live_and_a_spent_budget(&markdown_lang(), &source);

        assert!(
            budgeted
                .snapshot()
                .iter_layers()
                .any(|l| l.language.name == "rust"),
            "the fixture must inject a fence layer for the deadline to have cut one off"
        );
    }

    #[test]
    fn an_expired_deadline_aborts_a_combined_injection_parse() {
        // Two doc-comment blocks put two markdown host ranges at the same
        // depth, which the walk merges into one combined-range parse rather
        // than a layer each, so this fixture routes through a different parse
        // helper than the ranged test above. The layer assertion below is what
        // pins that the merge happened rather than a layer per block.
        let block = "/// some **bold** text and `code` and [a link](url)\n".repeat(200);
        let source = format!("{block}fn a() {{}}\n{block}fn b() {{}}\n");
        let budgeted = reparse_on_a_live_and_a_spent_budget(&rust_lang(), &source);

        let second_block = source.rfind("///").expect("the fixture has two blocks");
        let host = budgeted
            .snapshot()
            .iter_layers()
            .find(|l| l.depth == 1)
            .expect("a depth-1 markdown layer");
        assert!(
            (host.start_offset as usize) < second_block
                && (host.end_offset as usize) > second_block,
            "one layer must span both blocks, which is what makes it combined, got {host:?}"
        );
    }

    /// The range filter reaches a layer that a later sibling's extent hides.
    ///
    /// The root layer spans the file and sorts ahead of every injection, so
    /// the subtree holding it also holds shorter layers that end long before
    /// it does. Only the largest end in a subtree bounds what that subtree
    /// reaches. Taking the last layer's end instead skips the root for a range
    /// past the doc comments, and the fixture needs more layers than one
    /// sum-tree leaf holds to put the two in one subtree at all.
    #[test]
    fn a_range_filter_reaches_a_layer_a_later_sibling_ends_before() {
        let lang = rust_lang();
        let mut source = String::new();
        for i in 0..16 {
            source.push_str(&format!(
                "/// doc **bold{i}**\nfn f{i}() {{ let _ = {i}; }}\n\n"
            ));
        }
        let rope = Rope::from(source.as_str());
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1, None, None).unwrap();
        let snapshot = map.snapshot();
        assert!(
            snapshot.layer_count() > 12,
            "the fixture must outgrow one sum-tree leaf, got {} layers",
            snapshot.layer_count(),
        );

        let tail = source.rfind("fn f15").expect("fixture")..source.len();
        let captures = snapshot.captures(tail, &rope, |l| Some(&l.highlight_query));
        assert!(
            captures.iter().any(|c| c.depth == 0),
            "the root layer must answer a range that starts past every \
             injected layer's end",
        );
    }

    /// A layer the edit never reaches carries over by subtree rather than by
    /// copy.
    ///
    /// A whole-file rebuild answers every question about the layer set the
    /// same way, so what separates it is where the layers live. Slicing shares
    /// the aligned subtrees by pointer and copies only the partial leaves at
    /// the two ends of a run, which leaves most untouched layers at the
    /// address they already had. A rebuild leaves none of them there.
    #[test]
    fn interpolate_carries_an_untouched_layer_by_subtree() {
        use stoat_text::patch::Edit as PatchEdit;
        let lang = rust_lang();
        let mut original = String::new();
        for i in 0..16 {
            original.push_str(&format!(
                "/// doc **bold{i}**\nfn f{i}() {{ let _ = {i}; }}\n\n"
            ));
        }
        let old_rope = Rope::from(original.as_str());
        let mut map = SyntaxMap::new();
        map.reparse(&old_rope, lang, 1, None, None).unwrap();
        assert!(
            map.snapshot().layer_count() > 12,
            "the fixture must outgrow one sum-tree leaf, got {} layers",
            map.snapshot().layer_count(),
        );

        let insert_pos = original.len();
        let inserted = "fn tail() {}\n";
        let new_rope = Rope::from(format!("{original}{inserted}").as_str());
        let edits = vec![PatchEdit {
            old: insert_pos..insert_pos,
            new: insert_pos..(insert_pos + inserted.len()),
        }];

        let before = map.snapshot().clone();
        map.interpolate(&edits, &old_rope, &new_rope);

        let carried = before
            .iter_layers()
            .zip(map.snapshot().iter_layers())
            .filter(|(old, new)| std::ptr::eq(*old, *new))
            .count();
        assert!(
            carried > 0,
            "an edit past every injected layer must leave some of them in \
             place, got {carried} of {} still at their address",
            before.layer_count(),
        );
    }
}
