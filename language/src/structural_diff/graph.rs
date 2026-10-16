//! Edit-graph primitives for the structural-diff Dijkstra search.
//!
//! The search itself lives in [`super::dijkstra`]. This module owns the
//! pieces the search builds on. Those are the [`Edge`] type and its cost
//! model, the [`EnteredDelimiter`] stack that tracks which lists each side
//! has descended into, the parent-stack helpers ([`pop_all_parents`],
//! [`push_lhs_delimiter`], [`push_rhs_delimiter`]), and the atom
//! predicates that classify punctuation, comments, and strings.
//!
//! Reference: `references/difftastic/src/diff/graph.rs`. The cost values
//! and delimiter bookkeeping are ported from there. See the in-code
//! citations.
//!
//! The [`EnteredDelimiter::PopBoth`] vs [`EnteredDelimiter::PopEither`]
//! distinction is load-bearing. Without `PopBoth`, structural changes
//! like `(a b c)` vs `(a b) c` are missed, and [`pop_all_parents`]
//! encodes the matching list-ending semantics.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    line_diff,
    stack::Stack,
};
use std::cmp::min;

/// The byte length above which a comment or a string pair measures its
/// similarity by lines rather than by chars.
///
/// [`levenshtein_pct`] fills a table of one cell per char pair, so the cap keeps
/// one vertex expansion under about 16 million cells. YAML block scalars,
/// markdown paragraphs, and doc comments run long enough to pass it. Past it,
/// [`similarity_pct`] counts the lines the two texts share, which stays linear
/// in the text.
const MAX_SIMILARITY_BYTES: usize = 4096;

/// One delimiter pair we have descended into. `PopBoth` means both
/// sides entered the same matching list together (the ideal case);
/// `PopEither` means LHS and RHS entered separate, mismatched lists
/// and each can pop independently.
#[derive(Clone, Debug)]
pub enum EnteredDelimiter {
    /// Both sides entered a matching list. The two payloads are the
    /// LHS and RHS list nodes that were entered together.
    PopBoth(SyntaxId, SyntaxId),
    /// LHS and RHS each have a persistent stack of unmatched delimiters
    /// they have descended into independently. Each side can pop on its
    /// own, and consecutive same-side descents share the tail rather than
    /// nesting a fresh frame.
    PopEither {
        lhs_delims: Stack<SyntaxId>,
        rhs_delims: Stack<SyntaxId>,
    },
}

impl PartialEq for EnteredDelimiter {
    fn eq(&self, other: &Self) -> bool {
        // Equality compares only the kind and the top lhs/rhs delim ids
        // of each substack, never the deeper frames. This keeps it in
        // step with the shallow key the search dedups vertices by (see
        // `TopKey` in `super::dijkstra`).
        match (self, other) {
            (EnteredDelimiter::PopBoth(a1, b1), EnteredDelimiter::PopBoth(a2, b2)) => {
                a1 == a2 && b1 == b2
            },
            (
                EnteredDelimiter::PopEither {
                    lhs_delims: a1,
                    rhs_delims: b1,
                },
                EnteredDelimiter::PopEither {
                    lhs_delims: a2,
                    rhs_delims: b2,
                },
            ) => a1.peek() == a2.peek() && b1.peek() == b2.peek(),
            _ => false,
        }
    }
}

impl Eq for EnteredDelimiter {}

/// One outgoing edge from a diff-graph vertex.
#[derive(Clone, Copy, Debug)]
pub enum Edge {
    /// Both sides have a node with the same `ContentId`. Lowest cost.
    /// `probably_punctuation` adds +200 to the cost so structural
    /// matches (variable names, keywords) win against incidental
    /// matches (commas, semicolons).
    UnchangedNode {
        depth_difference: u32,
        probably_punctuation: bool,
    },
    /// Both sides have a list with matching delimiters; the children
    /// will be diffed inside. Cheap but not free.
    EnterUnchangedDelimiter { depth_difference: u32 },
    /// Both sides have a comment-kind atom whose contents are
    /// similar, by [`similarity_pct`]. The replacement is preferred
    /// over two separate Novel edges when similarity is above 20%.
    /// Cost is 500 + (100 - similarity_pct), so range is 500-600.
    ReplacedComment { similarity_pct: u8 },
    /// Same as [`Edge::ReplacedComment`] but for string-kind atoms.
    ReplacedString { similarity_pct: u8 },
    /// LHS has a leaf with no counterpart on RHS.
    NovelAtomLHS,
    /// RHS has a leaf with no counterpart on LHS.
    NovelAtomRHS,
    /// LHS has a list whose delimiters do not match anything on RHS.
    EnterNovelDelimiterLHS,
    /// RHS has a list whose delimiters do not match anything on LHS.
    EnterNovelDelimiterRHS,
}

