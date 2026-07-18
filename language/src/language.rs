use crate::{grammar, highlight_map::HighlightMap};
use std::{
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};
use tree_sitter::{Language as TsLanguage, Query};

pub struct Language {
    pub name: &'static str,
    pub extensions: &'static [&'static str],
    pub grammar: TsLanguage,
    pub highlight_query: Query,
    /// Theme-resolved capture index -> [`HighlightId`] table. Mutable
    /// so the host can rebuild it when the active theme changes via
    /// [`Language::set_highlight_map`]. Defaults to an empty map
    /// (every lookup returns [`HighlightId::DEFAULT`]) until the host
    /// installs one. The host's `parse_buffer_step` calls
    /// `id_for_highlight(span.id)` to look up the rendered style;
    /// spans whose id is `DEFAULT` are rendered without a foreground.
    pub highlight_map: Mutex<HighlightMap>,
    /// Inner languages parsed inside specific node kinds of this grammar.
    /// Markdown injects the markdown-inline grammar inside `inline` nodes and
    /// resolves fenced code blocks to their info-string language. Rust injects
    /// markdown inside `doc_comment` nodes.
    pub injections: Vec<LanguageInjection>,
    /// Compiled query that captures injection host nodes by kind. Built
    /// from [`Language::injections`] when the language is constructed; the
    /// capture names match the host node kinds. `None` when there are no
    /// injections configured.
    pub injection_query: Option<Query>,
    /// Bracket-pair query loaded from `brackets.scm`. Captures `@open` and
    /// `@close` for matched bracket-like tokens, driving the editor's
    /// match-brackets motion through [`crate::matching_bracket`]. `None` for
    /// grammars that ship no `brackets.scm`.
    pub bracket_query: Option<Query>,
    /// Indent query loaded from `indents.scm`. Captures `@indent` and
    /// `@end` markers for grammar-driven auto-indentation. Loaded but
    /// not yet wired.
    pub indent_query: Option<Query>,
    /// Textobjects query loaded from `textobjects.scm`. Captures
    /// `@function.around` / `@function.inside`, `@class.around` /
    /// `@class.inside`, `@parameter.around` / `@parameter.inside`,
    /// `@comment.around` / `@comment.inside`, plus auxiliaries
    /// (`@entry`, `@test`) used by `select_textobject_around` and
    /// `select_textobject_inner`. `None` for languages without
    /// structural textobjects (json, markdown).
    pub textobjects_query: Option<Query>,
    /// Outline query loaded from `outline.scm`. Captures `@item` (a
    /// definition's full range), `@name` (its identifier), `@context`
    /// (keyword and modifier tokens), and `@annotation` (attributes,
    /// doc comments) for the symbols a file defines. Loaded but not yet
    /// wired into a consumer. Reserved for symbol extraction. `None` for
    /// languages without an `outline.scm` (e.g. toml).
    pub outline_query: Option<Query>,
    /// Tags query loaded from `tags.scm`. Captures `@reference.call`
    /// at each call site (free functions, method calls, and macro
    /// invocations) for building a call graph. Loaded but not yet
    /// wired into a consumer. Reserved for reference extraction.
    /// `None` for languages without a `tags.scm` (only rust ships one).
    pub tags_query: Option<Query>,
    /// Line-comment marker for languages that have one (e.g. `"//"`
    /// for rust, `"#"` for toml). `None` for languages without line
    /// comments (e.g. JSON, markdown). Used by the `ToggleComments`
    /// action to insert / remove the prefix on each line.
    pub line_comment: Option<&'static str>,
    /// Languages a fenced code block's info string may resolve to, for a
    /// grammar carrying an [`InjectionInner::Fence`] injection. Late-bound: the
    /// registry fills it after every language exists, since a fence host (e.g.
    /// markdown) may need to resolve to a language that already holds it (e.g.
    /// rust, which injects markdown into doc comments). Empty for grammars with
    /// no fence injection.
    pub fence_candidates: OnceLock<Vec<Arc<Language>>>,
}

