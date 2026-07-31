//! Shared fuzzy-matching helpers used by every picker.
//!
//! Centralises the [`nucleo`] matcher, the
//! [`Pattern::parse`] empty-atoms guard, and the
//! score-plus-indices loop so the file finder, command palette,
//! and completion popup all see the same ranking and
//! highlighting. Future bonuses (in-order tokens, basename
//! preference) layer on here so they apply uniformly.

use nucleo::{
    pattern::{CaseMatching, Normalization, Pattern},
    Matcher, Utf32Str,
};
use std::cell::RefCell;

thread_local! {
    static MATCHER: RefCell<Matcher> = RefCell::new(Matcher::default());
}

/// Run `f` against this thread's `nucleo` matcher.
///
/// [`Matcher`] carries scratch state and is not [`Sync`], so it cannot simply be
/// shared. One per thread rather than one behind a lock is what lets a
/// background scan and the UI thread match at the same time instead of
/// serialising on each other.
///
/// It is reused rather than constructed per call because constructing one
/// eagerly allocates a matrix slab of some 135KB, which a caller matching a few
/// hundred rows per keystroke would otherwise pay for every time.
///
/// Do not call this from inside `f`. The matcher is behind a [`RefCell`], so a
/// nested call panics rather than handing out a second mutable borrow.
pub(crate) fn with_matcher<R>(f: impl FnOnce(&mut Matcher) -> R) -> R {
    MATCHER.with(|matcher| f(&mut matcher.borrow_mut()))
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

/// Whether appending to `query` can only shrink the set of haystacks it
/// matches.
///
/// A picker that knows this holds can answer the longer query by re-scoring
/// only the rows the shorter one matched. An ordinary atom gets harder as
/// characters arrive, so the subset relation normally falls out for free.
///
/// Two constructs invert it. A negated atom rejects what it matches, so `!ab`
/// excludes strictly more than `!abc` does and extending it widens the result.
/// A trailing backslash escapes the space that follows it, joining two atoms
/// into one whose needle is not an extension of either.
///
/// Smart case is not one of them. Appending an uppercase character flips an
/// atom to case-sensitive, but every haystack matching it case-sensitively
/// already matched case-insensitively.
pub(crate) fn extension_narrows(query: &str) -> bool {
    if query.contains('\\') {
        return false;
    }

    parse_query(query).is_none_or(|pattern| !pattern.atoms.iter().any(|atom| atom.negative))
}

/// One scored match returned by [`match_and_rank`].
///
/// `haystack` is returned alongside the original `item` so callers
/// can use it for tie-break ordering without having to recompute it.
/// `matched_indices` is sorted and deduplicated so renderers can do
/// `binary_search` lookups when painting cells.
pub struct RankedMatch<'a, T> {
    pub item: T,
    pub haystack: &'a str,
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
pub fn match_and_rank<'a, T>(
    query: &str,
    items: impl IntoIterator<Item = (T, &'a str)>,
) -> Option<Vec<RankedMatch<'a, T>>> {
    let pattern = parse_query(query)?;
    let out = with_matcher(|matcher| {
        let mut hay_buf: Vec<char> = Vec::new();
        let mut scratch = Scratch::default();
        let mut out: Vec<RankedMatch<'a, T>> = Vec::new();

        for (item, haystack) in items {
            let hay = Utf32Str::new(haystack, &mut hay_buf);
            if let Some(scored) = score_with_bonuses(&pattern, haystack, hay, matcher, &mut scratch)
            {
                out.push(RankedMatch {
                    item,
                    haystack,
                    score: scored.score,
                    matched_indices: scored.indices,
                });
            }
        }

        out
    });

    Some(out)
}

/// Matches ordered best-first, indexed only as deep as `indexed`.
///
/// See [`rank_indexing_best`], which produces this.
pub(crate) struct Ranked<'a, T> {
    pub(crate) matches: Vec<RankedMatch<'a, T>>,
    /// How many leading matches carry `matched_indices`. Past this the field is
    /// empty because it was never computed, which a caller deep enough to paint
    /// those rows answers with [`indices_of`].
    pub(crate) indexed: usize,
}

