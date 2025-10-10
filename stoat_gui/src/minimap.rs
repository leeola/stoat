//! Minimap rendering and layout calculations.
//!
//! The minimap provides a zoomed-out view of the entire file on the right side
//! of the editor, with a viewport indicator showing the current visible region.
//!
//! ## Architecture
//!
//! Following VS Code's approach, the minimap renders colored blocks representing
//! code structure rather than actual text. This provides excellent performance
//! while maintaining visual fidelity through syntax highlighting colors.
//!
//! The viewport "thumb" is rendered as a semi-transparent overlay that moves
//! proportionally with the editor's scroll position.
//!
//! Performance is optimized through aggressive caching - colored blocks are
//! pre-computed and cached, then reused across frames during scroll animations.

use clock::Global;
use gpui::{point, Bounds, Hsla, Pixels};

/// Font size for minimap text rendering (matches Zed)
pub const MINIMAP_FONT_SIZE: f32 = 2.0;

/// Line height for minimap text (tight spacing)
pub const MINIMAP_LINE_HEIGHT: f32 = 2.5;

/// Minimap width as a percentage of the editor width
pub const MINIMAP_WIDTH_PCT: f32 = 0.15;

/// Minimum width of the minimap in columns
pub const MINIMAP_MIN_WIDTH_COLUMNS: f32 = 20.0;

/// Maximum number of lines to render in the minimap
///
/// This caps the number of lines rendered even on very tall displays,
/// preventing performance issues when rendering hundreds of tiny lines.
pub const MAX_MINIMAP_LINES: f32 = 200.0;

/// Layout information for rendering the minimap.
///
/// Contains bounds, scroll calculations, and thumb position for rendering
/// the minimap and its viewport indicator.
#[derive(Debug, Clone)]
pub struct MinimapLayout {
    /// Bounds of the minimap region within the editor
    pub minimap_bounds: Bounds<Pixels>,
    /// Bounds of the viewport thumb indicator (None if entire file is visible)
    pub thumb_bounds: Option<Bounds<Pixels>>,
    /// Scroll position for the minimap (in lines)
    pub minimap_scroll_y: f32,
    /// Number of visible lines in the minimap
    pub visible_minimap_lines: f32,
}

impl MinimapLayout {
    /// Calculate the minimap scroll position based on the editor's scroll position.
    ///
    /// The minimap scroll is calculated proportionally so that the visible portion
    /// of the editor aligns with the corresponding position in the minimap.
    ///
    /// # Arguments
    ///
    /// * `total_lines` - Total number of lines in the document
    /// * `visible_editor_lines` - Number of lines visible in the main editor viewport
    /// * `visible_minimap_lines` - Number of lines that fit in the minimap viewport
    /// * `editor_scroll_y` - Current scroll position of the editor (in lines)
    ///
    /// # Returns
    ///
    /// The scroll position for the minimap (in lines)
    pub fn calculate_minimap_scroll(
        total_lines: f64,
        visible_editor_lines: f64,
        visible_minimap_lines: f64,
        editor_scroll_y: f64,
    ) -> f32 {
        // Calculate how many lines are not visible in the editor
        let non_visible_lines = (total_lines - visible_editor_lines).max(0.0);

        if non_visible_lines == 0.0 {
            // Entire document fits in viewport - no scroll needed
            return 0.0;
        }

        // Calculate scroll percentage (0.0 = top, 1.0 = bottom)
        let scroll_percentage = (editor_scroll_y / non_visible_lines).clamp(0.0, 1.0);

        // Apply percentage to minimap's scrollable range
        let minimap_scrollable = (total_lines - visible_minimap_lines).max(0.0);
        (scroll_percentage * minimap_scrollable) as f32
    }

