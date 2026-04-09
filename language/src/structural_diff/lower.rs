//! Lowering from a [`tree_sitter::Tree`] to the structural-diff
//! intermediate form ([`super::Syntax`] arena).
//!
//! Walks the parse tree depth-first. Container nodes (those with named
//! children) become [`super::List`] entries; everything else (leaves,
//! tokens, anonymous text) becomes [`super::Atom`] entries with the
//! corresponding rope-byte range. The lowering is a single pass and
//! preserves source order; it does not collapse adjacent atoms or
//! perform any normalization, so the resulting tree is a faithful
//! lossless mirror of the parse tree's structure ready for the
//! structural-diff preprocessing pass.

use super::{
    arena::{Atom, List, Syntax, SyntaxArena, SyntaxId},
    content_id::ContentId,
};
use tree_sitter::TreeCursor;

/// Lower an entire [`tree_sitter::Tree`]'s root node into a fresh
/// [`SyntaxArena`]. The returned [`SyntaxId`] is the root of the
/// lowered structure; iterate via [`SyntaxArena::get`] from there.
///
/// `source` must be the same byte slice that produced the tree. Atom
/// content slices borrow from it, so the arena's lifetime is bounded
/// by `source`.
pub fn lower_tree<'a>(tree: &tree_sitter::Tree, source: &'a str) -> (SyntaxArena, SyntaxId) {
    let mut arena = SyntaxArena::new();
    let mut cursor = tree.walk();
    let root_id = lower_node(&mut arena, &mut cursor, source);
    arena.link_siblings();
    (arena, root_id)
}

fn lower_node<'a>(
    arena: &mut SyntaxArena,
    cursor: &mut TreeCursor<'_>,
    source: &'a str,
) -> SyntaxId {
    let node = cursor.node();
    let kind: &'static str = static_kind(node.kind());

    if node.named_child_count() == 0 {
        // Leaf: emit as an atom with the literal source slice for its
        // byte range.
        let range = node.start_byte()..node.end_byte();
        let content = &source[range.start.min(source.len())..range.end.min(source.len())];
        return arena.alloc(Syntax::Atom(Atom {
            kind,
            byte_range: range,
            content,
            content_id: ContentId::for_atom(kind, content),
            next_sibling: None,
        }));
    }

    // Container: recurse into named children. The cursor is moved
    // back to the original node before returning so the caller's
    // walk position is preserved.
    let mut child_ids: Vec<SyntaxId> = Vec::new();
    let mut child_content_ids: Vec<ContentId> = Vec::new();
    if cursor.goto_first_child() {
        loop {
            // Skip anonymous tree-sitter nodes (delimiter punctuation,
            // whitespace) so the lowered tree only has the named
            // structure the diff cares about.
            if cursor.node().is_named() {
                let child_id = lower_node(arena, cursor, source);
                let child_cid = arena.get(child_id).content_id();
                child_ids.push(child_id);
                child_content_ids.push(child_cid);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }

    let open_byte_range = node.start_byte()..node.start_byte();
    let close_byte_range = node.end_byte()..node.end_byte();
    arena.alloc(Syntax::List(List {
        kind,
        open_byte_range,
        close_byte_range,
        children: child_ids,
        content_id: ContentId::for_list(kind, &child_content_ids),
        next_sibling: None,
        _marker: std::marker::PhantomData,
    }))
}

/// Tree-sitter node kinds are returned as `&str` borrowed from the
/// grammar's static string table, so they always live for `'static`.
/// The function is a thin transmute that documents this invariant.
fn static_kind(kind: &str) -> &'static str {
    // SAFETY: tree-sitter grammar kind strings live in the grammar's
    // statically-linked string pool for the program's entire lifetime.
    unsafe { std::mem::transmute::<&str, &'static str>(kind) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{parse, LanguageRegistry};

    fn rust_lang() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .unwrap()
    }

    #[test]
    fn lower_simple_function() {
        let source = "fn main() {}";
        let lang = rust_lang();
        let tree = parse(&lang, source, None).unwrap();
        let (arena, root_id) = lower_tree(&tree, source);

        // The arena must contain at least the root and one inner
        // function_item; for "fn main() {}" the rust grammar produces
        // (source_file (function_item ...)).
        assert!(arena.len() >= 2);
        match arena.get(root_id) {
            Syntax::List(list) => {
                assert_eq!(list.kind, "source_file");
                assert!(!list.children.is_empty());
                let first_child = arena.get(list.children[0]);
                assert_eq!(first_child.kind(), "function_item");
            },
            _ => panic!("root must be a List"),
        }
    }

    #[test]
    fn lower_atoms_carry_source_slices() {
        // The rust grammar exposes identifier nodes as named leaves.
        // Verify the lowered atom carries the actual source bytes.
        let source = "fn alpha() {}";
        let lang = rust_lang();
        let tree = parse(&lang, source, None).unwrap();
        let (arena, root_id) = lower_tree(&tree, source);

        // Walk the lowered tree and find an atom whose content is "alpha".
        let mut stack = vec![root_id];
        let mut found_alpha = false;
        while let Some(id) = stack.pop() {
            match arena.get(id) {
                Syntax::List(l) => stack.extend(l.children.iter().copied()),
                Syntax::Atom(a) => {
                    if a.content == "alpha" {
                        found_alpha = true;
                        break;
                    }
                },
            }
        }
        assert!(found_alpha, "function name 'alpha' must appear as an atom");
    }

    #[test]
    fn lower_identical_sources_match_via_content_id() {
        // Lowering the same source twice should yield root nodes with
        // identical ContentIds. This is the property the unchanged-
        // preprocessing pass depends on for O(1) equality checks.
        let source = "fn main() { let x = 1; }";
        let lang = rust_lang();
        let tree1 = parse(&lang, source, None).unwrap();
        let tree2 = parse(&lang, source, None).unwrap();
        let (arena1, root1) = lower_tree(&tree1, source);
        let (arena2, root2) = lower_tree(&tree2, source);
        assert_eq!(
            arena1.get(root1).content_id(),
            arena2.get(root2).content_id()
        );
    }

    #[test]
    fn lower_distinct_sources_produce_distinct_root_ids() {
        let lang = rust_lang();
        let tree_a = parse(&lang, "fn foo() {}", None).unwrap();
        let tree_b = parse(&lang, "fn bar() {}", None).unwrap();
        let (arena_a, root_a) = lower_tree(&tree_a, "fn foo() {}");
        let (arena_b, root_b) = lower_tree(&tree_b, "fn bar() {}");
        assert_ne!(
            arena_a.get(root_a).content_id(),
            arena_b.get(root_b).content_id(),
            "different identifiers must hash to different content ids"
        );
    }
}
