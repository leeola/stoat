//! Git diff computation and representation.
//!
//! This module provides data structures and functions for computing and representing diffs
//! between the working tree and git HEAD. It's the core of the git diff visualization system.
//!
//! # Architecture
//!
//! The diff system has three main components:
//!
//! 1. [`DiffHunk`] - Represents a single changed region (add/modify/delete)
//! 2. [`BufferDiff`] - Container for all hunks in a buffer, plus base text
//! 3. [`compute_diff`] - Function that computes hunks using git2
//!
//! ## How Diffs Work
//!
//! Diffs are computed by comparing the current buffer content with the git HEAD content:
//!
//! ```ignore
//! let repo = Repository::discover(file_path)?;
//! let head_content = repo.head_content(file_path)?;
//! let diff = BufferDiff::new(buffer_id, head_content, &buffer_snapshot)?;
//! ```
//!
//! The [`DiffHunk`] uses anchors from Zed's text system, which automatically track
//! positions as the buffer is edited. This ensures diff hunks remain correct even
//! as the user types.
//!
//! # Diff Status
//!
//! Three types of changes are tracked:
//!
//! - **Added**: New lines added to the buffer (green in UI)
//! - **Modified**: Existing lines changed (blue in UI)
//! - **Deleted**: Lines removed from buffer (red in UI)
//!
//! # Usage
//!
//! ```ignore
//! use crate::git_diff::BufferDiff;
//! use crate::git_repository::Repository;
//!
//! let repo = Repository::discover(Path::new("src/main.rs"))?;
//! let head_content = repo.head_content(Path::new("src/main.rs"))?;
//! let buffer_snapshot = buffer.read(cx).snapshot();
//!
//! let diff = BufferDiff::new(buffer.id(), head_content, &buffer_snapshot)?;
//!
//! for hunk in &diff.hunks {
//!     println!("Changed at line {}: {:?}", hunk.buffer_range.start.to_point(&buffer_snapshot).row, hunk.status);
//! }
//! ```
//!
//! # Related
//!
//! - [`git_repository`](crate::git_repository) - Provides access to git HEAD content
//! - [`BufferItem`](crate::BufferItem) - Stores computed diffs for display

use std::ops::Range;
use sum_tree::Bias;
use text::{Anchor, BufferId, BufferSnapshot, ToPoint};
use thiserror::Error;

/// Errors that can occur during diff computation.
#[derive(Debug, Error)]
pub enum DiffError {
    /// Failed to create git diff options
    #[error("Failed to create diff options: {0}")]
    DiffOptionsFailed(String),

    /// Failed to compute diff patch
    #[error("Failed to compute diff patch: {0}")]
    PatchFailed(String),

    /// Failed to parse diff hunk
    #[error("Failed to parse diff hunk: {0}")]
    HunkParseFailed(String),
}

/// Status of a diff hunk indicating the type of change.
///
/// Used to determine visual styling in the gutter (color of the indicator bar).
///
/// # Display
///
/// - [`Added`](Self::Added) - Green bar
/// - [`Modified`](Self::Modified) - Blue bar
/// - [`Deleted`](Self::Deleted) - Red bar (shows where lines were deleted)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffHunkStatus {
    /// Lines added to the buffer (not in HEAD)
    Added,
    /// Lines modified from HEAD version
    Modified,
    /// Lines deleted from HEAD (not in buffer)
    Deleted,
}

/// A single contiguous region of changes in the buffer.
///
/// Represents one "hunk" from a git diff - a region where the buffer content
/// differs from the HEAD content. Each hunk tracks its location in both the
/// buffer and the base text.
///
/// # Position Tracking
///
/// Uses [`Anchor`] for buffer positions, which automatically adjust as the buffer
/// is edited. The `diff_base_byte_range` points into the `base_text` stored
/// in [`BufferDiff`].
///
/// # Example
///
/// ```ignore
/// // A hunk representing lines 10-15 being modified
/// DiffHunk {
///     buffer_range: anchor(10,0)..anchor(15,0),
///     diff_base_byte_range: 200..350,  // bytes in HEAD content
///     status: DiffHunkStatus::Modified,
/// }
/// ```
#[derive(Debug, Clone)]
pub struct DiffHunk {
    /// Range in the buffer where this hunk applies.
    ///
    /// Uses anchors that track position through edits. Convert to points
    /// for display using `anchor.to_point(&buffer_snapshot)`.
    pub buffer_range: Range<Anchor>,

