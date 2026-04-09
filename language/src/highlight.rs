use crate::language::Language;
use std::{
    cell::Cell,
    ops::{ControlFlow, Deref, DerefMut, Range},
    sync::{
        mpsc::{channel, Sender},
        LazyLock, Mutex,
    },
    thread,
    time::Instant,
};
use stoat_text::{patch::Edit as PatchEdit, ChunksInRange, Point, Rope};
use tree_sitter::{
    InputEdit, Node, ParseOptions, ParseState, Parser, Point as TsPoint, QueryCursor,
    StreamingIterator, TextProvider, Tree,
};

pub struct SyntaxState {
    pub tree: Tree,
    pub version: u64,
    pub rope_snapshot: Rope,
    /// Per-host-node injection trees from the most recent extraction. Used
    /// as `old_tree` inputs on the next reparse so injected sub-trees can
    /// be re-used incrementally instead of full-parsed every keystroke.
    pub injection_trees: InjectionTreeCache,
}

/// Send `state` to a background drainer thread for destruction. Dropping a
/// large [`tree_sitter::Tree`] (deeply nested with many cached nodes) can
/// take milliseconds; the drainer keeps that cost off whichever thread
/// happens to be replacing the displaced state. The drainer thread is
/// spawned lazily on first use.
pub fn drop_syntax_in_background(state: SyntaxState) {
    static DROP_TX: LazyLock<Sender<SyntaxState>> = LazyLock::new(|| {
        let (tx, rx) = channel::<SyntaxState>();
        let _ = thread::Builder::new()
            .name("stoat-syntax-drop".into())
            .spawn(move || {
                while let Ok(state) = rx.recv() {
                    drop(state);
                }
            });
        tx
    });
    let _ = DROP_TX.send(state);
}

#[derive(Debug, Default)]
pub struct InjectionTreeCache {
    entries: Vec<InjectionTreeEntry>,
}

#[derive(Debug)]
struct InjectionTreeEntry {
    host_range: Range<usize>,
    language_name: &'static str,
    tree: Tree,
}

impl InjectionTreeCache {
    /// Pop and return the cached tree whose host range and language match
    /// `host_range` / `language_name`. Returns `None` if no entry matches;
    /// callers should fall through to a full parse in that case.
    fn take(&mut self, host_range: &Range<usize>, language_name: &'static str) -> Option<Tree> {
        let idx = self
            .entries
            .iter()
            .position(|e| e.host_range == *host_range && e.language_name == language_name)?;
        Some(self.entries.swap_remove(idx).tree)
    }

