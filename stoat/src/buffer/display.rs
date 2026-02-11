//! Display buffer with phantom rows for git diffs.
//!
//! Provides a view over a text buffer that includes phantom rows for deleted content
//! from git diffs. This follows Zed's approach of inserting deleted lines directly
//! into the text iteration, making all diff hunks visible with appropriate styling.
//!
//! # Architecture
//!
//! Unlike the actual [`Buffer`], which only contains the current file content, a
//! [`DisplayBuffer`] includes both:
//! - Real buffer rows (the actual file content)
//! - Phantom rows (deleted content from git HEAD)
//!
//! Each row has metadata indicating whether it's a phantom row and its diff status.
//!
//! # Display Rows vs Buffer Rows
//!
//! Display rows include phantom rows, so the numbering differs from buffer rows:
//!
//! ```ignore
//! Buffer:                  Display:
//! Row 0: "unchanged"       Row 0: "unchanged"
//! Row 1: "new content"     Row 1: "- old content" (phantom, from HEAD)
//!                          Row 2: "+ new content" (buffer row 1, marked Added)
//! Row 2: "unchanged"       Row 3: "unchanged"     (buffer row 2)
//! ```
//!
//! The [`DisplayBuffer`] provides mapping between buffer rows and display rows.
//!
//! # Usage
//!
//! ```ignore
//! let display_buffer = buffer_item.read(cx).display_buffer(cx);
//!
//! // Iterate over all rows (real + phantom)
//! for row_info in display_buffer.rows() {
//!     match row_info.diff_status {
//!         Some(DiffHunkStatus::Deleted) => paint_deleted_row(row_info),
//!         Some(DiffHunkStatus::Added) => paint_added_row(row_info),
//!         _ => paint_normal_row(row_info),
//!     }
//! }
//!
//! // Convert between row types
//! let display_row = display_buffer.buffer_row_to_display(5);
//! let buffer_row = display_buffer.display_row_to_buffer(display_row);
//! ```
//!
//! # Related
//!
//! - [`BufferItem`](crate::BufferItem) - Stores the diff and provides access to display buffer
//! - [`BufferDiff`](crate::git::diff::BufferDiff) - Contains the hunks used to build phantom rows

use crate::git::diff::{BufferDiff, DiffHunkStatus};
use std::ops::Range;
use text::{BufferSnapshot, ToPoint};

/// A display row index (includes phantom rows).
///
/// Display rows are numbered sequentially including both real buffer rows and
/// phantom rows inserted for deleted content. Use [`DisplayBuffer`] methods
/// to convert between display rows and buffer rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DisplayRow(pub u32);

impl DisplayRow {
    /// Create a new display row
    pub fn new(row: u32) -> Self {
        Self(row)
    }

    /// Get the raw row number
    pub fn row(&self) -> u32 {
        self.0
    }
}

/// Information about a single display row.
///
/// Each row in the display buffer has metadata indicating whether it's a real
/// buffer row or a phantom row, and its diff status if part of a git diff hunk.
#[derive(Debug, Clone)]
pub struct RowInfo {
    /// Display row index (sequential across all rows)
    pub display_row: DisplayRow,

    /// Buffer row index, or [`None`] for phantom rows
    ///
    /// Phantom rows (deleted content from git HEAD) have no corresponding buffer row.
    pub buffer_row: Option<u32>,

    /// Diff status if this row is part of a hunk
    ///
    /// - [`Some(DiffHunkStatus::Added)`] - Row from buffer, marked as added
    /// - [`Some(DiffHunkStatus::Deleted)`] - Phantom row, marked as deleted
    /// - [`Some(DiffHunkStatus::Modified)`] - Row from buffer, part of modification
    /// - [`None`] - Normal row, not part of any hunk
    pub diff_status: Option<DiffHunkStatus>,

    /// Text content for this row
    ///
    /// For phantom rows, this is from the git base text. For buffer rows, this is
    /// from the actual buffer content.
    pub content: String,

