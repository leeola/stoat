//! Unified-diff emission for a single [`ReviewChunk`].
//!
//! The output is a minimal patch file that `git apply --cached` (and
//! `git2::Diff::from_buffer` + `Repository::apply(..., ApplyLocation::Index)`)
//! will accept. One chunk produces exactly one hunk with its own
//! `--- a/<rel>` / `+++ b/<rel>` headers so callers can apply a subset
//! of a file's chunks independently.

use crate::{
    diff_map::DiffHunk,
    review::{line_count, ReviewRow, ReviewSide},
};
use std::{ops::Range, path::Path};

/// Unchanged rows a staged patch carries on each side of its hunk.
///
/// Three is what git emits by default, and enough for the index apply to place
/// the hunk when the file has moved under it.
pub(crate) const HUNK_CONTEXT: u32 = 3;

const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file\n";

/// 0-based base line that hunk `k` starts at.
///
/// [`DiffHunk`] records buffer rows and base bytes but no base line, so the
/// anchor comes from the buffer start less every line the prior hunks added or
/// removed. An `Added` hunk contributes nothing to that walk, which is what
/// makes the anchor right for a pure insertion.
fn base_line_start(base_text: &str, hunks: &[DiffHunk], k: usize) -> u32 {
    let mut delta: i64 = 0;
    for prior in &hunks[..k] {
        let buffer_len = prior.buffer_line_range.end - prior.buffer_line_range.start;
        let base_len = base_span_lines(base_text, prior);
        delta += i64::from(buffer_len) - i64::from(base_len);
    }
    let start = hunks.get(k).map_or(0, |hunk| hunk.buffer_line_range.start);
    (i64::from(start) - delta).max(0) as u32
}

/// 0-based base lines hunk `k` covers.
///
/// Empty for an `Added` hunk, which replaces no base line. A caller resolving
/// a hunk by index row reads this the way the gutter reads
/// [`DiffHunk::buffer_line_range`].
pub(crate) fn base_line_range(base_text: &str, hunks: &[DiffHunk], k: usize) -> Range<u32> {
    let start = base_line_start(base_text, hunks, k);
    let len = hunks
        .get(k)
        .map_or(0, |hunk| base_span_lines(base_text, hunk));
    start..start + len
}

/// Rows covering line hunk `k` alone, with up to `context` unchanged rows on
/// each side.
///
/// The staging unit has to be the hunk the gutter draws, so these rows come
/// straight from the hunk extents rather than from a chunk extraction that
/// merges neighbors within a context window.
///
/// [`DiffHunk`] records where a hunk sits in the buffer and which base bytes it
/// replaces, but not its base *line*, so that anchor is derived by walking the
/// prior hunks' line deltas. An `Added` hunk's base range is zero-width and
/// contributes nothing to the walk, which is what makes the derived anchor
/// right for a pure addition.
///
/// The lines between two line hunks are byte-identical on both sides, so a
/// context row carries both sides' line numbers without a second diff. Context
/// is clipped at the neighbor hunks and at the file edges.
///
/// Returns [`None`] when `k` is out of range.
pub(crate) fn hunk_rows(
    base_text: &str,
    buffer_text: &str,
    hunks: &[DiffHunk],
    k: usize,
    context: u32,
) -> Option<Vec<ReviewRow>> {
    let hunk = hunks.get(k)?;
    let base_lines: Vec<&str> = split_lines(base_text);
    let buffer_lines: Vec<&str> = split_lines(buffer_text);

    let base_start = base_line_start(base_text, hunks, k);
    let base_len = base_span_lines(base_text, hunk);
    let buffer_start = hunk.buffer_line_range.start;
    let buffer_len = hunk.buffer_line_range.end - buffer_start;

    // Context stops at the neighbor hunk rather than running into it, so two
    // nearby hunks stage independently.
    let leading = {
        let prior_end = k
            .checked_sub(1)
            .map_or(0, |i| hunks[i].buffer_line_range.end);
        context
            .min(buffer_start.saturating_sub(prior_end))
            .min(base_start)
    };
    let trailing = {
        let next_start = hunks
            .get(k + 1)
            .map_or(u32::MAX, |next| next.buffer_line_range.start);
        let buffer_room = next_start.saturating_sub(buffer_start + buffer_len);
        let base_room = (base_lines.len() as u32).saturating_sub(base_start + base_len);
        context.min(buffer_room).min(base_room)
    };

    let side = |lines: &[&str], line: u32| ReviewSide {
        text: lines.get(line as usize).copied().unwrap_or("").to_string(),
        line_num: line + 1,
        change_spans: Vec::new(),
        moved_spans: Vec::new(),
        move_provenance: None,
    };

    let mut rows = Vec::new();
    for offset in (1..=leading).rev() {
        rows.push(ReviewRow::Context {
            left: side(&base_lines, base_start - offset),
            right: side(&buffer_lines, buffer_start - offset),
        });
    }
    for i in 0..base_len.max(buffer_len) {
        rows.push(ReviewRow::Changed {
            left: (i < base_len).then(|| side(&base_lines, base_start + i)),
            right: (i < buffer_len).then(|| side(&buffer_lines, buffer_start + i)),
        });
    }
    for offset in 0..trailing {
        rows.push(ReviewRow::Context {
            left: side(&base_lines, base_start + base_len + offset),
            right: side(&buffer_lines, buffer_start + buffer_len + offset),
        });
    }
    Some(rows)
}

