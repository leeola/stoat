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
//! Slide-LEFT is the preferred direction: cascading runs of slide-LEFT
//! migrate novel regions as far left as matching content allows.
//! Slide-RIGHT runs only as a fallback when slide-LEFT is
//! structurally blocked for a region (mainly `region_start == 0`).
//! The slide-RIGHT precondition implies the post-slide state matches
//! slide-LEFT's precondition, so without a text-distance tiebreaker
//! the two directions can oscillate; the outer fixed-point loop
//! caps iterations at 4 as a safety net.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    unchanged::{ChangeKind, ChangeMap},
};
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_PASSES: usize = 4;

/// Run slider correction over the entire tree rooted at `root`,
/// updating `changes` in place. Loops until a pass leaves the change
/// map unchanged or after `MAX_PASSES` iterations. The `cancel` flag
/// is polled once per `fix_sliders_recursive` entry; on cancel the
/// recursion returns early and the caller observes the partial state.
pub fn fix_all_sliders(
    arena: &SyntaxArena,
    root: SyntaxId,
    changes: &mut ChangeMap,
    cancel: Option<&AtomicBool>,
) {
    let mut dirty = true;
    let mut pass = 0;
    while dirty && pass < MAX_PASSES {
        dirty = false;
        fix_sliders_recursive(arena, root, changes, &mut dirty, cancel);
        pass += 1;
    }
}

fn fix_sliders_recursive(
    arena: &SyntaxArena,
    id: SyntaxId,
    changes: &mut ChangeMap,
    dirty: &mut bool,
    cancel: Option<&AtomicBool>,
) {
    if cancel.is_some_and(|c| c.load(Ordering::Relaxed)) {
        return;
    }
    if let Syntax::List(list) = arena.get(id) {
        slide_within_children(arena, &list.children, changes, dirty);
        for child in &list.children {
            fix_sliders_recursive(arena, *child, changes, dirty, cancel);
        }
    }
}

/// Walk a single sibling list and rewrite slider boundaries.
///
/// For each run of [`ChangeKind::Pending`] nodes the function first
/// attempts a slide-LEFT: when the preceding Unchanged neighbor's
/// `content_id` matches the last node in the Pending region, the
/// previous node is marked Pending and the last-in-region is marked
/// Unchanged (the boundary shifts left by one). If slide-LEFT did not
/// apply for the region -- because `region_start == 0` or the
/// content IDs do not match -- the function then tries slide-RIGHT:
/// when the following Unchanged neighbor's `content_id` matches the
/// first node in the region, the first-in-region is marked Unchanged
/// and the next node is marked Pending (boundary shifts right by
/// one). The exclusivity preserves the existing slide-LEFT cascade
/// behavior; slide-RIGHT widens coverage to regions that begin at
/// the start of the list.
fn slide_within_children(
    arena: &SyntaxArena,
    children: &[SyntaxId],
    changes: &mut ChangeMap,
    dirty: &mut bool,
) {
    let n = children.len();
    let mut i = 0;
    while i < n {
        while i < n && changes.get(children[i]) == ChangeKind::Unchanged {
            i += 1;
        }
        if i >= n {
            break;
        }
        let region_start = i;
        while i < n && changes.get(children[i]) != ChangeKind::Unchanged {
            i += 1;
        }
        let region_end = i;

        let mut left_applied = false;
        if region_start > 0 && region_end > region_start {
            let prev = children[region_start - 1];
            let last_in_region = children[region_end - 1];
            if arena.get(prev).content_id() == arena.get(last_in_region).content_id() {
                changes.mark(prev, ChangeKind::Pending);
                changes.mark(last_in_region, ChangeKind::Unchanged);
                *dirty = true;
                left_applied = true;
            }
        }

        if !left_applied && region_end < n && region_end > region_start {
            let next = children[region_end];
            let first_in_region = children[region_start];
            if arena.get(next).content_id() == arena.get(first_in_region).content_id() {
                changes.mark(first_in_region, ChangeKind::Unchanged);
                changes.mark(next, ChangeKind::Pending);
                *dirty = true;
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

        fix_all_sliders(&arena, root, &mut changes, None);

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

        fix_all_sliders(&arena, root, &mut changes, None);

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
        fix_all_sliders(&arena, root, &mut changes, None);
        for c in children {
            assert_eq!(changes.get(c), ChangeKind::Unchanged);
        }
    }

    #[test]
    fn slide_within_children_pushes_right_when_left_blocked() {
        // Children: A B A
        // Initial:  P P U  (region [A B] starts at index 0)
        // Slide-LEFT cannot apply (region_start == 0).
        // Slide-RIGHT: next-Unchanged (idx 2, A) matches first-in-region
        // (idx 0, A), so children[0] -> Unchanged, children[2] -> Pending.
        // Final single-pass state: U P P.
        let mut arena = SyntaxArena::new();
        let a1 = mk_atom(&mut arena, "ident", "A");
        let b = mk_atom(&mut arena, "ident", "B");
        let a2 = mk_atom(&mut arena, "ident", "A");
        let cids = [
            ContentId::for_atom("ident", "A"),
            ContentId::for_atom("ident", "B"),
            ContentId::for_atom("ident", "A"),
        ];
        let root = mk_list(&mut arena, "tuple", vec![a1, b, a2], &cids);
        arena.link_siblings();
        let children = match arena.get(root) {
            Syntax::List(list) => list.children.clone(),
            Syntax::Atom(_) => unreachable!(),
        };

        let mut changes = ChangeMap::new();
        changes.mark(a1, ChangeKind::Pending);
        changes.mark(b, ChangeKind::Pending);
        changes.mark(a2, ChangeKind::Unchanged);

        let mut dirty = false;
        slide_within_children(&arena, &children, &mut changes, &mut dirty);

        assert!(dirty, "slide-RIGHT should have marked changes");
        assert_eq!(changes.get(a1), ChangeKind::Unchanged);
        assert_eq!(changes.get(b), ChangeKind::Pending);
        assert_eq!(changes.get(a2), ChangeKind::Pending);
    }
}