    /// Byte ranges within the line that were modified (for Modified rows only).
    ///
    /// Indicates which parts of the line text changed from the base version.
    /// Used to highlight specific changed words/characters more strongly.
    /// Empty for non-Modified rows.
    pub modified_ranges: Vec<Range<usize>>,

    /// Whether this row's change is staged in the git index.
    /// Used to render staged hunks with desaturated colors.
    pub is_staged: bool,
}

/// A view over a buffer that includes phantom rows for git diffs.
///
/// Wraps a [`BufferSnapshot`] and optional [`BufferDiff`] to provide iteration
/// over rows including phantom deleted rows. Builds and caches the complete list
/// of display rows for efficient access.
///
/// # Row Ordering
///
/// Rows are ordered as follows:
/// 1. Normal buffer rows appear at their buffer position
/// 2. Phantom deleted rows are inserted before the buffer position where the deletion occurred
/// 3. For Modified hunks, deleted rows appear before the added rows
///
/// # Caching
///
/// The row list is built once when the [`DisplayBuffer`] is created and cached.
/// If the buffer or diff changes, a new [`DisplayBuffer`] must be created.
pub struct DisplayBuffer {
    /// Snapshot of the buffer content
    buffer_snapshot: BufferSnapshot,

    /// Git diff information, if available
    diff: Option<BufferDiff>,

    /// All display rows (real + phantom), built during construction
    rows: Vec<RowInfo>,

    /// Mapping from buffer row to display row for fast lookup
    buffer_to_display: Vec<DisplayRow>,
}

