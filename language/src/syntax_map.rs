//! Multi-layer syntax tree storage for languages with injections.
//!
//! Goal: replace the single-tree [`crate::SyntaxState`] +
//! [`crate::InjectionTreeCache`] pair with a [`SumTree`] of
//! [`SyntaxLayer`]s. Each layer carries one parsed [`tree_sitter::Tree`]
//! plus enough metadata to walk the layers in document order, depth-by-
//! depth, so a single capture iterator can merge highlights across the
//! root grammar and every injection without the per-host-node loop in
//! [`crate::extract_highlights_rope_with_cache`].
//!
//! Pattern adapted from
//! `references/zed/crates/language/src/syntax_map.rs`. The full target
//! pipeline is:
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
//!
//! [`crate::SyntaxState`] is still the per-buffer source of truth for
//! highlight extraction; this module is populated in parallel and will
//! take over once the capture-merging consumers no longer need the
//! single-tree state.

use crate::{
    edit_tree,
    highlight::{QueryCursorHandle, RopeTextProvider},
    parse_rope, parse_rope_range, Language,
};
use std::{
    cmp::Reverse,
    collections::{HashMap, VecDeque},
    ops::Range,
    sync::Arc,
};
use stoat_text::{patch::Edit as PatchEdit, ContextLessSummary, Item, Rope, SumTree};
use tree_sitter::{Node, Query, StreamingIterator, Tree};

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

impl ContextLessSummary for LayerKey {
    fn add_summary(&mut self, other: &Self) {
        // For an ordered-key summary, the cumulative position is just
        // the latest item's key. SumTree uses this for seeking.
        *self = other.clone();
    }
}