/// A standalone patch for line hunk `k`, keyed at `rel`.
///
/// With `reverse` set the two sides swap, so applying the patch undoes the
/// hunk. That is how a hunk is unstaged: libgit2's index apply has no reverse
/// mode, so the reversal lives in the patch text.
pub(crate) fn hunk_to_patch(
    rel: &Path,
    base_text: &str,
    buffer_text: &str,
    hunks: &[DiffHunk],
    k: usize,
    reverse: bool,
) -> Option<String> {
    let rows = hunk_rows(base_text, buffer_text, hunks, k, HUNK_CONTEXT)?;
    Some(match reverse {
        true => {
            let swapped: Vec<ReviewRow> = rows.iter().map(swap_row).collect();
            rows_to_unified_diff(rel, buffer_text, base_text, &swapped)
        },
        false => rows_to_unified_diff(rel, base_text, buffer_text, &rows),
    })
}

/// Lines the hunk covers on the base side.
///
/// Derived from the byte range rather than stored, and zero for an `Added`
/// hunk, whose base range is empty.
fn base_span_lines(base_text: &str, hunk: &DiffHunk) -> u32 {
    let range = &hunk.base_byte_range;
    if range.is_empty() {
        return 0;
    }
    line_count(&base_text[range.clone()])
}

/// The text's lines without their terminators, and without the empty tail a
/// trailing newline would otherwise produce.
fn split_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}
/// Restrict a chunk's rows to the single change at 1-based `side_line`, for
/// staging or unstaging one line.
///
/// Keeps the [`ReviewRow::Changed`] row whose selected side -- right when
/// `right_side`, else left -- sits at `side_line`, and rewrites every other
/// `Changed` row so the emitted patch touches nothing else. Both callers apply
/// the forward patch against the base (left) side, so a non-selected row with a
/// base line becomes a [`ReviewRow::Context`] carrying that base content, and a
/// right-only row (no base line) is dropped. Existing `Context` rows pass
/// through, so the surrounding hunk context still anchors the patch.
///
/// Returns [`None`] when no `Changed` row matched `side_line`, letting the
/// caller report that the cursor sits on no change.
pub(crate) fn line_restricted_rows(
    rows: &[ReviewRow],
    side_line: u32,
    right_side: bool,
) -> Option<Vec<ReviewRow>> {
    let mut matched = false;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        match row {
            ReviewRow::Context { .. } => out.push(row.clone()),
            ReviewRow::Changed { left, right } => {
                let selected = if right_side {
                    right.as_ref()
                } else {
                    left.as_ref()
                };
                if selected.is_some_and(|side| side.line_num == side_line) {
                    matched = true;
                    out.push(row.clone());
                } else if let Some(left) = left {
                    out.push(ReviewRow::Context {
                        left: left.clone(),
                        right: left.clone(),
                    });
                }
            },
        }
    }
    matched.then_some(out)
}

