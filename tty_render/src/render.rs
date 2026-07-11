//! The grid render passes that draw [`stoatty_term`]'s cells.

use bytemuck::{Pod, Zeroable};
use stoatty_term::{grid::Panel, term::Damage};

pub mod background;
pub mod bar;
pub mod decoration;
#[cfg(feature = "perf")]
pub mod hud;
pub mod icon;
pub mod overlay;
pub mod panel;
pub mod text;

/// The eased vertical scroll offsets a frame applies, in rows.
#[derive(Clone, Copy)]
pub struct Scroll<'a> {
    /// Whole-grid scroll, applied to every cell outside a scroll region.
    pub grid: f32,
    /// Sub-cell document-pool scroll, in rows, applied to the whole grid on top
    /// of [`Self::grid`].
    ///
    /// Carries the fractional remainder of an app-driven document scroll whose
    /// integer rows are already baked into which pooled page-rows fill the grid,
    /// so it glides the composed view pixel-by-pixel and rests at zero on a cell
    /// boundary. Zero outside document-pool rendering.
    pub document: f32,
    /// Sub-cell scrollback-history scroll, in rows, applied to the whole grid on
    /// top of [`Self::grid`].
    ///
    /// Carries the fractional remainder of an eased wheel move through the
    /// terminal's own scrollback, whose integer rows are already baked into which
    /// history rows fill the composed scrollback window, so it glides the window
    /// pixel-by-pixel and rests on a cell boundary. Zero outside scrollback
    /// rendering.
    pub scrollback: f32,
    /// Scroll-region content scroll, applied to the cells inside the grid's
    /// scroll region instead of [`Self::grid`].
    pub region: f32,
    /// One content scroll offset per overlay, in overlay order, so several
    /// popovers scroll independently. A missing entry is treated as zero.
    pub popovers: &'a [f32],
}

/// The per-frame dynamic inputs to render a grid.
///
/// Bundles the state that changes every frame, such as the cursor position, the
/// eased scroll offsets, and the rows the terminal changed since the previous
/// frame. The damaged rows let the text pass rebuild only changed rows.
/// [`Self::cursor_corners`] draws the cursor block, and [`Self::cursor`] breaks
/// ligatures on the cursor's cell.
pub struct Frame<'a> {
    /// Cursor cell origin in fractional cell coordinates, or `None` when
    /// hidden. Breaks the ligature on the cell it lands on. The drawn block
    /// comes from [`Self::cursor_corners`].
    pub cursor: Option<[f32; 2]>,
    /// The cursor block's four corners [TL, TR, BL, BR] in fractional cell
    /// coordinates, or `None` when hidden.
    ///
    /// Independent corners let the block be non-rectangular -- a warp stretches
    /// it along the motion path -- where a single position could only ever
    /// describe a rectangle. A rigid block sets the corners to one whole cell.
    pub cursor_corners: Option<[[f32; 2]; 4]>,
    pub scroll: Scroll<'a>,
    pub damage: &'a Damage,
    /// Rows where an APC cell decoration (border or scale) changed since the
    /// renderer last consumed this, distinct from the VT [`Damage`] in
    /// [`Self::damage`]. The cell-decoration passes gate their per-row rebuilds
    /// on it so an unchanged decoration is not re-uploaded every frame.
    pub decoration_damage: &'a Damage,
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

/// A panel's cell rectangle plus its declaration-order seq, uploaded to a
/// storage buffer the bar, text-run, and icon fragment shaders read to occlude
/// what a box covers.
///
/// The rect is in whole-cell units, which a shader scales by the cell size. A
/// drawn fragment is discarded when it lies inside an occluder whose `seq`
/// exceeds the drawn instance's own seq, so a box declared later (higher seq)
/// hides the lower chrome beneath its body while a box's own runs and bars
/// (seq above their panel) survive. Padded to 32 bytes so the storage-array
/// stride matches the 8-byte-aligned `vec2` layout WGSL computes.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
pub(crate) struct Occluder {
    cell: [f32; 2],
    size: [f32; 2],
    seq: u32,
    _pad: [u32; 3],
}

/// One occluder per panel, in declaration order.
pub(crate) fn build_occluders(panels: &[Panel]) -> Vec<Occluder> {
    panels
        .iter()
        .map(|panel| Occluder {
            cell: [panel.left as f32, panel.top as f32],
            size: [panel.width as f32, panel.height as f32],
            seq: panel.seq,
            _pad: [0; 3],
        })
        .collect()
}

/// The `(panel_count, occlude_all)` a pool composite writes into its globals
/// uniform for `panels`.
///
/// A pane pool sits beneath every box, so it occludes against all `panels`
/// with the seq test bypassed. The returned `occlude_all` of 1 tells the
/// shader to discard a fragment inside any panel rect regardless of seq. A
/// non-pane pool is box content itself and never occludes, so it reports no
/// panels. The returned pair maps directly onto the `panel_count` and
/// `occlude_all` fields the bar, text, and background composite shaders read.
pub(crate) fn composite_occlusion(occludable: bool, panels: &[Occluder]) -> (u32, u32) {
    if occludable {
        (panels.len() as u32, 1)
    } else {
        (0, 0)
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
    use super::{composite_occlusion, CellMetrics, Occluder};

    fn occluder() -> Occluder {
        Occluder {
            cell: [0.0, 0.0],
            size: [1.0, 1.0],
            seq: 0,
            _pad: [0; 3],
        }
    }

    #[test]
    fn occludable_pool_occludes_all_panels_without_a_seq_test() {
        let panels = [occluder(), occluder()];
        assert_eq!(
            composite_occlusion(true, &panels),
            (2, 1),
            "a pane pool reports every panel and bypasses the seq test"
        );
        assert_eq!(
            composite_occlusion(false, &panels),
            (0, 0),
            "a non-pane pool is box content and never occludes"
        );
    }

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