impl Language {
    /// Capture names from the highlight query, in capture-index order.
    /// Used by callers that want to build a [`HighlightMap`] against a
    /// host theme without having to crack open `highlight_query`.
    pub fn highlight_capture_names(&self) -> &[&str] {
        self.highlight_query.capture_names()
    }

    /// Replace the cached theme-resolved [`HighlightMap`]. Call this
    /// from the host when the active theme changes. Cheap (just an
    /// `Arc` swap inside a `Mutex`); does not force a reparse.
    pub fn set_highlight_map(&self, map: HighlightMap) {
        *self.highlight_map.lock().expect("highlight map poisoned") = map;
    }

    /// Snapshot the current [`HighlightMap`].
    pub fn highlight_map(&self) -> HighlightMap {
        self.highlight_map
            .lock()
            .expect("highlight map poisoned")
            .clone()
    }
}

/// How a [`LanguageInjection`] resolves the language to parse a host range as.
pub enum InjectionInner {
    /// Parse each host node's byte range as this fixed language.
    Fixed(Arc<Language>),
    /// Parse each fenced code block's content as the language its info string
    /// names, resolved against the host language's `fence_candidates`. The
    /// injection's `host_node_kind` is ignored, since the query matches fenced
    /// blocks directly.
    Fence,
}

/// Pairs an inner-language rule with the host node kind it applies to.
///
/// The host parser produces a tree. For each node whose kind matches
/// `host_node_kind` the inner parser runs over its byte range. A
/// [`InjectionInner::Fence`] rule instead matches fenced code blocks and
/// resolves each fence's language from its info string.
pub struct LanguageInjection {
    pub host_node_kind: &'static str,
    pub inner: InjectionInner,
}

pub struct LanguageRegistry {
    languages: Vec<Arc<Language>>,
}

impl LanguageRegistry {
    pub fn standard() -> Self {
        // Build markdown-inline first so we can wire it as an injection inside
        // the host markdown grammar. The host parses block structure and emits
        // `inline` nodes; the inline grammar parses each of those for emphasis,
        // links, code spans, etc.
        let markdown_inline = Arc::new(make_markdown_inline());
        let markdown = Arc::new(make_markdown_with_injections(vec![LanguageInjection {
            host_node_kind: "inline",
            inner: InjectionInner::Fixed(markdown_inline.clone()),
        }]));
        let registry = Self {
            languages: vec![
                Arc::new(make_rust(markdown.clone())),
                Arc::new(make_json()),
                Arc::new(make_toml()),
                markdown,
                markdown_inline,
            ],
        };
        // A fence injection resolves its info-string token against every
        // registered language. Late-bind the candidate set now that all exist.
        // Build time is too early, since rust already holds markdown for doc
        // comments, so markdown cannot hold rust back then.
        for lang in &registry.languages {
            if lang
                .injections
                .iter()
                .any(|i| matches!(i.inner, InjectionInner::Fence))
            {
                let _ = lang.fence_candidates.set(registry.languages.clone());
            }
        }
        registry
    }

    pub fn for_path(&self, path: &Path) -> Option<Arc<Language>> {
        let ext = path.extension()?.to_str()?;
        self.languages
            .iter()
            .find(|l| l.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
            .cloned()
    }

    pub fn languages(&self) -> &[Arc<Language>] {
        &self.languages
    }

    /// Resolve a fenced code block's info-string `token` to a registered
    /// language, matching its name or one extension case-insensitively.
    pub fn language_for_fence_token(&self, token: &str) -> Option<Arc<Language>> {
        language_for_fence_token(token, &self.languages)
    }
}

/// Resolve a fenced code block's info-string `token` to a language in
/// `languages`, matching its name or one extension case-insensitively.
///
/// Returns [`None`] for an empty or whitespace-only token, or one that names no
/// registered language. Shared by the hover renderer and the buffer fence
/// injection so both resolve fences identically.
pub fn language_for_fence_token(token: &str, languages: &[Arc<Language>]) -> Option<Arc<Language>> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    languages
        .iter()
        .find(|lang| {
            lang.name.eq_ignore_ascii_case(token)
                || lang
                    .extensions
                    .iter()
                    .any(|ext| ext.eq_ignore_ascii_case(token))
        })
        .cloned()
}

