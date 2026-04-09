//! Intermediate representation for structural diff: a tagged tree of
//! [`Syntax::List`] (delimited node, has children) and [`Syntax::Atom`]
//! (leaf with literal text).
//!
//! Difftastic stores these in a `typed_arena::Arena` so children can be
//! referenced by `&'a Syntax<'a>` without `Rc` overhead. To avoid pulling
//! in another arena crate at the scaffolding stage we use index-based
//! references: each node lives in [`SyntaxArena::nodes`] and children are
//! [`SyntaxId`]s into that vector. Lookup is `O(1)`, the borrow story
//! stays trivial, and the arena can hand out shared `&Syntax` borrows
//! without unsafe interior mutation.
//!
//! Swapping in a typed-arena later is mechanical: nodes already own their
//! data, and `Vec<SyntaxId>` becomes `Vec<&'a Syntax<'a>>`.
//!
//! See `references/difftastic/src/parse/syntax.rs` for the Difftastic
//! reference that this scaffolding will eventually feed.

use super::content_id::ContentId;
use std::ops::Range;

/// Stable handle to a node owned by a [`SyntaxArena`]. Indices are dense
/// (one per allocation) and only valid for the arena that produced them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyntaxId(pub usize);

/// One node in the structural-diff intermediate tree. Either a delimited
/// container with children (`List`) or a literal leaf (`Atom`).
#[derive(Debug)]
pub enum Syntax<'a> {
    List(List<'a>),
    Atom(Atom<'a>),
}

#[derive(Debug)]
pub struct List<'a> {
    /// Tree-sitter node kind, kept for diagnostics and the punctuation
    /// heuristic in the cost function.
    pub kind: &'static str,
    /// Opening delimiter byte range in the source rope (e.g. `(` of a
    /// function call). Empty when the node has no explicit open delim.
    pub open_byte_range: Range<usize>,
    /// Closing delimiter byte range. Empty when the node has no explicit
    /// close delim.
    pub close_byte_range: Range<usize>,
    /// Child nodes in source order.
    pub children: Vec<SyntaxId>,
    /// Hash of the node's structure plus leaf content. Two `List`s with
    /// the same `content_id` are equal byte-for-byte (modulo 64-bit
    /// collisions); enables `O(1)` equality during the unchanged-
    /// preprocessing pass.
    pub content_id: ContentId,
    /// Sibling that follows this node in its parent's children list,
    /// or `None` if this is the last child (or the root). Set by
    /// [`SyntaxArena::link_siblings`] after the tree is fully
    /// constructed; until then it stays `None`.
    pub next_sibling: Option<SyntaxId>,
    /// Reserved for borrow-aware structural-diff variants that want to
    /// store source-text slices on the node. Unused at the scaffolding
    /// stage; the `'a` parameter on `Syntax` keeps the public type stable.
    pub _marker: std::marker::PhantomData<&'a ()>,
}

#[derive(Debug)]
pub struct Atom<'a> {
    pub kind: &'static str,
    pub byte_range: Range<usize>,
    /// Literal text of this atom. The lifetime ties the slice to the
    /// source string the arena was built against; nodes never outlive
    /// their source.
    pub content: &'a str,
    pub content_id: ContentId,
    /// Sibling that follows this node in its parent's children list,
    /// or `None` if this is the last child (or the root). Set by
    /// [`SyntaxArena::link_siblings`] after the tree is fully
    /// constructed.
    pub next_sibling: Option<SyntaxId>,
}

/// Owns every [`Syntax`] node in a single diff invocation. Drop the arena
/// and the entire tree drops with it.
#[derive(Default)]
pub struct SyntaxArena {
    nodes: Vec<Syntax<'static>>,
}

impl SyntaxArena {
    pub fn new() -> Self {
        Self::default()
    }

    /// Move `node` into the arena and return its [`SyntaxId`]. The node's
    /// lifetime parameter is erased to `'static` for storage; reads via
    /// [`get`](Self::get) re-attach the arena's borrow lifetime.
    pub fn alloc<'a>(&'a mut self, node: Syntax<'a>) -> SyntaxId {
        let id = SyntaxId(self.nodes.len());
        // SAFETY: We never expose the inner `Syntax<'static>` directly --
        // [`get`] re-borrows it as `Syntax<'a>` where `'a` is the arena's
        // borrow. Internal storage at `'static` is sound because the
        // arena owns the data and any borrowed `&'a str` content fields
        // (or future borrowed children) are tied to the public lifetime
        // by the get accessor.
        let static_node: Syntax<'static> =
            unsafe { std::mem::transmute::<Syntax<'a>, Syntax<'static>>(node) };
        self.nodes.push(static_node);
        id
    }

    pub fn get(&self, id: SyntaxId) -> &Syntax<'_> {
        // Re-attach the arena's borrow lifetime on the way out.
        let stored: &Syntax<'static> = &self.nodes[id.0];
        // SAFETY: storage at `'static` is internal-only; the public
        // borrow is tied to `&self`, which keeps the underlying data
        // alive for the returned reference.
        unsafe { std::mem::transmute::<&Syntax<'static>, &Syntax<'_>>(stored) }
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Walk every [`Syntax::List`] in the arena and set the
    /// `next_sibling` pointer of each child to its successor in the
    /// parent's `children` Vec. The last child's `next_sibling` is
    /// left at `None`. Idempotent: re-running it produces the same
    /// pointers.
    ///
    /// Call once after the arena is fully populated; the
    /// structural-diff Dijkstra search relies on these pointers to
    /// advance through siblings without an explicit parent walk.
    pub fn link_siblings(&mut self) {
        // Collect (child_id, next_sibling_id) pairs from each list,
        // then write them in a second pass so we don't hold an
        // immutable borrow on `self.nodes` during mutation.
        let mut updates: Vec<(SyntaxId, Option<SyntaxId>)> = Vec::new();
        for node in self.nodes.iter() {
            if let Syntax::List(list) = node {
                for (idx, child) in list.children.iter().enumerate() {
                    let next = list.children.get(idx + 1).copied();
                    updates.push((*child, next));
                }
            }
        }
        for (child_id, next) in updates {
            match &mut self.nodes[child_id.0] {
                Syntax::List(l) => l.next_sibling = next,
                Syntax::Atom(a) => a.next_sibling = next,
            }
        }
    }
}