impl DisplayBuffer {
    /// Create a new display buffer.
    ///
    /// Builds the complete list of display rows by iterating through the buffer
    /// and inserting phantom rows for deleted content from the diff.
    ///
    /// # Arguments
    ///
    /// * `buffer_snapshot` - Snapshot of the buffer content
    /// * `diff` - Optional git diff information
    /// * `show_phantom_rows` - Whether to show phantom deleted rows (false in normal mode, true in
    ///   review mode)
    ///
    /// # Returns
    ///
    /// A new [`DisplayBuffer`] with all rows built and cached
    pub fn new(
        buffer_snapshot: BufferSnapshot,
        diff: Option<BufferDiff>,
        show_phantom_rows: bool,
        staged_rows: Option<&[Range<u32>]>,
    ) -> Self {
        let row_is_staged = |buffer_row: u32| -> bool {
            staged_rows
                .map(|ranges| ranges.iter().any(|r| r.contains(&buffer_row)))
                .unwrap_or(false)
        };

        let max_buffer_row = buffer_snapshot.max_point().row;
        let mut rows = Vec::new();
        let mut buffer_to_display = vec![DisplayRow(0); (max_buffer_row + 1) as usize];
        let mut display_row = 0u32;

        if let Some(ref diff) = diff {
            // Build rows by walking through buffer and inserting phantom rows from hunks
            let mut buffer_row = 0u32;
            let mut hunk_idx = 0;

            while buffer_row <= max_buffer_row {
                // Check if current buffer row is the start of a hunk
                let hunk_at_row = diff.hunks.get(hunk_idx).and_then(|hunk| {
                    let hunk_start_row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
                    if hunk_start_row == buffer_row {
                        Some((hunk_idx, hunk))
                    } else {
                        None
                    }
                });

                if let Some((idx, hunk)) = hunk_at_row {
                    // Hunk starts at this buffer row
                    let hunk_start_row = hunk.buffer_range.start.to_point(&buffer_snapshot).row;
                    let hunk_end_row = hunk.buffer_range.end.to_point(&buffer_snapshot).row;
                    let has_deleted_content = !hunk.diff_base_byte_range.is_empty();
                    let is_pure_deletion = hunk_start_row == hunk_end_row;

                    if is_pure_deletion {
                        // For pure deletions: first add the current buffer row (where deletion
                        // occurred) then insert phantom deleted rows after
                        // it
                        buffer_to_display[buffer_row as usize] = DisplayRow(display_row);

                        let start = text::Point::new(buffer_row, 0);
                        let end = if buffer_row < max_buffer_row {
                            text::Point::new(buffer_row + 1, 0)
                        } else {
                            buffer_snapshot.max_point()
                        };

                        let mut content: String =
                            buffer_snapshot.text_for_range(start..end).collect();
                        if content.ends_with('\n') {
                            content.pop();
                        }

                        rows.push(RowInfo {
                            display_row: DisplayRow(display_row),
                            buffer_row: Some(buffer_row),
                            diff_status: None,
                            content,
                            modified_ranges: Vec::new(),
                            is_staged: false,
                        });

                        display_row += 1;
                        buffer_row += 1;

                        // Now insert phantom rows for deleted content (only in review mode)
                        if show_phantom_rows && has_deleted_content {
                            let deleted_text = diff.base_text_for_hunk(idx);
                            for deleted_line in deleted_text.lines() {
                                rows.push(RowInfo {
                                    display_row: DisplayRow(display_row),
                                    buffer_row: None,
                                    diff_status: Some(DiffHunkStatus::Deleted),
                                    content: deleted_line.to_string(),
                                    modified_ranges: Vec::new(),
                                    is_staged: false,
                                });
                                display_row += 1;
                            }
                        }

                        hunk_idx += 1;
                    } else {
                        // For Modified and Added hunks: show phantom deleted rows in review mode
                        // Modified hunks also compute intra-line diff for word-level highlighting
                        let is_modified = matches!(hunk.status, DiffHunkStatus::Modified);

                        if show_phantom_rows && has_deleted_content {
                            let deleted_text = diff.base_text_for_hunk(idx);
                            for deleted_line in deleted_text.lines() {
                                rows.push(RowInfo {
                                    display_row: DisplayRow(display_row),
                                    buffer_row: None,
                                    diff_status: Some(DiffHunkStatus::Deleted),
                                    content: deleted_line.to_string(),
                                    modified_ranges: Vec::new(),
                                    is_staged: false,
                                });
                                display_row += 1;
                            }
                        }

                        // Add buffer rows for this hunk (marked as Added or Modified)
                        let base_lines: Vec<&str> = if is_modified && has_deleted_content {
                            diff.base_text_for_hunk(idx).lines().collect()
                        } else {
                            Vec::new()
                        };

                        for (row_offset, row) in (hunk_start_row..hunk_end_row).enumerate() {
                            if row > max_buffer_row {
                                break;
                            }

                            buffer_to_display[row as usize] = DisplayRow(display_row);

                            // Get content for this buffer row
                            let start = text::Point::new(row, 0);
                            let end = if row < max_buffer_row {
                                text::Point::new(row + 1, 0)
                            } else {
                                buffer_snapshot.max_point()
                            };

                            let mut content: String =
                                buffer_snapshot.text_for_range(start..end).collect();
                            if content.ends_with('\n') {
                                content.pop();
                            }

                            // Compute modified_ranges for Modified rows
                            let modified_ranges = if is_modified {
                                base_lines
                                    .get(row_offset)
                                    .map(|base_line| compute_word_diff(base_line, &content))
                                    .unwrap_or_default()
                            } else {
                                Vec::new()
                            };

                            rows.push(RowInfo {
                                display_row: DisplayRow(display_row),
                                buffer_row: Some(row),
                                diff_status: Some(hunk.status),
                                content,
                                modified_ranges,
                                is_staged: row_is_staged(row),
                            });

                            display_row += 1;
                        }

                        // Move to next buffer row after hunk
                        buffer_row = hunk_end_row;
                        hunk_idx += 1;
                    }
                } else {
                    // Normal row, not part of a hunk
                    buffer_to_display[buffer_row as usize] = DisplayRow(display_row);

                    // Get content for this buffer row
                    let start = text::Point::new(buffer_row, 0);
                    let end = if buffer_row < max_buffer_row {
                        text::Point::new(buffer_row + 1, 0)
                    } else {
                        buffer_snapshot.max_point()
                    };

                    let mut content: String = buffer_snapshot.text_for_range(start..end).collect();
                    if content.ends_with('\n') {
                        content.pop();
                    }

                    rows.push(RowInfo {
                        display_row: DisplayRow(display_row),
                        buffer_row: Some(buffer_row),
                        diff_status: None,
                        content,
                        modified_ranges: Vec::new(),
                        is_staged: false,
                    });

                    display_row += 1;
                    buffer_row += 1;
                }
            }
        } else {
            // No diff, just build normal rows
            for buffer_row in 0..=max_buffer_row {
                buffer_to_display[buffer_row as usize] = DisplayRow(display_row);

                let start = text::Point::new(buffer_row, 0);
                let end = if buffer_row < max_buffer_row {
                    text::Point::new(buffer_row + 1, 0)
                } else {
                    buffer_snapshot.max_point()
                };

                let mut content: String = buffer_snapshot.text_for_range(start..end).collect();
                if content.ends_with('\n') {
                    content.pop();
                }

                rows.push(RowInfo {
                    display_row: DisplayRow(display_row),
                    buffer_row: Some(buffer_row),
                    diff_status: None,
                    content,
                    modified_ranges: Vec::new(),
                    is_staged: false,
                });

                display_row += 1;
            }
        }

        Self {
            buffer_snapshot,
            diff,
            rows,
            buffer_to_display,
        }
    }

