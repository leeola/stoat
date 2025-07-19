//! Simple word/whitespace syntax for testing

/// Kinds of nodes in simple text
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimpleKind {
    /// Root node
    Root,
    /// A word
    Word,
    /// Whitespace
    Whitespace,
    /// A line
    Line,
}

#[cfg(test)]
mod tests {
    use crate::{SyntaxKind, TextRange, syntax::SyntaxElement};

    #[test]
    fn test_parse_single_line() {
        let text = "hello world";
        let result = crate::syntax::parse::parse_simple(text);
        let root = result.root;

        assert_eq!(root.kind(), SyntaxKind::Root);
        assert_eq!(root.text_range(), TextRange::new(0.into(), 11.into()));

        // Check children: should have one Line node
        let children = root.children();
        assert_eq!(children.len(), 1);

        // Check the line node
        let line = match &children[0] {
            SyntaxElement::Node(n) => n,
            _ => panic!("Expected line node"),
        };
        assert_eq!(line.kind(), SyntaxKind::Line);
        assert_eq!(line.text_range(), TextRange::new(0.into(), 11.into()));

        // Check line's children: Word "hello", Whitespace " ", Word "world"
        let line_children = line.children();
        assert_eq!(line_children.len(), 3);

        // First word "hello"
        match &line_children[0] {
            SyntaxElement::Token(t) => {
                assert_eq!(t.kind(), SyntaxKind::Word);
                assert_eq!(t.text(), "hello");
                assert_eq!(t.text_range(), TextRange::new(0.into(), 5.into()));
            },
            _ => panic!("Expected word token"),
        }

        // Whitespace
        match &line_children[1] {
            SyntaxElement::Token(t) => {
                assert_eq!(t.kind(), SyntaxKind::Whitespace);
                assert_eq!(t.text(), " ");
                assert_eq!(t.text_range(), TextRange::new(5.into(), 6.into()));
            },
            _ => panic!("Expected whitespace token"),
        }

        // Second word "world"
        match &line_children[2] {
            SyntaxElement::Token(t) => {
                assert_eq!(t.kind(), SyntaxKind::Word);
                assert_eq!(t.text(), "world");
                assert_eq!(t.text_range(), TextRange::new(6.into(), 11.into()));
            },
            _ => panic!("Expected word token"),
        }
    }

    #[test]
    fn test_parse_multiple_lines() {
        let text = "hello world\nsecond line";
        let result = crate::syntax::parse::parse_simple(text);
        let root = result.root;

        assert_eq!(root.kind(), SyntaxKind::Root);
        assert_eq!(root.text_range(), TextRange::new(0.into(), 23.into()));

        // Check children: Line, Whitespace (newline), Line
        let children = root.children();
        assert_eq!(children.len(), 3);

        // First line
        match &children[0] {
            SyntaxElement::Node(n) => {
                assert_eq!(n.kind(), SyntaxKind::Line);
                assert_eq!(n.text_range(), TextRange::new(0.into(), 11.into()));
            },
            _ => panic!("Expected line node"),
        }

        // Newline
        match &children[1] {
            SyntaxElement::Token(t) => {
                assert_eq!(t.kind(), SyntaxKind::Whitespace);
                assert_eq!(t.text(), "\n");
                assert_eq!(t.text_range(), TextRange::new(11.into(), 12.into()));
            },
            _ => panic!("Expected newline token"),
        }

        // Second line
        match &children[2] {
            SyntaxElement::Node(n) => {
                assert_eq!(n.kind(), SyntaxKind::Line);
                assert_eq!(n.text_range(), TextRange::new(12.into(), 23.into()));

                // Check it has the right content
                let line_children = n.children();
                assert_eq!(line_children.len(), 3); // "second" " " "line"
            },
            _ => panic!("Expected line node"),
        }
    }

    #[test]
    fn test_parse_empty_lines() {
        let text = "hello\n\nworld";
        let result = crate::syntax::parse::parse_simple(text);
        let root = result.root;

        // Should have: Line("hello"), Newline, Line(""), Newline, Line("world")
        let children = root.children();
        assert_eq!(children.len(), 5);

        // Check empty line
        match &children[2] {
            SyntaxElement::Node(n) => {
                assert_eq!(n.kind(), SyntaxKind::Line);
                assert_eq!(n.children().len(), 0); // Empty line has no children
            },
            _ => panic!("Expected empty line node"),
        }
    }

    #[test]
    fn test_navigation() {
        let text = "hello world\ntest";
        let result = crate::syntax::parse::parse_simple(text);
        let root = result.root;

        // Test first_child
        let first_line = root.first_child().expect("Should have first child");
        assert_eq!(first_line.kind(), SyntaxKind::Line);

        // Test parent navigation
        assert!(first_line.parent().is_some());
        let parent = first_line.parent().expect("Line should have parent");
        assert_eq!(parent.kind(), SyntaxKind::Root);

        // Test finding words
        let words: Vec<_> = root
            .tokens()
            .into_iter()
            .filter(|t| t.kind() == SyntaxKind::Word)
            .collect();
        assert_eq!(words.len(), 3); // "hello", "world", "test"
        assert_eq!(words[0].text(), "hello");
        assert_eq!(words[1].text(), "world");
        assert_eq!(words[2].text(), "test");
    }

    #[test]
    fn test_ast_enables_word_navigation() {
        let text = "fn hello_world() { println!(\"hello world!\"); }";
        let result = crate::syntax::parse::parse_simple(text);
        let root = result.root;

        // The AST should parse this as words and whitespace
        let words: Vec<_> = root
            .tokens()
            .into_iter()
            .filter(|t| t.kind() == SyntaxKind::Word)
            .collect();

        // Check we can navigate through the words
        assert!(!words.is_empty());
        assert_eq!(words[0].text(), "fn");
        assert_eq!(words[1].text(), "hello_world()");
        assert_eq!(words[2].text(), "{");
        assert_eq!(words[3].text(), "println!(\"hello");
        assert_eq!(words[4].text(), "world!\");");
        assert_eq!(words[5].text(), "}");
    }
}
