use crate::{grammar, highlight_map::HighlightMap};
use std::{
    path::Path,
    sync::{Arc, Mutex},
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
    /// Used by markdown to inject the markdown-inline grammar inside
    /// `inline` nodes, and could support code-fence injections later.
    pub injections: Vec<LanguageInjection>,
    /// Compiled query that captures injection host nodes by kind. Built
    /// from [`Language::injections`] when the language is constructed; the
    /// capture names match the host node kinds. `None` when there are no
    /// injections configured.
    pub injection_query: Option<Query>,
    /// Bracket-pair query loaded from `brackets.scm`. Captures `@open` and
    /// `@close` for matched bracket-like tokens. Loaded but not yet wired
    /// into a runtime consumer; reserved for grammar-driven bracket
    /// matching.
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
    /// Line-comment tokens for this language, e.g. `["//", "///", "//!"]`
    /// for rust or `["#"]` for toml. Empty for languages without line
    /// comments (e.g. JSON, markdown).
    ///
    /// The `ToggleComments` action toggles the first token, so it stays the
    /// canonical marker. [`comment_token_for_line`] resolves which flavor a
    /// given line carries by longest match, so doc-comment markers (`///`,
    /// `//!`) are recognized distinctly from a plain `//`.
    pub comment_tokens: &'static [&'static str],
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

/// The longest token in `tokens` that `line` opens with at its first
/// non-whitespace character, or `None` for a blank line or one starting
/// with no token.
///
/// Longest match so `///` and `//!` win over `//`: a line carries the
/// comment flavor of its most specific token. Pure over the inputs --
/// callers pass a language's [`Language::comment_tokens`] and a single
/// line's text.
pub fn comment_token_for_line(line: &str, tokens: &[&'static str]) -> Option<&'static str> {
    let rest = line.trim_start();
    if rest.is_empty() {
        return None;
    }
    tokens
        .iter()
        .copied()
        .filter(|token| rest.starts_with(token))
        .max_by_key(|token| token.len())
}

/// Pairs an inner [`Language`] with the host node kind it should be parsed
/// inside. The host parser produces a tree; for each node whose kind matches
/// `host_node_kind`, the inner parser is run over that node's byte range.
pub struct LanguageInjection {
    pub host_node_kind: &'static str,
    pub inner: Arc<Language>,
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
            inner: markdown_inline.clone(),
        }]));
        Self {
            languages: vec![
                Arc::new(make_rust()),
                Arc::new(make_json()),
                Arc::new(make_toml()),
                Arc::new(make_stcfg()),
                markdown,
                markdown_inline,
            ],
        }
    }

    pub fn for_path(&self, path: &Path) -> Option<Arc<Language>> {
        let ext = path.extension()?.to_str()?;
        self.languages
            .iter()
            .find(|l| l.extensions.iter().any(|e| e.eq_ignore_ascii_case(ext)))
            .cloned()
    }

    /// Look up a registered language by [`Language::name`]. The match
    /// is case-insensitive so callers accepting user input (e.g. a
    /// CLI flag) accept both `Rust` and `rust`.
    pub fn find_by_name(&self, name: &str) -> Option<Arc<Language>> {
        self.languages
            .iter()
            .find(|l| l.name.eq_ignore_ascii_case(name))
            .cloned()
    }

    pub fn languages(&self) -> &[Arc<Language>] {
        &self.languages
    }
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
    comment_tokens: &'static [&'static str],
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
    let highlight_query = Query::new(&grammar, highlight_src)
        .unwrap_or_else(|e| panic!("highlight query for {name} failed to compile: {e}"));
    let injection_query = build_injection_query(name, &grammar, &injections);
    let bracket_query = aux
        .brackets
        .and_then(|src| try_compile_query(name, "brackets", &grammar, src));
    let indent_query = aux
        .indents
        .and_then(|src| try_compile_query(name, "indents", &grammar, src));
    let textobjects_query = aux
        .textobjects
        .and_then(|src| try_compile_query(name, "textobjects", &grammar, src));
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
        comment_tokens: aux.comment_tokens,
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
        source.push_str("((");
        source.push_str(injection.host_node_kind);
        source.push_str(") @injection)\n");
    }
    let query = Query::new(grammar, &source)
        .unwrap_or_else(|e| panic!("injection query for {name} failed to compile: {e}"));
    Some(query)
}

