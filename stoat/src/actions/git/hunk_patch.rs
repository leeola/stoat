//! Utilities for generating and applying git patch files from diff hunks.
//!
//! This module provides functionality to convert [`DiffHunk`](crate::git::diff::DiffHunk)
//! instances into unified diff format patches, and to apply them to the git index
//! via libgit2. Used by [`git_stage_hunk`](crate::Stoat::git_stage_hunk) and
//! [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) actions.

use crate::git::{
    diff::{BufferDiff, DiffHunk, DiffHunkStatus, HunkLineOrigin},
    line_selection::LineSelection,
};
use git2::{ApplyLocation, Diff, Repository};
use std::path::Path;
use text::{BufferSnapshot, ToPoint};

/// Generates a unified diff format patch for a single hunk.
///
/// Creates a zero-context patch suitable for [`apply_patch`] via libgit2. For Added
/// hunks (old_lines == 0), `new_start` is set to `old_start + 1` so that insertions
/// land after the anchor line rather than before it.
pub(super) fn generate_hunk_patch(
    diff: &BufferDiff,
    hunk: &DiffHunk,
    buffer_snapshot: &BufferSnapshot,
    file_path: &std::path::Path,
) -> Result<String, String> {
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "Invalid file name".to_string())?;

    let mut patch = String::new();

    patch.push_str(&format!("diff --git a/{file_name} b/{file_name}\n"));
    patch.push_str(&format!("--- a/{file_name}\n"));
    patch.push_str(&format!("+++ b/{file_name}\n"));

    let buffer_start = hunk.buffer_range.start.to_point(buffer_snapshot);
    let buffer_end = hunk.buffer_range.end.to_point(buffer_snapshot);

    let base_content = &diff.base_text[hunk.diff_base_byte_range.clone()];
    let old_start = hunk.old_start;
    let old_lines = hunk.old_lines;
    let new_lines = buffer_end.row.saturating_sub(buffer_start.row);
    let new_start = if old_lines == 0 {
        old_start + 1
    } else {
        old_start
    };

    // Hunk header
    patch.push_str(&format!(
        "@@ -{old_start},{old_lines} +{new_start},{new_lines} @@\n"
    ));

    // Hunk content
    match hunk.status {
        DiffHunkStatus::Added => {
            // Added lines - get from buffer
            let buffer_text = buffer_snapshot.text();
            let start_offset = buffer_snapshot.point_to_offset(buffer_start);
            let end_offset = buffer_snapshot.point_to_offset(buffer_end);
            let content = &buffer_text[start_offset..end_offset];

            for line in content.lines() {
                patch.push_str(&format!("+{line}\n"));
            }
        },
        DiffHunkStatus::Deleted => {
            // Deleted lines - get from base text
            for line in base_content.lines() {
                patch.push_str(&format!("-{line}\n"));
            }
        },
        DiffHunkStatus::Modified => {
            // Modified - show deletion then addition
            for line in base_content.lines() {
                patch.push_str(&format!("-{line}\n"));
            }

            let buffer_text = buffer_snapshot.text();
            let start_offset = buffer_snapshot.point_to_offset(buffer_start);
            let end_offset = buffer_snapshot.point_to_offset(buffer_end);
            let content = &buffer_text[start_offset..end_offset];

            for line in content.lines() {
                patch.push_str(&format!("+{line}\n"));
            }
        },
    }

    Ok(patch)
}

/// Generate a partial patch from a [`LineSelection`], including only selected lines.
///
/// Unselected `-` lines become context (` `), unselected `+` lines are omitted,
/// and the `@@` header is recomputed from actual included lines.
pub(super) fn generate_partial_hunk_patch(
    selection: &LineSelection,
    file_path: &Path,
) -> Result<String, String> {
    let file_name = file_path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| "Invalid file name".to_string())?;

    let mut body = String::new();
    let mut old_count: u32 = 0;
    let mut new_count: u32 = 0;

    for (i, line) in selection.hunk_lines.lines.iter().enumerate() {
        let content = line.content.trim_end_matches('\n');
        match line.origin {
            HunkLineOrigin::Context => {
                body.push_str(&format!(" {content}\n"));
                old_count += 1;
                new_count += 1;
            },
            HunkLineOrigin::Deletion => {
                if selection.selected[i] {
                    body.push_str(&format!("-{content}\n"));
                    old_count += 1;
                } else {
                    body.push_str(&format!(" {content}\n"));
                    old_count += 1;
                    new_count += 1;
                }
            },
            HunkLineOrigin::Addition => {
                if selection.selected[i] {
                    body.push_str(&format!("+{content}\n"));
                    new_count += 1;
                }
            },
        }
    }

    let old_start = selection.hunk_lines.old_start;
    let new_start = selection.hunk_lines.new_start;

    let mut patch = String::new();
    patch.push_str(&format!("diff --git a/{file_name} b/{file_name}\n"));
    patch.push_str(&format!("--- a/{file_name}\n"));
    patch.push_str(&format!("+++ b/{file_name}\n"));
    patch.push_str(&format!(
        "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
    ));
    patch.push_str(&body);

    Ok(patch)
}

