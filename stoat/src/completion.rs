//! Editor autocomplete (LSP, path, word). This module owns the
//! shared [`CompletionSource`] enum, the unified [`CompletionItem`]
//! shape every source produces, and the dispatch entry that asks
//! each source whether the current cursor context invites it.
//!
//! Per-source fetch routines (path entries via
//! [`crate::host::FsHost`], LSP completion via
//! [`crate::host::LspHost`], rope-walk word source) live in
//! sibling submodules.

pub(crate) mod accept;
#[cfg(test)]
mod e2e;
pub mod lsp;
pub mod path;
pub(crate) mod request;
pub mod word;

use std::ops::Range;

/// Identifies which subsystem produced a completion item; consumed
/// by the dispatch entry to decide whether the source applies in
/// the current cursor context, and by the popup renderer to badge
/// each row by origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CompletionSource {
    /// Items returned by `textDocument/completion` from the active
    /// LSP server.
    Lsp,
    /// Filesystem entries when the cursor sits on a path-like
    /// prefix.
    Path,
    /// Word-shaped tokens scraped from the focused buffer's rope.
    Word,
}

/// High-level category badge shared across sources. The popup
/// paints a one- or two-letter glyph derived from this; LSP
/// completion items map to the closest variant, the path source
/// uses [`CompletionItemKind::File`] / [`CompletionItemKind::Folder`],
/// the word source uses [`CompletionItemKind::Other`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionItemKind {
    Function,
    Method,
    Variable,
    Field,
    Module,
    Class,
    Struct,
    Enum,
    Constant,
    Keyword,
    File,
    Folder,
    Snippet,
    Other,
}

/// One row in the completion popup. The unified shape lets the
/// popup renderer, the acceptance handler, and the persistence
/// layer treat every source identically; per-source extras (LSP
/// text edits, snippet placeholders) extend this struct as the
/// items 85/89 land them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionItem {
    /// The text the popup renders for this row.
    pub label: String,
    /// Which source produced the item.
    pub source: CompletionSource,
    /// Optional badge category. Sources omit when they cannot infer
    /// one; the popup falls back to a blank gutter.
    pub kind: Option<CompletionItemKind>,
    /// Single-line summary painted dimmed beside the label when
    /// the terminal width permits.
    pub detail: Option<String>,
    /// Byte-range in the source buffer that acceptance replaces.
    /// Sources scope this to the current prefix at minimum; LSP
    /// items widen it to the server's `text_edit.range`.
    pub replace_range: Range<usize>,
    /// Text inserted into the buffer when the user accepts this
    /// row.
    pub insert_text: String,
}

/// In-flight completion popup. Held on
/// [`crate::Stoat::pending_completion`] while the popup is showing;
/// the trigger pipeline (item 83) replaces the items as the prefix
/// changes and clears the field when the cursor leaves
/// [`Self::prefix_range`]; the acceptance handler (item 89)
/// consumes the entry on `Tab`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletionPopup {
    /// Rows the popup paints, in display order (already filtered
    /// and ranked by the trigger pipeline).
    pub items: Vec<CompletionItem>,
    /// Index of the highlighted row in [`Self::items`]. Tab accepts
    /// this row; Up / Down adjust it within bounds.
    pub selected_idx: usize,
    /// Byte offset in the buffer the popup is anchored to. The
    /// renderer maps this to a screen cell via the focused editor's
    /// display map.
    pub anchor_offset: usize,
    /// Byte range the popup matches against -- the substring
    /// acceptance replaces. Defaults to the buffer span the prefix
    /// at trigger time covers; LSP items may widen via their own
    /// `text_edit.range`.
    pub prefix_range: Range<usize>,
}

/// State the dispatch entry hands to each source so it can decide
/// whether to fire and what to match against. Borrowed view over
/// the focused buffer's rope; the caller computes prefix bounds.
pub struct CompletionContext<'a> {
    /// Byte offset of the cursor in the buffer.
    pub cursor_offset: usize,
    /// Substring immediately preceding the cursor that completions
    /// should match against (path tail, identifier, etc).
    pub prefix: &'a str,
    /// Byte range of [`Self::prefix`] in the buffer; acceptance
    /// scopes the default `replace_range` to this span.
    pub prefix_range: Range<usize>,
    /// Buffer text from the start of the line (or other relevant
    /// scope) up to the cursor. Sources that need wider context
    /// than [`Self::prefix`] (e.g. path-prefix detection on `./`)
    /// read from this slice.
    pub text_before_cursor: &'a str,
}

