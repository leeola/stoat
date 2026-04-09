//! Edit graph for the structural-diff Dijkstra search.
//!
//! Each [`Vertex`] is a position in the diff state space: a pair of
//! syntax pointers (one per side, possibly `None` for "exhausted")
//! plus a stack of [`EnteredDelimiter`]s tracking which lists we
//! have descended into. Edges between vertices have a cost; the
//! Dijkstra search finds the minimum-cost path from start to end,
//! and the resulting edge sequence describes the diff.
//!
//! Reference: `references/difftastic/src/diff/graph.rs`. The cost
//! values and edge generation cases are ported verbatim from there;
//! see in-code citations.
//!
//! Differences from Difftastic for this minimum-viable port:
//! - No `ReplacedComment`/`ReplacedString` (Levenshtein matching). Falls back to two `Novel` edges
//!   (one per side).
//! - No punctuation penalty. `probably_punctuation` is always `false`, so the +200 cost doesn't
//!   apply.
//! - No 2-variant vertex deduplication; the seen-set picks the first variant.
//! - No slider correction (separate post-pass, deferred to a follow-up).
//!
//! Required for correctness:
//! - The `EnteredDelimiter::PopBoth` vs `PopEither` distinction. Without `PopBoth`, structural
//!   changes like `(a b c)` vs `(a b) c` are missed.
//! - Vertex equality on the *top of the parents stack only* (shallow). This is intentional and
//!   load-bearing per the Difftastic comments.
//! - `pop_all_parents` semantics for handling list endings.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    stack::Stack,
};
use std::cmp::min;

/// One delimiter pair we have descended into. `PopBoth` means both
/// sides entered the same matching list together (the ideal case);
/// `PopEither` means LHS and RHS entered separate, mismatched lists
/// and each can pop independently.
#[derive(Clone, Debug)]
pub enum EnteredDelimiter {
    /// Both sides entered a matching list. The two payloads are the
    /// LHS and RHS list nodes that were entered together.
    PopBoth(SyntaxId, SyntaxId),
    /// LHS and RHS each have a stack of unmatched delimiters they
    /// have descended into independently. Each side can pop on its
    /// own.
    PopEither {
        lhs_delims: Vec<SyntaxId>,
        rhs_delims: Vec<SyntaxId>,
    },
}

impl PartialEq for EnteredDelimiter {
    fn eq(&self, other: &Self) -> bool {
        // Equality only compares the *kind* and the lhs/rhs delim ids
        // at the top of each stack. The full vector contents are not
        // compared because vertex equality is intentionally shallow
        // (see `Vertex` below).
        match (self, other) {
            (EnteredDelimiter::PopBoth(a1, b1), EnteredDelimiter::PopBoth(a2, b2)) => {
                a1 == a2 && b1 == b2
            },
            (
                EnteredDelimiter::PopEither {
                    lhs_delims: a1,
                    rhs_delims: b1,
                },
                EnteredDelimiter::PopEither {
                    lhs_delims: a2,
                    rhs_delims: b2,
                },
            ) => a1.last() == a2.last() && b1.last() == b2.last(),
            _ => false,
        }
    }
}

impl Eq for EnteredDelimiter {}

impl std::hash::Hash for EnteredDelimiter {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            EnteredDelimiter::PopBoth(a, b) => {
                0u8.hash(state);
                a.hash(state);
                b.hash(state);
            },
            EnteredDelimiter::PopEither {
                lhs_delims,
                rhs_delims,
            } => {
                1u8.hash(state);
                lhs_delims.last().hash(state);
                rhs_delims.last().hash(state);
            },
        }
    }
}

/// One position in the diff state space.
///
/// Equality and hashing are intentionally **shallow** on the
/// `parents` stack: only the top entry participates. The first
/// vertex to reach a `(lhs_id, rhs_id, top_parent)` tuple "wins"
/// the seen-set entry; since Dijkstra explores in cost order, this
/// is also the lowest-cost path to that position.
#[derive(Clone, Debug)]
pub struct Vertex {
    pub lhs_syntax: Option<SyntaxId>,
    pub rhs_syntax: Option<SyntaxId>,
    pub parents: Stack<EnteredDelimiter>,
}

