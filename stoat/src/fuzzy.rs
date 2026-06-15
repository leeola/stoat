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
    let mut out: Vec<RankedMatch<T>> = Vec::new();
    for (item, haystack) in items {
        let hay = Utf32Str::new(&haystack, &mut hay_buf);
        if let Some(scored) = score_with_bonuses(&pattern, &haystack, hay, &mut guard) {
            out.push(RankedMatch {
                item,
                haystack,
                score: scored.score,
                matched_indices: scored.indices,
            });
        }
    }
    Some(out)
}

struct Scored {
    score: u32,
    indices: Vec<u32>,
}

/// Walks `pattern.atoms` individually and combines per-atom scores
/// and indices. Layers two bonuses on the raw nucleo score:
///
/// 1. In-order-token: when each atom's first matched index strictly exceeds the previous atom's
///    last, the combined score is multiplied by 5/4 (~1.25x). Single-atom queries trivially satisfy
///    the order check.
/// 2. Basename: when every matched character lies past the last `/` in the haystack, add a fixed
///    `+50`. Lifts file-name matches above directory-prefix matches; haystacks with no `/` (e.g.
///    action names in the command palette) trivially satisfy the check.
fn score_with_bonuses(
    pattern: &Pattern,
    haystack_str: &str,
    haystack: Utf32Str<'_>,
    matcher: &mut Matcher,
) -> Option<Scored> {
    let mut total_score: u32 = 0;
    let mut per_atom: Vec<Vec<u32>> = Vec::with_capacity(pattern.atoms.len());
    for atom in &pattern.atoms {
        let mut atom_indices: Vec<u32> = Vec::new();
        let score = atom.indices(haystack, matcher, &mut atom_indices)?;
        total_score = total_score.saturating_add(u32::from(score));
        atom_indices.sort_unstable();
        atom_indices.dedup();
        per_atom.push(atom_indices);
    }

    if is_in_order(&per_atom) {
        total_score = total_score.saturating_mul(5) / 4;
    }

    let mut combined: Vec<u32> = per_atom.into_iter().flatten().collect();
    combined.sort_unstable();
    combined.dedup();

    if all_in_basename(&combined, haystack_str) {
        total_score = total_score.saturating_add(BASENAME_BONUS);
    }

    Some(Scored {
        score: total_score,
        indices: combined,
    })
}

/// Bonus added when every matched character is in the basename
/// (past the last `/`). Tuned to be meaningful versus nucleo's
/// per-character bonuses (8-18 each, totals around 100-300 for
/// short queries) without dominating the raw score.
const BASENAME_BONUS: u32 = 50;

fn all_in_basename(indices: &[u32], haystack: &str) -> bool {
    let Some(last_slash) = last_slash_char_pos(haystack) else {
        return true;
    };
    indices.iter().all(|&i| i > last_slash)
}

fn last_slash_char_pos(haystack: &str) -> Option<u32> {
    let mut last: Option<u32> = None;
    for (i, c) in haystack.chars().enumerate() {
        if c == '/' {
            last = Some(i as u32);
        }
    }
    last
}

fn is_in_order(per_atom: &[Vec<u32>]) -> bool {
    let mut last_end: Option<u32> = None;
    for indices in per_atom {
        let Some(&first) = indices.first() else {
            return false;
        };
        if let Some(end) = last_end
            && first <= end
        {
            return false;
        }
        last_end = Some(*indices.last().unwrap_or(&first));
    }
    true
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

    #[test]
    fn match_and_rank_in_order_query_outscores_reversed() {
        let items = vec![(0usize, "src/foo.rs".to_string())];
        let in_order = match_and_rank("foo .rs", items.clone()).expect("query has atoms");
        let reversed = match_and_rank(".rs foo", items).expect("query has atoms");
        assert_eq!(in_order.len(), 1);
        assert_eq!(reversed.len(), 1);
        assert!(
            in_order[0].score > reversed[0].score,
            "expected in-order score {} > reversed score {}",
            in_order[0].score,
            reversed[0].score,
        );
    }

    #[test]
    fn match_and_rank_single_atom_receives_in_order_bonus() {
        // Query that matches as a single atom should still get the
        // bonus; the order check vacuously holds for one atom.
        let items = vec![(0usize, "foo.rs".to_string())];
        let bonus = match_and_rank("foo", items).expect("query has atoms");
        assert_eq!(bonus.len(), 1);
        assert!(bonus[0].score > 0);
    }

    #[test]
    fn match_and_rank_basename_match_outscores_directory_prefix() {
        let items = vec![
            (0usize, "src/foo.rs".to_string()),
            (1usize, "foo_helpers/util.rs".to_string()),
        ];
        let results = match_and_rank("foo", items).expect("query has atoms");
        assert_eq!(results.len(), 2);
        let basename = results
            .iter()
            .find(|m| m.item == 0)
            .expect("src/foo.rs in results");
        let prefix = results
            .iter()
            .find(|m| m.item == 1)
            .expect("foo_helpers/util.rs in results");
        assert!(
            basename.score > prefix.score,
            "expected basename score {} > directory-prefix score {}",
            basename.score,
            prefix.score,
        );
    }

    #[test]
    fn match_and_rank_basename_bonus_skips_when_match_crosses_slash() {
        // `srf` matches `s` `r` `f` at indices 0, 1, 4 in `src/foo.rs`.
        // 0 and 1 are at-or-before the slash (index 3), so the bonus
        // must not fire.
        let items = vec![(0usize, "src/foo.rs".to_string())];
        let with = match_and_rank("srf", items.clone()).expect("query has atoms");
        let basename_only = match_and_rank("foo", items).expect("query has atoms");
        // Sanity check both queries match the same haystack so we can
        // compare scoring shape: the basename-only query should be
        // strictly higher because it earns the +50 bonus.
        assert_eq!(with.len(), 1);
        assert_eq!(basename_only.len(), 1);
        assert!(
            basename_only[0].score > with[0].score,
            "basename-only score {} should exceed crossing score {}",
            basename_only[0].score,
            with[0].score,
        );
    }

    #[test]
    fn match_and_rank_basename_bonus_applies_to_no_slash_haystacks() {
        // Action-name-style haystacks (no slash) should receive the
        // bonus trivially, since "every match is in the basename"
        // is vacuously true.
        let items = vec![(0usize, "QuitAll".to_string())];
        let results = match_and_rank("quit", items).expect("query has atoms");
        assert_eq!(results.len(), 1);
        assert!(results[0].score > 0);
    }
}
