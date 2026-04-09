//! Tree-aware structural diff entry point.
//!
//! Composes the [`super::lower`] arena lowering with the
//! [`super::unchanged`] preprocessing pass and converts the resulting
//! [`super::ChangeMap`]s into [`super::DiffChange`]s. The output is
//! semantically equivalent to the line-diff fallback but operates at
//! syntax-node granularity, so a one-line edit inside a function only
//! marks that line's atom rather than the entire function.
//!
//! This sits *between* the line-diff fallback and the (forthcoming)
//! Difftastic-style Dijkstra search:
//!
//! - The line-diff fallback (in `line_diff.rs`) is the safety net for inputs without a working
//!   parse, or when the structural search would exceed the graph cap.
//!
//! - This module is the cheap structural pass: lowering + LCS preprocessing + a sequential walk
//!   that emits per-Pending-atom changes. Quality is much better than line diff for most edits.
//!
//! - The Dijkstra search (forthcoming) handles the cases this pass misses: detecting when a *moved*
//!   subtree is "the same" rather than "deleted+added". Until that lands the sequential walk is the
//!   structural path.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    dijkstra::{populate_change_map, shortest_path, SearchOutcome, DEFAULT_GRAPH_LIMIT},
    lower::lower_tree,
    sliders::fix_all_sliders,
    unchanged::{mark_unchanged, ChangeKind, ChangeMap},
    ChangeKind as DiffChangeKind, DiffChange, DiffResult, Side,
};
use crate::{parse, Language};
use std::{ops::Range, sync::Arc};

/// Run the full structural-diff pipeline against two source strings.
///
/// Two-stage algorithm:
///
/// 1. **Unchanged preprocessing** ([`mark_unchanged`]) tags every trivially-unchanged node via
///    shrink-from-endpoints + LCS over sibling content_ids. This collapses the search space so the
///    expensive Dijkstra search only sees the parts that actually differ.
///
/// 2. **Dijkstra search** ([`shortest_path`]) finds the minimum-cost edit path through the residual
///    graph and marks every node it visits via an `UnchangedNode` / `EnterUnchangedDelimiter` edge
///    as Unchanged. The remaining `Pending` nodes become the diff's novel/replaced byte ranges.
///
/// On `ExceededGraphLimit` the function falls back to the
/// preprocessing-only output (which is still a valid diff, just
/// missing the per-edit cost-optimal pairing). On parse failure
/// the function returns `None` and the caller falls through to
/// [`super::diff_lines`].
pub fn diff_with_language(language: &Arc<Language>, lhs: &str, rhs: &str) -> Option<DiffResult> {
    let lhs_tree = parse(language, lhs, None)?;
    let rhs_tree = parse(language, rhs, None)?;
    let (lhs_arena, lhs_root) = lower_tree(&lhs_tree, lhs);
    let (rhs_arena, rhs_root) = lower_tree(&rhs_tree, rhs);

    // Preprocessing pass: mark trivially-unchanged regions.
    let mut preprocess = mark_unchanged(&lhs_arena, lhs_root, &rhs_arena, rhs_root);

    // Dijkstra search over the remaining edit graph; refines the
    // ChangeMaps with cost-optimal Unchanged tagging that the
    // preprocessing's structural-only matching couldn't see.
    match shortest_path(
        &lhs_arena,
        &rhs_arena,
        lhs_root,
        rhs_root,
        DEFAULT_GRAPH_LIMIT,
    ) {
        SearchOutcome::Found(path) => {
            populate_change_map(
                &lhs_arena,
                &rhs_arena,
                &path,
                &mut preprocess.lhs_changes,
                &mut preprocess.rhs_changes,
            );
        },
        SearchOutcome::ExceededGraphLimit => {
            // Search bailed; the preprocessing tags are still valid
            // (just less precise). The output is conservative: every
            // node the preprocessing left as Pending becomes a Novel
            // run, even if a deeper search would have found a match.
        },
    }

    // Slider correction: rewrite ambiguous Pending/Unchanged
    // boundaries so the visible diff lines up with structural cues.
    // Operates on each side independently.
    fix_all_sliders(&lhs_arena, lhs_root, &mut preprocess.lhs_changes);
    fix_all_sliders(&rhs_arena, rhs_root, &mut preprocess.rhs_changes);

    let mut changes = Vec::new();
    collect_changes(
        &lhs_arena,
        lhs_root,
        &preprocess.lhs_changes,
        Side::Lhs,
        &mut changes,
    );
    collect_changes(
        &rhs_arena,
        rhs_root,
        &preprocess.rhs_changes,
        Side::Rhs,
        &mut changes,
    );
    changes.sort_by_key(|c| (c.byte_range.start, c.byte_range.end));
    pair_adjacent_replacements(&mut changes);

    Some(DiffResult {
        changes,
        fell_back_to_line_diff: false,
    })
}