impl CompletionSource {
    /// Whether this source should fire given the current cursor
    /// context. Path source triggers on path-shaped prefixes; LSP
    /// and Word source trigger on identifier-shaped prefixes. Each
    /// per-source fetch routine (added in items 84-86) is
    /// responsible for returning items consistent with its
    /// predicate.
    pub fn applies(&self, ctx: &CompletionContext<'_>) -> bool {
        match self {
            CompletionSource::Path => is_path_like(ctx),
            CompletionSource::Lsp => is_identifier_like(ctx),
            CompletionSource::Word => is_identifier_like(ctx),
        }
    }
}

/// Returns the sources that apply, in priority order
/// (Path, then Lsp, then Word). The popup state (item 87) and the
/// request pipeline (item 83) call this to decide which sources to
/// ask for items on each keystroke.
pub fn applicable_sources(ctx: &CompletionContext<'_>) -> Vec<CompletionSource> {
    let mut sources = Vec::with_capacity(3);
    for source in [
        CompletionSource::Path,
        CompletionSource::Lsp,
        CompletionSource::Word,
    ] {
        if source.applies(ctx) {
            sources.push(source);
        }
    }
    sources
}

/// Path-like prefix detection. Returns true when the prefix already
/// contains a `/`, or when the wider text immediately before the
/// cursor ends with one of the leading-segment shapes `/`, `./`,
/// `../`, `~`. Together these cover both "user is typing inside a
/// path segment" and "cursor sits at the start of a fresh segment".
fn is_path_like(ctx: &CompletionContext<'_>) -> bool {
    if ctx.prefix.contains('/') {
        return true;
    }
    let trail = ctx.text_before_cursor;
    trail.ends_with('/') || trail.ends_with("./") || trail.ends_with("../") || trail.ends_with('~')
}

