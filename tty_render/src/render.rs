//! The grid render passes that draw [`stoatty_term`]'s cells.

use bytemuck::{Pod, Zeroable};
use std::ops::Range;
use stoatty_term::{grid::Panel, term::Damage};
use wgpu::Buffer;

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
    /// Rows the grid's content moved since the previous frame, positive for a
    /// move up the screen.
    ///
    /// A scroll moves every row's content without changing most of it, so the
    /// passes that cache per row slide those caches by this instead of
    /// rebuilding them. Live output only ever moves content up. Gliding back
    /// through scrollback moves it down, which is what the sign carries. Zero
    /// for a frame that did not scroll, and for any grid whose caller does not
    /// track it.
    pub scrolled_rows: isize,
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

/// One pool's instances for a composite draw.
///
/// A frame composites several pools over the live grid, and each needs its own
/// buffer so every pool can be prepared before any of them draws. With one buffer
/// per pass, a pool that reuses last frame's instances reads whatever pool was
/// prepared after it, so the draws have to be separated by a submit.
pub(crate) struct CompositeSlot {
    pub(crate) instances: Buffer,
    pub(crate) capacity: usize,
    pub(crate) count: u32,
}

/// Distinct pools a pass keeps composite buffers for.
///
/// Sized past what a window reaches in practice, which is a pane and a status-row
/// pool per pane plus the fixed modal pools. The cap bounds the renderer's own
/// memory rather than limiting a working set, so exceeding it costs a rebuild
/// rather than correctness.
const MAX_COMPOSITE_POOLS: usize = 32;

/// A pass's per-pool composite state, keyed by pool id.
///
/// Keyed by id rather than by the pool's position in the frame, because pools
/// appear in a frame only while they glide. One settling shifts the rest down a
/// position, and a pool that reuses last frame's instances would then read a
/// sibling's and paint them inside its own scissor.
///
/// Holds at most [`MAX_COMPOSITE_POOLS`] pools, taking over the least recently
/// requested when a new one arrives, so the buffers stay bounded by the renderer
/// itself rather than by how many distinct ids its callers happen to use.
pub(crate) struct CompositeSlots<T> {
    /// Least-recently-requested first, so the front is the one to take over.
    entries: Vec<(u32, T)>,
}

impl<T> CompositeSlots<T> {
    pub(crate) fn new() -> CompositeSlots<T> {
        CompositeSlots {
            entries: Vec::new(),
        }
    }

    /// Pool `pool`'s slot, built by `make` when it has none.
    ///
    /// Marks the slot most recently requested, so a frame that composites the same
    /// pools as the last never takes one over.
    pub(crate) fn entry(&mut self, pool: u32, make: impl FnOnce() -> T) -> &mut T {
        if let Some(at) = self.entries.iter().position(|(id, _)| *id == pool) {
            self.entries[at..].rotate_left(1);
        } else if self.entries.len() == MAX_COMPOSITE_POOLS {
            self.entries[0] = (pool, make());
            self.entries.rotate_left(1);
        } else {
            self.entries.push((pool, make()));
        }

        &mut self.entries.last_mut().expect("just placed").1
    }

