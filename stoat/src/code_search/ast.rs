use super::{line_snippet, line_start_at, offset_to_line_column, SearchMatch};
use ast_grep_core::{
    matcher::{PatternBuilder, PatternError},
    tree_sitter::{LanguageExt, StrDoc, TSLanguage},
    Language, Pattern,
};
use std::{path::Path, sync::Arc};

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

/// Parse `text` with `lang`, match `pattern`, and push one [`SearchMatch`] per hit
/// onto `out`, keyed by the matched node's start byte offset.
pub(crate) fn ast_scan_file(
    text: &str,
    lang: &AstLang,
    pattern: &Pattern,
    path: &Path,
    out: &mut Vec<SearchMatch>,
) {
    let root = lang.ast_grep(text);

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
                path: path.to_path_buf(),
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
            path: path.to_path_buf(),
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

    #[test]
    fn scans_a_rust_fn_pattern_across_lines() {
        let lang = AstLang::new(rust_lang());
        let pattern = Pattern::try_new("fn $NAME() {}", lang.clone()).expect("pattern compiles");
        let text = "fn alpha() {}\nfn beta() {}\n";

        let mut out = Vec::new();
        ast_scan_file(text, &lang, &pattern, Path::new("/x.rs"), &mut out);

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

        let mut out = Vec::new();
        ast_scan_file(text, &lang, &pattern, Path::new("/x.rs"), &mut out);

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

    #[test]
    fn empty_pattern_is_a_compile_error() {
        let lang = AstLang::new(rust_lang());
        assert!(Pattern::try_new("", lang).is_err());
    }
}