/// Identifier-like prefix detection. Matches non-empty prefixes
/// that start with an alphabetic character or `_` and continue with
/// alphanumerics or `_`. Slashed prefixes are rejected -- those
/// route through the Path source instead.
fn is_identifier_like(ctx: &CompletionContext<'_>) -> bool {
    let mut chars = ctx.prefix.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_alphabetic() || first == '_') {
        return false;
    }
    chars.all(|c| c.is_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(text_before_cursor: &'a str, prefix: &'a str) -> CompletionContext<'a> {
        let cursor_offset = text_before_cursor.len();
        let prefix_start = cursor_offset.saturating_sub(prefix.len());
        CompletionContext {
            cursor_offset,
            prefix,
            prefix_range: prefix_start..cursor_offset,
            text_before_cursor,
        }
    }

    #[test]
    fn path_like_triggers_on_dot_slash() {
        assert!(is_path_like(&ctx("./", "")));
    }

    #[test]
    fn path_like_triggers_on_dot_dot_slash() {
        assert!(is_path_like(&ctx("../", "")));
    }

    #[test]
    fn path_like_triggers_on_root_slash() {
        assert!(is_path_like(&ctx("/", "")));
    }

    #[test]
    fn path_like_triggers_on_tilde() {
        assert!(is_path_like(&ctx("~", "")));
    }

    #[test]
    fn path_like_triggers_when_prefix_contains_slash() {
        assert!(is_path_like(&ctx("foo/bar", "foo/bar")));
    }

    #[test]
    fn path_like_triggers_after_subpath_separator() {
        assert!(is_path_like(&ctx("src/", "")));
    }

    #[test]
    fn path_like_rejects_bare_identifier() {
        assert!(!is_path_like(&ctx("foo", "foo")));
    }

    #[test]
    fn path_like_rejects_empty() {
        assert!(!is_path_like(&ctx("", "")));
    }

    #[test]
    fn identifier_like_accepts_simple() {
        assert!(is_identifier_like(&ctx("foo", "foo")));
    }

    #[test]
    fn identifier_like_accepts_underscore_lead() {
        assert!(is_identifier_like(&ctx("_bar", "_bar")));
    }

    #[test]
    fn identifier_like_accepts_alphanumeric() {
        assert!(is_identifier_like(&ctx("foo_baz9", "foo_baz9")));
    }

    #[test]
    fn identifier_like_rejects_empty_prefix() {
        assert!(!is_identifier_like(&ctx("foo", "")));
    }

    #[test]
    fn identifier_like_rejects_leading_digit() {
        assert!(!is_identifier_like(&ctx("9foo", "9foo")));
    }

    #[test]
    fn identifier_like_rejects_slashed_prefix() {
        assert!(!is_identifier_like(&ctx("foo/bar", "foo/bar")));
    }

    #[test]
    fn applies_path_source_on_path_context() {
        let c = ctx("./", "");
        assert!(CompletionSource::Path.applies(&c));
        assert!(!CompletionSource::Lsp.applies(&c));
        assert!(!CompletionSource::Word.applies(&c));
    }

    #[test]
    fn applies_identifier_sources_on_identifier_context() {
        let c = ctx("foo", "foo");
        assert!(!CompletionSource::Path.applies(&c));
        assert!(CompletionSource::Lsp.applies(&c));
        assert!(CompletionSource::Word.applies(&c));
    }

    #[test]
    fn applicable_sources_returns_path_first_for_slashed_prefix() {
        let s = applicable_sources(&ctx("foo/bar", "foo/bar"));
        assert_eq!(s, vec![CompletionSource::Path]);
    }

    #[test]
    fn applicable_sources_returns_lsp_then_word_for_identifier() {
        let s = applicable_sources(&ctx("foo", "foo"));
        assert_eq!(s, vec![CompletionSource::Lsp, CompletionSource::Word]);
    }

    #[test]
    fn applicable_sources_returns_path_only_for_path_segment_start() {
        let s = applicable_sources(&ctx("./", ""));
        assert_eq!(s, vec![CompletionSource::Path]);
    }

    #[test]
    fn applicable_sources_returns_empty_for_empty_prefix() {
        let s = applicable_sources(&ctx("", ""));
        assert!(s.is_empty());
    }

    #[test]
    fn completion_item_clone_round_trip() {
        let item = CompletionItem {
            label: "open_file".into(),
            source: CompletionSource::Lsp,
            kind: Some(CompletionItemKind::Function),
            detail: Some("fn open_file(path: &Path)".into()),
            replace_range: 4..12,
            insert_text: "open_file(${1:path})".into(),
        };
        assert_eq!(item.clone(), item);
    }

    #[test]
    fn context_records_cursor_and_prefix_range() {
        let c = ctx("let foo", "foo");
        assert_eq!(c.cursor_offset, 7);
        assert_eq!(c.prefix_range, 4..7);
        assert_eq!(c.prefix, "foo");
        assert_eq!(c.text_before_cursor, "let foo");
    }

    #[test]
    fn completion_popup_round_trip() {
        let item = CompletionItem {
            label: "open_file".into(),
            source: CompletionSource::Lsp,
            kind: Some(CompletionItemKind::Function),
            detail: None,
            replace_range: 4..7,
            insert_text: "open_file()".into(),
        };
        let popup = CompletionPopup {
            items: vec![item.clone()],
            selected_idx: 0,
            anchor_offset: 4,
            prefix_range: 4..7,
        };
        assert_eq!(popup.clone(), popup);
        assert_eq!(popup.items, [item]);
        assert_eq!(popup.selected_idx, 0);
        assert_eq!(popup.anchor_offset, 4);
        assert_eq!(popup.prefix_range, 4..7);
    }

    #[test]
    fn completion_item_kind_covers_each_variant() {
        let kinds = [
            CompletionItemKind::Function,
            CompletionItemKind::Method,
            CompletionItemKind::Variable,
            CompletionItemKind::Field,
            CompletionItemKind::Module,
            CompletionItemKind::Class,
            CompletionItemKind::Struct,
            CompletionItemKind::Enum,
            CompletionItemKind::Constant,
            CompletionItemKind::Keyword,
            CompletionItemKind::File,
            CompletionItemKind::Folder,
            CompletionItemKind::Snippet,
            CompletionItemKind::Other,
        ];
        for kind in kinds {
            let item = CompletionItem {
                label: "x".into(),
                source: CompletionSource::Lsp,
                kind: Some(kind),
                detail: None,
                replace_range: 0..0,
                insert_text: String::new(),
            };
            assert_eq!(item.kind, Some(kind));
        }
    }
}
