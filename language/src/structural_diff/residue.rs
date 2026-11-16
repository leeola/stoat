//! Moves between files that the editor diffs one file at a time.
//!
//! The editor runs one diff job per file, and no job keeps the [`PreparedDiff`]
//! of its file for the other jobs, because a prepared diff borrows both texts.
//! Each job exports a [`MoveResidue`] instead, an owned copy of the subtrees
//! that its diff paired with nothing. [`pair_residues`] then pairs the
//! residues of several files on a thread of its own.

use super::{
    arena::{Atom, List, Syntax, SyntaxArena, SyntaxId},
    moves::{find_moves_changeset, precompute_has_pending, FileMoveInput},
    tree_diff::{
        build_move_metadata_changeset, collect_changes, finalize_single_parts, LineIndex,
        PreparedFileView,
    },
    unchanged::ChangeMap,
    BufferRef, ChangeKind, DiffChange, DiffResult, MatchedPairs, PreparedDiff, Side,
};

/// The subtrees of one file's diff that paired with nothing, kept to pair with
/// the code that other files lost or gained.
///
/// A file diffed alone does not see that its removed code arrived in another
/// file, so both halves read as a removal and an addition. A residue holds
/// only the nodes above an unpaired token, with the byte ranges and the
/// content ids of the whole file. [`pair_residues`] pairs it with the residues
/// of other files after the diff drops its texts.
///
/// A list keeps only the children that hold an unpaired token, so the leaf
/// count of a list in a residue is the count of its kept leaves. Its content
/// id still names its whole subtree.
pub struct MoveResidue {
    lhs: ResidueSide,
    rhs: ResidueSide,
}

/// One side of a [`MoveResidue`].
struct ResidueSide {
    arena: SyntaxArena,
    /// The copy of the side's root, or `None` when every token of the side
    /// paired.
    root: Option<SyntaxId>,
    lines: LineIndex,
}

impl ResidueSide {
    /// Copy the nodes of `arena` that hold an atom which `changes` leaves
    /// pending.
    ///
    /// The lowering allocates a child before its parent, and the parent of a
    /// kept node holds the same pending atom. So one pass in id order copies
    /// each child before its parent, and the last copy is the root.
    fn of(arena: &SyntaxArena, changes: &ChangeMap, lines: LineIndex) -> Self {
        let pending = precompute_has_pending(arena, changes);
        let mut copy = arena.with_same_source(pending.iter().filter(|&&kept| kept).count());
        let mut copied: Vec<Option<SyntaxId>> = vec![None; arena.len()];
        let mut root = None;

        for (i, &kept) in pending.iter().enumerate() {
            if !kept {
                continue;
            }
            let node = match arena.get(SyntaxId(i)) {
                Syntax::Atom(atom) => Syntax::Atom(Atom {
                    kind: atom.kind,
                    class: atom.class,
                    byte_range: atom.byte_range.clone(),
                    content_id: atom.content_id,
                    next_sibling: None,
                }),
                Syntax::List(list) => Syntax::List(List {
                    kind: list.kind,
                    open_byte_range: list.open_byte_range.clone(),
                    close_byte_range: list.close_byte_range.clone(),
                    children: list
                        .children
                        .iter()
                        .filter_map(|child| copied[child.0])
                        .collect(),
                    content_id: list.content_id,
                    layout_id: list.layout_id,
                    next_sibling: None,
                }),
            };
            let id = copy.alloc(node);
            copied[i] = Some(id);
            root = Some(id);
        }

        Self {
            arena: copy,
            root,
            lines,
        }
    }
}

/// Finalize one prepared file as [`super::finalize_single`] does, and export
/// the subtrees that it paired with nothing.
///
/// The residue owns its copies, so it outlives `prepared` and the texts that
/// `prepared` borrows.
pub fn finalize_single_with_residue(
    prepared: &PreparedDiff<'_>,
    pairs: MatchedPairs,
) -> (DiffResult, MoveResidue) {
    let (result, lhs_changes, rhs_changes) = finalize_single_parts(prepared, pairs);
    let p = &prepared.prepared;
    let residue = MoveResidue {
        lhs: ResidueSide::of(&p.lhs_arena, &lhs_changes, p.lhs_lines.clone()),
        rhs: ResidueSide::of(&p.rhs_arena, &rhs_changes, p.rhs_lines.clone()),
    };
    (result, residue)
}

