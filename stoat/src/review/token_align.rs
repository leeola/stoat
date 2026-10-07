//! Row alignment by the tokens a tree diff holds unchanged.
//!
//! A line-text walk pairs a reformatted line with whatever line holds the same
//! text, which is often none, so one reformat shifts every row after it. A
//! token keeps its identity when it moves to another line, so pairing lines by
//! the tokens they share keeps the rows aligned. The steps follow the
//! side-by-side display of difftastic.

use super::{line_byte_offsets, RowPlan, WalkSide};
use std::{cmp::Ordering, collections::VecDeque, ops::Range};
use stoat_language::structural_diff::MatchedPair;

/// One row's line index on each side, `None` where the row leaves that side
/// empty.
type LinePair = (Option<usize>, Option<usize>);

/// Plan the rows of a diff whose tree diff matched tokens, one row per aligned
/// line pair.
///
/// Every line of each side appears in exactly one row, in order. The merge view
/// pairs two walks over one ancestor by their base lines, and relies on that.
pub(super) fn token_aligned_plans(
    lhs: &WalkSide<'_>,
    rhs: &WalkSide<'_>,
    matched: &[MatchedPair],
) -> Vec<RowPlan> {
    let anchors = anchor_lines(
        matched,
        &line_byte_offsets(lhs.lines),
        &line_byte_offsets(rhs.lines),
    );
    let merged = {
        let with_rhs = merge_in_opposite_lines(anchors, &token_lines(rhs.lines));
        flip(merge_in_opposite_lines(
            flip(with_rhs),
            &token_lines(lhs.lines),
        ))
    };

    let aligned = {
        let ended = add_ends(merged, lhs.lines.len(), rhs.lines.len());
        let blanks = match_preceding_blanks(&ended, lhs.lines, rhs.lines);
        compact_gaps(ensure_contiguous(&blanks))
    };
    debug_assert!(
        aligned
            .iter()
            .filter_map(|pair| pair.0)
            .eq(0..lhs.lines.len())
            && aligned
                .iter()
                .filter_map(|pair| pair.1)
                .eq(0..rhs.lines.len()),
        "every line of each side appears once, in order",
    );

    aligned
        .into_iter()
        .map(|pair| classify(pair, lhs, rhs))
        .collect()
}

/// The line pairs the matched tokens pin, walked in lhs order.
///
/// A pair is kept only when it lies past the last kept pair on both sides, so
/// the anchors never cross. A token that spans several lines anchors each of
/// its lines, against the first line of its counterpart past the last anchor.
fn anchor_lines(
    matched: &[MatchedPair],
    lhs_offsets: &[(usize, usize)],
    rhs_offsets: &[(usize, usize)],
) -> Vec<LinePair> {
    let mut anchors = Vec::new();
    let mut last: Option<(usize, usize)> = None;
    for pair in matched {
        let rhs_lines = lines_of(rhs_offsets, &pair.rhs);
        for lhs_line in lines_of(lhs_offsets, &pair.lhs) {
            if last.is_some_and(|(lhs_last, _)| lhs_line <= lhs_last) {
                continue;
            }
            let rhs_line = match last {
                Some((_, rhs_last)) => rhs_lines.clone().find(|&line| line > rhs_last),
                None => Some(rhs_lines.start),
            };
            if let Some(rhs_line) = rhs_line {
                anchors.push((Some(lhs_line), Some(rhs_line)));
                last = Some((lhs_line, rhs_line));
            }
        }
    }
    anchors
}

/// The lines a non-empty byte `range` covers, by the offsets
/// [`line_byte_offsets`] returns.
///
/// The newline after a line belongs to that line, so a token that ends with
/// its newline covers no line past its own.
fn lines_of(offsets: &[(usize, usize)], range: &Range<usize>) -> Range<usize> {
    let line_at = |byte: usize| offsets.partition_point(|&(_, end)| end < byte);
    line_at(range.start)..line_at(range.end - 1) + 1
}

/// The indices of the lines that hold a token, which are the lines with
/// anything but whitespace.
fn token_lines(lines: &[&str]) -> Vec<usize> {
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, _)| index)
        .collect()
}

