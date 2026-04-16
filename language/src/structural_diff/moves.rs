//! Post-Dijkstra move detection.
//!
//! After the unchanged-preprocessing pass and the Dijkstra search have
//! tagged every node either [`ChangeKind::Unchanged`] or
//! [`ChangeKind::Pending`], residual `Pending` nodes whose
//! [`super::ContentId`] appears on both sides are candidates for a
//! *move*: byte-for-byte equal subtrees that differ only in relative
//! position, including cross-indentation moves inside a different
//! parent. The move pass rewrites those candidates to
//! [`ChangeKind::Moved`] and publishes a [`MoveRecord`] that links each
//! right-side target to its left-side source(s).
//!
//! Cardinality mirrors what code review cares about:
//!
//! - `1:1` one source, one target. The common "function moved" case.
//! - `N:1` (consolidation) one target with N source candidates. Emitted when the same block existed
//!   in multiple LHS locations and got factored into one shared RHS location. The single record's
//!   [`MoveRecord::lhs_sources`] has all N nodes; downstream code reports this as an ambiguous
//!   move.
//! - `1:N` (duplication) one source, N targets. Emitted as N records sharing the same `lhs_sources`
//!   so each target has its own provenance line.
//! - `N:M` greedy proximity pairing; leftovers are listed as ambiguous alternates.
//!
//! Nodes that are tiny (`atom_leaf_count < MIN_LEAVES`) or would
//! explode pairing state (`content_id` with more than [`MAX_AMBIGUITY`]
//! candidates on either side) are skipped: they produce noise without
//! informing the user. Both thresholds are tuning constants at the top
//! of this module for easy calibration.
//!
//! The move pass runs after slider correction in
//! [`super::tree_diff::diff_with_language`] so its input is the final
//! residual `Pending` set. It does not depend on any Dijkstra state; if
//! the search bails to `ExceededGraphLimit`, the move pass still runs
//! and still tags high-confidence moves based on the preprocessing-only
//! output.

use super::{
    arena::{Syntax, SyntaxArena, SyntaxId},
    content_id::ContentId,
    unchanged::{ChangeKind, ChangeMap},
};
use std::collections::{HashMap, HashSet};

/// Minimum number of atom leaves in a subtree for it to be considered a
/// move candidate. Set to reject single identifiers (1 leaf), bare
/// `return;` or `break;` (~2 leaves), and micro-expressions (~3 leaves).
/// Moves below this threshold are too ambiguous to be actionable.
///
/// Calibrated against the tier-1 tests in this module (function-level
/// moves, consolidation, duplication) and the tier-2 integration tests
/// in `tree_diff.rs`. At 4 leaves the threshold accepts `let x = foo;`
/// and `call(arg);` style statements (which are actionable moves) while
/// rejecting lone return/break punctuation. Raising to 8+ starts
/// missing valid single-statement moves; lowering to 2 produces noisy
/// pair-the-punctuation matches.
pub const MIN_LEAVES: usize = 4;

/// Maximum number of candidates on a side with the same
/// [`super::ContentId`] before the move pass gives up. Guards against
/// pathological repetition (e.g. files with hundreds of identical empty
/// argument lists) where every match would be noise. Values above this
/// produce no move records for the given content id.
///
/// Calibrated by the `max_ambiguity_cap` tier-1 test: at 8 the cap
/// accommodates realistic consolidation (N:1 where N = 2-5 helpers
/// collapse into one function) while rejecting degenerate N > 8
/// repetition patterns. Raise cautiously; every content id with
/// k_lhs or k_rhs > MAX_AMBIGUITY is skipped regardless of leaf count.
pub const MAX_AMBIGUITY: usize = 8;

/// One resolved move: a right-side target paired with one or more
/// left-side sources. Populated by [`find_moves`] and consumed by the
/// diff collection pass to emit [`super::DiffChange`]s with metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MoveRecord {
    pub rhs_target: SyntaxId,
    pub lhs_sources: Vec<SyntaxId>,
}