impl Vertex {
    pub fn is_end(&self) -> bool {
        self.lhs_syntax.is_none() && self.rhs_syntax.is_none() && self.parents.is_empty()
    }

    /// Hash the FULL parents stack (not just the shallow top entry
    /// that [`PartialEq`] / [`Hash`] use). Returned value distinguishes
    /// vertices that share the same `(lhs_syntax, rhs_syntax, top_parent)`
    /// shallow key but have different deeper stack contents. Used by
    /// the 2-variant dedup in
    /// [`super::dijkstra::shortest_path`] to allow up to two distinct
    /// nesting variants per shallow key.
    pub fn deep_parents_hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        let mut cur = self.parents.clone();
        while let Some((entry, rest)) = cur.pop() {
            entry.hash(&mut h);
            cur = rest;
        }
        h.finish()
    }
}

impl PartialEq for Vertex {
    fn eq(&self, other: &Self) -> bool {
        self.lhs_syntax == other.lhs_syntax
            && self.rhs_syntax == other.rhs_syntax
            && self.parents.peek() == other.parents.peek()
    }
}

impl Eq for Vertex {}

impl std::hash::Hash for Vertex {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.lhs_syntax.hash(state);
        self.rhs_syntax.hash(state);
        self.parents.peek().hash(state);
    }
}

/// One outgoing edge from a [`Vertex`].
#[derive(Clone, Copy, Debug)]
pub enum Edge {
    /// Both sides have a node with the same `ContentId`. Lowest cost.
    /// `probably_punctuation` adds +200 to the cost so structural
    /// matches (variable names, keywords) win against incidental
    /// matches (commas, semicolons).
    UnchangedNode {
        depth_difference: u32,
        probably_punctuation: bool,
    },
    /// Both sides have a list with matching delimiters; the children
    /// will be diffed inside. Cheap but not free.
    EnterUnchangedDelimiter { depth_difference: u32 },
    /// Both sides have a comment-kind atom whose contents are
    /// similar (Levenshtein-based). The replacement is preferred
    /// over two separate Novel edges when similarity is above 20%.
    /// Cost is 500 + (100 - levenshtein_pct), so range is 500-600.
    ReplacedComment { levenshtein_pct: u8 },
    /// Same as [`Edge::ReplacedComment`] but for string-kind atoms.
    ReplacedString { levenshtein_pct: u8 },
    /// LHS has a leaf with no counterpart on RHS.
    NovelAtomLHS,
    /// RHS has a leaf with no counterpart on LHS.
    NovelAtomRHS,
    /// LHS has a list whose delimiters do not match anything on RHS.
    EnterNovelDelimiterLHS,
    /// RHS has a list whose delimiters do not match anything on LHS.
    EnterNovelDelimiterRHS,
}

impl Edge {
    /// Cost values are ported verbatim from
    /// `references/difftastic/src/diff/graph.rs:316-360`.
    pub fn cost(self) -> u32 {
        match self {
            Edge::UnchangedNode {
                depth_difference,
                probably_punctuation,
            } => {
                let base = min(40, depth_difference + 1);
                base + if probably_punctuation { 200 } else { 0 }
            },
            Edge::EnterUnchangedDelimiter { depth_difference } => 100 + min(40, depth_difference),
            Edge::NovelAtomLHS | Edge::NovelAtomRHS => 300,
            Edge::EnterNovelDelimiterLHS | Edge::EnterNovelDelimiterRHS => 300,
            Edge::ReplacedComment { levenshtein_pct }
            | Edge::ReplacedString { levenshtein_pct } => 500 + u32::from(100 - levenshtein_pct),
        }
    }
}