/// Pair the residues of several files, and answer the code of each file that
/// moved to or from a residue.
///
/// Each answer holds only [`ChangeKind::Moved`] changes, the left side's
/// first, and each side in byte order. A file with no move answers an empty
/// list. A source in another file names that file's [`BufferRef`]. A source
/// in the same file names none, as the source of an in-file move does.
pub fn pair_residues(files: &[(&BufferRef, &MoveResidue)]) -> Vec<Vec<DiffChange>> {
    let mut maps: Vec<[ChangeMap; 2]> = files
        .iter()
        .map(|(_, residue)| {
            [
                ChangeMap::with_len(residue.lhs.arena.len()),
                ChangeMap::with_len(residue.rhs.arena.len()),
            ]
        })
        .collect();
    let records = {
        let mut inputs: Vec<FileMoveInput<'_>> = files
            .iter()
            .zip(maps.iter_mut())
            .map(|((_, residue), [lhs_changes, rhs_changes])| FileMoveInput {
                lhs_arena: &residue.lhs.arena,
                rhs_arena: &residue.rhs.arena,
                lhs_changes,
                rhs_changes,
            })
            .collect();
        find_moves_changeset(&mut inputs)
    };

    let buffers: Vec<Option<&BufferRef>> = files.iter().map(|&(buffer, _)| Some(buffer)).collect();
    let views: Vec<PreparedFileView<'_>> = files
        .iter()
        .map(|(_, residue)| PreparedFileView {
            lhs_arena: &residue.lhs.arena,
            rhs_arena: &residue.rhs.arena,
            lhs_lines: &residue.lhs.lines,
            rhs_lines: &residue.rhs.lines,
        })
        .collect();

    files
        .iter()
        .zip(&maps)
        .enumerate()
        .map(|(idx, ((_, residue), [lhs_changes, rhs_changes]))| {
            let mut moved = Vec::new();
            for (side, half, changes) in [
                (Side::Lhs, &residue.lhs, lhs_changes),
                (Side::Rhs, &residue.rhs, rhs_changes),
            ] {
                let Some(root) = half.root else {
                    continue;
                };
                let metadata = build_move_metadata_changeset(&records, idx, side, &buffers, &views);
                let mut collected = Vec::new();
                collect_changes(&half.arena, root, changes, &metadata, side, &mut collected);
                moved.extend(
                    collected
                        .into_iter()
                        .filter(|change| change.kind == ChangeKind::Moved),
                );
            }
            moved
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{finalize_single_with_residue, pair_residues};
    use crate::{
        structural_diff::{
            prepare_diff, BufferRef, ChangeKind, DiffChange, DiffResult, MatchedPairs, MoveSource,
            Side,
        },
        test_util::rust_lang,
    };

    /// A test file of `n` tests that share one helper.
    fn tests_file(n: usize) -> String {
        let cases: String = (1..=n)
            .map(|i| {
                format!(
                    "#[test]
fn scan_case_{i}() {{
    let found = scan(&rows({i}), &query(\"level >= {i}\"));
    let by_day: BTreeMap<u32, usize> = found.iter().map(|row| (row.day, row.index)).collect();

    assert_eq!(by_day.len(), {i}, \"rows of case {i}\");
}}

"
                )
            })
            .collect();
        format!(
            "use super::{{scan, Query, Row}};

/// Rows of one day.
fn rows(day: u32) -> Vec<Row> {{
    (0..4).map(|i| Row::new(day, i, i * 10)).collect()
}}

{cases}"
        )
    }

    /// `body` as the `tests` module of a file that holds `scan`, with each
    /// non-blank line indented four spaces.
    fn in_module(body: &str) -> String {
        let indented: String = body
            .lines()
            .map(|line| match line.is_empty() {
                true => "\n".to_string(),
                false => format!("    {line}\n"),
            })
            .collect();
        format!("pub fn scan() {{}}\n\n#[cfg(test)]\nmod tests {{\n{indented}}}\n")
    }

    /// The buffer of file `i` in [`diff_and_pair`].
    fn buffer_ref(i: usize) -> BufferRef {
        BufferRef {
            path: format!("f{i}.rs").into(),
            fingerprint: [i as u8; 32],
        }
    }

    /// The single-file diff of each pair, and the moves that [`pair_residues`]
    /// finds among their residues, with file `i` as [`buffer_ref`] `i`.
    fn diff_and_pair(files: &[(&str, &str)]) -> (Vec<DiffResult>, Vec<Vec<DiffChange>>) {
        let language = rust_lang();
        let (results, residues): (Vec<_>, Vec<_>) = files
            .iter()
            .map(|&(lhs, rhs)| {
                let prepared =
                    prepare_diff(&language, lhs, rhs, None, None, None).expect("rust parses");
                finalize_single_with_residue(&prepared, MatchedPairs::Skip)
            })
            .unzip();
        let buffers: Vec<BufferRef> = (0..files.len()).map(buffer_ref).collect();
        let pairing = pair_residues(&buffers.iter().zip(&residues).collect::<Vec<_>>());
        (results, pairing)
    }

    /// The 0-based lines of each side of each file that hold a letter, a digit,
    /// or an underscore of a `Novel` or `Replaced` change that no move of the
    /// pairing covers.
    fn unmoved_lines(files: &[(&str, &str)]) -> Vec<(Vec<u32>, Vec<u32>)> {
        let (results, pairing) = diff_and_pair(files);
        let lines = |text: &str, side: Side, result: &DiffResult, moved: &[DiffChange]| {
            let mut lines: Vec<u32> = result
                .changes
                .iter()
                .filter(|change| change.side == side)
                .filter(|change| matches!(change.kind, ChangeKind::Novel | ChangeKind::Replaced))
                .flat_map(|change| change.byte_range.clone())
                .filter(|byte| {
                    !moved
                        .iter()
                        .any(|change| change.side == side && change.byte_range.contains(byte))
                })
                .filter(|&byte| {
                    let ch = text.as_bytes()[byte];
                    ch.is_ascii_alphanumeric() || ch == b'_'
                })
                .map(|byte| text[..byte].matches('\n').count() as u32)
                .collect();
            lines.dedup();
            lines
        };

        files
            .iter()
            .zip(&results)
            .zip(&pairing)
            .map(|((&(lhs, rhs), result), moved)| {
                (
                    lines(lhs, Side::Lhs, result, moved),
                    lines(rhs, Side::Rhs, result, moved),
                )
            })
            .collect()
    }

    /// The 0-based line of `text` that holds `needle`.
    fn line_holding(text: &str, needle: &str) -> u32 {
        text.lines()
            .position(|line| line.contains(needle))
            .expect("text holds the needle") as u32
    }

    #[test]
    fn tests_that_move_into_a_new_module_read_as_moved() {
        let removed = tests_file(12);
        assert_eq!(
            unmoved_lines(&[(&removed, ""), ("pub fn scan() {}\n", &in_module(&removed))]),
            [(vec![], vec![]), (vec![], vec![2, 3])],
            "only the module's attribute and header stay unmoved",
        );
    }

    #[test]
    fn a_module_that_moves_out_to_its_own_file_reads_as_moved() {
        let tests = tests_file(12);
        assert_eq!(
            unmoved_lines(&[(&in_module(&tests), "pub fn scan() {}\n"), ("", &tests)]),
            [(vec![2, 3], vec![]), (vec![], vec![])],
            "only the removed module's attribute and header stay unmoved",
        );
    }

    #[test]
    fn a_call_rewrapped_after_the_move_still_moves() {
        let removed = tests_file(12);
        // A formatter that re-wraps the call puts each argument on its own
        // line, and its vertical form takes a trailing comma.
        let row = |comma: &str| {
            let rewrapped = removed.replace(
                "let found = scan(&rows(3), &query(\"level >= 3\"));",
                &format!(
                    "let found = scan(\n        &rows(3),\n        &query(\"level >= 3\"){comma}\n    );"
                ),
            );
            (
                rewrapped.lines().count() - removed.lines().count(),
                unmoved_lines(&[
                    (&removed, ""),
                    ("pub fn scan() {}\n", &in_module(&rewrapped)),
                ]),
            )
        };
        let moved = (3, vec![(vec![], vec![]), (vec![], vec![2, 3])]);
        assert_eq!(
            [row(""), row(",")],
            [moved.clone(), moved],
            "the call takes three more lines and still moves, with a trailing comma or without",
        );
    }

    #[test]
    fn a_copy_whose_original_stays_pairs_with_nothing() {
        let tests = tests_file(3);
        let (_, pairing) =
            diff_and_pair(&[(&tests, &tests), ("pub fn scan() {}\n", &in_module(&tests))]);
        assert_eq!(
            pairing,
            [vec![], vec![]],
            "the copy is an addition, not a move"
        );
    }

    #[test]
    fn a_renamed_test_leaves_only_its_fn_line_unmoved() {
        let removed = tests_file(12);
        let module = in_module(&removed.replace("fn scan_case_3()", "fn scan_case_three()"));
        assert_eq!(
            unmoved_lines(&[(&removed, ""), ("pub fn scan() {}\n", &module)]),
            [
                (vec![line_holding(&removed, "fn scan_case_3()")], vec![]),
                (
                    vec![],
                    vec![2, 3, line_holding(&module, "fn scan_case_three()")]
                ),
            ],
            "the renamed test's fn line stays unmoved on each side",
        );
    }

    #[test]
    fn a_move_from_another_file_names_that_file() {
        let removed = tests_file(12);
        let (_, pairing) =
            diff_and_pair(&[(&removed, ""), ("pub fn scan() {}\n", &in_module(&removed))]);
        let first_sources = pairing[1]
            .iter()
            .find(|change| change.side == Side::Rhs)
            .and_then(|change| change.move_metadata.as_ref())
            .map(|metadata| metadata.sources.clone());

        let use_super = "use super::{scan, Query, Row};";
        assert_eq!(
            first_sources,
            Some(vec![MoveSource {
                buffer: Some(buffer_ref(0)),
                side: Side::Lhs,
                byte_range: 0..use_super.len(),
                line_range: 0..1,
            }]),
            "the module's first move names the removed file and its `use super` line",
        );
    }
}