/// Optional auxiliary query sources bundled alongside `highlights.scm`.
/// Each is loaded best-effort: a query that fails to compile against the
/// grammar (e.g. references a node kind the grammar version does not
/// expose) is silently set to `None` so the build never breaks. Required
/// queries (e.g. `highlights.scm`) still panic on compile failure.
#[derive(Default)]
struct AuxQuerySources {
    brackets: Option<&'static str>,
    indents: Option<&'static str>,
    textobjects: Option<&'static str>,
    outline: Option<&'static str>,
    tags: Option<&'static str>,
    line_comment: Option<&'static str>,
}

fn make_language(
    name: &'static str,
    extensions: &'static [&'static str],
    grammar: TsLanguage,
    highlight_src: &str,
    aux: AuxQuerySources,
) -> Language {
    make_language_with_injections(name, extensions, grammar, highlight_src, Vec::new(), aux)
}

fn make_language_with_injections(
    name: &'static str,
    extensions: &'static [&'static str],
    grammar: TsLanguage,
    highlight_src: &str,
    injections: Vec<LanguageInjection>,
    aux: AuxQuerySources,
) -> Language {
    let AuxQuerySources {
        brackets,
        indents,
        textobjects,
        outline,
        tags,
        line_comment,
    } = aux;

    // Each Query::new is tens of milliseconds against the rust grammar, and the
    // seven are independent, so compile them concurrently to shrink the
    // pre-first-frame path. The grammar and injections are shared immutably and
    // the scoped threads join before either is moved into the returned Language.
    let (
        highlight_query,
        injection_query,
        bracket_query,
        indent_query,
        textobjects_query,
        outline_query,
        tags_query,
    ) = std::thread::scope(|s| {
        let injection_handle = s.spawn(|| build_injection_query(name, &grammar, &injections));
        let bracket_handle =
            s.spawn(|| brackets.and_then(|src| try_compile_query(name, "brackets", &grammar, src)));
        let indent_handle =
            s.spawn(|| indents.and_then(|src| try_compile_query(name, "indents", &grammar, src)));
        let textobjects_handle = s.spawn(|| {
            textobjects.and_then(|src| try_compile_query(name, "textobjects", &grammar, src))
        });
        let outline_handle =
            s.spawn(|| outline.and_then(|src| try_compile_query(name, "outline", &grammar, src)));
        let tags_handle =
            s.spawn(|| tags.and_then(|src| try_compile_query(name, "tags", &grammar, src)));

        let highlight_query = Query::new(&grammar, highlight_src)
            .unwrap_or_else(|e| panic!("highlight query for {name} failed to compile: {e}"));

        (
            highlight_query,
            injection_handle
                .join()
                .expect("injection query thread panicked"),
            bracket_handle
                .join()
                .expect("brackets query thread panicked"),
            indent_handle.join().expect("indents query thread panicked"),
            textobjects_handle
                .join()
                .expect("textobjects query thread panicked"),
            outline_handle
                .join()
                .expect("outline query thread panicked"),
            tags_handle.join().expect("tags query thread panicked"),
        )
    });

    Language {
        name,
        extensions,
        grammar,
        highlight_query,
        highlight_map: Mutex::new(HighlightMap::default()),
        injections,
        injection_query,
        bracket_query,
        indent_query,
        textobjects_query,
        outline_query,
        tags_query,
        line_comment,
        fence_candidates: OnceLock::new(),
    }
}

