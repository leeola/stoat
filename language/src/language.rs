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
    /// The action-driven queries, compiled the first time one is asked for.
    ///
    /// None of these is read to paint a frame, and each costs tens of
    /// milliseconds to compile, so a session that never runs a bracket motion
    /// never pays for `brackets.scm`.
    aux: AuxQueries,
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

/// The queries no frame needs, held as source until something asks.
///
/// Each is compiled at most once, on first access. A source that does not match
/// the bundled grammar compiles to nothing and stays that way, which is the
/// best-effort contract every reader already tolerates.
#[derive(Default)]
struct AuxQueries {
    brackets: LazyQuery,
    indents: LazyQuery,
    textobjects: LazyQuery,
    outline: LazyQuery,
    tags: LazyQuery,
}

/// One optional query, compiled from its source the first time it is read.
#[derive(Default)]
struct LazyQuery {
    /// `None` for a grammar that ships no such file, which compiles to nothing
    /// without being asked.
    src: Option<&'static str>,
    compiled: OnceLock<Option<Query>>,
}

impl LazyQuery {
    fn new(src: Option<&'static str>) -> LazyQuery {
        LazyQuery {
            src,
            compiled: OnceLock::new(),
        }
    }

    /// The compiled query, compiling it on the first call.
    ///
    /// A source that fails against the bundled grammar yields nothing and is
    /// not retried. These are optional files that may lag a grammar bump, so
    /// dropping one is better than failing the build over it, and every reader
    /// already handles their absence.
    fn get(&self, grammar: &TsLanguage) -> Option<&Query> {
        self.compiled
            .get_or_init(|| self.src.and_then(|src| Query::new(grammar, src).ok()))
            .as_ref()
    }
}

impl Language {
    /// Bracket-pair query loaded from `brackets.scm`. Captures `@open` and
    /// `@close` for matched bracket-like tokens, driving the editor's
    /// match-brackets motion through [`crate::matching_bracket`]. `None` for
    /// grammars that ship no `brackets.scm`.
    ///
    /// Compiled on the first call, which costs tens of milliseconds.
    pub fn bracket_query(&self) -> Option<&Query> {
        self.aux.brackets.get(&self.grammar)
    }

    /// Indent query loaded from `indents.scm`. Captures `@indent` and `@end`
    /// markers for grammar-driven auto-indentation. Compiled on the first call.
    pub fn indent_query(&self) -> Option<&Query> {
        self.aux.indents.get(&self.grammar)
    }

    /// Textobjects query loaded from `textobjects.scm`. Captures
    /// `@function.around` / `@function.inside`, `@class.around` /
    /// `@class.inside`, `@parameter.around` / `@parameter.inside`,
    /// `@comment.around` / `@comment.inside`, plus auxiliaries (`@entry`,
    /// `@test`) used by `select_textobject_around` and
    /// `select_textobject_inner`. `None` for languages without structural
    /// textobjects (json, markdown).
    ///
    /// Compiled on the first call.
    pub fn textobjects_query(&self) -> Option<&Query> {
        self.aux.textobjects.get(&self.grammar)
    }

    /// Outline query loaded from `outline.scm`. Captures `@item` (a
    /// definition's full range), `@name` (its identifier), `@context` (keyword
    /// and modifier tokens), and `@annotation` (attributes, doc comments) for
    /// the symbols a file defines. `None` for languages without an
    /// `outline.scm` (e.g. toml).
    ///
    /// Compiled on the first call.
    pub fn outline_query(&self) -> Option<&Query> {
        self.aux.outline.get(&self.grammar)
    }

    /// Tags query loaded from `tags.scm`. Captures `@reference.call` at each
    /// call site (free functions, method calls, and macro invocations) for
    /// building a call graph. `None` for languages without a `tags.scm` (only
    /// rust ships one).
    ///
    /// Compiled on the first call.
    pub fn tags_query(&self) -> Option<&Query> {
        self.aux.tags.get(&self.grammar)
    }

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
    /// Parse each host node's byte range as the language `name` names.
    ///
    /// Named rather than held, and filled by the registry once every language
    /// exists, as [`Language::fence_candidates`] is. Holding it would mean a
    /// grammar could not be compiled until the one it injects had been, and
    /// that wait buys nothing. The injection query is built from host node
    /// kinds alone, and the inner language is not read until a parse descends
    /// into one.
    Fixed {
        name: &'static str,
        language: OnceLock<Arc<Language>>,
    },
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
        // No language waits on another. An injection names the language it
        // hosts rather than holding it, so each of these is its own highlight
        // query and nothing else, and the wall clock is the slowest one rather
        // than the sum.
        let (rust, json, toml, markdown, markdown_inline) = std::thread::scope(|s| {
            let rust = s.spawn(make_rust);
            let json = s.spawn(make_json);
            let toml = s.spawn(make_toml);
            let markdown_inline = s.spawn(make_markdown_inline);
            let markdown = make_markdown();
            (
                rust.join().expect("rust language thread panicked"),
                json.join().expect("json language thread panicked"),
                toml.join().expect("toml language thread panicked"),
                markdown,
                markdown_inline
                    .join()
                    .expect("markdown-inline language thread panicked"),
            )
        });

        let registry = Self {
            languages: vec![
                Arc::new(rust),
                Arc::new(json),
                Arc::new(toml),
                Arc::new(markdown),
                Arc::new(markdown_inline),
            ],
        };