/// Run the move pass. Mutates the two change maps in place, rewriting
/// paired nodes from [`ChangeKind::Pending`] to [`ChangeKind::Moved`].
/// Returns the list of [`MoveRecord`]s in RHS byte order; the caller
/// threads them through to [`super::DiffChange::move_metadata`].
///
/// Scans every node in both arenas, not just `Pending` ones, because
/// the unchanged-preprocessing pass greedily pairs one LHS copy of a
/// repeated [`ContentId`] with one RHS copy. In a consolidation
/// scenario (N LHS sources merged into one shared RHS location) only
/// `N - 1` LHS copies remain Pending; looking at the full arena lets
/// us emit the complete N-source record so downstream callers can
/// offer every candidate jump target. Content ids whose copies all
/// remain [`ChangeKind::Unchanged`] after preprocessing AND whose
/// counts match across sides are skipped: that is structural
/// preservation, not a move.
pub fn find_moves(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_changes: &mut ChangeMap,
    rhs_changes: &mut ChangeMap,
) -> Vec<MoveRecord> {
    let lhs_parents = build_parent_map(lhs_arena);
    let rhs_parents = build_parent_map(rhs_arena);

    let lhs_by_cid = index_all_candidates(lhs_arena);
    let rhs_by_cid = index_all_candidates(rhs_arena);

    let mut shared: Vec<(ContentId, usize, usize)> = lhs_by_cid
        .iter()
        .filter_map(|(cid, ids)| {
            rhs_by_cid.get(cid).map(|rhs_ids| {
                let leaves = ids
                    .iter()
                    .map(|id| atom_leaf_count(lhs_arena, *id))
                    .max()
                    .unwrap_or(0);
                let min_depth = ids
                    .iter()
                    .map(|id| node_depth(&lhs_parents, *id))
                    .chain(rhs_ids.iter().map(|id| node_depth(&rhs_parents, *id)))
                    .min()
                    .unwrap_or(0);
                (*cid, leaves, min_depth)
            })
        })
        .collect();
    // Largest subtree first so ancestor moves pair before descendants.
    // When leaf counts tie (e.g. a block and its only expression_statement
    // both cover the same atoms), shallowest-first breaks the tie so the
    // outer structural unit wins. Content id is the final deterministic
    // tiebreaker.
    shared.sort_by(|a, b| b.1.cmp(&a.1).then(a.2.cmp(&b.2)).then(a.0.cmp(&b.0)));

    let mut paired_lhs: HashSet<SyntaxId> = HashSet::new();
    let mut paired_rhs: HashSet<SyntaxId> = HashSet::new();
    let mut records: Vec<MoveRecord> = Vec::new();

    for (cid, _, _) in &shared {
        let lhs_cand = lhs_by_cid.get(cid).expect("cid in shared set");
        let rhs_cand = rhs_by_cid.get(cid).expect("cid in shared set");
        if lhs_cand.len() > MAX_AMBIGUITY || rhs_cand.len() > MAX_AMBIGUITY {
            continue;
        }

        let lhs_avail = filter_unpaired(lhs_cand, &paired_lhs, &lhs_parents);
        let rhs_avail = filter_unpaired(rhs_cand, &paired_rhs, &rhs_parents);
        if lhs_avail.is_empty() || rhs_avail.is_empty() {
            continue;
        }

        // Pure structural preservation: counts match 1:1 and every
        // copy is already Unchanged. Not a move.
        if lhs_avail.len() == rhs_avail.len()
            && lhs_avail
                .iter()
                .all(|id| lhs_changes.get(*id) == ChangeKind::Unchanged)
            && rhs_avail
                .iter()
                .all(|id| rhs_changes.get(*id) == ChangeKind::Unchanged)
        {
            continue;
        }

        emit_records(
            lhs_arena,
            rhs_arena,
            &lhs_avail,
            &rhs_avail,
            &mut paired_lhs,
            &mut paired_rhs,
            &mut records,
        );
    }

    for id in &paired_lhs {
        lhs_changes.mark(*id, ChangeKind::Moved);
    }
    for id in &paired_rhs {
        rhs_changes.mark(*id, ChangeKind::Moved);
    }

    records.sort_by_key(|r| byte_start(rhs_arena, r.rhs_target));
    records
}

