//! The grid render passes that draw [`stoatty_term`]'s cells.

use bytemuck::{Pod, Zeroable};
use stoatty_term::{grid::Panel, term::Damage};

pub mod background;
pub mod bar;
pub mod decoration;
#[cfg(feature = "perf")]
pub mod hud;
pub mod icon;
pub mod minimap;
pub mod overlay;
pub mod panel;
pub mod polyline;
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
    /// Rows the grid's content scrolled up by since the previous frame.
    ///
    /// A scroll moves every row's content without changing most of it, so the
    /// passes that cache per row slide those caches by this instead of
    /// rebuilding them. Zero for a frame that did not scroll, and for any grid
    /// whose caller does not track it.
    pub scrolled_rows: usize,
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

/// Whether `built` differs from what was last uploaded, and so has to be sent
/// to the GPU again.
///
/// Chrome changes far less often than frames are drawn, so without this a
/// cursor blink or a scroll ease re-uploads bytes the GPU already holds. A
/// caller that grows its buffer never reaches a false answer, since a grow
/// implies the instance count changed.
pub(crate) fn upload_needed<T: PartialEq>(built: &[T], last: &[T]) -> bool {
    built != last
}

/// Move each row of `cache` up by `by`, repairing what survives and emptying
/// what the move vacates.
///
/// The passes cache built instances per row, and a scroll moves a row's content
/// without changing it, so sliding the cache keeps the shaping and building
/// those rows already paid for. Every cached instance bakes where it sits,
/// though, as a row index or a position, so `repair` corrects each survivor for
/// the row it lands on.
///
/// The last `by` rows are cleared rather than left holding what the first rows
/// held, so a caller that neglects to damage them draws nothing instead of the
/// previous occupants. A scroll of at least the whole height leaves nothing to
/// keep and empties every row.
pub(crate) fn rotate_row_cache<T>(cache: &mut [Vec<T>], by: usize, mut repair: impl FnMut(&mut T)) {
    if by == 0 {
        return;
    }
    if by >= cache.len() {
        for row in cache.iter_mut() {
            row.clear();
        }
        return;
    }

    cache.rotate_left(by);
    let kept = cache.len() - by;
    for row in &mut cache[..kept] {
        for instance in row.iter_mut() {
            repair(instance);
        }
    }
    for row in &mut cache[kept..] {
        row.clear();
    }
}

/// One occluder per panel, in declaration order.
pub(crate) fn build_occluders(panels: &[Panel]) -> Vec<Occluder> {
    panels.iter().map(panel_occluder).collect()
}

/// The occluders a pool composite uploads, given the panels on the live grid.
///
/// A pane pool sits beneath every box, so an occludable pool takes all `panels`.
/// A non-pane pool is box content itself, drawn where a box already sits, so
/// nothing occludes it except chrome that floats above every pooled surface.
/// Those are exactly the panels flagged [`Panel::above_pools`], and the rest are
/// filtered out.
///
/// See also:
/// - [`occlusion_globals`] for the count the composite shaders read off this list.
pub(crate) fn pool_occluders(occludable: bool, panels: &[Panel]) -> Vec<Occluder> {
    panels
        .iter()
        .filter(|panel| occludable || panel.above_pools)
        .map(panel_occluder)
        .collect()
}

/// The `(panel_count, occlude_all)` a pool composite writes into its globals
/// uniform for `occluders`.
///
/// The pair maps directly onto the `panel_count` and `occlude_all` fields the
/// bar, text, polyline, and background composite shaders read. An `occlude_all`
/// of 1 tells the shader to discard a fragment inside any uploaded occluder
/// regardless of seq, which is what a pool wants: [`pool_occluders`] has already
/// narrowed the list to the panels that cover this pool, so a seq test would only
/// undo that. An empty list occludes nothing.
pub(crate) fn occlusion_globals(occluders: &[Occluder]) -> (u32, u32) {
    if occluders.is_empty() {
        (0, 0)
    } else {
        (occluders.len() as u32, 1)
    }
}

