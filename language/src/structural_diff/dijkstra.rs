//! Dijkstra shortest-path search over the structural-diff edit graph.
//!
//! Reference: `references/difftastic/src/diff/dijkstra.rs` and
//! `graph.rs`. Every vertex's position, tentative distance, predecessor,
//! and cached successor edges live together in an index-based
//! [`VertexArena`]. The search dedups vertices through a shallow-key seen
//! map. This mirrors difftastic's intrusive-arena design, with arena
//! indices standing in for its `bumpalo` back-references.
//!
//! [`std::collections::BinaryHeap`] replaces `radix_heap::RadixHeapMap`;
//! its `O(log n)` ops are fast enough for the bounded graph sizes the
//! fallback handles.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    fx::FxBuildHasher,
    graph::{
        is_comment_atom, is_string_atom, levenshtein_pct, pop_all_parents, probably_punctuation,
        push_lhs_delimiter, push_rhs_delimiter, Edge, EnteredDelimiter,
    },
    stack::Stack,
    unchanged::{ChangeKind, ChangeMap},
};
use smallvec::SmallVec;
use std::{
    cmp::Reverse,
    collections::{BinaryHeap, HashMap},
    sync::atomic::{AtomicBool, Ordering},
};

/// Interval at which [`shortest_path`] polls the cancellation flag. A
/// power of two so the check is a cheap `& mask` rather than a modulo.
/// Tuned large enough that the atomic load never dominates the search
/// cost (a few microseconds of extra work per cancel response) and
/// small enough that a cancel takes visibly less than a tick (100ms)
/// even on the biggest graphs we'd run.
const CANCEL_POLL_INTERVAL: usize = 4096;

/// Default graph cap, matching difftastic's default. It bounds the vertex
/// arena so that a pathological single-section rewrite -- one changed
/// region too large to diff structurally -- degrades to
/// preprocessing-only instead of searching an unbounded graph.
///
/// Per-section search keeps ordinary edits orders of magnitude below this,
/// so only a worst case ever reaches the cap. The fallback path is always
/// available when it does.
pub const DEFAULT_GRAPH_LIMIT: usize = 3_000_000;

/// Distinct full-parents-stack variants explored per shallow key.
/// Difftastic caps at 2 so the search can tell popping both delimiters
/// together apart from popping each independently, without the
/// exponential blow-up of tracking every deeper nesting.
const MAX_VARIANTS_PER_KEY: usize = 2;

/// Outcome of a structural-diff search. `ExceededGraphLimit` covers
/// both the honest budget-exceeded case and caller-driven cancellation
/// via the `cancel` parameter. Downstream callers handle both by falling
/// back to a coarser diff (e.g. line diff).
pub enum SearchOutcome {
    Found(Vec<PathStep>),
    ExceededGraphLimit,
}

/// One edge on the resolved shortest path, tagged with the syntax
/// positions of the vertex it left from.
///
/// [`populate_change_map`] reads `lhs`/`rhs` to learn which nodes the
/// edge acted on. These are the predecessor vertex's positions, not the
/// successor's, because an edge describes the step taken *from* a node.
pub struct PathStep {
    pub edge: Edge,
    pub lhs: Option<SyntaxId>,
    pub rhs: Option<SyntaxId>,
}

/// Shallow dedup key formed from the syntax-position pair plus the top of
/// the parents stack. Vertices sharing a key are the "same" graph position
/// up to deeper nesting. The seen map keeps up to [`MAX_VARIANTS_PER_KEY`]
/// of them, told apart by full stack equality.
type SeenKey = (Option<SyntaxId>, Option<SyntaxId>, TopKey);

type SeenMap = HashMap<SeenKey, SmallVec<[u32; MAX_VARIANTS_PER_KEY]>, FxBuildHasher>;

/// The top of a parents stack reduced to what the shallow key needs. It
/// carries the delimiter kind and its immediate ids, never the deeper
/// frames.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum TopKey {
    None,
    Both(SyntaxId, SyntaxId),
    Either(Option<SyntaxId>, Option<SyntaxId>),
}