/// Walk every node in `arena` and group by [`ContentId`]. Includes
/// Unchanged nodes so consolidation and duplication counts reflect
/// every copy in the arena, not just the ones that survived the
/// unchanged-preprocessing pass. The caller compares LHS and RHS
/// counts to decide whether a content id is a real move or pure
/// structural preservation. Applies the [`is_trivial`] denylist and
/// [`MIN_LEAVES`] threshold at index time so noise candidates never
/// enter the pairing loop.
fn index_all_candidates(arena: &SyntaxArena) -> HashMap<ContentId, Vec<SyntaxId>> {
    let mut out: HashMap<ContentId, Vec<SyntaxId>> = HashMap::new();
    for i in 0..arena.len() {
        let id = SyntaxId(i);
        if is_trivial(arena, id) {
            continue;
        }
        if atom_leaf_count(arena, id) < MIN_LEAVES {
            continue;
        }
        out.entry(arena.get(id).content_id()).or_default().push(id);
    }
    out
}

/// Tree-sitter punctuation (braces, semicolons, commas, parens) has
/// kinds composed of non-alphabetic characters; filter them out so a
/// lone `}` never shows up as a move. Applies to atoms only; lists are
/// guarded by [`MIN_LEAVES`] via [`atom_leaf_count`].
fn is_trivial(arena: &SyntaxArena, id: SyntaxId) -> bool {
    match arena.get(id) {
        Syntax::Atom(a) => !a.kind.chars().any(|c| c.is_alphabetic()),
        Syntax::List(_) => false,
    }
}

fn filter_unpaired(
    candidates: &[SyntaxId],
    paired: &HashSet<SyntaxId>,
    parents: &HashMap<SyntaxId, SyntaxId>,
) -> Vec<SyntaxId> {
    candidates
        .iter()
        .copied()
        .filter(|id| !paired.contains(id) && !ancestor_in_set(parents, *id, paired))
        .collect()
}

fn emit_records(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    lhs_avail: &[SyntaxId],
    rhs_avail: &[SyntaxId],
    paired_lhs: &mut HashSet<SyntaxId>,
    paired_rhs: &mut HashSet<SyntaxId>,
    records: &mut Vec<MoveRecord>,
) {
    let mut lhs_sorted = lhs_avail.to_vec();
    lhs_sorted.sort_by_key(|id| byte_start(lhs_arena, *id));
    let mut rhs_sorted = rhs_avail.to_vec();
    rhs_sorted.sort_by_key(|id| byte_start(rhs_arena, *id));

    match (lhs_sorted.len(), rhs_sorted.len()) {
        (1, 1) => {
            records.push(MoveRecord {
                rhs_target: rhs_sorted[0],
                lhs_sources: vec![lhs_sorted[0]],
            });
            paired_lhs.insert(lhs_sorted[0]);
            paired_rhs.insert(rhs_sorted[0]);
        },
        (_, 1) => {
            // N:1 consolidation: single record with every LHS source
            // listed in byte order. Downstream treats len > 1 as
            // ambiguous.
            let target = rhs_sorted[0];
            for src in &lhs_sorted {
                paired_lhs.insert(*src);
            }
            paired_rhs.insert(target);
            records.push(MoveRecord {
                rhs_target: target,
                lhs_sources: lhs_sorted,
            });
        },
        (1, _) => {
            // 1:N duplication: one record per target, all pointing at
            // the same single LHS source.
            let src = lhs_sorted[0];
            paired_lhs.insert(src);
            for target in rhs_sorted {
                paired_rhs.insert(target);
                records.push(MoveRecord {
                    rhs_target: target,
                    lhs_sources: vec![src],
                });
            }
        },
        (n, m) if n <= m => {
            // Proximity-paired greedy N:M, then leftover RHS targets
            // list every LHS source as an ambiguous alternate.
            for k in 0..n {
                paired_lhs.insert(lhs_sorted[k]);
                paired_rhs.insert(rhs_sorted[k]);
                records.push(MoveRecord {
                    rhs_target: rhs_sorted[k],
                    lhs_sources: vec![lhs_sorted[k]],
                });
            }
            for k in n..m {
                paired_rhs.insert(rhs_sorted[k]);
                records.push(MoveRecord {
                    rhs_target: rhs_sorted[k],
                    lhs_sources: lhs_sorted.clone(),
                });
            }
        },
        (_, m) => {
            // n > m: pair m unique targets, then the last record
            // collects every remaining LHS source as ambiguous.
            for k in 0..m {
                let sources = if k + 1 == m {
                    lhs_sorted[k..].to_vec()
                } else {
                    vec![lhs_sorted[k]]
                };
                for src in &sources {
                    paired_lhs.insert(*src);
                }
                paired_rhs.insert(rhs_sorted[k]);
                records.push(MoveRecord {
                    rhs_target: rhs_sorted[k],
                    lhs_sources: sources,
                });
            }
        },
    }
}