/// Walk an arena depth-first and emit one [`DiffChange`] per maximal
/// contiguous run of [`Syntax::Atom`] nodes that the preprocessing pass
/// left as `Pending`. Lists are descended into; their delimiters are
/// not emitted as separate atoms because the lowering pass folds
/// delimiter punctuation into the parent.
fn collect_changes(
    arena: &SyntaxArena,
    root: SyntaxId,
    changes: &ChangeMap,
    side: Side,
    out: &mut Vec<DiffChange>,
) {
    let mut current_run: Option<Range<usize>> = None;
    walk_pending_atoms(
        arena,
        root,
        changes,
        &mut |byte_range| match &mut current_run {
            Some(run) if run.end == byte_range.start => {
                run.end = byte_range.end;
            },
            Some(run) => {
                out.push(DiffChange {
                    side,
                    byte_range: run.clone(),
                    kind: DiffChangeKind::Novel,
                });
                *run = byte_range;
            },
            None => current_run = Some(byte_range),
        },
    );
    if let Some(run) = current_run {
        out.push(DiffChange {
            side,
            byte_range: run,
            kind: DiffChangeKind::Novel,
        });
    }
}

fn walk_pending_atoms(
    arena: &SyntaxArena,
    id: SyntaxId,
    changes: &ChangeMap,
    callback: &mut impl FnMut(Range<usize>),
) {
    match arena.get(id) {
        Syntax::Atom(atom) => {
            // For atoms, the Unchanged tag means "this atom matches
            // its counterpart" so we skip it. Pending atoms become
            // diff output.
            if changes.get(id) == ChangeKind::Unchanged {
                return;
            }
            // Skip empty atoms (e.g. anonymous nodes lowered to empty
            // ranges) so the diff output stays compact.
            if atom.byte_range.start < atom.byte_range.end {
                callback(atom.byte_range.clone());
            }
        },
        Syntax::List(list) => {
            // For lists, the Unchanged tag means "the delimiters
            // match" -- the children may still differ. Always recurse
            // into list children to find Pending atoms inside an
            // otherwise-unchanged container.
            //
            // The deep `mark_subtree` path in
            // [`super::dijkstra::populate_change_map`] only fires for
            // `Edge::UnchangedNode` (i.e. `content_id` matched), in
            // which case every descendant is also marked Unchanged
            // and the recursion skips them naturally.
            for child in &list.children {
                walk_pending_atoms(arena, *child, changes, callback);
            }
        },
    }
}

