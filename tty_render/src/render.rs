//! The grid render passes that draw [`stoatty_term`]'s cells.

pub mod background;
pub mod decoration;
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

/// Cell layout metrics in physical pixels, derived from the configured font size.
///
/// The grid passes need one consistent cell rectangle, and the background and
/// text passes must agree on it so glyphs land on their cells. Width and height
/// keep a placeholder ratio to the font size (0.6 and 1.2) until real font
/// metrics replace them.
#[derive(Clone, Copy)]
pub(crate) struct CellMetrics {
    pub(crate) font_size: f32,
    pub(crate) width: f32,
    pub(crate) height: f32,
}

impl CellMetrics {
    /// Derive the cell rectangle from `font_size` using the placeholder ratio.
    pub(crate) fn from_font_size(font_size: u32) -> CellMetrics {
        let font_size = font_size as f32;
        CellMetrics {
            font_size,
            width: font_size * 0.6,
            height: font_size * 1.2,
        }
    }
}
