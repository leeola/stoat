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
//! Reference: `references/difftastic/src/diff/sliders.rs`. We
//! implement only the single-step slider correction; nested slider
//! handling (which prefers inner vs outer delimiters per language) is
//! a future enhancement.
//!
//! Two passes are run because some sliders only become fixable after
//! their neighbors are corrected (cascading).

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    unchanged::{ChangeKind, ChangeMap},
};

/// Run two passes of slider correction over the entire tree rooted at
/// `root`, updating `changes` in place.
pub fn fix_all_sliders(arena: &SyntaxArena, root: SyntaxId, changes: &mut ChangeMap) {
    for _ in 0..2 {
        fix_sliders_recursive(arena, root, changes);
    }
}

fn fix_sliders_recursive(arena: &SyntaxArena, id: SyntaxId, changes: &mut ChangeMap) {
    if let Syntax::List(list) = arena.get(id) {
        slide_within_children(arena, &list.children, changes);
        for child in &list.children {
            fix_sliders_recursive(arena, *child, changes);
        }
    }
}

/// Walk a single sibling list and rewrite slider boundaries.
///
/// The algorithm scans for runs of [`ChangeKind::Pending`] nodes
/// bracketed by [`ChangeKind::Unchanged`] neighbors. When the last
/// node of the Pending run has the same `content_id` as the
/// preceding Unchanged neighbor, the boundary can slide LEFT
/// (the prev node becomes Pending and the last-in-region becomes
/// Unchanged). The result: novel regions always migrate as far
/// LEFT as the matching content allows, leaving Unchanged
/// trailers that match Difftastic's preferred presentation for
/// most languages.
///
/// Slide-RIGHT is intentionally not implemented: the asymmetry
/// gives the algorithm a stable fixed point (single direction =>
/// no oscillation between successive passes). Two passes still
/// run because cascading slides produce different boundaries on
/// the second iteration.
fn slide_within_children(arena: &SyntaxArena, children: &[SyntaxId], changes: &mut ChangeMap) {
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

        // Slide LEFT: prev node Unchanged + matches last in region.
        if region_start > 0 && region_end > region_start {
            let prev = children[region_start - 1];
            let last_in_region = children[region_end - 1];
            if arena.get(prev).content_id() == arena.get(last_in_region).content_id() {
                changes.mark(prev, ChangeKind::Pending);
                changes.mark(last_in_region, ChangeKind::Unchanged);
            }
        }
    }
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
            byte_range: 0..0,
            content,
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
            _marker: std::marker::PhantomData,
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

    #[test]
    fn slider_pulls_match_to_left_edge() {
        // Children: A B A B C
        // Initial:  U U N N U  (region [A B] is novel)
        // After slide-left: B in region matches B preceding it, so
        // the boundary moves: U N N U U.
        let mut arena = SyntaxArena::new();
        let (root, children) = build_test_list(&mut arena);
        let mut changes = ChangeMap::new();
        changes.mark(children[0], ChangeKind::Unchanged); // A
        changes.mark(children[1], ChangeKind::Unchanged); // B
        changes.mark(children[2], ChangeKind::Pending); // A
        changes.mark(children[3], ChangeKind::Pending); // B
        changes.mark(children[4], ChangeKind::Unchanged); // C

        fix_all_sliders(&arena, root, &mut changes);

        // After sliding left: prev (B at index 1) and last in region
        // (B at index 3) have matching content_ids, so they swap.
        assert_eq!(changes.get(children[1]), ChangeKind::Pending);
        assert_eq!(changes.get(children[3]), ChangeKind::Unchanged);
    }

    #[test]
    fn slider_no_op_when_neighbors_dont_match() {
        // Children: A B C D E (all distinct, novel run [B C D])
        let mut arena = SyntaxArena::new();
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

        fix_all_sliders(&arena, root, &mut changes);

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
        let mut arena = SyntaxArena::new();
        let (root, children) = build_test_list(&mut arena);
        let mut changes = ChangeMap::new();
        for c in children {
            changes.mark(c, ChangeKind::Unchanged);
        }
        fix_all_sliders(&arena, root, &mut changes);
        for c in children {
            assert_eq!(changes.get(c), ChangeKind::Unchanged);
        }
    }
}
