//! Dijkstra shortest-path search over the structural-diff edit graph.
//!
//! Reference: `references/difftastic/src/diff/dijkstra.rs`. The
//! algorithm is identical (priority queue + visited map + path
//! reconstruction); we use [`std::collections::BinaryHeap`] instead
//! of `radix_heap::RadixHeapMap` to avoid an external dependency.
//! BinaryHeap's `O(log n)` ops are fast enough for the bounded graph
//! sizes the structural-diff fallback handles.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    graph::{neighbours, start_vertex, Edge, Vertex},
    unchanged::{ChangeKind, ChangeMap},
};
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    sync::Arc,
};

/// Outcome of a structural-diff search. `ExceededGraphLimit` means
/// the search saw more vertices than `graph_limit` allowed; the
/// caller should fall back to a coarser diff (e.g. line diff).
pub enum SearchOutcome {
    Found(Vec<(Edge, Arc<Vertex>)>),
    ExceededGraphLimit,
}

/// Default graph cap. Difftastic uses 3,000,000; we use 250,000 for a
/// minimum-viable port that handles small inputs and bails fast on
/// large ones. The fallback path is always available.
pub const DEFAULT_GRAPH_LIMIT: usize = 250_000;

/// One variant entry within a shallow-key bucket. Difftastic allows
/// up to 2 variants per shallow `(lhs, rhs, top_parent)` key so the
/// search can explore distinct deep nesting stacks without the full
/// exponential blow-up.
struct VariantState {
    /// Hash of the FULL parents stack via [`Vertex::deep_parents_hash`].
    /// Distinguishes variants that share the shallow key.
    parents_signature: u64,
    distance: u32,
    predecessor: Option<(Edge, Arc<Vertex>)>,
}

const MAX_VARIANTS_PER_KEY: usize = 2;

/// Run Dijkstra from the start vertex (the root pair) to the end
/// vertex (both sides exhausted, empty parents stack). Returns the
/// edge sequence in start-to-end order, or `ExceededGraphLimit` if
/// the visited set grew past `graph_limit`.
///
/// Vertex deduplication: the seen-set is keyed by [`Vertex`]'s
/// shallow eq/hash (lhs, rhs, top of parents stack), but each bucket
/// holds up to 2 distinct full-parents-stack variants. Mirrors
/// `references/difftastic/src/diff/graph.rs:363-410`
/// `allocate_if_new`.
pub fn shortest_path(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_root: SyntaxId,
    rhs_root: SyntaxId,
    graph_limit: usize,
) -> SearchOutcome {
    let start = Arc::new(start_vertex(lhs_root, rhs_root));

    // Bucketed state. The key uses Vertex's shallow eq+hash; the
    // value holds up to MAX_VARIANTS_PER_KEY variants with the same
    // shallow key but distinct deep parents stacks.
    let mut state: HashMap<Vertex, Vec<VariantState>> = HashMap::new();
    state.insert(
        (*start).clone(),
        vec![VariantState {
            parents_signature: start.deep_parents_hash(),
            distance: 0,
            predecessor: None,
        }],
    );

    let mut heap: BinaryHeap<Reverse<HeapEntry>> = BinaryHeap::new();
    heap.push(Reverse(HeapEntry {
        distance: 0,
        vertex: start.clone(),
    }));

    let mut total_variants: usize = 1;

    while let Some(Reverse(HeapEntry { distance, vertex })) = heap.pop() {
        if vertex.is_end() {
            return SearchOutcome::Found(reconstruct_path(&state, &vertex));
        }
        // Stale heap entry from a later, better-distance push? Skip.
        let vertex_signature = vertex.deep_parents_hash();
        if let Some(bucket) = state.get(vertex.as_ref()) {
            if let Some(variant) = bucket
                .iter()
                .find(|v| v.parents_signature == vertex_signature)
            {
                if variant.distance < distance {
                    continue;
                }
            }
        }

        for (edge, next) in neighbours(lhs_arena, rhs_arena, vertex.as_ref()) {
            let next_distance = distance.saturating_add(edge.cost());
            let next_arc = Arc::new(next);
            let next_signature = next_arc.deep_parents_hash();
            let bucket = state.entry((*next_arc).clone()).or_default();

            // Try to find an existing variant with the same deep
            // parents signature.
            if let Some(variant) = bucket
                .iter_mut()
                .find(|v| v.parents_signature == next_signature)
            {
                if next_distance < variant.distance {
                    variant.distance = next_distance;
                    variant.predecessor = Some((edge, vertex.clone()));
                    heap.push(Reverse(HeapEntry {
                        distance: next_distance,
                        vertex: next_arc,
                    }));
                }
                continue;
            }

            // No matching variant: append if we have room. The cap
            // of MAX_VARIANTS_PER_KEY bounds the worst-case branching
            // for deeply-nested adversarial inputs.
            if bucket.len() < MAX_VARIANTS_PER_KEY {
                bucket.push(VariantState {
                    parents_signature: next_signature,
                    distance: next_distance,
                    predecessor: Some((edge, vertex.clone())),
                });
                total_variants += 1;
                heap.push(Reverse(HeapEntry {
                    distance: next_distance,
                    vertex: next_arc,
                }));
            }
            // Bucket full and the new vertex didn't match either
            // existing variant: drop it. The search continues with
            // the variants it has.
        }

        if total_variants > graph_limit {
            return SearchOutcome::ExceededGraphLimit;
        }
    }

    // Heap exhausted without finding the end. Should not happen for
    // well-formed inputs because there's always at least one outgoing
    // edge until we reach the end vertex; but defensively return as
    // exceeded so the caller falls back.
    SearchOutcome::ExceededGraphLimit
}

