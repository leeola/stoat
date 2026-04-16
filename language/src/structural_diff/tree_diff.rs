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
    moves::{find_moves, MoveRecord},
    sliders::fix_all_sliders,
    unchanged::{mark_unchanged, ChangeKind, ChangeMap},
    ChangeKind as DiffChangeKind, DiffChange, DiffResult, MoveMetadata, MoveSource, Side,
};
use crate::{parse, Language};
use std::{
    collections::HashMap,
    ops::Range,
    sync::{atomic::AtomicBool, Arc},
};

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
    diff_with_language_cancellable(language, lhs, rhs, None)
}

/// Variant of [`diff_with_language`] that accepts an optional
/// cancellation flag. The Dijkstra search polls the flag periodically
/// and returns `ExceededGraphLimit` on cancel, which cleanly drops
/// through to the preprocessing-only diff. Intended for
/// cancel-on-edit scheduling: the caller sets the flag when a newer
/// edit supersedes the in-flight job.
pub fn diff_with_language_cancellable(
    language: &Arc<Language>,
    lhs: &str,
    rhs: &str,
    cancel: Option<&AtomicBool>,
) -> Option<DiffResult> {
    let lhs_tree = parse(language, lhs, None)?;
    let rhs_tree = parse(language, rhs, None)?;
    let (lhs_arena, lhs_root) = lower_tree(&lhs_tree, lhs);
    let (rhs_arena, rhs_root) = lower_tree(&rhs_tree, rhs);

    let mut preprocess = mark_unchanged(&lhs_arena, lhs_root, &rhs_arena, rhs_root);

    match shortest_path(
        &lhs_arena,
        &rhs_arena,
        lhs_root,
        rhs_root,
        DEFAULT_GRAPH_LIMIT,
        cancel,
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
        SearchOutcome::ExceededGraphLimit => {},
    }

    fix_all_sliders(&lhs_arena, lhs_root, &mut preprocess.lhs_changes);
    fix_all_sliders(&rhs_arena, rhs_root, &mut preprocess.rhs_changes);

    let records = find_moves(
        &lhs_arena,
        &rhs_arena,
        &mut preprocess.lhs_changes,
        &mut preprocess.rhs_changes,
    );

    let lhs_lines = LineIndex::new(lhs);
    let rhs_lines = LineIndex::new(rhs);
    let lhs_meta = build_move_metadata(
        &lhs_arena,
        &rhs_arena,
        &records,
        Side::Lhs,
        &lhs_lines,
        &rhs_lines,
    );
    let rhs_meta = build_move_metadata(
        &lhs_arena,
        &rhs_arena,
        &records,
        Side::Rhs,
        &lhs_lines,
        &rhs_lines,
    );

    let mut changes = Vec::new();
    collect_changes(
        &lhs_arena,
        lhs_root,
        &preprocess.lhs_changes,
        &lhs_meta,
        Side::Lhs,
        &mut changes,
    );
    collect_changes(
        &rhs_arena,
        rhs_root,
        &preprocess.rhs_changes,
        &rhs_meta,
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
/// contiguous run of [`Syntax::Atom`] nodes with the same classification.
/// Pending atoms become [`DiffChangeKind::Novel`] runs (later paired
/// into `Replaced` by [`pair_adjacent_replacements`]); atoms tagged
/// Moved by [`find_moves`] or sitting inside a moved subtree become
/// [`DiffChangeKind::Moved`] runs with the shared
/// [`MoveMetadata`] attached. A Moved run never coalesces with a
/// Novel run even if byte-adjacent, and two adjacent Moved runs with
/// different metadata stay separate so downstream callers can offer
/// per-move jump actions.
fn collect_changes(
    arena: &SyntaxArena,
    root: SyntaxId,
    changes: &ChangeMap,
    metadata: &HashMap<SyntaxId, Arc<MoveMetadata>>,
    side: Side,
    out: &mut Vec<DiffChange>,
) {
    let mut current: Option<(DiffChangeKind, Option<Arc<MoveMetadata>>, Range<usize>)> = None;
    walk_emit_atoms(
        arena,
        root,
        changes,
        metadata,
        &mut |kind, meta, byte_range| match &mut current {
            Some((cur_kind, cur_meta, run))
                if *cur_kind == kind
                    && arc_opt_eq(cur_meta, &meta)
                    && run.end == byte_range.start =>
            {
                run.end = byte_range.end;
            },
            Some((cur_kind, cur_meta, run)) => {
                out.push(DiffChange {
                    side,
                    byte_range: run.clone(),
                    kind: *cur_kind,
                    move_metadata: cur_meta.clone(),
                    pair_id: None,
                    deletion_rhs_anchor: None,
                });
                *cur_kind = kind;
                *cur_meta = meta;
                *run = byte_range;
            },
            None => current = Some((kind, meta, byte_range)),
        },
    );
    if let Some((kind, meta, run)) = current {
        out.push(DiffChange {
            side,
            byte_range: run,
            kind,
            move_metadata: meta,
            pair_id: None,
            deletion_rhs_anchor: None,
        });
    }
}

fn arc_opt_eq(a: &Option<Arc<MoveMetadata>>, b: &Option<Arc<MoveMetadata>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => Arc::ptr_eq(x, y),
        _ => false,
    }
}

fn walk_emit_atoms(
    arena: &SyntaxArena,
    id: SyntaxId,
    changes: &ChangeMap,
    metadata: &HashMap<SyntaxId, Arc<MoveMetadata>>,
    callback: &mut impl FnMut(DiffChangeKind, Option<Arc<MoveMetadata>>, Range<usize>),
) {
    match arena.get(id) {
        Syntax::Atom(atom) => {
            if atom.byte_range.start >= atom.byte_range.end {
                return;
            }
            match changes.get(id) {
                ChangeKind::Moved => {
                    if let Some(meta) = metadata.get(&id) {
                        callback(
                            DiffChangeKind::Moved,
                            Some(meta.clone()),
                            atom.byte_range.clone(),
                        );
                    }
                },
                ChangeKind::Pending => {
                    callback(DiffChangeKind::Novel, None, atom.byte_range.clone());
                },
                ChangeKind::Unchanged => {},
            }
        },
        Syntax::List(list) => {
            for child in &list.children {
                walk_emit_atoms(arena, *child, changes, metadata, callback);
            }
        },
    }
}

/// Convert a sorted [`DiffChange`] list so that an Lhs Novel run that
/// is followed (in source order across the two inputs) by an Rhs Novel
/// run becomes a `Replaced` pair. The line-diff path uses an explicit
/// op stream to do this; the structural path discovers it post-hoc by
/// adjacency in `(side, byte_range.start)` order. Moved entries are
/// excluded: their pairing is already resolved in the `move_metadata`
/// side-table and is independent of Novel/Replaced adjacency.
fn pair_adjacent_replacements(changes: &mut [DiffChange]) {
    let lhs_indices: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.side == Side::Lhs && c.kind == DiffChangeKind::Novel)
        .map(|(i, _)| i)
        .collect();
    let rhs_indices: Vec<usize> = changes
        .iter()
        .enumerate()
        .filter(|(_, c)| c.side == Side::Rhs && c.kind == DiffChangeKind::Novel)
        .map(|(i, _)| i)
        .collect();
    let pair_count = lhs_indices.len().min(rhs_indices.len());
    for k in 0..pair_count {
        changes[lhs_indices[k]].kind = DiffChangeKind::Replaced;
        changes[rhs_indices[k]].kind = DiffChangeKind::Replaced;
        changes[lhs_indices[k]].pair_id = Some(k as u32);
        changes[rhs_indices[k]].pair_id = Some(k as u32);
    }
}