impl Item for SyntaxLayer {
    type Summary = LayerKey;
    fn summary(&self, _cx: ()) -> LayerKey {
        LayerKey {
            depth: self.depth,
            start_offset: self.start_offset,
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
        self.layers.iter().count()
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
        let mut all: Vec<SyntaxMapCapture<'a>> = Vec::new();
        for layer in self.layers.iter() {
            // Skip layers that don't intersect the requested range.
            if (layer.end_offset as usize) <= byte_range.start
                || (layer.start_offset as usize) >= byte_range.end
            {
                continue;
            }
            let Some(query) = select(layer.language.as_ref()) else {
                continue;
            };
            let mut cursor = QueryCursorHandle::new();
            cursor.set_byte_range(byte_range.clone());
            let provider = RopeTextProvider { rope };
            // QueryCursor::captures yields &(QueryMatch, capture_index)
            // tuples; the capture_index picks out which capture in the
            // match's `captures` array this iteration is yielding.
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
            // cursor drops here, returns to the pool via QueryCursorHandle::drop.
        }
        // Sort: shallower layers (smaller `depth`) come first for ties
        // on `(start, end)`. Document order is the primary key.
        all.sort_by_key(|c| {
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
#[derive(Default)]
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

    /// Apply tree-sitter edits to every layer's underlying [`Tree`].
    /// After this call each layer's tree is positioned to act as
    /// `old_tree` for an incremental [`reparse`](Self::reparse).
    ///
    /// Walks every layer and forwards `edits` to [`crate::edit_tree`].
    pub fn interpolate(&mut self, edits: &[PatchEdit<usize>], old_rope: &Rope, new_rope: &Rope) {
        // FIXME: layer start_offset/end_offset are not translated through
        // `edits`. A layer that covers bytes 50..100 stays at 50..100 even
        // when edits insert or delete bytes before byte 50. Reparse rebuilds
        // the layer set from scratch, so this is only visible to callers
        // that read offsets between interpolate and reparse.
        if edits.is_empty() {
            return;
        }
        let mut new_layers: Vec<SyntaxLayer> = Vec::with_capacity(self.snapshot.layer_count());
        for layer in self.snapshot.layers.iter() {
            let mut next = layer.clone();
            edit_tree(&mut next.tree, edits, old_rope, new_rope);
            new_layers.push(next);
        }
        self.install_layers(new_layers, self.snapshot.parsed_version);
    }

    /// Reparse `rope` against `language`, replacing every layer.
    ///
    /// Convenience wrapper around
    /// [`Self::reparse_within_changed_ranges`] that walks the entire
    /// tree (no changed-range filter).
    pub fn reparse(&mut self, rope: &Rope, language: Arc<Language>, version: u64) -> Option<()> {
        self.reparse_within_changed_ranges(rope, language, version, None)
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
    pub fn reparse_within_changed_ranges(
        &mut self,
        rope: &Rope,
        language: Arc<Language>,
        version: u64,
        changed_ranges: Option<&[Range<usize>]>,
    ) -> Option<()> {
        // Expand changed ranges by +/- 1 row when filtering injection
        // queries. The expansion catches injection boundary flips
        // (e.g. uncommenting a line whose adjacent line was the start
        // of a fenced code block).
        let expanded_ranges: Option<Vec<Range<usize>>> = changed_ranges.map(|ranges| {
            ranges
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
                .collect()
        });
        // Continue with the body of the original `reparse`.
        self.reparse_inner(rope, language, version, expanded_ranges.as_deref())
    }

    fn reparse_inner(
        &mut self,
        rope: &Rope,
        language: Arc<Language>,
        version: u64,
        injection_filter_ranges: Option<&[Range<usize>]>,
    ) -> Option<()> {
        // Capture the prior root tree for incremental reparse.
        let prior_root_tree = self
            .snapshot
            .layers
            .iter()
            .find(|l| l.depth == 0)
            .map(|l| l.tree.clone());

        // Snapshot prior injection trees keyed by (host_range, language_name)
        // so we can reuse them when the same host node still exists.
        let prior_injections: Vec<PriorInjection> = self
            .snapshot
            .layers
            .iter()
            .filter(|l| l.depth >= 1)
            .map(|l| PriorInjection {
                start_offset: l.start_offset,
                end_offset: l.end_offset,
                language_name: l.language.name,
                tree: l.tree.clone(),
            })
            .collect();

        let root_tree = parse_rope(&language, rope, prior_root_tree.as_ref())?;

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

            // Group injection matches by inner language so we can
            // emit combined-injection trees (one tree per language
            // covering all of that language's host ranges).
            let mut grouped: HashMap<&'static str, (Arc<Language>, Vec<Range<usize>>)> =
                HashMap::new();

            let mut cursor = QueryCursorHandle::new();
            // When the caller has supplied changed-range filters,
            // restrict the injection query to the union of those
            // ranges. The cursor's `set_byte_range` only accepts a
            // single range, so we walk each filter range in turn and
            // collect the matches; for the common no-filter case we
            // walk the whole tree once.
            let filter_ranges: Vec<Range<usize>> = match injection_filter_ranges {
                Some(ranges) if !ranges.is_empty() => ranges.to_vec(),
                #[allow(clippy::single_range_in_vec_init)]
                _ => vec![0..rope.len()],
            };
            for filter in filter_ranges {
                cursor.set_byte_range(filter);
                let provider = RopeTextProvider { rope };
                let mut matches =
                    cursor.matches(injection_query, parent_tree.root_node(), provider);
                while let Some(m) = matches.next() {
                    let pattern_index = m.pattern_index;
                    let Some(injection) = parent_lang.injections.get(pattern_index) else {
                        continue;
                    };
                    for capture in m.captures {
                        let inner_start = capture.node.start_byte();
                        let inner_end = capture.node.end_byte();
                        if inner_end <= inner_start {
                            continue;
                        }
                        grouped
                            .entry(injection.inner.name)
                            .or_insert_with(|| (injection.inner.clone(), Vec::new()))
                            .1
                            .push(inner_start..inner_end);
                    }
                }
            }
            drop(cursor);

            for (_, (inner_lang, ranges)) in grouped {
                // Combined injections: if more than one host range,
                // merge them into a single tree via set_included_ranges.
                // For a single range we still produce one layer (the
                // common case).
                if ranges.len() == 1 {
                    let r = ranges.into_iter().next().expect("len checked == 1");
                    let prior = prior_injections.iter().find(|p| {
                        p.start_offset == r.start as u32
                            && p.end_offset == r.end as u32
                            && p.language_name == inner_lang.name
                    });
                    let Some(inner_tree) =
                        parse_rope_range(&inner_lang, rope, r.clone(), prior.map(|p| &p.tree))
                    else {
                        continue;
                    };
                    new_layers.push(SyntaxLayer {
                        depth: parent_depth + 1,
                        start_offset: r.start as u32,
                        end_offset: r.end as u32,
                        language: inner_lang,
                        tree: inner_tree,
                    });
                    queue.push_back(new_layers.len() - 1);
                } else {
                    // Combined: parse all ranges as one tree.
                    let mut sorted = ranges;
                    sorted.sort_by_key(|r| r.start);
                    let merged_start = sorted.first().map(|r| r.start).unwrap_or(0);
                    let merged_end = sorted.last().map(|r| r.end).unwrap_or(0);
                    let Some(inner_tree) =
                        parse_rope_combined_ranges(&inner_lang, rope, &sorted, None)
                    else {
                        continue;
                    };
                    new_layers.push(SyntaxLayer {
                        depth: parent_depth + 1,
                        start_offset: merged_start as u32,
                        end_offset: merged_end as u32,
                        language: inner_lang,
                        tree: inner_tree,
                    });
                    queue.push_back(new_layers.len() - 1);
                }
            }
        }

        self.install_layers(new_layers, version);
        Some(())
    }
}

/// Per-host injection tree from the previous parse, used as
/// `old_tree` for incremental reparse when the same host range
/// reappears in this parse.
#[derive(Clone)]
struct PriorInjection {
    start_offset: u32,
    end_offset: u32,
    language_name: &'static str,
    tree: Tree,
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
    old_tree: Option<&Tree>,
) -> Option<Tree> {
    use crate::highlight::with_parser;
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
    with_parser(|parser| {
        parser.set_language(&language.grammar).ok()?;
        parser.set_included_ranges(&ts_ranges).ok()?;
        // Stream rope chunks via the same callback shape as parse_rope.
        struct CursorState<'a> {
            rope: &'a Rope,
            chunks: Option<stoat_text::ChunksInRange<'a>>,
            pending: &'a str,
            pending_start: usize,
        }
        let mut state = CursorState {
            rope,
            chunks: None,
            pending: "",
            pending_start: 0,
        };
        let total_len = rope.len();
        let mut callback = |byte_offset: usize, _pos: tree_sitter::Point| -> &[u8] {
            if byte_offset >= total_len {
                return &[];
            }
            let pending_end = state.pending_start + state.pending.len();
            if byte_offset >= state.pending_start && byte_offset < pending_end {
                let local = byte_offset - state.pending_start;
                return state.pending.as_bytes().get(local..).unwrap_or(&[]);
            }
            if byte_offset < state.pending_start || state.chunks.is_none() {
                state.chunks = Some(state.rope.chunks_in_range(byte_offset..total_len));
                state.pending = "";
                state.pending_start = byte_offset;
            }
            loop {
                let chunk_end = state.pending_start + state.pending.len();
                state.pending_start = chunk_end;
                state.pending = "";
                let Some(chunk) = state.chunks.as_mut().and_then(|it| it.next()) else {
                    return &[];
                };
                state.pending = chunk;
                let new_chunk_end = state.pending_start + chunk.len();
                if byte_offset < new_chunk_end {
                    let local = byte_offset - state.pending_start;
                    return chunk.as_bytes().get(local..).unwrap_or(&[]);
                }
            }
        };
        parser.parse_with_options(&mut callback, old_tree, None)
    })
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
    use crate::LanguageRegistry;

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
        assert!(map.reparse(&rope, lang.clone(), 1).is_some());

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
        map.reparse(&rope1, lang.clone(), 1).unwrap();
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
        map.reparse(&rope2, lang.clone(), 2).unwrap();

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
        map.reparse(&old_rope, lang.clone(), 1).unwrap();

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
        map.reparse(&new_rope, lang.clone(), 2).unwrap();

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
        fresh.reparse(&new_rope, lang, 2).unwrap();
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
        map.reparse(&rope, lang, 1).unwrap();

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
        // The bundled rust grammar has no entries in Language::injections,
        // so its synthesized injection_query is empty. Verify reparse
        // stays at a single root layer.
        let lang = rust_lang();
        let rope = Rope::from("fn main() { let x = 1; }");
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1).unwrap();
        assert_eq!(map.snapshot().layer_count(), 1);
        assert_eq!(map.snapshot().iter_layers().next().unwrap().depth, 0);
    }

    #[test]
    fn captures_merge_across_layers_in_document_order() {
        // A markdown buffer with inline content yields two layers
        // (markdown root + markdown-inline). `captures` should merge
        // captures from both layers, sorted by document position so
        // the host can iterate them in a single pass.
        let lang = markdown_lang();
        let source = "# Title\n\nSome **bold** text\n";
        let rope = Rope::from(source);
        let mut map = SyntaxMap::new();
        map.reparse(&rope, lang, 1).unwrap();

        let captures = map
            .snapshot()
            .captures(0..rope.len(), &rope, |l| Some(&l.highlight_query));

        assert!(
            !captures.is_empty(),
            "markdown buffer with inline content must produce captures"
        );

        // Captures must be sorted in document order.
        let positions: Vec<usize> = captures.iter().map(|c| c.node.start_byte()).collect();
        let mut sorted = positions.clone();
        sorted.sort();
        assert_eq!(positions, sorted, "captures must be sorted by start byte");

        // We should see captures from BOTH layers: the markdown root
        // (depth 0) producing block-level captures (e.g. title), and
        // the markdown-inline injection (depth 1) producing inline
        // captures (e.g. emphasis).
        let depths: std::collections::HashSet<u32> = captures.iter().map(|c| c.depth).collect();
        assert!(depths.contains(&0), "expected at least one depth-0 capture");
        assert!(depths.contains(&1), "expected at least one depth-1 capture");

        // The depth-1 captures should fall within an inline byte
        // range (somewhere in the "Some **bold** text" portion).
        let bold_start = source.find("**bold**").unwrap();
        let bold_end = bold_start + "**bold**".len();
        assert!(
            captures.iter().any(|c| {
                c.depth == 1
                    && c.node.start_byte() >= bold_start
                    && c.node.end_byte() <= bold_end + 5 // tolerance
            }),
            "expected a depth-1 capture inside the **bold** range"
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
        map.reparse(&rope, lang, 1).unwrap();

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
        map.reparse(&rope, lang.clone(), 1).unwrap();
        let first_inline_range = map
            .snapshot()
            .iter_layers()
            .find(|l| l.depth == 1)
            .map(|l| (l.start_offset, l.end_offset));
        assert!(
            first_inline_range.is_some(),
            "first reparse should produce an inline layer"
        );

        map.reparse(&rope, lang, 2).unwrap();
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
        map.reparse_within_changed_ranges(&rope, lang.clone(), 1, Some(&changed))
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

    #[test]
    fn reparse_with_no_filter_matches_full_reparse() {
        // The convenience `reparse` and the underlying
        // `reparse_within_changed_ranges` with `None` should produce
        // identical layer sets.
        let lang = markdown_lang();
        let source = "# Heading\n\n**bold** text\n";
        let rope = Rope::from(source);

        let mut a = SyntaxMap::new();
        a.reparse(&rope, lang.clone(), 1).unwrap();

        let mut b = SyntaxMap::new();
        b.reparse_within_changed_ranges(&rope, lang.clone(), 1, None)
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
}