#[derive(Clone)]
struct HeapEntry {
    distance: u32,
    vertex: Arc<Vertex>,
}

impl PartialEq for HeapEntry {
    fn eq(&self, other: &Self) -> bool {
        self.distance == other.distance
    }
}

impl Eq for HeapEntry {}

impl Ord for HeapEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.distance.cmp(&other.distance)
    }
}

impl PartialOrd for HeapEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

fn reconstruct_path(
    state: &HashMap<Vertex, Vec<VariantState>>,
    end: &Arc<Vertex>,
) -> Vec<(Edge, Arc<Vertex>)> {
    let mut out: Vec<(Edge, Arc<Vertex>)> = Vec::new();
    let mut current: Arc<Vertex> = end.clone();
    loop {
        let signature = current.deep_parents_hash();
        let Some(bucket) = state.get(current.as_ref()) else {
            break;
        };
        let Some(variant) = bucket.iter().find(|v| v.parents_signature == signature) else {
            break;
        };
        let Some((edge, prev)) = &variant.predecessor else {
            break;
        };
        out.push((*edge, prev.clone()));
        current = prev.clone();
    }
    out.reverse();
    out
}

/// Walk the resolved path and tag every node visited by an
/// `UnchangedNode` or `EnterUnchangedDelimiter` edge as
/// [`ChangeKind::Unchanged`] in the corresponding side's [`ChangeMap`].
/// Nodes touched by Novel edges are left as `Pending`; the caller's
/// downstream pass converts them into `DiffChange` byte ranges.
///
/// Mirrors `references/difftastic/src/diff/graph.rs:796-847`
/// `populate_change_map`.
pub fn populate_change_map(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    path: &[(Edge, Arc<Vertex>)],
    lhs_changes: &mut ChangeMap,
    rhs_changes: &mut ChangeMap,
) {
    for (edge, predecessor) in path {
        match edge {
            Edge::UnchangedNode { .. } => {
                if let Some(lhs_id) = predecessor.lhs_syntax {
                    mark_subtree(lhs_arena, lhs_id, lhs_changes, ChangeKind::Unchanged);
                }
                if let Some(rhs_id) = predecessor.rhs_syntax {
                    mark_subtree(rhs_arena, rhs_id, rhs_changes, ChangeKind::Unchanged);
                }
            },
            Edge::EnterUnchangedDelimiter { .. } => {
                if let Some(lhs_id) = predecessor.lhs_syntax {
                    lhs_changes.mark(lhs_id, ChangeKind::Unchanged);
                }
                if let Some(rhs_id) = predecessor.rhs_syntax {
                    rhs_changes.mark(rhs_id, ChangeKind::Unchanged);
                }
            },
            Edge::ReplacedComment { .. } | Edge::ReplacedString { .. } => {
                // The replacement edge says "treat these two atoms as
                // a structural pair, not as two separate Novel runs."
                // We mark both sides Unchanged so collect_changes
                // skips them; the diff renderer will surface the
                // similarity via a separate channel in a follow-up.
                // The current minimum-viable consumer just sees them
                // as unchanged (which is still better than two
                // mis-aligned Novel runs).
                if let Some(lhs_id) = predecessor.lhs_syntax {
                    lhs_changes.mark(lhs_id, ChangeKind::Unchanged);
                }
                if let Some(rhs_id) = predecessor.rhs_syntax {
                    rhs_changes.mark(rhs_id, ChangeKind::Unchanged);
                }
            },
            Edge::NovelAtomLHS | Edge::EnterNovelDelimiterLHS => {
                // Leave LHS as Pending; the downstream collector will
                // emit a Novel DiffChange.
                let _ = predecessor.lhs_syntax;
            },
            Edge::NovelAtomRHS | Edge::EnterNovelDelimiterRHS => {
                let _ = predecessor.rhs_syntax;
            },
        }
    }
}