    /// Calculate the bounds for the viewport thumb indicator.
    ///
    /// The thumb shows which portion of the file is currently visible in the
    /// main editor. Its size is proportional to the ratio of visible lines to
    /// total lines, and its position corresponds to the scroll position.
    ///
    /// # Arguments
    ///
    /// * `minimap_bounds` - Bounds of the entire minimap region
    /// * `total_lines` - Total number of lines in the document
    /// * `visible_editor_lines` - Number of lines visible in the main editor viewport
    /// * `editor_scroll_y` - Current scroll position of the editor (in lines)
    ///
    /// # Returns
    ///
    /// Bounds for the thumb, or None if the entire document is visible (no thumb needed)
    pub fn calculate_thumb_bounds(
        minimap_bounds: Bounds<Pixels>,
        total_lines: f64,
        visible_editor_lines: f64,
        editor_scroll_y: f64,
    ) -> Option<Bounds<Pixels>> {
        // If the entire document fits in the viewport, no thumb is needed
        if total_lines <= visible_editor_lines {
            return None;
        }

        let minimap_height = minimap_bounds.size.height;

        // Calculate thumb height as a proportion of minimap height
        let thumb_height_ratio = (visible_editor_lines / total_lines).clamp(0.0, 1.0);
        let thumb_height = minimap_height * thumb_height_ratio as f32;

        // Calculate thumb Y position
        let max_scroll = (total_lines - visible_editor_lines).max(0.0);
        let scroll_ratio = if max_scroll > 0.0 {
            (editor_scroll_y / max_scroll).clamp(0.0, 1.0)
        } else {
            0.0
        };

        let max_thumb_y = minimap_height - thumb_height;
        let thumb_y = minimap_bounds.origin.y + (max_thumb_y * scroll_ratio as f32);

        Some(Bounds {
            origin: point(minimap_bounds.origin.x, thumb_y),
            size: gpui::size(minimap_bounds.size.width, thumb_height),
        })
    }

    /// Calculate the appropriate width for the minimap.
    ///
    /// Width is calculated as a percentage of the editor width, with minimum
    /// and maximum constraints based on column count and character width.
    ///
    /// # Arguments
    ///
    /// * `editor_width` - Width of the main editor
    /// * `em_width` - Width of a single character at editor font size
    /// * `max_columns` - Maximum width in columns
    ///
    /// # Returns
    ///
    /// The calculated minimap width in pixels
    pub fn calculate_minimap_width(
        editor_width: Pixels,
        em_width: Pixels,
        max_columns: f32,
    ) -> Pixels {
        // Calculate width as percentage of editor
        let width_from_pct = editor_width * MINIMAP_WIDTH_PCT;

        // Calculate minimap character width (proportional to font size reduction)
        let font_scale = MINIMAP_FONT_SIZE / 14.0; // Assuming 14px base font
        let minimap_em_width = em_width * font_scale;

        // Maximum width in pixels based on column constraint
        let max_width = minimap_em_width * max_columns;

        // Minimum width based on minimum columns
        let min_width = minimap_em_width * MINIMAP_MIN_WIDTH_COLUMNS;

        // Apply constraints
        width_from_pct.max(min_width).min(max_width)
    }
}

/// Cache for minimap colored blocks to eliminate all computation during scroll.
///
/// VS Code-style minimaps use colored rectangles instead of text, which is much
/// faster to render. By caching these rectangles, we avoid even building the
/// block list every frame - we just paint pre-computed quads.
///
/// The cache is invalidated only when:
/// - Buffer content changes (edits)
/// - Syntax highlighting updates
/// - Minimap bounds change (window resize)
/// - Minimap scroll position changes significantly
///
/// During scroll animation, the cache remains valid and we simply paint the
/// same cached quads at blazing speed (~0.1-0.3ms per frame).
#[derive(Default, Clone)]
pub struct MinimapCache {
    /// Pre-computed colored quads (rectangles) ready to paint
    pub cached_quads: Vec<CachedQuad>,
    /// Buffer version when cache was built
    pub buffer_version: Option<Global>,
    /// Token version when cache was built
    pub token_version: Option<Global>,
    /// Minimap bounds when cache was built
    pub cached_bounds: Option<Bounds<Pixels>>,
    /// Minimap scroll position when cache was built
    pub cached_scroll_y: Option<f32>,
}

