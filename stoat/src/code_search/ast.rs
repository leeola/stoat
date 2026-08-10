use super::{line_snippet, line_start_at, offset_to_line_column, SearchMatch};
use ast_grep_core::{
    matcher::{PatternBuilder, PatternError},
    tree_sitter::{LanguageExt, StrDoc, TSLanguage},
    AstGrep, Language, Pattern,
};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Parsed files the AST search keeps across queries.
///
/// Refining a pattern re-walks the workspace, and every file of the target
/// language would otherwise be handed to tree-sitter again for a query that
/// differs from the last by one character. Keeping the parse means a refinement
/// pays only for matching.
///
/// Keyed by path and a hash of the text, so a file edited between queries misses
/// and is re-parsed rather than matched against what it used to say. Bounded at
/// [`PARSE_CACHE_CAP`] entries by least-recently-used, since the whole point is
/// to hold a workspace's worth of parses without holding a workspace's worth of
/// memory forever.
pub(crate) struct AstParseCache {
    entries: HashMap<(PathBuf, [u8; 32]), Entry>,
    counter: u64,
    cap: usize,
    hits: u64,
}

struct Entry {
    parsed: AstGrep<StrDoc<AstLang>>,
    last_used: u64,
}

/// Entries [`AstParseCache`] holds before evicting.
///
/// A workspace's target-language files usually fit, and a scan that overruns it
/// degrades to re-parsing rather than to anything worse.
pub(crate) const PARSE_CACHE_CAP: usize = 256;

impl AstParseCache {
    pub(crate) fn new(cap: usize) -> Self {
        assert!(cap > 0, "AstParseCache capacity must be positive");
        Self {
            entries: HashMap::new(),
            counter: 0,
            cap,
            hits: 0,
        }
    }

    /// How many scans have reused a parse, for a test that wants to see the
    /// cache working rather than infer it from timing.
    #[cfg(test)]
    fn hits(&self) -> u64 {
        self.hits
    }

    /// The parse of `text` at `path`, parsing it with `lang` on a miss.
    ///
    /// Borrows for as long as the caller reads the tree, which is what keeps the
    /// parse out of an [`Arc`]. A [`tree_sitter::Tree`] is `Send` but not
    /// `Sync`, so sharing one would put the whole cache out of reach of the
    /// blocking scan task.
    fn parsed(&mut self, path: &Path, text: &str, lang: &AstLang) -> &AstGrep<StrDoc<AstLang>> {
        let key = (path.to_path_buf(), blake3::hash(text.as_bytes()).into());
        let counter = {
            self.counter = self.counter.wrapping_add(1);
            self.counter
        };

        if self.entries.contains_key(&key) {
            self.hits += 1;
        } else {
            self.entries.insert(
                key.clone(),
                Entry {
                    parsed: lang.ast_grep(text),
                    last_used: counter,
                },
            );
            self.evict_if_over_cap(&key);
        }

        let entry = self
            .entries
            .get_mut(&key)
            .expect("the entry was just inserted or found");
        entry.last_used = counter;
        &entry.parsed
    }

    /// Drop the least recently used entry once an insert takes the map past its
    /// cap, never `keep` itself.
    fn evict_if_over_cap(&mut self, keep: &(PathBuf, [u8; 32])) {
        if self.entries.len() <= self.cap {
            return;
        }
        let victim = self
            .entries
            .iter()
            .filter(|(k, _)| *k != keep)
            .min_by_key(|(_, e)| e.last_used)
            .map(|(k, _)| k.clone());
        if let Some(k) = victim {
            self.entries.remove(&k);
        }
    }
}

/// ast-grep [`Language`] adapter over a stoat grammar.
///
/// Lets the code-search AST mode compile ast-grep `$VAR`/`$$$` patterns and match
/// them against a buffer's language. Wraps the grammar in an [`Arc`] so cloning
/// the adapter (which ast-grep does per pattern build and parse) is cheap.
#[derive(Clone)]
pub(crate) struct AstLang(Arc<stoat_language::Language>);

