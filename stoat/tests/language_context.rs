//! Language context detection and switching tests.

use stoat::Stoat;
use stoat_text::parser::Language;

#[test]
fn markdown_language_context_detection() {
    let markdown_text = r#"# Heading

This is markdown text.

```text
This is plain text in a code block
It should have Language::PlainText
```

More markdown after the code block."#;

    Stoat::test()
        .with_text_and_language(markdown_text, Language::Markdown)
        // Cursor starts at beginning - should be markdown
        .assert_language(stoat_rope::Language::Markdown)
        // Move to line 5, column 10 (inside the code block text content, not at start)
        .cursor(5, 10)
        .assert_language(stoat_rope::Language::PlainText)
        // Move back to markdown area (line 2)
        .cursor(2, 0)
        .assert_language(stoat_rope::Language::Markdown)
        // Move to the last line (markdown area)
        .cursor(9, 0)
        .assert_language(stoat_rope::Language::Markdown);
}