/// Scores every candidate but only indexes the `indexed` best, returning them
/// already ordered best-first.
///
/// Deriving matched indices is the score-matrix traceback plus a vector per
/// atom, and a list far longer than its viewport spends nearly all of that on
/// rows nobody sees. Scoring alone decides what matches, so the traceback is
/// held back for the rows that can lead the list.
///
/// The ordering is exact down to `indexed` and approximate below it. Two of the
/// bonuses need the indices, so rows past the block are ranked on their raw
/// score, and one whose bonuses would have lifted it just inside the block
/// stays just outside. The bonuses are bounded, so this only ever reshuffles
/// rows across that boundary.
///
/// Ordering is otherwise [`sort_ranked`]'s. This sorts rather than leaving that
/// to the caller because `indexed` counts positions, and a caller's own
/// tie-break could move an unindexed row above it.
///
/// See also:
/// - [`match_and_rank`] for the unbounded form, which indexes every match.
/// - [`indices_of`] to derive one row's indices after the fact.
pub(crate) fn rank_indexing_best<'a, T>(
    query: &str,
    items: impl IntoIterator<Item = (T, &'a str)>,
    indexed: usize,
) -> Option<Ranked<'a, T>> {
    let pattern = parse_query(query)?;
    let (scored, indexed) = with_matcher(|matcher| {
        let mut hay_buf: Vec<char> = Vec::new();

        let mut scored: Vec<RankedMatch<'a, T>> = Vec::new();
        for (item, haystack) in items {
            let hay = Utf32Str::new(haystack, &mut hay_buf);
            if let Some(score) = pattern.score(hay, matcher) {
                scored.push(RankedMatch {
                    item,
                    haystack,
                    score,
                    matched_indices: Vec::new(),
                });
            }
        }

        // Ordering the whole set by raw score first is what makes the block a
        // deterministic set of rows rather than whichever equal-scoring ones a
        // partition happened to leave in front.
        sort_ranked(&mut scored);

        let indexed = indexed.min(scored.len());
        let mut buffers = Scratch::default();
        for ranked in &mut scored[..indexed] {
            let hay = Utf32Str::new(ranked.haystack, &mut hay_buf);
            let Some(with_bonuses) =
                score_with_bonuses(&pattern, ranked.haystack, hay, matcher, &mut buffers)
            else {
                continue;
            };
            ranked.score = with_bonuses.score;
            ranked.matched_indices = with_bonuses.indices;
        }
        sort_ranked(&mut scored[..indexed]);

        (scored, indexed)
    });

    Some(Ranked {
        matches: scored,
        indexed,
    })
}

/// Matched offsets for one haystack, into `out`.
///
/// Answers for a single row what [`rank_indexing_best`] holds back below its
/// block. `out` is cleared first and is meant to be one buffer reused across a
/// painted window. Returns whether `haystack` matched at all.
pub(crate) fn indices_of(query: &str, haystack: &str, out: &mut Vec<u32>) -> bool {
    out.clear();
    let Some(pattern) = parse_query(query) else {
        return false;
    };

    with_matcher(|matcher| {
        let mut hay_buf: Vec<char> = Vec::new();
        let hay = Utf32Str::new(haystack, &mut hay_buf);
        let mut scratch = Scratch::default();

        match score_with_bonuses(&pattern, haystack, hay, matcher, &mut scratch) {
            Some(scored) => {
                *out = scored.indices;
                true
            },
            None => false,
        }
    })
}

/// Order matches best-first, breaking ties alphabetically by haystack.
///
/// This is the ordering a fuzzy result list wants when it has nothing to say
/// about its own candidates. Alphabetical ties are what stop equally-scored
/// rows from reshuffling between keystrokes, which is far more distracting than
/// whatever order they land in.
///
/// A list that ranks by something of its own -- the palette's command priority,
/// the workspace picker's insertion index -- sorts for itself instead.
pub(crate) fn sort_ranked<T>(matches: &mut [RankedMatch<'_, T>]) {
    matches.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| a.haystack.cmp(b.haystack))
    });
}

struct Scored {
    score: u32,
    indices: Vec<u32>,
}