/// Heuristic: a node is "probably punctuation" if its kind contains
/// `punctuation` (the standard tree-sitter scope name) or if its
/// content is a single non-alphanumeric ASCII character. The exact
/// definition matches Difftastic closely enough that the +200
/// penalty has the intended effect (variable matches win over comma
/// matches).
pub fn probably_punctuation(node: &Syntax<'_>) -> bool {
    match node {
        Syntax::Atom(atom) => {
            let kind = atom.kind;
            if kind.contains("punctuation") {
                return true;
            }
            let trimmed = atom.content.trim();
            // Single ASCII char that isn't alphanumeric: probably a
            // delimiter or operator.
            trimmed.len() == 1
                && trimmed
                    .as_bytes()
                    .first()
                    .is_some_and(|b| !b.is_ascii_alphanumeric() && b.is_ascii())
        },
        Syntax::List(_) => false,
    }
}

/// Detect a comment-kind atom. Tree-sitter grammars conventionally
/// name comment nodes with `comment` somewhere in the kind.
pub fn is_comment_atom(node: &Syntax<'_>) -> bool {
    match node {
        Syntax::Atom(atom) => atom.kind.contains("comment"),
        Syntax::List(_) => false,
    }
}

/// Detect a string-literal atom. Tree-sitter grammars conventionally
/// name string nodes with `string` in the kind.
pub fn is_string_atom(node: &Syntax<'_>) -> bool {
    match node {
        Syntax::Atom(atom) => atom.kind.contains("string") || atom.kind.contains("char"),
        Syntax::List(_) => false,
    }
}