    /// Get an iterator over all display rows.
    ///
    /// Yields rows in display order, including both real buffer rows and phantom rows.
    pub fn rows(&self) -> impl Iterator<Item = &RowInfo> + '_ {
        self.rows.iter()
    }

    /// Get a specific display row by index.
    ///
    /// # Arguments
    ///
    /// * `display_row` - The display row to fetch
    ///
    /// # Returns
    ///
    /// [`Some(&RowInfo)`] if the row exists, [`None`] otherwise
    pub fn row_at(&self, display_row: DisplayRow) -> Option<&RowInfo> {
        self.rows.get(display_row.0 as usize)
    }

    /// Get a range of display rows.
    ///
    /// # Arguments
    ///
    /// * `range` - Range of display rows to fetch
    ///
    /// # Returns
    ///
    /// Iterator over the rows in the range
    pub fn rows_in_range(&self, range: Range<DisplayRow>) -> impl Iterator<Item = &RowInfo> + '_ {
        self.rows
            .iter()
            .skip(range.start.0 as usize)
            .take((range.end.0 - range.start.0) as usize)
    }

    /// Convert a buffer row to a display row.
    ///
    /// # Arguments
    ///
    /// * `buffer_row` - Buffer row index
    ///
    /// # Returns
    ///
    /// The corresponding display row index
    pub fn buffer_row_to_display(&self, buffer_row: u32) -> DisplayRow {
        self.buffer_to_display
            .get(buffer_row as usize)
            .copied()
            .unwrap_or(DisplayRow(buffer_row))
    }

    /// Convert a display row to a buffer row.
    ///
    /// # Arguments
    ///
    /// * `display_row` - Display row index
    ///
    /// # Returns
    ///
    /// The corresponding buffer row, or [`None`] if the display row is a phantom row
    pub fn display_row_to_buffer(&self, display_row: DisplayRow) -> Option<u32> {
        self.row_at(display_row)?.buffer_row
    }

    /// Get the maximum display row index.
    ///
    /// # Returns
    ///
    /// The highest display row index, or 0 if the buffer is empty
    pub fn max_display_row(&self) -> DisplayRow {
        DisplayRow((self.rows.len().saturating_sub(1)) as u32)
    }

    /// Get the total number of display rows.
    ///
    /// # Returns
    ///
    /// Count of all rows (real + phantom)
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Get the underlying buffer snapshot.
    pub fn buffer_snapshot(&self) -> &BufferSnapshot {
        &self.buffer_snapshot
    }

    /// Get the diff information, if any.
    pub fn diff(&self) -> Option<&BufferDiff> {
        self.diff.as_ref()
    }
}

