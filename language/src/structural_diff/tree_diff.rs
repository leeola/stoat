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
    line_diff,
    lower::lower_tree,
    moves::{find_moves_changeset, ChangesetMoveRecord, FileMoveInput},
    sliders::fix_all_sliders,
    unchanged::{mark_unchanged, ChangeKind, ChangeMap},
    BufferRef, ChangeKind as DiffChangeKind, DiffChange, DiffResult, FileDiffInput, MoveMetadata,
    MoveSource, Side,
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
    let mut prepared = prepare_per_file(language, lhs, rhs, cancel)?;

    let records = {
        let mut input = [FileMoveInput {
            lhs_arena: &prepared.lhs_arena,
            rhs_arena: &prepared.rhs_arena,
            lhs_changes: &mut prepared.lhs_changes,
            rhs_changes: &mut prepared.rhs_changes,
        }];
        find_moves_changeset(&mut input)
    };

    let no_buffer: [Option<&BufferRef>; 1] = [None];
    let view_borrowed: [PreparedFileView<'_>; 1] = [PreparedFileView {
        lhs_arena: &prepared.lhs_arena,
        rhs_arena: &prepared.rhs_arena,
        lhs_lines: &prepared.lhs_lines,
        rhs_lines: &prepared.rhs_lines,
    }];
    Some(finalize_per_file(
        &prepared,
        0,
        &records,
        &no_buffer,
        &view_borrowed,
    ))
}

/// Run [`diff_with_language`]-quality diffs across a multi-file
/// changeset. Move detection runs over the union of all per-file
/// arenas so a function migrating from `inputs[i].buffer` to
/// `inputs[j].buffer` is tagged [`super::ChangeKind::Moved`] (with
/// [`MoveSource::buffer`] identifying the counterpart file) instead
/// of surfacing as a deletion in `i` plus an addition in `j`.
///
/// Returns one [`DiffResult`] per input in input order. Inputs whose
/// `language` is `None` (or whose parse fails) fall back to
/// [`super::diff_lines`] for that file and do not participate in
/// cross-file move detection.
pub fn diff_changeset(inputs: Vec<FileDiffInput<'_>>) -> Vec<DiffResult> {
    enum Slot<'a> {
        Structural(PreparedFile),
        LineDiff {
            lhs_text: &'a str,
            rhs_text: &'a str,
        },
    }

    let mut slots: Vec<Slot<'_>> = inputs
        .into_iter()
        .map(|input| match input.language.as_ref() {
            Some(lang) => match prepare_per_file(lang, input.lhs_text, input.rhs_text, None) {
                Some(prepared) => Slot::Structural(PreparedFile {
                    buffer: input.buffer,
                    ..prepared
                }),
                None => Slot::LineDiff {
                    lhs_text: input.lhs_text,
                    rhs_text: input.rhs_text,
                },
            },
            None => Slot::LineDiff {
                lhs_text: input.lhs_text,
                rhs_text: input.rhs_text,
            },
        })
        .collect();

    let cs_records = {
        let mut move_inputs: Vec<FileMoveInput<'_>> = Vec::new();
        for slot in slots.iter_mut() {
            if let Slot::Structural(p) = slot {
                move_inputs.push(FileMoveInput {
                    lhs_arena: &p.lhs_arena,
                    rhs_arena: &p.rhs_arena,
                    lhs_changes: &mut p.lhs_changes,
                    rhs_changes: &mut p.rhs_changes,
                });
            }
        }
        find_moves_changeset(&mut move_inputs)
    };

    // Build per-file index map (slot index -> structural index in cs_records' tuples).
    // cs_records refer to file indices in the move_inputs slice, which only contains
    // Structural slots in slot-iteration order. Map each Structural slot's iteration
    // index back so finalize_per_file can identify "this file's records".
    let structural_idx: Vec<Option<usize>> = {
        let mut next_struct = 0usize;
        slots
            .iter()
            .map(|slot| match slot {
                Slot::Structural(_) => {
                    let i = next_struct;
                    next_struct += 1;
                    Some(i)
                },
                Slot::LineDiff { .. } => None,
            })
            .collect()
    };

    let buffer_refs: Vec<Option<&BufferRef>> = slots
        .iter()
        .filter_map(|slot| match slot {
            Slot::Structural(p) => Some(Some(&p.buffer)),
            Slot::LineDiff { .. } => None,
        })
        .collect();

    let views: Vec<PreparedFileView<'_>> = slots
        .iter()
        .filter_map(|slot| match slot {
            Slot::Structural(p) => Some(PreparedFileView {
                lhs_arena: &p.lhs_arena,
                rhs_arena: &p.rhs_arena,
                lhs_lines: &p.lhs_lines,
                rhs_lines: &p.rhs_lines,
            }),
            Slot::LineDiff { .. } => None,
        })
        .collect();

    slots
        .iter()
        .enumerate()
        .map(|(slot_idx, slot)| match slot {
            Slot::Structural(p) => {
                let my_struct_idx = structural_idx[slot_idx].expect("structural slot");
                finalize_per_file(p, my_struct_idx, &cs_records, &buffer_refs, &views)
            },
            Slot::LineDiff { lhs_text, rhs_text } => line_diff::diff_lines(lhs_text, rhs_text),
        })
        .collect()
}

