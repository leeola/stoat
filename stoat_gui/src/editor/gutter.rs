//! Git diff gutter visualization.
//!
//! This module provides structures and logic for displaying git diff indicators
//! in the editor gutter (the margin on the left side of the editor).
//!
//! # Architecture
//!
//! The gutter system has three main components:
//!
//! 1. [`GutterDimensions`] - Size and position of the gutter area
//! 2. [`DiffIndicator`] - Individual colored bar for a changed line
//! 3. [`GutterLayout`] - Complete gutter layout computed during prepaint
//!
//! ## Rendering Flow
//!
//! During editor prepaint:
//! 1. Reserve space on left side for gutter
//! 2. Query [`BufferDiff`](stoat::git_diff::BufferDiff) for hunks
//! 3. Convert hunk anchors to visible row numbers
//! 4. Create [`DiffIndicator`] for each changed line
//!
//! During editor paint:
//! 1. Paint gutter background
//! 2. Paint each [`DiffIndicator`] with appropriate color
//!
//! # Colors
//!
//! - **Green** - Added lines
//! - **Blue** - Modified lines
//! - **Red** - Deleted lines (marker at deletion point)
//!
//! # Related
//!
//! - [`EditorElement`](super::element::EditorElement) - Renders the gutter
//! - [`EditorStyle`](super::style::EditorStyle) - Configures gutter appearance
//! - [`BufferDiff`](stoat::git_diff::BufferDiff) - Source of diff data

use gpui::{point, px, size, Bounds, Pixels, Point};
use std::ops::Range;
use stoat::git_diff::{BufferDiff, DiffHunkStatus};
use text::{BufferSnapshot, ToPoint};

/// Dimensions and position of the gutter area.
///
/// The gutter is a vertical strip on the left side of the editor that displays
/// line numbers, breakpoints, and git diff indicators.
#[derive(Debug, Clone)]
pub struct GutterDimensions {
    /// Width of the gutter in pixels (typically 40px)
    pub width: Pixels,
    /// Bounds of the entire gutter area
    pub bounds: Bounds<Pixels>,
}

/// A visual indicator for a git diff change in the gutter.
///
/// Represents a single colored bar showing that a line has been added, modified, or deleted.
/// Each indicator is positioned at a specific row and colored according to its [`DiffHunkStatus`].
///
/// Indicators are thin vertical bars (3px wide) positioned at the right edge of the gutter.
#[derive(Debug, Clone)]
pub struct DiffIndicator {
    /// Row number in the buffer (0-indexed)
    pub row: u32,
    /// Type of change (determines color)
    pub status: DiffHunkStatus,
    /// Bounds where this indicator should be painted
    pub bounds: Bounds<Pixels>,
}

/// Complete gutter layout for rendering.
///
/// Computed during prepaint phase and contains all information needed to paint
/// the gutter, including dimensions and diff indicators for visible lines.
///
/// # Lifecycle
///
/// 1. Created in [`EditorElement::prepaint`](super::element::EditorElement::prepaint)
/// 2. Stored in [`EditorLayout`](super::layout::EditorLayout)
/// 3. Used in [`EditorElement::paint`](super::element::EditorElement::paint)
///
/// # Example
///
/// ```ignore
/// let gutter = GutterLayout::new(
///     gutter_bounds,
///     start_row..end_row,
///     Some(&diff),
///     &buffer_snapshot,
///     px(40.0),
///     px(20.0),
/// );
///
/// // Later in paint:
/// for indicator in &gutter.diff_indicators {
///     paint_colored_bar(indicator);
/// }
/// ```
#[derive(Debug, Clone)]
pub struct GutterLayout {
    /// Gutter dimensions and position
    pub dimensions: GutterDimensions,
    /// Diff indicators for visible lines
    pub diff_indicators: Vec<DiffIndicator>,
}

impl GutterLayout {
    /// Compute gutter layout for visible rows.
    ///
    /// Finds all diff hunks that intersect with visible rows and creates
    /// indicators for each changed line.
    ///
    /// # Arguments
    ///
    /// * `gutter_bounds` - Bounds for the gutter area
    /// * `visible_rows` - Range of rows currently visible in viewport
    /// * `diff` - Optional diff data (None if not in git repo)
    /// * `buffer_snapshot` - Buffer snapshot for converting anchors to positions
    /// * `gutter_width` - Total width of gutter in pixels
    /// * `line_height` - Height of one line in pixels
    ///
    /// # Returns
    ///
    /// [`GutterLayout`] with indicators for all visible changed lines
    ///
    /// # Algorithm
    ///
    /// 1. For each hunk in the diff:
    ///    - Convert anchor range to row range
    ///    - Check if hunk intersects visible rows
    ///    - Create indicator for each visible row in hunk
    /// 2. Position indicators at right edge of gutter
    /// 3. Size indicators to match line height
    pub fn new(
        gutter_bounds: Bounds<Pixels>,
        visible_rows: Range<u32>,
        diff: Option<&BufferDiff>,
        buffer_snapshot: &BufferSnapshot,
        gutter_width: Pixels,
        line_height: Pixels,
    ) -> Self {
        let dimensions = GutterDimensions {
            width: gutter_width,
            bounds: gutter_bounds,
        };

        let mut diff_indicators = Vec::new();

        if let Some(diff) = diff {
            let indicator_width = px(3.0);
            let indicator_padding = px(4.0);

            for hunk in &diff.hunks {
                // Convert anchor range to row range
                let hunk_start_row = hunk.buffer_range.start.to_point(buffer_snapshot).row;
                let hunk_end_row = hunk.buffer_range.end.to_point(buffer_snapshot).row;

                // Handle deleted hunks (zero-length range)
                let (start_row, end_row) = if hunk_start_row == hunk_end_row
                    && matches!(hunk.status, DiffHunkStatus::Deleted)
                {
                    // Deleted hunk - show indicator at deletion point
                    (hunk_start_row, hunk_start_row)
                } else {
                    (hunk_start_row, hunk_end_row.max(hunk_start_row))
                };

                // Create indicator for each visible row in this hunk
                for row in start_row..=end_row {
                    if row >= visible_rows.start && row < visible_rows.end {
                        let relative_row = row - visible_rows.start;

                        let indicator_bounds = Bounds {
                            origin: point(
                                gutter_bounds.origin.x + gutter_width
                                    - indicator_width
                                    - indicator_padding,
                                gutter_bounds.origin.y + line_height * (relative_row as f32),
                            ),
                            size: size(indicator_width, line_height),
                        };

                        diff_indicators.push(DiffIndicator {
                            row,
                            status: hunk.status,
                            bounds: indicator_bounds,
                        });
                    }
                }
            }
        }

        Self {
            dimensions,
            diff_indicators,
        }
    }

    /// Check if a pixel position is within the gutter bounds.
    ///
    /// Used for mouse interaction (future Phase 3 feature).
    ///
    /// # Arguments
    ///
    /// * `position` - Pixel position to test
    ///
    /// # Returns
    ///
    /// `true` if position is inside gutter, `false` otherwise
    pub fn contains_point(&self, position: Point<Pixels>) -> bool {
        self.dimensions.bounds.contains(&position)
    }

    /// Find the diff indicator at a given row.
    ///
    /// Used for mouse interaction (future Phase 3 feature).
    ///
    /// # Arguments
    ///
    /// * `row` - Row number to search for
    ///
    /// # Returns
    ///
    /// Reference to the indicator at this row, if one exists
    pub fn indicator_for_row(&self, row: u32) -> Option<&DiffIndicator> {
        self.diff_indicators.iter().find(|ind| ind.row == row)
    }
}