/// Per-arena map of `SyntaxId` to the shared [`MoveMetadata`] for the
/// subtree it belongs to. Produced by [`build_move_metadata`]; consumed
/// by [`collect_changes`] to classify each emitted atom.
type MoveMetadataMap = HashMap<SyntaxId, Arc<MoveMetadata>>;

/// Expand the [`MoveRecord`] list into one per-atom metadata map for
/// one side of the diff. For each record:
///
/// - On the RHS side, the target subtree's descendants all carry metadata listing the LHS sources
///   (multiple entries = ambiguous).
/// - On the LHS side, each source subtree carries metadata pointing back at the record's RHS
///   target. In the 1:N duplication case, a single LHS source appears in multiple records; those
///   records' RHS targets are accumulated into the source's metadata.sources so the caller can jump
///   to any of the duplicate targets.
///
/// The returned map keys every descendant of the move root, not just
/// the root itself, because the per-atom walk in [`collect_changes`]
/// needs O(1) lookup on the leaf.
fn build_move_metadata(
    lhs_arena: &SyntaxArena,
    rhs_arena: &SyntaxArena,
    records: &[MoveRecord],
    side: Side,
    lhs_lines: &LineIndex,
    rhs_lines: &LineIndex,
) -> MoveMetadataMap {
    let mut roots_with_sources: HashMap<SyntaxId, Vec<MoveSource>> = HashMap::new();

    for record in records {
        match side {
            Side::Rhs => {
                let sources: Vec<MoveSource> = record
                    .lhs_sources
                    .iter()
                    .map(|id| move_source_for(lhs_arena, *id, Side::Lhs, lhs_lines))
                    .collect();
                roots_with_sources
                    .entry(record.rhs_target)
                    .or_insert_with(Vec::new)
                    .extend(sources);
            },
            Side::Lhs => {
                let target_source =
                    move_source_for(rhs_arena, record.rhs_target, Side::Rhs, rhs_lines);
                for src in &record.lhs_sources {
                    roots_with_sources
                        .entry(*src)
                        .or_insert_with(Vec::new)
                        .push(target_source.clone());
                }
            },
        }
    }

    let mut out: MoveMetadataMap = HashMap::new();
    let arena = match side {
        Side::Lhs => lhs_arena,
        Side::Rhs => rhs_arena,
    };
    for (root, sources) in roots_with_sources {
        let meta = Arc::new(MoveMetadata { sources });
        walk_descendants(arena, root, &mut |id| {
            out.insert(id, meta.clone());
        });
    }
    out
}

