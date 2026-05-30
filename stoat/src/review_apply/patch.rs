//! Unified-diff emission for a single [`ReviewChunk`].
//!
//! The output is a minimal patch file that `git apply --cached` (and
//! `git2::Diff::from_buffer` + `Repository::apply(..., ApplyLocation::Index)`)
//! will accept. One chunk produces exactly one hunk with its own
//! `--- a/<rel>` / `+++ b/<rel>` headers so callers can apply a subset
//! of a file's chunks independently.

use crate::{
    review::{line_count, ReviewRow, ReviewSide},
    review_session::{ReviewChunk, ReviewFile},
};
use std::path::Path;

const NO_NEWLINE_MARKER: &str = "\\ No newline at end of file\n";

/// Serializes `chunk` into a standalone unified-diff patch keyed at
/// `file.path` relative to `workdir`. The caller is expected to feed
/// the resulting string to a git-apply-compatible consumer.
///
/// The patch has exactly one hunk. When the underlying source text
/// lacks a trailing newline and the chunk covers the file's final
/// line, `\ No newline at end of file` is emitted on the affected
/// side(s) per the gnu diff convention.
///
/// When `reverse` is set, the base (LHS) and buffer (RHS) sides trade
/// places: the hunk's from/to ranges swap, `-`/`+` line prefixes flip,
/// and a forward file creation is emitted as a deletion (and vice
/// versa). Applying the reversed patch undoes the forward one.
pub fn chunk_to_unified_diff(
    file: &ReviewFile,
    chunk: &ReviewChunk,
    workdir: &Path,
    reverse: bool,
) -> String {
    let rel = file.path.strip_prefix(workdir).unwrap_or(&file.path);
    let rel_display = rel.display();

    let (base_start, base_count) = base_header(&chunk.hunk.rows);
    let (buffer_start, buffer_count) = buffer_header(&chunk.hunk.rows);

    let base_total = line_count(&file.base_text);
    let buffer_total = line_count(&file.buffer_text);
    let base_no_nl = !file.base_text.is_empty() && !file.base_text.ends_with('\n');
    let buffer_no_nl = !file.buffer_text.is_empty() && !file.buffer_text.ends_with('\n');

    let last_left_idx = last_row_with_left(&chunk.hunk.rows);
    let last_right_idx = last_row_with_right(&chunk.hunk.rows);

    let base_is_new_file = file.base_text.is_empty();
    let buffer_is_deleted_file = file.buffer_text.is_empty();

    // `reverse` swaps the base (LHS) and buffer (RHS) roles so the patch
    // undoes the forward one: from/to ranges trade places, a creation
    // becomes a deletion, and the per-row prefixes flip below.
    let (from_start, from_count, to_start, to_count) = if reverse {
        (buffer_start, buffer_count, base_start, base_count)
    } else {
        (base_start, base_count, buffer_start, buffer_count)
    };
    let from_is_dev_null = if reverse {
        buffer_is_deleted_file
    } else {
        base_is_new_file
    };
    let to_is_dev_null = if reverse {
        base_is_new_file
    } else {
        buffer_is_deleted_file
    };
    let (left_prefix, right_prefix) = if reverse { ('+', '-') } else { ('-', '+') };

    let mut out = String::new();
    out.push_str(&format!("diff --git a/{rel_display} b/{rel_display}\n"));
    if from_is_dev_null {
        out.push_str("new file mode 100644\n");
    } else if to_is_dev_null {
        out.push_str("deleted file mode 100644\n");
    }
    if from_is_dev_null {
        out.push_str("--- /dev/null\n");
    } else {
        out.push_str(&format!("--- a/{rel_display}\n"));
    }
    if to_is_dev_null {
        out.push_str("+++ /dev/null\n");
    } else {
        out.push_str(&format!("+++ b/{rel_display}\n"));
    }
    out.push_str(&format!(
        "@@ -{from_start},{from_count} +{to_start},{to_count} @@\n"
    ));

    for (i, row) in chunk.hunk.rows.iter().enumerate() {
        let is_last_left = Some(i) == last_left_idx;
        let is_last_right = Some(i) == last_right_idx;

        match row {
            ReviewRow::Context { left, right } => {
                let text = if reverse { &left.text } else { &right.text };
                emit_prefixed(&mut out, ' ', text);
                let left_at_eof = base_no_nl && is_last_left && touches_base_eof(left, base_total);
                let right_at_eof =
                    buffer_no_nl && is_last_right && touches_buffer_eof(right, buffer_total);
                if left_at_eof || right_at_eof {
                    out.push_str(NO_NEWLINE_MARKER);
                }
            },
            ReviewRow::Changed { left, right } => {
                let emit_left = |out: &mut String| {
                    if let Some(l) = left {
                        emit_prefixed(out, left_prefix, &l.text);
                        if base_no_nl && is_last_left && touches_base_eof(l, base_total) {
                            out.push_str(NO_NEWLINE_MARKER);
                        }
                    }
                };
                let emit_right = |out: &mut String| {
                    if let Some(r) = right {
                        emit_prefixed(out, right_prefix, &r.text);
                        if buffer_no_nl && is_last_right && touches_buffer_eof(r, buffer_total) {
                            out.push_str(NO_NEWLINE_MARKER);
                        }
                    }
                };

                // Removed side first in both directions: forward emits the
                // base (`-`) then the buffer (`+`); reverse flips both the
                // prefixes and the order so the buffer line is the removal.
                if reverse {
                    emit_right(&mut out);
                    emit_left(&mut out);
                } else {
                    emit_left(&mut out);
                    emit_right(&mut out);
                }
            },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review_session::{InMemoryFile, ReviewSession, ReviewSource};
    use std::{path::PathBuf, sync::Arc};

    fn session_with_file(path: &str, base: &str, buffer: &str) -> ReviewSession {
        let mut s = ReviewSession::new(ReviewSource::InMemory {
            files: Arc::new(Vec::<InMemoryFile>::new()),
        });
        s.add_file(
            PathBuf::from(format!("/work/{path}")),
            path.to_string(),
            None,
            Arc::new(base.to_string()),
            Arc::new(buffer.to_string()),
        );
        s
    }

    fn first_chunk_patch(path: &str, base: &str, buffer: &str) -> String {
        let session = session_with_file(path, base, buffer);
        let id = session.order[0];
        let chunk = &session.chunks[&id];
        let file = &session.files[chunk.file_index];
        chunk_to_unified_diff(file, chunk, Path::new("/work"), false)
    }

    #[test]
    fn modification_emits_paired_minus_plus() {
        let patch = first_chunk_patch("a.txt", "a\nOLD\nc\n", "a\nNEW\nc\n");
        assert!(patch.starts_with("diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n"));
        assert!(patch.contains("@@ -1,3 +1,3 @@\n"));
        assert!(patch.contains("-OLD\n"));
        assert!(patch.contains("+NEW\n"));
        assert!(patch.contains(" a\n"));
        assert!(patch.contains(" c\n"));
    }

    #[test]
    fn pure_addition_has_zero_base_count() {
        let patch = first_chunk_patch("a.txt", "a\nb\n", "a\nNEW\nb\n");
        assert!(
            patch.contains("@@ -1,2 +1,3 @@\n"),
            "header must include surrounding context: {patch}"
        );
        assert!(patch.contains("+NEW\n"));
    }

    #[test]
    fn pure_deletion_has_zero_buffer_count_for_addition_only_lines() {
        let patch = first_chunk_patch("a.txt", "a\nOLD\nb\n", "a\nb\n");
        assert!(patch.contains("@@ -1,3 +1,2 @@\n"), "header: {patch}");
        assert!(patch.contains("-OLD\n"));
    }

    #[test]
    fn pure_addition_at_file_start_uses_zero_base_start() {
        let patch = first_chunk_patch("a.txt", "", "NEW\n");
        assert!(
            patch.contains("new file mode 100644\n"),
            "new-file path must carry new file mode: {patch}"
        );
        assert!(
            patch.contains("--- /dev/null\n"),
            "new-file LHS must be /dev/null: {patch}"
        );
        assert!(
            patch.contains("@@ -0,0 +1,1 @@\n"),
            "pure-addition header should anchor at 0,0: {patch}"
        );
        assert!(patch.ends_with("+NEW\n"));
    }

    #[test]
    fn pure_deletion_to_empty_uses_zero_buffer_start() {
        let patch = first_chunk_patch("a.txt", "OLD\n", "");
        assert!(
            patch.contains("deleted file mode 100644\n"),
            "deleted-file path must carry deleted file mode: {patch}"
        );
        assert!(
            patch.contains("+++ /dev/null\n"),
            "deleted-file RHS must be /dev/null: {patch}"
        );
        assert!(
            patch.contains("@@ -1,1 +0,0 @@\n"),
            "pure-deletion header should anchor buffer at 0,0: {patch}"
        );
        assert!(patch.ends_with("-OLD\n"));
    }

    #[test]
    fn context_rows_use_space_prefix() {
        let patch = first_chunk_patch(
            "a.txt",
            "a\nb\nc\nd\ne\nf\nOLD\ng\nh\n",
            "a\nb\nc\nd\ne\nf\nNEW\ng\nh\n",
        );
        let body_lines: Vec<&str> = patch
            .lines()
            .skip_while(|l| !l.starts_with("@@"))
            .skip(1)
            .collect();
        for line in &body_lines {
            let first = line.chars().next().unwrap_or(' ');
            assert!(
                matches!(first, ' ' | '-' | '+' | '\\'),
                "unexpected prefix in body line: {line:?}"
            );
        }
        let ctx_count = body_lines.iter().filter(|l| l.starts_with(' ')).count();
        assert!(
            ctx_count >= 4,
            "expected context lines above/below: {patch}"
        );
    }

    #[test]
    fn rel_path_strips_workdir_prefix() {
        let mut session = ReviewSession::new(ReviewSource::InMemory {
            files: Arc::new(Vec::<InMemoryFile>::new()),
        });
        session.add_file(
            PathBuf::from("/work/sub/a.txt"),
            "sub/a.txt".into(),
            None,
            Arc::new("a\n".to_string()),
            Arc::new("b\n".to_string()),
        );
        let chunk = &session.chunks[&session.order[0]];
        let file = &session.files[chunk.file_index];
        let patch = chunk_to_unified_diff(file, chunk, Path::new("/work"), false);
        assert!(
            patch.starts_with(
                "diff --git a/sub/a.txt b/sub/a.txt\n--- a/sub/a.txt\n+++ b/sub/a.txt\n"
            ),
            "rel path should be relative to workdir: {patch}"
        );
    }

    #[test]
    fn base_without_trailing_newline_emits_no_newline_marker() {
        let patch = first_chunk_patch("a.txt", "a\nOLD", "a\nNEW\n");
        assert!(
            patch.contains("-OLD\n\\ No newline at end of file\n"),
            "no-newline marker must follow base EOF line: {patch}"
        );
        assert!(
            !patch.contains("+NEW\n\\ No newline"),
            "buffer side has trailing newline; marker must not be emitted there: {patch}"
        );
    }

    #[test]
    fn buffer_without_trailing_newline_emits_no_newline_marker() {
        let patch = first_chunk_patch("a.txt", "a\nOLD\n", "a\nNEW");
        assert!(
            patch.contains("+NEW\n\\ No newline at end of file\n"),
            "marker must follow buffer EOF line: {patch}"
        );
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

    #[test]
    fn emitted_patch_from_chunk_applies_cleanly() {
        use crate::host::{fake::FakeGit, GitHost};

        let workdir = PathBuf::from("/repo");
        let git = FakeGit::new();
        git.add_repo(workdir.clone())
            .head_file("a.rs", "line1\nOLD\nline3\n");

        let mut session = ReviewSession::new(ReviewSource::WorkingTree {
            workdir: workdir.clone(),
        });
        session.add_file(
            workdir.join("a.rs"),
            "a.rs".into(),
            None,
            Arc::new("line1\nOLD\nline3\n".into()),
            Arc::new("line1\nNEW\nline3\n".into()),
        );
        let id = session.order[0];
        let chunk = &session.chunks[&id];
        let file = &session.files[chunk.file_index];
        let patch = chunk_to_unified_diff(file, chunk, &workdir, false);

        let host_repo = git.discover(&workdir).unwrap();
        host_repo
            .apply_to_index(&patch)
            .expect("emitted patch must apply to the fake index");

        assert_eq!(
            git.staged_content(&workdir, "a.rs"),
            Some("line1\nNEW\nline3\n".to_string()),
            "staged index must reflect the applied change"
        );
    }
}