fn panel_occluder(panel: &Panel) -> Occluder {
    Occluder {
        cell: [panel.left as f32, panel.top as f32],
        size: [panel.width as f32, panel.height as f32],
        seq: panel.seq,
        _pad: [0; 3],
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
    use super::{occlusion_globals, pool_occluders, rotate_row_cache, CellMetrics};
    use stoatty_term::grid::{BorderStyle, Panel, PanelShadow, Rgb};

    /// Rows keep the work already done for them, land repaired for where they
    /// now sit, and the rows the scroll exposed come back empty.
    #[test]
    fn rotating_a_cache_moves_repairs_and_empties() {
        // Five rows moved by two, so rotating the wrong way lands somewhere
        // else. An even split would leave the two directions indistinguishable.
        let mut cache = vec![vec![10, 11], vec![20], vec![30], vec![40], vec![50]];
        rotate_row_cache(&mut cache, 2, |value| *value -= 20);

        assert_eq!(
            cache,
            vec![vec![10], vec![20], vec![30], Vec::<i32>::new(), Vec::new()],
            "the rows below the scroll move up repaired, and the rows they \
             vacated are emptied",
        );
    }

    #[test]
    fn rotating_a_cache_by_nothing_leaves_it_alone() {
        let mut cache = vec![vec![1], vec![2]];
        rotate_row_cache(&mut cache, 0, |_: &mut i32| panic!("nothing is repaired"));

        assert_eq!(cache, vec![vec![1], vec![2]]);
    }

    /// A scroll of at least the whole height leaves nothing that was on screen,
    /// so every row has to come back empty rather than wrap around.
    #[test]
    fn rotating_a_cache_past_its_height_empties_every_row() {
        let mut cache = vec![vec![1], vec![2], vec![3]];
        rotate_row_cache(&mut cache, 3, |_: &mut i32| panic!("nothing survives"));

        assert_eq!(cache, vec![Vec::<i32>::new(), Vec::new(), Vec::new()]);
    }

    fn panel(seq: u32, above_pools: bool) -> Panel {
        Panel {
            top: 0,
            left: 0,
            width: 1,
            height: 1,
            style: BorderStyle::Light,
            border: Rgb::new(0, 0, 0),
            corner_radius: 0,
            fill: None,
            shadow: PanelShadow::None_,
            inset_x: 0,
            above_pools,
            seq,
        }
    }

    #[test]
    fn occludable_pool_occludes_all_panels_without_a_seq_test() {
        let occluders = pool_occluders(true, &[panel(1, false), panel(2, true)]);

        assert_eq!(
            occluders.iter().map(|o| o.seq).collect::<Vec<_>>(),
            [1, 2],
            "a pane pool sits beneath every box, flagged or not"
        );
        assert_eq!(
            occlusion_globals(&occluders),
            (2, 1),
            "a pane pool reports every panel and bypasses the seq test"
        );
    }

    #[test]
    fn non_pane_pool_occludes_only_against_panels_above_pools() {
        let occluders = pool_occluders(false, &[panel(1, false), panel(2, true)]);

        assert_eq!(
            occluders.iter().map(|o| o.seq).collect::<Vec<_>>(),
            [2],
            "a non-pane pool is box content, so only chrome above every pooled \
             surface covers it"
        );
        assert_eq!(occlusion_globals(&occluders), (1, 1));
    }

    #[test]
    fn non_pane_pool_with_no_panel_above_pools_occludes_nothing() {
        let occluders = pool_occluders(false, &[panel(1, false), panel(2, false)]);

        assert_eq!(occluders.len(), 0, "no panel floats above the pool");
        assert_eq!(
            occlusion_globals(&occluders),
            (0, 0),
            "an empty list leaves the composite unoccluded, as before the flag"
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
