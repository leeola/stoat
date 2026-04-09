use crate::language::{Language, TokenStyle};
use std::ops::Range;
use stoat_text::{patch::Edit as PatchEdit, ChunksInRange, Point, Rope};
use tree_sitter::{
    InputEdit, Node, Parser, Point as TsPoint, QueryCursor, StreamingIterator, TextProvider, Tree,
};

pub struct SyntaxState {
    pub tree: Tree,
    pub version: u64,
    pub rope_snapshot: Rope,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub byte_range: Range<usize>,
    pub style: TokenStyle,
}

pub fn parse(language: &Language, text: &str, old_tree: Option<&Tree>) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&language.grammar).ok()?;
    parser.parse(text, old_tree)
}

/// Parse `rope` incrementally without copying its bytes.
///
/// Calls [`tree_sitter::Parser::parse_with_options`] with a callback that
/// streams chunks from a single [`ChunksInRange`] cursor. When the parser
/// asks for bytes ahead of the current cursor position, the callback advances
/// the iterator; when it asks for bytes before the current position (rare),
/// the callback re-creates the iterator from the new offset. No `String`
/// or `Vec<u8>` allocation of the rope text occurs.
///
/// `old_tree` enables incremental parsing: pass the previous [`Tree`] (after
/// applying [`edit_tree`]) so tree-sitter can reuse unchanged subtrees.
pub fn parse_rope(language: &Language, rope: &Rope, old_tree: Option<&Tree>) -> Option<Tree> {
    let mut parser = Parser::new();
    parser.set_language(&language.grammar).ok()?;

    // Cursor state shared across callback invocations.
    struct CursorState<'a> {
        rope: &'a Rope,
        chunks: Option<ChunksInRange<'a>>,
        pending: &'a str,
        // Byte offset of the start of `pending` within the rope.
        pending_start: usize,
    }

    let mut state = CursorState {
        rope,
        chunks: None,
        pending: "",
        pending_start: 0,
    };

    let total_len = rope.len();

    let mut callback = |byte_offset: usize, _position: TsPoint| -> &[u8] {
        if byte_offset >= total_len {
            return &[];
        }

        // If `byte_offset` falls within the current pending chunk, return the
        // suffix starting at the requested offset.
        let pending_end = state.pending_start + state.pending.len();
        if byte_offset >= state.pending_start && byte_offset < pending_end {
            let local = byte_offset - state.pending_start;
            return state.pending.as_bytes().get(local..).unwrap_or(&[]);
        }

        // Backward jump or first call: rebuild the iterator from the
        // requested offset onward.
        if byte_offset < state.pending_start || state.chunks.is_none() {
            state.chunks = Some(state.rope.chunks_in_range(byte_offset..total_len));
            state.pending = "";
            state.pending_start = byte_offset;
        }

        // Pull chunks until one covers the requested offset. Each iteration
        // advances past the previous chunk before pulling the next.
        loop {
            let chunk_end = state.pending_start + state.pending.len();
            state.pending_start = chunk_end;
            state.pending = "";

            let Some(chunk) = state.chunks.as_mut().and_then(|it| it.next()) else {
                return &[];
            };
            state.pending = chunk;
            let new_chunk_end = state.pending_start + chunk.len();
            if byte_offset < new_chunk_end {
                let local = byte_offset - state.pending_start;
                return chunk.as_bytes().get(local..).unwrap_or(&[]);
            }
        }
    };

    parser.parse_with_options(&mut callback, old_tree, None)
}

/// Apply `edits` to `tree` so it can be used as `old_tree` for an incremental
/// re-parse against `new_rope`. Edits are applied in patch order.
pub fn edit_tree(tree: &mut Tree, edits: &[PatchEdit<usize>], old_rope: &Rope, new_rope: &Rope) {
    for edit in edits {
        let start_byte = edit.old.start;
        let old_end_byte = edit.old.end;
        let new_end_byte = edit.new.end;
        let start_position = stoat_to_ts(old_rope.offset_to_point(start_byte));
        let old_end_position = stoat_to_ts(old_rope.offset_to_point(old_end_byte));
        let new_end_position = stoat_to_ts(new_rope.offset_to_point(new_end_byte));
        tree.edit(&InputEdit {
            start_byte,
            old_end_byte,
            new_end_byte,
            start_position,
            old_end_position,
            new_end_position,
        });
    }
}

fn stoat_to_ts(p: Point) -> TsPoint {
    TsPoint {
        row: p.row as usize,
        column: p.column as usize,
    }
}