impl TopKey {
    fn from_parents(parents: &Stack<EnteredDelimiter>) -> Self {
        match parents.peek() {
            None => TopKey::None,
            Some(EnteredDelimiter::PopBoth(lhs, rhs)) => TopKey::Both(*lhs, *rhs),
            Some(EnteredDelimiter::PopEither {
                lhs_delims,
                rhs_delims,
            }) => TopKey::Either(lhs_delims.peek().copied(), rhs_delims.peek().copied()),
        }
    }
}

/// One position in the diff state space plus its live Dijkstra state.
///
/// `distance` starts at [`u32::MAX`] (unreached). `predecessor` records
/// the edge and vertex the shortest known route arrived by. `neighbours`
/// caches the half-open range of [`VertexArena::edges`] generated for
/// this vertex, so re-popping it never regenerates them.
struct VertexState {
    lhs_syntax: Option<SyntaxId>,
    rhs_syntax: Option<SyntaxId>,
    parents: Stack<EnteredDelimiter>,
    distance: u32,
    predecessor: Option<(Edge, u32)>,
    neighbours: Option<(u32, u32)>,
}

/// Index-based store of every vertex reached and every successor edge
/// generated. Vertices reference each other by their `u32` index into
/// `vertices`; `edges` is a flat pool sliced by each vertex's cached
/// `neighbours` range.
struct VertexArena {
    vertices: Vec<VertexState>,
    edges: Vec<(Edge, u32)>,
}

impl VertexArena {
    fn new() -> Self {
        VertexArena {
            vertices: Vec::new(),
            edges: Vec::new(),
        }
    }

    fn alloc(
        &mut self,
        lhs_syntax: Option<SyntaxId>,
        rhs_syntax: Option<SyntaxId>,
        parents: Stack<EnteredDelimiter>,
    ) -> u32 {
        let id = self.vertices.len() as u32;
        self.vertices.push(VertexState {
            lhs_syntax,
            rhs_syntax,
            parents,
            distance: u32::MAX,
            predecessor: None,
            neighbours: None,
        });
        id
    }

    fn is_end(&self, id: u32) -> bool {
        let v = &self.vertices[id as usize];
        v.lhs_syntax.is_none() && v.rhs_syntax.is_none() && v.parents.is_empty()
    }
}

/// Run Dijkstra from the start vertex (the root pair) to the end vertex
/// (both sides exhausted, empty parents stack). Returns the edge
/// sequence in start-to-end order, or `ExceededGraphLimit` if the vertex
/// arena grew past `graph_limit`.
///
/// Vertices are deduplicated by a shallow `(lhs, rhs, top_parent)`
/// [`SeenKey`], but each entry holds up to [`MAX_VARIANTS_PER_KEY`]
/// distinct full-parents-stack variants.
/// Mirrors `references/difftastic/src/diff/graph.rs:363-410`
/// `allocate_if_new`.
pub fn shortest_path(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_root: Option<SyntaxId>,
    rhs_root: Option<SyntaxId>,
    graph_limit: usize,
    cancel: Option<&AtomicBool>,
) -> SearchOutcome {
    let mut va = VertexArena::new();
    let mut seen: SeenMap = HashMap::default();

    let start = va.alloc(lhs_root, rhs_root, Stack::new());
    va.vertices[start as usize].distance = 0;

    let mut heap: BinaryHeap<Reverse<(u32, u32)>> = BinaryHeap::new();
    heap.push(Reverse((0, start)));

    let mut expansions: usize = 0;

    while let Some(Reverse((distance, vid))) = heap.pop() {
        if va.is_end(vid) {
            return SearchOutcome::Found(reconstruct_path(&va, vid));
        }
        // A later, shorter route to vid already won, making this stale.
        if distance > va.vertices[vid as usize].distance {
            continue;
        }

        expansions += 1;
        if expansions & (CANCEL_POLL_INTERVAL - 1) == 0
            && let Some(flag) = cancel
            && flag.load(Ordering::Relaxed)
        {
            return SearchOutcome::ExceededGraphLimit;
        }

        expand(lhs_arena, rhs_arena, &mut va, &mut seen, vid);

        let (edge_start, edge_end) = va.vertices[vid as usize]
            .neighbours
            .expect("expand sets the neighbours range");
        for edge_idx in edge_start..edge_end {
            let (edge, succ) = va.edges[edge_idx as usize];
            let next_distance = distance.saturating_add(edge.cost());
            if next_distance < va.vertices[succ as usize].distance {
                va.vertices[succ as usize].distance = next_distance;
                va.vertices[succ as usize].predecessor = Some((edge, vid));
                heap.push(Reverse((next_distance, succ)));
            }
        }

        if va.vertices.len() > graph_limit {
            return SearchOutcome::ExceededGraphLimit;
        }
    }

    // Heap exhausted without reaching the end. A well-formed input always
    // has an outgoing edge until the end vertex, so treat this defensively
    // as exceeded and let the caller fall back.
    SearchOutcome::ExceededGraphLimit
}

