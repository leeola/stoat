//! Stable content identity for [`super::arena::Syntax`] nodes.
//!
//! A `ContentId` is a 64-bit hash of a node's structure: its kind, the
//! kinds-and-content of its children, recursively. Two nodes with the
//! same `ContentId` are guaranteed equal byte-for-byte (modulo the
//! 64-bit collision space). The unchanged-preprocessing pass uses this
//! for O(1) equality checks during LCS over top-level children, which
//! is what makes Difftastic-style structural diff tractable on real
//! files.
//!
//! Difftastic uses an interned `u32` table; we use a precomputed
//! `u64` hash. Either is fine for the diff algorithm; the hash form
//! avoids the side-table lookup at the cost of 4 extra bytes per node.

use std::hash::{Hash, Hasher};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ContentId(pub u64);

impl ContentId {
    /// Hash a leaf atom: its kind plus literal content.
    pub fn for_atom(kind: &str, content: &str) -> Self {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        // Tag the hash with a sentinel byte so an Atom and a List with
        // identical hash inputs can never collide.
        0u8.hash(&mut h);
        kind.hash(&mut h);
        content.hash(&mut h);
        Self(h.finish())
    }

    /// Hash a list node from its kind plus the already-computed content
    /// ids of its children. Children must be passed in source order; two
    /// lists with the same children in different orders intentionally
    /// hash differently.
    pub fn for_list(kind: &str, children: &[ContentId]) -> Self {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        1u8.hash(&mut h);
        kind.hash(&mut h);
        for child in children {
            child.0.hash(&mut h);
        }
        Self(h.finish())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn atom_hash_is_stable_across_calls() {
        let a = ContentId::for_atom("ident", "foo");
        let b = ContentId::for_atom("ident", "foo");
        assert_eq!(a, b);
    }

    #[test]
    fn atom_hash_distinguishes_kind_and_content() {
        let id_x = ContentId::for_atom("ident", "x");
        let kw_x = ContentId::for_atom("keyword", "x");
        let id_y = ContentId::for_atom("ident", "y");
        assert_ne!(id_x, kw_x, "same content, different kind");
        assert_ne!(id_x, id_y, "same kind, different content");
    }

    #[test]
    fn list_hash_is_order_sensitive() {
        let a = ContentId::for_atom("ident", "x");
        let b = ContentId::for_atom("ident", "y");
        let xy = ContentId::for_list("tuple", &[a, b]);
        let yx = ContentId::for_list("tuple", &[b, a]);
        assert_ne!(xy, yx, "order matters in structural diff");
    }

    #[test]
    fn atom_and_list_with_matching_inputs_dont_collide() {
        // Sentinel byte ensures Atom("foo", "") and List("foo", []) hash differently.
        let atom = ContentId::for_atom("foo", "");
        let list = ContentId::for_list("foo", &[]);
        assert_ne!(atom, list);
    }
}