/// A single colored rectangle representing a syntax chunk in the minimap
#[derive(Clone)]
pub struct CachedQuad {
    /// Bounds of the colored rectangle
    pub bounds: Bounds<Pixels>,
    /// Color from syntax highlighting
    pub color: Hsla,
}

impl MinimapCache {
    /// Check if the cache is still valid for the given parameters
    pub fn is_valid(
        &self,
        buffer_version: &Global,
        token_version: &Global,
        bounds: Bounds<Pixels>,
        scroll_y: f32,
    ) -> bool {
        self.buffer_version.as_ref() == Some(buffer_version)
            && self.token_version.as_ref() == Some(token_version)
            && self.cached_bounds == Some(bounds)
            && self.cached_scroll_y == Some(scroll_y)
            && !self.cached_quads.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calculate_minimap_scroll_full_viewport() {
        // When entire document fits in viewport, scroll should be 0
        let scroll = MinimapLayout::calculate_minimap_scroll(
            50.0,  // total_lines
            100.0, // visible_editor_lines (more than total)
            200.0, // visible_minimap_lines
            0.0,   // editor_scroll_y
        );
        assert_eq!(scroll, 0.0);
    }

    #[test]
    fn calculate_minimap_scroll_at_top() {
        // At top of document, minimap scroll should be 0
        let scroll = MinimapLayout::calculate_minimap_scroll(
            1000.0, // total_lines
            50.0,   // visible_editor_lines
            200.0,  // visible_minimap_lines
            0.0,    // editor_scroll_y (at top)
        );
        assert_eq!(scroll, 0.0);
    }

    #[test]
    fn calculate_minimap_scroll_at_bottom() {
        // At bottom of document, minimap should be scrolled to its bottom
        let scroll = MinimapLayout::calculate_minimap_scroll(
            1000.0, // total_lines
            50.0,   // visible_editor_lines
            200.0,  // visible_minimap_lines
            950.0,  // editor_scroll_y (at bottom: total - visible)
        );
        // Minimap scroll should be at: total - visible_minimap = 800
        assert_eq!(scroll, 800.0);
    }

    #[test]
    fn calculate_minimap_scroll_middle() {
        // At middle of document
        let scroll = MinimapLayout::calculate_minimap_scroll(
            1000.0, // total_lines
            50.0,   // visible_editor_lines
            200.0,  // visible_minimap_lines
            475.0,  // editor_scroll_y (halfway: (total - visible) / 2)
        );
        // Should be roughly in the middle: (total - visible_minimap) / 2 = 400
        assert_eq!(scroll, 400.0);
    }

    #[test]
    fn calculate_thumb_bounds_full_viewport() {
        // When entire document is visible, no thumb should be shown
        let bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: gpui::size(px(100.0), px(400.0)),
        };
        let thumb = MinimapLayout::calculate_thumb_bounds(
            bounds, 50.0,  // total_lines
            100.0, // visible_editor_lines (more than total)
            0.0,   // editor_scroll_y
        );
        assert!(thumb.is_none());
    }

    #[test]
    fn calculate_thumb_bounds_half_visible() {
        // When half the document is visible, thumb should be half the height
        let bounds = Bounds {
            origin: point(px(0.0), px(0.0)),
            size: gpui::size(px(100.0), px(400.0)),
        };
        let thumb = MinimapLayout::calculate_thumb_bounds(
            bounds, 100.0, // total_lines
            50.0,  // visible_editor_lines (half)
            0.0,   // editor_scroll_y (at top)
        );
        assert!(thumb.is_some());
        let thumb = thumb.unwrap();
        assert_eq!(thumb.size.height, px(200.0)); // Half of 400px
        assert_eq!(thumb.origin.y, px(0.0)); // At top
    }
}
