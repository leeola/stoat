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
//! use crate::git::diff::BufferDiff;
//! use crate::git::repository::Repository;
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
//! - [`git_repository`](crate::git::repository) - Provides access to git HEAD content
//! - [`BufferItem`](crate::BufferItem) - Stores computed diffs for display

use std::ops::Range;
use sum_tree::Bias;
use text::{Anchor, BufferId, BufferSnapshot, ToPoint};
use thiserror::Error;

/// Origin of a line within a diff hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HunkLineOrigin {
    Context,
    Addition,
    Deletion,
}

/// A single line extracted from a diff hunk via `git2::Patch::line_in_hunk()`.
#[derive(Debug, Clone)]
pub struct HunkLine {
    pub origin: HunkLineOrigin,
    pub content: String,
    pub old_lineno: Option<u32>,
    pub new_lineno: Option<u32>,
}

/// All lines in a single hunk, extracted on-demand for line-level selection.
#[derive(Debug, Clone)]
pub struct HunkLines {
    pub old_start: u32,
    pub old_lines: u32,
    pub new_start: u32,
    pub new_lines: u32,
    pub lines: Vec<HunkLine>,
}

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

    /// 1-indexed start line in HEAD, from libgit2.
    pub old_start: u32,

    /// Number of lines in HEAD covered by this hunk, from libgit2.
    pub old_lines: u32,
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
            old_start,
            old_lines,
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

/// Compute which buffer rows are staged by comparing two diffs that share the
/// same buffer snapshot.
///
/// `display_diff` is the working-vs-HEAD diff shown in the editor. `wi_diff`
/// is the working-vs-index diff. Both use anchors into the same working-tree
/// buffer, so their buffer coordinates are directly comparable.
///
/// A row within a display hunk is staged when it does NOT fall inside any
/// `wi_diff` hunk (working matches index at that row, so the HEAD difference
/// comes entirely from the index). Pure deletions (start == end) are skipped
/// since they have no visible buffer rows.
///
/// Rows are checked individually so that partially-staged hunks produce correct
/// per-row ranges.
pub fn compute_staged_buffer_rows(
    display_diff: &BufferDiff,
    wi_diff: Option<&BufferDiff>,
    buffer_snapshot: &BufferSnapshot,
) -> Vec<Range<u32>> {
    let Some(wi) = wi_diff else {
        return Vec::new();
    };

    let wi_ranges: Vec<(u32, u32)> = wi
        .hunks
        .iter()
        .map(|h| {
            let s = h.buffer_range.start.to_point(buffer_snapshot).row;
            let e = h.buffer_range.end.to_point(buffer_snapshot).row;
            (s, e)
        })
        .collect();

    let mut staged = Vec::new();
    for hunk in &display_diff.hunks {
        let start = hunk.buffer_range.start.to_point(buffer_snapshot).row;
        let end = hunk.buffer_range.end.to_point(buffer_snapshot).row;
        if start == end {
            continue;
        }
        let mut range_start: Option<u32> = None;
        for row in start..end {
            let in_wi = wi_ranges.iter().any(|&(ws, we)| row >= ws && row < we);
            if in_wi {
                if let Some(rs) = range_start.take() {
                    staged.push(rs..row);
                }
            } else if range_start.is_none() {
                range_start = Some(row);
            }
        }
        if let Some(rs) = range_start {
            staged.push(rs..end);
        }
    }
    staged
}

/// Compute which display_diff hunk indices are staged.
///
/// Same overlap logic as [`compute_staged_buffer_rows`] but returns hunk indices
/// instead of buffer row ranges, and includes pure deletions (`start == end`).
///
/// A display_diff hunk is staged when it has no overlapping hunk in `wi_diff`
/// (working tree matches index in that region).
pub fn compute_staged_hunk_indices(
    display_diff: &BufferDiff,
    wi_diff: Option<&BufferDiff>,
    buffer_snapshot: &BufferSnapshot,
) -> Vec<usize> {
    let Some(wi) = wi_diff else {
        return Vec::new();
    };

    let wi_ranges: Vec<(u32, u32)> = wi
        .hunks
        .iter()
        .map(|h| {
            let s = h.buffer_range.start.to_point(buffer_snapshot).row;
            let e = h.buffer_range.end.to_point(buffer_snapshot).row;
            (s, e)
        })
        .collect();

    let mut staged = Vec::new();
    for (idx, hunk) in display_diff.hunks.iter().enumerate() {
        let start = hunk.buffer_range.start.to_point(buffer_snapshot).row;
        let end = hunk.buffer_range.end.to_point(buffer_snapshot).row;
        let overlaps = if start == end {
            // Pure deletion at row R: overlaps if wi has a pure deletion at same
            // row, or a non-pure hunk covering that row
            wi_ranges.iter().any(|&(ws, we)| {
                if ws == we {
                    ws == start
                } else {
                    ws <= start && we > start
                }
            })
        } else {
            wi_ranges.iter().any(|&(ws, we)| ws < end && we > start)
        };
        if !overlaps {
            staged.push(idx);
        }
    }
    staged
}

