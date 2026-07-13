//! Buffer-word completion source. Walks the focused buffer's
//! rope for word-shaped tokens that start with the typed prefix
//! and returns each unique match. Acts as a fallback when neither
//! the LSP source nor the path source fires (plain-text buffers,
//! files without an LSP).

use crate::completion::{CompletionContext, CompletionItem, CompletionSource};
use std::collections::BTreeSet;
use stoat_text::Rope;

/// Collect every word-shaped token in `rope` whose label starts
/// with `ctx.prefix`. Skips the prefix itself (no point suggesting
/// what is already typed) and dedupes repeats.
///
/// Returns empty when `ctx.prefix` is empty -- the fallback source
/// only fires once the user has typed at least one identifier
/// character.
pub fn fetch(ctx: &CompletionContext<'_>, rope: &Rope) -> Vec<CompletionItem> {
    if ctx.prefix.is_empty() {
        return Vec::new();
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut current: String = String::new();

    for ch in rope.chars() {
        if is_word_char(ch) {
            current.push(ch);
        } else if !current.is_empty() {
            collect(&mut current, ctx.prefix, &mut seen);
        }
    }
    if !current.is_empty() {
        collect(&mut current, ctx.prefix, &mut seen);
    }

    seen.into_iter()
        .map(|label| CompletionItem {
            label: label.clone(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            replace_range: ctx.prefix_range.clone(),
            insert_text: label,
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        })
        .collect()
}

fn collect(current: &mut String, prefix: &str, seen: &mut BTreeSet<String>) {
    if current.starts_with(prefix) && current != prefix {
        seen.insert(current.clone());
    }
    current.clear();
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx<'a>(prefix: &'a str) -> CompletionContext<'a> {
        let len = prefix.len();
        CompletionContext {
            cursor_offset: len,
            prefix,
            prefix_range: 0..len,
            text_before_cursor: prefix,
        }
    }

    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn empty_prefix_returns_empty() {
        let rope = Rope::from("foo bar baz");
        let items = fetch(&ctx(""), &rope);
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn prefix_without_matches_returns_empty() {
        let rope = Rope::from("alpha beta gamma");
        let items = fetch(&ctx("xy"), &rope);
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn single_match_returns_one_item() {
        let rope = Rope::from("println foo");
        let items = fetch(&ctx("pr"), &rope);
        assert_eq!(labels(&items), vec!["println"]);
    }

    #[test]
    fn duplicates_are_deduped() {
        let rope = Rope::from("foo bar foo baz foo");
        let items = fetch(&ctx("fo"), &rope);
        assert_eq!(labels(&items), vec!["foo"]);
    }

    #[test]
    fn prefix_itself_not_suggested() {
        let rope = Rope::from("foo bar");
        let items = fetch(&ctx("foo"), &rope);
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn multiple_matches_returned_sorted() {
        let rope = Rope::from("foo foobar foobaz qux");
        let items = fetch(&ctx("foo"), &rope);
        assert_eq!(labels(&items), vec!["foobar", "foobaz"]);
    }

    #[test]
    fn punctuation_separates_tokens() {
        let rope = Rope::from("foo,bar.baz;qux");
        let items = fetch(&ctx("ba"), &rope);
        assert_eq!(labels(&items), vec!["bar", "baz"]);
    }

    #[test]
    fn underscore_is_part_of_token() {
        let rope = Rope::from("_foo bar_baz hello");
        let items_underscore = fetch(&ctx("_f"), &rope);
        assert_eq!(labels(&items_underscore), vec!["_foo"]);
        let items_bar = fetch(&ctx("bar"), &rope);
        assert_eq!(labels(&items_bar), vec!["bar_baz"]);
    }

    #[test]
    fn final_token_at_buffer_end_is_collected() {
        let rope = Rope::from("alpha beta foobar");
        let items = fetch(&ctx("foo"), &rope);
        assert_eq!(labels(&items), vec!["foobar"]);
    }

    #[test]
    fn replace_range_mirrors_context_prefix_range() {
        let rope = Rope::from("foobar");
        let mut c = ctx("foo");
        c.prefix_range = 5..8;
        c.cursor_offset = 8;
        let items = fetch(&c, &rope);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].replace_range, 5..8);
    }

    #[test]
    fn case_sensitive_match() {
        let rope = Rope::from("Foo foo FOO");
        let items = fetch(&ctx("Fo"), &rope);
        assert_eq!(labels(&items), vec!["Foo"]);
    }
}
