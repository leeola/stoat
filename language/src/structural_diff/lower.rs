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
//!
//! Anonymous tokens are lowered like any other child. They are most of what
//! distinguishes one expression from another with the same shape, so a tree
//! carrying only the named skeleton cannot tell `a + b` from `a - b`.

use super::{
    arena::{Atom, List, Syntax, SyntaxArena, SyntaxId},
    content_id::ContentId,
};
use std::ops::Range;
use tree_sitter::TreeCursor;

/// Lower an entire [`tree_sitter::Tree`]'s root node into a fresh
/// [`SyntaxArena`]. The returned [`SyntaxId`] is the root of the
/// lowered structure; iterate via [`SyntaxArena::get`] from there.
///
/// `source` must be the same byte slice that produced the tree. Atom
/// content slices borrow from it, so the arena's lifetime is bounded
/// by `source`.
pub fn lower_tree<'a>(tree: &tree_sitter::Tree, source: &'a str) -> (SyntaxArena<'a>, SyntaxId) {
    let mut arena = SyntaxArena::new();
    let mut cursor = tree.walk();
    let root_id = lower_node(&mut arena, &mut cursor, source);
    arena.link_siblings();
    (arena, root_id)
}

fn lower_node<'a>(
    arena: &mut SyntaxArena<'a>,
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

    // The cursor is moved back to the original node before returning so
    // the caller's walk position is preserved.
    let mut child_ids: Vec<SyntaxId> = Vec::new();
    let mut child_content_ids: Vec<ContentId> = Vec::new();
    if cursor.goto_first_child() {
        loop {
            let child_id = lower_node(arena, cursor, source);
            let child_cid = arena.get(child_id).content_id();
            child_ids.push(child_id);
            child_content_ids.push(child_cid);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }

    let (open_byte_range, close_byte_range) = delimiter_ranges(arena, node, &child_ids);
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

/// Byte ranges of `node`'s opening and closing delimiters.
///
/// A pair is recognized only when the first and last children are the two
/// halves of one bracket pair. Requiring the boundary is what keeps a leading
/// keyword or a trailing separator from being read as a delimiter. The children
/// of `let x = 1;` open with `let` and close with `;`, and neither delimits
/// anything.
///
/// Nodes without such a pair get empty ranges pinned to the node's own
/// boundaries, which is the convention the extent helpers read as "no explicit
/// delimiter, derive the extent from the children instead".
fn delimiter_ranges(
    arena: &SyntaxArena<'_>,
    node: tree_sitter::Node<'_>,
    child_ids: &[SyntaxId],
) -> (Range<usize>, Range<usize>) {
    let undelimited = || {
        (
            node.start_byte()..node.start_byte(),
            node.end_byte()..node.end_byte(),
        )
    };

    let (Some(first), Some(last)) = (child_ids.first(), child_ids.last()) else {
        return undelimited();
    };
    if first == last {
        return undelimited();
    }

    let (Syntax::Atom(open), Syntax::Atom(close)) = (arena.get(*first), arena.get(*last)) else {
        return undelimited();
    };
    if !BRACKET_PAIRS.contains(&(open.content, close.content)) {
        return undelimited();
    }

    (open.byte_range.clone(), close.byte_range.clone())
}

/// Bracket pairs that delimit a node when they sit at its boundary.
///
/// Tree-sitter has no notion of which anonymous tokens delimit rather than
/// separate, and there is no per-language delimiter configuration to consult,
/// so the set is fixed. The angle brackets are here for generic argument and
/// parameter lists.
const BRACKET_PAIRS: [(&str, &str); 4] = [("(", ")"), ("[", "]"), ("{", "}"), ("<", ">")];

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

    /// Depth-first search for the first list of `kind`.
    fn find_list<'a>(arena: &'a SyntaxArena<'a>, root: SyntaxId, kind: &str) -> &'a List<'a> {
        let mut stack = vec![root];
        while let Some(id) = stack.pop() {
            if let Syntax::List(list) = arena.get(id) {
                if list.kind == kind {
                    return list;
                }
                stack.extend(list.children.iter().copied());
            }
        }
        panic!("no {kind} list in the lowered tree");
    }

    #[test]
    fn anonymous_tokens_become_atoms() {
        let source = "fn a() { x + y; }";
        let lang = rust_lang();
        let tree = parse(&lang, source, None).unwrap();
        let (arena, root_id) = lower_tree(&tree, source);

        let binary = find_list(&arena, root_id, "binary_expression");
        let contents: Vec<&str> = binary
            .children
            .iter()
            .map(|c| match arena.get(*c) {
                Syntax::Atom(a) => a.content,
                Syntax::List(l) => l.kind,
            })
            .collect();
        assert_eq!(contents, ["x", "+", "y"], "the operator is a child atom");
    }

    #[test]
    fn a_bracket_pair_at_the_boundary_becomes_the_delimiters() {
        let source = "fn a() { x + y; }";
        let lang = rust_lang();
        let tree = parse(&lang, source, None).unwrap();
        let (arena, root_id) = lower_tree(&tree, source);

        let block = find_list(&arena, root_id, "block");
        assert_eq!(&source[block.open_byte_range.clone()], "{");
        assert_eq!(&source[block.close_byte_range.clone()], "}");
    }

    #[test]
    fn a_node_whose_ends_are_not_a_bracket_pair_has_no_delimiters() {
        // `let x = 1;` opens on the `let` keyword and closes on the `;`.
        // Both are anonymous tokens sitting where a delimiter would, and
        // neither delimits anything.
        let source = "fn a() { let x = 1; }";
        let lang = rust_lang();
        let tree = parse(&lang, source, None).unwrap();
        let (arena, root_id) = lower_tree(&tree, source);

        let decl = find_list(&arena, root_id, "let_declaration");
        let ends: Vec<&str> = [decl.children.first(), decl.children.last()]
            .iter()
            .map(|c| match arena.get(*c.expect("children")) {
                Syntax::Atom(a) => a.content,
                Syntax::List(l) => l.kind,
            })
            .collect();
        assert_eq!(ends, ["let", ";"], "the fixture must end in bare tokens");

        assert!(decl.open_byte_range.is_empty(), "no opening delimiter");
        assert!(decl.close_byte_range.is_empty(), "no closing delimiter");
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
