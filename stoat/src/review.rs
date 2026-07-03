use crate::buffer_registry::fingerprint_bytes;
use serde::{Deserialize, Serialize};
use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::{atomic::AtomicBool, Arc},
};
use stoat_language::{
    structural_diff::{
        self, BufferRef, ChangeKind as LangChangeKind, DiffChange, DiffResult, FileDiffInput, Side,
    },
    Language,
};

/// One file's contribution to a review session. Used both by the
/// changeset entry point [`extract_review_hunks_changeset`] and as
/// the input shape for [`crate::review_session::ReviewSession::add_files`].
#[derive(Clone)]
pub struct ReviewFileInput {
    pub path: PathBuf,
    pub rel_path: String,
    pub language: Option<Arc<Language>>,
    pub base_text: Arc<String>,
    pub buffer_text: Arc<String>,
}

/// Cross-file move provenance for a single review row. Set when the
/// row participates in a [`stoat_language::structural_diff::ChangeKind::Moved`]
/// hunk whose [`stoat_language::structural_diff::MoveSource`] points
/// at a *different* file in the same review session. Intra-file moves
/// keep this as `None`. The renderer paints a chip
/// `<- {rel_path}:{line+1}` next to the row when set.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MoveProvenance {
    pub rel_path: String,
    pub line: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewSide {
    pub text: String,
    pub line_num: u32,
    /// Byte ranges (within `text`) that are Novel or Replaced on this
    /// side. Rendered with the side-specific add/delete highlight.
    #[serde(with = "range_vec_codec")]
    pub change_spans: Vec<Range<usize>>,
    /// Byte ranges (within `text`) that are tagged as part of a move:
    /// byte-for-byte equal to content elsewhere, just relocated.
    /// Rendered with the central [`crate::display_map::syntax_theme::DiffTheme`]
    /// move color (cyan by default), not red/green, so users see at
    /// a glance that the change is a relocation rather than a gain or
    /// loss.
    #[serde(with = "range_vec_codec")]
    pub moved_spans: Vec<Range<usize>>,
    /// First cross-file move source covering this row, if any.
    pub move_provenance: Option<MoveProvenance>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReviewRow {
    Context {
        left: ReviewSide,
        right: ReviewSide,
    },
    Changed {
        left: Option<ReviewSide>,
        right: Option<ReviewSide>,
    },
}

impl ReviewRow {
    fn is_changed(&self) -> bool {
        matches!(self, ReviewRow::Changed { .. })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReviewHunk {
    pub rows: Vec<ReviewRow>,
}

mod range_vec_codec {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::ops::Range;

    pub fn serialize<S: Serializer>(v: &[Range<usize>], s: S) -> Result<S::Ok, S::Error> {
        let pairs: Vec<(usize, usize)> = v.iter().map(|r| (r.start, r.end)).collect();
        pairs.serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Range<usize>>, D::Error> {
        let pairs: Vec<(usize, usize)> = Vec::deserialize(d)?;
        Ok(pairs.into_iter().map(|(a, b)| a..b).collect())
    }
}

/// Multi-file entry point that runs one cross-file
/// [`structural_diff::diff_changeset`] over the union of all inputs
/// before extracting per-file hunks. Returns one `Vec<ReviewHunk>`
/// per input in input order. Cross-file [`stoat_language::structural_diff::ChangeKind::Moved`]
/// metadata produced by the diff pass survives into the returned
/// hunks via the existing
/// [`stoat_language::structural_diff::DiffChange::move_metadata`] field
/// and a per-row [`MoveProvenance`] chip that the renderer paints
/// next to cross-file moved rows.
pub fn extract_review_hunks_changeset(
    files: &[ReviewFileInput],
    context: u32,
) -> Vec<Vec<ReviewHunk>> {
    let inputs: Vec<FileDiffInput> = files
        .iter()
        .map(|f| FileDiffInput {
            buffer: BufferRef {
                path: f.path.clone(),
                fingerprint: fingerprint_bytes(&f.buffer_text),
            },
            language: f.language.clone(),
            lhs_text: (*f.base_text).clone(),
            rhs_text: (*f.buffer_text).clone(),
        })
        .collect();

    let diff_results = structural_diff::diff_changeset(inputs);

    files
        .iter()
        .zip(diff_results)
        .map(|(f, diff)| {
            let rel_path_for = |path: &Path| -> Option<String> {
                files
                    .iter()
                    .find(|other| other.path == path)
                    .map(|other| other.rel_path.clone())
            };
            extract_review_hunks_from_diff(
                &diff,
                &f.base_text,
                &f.buffer_text,
                context,
                &rel_path_for,
            )
        })
        .collect()
}

/// Extract review hunks for a single file via the single-file diff pipeline.
///
/// Unlike [`extract_review_hunks_changeset`], this runs no cross-file move
/// pass, so a streaming scan can show one file's hunks before the whole
/// changeset is diffed. Cross-file move provenance never resolves here: a
/// single-file diff produces no cross-file origins, so the callback only
/// answers for this file's own path.
///
/// `cancel` is polled by the structural diff's search. A superseded scan sets
/// it to abandon the in-flight diff, which drops through to a coarse line diff.
pub fn extract_review_hunks_single(
    file: &ReviewFileInput,
    context: u32,
    cancel: Option<&AtomicBool>,
) -> Vec<ReviewHunk> {
    let diff = match &file.language {
        Some(language) => structural_diff::diff_with_language_cancellable(
            language,
            &file.base_text,
            &file.buffer_text,
            cancel,
        )
        .unwrap_or_else(|| structural_diff::diff(&file.base_text, &file.buffer_text)),
        None => structural_diff::diff(&file.base_text, &file.buffer_text),
    };

    let rel_path_for = |path: &Path| (path == file.path.as_path()).then(|| file.rel_path.clone());

    extract_review_hunks_from_diff(
        &diff,
        &file.base_text,
        &file.buffer_text,
        context,
        &rel_path_for,
    )
}

fn extract_review_hunks_from_diff(
    diff_result: &DiffResult,
    base_text: &str,
    buffer_text: &str,
    context: u32,
    rel_path_for: &dyn Fn(&Path) -> Option<String>,
) -> Vec<ReviewHunk> {
    let lhs_lines = split_lines(base_text);
    let rhs_lines = split_lines(buffer_text);

    let lhs_changed = mark_changed_lines(&lhs_lines, &diff_result.changes, Side::Lhs);
    let rhs_changed = mark_changed_lines(&rhs_lines, &diff_result.changes, Side::Rhs);

    let lhs_spans = collect_line_spans(&lhs_lines, &diff_result.changes, Side::Lhs);
    let rhs_spans = collect_line_spans(&rhs_lines, &diff_result.changes, Side::Rhs);
    let lhs_moved = collect_moved_spans(&lhs_lines, &diff_result.changes, Side::Lhs);
    let rhs_moved = collect_moved_spans(&rhs_lines, &diff_result.changes, Side::Rhs);
    let lhs_prov =
        collect_moved_provenance(&lhs_lines, &diff_result.changes, Side::Lhs, rel_path_for);
    let rhs_prov =
        collect_moved_provenance(&rhs_lines, &diff_result.changes, Side::Rhs, rel_path_for);

    let all_rows = structural_walk(
        WalkSide {
            lines: &lhs_lines,
            changed: &lhs_changed,
            spans: &lhs_spans,
            moved: &lhs_moved,
            provenance: &lhs_prov,
        },
        WalkSide {
            lines: &rhs_lines,
            changed: &rhs_changed,
            spans: &rhs_spans,
            moved: &rhs_moved,
            provenance: &rhs_prov,
        },
    );
    extract_hunks_with_context(&all_rows, context)
}

pub(crate) fn split_lines(text: &str) -> Vec<&str> {
    if text.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

pub(crate) fn line_count(text: &str) -> u32 {
    if text.is_empty() {
        return 0;
    }
    let newlines = text.bytes().filter(|&b| b == b'\n').count() as u32;
    if text.ends_with('\n') {
        newlines
    } else {
        newlines + 1
    }
}

fn mark_changed_lines(lines: &[&str], changes: &[DiffChange], side: Side) -> Vec<bool> {
    let mut changed = vec![false; lines.len()];
    if lines.is_empty() {
        return changed;
    }

    let offsets = line_byte_offsets(lines);

    for change in changes {
        if change.side != side || change.byte_range.start >= change.byte_range.end {
            continue;
        }
        let cr = &change.byte_range;
        let first = offsets.partition_point(|&(_, end)| end < cr.start);
        for (i, &(start, end)) in offsets[first..].iter().enumerate() {
            if start >= cr.end {
                break;
            }
            if cr.start < end + 1 && cr.end > start {
                changed[first + i] = true;
            }
        }
    }

    changed
}

fn collect_line_spans(
    lines: &[&str],
    changes: &[DiffChange],
    side: Side,
) -> Vec<Vec<Range<usize>>> {
    collect_spans_by(lines, changes, side, |kind| {
        matches!(kind, LangChangeKind::Novel | LangChangeKind::Replaced)
    })
}

fn collect_moved_spans(
    lines: &[&str],
    changes: &[DiffChange],
    side: Side,
) -> Vec<Vec<Range<usize>>> {
    collect_spans_by(lines, changes, side, |kind| {
        matches!(kind, LangChangeKind::Moved)
    })
}

/// Per-line cross-file move provenance. For each line on `side` that
/// participates in a [`LangChangeKind::Moved`] change whose first
/// `MoveSource` carries `Some(BufferRef)` for a *different* file
/// (resolved via `rel_path_for`), emit `Some(MoveProvenance { rel_path,
/// line })`. Intra-file moves and lines untouched by a Moved change get
/// `None`. The renderer paints the chip with the move-highlight style.
fn collect_moved_provenance(
    lines: &[&str],
    changes: &[DiffChange],
    side: Side,
    rel_path_for: &dyn Fn(&Path) -> Option<String>,
) -> Vec<Option<MoveProvenance>> {
    let mut out: Vec<Option<MoveProvenance>> = vec![None; lines.len()];
    if lines.is_empty() {
        return out;
    }

    let offsets = line_byte_offsets(lines);

    for change in changes {
        if change.side != side
            || change.byte_range.start >= change.byte_range.end
            || !matches!(change.kind, LangChangeKind::Moved)
        {
            continue;
        }
        let metadata = match change.move_metadata.as_ref() {
            Some(m) => m,
            None => continue,
        };
        let foreign = metadata.sources.iter().find_map(|s| {
            let path = s.buffer.as_ref()?.path.as_path();
            let rel_path = rel_path_for(path)?;
            Some(MoveProvenance {
                rel_path,
                line: s.line_range.start,
            })
        });
        let Some(prov) = foreign else {
            continue;
        };
        let cr = &change.byte_range;
        let first = offsets.partition_point(|&(_, end)| end < cr.start);
        for (i, &(line_start, _line_end)) in offsets[first..].iter().enumerate() {
            if line_start >= cr.end {
                break;
            }
            let row = first + i;
            if out[row].is_none() {
                out[row] = Some(prov.clone());
            }
        }
    }

    out
}

fn collect_spans_by(
    lines: &[&str],
    changes: &[DiffChange],
    side: Side,
    include: impl Fn(&LangChangeKind) -> bool,
) -> Vec<Vec<Range<usize>>> {
    let mut spans: Vec<Vec<Range<usize>>> = vec![Vec::new(); lines.len()];
    if lines.is_empty() {
        return spans;
    }

    let offsets = line_byte_offsets(lines);

    for change in changes {
        if change.side != side
            || change.byte_range.start >= change.byte_range.end
            || !include(&change.kind)
        {
            continue;
        }
        let cr = &change.byte_range;
        let first = offsets.partition_point(|&(_, end)| end < cr.start);
        for (i, &(line_start, line_end)) in offsets[first..].iter().enumerate() {
            if line_start >= cr.end {
                break;
            }
            let span_start = cr.start.max(line_start) - line_start;
            let span_end = cr.end.min(line_end) - line_start;
            if span_start < span_end {
                spans[first + i].push(span_start..span_end);
            }
        }
    }

    spans
}

pub(crate) fn line_byte_offsets(lines: &[&str]) -> Vec<(usize, usize)> {
    let mut offsets = Vec::with_capacity(lines.len());
    let mut pos = 0usize;
    for &line in lines {
        let end = pos + line.len();
        offsets.push((pos, end));
        pos = end + 1;
    }
    offsets
}

struct WalkSide<'a> {
    lines: &'a [&'a str],
    changed: &'a [bool],
    spans: &'a [Vec<Range<usize>>],
    moved: &'a [Vec<Range<usize>>],
    provenance: &'a [Option<MoveProvenance>],
}

/// Walk both files using unchanged lines as alignment anchors. Changed
/// regions are collected and paired into side-by-side rows.
fn structural_walk(lhs: WalkSide<'_>, rhs: WalkSide<'_>) -> Vec<ReviewRow> {
    let mut result = Vec::new();
    let mut li = 0usize;
    let mut ri = 0usize;
    let mut old_line = 1u32;
    let mut new_line = 1u32;

    while li < lhs.lines.len() || ri < rhs.lines.len() {
        let l_ok = li < lhs.lines.len();
        let r_ok = ri < rhs.lines.len();
        let l_unchanged = l_ok && !lhs.changed[li];
        let r_unchanged = r_ok && !rhs.changed[ri];

        if l_unchanged && r_unchanged && lhs.lines[li] == rhs.lines[ri] {
            result.push(ReviewRow::Context {
                left: ReviewSide {
                    text: lhs.lines[li].to_string(),
                    line_num: old_line,
                    change_spans: Vec::new(),
                    moved_spans: Vec::new(),
                    move_provenance: None,
                },
                right: ReviewSide {
                    text: rhs.lines[ri].to_string(),
                    line_num: new_line,
                    change_spans: Vec::new(),
                    moved_spans: Vec::new(),
                    move_provenance: None,
                },
            });
            li += 1;
            ri += 1;
            old_line += 1;
            new_line += 1;
            continue;
        }

        // Collect runs of changed lines from both sides, then pair them.
        let mut left_run: Vec<ReviewSide> = Vec::new();
        let mut right_run: Vec<ReviewSide> = Vec::new();

        while li < lhs.lines.len() && lhs.changed[li] {
            left_run.push(ReviewSide {
                text: lhs.lines[li].to_string(),
                line_num: old_line,
                change_spans: lhs.spans[li].clone(),
                moved_spans: lhs.moved[li].clone(),
                move_provenance: lhs.provenance[li].clone(),
            });
            li += 1;
            old_line += 1;
        }

        while ri < rhs.lines.len() && rhs.changed[ri] {
            right_run.push(ReviewSide {
                text: rhs.lines[ri].to_string(),
                line_num: new_line,
                change_spans: rhs.spans[ri].clone(),
                moved_spans: rhs.moved[ri].clone(),
                move_provenance: rhs.provenance[ri].clone(),
            });
            ri += 1;
            new_line += 1;
        }

        // A one-sided in-line change marks only the modified side, so one run
        // holds the plain change while the other is empty. Pull the unmarked,
        // still-mismatched lines from the empty side into its run so the modified
        // line pairs with its old text rather than leaving the cursor to cascade
        // misaligned pairs to EOF.
        //
        // The pull is committed only when it realigns the cursor. The line after
        // the pulled block must match the opposite run's resume line, or both
        // must reach EOF. A pull that runs the whole length without realigning has grabbed
        // unrelated content (a moved or consolidated block that should stay
        // gap-aligned), so it is rolled back. Restricting to a non-moved run
        // skips relocated blocks up front.
        let is_plain_change = |run: &[ReviewSide]| {
            run.iter()
                .all(|s| s.move_provenance.is_none() && s.moved_spans.is_empty())
        };
        if left_run.is_empty() && !right_run.is_empty() && is_plain_change(&right_run) {
            let (start_li, start_old) = (li, old_line);
            let mut pulled = Vec::new();
            while pulled.len() < right_run.len()
                && li < lhs.lines.len()
                && !lhs.changed[li]
                && (ri >= rhs.lines.len() || lhs.lines[li] != rhs.lines[ri])
            {
                pulled.push(ReviewSide {
                    text: lhs.lines[li].to_string(),
                    line_num: old_line,
                    change_spans: lhs.spans[li].clone(),
                    moved_spans: lhs.moved[li].clone(),
                    move_provenance: lhs.provenance[li].clone(),
                });
                li += 1;
                old_line += 1;
            }
            let resynced = !pulled.is_empty()
                && (li >= lhs.lines.len()
                    || ri >= rhs.lines.len()
                    || lhs.lines[li] == rhs.lines[ri]);
            if resynced {
                left_run = pulled;
            } else {
                li = start_li;
                old_line = start_old;
            }
        } else if right_run.is_empty() && !left_run.is_empty() && is_plain_change(&left_run) {
            let (start_ri, start_new) = (ri, new_line);
            let mut pulled = Vec::new();
            while pulled.len() < left_run.len()
                && ri < rhs.lines.len()
                && !rhs.changed[ri]
                && (li >= lhs.lines.len() || rhs.lines[ri] != lhs.lines[li])
            {
                pulled.push(ReviewSide {
                    text: rhs.lines[ri].to_string(),
                    line_num: new_line,
                    change_spans: rhs.spans[ri].clone(),
                    moved_spans: rhs.moved[ri].clone(),
                    move_provenance: rhs.provenance[ri].clone(),
                });
                ri += 1;
                new_line += 1;
            }
            let resynced = !pulled.is_empty()
                && (ri >= rhs.lines.len()
                    || li >= lhs.lines.len()
                    || rhs.lines[ri] == lhs.lines[li]);
            if resynced {
                right_run = pulled;
            } else {
                ri = start_ri;
                new_line = start_new;
            }
        }

        if left_run.is_empty() && right_run.is_empty() {
            // Both unchanged but text differs (structural diff paired
            // tokens across non-corresponding lines). Treat as change.
            if l_ok && r_ok {
                result.push(ReviewRow::Changed {
                    left: Some(ReviewSide {
                        text: lhs.lines[li].to_string(),
                        line_num: old_line,
                        change_spans: Vec::new(),
                        moved_spans: Vec::new(),
                        move_provenance: None,
                    }),
                    right: Some(ReviewSide {
                        text: rhs.lines[ri].to_string(),
                        line_num: new_line,
                        change_spans: Vec::new(),
                        moved_spans: Vec::new(),
                        move_provenance: None,
                    }),
                });
                li += 1;
                ri += 1;
                old_line += 1;
                new_line += 1;
            } else if l_ok {
                result.push(ReviewRow::Changed {
                    left: Some(ReviewSide {
                        text: lhs.lines[li].to_string(),
                        line_num: old_line,
                        change_spans: Vec::new(),
                        moved_spans: Vec::new(),
                        move_provenance: None,
                    }),
                    right: None,
                });
                li += 1;
                old_line += 1;
            } else {
                result.push(ReviewRow::Changed {
                    left: None,
                    right: Some(ReviewSide {
                        text: rhs.lines[ri].to_string(),
                        line_num: new_line,
                        change_spans: Vec::new(),
                        moved_spans: Vec::new(),
                        move_provenance: None,
                    }),
                });
                ri += 1;
                new_line += 1;
            }
            continue;
        }

        // Pair left and right runs side-by-side.
        let max = left_run.len().max(right_run.len());
        for i in 0..max {
            result.push(ReviewRow::Changed {
                left: left_run.get(i).cloned(),
                right: right_run.get(i).cloned(),
            });
        }
    }

    result
}

fn extract_hunks_with_context(all_rows: &[ReviewRow], context: u32) -> Vec<ReviewHunk> {
    let change_indices: Vec<usize> = all_rows
        .iter()
        .enumerate()
        .filter(|(_, r)| r.is_changed())
        .map(|(i, _)| i)
        .collect();

    if change_indices.is_empty() {
        return Vec::new();
    }

    let len = all_rows.len();
    let ctx = context as usize;

    let mut regions: Vec<Range<usize>> = Vec::new();
    for &ci in &change_indices {
        let start = ci.saturating_sub(ctx);
        let end = (ci + 1 + ctx).min(len);
        match regions.last_mut() {
            Some(last) if start <= last.end => last.end = last.end.max(end),
            _ => regions.push(start..end),
        }
    }

    regions
        .into_iter()
        .map(|r| ReviewHunk {
            rows: all_rows[r].to_vec(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hunks(base: &str, buffer: &str, ctx: u32) -> Vec<ReviewHunk> {
        let inputs = vec![ReviewFileInput {
            path: PathBuf::from("test.txt"),
            rel_path: "test.txt".to_string(),
            language: None,
            base_text: Arc::new(base.to_string()),
            buffer_text: Arc::new(buffer.to_string()),
        }];
        extract_review_hunks_changeset(&inputs, ctx)
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    /// Like [`hunks`] but drives the structural (tree-sitter) diff by tagging the
    /// input as Rust, so a one-sided in-line change is marked on one side only.
    fn hunks_rust(base: &str, buffer: &str, ctx: u32) -> Vec<ReviewHunk> {
        use stoat_language::LanguageRegistry;
        let inputs = vec![ReviewFileInput {
            path: PathBuf::from("x.rs"),
            rel_path: "x.rs".to_string(),
            language: LanguageRegistry::standard().for_path(Path::new("x.rs")),
            base_text: Arc::new(base.to_string()),
            buffer_text: Arc::new(buffer.to_string()),
        }];
        extract_review_hunks_changeset(&inputs, ctx)
            .into_iter()
            .next()
            .unwrap_or_default()
    }

    #[test]
    fn one_sided_inline_change_pairs_instead_of_cascading() {
        let base = "fn main() {\n    let x = 1;\n    draw(a, b);\n}\n\
             fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n\
             fn f() {}\nfn g() {}\nfn h() {}\nfn i() {}\n";
        let buffer = "fn main() {\n    let x = 1;\n    draw(a, b, None);\n}\n\
             fn a() {}\nfn b() {}\nfn c() {}\nfn d() {}\nfn e() {}\n\
             fn f() {}\nfn g() {}\nfn h() {}\nfn i() {}\n";

        let hs = hunks_rust(base, buffer, 3);

        assert_eq!(hs.len(), 1, "one edit is one hunk, not a cascade to EOF");
        let rows = &hs[0].rows;
        assert_eq!(
            rows.iter().filter(|r| r.is_changed()).count(),
            1,
            "exactly one changed row, not a misaligned pair per line"
        );
        let changed = rows.iter().find(|r| r.is_changed()).expect("a changed row");
        assert!(
            matches!(changed, ReviewRow::Changed { left: Some(l), right: Some(r) }
                if l.text == "    draw(a, b);" && r.text == "    draw(a, b, None);"),
            "the change pairs the old call with the new one, got {changed:?}"
        );
    }

    #[test]
    fn duplicated_statement_leaves_untouched_file_clean() {
        use stoat_language::LanguageRegistry;
        let unchanged = "fn a() {\n    let x = make_thing(1, 2);\n}\n";
        let b_base = "fn b() {\n}\n";
        let b_buffer = "fn b() {\n    let x = make_thing(1, 2);\n}\n";
        let inputs = vec![
            ReviewFileInput {
                path: PathBuf::from("a.rs"),
                rel_path: "a.rs".to_string(),
                language: LanguageRegistry::standard().for_path(Path::new("a.rs")),
                base_text: Arc::new(unchanged.to_string()),
                buffer_text: Arc::new(unchanged.to_string()),
            },
            ReviewFileInput {
                path: PathBuf::from("b.rs"),
                rel_path: "b.rs".to_string(),
                language: LanguageRegistry::standard().for_path(Path::new("b.rs")),
                base_text: Arc::new(b_base.to_string()),
                buffer_text: Arc::new(b_buffer.to_string()),
            },
        ];

        let per_file = extract_review_hunks_changeset(&inputs, 3);

        assert!(
            per_file[0].is_empty(),
            "the untouched file gets no hunks even when its body is copied \
             elsewhere; got {:?}",
            per_file[0]
        );
        assert!(
            !per_file[1].is_empty(),
            "the file that gained the copy shows a real change"
        );
    }

    #[test]
    fn no_changes() {
        assert!(hunks("a\nb\nc\n", "a\nb\nc\n", 3).is_empty());
    }

    #[test]
    fn single_addition() {
        let hs = hunks("a\nb\n", "a\nnew\nb\n", 1);
        assert_eq!(hs.len(), 1);
        let rows = &hs[0].rows;
        assert_eq!(rows.len(), 3);
        // Row 0: context "a"
        assert!(matches!(&rows[0], ReviewRow::Context { left, right }
            if left.text == "a" && right.text == "a"
               && left.line_num == 1 && right.line_num == 1));
        // Row 1: added "new" (left=None, right=Some)
        assert!(
            matches!(&rows[1], ReviewRow::Changed { left: None, right: Some(r) }
            if r.text == "new" && r.line_num == 2)
        );
        // Row 2: context "b"
        assert!(matches!(&rows[2], ReviewRow::Context { .. }));
    }

    #[test]
    fn single_deletion() {
        let hs = hunks("a\nold\nb\n", "a\nb\n", 1);
        assert_eq!(hs.len(), 1);
        let rows = &hs[0].rows;
        assert_eq!(rows.len(), 3);
        // Row 0: context "a"
        assert!(matches!(&rows[0], ReviewRow::Context { .. }));
        // Row 1: deleted "old" (left=Some, right=None)
        assert!(
            matches!(&rows[1], ReviewRow::Changed { left: Some(l), right: None }
            if l.text == "old" && l.line_num == 2)
        );
        // Row 2: context "b"
        assert!(matches!(&rows[2], ReviewRow::Context { .. }));
    }

    #[test]
    fn modification_pairs_side_by_side() {
        let hs = hunks("a\nold\nb\n", "a\nnew\nb\n", 1);
        assert_eq!(hs.len(), 1);
        let rows = &hs[0].rows;
        // Row 1: old on left, new on right (same row)
        assert!(
            matches!(&rows[1], ReviewRow::Changed { left: Some(l), right: Some(r) }
            if l.text == "old" && r.text == "new")
        );
    }

    #[test]
    fn two_separate_hunks() {
        let hs = hunks("a\nb\nc\nd\ne\nf\ng\nh\n", "a\nB\nc\nd\ne\nF\ng\nh\n", 1);
        assert_eq!(hs.len(), 2);
        // First hunk: b→B paired
        let row = &hs[0].rows[1];
        assert!(
            matches!(row, ReviewRow::Changed { left: Some(l), right: Some(r) }
            if l.text == "b" && r.text == "B")
        );
    }

    #[test]
    fn adjacent_hunks_merge() {
        let hs = hunks("a\nb\nc\nd\ne\n", "a\nB\nc\nD\ne\n", 1);
        assert_eq!(hs.len(), 1);
    }

    #[test]
    fn empty_base() {
        let hs = hunks("", "a\nb\n", 1);
        assert_eq!(hs.len(), 1);
        assert!(hs[0].rows.iter().all(|r| matches!(
            r,
            ReviewRow::Changed {
                left: None,
                right: Some(_)
            }
        )));
    }

    #[test]
    fn empty_buffer() {
        let hs = hunks("a\nb\n", "", 1);
        assert_eq!(hs.len(), 1);
        assert!(hs[0].rows.iter().all(|r| matches!(
            r,
            ReviewRow::Changed {
                left: Some(_),
                right: None
            }
        )));
    }

    #[test]
    fn line_numbers_track_correctly() {
        let hs = hunks("a\nb\nc\nd\n", "a\nx\ny\nc\nd\n", 1);
        assert_eq!(hs.len(), 1);
        let rows = &hs[0].rows;
        // "a": context
        match &rows[0] {
            ReviewRow::Context { left, right } => {
                assert_eq!(left.line_num, 1);
                assert_eq!(right.line_num, 1);
            },
            _ => panic!("expected context"),
        }
        // "b" deleted, "x" added, "y" added → 2 changed rows
        match &rows[1] {
            ReviewRow::Changed {
                left: Some(l),
                right: Some(r),
            } => {
                assert_eq!(l.line_num, 2); // old "b"
                assert_eq!(r.line_num, 2); // new "x"
            },
            _ => panic!("expected changed with both sides"),
        }
        match &rows[2] {
            ReviewRow::Changed {
                left: None,
                right: Some(r),
            } => {
                assert_eq!(r.line_num, 3); // new "y"
            },
            _ => panic!("expected addition-only"),
        }
        // "c": context
        match &rows[3] {
            ReviewRow::Context { left, right } => {
                assert_eq!(left.line_num, 3);
                assert_eq!(right.line_num, 4);
            },
            _ => panic!("expected context"),
        }
    }

    #[test]
    fn change_spans_present_on_changed_rows() {
        let hs = hunks("let x = 1;\n", "let x = 2;\n", 0);
        assert_eq!(hs.len(), 1);
        match &hs[0].rows[0] {
            ReviewRow::Changed {
                left: Some(l),
                right: Some(r),
            } => {
                assert!(!l.change_spans.is_empty());
                assert!(!r.change_spans.is_empty());
            },
            _ => panic!("expected paired change"),
        }
    }

    #[test]
    fn mark_changed_lines_basic() {
        let text = "aaa\nbbb\nccc\n";
        let lines = split_lines(text);
        let changes = vec![DiffChange {
            side: Side::Lhs,
            byte_range: 4..7,
            kind: structural_diff::ChangeKind::Novel,
            move_metadata: None,
            pair_id: None,
            deletion_rhs_anchor: None,
        }];
        assert_eq!(
            mark_changed_lines(&lines, &changes, Side::Lhs),
            vec![false, true, false]
        );
    }

    #[test]
    fn collect_spans_within_line() {
        let text = "hello world\n";
        let lines = split_lines(text);
        let changes = vec![DiffChange {
            side: Side::Rhs,
            byte_range: 6..11,
            kind: structural_diff::ChangeKind::Replaced,
            move_metadata: None,
            pair_id: None,
            deletion_rhs_anchor: None,
        }];
        let spans = collect_line_spans(&lines, &changes, Side::Rhs);
        assert_eq!(spans, vec![vec![6..11]]);
    }

    use crate::test_harness::TestHarness;

    #[test]
    fn snapshot_review_addition() {
        let mut h = TestHarness::with_size(80, 10);
        h.open_review_from_texts(&[(
            "test.rs",
            "fn a() {}\nfn b() {}\n",
            "fn a() {}\nfn new() {}\nfn b() {}\n",
        )]);
        h.assert_snapshot("review_addition");
    }

    #[test]
    fn snapshot_review_deletion() {
        let mut h = TestHarness::with_size(80, 10);
        h.open_review_from_texts(&[(
            "test.rs",
            "fn a() {}\nfn old() {}\nfn b() {}\n",
            "fn a() {}\nfn b() {}\n",
        )]);
        h.assert_snapshot("review_deletion");
    }

    #[test]
    fn snapshot_review_modification() {
        let mut h = TestHarness::with_size(80, 10);
        h.open_review_from_texts(&[(
            "test.rs",
            "fn main() {\n    let x = 1;\n}\n",
            "fn main() {\n    let x = 2;\n}\n",
        )]);
        h.assert_snapshot("review_modification");
    }

    #[test]
    fn snapshot_review_multi_file() {
        let mut h = TestHarness::with_size(80, 16);
        h.open_review_from_texts(&[
            ("a.rs", "fn a() {}\n", "fn a_renamed() {}\n"),
            ("b.rs", "let x = 1;\n", "let x = 1;\nlet y = 2;\n"),
        ]);
        h.assert_snapshot("review_multi_file");
    }

    #[test]
    fn snapshot_review_move_straight() {
        let mut h = TestHarness::with_size(100, 20);
        let base = "\
fn alpha() {
    let x = 1;
    let y = 2;
    let z = 3;
}

fn beta() {
    let p = 10;
    let q = 20;
    let r = 30;
}
";
        let rhs = "\
fn beta() {
    let p = 10;
    let q = 20;
    let r = 30;
}

fn alpha() {
    let x = 1;
    let y = 2;
    let z = 3;
}
";
        h.open_review_from_texts(&[("swap.rs", base, rhs)]);
        h.assert_snapshot("review_move_straight");
    }

    #[test]
    fn snapshot_review_move_cross_indent() {
        let mut h = TestHarness::with_size(100, 20);
        let base = "\
fn outer() {
    let relocated = compute(arg1, arg2, arg3);
}

fn wrapper() {
    println!(\"hello\");
}
";
        let rhs = "\
fn outer() {}

fn wrapper() {
    println!(\"hello\");
    let relocated = compute(arg1, arg2, arg3);
}
";
        h.open_review_from_texts(&[("nest.rs", base, rhs)]);
        h.assert_snapshot("review_move_cross_indent");
    }

    #[test]
    fn snapshot_review_move_consolidation() {
        let mut h = TestHarness::with_size(100, 24);
        let base = "\
fn first() {
    let temp = heavy_computation(a, b, c);
    save(temp);
}

fn second() {
    let temp = heavy_computation(a, b, c);
    save(temp);
}
";
        let rhs = "\
fn shared() {
    let temp = heavy_computation(a, b, c);
    save(temp);
}

fn first() {
    shared();
}

fn second() {
    shared();
}
";
        h.open_review_from_texts(&[("consolidate.rs", base, rhs)]);
        h.assert_snapshot("review_move_consolidation");
    }

    #[test]
    fn snapshot_review_cross_file_move() {
        // Function `migrated` lives at the top of a.rs's base and is
        // gone from a.rs's rhs; it reappears at the bottom of b.rs's
        // rhs. The cross-file `diff_changeset` pass must emit Moved
        // hunks on both files (referencing the foreign BufferRef in
        // their MoveMetadata), and the renderer must surface them
        // with the moved-span styling rather than red/green
        // add/delete on either side.
        let mut h = TestHarness::with_size(140, 32);
        let a_base = "\
fn migrated() {
    let x = 1;
    let y = 2;
    let z = 3;
}

fn stays_a() {
    call_a();
}
";
        let a_rhs = "\
fn stays_a() {
    call_a();
}
";
        let b_base = "\
fn stays_b() {
    call_b();
}
";
        let b_rhs = "\
fn stays_b() {
    call_b();
}

fn migrated() {
    let x = 1;
    let y = 2;
    let z = 3;
}
";
        h.open_review_from_texts(&[("a.rs", a_base, a_rhs), ("b.rs", b_base, b_rhs)]);
        h.assert_snapshot("review_cross_file_move");
    }
}
