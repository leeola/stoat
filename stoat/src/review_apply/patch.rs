//! Unified-diff emission for a single [`ReviewChunk`].
//!
//! The output is a minimal patch file that `git apply --cached` (and
//! `git2::Diff::from_buffer` + `Repository::apply(..., ApplyLocation::Index)`)
//! will accept. One chunk produces exactly one hunk with its own
//! `--- a/<rel>` / `+++ b/<rel>` headers so callers can apply a subset
//! of a file's chunks independently.

use crate::{
    diff_map::{hunk_base_lines, line_starts, DiffHunk},
    review::{line_count, ReviewRow, ReviewSide},
};
use std::{ops::Range, path::Path};

/// Unchanged rows a staged patch carries on each side of its hunk.
///
/// Three is what git emits by default, and enough for the index apply to place
/// the hunk when the file has moved under it.
pub(crate) const HUNK_CONTEXT: u32 = 3;

const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file\n";

/// 0-based base line that hunk `k` starts at, against the [`line_starts`] of
/// `base_text`.
///
/// [`DiffHunk`] records buffer rows and base bytes but no base line, so the
/// anchor comes from the buffer start less every line the prior hunks added or
/// removed. An `Added` hunk contributes nothing to that walk, which is what
/// makes the anchor right for a pure insertion.
fn base_line_start(base_text: &str, starts: &[usize], hunks: &[DiffHunk], k: usize) -> u32 {
    let mut delta: i64 = 0;
    for prior in &hunks[..k] {
        let buffer_len = prior.buffer_line_range.end - prior.buffer_line_range.start;
        let base_len = hunk_base_lines(prior, starts, base_text).len() as u32;
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
    let starts = line_starts(base_text);
    let start = base_line_start(base_text, &starts, hunks, k);
    let len = hunks.get(k).map_or(0, |hunk| {
        hunk_base_lines(hunk, &starts, base_text).len() as u32
    });
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
/// Inside the hunk, a base line and a buffer line share a row when they are
/// equal once whitespace is collapsed, and the lines between two such rows pair
/// by position. A reindent therefore pairs each moved line with its old self,
/// so a patch narrowed to some rows replaces the lines those rows came from and
/// inserts the rest.
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

    let starts = line_starts(base_text);
    let base_start = base_line_start(base_text, &starts, hunks, k);
    let base_len = hunk_base_lines(hunk, &starts, base_text).len() as u32;
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
    let pairs = stoat_language::structural_diff::align_lines(
        &hunk_lines(&base_lines, base_start, base_len),
        &hunk_lines(&buffer_lines, buffer_start, buffer_len),
    );
    for (left, right) in pairs {
        rows.push(ReviewRow::Changed {
            left: left.map(|i| side(&base_lines, base_start + i as u32)),
            right: right.map(|i| side(&buffer_lines, buffer_start + i as u32)),
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

/// The `len` lines of `lines` from `start`, with a line past the end read as
/// empty, the way [`hunk_rows`] reads a row's text.
fn hunk_lines<'a>(lines: &[&'a str], start: u32, len: u32) -> Vec<&'a str> {
    (start..start + len)
        .map(|line| lines.get(line as usize).copied().unwrap_or(""))
        .collect()
}

/// A standalone patch for line hunk `k`, keyed at `rel`.
///
/// The patch turns the hunk's `base_text` lines into its `buffer_text` lines,
/// and it applies where the hunk sits in `base_text`. An index write passes the
/// index as the base, so the patch lands whatever the index holds above it.
pub(crate) fn hunk_to_patch(
    rel: &Path,
    base_text: &str,
    buffer_text: &str,
    hunks: &[DiffHunk],
    k: usize,
) -> Option<String> {
    let rows = hunk_rows(base_text, buffer_text, hunks, k, HUNK_CONTEXT)?;
    Some(rows_to_unified_diff(rel, base_text, buffer_text, &rows))
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
/// Restrict a chunk's rows to the changes at the 1-based `side_lines`, for
/// staging or unstaging a line or a run of lines.
///
/// Keeps each [`ReviewRow::Changed`] row whose selected side, the right one
/// when `right_side` and the left one otherwise, sits in `side_lines`, and
/// rewrites every other `Changed` row so the emitted patch touches nothing
/// else. Both callers apply the forward patch against the base (left) side, so
/// a non-selected row with a base line becomes a [`ReviewRow::Context`]
/// carrying that base content, and a right-only row (no base line) is dropped.
/// Existing `Context` rows pass through, so the surrounding hunk context still
/// anchors the patch.
///
/// Returns [`None`] when no `Changed` row matched, letting the caller report
/// that the cursor sits on no change.
pub(crate) fn line_restricted_rows(
    rows: &[ReviewRow],
    side_lines: Range<u32>,
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
                if selected.is_some_and(|side| side_lines.contains(&side.line_num)) {
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

/// The unified diff of `rows` as a one-hunk patch keyed at `rel`.
///
/// The hunk's `+` start repeats its `-` start. libgit2's index apply places a
/// hunk at exactly its `+` start, with no search. A one-hunk patch has no
/// earlier hunk to shift the lines above it, so the `-` start is where the
/// pre-image sits. A hunk with no `-` lines, the new-file form
/// `@@ -0,0 +1,N @@`, keeps its own `+` start.
pub(crate) fn rows_to_unified_diff(
    rel: &Path,
    base_text: &str,
    buffer_text: &str,
    rows: &[ReviewRow],
) -> String {
    let rel_display = rel.display();

    let (base_start, base_count) = base_header(rows);
    let (buffer_start, buffer_count) = match buffer_header(rows) {
        (_, count) if base_count > 0 => (base_start, count),
        header => header,
    };

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

    // The parser takes the post-image's no-newline marker only once every
    // pre-image line is read. A `+` line that ends the post-image while a later
    // row still removes a line waits here, and every row after it removes only,
    // so moving it to the end keeps the hunk valid.
    let mut deferred = String::new();
    for (i, row) in rows.iter().enumerate() {
        let is_last_left = Some(i) == last_left_idx;
        let is_last_right = Some(i) == last_right_idx;
        let later_left = last_left_idx.is_some_and(|last| last > i);

        match row {
            // A context line reads as unterminated on both sides when the marker
            // follows it. Only the pre-image's end decides that. A line that a
            // staging narrowed back to its base carries a base line on its right
            // side too, so the buffer's own ending says nothing about it.
            ReviewRow::Context { left, right } => {
                let base_eof = base_no_nl && is_last_left && touches_base_eof(left, base_total);
                match (base_eof, is_last_right) {
                    // The pre-image ends here, but a later line follows in the
                    // post-image, so this line gains a newline and stops being
                    // context.
                    (true, false) => {
                        emit_prefixed(&mut out, '-', &left.text);
                        out.push_str(NO_NEWLINE_MARKER);
                        emit_prefixed(&mut out, '+', &right.text);
                    },
                    (true, true) => {
                        emit_prefixed(&mut out, ' ', &right.text);
                        out.push_str(NO_NEWLINE_MARKER);
                    },
                    (false, _) => emit_prefixed(&mut out, ' ', &right.text),
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
                let r_eof = buffer_no_nl && is_last_right && touches_buffer_eof(r, buffer_total);
                emit_added(&mut out, &mut deferred, &r.text, r_eof, later_left);
            },
            ReviewRow::Changed {
                left: Some(l),
                right: Some(r),
            } => {
                emit_prefixed(&mut out, '-', &l.text);
                if base_no_nl && is_last_left && touches_base_eof(l, base_total) {
                    out.push_str(NO_NEWLINE_MARKER);
                }
                let r_eof = buffer_no_nl && is_last_right && touches_buffer_eof(r, buffer_total);
                emit_added(&mut out, &mut deferred, &r.text, r_eof, later_left);
            },
            ReviewRow::Changed {
                left: None,
                right: None,
            } => {},
        }
    }
    out.push_str(&deferred);

    out
}

/// Emit `+text`, with the no-newline marker when `at_eof`, into `out`, or into
/// `deferred` when it ends the post-image while `later_left` rows still remove
/// lines.
fn emit_added(out: &mut String, deferred: &mut String, text: &str, at_eof: bool, later_left: bool) {
    let target = match at_eof && later_left {
        true => deferred,
        false => out,
    };
    emit_prefixed(target, '+', text);
    if at_eof {
        target.push_str(NO_NEWLINE_MARKER);
    }
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
    use crate::host::{GitHost, GitRepo, LocalGit};
    use git2::{Repository, Signature};
    use std::sync::Arc;
    use tempfile::TempDir;

    /// A real repository whose HEAD and index hold `base` for `a.rs` and whose
    /// working file holds `buffer`, with the host handle that writes its index.
    fn index_repo(base: &str, buffer: &str) -> (TempDir, Repository, Arc<dyn GitRepo>) {
        let dir = tempfile::tempdir().unwrap();
        let workdir = dir.path().to_path_buf();
        let repo = Repository::init(&workdir).unwrap();

        std::fs::write(workdir.join("a.rs"), base).unwrap();
        {
            let mut index = repo.index().unwrap();
            index.add_path(Path::new("a.rs")).unwrap();
            index.write().unwrap();
            let tree_id = index.write_tree().unwrap();
            let tree = repo.find_tree(tree_id).unwrap();
            let sig = Signature::now("test", "t@t").unwrap();
            repo.commit(Some("HEAD"), &sig, &sig, "c", &tree, &[])
                .unwrap();
        }
        std::fs::write(workdir.join("a.rs"), buffer).unwrap();

        let host_repo = LocalGit::new().discover(&workdir).unwrap();
        (dir, repo, host_repo)
    }

    /// The text of `a.rs` in `repo`'s index.
    fn staged_text(repo: &Repository) -> String {
        let mut index = repo.index().unwrap();
        index.read(true).unwrap();
        let entry = index.get_path(Path::new("a.rs"), 0).unwrap();
        let blob = repo.find_blob(entry.id).unwrap();
        std::str::from_utf8(blob.content()).unwrap().to_string()
    }

    /// The line hunks between `base` and `buffer`, as the staging keys read them.
    fn hunks(base: &str, buffer: &str) -> Vec<DiffHunk> {
        let result = stoat_language::structural_diff::diff(base, buffer);
        crate::diff_map::changes_to_hunks(&result.changes, base, buffer)
    }

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
    /// leaves the other alone, and an index-to-HEAD patch takes it back out.
    #[test]
    fn a_hunk_patch_stages_only_its_own_change() {
        const BASE: &str = "a\nb\nc\nd\ne\nf\ng\nh\n";
        const BUFFER: &str = "a\nZ\nc\nd\ne\nf\nY\nh\n";
        let (_dir, repo, host_repo) = index_repo(BASE, BUFFER);
        let rel = Path::new("a.rs");

        let buffer_hunks = hunks(BASE, BUFFER);
        assert_eq!(
            buffer_hunks.len(),
            2,
            "the file has two hunks to keep apart"
        );
        let stage = hunk_to_patch(rel, BASE, BUFFER, &buffer_hunks, 0).expect("hunk 0 exists");
        host_repo
            .apply_to_index(&stage)
            .expect("the hunk patch must apply to real libgit2");
        let staged = staged_text(&repo);
        assert_eq!(
            staged, "a\nZ\nc\nd\ne\nf\ng\nh\n",
            "the index carries the first change and not the second"
        );

        let unstage =
            hunk_to_patch(rel, &staged, BASE, &hunks(&staged, BASE), 0).expect("hunk 0 is staged");
        host_repo
            .apply_to_index(&unstage)
            .expect("the unstage patch must apply too");
        assert_eq!(
            staged_text(&repo),
            BASE,
            "and the unstage takes it back out"
        );
    }

    /// The first hunk adds a line and stays out of the index, so the second
    /// sits one row higher in the index than in the buffer. libgit2 looks for
    /// the hunk exactly at its `+` start, so that start has to name the index
    /// row.
    #[test]
    fn a_hunk_below_an_unstaged_hunk_stages_first() {
        const BASE: &str = "a\nb\nc\nd\ne\nf\ng\nh\n";
        const BUFFER: &str = "a\nZ1\nZ2\nc\nd\ne\nf\nY\nh\n";
        let (_dir, repo, host_repo) = index_repo(BASE, BUFFER);

        let patch = hunk_to_patch(Path::new("a.rs"), BASE, BUFFER, &hunks(BASE, BUFFER), 1)
            .expect("hunk 1 exists");
        host_repo
            .apply_to_index(&patch)
            .expect("the second hunk applies with the first unstaged");
        assert_eq!(staged_text(&repo), "a\nb\nc\nd\ne\nf\nY\nh\n");
    }

    /// The buffer rows of each line hunk between `base` and `buffer`.
    fn buffer_rows(base: &str, buffer: &str) -> Vec<Range<u32>> {
        hunks(base, buffer)
            .into_iter()
            .map(|hunk| hunk.buffer_line_range)
            .collect()
    }

    /// The index text after hunk `k` stages over a HEAD and index at `base`.
    fn stage_hunk(base: &str, buffer: &str, k: usize) -> String {
        let (_dir, repo, host_repo) = index_repo(base, buffer);
        let patch = hunk_to_patch(Path::new("a.rs"), base, buffer, &hunks(base, buffer), k)
            .expect("the hunk exists");
        host_repo
            .apply_to_index(&patch)
            .expect("the hunk patch applies to real libgit2");
        staged_text(&repo)
    }

    /// The line differ gives a lone blank line an empty byte range, and the
    /// hunk below it still sits one index line above its buffer row.
    #[test]
    fn a_hunk_below_an_added_blank_line_stages() {
        const BASE: &str = "a\nb\nc\nd\ne\nf\ng\nh\n";
        const BUFFER: &str = "a\nb\n\nc\nd\ne\nf\nY\nh\n";
        assert_eq!(buffer_rows(BASE, BUFFER), [2..3, 7..8]);
        assert_eq!(stage_hunk(BASE, BUFFER, 1), "a\nb\nc\nd\ne\nf\nY\nh\n");
    }

    #[test]
    fn a_hunk_below_a_deleted_blank_line_stages() {
        const BASE: &str = "a\nb\n\nc\nd\ne\nf\ng\nh\n";
        const BUFFER: &str = "a\nb\nc\nd\ne\nf\nY\nh\n";
        assert_eq!(buffer_rows(BASE, BUFFER), [2..2, 6..7]);
        assert_eq!(stage_hunk(BASE, BUFFER, 1), "a\nb\n\nc\nd\ne\nf\nY\nh\n");
    }

    #[test]
    fn a_deleted_run_that_ends_in_a_blank_line_stages_whole() {
        assert_eq!(stage_hunk("a\nb\n\nc\n", "a\nc\n", 0), "a\nc\n");
    }

    #[test]
    fn an_added_blank_line_stages_on_its_own() {
        assert_eq!(stage_hunk("a\nb\n", "a\n\nb\n", 0), "a\n\nb\n");
    }

    /// The index text after the 1-based buffer `lines` of hunk 0 stage over a
    /// HEAD and index at `base`.
    fn stage_lines(base: &str, buffer: &str, lines: Range<u32>) -> String {
        let (_dir, repo, host_repo) = index_repo(base, buffer);
        let rows =
            hunk_rows(base, buffer, &hunks(base, buffer), 0, HUNK_CONTEXT).expect("hunk 0 exists");
        let rows = line_restricted_rows(&rows, lines, true).expect("a change sits on the lines");
        let patch = rows_to_unified_diff(Path::new("a.rs"), base, buffer, &rows);
        host_repo
            .apply_to_index(&patch)
            .expect("the line patch applies to real libgit2");
        staged_text(&repo)
    }

    /// The index's last line has no newline, and the staged line goes after it,
    /// so that line gains one in the index.
    #[test]
    fn a_line_staged_after_an_unterminated_last_line_keeps_both_lines() {
        assert_eq!(stage_lines("a\nb", "a\nb\nc", 3..4), "a\nb\nc");
    }

    /// An unselected row keeps its index line as the index ends it, though the
    /// buffer ends without a newline.
    #[test]
    fn a_line_staged_from_an_unterminated_buffer_keeps_the_index_ending() {
        assert_eq!(stage_lines("x\ny\n", "X\nY", 1..2), "X\ny\n");
    }

    /// The buffer's unterminated last line replaces a line above a removed one.
    #[test]
    fn a_hunk_that_ends_unterminated_above_a_removal_stages() {
        assert_eq!(stage_hunk("a\nb\nc\n", "a\nB", 0), "a\nB");
    }
}