/// Like [`extract_highlights`] but reads node text from a [`Rope`] instead of
/// a flat `&str`. Tree-sitter pulls bytes for each captured node via the
/// [`TextProvider`] trait, so the rope storage is the source of truth and no
/// copy is required.
///
/// Also walks any registered language injections (see
/// [`crate::language::Language::injections`]): for each host node whose kind
/// matches an injection's `host_node_kind`, parses the node's byte range with
/// the inner grammar and merges its highlights into the result. Injection
/// trees are full-reparsed each call; the host tree benefits from incremental
/// reparse via [`parse_rope`].
pub fn extract_highlights_rope(
    language: &Language,
    tree: &Tree,
    rope: &Rope,
) -> Vec<HighlightSpan> {
    let mut raw: Vec<(Range<usize>, usize, TokenStyle)> = Vec::new();
    collect_highlights_into(&mut raw, language, tree, rope);

    if !language.injections.is_empty() {
        let mut walker = tree.walk();
        loop {
            let node = walker.node();
            let kind = node.kind();
            for injection in &language.injections {
                if injection.host_node_kind == kind {
                    let inner_start = node.start_byte();
                    let inner_end = node.end_byte();
                    if inner_end <= inner_start {
                        continue;
                    }
                    let inner_rope_text: String =
                        rope.chunks_in_range(inner_start..inner_end).collect();
                    // For now, parse the inner range as a string. Falling back
                    // to a string here keeps the injection path simple at the
                    // cost of one allocation per `inline` node.
                    let Some(inner_tree) = parse(&injection.inner, &inner_rope_text, None) else {
                        continue;
                    };
                    let mut inner_raw: Vec<(Range<usize>, usize, TokenStyle)> = Vec::new();
                    collect_highlights_from_str(
                        &mut inner_raw,
                        &injection.inner,
                        &inner_tree,
                        &inner_rope_text,
                    );
                    // Translate inner-relative byte offsets back into the
                    // host's coordinate space.
                    for (range, pattern, style) in inner_raw {
                        raw.push((
                            (range.start + inner_start)..(range.end + inner_start),
                            pattern,
                            style,
                        ));
                    }
                }
            }

            // Depth-first walk: descend if possible, otherwise advance to the
            // next sibling, ascending until one is available.
            if walker.goto_first_child() {
                continue;
            }
            loop {
                if walker.goto_next_sibling() {
                    break;
                }
                if !walker.goto_parent() {
                    // Walked back to the root with no more siblings.
                    raw.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(a.1.cmp(&b.1)));
                    return raw
                        .into_iter()
                        .map(|(byte_range, _, style)| HighlightSpan { byte_range, style })
                        .collect();
                }
            }
        }
    }

    raw.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(a.1.cmp(&b.1)));
    raw.into_iter()
        .map(|(byte_range, _, style)| HighlightSpan { byte_range, style })
        .collect()
}

fn collect_highlights_into(
    raw: &mut Vec<(Range<usize>, usize, TokenStyle)>,
    language: &Language,
    tree: &Tree,
    rope: &Rope,
) {
    let mut cursor = QueryCursor::new();
    let provider = RopeTextProvider { rope };
    let mut matches = cursor.matches(&language.highlight_query, tree.root_node(), provider);
    while let Some(m) = matches.next() {
        let pattern = m.pattern_index;
        for capture in m.captures {
            let style = match language.capture_styles.get(capture.index as usize) {
                Some(Some(s)) => *s,
                _ => continue,
            };
            let start = capture.node.start_byte();
            let end = capture.node.end_byte();
            if start == end {
                continue;
            }
            raw.push((start..end, pattern, style));
        }
    }
}

fn collect_highlights_from_str(
    raw: &mut Vec<(Range<usize>, usize, TokenStyle)>,
    language: &Language,
    tree: &Tree,
    text: &str,
) {
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&language.highlight_query, tree.root_node(), text.as_bytes());
    while let Some(m) = matches.next() {
        let pattern = m.pattern_index;
        for capture in m.captures {
            let style = match language.capture_styles.get(capture.index as usize) {
                Some(Some(s)) => *s,
                _ => continue,
            };
            let start = capture.node.start_byte();
            let end = capture.node.end_byte();
            if start == end {
                continue;
            }
            raw.push((start..end, pattern, style));
        }
    }
}

/// `TextProvider` over a stoat [`Rope`]. For each query node, returns an
/// iterator of borrowed `&[u8]` slices spanning the node's byte range.
struct RopeTextProvider<'a> {
    rope: &'a Rope,
}

