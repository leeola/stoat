//! End-to-end tests for the display map, driven through
//! [`crate::test_harness::TestHarness`].

#[test]
fn typing_mid_buffer_keeps_the_last_line_rendered() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 10);
    let path = h.write_file("edit.txt", "alpha\nbravo\ncharlie\ndelta\n");
    h.open_file(&path);

    h.type_keys("j j i");
    h.type_text("XY");

    let frame = h.snapshot();
    assert!(
        frame.content.contains("delta"),
        "the last line stays rendered while typing mid-buffer:\n{}",
        frame.content,
    );
}

#[test]
fn snapshot_open_rust_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.rs", "fn main() {\n    let x = \"hi\";\n}\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_rust_file_highlights");
}

#[test]
fn snapshot_open_json_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.json", "{\n  \"a\": 1\n}\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_json_file_highlights");
}

#[test]
fn snapshot_open_yaml_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.yaml", "a: 1\nb: \"x\"\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_yaml_file_highlights");
}

#[test]
fn snapshot_open_ron_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.ron", "Foo(\n  a: 1,\n)\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_ron_file_highlights");
}

#[test]
fn snapshot_open_csv_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.csv", "a,b\n1,2\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_csv_file_highlights");
}

#[test]
fn snapshot_open_bash_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.sh", "for f in a b; do\n  echo \"$f\" # c\ndone\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_bash_file_highlights");
}

#[test]
fn snapshot_open_fish_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.fish", "for f in a b\n  echo \"$f\" # c\nend\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_fish_file_highlights");
}

#[test]
fn snapshot_open_markdown_file_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.md", "# Title\n\nbody\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_markdown_file_highlights");
}

#[test]
fn snapshot_open_markdown_file_with_bold_inline() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("bold.md", "# Title\n\n**bold** text\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_markdown_file_with_bold_inline");
}

#[test]
fn snapshot_open_unknown_extension_no_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("sample.txt", "fn main() {}\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_unknown_extension_no_highlights");
}

#[test]
fn snapshot_open_rust_file_nested_captures() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("nested.rs", "fn main() { \"a\\nb\"; }\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_open_rust_file_nested_captures");
}

#[test]
fn snapshot_open_rust_file_then_edit_highlights() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file("edit.rs", "fn a() {}\n");

    h.open_file(&path);
    h.edit_focused(8..8, " let x = 1; ");
    h.assert_snapshot("snapshot_open_rust_file_then_edit_highlights");
}

#[test]
fn snapshot_rust_doc_comment_markdown() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 6);
    let path = h.write_file(
        "doc.rs",
        "/// A **bold** word and a [link](url).\nfn a() {}\n",
    );

    h.open_file(&path);
    h.assert_snapshot("snapshot_rust_doc_comment_markdown");
}

#[test]
fn snapshot_markdown_fence_highlight() {
    let mut h = crate::test_harness::TestHarness::with_size(40, 8);
    let path = h.write_file("doc.md", "# Title\n\n```rust\nfn a() {}\n```\n");

    h.open_file(&path);
    h.assert_snapshot("snapshot_markdown_fence_highlight");
}

#[test]
fn snapshot_open_rust_file_with_fold() {
    use stoat_text::Point;
    let mut h = crate::test_harness::TestHarness::with_size(40, 8);
    let path = h.write_file("folded.rs", "fn a() { 1 }\nfn b() { 2 }\nfn c() { 3 }\n");

    h.open_file(&path);
    h.fold_focused(Point::new(1, 7)..Point::new(1, 12));
    h.assert_snapshot("snapshot_open_rust_file_with_fold");
}