/// Generate and intern every outgoing edge from vertex `vid`, caching the
/// resulting [`VertexArena::edges`] range on the vertex so a re-pop
/// returns immediately.
///
/// Mirrors `references/difftastic/src/diff/graph.rs:493-794`
/// `set_neighbours`.
fn expand(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    va: &mut VertexArena,
    seen: &mut SeenMap,
    vid: u32,
) {
    if va.vertices[vid as usize].neighbours.is_some() {
        return;
    }

    let lhs_syntax = va.vertices[vid as usize].lhs_syntax;
    let rhs_syntax = va.vertices[vid as usize].rhs_syntax;
    let parents = va.vertices[vid as usize].parents.clone();

    let edge_start = va.edges.len() as u32;

    match (lhs_syntax, rhs_syntax) {
        (Some(lhs_id), Some(rhs_id)) => {
            let lhs_node = lhs_arena.get(lhs_id);
            let rhs_node = rhs_arena.get(rhs_id);

            if lhs_node.content_id() == rhs_node.content_id() {
                // Identical nodes match and advance both sides. No other
                // edge can beat a same-content match, so stop here.
                let edge = Edge::UnchangedNode {
                    depth_difference: 0,
                    probably_punctuation: probably_punctuation(lhs_node),
                };
                let (lhs_after, rhs_after, parents_after) = pop_all_parents(
                    lhs_arena,
                    rhs_arena,
                    lhs_node.next_sibling(),
                    rhs_node.next_sibling(),
                    parents,
                );
                push_edge(va, seen, edge, lhs_after, rhs_after, parents_after);
            } else {
                // Case 2: both sides are lists of the same kind. Enter
                // their children together via PopBoth.
                if let (Syntax::List(lhs_list), Syntax::List(rhs_list)) = (lhs_node, rhs_node)
                    && lhs_list.kind == rhs_list.kind
                {
                    let edge = Edge::EnterUnchangedDelimiter {
                        depth_difference: 0,
                    };
                    let entered = parents.push(EnteredDelimiter::PopBoth(lhs_id, rhs_id));
                    let (lhs_after, rhs_after, parents_after) = pop_all_parents(
                        lhs_arena,
                        rhs_arena,
                        lhs_list.children.first().copied(),
                        rhs_list.children.first().copied(),
                        entered,
                    );
                    push_edge(va, seen, edge, lhs_after, rhs_after, parents_after);
                }

                // Case 2b: comment- or string-kind atoms with similar
                // content pair via a Replaced* edge, so a tweaked
                // comment/string does not show as two unrelated Novel runs.
                if let (Syntax::Atom(lhs_atom), Syntax::Atom(rhs_atom)) = (lhs_node, rhs_node) {
                    // The kind check is a cheap enum compare, while
                    // Levenshtein is an O(n*m) DP. Gate on the kind first so
                    // only same-kind comment/string pairs pay the similarity
                    // cost -- code atoms, the common case, never match here.
                    let kind_match_comment = is_comment_atom(lhs_node) && is_comment_atom(rhs_node);
                    let kind_match_string = is_string_atom(lhs_node) && is_string_atom(rhs_node);
                    if kind_match_comment || kind_match_string {
                        let levenshtein = levenshtein_pct(lhs_atom.content, rhs_atom.content);
                        if levenshtein > 20 {
                            let edge = if kind_match_comment {
                                Edge::ReplacedComment {
                                    levenshtein_pct: levenshtein,
                                }
                            } else {
                                Edge::ReplacedString {
                                    levenshtein_pct: levenshtein,
                                }
                            };
                            let (lhs_after, rhs_after, parents_after) = pop_all_parents(
                                lhs_arena,
                                rhs_arena,
                                lhs_atom.next_sibling,
                                rhs_atom.next_sibling,
                                parents.clone(),
                            );
                            push_edge(va, seen, edge, lhs_after, rhs_after, parents_after);
                        }
                    }
                }

                // Cases 3 & 4: treat the node on one side as novel.
                expand_novel_lhs(lhs_arena, rhs_arena, va, seen, lhs_id, rhs_syntax, &parents);
                expand_novel_rhs(lhs_arena, rhs_arena, va, seen, rhs_id, lhs_syntax, &parents);
            }
        },
        (Some(lhs_id), None) => {
            expand_novel_lhs(lhs_arena, rhs_arena, va, seen, lhs_id, None, &parents);
        },
        (None, Some(rhs_id)) => {
            expand_novel_rhs(lhs_arena, rhs_arena, va, seen, rhs_id, None, &parents);
        },
        (None, None) => {
            // Both sides exhausted at this level. pop_all_parents unwinds
            // any remaining frames. Emit the step only if it makes progress.
            let (lhs_next, rhs_next, parents_after) =
                pop_all_parents(lhs_arena, rhs_arena, None, None, parents.clone());
            if lhs_next.is_some() || rhs_next.is_some() || parents_after.len() < parents.len() {
                let edge = Edge::UnchangedNode {
                    depth_difference: 0,
                    probably_punctuation: false,
                };
                push_edge(va, seen, edge, lhs_next, rhs_next, parents_after);
            }
        },
    }

    let edge_end = va.edges.len() as u32;
    va.vertices[vid as usize].neighbours = Some((edge_start, edge_end));
}