fn try_compile_query(
    _lang_name: &'static str,
    _query_kind: &'static str,
    grammar: &TsLanguage,
    src: &str,
) -> Option<Query> {
    // Silent best-effort compilation: optional aux queries that don't match
    // the bundled grammar version are dropped instead of breaking the build.
    // Consumers must tolerate `None`.
    Query::new(grammar, src).ok()
}

/// Build a tree-sitter [`Query`] that captures every host node configured in
/// `injections`. Each injection's `host_node_kind` becomes one query pattern
/// of the form `((<kind>) @injection)`. The capture index per pattern is
/// the same as the index in [`Language::injections`], so the highlight code
/// can map a capture back to its injection by [`tree_sitter::QueryMatch::pattern_index`].
fn build_injection_query(
    name: &'static str,
    grammar: &TsLanguage,
    injections: &[LanguageInjection],
) -> Option<Query> {
    if injections.is_empty() {
        return None;
    }
    let mut source = String::new();
    for injection in injections {
        match &injection.inner {
            InjectionInner::Fixed(_) => {
                source.push_str("((");
                source.push_str(injection.host_node_kind);
                source.push_str(") @injection)\n");
            },
            InjectionInner::Fence => {
                source.push_str(
                    "(fenced_code_block (info_string (language) @injection.language) \
                     (code_fence_content) @injection.content)\n",
                );
            },
        }
    }
    let query = Query::new(grammar, &source)
        .unwrap_or_else(|e| panic!("injection query for {name} failed to compile: {e}"));
    Some(query)
}

fn make_rust(markdown: Arc<Language>) -> Language {
    make_language_with_injections(
        "rust",
        &["rs"],
        grammar::rust(),
        include_str!("../../vendor/zed/crates/languages/src/rust/highlights.scm"),
        // Doc comments host a combined markdown injection, so `/// **bold**`
        // renders as styled markdown. The `doc_comment` node covers the text
        // after the `///` marker. The marker keeps its rust comment style.
        vec![LanguageInjection {
            host_node_kind: "doc_comment",
            inner: InjectionInner::Fixed(markdown),
        }],
        AuxQuerySources {
            brackets: Some(include_str!(
                "../../vendor/zed/crates/languages/src/rust/brackets.scm"
            )),
            indents: Some(include_str!(
                "../../vendor/zed/crates/languages/src/rust/indents.scm"
            )),
            textobjects: Some(include_str!(
                "../../vendor/helix/runtime/queries/rust/textobjects.scm"
            )),
            outline: Some(include_str!(
                "../../vendor/zed/crates/languages/src/rust/outline.scm"
            )),
            tags: Some(include_str!("queries/rust/tags.scm")),
            line_comment: Some("//"),
        },
    )
}

fn make_json() -> Language {
    make_language(
        "json",
        &["json"],
        grammar::json(),
        include_str!("../../vendor/zed/crates/languages/src/json/highlights.scm"),
        AuxQuerySources {
            brackets: Some(include_str!(
                "../../vendor/zed/crates/languages/src/json/brackets.scm"
            )),
            indents: Some(include_str!(
                "../../vendor/zed/crates/languages/src/json/indents.scm"
            )),
            textobjects: None,
            outline: Some(include_str!(
                "../../vendor/zed/crates/languages/src/json/outline.scm"
            )),
            tags: None,
            line_comment: None,
        },
    )
}

fn make_toml() -> Language {
    make_language(
        "toml",
        &["toml"],
        grammar::toml(),
        include_str!("../../vendor/helix/runtime/queries/toml/highlights.scm"),
        AuxQuerySources {
            textobjects: Some(include_str!(
                "../../vendor/helix/runtime/queries/toml/textobjects.scm"
            )),
            line_comment: Some("#"),
            ..Default::default()
        },
    )
}