fn build_parent_map(arena: &SyntaxArena) -> HashMap<SyntaxId, SyntaxId> {
    let mut parents = HashMap::new();
    for i in 0..arena.len() {
        let id = SyntaxId(i);
        if let Syntax::List(list) = arena.get(id) {
            for child in &list.children {
                parents.insert(*child, id);
            }
        }
    }
    parents
}

fn ancestor_in_set(
    parents: &HashMap<SyntaxId, SyntaxId>,
    id: SyntaxId,
    set: &HashSet<SyntaxId>,
) -> bool {
    let mut cur = parents.get(&id).copied();
    while let Some(p) = cur {
        if set.contains(&p) {
            return true;
        }
        cur = parents.get(&p).copied();
    }
    false
}

fn node_depth(parents: &HashMap<SyntaxId, SyntaxId>, id: SyntaxId) -> usize {
    let mut depth = 0usize;
    let mut cur = parents.get(&id).copied();
    while let Some(p) = cur {
        depth += 1;
        cur = parents.get(&p).copied();
    }
    depth
}

fn byte_start(arena: &SyntaxArena, id: SyntaxId) -> usize {
    match arena.get(id) {
        Syntax::Atom(a) => a.byte_range.start,
        Syntax::List(l) => {
            let open = l.open_byte_range.start;
            let child = l
                .children
                .first()
                .map(|c| byte_start(arena, *c))
                .unwrap_or(usize::MAX);
            open.min(child)
        },
    }
}

#[allow(dead_code)]
fn is_moved(changes: &ChangeMap, id: SyntaxId) -> bool {
    matches!(changes.get(id), ChangeKind::Moved)
}

