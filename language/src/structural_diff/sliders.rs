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
#[cfg(test)]
use std::cell::RefCell;
use std::collections::HashMap;

#[cfg(test)]
thread_local! {
    /// Each list whose children this thread's slider passes scanned, in scan
    /// order, which shows a test the lists a pass skipped.
    static SCANNED: RefCell<Vec<SyntaxId>> = const { RefCell::new(Vec::new()) };
}

/// Run two passes of slider correction over the entire tree rooted at
/// `root`, updating `changes` in place.
///
/// `pairs` holds the whole-subtree matches of both sides, and `side` names the
/// component that indexes `arena`. A pair whose node slides out of an
/// unchanged run moves to the node that takes its place.
///
/// The passes scan only the lists that hold a change somewhere below them, so
/// their cost follows the size of the edit rather than the size of the file.
pub fn fix_all_sliders(
    arena: &SyntaxArena,
    root: SyntaxId,
    changes: &mut ChangeMap,
    pairs: &mut [(SyntaxId, SyntaxId)],
    side: Side,
) {
    // FIXME: Nested sliders take no per-language inner-versus-outer delimiter preference
    let changed = changed_below(arena, changes);
    let mut slides = Vec::new();
    for _ in 0..2 {
        fix_sliders_recursive(arena, root, changes, &changed, &mut slides);
    }
    repoint_pairs(pairs, side, &slides);
}

/// Each node of `arena` that is not `Unchanged` or has such a node below it,
/// as flags indexed by [`SyntaxId`].
///
/// A list that is `Unchanged` all the way down holds no run to slide. The
/// flags stay true to that through both passes, because a slide turns only an
/// unflagged subtree `Pending` all the way down, and a list that is `Pending`
/// all the way down holds no slide either. Every run in it starts at its first
/// child, which has no node before it to trade places with. A flag that
/// outlives the change under it costs only a scan.
fn changed_below(arena: &SyntaxArena, changes: &ChangeMap) -> Vec<bool> {
    let mut changed = vec![false; arena.len()];
    // One forward pass is enough, because every child sits at a lower index
    // than its parent (see `SyntaxArena::alloc`). A walk from the root costs
    // about twice as much on a tree the size of app.rs.
    for index in 0..arena.len() {
        let id = SyntaxId(index);
        let children: &[SyntaxId] = match arena.get(id) {
            Syntax::List(list) => &list.children,
            Syntax::Atom(_) => &[],
        };
        debug_assert!(
            children.iter().all(|child| child.0 < index),
            "list {index} holds a child allocated after it",
        );
        changed[index] = changes.get(id) != ChangeKind::Unchanged
            || children.iter().any(|child| changed[child.0]);
    }
    changed
}