impl<'a> TextProvider<&'a [u8]> for RopeTextProvider<'a> {
    type I = RopeNodeBytes<'a>;

    fn text(&mut self, node: Node<'_>) -> Self::I {
        RopeNodeBytes {
            chunks: self
                .rope
                .chunks_in_range(node.start_byte()..node.end_byte()),
        }
    }
}

struct RopeNodeBytes<'a> {
    chunks: ChunksInRange<'a>,
}

impl<'a> Iterator for RopeNodeBytes<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        self.chunks.next().map(|s| s.as_bytes())
    }
}

// Tree-sitter highlight query convention: later patterns in the .scm file
// are more specific and should override earlier ones. Sorting emitted spans
// by (start, pattern_index) gives the downstream merger a stable order where
// later-listed patterns end up in later slots, so a more-specific capture
// wins over a less-specific one at the same byte range.
pub fn extract_highlights(language: &Language, tree: &Tree, text: &str) -> Vec<HighlightSpan> {
    let mut raw: Vec<(Range<usize>, usize, TokenStyle)> = Vec::new();
    let mut cursor = QueryCursor::new();
    let mut matches = cursor.matches(&language.highlight_query, tree.root_node(), text.as_bytes());

    while let Some(m) = matches.next() {
        let pattern = m.pattern_index;
        for capture in m.captures {
            let style = match language.capture_styles.get(capture.index as usize) {
                Some(Some(s)) => *s,
                _ => continue,
            };
            let start = capture.node.start_byte();
            let end = capture.node.end_byte();
            if start == end {
                continue;
            }
            raw.push((start..end, pattern, style));
        }
    }

    raw.sort_by(|a, b| a.0.start.cmp(&b.0.start).then(a.1.cmp(&b.1)));

    raw.into_iter()
        .map(|(byte_range, _, style)| HighlightSpan { byte_range, style })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{extract_highlights, parse, HighlightSpan};
    use crate::language::{LanguageRegistry, TokenStyle};

    fn rust() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .unwrap()
    }

    fn json() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.json"))
            .unwrap()
    }

    fn toml() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.toml"))
            .unwrap()
    }

    fn markdown() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.md"))
            .unwrap()
    }

    /// Returns the last span matching the byte range of `fragment` in `text`.
    /// Last is the one the downstream merger picks for overlapping spans.
    fn span_for_text<'a>(
        spans: &'a [HighlightSpan],
        text: &str,
        fragment: &str,
    ) -> Option<&'a HighlightSpan> {
        let start = text.find(fragment)?;
        let end = start + fragment.len();
        spans
            .iter()
            .rev()
            .find(|s| s.byte_range.start == start && s.byte_range.end == end)
    }

    #[test]
    fn parse_rust_smoke() {
        let lang = rust();
        let tree = parse(&lang, "fn main() {}", None).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
    }

    #[test]
    fn parse_json_smoke() {
        let lang = json();
        let tree = parse(&lang, "{}", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
    }

    #[test]
    fn parse_toml_smoke() {
        let lang = toml();
        let tree = parse(&lang, "a = 1\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
    }

    #[test]
    fn parse_markdown_smoke() {
        let lang = markdown();
        let tree = parse(&lang, "# Title\n", None).unwrap();
        assert_eq!(tree.root_node().kind(), "document");
    }

    #[test]
    fn rust_keyword_captured() {
        let text = "fn main() {}";
        let lang = rust();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let span = span_for_text(&spans, text, "fn").expect("fn span");
        assert_eq!(span.style, TokenStyle::Keyword);
    }

    #[test]
    fn rust_string_captured() {
        let text = r#"fn main() { let _ = "hi"; }"#;
        let lang = rust();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let span = span_for_text(&spans, text, "\"hi\"").expect("string span");
        assert_eq!(span.style, TokenStyle::String);
    }

    #[test]
    fn rust_function_definition_captured() {
        let text = "fn main() {}";
        let lang = rust();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let span = span_for_text(&spans, text, "main").expect("main span");
        assert_eq!(span.style, TokenStyle::Function);
    }

    #[test]
    fn json_property_and_string_captured() {
        let text = r#"{"a":"b"}"#;
        let lang = json();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let key = span_for_text(&spans, text, "\"a\"").expect("key span");
        let value = span_for_text(&spans, text, "\"b\"").expect("value span");
        assert_eq!(key.style, TokenStyle::Property);
        assert_eq!(value.style, TokenStyle::String);
    }

    #[test]
    fn markdown_title_captured() {
        let text = "# Title\n";
        let lang = markdown();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        assert!(
            spans.iter().any(|s| s.style == TokenStyle::Title),
            "expected at least one Title span, got {spans:?}"
        );
    }

    #[test]
    fn spans_sorted_by_start() {
        let text = "fn a() { fn b() {} }";
        let lang = rust();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        for w in spans.windows(2) {
            assert!(w[0].byte_range.start <= w[1].byte_range.start);
        }
    }

    #[test]
    fn parse_rope_matches_parse_str() {
        use stoat_text::Rope;
        let text = "fn main() { let x = 1; }";
        let rope = Rope::from(text);
        let lang = rust();
        let tree_str = parse(&lang, text, None).unwrap();
        let tree_rope = super::parse_rope(&lang, &rope, None).unwrap();
        // Both should produce the same root node kind and same byte range.
        assert_eq!(tree_str.root_node().kind(), tree_rope.root_node().kind());
        assert_eq!(
            tree_str.root_node().byte_range(),
            tree_rope.root_node().byte_range()
        );
    }

    #[test]
    fn parse_rope_handles_multi_chunk_rope() {
        use stoat_text::Rope;
        // Build a rope big enough to span multiple internal chunks.
        let text: String = "fn a() {}\n".repeat(200);
        let rope = Rope::from(text.as_str());
        assert!(
            rope.chunks().count() > 1,
            "test precondition: multi-chunk rope",
        );
        let lang = rust();
        let tree = super::parse_rope(&lang, &rope, None).unwrap();
        assert_eq!(tree.root_node().kind(), "source_file");
        assert_eq!(tree.root_node().byte_range(), 0..text.len());
    }

    #[test]
    fn extract_highlights_rope_matches_string_form() {
        use stoat_text::Rope;
        let text = "fn main() { let x = \"hi\"; }";
        let rope = Rope::from(text);
        let lang = rust();
        let tree_rope = super::parse_rope(&lang, &rope, None).unwrap();
        let spans_rope = super::extract_highlights_rope(&lang, &tree_rope, &rope);
        let tree_str = parse(&lang, text, None).unwrap();
        let spans_str = extract_highlights(&lang, &tree_str, text);
        assert_eq!(spans_rope, spans_str);
    }

    #[test]
    fn markdown_inline_emphasis_captured_via_injection() {
        // Top-level markdown only emits block-level captures (e.g. titles).
        // The inline grammar is registered as an injection inside `inline`
        // nodes, so `**bold**` produces an EmphasisStrong span when extracted
        // via the rope path.
        use stoat_text::Rope;
        let lang = markdown();
        let text = "**bold** and *italic*\n";
        let rope = Rope::from(text);
        let tree = super::parse_rope(&lang, &rope, None).unwrap();
        let spans = super::extract_highlights_rope(&lang, &tree, &rope);

        let bold_start = text.find("**bold**").unwrap();
        let bold_end = bold_start + "**bold**".len();
        assert!(
            spans.iter().any(|s| s.style == TokenStyle::EmphasisStrong
                && s.byte_range.start >= bold_start
                && s.byte_range.end <= bold_end),
            "expected EmphasisStrong span inside `**bold**`, got {spans:?}",
        );
    }

    #[test]
    fn edit_tree_then_parse_rope_reuses_old_tree() {
        use stoat_text::{patch::Edit as PatchEdit, Rope};
        let lang = rust();
        let original = "fn main() { let x = 1; }";
        let old_rope = Rope::from(original);
        let mut tree = super::parse_rope(&lang, &old_rope, None).unwrap();

        // Insert "let y = 2; " before the closing brace of `main`.
        // Original byte 23 is the position just before `}`.
        let insert_pos = 23;
        let inserted = "let y = 2; ";
        let mut new_text = String::new();
        new_text.push_str(&original[..insert_pos]);
        new_text.push_str(inserted);
        new_text.push_str(&original[insert_pos..]);
        let new_rope = Rope::from(new_text.as_str());

        let edits = vec![PatchEdit {
            old: insert_pos..insert_pos,
            new: insert_pos..(insert_pos + inserted.len()),
        }];

        super::edit_tree(&mut tree, &edits, &old_rope, &new_rope);
        let new_tree = super::parse_rope(&lang, &new_rope, Some(&tree)).unwrap();
        assert_eq!(new_tree.root_node().kind(), "source_file");
        assert_eq!(new_tree.root_node().byte_range(), 0..new_text.len());

        // Equivalence check: a fresh full parse must produce the same root.
        let fresh = super::parse_rope(&lang, &new_rope, None).unwrap();
        assert_eq!(
            new_tree.root_node().to_sexp(),
            fresh.root_node().to_sexp(),
            "incremental and full parse must agree on tree shape",
        );
    }
}