/// Compute a Levenshtein-distance-based similarity percentage
/// between two strings. Range: 0..=100. Two empty strings return
/// 100. The implementation is the classic O(n*m) DP, plenty fast for
/// the comment-/string-sized inputs the structural-diff sees.
pub fn levenshtein_pct(a: &str, b: &str) -> u8 {
    if a.is_empty() && b.is_empty() {
        return 100;
    }
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = a_chars.len();
    let m = b_chars.len();
    if n == 0 || m == 0 {
        return 0;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr: Vec<usize> = vec![0; m + 1];
    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (curr[j - 1] + 1).min(prev[j] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    let distance = prev[m];
    let max_len = n.max(m);
    let similarity = (max_len.saturating_sub(distance) * 100) / max_len;
    similarity.min(100) as u8
}

/// Build the start vertex for diffing two roots. Both roots are
/// expected to be `Syntax::List` (e.g., the top-level `source_file`
/// node). The start vertex sits at the root pair; the search will
/// match them via [`Edge::UnchangedNode`] or [`Edge::EnterUnchangedDelimiter`]
/// or descend independently via the novel-delimiter edges.
pub fn start_vertex(lhs_root: SyntaxId, rhs_root: SyntaxId) -> Vertex {
    Vertex {
        lhs_syntax: Some(lhs_root),
        rhs_syntax: Some(rhs_root),
        parents: Stack::new(),
    }
}

/// Pop empty list parents. After matching or skipping a node, both
/// pointers may have moved past the end of their current sibling
/// list; this routine pops back up through the parents stack until
/// at least one side has a remaining node, or the stack is empty.
///
/// Returns `(new_lhs_pos, new_rhs_pos, new_parents_stack)`.
///
/// Mirrors `references/difftastic/src/diff/graph.rs:425-489`
/// `pop_all_parents`. The Difftastic version also tracks
/// `lhs_parent_id` / `rhs_parent_id` separately; we fold those into
/// the `EnteredDelimiter` payloads.
pub fn pop_all_parents(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_pos: Option<SyntaxId>,
    rhs_pos: Option<SyntaxId>,
    parents: Stack<EnteredDelimiter>,
) -> (Option<SyntaxId>, Option<SyntaxId>, Stack<EnteredDelimiter>) {
    let mut lhs = lhs_pos;
    let mut rhs = rhs_pos;
    let mut stack = parents;
    loop {
        // If both sides still have a position, no parent can be popped.
        if lhs.is_some() && rhs.is_some() {
            break;
        }
        let Some((top, rest)) = stack.pop() else {
            break;
        };
        match top {
            EnteredDelimiter::PopBoth(lhs_parent, rhs_parent) => {
                if lhs.is_none() && rhs.is_none() {
                    // Both sides exhausted their children: pop the
                    // pair and resume at each parent's next sibling.
                    lhs = lhs_arena.get(*lhs_parent).next_sibling();
                    rhs = rhs_arena.get(*rhs_parent).next_sibling();
                    stack = rest.clone();
                    continue;
                }
                // Asymmetric exhaustion under a `PopBoth` is an
                // illegal state because both sides entered together;
                // they must exit together. Stop the pop chain.
                break;
            },
            EnteredDelimiter::PopEither {
                lhs_delims,
                rhs_delims,
            } => {
                let mut new_lhs_delims = lhs_delims.clone();
                let mut new_rhs_delims = rhs_delims.clone();
                let mut popped_anything = false;
                if lhs.is_none() {
                    if let Some(parent) = new_lhs_delims.pop() {
                        lhs = lhs_arena.get(parent).next_sibling();
                        popped_anything = true;
                    }
                }
                if rhs.is_none() {
                    if let Some(parent) = new_rhs_delims.pop() {
                        rhs = rhs_arena.get(parent).next_sibling();
                        popped_anything = true;
                    }
                }
                if !popped_anything {
                    break;
                }
                if new_lhs_delims.is_empty() && new_rhs_delims.is_empty() {
                    stack = rest.clone();
                } else {
                    stack = rest.push(EnteredDelimiter::PopEither {
                        lhs_delims: new_lhs_delims,
                        rhs_delims: new_rhs_delims,
                    });
                }
                continue;
            },
        }
    }
    (lhs, rhs, stack)
}

/// Generate every outgoing edge from `vertex`, returning a Vec of
/// `(edge, next_vertex)` pairs.
///
/// Mirrors `references/difftastic/src/diff/graph.rs:493-794`
/// `set_neighbours`, simplified to skip `ReplacedComment` and
/// `ReplacedString` edges.
pub fn neighbours(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    vertex: &Vertex,
) -> Vec<(Edge, Vertex)> {
    let mut out: Vec<(Edge, Vertex)> = Vec::new();

    match (vertex.lhs_syntax, vertex.rhs_syntax) {
        (Some(lhs_id), Some(rhs_id)) => {
            let lhs_node = lhs_arena.get(lhs_id);
            let rhs_node = rhs_arena.get(rhs_id);

            // Case 1: nodes have identical content_ids. Match them
            // and advance to next siblings on both sides.
            if lhs_node.content_id() == rhs_node.content_id() {
                let edge = Edge::UnchangedNode {
                    depth_difference: 0,
                    probably_punctuation: probably_punctuation(lhs_node),
                };
                let lhs_next = lhs_node.next_sibling();
                let rhs_next = rhs_node.next_sibling();
                let (lhs_after, rhs_after, parents_after) = pop_all_parents(
                    lhs_arena,
                    rhs_arena,
                    lhs_next,
                    rhs_next,
                    vertex.parents.clone(),
                );
                out.push((
                    edge,
                    Vertex {
                        lhs_syntax: lhs_after,
                        rhs_syntax: rhs_after,
                        parents: parents_after,
                    },
                ));
                return out;
            }

            // Case 2: both sides are lists of the same kind. Enter
            // their children together via PopBoth.
            if let (Syntax::List(lhs_list), Syntax::List(rhs_list)) = (lhs_node, rhs_node) {
                if lhs_list.kind == rhs_list.kind {
                    let edge = Edge::EnterUnchangedDelimiter {
                        depth_difference: 0,
                    };
                    let parents = vertex
                        .parents
                        .push(EnteredDelimiter::PopBoth(lhs_id, rhs_id));
                    let lhs_first = lhs_list.children.first().copied();
                    let rhs_first = rhs_list.children.first().copied();
                    let (lhs_after, rhs_after, parents_after) =
                        pop_all_parents(lhs_arena, rhs_arena, lhs_first, rhs_first, parents);
                    out.push((
                        edge,
                        Vertex {
                            lhs_syntax: lhs_after,
                            rhs_syntax: rhs_after,
                            parents: parents_after,
                        },
                    ));
                    // Fall through so the search can also try the
                    // novel-delimiter alternatives.
                }
            }

            // Case 2b: comment- or string-kind atoms with similar
            // content can be paired via the Replaced* edges so the
            // diff doesn't show two unrelated Novel runs for a
            // tweaked comment/string.
            if let (Syntax::Atom(lhs_atom), Syntax::Atom(rhs_atom)) = (lhs_node, rhs_node) {
                let levenshtein = levenshtein_pct(lhs_atom.content, rhs_atom.content);
                if levenshtein > 20 {
                    let kind_match_comment = is_comment_atom(lhs_node) && is_comment_atom(rhs_node);
                    let kind_match_string = is_string_atom(lhs_node) && is_string_atom(rhs_node);
                    if kind_match_comment || kind_match_string {
                        let edge = if kind_match_comment {
                            Edge::ReplacedComment {
                                levenshtein_pct: levenshtein,
                            }
                        } else {
                            Edge::ReplacedString {
                                levenshtein_pct: levenshtein,
                            }
                        };
                        let lhs_next = lhs_atom.next_sibling;
                        let rhs_next = rhs_atom.next_sibling;
                        let (lhs_after, rhs_after, parents_after) = pop_all_parents(
                            lhs_arena,
                            rhs_arena,
                            lhs_next,
                            rhs_next,
                            vertex.parents.clone(),
                        );
                        out.push((
                            edge,
                            Vertex {
                                lhs_syntax: lhs_after,
                                rhs_syntax: rhs_after,
                                parents: parents_after,
                            },
                        ));
                    }
                }
            }

            // Case 3: emit "novel on lhs": skip lhs and stay on rhs.
            push_novel_lhs(&mut out, lhs_arena, rhs_arena, lhs_id, vertex);
            // Case 4: emit "novel on rhs": skip rhs and stay on lhs.
            push_novel_rhs(&mut out, lhs_arena, rhs_arena, rhs_id, vertex);
        },
        (Some(lhs_id), None) => {
            push_novel_lhs(&mut out, lhs_arena, rhs_arena, lhs_id, vertex);
        },
        (None, Some(rhs_id)) => {
            push_novel_rhs(&mut out, lhs_arena, rhs_arena, rhs_id, vertex);
        },
        (None, None) => {
            // Both exhausted; the search loop checks `is_end` first.
            // pop_all_parents resolves any remaining stack frames.
            let (lhs_next, rhs_next, parents) =
                pop_all_parents(lhs_arena, rhs_arena, None, None, vertex.parents.clone());
            if lhs_next.is_some() || rhs_next.is_some() || parents.len() < vertex.parents.len() {
                out.push((
                    Edge::UnchangedNode {
                        depth_difference: 0,
                        probably_punctuation: false,
                    },
                    Vertex {
                        lhs_syntax: lhs_next,
                        rhs_syntax: rhs_next,
                        parents,
                    },
                ));
            }
        },
    }
    out
}

fn push_novel_lhs(
    out: &mut Vec<(Edge, Vertex)>,
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_id: SyntaxId,
    vertex: &Vertex,
) {
    let lhs_node = lhs_arena.get(lhs_id);
    let (edge, lhs_next, parents) = match lhs_node {
        Syntax::Atom(atom) => {
            // Atom: skip it; advance to its next sibling.
            (
                Edge::NovelAtomLHS,
                atom.next_sibling,
                vertex.parents.clone(),
            )
        },
        Syntax::List(list) => {
            // List: enter it, the search will recurse through its
            // children; we record the entered list on the parents
            // stack so a later pop returns to the list's next sibling.
            let parents = vertex.parents.push(EnteredDelimiter::PopEither {
                lhs_delims: vec![lhs_id],
                rhs_delims: vec![],
            });
            (
                Edge::EnterNovelDelimiterLHS,
                list.children.first().copied(),
                parents,
            )
        },
    };
    let (lhs_after, rhs_after, parents_after) =
        pop_all_parents(lhs_arena, rhs_arena, lhs_next, vertex.rhs_syntax, parents);
    out.push((
        edge,
        Vertex {
            lhs_syntax: lhs_after,
            rhs_syntax: rhs_after,
            parents: parents_after,
        },
    ));
}

fn push_novel_rhs(
    out: &mut Vec<(Edge, Vertex)>,
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    rhs_id: SyntaxId,
    vertex: &Vertex,
) {
    let rhs_node = rhs_arena.get(rhs_id);
    let (edge, rhs_next, parents) = match rhs_node {
        Syntax::Atom(atom) => (
            Edge::NovelAtomRHS,
            atom.next_sibling,
            vertex.parents.clone(),
        ),
        Syntax::List(list) => {
            let parents = vertex.parents.push(EnteredDelimiter::PopEither {
                lhs_delims: vec![],
                rhs_delims: vec![rhs_id],
            });
            (
                Edge::EnterNovelDelimiterRHS,
                list.children.first().copied(),
                parents,
            )
        },
    };
    let (lhs_after, rhs_after, parents_after) =
        pop_all_parents(lhs_arena, rhs_arena, vertex.lhs_syntax, rhs_next, parents);
    out.push((
        edge,
        Vertex {
            lhs_syntax: lhs_after,
            rhs_syntax: rhs_after,
            parents: parents_after,
        },
    ));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structural_diff::{
        arena::{Atom, List, Syntax, SyntaxArena, SyntaxId},
        ContentId,
    };

    fn mk_atom(arena: &mut SyntaxArena, kind: &'static str, content: &'static str) -> SyntaxId {
        arena.alloc(Syntax::Atom(Atom {
            kind,
            byte_range: 0..content.len(),
            content,
            content_id: ContentId::for_atom(kind, content),
            next_sibling: None,
        }))
    }

    #[allow(dead_code)]
    fn mk_list(
        arena: &mut SyntaxArena,
        kind: &'static str,
        children: Vec<SyntaxId>,
        children_ids: &[ContentId],
    ) -> SyntaxId {
        let id = arena.alloc(Syntax::List(List {
            kind,
            open_byte_range: 0..0,
            close_byte_range: 0..0,
            children,
            content_id: ContentId::for_list(kind, children_ids),
            next_sibling: None,
            _marker: std::marker::PhantomData,
        }));
        arena.link_siblings();
        id
    }

    #[test]
    fn vertex_equality_is_shallow_on_parents() {
        let v1 = Vertex {
            lhs_syntax: Some(SyntaxId(0)),
            rhs_syntax: Some(SyntaxId(0)),
            parents: Stack::new(),
        };
        let v2 = Vertex {
            lhs_syntax: Some(SyntaxId(0)),
            rhs_syntax: Some(SyntaxId(0)),
            parents: Stack::new(),
        };
        assert_eq!(v1, v2);

        let v3 = Vertex {
            lhs_syntax: Some(SyntaxId(0)),
            rhs_syntax: Some(SyntaxId(0)),
            parents: Stack::new().push(EnteredDelimiter::PopBoth(SyntaxId(1), SyntaxId(2))),
        };
        assert_ne!(v1, v3);
    }

    #[test]
    fn edge_costs_are_ordered_correctly() {
        // Unchanged should always beat enter-unchanged-delim, which
        // should always beat novel.
        assert!(
            Edge::UnchangedNode {
                depth_difference: 0,
                probably_punctuation: false,
            }
            .cost()
                < Edge::EnterUnchangedDelimiter {
                    depth_difference: 0
                }
                .cost()
        );
        assert!(
            Edge::EnterUnchangedDelimiter {
                depth_difference: 0
            }
            .cost()
                < Edge::NovelAtomLHS.cost()
        );
        assert_eq!(Edge::NovelAtomLHS.cost(), Edge::NovelAtomRHS.cost());
        assert_eq!(
            Edge::EnterNovelDelimiterLHS.cost(),
            Edge::EnterNovelDelimiterRHS.cost()
        );
    }

    #[test]
    fn matching_atoms_yield_unchanged_edge() {
        let mut arena = SyntaxArena::new();
        let lhs_id = mk_atom(&mut arena, "ident", "x");
        let rhs_id = mk_atom(&mut arena, "ident", "x");
        let vertex = Vertex {
            lhs_syntax: Some(lhs_id),
            rhs_syntax: Some(rhs_id),
            parents: Stack::new(),
        };
        let edges = neighbours(&arena, &arena, &vertex);
        assert!(edges
            .iter()
            .any(|(e, _)| matches!(e, Edge::UnchangedNode { .. })));
    }

    #[test]
    fn distinct_atoms_yield_only_novel_edges() {
        let mut arena = SyntaxArena::new();
        let lhs_id = mk_atom(&mut arena, "ident", "alpha");
        let rhs_id = mk_atom(&mut arena, "ident", "beta");
        let vertex = Vertex {
            lhs_syntax: Some(lhs_id),
            rhs_syntax: Some(rhs_id),
            parents: Stack::new(),
        };
        let edges = neighbours(&arena, &arena, &vertex);
        assert!(!edges.is_empty());
        assert!(edges
            .iter()
            .all(|(e, _)| matches!(e, Edge::NovelAtomLHS | Edge::NovelAtomRHS)));
    }

    #[test]
    fn end_vertex_is_recognized() {
        let v = Vertex {
            lhs_syntax: None,
            rhs_syntax: None,
            parents: Stack::new(),
        };
        assert!(v.is_end());
    }

    #[test]
    fn punctuation_penalty_makes_unchanged_pun_more_expensive_than_unchanged_var() {
        let var = Edge::UnchangedNode {
            depth_difference: 0,
            probably_punctuation: false,
        };
        let pun = Edge::UnchangedNode {
            depth_difference: 0,
            probably_punctuation: true,
        };
        assert_eq!(pun.cost(), var.cost() + 200);
    }

    #[test]
    fn levenshtein_pct_basic_cases() {
        assert_eq!(levenshtein_pct("", ""), 100);
        assert_eq!(levenshtein_pct("abc", "abc"), 100);
        assert_eq!(levenshtein_pct("", "abc"), 0);
        assert_eq!(levenshtein_pct("abc", ""), 0);
        // "kitten" -> "sitting" is the textbook 3-edit example, so
        // similarity = (7 - 3) / 7 = ~57%.
        let pct = levenshtein_pct("kitten", "sitting");
        assert!(pct >= 50 && pct <= 70, "got {pct}");
    }

    #[test]
    fn replaced_comment_cost_in_500_to_600_range() {
        let edge = Edge::ReplacedComment {
            levenshtein_pct: 80,
        };
        let cost = edge.cost();
        assert!((500..=600).contains(&cost), "got cost {cost}");
        // High similarity should produce a lower cost.
        let high_sim = Edge::ReplacedComment {
            levenshtein_pct: 95,
        }
        .cost();
        let low_sim = Edge::ReplacedComment {
            levenshtein_pct: 30,
        }
        .cost();
        assert!(high_sim < low_sim);
    }

    #[test]
    fn probably_punctuation_detects_single_chars() {
        let mut arena = SyntaxArena::new();
        let comma = mk_atom(&mut arena, "punctuation.delimiter", ",");
        let ident = mk_atom(&mut arena, "identifier", "alpha");
        assert!(probably_punctuation(arena.get(comma)));
        assert!(!probably_punctuation(arena.get(ident)));
    }

    #[test]
    fn similar_comments_yield_replaced_comment_edge() {
        let mut arena = SyntaxArena::new();
        let lhs_id = mk_atom(&mut arena, "line_comment", "// alpha beta gamma");
        let rhs_id = mk_atom(&mut arena, "line_comment", "// alpha beta delta");
        let vertex = Vertex {
            lhs_syntax: Some(lhs_id),
            rhs_syntax: Some(rhs_id),
            parents: Stack::new(),
        };
        let edges = neighbours(&arena, &arena, &vertex);
        assert!(edges
            .iter()
            .any(|(e, _)| matches!(e, Edge::ReplacedComment { .. })));
    }
}