    /// Byte range in the base text (HEAD content) that corresponds to this hunk.
    ///
    /// For added hunks, this may be empty. For deleted/modified hunks,
    /// this range indexes into `BufferDiff.base_text`.
    pub diff_base_byte_range: Range<usize>,

    /// Type of change this hunk represents
    pub status: DiffHunkStatus,
}

/// Container for all diff hunks in a buffer, plus the base text they compare against.
///
/// This is the main data structure for git diff information. It stores both the hunks
/// and the HEAD content, allowing the UI to show deleted lines when a hunk is expanded.
///
/// # Lifecycle
///
/// 1. Created via [`new`](Self::new) when a file is opened
/// 2. Stored in [`BufferItem`](crate::BufferItem)
/// 3. Updated when buffer is saved or on-demand
/// 4. Queried during rendering to show gutter indicators
///
/// # Example
///
/// ```ignore
/// let diff = BufferDiff::new(buffer_id, head_content, &buffer_snapshot)?;
///
/// // Show hunks in gutter
/// for hunk in &diff.hunks {
///     let row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
///     paint_gutter_indicator(row, hunk.status);
/// }
///
/// // Show deleted content when hunk is expanded
/// let deleted_text = diff.base_text_for_hunk(0);
/// ```
#[derive(Clone, Debug)]
pub struct BufferDiff {
    /// Buffer this diff applies to
    pub buffer_id: BufferId,

    /// Content from git HEAD.
    ///
    /// This is the "old" text that the buffer is compared against. Byte ranges
    /// in [`DiffHunk::diff_base_byte_range`] index into this string.
    pub base_text: String,

    /// All hunks in this buffer, in order by position.
    ///
    /// Hunks do not overlap and are sorted by buffer position.
    pub hunks: Vec<DiffHunk>,
}

impl BufferDiff {
    /// Compute diff between buffer content and git HEAD.
    ///
    /// Uses libgit2 to compute the diff, then converts git's patch format
    /// into our hunk representation with anchors.
    ///
    /// # Arguments
    ///
    /// * `buffer_id` - ID of the buffer being diffed
    /// * `base_text` - Content from git HEAD
    /// * `buffer_snapshot` - Current buffer state
    ///
    /// # Returns
    ///
    /// [`BufferDiff`] with all hunks, or error if diff computation fails
    ///
    /// # Errors
    ///
    /// Returns error if libgit2 diff fails. This shouldn't happen with valid UTF-8 text.
    pub fn new(
        buffer_id: BufferId,
        base_text: String,
        buffer_snapshot: &BufferSnapshot,
    ) -> Result<Self, DiffError> {
        let hunks = compute_diff(&base_text, buffer_snapshot)?;

        Ok(Self {
            buffer_id,
            base_text,
            hunks,
        })
    }

    /// Get the base text content for a specific hunk.
    ///
    /// Extracts the portion of `base_text` corresponding to this hunk's
    /// `diff_base_byte_range`. Used when expanding a hunk to show deleted content.
    ///
    /// # Arguments
    ///
    /// * `hunk_index` - Index into `hunks` vec
    ///
    /// # Returns
    ///
    /// String slice of the base text, or empty string if index is invalid
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Show what was deleted
    /// let deleted_lines = diff.base_text_for_hunk(hunk_index);
    /// for line in deleted_lines.lines() {
    ///     println!("- {}", line);
    /// }
    /// ```
    pub fn base_text_for_hunk(&self, hunk_index: usize) -> &str {
        self.hunks
            .get(hunk_index)
            .map(|hunk| &self.base_text[hunk.diff_base_byte_range.clone()])
            .unwrap_or("")
    }

