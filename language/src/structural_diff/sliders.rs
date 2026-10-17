//! Slider correction for the structural-diff [`super::ChangeMap`].
//!
//! After [`super::dijkstra::populate_change_map`] tags every node, the
//! diff is structurally correct but the boundary between Unchanged and
//! Pending regions can be visually ambiguous. Consider:
//!
//! ```text
//! Old:        New:
//! A B         A B
//! C D         A B
//!             C D
//! ```
//!
//! Both of these markings produce a valid diff:
//!
//! ```text
//! Marking 1:   A B          Marking 2:   A B
//!             +A B+                     +A B+
//!              C D                       C D
//! ```
//!
//! Difftastic prefers Marking 1 (the *trailing* unchanged region) for
//! readability. The slider pass walks the tree and rewrites the
//! boundaries when sliding produces an equally-valid diff with the
//! Pending region positioned next to a structural cue (the start or
//! end of a list).
//!
//! A slide moves whole subtrees, because the change collector reads atom
//! tags alone. The node that leaves the unchanged run turns `Pending` with
//! every node under it. The node that joins the run turns `Unchanged` the same
//! way. The matched pair of the node that left moves to the node that joined,
//! so the token pairs agree with the tags.
//!
//! The pass follows `references/difftastic/src/diff/sliders.rs`, whose
//! `insert_deep_novel` and `insert_deep_unchanged` mark the same subtrees.
//! Only the single-step correction exists here. The nested slider handling,
//! which prefers inner or outer delimiters per language, does not.
//!
//! Two passes run, because some sliders become fixable only after the first
//! pass corrects their neighbors.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    fx::FxBuildHasher,
    unchanged::{mark_subtree, ChangeKind, ChangeMap},
    Side,
};
use std::collections::HashMap;

/// Run two passes of slider correction over the entire tree rooted at
/// `root`, updating `changes` in place.
///
/// `pairs` holds the whole-subtree matches of both sides, and `side` names the
/// component that indexes `arena`. A pair whose node slides out of an
/// unchanged run moves to the node that takes its place.
pub fn fix_all_sliders(
    arena: &SyntaxArena,
    root: SyntaxId,
    changes: &mut ChangeMap,
    pairs: &mut [(SyntaxId, SyntaxId)],
    side: Side,
) {
    // FIXME: Nested sliders take no per-language inner-versus-outer delimiter preference
    let mut slides = Vec::new();
    for _ in 0..2 {
        fix_sliders_recursive(arena, root, changes, &mut slides);
    }
    repoint_pairs(pairs, side, &slides);
}

fn fix_sliders_recursive(
    arena: &SyntaxArena,
    id: SyntaxId,
    changes: &mut ChangeMap,
    slides: &mut Vec<(SyntaxId, SyntaxId)>,
) {
    if let Syntax::List(list) = arena.get(id) {
        slide_within_children(arena, &list.children, changes, slides);
        for child in &list.children {
            fix_sliders_recursive(arena, *child, changes, slides);
        }
    }
}