/// Apply a unified diff patch via libgit2.
///
/// When `reverse` is true, the patch is reversed (swapping additions/deletions
/// and header fields) before application, used for unstaging or reverting.
/// The `location` parameter controls where the patch is applied (Index or WorkDir).
pub(super) fn apply_patch(
    patch: &str,
    repo_dir: &Path,
    reverse: bool,
    location: ApplyLocation,
) -> Result<(), String> {
    let repo = Repository::open(repo_dir).map_err(|e| format!("Failed to open repository: {e}"))?;

    let patch_bytes = if reverse {
        reverse_patch(patch)
    } else {
        patch.to_string()
    };

    let diff = Diff::from_buffer(patch_bytes.as_bytes())
        .map_err(|e| format!("Failed to parse patch: {e}"))?;

    repo.apply(&diff, location, None)
        .map_err(|e| format!("Failed to apply patch: {e}"))?;

    Ok(())
}

/// Reverse a unified diff patch by swapping additions/deletions and the `@@` header ranges.
///
/// The `diff --git`, `---`, and `+++` header lines are kept unchanged so that
/// libgit2 can match the path names consistently.
fn reverse_patch(patch: &str) -> String {
    let mut result = String::with_capacity(patch.len());
    for line in patch.lines() {
        if line.starts_with("diff --git ") || line.starts_with("--- ") || line.starts_with("+++ ") {
            result.push_str(line);
            result.push('\n');
        } else if let Some(rest) = line.strip_prefix("@@ ") {
            if let Some(reversed) = reverse_hunk_header(rest) {
                result.push_str(&format!("@@ {reversed}\n"));
            } else {
                result.push_str(line);
                result.push('\n');
            }
        } else if let Some(rest) = line.strip_prefix('-') {
            result.push('+');
            result.push_str(rest);
            result.push('\n');
        } else if let Some(rest) = line.strip_prefix('+') {
            result.push('-');
            result.push_str(rest);
            result.push('\n');
        } else {
            result.push_str(line);
            result.push('\n');
        }
    }
    result
}