fn make_markdown_with_injections(mut injections: Vec<LanguageInjection>) -> Language {
    // Fenced code blocks parse as the language their info string names.
    injections.push(LanguageInjection {
        host_node_kind: "fenced_code_block",
        inner: InjectionInner::Fence,
    });
    make_language_with_injections(
        "markdown",
        &["md", "markdown"],
        grammar::markdown(),
        include_str!("../../vendor/zed/crates/languages/src/markdown/highlights.scm"),
        injections,
        AuxQuerySources {
            brackets: Some(include_str!(
                "../../vendor/zed/crates/languages/src/markdown/brackets.scm"
            )),
            indents: Some(include_str!(
                "../../vendor/zed/crates/languages/src/markdown/indents.scm"
            )),
            textobjects: None,
            outline: Some(include_str!(
                "../../vendor/zed/crates/languages/src/markdown/outline.scm"
            )),
            tags: None,
            line_comment: None,
        },
    )
}

fn make_markdown_inline() -> Language {
    // Registered without file extensions: this grammar only runs as an
    // injected layer inside markdown `inline` nodes. It must still be
    // reachable by name for injection lookup.
    make_language(
        "markdown-inline",
        &[],
        grammar::markdown_inline(),
        include_str!("../../vendor/zed/crates/languages/src/markdown-inline/highlights.scm"),
        AuxQuerySources::default(),
    )
}

#[cfg(test)]
mod tests {
    use super::LanguageRegistry;
    use std::path::Path;

    #[test]
    fn for_path_resolves_extensions() {
        let reg = LanguageRegistry::standard();
        assert_eq!(reg.for_path(Path::new("a.rs")).unwrap().name, "rust");
        assert_eq!(reg.for_path(Path::new("a.json")).unwrap().name, "json");
        assert_eq!(reg.for_path(Path::new("a.toml")).unwrap().name, "toml");
        assert_eq!(reg.for_path(Path::new("a.md")).unwrap().name, "markdown");
        assert_eq!(
            reg.for_path(Path::new("a.markdown")).unwrap().name,
            "markdown"
        );
        assert_eq!(reg.for_path(Path::new("a.RS")).unwrap().name, "rust");
        assert!(reg.for_path(Path::new("a.txt")).is_none());
        assert!(reg.for_path(Path::new("noext")).is_none());
    }

    #[test]
    fn language_for_fence_token_matches_name_or_extension() {
        let reg = LanguageRegistry::standard();
        let name = |t: &str| reg.language_for_fence_token(t).map(|l| l.name);

        // Name match, case-insensitive.
        assert_eq!(name("rust"), Some("rust"));
        assert_eq!(name("RUST"), Some("rust"));
        assert_eq!(name("Json"), Some("json"));

        // Extension match, case-insensitive.
        assert_eq!(name("rs"), Some("rust"));
        assert_eq!(name("RS"), Some("rust"));
        assert_eq!(name("md"), Some("markdown"));

        // Surrounding whitespace is trimmed off the token.
        assert_eq!(name("  rust  "), Some("rust"));

        // Empty, whitespace-only, and unknown tokens resolve to nothing.
        assert_eq!(name(""), None);
        assert_eq!(name("   "), None);
        assert_eq!(name("cobol"), None);
    }

    #[test]
    fn standard_compiles_all_queries() {
        // Constructor unwraps query compile errors; this test triggers
        // those panics in CI to catch query/runtime mismatches early.
        let _reg = LanguageRegistry::standard();
    }

    #[test]
    fn standard_registers_expected_languages() {
        let reg = LanguageRegistry::standard();
        let names: Vec<&str> = reg.languages().iter().map(|l| l.name).collect();
        assert_eq!(
            names,
            vec!["rust", "json", "toml", "markdown", "markdown-inline"],
        );
    }

    #[test]
    fn markdown_inline_has_no_path_extension() {
        // markdown-inline runs as an injected layer, never as a host file.
        // `for_path` must not resolve to it for any extension.
        let reg = LanguageRegistry::standard();
        assert!(reg.for_path(Path::new("a.inline")).is_none());
    }

    #[test]
    fn highlight_capture_names_populated() {
        let reg = LanguageRegistry::standard();
        for lang in reg.languages() {
            assert!(
                !lang.highlight_capture_names().is_empty(),
                "{} highlight query has no captures",
                lang.name
            );
        }
    }