fn walk_descendants(arena: &SyntaxArena, root: SyntaxId, f: &mut impl FnMut(SyntaxId)) {
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        f(id);
        if let Syntax::List(list) = arena.get(id) {
            stack.extend(list.children.iter().copied());
        }
    }
}

fn move_source_for(arena: &SyntaxArena, id: SyntaxId, side: Side, lines: &LineIndex) -> MoveSource {
    let byte_range = full_byte_range(arena, id);
    let line_range = lines.lines_for(&byte_range);
    MoveSource {
        buffer: None,
        side,
        byte_range,
        line_range,
    }
}

fn full_byte_range(arena: &SyntaxArena, id: SyntaxId) -> Range<usize> {
    match arena.get(id) {
        Syntax::Atom(a) => a.byte_range.clone(),
        Syntax::List(l) => {
            let start = if l.open_byte_range.start < l.open_byte_range.end {
                l.open_byte_range.start
            } else {
                l.children
                    .first()
                    .map(|c| full_byte_range(arena, *c).start)
                    .unwrap_or(0)
            };
            let end = if l.close_byte_range.start < l.close_byte_range.end {
                l.close_byte_range.end
            } else {
                l.children
                    .last()
                    .map(|c| full_byte_range(arena, *c).end)
                    .unwrap_or(start)
            };
            start..end
        },
    }
}