fn mark_subtree(arena: &SyntaxArena, id: SyntaxId, changes: &mut ChangeMap, kind: ChangeKind) {
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        changes.mark(current, kind);
        if let Syntax::List(list) = arena.get(current) {
            stack.extend(list.children.iter().copied());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        parse,
        structural_diff::{lower_tree, ChangeMap},
        LanguageRegistry,
    };

    fn rust_lang() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .unwrap()
    }

    fn lower(source: &str) -> (SyntaxArena, SyntaxId) {
        let lang = rust_lang();
        let tree = parse(&lang, source, None).unwrap();
        lower_tree(&tree, source)
    }

    #[test]
    fn identical_inputs_emit_unchanged_path() {
        let source = "fn main() {}";
        let (lhs_arena, lhs_root) = lower(source);
        let (rhs_arena, rhs_root) = lower(source);
        let outcome = shortest_path(
            &lhs_arena,
            &rhs_arena,
            lhs_root,
            rhs_root,
            DEFAULT_GRAPH_LIMIT,
        );
        let path = match outcome {
            SearchOutcome::Found(p) => p,
            SearchOutcome::ExceededGraphLimit => panic!("graph limit hit on trivial input"),
        };
        // The path should consist entirely of UnchangedNode edges
        // (the start vertex matches the root pair via content_id and
        // pop_all_parents takes both to the end).
        assert!(!path.is_empty());
        assert!(path
            .iter()
            .all(|(e, _)| matches!(e, Edge::UnchangedNode { .. })));
    }

    #[test]
    fn distinct_inputs_emit_novel_path() {
        let lhs = "fn alpha() {}";
        let rhs = "fn beta() {}";
        let (lhs_arena, lhs_root) = lower(lhs);
        let (rhs_arena, rhs_root) = lower(rhs);
        let outcome = shortest_path(
            &lhs_arena,
            &rhs_arena,
            lhs_root,
            rhs_root,
            DEFAULT_GRAPH_LIMIT,
        );
        let path = match outcome {
            SearchOutcome::Found(p) => p,
            SearchOutcome::ExceededGraphLimit => panic!("graph limit hit on trivial input"),
        };
        // The path must include at least one Novel edge.
        assert!(path.iter().any(|(e, _)| matches!(
            e,
            Edge::NovelAtomLHS
                | Edge::NovelAtomRHS
                | Edge::EnterNovelDelimiterLHS
                | Edge::EnterNovelDelimiterRHS
        )));
    }

    #[test]
    fn populate_change_map_marks_unchanged_for_identical_inputs() {
        let source = "fn main() { let x = 1; }";
        let (lhs_arena, lhs_root) = lower(source);
        let (rhs_arena, rhs_root) = lower(source);
        let outcome = shortest_path(
            &lhs_arena,
            &rhs_arena,
            lhs_root,
            rhs_root,
            DEFAULT_GRAPH_LIMIT,
        );
        let path = match outcome {
            SearchOutcome::Found(p) => p,
            SearchOutcome::ExceededGraphLimit => panic!("limit hit"),
        };
        let mut lhs_changes = ChangeMap::new();
        let mut rhs_changes = ChangeMap::new();
        populate_change_map(
            &lhs_arena,
            &rhs_arena,
            &path,
            &mut lhs_changes,
            &mut rhs_changes,
        );
        // The root pair should be marked Unchanged on both sides.
        assert_eq!(lhs_changes.get(lhs_root), ChangeKind::Unchanged);
        assert_eq!(rhs_changes.get(rhs_root), ChangeKind::Unchanged);
    }
}