/// Emit the "novel on lhs" step: skip the lhs node (descending into it if
/// it is a list) while the rhs position stays put.
fn expand_novel_lhs(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    va: &mut VertexArena,
    seen: &mut SeenMap,
    lhs_id: SyntaxId,
    rhs_syntax: Option<SyntaxId>,
    parents: &Stack<EnteredDelimiter>,
) {
    let (edge, lhs_next, entered) = match lhs_arena.get(lhs_id) {
        Syntax::Atom(atom) => (Edge::NovelAtomLHS, atom.next_sibling, parents.clone()),
        Syntax::List(list) => (
            Edge::EnterNovelDelimiterLHS,
            list.children.first().copied(),
            push_lhs_delimiter(parents, lhs_id),
        ),
    };
    let (lhs_after, rhs_after, parents_after) =
        pop_all_parents(lhs_arena, rhs_arena, lhs_next, rhs_syntax, entered);
    push_edge(va, seen, edge, lhs_after, rhs_after, parents_after);
}

/// The rhs counterpart of [`expand_novel_lhs`].
fn expand_novel_rhs(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    va: &mut VertexArena,
    seen: &mut SeenMap,
    rhs_id: SyntaxId,
    lhs_syntax: Option<SyntaxId>,
    parents: &Stack<EnteredDelimiter>,
) {
    let (edge, rhs_next, entered) = match rhs_arena.get(rhs_id) {
        Syntax::Atom(atom) => (Edge::NovelAtomRHS, atom.next_sibling, parents.clone()),
        Syntax::List(list) => (
            Edge::EnterNovelDelimiterRHS,
            list.children.first().copied(),
            push_rhs_delimiter(parents, rhs_id),
        ),
    };
    let (lhs_after, rhs_after, parents_after) =
        pop_all_parents(lhs_arena, rhs_arena, lhs_syntax, rhs_next, entered);
    push_edge(va, seen, edge, lhs_after, rhs_after, parents_after);
}

/// Intern a successor and record the edge into `va.edges`.
fn push_edge(
    va: &mut VertexArena,
    seen: &mut SeenMap,
    edge: Edge,
    lhs: Option<SyntaxId>,
    rhs: Option<SyntaxId>,
    parents: Stack<EnteredDelimiter>,
) {
    let succ = intern(va, seen, lhs, rhs, parents);
    va.edges.push((edge, succ));
}