        // Both kinds of injection are wired now that every language exists,
        // which is the earliest either can be. A named injection would
        // otherwise force its host to wait for it, and a fence resolves its
        // info-string token against the whole set, which includes languages
        // that inject the fence host back (rust hosts markdown in its doc
        // comments, and markdown fences resolve to rust).
        for lang in &registry.languages {
            let mut hosts_fence = false;
            for injection in &lang.injections {
                match &injection.inner {
                    InjectionInner::Fixed { name, language } => {
                        if let Some(inner) = registry.by_name(name) {
                            let _ = language.set(inner);
                        }
                    },
                    InjectionInner::Fence => hosts_fence = true,
                }
            }
            if hosts_fence {
                let _ = lang.fence_candidates.set(registry.languages.clone());
            }
        }
        registry
    }

    fn by_name(&self, name: &str) -> Option<Arc<Language>> {
        self.languages.iter().find(|l| l.name == name).cloned()
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

    // Only these two are read to paint, so only these two are compiled here.
    // Each Query::new is tens of milliseconds against the rust grammar, and the
    // action-driven five wait until something asks for them.
    let highlight_query = Query::new(&grammar, highlight_src)
        .unwrap_or_else(|e| panic!("highlight query for {name} failed to compile: {e}"));
    let injection_query = build_injection_query(name, &grammar, &injections);

    Language {
        name,
        extensions,
        grammar,
        highlight_query,
        highlight_map: Mutex::new(HighlightMap::default()),
        injections,
        injection_query,
        aux: AuxQueries {
            brackets: LazyQuery::new(brackets),
            indents: LazyQuery::new(indents),
            textobjects: LazyQuery::new(textobjects),
            outline: LazyQuery::new(outline),
            tags: LazyQuery::new(tags),
        },
        line_comment,
        fence_candidates: OnceLock::new(),
    }
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
            InjectionInner::Fixed { .. } => {
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

fn make_rust() -> Language {
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
            inner: InjectionInner::Fixed {
                name: "markdown",
                language: OnceLock::new(),
            },
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

fn make_markdown() -> Language {
    // Inline nodes the block grammar emits parse as markdown-inline, for
    // emphasis, links and code spans.
    let mut injections = vec![LanguageInjection {
        host_node_kind: "inline",
        inner: InjectionInner::Fixed {
            name: "markdown-inline",
            language: OnceLock::new(),
        },
    }];
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
    use super::{LanguageRegistry, LazyQuery};
    use std::path::Path;

    /// Nothing action-driven is compiled until something asks for it, which is
    /// what keeps it off the path to the first frame. Each costs tens of
    /// milliseconds, and a session may never run the motion that needs one.
    #[test]
    fn an_aux_query_waits_to_be_asked_for_and_then_is_compiled_once() {
        let reg = LanguageRegistry::standard();
        let rust = reg.for_path(Path::new("a.rs")).expect("rust");

        assert!(
            rust.aux.brackets.compiled.get().is_none(),
            "building the language did not compile it",
        );

        let first = rust.bracket_query().expect("rust ships brackets.scm") as *const _;
        assert_eq!(
            rust.bracket_query().expect("still there") as *const _,
            first,
            "and asking again hands back the one already compiled",
        );
    }

    /// A grammar that ships no such file has nothing to compile, and asking
    /// settles that rather than leaving it to be re-decided per call.
    #[test]
    fn an_absent_aux_query_is_settled_by_the_first_ask() {
        let reg = LanguageRegistry::standard();
        let rust = reg.for_path(Path::new("a.rs")).expect("rust");
        let absent = LazyQuery::new(None);

        assert!(absent.get(&rust.grammar).is_none());
        assert_eq!(
            absent.compiled.get(),
            Some(&None),
            "the answer is recorded, so a second ask is not another attempt",
        );
    }

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
            rust.bracket_query().is_some(),
            "rust brackets.scm must compile against the bundled grammar"
        );
        assert!(
            rust.indent_query().is_some(),
            "rust indents.scm must compile against the bundled grammar"
        );
        let json = reg.languages().iter().find(|l| l.name == "json").unwrap();
        assert!(
            json.bracket_query().is_some(),
            "json brackets.scm must compile"
        );
    }

    #[test]
    fn textobjects_query_loaded_for_rust_and_toml() {
        let reg = LanguageRegistry::standard();
        let rust = reg.languages().iter().find(|l| l.name == "rust").unwrap();
        assert!(
            rust.textobjects_query().is_some(),
            "rust textobjects.scm must compile against the bundled grammar"
        );
        let toml = reg.languages().iter().find(|l| l.name == "toml").unwrap();
        assert!(
            toml.textobjects_query().is_some(),
            "toml textobjects.scm must compile against the bundled grammar"
        );
        let json = reg.languages().iter().find(|l| l.name == "json").unwrap();
        assert!(
            json.textobjects_query().is_none(),
            "json has no textobjects.scm; query should be None"
        );
    }

    #[test]
    fn outline_query_loaded_for_rust_json_markdown() {
        let reg = LanguageRegistry::standard();
        for name in ["rust", "json", "markdown"] {
            let lang = reg.languages().iter().find(|l| l.name == name).unwrap();
            assert!(
                lang.outline_query().is_some(),
                "{name} outline.scm must compile against the bundled grammar"
            );
        }

        let json = reg.languages().iter().find(|l| l.name == "json").unwrap();
        assert!(
            json.outline_query()
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
            rust.tags_query()
                .as_ref()
                .expect("rust tags.scm must compile against the bundled grammar")
                .capture_index_for_name("reference.call")
                .is_some(),
            "rust tags.scm must expose a @reference.call capture"
        );

        let json = reg.languages().iter().find(|l| l.name == "json").unwrap();
        assert!(
            json.tags_query().is_none(),
            "json has no tags.scm; query should be None"
        );
    }
}