/// Swap old/new ranges in a hunk header: `-A,B +C,D @@` becomes `-C,D +A,B @@`.
///
/// For zero-count ranges, the start position is set to match the other side's
/// start, since libgit2 requires positional consistency for empty ranges.
fn reverse_hunk_header(header: &str) -> Option<String> {
    let at_end = header.find(" @@")?;
    let ranges = &header[..at_end];
    let after = &header[at_end..];

    let plus_idx = ranges.find(" +")?;
    let old_range = &ranges[1..plus_idx];
    let new_range = &ranges[plus_idx + 2..];

    fn parse_range(r: &str) -> Option<(u32, u32)> {
        let (start, count) = r.split_once(',')?;
        Some((start.parse().ok()?, count.parse().ok()?))
    }

    let (old_start, old_count) = parse_range(old_range)?;
    let (new_start, new_count) = parse_range(new_range)?;

    let rev_old_start = new_start;
    let rev_old_count = new_count;
    let rev_new_start = if old_count == 0 { new_start } else { old_start };
    let rev_new_count = old_count;

    Some(format!(
        "-{rev_old_start},{rev_old_count} +{rev_new_start},{rev_new_count}{after}"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::{
        diff::{HunkLine, HunkLineOrigin, HunkLines},
        line_selection::LineSelection,
    };
    use std::path::Path;

    fn make_hunk_lines(
        old_start: u32,
        new_start: u32,
        specs: &[(HunkLineOrigin, &str)],
    ) -> HunkLines {
        let old_lines = specs
            .iter()
            .filter(|(o, _)| matches!(o, HunkLineOrigin::Deletion | HunkLineOrigin::Context))
            .count() as u32;
        let new_lines = specs
            .iter()
            .filter(|(o, _)| matches!(o, HunkLineOrigin::Addition | HunkLineOrigin::Context))
            .count() as u32;
        HunkLines {
            old_start,
            old_lines,
            new_start,
            new_lines,
            lines: specs
                .iter()
                .map(|(origin, content)| HunkLine {
                    origin: *origin,
                    content: format!("{content}\n"),
                    old_lineno: None,
                    new_lineno: None,
                })
                .collect(),
        }
    }

    #[test]
    fn all_lines_selected() {
        use HunkLineOrigin::{Addition, Deletion};
        let hl = make_hunk_lines(
            10,
            10,
            &[
                (Deletion, "old_a"),
                (Deletion, "old_b"),
                (Addition, "new_a"),
                (Addition, "new_b"),
            ],
        );
        let sel = LineSelection::new(hl);
        let patch = generate_partial_hunk_patch(&sel, Path::new("foo.rs")).unwrap();
        assert!(patch.contains("--- a/foo.rs\n"));
        assert!(patch.contains("+++ b/foo.rs\n"));
        assert!(patch.contains("@@ -10,2 +10,2 @@\n"));
        assert!(patch.contains("-old_a\n"));
        assert!(patch.contains("-old_b\n"));
        assert!(patch.contains("+new_a\n"));
        assert!(patch.contains("+new_b\n"));
    }

    #[test]
    fn unselected_deletion_becomes_context() {
        use HunkLineOrigin::{Addition, Deletion};
        let hl = make_hunk_lines(
            5,
            5,
            &[
                (Deletion, "old_a"),
                (Deletion, "old_b"),
                (Addition, "new_a"),
            ],
        );
        let mut sel = LineSelection::new(hl);
        // Deselect first deletion
        sel.selected[0] = false;
        let patch = generate_partial_hunk_patch(&sel, Path::new("f.rs")).unwrap();
        // old_a becomes context: appears as " old_a", counts in both old and new
        assert!(patch.contains(" old_a\n"));
        assert!(patch.contains("-old_b\n"));
        assert!(patch.contains("+new_a\n"));
        // old_count=2 (context old_a + deletion old_b), new_count=2 (context old_a + addition
        // new_a)
        assert!(patch.contains("@@ -5,2 +5,2 @@\n"));
    }

    #[test]
    fn unselected_addition_omitted() {
        use HunkLineOrigin::{Addition, Deletion};
        let hl = make_hunk_lines(
            3,
            3,
            &[(Deletion, "old"), (Addition, "new_a"), (Addition, "new_b")],
        );
        let mut sel = LineSelection::new(hl);
        // Deselect second addition
        sel.selected[2] = false;
        let patch = generate_partial_hunk_patch(&sel, Path::new("f.rs")).unwrap();
        assert!(patch.contains("-old\n"));
        assert!(patch.contains("+new_a\n"));
        assert!(!patch.contains("new_b"));
        // old_count=1, new_count=1
        assert!(patch.contains("@@ -3,1 +3,1 @@\n"));
    }

    #[test]
    fn mixed_selection() {
        use HunkLineOrigin::{Addition, Context, Deletion};
        let hl = make_hunk_lines(
            1,
            1,
            &[
                (Context, "ctx"),
                (Deletion, "del_a"),
                (Deletion, "del_b"),
                (Addition, "add_a"),
                (Addition, "add_b"),
            ],
        );
        let mut sel = LineSelection::new(hl);
        // Deselect del_a (index 1) and add_b (index 4)
        sel.selected[1] = false;
        sel.selected[4] = false;
        let patch = generate_partial_hunk_patch(&sel, Path::new("f.rs")).unwrap();
        assert!(patch.contains(" ctx\n"), "missing context line");
        assert!(patch.contains(" del_a\n"), "del_a should be context");
        assert!(patch.contains("-del_b\n"), "del_b should be deletion");
        assert!(patch.contains("+add_a\n"), "add_a should be addition");
        assert!(!patch.contains("add_b"), "add_b should be omitted");
        // old: ctx(1) + context-del_a(1) + del_b(1) = 3
        // new: ctx(1) + context-del_a(1) + add_a(1) = 3
        assert!(patch.contains("@@ -1,3 +1,3 @@\n"));
    }

    #[test]
    fn no_lines_selected() {
        use HunkLineOrigin::{Addition, Deletion};
        let hl = make_hunk_lines(7, 7, &[(Deletion, "old"), (Addition, "new")]);
        let mut sel = LineSelection::new(hl);
        sel.deselect_all();
        assert!(!sel.has_selection());
        let patch = generate_partial_hunk_patch(&sel, Path::new("f.rs")).unwrap();
        // Deletion becomes context, addition omitted
        assert!(patch.contains(" old\n"));
        assert!(!patch.contains("new"));
        // old_count=1 (context), new_count=1 (context)
        assert!(patch.contains("@@ -7,1 +7,1 @@\n"));
    }
}
