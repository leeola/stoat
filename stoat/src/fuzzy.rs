//! Shared fuzzy-matching helpers used by every picker.
//!
//! Centralises the [`nucleo`] matcher mutex, the
//! [`Pattern::parse`] empty-atoms guard, and the
//! score-plus-indices loop so the file finder, command palette,
//! and completion popup all see the same ranking and
//! highlighting. Future bonuses (in-order tokens, basename
//! preference) layer on here so they apply uniformly.

use nucleo::{
    pattern::{CaseMatching, Normalization, Pattern},
    Matcher, Utf32Str,
};
use std::sync::{Mutex, OnceLock};

/// Returns the singleton `nucleo` matcher used by every picker.
/// Held behind a [`Mutex`] because [`Matcher`] carries scratch
/// state that is not [`Sync`].
pub fn matcher() -> &'static Mutex<Matcher> {
    static MATCHER: OnceLock<Mutex<Matcher>> = OnceLock::new();
    MATCHER.get_or_init(|| Mutex::new(Matcher::default()))
}

/// Parses `text` into a [`Pattern`]. Returns `None` when there are
/// no usable atoms -- empty input, whitespace-only input, or input
/// that the parser drops entirely. Callers treat `None` as "no
/// active query, use the picker's default ordering".
///
/// Smart-case matching applies (`CaseMatching::Smart`,
/// `Normalization::Smart`): all-lowercase queries are
/// case-insensitive; queries containing uppercase trigger
/// case-sensitive matching against that atom.
pub fn parse_query(text: &str) -> Option<Pattern> {
    if text.is_empty() {
        return None;
    }
    let pattern = Pattern::parse(text, CaseMatching::Smart, Normalization::Smart);
    if pattern.atoms.is_empty() {
        return None;
    }
    Some(pattern)
}

/// One scored match returned by [`match_and_rank`].
///
/// `haystack` is returned alongside the original `item` so callers
/// can use it for tie-break ordering without having to recompute it.
/// `matched_indices` is sorted and deduplicated so renderers can do
/// `binary_search` lookups when painting cells.
pub struct RankedMatch<T> {
    pub item: T,
    pub haystack: String,
    pub score: u32,
    pub matched_indices: Vec<u32>,
}

/// Scores every `(item, haystack)` pair against `query` and returns
/// the matched ones with their score and matched-cell indices.
///
/// Returns `None` when `query` produces no usable atoms (per
/// [`parse_query`]); the caller is expected to fall back to its
/// default ordering in that case.
///
/// The result is **not** sorted -- callers tie-break per their own
/// rules (alphabetical, priority+name, etc.) after sorting by
/// `score` descending.
pub fn match_and_rank<T>(
    query: &str,
    items: impl IntoIterator<Item = (T, String)>,
) -> Option<Vec<RankedMatch<T>>> {
    let pattern = parse_query(query)?;
    let mut guard = matcher().lock().expect("fuzzy matcher poisoned");
    let mut hay_buf: Vec<char> = Vec::new();
    let mut indices_buf: Vec<u32> = Vec::new();
    let mut out: Vec<RankedMatch<T>> = Vec::new();
    for (item, haystack) in items {
        indices_buf.clear();
        let hay = Utf32Str::new(&haystack, &mut hay_buf);
        if let Some(score) = pattern.indices(hay, &mut guard, &mut indices_buf) {
            indices_buf.sort_unstable();
            indices_buf.dedup();
            out.push(RankedMatch {
                item,
                haystack,
                score,
                matched_indices: indices_buf.clone(),
            });
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_returns_none() {
        assert!(parse_query("").is_none());
    }

    #[test]
    fn whitespace_only_query_returns_none() {
        assert!(parse_query("   ").is_none());
    }

    #[test]
    fn match_and_rank_with_no_query_returns_none() {
        let items = vec![(0usize, "foo.rs".to_string())];
        assert!(match_and_rank("", items).is_none());
    }

    #[test]
    fn match_and_rank_returns_matched_indices_sorted_and_deduped() {
        let items = vec![(0usize, "foo.rs".to_string())];
        let results = match_and_rank("foo", items).expect("query has atoms");
        assert_eq!(results.len(), 1);
        let m = &results[0];
        assert!(!m.matched_indices.is_empty());
        let mut sorted = m.matched_indices.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(m.matched_indices, sorted);
    }

    #[test]
    fn match_and_rank_filters_non_matches() {
        let items = vec![
            (0usize, "foo.rs".to_string()),
            (1usize, "bar.rs".to_string()),
        ];
        let results = match_and_rank("foo", items).expect("query has atoms");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].item, 0);
    }

    #[test]
    fn match_and_rank_smart_case_lowercase_query_is_insensitive() {
        let items = vec![(0usize, "Foo.rs".to_string())];
        let results = match_and_rank("foo", items).expect("query has atoms");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn match_and_rank_smart_case_uppercase_query_is_sensitive() {
        let items = vec![
            (0usize, "Foo.rs".to_string()),
            (1usize, "foo.rs".to_string()),
        ];
        let results = match_and_rank("F", items).expect("query has atoms");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].item, 0);
    }

    #[test]
    fn match_and_rank_multi_token_matches_in_either_order() {
        let items = vec![(0usize, "src/foo.rs".to_string())];
        let forward = match_and_rank(".rs foo", items.clone()).expect("query has atoms");
        let reverse = match_and_rank("foo .rs", items).expect("query has atoms");
        assert_eq!(forward.len(), 1);
        assert_eq!(reverse.len(), 1);
    }
}