/// Byte-offset to 0-based line-number index. Precomputed once per side
/// so every move-metadata lookup is two O(log n) binary searches.
struct LineIndex {
    /// Byte offset at the start of each line (line 0 starts at offset 0).
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut line_starts = vec![0usize];
        for (idx, byte) in text.bytes().enumerate() {
            if byte == b'\n' {
                line_starts.push(idx + 1);
            }
        }
        Self { line_starts }
    }

    fn line_of(&self, offset: usize) -> u32 {
        let idx = self
            .line_starts
            .binary_search(&offset)
            .unwrap_or_else(|insert| insert.saturating_sub(1));
        idx as u32
    }

    fn lines_for(&self, byte_range: &Range<usize>) -> Range<u32> {
        let start = self.line_of(byte_range.start);
        let end_byte = byte_range.end.saturating_sub(1).max(byte_range.start);
        let end_inclusive = self.line_of(end_byte);
        start..(end_inclusive + 1)
    }
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

    #[test]
    fn straight_move_integration() {
        // Two swapped functions: one pairs as Unchanged via LCS, the
        // other emerges as a Moved DiffChange with populated metadata.
        let lang = rust_lang();
        let lhs = "fn alpha() { let x = 1; let y = 2; let z = 3; }\n\
                   fn beta() { let p = 10; let q = 20; let r = 30; }";
        let rhs = "fn beta() { let p = 10; let q = 20; let r = 30; }\n\
                   fn alpha() { let x = 1; let y = 2; let z = 3; }";
        let result = diff_with_language(&lang, lhs, rhs).unwrap();
        assert!(!result.fell_back_to_line_diff);

        let moved_rhs: Vec<&DiffChange> = result
            .changes
            .iter()
            .filter(|c| c.side == Side::Rhs && c.kind == DiffChangeKind::Moved)
            .collect();
        assert!(
            !moved_rhs.is_empty(),
            "RHS must emit at least one Moved DiffChange"
        );

        for change in &moved_rhs {
            let meta = change
                .move_metadata
                .as_ref()
                .expect("Moved DiffChange must carry MoveMetadata");
            assert!(
                !meta.sources.is_empty(),
                "Moved metadata must list >= 1 source"
            );
            for src in &meta.sources {
                assert_eq!(src.side, Side::Lhs, "RHS moves point back to LHS sources");
                assert!(
                    src.line_range.end > src.line_range.start,
                    "line_range non-empty"
                );
            }
        }

        // Multi-atom move emits many per-atom DiffChanges that all
        // share one Arc<MoveMetadata>; check that at least two
        // Moved DiffChanges on this side are ptr-eq to prove
        // subtree-level metadata sharing.
        let first_meta = moved_rhs[0].move_metadata.as_ref().unwrap().clone();
        let sharing = moved_rhs
            .iter()
            .filter(|c| Arc::ptr_eq(c.move_metadata.as_ref().unwrap(), &first_meta))
            .count();
        assert!(
            sharing >= 2,
            "multi-atom move must emit multiple DiffChanges sharing one metadata Arc"
        );

        // A source byte_range must cover the moved function's body
        // (signature identifier + body are captured by the paired
        // function_item List; the `fn` keyword itself lives outside
        // the open_byte_range that `full_byte_range` tracks, so it
        // may or may not be included depending on lowering).
        let source_text = &lhs[first_meta.sources[0].byte_range.clone()];
        let recognizable = source_text.contains("alpha()") || source_text.contains("beta()");
        assert!(
            recognizable,
            "move source must span a moved function's signature-and-body; got {source_text:?}"
        );

        let moved_lhs: Vec<&DiffChange> = result
            .changes
            .iter()
            .filter(|c| c.side == Side::Lhs && c.kind == DiffChangeKind::Moved)
            .collect();
        assert!(
            !moved_lhs.is_empty(),
            "LHS must emit at least one Moved DiffChange"
        );
        for change in &moved_lhs {
            let meta = change
                .move_metadata
                .as_ref()
                .expect("Moved DiffChange must carry MoveMetadata");
            for src in &meta.sources {
                assert_eq!(
                    src.side,
                    Side::Rhs,
                    "LHS moves point forward to RHS targets"
                );
            }
        }
    }

    #[test]
    fn graph_limit_bailout_still_finds_moves() {
        // Force the Dijkstra graph cap by diffing a huge pair of
        // inputs. Even when the search bails to preprocessing-only,
        // the move pass still runs on whatever Pending nodes remain
        // and finds the one unambiguous function-level move.
        let lang = rust_lang();

        let mut lhs = String::new();
        let mut rhs = String::new();
        // Many unique functions to force the graph cap.
        for i in 0..400 {
            lhs.push_str(&format!(
                "fn f_{i}(x: u32, y: u32, z: u32) -> u32 {{ x + y + z }}\n"
            ));
            rhs.push_str(&format!(
                "fn f_{i}(x: u32, y: u32, z: u32) -> u32 {{ x + y + z }}\n"
            ));
        }
        // One clean move: the moved function only appears on one side
        // at one position.
        lhs.push_str("fn moved_payload(a: u32) -> u32 { a * 2 + a * 3 + a * 5 }\n");
        rhs.insert_str(
            0,
            "fn moved_payload(a: u32) -> u32 { a * 2 + a * 3 + a * 5 }\n",
        );

        let result = diff_with_language(&lang, &lhs, &rhs).unwrap();
        // Even if the Dijkstra search bailed, the preprocessing pass
        // plus the move pass should have tagged the relocated function.
        let moved_change = result.changes.iter().find(|c| {
            c.kind == DiffChangeKind::Moved
                && (lhs[c.byte_range.clone()].contains("moved_payload")
                    || rhs[c.byte_range.clone()].contains("moved_payload"))
        });
        assert!(
            moved_change.is_some(),
            "move pass must find the relocated function even under graph cap; got {} changes",
            result.changes.len()
        );
    }

    #[test]
    fn cancellation_flag_returns_preprocessing_only_result() {
        // Setting the cancel flag before the first vertex expansion
        // should not prevent a result; Dijkstra bails on the first poll
        // and we fall through to preprocessing + moves. The output may
        // be coarser, but it is still a valid DiffResult.
        let lang = rust_lang();
        let cancel = AtomicBool::new(true);
        let lhs = "fn a() { call(arg1, arg2, arg3); }";
        let rhs = "fn b() { call(arg1, arg2, arg3); }";
        let result = diff_with_language_cancellable(&lang, lhs, rhs, Some(&cancel))
            .expect("parse succeeds even on cancel");
        assert!(!result.fell_back_to_line_diff);
        // The structural-diff pipeline completed; we got changes even
        // though Dijkstra bailed immediately.
        assert!(!result.changes.is_empty());
    }
}