    fn push(&mut self, host_range: Range<usize>, language_name: &'static str, tree: Tree) {
        self.entries.push(InjectionTreeEntry {
            host_range,
            language_name,
            tree,
        });
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HighlightSpan {
    pub byte_range: Range<usize>,
    /// Theme-resolved [`HighlightId`] from the language's
    /// [`crate::Language::highlight_map`]. [`HighlightId::DEFAULT`]
    /// means the capture has no entry in the active theme; the
    /// host's renderer should treat such spans as unstyled.
    pub id: crate::highlight_map::HighlightId,
    /// Tree-sitter capture index from the language's
    /// [`crate::Language::highlight_query`]. Lets tests assert
    /// against the capture name without depending on a particular
    /// theme being installed.
    pub capture_index: u32,
}

/// Result of [`extract_highlights_rope_with_cache`]: the highlight spans
/// for `tree` plus the injection sub-trees that were parsed. Caller stores
/// the cache on the per-buffer [`SyntaxState`] for re-use on the next
/// reparse.
pub struct ExtractedHighlights {
    pub spans: Vec<HighlightSpan>,
    pub injection_trees: InjectionTreeCache,
}

static PARSERS: LazyLock<Mutex<Vec<Parser>>> = LazyLock::new(Default::default);
static QUERY_CURSORS: LazyLock<Mutex<Vec<QueryCursor>>> = LazyLock::new(Default::default);

/// Borrow a recycled [`tree_sitter::Parser`] from a process-wide pool, run
/// `f`, and return the parser to the pool. Included ranges are reset before
/// `f` runs so a prior caller cannot leak parser state into this one.
pub(crate) fn with_parser<R>(f: impl FnOnce(&mut Parser) -> R) -> R {
    let mut parser = PARSERS
        .lock()
        .expect("parser pool poisoned")
        .pop()
        .unwrap_or_else(Parser::new);
    let _ = parser.set_included_ranges(&[]);
    let result = f(&mut parser);
    PARSERS.lock().expect("parser pool poisoned").push(parser);
    result
}

/// RAII handle for a recycled [`tree_sitter::QueryCursor`]. The cursor's
/// match limit is bounded so pathological queries terminate, matching the
/// behavior used by Zed's syntax_map.
pub(crate) struct QueryCursorHandle(Option<QueryCursor>);

impl QueryCursorHandle {
    pub(crate) fn new() -> Self {
        let mut cursor = QUERY_CURSORS
            .lock()
            .expect("query cursor pool poisoned")
            .pop()
            .unwrap_or_default();
        cursor.set_match_limit(64);
        // Reset any prior `set_byte_range` so the borrowed cursor
        // never inherits a sub-range filter from an earlier user.
        // QueryCursor::set_byte_range is sticky; without this reset,
        // a test that filtered a query could leak its filter into the
        // next test that pops the same cursor from the pool.
        cursor.set_byte_range(0..usize::MAX);
        Self(Some(cursor))
    }
}

impl Deref for QueryCursorHandle {
    type Target = QueryCursor;
    fn deref(&self) -> &QueryCursor {
        self.0.as_ref().expect("cursor handle drained")
    }
}

impl DerefMut for QueryCursorHandle {
    fn deref_mut(&mut self) -> &mut QueryCursor {
        self.0.as_mut().expect("cursor handle drained")
    }
}

impl Drop for QueryCursorHandle {
    fn drop(&mut self) {
        if let Some(cursor) = self.0.take() {
            QUERY_CURSORS
                .lock()
                .expect("query cursor pool poisoned")
                .push(cursor);
        }
    }
}

pub fn parse(language: &Language, text: &str, old_tree: Option<&Tree>) -> Option<Tree> {
    with_parser(|parser| {
        parser.set_language(&language.grammar).ok()?;
        parser.parse(text, old_tree)
    })
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
    parse_rope_inner(language, rope, old_tree, None, None)
}

/// Parse `rope` with a wall-clock deadline. Returns `None` if the parser
/// aborts (deadline exceeded) or fails for any other reason. Used by the
/// sync-parse fast path in [`crate`]'s host editor: small edits typically
/// finish well within a millisecond, large reparses fall through to the
/// background parser pool.
pub fn parse_rope_within(
    language: &Language,
    rope: &Rope,
    old_tree: Option<&Tree>,
    deadline: Instant,
) -> Option<Tree> {
    parse_rope_inner(language, rope, old_tree, None, Some(deadline))
}

/// Parse `rope` restricted to the given byte range via
/// [`tree_sitter::Parser::set_included_ranges`]. Node positions in the
/// returned tree carry rope-absolute byte offsets.
///
/// Used for language injections: the parser only consumes bytes within
/// `range` (e.g. an injected code block) but reports captured nodes in the
/// host document's coordinate space. Avoids the [`String`] allocation that
/// the older flat-string injection path required.
pub fn parse_rope_range(
    language: &Language,
    rope: &Rope,
    range: Range<usize>,
    old_tree: Option<&Tree>,
) -> Option<Tree> {
    if range.start >= range.end || range.end > rope.len() {
        return None;
    }
    let start_point = stoat_to_ts(rope.offset_to_point(range.start));
    let end_point = stoat_to_ts(rope.offset_to_point(range.end));
    let included = [tree_sitter::Range {
        start_byte: range.start,
        end_byte: range.end,
        start_point,
        end_point,
    }];
    parse_rope_inner(language, rope, old_tree, Some(&included), None)
}

fn parse_rope_inner(
    language: &Language,
    rope: &Rope,
    old_tree: Option<&Tree>,
    included_ranges: Option<&[tree_sitter::Range]>,
    deadline: Option<Instant>,
) -> Option<Tree> {
    with_parser(|parser| {
        parser.set_language(&language.grammar).ok()?;
        if let Some(ranges) = included_ranges {
            parser.set_included_ranges(ranges).ok()?;
        }

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

            // If `byte_offset` falls within the current pending chunk, return
            // the suffix starting at the requested offset.
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

        if let Some(deadline) = deadline {
            let timed_out = Cell::new(false);
            let mut progress = |_state: &ParseState| -> ControlFlow<()> {
                if Instant::now() >= deadline {
                    timed_out.set(true);
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            };
            let options = ParseOptions::new().progress_callback(&mut progress);
            let tree = parser.parse_with_options(&mut callback, old_tree, Some(options));
            if timed_out.get() {
                None
            } else {
                tree
            }
        } else {
            parser.parse_with_options(&mut callback, old_tree, None)
        }
    })
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
/// Walks any registered language injections via the language's pre-compiled
/// [`crate::language::Language::injection_query`]: each query match locates
/// a host node, the inner grammar parses that node's byte range, and the
/// resulting highlight spans are merged into the host coordinate space.
///
/// Injection sub-trees are full-reparsed each call. Use
/// [`extract_highlights_rope_with_cache`] to thread an [`InjectionTreeCache`]
/// across reparses for incremental injection parsing.
pub fn extract_highlights_rope(
    language: &Language,
    tree: &Tree,
    rope: &Rope,
) -> Vec<HighlightSpan> {
    extract_highlights_rope_with_cache(language, tree, rope, InjectionTreeCache::default()).spans
}

/// Cache-aware variant of [`extract_highlights_rope`]. Reuses prior
/// injection sub-trees from `prev_cache` whose host range and language
/// match the current parse, and returns a fresh cache containing the
/// trees parsed during this call so the caller can persist them on the
/// per-buffer state.
pub fn extract_highlights_rope_with_cache(
    language: &Language,
    tree: &Tree,
    rope: &Rope,
    mut prev_cache: InjectionTreeCache,
) -> ExtractedHighlights {
    let mut raw: Vec<RawSpan> = Vec::new();
    collect_highlights_into(&mut raw, language, tree, rope);

    let mut new_cache = InjectionTreeCache::default();

    if let Some(injection_query) = language.injection_query.as_ref() {
        let mut cursor = QueryCursorHandle::new();
        let provider = RopeTextProvider { rope };
        let mut matches = cursor.matches(injection_query, tree.root_node(), provider);
        while let Some(m) = matches.next() {
            let pattern_index = m.pattern_index;
            let Some(injection) = language.injections.get(pattern_index) else {
                continue;
            };
            for capture in m.captures {
                let inner_start = capture.node.start_byte();
                let inner_end = capture.node.end_byte();
                if inner_end <= inner_start {
                    continue;
                }
                let host_range = inner_start..inner_end;
                let lang_name = injection.inner.name;
                let prev_tree = prev_cache.take(&host_range, lang_name);
                // Parse the host node's bytes via included_ranges so the
                // inner tree's nodes already carry rope-absolute byte
                // offsets. No flat-string allocation, no offset translation.
                let Some(inner_tree) = parse_rope_range(
                    &injection.inner,
                    rope,
                    host_range.clone(),
                    prev_tree.as_ref(),
                ) else {
                    continue;
                };
                collect_highlights_into(&mut raw, &injection.inner, &inner_tree, rope);
                new_cache.push(host_range, lang_name, inner_tree);
            }
        }
    }

    raw.sort_by(|a, b| {
        a.byte_range
            .start
            .cmp(&b.byte_range.start)
            .then(a.pattern.cmp(&b.pattern))
    });
    let spans = raw.into_iter().map(RawSpan::into_highlight_span).collect();
    ExtractedHighlights {
        spans,
        injection_trees: new_cache,
    }
}

/// Working tuple used while extracting highlights. Carries the
/// theme-resolved [`crate::HighlightId`] plus the original tree-sitter
/// capture index so consumers can resolve the capture name later.
struct RawSpan {
    byte_range: Range<usize>,
    pattern: usize,
    id: crate::highlight_map::HighlightId,
    capture_index: u32,
}

impl RawSpan {
    fn into_highlight_span(self) -> HighlightSpan {
        HighlightSpan {
            byte_range: self.byte_range,
            id: self.id,
            capture_index: self.capture_index,
        }
    }
}

fn collect_highlights_into(raw: &mut Vec<RawSpan>, language: &Language, tree: &Tree, rope: &Rope) {
    let highlight_map = language.highlight_map();
    let mut cursor = QueryCursorHandle::new();
    let provider = RopeTextProvider { rope };
    let mut matches = cursor.matches(&language.highlight_query, tree.root_node(), provider);
    while let Some(m) = matches.next() {
        let pattern = m.pattern_index;
        for capture in m.captures {
            let start = capture.node.start_byte();
            let end = capture.node.end_byte();
            if start == end {
                continue;
            }
            raw.push(RawSpan {
                byte_range: start..end,
                pattern,
                id: highlight_map.get(capture.index),
                capture_index: capture.index,
            });
        }
    }
}

/// `TextProvider` over a stoat [`Rope`]. For each query node, returns an
/// iterator of borrowed `&[u8]` slices spanning the node's byte range.
pub(crate) struct RopeTextProvider<'a> {
    pub rope: &'a Rope,
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

pub(crate) struct RopeNodeBytes<'a> {
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
    let highlight_map = language.highlight_map();
    let mut raw: Vec<RawSpan> = Vec::new();
    let mut cursor = QueryCursorHandle::new();
    let mut matches = cursor.matches(&language.highlight_query, tree.root_node(), text.as_bytes());

    while let Some(m) = matches.next() {
        let pattern = m.pattern_index;
        for capture in m.captures {
            let start = capture.node.start_byte();
            let end = capture.node.end_byte();
            if start == end {
                continue;
            }
            raw.push(RawSpan {
                byte_range: start..end,
                pattern,
                id: highlight_map.get(capture.index),
                capture_index: capture.index,
            });
        }
    }

    raw.sort_by(|a, b| {
        a.byte_range
            .start
            .cmp(&b.byte_range.start)
            .then(a.pattern.cmp(&b.pattern))
    });

    raw.into_iter().map(RawSpan::into_highlight_span).collect()
}

#[cfg(test)]
mod tests {
    use super::{extract_highlights, parse, HighlightSpan};
    use crate::language::LanguageRegistry;

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

    /// Lookup the capture name for a span via the language's
    /// `highlight_capture_names()` table. Used by tests that want to
    /// assert "this fragment was tagged with capture X" without
    /// depending on a particular theme being installed.
    fn capture_name<'a>(lang: &'a crate::Language, span: &HighlightSpan) -> Option<&'a str> {
        lang.highlight_capture_names()
            .get(span.capture_index as usize)
            .copied()
    }

    /// True if `name` matches `target` either exactly or as the first
    /// dot-separated component (so `"keyword"` matches `"keyword"` and
    /// `"keyword.control"`).
    fn capture_name_matches(name: &str, target: &str) -> bool {
        name == target || name.starts_with(&format!("{target}."))
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
        let name = capture_name(&lang, span).expect("capture name");
        assert!(
            capture_name_matches(name, "keyword"),
            "fn should be a keyword capture, got {name}"
        );
    }

    #[test]
    fn rust_string_captured() {
        let text = r#"fn main() { let _ = "hi"; }"#;
        let lang = rust();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let span = span_for_text(&spans, text, "\"hi\"").expect("string span");
        let name = capture_name(&lang, span).expect("capture name");
        assert!(
            capture_name_matches(name, "string"),
            "string literal should be a string capture, got {name}"
        );
    }

    #[test]
    fn rust_function_definition_captured() {
        let text = "fn main() {}";
        let lang = rust();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let span = span_for_text(&spans, text, "main").expect("main span");
        let name = capture_name(&lang, span).expect("capture name");
        assert!(
            capture_name_matches(name, "function"),
            "main should be a function capture, got {name}"
        );
    }

    #[test]
    fn json_property_and_string_captured() {
        let text = r#"{"a":"b"}"#;
        let lang = json();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        let key = span_for_text(&spans, text, "\"a\"").expect("key span");
        let value = span_for_text(&spans, text, "\"b\"").expect("value span");
        let key_name = capture_name(&lang, key).expect("key capture name");
        let value_name = capture_name(&lang, value).expect("value capture name");
        assert!(
            capture_name_matches(key_name, "property")
                || key_name.contains("property")
                || key_name.contains("variable.other.member"),
            "json key should be a property capture, got {key_name}"
        );
        assert!(
            capture_name_matches(value_name, "string"),
            "json value should be a string capture, got {value_name}"
        );
    }

    #[test]
    fn markdown_title_captured() {
        let text = "# Title\n";
        let lang = markdown();
        let tree = parse(&lang, text, None).unwrap();
        let spans = extract_highlights(&lang, &tree, text);
        assert!(
            spans.iter().any(|s| {
                capture_name(&lang, s)
                    .map(|n| n.contains("title"))
                    .unwrap_or(false)
            }),
            "expected at least one title capture, got {spans:?}"
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
        // nodes, so `**bold**` produces an emphasis.strong capture when
        // extracted via the rope path.
        use stoat_text::Rope;
        let lang = markdown();
        let text = "**bold** and *italic*\n";
        let rope = Rope::from(text);
        let tree = super::parse_rope(&lang, &rope, None).unwrap();
        let spans = super::extract_highlights_rope(&lang, &tree, &rope);

        let bold_start = text.find("**bold**").unwrap();
        let bold_end = bold_start + "**bold**".len();
        // The injection layer is markdown-inline; resolve capture
        // names against that language. We accept both "emphasis.strong"
        // and the dotted variant the markdown-inline grammar uses.
        let inline_lang = LanguageRegistry::standard()
            .languages()
            .iter()
            .find(|l| l.name == "markdown-inline")
            .unwrap()
            .clone();
        assert!(
            spans.iter().any(|s| {
                let in_bold = s.byte_range.start >= bold_start && s.byte_range.end <= bold_end;
                let name = inline_lang
                    .highlight_capture_names()
                    .get(s.capture_index as usize)
                    .copied()
                    .unwrap_or("");
                in_bold && name.contains("emphasis")
            }),
            "expected an emphasis capture inside `**bold**`, got {spans:?}",
        );
    }

    #[test]
    fn extracted_spans_carry_highlight_id_after_theme_install() {
        // With no theme installed, every span's id is DEFAULT.
        // After installing a HighlightMap built from the language's
        // capture names against a sample theme, spans whose capture
        // names match a theme key should carry a non-default id.
        use crate::highlight_map::{HighlightId, HighlightMap};
        let lang = rust();
        let text = "fn main() {}";
        let tree = parse(&lang, text, None).unwrap();

        // Default state: every id is DEFAULT.
        let spans_default = extract_highlights(&lang, &tree, text);
        assert!(
            spans_default.iter().all(|s| s.id == HighlightId::DEFAULT),
            "uninstalled HighlightMap should leave every span at DEFAULT"
        );

        // Install a small theme and re-extract.
        let theme_keys = ["function", "keyword"];
        let map = HighlightMap::new(lang.highlight_capture_names(), &theme_keys);
        lang.set_highlight_map(map);

        let spans_themed = extract_highlights(&lang, &tree, text);
        // The keyword 'fn' must now resolve to the "keyword" theme entry
        // (id 1), and the function name 'main' must resolve to the
        // "function" theme entry (id 0).
        let fn_span = span_for_text(&spans_themed, text, "fn").expect("fn span");
        assert_eq!(
            fn_span.id,
            HighlightId(1),
            "fn keyword should resolve to theme key 'keyword'"
        );
        let main_span = span_for_text(&spans_themed, text, "main").expect("main span");
        assert_eq!(
            main_span.id,
            HighlightId(0),
            "main function name should resolve to theme key 'function'"
        );

        // Reset for other tests that share the registry.
        lang.set_highlight_map(HighlightMap::default());
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
