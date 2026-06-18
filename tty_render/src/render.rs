//! The grid render passes that draw [`stoatty_term`]'s cells.

pub mod background;
pub mod bar;
pub mod decoration;
pub mod icon;
pub mod overlay;
pub mod text;

/// The eased vertical scroll offsets a frame applies, in rows.
#[derive(Clone, Copy)]
pub struct Scroll<'a> {
    /// Whole-grid scroll, applied to every cell outside a scroll region.
    pub grid: f32,
    /// Scroll-region content scroll, applied to the cells inside the grid's
    /// scroll region instead of [`Self::grid`].
    pub region: f32,
    /// One content scroll offset per overlay, in overlay order, so several
    /// popovers scroll independently. A missing entry is treated as zero.
    pub popovers: &'a [f32],
}

/// Cell layout metrics in physical pixels, derived from the configured logical
/// font size and the display scale factor.
///
/// The grid passes need one consistent cell rectangle, and the background and
/// text passes must agree on it so glyphs land on their cells. `font_size` is
/// the physical rasterization size, the logical points scaled by the display
/// density, so glyphs stay crisp on a high-DPI display. Width and height keep a
/// placeholder ratio to it (0.6 and 1.2) until real font metrics replace them.
#[derive(Clone, Copy)]
pub(crate) struct CellMetrics {
    pub(crate) font_size: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

impl CellMetrics {
    /// Derive the physical cell rectangle from the logical `font_size` and the
    /// display `scale_factor`, so a given font size keeps the same apparent size
    /// across display densities and rasterizes crisply on each.
    pub(crate) fn from_font_size(font_size: u32, scale_factor: f32) -> CellMetrics {
        let font_size = font_size as f32 * scale_factor;
        CellMetrics {
            font_size,
            width: font_size * 0.6,
            height: font_size * 1.2,
        }
    }
}

/// The `[width, height]` of one cell, in pixels, for `font_size` at
/// `scale_factor`.
///
/// A windowing layer sizes a window to a cols-by-rows cell extent by
/// multiplying by this, matching the cell rectangle the renderer lays the grid
/// out on. Pass `scale_factor` 1.0 for logical pixels, leaving the display
/// scaling to the window toolkit.
pub fn cell_size(font_size: u32, scale_factor: f32) -> [f32; 2] {
    let metrics = CellMetrics::from_font_size(font_size, scale_factor);
    [metrics.width, metrics.height]
}

#[cfg(test)]
mod tests {
    use super::CellMetrics;

    #[test]
    fn metrics_scale_logical_font_size_by_density() {
        let retina = CellMetrics::from_font_size(15, 2.0);
        assert_eq!(
            (retina.font_size, retina.width, retina.height),
            (30.0, 18.0, 36.0),
            "15 logical points at 2x render as 30 physical pixels"
        );

        let low = CellMetrics::from_font_size(15, 1.0);
        assert_eq!(
            (low.font_size, low.width, low.height),
            (15.0, 9.0, 18.0),
            "the same 15 logical points at 1x render half the pixels"
        );
    }
}