fn fix_sliders_recursive(
    arena: &SyntaxArena,
    id: SyntaxId,
    changes: &mut ChangeMap,
    changed: &[bool],
    slides: &mut Vec<(SyntaxId, SyntaxId)>,
) {
    if !changed[id.0] {
        return;
    }
    if let Syntax::List(list) = arena.get(id) {
        #[cfg(test)]
        SCANNED.with_borrow_mut(|scanned| scanned.push(id));
        slide_within_children(arena, &list.children, changes, slides);
        for child in &list.children {
            fix_sliders_recursive(arena, *child, changes, changed, slides);
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

    /// The passes slide what passes that scan every list slide, over random
    /// trees, in tags and in the pairs they move.
    #[test]
    fn the_flagged_passes_slide_what_a_full_scan_slides() {
        let mut state = 1;
        let mut cases_that_slid = 0;
        for case in 0..2000 {
            let mut arena = SyntaxArena::default();
            let mut changes = ChangeMap::new();
            let root = random_tree(&mut state, &mut arena, &mut changes, 4, None);
            let ids: Vec<SyntaxId> = (0..arena.len()).map(SyntaxId).collect();
            let tags =
                |changes: &ChangeMap| ids.iter().map(|&id| changes.get(id)).collect::<Vec<_>>();
            let before = tags(&changes);
            let mut pairs: Vec<_> = ids
                .iter()
                .map(|&id| (SyntaxId(id.0 + 100_000), id))
                .collect();
            let (mut scanned_changes, mut scanned_pairs) = (changes.clone(), pairs.clone());

            fix_all_sliders(&arena, root, &mut changes, &mut pairs, Side::Rhs);
            fix_all_sliders_scanning_every_list(
                &arena,
                root,
                &mut scanned_changes,
                &mut scanned_pairs,
                Side::Rhs,
            );

            assert_eq!(tags(&changes), tags(&scanned_changes), "case {case} tags");
            assert_eq!(pairs, scanned_pairs, "case {case} pairs");
            cases_that_slid += usize::from(tags(&changes) != before);
        }
        assert!(cases_that_slid >= 100, "{cases_that_slid} cases slid");
    }

    /// One pending atom deep under unchanged lists has each pass scan the lists
    /// above it, and no other list.
    #[test]
    fn a_pass_scans_only_the_lists_above_a_change() {
        let mut arena = SyntaxArena::default();
        let pairs = [0; 4].map(|_| mk_pair(&mut arena)[0]);
        let [inner, _, pending] = mk_pair(&mut arena);
        let list = |arena: &mut SyntaxArena, children: Vec<SyntaxId>| {
            let cids: Vec<_> = children
                .iter()
                .map(|&id| arena.get(id).content_id())
                .collect();
            mk_list(arena, "block", children, &cids)
        };
        let left = list(&mut arena, vec![pairs[0], pairs[1]]);
        let middle = list(&mut arena, vec![inner, pairs[2]]);
        let right = list(&mut arena, vec![pairs[3]]);
        let root = list(&mut arena, vec![left, middle, right]);
        let mut changes = ChangeMap::new();
        for id in (0..arena.len()).map(SyntaxId) {
            changes.mark(id, U);
        }
        changes.mark(pending, N);

        SCANNED.with_borrow_mut(Vec::clear);
        fix_all_sliders(&arena, root, &mut changes, &mut [], Side::Rhs);

        assert_eq!(SCANNED.take(), [root, middle, inner, root, middle, inner]);
    }

    /// Two slider passes that scan every list of the tree.
    fn fix_all_sliders_scanning_every_list(
        arena: &SyntaxArena,
        root: SyntaxId,
        changes: &mut ChangeMap,
        pairs: &mut [(SyntaxId, SyntaxId)],
        side: Side,
    ) {
        let mut slides = Vec::new();
        for _ in 0..2 {
            scan_every_list(arena, root, changes, &mut slides);
        }
        repoint_pairs(pairs, side, &slides);
    }

    fn scan_every_list(
        arena: &SyntaxArena,
        id: SyntaxId,
        changes: &mut ChangeMap,
        slides: &mut Vec<(SyntaxId, SyntaxId)>,
    ) {
        if let Syntax::List(list) = arena.get(id) {
            slide_within_children(arena, &list.children, changes, slides);
            for child in &list.children {
                scan_every_list(arena, *child, changes, slides);
            }
        }
    }

    /// A random tree `depth` levels deep, its root tagged `inherited` three
    /// times in four.
    ///
    /// Each node mostly keeps its parent's tag, so whole subtrees share one tag
    /// the way the diff leaves them, and two contents make equal neighbors
    /// common enough for slides.
    fn random_tree(
        state: &mut u64,
        arena: &mut SyntaxArena,
        changes: &mut ChangeMap,
        depth: u32,
        inherited: Option<ChangeKind>,
    ) -> SyntaxId {
        let kind = match inherited {
            Some(kind) if !lcg(state).is_multiple_of(4) => kind,
            _ => match lcg(state) % 8 {
                0..=1 => N,
                2 => ChangeKind::Moved,
                _ => U,
            },
        };
        let id = if depth == 0 || lcg(state).is_multiple_of(3) {
            mk_atom(arena, "ident", ["a", "b"][(lcg(state) % 2) as usize])
        } else {
            let children: Vec<_> = (0..lcg(state) % 5)
                .map(|_| random_tree(state, arena, changes, depth - 1, Some(kind)))
                .collect();
            let cids: Vec<_> = children
                .iter()
                .map(|&id| arena.get(id).content_id())
                .collect();
            mk_list(arena, "list", children, &cids)
        };
        changes.mark(id, kind);
        id
    }

    /// A deterministic pseudo-random walk, so the trees above cover shapes
    /// nobody hand-picked while staying reproducible.
    fn lcg(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        *state >> 33
    }
}
