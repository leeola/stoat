//! Utilities for generating git patch files from diff hunks.
//!
//! This module provides functionality to convert [`DiffHunk`](crate::git_diff::DiffHunk)
//! instances into unified diff format patches that can be applied with `git apply`.
//! Used by [`git_stage_hunk`](crate::Stoat::git_stage_hunk) and
//! [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) actions.

use crate::git_diff::{BufferDiff, DiffHunk, DiffHunkStatus};
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