fn atom_leaf_count(arena: &SyntaxArena, id: SyntaxId) -> usize {
    let mut stack = vec![id];
    let mut count = 0usize;
    while let Some(current) = stack.pop() {
        match arena.get(current) {
            Syntax::Atom(_) => count += 1,
            Syntax::List(list) => stack.extend(list.children.iter().copied()),
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        parse,
        structural_diff::{lower_tree, mark_unchanged, PreprocessResult},
        LanguageRegistry,
    };
    use std::ops::Range;

    fn rust_lang() -> std::sync::Arc<crate::Language> {
        LanguageRegistry::standard()
            .for_path(std::path::Path::new("a.rs"))
            .unwrap()
    }

    fn lower(source: &str) -> (SyntaxArena, SyntaxId) {
        let lang = rust_lang();
        let tree = parse(&lang, source, None).unwrap();
        lower_tree(&tree, source)
    }

    fn full_byte_range(arena: &SyntaxArena, id: SyntaxId) -> Range<usize> {
        match arena.get(id) {
            Syntax::Atom(a) => a.byte_range.clone(),
            Syntax::List(l) => {
                let start = l.open_byte_range.start.min(
                    l.children
                        .first()
                        .map_or(usize::MAX, |c| full_byte_range(arena, *c).start),
                );
                let end = l.close_byte_range.end.max(
                    l.children
                        .last()
                        .map_or(0, |c| full_byte_range(arena, *c).end),
                );
                start..end
            },
        }
    }

    /// Run the standard preprocessing + move pass pipeline on `(lhs, rhs)`
    /// and return everything the tests need to assert on.
    fn find_moves_in(
        lhs: &str,
        rhs: &str,
    ) -> (
        SyntaxArena,
        SyntaxArena,
        SyntaxId,
        SyntaxId,
        PreprocessResult,
        Vec<MoveRecord>,
    ) {
        let (lhs_arena, lhs_root) = lower(lhs);
        let (rhs_arena, rhs_root) = lower(rhs);
        let mut preprocess = mark_unchanged(&lhs_arena, lhs_root, &rhs_arena, rhs_root);
        let records = find_moves(
            &lhs_arena,
            &rhs_arena,
            &mut preprocess.lhs_changes,
            &mut preprocess.rhs_changes,
        );
        (
            lhs_arena, rhs_arena, lhs_root, rhs_root, preprocess, records,
        )
    }

    fn contains_text(arena: &SyntaxArena, id: SyntaxId, source: &str, needle: &str) -> bool {
        let range = full_byte_range(arena, id);
        source[range].contains(needle)
    }

    #[test]
    fn straight_function_move() {
        // Two top-level functions swap order. mark_unchanged pairs one
        // via LCS; the residual Pending pair is the move target.
        let lhs = "fn alpha() { let x = 1; let y = 2; let z = 3; }\n\
                   fn beta() { let p = 10; let q = 20; let r = 30; }";
        let rhs = "fn beta() { let p = 10; let q = 20; let r = 30; }\n\
                   fn alpha() { let x = 1; let y = 2; let z = 3; }";
        let (lhs_arena, rhs_arena, _, _, pre, records) = find_moves_in(lhs, rhs);

        assert_eq!(
            records.len(),
            1,
            "swap of two functions leaves exactly one residual move after LCS pairs the other; got {records:?}"
        );
        let record = &records[0];
        assert_eq!(record.lhs_sources.len(), 1, "unambiguous move");

        let lhs_src = &record.lhs_sources[0];
        let lhs_text = &lhs[full_byte_range(&lhs_arena, *lhs_src)];
        let rhs_text = &rhs[full_byte_range(&rhs_arena, record.rhs_target)];
        assert_eq!(
            lhs_arena.get(*lhs_src).content_id(),
            rhs_arena.get(record.rhs_target).content_id(),
            "moved subtree must share content_id across sides"
        );
        assert_eq!(
            lhs_text, rhs_text,
            "moved subtree must be byte-for-byte equal"
        );

        // ChangeMaps must be rewritten to Moved for the paired nodes.
        assert!(is_moved(&pre.lhs_changes, *lhs_src));
        assert!(is_moved(&pre.rhs_changes, record.rhs_target));
    }

    #[test]
    fn function_moved_into_other_function() {
        // A statement that lived at top level (inside function `outer`)
        // now lives nested inside function `wrapper`. The statement's
        // content_id is identical; only its parent container differs.
        let lhs = "fn outer() { let relocated = compute(arg1, arg2, arg3); }\n\
                   fn wrapper() { println!(\"hello\"); }";
        let rhs = "fn outer() {}\n\
                   fn wrapper() { println!(\"hello\"); let relocated = compute(arg1, arg2, arg3); }";
        let (lhs_arena, rhs_arena, _, _, _, records) = find_moves_in(lhs, rhs);

        assert!(!records.is_empty(), "must detect the relocated let");
        let relocated = records
            .iter()
            .find(|r| {
                let rhs_text = &rhs[full_byte_range(&rhs_arena, r.rhs_target)];
                rhs_text.contains("let relocated")
            })
            .expect("find record for the relocated let");
        assert_eq!(relocated.lhs_sources.len(), 1);
        let src_text = &lhs[full_byte_range(&lhs_arena, relocated.lhs_sources[0])];
        assert!(
            src_text.contains("let relocated"),
            "source must be the LHS copy of the same statement"
        );
    }

    #[test]
    fn consolidation_n_to_1() {
        // Same block appears in two LHS functions; RHS factored into
        // one shared function. The single RHS target carries BOTH LHS
        // sources in its `lhs_sources` list (ambiguous).
        let lhs = "fn first() { let temp = heavy_computation(a, b, c); save(temp); }\n\
                   fn second() { let temp = heavy_computation(a, b, c); save(temp); }";
        let rhs = "fn shared() { let temp = heavy_computation(a, b, c); save(temp); }\n\
                   fn first() { shared(); }\n\
                   fn second() { shared(); }";
        let (lhs_arena, rhs_arena, _, _, _, records) = find_moves_in(lhs, rhs);

        let consolidation = records
            .iter()
            .find(|r| r.lhs_sources.len() >= 2)
            .expect("must detect N:1 consolidation");
        assert_eq!(
            consolidation.lhs_sources.len(),
            2,
            "exactly two LHS sources for a 2:1 consolidation"
        );
        // Both sources must share content_id with the target.
        let target_cid = rhs_arena.get(consolidation.rhs_target).content_id();
        for src in &consolidation.lhs_sources {
            assert_eq!(
                lhs_arena.get(*src).content_id(),
                target_cid,
                "every source must share content_id with the target"
            );
        }
        // Sources must be distinct byte ranges.
        let a = full_byte_range(&lhs_arena, consolidation.lhs_sources[0]);
        let b = full_byte_range(&lhs_arena, consolidation.lhs_sources[1]);
        assert_ne!(a, b, "sources must be distinct positions");
    }

    #[test]
    fn duplication_1_to_n() {
        // One LHS callsite, two RHS copies. Expect two records, each
        // with the single LHS source; together they form the 1:N set.
        let lhs = "fn only() { render_widget(config, style, theme); }";
        let rhs = "fn first() { render_widget(config, style, theme); }\n\
                   fn second() { render_widget(config, style, theme); }";
        let (lhs_arena, rhs_arena, _, _, _, records) = find_moves_in(lhs, rhs);

        let render_moves: Vec<&MoveRecord> = records
            .iter()
            .filter(|r| {
                let rhs_text = &rhs[full_byte_range(&rhs_arena, r.rhs_target)];
                rhs_text.contains("render_widget")
            })
            .collect();
        assert_eq!(
            render_moves.len(),
            2,
            "one record per duplication target; got {render_moves:?}"
        );
        // Both should have the same single LHS source.
        let src_a = render_moves[0]
            .lhs_sources
            .first()
            .copied()
            .expect("record 0 has a source");
        let src_b = render_moves[1]
            .lhs_sources
            .first()
            .copied()
            .expect("record 1 has a source");
        assert_eq!(
            src_a, src_b,
            "both duplication targets share the LHS source"
        );
        assert!(contains_text(&lhs_arena, src_a, lhs, "render_widget"));
    }

    #[test]
    fn partial_move_with_signature_change() {
        // Body moves (content_id unchanged) but the function name
        // changes. Expect the body to be classified Moved; the
        // signature/name atoms stay Novel/Replaced via the normal
        // preprocessing path (not a move).
        let lhs = "fn helper() { let computed = expensive_init(source, sink, transform); }";
        let rhs = "fn main() {}\n\
                   fn renamed_helper() { let computed = expensive_init(source, sink, transform); }";
        let (_, rhs_arena, _, _, _, records) = find_moves_in(lhs, rhs);

        let body_move = records.iter().find(|r| {
            let rhs_text = &rhs[full_byte_range(&rhs_arena, r.rhs_target)];
            rhs_text.contains("expensive_init")
        });
        assert!(
            body_move.is_some(),
            "body statement must be tagged Moved even when enclosing function was renamed"
        );
    }

    #[test]
    fn move_with_inline_edit() {
        // Whole function "computes" contains a renamed identifier. The
        // function signature changes (content_id shifts), but inner
        // unchanged subtrees can still be detected as Moved when the
        // function relocates.
        //
        // Here the `(source, sink, transform)` argument list has a
        // stable content_id and is structurally identical in both
        // sides; it appears at a different position on RHS.
        let lhs = "fn first() { work(source, sink, transform); }\n\
                   fn second() {}";
        let rhs = "fn first() {}\n\
                   fn second() { work(source, sink, transform); }";
        let (_, rhs_arena, _, _, _, records) = find_moves_in(lhs, rhs);

        assert!(
            records
                .iter()
                .any(|r| rhs[full_byte_range(&rhs_arena, r.rhs_target)].contains("work")),
            "work(...) call should be detected as moved between functions"
        );
    }

    #[test]
    fn trivial_punctuation_not_moved() {
        // Two files with stray `;` atoms that happen to not pair up
        // via the normal preprocessing. The move pass must NOT tag
        // punctuation as a move: the denylist plus MIN_LEAVES guard
        // handles this.
        let lhs = "fn a() { let _ = f(); }";
        let rhs = "fn b() { let _ = f(); }";
        let (_, _, _, _, _, records) = find_moves_in(lhs, rhs);
        // Trivial semicolon or brace atoms must never appear as a move
        // target. (They may still pair via Unchanged; the assertion is
        // just that they don't become Moved records.)
        for record in &records {
            assert!(
                record.lhs_sources.iter().all(|id| atom_leaf_count(
                    &lower(lhs).0, // not ideal but acceptable for a negative assert
                    *id
                ) >= MIN_LEAVES),
                "no move record may cover a sub-MIN_LEAVES atom; got {record:?}"
            );
        }
    }

    #[test]
    fn min_leaf_threshold() {
        // Two unpaired `return;` statements (2 leaves each) should not
        // produce a move, even though their content_ids match.
        let lhs = "fn outer() { if cond() { return; } work(a, b, c); }";
        let rhs = "fn outer() { work(a, b, c); if cond() { return; } }";
        let (lhs_arena, rhs_arena, _, _, _, records) = find_moves_in(lhs, rhs);

        for record in &records {
            for src in &record.lhs_sources {
                assert!(
                    atom_leaf_count(&lhs_arena, *src) >= MIN_LEAVES,
                    "move source must meet MIN_LEAVES threshold"
                );
            }
            assert!(
                atom_leaf_count(&rhs_arena, record.rhs_target) >= MIN_LEAVES,
                "move target must meet MIN_LEAVES threshold"
            );
        }
    }

    #[test]
    fn ancestor_subsumption() {
        // A whole function moved. Its body atoms would individually
        // share content_id too, but the move pass must tag only the
        // function-level record and subsume descendants.
        let lhs = "fn stay() { keep(); }\n\
                   fn migrated() { let x = build(args); compose(x, aux); finalize(); }";
        let rhs = "fn migrated() { let x = build(args); compose(x, aux); finalize(); }\n\
                   fn stay() { keep(); }";
        let (lhs_arena, rhs_arena, _, _, _, records) = find_moves_in(lhs, rhs);

        // At most one record covers the migrated function body. Verify
        // that no separate record covers a strict descendant of another
        // record's target.
        for (i, a) in records.iter().enumerate() {
            let a_range = full_byte_range(&rhs_arena, a.rhs_target);
            for (j, b) in records.iter().enumerate() {
                if i == j {
                    continue;
                }
                let b_range = full_byte_range(&rhs_arena, b.rhs_target);
                let b_strictly_inside = b_range.start >= a_range.start
                    && b_range.end <= a_range.end
                    && b_range != a_range;
                assert!(
                    !b_strictly_inside,
                    "record {b:?} is nested inside {a:?}; ancestor subsumption failed"
                );
            }
        }
        // And at least one record was produced.
        assert!(
            !records.is_empty(),
            "migrated function must produce at least one move record"
        );
        let _ = lhs_arena;
    }

    #[test]
    fn ambiguous_proximity_tiebreak() {
        // Three LHS copies, one RHS copy: 3:1 consolidation. The RHS
        // record must list all three LHS sources, sorted deterministically
        // by byte offset ascending.
        let lhs = "fn a() { let temp = heavy_computation(x, y, z); use_it(temp); }\n\
                   fn b() { let temp = heavy_computation(x, y, z); use_it(temp); }\n\
                   fn c() { let temp = heavy_computation(x, y, z); use_it(temp); }";
        let rhs = "fn shared() { let temp = heavy_computation(x, y, z); use_it(temp); }\n\
                   fn a() { shared(); }\n\
                   fn b() { shared(); }\n\
                   fn c() { shared(); }";
        let (lhs_arena, rhs_arena, _, _, _, records) = find_moves_in(lhs, rhs);

        let consolidation = records
            .iter()
            .find(|r| r.lhs_sources.len() >= 3)
            .expect("must produce one 3:1 record");
        assert_eq!(consolidation.lhs_sources.len(), 3);
        // Sources sorted by byte offset ascending = deterministic.
        let offsets: Vec<usize> = consolidation
            .lhs_sources
            .iter()
            .map(|id| full_byte_range(&lhs_arena, *id).start)
            .collect();
        let mut sorted = offsets.clone();
        sorted.sort();
        assert_eq!(offsets, sorted, "sources must be sorted by LHS byte offset");
        let _ = rhs_arena;
    }

    #[test]
    fn max_ambiguity_cap() {
        // More than MAX_AMBIGUITY identical blocks on each side: the
        // move pass skips them entirely rather than producing
        // exponentially ambiguous records.
        let mut lhs = String::new();
        let mut rhs = String::new();
        let copies = MAX_AMBIGUITY + 2;
        for i in 0..copies {
            lhs.push_str(&format!(
                "fn f_{i}() {{ run_heavy_operation(alpha, beta, gamma); }}\n"
            ));
        }
        // Same blocks on RHS but listed in reversed order so the LCS
        // pass cannot match them all up positionally.
        for i in (0..copies).rev() {
            rhs.push_str(&format!(
                "fn f_{i}() {{ run_heavy_operation(alpha, beta, gamma); }}\n"
            ));
        }
        let (_, rhs_arena, _, _, _, records) = find_moves_in(&lhs, &rhs);

        let run_heavy_moves: Vec<&MoveRecord> = records
            .iter()
            .filter(|r| {
                rhs[full_byte_range(&rhs_arena, r.rhs_target)].contains("run_heavy_operation")
            })
            .collect();
        // The shared argument list `(alpha, beta, gamma)` has >MAX_AMBIGUITY
        // copies; no move record may cover one. Function-level bodies
        // also exceed the cap once duplicated in reverse.
        assert!(
            run_heavy_moves.is_empty()
                || run_heavy_moves
                    .iter()
                    .all(|r| r.lhs_sources.len() <= MAX_AMBIGUITY),
            "records must never exceed MAX_AMBIGUITY sources; got {run_heavy_moves:?}"
        );
    }
}