/// Give every opposite-side line in `opposite_lines` that no pair pins a row
/// of its own, before the pair that follows it.
fn merge_in_opposite_lines(pairs: Vec<LinePair>, opposite_lines: &[usize]) -> Vec<LinePair> {
    let mut merged = Vec::with_capacity(pairs.len() + opposite_lines.len());
    let mut next = 0;
    for (line, opposite) in pairs {
        if let Some(opposite) = opposite {
            while let Some(&unpinned) = opposite_lines.get(next) {
                match unpinned.cmp(&opposite) {
                    Ordering::Less => merged.push((None, Some(unpinned))),
                    Ordering::Equal => {},
                    Ordering::Greater => break,
                }
                next += 1;
            }
        }
        merged.push((line, opposite));
    }
    merged.extend(
        opposite_lines[next..]
            .iter()
            .map(|&unpinned| (None, Some(unpinned))),
    );
    merged
}

/// Swap the two sides of every pair.
fn flip(mut pairs: Vec<LinePair>) -> Vec<LinePair> {
    for pair in &mut pairs {
        *pair = (pair.1, pair.0);
    }
    pairs
}

/// Extend `pairs` with the lines before its first line and after its last line
/// on each side.
///
/// A blank line holds no token, so leading and trailing blanks reach no pair
/// any other way.
fn add_ends(pairs: Vec<LinePair>, lhs_len: usize, rhs_len: usize) -> Vec<LinePair> {
    let lhs_first = pairs.iter().find_map(|pair| pair.0);
    let rhs_first = pairs.iter().find_map(|pair| pair.1);
    let lhs_last = pairs.iter().rev().find_map(|pair| pair.0);
    let rhs_last = pairs.iter().rev().find_map(|pair| pair.1);

    let mut ended = Vec::with_capacity(pairs.len());
    if let (Some(lhs_first), Some(rhs_first)) = (lhs_first, rhs_first) {
        ended.extend(by_position(0..lhs_first, 0..rhs_first));
    }
    ended.extend(pairs);
    if let (Some(lhs_last), Some(rhs_last)) = (lhs_last, rhs_last) {
        ended.extend(by_position(lhs_last + 1..lhs_len, rhs_last + 1..rhs_len));
    }
    ended
}

/// Pair two runs of lines by position, then the rest of the longer run alone.
fn by_position(lhs: Range<usize>, rhs: Range<usize>) -> impl Iterator<Item = LinePair> {
    let common = lhs.len().min(rhs.len());
    let paired = lhs.clone().zip(rhs.clone());
    let lhs_rest = lhs.skip(common);
    let rhs_rest = rhs.skip(common);

    paired
        .map(|(lhs, rhs)| (Some(lhs), Some(rhs)))
        .chain(lhs_rest.map(|lhs| (Some(lhs), None)))
        .chain(rhs_rest.map(|rhs| (None, Some(rhs))))
}

/// Pair the blank lines directly above each two-sided pair, on both sides at
/// once, back to the pair before it.
///
/// Nothing else pairs a blank line. Without this step, a removed block between
/// two blanks leaves the blank after it beside nothing.
fn match_preceding_blanks(pairs: &[LinePair], lhs: &[&str], rhs: &[&str]) -> Vec<LinePair> {
    let mut matched = Vec::with_capacity(pairs.len());
    let mut prev: LinePair = (None, None);
    for &pair in pairs {
        if let (Some(lhs_line), Some(rhs_line)) = pair {
            let blanks = match prev {
                (None, None) => blank_run_above(lhs_line, rhs_line, None, lhs, rhs),
                (Some(lhs_prev), Some(rhs_prev)) => {
                    blank_run_above(lhs_line, rhs_line, Some((lhs_prev, rhs_prev)), lhs, rhs)
                },
                // The ends start at line 0 on both sides, so a side with no
                // line before this pair is at its first line, with no blank
                // above to pair.
                _ => Vec::new(),
            };
            matched.extend(blanks);
        }
        matched.push(pair);
        prev = (pair.0.or(prev.0), pair.1.or(prev.1));
    }
    matched
}