    /// Pool `pool`'s slot, or `None` when it has none.
    ///
    /// Leaves the order alone, so a draw reading a slot cannot change which pool is
    /// next to be taken over.
    pub(crate) fn get(&self, pool: u32) -> Option<&T> {
        self.entries
            .iter()
            .find(|(id, _)| *id == pool)
            .map(|(_, slot)| slot)
    }
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

/// Move each row of `cache` by `by`, up the screen when positive and down when
/// negative, repairing what survives and emptying what the move vacates.
///
/// The passes cache built instances per row, and a scroll moves a row's content
/// without changing it, so sliding the cache keeps the shaping and building
/// those rows already paid for. Every cached instance bakes where it sits,
/// though, as a row index or a position, so `repair` corrects each survivor for
/// the row it lands on.
///
/// The rows the move vacates are cleared rather than left holding what wrapped
/// around into them, so a caller that neglects to damage them draws nothing
/// instead of the wrong thing. A move of at least the whole height leaves
/// nothing to keep and empties every row.
pub(crate) fn rotate_row_cache<T>(cache: &mut [Vec<T>], by: isize, mut repair: impl FnMut(&mut T)) {
    if by == 0 {
        return;
    }
    let magnitude = by.unsigned_abs();
    if magnitude >= cache.len() {
        for row in cache.iter_mut() {
            row.clear();
        }
        return;
    }

    let (repaired, vacated) = if by > 0 {
        cache.rotate_left(magnitude);
        cache.split_at_mut(cache.len() - magnitude)
    } else {
        cache.rotate_right(magnitude);
        let (vacated, repaired) = cache.split_at_mut(magnitude);
        (repaired, vacated)
    };

    for row in repaired {
        for instance in row.iter_mut() {
            repair(instance);
        }
    }
    for row in vacated {
        row.clear();
    }
}

/// The writes that carry a frame's changed rows into a per-row instance buffer,
/// each an instance offset and the rows to send there.
///
/// `rebuilt` are the rows rebuilt this frame, ascending. They are patched where
/// they sit, adjacent ones sharing a write, which is only sound while the rows
/// before them still hold the lengths the buffer was written with.
///
/// `rewrite_from` is the row where that stops holding, because it or an earlier
/// row came back a different length and displaced everything after it. From
/// there to the end of `cache` the buffer is rewritten as one span, and the
/// rebuilt rows it covers are left out of the per-row writes. `None` means every
/// rebuilt row kept its length and nothing moved.
pub(crate) fn row_uploads<T>(
    cache: &[Vec<T>],
    rebuilt: &[usize],
    rewrite_from: Option<usize>,
) -> impl Iterator<Item = (usize, Range<usize>)> {
    let patchable = rewrite_from.unwrap_or(cache.len());
    let patched = &rebuilt[..rebuilt.partition_point(|&row| row < patchable)];

    row_runs(cache, patched)
        .chain(rewrite_from.map(|first| (row_len(&cache[..first]), first..cache.len())))
}

/// Group `rows` into contiguous runs, pairing each with where the run's
/// instances start in the buffer.
///
/// The per-row passes re-upload only the rows that changed, and rows that sit
/// next to each other occupy one contiguous stretch of the buffer, so a run of
/// them travels as a single write. `rows` must be ascending and free of
/// duplicates, which is how both passes collect it.
///
/// The offset counts instances, not bytes, and is the sum of every row's length
/// before the run. That only locates a run correctly while the rows before it
/// still hold the lengths the buffer was written with, so a caller that rebuilds
/// a row to a different length must stop patching at that row.
pub(crate) fn row_runs<'cache, 'rows, T>(
    cache: &'cache [Vec<T>],
    rows: &'rows [usize],
) -> RowRuns<'cache, 'rows, T> {
    RowRuns {
        cache,
        rows,
        offset: 0,
        walked: 0,
    }
}

/// The iterator [`row_runs`] returns.
pub(crate) struct RowRuns<'cache, 'rows, T> {
    cache: &'cache [Vec<T>],
    rows: &'rows [usize],
    /// Instances counted so far, which is the offset of the next run once the
    /// gap before it is added.
    offset: usize,
    /// The first row not yet counted into [`Self::offset`].
    walked: usize,
}

impl<T> Iterator for RowRuns<'_, '_, T> {
    type Item = (usize, Range<usize>);