impl Edge {
    /// Cost values are ported verbatim from
    /// `references/difftastic/src/diff/graph.rs:316-360`.
    pub fn cost(self) -> u32 {
        match self {
            Edge::UnchangedNode {
                depth_difference,
                probably_punctuation,
            } => {
                let base = min(40, depth_difference + 1);
                base + if probably_punctuation { 200 } else { 0 }
            },
            Edge::EnterUnchangedDelimiter { depth_difference } => 100 + min(40, depth_difference),
            Edge::NovelAtomLHS | Edge::NovelAtomRHS => 300,
            Edge::EnterNovelDelimiterLHS | Edge::EnterNovelDelimiterRHS => 300,
            Edge::ReplacedComment { similarity_pct } | Edge::ReplacedString { similarity_pct } => {
                500 + u32::from(100 - similarity_pct)
            },
        }
    }
}

/// Heuristic: a node is "probably punctuation" if its kind contains
/// `punctuation` (the standard tree-sitter scope name) or if its
/// content is a single non-alphanumeric ASCII character. The exact
/// definition matches Difftastic closely enough that the +200
/// penalty has the intended effect (variable matches win over comma
/// matches).
pub fn probably_punctuation(arena: &SyntaxArena, node: &Syntax) -> bool {
    match node {
        Syntax::Atom(atom) => {
            let kind = atom.kind;
            if kind.contains("punctuation") {
                return true;
            }
            let trimmed = arena.text(atom).trim();
            // Single ASCII char that isn't alphanumeric: probably a
            // delimiter or operator.
            trimmed.len() == 1
                && trimmed
                    .as_bytes()
                    .first()
                    .is_some_and(|b| !b.is_ascii_alphanumeric() && b.is_ascii())
        },
        Syntax::List(_) => false,
    }
}

/// Detect a comment-kind atom. Tree-sitter grammars conventionally
/// name comment nodes with `comment` somewhere in the kind.
pub fn is_comment_atom(node: &Syntax) -> bool {
    match node {
        Syntax::Atom(atom) => atom.kind.contains("comment"),
        Syntax::List(_) => false,
    }
}

/// Detect a string-literal atom.
///
/// Tree-sitter grammars conventionally name string nodes with `string` or
/// `char` in the kind. tree-sitter-yaml names its quoted and block scalars
/// `double_quote_scalar`, `single_quote_scalar`, and `block_scalar`, and the
/// block grammar of tree-sitter-markdown holds the text of a paragraph or a
/// heading in one `inline` leaf, so those kinds count too.
pub fn is_string_atom(node: &Syntax) -> bool {
    match node {
        Syntax::Atom(atom) => {
            atom.kind.contains("string")
                || atom.kind.contains("char")
                || matches!(
                    atom.kind,
                    "double_quote_scalar" | "single_quote_scalar" | "block_scalar" | "inline"
                )
        },
        Syntax::List(_) => false,
    }
}

/// Whether a comment or a string pair goes on to [`similarity_pct`].
///
/// The similarity never passes min(chars) / max(chars). A pair at or under 20
/// percent by char count, which a replacement edge rejects anyway, therefore
/// skips the test. A floor over byte lengths is not exact, because a multibyte
/// char counts once in the similarity and several times in the length.
pub fn similarity_possible(a: &str, b: &str) -> bool {
    let (a_chars, b_chars) = (a.chars().count(), b.chars().count());
    let (shorter, longer) = (a_chars.min(b_chars), a_chars.max(b_chars));
    longer == 0 || shorter * 100 > 20 * longer
}

/// The similarity of a comment or a string pair as a percentage, 0 to 100.
///
/// A pair at or under [`MAX_SIMILARITY_BYTES`] measures by [`levenshtein_pct`].
/// A longer pair measures by lines. Its similarity is the bytes of the lines
/// that [`line_diff::align_lines`] pairs with a line of the same words,
/// newlines included, over the longer side's bytes. So a long doc comment that
/// kept most of its lines stays one replacement, at a cost linear in its text.
///
/// A one-line pair past the cap shares no line unless the two are equal, so it
/// measures 0 however few chars changed.
pub fn similarity_pct(a: &str, b: &str) -> u8 {
    let longer = a.len().max(b.len());
    if longer <= MAX_SIMILARITY_BYTES {
        return levenshtein_pct(a, b);
    }

    let (a_lines, b_lines): (Vec<&str>, Vec<&str>) = (
        a.split_inclusive('\n').collect(),
        b.split_inclusive('\n').collect(),
    );
    let shared: usize = line_diff::align_lines(&a_lines, &b_lines)
        .into_iter()
        .filter_map(|pair| match pair {
            (Some(i), Some(j))
                if a_lines[i]
                    .split_whitespace()
                    .eq(b_lines[j].split_whitespace()) =>
            {
                Some(a_lines[i].len())
            },
            _ => None,
        })
        .sum();
    (shared * 100 / longer) as u8
}