/// The blank lines directly above `(lhs_line, rhs_line)`, paired upward while
/// both sides are blank and above `floor`, in top-down order.
fn blank_run_above(
    lhs_line: usize,
    rhs_line: usize,
    floor: Option<(usize, usize)>,
    lhs: &[&str],
    rhs: &[&str],
) -> Vec<LinePair> {
    let mut run = Vec::new();
    let (mut lhs_at, mut rhs_at) = (lhs_line, rhs_line);
    while lhs_at > 0
        && rhs_at > 0
        && floor
            .is_none_or(|(lhs_floor, rhs_floor)| lhs_at - 1 > lhs_floor && rhs_at - 1 > rhs_floor)
        && is_blank(lhs[lhs_at - 1])
        && is_blank(rhs[rhs_at - 1])
    {
        lhs_at -= 1;
        rhs_at -= 1;
        run.push((Some(lhs_at), Some(rhs_at)));
    }
    run.reverse();
    run
}

/// A line with no text, read past the carriage return a CRLF file leaves on it.
fn is_blank(line: &str) -> bool {
    line.trim_end_matches('\r').is_empty()
}

/// Fill each gap in either side's lines with the missing lines standing alone,
/// so every line between two listed ones gets a row.
fn ensure_contiguous(pairs: &[LinePair]) -> Vec<LinePair> {
    let mut contiguous = Vec::with_capacity(pairs.len());
    let (mut lhs_last, mut rhs_last): (Option<usize>, Option<usize>) = (None, None);
    for &(lhs_line, rhs_line) in pairs {
        if let Some(line) = lhs_line {
            if let Some(last) = lhs_last {
                contiguous.extend((last + 1..line).map(|gap| (Some(gap), None)));
            }
            lhs_last = Some(line);
        }
        if let Some(line) = rhs_line {
            if let Some(last) = rhs_last {
                contiguous.extend((last + 1..line).map(|gap| (None, Some(gap))));
            }
            rhs_last = Some(line);
        }
        contiguous.push((lhs_line, rhs_line));
    }
    contiguous
}

/// Pair each run of lines standing alone on one side with the lines standing
/// alone on the other side that follow it, by position.
///
/// A removal next to an addition otherwise takes two rows per line, each half
/// empty.
fn compact_gaps(pairs: Vec<LinePair>) -> Vec<LinePair> {
    let mut compacted = Vec::with_capacity(pairs.len());
    let mut alone: VecDeque<LinePair> = VecDeque::new();
    for pair in pairs {
        match pair {
            (Some(lhs_line), None) => match alone.front() {
                Some(&(None, Some(rhs_line))) => {
                    alone.pop_front();
                    compacted.push((Some(lhs_line), Some(rhs_line)));
                },
                _ => alone.push_back(pair),
            },
            (None, Some(rhs_line)) => match alone.front() {
                Some(&(Some(lhs_line), None)) => {
                    alone.pop_front();
                    compacted.push((Some(lhs_line), Some(rhs_line)));
                },
                _ => alone.push_back(pair),
            },
            _ => {
                compacted.extend(alone.drain(..));
                compacted.push(pair);
            },
        }
    }
    compacted.extend(alone);
    compacted
}

/// The row a line pair plans.
///
/// Two equal unchanged lines are context. A side that marks a change or a move
/// makes the row changed. Two lines the alignment paired with nothing marked on
/// either are a mismatch, which reads as changed with no span.
fn classify(pair: LinePair, lhs: &WalkSide<'_>, rhs: &WalkSide<'_>) -> RowPlan {
    let at = |index: usize| (index, index as u32 + 1);
    let left = pair.0.map(at);
    let right = pair.1.map(at);
    let marks = |side: &WalkSide<'_>, index: Option<usize>| {
        index.is_some_and(|index| !side.spans[index].is_empty() || !side.moved[index].is_empty())
    };

    match pair {
        (Some(lhs_index), Some(rhs_index))
            if !lhs.changed[lhs_index]
                && !rhs.changed[rhs_index]
                && lhs.lines[lhs_index] == rhs.lines[rhs_index] =>
        {
            RowPlan::Context {
                left: at(lhs_index),
                right: at(rhs_index),
            }
        },
        _ if marks(lhs, pair.0) || marks(rhs, pair.1) => RowPlan::Changed { left, right },
        _ => RowPlan::Mismatch { left, right },
    }
}
