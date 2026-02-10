//! Utilities for generating git patch files from diff hunks.
//!
//! This module provides functionality to convert [`DiffHunk`](crate::git::diff::DiffHunk)
//! instances into unified diff format patches that can be applied with `git apply`.
//! Used by [`git_stage_hunk`](crate::Stoat::git_stage_hunk) and
//! [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) actions.

use crate::git::{
    diff::{BufferDiff, DiffHunk, DiffHunkStatus, HunkLineOrigin},
    line_selection::LineSelection,
};
use std::path::Path;
use text::{BufferSnapshot, ToPoint};

/// Generates a unified diff format patch for a single hunk.
///
/// Creates a patch in unified diff format that can be applied to the git staging area
/// using `git apply --cached --unidiff-zero`. The patch includes file headers, hunk
/// header with line numbers, and the diff content based on hunk status (added, deleted,
/// or modified).
///
/// # Arguments
///
/// * `diff` - The [`BufferDiff`] containing the base text for comparison
/// * `hunk` - The [`DiffHunk`] to generate a patch for
/// * `buffer_snapshot` - The current buffer state to extract new content from
/// * `file_path` - Path to the file being patched, used for patch headers
///
/// # Returns
///
/// A string containing the complete unified diff patch, or an error if the file name
/// is invalid.
///
/// # Patch Format
///
/// The generated patch follows the unified diff format:
/// ```text
/// --- a/filename
/// +++ b/filename
/// @@ -old_start,old_lines +new_start,new_lines @@
/// -deleted line
/// +added line
/// ```
///
/// # Example
///
/// This function is used internally by git staging actions:
/// ```ignore
/// let patch = generate_hunk_patch(&diff, &hunk, &buffer_snapshot, &file_path)?;
/// // Apply patch with: git apply --cached --unidiff-zero
/// ```
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

    // File headers
    patch.push_str(&format!("--- a/{file_name}\n"));
    patch.push_str(&format!("+++ b/{file_name}\n"));

    // Get hunk line numbers
    let buffer_start = hunk.buffer_range.start.to_point(buffer_snapshot);
    let buffer_end = hunk.buffer_range.end.to_point(buffer_snapshot);

    let new_start = buffer_start.row + 1; // git uses 1-indexed
    let new_lines = buffer_end.row.saturating_sub(buffer_start.row);

    // Calculate old (HEAD) line numbers from base text
    let base_content = &diff.base_text[hunk.diff_base_byte_range.clone()];
    let old_lines = base_content.lines().count() as u32;

    // For added hunks, old_start should point to the line where insertion happened
    // For other hunks, calculate from the hunk's position
    let old_start = if old_lines == 0 {
        new_start
    } else {
        // Count lines before this hunk in base text
        let bytes_before = hunk.diff_base_byte_range.start;
        let lines_before = diff.base_text[..bytes_before].lines().count() as u32;
        lines_before + 1
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
    patch.push_str(&format!("--- a/{file_name}\n"));
    patch.push_str(&format!("+++ b/{file_name}\n"));
    patch.push_str(&format!(
        "@@ -{old_start},{old_count} +{new_start},{new_count} @@\n"
    ));
    patch.push_str(&body);

    Ok(patch)
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

/// Apply a unified diff patch to the git staging area.
///
/// Pipes the patch to `git apply --cached --unidiff-zero`. When `reverse` is true,
/// adds `--reverse` to unstage instead.
pub(super) fn apply_patch(patch: &str, repo_dir: &Path, reverse: bool) -> Result<(), String> {
    let mut args = vec!["apply", "--cached", "--unidiff-zero"];
    if reverse {
        args.push("--reverse");
    }
    args.push("-");

    let mut child = std::process::Command::new("git")
        .args(&args)
        .current_dir(repo_dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("Failed to spawn git apply: {e}"))?;

    {
        use std::io::Write;
        let stdin = child
            .stdin
            .as_mut()
            .ok_or_else(|| "Failed to open stdin".to_string())?;
        stdin
            .write_all(patch.as_bytes())
            .map_err(|e| format!("Failed to write patch to stdin: {e}"))?;
    }

    let output = child
        .wait_with_output()
        .map_err(|e| format!("Failed to wait for git apply: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("git apply failed: {stderr}"));
    }

    Ok(())
}
