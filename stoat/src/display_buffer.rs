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
//! - [`BufferDiff`](crate::git_diff::BufferDiff) - Contains the hunks used to build phantom rows

use crate::git_diff::{BufferDiff, DiffHunkStatus};
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
    ///
    /// # Returns
    ///
    /// A new [`DisplayBuffer`] with all rows built and cached
    pub fn new(buffer_snapshot: BufferSnapshot, diff: Option<BufferDiff>) -> Self {
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

                        let content: String = buffer_snapshot.text_for_range(start..end).collect();
                        let content = content.trim_end_matches('\n').to_string();

                        rows.push(RowInfo {
                            display_row: DisplayRow(display_row),
                            buffer_row: Some(buffer_row),
                            diff_status: None,
                            content,
                        });

                        display_row += 1;
                        buffer_row += 1;

                        // Now insert phantom rows for deleted content
                        if has_deleted_content {
                            let deleted_text = diff.base_text_for_hunk(idx);
                            for deleted_line in deleted_text.lines() {
                                rows.push(RowInfo {
                                    display_row: DisplayRow(display_row),
                                    buffer_row: None, // Phantom row
                                    diff_status: Some(DiffHunkStatus::Deleted),
                                    content: deleted_line.to_string(),
                                });
                                display_row += 1;
                            }
                        }

                        hunk_idx += 1;
                    } else {
                        // For Added/Modified hunks: insert phantom deleted rows first,
                        // then add buffer rows
                        if has_deleted_content {
                            let deleted_text = diff.base_text_for_hunk(idx);
                            for deleted_line in deleted_text.lines() {
                                rows.push(RowInfo {
                                    display_row: DisplayRow(display_row),
                                    buffer_row: None, // Phantom row
                                    diff_status: Some(DiffHunkStatus::Deleted),
                                    content: deleted_line.to_string(),
                                });
                                display_row += 1;
                            }
                        }

                        // Add buffer rows for this hunk (marked as Added or Modified)
                        for row in hunk_start_row..hunk_end_row {
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

                            let content: String =
                                buffer_snapshot.text_for_range(start..end).collect();
                            let content = content.trim_end_matches('\n').to_string();

                            rows.push(RowInfo {
                                display_row: DisplayRow(display_row),
                                buffer_row: Some(row),
                                diff_status: Some(hunk.status),
                                content,
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

                    let content: String = buffer_snapshot.text_for_range(start..end).collect();
                    let content = content.trim_end_matches('\n').to_string();

                    rows.push(RowInfo {
                        display_row: DisplayRow(display_row),
                        buffer_row: Some(buffer_row),
                        diff_status: None,
                        content,
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

                let content: String = buffer_snapshot.text_for_range(start..end).collect();
                let content = content.trim_end_matches('\n').to_string();

                rows.push(RowInfo {
                    display_row: DisplayRow(display_row),
                    buffer_row: Some(buffer_row),
                    diff_status: None,
                    content,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_diff::BufferDiff;
    use std::num::NonZeroU64;
    use text::{Buffer, BufferId};

    fn create_buffer(text: &str) -> Buffer {
        Buffer::new(0, BufferId::from(NonZeroU64::new(1).unwrap()), text)
    }

    #[test]
    fn display_buffer_without_diff() {
        let buffer = create_buffer("line 1\nline 2\nline 3");
        let snapshot = buffer.snapshot();

        let display_buffer = DisplayBuffer::new(snapshot, None);

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

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff));

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

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff));

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

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff));

        // Should have 4 rows: 1 normal, 1 phantom deleted, 1 modified, 1 normal
        assert_eq!(display_buffer.row_count(), 4);

        let rows: Vec<_> = display_buffer.rows().collect();

        // Find deleted and modified rows
        let deleted_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.diff_status == Some(DiffHunkStatus::Deleted))
            .collect();
        let modified_rows: Vec<_> = rows
            .iter()
            .filter(|r| r.diff_status == Some(DiffHunkStatus::Modified))
            .collect();

        assert_eq!(
            deleted_rows.len(),
            1,
            "Should have one deleted phantom row for old content"
        );
        assert_eq!(
            modified_rows.len(),
            1,
            "Should have one modified row for new content"
        );
    }

    #[test]
    fn buffer_row_to_display_mapping() {
        let buffer = create_buffer("line 1\nline 3");
        let snapshot = buffer.snapshot();

        let base_text = "line 1\nline 2\nline 3";
        let diff = BufferDiff::new(buffer.remote_id(), base_text.to_string(), &snapshot)
            .expect("Failed to create diff");

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff));

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

        let display_buffer = DisplayBuffer::new(snapshot, Some(diff));

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