/// Swaps a row's two sides so a forward hunk emits as its reverse.
///
/// Context rows carry identical text on both sides, so the swap only
/// matters for [`ReviewRow::Changed`] rows, whose `-`/`+` roles flip.
fn swap_row(row: &ReviewRow) -> ReviewRow {
    match row {
        ReviewRow::Context { left, right } => ReviewRow::Context {
            left: right.clone(),
            right: left.clone(),
        },
        ReviewRow::Changed { left, right } => ReviewRow::Changed {
            left: right.clone(),
            right: left.clone(),
        },
    }
}

pub(crate) fn rows_to_unified_diff(
    rel: &Path,
    base_text: &str,
    buffer_text: &str,
    rows: &[ReviewRow],
) -> String {
    let rel_display = rel.display();

    let (base_start, base_count) = base_header(rows);
    let (buffer_start, buffer_count) = buffer_header(rows);

    let base_total = line_count(base_text);
    let buffer_total = line_count(buffer_text);
    let base_no_nl = !base_text.is_empty() && !base_text.ends_with('\n');
    let buffer_no_nl = !buffer_text.is_empty() && !buffer_text.ends_with('\n');

    let last_left_idx = last_row_with_left(rows);
    let last_right_idx = last_row_with_right(rows);

    let base_is_new_file = base_text.is_empty();
    let buffer_is_deleted_file = buffer_text.is_empty();

    let mut out = String::new();
    out.push_str(&format!("diff --git a/{rel_display} b/{rel_display}\n"));
    if base_is_new_file {
        out.push_str("new file mode 100644\n");
    } else if buffer_is_deleted_file {
        out.push_str("deleted file mode 100644\n");
    }
    if base_is_new_file {
        out.push_str("--- /dev/null\n");
    } else {
        out.push_str(&format!("--- a/{rel_display}\n"));
    }
    if buffer_is_deleted_file {
        out.push_str("+++ /dev/null\n");
    } else {
        out.push_str(&format!("+++ b/{rel_display}\n"));
    }
    out.push_str(&format!(
        "@@ -{base_start},{base_count} +{buffer_start},{buffer_count} @@\n"
    ));

    for (i, row) in rows.iter().enumerate() {
        let is_last_left = Some(i) == last_left_idx;
        let is_last_right = Some(i) == last_right_idx;

        match row {
            ReviewRow::Context { left, right } => {
                emit_prefixed(&mut out, ' ', &right.text);
                let left_at_eof = base_no_nl && is_last_left && touches_base_eof(left, base_total);
                let right_at_eof =
                    buffer_no_nl && is_last_right && touches_buffer_eof(right, buffer_total);
                if left_at_eof || right_at_eof {
                    out.push_str(NO_NEWLINE_MARKER);
                }
            },
            ReviewRow::Changed {
                left: Some(l),
                right: None,
            } => {
                emit_prefixed(&mut out, '-', &l.text);
                if base_no_nl && is_last_left && touches_base_eof(l, base_total) {
                    out.push_str(NO_NEWLINE_MARKER);
                }
            },
            ReviewRow::Changed {
                left: None,
                right: Some(r),
            } => {
                emit_prefixed(&mut out, '+', &r.text);
                if buffer_no_nl && is_last_right && touches_buffer_eof(r, buffer_total) {
                    out.push_str(NO_NEWLINE_MARKER);
                }
            },
            ReviewRow::Changed {
                left: Some(l),
                right: Some(r),
            } => {
                emit_prefixed(&mut out, '-', &l.text);
                if base_no_nl && is_last_left && touches_base_eof(l, base_total) {
                    out.push_str(NO_NEWLINE_MARKER);
                }
                emit_prefixed(&mut out, '+', &r.text);
                if buffer_no_nl && is_last_right && touches_buffer_eof(r, buffer_total) {
                    out.push_str(NO_NEWLINE_MARKER);
                }
            },
            ReviewRow::Changed {
                left: None,
                right: None,
            } => {},
        }
    }

    out
}

fn emit_prefixed(out: &mut String, prefix: char, text: &str) {
    out.push(prefix);
    out.push_str(text);
    out.push('\n');
}

fn base_header(rows: &[ReviewRow]) -> (u32, u32) {
    let mut start: Option<u32> = None;
    let mut count = 0u32;
    for row in rows {
        if let Some(l) = row_left(row) {
            start.get_or_insert(l.line_num);
            count += 1;
        }
    }
    match start {
        Some(s) => (s, count),
        None => (0, 0),
    }
}

