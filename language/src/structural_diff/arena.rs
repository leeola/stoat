//! Intermediate representation for structural diff: a tagged tree of
//! [`Syntax::List`] (delimited node, has children) and [`Syntax::Atom`]
//! (leaf with literal text).
//!
//! Difftastic stores these in a `typed_arena::Arena` so children can be
//! referenced by `&'a Syntax<'a>` without `Rc` overhead. This module uses
//! index-based references instead: each node lives in
//! [`SyntaxArena::nodes`] and children are [`SyntaxId`]s into that
//! vector, so lookup is `O(1)` and the arena hands out shared `&Syntax`
//! borrows without interior mutation.

use super::content_id::ContentId;
use std::{ops::Range, sync::Arc};

/// Stable handle to a node owned by a [`SyntaxArena`]. Indices are dense
/// (one per allocation) and only valid for the arena that produced them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SyntaxId(pub usize);

/// One node in the structural-diff intermediate tree. Either a delimited
/// container with children (`List`) or a literal leaf (`Atom`).
#[derive(Debug)]
pub enum Syntax {
    List(List),
    Atom(Atom),
}

#[derive(Debug)]
pub struct List {
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
}

#[derive(Debug)]
pub struct Atom {
    pub kind: &'static str,
    /// The atom's text in the source of its arena, which
    /// [`SyntaxArena::text`] reads.
    pub byte_range: Range<usize>,
    pub content_id: ContentId,
    /// Sibling that follows this node in its parent's children list,
    /// or `None` if this is the last child (or the root). Set by
    /// [`SyntaxArena::link_siblings`] after the tree is fully
    /// constructed.
    pub next_sibling: Option<SyntaxId>,
}

/// Owns every [`Syntax`] node of one lowered tree, and the source text that
/// its atoms name by byte range.
///
/// The arena holds the text rather than borrowing it, so it outlives the call
/// that lowered it. A diff base that does not change between diffs is then
/// lowered once.
#[derive(Default)]
pub struct SyntaxArena {
    nodes: Vec<Syntax>,
    source: Arc<str>,
}

impl SyntaxArena {
    /// An arena over `source`, sized for `nodes` entries up front.
    ///
    /// Lowering pushes one entry per node of a whole parse tree, which is tens
    /// of thousands for an ordinary source file and a dozen or more reallocs
    /// from an empty `Vec`. Callers that know the tree's node count in advance
    /// pay one allocation instead.
    pub fn with_capacity(source: Arc<str>, nodes: usize) -> Self {
        Self {
            nodes: Vec::with_capacity(nodes),
            source,
        }
    }

    /// Move `node` into the arena and return its [`SyntaxId`].
    pub fn alloc(&mut self, node: Syntax) -> SyntaxId {
        let id = SyntaxId(self.nodes.len());
        self.nodes.push(node);
        id
    }

    pub fn get(&self, id: SyntaxId) -> &Syntax {
        &self.nodes[id.0]
    }

    /// The text of `atom`, read from this arena's source.
    ///
    /// A range past the end of the source reads as cut off at the end, so a
    /// tree that names more bytes than its text holds yields short text.
    pub fn text(&self, atom: &Atom) -> &str {
        let len = self.source.len();
        &self.source[atom.byte_range.start.min(len)..atom.byte_range.end.min(len)]
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// The node count and the atom leaf count of each node's subtree, indexed
    /// by [`SyntaxId`], each count taking in the node itself.
    ///
    /// Every child must sit at a lower index than its parent, as
    /// [`super::lower_tree`] allocates them, because one forward pass fills the
    /// table.
    pub fn subtree_counts(&self) -> Vec<(u32, u32)> {
        let mut counts = vec![(0, 0); self.nodes.len()];
        for (i, node) in self.nodes.iter().enumerate() {
            counts[i] = match node {
                Syntax::Atom(_) => (1, 1),
                Syntax::List(list) => {
                    list.children.iter().fold((1, 0), |(nodes, leaves), child| {
                        (nodes + counts[child.0].0, leaves + counts[child.0].1)
                    })
                },
            };
        }
        counts
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

    /// Rewrite `ids` into a standalone sibling chain. Each id points at the
    /// next via `next_sibling`, and the last terminates with `None`.
    ///
    /// A per-section diff search walks a run of changed siblings via
    /// `next_sibling`, so relinking isolates the run and the search stops at
    /// its end instead of continuing into the rest of the file. This
    /// overwrites whatever [`Self::link_siblings`] set for these nodes. Their
    /// subtrees keep their links, so the search descends into each normally.
    pub fn relink_run(&mut self, ids: &[SyntaxId]) {
        for (idx, id) in ids.iter().enumerate() {
            let next = ids.get(idx + 1).copied();
            match &mut self.nodes[id.0] {
                Syntax::List(l) => l.next_sibling = next,
                Syntax::Atom(a) => a.next_sibling = next,
            }
        }
    }
}

#[cfg(test)]
impl SyntaxArena {
    /// Append `content` to the source and allocate an atom over it.
    ///
    /// A test that builds an arena by hand and reads its atoms' text back needs
    /// each atom at its own range of one source.
    pub(super) fn alloc_atom(&mut self, kind: &'static str, content: &str) -> SyntaxId {
        let start = self.source.len();
        self.source = Arc::from(format!("{}{content}", self.source));
        self.alloc(Syntax::Atom(Atom {
            kind,
            byte_range: start..start + content.len(),
            content_id: ContentId::for_atom(kind, content),
            next_sibling: None,
        }))
    }
}

impl Syntax {
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

    fn mk_atom(kind: &'static str, content: &'static str, range: Range<usize>) -> Syntax {
        Syntax::Atom(Atom {
            kind,
            byte_range: range,
            content_id: ContentId::for_atom(kind, content),
            next_sibling: None,
        })
    }

    fn mk_list_with(
        kind: &'static str,
        children: Vec<SyntaxId>,
        child_ids: &[ContentId],
    ) -> Syntax {
        Syntax::List(List {
            kind,
            open_byte_range: 0..1,
            close_byte_range: 4..5,
            children,
            content_id: ContentId::for_list(kind, child_ids),
            next_sibling: None,
        })
    }

    #[test]
    fn arena_holds_atoms() {
        let mut arena = SyntaxArena::default();
        let a = arena.alloc(mk_atom("ident", "foo", 0..3));
        let b = arena.alloc(mk_atom("ident", "bar", 4..7));
        assert_eq!(arena.len(), 2);
        assert_ne!(arena.get(a).content_id(), arena.get(b).content_id());
    }

    #[test]
    fn arena_holds_lists_with_children() {
        let mut arena = SyntaxArena::default();
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
        let mut arena = SyntaxArena::default();
        let a = build(&mut arena);
        let b = build(&mut arena);
        assert_eq!(arena.get(a).content_id(), arena.get(b).content_id());
    }

    #[test]
    fn link_siblings_sets_next_sibling_for_each_child() {
        let mut arena = SyntaxArena::default();
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
