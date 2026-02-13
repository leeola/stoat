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
//! 2. Query [`BufferDiff`](crate::git::diff::BufferDiff) for hunks
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
//! - [`EditorElement`](super::editor::element::EditorElement) - Renders the gutter
//! - [`EditorStyle`](super::editor::style::EditorStyle) - Configures gutter appearance
//! - [`BufferDiff`](crate::git::diff::BufferDiff) - Source of diff data

use crate::{
    buffer::display::DiffOrigin,
    git::diff::{BufferDiff, DiffHunkStatus},
};
use gpui::{point, px, size, Bounds, Corners, Pixels, Point};
use std::ops::Range;
use text::{BufferSnapshot, ToPoint};

/// Minimal row info needed for gutter rendering.
pub trait DisplayRow {
    fn y_position(&self) -> Pixels;
    fn diff_status(&self) -> Option<DiffHunkStatus>;
    fn diff_origin(&self) -> DiffOrigin;
}

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
    /// Origin of this indicator (unstaged, staged, or committed)
    pub origin: DiffOrigin,
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
    /// Creates diff indicators for both buffer rows (from buffer hunks) and
    /// display rows (including phantom rows for deleted content).
    ///
    /// # Arguments
    ///
    /// * `gutter_bounds` - Bounds for the gutter area
    /// * `visible_rows` - Range of buffer rows currently visible in viewport
    /// * `display_rows` - Display row information including phantom rows
    /// * `diff` - Optional diff data (None if not in git repo)
    /// * `buffer_snapshot` - Buffer snapshot for converting anchors to positions
    /// * `gutter_width` - Total width of gutter in pixels
    /// * `right_padding` - Spacing between gutter content and editor text
    /// * `line_height` - Height of one line in pixels
    /// * `strip_width` - Width of diff indicator strip
    /// * `is_in_diff_review` - When true, skips Phase 2 deletion pills (Phase 1 already covers
    ///   them)
    ///
    /// # Returns
    ///
    /// [`GutterLayout`] with indicators for all visible changed lines
    ///
    /// # Algorithm
    ///
    /// 1. Create indicators for display rows (includes phantom deleted rows)
    /// 2. Create indicators for buffer hunks (includes deleted markers at buffer positions)
    /// 3. Position indicators at left edge of gutter
    /// 4. Size indicators based on provided strip_width
    #[allow(clippy::too_many_arguments)]
    pub fn new<T: DisplayRow>(
        gutter_bounds: Bounds<Pixels>,
        visible_rows: Range<u32>,
        display_rows: &[T],
        diff: Option<&BufferDiff>,
        buffer_snapshot: &BufferSnapshot,
        gutter_width: Pixels,
        right_padding: Pixels,
        line_height: Pixels,
        strip_width: Pixels,
        is_in_diff_review: bool,
    ) -> Self {
        let dimensions = GutterDimensions {
            width: gutter_width,
            right_padding,
            bounds: gutter_bounds,
        };

        let mut diff_indicators = Vec::new();

        // PHASE 1: Create indicators for display rows (includes phantom rows)
        // Group consecutive rows with the same diff status and origin
        let mut current_group: Option<(DiffHunkStatus, DiffOrigin, Pixels, Pixels)> = None;

        for row in display_rows {
            if let Some(status) = row.diff_status() {
                let origin = row.diff_origin();
                match current_group {
                    Some((group_status, group_origin, start_y, _))
                        if group_status == status && group_origin == origin =>
                    {
                        current_group = Some((
                            group_status,
                            group_origin,
                            start_y,
                            row.y_position() + line_height,
                        ));
                    },
                    Some((group_status, group_origin, start_y, end_y)) => {
                        diff_indicators.push(DiffIndicator {
                            status: group_status,
                            bounds: Bounds {
                                origin: point(gutter_bounds.origin.x, start_y),
                                size: size(strip_width, end_y - start_y),
                            },
                            corner_radii: Corners::all(px(0.0)),
                            origin: group_origin,
                        });
                        current_group = Some((
                            status,
                            origin,
                            row.y_position(),
                            row.y_position() + line_height,
                        ));
                    },
                    None => {
                        current_group = Some((
                            status,
                            origin,
                            row.y_position(),
                            row.y_position() + line_height,
                        ));
                    },
                }
            } else if let Some((group_status, group_origin, start_y, end_y)) = current_group {
                diff_indicators.push(DiffIndicator {
                    status: group_status,
                    bounds: Bounds {
                        origin: point(gutter_bounds.origin.x, start_y),
                        size: size(strip_width, end_y - start_y),
                    },
                    corner_radii: Corners::all(px(0.0)),
                    origin: group_origin,
                });
                current_group = None;
            }
        }

        if let Some((group_status, group_origin, start_y, end_y)) = current_group {
            diff_indicators.push(DiffIndicator {
                status: group_status,
                bounds: Bounds {
                    origin: point(gutter_bounds.origin.x, start_y),
                    size: size(strip_width, end_y - start_y),
                },
                corner_radii: Corners::all(px(0.0)),
                origin: group_origin,
            });
        }

        // PHASE 2: Add special markers for zero-length deleted hunks at buffer positions.
        // Skipped in diff review mode where Phase 1 already creates deleted indicators
        // from phantom display rows.
        if !is_in_diff_review {
            if let Some(diff) = diff {
                for hunk in &diff.hunks {
                    let hunk_start_row = hunk.buffer_range.start.to_point(buffer_snapshot).row;
                    let hunk_end_row = hunk.buffer_range.end.to_point(buffer_snapshot).row;

                    if hunk_start_row == hunk_end_row
                        && matches!(hunk.status, DiffHunkStatus::Deleted)
                    {
                        let display_row = hunk_start_row + 1;
                        if display_row < visible_rows.start || display_row >= visible_rows.end {
                            continue;
                        }

                        let pill_height = line_height;
                        let y_offset =
                            right_padding + line_height * (display_row - visible_rows.start) as f32;

                        let bounds = Bounds {
                            origin: point(
                                gutter_bounds.origin.x,
                                gutter_bounds.origin.y + y_offset,
                            ),
                            size: size(strip_width, pill_height),
                        };

                        diff_indicators.push(DiffIndicator {
                            status: DiffHunkStatus::Deleted,
                            bounds,
                            corner_radii: Corners::all(px(0.0)),
                            origin: DiffOrigin::Unstaged,
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
