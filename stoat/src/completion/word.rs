//! Buffer-word completion source. Walks the focused buffer's
//! rope for word-shaped tokens that start with the typed prefix
//! and returns each unique match. Acts as a fallback when neither
//! the LSP source nor the path source fires (plain-text buffers,
//! files without an LSP).

use crate::{
    buffer::TextBufferSnapshot,
    completion::{anchor_range, CompletionContext, CompletionItem, CompletionSource},
};
use std::collections::BTreeSet;
use stoat_text::char_is_word;

/// Unique matches one fetch keeps before it stops walking the buffer.
///
/// The popup shows a screen of rows at a time, so a list this long is already
/// far past what anyone reads down to. Everything past it costs the rest of the
/// walk, an allocation apiece, and another pass in every ranking and refine the
/// list then goes through.
///
/// The cap takes the first matches in buffer order rather than the best ones,
/// which is what makes stopping the walk worth anything. They come back
/// alphabetical either way.
const MAX_MATCHES: usize = 500;

/// Collect every word-shaped token in `buffer` whose label starts
/// with `ctx.prefix`. Skips the prefix itself (no point suggesting
/// what is already typed) and dedupes repeats.
///
/// Returns at most [`MAX_MATCHES`], stopping the walk once it has them.
///
/// Returns empty when `ctx.prefix` is empty -- the fallback source
/// only fires once the user has typed at least one identifier
/// character.
pub fn fetch(ctx: &CompletionContext<'_>, buffer: &TextBufferSnapshot) -> Vec<CompletionItem> {
    if ctx.prefix.is_empty() {
        return Vec::new();
    }

    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut current: String = String::new();

    for ch in buffer.visible_text.chars() {
        if char_is_word(ch) {
            current.push(ch);
        } else if !current.is_empty() {
            collect(&mut current, ctx.prefix, &mut seen);
            // `collect` left the token empty, so the tail below adds nothing.
            if seen.len() >= MAX_MATCHES {
                break;
            }
        }
    }
    if !current.is_empty() && seen.len() < MAX_MATCHES {
        collect(&mut current, ctx.prefix, &mut seen);
    }

    let replace_range = anchor_range(buffer, ctx.prefix_range.clone());
    seen.into_iter()
        .map(|label| CompletionItem {
            label: label.clone(),
            source: CompletionSource::Word,
            kind: None,
            detail: None,
            replace_range: replace_range.clone(),
            insert_text: label,
            is_snippet: false,
            documentation: None,
            lsp_item: None,
            server: None,
        })
        .collect()
}

fn collect(current: &mut String, prefix: &str, seen: &mut BTreeSet<String>) {
    // Asked with a borrow, so a word already held costs a lookup rather than an
    // allocation the insert then drops. Most of what a large buffer offers is
    // repeats of what it offered already.
    if current.starts_with(prefix) && current != prefix && !seen.contains(current.as_str()) {
        seen.insert(current.clone());
    }
    current.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        buffer::{BufferId, TextBuffer},
        completion::replace_offsets,
    };

    fn ctx<'a>(prefix: &'a str) -> CompletionContext<'a> {
        let len = prefix.len();
        CompletionContext {
            cursor_offset: len,
            prefix,
            prefix_range: 0..len,
            text_before_cursor: prefix,
        }
    }

    fn snapshot(text: &str) -> TextBufferSnapshot {
        TextBuffer::with_text(BufferId::new(1), text).snapshot
    }

    fn labels(items: &[CompletionItem]) -> Vec<String> {
        items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn empty_prefix_returns_empty() {
        let buffer = snapshot("foo bar baz");
        let items = fetch(&ctx(""), &buffer);
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn prefix_without_matches_returns_empty() {
        let buffer = snapshot("alpha beta gamma");
        let items = fetch(&ctx("xy"), &buffer);
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn single_match_returns_one_item() {
        let buffer = snapshot("println foo");
        let items = fetch(&ctx("pr"), &buffer);
        assert_eq!(labels(&items), vec!["println"]);
    }

    #[test]
    fn duplicates_are_deduped() {
        let buffer = snapshot("foo bar foo baz foo");
        let items = fetch(&ctx("fo"), &buffer);
        assert_eq!(labels(&items), vec!["foo"]);
    }

    #[test]
    fn prefix_itself_not_suggested() {
        let buffer = snapshot("foo bar");
        let items = fetch(&ctx("foo"), &buffer);
        assert_eq!(items, Vec::new());
    }

    #[test]
    fn multiple_matches_returned_sorted() {
        let buffer = snapshot("foo foobar foobaz qux");
        let items = fetch(&ctx("foo"), &buffer);
        assert_eq!(labels(&items), vec!["foobar", "foobaz"]);
    }

    #[test]
    fn punctuation_separates_tokens() {
        let buffer = snapshot("foo,bar.baz;qux");
        let items = fetch(&ctx("ba"), &buffer);
        assert_eq!(labels(&items), vec!["bar", "baz"]);
    }

    #[test]
    fn underscore_is_part_of_token() {
        let buffer = snapshot("_foo bar_baz hello");
        let items_underscore = fetch(&ctx("_f"), &buffer);
        assert_eq!(labels(&items_underscore), vec!["_foo"]);
        let items_bar = fetch(&ctx("bar"), &buffer);
        assert_eq!(labels(&items_bar), vec!["bar_baz"]);
    }

    #[test]
    fn final_token_at_buffer_end_is_collected() {
        let buffer = snapshot("alpha beta foobar");
        let items = fetch(&ctx("foo"), &buffer);
        assert_eq!(labels(&items), vec!["foobar"]);
    }

    #[test]
    fn replace_range_mirrors_context_prefix_range() {
        // The prefix is the "foo" the word starts with, typed five bytes in.
        let buffer = snapshot("     foobar");
        let mut c = ctx("foo");
        c.prefix_range = 5..8;
        c.cursor_offset = 8;
        let items = fetch(&c, &buffer);
        assert_eq!(items.len(), 1);
        assert_eq!(replace_offsets(&buffer, &items[0]), 5..8);
    }

    /// A one-character prefix in a large buffer matches most of it, and the
    /// whole set is walked again by every ranking and refine downstream.
    #[test]
    fn the_match_set_stops_at_the_cap() {
        let text: String = (0..MAX_MATCHES + 50).map(|i| format!("foo{i} ")).collect();
        let buffer = snapshot(text.as_str());

        let items = fetch(&ctx("foo"), &buffer);
        assert_eq!(items.len(), MAX_MATCHES);
    }

    #[test]
    fn case_sensitive_match() {
        let buffer = snapshot("Foo foo FOO");
        let items = fetch(&ctx("Fo"), &buffer);
        assert_eq!(labels(&items), vec!["Foo"]);
    }
}