/// Compute a Levenshtein-distance-based similarity percentage
/// between two strings. Range: 0..=100. Two empty strings return
/// 100. The implementation is the classic O(n*m) DP, plenty fast for
/// the comment-/string-sized inputs the structural-diff sees.
pub fn levenshtein_pct(a: &str, b: &str) -> u8 {
    if a.is_empty() && b.is_empty() {
        return 100;
    }
    let b_chars: Vec<char> = b.chars().collect();
    let n = a.chars().count();
    let m = b_chars.len();
    if n == 0 || m == 0 {
        return 0;
    }
    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr: Vec<usize> = vec![0; m + 1];
    for (row, a_char) in a.chars().enumerate() {
        let i = row + 1;
        curr[0] = i;
        for j in 1..=m {
            let cost = if a_char == b_chars[j - 1] { 0 } else { 1 };
            curr[j] = (curr[j - 1] + 1).min(prev[j] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    let distance = prev[m];
    let max_len = n.max(m);
    let similarity = (max_len.saturating_sub(distance) * 100) / max_len;
    similarity.min(100) as u8
}

/// Pop empty list parents. After matching or skipping a node, both
/// pointers may have moved past the end of their current sibling
/// list; this routine pops back up through the parents stack until
/// at least one side has a remaining node, or the stack is empty.
///
/// Returns `(new_lhs_pos, new_rhs_pos, new_parents_stack)`.
///
/// Mirrors `references/difftastic/src/diff/graph.rs:425-489`
/// `pop_all_parents`. The Difftastic version also tracks
/// `lhs_parent_id` / `rhs_parent_id` separately; we fold those into
/// the `EnteredDelimiter` payloads.
pub fn pop_all_parents(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_pos: Option<SyntaxId>,
    rhs_pos: Option<SyntaxId>,
    parents: Stack<EnteredDelimiter>,
) -> (Option<SyntaxId>, Option<SyntaxId>, Stack<EnteredDelimiter>) {
    let mut lhs = lhs_pos;
    let mut rhs = rhs_pos;
    let mut stack = parents;
    loop {
        // If both sides still have a position, no parent can be popped.
        if lhs.is_some() && rhs.is_some() {
            break;
        }
        // When both sides are exhausted under a PopBoth, pop the pair together
        // and resume at each parent's next sibling. They entered together, so
        // they must exit together -- asymmetric exhaustion stops the chain.
        if lhs.is_none()
            && rhs.is_none()
            && let Some((lhs_parent, rhs_parent, rest)) = try_pop_both(&stack)
        {
            lhs = lhs_arena.get(lhs_parent).next_sibling();
            rhs = rhs_arena.get(rhs_parent).next_sibling();
            stack = rest;
            continue;
        }
        // Otherwise pop the exhausted side(s) from a PopEither, each side
        // independently.
        let mut popped_anything = false;
        if lhs.is_none()
            && let Some((lhs_parent, rest)) = try_pop_lhs(&stack)
        {
            lhs = lhs_arena.get(lhs_parent).next_sibling();
            stack = rest;
            popped_anything = true;
        }
        if rhs.is_none()
            && let Some((rhs_parent, rest)) = try_pop_rhs(&stack)
        {
            rhs = rhs_arena.get(rhs_parent).next_sibling();
            stack = rest;
            popped_anything = true;
        }
        if !popped_anything {
            break;
        }
    }
    (lhs, rhs, stack)
}

/// If the top of `parents` is a [`EnteredDelimiter::PopBoth`], return its two
/// delimiters and the stack with that frame popped.
fn try_pop_both(
    parents: &Stack<EnteredDelimiter>,
) -> Option<(SyntaxId, SyntaxId, Stack<EnteredDelimiter>)> {
    match parents.peek() {
        Some(EnteredDelimiter::PopBoth(lhs, rhs)) => {
            let (_, rest) = parents.pop().expect("peek returned Some");
            Some((*lhs, *rhs, rest))
        },
        _ => None,
    }
}

/// If the top of `parents` is a [`EnteredDelimiter::PopEither`] with a non-empty
/// LHS delimiter stack, pop its top LHS delimiter and return it with the updated
/// parents stack. The RHS stack rides along unchanged.
fn try_pop_lhs(parents: &Stack<EnteredDelimiter>) -> Option<(SyntaxId, Stack<EnteredDelimiter>)> {
    let Some(EnteredDelimiter::PopEither {
        lhs_delims,
        rhs_delims,
    }) = parents.peek()
    else {
        return None;
    };
    let (lhs_delim, new_lhs_delims) = lhs_delims.pop()?;
    let (_, mut rest) = parents.pop().expect("peek returned Some");
    if !new_lhs_delims.is_empty() || !rhs_delims.is_empty() {
        rest = rest.push(EnteredDelimiter::PopEither {
            lhs_delims: new_lhs_delims,
            rhs_delims: rhs_delims.clone(),
        });
    }
    Some((*lhs_delim, rest))
}

/// The RHS counterpart of [`try_pop_lhs`].
fn try_pop_rhs(parents: &Stack<EnteredDelimiter>) -> Option<(SyntaxId, Stack<EnteredDelimiter>)> {
    let Some(EnteredDelimiter::PopEither {
        lhs_delims,
        rhs_delims,
    }) = parents.peek()
    else {
        return None;
    };
    let (rhs_delim, new_rhs_delims) = rhs_delims.pop()?;
    let (_, mut rest) = parents.pop().expect("peek returned Some");
    if !lhs_delims.is_empty() || !new_rhs_delims.is_empty() {
        rest = rest.push(EnteredDelimiter::PopEither {
            lhs_delims: lhs_delims.clone(),
            rhs_delims: new_rhs_delims,
        });
    }
    Some((*rhs_delim, rest))
}

/// Record descending into an LHS-only novel list. Extends the top
/// [`EnteredDelimiter::PopEither`] frame's LHS stack when present, so
/// consecutive LHS descents share one frame. Otherwise it pushes a fresh frame.
pub(crate) fn push_lhs_delimiter(
    parents: &Stack<EnteredDelimiter>,
    delimiter: SyntaxId,
) -> Stack<EnteredDelimiter> {
    match parents.peek() {
        Some(EnteredDelimiter::PopEither {
            lhs_delims,
            rhs_delims,
        }) => {
            let lhs_delims = lhs_delims.push(delimiter);
            let rhs_delims = rhs_delims.clone();
            let (_, rest) = parents.pop().expect("peek returned Some");
            rest.push(EnteredDelimiter::PopEither {
                lhs_delims,
                rhs_delims,
            })
        },
        _ => parents.push(EnteredDelimiter::PopEither {
            lhs_delims: Stack::new().push(delimiter),
            rhs_delims: Stack::new(),
        }),
    }
}

/// The RHS counterpart of [`push_lhs_delimiter`].
pub(crate) fn push_rhs_delimiter(
    parents: &Stack<EnteredDelimiter>,
    delimiter: SyntaxId,
) -> Stack<EnteredDelimiter> {
    match parents.peek() {
        Some(EnteredDelimiter::PopEither {
            lhs_delims,
            rhs_delims,
        }) => {
            let lhs_delims = lhs_delims.clone();
            let rhs_delims = rhs_delims.push(delimiter);
            let (_, rest) = parents.pop().expect("peek returned Some");
            rest.push(EnteredDelimiter::PopEither {
                lhs_delims,
                rhs_delims,
            })
        },
        _ => parents.push(EnteredDelimiter::PopEither {
            lhs_delims: Stack::new(),
            rhs_delims: Stack::new().push(delimiter),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_costs_are_ordered_correctly() {
        // Unchanged should always beat enter-unchanged-delim, which
        // should always beat novel.
        assert!(
            Edge::UnchangedNode {
                depth_difference: 0,
                probably_punctuation: false,
            }
            .cost()
                < Edge::EnterUnchangedDelimiter {
                    depth_difference: 0
                }
                .cost()
        );
        assert!(
            Edge::EnterUnchangedDelimiter {
                depth_difference: 0
            }
            .cost()
                < Edge::NovelAtomLHS.cost()
        );
        assert_eq!(Edge::NovelAtomLHS.cost(), Edge::NovelAtomRHS.cost());
        assert_eq!(
            Edge::EnterNovelDelimiterLHS.cost(),
            Edge::EnterNovelDelimiterRHS.cost()
        );
    }

    #[test]
    fn punctuation_penalty_makes_unchanged_pun_more_expensive_than_unchanged_var() {
        let var = Edge::UnchangedNode {
            depth_difference: 0,
            probably_punctuation: false,
        };
        let pun = Edge::UnchangedNode {
            depth_difference: 0,
            probably_punctuation: true,
        };
        assert_eq!(pun.cost(), var.cost() + 200);
    }

    #[test]
    fn levenshtein_pct_basic_cases() {
        assert_eq!(levenshtein_pct("", ""), 100);
        assert_eq!(levenshtein_pct("abc", "abc"), 100);
        assert_eq!(levenshtein_pct("", "abc"), 0);
        assert_eq!(levenshtein_pct("abc", ""), 0);
        // "kitten" -> "sitting" is the textbook 3-edit example, so
        // similarity = (7 - 3) / 7 = ~57%.
        let pct = levenshtein_pct("kitten", "sitting");
        assert!((50..=70).contains(&pct), "got {pct}");
    }

    #[test]
    fn yaml_scalar_and_markdown_inline_kinds_are_string_atoms() {
        let mut arena = SyntaxArena::default();
        let kinds = [
            "double_quote_scalar",
            "single_quote_scalar",
            "block_scalar",
            "inline",
            "plain_scalar",
        ];

        let strings = kinds.map(|kind| {
            let id = arena.alloc_atom(kind, "text");
            is_string_atom(arena.get(id))
        });
        assert_eq!(
            strings,
            [true, true, true, true, false],
            "the quoted and block scalars and markdown's inline are strings, a plain scalar is not",
        );
    }

    #[test]
    fn similarity_is_impossible_under_the_length_floor() {
        let long = "x".repeat(MAX_SIMILARITY_BYTES + 1);
        let multibyte = "ab\u{e9}\u{e9}\u{e9}\u{e9}";

        assert_eq!(
            [
                similarity_possible("abcde", "a"),
                similarity_possible("abcd", "a"),
                similarity_possible(multibyte, "ab"),
                similarity_possible(&long, &long),
                similarity_possible("", ""),
            ],
            [false, true, true, true, true],
            "one char in five sits on the floor, one in four passes it, two in six chars pass \
             it though two in ten bytes do not, a pair past the cap goes on to the line \
             measure, two empty sides stay",
        );
        assert!(
            levenshtein_pct(multibyte, "ab") > 20,
            "the search takes the multibyte pair, so the floor must count chars",
        );
    }

    /// Past the cap a pair measures by the lines it shares, a reindented line
    /// among them. One line past the cap shares nothing however close its chars
    /// sit. Under the cap the measure stays the char one.
    #[test]
    fn a_pair_past_the_cap_measures_its_similarity_by_lines() {
        let line = |i: usize, fill: &str| format!("{i:04}{}\n", fill.repeat(59));
        let lhs: String = (0..100).map(|i| line(i, "x")).collect();
        let rhs: String = (0..100)
            .map(|i| match i {
                42 => line(i, "y"),
                77 => format!("    {}", line(i, "x")),
                _ => line(i, "x"),
            })
            .collect();
        let one_line = "x".repeat(MAX_SIMILARITY_BYTES + 1);

        assert_eq!(
            [
                similarity_pct(&lhs, &rhs),
                similarity_pct(&lhs, &lhs.replace('x', "z")),
                similarity_pct(&one_line, &format!("{one_line}y")),
                similarity_pct("kitten", "sitting"),
            ],
            [98, 0, 0, levenshtein_pct("kitten", "sitting")],
            "99 shared lines of 64 bytes over 6,404, no shared line, one line past the cap, \
             and the char measure under it",
        );
    }

    #[test]
    fn replaced_comment_cost_in_500_to_600_range() {
        let edge = Edge::ReplacedComment { similarity_pct: 80 };
        let cost = edge.cost();
        assert!((500..=600).contains(&cost), "got cost {cost}");
        // High similarity should produce a lower cost.
        let high_sim = Edge::ReplacedComment { similarity_pct: 95 }.cost();
        let low_sim = Edge::ReplacedComment { similarity_pct: 30 }.cost();
        assert!(high_sim < low_sim);
    }

    #[test]
    fn probably_punctuation_detects_single_chars() {
        let mut arena = SyntaxArena::default();
        let comma = arena.alloc_atom("punctuation.delimiter", ",");
        let ident = arena.alloc_atom("identifier", "alpha");
        assert!(probably_punctuation(&arena, arena.get(comma)));
        assert!(!probably_punctuation(&arena, arena.get(ident)));
    }
}
