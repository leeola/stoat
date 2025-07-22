//! Test utilities for reducing boilerplate in AST tests

use crate::{Arena, Node};

/// Build a tree using a compact syntax
///
/// # Examples
/// ```
/// let doc = tree!(arena, Document[Paragraph[Text("hello")]]);
/// let leaf = tree!(arena, Text("hello world"));
/// ```
#[macro_export]
macro_rules! tree {
    // Base case: leaf node with text
    ($arena:expr, $kind:ident($text:expr)) => {
        $arena.alloc($crate::Node::leaf($crate::SyntaxKind::$kind, $text))
    };

    // Internal node - parse contents as token trees
    ($arena:expr, $kind:ident[$($contents:tt)*]) => {{
        tree!(@internal $arena, $kind, [], $($contents)*)
    }};

    // Helper rule to parse children one by one
    (@internal $arena:expr, $kind:ident, [$($parsed:expr),*], ) => {{
        let children = vec![$($parsed),*];
        $arena.alloc($crate::Node::internal($crate::SyntaxKind::$kind, children))
    }};

    // Parse a simple identifier (reference to existing node)
    (@internal $arena:expr, $kind:ident, [$($parsed:expr),*], $child:ident $(, $($rest:tt)*)?) => {{
        tree!(@internal $arena, $kind, [$($parsed,)* $child], $($($rest)*)?)
    }};

    // Parse a leaf child with parentheses
    (@internal $arena:expr, $kind:ident, [$($parsed:expr),*], $child_kind:ident($text:expr) $(, $($rest:tt)*)?) => {{
        let child = tree!($arena, $child_kind($text));
        tree!(@internal $arena, $kind, [$($parsed,)* child], $($($rest)*)?)
    }};

    // Parse an internal child with brackets
    (@internal $arena:expr, $kind:ident, [$($parsed:expr),*], $child_kind:ident[$($child_contents:tt)*] $(, $($rest:tt)*)?) => {{
        let child = tree!($arena, $child_kind[$($child_contents)*]);
        tree!(@internal $arena, $kind, [$($parsed,)* child], $($($rest)*)?)
    }};
}

/// Assert that a node has a specific kind
#[macro_export]
macro_rules! assert_kind {
    ($node:expr, $expected:ident) => {
        assert_eq!(
            $node.kind(),
            $crate::SyntaxKind::$expected,
            "Expected {:?} but got {:?}",
            $crate::SyntaxKind::$expected,
            $node.kind()
        );
    };
}

/// Assert that a leaf node has specific text
#[macro_export]
macro_rules! assert_text {
    ($node:expr, $expected:expr) => {
        assert!(
            $node.is_leaf(),
            "Expected leaf node but got internal node with {} children",
            $node.children().len()
        );
        assert_eq!(
            $node.text(),
            $expected,
            "Expected text '{}' but got '{}'",
            $expected,
            $node.text()
        );
    };
}

/// Format a tree structure as a string for debugging
pub fn tree_structure<'a>(node: &'a Node<'a>) -> String {
    fn format_tree<'a>(node: &'a Node<'a>, indent: usize, buffer: &mut String) {
        // Add indentation
        for _ in 0..indent {
            buffer.push_str("  ");
        }

        // Add node info
        buffer.push_str(&format!("{:?}", node.kind()));
        if node.is_leaf() && !node.text().is_empty() {
            buffer.push_str(&format!("({})", node.text()));
        }
        buffer.push('\n');

        // Recursively format children
        for child in node.children() {
            format_tree(child, indent + 1, buffer);
        }
    }

    let mut buffer = String::new();
    format_tree(node, 0, &mut buffer);
    buffer.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tree_macro_leaf() {
        let arena = Arena::new();
        let node = tree!(arena, Text("hello"));

        assert_kind!(node, Text);
        assert_text!(node, "hello");
        assert_eq!(node.children().len(), 0);
    }

    #[test]
    fn test_tree_macro_nested() {
        let arena = Arena::new();

        // Build tree bottom-up
        let hello = tree!(arena, Text("Hello"));
        let world = tree!(arena, Text("world"));
        let para = tree!(arena, Paragraph[hello, world]);
        let doc = tree!(arena, Document[para]);

        assert_kind!(doc, Document);
        assert_eq!(doc.children().len(), 1);

        let para_ref = doc.children()[0];
        assert_kind!(para_ref, Paragraph);
        assert_eq!(para_ref.children().len(), 2);

        assert_text!(para_ref.children()[0], "Hello");
        assert_text!(para_ref.children()[1], "world");
    }

    #[test]
    fn test_tree_structure_formatting() {
        let arena = Arena::new();
        let doc = tree!(arena, Document[Paragraph[Text("Hello"), Text("world")]]);

        let structure = tree_structure(doc);
        let expected = "Document\n  Paragraph\n    Text(Hello)\n    Text(world)";
        assert_eq!(structure, expected);
    }

    #[test]
    fn test_nested_tree_macro() {
        let arena = Arena::new();

        // Test direct nested syntax
        let doc = tree!(arena, Document[Paragraph[Text("Hello"), Text("world")]]);

        assert_kind!(doc, Document);
        assert_eq!(doc.children().len(), 1);

        let para = doc.children()[0];
        assert_kind!(para, Paragraph);
        assert_eq!(para.children().len(), 2);

        assert_text!(para.children()[0], "Hello");
        assert_text!(para.children()[1], "world");
    }

    #[test]
    fn test_complex_tree() {
        let arena = Arena::new();

        // Build a more complex tree with multiple levels
        let tree = tree!(arena, Module[
            Block[
                Line[Text("fn"), Whitespace(" "), Identifier("main"), Text("()")],
                Line[Text("{")],
                Line[
                    Whitespace("    "),
                    Identifier("println!"),
                    Text("("),
                    String("\"Hello, world!\""),
                    Text(")")
                ],
                Line[Text("}")]
            ]
        ]);

        assert_kind!(tree, Module);
        assert_eq!(tree.children().len(), 1);

        let block = tree.children()[0];
        assert_kind!(block, Block);
        assert_eq!(block.children().len(), 4);

        // Verify first line
        let line1 = block.children()[0];
        assert_eq!(line1.children().len(), 4);
        assert_text!(line1.children()[0], "fn");
        assert_kind!(line1.children()[1], Whitespace);
        assert_text!(line1.children()[2], "main");
    }
}