    /// Find the hunk containing the given buffer row.
    ///
    /// # Arguments
    ///
    /// * `row` - Buffer row to search for
    /// * `buffer_snapshot` - Buffer snapshot to convert anchors to points
    ///
    /// # Returns
    ///
    /// Index of the hunk containing this row, if any
    pub fn hunk_for_row(&self, row: u32, buffer_snapshot: &BufferSnapshot) -> Option<usize> {
        self.hunks.iter().position(|hunk| {
            let start_row = hunk.buffer_range.start.to_point(buffer_snapshot).row;
            let end_row = hunk.buffer_range.end.to_point(buffer_snapshot).row;
            row >= start_row && row <= end_row
        })
    }
}

/// Compute diff hunks between base text and buffer content.
///
/// Core diff computation function. Uses libgit2's diff engine to compare two text
/// strings and produces a vec of hunks with buffer anchors.
///
/// # How It Works
///
/// 1. Create git2 patch from old text (base) and new text (buffer)
/// 2. Iterate through hunks in the patch
/// 3. Convert line numbers to byte offsets
/// 4. Create anchors in the buffer
/// 5. Classify as Added/Modified/Deleted based on line counts
///
/// # Arguments
///
/// * `base_text` - Old text (from git HEAD)
/// * `buffer_snapshot` - Current buffer state (new text)
///
/// # Returns
///
/// Vector of hunks sorted by buffer position
///
/// # Errors
///
/// Returns error if git2 diff fails. This is rare with valid UTF-8.
fn compute_diff(
    base_text: &str,
    buffer_snapshot: &BufferSnapshot,
) -> Result<Vec<DiffHunk>, DiffError> {
    let buffer_text = buffer_snapshot.text();

    let mut diff_options = git2::DiffOptions::new();
    diff_options.context_lines(0); // No context, just changed lines
    diff_options.ignore_whitespace(false);

    let patch = git2::Patch::from_buffers(
        base_text.as_bytes(),
        None,
        buffer_text.as_bytes(),
        None,
        Some(&mut diff_options),
    )
    .map_err(|e| DiffError::PatchFailed(e.message().to_string()))?;

    let mut hunks = Vec::new();

    for hunk_idx in 0..patch.num_hunks() {
        let (hunk, _lines) = patch
            .hunk(hunk_idx)
            .map_err(|e| DiffError::HunkParseFailed(e.message().to_string()))?;

        // Extract hunk info
        let old_start = hunk.old_start(); // 1-indexed
        let old_lines = hunk.old_lines();
        let new_start = hunk.new_start(); // 1-indexed
        let new_lines = hunk.new_lines();

        // Determine status
        let status = if old_lines == 0 {
            DiffHunkStatus::Added
        } else if new_lines == 0 {
            DiffHunkStatus::Deleted
        } else {
            DiffHunkStatus::Modified
        };

        // Convert to 0-indexed buffer positions
        let buffer_start_row = new_start.saturating_sub(1);
        let buffer_end_row = buffer_start_row + new_lines;

        // Create anchors
        let buffer_range = if new_lines > 0 {
            let start_point = text::Point::new(buffer_start_row, 0);
            let end_point = text::Point::new(buffer_end_row, 0);
            buffer_snapshot.anchor_before(start_point)..buffer_snapshot.anchor_after(end_point)
        } else {
            // Deleted hunk - point to the line where deletion happened
            let point = text::Point::new(buffer_start_row, 0);
            let anchor = buffer_snapshot.anchor_at(point, Bias::Left);
            anchor..anchor
        };

        // Compute byte range in base text
        let base_start_row = old_start.saturating_sub(1);
        let base_end_row = base_start_row + old_lines;

        let base_start_offset = line_offset_to_byte_offset(base_text, base_start_row as usize);
        let base_end_offset = line_offset_to_byte_offset(base_text, base_end_row as usize);

        hunks.push(DiffHunk {
            buffer_range,
            diff_base_byte_range: base_start_offset..base_end_offset,
            status,
        });
    }

    Ok(hunks)
}