/// Slide each `Pending` run of `children` one node left when the content
/// matches, and append each slide to `slides` as `(left, joined)`.
///
/// A run of [`ChangeKind::Pending`] nodes after a [`ChangeKind::Unchanged`]
/// node slides when that node has the same `content_id` as the run's last
/// node. The node before the run turns `Pending` and the run's last node turns
/// `Unchanged`, each with its whole subtree. Novel regions therefore move
/// left, one node per pass, and leave the unchanged trailers that readers
/// expect.
///
/// The node before the run must be `Unchanged` all the way down, and every
/// node of the run must be `Pending` all the way down. A partial match has
/// counterparts that the swap leaves behind. The swap then claims unchanged
/// text that the other side lacks, or crosses the order of the matches.
///
/// Slide-right does not exist. One direction gives the pass a stable fixed
/// point, with no oscillation between passes. Two passes still run, because a
/// cascade of slides moves the boundaries again in the second pass.
fn slide_within_children(
    arena: &SyntaxArena,
    children: &[SyntaxId],
    changes: &mut ChangeMap,
    slides: &mut Vec<(SyntaxId, SyntaxId)>,
) {
    let n = children.len();
    let mut i = 0;
    while i < n {
        // Skip leading Unchanged.
        while i < n && changes.get(children[i]) == ChangeKind::Unchanged {
            i += 1;
        }
        if i >= n {
            break;
        }
        let region_start = i;
        // Find end of Pending run.
        while i < n && changes.get(children[i]) != ChangeKind::Unchanged {
            i += 1;
        }
        let region_end = i;

        if region_start > 0 && region_end > region_start {
            let prev = children[region_start - 1];
            let last_in_region = children[region_end - 1];
            if arena.get(prev).content_id() == arena.get(last_in_region).content_id()
                && is_deep(arena, prev, changes, ChangeKind::Unchanged)
                && children[region_start..region_end]
                    .iter()
                    .all(|&id| is_deep(arena, id, changes, ChangeKind::Pending))
            {
                mark_subtree(arena, prev, changes, ChangeKind::Pending);
                mark_subtree(arena, last_in_region, changes, ChangeKind::Unchanged);
                slides.push((prev, last_in_region));
            }
        }
    }
}

/// Whether `id` and every node under it carry `kind`.
fn is_deep(arena: &SyntaxArena, id: SyntaxId, changes: &ChangeMap, kind: ChangeKind) -> bool {
    let mut stack = vec![id];
    while let Some(current) = stack.pop() {
        if changes.get(current) != kind {
            return false;
        }
        if let Syntax::List(list) = arena.get(current) {
            stack.extend(list.children.iter().copied());
        }
    }
    true
}

/// Move each pair whose `side` node left an unchanged run to the node that
/// joined it.
///
/// The slides replay in order, so a node that slides twice in a cascade
/// carries its pair to the last place it reached. The function builds its
/// pair map only after a slide, so a diff with no slide pays nothing here.
fn repoint_pairs(pairs: &mut [(SyntaxId, SyntaxId)], side: Side, slides: &[(SyntaxId, SyntaxId)]) {
    if slides.is_empty() {
        return;
    }

    let mut pair_of: HashMap<SyntaxId, usize, FxBuildHasher> = pairs
        .iter_mut()
        .enumerate()
        .map(|(index, pair)| (*side_node(pair, side), index))
        .collect();
    for &(left, joined) in slides {
        if let Some(index) = pair_of.remove(&left) {
            *side_node(&mut pairs[index], side) = joined;
            pair_of.insert(joined, index);
        }
    }
}