/// Extract individual lines from a specific hunk, for line-level selection.
///
/// Recomputes the git2 patch from the two texts and calls
/// `patch.line_in_hunk()` to extract each line with its origin.
pub fn extract_hunk_lines(
    base_text: &str,
    buffer_text: &str,
    hunk_index: usize,
) -> Result<HunkLines, DiffError> {
    let mut diff_options = git2::DiffOptions::new();
    diff_options.context_lines(0);
    diff_options.ignore_whitespace(false);

    let patch = git2::Patch::from_buffers(
        base_text.as_bytes(),
        None,
        buffer_text.as_bytes(),
        None,
        Some(&mut diff_options),
    )
    .map_err(|e| DiffError::PatchFailed(e.message().to_string()))?;

    let (hunk_header, num_lines) = patch
        .hunk(hunk_index)
        .map_err(|e| DiffError::HunkParseFailed(e.message().to_string()))?;

    let old_start = hunk_header.old_start();
    let old_lines = hunk_header.old_lines();
    let new_start = hunk_header.new_start();
    let new_lines = hunk_header.new_lines();

    let mut lines = Vec::with_capacity(num_lines);
    for line_idx in 0..num_lines {
        let line = patch
            .line_in_hunk(hunk_index, line_idx)
            .map_err(|e| DiffError::HunkParseFailed(e.message().to_string()))?;

        let origin = match line.origin() {
            '+' => HunkLineOrigin::Addition,
            '-' => HunkLineOrigin::Deletion,
            _ => HunkLineOrigin::Context,
        };

        let content = String::from_utf8_lossy(line.content()).to_string();

        lines.push(HunkLine {
            origin,
            content,
            old_lineno: line.old_lineno(),
            new_lineno: line.new_lineno(),
        });
    }

    Ok(HunkLines {
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines,
    })
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

    #[test]
    fn extract_hunk_lines_modified() {
        let base = "line 1\nline 2\nline 3\n";
        let modified = "line 1\nchanged\nline 3\n";
        let hunk = extract_hunk_lines(base, modified, 0).unwrap();

        assert_eq!(hunk.old_start, 2);
        assert_eq!(hunk.old_lines, 1);
        assert_eq!(hunk.new_start, 2);
        assert_eq!(hunk.new_lines, 1);
        assert_eq!(hunk.lines.len(), 2);
        assert_eq!(hunk.lines[0].origin, HunkLineOrigin::Deletion);
        assert_eq!(hunk.lines[0].content, "line 2\n");
        assert_eq!(hunk.lines[1].origin, HunkLineOrigin::Addition);
        assert_eq!(hunk.lines[1].content, "changed\n");
    }

    #[test]
    fn extract_hunk_lines_added() {
        let base = "line 1\nline 2\n";
        let modified = "line 1\nline 2\nline 3\n";
        let hunk = extract_hunk_lines(base, modified, 0).unwrap();

        assert_eq!(hunk.lines.len(), 1);
        assert_eq!(hunk.lines[0].origin, HunkLineOrigin::Addition);
        assert_eq!(hunk.lines[0].content, "line 3\n");
    }

    #[test]
    fn extract_hunk_lines_deleted() {
        let base = "line 1\nline 2\nline 3\n";
        let modified = "line 1\nline 3\n";
        let hunk = extract_hunk_lines(base, modified, 0).unwrap();

        assert_eq!(hunk.lines.len(), 1);
        assert_eq!(hunk.lines[0].origin, HunkLineOrigin::Deletion);
        assert_eq!(hunk.lines[0].content, "line 2\n");
    }

    #[test]
    fn extract_hunk_lines_multi_line() {
        let base = "a\nb\nc\nd\n";
        let modified = "a\nx\ny\nd\n";
        let hunk = extract_hunk_lines(base, modified, 0).unwrap();

        assert_eq!(hunk.lines.len(), 4);
        assert_eq!(hunk.lines[0].origin, HunkLineOrigin::Deletion);
        assert_eq!(hunk.lines[1].origin, HunkLineOrigin::Deletion);
        assert_eq!(hunk.lines[2].origin, HunkLineOrigin::Addition);
        assert_eq!(hunk.lines[3].origin, HunkLineOrigin::Addition);
    }

    fn make_diff(base: &str, working: &str) -> (BufferDiff, Buffer) {
        let buf = create_buffer(working);
        let snap = buf.snapshot();
        let diff = BufferDiff::new(snap.remote_id(), base.to_string(), &snap).unwrap();
        (diff, buf)
    }

    #[test]
    fn staged_buffer_rows_fully_staged() {
        let head = "line 1\nline 2\n";
        let working = "line 1\nline 2\nline 3\n";
        let index = working;
        let (display_diff, buf) = make_diff(head, working);
        let snap = buf.snapshot();
        let (wi_diff, _) = make_diff(index, working);
        let rows = compute_staged_buffer_rows(&display_diff, Some(&wi_diff), &snap);
        assert_eq!(rows, vec![2..3]);
    }

    #[test]
    fn staged_buffer_rows_nothing_staged() {
        let head = "line 1\nline 2\n";
        let working = "line 1\nline 2\nline 3\n";
        let index = head;
        let (display_diff, buf) = make_diff(head, working);
        let snap = buf.snapshot();
        let (wi_diff, _) = make_diff(index, working);
        let rows = compute_staged_buffer_rows(&display_diff, Some(&wi_diff), &snap);
        assert!(rows.is_empty());
    }

    #[test]
    fn staged_buffer_rows_no_index() {
        let head = "line 1\nline 2\n";
        let working = "line 1\nline 2\nline 3\n";
        let (display_diff, buf) = make_diff(head, working);
        let snap = buf.snapshot();
        let rows = compute_staged_buffer_rows(&display_diff, None, &snap);
        assert!(rows.is_empty());
    }

    #[test]
    fn staged_buffer_rows_pure_deletion_skipped() {
        let head = "line 1\nline 2\nline 3\n";
        let working = "line 1\nline 3\n";
        let index = working;
        let (display_diff, buf) = make_diff(head, working);
        let snap = buf.snapshot();
        let (wi_diff, _) = make_diff(index, working);
        let rows = compute_staged_buffer_rows(&display_diff, Some(&wi_diff), &snap);
        assert!(rows.is_empty());
    }

    #[test]
    fn staged_buffer_rows_partial_hunk() {
        let head = "A\nB\nC\n";
        let working = "A\nB\nC\nD\nE\n";
        // D is staged (index has it), E is not
        let index = "A\nB\nC\nD\n";
        let (display_diff, buf) = make_diff(head, working);
        let snap = buf.snapshot();
        let (wi_diff, _) = make_diff(index, working);
        let rows = compute_staged_buffer_rows(&display_diff, Some(&wi_diff), &snap);
        assert_eq!(rows, vec![3..4]);
    }

    #[test]
    fn staged_hunk_indices_fully_staged_addition() {
        let head = "line 1\nline 2\n";
        let working = "line 1\nline 2\nline 3\n";
        let index = working;
        let (display_diff, buf) = make_diff(head, working);
        let snap = buf.snapshot();
        let (wi_diff, _) = make_diff(index, working);
        let indices = compute_staged_hunk_indices(&display_diff, Some(&wi_diff), &snap);
        assert_eq!(indices, vec![0]);
    }

    #[test]
    fn staged_hunk_indices_nothing_staged() {
        let head = "line 1\nline 2\n";
        let working = "line 1\nline 2\nline 3\n";
        let index = head;
        let (display_diff, buf) = make_diff(head, working);
        let snap = buf.snapshot();
        let (wi_diff, _) = make_diff(index, working);
        let indices = compute_staged_hunk_indices(&display_diff, Some(&wi_diff), &snap);
        assert!(indices.is_empty());
    }

    #[test]
    fn staged_hunk_indices_no_index() {
        let head = "line 1\nline 2\n";
        let working = "line 1\nline 2\nline 3\n";
        let (display_diff, buf) = make_diff(head, working);
        let snap = buf.snapshot();
        let indices = compute_staged_hunk_indices(&display_diff, None, &snap);
        assert!(indices.is_empty());
    }

    #[test]
    fn staged_hunk_indices_pure_deletion_staged() {
        let head = "line 1\nline 2\nline 3\n";
        let working = "line 1\nline 3\n";
        let index = working;
        let (display_diff, buf) = make_diff(head, working);
        let snap = buf.snapshot();
        let (wi_diff, _) = make_diff(index, working);
        let indices = compute_staged_hunk_indices(&display_diff, Some(&wi_diff), &snap);
        assert_eq!(indices, vec![0]);
    }

    #[test]
    fn staged_hunk_indices_pure_deletion_unstaged() {
        let head = "line 1\nline 2\nline 3\n";
        let working = "line 1\nline 3\n";
        let index = head;
        let (display_diff, buf) = make_diff(head, working);
        let snap = buf.snapshot();
        let (wi_diff, _) = make_diff(index, working);
        let indices = compute_staged_hunk_indices(&display_diff, Some(&wi_diff), &snap);
        assert!(indices.is_empty());
    }
}