/// Count hunks in a diff without creating full BufferDiff.
///
/// Fast utility for counting the number of change hunks between two texts.
/// Used by diff review to compute total hunk counts across all files.
///
/// # Arguments
///
/// * `base_text` - Old text (from git)
/// * `modified_text` - New text (from working tree or index)
///
/// # Returns
///
/// Number of hunks in the diff, or 0 if diff computation fails
///
/// # Example
///
/// ```ignore
/// let head_content = repo.head_content(&path)?;
/// let working_content = std::fs::read_to_string(&path)?;
/// let hunk_count = count_hunks(&head_content, &working_content);
/// ```
pub fn count_hunks(base_text: &str, modified_text: &str) -> usize {
    let mut diff_options = git2::DiffOptions::new();
    diff_options.context_lines(0);
    diff_options.ignore_whitespace(false);

    match git2::Patch::from_buffers(
        base_text.as_bytes(),
        None,
        modified_text.as_bytes(),
        None,
        Some(&mut diff_options),
    ) {
        Ok(patch) => patch.num_hunks(),
        Err(_) => 0,
    }
}

/// Convert a line number to a byte offset in text.
///
/// Helper function for diff computation. Handles line counting and byte indexing.
///
/// # Arguments
///
/// * `text` - Text to index into
/// * `line` - Line number (0-indexed)
///
/// # Returns
///
/// Byte offset of the start of the line, or text length if line is past end
fn line_offset_to_byte_offset(text: &str, line: usize) -> usize {
    text.lines()
        .take(line)
        .map(|l| l.len() + 1) // +1 for newline
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use text::Buffer;

    fn create_buffer(text: &str) -> Buffer {
        use std::num::NonZeroU64;
        Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text)
    }

    #[test]
    fn compute_diff_added_lines() {
        let base_text = "line 1\nline 2\n";
        let buffer = create_buffer("line 1\nline 2\nline 3\n");
        let snapshot = buffer.snapshot();

        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Diff computation failed");

        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(diff.hunks[0].status, DiffHunkStatus::Added);
    }

    #[test]
    fn compute_diff_modified_lines() {
        let base_text = "line 1\nline 2\nline 3\n";
        let buffer = create_buffer("line 1\nmodified\nline 3\n");
        let snapshot = buffer.snapshot();

        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Diff computation failed");

        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(diff.hunks[0].status, DiffHunkStatus::Modified);
    }

    #[test]
    fn compute_diff_deleted_lines() {
        let base_text = "line 1\nline 2\nline 3\n";
        let buffer = create_buffer("line 1\nline 3\n");
        let snapshot = buffer.snapshot();

        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Diff computation failed");

        assert_eq!(diff.hunks.len(), 1);
        assert_eq!(diff.hunks[0].status, DiffHunkStatus::Deleted);
    }

    #[test]
    fn base_text_for_hunk() {
        let base_text = "line 1\nline 2\nline 3\n";
        let buffer = create_buffer("line 1\nmodified\nline 3\n");
        let snapshot = buffer.snapshot();

        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Diff computation failed");

        let base_content = diff.base_text_for_hunk(0);
        assert_eq!(base_content, "line 2\n");
    }

    #[test]
    fn line_offset_to_byte_offset_first_line() {
        let text = "line 1\nline 2\nline 3\n";
        assert_eq!(line_offset_to_byte_offset(text, 0), 0);
    }

    #[test]
    fn line_offset_to_byte_offset_second_line() {
        let text = "line 1\nline 2\nline 3\n";
        assert_eq!(line_offset_to_byte_offset(text, 1), 7); // "line 1\n" = 7 bytes
    }

    #[test]
    fn line_offset_to_byte_offset_past_end() {
        let text = "line 1\nline 2\n";
        assert_eq!(line_offset_to_byte_offset(text, 10), 14); // End of text
    }
}