impl<'a> Syntax<'a> {
    pub fn content_id(&self) -> ContentId {
        match self {
            Syntax::List(l) => l.content_id,
            Syntax::Atom(a) => a.content_id,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Syntax::List(l) => l.kind,
            Syntax::Atom(a) => a.kind,
        }
    }

    /// Sibling that follows this node in its parent's `children`,
    /// or `None` if this is the last child or the root. Populated by
    /// [`SyntaxArena::link_siblings`].
    pub fn next_sibling(&self) -> Option<SyntaxId> {
        match self {
            Syntax::List(l) => l.next_sibling,
            Syntax::Atom(a) => a.next_sibling,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_atom(kind: &'static str, content: &'static str, range: Range<usize>) -> Syntax<'static> {
        Syntax::Atom(Atom {
            kind,
            byte_range: range,
            content,
            content_id: ContentId::for_atom(kind, content),
            next_sibling: None,
        })
    }

    fn mk_list_with(
        kind: &'static str,
        children: Vec<SyntaxId>,
        child_ids: &[ContentId],
    ) -> Syntax<'static> {
        Syntax::List(List {
            kind,
            open_byte_range: 0..1,
            close_byte_range: 4..5,
            children,
            content_id: ContentId::for_list(kind, child_ids),
            next_sibling: None,
            _marker: std::marker::PhantomData,
        })
    }

    #[test]
    fn arena_holds_atoms() {
        let mut arena = SyntaxArena::new();
        let a = arena.alloc(mk_atom("ident", "foo", 0..3));
        let b = arena.alloc(mk_atom("ident", "bar", 4..7));
        assert_eq!(arena.len(), 2);
        assert_ne!(arena.get(a).content_id(), arena.get(b).content_id());
    }

    #[test]
    fn arena_holds_lists_with_children() {
        let mut arena = SyntaxArena::new();
        let child1 = arena.alloc(mk_atom("ident", "x", 1..2));
        let child2 = arena.alloc(mk_atom("ident", "y", 3..4));
        let parent_id = arena.alloc(mk_list_with(
            "tuple",
            vec![child1, child2],
            &[
                ContentId::for_atom("ident", "x"),
                ContentId::for_atom("ident", "y"),
            ],
        ));
        let Syntax::List(parent_list) = arena.get(parent_id) else {
            panic!("expected list");
        };
        assert_eq!(parent_list.children.len(), 2);
        assert_eq!(arena.get(parent_list.children[0]).kind(), "ident");
        assert_eq!(arena.get(parent_list.children[1]).kind(), "ident");
    }

    #[test]
    fn list_content_id_collides_for_identical_structure() {
        fn build(arena: &mut SyntaxArena) -> SyntaxId {
            let c1 = arena.alloc(mk_atom("ident", "x", 0..1));
            let c2 = arena.alloc(mk_atom("ident", "y", 0..1));
            arena.alloc(mk_list_with(
                "tuple",
                vec![c1, c2],
                &[
                    ContentId::for_atom("ident", "x"),
                    ContentId::for_atom("ident", "y"),
                ],
            ))
        }
        let mut arena = SyntaxArena::new();
        let a = build(&mut arena);
        let b = build(&mut arena);
        assert_eq!(arena.get(a).content_id(), arena.get(b).content_id());
    }

    #[test]
    fn link_siblings_sets_next_sibling_for_each_child() {
        let mut arena = SyntaxArena::new();
        let c1 = arena.alloc(mk_atom("ident", "a", 0..1));
        let c2 = arena.alloc(mk_atom("ident", "b", 0..1));
        let c3 = arena.alloc(mk_atom("ident", "c", 0..1));
        let _parent = arena.alloc(mk_list_with(
            "tuple",
            vec![c1, c2, c3],
            &[
                ContentId::for_atom("ident", "a"),
                ContentId::for_atom("ident", "b"),
                ContentId::for_atom("ident", "c"),
            ],
        ));
        arena.link_siblings();
        assert_eq!(arena.get(c1).next_sibling(), Some(c2));
        assert_eq!(arena.get(c2).next_sibling(), Some(c3));
        assert_eq!(arena.get(c3).next_sibling(), None);
    }
}