/// Convert a sorted [`DiffChange`] list so that an Lhs Novel run that
/// is followed (in source order across the two inputs) by an Rhs Novel
/// run becomes a `Replaced` pair. The line-diff path uses an explicit
/// op stream to do this; the structural path discovers it post-hoc by
/// adjacency in `(side, byte_range.start)` order.
fn pair_adjacent_replacements(changes: &mut [DiffChange]) {
    // Sort by side first so we can pair lhs runs with rhs runs by
    // index, mirroring how the line-diff pass already handles this.
    let mut lhs_indices: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.side == Side::Lhs)
        .map(|(i, _)| i)
        .collect();
    let mut rhs_indices: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.side == Side::Rhs)
        .map(|(i, _)| i)
        .collect();
    let pair_count = lhs_indices.len().min(rhs_indices.len());
    for k in 0..pair_count {
        changes[lhs_indices[k]].kind = DiffChangeKind::Replaced;
        changes[rhs_indices[k]].kind = DiffChangeKind::Replaced;
    }
    let _ = (&mut lhs_indices, &mut rhs_indices); // silence dead-mut warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::LanguageRegistry;

    fn rust_lang() -> Arc<Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .unwrap()
    }

    #[test]
    fn identical_sources_emit_no_changes() {
        let lang = rust_lang();
        let source = "fn main() { let x = 1; }";
        let result = diff_with_language(&lang, source, source).unwrap();
        assert!(result.changes.is_empty());
        assert!(!result.fell_back_to_line_diff);
    }

    #[test]
    fn renamed_function_emits_replaced_atom_pair() {
        let lang = rust_lang();
        let lhs = "fn alpha() {}";
        let rhs = "fn beta() {}";
        let result = diff_with_language(&lang, lhs, rhs).unwrap();
        assert!(!result.changes.is_empty(), "must produce changes");

        // The structural pass tags the renamed identifier as a
        // Pending atom on each side. The pairing pass turns the lhs
        // and rhs runs into a Replaced pair.
        let lhs_changes: Vec<&DiffChange> = result
            .changes
            .iter()
            .filter(|c| c.side == Side::Lhs)
            .collect();
        let rhs_changes: Vec<&DiffChange> = result
            .changes
            .iter()
            .filter(|c| c.side == Side::Rhs)
            .collect();
        assert!(!lhs_changes.is_empty());
        assert!(!rhs_changes.is_empty());
        assert!(lhs_changes
            .iter()
            .all(|c| c.kind == DiffChangeKind::Replaced));
        assert!(rhs_changes
            .iter()
            .all(|c| c.kind == DiffChangeKind::Replaced));

        // Verify the lhs change covers exactly "alpha" and the rhs
        // covers exactly "beta" (no spurious surrounding bytes).
        assert!(
            lhs_changes
                .iter()
                .any(|c| &lhs[c.byte_range.clone()] == "alpha"),
            "lhs change should cover the renamed identifier 'alpha'"
        );
        assert!(
            rhs_changes
                .iter()
                .any(|c| &rhs[c.byte_range.clone()] == "beta"),
            "rhs change should cover the renamed identifier 'beta'"
        );
    }

    #[test]
    fn appended_function_emits_only_rhs_novel() {
        let lang = rust_lang();
        let lhs = "fn main() {}";
        let rhs = "fn main() {}\nfn extra() {}";
        let result = diff_with_language(&lang, lhs, rhs).unwrap();
        assert!(!result.changes.is_empty());
        let lhs_runs = result
            .changes
            .iter()
            .filter(|c| c.side == Side::Lhs)
            .count();
        let rhs_runs = result
            .changes
            .iter()
            .filter(|c| c.side == Side::Rhs)
            .count();
        assert_eq!(
            lhs_runs, 0,
            "no lhs changes expected for pure addition, got {result:?}"
        );
        assert!(rhs_runs >= 1, "at least one rhs change expected");
        assert!(result.changes.iter().any(|c| {
            c.side == Side::Rhs
                && c.kind == DiffChangeKind::Novel
                && rhs[c.byte_range.clone()].contains("extra")
        }));
    }

    #[test]
    fn body_edit_inside_function_does_not_mark_signature() {
        // The function signature is unchanged; only the body's atom
        // for the literal `1` -> `2` should be marked novel. This is
        // the structural diff's headline win over line diff.
        let lang = rust_lang();
        let lhs = "fn main() { let x = 1; }";
        let rhs = "fn main() { let x = 2; }";
        let result = diff_with_language(&lang, lhs, rhs).unwrap();
        assert!(!result.changes.is_empty());

        // The lhs change should cover '1', not the whole function.
        let lhs_change = result
            .changes
            .iter()
            .find(|c| c.side == Side::Lhs)
            .expect("lhs change");
        assert_eq!(&lhs[lhs_change.byte_range.clone()], "1");
        let rhs_change = result
            .changes
            .iter()
            .find(|c| c.side == Side::Rhs)
            .expect("rhs change");
        assert_eq!(&rhs[rhs_change.byte_range.clone()], "2");
    }
}