/// Per-file output of [`prepare_per_file`]: the parsed-and-preprocessed
/// state that the move pass mutates and that
/// [`finalize_per_file`] consumes to emit a [`DiffResult`].
struct PreparedFile {
    buffer: BufferRef,
    lhs_arena: SyntaxArena,
    rhs_arena: SyntaxArena,
    lhs_root: SyntaxId,
    rhs_root: SyntaxId,
    lhs_changes: ChangeMap,
    rhs_changes: ChangeMap,
    lhs_lines: LineIndex,
    rhs_lines: LineIndex,
}

/// Borrowed view of the arenas + line indices for one prepared file.
/// Used by [`build_move_metadata_changeset`] when materialising
/// [`MoveSource`]s for cross-file moves.
struct PreparedFileView<'a> {
    lhs_arena: &'a SyntaxArena,
    rhs_arena: &'a SyntaxArena,
    lhs_lines: &'a LineIndex,
    rhs_lines: &'a LineIndex,
}

/// Run the parse / lower / preprocess / Dijkstra / slider stages for
/// one file. Returns `None` if either side fails to parse, mirroring
/// the existing [`diff_with_language`] fallback contract -- the caller
/// then routes that file through [`super::diff_lines`].
///
/// `buffer` is filled in by [`diff_changeset`] after this returns; the
/// single-file [`diff_with_language_cancellable`] caller substitutes a
/// placeholder it never observes.
fn prepare_per_file(
    language: &Arc<Language>,
    lhs: &str,
    rhs: &str,
    cancel: Option<&AtomicBool>,
) -> Option<PreparedFile> {
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

    Some(PreparedFile {
        buffer: BufferRef {
            path: std::path::PathBuf::new(),
            fingerprint: [0u8; 32],
        },
        lhs_arena,
        rhs_arena,
        lhs_root,
        rhs_root,
        lhs_changes: preprocess.lhs_changes,
        rhs_changes: preprocess.rhs_changes,
        lhs_lines: LineIndex::new(lhs),
        rhs_lines: LineIndex::new(rhs),
    })
}