/// Return the vertex index a successor edge should point at. An existing
/// variant whose full parents stack is equal is reused. A new one is
/// allocated while the shallow key has room. Once [`MAX_VARIANTS_PER_KEY`]
/// variants exist, the last is reused so the search always has a
/// successor. Mirrors difftastic `allocate_if_new`.
fn intern(
    va: &mut VertexArena,
    seen: &mut SeenMap,
    lhs: Option<SyntaxId>,
    rhs: Option<SyntaxId>,
    parents: Stack<EnteredDelimiter>,
) -> u32 {
    let key = (lhs, rhs, TopKey::from_parents(&parents));
    let bucket = seen.entry(key).or_default();

    if bucket.len() >= MAX_VARIANTS_PER_KEY {
        return *bucket.last().expect("a full bucket is non-empty");
    }
    for &existing in bucket.iter() {
        if va.vertices[existing as usize].parents == parents {
            return existing;
        }
    }

    let vid = va.alloc(lhs, rhs, parents);
    bucket.push(vid);
    vid
}

/// Walk `predecessor` back-pointers from the end vertex to the start,
/// producing the edge sequence in start-to-end order. Each step carries
/// the syntax positions of the vertex the edge left from.
fn reconstruct_path(va: &VertexArena, end: u32) -> Vec<PathStep> {
    let mut out: Vec<PathStep> = Vec::new();
    let mut current = end;
    while let Some((edge, prev)) = va.vertices[current as usize].predecessor {
        out.push(PathStep {
            edge,
            lhs: va.vertices[prev as usize].lhs_syntax,
            rhs: va.vertices[prev as usize].rhs_syntax,
        });
        current = prev;
    }
    out.reverse();
    out
}