fn side_node(pair: &mut (SyntaxId, SyntaxId), side: Side) -> &mut SyntaxId {
    match side {
        Side::Lhs => &mut pair.0,
        Side::Rhs => &mut pair.1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::structural_diff::{
        arena::{Atom, AtomClass, List, Syntax, SyntaxArena, SyntaxId},
        ContentId,
    };

    const U: ChangeKind = ChangeKind::Unchanged;
    const N: ChangeKind = ChangeKind::Pending;

    fn mk_atom(arena: &mut SyntaxArena, kind: &'static str, content: &'static str) -> SyntaxId {
        arena.alloc(Syntax::Atom(Atom {
            kind,
            class: AtomClass::of(kind),
            byte_range: 0..0,
            content_id: ContentId::for_atom(kind, content),
            next_sibling: None,
        }))
    }

    fn mk_list(
        arena: &mut SyntaxArena,
        kind: &'static str,
        children: Vec<SyntaxId>,
        cids: &[ContentId],
    ) -> SyntaxId {
        arena.alloc(Syntax::List(List {
            kind,
            open_byte_range: 0..0,
            close_byte_range: 0..0,
            children,
            content_id: ContentId::for_list(kind, cids),
            next_sibling: None,
        }))
    }

    /// Build a list with children [A, B, A, B, C] all of identical
    /// kind so that `next_sibling` linking puts them in sequence.
    fn build_test_list(arena: &mut SyntaxArena) -> (SyntaxId, [SyntaxId; 5]) {
        let a1 = mk_atom(arena, "ident", "A");
        let b1 = mk_atom(arena, "ident", "B");
        let a2 = mk_atom(arena, "ident", "A");
        let b2 = mk_atom(arena, "ident", "B");
        let c = mk_atom(arena, "ident", "C");
        let cids = [
            ContentId::for_atom("ident", "A"),
            ContentId::for_atom("ident", "B"),
            ContentId::for_atom("ident", "A"),
            ContentId::for_atom("ident", "B"),
            ContentId::for_atom("ident", "C"),
        ];
        let list = mk_list(arena, "tuple", vec![a1, b1, a2, b2, c], &cids);
        arena.link_siblings();
        (list, [a1, b1, a2, b2, c])
    }

    /// A list over the atoms `x y`, as `[list, x, y]`.
    fn mk_pair(arena: &mut SyntaxArena) -> [SyntaxId; 3] {
        let x = mk_atom(arena, "ident", "x");
        let y = mk_atom(arena, "ident", "y");
        let cids = [
            ContentId::for_atom("ident", "x"),
            ContentId::for_atom("ident", "y"),
        ];
        [mk_list(arena, "pair", vec![x, y], &cids), x, y]
    }

    /// `[A1, B, A2, C]`, where `A1` and `A2` are equal lists from [`mk_pair`],
    /// tagged `U N N U` all the way down. Returns the root, every node in
    /// source order (`A1 x y B A2 x y C`), and the tags.
    fn list_slide_input(arena: &mut SyntaxArena) -> (SyntaxId, [SyntaxId; 8], ChangeMap) {
        let [a1, a1x, a1y] = mk_pair(arena);
        let b = mk_atom(arena, "ident", "B");
        let [a2, a2x, a2y] = mk_pair(arena);
        let c = mk_atom(arena, "ident", "C");
        let cids = [a1, b, a2, c].map(|id| arena.get(id).content_id());
        let root = mk_list(arena, "tuple", vec![a1, b, a2, c], &cids);
        arena.link_siblings();

        let nodes = [a1, a1x, a1y, b, a2, a2x, a2y, c];
        let mut changes = ChangeMap::new();
        for (id, kind) in nodes.into_iter().zip([U, U, U, N, N, N, N, U]) {
            changes.mark(id, kind);
        }
        (root, nodes, changes)
    }

    #[test]
    fn slider_pulls_match_to_left_edge() {
        // Children: A B A B C
        // Initial:  U U N N U  (region [A B] is novel)
        // After slide-left: B in region matches B preceding it, so
        // the boundary moves: U N N U U.
        let mut arena = SyntaxArena::default();
        let (root, children) = build_test_list(&mut arena);
        let mut changes = ChangeMap::new();
        changes.mark(children[0], ChangeKind::Unchanged); // A
        changes.mark(children[1], ChangeKind::Unchanged); // B
        changes.mark(children[2], ChangeKind::Pending); // A
        changes.mark(children[3], ChangeKind::Pending); // B
        changes.mark(children[4], ChangeKind::Unchanged); // C

        fix_all_sliders(&arena, root, &mut changes, &mut [], Side::Rhs);

        // After sliding left: prev (B at index 1) and last in region
        // (B at index 3) have matching content_ids, so they swap.
        assert_eq!(changes.get(children[1]), ChangeKind::Pending);
        assert_eq!(changes.get(children[3]), ChangeKind::Unchanged);
    }

    #[test]
    fn slider_no_op_when_neighbors_dont_match() {
        // Children: A B C D E (all distinct, novel run [B C D])
        let mut arena = SyntaxArena::default();
        let a = mk_atom(&mut arena, "ident", "A");
        let b = mk_atom(&mut arena, "ident", "B");
        let c = mk_atom(&mut arena, "ident", "C");
        let d = mk_atom(&mut arena, "ident", "D");
        let e = mk_atom(&mut arena, "ident", "E");
        let cids = [
            ContentId::for_atom("ident", "A"),
            ContentId::for_atom("ident", "B"),
            ContentId::for_atom("ident", "C"),
            ContentId::for_atom("ident", "D"),
            ContentId::for_atom("ident", "E"),
        ];
        let root = mk_list(&mut arena, "tuple", vec![a, b, c, d, e], &cids);
        arena.link_siblings();
        let mut changes = ChangeMap::new();
        changes.mark(a, ChangeKind::Unchanged);
        changes.mark(b, ChangeKind::Pending);
        changes.mark(c, ChangeKind::Pending);
        changes.mark(d, ChangeKind::Pending);
        changes.mark(e, ChangeKind::Unchanged);

        fix_all_sliders(&arena, root, &mut changes, &mut [], Side::Rhs);

        // No content_id match means no slide.
        assert_eq!(changes.get(b), ChangeKind::Pending);
        assert_eq!(changes.get(c), ChangeKind::Pending);
        assert_eq!(changes.get(d), ChangeKind::Pending);
        assert_eq!(changes.get(a), ChangeKind::Unchanged);
        assert_eq!(changes.get(e), ChangeKind::Unchanged);
    }

    #[test]
    fn slider_handles_empty_pending_run() {
        // All Unchanged: nothing should change.
        let mut arena = SyntaxArena::default();
        let (root, children) = build_test_list(&mut arena);
        let mut changes = ChangeMap::new();
        for c in children {
            changes.mark(c, ChangeKind::Unchanged);
        }
        fix_all_sliders(&arena, root, &mut changes, &mut [], Side::Rhs);
        for c in children {
            assert_eq!(changes.get(c), ChangeKind::Unchanged);
        }
    }

    #[test]
    fn a_list_slide_moves_its_atoms_too() {
        let mut arena = SyntaxArena::default();
        let (root, nodes, mut changes) = list_slide_input(&mut arena);

        fix_all_sliders(&arena, root, &mut changes, &mut [], Side::Rhs);

        assert_eq!(
            nodes.map(|id| changes.get(id)),
            [N, N, N, N, U, U, U, U],
            "A1 turns novel with its atoms, and A2 turns unchanged with its atoms",
        );
    }

    #[test]
    fn a_slide_repoints_the_matched_pair() {
        let mut arena = SyntaxArena::default();
        let (root, nodes, mut changes) = list_slide_input(&mut arena);
        let lhs = SyntaxId(100);
        let mut pairs = [(lhs, nodes[0])];

        fix_all_sliders(&arena, root, &mut changes, &mut pairs, Side::Rhs);

        assert_eq!(pairs, [(lhs, nodes[4])], "the pair of A1 moves to A2");
    }

    #[test]
    fn a_cascade_carries_each_pair_one_step() {
        let mut arena = SyntaxArena::default();
        let lists = [
            mk_pair(&mut arena),
            mk_pair(&mut arena),
            mk_pair(&mut arena),
        ];
        let [w, x, y] = lists.map(|[list, ..]| list);
        let cids = [w, x, y].map(|id| arena.get(id).content_id());
        let root = mk_list(&mut arena, "tuple", vec![w, x, y], &cids);
        arena.link_siblings();
        let mut changes = ChangeMap::new();
        for (subtree, kind) in lists.into_iter().zip([U, U, N]) {
            for id in subtree {
                changes.mark(id, kind);
            }
        }
        let (first, second) = (SyntaxId(100), SyntaxId(101));
        let mut pairs = [(first, w), (second, x)];

        fix_all_sliders(&arena, root, &mut changes, &mut pairs, Side::Rhs);

        assert_eq!(
            pairs,
            [(first, x), (second, y)],
            "each pair moves one list right, in the order the two passes slid them",
        );
    }

    #[test]
    fn a_partial_match_on_either_side_blocks_the_slide() {
        for (node, kind) in [(2, N), (5, U)] {
            let mut arena = SyntaxArena::default();
            let (root, nodes, mut changes) = list_slide_input(&mut arena);
            changes.mark(nodes[node], kind);
            let before = nodes.map(|id| changes.get(id));

            fix_all_sliders(&arena, root, &mut changes, &mut [], Side::Rhs);

            assert_eq!(
                nodes.map(|id| changes.get(id)),
                before,
                "node {node} as {kind:?}"
            );
        }
    }
}