    #[test]
    fn highlight_map_resolves_against_theme_keys() {
        use crate::highlight_map::{HighlightId, HighlightMap};
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();

        // Sample theme: a few common syntax categories.
        let theme_keys = ["string", "keyword", "function", "comment", "type"];
        let map = HighlightMap::new(rust.highlight_capture_names(), &theme_keys);

        // The map must have the same length as the capture name table.
        assert_eq!(map.len(), rust.highlight_capture_names().len());

        // At least one capture should resolve against each theme key
        // (rust's highlights.scm uses these standard categories).
        let resolved: Vec<HighlightId> = (0..map.len() as u32).map(|i| map.get(i)).collect();
        for (theme_idx, theme_key) in theme_keys.iter().enumerate() {
            assert!(
                resolved.contains(&HighlightId(theme_idx as u32)),
                "no rust capture resolves to theme key {theme_key:?}",
            );
        }
    }

    #[test]
    fn highlight_map_install_and_read_back() {
        use crate::highlight_map::HighlightMap;
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();

        // Initially the cached map is empty (default).
        assert!(rust.highlight_map().is_empty());

        // Install a real one and read it back through the snapshot.
        let theme_keys = ["string", "keyword"];
        let map = HighlightMap::new(rust.highlight_capture_names(), &theme_keys);
        let expected_len = map.len();
        rust.set_highlight_map(map);
        assert_eq!(rust.highlight_map().len(), expected_len);
    }

    #[test]
    fn aux_queries_loaded_for_rust_and_json() {
        // Best-effort load: rust and json bundle both brackets.scm and
        // indents.scm. Markdown also bundles them. Confirm at least one
        // language exposes each so the loader is wired correctly.
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();
        assert!(
            rust.bracket_query.is_some(),
            "rust brackets.scm must compile against the bundled grammar"
        );
        assert!(
            rust.indent_query.is_some(),
            "rust indents.scm must compile against the bundled grammar"
        );
        let json = reg.languages().iter().find(|l| l.name == "json").unwrap();
        assert!(
            json.bracket_query.is_some(),
            "json brackets.scm must compile"
        );
    }

    #[test]
    fn textobjects_query_loaded_for_rust_and_toml() {
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();
        assert!(
            rust.textobjects_query.is_some(),
            "rust textobjects.scm must compile against the bundled grammar"
        );
        let toml = reg.languages().iter().find(|l| l.name == "toml").unwrap();
        assert!(
            toml.textobjects_query.is_some(),
            "toml textobjects.scm must compile against the bundled grammar"
        );
        let json = reg.languages().iter().find(|l| l.name == "json").unwrap();
        assert!(
            json.textobjects_query.is_none(),
            "json has no textobjects.scm; query should be None"
        );
    }

    #[test]
    fn outline_query_loaded_for_rust_json_markdown() {
        let reg = LanguageRegistry::standard();
        for name in ["rust", "json", "markdown"] {
            let lang = reg.languages().iter().find(|l| l.name == name).unwrap();
            assert!(
                lang.outline_query.is_some(),
                "{name} outline.scm must compile against the bundled grammar"
            );
        }

        let json = reg.languages().iter().find(|l| l.name == "json").unwrap();
        assert!(
            json.outline_query
                .as_ref()
                .unwrap()
                .capture_index_for_name("name")
                .is_some(),
            "json outline.scm must expose a @name capture"
        );
    }

    #[test]
    fn tags_query_loaded_for_rust() {
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();
        assert!(
            rust.tags_query
                .as_ref()
                .expect("rust tags.scm must compile against the bundled grammar")
                .capture_index_for_name("reference.call")
                .is_some(),
            "rust tags.scm must expose a @reference.call capture"
        );

        let json = reg.languages().iter().find(|l| l.name == "json").unwrap();
        assert!(
            json.tags_query.is_none(),
            "json has no tags.scm; query should be None"
        );
    }
}