    fn next(&mut self) -> Option<(usize, Range<usize>)> {
        let &start = self.rows.first()?;
        self.offset += row_len(&self.cache[self.walked..start]);

        let mut len = 1;
        while self.rows.get(len) == Some(&(start + len)) {
            len += 1;
        }
        self.rows = &self.rows[len..];

        let run = start..start + len;
        let at = self.offset;
        self.offset += row_len(&self.cache[run.clone()]);
        self.walked = run.end;

        Some((at, run))
    }
}

/// The instances `rows` holds in total.
pub(crate) fn row_len<T>(rows: &[Vec<T>]) -> usize {
    rows.iter().map(Vec::len).sum()
}

/// Build into `out` one occluder per panel, in declaration order.
///
/// Every pass that occludes reads the same list off the same panels, so a frame
/// builds it once and lends it around. `out` is cleared first, so the frame's
/// scratch holds only this frame's panels.
pub(crate) fn build_occluders_into(panels: &[Panel], out: &mut Vec<Occluder>) {
    out.clear();
    out.extend(panels.iter().map(panel_occluder));
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
    use super::{
        occlusion_globals, pool_occluders, rotate_row_cache, row_runs, row_uploads, CellMetrics,
        CompositeSlots, MAX_COMPOSITE_POOLS,
    };
    use stoatty_term::grid::{BorderStyle, Panel, PanelShadow, Rgb};

    /// A pool keeps its own slot across frames whose pool set changes shape, which
    /// is what keying by id buys over keying by position in the frame.
    #[test]
    fn a_slot_follows_its_pool_id_not_its_position() {
        let mut slots: CompositeSlots<u32> = CompositeSlots::new();
        for pool in [7, 4, 9] {
            *slots.entry(pool, || 0) = pool * 10;
        }

        // Pool 7 settles and drops out, so 4 and 9 shift down a position.
        let seen: Vec<Option<&u32>> = [4, 9].iter().map(|p| slots.get(*p)).collect();
        assert_eq!(
            seen,
            [Some(&40), Some(&90)],
            "each surviving pool reads back what it built, not its new neighbour's"
        );
        assert_eq!(slots.get(3), None, "a pool never prepared has no slot");
    }

    /// A repeat request hands back that pool's own slot, and consumes none.
    ///
    /// Two pools are needed to catch it. With one, the pool's slot and the most
    /// recently requested slot are the same entry.
    #[test]
    fn requesting_a_held_pool_reuses_its_own_slot() {
        let mut slots: CompositeSlots<u32> = CompositeSlots::new();
        *slots.entry(1, || 0) = 11;
        *slots.entry(2, || 0) = 22;
        *slots.entry(1, || 0) += 100;

        assert_eq!(
            (slots.entries.len(), slots.get(1), slots.get(2)),
            (2, Some(&111), Some(&22)),
            "the repeat request reached pool 1's slot without building or touching another"
        );
    }

    /// Past the cap the least recently requested pool gives up its slot, so a
    /// renderer's buffers stay bounded however many pool ids pass through it.
    #[test]
    fn a_pool_past_the_cap_takes_over_the_least_recently_requested_slot() {
        let mut slots: CompositeSlots<u32> = CompositeSlots::new();
        for pool in 0..MAX_COMPOSITE_POOLS as u32 {
            *slots.entry(pool, || 0) = pool;
        }

        // Touching pool 0 leaves pool 1 as the oldest request.
        slots.entry(0, || 0);
        let overflow = MAX_COMPOSITE_POOLS as u32;
        *slots.entry(overflow, || 0) = overflow;

        assert_eq!(
            (
                slots.entries.len(),
                slots.get(1),
                slots.get(0),
                slots.get(overflow)
            ),
            (MAX_COMPOSITE_POOLS, None, Some(&0), Some(&overflow)),
            "the untouched pool lost its slot, the touched one kept it"
        );
    }

    /// Rows of uneven length, so a run's offset can only come out right by
    /// summing the rows before it rather than multiplying by a fixed width.
    fn uneven_cache() -> Vec<Vec<u8>> {
        vec![
            vec![1, 2, 3],
            vec![4],
            vec![],
            vec![5, 6],
            vec![7, 8, 9, 10],
        ]
    }

    /// Two rows with an untouched row between them must travel as two writes.
    /// One write spanning both would re-send the row in the middle, which is
    /// the whole cost the run walk exists to avoid.
    #[test]
    fn disjoint_rows_upload_separately() {
        let cache = uneven_cache();

        assert_eq!(
            row_runs(&cache, &[1, 3]).collect::<Vec<_>>(),
            vec![(3, 1..2), (4, 3..4)],
            "each row goes out on its own, offset past the rows before it",
        );
    }

    #[test]
    fn adjacent_rows_upload_as_one_run() {
        let cache = uneven_cache();

        assert_eq!(
            row_runs(&cache, &[2, 3, 4]).collect::<Vec<_>>(),
            vec![(4, 2..5)],
            "rows sitting next to each other share one contiguous write",
        );
    }

    #[test]
    fn a_row_list_splits_into_runs_at_every_gap() {
        let cache = uneven_cache();

        assert_eq!(
            row_runs(&cache, &[0, 1, 3, 4]).collect::<Vec<_>>(),
            vec![(0, 0..2), (4, 3..5)],
            "the gap at row 2 splits the list, and the second run skips its \
             length",
        );
        assert!(
            row_runs(&cache, &[]).next().is_none(),
            "no changed row is no write",
        );
    }

    /// Apply an upload plan to a buffer holding `stale` and report what the
    /// buffer ends up with, so a plan that misplaces or omits a write shows up
    /// as bytes that differ from the rows it was meant to deliver.
    fn apply(
        cache: &[Vec<u8>],
        stale: &[u8],
        rebuilt: &[usize],
        rewrite_from: Option<usize>,
    ) -> Vec<u8> {
        let mut buffer = stale.to_vec();
        buffer.resize(cache.iter().map(Vec::len).sum(), 0);

        for (offset, rows) in row_uploads(cache, rebuilt, rewrite_from) {
            let sent: Vec<u8> = cache[rows].iter().flatten().copied().collect();
            buffer[offset..offset + sent.len()].copy_from_slice(&sent);
        }
        buffer
    }

    /// Rows that kept their lengths are patched where they sit, so the buffer
    /// ends up holding the whole cache without the untouched rows being re-sent.
    #[test]
    fn patched_rows_land_where_the_buffer_holds_them() {
        let cache = uneven_cache();
        let flat: Vec<u8> = cache.iter().flatten().copied().collect();

        // Rows 1 and 3 changed content but not length, so the buffer still holds
        // the right bytes everywhere else.
        let stale = vec![1, 2, 3, 0, 0, 0, 7, 8, 9, 10];

        assert_eq!(
            apply(&cache, &stale, &[1, 3], None),
            flat,
            "patching the changed rows in place restores the whole buffer",
        );
    }

    /// A row that changed length moves every later row, so the plan must rewrite
    /// from there rather than patch the rows after it where they used to be.
    #[test]
    fn a_resized_row_rewrites_the_rest_of_the_buffer() {
        let cache = uneven_cache();
        let flat: Vec<u8> = cache.iter().flatten().copied().collect();

        // Row 1 grew from one instance to two before this frame, so every row
        // after it sits one instance earlier in the stale buffer.
        let stale = vec![1, 2, 3, 4, 0, 5, 6, 7, 8, 9];

        assert_eq!(
            apply(&cache, &stale, &[1, 4], Some(1)),
            flat,
            "the rewrite carries every row the resize displaced",
        );
    }

    /// A rebuilt row at or past the rewrite point is already covered by the
    /// span, so patching it individually would write bytes about to be
    /// overwritten at an offset the resize has already invalidated.
    #[test]
    fn rows_past_the_rewrite_point_are_left_to_the_span() {
        let cache = uneven_cache();

        assert_eq!(
            row_uploads(&cache, &[0, 3, 4], Some(3)).collect::<Vec<_>>(),
            vec![(0, 0..1), (4, 3..5)],
            "row 0 is patched, and rows 3 and 4 ride the span rather than two \
             writes of their own",
        );
    }

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

    /// Gliding back through scrollback moves content down the screen, so the
    /// slide has to run the other way and empty the rows it vacates at the top.
    #[test]
    fn rotating_a_cache_downward_moves_rows_the_other_way() {
        let mut cache = vec![vec![10, 11], vec![20], vec![30], vec![40], vec![50]];
        rotate_row_cache(&mut cache, -2, |value| *value += 20);

        assert_eq!(
            cache,
            vec![
                Vec::<i32>::new(),
                Vec::new(),
                vec![30, 31],
                vec![40],
                vec![50]
            ],
            "the rows above the scroll move down repaired, and the rows they \
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