fn make_rust() -> Language {
    make_language(
        "rust",
        &["rs"],
        grammar::rust(),
        include_str!("../../vendor/zed/crates/languages/src/rust/highlights.scm"),
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
            comment_tokens: &["//", "///", "//!"],
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
            comment_tokens: &[],
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
            comment_tokens: &["#"],
            ..Default::default()
        },
    )
}

fn make_stcfg() -> Language {
    make_language(
        "stcfg",
        &["stcfg"],
        grammar::stcfg(),
        include_str!("../../vendor/tree-sitter-stcfg/queries/highlights.scm"),
        AuxQuerySources {
            comment_tokens: &["#"],
            ..Default::default()
        },
    )
}

fn make_markdown_with_injections(injections: Vec<LanguageInjection>) -> Language {
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
            comment_tokens: &[],
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
    use super::{comment_token_for_line, LanguageRegistry};
    use std::path::Path;

    #[test]
    fn comment_token_for_line_returns_longest_match() {
        let rust = &["//", "///", "//!"];
        assert_eq!(comment_token_for_line("// x", rust), Some("//"));
        assert_eq!(comment_token_for_line("/// x", rust), Some("///"));
        assert_eq!(comment_token_for_line("//! x", rust), Some("//!"));
        assert_eq!(comment_token_for_line("    /// x", rust), Some("///"));
        assert_eq!(comment_token_for_line("code // trailing", rust), None);
        assert_eq!(comment_token_for_line("   ", rust), None);
        assert_eq!(comment_token_for_line("", rust), None);
        assert_eq!(comment_token_for_line("# x", &["#"]), Some("#"));
        assert_eq!(comment_token_for_line("// x", &[]), None);
    }

    #[test]
    fn for_path_resolves_extensions() {
        let reg = LanguageRegistry::standard();
        assert_eq!(reg.for_path(Path::new("a.rs")).unwrap().name, "rust");
        assert_eq!(reg.for_path(Path::new("a.json")).unwrap().name, "json");
        assert_eq!(reg.for_path(Path::new("a.toml")).unwrap().name, "toml");
        assert_eq!(reg.for_path(Path::new("a.stcfg")).unwrap().name, "stcfg");
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
    fn find_by_name_matches_case_insensitively() {
        let reg = LanguageRegistry::standard();
        assert_eq!(reg.find_by_name("rust").unwrap().name, "rust");
        assert_eq!(reg.find_by_name("RUST").unwrap().name, "rust");
        assert_eq!(reg.find_by_name("Markdown").unwrap().name, "markdown");
        assert!(reg.find_by_name("notalang").is_none());
        assert!(reg.find_by_name("").is_none());
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
            vec![
                "rust",
                "json",
                "toml",
                "stcfg",
                "markdown",
                "markdown-inline"
            ],
        );
    }

    #[test]
    fn stcfg_registered_with_highlights() {
        let reg = LanguageRegistry::standard();
        let stcfg = reg
            .for_path(Path::new("conf.stcfg"))
            .expect("stcfg resolves by extension");
        assert_eq!(stcfg.name, "stcfg");
        assert_eq!(stcfg.comment_tokens, &["#"]);
        // The highlight query compiled against the grammar (the constructor
        // would have panicked otherwise) and carries the expected scopes.
        let captures = stcfg.highlight_capture_names();
        for expected in [
            "keyword", "string", "comment", "operator", "property", "function",
        ] {
            assert!(captures.contains(&expected), "missing capture {expected}");
        }
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
}