/// Index buffers [`score_with_bonuses`] reuses across candidates.
///
/// Most candidates fail to match, and each one would otherwise allocate a
/// vector per query atom before being discarded. The buffers are cleared rather
/// than replaced so their capacity survives, and only a candidate that actually
/// matches takes one away.
#[derive(Default)]
struct Scratch {
    /// Matched indices per query atom. The atom count is fixed for a whole
    /// [`match_and_rank`] call, so this settles on the first candidate.
    per_atom: Vec<Vec<u32>>,
    /// Every atom's indices merged, which a match carries off as its own.
    combined: Vec<u32>,
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
    scratch: &mut Scratch,
) -> Option<Scored> {
    scratch.per_atom.resize_with(pattern.atoms.len(), Vec::new);
    for atom_indices in &mut scratch.per_atom {
        atom_indices.clear();
    }
    scratch.combined.clear();

    let mut total_score: u32 = 0;
    for (atom, atom_indices) in pattern.atoms.iter().zip(&mut scratch.per_atom) {
        let score = atom.indices(haystack, matcher, atom_indices)?;
        total_score = total_score.saturating_add(u32::from(score));
        atom_indices.sort_unstable();
        atom_indices.dedup();
    }

    if is_in_order(&scratch.per_atom) {
        total_score = total_score.saturating_mul(5) / 4;
    }

    scratch
        .combined
        .extend(scratch.per_atom.iter().flatten().copied());
    scratch.combined.sort_unstable();
    scratch.combined.dedup();

    if all_in_basename(&scratch.combined, haystack_str) {
        total_score = total_score.saturating_add(BASENAME_BONUS);
    }

    // The match owns its indices from here, so the buffer it grew is handed
    // over rather than copied. The next candidate starts from a fresh one.
    Some(Scored {
        score: total_score,
        indices: std::mem::take(&mut scratch.combined),
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
        let items = vec![(0usize, "foo.rs")];
        assert!(match_and_rank("", items).is_none());
    }

    /// The index buffers are reused across candidates, so a candidate that
    /// records indices for one atom and then fails on a later one leaves them
    /// behind. What it left must not reach the candidate after it.
    #[test]
    fn a_rejected_candidate_leaves_nothing_behind_for_the_next() {
        let after_partial =
            match_and_rank("foo bar", vec![(0usize, "foo zzz"), (1usize, "xfoo bar")])
                .expect("query has atoms");
        let alone = match_and_rank("foo bar", vec![(1usize, "xfoo bar")]).expect("query has atoms");

        assert_eq!(after_partial.len(), 1, "only the second candidate matches");
        assert_eq!(after_partial[0].item, 1);
        assert_eq!(
            after_partial[0].matched_indices, alone[0].matched_indices,
            "the rejected candidate's indices must not survive into this one",
        );
        assert_eq!(
            after_partial[0].score, alone[0].score,
            "and its score must be the same as if it were scored on its own",
        );
    }

    #[test]
    fn match_and_rank_returns_matched_indices_sorted_and_deduped() {
        let items = vec![(0usize, "foo.rs")];
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
        let items = vec![(0usize, "foo.rs"), (1usize, "bar.rs")];
        let results = match_and_rank("foo", items).expect("query has atoms");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].item, 0);
    }

    #[test]
    fn match_and_rank_smart_case_lowercase_query_is_insensitive() {
        let items = vec![(0usize, "Foo.rs")];
        let results = match_and_rank("foo", items).expect("query has atoms");
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn match_and_rank_smart_case_uppercase_query_is_sensitive() {
        let items = vec![(0usize, "Foo.rs"), (1usize, "foo.rs")];
        let results = match_and_rank("F", items).expect("query has atoms");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].item, 0);
    }

    #[test]
    fn match_and_rank_multi_token_matches_in_either_order() {
        let items = vec![(0usize, "src/foo.rs")];
        let forward = match_and_rank(".rs foo", items.clone()).expect("query has atoms");
        let reverse = match_and_rank("foo .rs", items).expect("query has atoms");
        assert_eq!(forward.len(), 1);
        assert_eq!(reverse.len(), 1);
    }

    #[test]
    fn match_and_rank_in_order_query_outscores_reversed() {
        let items = vec![(0usize, "src/foo.rs")];
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
        let items = vec![(0usize, "foo.rs")];
        let bonus = match_and_rank("foo", items).expect("query has atoms");
        assert_eq!(bonus.len(), 1);
        assert!(bonus[0].score > 0);
    }

    #[test]
    fn match_and_rank_basename_match_outscores_directory_prefix() {
        let items = vec![(0usize, "src/foo.rs"), (1usize, "foo_helpers/util.rs")];
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
        let items = vec![(0usize, "src/foo.rs")];
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
        let items = vec![(0usize, "QuitAll")];
        let results = match_and_rank("quit", items).expect("query has atoms");
        assert_eq!(results.len(), 1);
        assert!(results[0].score > 0);
    }

    fn haystacks() -> Vec<String> {
        let mut out = Vec::new();
        for dir in ["src", "srv", "tests/deep"] {
            for stem in ["main", "lib", "loader", "read_me"] {
                for ext in ["rs", "md"] {
                    out.push(format!("{dir}/{stem}.{ext}"));
                }
            }
        }
        out
    }

    fn as_rows(haystacks: &[String]) -> Vec<(usize, &str)> {
        haystacks
            .iter()
            .enumerate()
            .map(|(i, h)| (i, h.as_str()))
            .collect()
    }

    fn shape(matches: &[RankedMatch<'_, usize>]) -> Vec<(usize, u32, Vec<u32>)> {
        matches
            .iter()
            .map(|m| (m.item, m.score, m.matched_indices.clone()))
            .collect()
    }

    #[test]
    fn indexing_every_row_ranks_as_the_unbounded_path_does() {
        let haystacks = haystacks();

        for query in ["ma", "main rs", "^src", "'lib", "read me"] {
            let mut unbounded =
                match_and_rank(query, as_rows(&haystacks)).expect("the query has atoms");
            sort_ranked(&mut unbounded);

            let capped = rank_indexing_best(query, as_rows(&haystacks), usize::MAX)
                .expect("the query has atoms");

            assert_eq!(
                capped.indexed,
                unbounded.len(),
                "every match is indexed for {query:?}"
            );
            assert_eq!(
                shape(&capped.matches),
                shape(&unbounded),
                "ranking or highlights differ for {query:?}"
            );
        }
    }

    #[test]
    fn the_block_leads_the_list_and_carries_the_bonuses() {
        // One row whose basename is the query outright, buried among many that
        // match it only as scattered characters across a directory prefix.
        let mut haystacks: Vec<String> = (0..600).map(|i| format!("m{i:04}/a-i-n/x.rs")).collect();
        haystacks.push("other/main.rs".to_string());

        let ranked =
            rank_indexing_best("main", as_rows(&haystacks), 512).expect("the query has atoms");

        assert_eq!(ranked.indexed, 512, "the block caps at what was asked for");
        assert_eq!(
            ranked.matches[0].item,
            haystacks.len() - 1,
            "the basename match leads, so the bonuses reached it"
        );
        assert!(
            !ranked.matches[0].matched_indices.is_empty(),
            "and it carries its offsets"
        );
        assert!(
            ranked.matches[512..]
                .iter()
                .all(|m| m.matched_indices.is_empty()),
            "rows past the block carry none"
        );
    }

    #[test]
    fn the_block_is_reordered_once_its_bonuses_are_known() {
        // The directory match scores better on the raw pass, matching at the
        // very start, but only the basename match earns the basename bonus, so
        // knowing the indices reverses them.
        let haystacks = ["abc/z.rs".to_string(), "z/qabc.rs".to_string()];

        let bonused: Vec<u32> = as_rows(&haystacks)
            .into_iter()
            .map(|(_, h)| {
                let mut one = match_and_rank("abc", std::iter::once((0usize, h)))
                    .expect("the query has atoms");
                one.pop().expect("both match").score
            })
            .collect();
        assert!(
            bonused[1] > bonused[0],
            "the basename match wins on bonused score, which is the order to reach"
        );

        let ranked =
            rank_indexing_best("abc", as_rows(&haystacks), 512).expect("the query has atoms");
        assert_eq!(
            ranked.matches[0].item, 1,
            "the block is ranked on its bonused scores, not the raw ones it was chosen by"
        );
    }

    #[test]
    fn a_row_past_the_block_derives_the_offsets_it_would_have_stored() {
        let haystacks = haystacks();
        let ranked = rank_indexing_best("ma", as_rows(&haystacks), 1).expect("the query has atoms");

        let past = &ranked.matches[1];
        assert!(
            past.matched_indices.is_empty(),
            "the fixture has to reach past the block"
        );

        let mut derived = Vec::new();
        assert!(indices_of("ma", past.haystack, &mut derived));

        let mut unbounded = match_and_rank("ma", std::iter::once((0usize, past.haystack)))
            .expect("the query has atoms");
        let stored = unbounded.pop().expect("the row matches").matched_indices;

        assert_eq!(
            derived, stored,
            "deriving late gives what indexing early would"
        );
    }
}