fn buffer_header(rows: &[ReviewRow]) -> (u32, u32) {
    let mut start: Option<u32> = None;
    let mut count = 0u32;
    for row in rows {
        if let Some(r) = row_right(row) {
            start.get_or_insert(r.line_num);
            count += 1;
        }
    }
    match start {
        Some(s) => (s, count),
        None => (0, 0),
    }
}

fn row_left(row: &ReviewRow) -> Option<&ReviewSide> {
    match row {
        ReviewRow::Context { left, .. } => Some(left),
        ReviewRow::Changed { left: Some(l), .. } => Some(l),
        _ => None,
    }
}

fn row_right(row: &ReviewRow) -> Option<&ReviewSide> {
    match row {
        ReviewRow::Context { right, .. } => Some(right),
        ReviewRow::Changed { right: Some(r), .. } => Some(r),
        _ => None,
    }
}

fn last_row_with_left(rows: &[ReviewRow]) -> Option<usize> {
    rows.iter()
        .enumerate()
        .rev()
        .find(|(_, r)| row_left(r).is_some())
        .map(|(i, _)| i)
}

fn last_row_with_right(rows: &[ReviewRow]) -> Option<usize> {
    rows.iter()
        .enumerate()
        .rev()
        .find(|(_, r)| row_right(r).is_some())
        .map(|(i, _)| i)
}

fn touches_base_eof(side: &ReviewSide, base_total: u32) -> bool {
    side.line_num == base_total
}

fn touches_buffer_eof(side: &ReviewSide, buffer_total: u32) -> bool {
    side.line_num == buffer_total
}

// Two tests materialize a real git repo in a tempdir to exercise libgit2 patch
// application, so they write to disk directly.
#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    #[test]
    fn line_count_matches_split_lines() {
        assert_eq!(line_count(""), 0);
        assert_eq!(line_count("a"), 1);
        assert_eq!(line_count("a\n"), 1);
        assert_eq!(line_count("a\nb"), 2);
        assert_eq!(line_count("a\nb\n"), 2);
        assert_eq!(line_count("\n"), 1);
    }

    /// The whole point of hunk-direct patches, checked against real libgit2:
    /// staging one of two nearby hunks moves that change into the index and
    /// leaves the other alone, and the reverse patch takes it back out.
    #[test]
    fn a_hunk_patch_stages_only_its_own_change() {
        use crate::host::{GitHost, LocalGit};
        use git2::{Repository, Signature};

        const BASE: &str = "a\nb\nc\nd\ne\nf\ng\nh\n";
        const BUFFER: &str = "a\nZ\nc\nd\ne\nf\nY\nh\n";

        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        let repo = Repository::init(&workdir).unwrap();

        std::fs::write(workdir.join("a.rs"), BASE).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("a.rs")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = Signature::now("test", "t@t").unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &[])
            .unwrap();
        std::fs::write(workdir.join("a.rs"), BUFFER).unwrap();

        let hunks = {
            let result = stoat_language::structural_diff::diff(BASE, BUFFER);
            crate::diff_map::changes_to_hunks(&result.changes, BASE, BUFFER)
        };
        assert_eq!(hunks.len(), 2, "the file has two hunks to keep apart");

        let rel = Path::new("a.rs");
        let host_repo = LocalGit::new().discover(&workdir).unwrap();
        let staged = |repo: &Repository| {
            let mut index = repo.index().unwrap();
            index.read(true).unwrap();
            let entry = index.get_path(Path::new("a.rs"), 0).unwrap();
            let blob = repo.find_blob(entry.id).unwrap();
            std::str::from_utf8(blob.content()).unwrap().to_string()
        };

        let forward = hunk_to_patch(rel, BASE, BUFFER, &hunks, 0, false).expect("hunk 0 exists");
        host_repo
            .apply_to_index(&forward)
            .expect("the hunk patch must apply to real libgit2");
        assert_eq!(
            staged(&repo),
            "a\nZ\nc\nd\ne\nf\ng\nh\n",
            "the index carries the first change and not the second"
        );

        let reverse = hunk_to_patch(rel, BASE, BUFFER, &hunks, 0, true).expect("hunk 0 exists");
        host_repo
            .apply_to_index(&reverse)
            .expect("the reverse patch must apply too");
        assert_eq!(staged(&repo), BASE, "and the reverse takes it back out");
    }
}