/// Compute word-level diff between two lines.
///
/// Compares two strings word-by-word and returns the byte ranges of words
/// that differ in the new line. Used for intra-line highlighting of modified rows.
///
/// # Arguments
///
/// * `base_line` - Original line from git HEAD
/// * `new_line` - Modified line from buffer
///
/// # Returns
///
/// Vector of byte ranges indicating modified words in `new_line`
///
/// # Algorithm
///
/// 1. Split both lines into words (on whitespace)
/// 2. Compare word sequences
/// 3. Mark words that differ or are added
/// 4. Convert word positions to byte ranges
fn compute_word_diff(base_line: &str, new_line: &str) -> Vec<Range<usize>> {
    let base_words: Vec<&str> = base_line.split_whitespace().collect();

    let mut modified_ranges = Vec::new();
    let mut byte_offset = 0;

    for (i, new_word) in new_line.split_whitespace().enumerate() {
        // Find the byte position of this word in new_line
        if let Some(word_start) = new_line[byte_offset..].find(new_word) {
            let word_start = byte_offset + word_start;
            let word_end = word_start + new_word.len();

            // Check if this word differs from the corresponding base word
            let differs = if i < base_words.len() {
                base_words[i] != new_word
            } else {
                // Word was added (beyond base line length)
                true
            };

            if differs {
                modified_ranges.push(word_start..word_end);
            }

            byte_offset = word_end;
        }
    }

    modified_ranges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::diff::BufferDiff;
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId};

    fn create_buffer(text: &str) -> Buffer {
        Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text)
    }

    #[test]
    fn display_buffer_without_diff() {
        let buffer = create_buffer("line 1\nline 2\nline 3");
        let snapshot = buffer.snapshot();

        let display_buffer = DisplayBuffer::new(snapshot, None, true, None);

        assert_eq!(display_buffer.row_count(), 3);

        let rows: Vec<_> = display_buffer.rows().collect();
        assert_eq!(rows.len(), 3);

        // All rows should be normal buffer rows
        for (i, row) in rows.iter().enumerate() {
            assert_eq!(row.buffer_row, Some(i as u32));
            assert_eq!(row.diff_status, None);
        }
    }

    #[test]
    fn display_buffer_with_added_hunk() {
        let buffer = create_buffer("line 1\nline 2\nnew line\nline 3");
        let snapshot = buffer.snapshot();

        // Base text without the added line
        let base_text = "line 1\nline 2\nline 3";
        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Failed to create diff");

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff), true, None);

        // Should have 4 rows (no phantom rows for Added hunks)
        assert_eq!(display_buffer.row_count(), 4);

        let rows: Vec<_> = display_buffer.rows().collect();

        // Find the added row
        let added_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.diff_status == Some(DiffHunkStatus::Added))
            .collect();

        assert!(!added_rows.is_empty(), "Should have at least one added row");
    }

    #[test]
    fn display_buffer_with_deleted_hunk() {
        let buffer = create_buffer("line 1\nline 3");
        let snapshot = buffer.snapshot();

        // Base text with the deleted line
        let base_text = "line 1\nline 2\nline 3";
        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Failed to create diff");

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff), true, None);

        // Should have 3 rows: 1 normal, 1 phantom deleted, 1 normal
        assert_eq!(display_buffer.row_count(), 3);

        let rows: Vec<_> = display_buffer.rows().collect();

        // Find the phantom deleted row
        let deleted_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.diff_status == Some(DiffHunkStatus::Deleted))
            .collect();

        assert_eq!(deleted_rows.len(), 1, "Should have one deleted phantom row");
        assert_eq!(
            deleted_rows[0].buffer_row, None,
            "Deleted row should be phantom"
        );
    }

    #[test]
    fn display_buffer_with_modified_hunk() {
        let buffer = create_buffer("line 1\nmodified\nline 3");
        let snapshot = buffer.snapshot();

        // Base text with original line
        let base_text = "line 1\nline 2\nline 3";
        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Failed to create diff");

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff), true, None);

        // Should have 4 rows: 1 normal, 1 phantom deleted ("line 2"), 1 modified ("modified"), 1
        // normal Modified hunks show both old content (phantom) and new content (with intra-line
        // highlighting)
        assert_eq!(display_buffer.row_count(), 4);

        let rows: Vec<_> = display_buffer.rows().collect();

        // Find modified rows
        let modified_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.diff_status == Some(DiffHunkStatus::Modified))
            .collect();

        assert_eq!(modified_rows.len(), 1, "Should have one modified row");

        // Verify that the modified row has computed modified_ranges
        assert!(
            !modified_rows[0].modified_ranges.is_empty(),
            "Modified row should have non-empty modified_ranges for intra-line diff"
        );
    }

    #[test]
    fn modified_line_shows_both_versions() {
        let buffer = create_buffer("line 1\nhello universe\nline 3");
        let snapshot = buffer.snapshot();

        // Base text with original mid-line content
        let base_text = "line 1\nhello world\nline 3";
        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Failed to create diff");

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff), true, None);

        // Should have 4 rows: 1 normal, 1 phantom deleted, 1 modified, 1 normal
        assert_eq!(
            display_buffer.row_count(),
            4,
            "Should have normal + phantom deleted + modified + normal rows"
        );

        let rows: Vec<_> = display_buffer.rows().collect();

        // Row 0 should be normal
        assert_eq!(rows[0].content, "line 1");
        assert_eq!(rows[0].diff_status, None);

        // Row 1 should be phantom deleted with old content
        assert_eq!(rows[1].buffer_row, None, "Row 1 should be phantom");
        assert_eq!(
            rows[1].diff_status,
            Some(DiffHunkStatus::Deleted),
            "Row 1 should be marked Deleted"
        );
        assert_eq!(
            rows[1].content, "hello world",
            "Phantom row should show old content"
        );

        // Row 2 should be modified with new content
        assert_eq!(rows[2].buffer_row, Some(1), "Row 2 should be buffer row 1");
        assert_eq!(
            rows[2].diff_status,
            Some(DiffHunkStatus::Modified),
            "Row 2 should be marked Modified"
        );
        assert_eq!(
            rows[2].content, "hello universe",
            "Modified row should show new content"
        );

        // Modified row should have word-level highlighting on "universe"
        assert!(
            !rows[2].modified_ranges.is_empty(),
            "Modified row should have non-empty modified_ranges for changed word"
        );

        // Row 3 should be normal
        assert_eq!(rows[3].content, "line 3");
        assert_eq!(rows[3].diff_status, None);
    }

    #[test]
    fn buffer_row_to_display_mapping() {
        let buffer = create_buffer("line 1\nline 3");
        let snapshot = buffer.snapshot();

        let base_text = "line 1\nline 2\nline 3";
        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Failed to create diff");

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff), true, None);

        // Debug: print all rows
        eprintln!("All rows:");
        for row in display_buffer.rows() {
            eprintln!(
                "  Display {} -> Buffer {:?}, status {:?}, content: {:?}",
                row.display_row.0, row.buffer_row, row.diff_status, row.content
            );
        }

        // Buffer row 0 -> Display row 0
        assert_eq!(display_buffer.buffer_row_to_display(0), DisplayRow(0));

        // Buffer row 1 should map to display row 2 (after phantom row)
        let display_row_1 = display_buffer.buffer_row_to_display(1);
        assert!(
            display_row_1.0 > 1,
            "Buffer row 1 should map after phantom row"
        );
    }

    #[test]
    fn display_row_to_buffer_mapping() {
        let buffer = create_buffer("line 1\nline 3");
        let snapshot = buffer.snapshot();

        let base_text = "line 1\nline 2\nline 3";
        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Failed to create diff");

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff), true, None);

        // Display row 0 -> Buffer row 0
        assert_eq!(display_buffer.display_row_to_buffer(DisplayRow(0)), Some(0));

        // Display row 1 (phantom) -> None
        assert_eq!(
            display_buffer.display_row_to_buffer(DisplayRow(1)),
            None,
            "Phantom row should not map to buffer row"
        );
    }
}