/// Walk the resolved path and tag every node visited by an
/// `UnchangedNode` or `EnterUnchangedDelimiter` edge as
/// [`ChangeKind::Unchanged`] in the corresponding side's [`ChangeMap`].
/// Nodes touched by Novel edges are left as `Pending`. The caller's
/// downstream pass converts them into `DiffChange` byte ranges.
///
/// Mirrors `references/difftastic/src/diff/graph.rs:796-847`
/// `populate_change_map`.
pub fn populate_change_map(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    path: &[PathStep],
    lhs_changes: &mut ChangeMap,
    rhs_changes: &mut ChangeMap,
) {
    for step in path {
        match step.edge {
            Edge::UnchangedNode { .. } => {
                if let Some(lhs_id) = step.lhs {
                    mark_subtree(lhs_arena, lhs_id, lhs_changes, ChangeKind::Unchanged);
                }
                if let Some(rhs_id) = step.rhs {
                    mark_subtree(rhs_arena, rhs_id, rhs_changes, ChangeKind::Unchanged);
                }
            },
            Edge::EnterUnchangedDelimiter { .. } => {
                if let Some(lhs_id) = step.lhs {
                    lhs_changes.mark(lhs_id, ChangeKind::Unchanged);
                }
                if let Some(rhs_id) = step.rhs {
                    rhs_changes.mark(rhs_id, ChangeKind::Unchanged);
                }
            },
            Edge::ReplacedComment { .. } | Edge::ReplacedString { .. } => {
                // The replacement edge pairs two atoms structurally rather
                // than emitting two Novel runs. Both sides are marked
                // Unchanged so collect_changes skips them. Surfacing the
                // similarity itself is a follow-up concern.
                if let Some(lhs_id) = step.lhs {
                    lhs_changes.mark(lhs_id, ChangeKind::Unchanged);
                }
                if let Some(rhs_id) = step.rhs {
                    rhs_changes.mark(rhs_id, ChangeKind::Unchanged);
                }
            },
            // Novel edges leave the node Pending. The downstream collector
            // emits it as a Novel DiffChange.
            Edge::NovelAtomLHS | Edge::EnterNovelDelimiterLHS => {},
            Edge::NovelAtomRHS | Edge::EnterNovelDelimiterRHS => {},
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
        structural_diff::{arena::Atom, lower_tree, ChangeMap, ContentId},
        LanguageRegistry,
    };
    use std::sync::Arc;

    fn rust_lang() -> Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .unwrap()
    }

    fn lower(source: &str) -> (SyntaxArena, SyntaxId) {
        let lang = rust_lang();
        let tree = parse(&lang, source, None).unwrap();
        lower_tree(&tree, source)
    }

    fn mk_atom(arena: &mut SyntaxArena, kind: &'static str, content: &'static str) -> SyntaxId {
        arena.alloc(Syntax::Atom(Atom {
            kind,
            byte_range: 0..content.len(),
            content,
            content_id: ContentId::for_atom(kind, content),
            next_sibling: None,
        }))
    }

    /// Expand a fresh single-vertex arena and return the kinds of edge it
    /// generates, exercising [`expand`] the way the search loop does.
    fn expanded_edges(
        lhs_arena: &SyntaxArena,
        rhs_arena: &SyntaxArena,
        lhs: Option<SyntaxId>,
        rhs: Option<SyntaxId>,
    ) -> Vec<Edge> {
        let mut va = VertexArena::new();
        let mut seen: SeenMap = HashMap::default();
        let vid = va.alloc(lhs, rhs, Stack::new());
        expand(lhs_arena, rhs_arena, &mut va, &mut seen, vid);
        let (start, end) = va.vertices[vid as usize].neighbours.unwrap();
        (start..end).map(|i| va.edges[i as usize].0).collect()
    }

    #[test]
    fn expand_matching_atoms_yields_unchanged() {
        let mut arena = SyntaxArena::new();
        let lhs = mk_atom(&mut arena, "ident", "x");
        let rhs = mk_atom(&mut arena, "ident", "x");
        let edges = expanded_edges(&arena, &arena, Some(lhs), Some(rhs));
        assert!(edges
            .iter()
            .any(|e| matches!(e, Edge::UnchangedNode { .. })));
    }

    #[test]
    fn expand_distinct_atoms_yields_only_novel() {
        let mut arena = SyntaxArena::new();
        let lhs = mk_atom(&mut arena, "ident", "alpha");
        let rhs = mk_atom(&mut arena, "ident", "beta");
        let edges = expanded_edges(&arena, &arena, Some(lhs), Some(rhs));
        assert!(!edges.is_empty());
        assert!(edges
            .iter()
            .all(|e| matches!(e, Edge::NovelAtomLHS | Edge::NovelAtomRHS)));
    }

    #[test]
    fn expand_similar_comments_yields_replaced() {
        let mut arena = SyntaxArena::new();
        let lhs = mk_atom(&mut arena, "line_comment", "// alpha beta gamma");
        let rhs = mk_atom(&mut arena, "line_comment", "// alpha beta delta");
        let edges = expanded_edges(&arena, &arena, Some(lhs), Some(rhs));
        assert!(edges
            .iter()
            .any(|e| matches!(e, Edge::ReplacedComment { .. })));
    }

    #[test]
    fn end_vertex_is_recognized() {
        let mut va = VertexArena::new();
        let vid = va.alloc(None, None, Stack::new());
        assert!(va.is_end(vid));
    }

    #[test]
    fn shallow_key_ignores_deeper_parents() {
        let empty = TopKey::from_parents(&Stack::new());
        let both = TopKey::from_parents(
            &Stack::new().push(EnteredDelimiter::PopBoth(SyntaxId(1), SyntaxId(2))),
        );
        assert_eq!(empty, TopKey::from_parents(&Stack::new()));
        assert_ne!(empty, both);
    }

    #[test]
    fn identical_inputs_emit_unchanged_path() {
        let source = "fn main() {}";
        let (lhs_arena, lhs_root) = lower(source);
        let (rhs_arena, rhs_root) = lower(source);
        let outcome = shortest_path(
            &lhs_arena,
            &rhs_arena,
            Some(lhs_root),
            Some(rhs_root),
            DEFAULT_GRAPH_LIMIT,
            None,
        );
        let path = match outcome {
            SearchOutcome::Found(p) => p,
            SearchOutcome::ExceededGraphLimit => panic!("graph limit hit on trivial input"),
        };
        assert!(!path.is_empty());
        assert!(path
            .iter()
            .all(|s| matches!(s.edge, Edge::UnchangedNode { .. })));
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
            Some(lhs_root),
            Some(rhs_root),
            DEFAULT_GRAPH_LIMIT,
            None,
        );
        let path = match outcome {
            SearchOutcome::Found(p) => p,
            SearchOutcome::ExceededGraphLimit => panic!("graph limit hit on trivial input"),
        };
        assert!(path.iter().any(|s| matches!(
            s.edge,
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
            Some(lhs_root),
            Some(rhs_root),
            DEFAULT_GRAPH_LIMIT,
            None,
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
        assert_eq!(lhs_changes.get(lhs_root), ChangeKind::Unchanged);
        assert_eq!(rhs_changes.get(rhs_root), ChangeKind::Unchanged);
    }
}