/// Convert a prepared file's residual change maps and the changeset's
/// move records into a [`DiffResult`]. `my_idx` is this file's index
/// in the structural slice (`buffer_refs` and `views`); records whose
/// `rhs_target.0` or any `lhs_sources[*].0` equals `my_idx` contribute
/// metadata for this file.
fn finalize_per_file(
    prepared: &PreparedFile,
    my_idx: usize,
    cs_records: &[ChangesetMoveRecord],
    buffer_refs: &[Option<&BufferRef>],
    views: &[PreparedFileView<'_>],
) -> DiffResult {
    let lhs_meta = build_move_metadata_changeset(cs_records, my_idx, Side::Lhs, buffer_refs, views);
    let rhs_meta = build_move_metadata_changeset(cs_records, my_idx, Side::Rhs, buffer_refs, views);

    let mut changes = Vec::new();
    collect_changes(
        &prepared.lhs_arena,
        prepared.lhs_root,
        &prepared.lhs_changes,
        &lhs_meta,
        Side::Lhs,
        &mut changes,
    );
    collect_changes(
        &prepared.rhs_arena,
        prepared.rhs_root,
        &prepared.rhs_changes,
        &rhs_meta,
        Side::Rhs,
        &mut changes,
    );
    changes.sort_by_key(|c| (c.byte_range.start, c.byte_range.end));
    pair_adjacent_replacements(&mut changes);

    DiffResult {
        changes,
        fell_back_to_line_diff: false,
    }
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

/// Cross-file analogue of the legacy `build_move_metadata` helper.
/// Walks the changeset's records and emits a per-atom metadata map
/// for `my_idx`'s side `side`. Cross-file [`MoveSource`]s carry the
/// source file's [`BufferRef`]; intra-file (`my_idx == other_idx`)
/// sources keep `buffer: None` per the
/// [`super::MoveSource`] documented invariant.
fn build_move_metadata_changeset(
    cs_records: &[ChangesetMoveRecord],
    my_idx: usize,
    side: Side,
    buffer_refs: &[Option<&BufferRef>],
    views: &[PreparedFileView<'_>],
) -> MoveMetadataMap {
    let mut roots_with_sources: HashMap<SyntaxId, Vec<MoveSource>> = HashMap::new();

    for record in cs_records {
        match side {
            Side::Rhs => {
                if record.rhs_target.0 != my_idx {
                    continue;
                }
                let sources: Vec<MoveSource> = record
                    .lhs_sources
                    .iter()
                    .map(|(src_idx, src_id)| {
                        move_source_for_changeset(
                            *src_idx,
                            *src_id,
                            Side::Lhs,
                            my_idx,
                            buffer_refs,
                            views,
                        )
                    })
                    .collect();
                roots_with_sources
                    .entry(record.rhs_target.1)
                    .or_default()
                    .extend(sources);
            },
            Side::Lhs => {
                let target_source = move_source_for_changeset(
                    record.rhs_target.0,
                    record.rhs_target.1,
                    Side::Rhs,
                    my_idx,
                    buffer_refs,
                    views,
                );
                for (src_idx, src_id) in &record.lhs_sources {
                    if *src_idx != my_idx {
                        continue;
                    }
                    roots_with_sources
                        .entry(*src_id)
                        .or_default()
                        .push(target_source.clone());
                }
            },
        }
    }

    let mut out: MoveMetadataMap = HashMap::new();
    let arena = match side {
        Side::Lhs => views[my_idx].lhs_arena,
        Side::Rhs => views[my_idx].rhs_arena,
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

/// Build a [`MoveSource`] pointing at `(src_idx, src_id)` on `side`.
/// `viewer_idx` is the file the [`MoveMetadata`] is being assembled
/// for; when `src_idx == viewer_idx` the move is intra-file and
/// `buffer` is `None` (documented invariant). When `src_idx !=
/// viewer_idx` the source lives in a different file and `buffer` is
/// `Some(buffer_refs[src_idx])`.
fn move_source_for_changeset(
    src_idx: usize,
    src_id: SyntaxId,
    side: Side,
    viewer_idx: usize,
    buffer_refs: &[Option<&BufferRef>],
    views: &[PreparedFileView<'_>],
) -> MoveSource {
    let view = &views[src_idx];
    let (arena, lines): (&SyntaxArena, &LineIndex) = match side {
        Side::Lhs => (view.lhs_arena, view.lhs_lines),
        Side::Rhs => (view.rhs_arena, view.rhs_lines),
    };
    let byte_range = full_byte_range(arena, src_id);
    let line_range = lines.lines_for(&byte_range);
    let buffer = if src_idx == viewer_idx {
        None
    } else {
        buffer_refs[src_idx].cloned()
    };
    MoveSource {
        buffer,
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

    #[test]
    fn changeset_detects_cross_file_function_migration() {
        let lang = rust_lang();
        let buf_a = BufferRef {
            path: std::path::PathBuf::from("a.rs"),
            fingerprint: [1u8; 32],
        };
        let buf_b = BufferRef {
            path: std::path::PathBuf::from("b.rs"),
            fingerprint: [2u8; 32],
        };

        let inputs = vec![
            FileDiffInput {
                buffer: buf_a.clone(),
                language: Some(lang.clone()),
                lhs_text: "fn relocated() { let x = 1; let y = 2; let z = 3; }\n\
                           fn stays_a() { call_a(); }",
                rhs_text: "fn stays_a() { call_a(); }",
            },
            FileDiffInput {
                buffer: buf_b.clone(),
                language: Some(lang.clone()),
                lhs_text: "fn stays_b() { call_b(); }",
                rhs_text: "fn stays_b() { call_b(); }\n\
                           fn relocated() { let x = 1; let y = 2; let z = 3; }",
            },
        ];

        let results = diff_changeset(inputs);
        assert_eq!(results.len(), 2);

        let file_a_lhs_moved: Vec<&DiffChange> = results[0]
            .changes
            .iter()
            .filter(|c| c.side == Side::Lhs && c.kind == DiffChangeKind::Moved)
            .collect();
        assert!(
            !file_a_lhs_moved.is_empty(),
            "file 0 LHS must contain Moved hunks for the migrated function; got {:?}",
            results[0].changes
        );
        let meta = file_a_lhs_moved[0]
            .move_metadata
            .as_ref()
            .expect("Moved hunk must carry metadata");
        assert!(
            meta.sources
                .iter()
                .any(|s| s.buffer.as_ref() == Some(&buf_b)),
            "file 0 LHS Moved hunk must point at file 1 via BufferRef; got sources {:?}",
            meta.sources
        );

        let file_b_rhs_moved: Vec<&DiffChange> = results[1]
            .changes
            .iter()
            .filter(|c| c.side == Side::Rhs && c.kind == DiffChangeKind::Moved)
            .collect();
        assert!(
            !file_b_rhs_moved.is_empty(),
            "file 1 RHS must contain Moved hunks for the migrated function; got {:?}",
            results[1].changes
        );
        let meta = file_b_rhs_moved[0]
            .move_metadata
            .as_ref()
            .expect("Moved hunk must carry metadata");
        assert!(
            meta.sources
                .iter()
                .any(|s| s.buffer.as_ref() == Some(&buf_a)),
            "file 1 RHS Moved hunk must point at file 0 via BufferRef; got sources {:?}",
            meta.sources
        );
    }

    #[test]
    fn changeset_intra_file_move_keeps_buffer_none() {
        // A move within a single file must keep MoveSource.buffer = None
        // even when invoked through the multi-file API. The documented
        // invariant on MoveSource is that intra-file moves carry no
        // BufferRef.
        let lang = rust_lang();
        let inputs = vec![FileDiffInput {
            buffer: BufferRef {
                path: std::path::PathBuf::from("a.rs"),
                fingerprint: [1u8; 32],
            },
            language: Some(lang),
            lhs_text: "fn alpha() { let x = 1; let y = 2; let z = 3; }\n\
                       fn beta() { let p = 10; let q = 20; let r = 30; }",
            rhs_text: "fn beta() { let p = 10; let q = 20; let r = 30; }\n\
                       fn alpha() { let x = 1; let y = 2; let z = 3; }",
        }];

        let results = diff_changeset(inputs);
        assert_eq!(results.len(), 1);
        let moved: Vec<&DiffChange> = results[0]
            .changes
            .iter()
            .filter(|c| c.kind == DiffChangeKind::Moved)
            .collect();
        assert!(
            !moved.is_empty(),
            "intra-file swap must produce Moved hunks"
        );
        for change in &moved {
            let meta = change.move_metadata.as_ref().expect("metadata required");
            for source in &meta.sources {
                assert!(
                    source.buffer.is_none(),
                    "intra-file MoveSource must have buffer=None; got {source:?}"
                );
            }
        }
    }
}
