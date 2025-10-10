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
//! 3. [`GutterLayout`] - Complete gutter layout computed during paint
//!
//! ## Rendering Flow
//!
//! During editor paint:
//! 1. Reserve space on left side for gutter
//! 2. Query [`BufferDiff`](stoat::git_diff::BufferDiff) for hunks
//! 3. Convert hunk anchors to visible row numbers
//! 4. Create [`DiffIndicator`] for each changed line
//! 5. Paint gutter background
//! 6. Paint each [`DiffIndicator`] with appropriate color
//!
//! # Colors
//!
//! - **Green** - Added lines
//! - **Blue** - Modified lines
//! - **Red** - Deleted lines (marker at deletion point)
//!
//! # Related
//!
//! - [`EditorElement`](super::editor_element::EditorElement) - Renders the gutter
//! - [`EditorStyle`](super::editor_style::EditorStyle) - Configures gutter appearance
//! - [`BufferDiff`](stoat::git_diff::BufferDiff) - Source of diff data

use gpui::{Bounds, Corners, Pixels, Point, point, px, size};
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
    /// Padding between gutter content and editor text
    pub right_padding: Pixels,
    /// Bounds of the entire gutter area
    pub bounds: Bounds<Pixels>,
}

/// A visual indicator for a git diff change in the gutter.
///
/// Represents an entire diff hunk rendered as a continuous colored shape.
/// Each indicator spans the full range of changed lines in a hunk and is styled
/// according to its [`DiffHunkStatus`].
///
/// Indicators are proportional-width vertical bars on the left edge of the gutter.
/// Deleted hunks (zero-length) are rendered as small rounded pills.
#[derive(Debug, Clone)]
pub struct DiffIndicator {
    /// Type of change (determines color)
    pub status: DiffHunkStatus,
    /// Bounds where this indicator should be painted
    pub bounds: Bounds<Pixels>,
    /// Corner radii for rounded corners (used for deleted hunks)
    pub corner_radii: Corners<Pixels>,
}

/// Complete gutter layout for rendering.
///
/// Computed during paint phase and contains all information needed to paint
/// the gutter, including dimensions and diff indicators for visible lines.
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
    /// * `right_padding` - Spacing between gutter content and editor text
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
    /// 2. Position indicators at left edge of gutter
    /// 3. Size indicators to match line height (following Zed's 0.275 ratio)
    pub fn new(
        gutter_bounds: Bounds<Pixels>,
        visible_rows: Range<u32>,
        diff: Option<&BufferDiff>,
        buffer_snapshot: &BufferSnapshot,
        gutter_width: Pixels,
        right_padding: Pixels,
        line_height: Pixels,
    ) -> Self {
        let dimensions = GutterDimensions {
            width: gutter_width,
            right_padding,
            bounds: gutter_bounds,
        };

        let mut diff_indicators = Vec::new();

        if let Some(diff) = diff {
            // Strip width scales with line height (matches Zed)
            let strip_width = (0.275 * line_height).floor();

            for hunk in &diff.hunks {
                // Convert anchor range to row range
                let hunk_start_row = hunk.buffer_range.start.to_point(buffer_snapshot).row;
                let hunk_end_row = hunk.buffer_range.end.to_point(buffer_snapshot).row;

                // Check if hunk intersects visible rows
                if hunk_end_row < visible_rows.start || hunk_start_row >= visible_rows.end {
                    continue;
                }

                // Clamp to visible range
                let visible_start = hunk_start_row.max(visible_rows.start);
                let visible_end = hunk_end_row.min(visible_rows.end.saturating_sub(1));

                // Compute bounds and corner radii based on hunk type
                let (bounds, corner_radii) = if hunk_start_row == hunk_end_row
                    && matches!(hunk.status, DiffHunkStatus::Deleted)
                {
                    // Deleted hunk (zero-length) - small rounded pill
                    let width = (0.35 * line_height).floor();
                    let y_offset = line_height * ((visible_start - visible_rows.start) as f32)
                        - line_height / 2.0;

                    let bounds = Bounds {
                        origin: point(gutter_bounds.origin.x, gutter_bounds.origin.y + y_offset),
                        size: size(width, line_height),
                    };

                    (bounds, Corners::all(line_height))
                } else {
                    // Added/Modified hunk - continuous vertical bar
                    let y_start = line_height * ((visible_start - visible_rows.start) as f32);
                    let y_end = line_height * ((visible_end - visible_rows.start + 1) as f32);

                    let bounds = Bounds {
                        origin: point(gutter_bounds.origin.x, gutter_bounds.origin.y + y_start),
                        size: size(strip_width, y_end - y_start),
                    };

                    (bounds, Corners::all(px(0.0)))
                };

                diff_indicators.push(DiffIndicator {
                    status: hunk.status,
                    bounds,
                    corner_radii,
                });
            }
        }

        Self {
            dimensions,
            diff_indicators,
        }
    }

    /// Check if a pixel position is within the gutter bounds.
    ///
    /// Used for mouse interaction (future feature).
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
}