impl AstLang {
    pub(crate) fn new(language: Arc<stoat_language::Language>) -> Self {
        Self(language)
    }
}

impl Language for AstLang {
    fn kind_to_id(&self, kind: &str) -> u16 {
        let named = self.0.grammar.id_for_node_kind(kind, true);
        if named != 0 {
            named
        } else {
            self.0.grammar.id_for_node_kind(kind, false)
        }
    }

    fn field_to_id(&self, field: &str) -> Option<u16> {
        self.0.grammar.field_id_for_name(field).map(|f| f.get())
    }

    fn build_pattern(&self, builder: &PatternBuilder<'_>) -> Result<Pattern, PatternError> {
        builder.build(|src| StrDoc::try_new(src, self.clone()))
    }
}

impl LanguageExt for AstLang {
    fn get_ts_language(&self) -> TSLanguage {
        self.0.grammar.clone()
    }
}

/// Match `pattern` against `text` parsed as `lang`, pushing one [`SearchMatch`]
/// per hit onto `out`, keyed by the matched node's start byte offset.
///
/// `cache` supplies the parse, so a repeated scan of unchanged text costs only
/// the match.
pub(crate) fn ast_scan_file(
    text: &str,
    lang: &AstLang,
    pattern: &Pattern,
    path: &Path,
    cache: &mut AstParseCache,
    out: &mut Vec<SearchMatch>,
) {
    let root = cache.parsed(path, text, lang);

    let path: Arc<Path> = Arc::from(path);

    // Tree traversal is document order, so each match's position is the last
    // one's plus what lies between them. Deriving each from the start of the
    // file would walk the file once per match.
    let mut counted_to = 0usize;
    let mut line = 1u32;
    let mut line_start = 0usize;

    for m in root.root().find_all(pattern) {
        let start = m.range().start;

        if start < counted_to {
            debug_assert!(false, "find_all yielded {start} after {counted_to}");
            let (line, column) = offset_to_line_column(text, start);
            out.push(SearchMatch {
                path: path.clone(),
                offset: start,
                line,
                column,
                snippet: line_snippet(text, line_start_at(text, start), start),
            });
            continue;
        }

        let since = &text[counted_to..start];
        line += since.bytes().filter(|&b| b == b'\n').count() as u32;
        if let Some(last) = since.rfind('\n') {
            line_start = counted_to + last + 1;
        }
        counted_to = start;

        out.push(SearchMatch {
            path: path.clone(),
            offset: start,
            line,
            column: text[line_start..start].chars().count() as u32 + 1,
            snippet: line_snippet(text, line_start, start),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_language::LanguageRegistry;

    fn rust_lang() -> Arc<stoat_language::Language> {
        LanguageRegistry::standard()
            .for_path(Path::new("x.rs"))
            .expect("rust language")
    }

    fn scan(text: &str, lang: &AstLang, pattern: &Pattern, path: &str) -> Vec<SearchMatch> {
        let mut out = Vec::new();
        let mut cache = AstParseCache::new(PARSE_CACHE_CAP);
        ast_scan_file(text, lang, pattern, Path::new(path), &mut cache, &mut out);
        out
    }

    #[test]
    fn scans_a_rust_fn_pattern_across_lines() {
        let lang = AstLang::new(rust_lang());
        let pattern = Pattern::try_new("fn $NAME() {}", lang.clone()).expect("pattern compiles");
        let text = "fn alpha() {}\nfn beta() {}\n";

        let out = scan(text, &lang, &pattern, "/x.rs");

        assert_eq!(
            out.iter().map(|m| m.line).collect::<Vec<_>>(),
            vec![1, 2],
            "both fns match on their own lines"
        );
        assert_eq!(out[0].snippet, "fn alpha() {}");
    }

    /// The scan derives each position from the previous match rather than from
    /// the start of the file, so what it reports has to equal what a
    /// from-scratch walk would say at every single match.
    ///
    /// The text puts a match inside another match, so the carried offset has to
    /// survive two hits that begin at the same place, and puts multi-byte
    /// characters ahead of a later line, so a column counted in bytes rather
    /// than characters would diverge.
    #[test]
    fn carried_positions_agree_with_walking_from_the_start() {
        let lang = AstLang::new(rust_lang());
        let pattern = Pattern::try_new("$A + $B", lang.clone()).expect("pattern compiles");
        let text = "let a = 1 + 2 + 3;\nlet \u{e9}\u{e9}\u{e9} = 4 + 5;\n\nlet c = 6 + 7;\n";

        let out = scan(text, &lang, &pattern, "/x.rs");

        assert!(out.len() >= 4, "the nested sum matches too, got {out:?}");
        for m in &out {
            assert_eq!(
                (m.line, m.column),
                offset_to_line_column(text, m.offset),
                "offset {} disagrees with a walk from byte 0",
                m.offset,
            );
            assert_eq!(
                m.snippet,
                line_snippet(text, line_start_at(text, m.offset), m.offset),
                "offset {} took its snippet from the wrong line start",
                m.offset,
            );
        }
    }

    /// Refining a query re-walks the workspace, so the pass after it has to
    /// reuse the parse of a file that did not change, and re-parse one that did.
    #[test]
    fn a_repeated_scan_reuses_the_parse_until_the_text_changes() {
        let lang = AstLang::new(rust_lang());
        let pattern = Pattern::try_new("fn $NAME() {}", lang.clone()).expect("pattern compiles");
        let path = Path::new("/x.rs");
        let mut cache = AstParseCache::new(PARSE_CACHE_CAP);
        let mut out = Vec::new();

        ast_scan_file(
            "fn alpha() {}\n",
            &lang,
            &pattern,
            path,
            &mut cache,
            &mut out,
        );
        assert_eq!(cache.hits(), 0, "the first scan has nothing to reuse");

        ast_scan_file(
            "fn alpha() {}\n",
            &lang,
            &pattern,
            path,
            &mut cache,
            &mut out,
        );
        assert_eq!(cache.hits(), 1, "the second scan reuses the parse");

        ast_scan_file(
            "fn beta() {}\n",
            &lang,
            &pattern,
            path,
            &mut cache,
            &mut out,
        );
        assert_eq!(cache.hits(), 1, "edited text is parsed again");

        assert_eq!(
            out.iter().map(|m| m.snippet.as_str()).collect::<Vec<_>>(),
            ["fn alpha() {}", "fn alpha() {}", "fn beta() {}"],
            "and every scan still reported its own match",
        );
    }

    /// What the cap drops is the parse longest unused, not whichever entry the
    /// map happened to reach first.
    #[test]
    fn the_cap_evicts_the_least_recently_used_parse() {
        let lang = AstLang::new(rust_lang());
        let pattern = Pattern::try_new("fn $NAME() {}", lang.clone()).expect("pattern compiles");
        let mut cache = AstParseCache::new(2);
        let mut out = Vec::new();

        let mut visit = |cache: &mut AstParseCache, path: &str| {
            ast_scan_file(
                "fn alpha() {}\n",
                &lang,
                &pattern,
                Path::new(path),
                cache,
                &mut out,
            );
        };

        visit(&mut cache, "/a.rs");
        visit(&mut cache, "/b.rs");
        visit(&mut cache, "/a.rs");
        assert_eq!(cache.hits(), 1, "a.rs was still held");

        visit(&mut cache, "/c.rs");
        visit(&mut cache, "/a.rs");
        assert_eq!(
            cache.hits(),
            2,
            "a.rs survived, having been used more recently"
        );

        visit(&mut cache, "/b.rs");
        assert_eq!(cache.hits(), 2, "b.rs is the one c.rs pushed out");
    }

    #[test]
    fn empty_pattern_is_a_compile_error() {
        let lang = AstLang::new(rust_lang());
        assert!(Pattern::try_new("", lang).is_err());
    }
}
