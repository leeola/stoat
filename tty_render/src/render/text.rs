//! Instanced per-glyph text pass.
//!
//! Draws one quad per visible cell glyph, rasterized through the [`GlyphAtlas`]
//! and composited over the cell background in linear light so thin glyphs on
//! dark backgrounds keep their weight. Characters are shaped one per cell (the
//! grid's model) via cosmic-text and cached; the fragment shader lifts
//! ghostty's linear blend and stem-darkening correction.
//!
//! [`GlyphAtlas`]: crate::atlas::GlyphAtlas

use crate::{
    atlas::{AtlasKind, GlyphAtlas, GlyphInfo},
    render::{
        globals_offset, occlusion_globals, row_len, row_uploads, CellMetrics, CompositeSlot,
        CompositeSlots, Frame, Occluder, GLOBALS_SLOTS, GLOBALS_SLOT_STRIDE,
    },
};
use bytemuck::{Pod, Zeroable};
use cosmic_text::{fontdb::Weight, CacheKey, Family, Font, FontSystem, SwashCache};
use rustc_hash::FxHashMap;
use std::{mem, ops::Range, sync::Arc};
use stoatty_term::{
    grid::{Cell, Grid, Overlay, Rgb, Scale, ScrollRegion, UnderlineStyle},
    term::Damage,
};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBinding, BufferBindingType, BufferDescriptor, BufferSize, BufferUsages,
    ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor, Queue,
    RenderPass, RenderPipeline, RenderPipelineDescriptor, Sampler, SamplerBindingType,
    SamplerDescriptor, ShaderModule, ShaderModuleDescriptor, ShaderSource, ShaderStages,
    TextureFormat, TextureSampleType, TextureView, TextureViewDimension, VertexBufferLayout,
    VertexState, VertexStepMode,
};

mod font;
mod powerline;

pub use font::build_font_system;

/// Instance buffer capacity, in glyphs, allocated up front. Grows by doubling
/// when a frame exceeds it.
const INITIAL_CAPACITY: usize = 2048;

/// Atlas selector packed into each instance, matching the shader's constants.
const KIND_MASK: u32 = 0;
const KIND_COLOR: u32 = 1;

/// The seq stamped on glyphs no box occludes. Grid, region, overlay, and HUD
/// glyphs carry it. It is larger than any panel's seq, so the occlusion loop
/// never discards them, leaving only the off-grid text runs (which carry a real
/// per-run seq) occludable.
const UNOCCLUDED_SEQ: u32 = u32::MAX;

/// Underline style packed into each decoration instance, matching the shader's
/// constants.
const STYLE_STRAIGHT: u32 = 0;
const STYLE_DOUBLE: u32 = 1;
const STYLE_CURLY: u32 = 2;
const STYLE_DOTTED: u32 = 3;
const STYLE_DASHED: u32 = 4;

/// Per-glyph instance: where to draw it, where to sample it, and how to color it.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TextInstance {
    /// Top-left of the glyph bitmap in physical pixels, with the vertical
    /// measured from the top of [`Self::row`] rather than from the surface.
    ///
    /// Split that way so a row that moves up or down the screen leaves every
    /// glyph it holds untouched, only the row index moving with it.
    pos: [f32; 2],
    /// Glyph bitmap size in physical pixels.
    dim: [f32; 2],
    /// Atlas sub-rect as `[u_min, v_min, u_max, v_max]`, normalized.
    uv: [f32; 4],
    /// Foreground color, normalized sRGB. The glyph is emitted premultiplied by
    /// its coverage and alpha-blends over whatever the framebuffer already holds.
    fg: [f32; 3],
    /// Atlas to sample: [`KIND_MASK`] or [`KIND_COLOR`].
    kind: u32,
    /// Declaration-order seq the fragment shader occludes by. Text-run glyphs
    /// carry their run's seq, and every other glyph carries [`UNOCCLUDED_SEQ`].
    seq: u32,
    /// Grid row [`Self::pos`] is measured from, which the vertex stage scales by
    /// the cell height to recover the surface position.
    ///
    /// Zero for the draws that are not built per row, being the overlays, text
    /// runs and readouts, whose positions are already absolute and which the
    /// same arithmetic then leaves alone.
    row: u32,
}

/// Per-underlined-cell decoration instance.
///
/// One quad per underlined cell, covering the whole cell; the fragment paints
/// only the underline shape selected by `style`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct UnderlineInstance {
    /// Top-left of the cell in physical pixels, with the vertical measured from
    /// the top of [`Self::row`] rather than from the surface.
    cell_pos: [f32; 2],
    /// Underline color, normalized sRGB.
    color: [f32; 3],
    /// Underline shape: one of the `STYLE_*` constants.
    style: u32,
    /// Grid row [`Self::cell_pos`] is measured from, as on [`TextInstance`].
    row: u32,
}

/// One opaque background rect behind a scaled text run's glyphs. It masks
/// whatever it sits over (a panel hairline) across the run's full span, spaces
/// included, where the per-glyph draw would leave gaps.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct RectInstance {
    /// Top-left of the rect in physical pixels.
    pos: [f32; 2],
    /// Rect size in physical pixels.
    dim: [f32; 2],
    /// Fill color, normalized sRGB.
    color: [f32; 3],
    /// Declaration-order seq the fragment shader occludes by, taken from the run
    /// this rect backs.
    seq: u32,
}

/// Uniform shared by every instance: the surface resolution the vertex shader
/// maps pixel coordinates through, and the cell box the underline pass draws in.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct TextGlobals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    /// Edge lengths of the mask and color atlases, in texels, indexed by
    /// [`AtlasKind`]'s own numbering.
    ///
    /// An instance carries its glyph's rectangle in texels, so that it survives
    /// the atlas doubling under it, and the vertex stage divides by the entry
    /// its glyph's kind selects. A pair indexed by the kind rather than a
    /// branch on it, because which size belongs to which kind is only visible
    /// on a frame where the two atlases differ in size.
    atlas_size: [f32; 2],
    /// Vertical scroll offset in pixels, added to each glyph's Y in the vertex
    /// shader so a scroll-only frame rewrites this uniform instead of rebuilding
    /// the glyph instances. Differs per draw: grid scroll for plain glyphs,
    /// region scroll for region glyphs, zero for screen-anchored runs/overlays.
    scroll_y: f32,
    /// Panel-occluder count the fragment shader loops over. Non-zero in the
    /// static globals bound for the live text-run draws and in the composite
    /// globals of an occludable pool. Zero for grid, region, and overlay draws,
    /// which skip the loop entirely.
    panel_count: u32,
    /// 1 makes the shader discard a fragment inside any occluder regardless of
    /// seq, set only for a pool composite that sits under every box; 0 keeps
    /// the seq test.
    occlude_all: u32,
    /// How far the instance buffers are rotated, in rows.
    ///
    /// An instance carries the slot it occupies rather than the display row it
    /// paints, and the vertex stage recovers the row from the two. Rotating by
    /// an integer is what lets a scroll leave the rows it kept alone.
    row_offset: u32,
    /// Grid height the rotation wraps at, so the vertex stage can take the
    /// slot back to a row.
    ///
    /// Zero marks a draw that is not rotated at all. A screen-anchored draw's
    /// rows are positions its builder chose rather than grid rows, and an
    /// overlay taller than the screen names rows past the bottom on purpose,
    /// which a wrap would fold back over the box.
    rows: u32,
    _pad0: u32,
    /// Cell the grid's own (0, 0) is drawn at, which the vertex stage scales to
    /// pixels and adds to every glyph position.
    ///
    /// A pool composite hands over a grid sized to its region rather than to
    /// the viewport, so the region's origin is what puts its glyphs on the
    /// screen. Zero for every other draw, whose positions already start at the
    /// screen's own origin.
    origin_cells: [f32; 2],
    /// Pads the struct to the 64-byte (16-aligned) size a uniform requires.
    _pad1: [u32; 2],
}

/// How far one set of instances is rotated in the buffer holding it.
///
/// An instance stores the slot its row occupies rather than the row it paints,
/// and the vertex stage inverts that. The two only agree when the rotation the
/// builder used is the one written into the globals the draw binds, so it
/// travels with the build instead of being read back off the pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RowRotation {
    offset: u32,
    /// Height the rotation wraps at, or zero for an unrotated draw. Mirrors
    /// [`TextGlobals::rows`], which is what makes the two halves comparable.
    rows: usize,
}

impl RowRotation {
    /// The rotation a screen-anchored draw builds under, where a slot is the
    /// row itself and nothing wraps.
    fn unrotated() -> Self {
        RowRotation { offset: 0, rows: 0 }
    }

    fn slot(self, row: usize) -> usize {
        row_slot(row, self.offset, self.rows)
    }
}

/// One composited pool's four instance buffers and the atlas state they resolved
/// against.
///
/// A pool draws in four parts, so a slot carries a buffer for each rather than the
/// single one the other passes need. The epoch belongs here too, because the reuse
/// it guards is per pool. One pool's prepare can evict a glyph another pool
/// already froze the texels of.
struct TextCompositeSlot {
    glyphs: CompositeSlot,
    underlines: CompositeSlot,
    /// Drawn with the pool-shift globals rather than the screen-anchored ones, so
    /// the runs glide with the page.
    text_runs: CompositeSlot,
    rects: CompositeSlot,
    /// The atlas content-epoch these instances resolved against. A shift-only
    /// composite reuses them only while the atlas still matches. An eviction
    /// since means a glyph moved, so the pool must be reshaped even though its
    /// grid content held steady. A grow leaves the texels alone.
    epoch: u64,
    /// What each row shaped to, kept so a scrolled frame can slide them and
    /// re-shape only the rows the scroll exposed.
    ///
    /// Shaping a row rasterizes each of its glyphs and inserts them into the
    /// shared atlas, which is the cost worth carrying. An eased scroll moves the
    /// top a row or three per frame, so nearly every row it composes was shaped
    /// for the frame before.
    ///
    /// Empty until the first full build, and emptied again whenever the row
    /// count changes, which is what makes a stale length mean "rebuild".
    ///
    /// The built instances are not kept beside these. Every one of them takes its
    /// position from the glyph's own row and column, so repairing the row a
    /// rotation moved is enough, and rebuilding the instances from the glyphs
    /// costs a walk rather than a reshape.
    glyph_rows: Vec<Vec<PendingGlyph>>,
    underline_rows: Vec<Vec<UnderlineInstance>>,
}

/// The instanced glyph pipeline together with the font system, glyph atlas, and
/// per-frame buffers it draws [`stoatty_term`]'s cell glyphs from.
///
/// Owns the cosmic-text [`FontSystem`]/[`SwashCache`] and the [`GlyphAtlas`]
/// because shaping, rasterization, and packing all happen inside
/// [`Self::prepare`].
pub struct TextPass {
    pipeline: RenderPipeline,
    /// Globals carrying the grid scroll offset; bound for the plain glyph and
    /// underline draws. [`Self::region_globals`] and [`Self::static_globals`]
    /// hold the same resolution and cell size but a different `scroll_y`, so
    /// each draw scrolls correctly without rewriting one buffer mid-pass.
    globals: Buffer,
    globals_bind_group: BindGroup,
    /// Globals carrying the scroll-region offset; bound for the region glyph draw.
    region_globals: Buffer,
    region_globals_bind_group: BindGroup,
    /// Globals carrying zero scroll; bound for the screen-anchored text-run and
    /// overlay-content draws, which must not move with the grid.
    static_globals: Buffer,
    static_globals_bind_group: BindGroup,
    /// The value last written to each of the three globals buffers, so a frame
    /// that moved none of them skips all three writes.
    ///
    /// One cache per buffer, since the three hold different `scroll_y` and panel
    /// counts. A shared cache would report a sibling's value as already written and
    /// leave a buffer stale.
    last_globals: Option<TextGlobals>,
    last_region_globals: Option<TextGlobals>,
    last_static_globals: Option<TextGlobals>,
    /// How far the instance buffers are rotated, in rows.
    ///
    /// Held at zero, where the slot map is the identity and every instance
    /// sits at its own display row. The rotation exists on both sides of the
    /// boundary so the arithmetic can be pinned, and starts moving when the
    /// row-indexed caches become slot-indexed and a scroll advances it instead
    /// of sliding them.
    row_offset: u32,
    /// Height of the live grid the row-indexed caches are sized to, which is
    /// what the rotation wraps at. Held here because the rows are rebuilt from
    /// the caches on paths that have no grid to ask.
    grid_rows: usize,
    /// The scroll region's rectangle as the cached instances were split
    /// against, without its offset.
    ///
    /// Which buffer a cell lands in follows from the rectangle, and the region
    /// buffer is drawn with the region's scroll applied. So a cell left on the
    /// wrong side of a moved rectangle is drawn at the wrong height, and the
    /// split has to be redone even on a frame that damaged no row.
    ///
    /// The offset is deliberately not part of this. It reaches the shader
    /// through the globals uniform rather than through the split, so a region
    /// that only scrolled keeps the instances it had.
    last_region_rect: Option<(u16, u16, u16, u16)>,
    /// The group-0 layout the three globals bind groups share, kept so they can
    /// be rebuilt when [`Self::occluders`] reallocates.
    globals_layout: BindGroupLayout,
    /// One occluder per live panel at binding 1, shared by all three globals
    /// bind groups. Read by the run-glyph and run-rect fragment shaders to
    /// discard run fragments a later box covers.
    occluders: Buffer,
    /// The occluder list last written to [`Self::occluders`], so a frame whose
    /// panels have not moved skips the upload. Panels change on layout events, not
    /// per frame, so most frames match.
    last_occluders: Vec<Occluder>,
    occluder_capacity: usize,
    atlas_layout: BindGroupLayout,
    sampler: Sampler,
    atlas_bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    count: u32,
    /// The composited pools' instances, one slot per pool.
    ///
    /// Held per pool so every pool can be prepared before any of them draws, and
    /// separate from the live buffers so a pool draw leaves the live grid's
    /// damage-tracked rows intact. Indexed by the pool's position in the frame's
    /// slice and grown on demand, so a renderer that never composites holds none.
    composite_slots: CompositeSlots<TextCompositeSlot>,
    /// The [`Grid::text_runs_epoch`] and [`Atlas::content_epoch`] the live text
    /// runs and rects were last built against. `prepare` reuses those instances
    /// while both still match, since the runs are unchanged and their atlas UVs
    /// have not moved.
    last_text_run_epoch: u64,
    last_atlas_epoch: u64,
    /// The [`Atlas::content_epoch`] the cached grid-glyph instances were last
    /// built against. An eviction reuses a slot without moving the texture size,
    /// and one can come from any earlier pass, such as a pool composite between
    /// frames. Persisting the epoch heals those cached rows on the next prepare.
    ///
    /// This records the epoch the *settled* atlas reached, after any in-frame
    /// re-resolve. A build that moved the atlas itself is healed within the same
    /// prepare, so this value never has to signal a rebuild the next frame owes.
    grid_atlas_epoch: u64,
    overlay_instances: Buffer,
    overlay_capacity: usize,
    overlay_count: u32,
    /// One scissored sub-range of [`Self::overlay_instances`] per overlay, in
    /// overlay order, so each popover's content is clipped to its own box and
    /// several can scroll independently.
    overlay_draws: Vec<OverlayDraw>,
    /// The shaped overlay glyphs per overlay, reused while
    /// [`Self::last_popovers_epoch`] holds so an unchanged frame skips the
    /// content walk and shape-cache lookups. The glyphs inside the box's current
    /// window are re-touched against the atlas every frame to keep them resident,
    /// since the packing phase evicts anything not marked in use this frame.
    overlay_pending: Vec<OverlayContent>,
    /// The content-line window each overlay's bases were built for. A base holds only
    /// the window's glyphs, so a scroll that moves the window past a line boundary has
    /// to rebuild while one within a line does not.
    overlay_windows: Vec<Range<usize>>,
    /// Reused buffers for the content walk in [`Self::rasterize_overlays`], which runs
    /// per popovers epoch over every line of every overlay.
    overlay_cell_scratch: Vec<(usize, usize, char)>,
    overlay_cell_line_scratch: Vec<u32>,
    /// The overlay instances before the per-frame anchor and scroll shift,
    /// embedding the atlas UVs. Reused while [`Self::last_popovers_epoch`] and
    /// [`Self::last_overlay_atlas_epoch`] both hold, so a scroll-only frame
    /// re-shifts these instead of rebuilding them.
    overlay_bases: Vec<Vec<TextInstance>>,
    /// The scissor box per overlay, cached alongside [`Self::overlay_bases`]
    /// since it depends only on the overlay geometry, not the scroll offset.
    overlay_base_scissors: Vec<Option<[u32; 4]>>,
    /// The [`Grid::popovers_epoch`] the pending groups and bases were built
    /// against. A change means overlay content or geometry moved, so both are
    /// rebuilt.
    last_popovers_epoch: u64,
    /// The [`Atlas::content_epoch`] [`Self::overlay_bases`] were built against.
    /// A change means an eviction moved a glyph, so the bases are rebuilt even
    /// when the content held.
    last_overlay_atlas_epoch: u64,
    /// The popover scroll offsets the uploaded overlay instances carry. While
    /// the bases are reused and these still match, the shift and upload are
    /// skipped entirely.
    last_popover_scrolls: Vec<f32>,
    region_instances: Buffer,
    region_capacity: usize,
    region_count: u32,
    /// Pixel rect `[x, y, w, h]` the scroll-region glyph draw scissors to, so
    /// the region's scrolled content is clipped to its rectangle. `None` when no
    /// scroll region is declared.
    region_scissor: Option<[u32; 4]>,
    text_run_instances: Buffer,
    text_run_capacity: usize,
    text_run_count: u32,
    /// Screen-anchored glyphs for the perf HUD readout, laid out in pixel space
    /// and drawn topmost with the text-run pipeline.
    #[cfg(feature = "perf")]
    hud_instances: Buffer,
    #[cfg(feature = "perf")]
    hud_capacity: usize,
    #[cfg(feature = "perf")]
    hud_count: u32,
    underline_pipeline: RenderPipeline,
    underline_instances: Buffer,
    underline_capacity: usize,
    underline_count: u32,
    /// One opaque background rect per scaled text run, drawn before the run's
    /// glyphs so they alpha-blend over it. Grid cells take their background from
    /// the background pass instead, so these cover only the off-grid runs.
    rect_pipeline: RenderPipeline,
    rect_instances: Buffer,
    rect_capacity: usize,
    rect_count: u32,
    atlas: GlyphAtlas,
    font_system: FontSystem,
    /// The resolved primary shaping family, being the first configured `font_family`
    /// entry present in the font db, or `None` to shape with the generic monospace
    /// fallback.
    ///
    /// Shared rather than owned because the shaping paths need the name while holding
    /// a mutable borrow of this pass for the font system, so they take a copy of it
    /// every frame and once per composited pool.
    family: Option<Arc<str>>,
    /// The face [`Self::family`] resolves to, looked up once at construction and
    /// reused so a frame's per-cell coverage test is a charmap lookup, not a
    /// font-database query. `None` when no family resolves, so coverage falls
    /// through to the fallback font. Fixed for the pass's lifetime, as the
    /// family is.
    primary_font: Option<Arc<Font>>,
    /// Whether adjacent same-style cells shape together so the font's ligatures
    /// form across cells. When false, every cell is shaped on its own.
    ligatures: bool,
    swash_cache: SwashCache,
    /// Keyed by the scale's bit pattern, so a fractional text-run scale caches
    /// alongside the integer cell scales.
    shape_cache: FxHashMap<(char, u32, u16), Option<CacheKey>>,
    /// Shaped glyphs of each ligature run, keyed by the run text, so a repainted
    /// row reuses them instead of rebuilding a cosmic-text buffer and reshaping.
    ///
    /// Keyed on text alone because runs only group same-scale primary-covered
    /// cells in the constant primary family, matching [`Self::shape_cache`]'s
    /// family-blind invariant. Flushed whole past [`RUN_SHAPE_CACHE_CAP`].
    run_shape_cache: font::RunShapeCache,
    /// The shaped glyphs of each grid row from the previous frame, indexed by
    /// row, so an unchanged row reuses them instead of re-shaping. Rebuilt for
    /// damaged rows, the cursor's old and new rows, and (wholesale) on resize or
    /// when scaled cells are present. Holds [`CacheKey`]s, not atlas rects, so it
    /// survives atlas growth.
    glyph_row_cache: Vec<Vec<PendingGlyph>>,
    /// The built plain-glyph instances of each row from the previous frame, so a
    /// damaged frame rebuilds and re-uploads only the rows that changed rather
    /// than every glyph on screen. Holds resolved atlas rects in texels, which a
    /// grow leaves alone, so only an eviction rebuilds all rows.
    ///
    /// A row's glyphs inside an active scroll region go to
    /// [`Self::region_row_instances`] instead, since the two are drawn from
    /// separate buffers under separate globals.
    plain_row_instances: Vec<Vec<TextInstance>>,
    /// The region-side half of [`Self::plain_row_instances`], holding each row's
    /// glyphs that fall inside the active scroll region.
    ///
    /// Empty for every row while no region is declared. A region is active for as
    /// long as a full-screen program is on screen, so its rows are patched the same
    /// way rather than rebuilt whole.
    region_row_instances: Vec<Vec<TextInstance>>,
    /// The scroll region's rectangle as the row caches were split against, so a
    /// moved rectangle rebuilds them.
    ///
    /// The rectangle rather than the whole [`ScrollRegion`], because its `offset`
    /// moves on every scroll tick and rides the globals uniform. It shifts where the
    /// region's rows are drawn without changing which cells fall inside it.
    region_split: Option<[u16; 4]>,
    /// The built underline instances of each row from the previous frame, so a
    /// damaged frame rebuilds and re-uploads only the changed rows. Underline is
    /// a VT cell attribute, so VT damage tracks it; scroll rides the globals
    /// uniform, so this survives a scroll-only frame.
    underline_row_instances: Vec<Vec<UnderlineInstance>>,
    /// The rows whose underlines rebuild this frame, ascending, which is both what
    /// gets rebuilt and what gets patched into the buffer. Held across frames so a
    /// damaged frame allocates no temporary.
    underline_rows_to_build: Vec<usize>,
    /// The row indices rebuilt this frame, reused across frames so
    /// `rasterize_visible` allocates no per-frame temporary.
    rebuilt_scratch: Vec<usize>,
    /// The same rows as slots, ascending, which is the order the upload plan
    /// needs them in. Reused for the same reason.
    patched_slots_scratch: Vec<usize>,
    /// Scratch reused each frame to flatten the row instances into a contiguous
    /// upload slice, so `patch_rows` allocates no per-frame temporary. Shared by
    /// both row buffers, which upload one after the other.
    plain_upload_scratch: Vec<TextInstance>,
    /// Scratch reused each frame for the underline upload slice, shared by
    /// `prepare_underlines` and `prepare_composite`.
    underline_upload_scratch: Vec<UnderlineInstance>,
    /// Scratch reused to partition one row's cached glyphs into the scroll-region
    /// and plain halves, so a region frame allocates no per-row temporary.
    plain_pending_scratch: Vec<PendingGlyph>,
    /// Region-side half of the [`Self::plain_pending_scratch`] split.
    region_pending_scratch: Vec<PendingGlyph>,
    /// Scratch reused across the frames that rebuild the off-grid text runs,
    /// which a chrome change or an atlas move triggers and which builds twice
    /// when packing a run glyph bounces the atlas.
    text_run_build_scratch: Vec<TextInstance>,
    /// Scratch for the runs' background rects, rebuilt alongside
    /// [`Self::text_run_build_scratch`].
    run_rect_build_scratch: Vec<RectInstance>,
    /// Scratch reused across the frames that re-anchor the overlay glyphs, which
    /// an autoscrolling popover triggers on every frame it moves.
    overlay_instance_scratch: Vec<TextInstance>,
    /// Scratch for the draw ranges built beside
    /// [`Self::overlay_instance_scratch`], swapped with
    /// [`Self::overlay_draws`] once filled.
    overlay_draw_scratch: Vec<OverlayDraw>,
    /// Scratch reused across `prepare_composite` calls for a pool's shaped glyphs,
    /// its glyph instances, its text-run instances, and its run rects, so a pool
    /// re-composite allocates no per-frame temporary. Dedicated to the composite
    /// path rather than shared with the live scratch above.
    composite_pending_scratch: Vec<PendingGlyph>,
    composite_upload_scratch: Vec<TextInstance>,
    composite_run_scratch: Vec<TextInstance>,
    composite_rect_scratch: Vec<RectInstance>,
    /// Scratch reused across `rasterize_row`'s ligature runs for a run's cells,
    /// its shaping string, and its per-byte column map, so a run that hits the
    /// shape cache allocates no per-frame temporary.
    run_scratch: Vec<(usize, char)>,
    run_text_scratch: String,
    run_cols_scratch: Vec<usize>,
    /// The grid width [`Self::glyph_row_cache`] was built at; a change invalidates
    /// every cached row since columns shift.
    glyph_cache_cols: usize,
    /// Lowest slot this frame's scroll emptied, or `None` when it emptied
    /// none.
    ///
    /// An emptied slot is shorter than the buffer was written with, which moves
    /// every slot packed after it, so each buffer rewrites from here down. The
    /// slots a scroll exposes are contiguous in display rows and so contiguous
    /// in slot space too, save for the wrap, which lands this at zero and
    /// rewrites the lot.
    exposed_from: Option<usize>,
    /// The cursor cell at the previous frame, so a move can re-shape the row it
    /// left and the row it entered (the cursor breaks ligatures on its cell).
    last_cursor_cell: Option<(usize, usize)>,
    baseline: f32,
    metrics: CellMetrics,
}

impl TextPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    ///
    /// Takes a ready `font_system` (see [`build_font_system`]), resolves
    /// `font_family` against it to pick the shaping primary, and creates the
    /// glyph atlas. `format` must be the non-sRGB surface format the text pass
    /// composites into; the shader does its own sRGB encoding.
    pub(crate) fn new(
        device: &Device,
        format: TextureFormat,
        metrics: CellMetrics,
        mut font_system: FontSystem,
        font_family: &[String],
        ligatures: bool,
    ) -> TextPass {
        let family = font::resolve_primary_family(&font_system, font_family);
        let baseline = font::probe_baseline(
            &mut font_system,
            metrics,
            font::shape_family(family.as_deref()),
        );
        let primary_font = font::resolve_primary_font(&mut font_system, family.as_deref());
        let swash_cache = SwashCache::new();
        let atlas = GlyphAtlas::new(device);

        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("text"),
            source: ShaderSource::Wgsl(
                crate::render::with_occlusion(include_str!("../shaders/text.wgsl")).into(),
            ),
        });

        let globals_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("text globals"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    // The underline pipeline shares this layout and reads
                    // globals.cell_size in its fragment to place the underline, so
                    // globals must be visible to the fragment stage.
                    visibility: ShaderStages::VERTEX_FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        // One slot per composited pool plus the live grid's, so
                        // every pool's globals coexist and a draw selects its own.
                        has_dynamic_offset: true,
                        min_binding_size: None,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
        });

        let atlas_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("text atlas"),
            entries: &[
                texture_entry(0),
                texture_entry(1),
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("text"),
            bind_group_layouts: &[Some(&globals_layout), Some(&atlas_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("text"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<TextInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x4,
                        3 => Float32x3,
                        4 => Uint32,
                        5 => Uint32,
                        6 => Uint32,
                    ],
                }],
            },
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: Some(BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        let underline_pipeline = build_underline_pipeline(device, &shader, &globals_layout, format);
        let rect_pipeline = build_rect_pipeline(device, &shader, &globals_layout, format);

        // The three globals buffers and the run-rect and glyph pipelines all
        // read the same panel occluders at binding 1, so it is allocated before
        // their bind groups.
        let occluders = alloc_occluders(device, INITIAL_CAPACITY);

        // Three globals buffers share one layout but carry a different scroll_y,
        // so the plain, region, and screen-anchored draws each scroll correctly
        // within a single render pass.
        // Only the plain buffer carries per-pool slots. The region and
        // screen-anchored draws have no composite path, so they hold slot 0 alone.
        let (globals, globals_bind_group) = make_globals(
            device,
            &globals_layout,
            &occluders,
            "text globals",
            GLOBALS_SLOTS,
        );
        let (region_globals, region_globals_bind_group) = make_globals(
            device,
            &globals_layout,
            &occluders,
            "text region globals",
            1,
        );
        let (static_globals, static_globals_bind_group) = make_globals(
            device,
            &globals_layout,
            &occluders,
            "text static globals",
            1,
        );

        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("text atlas"),
            ..Default::default()
        });

        let atlas_bind_group = create_atlas_bind_group(
            device,
            &atlas_layout,
            &sampler,
            atlas.mask_view(),
            atlas.color_view(),
        );

        let instances = alloc_instances(
            device,
            "text instances",
            instance_bytes::<TextInstance>(INITIAL_CAPACITY),
        );
        let overlay_instances = alloc_instances(
            device,
            "overlay text instances",
            instance_bytes::<TextInstance>(INITIAL_CAPACITY),
        );
        let region_instances = alloc_instances(
            device,
            "scroll region text instances",
            instance_bytes::<TextInstance>(INITIAL_CAPACITY),
        );
        let text_run_instances = alloc_instances(
            device,
            "text run instances",
            instance_bytes::<TextInstance>(INITIAL_CAPACITY),
        );
        let rect_instances = alloc_instances(
            device,
            "text run rect instances",
            instance_bytes::<RectInstance>(INITIAL_CAPACITY),
        );
        #[cfg(feature = "perf")]
        let hud_instances = alloc_instances(
            device,
            "hud text instances",
            instance_bytes::<TextInstance>(INITIAL_CAPACITY),
        );
        let underline_instances = alloc_instances(
            device,
            "underline instances",
            instance_bytes::<UnderlineInstance>(INITIAL_CAPACITY),
        );

        TextPass {
            pipeline,
            globals,
            globals_bind_group,
            region_globals,
            region_globals_bind_group,
            static_globals,
            static_globals_bind_group,
            last_globals: None,
            last_region_globals: None,
            last_static_globals: None,
            row_offset: 0,
            grid_rows: 0,
            last_region_rect: None,
            globals_layout,
            occluders,
            last_occluders: Vec::new(),
            occluder_capacity: INITIAL_CAPACITY,
            atlas_layout,
            sampler,
            atlas_bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            composite_slots: CompositeSlots::new(),
            last_text_run_epoch: 0,
            last_atlas_epoch: 0,
            // Never a real content epoch, so the first prepare always builds.
            grid_atlas_epoch: u64::MAX,
            overlay_instances,
            overlay_capacity: INITIAL_CAPACITY,
            overlay_count: 0,
            overlay_draws: Vec::new(),
            overlay_pending: Vec::new(),
            overlay_windows: Vec::new(),
            overlay_cell_scratch: Vec::new(),
            overlay_cell_line_scratch: Vec::new(),
            overlay_bases: Vec::new(),
            overlay_base_scissors: Vec::new(),
            last_popovers_epoch: 0,
            last_overlay_atlas_epoch: 0,
            last_popover_scrolls: Vec::new(),
            region_instances,
            region_capacity: INITIAL_CAPACITY,
            region_count: 0,
            region_scissor: None,
            text_run_instances,
            text_run_capacity: INITIAL_CAPACITY,
            text_run_count: 0,
            #[cfg(feature = "perf")]
            hud_instances,
            #[cfg(feature = "perf")]
            hud_capacity: INITIAL_CAPACITY,
            #[cfg(feature = "perf")]
            hud_count: 0,
            underline_pipeline,
            underline_instances,
            underline_capacity: INITIAL_CAPACITY,
            underline_count: 0,
            rect_pipeline,
            rect_instances,
            rect_capacity: INITIAL_CAPACITY,
            rect_count: 0,
            atlas,
            font_system,
            family,
            primary_font,
            ligatures,
            swash_cache,
            shape_cache: FxHashMap::default(),
            run_shape_cache: font::RunShapeCache::default(),
            glyph_row_cache: Vec::new(),
            exposed_from: None,
            plain_row_instances: Vec::new(),
            region_row_instances: Vec::new(),
            region_split: None,
            underline_row_instances: Vec::new(),
            underline_rows_to_build: Vec::new(),
            rebuilt_scratch: Vec::new(),
            patched_slots_scratch: Vec::new(),
            plain_upload_scratch: Vec::new(),
            underline_upload_scratch: Vec::new(),
            plain_pending_scratch: Vec::new(),
            region_pending_scratch: Vec::new(),
            text_run_build_scratch: Vec::new(),
            run_rect_build_scratch: Vec::new(),
            overlay_instance_scratch: Vec::new(),
            overlay_draw_scratch: Vec::new(),
            composite_pending_scratch: Vec::new(),
            composite_upload_scratch: Vec::new(),
            composite_run_scratch: Vec::new(),
            composite_rect_scratch: Vec::new(),
            run_scratch: Vec::new(),
            run_text_scratch: String::new(),
            run_cols_scratch: Vec::new(),
            glyph_cache_cols: 0,
            last_cursor_cell: None,
            baseline,
            metrics,
        }
    }

    /// Re-derive the text pass for `metrics` so the next frame shapes and
    /// rasterizes glyphs at the new size.
    ///
    /// Re-probes the baseline at the new size and clears the shape cache, whose
    /// keys encode the old rasterization size and would otherwise keep glyphs at
    /// the old size.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
        self.baseline = font::probe_baseline(
            &mut self.font_system,
            metrics,
            font::shape_family(self.family.as_deref()),
        );
        self.shape_cache.clear();
        self.run_shape_cache.clear();
    }

    /// Upload the panel occluders, reallocating the buffer and rebuilding all
    /// three globals bind groups when the panel count outgrows the current
    /// capacity.
    ///
    /// A list matching the one already in the buffer is not re-sent. Panels move on
    /// layout events rather than per frame, so most frames land here, including the
    /// idle ones a blinking cursor drives.
    fn upload_occluders(&mut self, device: &Device, queue: &Queue, occluders: &[Occluder]) {
        if !crate::render::upload_needed(occluders, &self.last_occluders) {
            return;
        }

        if occluders.len() > self.occluder_capacity {
            self.occluder_capacity = occluders.len().next_power_of_two();
            self.occluders = alloc_occluders(device, self.occluder_capacity);
            self.globals_bind_group = make_globals_bind_group(
                device,
                &self.globals_layout,
                &self.globals,
                &self.occluders,
                "text globals",
            );
            self.region_globals_bind_group = make_globals_bind_group(
                device,
                &self.globals_layout,
                &self.region_globals,
                &self.occluders,
                "text region globals",
            );
            self.static_globals_bind_group = make_globals_bind_group(
                device,
                &self.globals_layout,
                &self.static_globals,
                &self.occluders,
                "text static globals",
            );
        }
        if !occluders.is_empty() {
            queue.write_buffer(&self.occluders, 0, bytemuck::cast_slice(occluders));
        }

        self.last_occluders.clear();
        self.last_occluders.extend_from_slice(occluders);
    }

    /// The glyph atlas content epoch, which changes when an eviction moves a
    /// glyph.
    ///
    /// A caller that draws instances built against one epoch and then packs more
    /// glyphs can compare this before and after to tell whether the earlier
    /// instances now hold stale UVs.
    pub(crate) fn content_epoch(&self) -> u64 {
        self.atlas.content_epoch()
    }

    /// Shape, rasterize, and upload the frame's glyph instances for `grid`.
    ///
    /// `resolution` is the surface size in physical pixels. `scroll.popovers`
    /// holds one offset per overlay, in overlay order, each shifting that
    /// overlay's content up by that many rows and clipped to its own box by the
    /// scissor [`Self::draw_overlay_text`] applies; a missing entry is treated as
    /// zero.
    ///
    /// `scroll.grid` offsets the glyphs and underlines down by that many rows,
    /// the same offset the background and decoration passes apply, so the grid
    /// scrolls as one; the screen-anchored overlay content is left unmoved. The
    /// cells inside the grid's scroll region are excluded and instead offset by
    /// `scroll.region`, clipped to the region by the scissor
    /// [`Self::draw_region_text`] applies, so the region scrolls independently.
    ///
    /// `occluders` are the live panels' rects, built once per frame and shared
    /// with the other passes that occlude.
    ///
    /// Runs in two phases: every visible glyph is rasterized first (which may
    /// grow the atlas), then each glyph's atlas sub-rect is read once the atlas
    /// has reached its final size, so normalized coordinates stay valid.
    /// Reallocates the instance buffer only when the glyph count outgrows the
    /// current capacity.
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        resolution: [f32; 2],
        frame: &Frame<'_>,
        occluders: &[Occluder],
    ) {
        let cursor = frame.cursor;
        let scroll = frame.scroll;
        let damage = frame.damage;
        let decoration_damage = frame.decoration_damage;

        self.carry_caches_across_scroll(grid.rows(), frame.scrolled_rows);

        // Upload one occluder per live panel, shared by the three globals bind
        // groups. Only the static globals carry a non-zero panel count, so only
        // the screen-anchored text-run draws occlude against these.
        self.upload_occluders(device, queue, occluders);
        let panel_count = occluders.len() as u32;

        // Each globals buffer carries its own scroll. The plain glyphs take the grid
        // scroll, the region glyphs the region scroll, and the screen-anchored runs
        // and overlays none. Each buffer is sent only when its value moved, so a
        // scroll-only frame refreshes just the uniforms it changed without
        // rebuilding instances, and an idle frame writes none of them.
        let cell_size = [self.metrics.width, self.metrics.height];
        // Live draws never bypass the seq test, so occlude_all stays zero. Only
        // the static globals' text-run draws occlude, and they do it by seq.
        // The rotation rides the buffer its instances are drawn against. The
        // grid and region draws carry grid rows and rotate with them. The
        // screen-anchored buffer carries rows its builders chose, including an
        // overlay's past the bottom of the screen, so it never wraps.
        let grid_rotation = self.grid_rotation();
        let grid_scroll_y =
            (scroll.grid + scroll.document + scroll.scrollback) * self.metrics.height;
        let region_scroll_y = scroll.region * self.metrics.height;

        // Underlines are built first, before the glyph path can return early on
        // an all-blank grid: an underlined space has no glyph but still draws.
        self.prepare_underlines(device, queue, grid, damage);

        self.atlas.begin_frame();
        let atlas_dims = self.atlas.texture_dims();
        let rebuilt = self.rasterize_visible(
            device,
            queue,
            grid,
            cursor_cell(cursor),
            damage,
            decoration_damage,
        );
        // Overlay glyphs are shaped and packed here, in the same packing phase
        // as the grid and run glyphs. While the popovers epoch holds, the shaped
        // groups are reused, but the glyphs are still re-touched against the
        // atlas. The touch both stamps one as this frame's and moves it back to
        // the head of the eviction order. The run packing below and the next
        // frame's grid packing evict what neither protects, which would
        // otherwise move a reused overlay's UV out from under it.
        //
        // Only the box's current window is touched, matching the bases built
        // below, which hold that same window. A glyph scrolled out of the box can
        // therefore be evicted, which is what the bases' atlas-epoch check covers.
        // An eviction bumps the epoch, so the window is rebuilt against the atlas
        // before it is drawn from again.
        let scroll_at = |index: usize| scroll.popovers.get(index).copied().unwrap_or(0.0);
        let popovers_epoch = grid.popovers_epoch();
        let pending_reused = popovers_epoch == self.last_popovers_epoch
            && self.overlay_pending.len() == grid.overlays().len();
        if pending_reused {
            let pending = mem::take(&mut self.overlay_pending);
            for (index, (overlay, content)) in grid.overlays().iter().zip(&pending).enumerate() {
                let window = overlay_window(overlay, scroll_at(index), content);
                for glyph in content.window(window) {
                    self.resolve_glyph(device, queue, glyph.source);
                }
            }
            self.overlay_pending = pending;
        } else {
            self.overlay_pending = self.rasterize_overlays(device, queue, grid);
            self.last_popovers_epoch = popovers_epoch;
        }

        // Off-grid text runs are screen-anchored. No grid or region scroll
        // offset is applied, so they sit at their declared position. They pack
        // here, before the grid-instance build below, so an eviction packing
        // them causes bumps the content epoch that build reads. The run build
        // packs and emits in one pass, so an eviction midway through it would
        // leave the earlier instances naming texels another glyph now holds. It
        // re-resolves once when that happens (below).
        //
        // The chrome the runs back changes rarely, so reuse the instances built
        // on an earlier frame while the run content and their atlas texels both
        // held. Such a frame skips the build and upload and keeps the counts.
        let text_runs_epoch = grid.text_runs_epoch();
        let runs_rebuilt = text_runs_epoch != self.last_text_run_epoch
            || self.atlas.content_epoch() != self.last_atlas_epoch;
        if runs_rebuilt {
            let epoch_at_build = self.atlas.content_epoch();
            let mut instances = mem::take(&mut self.text_run_build_scratch);
            self.build_text_run_instances_into(device, queue, grid, &mut instances);
            // Packing a run glyph can move the UVs the instances already emitted
            // this pass froze. An eviction reuses a slot without resizing the
            // texture, so only the content epoch reveals it. Every run glyph is
            // resident after the first pass, so a second one reads final UVs.
            if self.atlas.content_epoch() != epoch_at_build {
                self.build_text_run_instances_into(device, queue, grid, &mut instances);
            }
            self.text_run_build_scratch = instances;

            let mut rects = mem::take(&mut self.run_rect_build_scratch);
            self.build_run_rects_into(grid, &mut rects);
            self.run_rect_build_scratch = rects;

            self.last_text_run_epoch = text_runs_epoch;
            self.last_atlas_epoch = self.atlas.content_epoch();
        }

        let region = grid.scroll_region();

        // Text runs pack above, so a text-run grow is already folded into the
        // content epoch grid_build reads. A scroll's pixel offset does not count,
        // since it rides the globals uniform, but a scroll that emptied a slot
        // leaves that slot's instances in the buffer for the plan to clear out.
        let region_rect = region.map(|r| (r.top, r.left, r.width, r.height));
        let build = grid_build(
            rebuilt.is_empty(),
            self.exposed_from.is_some(),
            region_rect != self.last_region_rect,
            self.atlas.content_epoch(),
            self.grid_atlas_epoch,
        );
        self.last_region_rect = region_rect;

        if build != GridBuild::Reuse {
            let epoch_at_build = self.atlas.content_epoch();
            self.build_grid_instances(
                device,
                queue,
                region,
                &rebuilt,
                build == GridBuild::RebuildAll,
            );
            // Resolving a row re-packs any glyph evicted on an earlier frame,
            // which moves every UV partway through the pass and leaves the rows
            // resolved before it holding pre-move ones. Every grid glyph is
            // resident after the first pass, so a second one settles them all.
            if self.atlas.content_epoch() != epoch_at_build {
                self.build_grid_instances(device, queue, region, &rebuilt, true);
            }
            // Both arms resolved every grid glyph against the current atlas, so
            // record the epoch they built against for the next frame's compare.
            self.grid_atlas_epoch = self.atlas.content_epoch();
        }

        self.rebuilt_scratch = rebuilt;

        self.region_scissor = region.and_then(|region| {
            cell_rect_scissor(
                region.top,
                region.left,
                region.width,
                region.height,
                [0.0, 0.0],
                resolution,
                self.metrics,
            )
        });

        // The base instances (positions before the anchor and scroll shift, with
        // final atlas UVs) are reused while the overlay content and the atlas UVs
        // both held. An eviction anywhere this frame bumps the content epoch, so
        // the check also catches an overlay glyph re-inserted by the touch
        // above. Each overlay's scissor depends only on its geometry, so it
        // rides the same cache.
        //
        // A base spans only the lines its box can show, so the window it was built
        // for is part of what has to hold. Scrolling within a line keeps the same
        // window and reuses the base. Crossing a line boundary rebuilds one box's
        // worth of instances rather than all of its content.
        let metrics = self.metrics;
        let overlays = grid.overlays();
        let content_epoch = self.atlas.content_epoch();
        let bases_reused = pending_reused
            && content_epoch == self.last_overlay_atlas_epoch
            && self.overlay_bases.len() == overlays.len()
            && self.overlay_windows.len() == overlays.len()
            && (0..overlays.len()).all(|index| {
                self.overlay_windows[index]
                    == overlay_window(
                        &overlays[index],
                        scroll_at(index),
                        &self.overlay_pending[index],
                    )
            });
        if !bases_reused {
            let pending = mem::take(&mut self.overlay_pending);
            let mut bases = mem::take(&mut self.overlay_bases);
            let mut scissors = mem::take(&mut self.overlay_base_scissors);

            bases.resize_with(pending.len(), Vec::new);
            scissors.clear();
            self.overlay_windows.clear();

            for (index, ((overlay, content), base)) in
                overlays.iter().zip(&pending).zip(&mut bases).enumerate()
            {
                let window = overlay_window(overlay, scroll_at(index), content);

                // Built into the base's own buffer, since a scroll that crosses a
                // line boundary reaches here on the frame it crosses.
                base.clear();
                // An overlay draws against the screen-anchored globals, and its
                // content rows run past the bottom of the screen whenever the
                // box holds more lines than fit, so nothing here rotates.
                self.build_text_instances_into(
                    device,
                    queue,
                    content.window(window.clone()),
                    RowRotation::unrotated(),
                    base,
                );
                self.overlay_windows.push(window);

                let anchor = [overlay.offset[0] as f32, overlay.offset[1] as f32];
                scissors.push(cell_rect_scissor(
                    overlay.top,
                    overlay.left,
                    overlay.width,
                    overlay.height,
                    anchor,
                    resolution,
                    metrics,
                ));
            }

            self.overlay_pending = pending;
            self.overlay_bases = bases;
            self.overlay_base_scissors = scissors;
            self.last_overlay_atlas_epoch = content_epoch;
        }

        // A scroll-only frame only slides each overlay's content within its box,
        // so re-add the anchor and the current scroll offset to the cached bases
        // rather than rebuilding them. When the scroll offsets also match the
        // uploaded ones, the buffer and draws already hold and nothing is redone.
        //
        // The offsets are compared one at a time. An autoscrolling popover
        // reaches here on every frame it moves, and gathering them into a vector
        // just to compare it would allocate on each of those frames.
        let scrolls_moved = self.last_popover_scrolls.len() != overlays.len()
            || (0..overlays.len())
                .any(|index| self.last_popover_scrolls[index] != scroll_at(index));

        if !bases_reused || scrolls_moved {
            let mut overlay_instances = mem::take(&mut self.overlay_instance_scratch);
            let mut draws = mem::take(&mut self.overlay_draw_scratch);
            overlay_instances.clear();
            draws.clear();

            for (index, (overlay, base)) in overlays.iter().zip(&self.overlay_bases).enumerate() {
                let start = overlay_instances.len() as u32;
                let anchor = [overlay.offset[0] as f32, overlay.offset[1] as f32];
                let scroll_px = scroll_at(index) * metrics.height;
                overlay_instances.extend(base.iter().map(|instance| {
                    let mut instance = *instance;
                    instance.pos[0] += anchor[0];
                    instance.pos[1] += anchor[1] - scroll_px;
                    instance
                }));
                draws.push(OverlayDraw {
                    start,
                    count: overlay_instances.len() as u32 - start,
                    scissor: self.overlay_base_scissors[index],
                });
            }

            self.overlay_count = overlay_instances.len() as u32;
            upload_instances(
                device,
                queue,
                &overlay_instances,
                &mut self.overlay_instances,
                &mut self.overlay_capacity,
                "overlay text instances",
            );
            self.overlay_instance_scratch = overlay_instances;

            // The draws just built become the ones drawn from, and the ones they
            // replace become next frame's scratch.
            self.overlay_draw_scratch = mem::replace(&mut self.overlay_draws, draws);

            self.last_popover_scrolls.clear();
            self.last_popover_scrolls
                .extend((0..overlays.len()).map(scroll_at));
        }
        if runs_rebuilt {
            let instances = mem::take(&mut self.text_run_build_scratch);
            self.text_run_count = instances.len() as u32;
            upload_instances(
                device,
                queue,
                &instances,
                &mut self.text_run_instances,
                &mut self.text_run_capacity,
                "text run instances",
            );
            self.text_run_build_scratch = instances;

            let rects = mem::take(&mut self.run_rect_build_scratch);
            self.rect_count = rects.len() as u32;
            upload_instances(
                device,
                queue,
                &rects,
                &mut self.rect_instances,
                &mut self.rect_capacity,
                "text run rect instances",
            );
            self.run_rect_build_scratch = rects;
        }

        // The bind group references only the atlas texture views, which are
        // recreated solely when an atlas grows. Reuse last frame's group unless
        // a grow this frame moved the views.
        if self.atlas.texture_dims() != atlas_dims {
            self.atlas_bind_group = create_atlas_bind_group(
                device,
                &self.atlas_layout,
                &self.sampler,
                self.atlas.mask_view(),
                self.atlas.color_view(),
            );
        }

        // Each globals buffer carries its own scroll. The plain glyphs take the
        // grid scroll, the region glyphs the region scroll, and the
        // screen-anchored runs and overlays none. Each buffer is sent only when
        // its value moved, so a scroll-only frame refreshes just the uniforms it
        // changed without rebuilding instances, and an idle frame writes none of
        // them.
        //
        // Written after the packing above rather than before it, because the
        // atlas sizes they carry are what the instances are normalized by, and
        // packing is what grows an atlas. Written earlier they would name the
        // size the atlas had before this frame's glyphs arrived, and every glyph
        // would sample at the wrong scale for a frame. Everything else here
        // settled before the packing and is unmoved by it.
        //
        // Live draws never bypass the seq test, so occlude_all stays zero. Only
        // the static globals' text-run draws occlude, and they do it by seq. The
        // rotation rides the buffer its instances are drawn against. The grid
        // and region draws carry grid rows and rotate with them. The
        // screen-anchored buffer carries rows its builders chose, including an
        // overlay's past the bottom of the screen, so it never wraps.
        let (mask_size, color_size) = self.atlas.texture_dims();
        let globals_with = |scroll_y: f32, panel_count: u32, rotation: RowRotation| TextGlobals {
            resolution,
            cell_size,
            atlas_size: [mask_size as f32, color_size as f32],
            scroll_y,
            panel_count,
            occlude_all: 0,
            row_offset: rotation.offset,
            rows: rotation.rows as u32,
            _pad0: 0,
            origin_cells: [0.0; 2],
            _pad1: [0; 2],
        };
        crate::render::upload_globals(
            queue,
            &self.globals,
            0,
            globals_with(grid_scroll_y, 0, grid_rotation),
            &mut self.last_globals,
        );
        crate::render::upload_globals(
            queue,
            &self.region_globals,
            0,
            globals_with(region_scroll_y, 0, grid_rotation),
            &mut self.last_region_globals,
        );
        crate::render::upload_globals(
            queue,
            &self.static_globals,
            0,
            globals_with(0.0, panel_count, RowRotation::unrotated()),
            &mut self.last_static_globals,
        );
    }

    /// Shape, rasterize, and upload a pool grid's plain glyphs and underlines
    /// for compositing over the live grid, into buffers separate from the live
    /// ones.
    ///
    /// Routing a pool through [`Self::prepare`] would rebuild the live glyph and
    /// underline buffers from the pool's cells, erasing the live grid's
    /// damage-tracked rows. This builds into dedicated buffers
    /// [`Self::draw_composite`] reads, leaving every live buffer and shaping
    /// cache intact, so the next live frame still patches only its damaged rows.
    ///
    /// `shift_rows` shifts the grid up by that many rows. The pool grid changes
    /// wholesale each frame, so every row is rebuilt with no per-row cache. The
    /// pool is shaped fresh into local storage rather than through
    /// [`Self::glyph_row_cache`], whose length and the sibling
    /// `glyph_cache_cols`/`last_cursor_cell` drive the live frame's incremental
    /// rebuild and so must not be disturbed.
    ///
    /// Pool glyphs rasterize into the shared atlas. A grow moves every UV, so
    /// the atlas bind group the live draw also binds is recreated afterward.
    /// Covers only plain glyphs and underlines, the two buffers [`Self::draw`]
    /// reads.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        occluders: &[Occluder],
        resolution: [f32; 2],
        shift_rows: f32,
        origin_cells: [f32; 2],
        content_changed: bool,
        scrolled_rows: Option<isize>,
        pool: u32,
        slot: usize,
    ) {
        // The composite draws bind self.globals, so the occlusion rides that buffer
        // alone.
        self.upload_occluders(device, queue, occluders);
        let (panel_count, occlude_all) = occlusion_globals(occluders);
        // Held before the composite slot below takes the name, since the
        // globals are not written until after the packing.
        let globals_slot = slot;

        // During a pure sub-cell glide the composed rows are identical and only
        // the shift moved, which the globals write above already carried. Reuse
        // the instances built for these rows on an earlier frame, unless the
        // atlas has since relocated their UVs.
        if !content_changed
            && self
                .composite_slots
                .get(pool)
                .is_some_and(|target| target.epoch == self.atlas.content_epoch())
        {
            return;
        }

        let metrics = self.metrics;
        let rows = grid.rows();

        // A glide moves the rows without changing them, so the shaping done for
        // them last frame still describes them. Carrying it needs a cache of
        // this shape and an atlas that has not moved since, because the kept
        // glyphs hold the placements it gave them.
        let epoch_before = self.atlas.content_epoch();
        let rotate_by = scrolled_rows.filter(|&by| by != 0).filter(|_| {
            self.composite_slots.get(pool).is_some_and(|slot| {
                slot.epoch == epoch_before
                    && slot.glyph_rows.len() == rows
                    && slot.underline_rows.len() == rows
            })
        });

        let slot = self.composite_slots.entry(pool, || new_slot(device));
        let mut glyph_rows = mem::take(&mut slot.glyph_rows);
        let mut underline_rows = mem::take(&mut slot.underline_rows);

        // Rows the frame has to shape for itself. A rotation leaves only the ones
        // it scrolled into view. Anything else starts from nothing.
        match rotate_by {
            Some(by) => {
                crate::render::rotate_row_cache(&mut glyph_rows, by, |glyph| {
                    glyph.row = glyph.row.saturating_add_signed(-by);
                });
                crate::render::rotate_row_cache(&mut underline_rows, by, |underline| {
                    underline.row = underline.row.saturating_add_signed(-by as i32);
                });
            },
            None => {
                glyph_rows.clear();
                glyph_rows.resize_with(rows, Vec::new);
                underline_rows.clear();
                underline_rows.resize_with(rows, Vec::new);
            },
        }
        let exposed = exposed_rows(rotate_by, rows);
        // A pool composes its own grid, whose height is its own.
        let rotation = RowRotation {
            offset: self.row_offset,
            rows: grid.rows(),
        };

        for row in exposed.clone() {
            underline_rows[row].clear();
            build_underline_row_into(grid, row, metrics, rotation, &mut underline_rows[row]);
        }

        self.underline_upload_scratch.clear();
        for row in &underline_rows {
            self.underline_upload_scratch.extend_from_slice(row);
        }
        let target = self.composite_slots.entry(pool, || new_slot(device));
        target.underlines.count = self.underline_upload_scratch.len() as u32;
        if !self.underline_upload_scratch.is_empty() {
            if self.underline_upload_scratch.len() > target.underlines.capacity {
                target.underlines.capacity =
                    self.underline_upload_scratch.len().next_power_of_two();
                target.underlines.instances = alloc_instances(
                    device,
                    "composite underline instances",
                    instance_bytes::<UnderlineInstance>(target.underlines.capacity),
                );
            }
            queue.write_buffer(
                &target.underlines.instances,
                0,
                bytemuck::cast_slice(&self.underline_upload_scratch),
            );
        }

        // Pack the composite text runs and their background rects before the
        // grid glyphs, so a run-glyph atlas grow reflects in the grid resolve
        // below rather than invalidating its UVs. The runs carry the pool shift
        // through the composite globals, so they glide with the page cells.
        //
        // The run build packs and emits in one pass, and the row pack below
        // packs more glyphs after it, so either can grow the atlas and rescale
        // the UVs the runs froze. The run instances are held here and
        // re-resolved, counted, and uploaded only after the row pack, so they
        // land against the final atlas.
        let atlas_dims = self.atlas.texture_dims();

        let mut run_instances = mem::take(&mut self.composite_run_scratch);
        self.build_text_run_instances_into(device, queue, grid, &mut run_instances);

        let mut run_rects = mem::take(&mut self.composite_rect_scratch);
        self.build_run_rects_into(grid, &mut run_rects);
        let target = self.composite_slots.entry(pool, || new_slot(device));
        target.rects.count = run_rects.len() as u32;
        upload_instances(
            device,
            queue,
            &run_rects,
            &mut target.rects.instances,
            &mut target.rects.capacity,
            "composite text run rect instances",
        );
        self.composite_rect_scratch = run_rects;

        // Shape every pool row fresh through the same per-row primitive
        // rasterize_visible uses, but into reused scratch, so glyph_row_cache and
        // its sibling shaping state stay the live frame's. rasterize_row inserts
        // each glyph into the shared atlas, so build_text_instances below reads
        // final UVs once the atlas has reached its size for this pool.
        let mut pending = mem::take(&mut self.composite_pending_scratch);
        pending.clear();
        {
            let primary_name = self.family.clone();
            let primary = font::shape_family(primary_name.as_deref());
            let primary_font = self.primary_font.clone();
            let charmap = primary_font.as_ref().map(|font| font.as_swash().charmap());
            let covers = |ch: char| charmap.as_ref().is_some_and(|map| map.map(ch) != 0);
            let shaping = RowShaping {
                primary,
                covers: &covers,
                cursor_cell: None,
            };

            for row in exposed.clone() {
                glyph_rows[row].clear();
                self.rasterize_row(device, queue, grid, row, &shaping, &mut glyph_rows[row]);
            }
        }
        for row in &glyph_rows {
            pending.extend_from_slice(row);
        }

        // The runs packed before the rows above, so either pass may have grown
        // the atlas since the runs froze their UVs. Every run glyph is resident
        // now, so a second pass reads final UVs without growing again. The grid
        // build below already resolves post-pack.
        if self.atlas.texture_dims() != atlas_dims {
            self.build_text_run_instances_into(device, queue, grid, &mut run_instances);
        }
        let target = self.composite_slots.entry(pool, || new_slot(device));
        target.text_runs.count = run_instances.len() as u32;
        upload_instances(
            device,
            queue,
            &run_instances,
            &mut target.text_runs.instances,
            &mut target.text_runs.capacity,
            "composite text run instances",
        );
        self.composite_run_scratch = run_instances;

        let mut instances = mem::take(&mut self.composite_upload_scratch);
        self.build_text_instances_into(device, queue, &pending, rotation, &mut instances);
        let target = self.composite_slots.entry(pool, || new_slot(device));
        target.glyphs.count = instances.len() as u32;
        upload_instances(
            device,
            queue,
            &instances,
            &mut target.glyphs.instances,
            &mut target.glyphs.capacity,
            "composite text instances",
        );
        self.composite_pending_scratch = pending;
        self.composite_upload_scratch = instances;

        if self.atlas.texture_dims() != atlas_dims {
            self.atlas_bind_group = create_atlas_bind_group(
                device,
                &self.atlas_layout,
                &self.sampler,
                self.atlas.mask_view(),
                self.atlas.color_view(),
            );
        }

        // After the packing above, which is what can grow an atlas. The sizes
        // here are what the instances just built are normalized by, so naming a
        // pre-grow one would draw this pool at the wrong scale for a frame.
        let (mask_size, color_size) = self.atlas.texture_dims();
        queue.write_buffer(
            &self.globals,
            u64::from(globals_offset(globals_slot)),
            bytemuck::bytes_of(&TextGlobals {
                resolution,
                cell_size: [self.metrics.width, self.metrics.height],
                atlas_size: [mask_size as f32, color_size as f32],
                scroll_y: shift_rows * self.metrics.height,
                panel_count,
                occlude_all,
                row_offset: self.row_offset,
                rows: grid.rows() as u32,
                _pad0: 0,
                origin_cells,
                _pad1: [0; 2],
            }),
        );

        // Record the atlas state these instances resolved against, so a later
        // shift-only frame can tell whether their UVs still hold.
        let epoch = self.atlas.content_epoch();
        let target = self.composite_slots.entry(pool, || new_slot(device));
        target.epoch = epoch;
        target.glyph_rows = glyph_rows;
        target.underline_rows = underline_rows;
    }

    /// The rotation the live grid's row draws are built and bound under.
    fn grid_rotation(&self) -> RowRotation {
        RowRotation {
            offset: self.row_offset,
            rows: self.grid_rows,
        }
    }

    /// Build the glyph instances for `pending` into `out`, clearing it first so a
    /// reused scratch buffer holds only this frame's instances.
    ///
    /// Each instance stores the slot its row occupies under the rotation the
    /// draw will be bound with, so `rotation` has to be the one written into
    /// the globals those instances are drawn against. Screen-anchored draws
    /// pass an unrotated one.
    fn build_text_instances_into(
        &mut self,
        device: &Device,
        queue: &Queue,
        pending: &[PendingGlyph],
        rotation: RowRotation,
        out: &mut Vec<TextInstance>,
    ) {
        out.clear();
        out.reserve(pending.len());
        let epoch = self.atlas.content_epoch();
        for glyph in pending {
            // Rasterizing the glyph already read where it landed, so an atlas that
            // has not moved since makes a second lookup pure cost. A grow or an
            // eviction moves every sub-rect and bumps the epoch, and a cached row's
            // glyphs can be older still, so a mismatch re-resolves.
            let info = if glyph.resolved_epoch == epoch {
                glyph.info
            } else if let Some(info) = self.resolve_glyph(device, queue, glyph.source) {
                info
            } else {
                continue;
            };

            // A procedural separator already fills the cell, so it lands on the
            // pixel-snapped cell rect; a font glyph sits at its bitmap placement,
            // with cell-fill codepoints scaled to the cell box.
            let (pos, dim) = match glyph.source {
                GlyphSource::Procedural { .. } => {
                    cell_box_rect(glyph.row, glyph.col, glyph.scale, self.metrics)
                },
                GlyphSource::Font(_) => {
                    let pos = glyph_origin(
                        glyph.col,
                        glyph.row,
                        info.placement,
                        self.baseline * glyph.scale,
                        self.metrics,
                    );
                    let dim = [info.size[0] as f32, info.size[1] as f32];
                    if glyph.cell_fill {
                        fill_cell_box(
                            pos,
                            dim,
                            glyph.row,
                            glyph.scale,
                            self.baseline,
                            self.metrics,
                        )
                    } else {
                        (pos, dim)
                    }
                },
            };

            out.push(TextInstance {
                pos: [pos[0], pos[1] - glyph.row as f32 * self.metrics.height],
                dim,
                uv: info.uv,
                fg: rgb_f32(glyph.fg),
                kind: kind_flag(info.kind),
                seq: UNOCCLUDED_SEQ,
                row: rotation.slot(glyph.row) as u32,
            });
        }
    }

    /// Resolve a pending glyph's final atlas placement, re-rasterizing only on a
    /// cache miss. The font and procedural paths share the atlas, so each
    /// resolves through its own keyed lookup.
    fn resolve_glyph(
        &mut self,
        device: &Device,
        queue: &Queue,
        source: GlyphSource,
    ) -> Option<GlyphInfo> {
        match source {
            GlyphSource::Font(key) => self.atlas.get_or_insert(
                device,
                queue,
                &mut self.font_system,
                &mut self.swash_cache,
                key,
            ),
            GlyphSource::Procedural { cp, width, height } => {
                self.atlas
                    .get_or_insert_procedural(device, queue, cp, width, height, || {
                        powerline::rasterize(cp, width, height).unwrap_or_default()
                    })
            },
        }
    }

    /// Build the glyph instances for the grid's off-grid text runs, into a
    /// vector of the caller's.
    #[cfg(test)]
    fn build_text_run_instances(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
    ) -> Vec<TextInstance> {
        let mut instances = Vec::new();
        self.build_text_run_instances_into(device, queue, grid, &mut instances);
        instances
    }

    /// Build the glyph instances for the grid's off-grid text runs into `out`.
    ///
    /// Each run is shaped at its fractional scale and laid out by
    /// [`text_run_origin`]: screen-anchored (no grid scroll), advancing one
    /// scaled cell width per glyph, vertically centered in its row. A
    /// non-positive scale draws nothing.
    ///
    /// `out` is cleared first, so a reused scratch buffer holds only this
    /// frame's instances.
    fn build_text_run_instances_into(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        out: &mut Vec<TextInstance>,
    ) {
        out.clear();
        for run in grid.text_runs() {
            let scale = f32::from(run.scale) / 256.0;
            if scale <= 0.0 {
                continue;
            }
            let col = f32::from(run.col) / 16.0;
            let row = f32::from(run.row) / 16.0;

            for (index, ch) in run.text.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                let Some(key) = self.glyph_key(ch, scale, Weight::NORMAL) else {
                    continue;
                };
                let Some(info) = self.atlas.get_or_insert(
                    device,
                    queue,
                    &mut self.font_system,
                    &mut self.swash_cache,
                    key,
                ) else {
                    continue;
                };

                out.push(TextInstance {
                    pos: text_run_origin(
                        col,
                        row,
                        index,
                        scale,
                        info.placement,
                        self.baseline,
                        self.metrics,
                    ),
                    dim: [info.size[0] as f32, info.size[1] as f32],
                    uv: info.uv,
                    fg: rgb_f32(run.color),
                    kind: kind_flag(info.kind),
                    seq: run.seq,
                    // A text run is placed absolutely rather than per grid row.
                    row: 0,
                });
            }
        }
    }

    /// Build the text runs' background rects into a vector of the caller's.
    #[cfg(test)]
    fn build_run_rects(&self, grid: &Grid) -> Vec<RectInstance> {
        let mut rects = Vec::new();
        self.build_run_rects_into(grid, &mut rects);
        rects
    }

    /// Build into `out` one opaque background rect per scaled text run that
    /// carries a background, painted before its glyphs so they alpha-blend over
    /// it.
    ///
    /// A run with no background contributes no rect. Its glyphs blend directly
    /// over whatever lies beneath, so a title run leaves the panel hairline it
    /// sits over unbroken. When present, the rect spans the run's full character
    /// width (spaces included) and one full cell height, an opaque backing where
    /// a run needs one.
    ///
    /// `out` is cleared first, so a reused scratch buffer holds only this
    /// frame's rects.
    fn build_run_rects_into(&self, grid: &Grid, out: &mut Vec<RectInstance>) {
        out.clear();
        for run in grid.text_runs() {
            let Some(bg) = run.bg else {
                continue;
            };
            let scale = f32::from(run.scale) / 256.0;
            if scale <= 0.0 {
                continue;
            }
            let width = run.text.chars().count() as f32 * scale * self.metrics.width;
            if width <= 0.0 {
                continue;
            }
            let col = f32::from(run.col) / 16.0;
            let row = f32::from(run.row) / 16.0;
            out.push(RectInstance {
                pos: [col * self.metrics.width, row * self.metrics.height],
                dim: [width, self.metrics.height],
                color: rgb_f32(bg),
                seq: run.seq,
            });
        }
    }

    /// Rebuild and re-upload only the damaged rows' underline instances.
    ///
    /// Underline is a VT cell attribute, so `damage` tracks it. Unchanged rows reuse
    /// last frame's [`Self::underline_row_instances`], while damaged rows, and every
    /// row on `Damage::Full` or a resize, rebuild. Scroll rides the globals uniform
    /// and the rotation, so a scroll-only frame rewrites from the lowest slot it
    /// emptied and leaves the rest of the buffer untouched.
    ///
    /// The rebuilt slots are patched where they sit, adjacent ones sharing a write.
    /// A slot that came back a different length displaced the slots after it, so
    /// from there to the end the buffer is rewritten as one span.
    fn prepare_underlines(&mut self, device: &Device, queue: &Queue, grid: &Grid, damage: &Damage) {
        let rows = grid.rows();
        let stale = self.underline_row_instances.len() != rows;
        if stale {
            self.underline_row_instances = vec![Vec::new(); rows];
        }

        underline_rows_to_build(damage, rows, stale, &mut self.underline_rows_to_build);
        if self.underline_rows_to_build.is_empty() && self.exposed_from.is_none() {
            return;
        }

        let metrics = self.metrics;
        let rotation = self.grid_rotation();
        let mut rewrite_from = self.exposed_from;
        {
            let TextPass {
                underline_rows_to_build,
                underline_row_instances,
                ..
            } = self;
            for &row in underline_rows_to_build.iter() {
                let slot = rotation.slot(row);
                let built = &mut underline_row_instances[slot];
                let held = built.len();

                built.clear();
                build_underline_row_into(grid, row, metrics, rotation, built);

                if built.len() != held {
                    rewrite_from = Some(rewrite_from.map_or(slot, |from| from.min(slot)));
                }
            }
        }

        let total = row_len(&self.underline_row_instances);
        self.underline_count = total as u32;
        if total == 0 {
            return;
        }

        let mut scratch = mem::take(&mut self.underline_upload_scratch);
        if total > self.underline_capacity {
            // Growing the buffer drops its contents, so re-upload every row.
            self.underline_capacity = total.next_power_of_two();
            self.underline_instances = alloc_instances(
                device,
                "underline instances",
                instance_bytes::<UnderlineInstance>(self.underline_capacity),
            );
            self.upload_underline_rows(queue, &mut scratch, 0, 0..rows);
        } else {
            let mut patched = mem::take(&mut self.patched_slots_scratch);
            slots_of_rows(rotation, &self.underline_rows_to_build, &mut patched);

            let uploads = row_uploads(&self.underline_row_instances, &patched, rewrite_from);
            for (offset, run) in uploads {
                self.upload_underline_rows(queue, &mut scratch, offset, run);
            }
            self.patched_slots_scratch = patched;
        }
        self.underline_upload_scratch = scratch;
    }

    /// Write rows `range` of the per-row underline cache into the instance buffer,
    /// at `offset` instances from its start.
    fn upload_underline_rows(
        &self,
        queue: &Queue,
        scratch: &mut Vec<UnderlineInstance>,
        offset: usize,
        range: Range<usize>,
    ) {
        scratch.clear();
        scratch.extend(
            self.underline_row_instances[range]
                .iter()
                .flatten()
                .copied(),
        );
        if scratch.is_empty() {
            return;
        }

        queue.write_buffer(
            &self.underline_instances,
            (offset * size_of::<UnderlineInstance>()) as u64,
            bytemuck::cast_slice(scratch),
        );
    }

    /// Record the glyph draw, then the underline draw, into `render_pass`.
    ///
    /// A no-op until [`Self::prepare`] has run. Must run after the background
    /// pass in the same render pass: each glyph quad composites over the cell
    /// background painted underneath, and underlines alpha-blend over the glyphs.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.count > 0 {
            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_bind_group(0, &self.globals_bind_group, &[0]);
            render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
            render_pass.set_vertex_buffer(0, self.instances.slice(..));
            render_pass.draw(0..6, 0..self.count);
        }

        if self.underline_count > 0 {
            render_pass.set_pipeline(&self.underline_pipeline);
            render_pass.set_bind_group(0, &self.globals_bind_group, &[0]);
            render_pass.set_vertex_buffer(0, self.underline_instances.slice(..));
            render_pass.draw(0..6, 0..self.underline_count);
        }
    }

    /// Record a composited pool's glyph draw, then its underline draw, into
    /// `render_pass`.
    ///
    /// A no-op until [`Self::prepare_composite`] has run for `slot`. Reads that
    /// slot's buffers, so a pool draw leaves both the live glyph and underline
    /// instances a prior [`Self::prepare`] uploaded and the other pools' slots
    /// untouched. Binds the grid-scroll globals [`Self::prepare_composite`] wrote,
    /// so the pool scrolls by its shift.
    pub fn draw_composite(&self, render_pass: &mut RenderPass<'_>, pool: u32, slot: usize) {
        let Some(target) = self.composite_slots.get(pool) else {
            return;
        };

        if target.glyphs.count > 0 {
            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_bind_group(0, &self.globals_bind_group, &[globals_offset(slot)]);
            render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
            render_pass.set_vertex_buffer(0, target.glyphs.instances.slice(..));
            render_pass.draw(0..6, 0..target.glyphs.count);
        }

        if target.underlines.count > 0 {
            render_pass.set_pipeline(&self.underline_pipeline);
            render_pass.set_bind_group(0, &self.globals_bind_group, &[globals_offset(slot)]);
            render_pass.set_vertex_buffer(0, target.underlines.instances.slice(..));
            render_pass.draw(0..6, 0..target.underlines.count);
        }
    }

    /// Record a composited pool's run-background and text-run glyph draw into
    /// `render_pass`.
    ///
    /// A no-op until [`Self::prepare_composite`] has run for `slot`. Reads that
    /// slot's run buffers, leaving the live text-run instances and the other pools'
    /// slots untouched. Binds the grid-scroll globals [`Self::prepare_composite`]
    /// wrote, so the runs glide with the page rather than staying screen-anchored
    /// like [`Self::draw_text_runs`].
    pub fn draw_composite_text_runs(
        &self,
        render_pass: &mut RenderPass<'_>,
        pool: u32,
        slot: usize,
    ) {
        let Some(target) = self.composite_slots.get(pool) else {
            return;
        };

        if target.rects.count > 0 {
            render_pass.set_pipeline(&self.rect_pipeline);
            render_pass.set_bind_group(0, &self.globals_bind_group, &[globals_offset(slot)]);
            render_pass.set_vertex_buffer(0, target.rects.instances.slice(..));
            render_pass.draw(0..6, 0..target.rects.count);
        }

        if target.text_runs.count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.globals_bind_group, &[globals_offset(slot)]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, target.text_runs.instances.slice(..));
        render_pass.draw(0..6, 0..target.text_runs.count);
    }

    /// Record the off-grid text-run glyph draw into `render_pass`.
    ///
    /// Reuses the glyph pipeline and atlas. The runs are screen-anchored, so no
    /// scissor is set; run it after the grid text so the runs sit on top. A
    /// no-op when no text run is present.
    pub fn draw_text_runs(&self, render_pass: &mut RenderPass<'_>) {
        // Opaque run backgrounds first, so the glyphs alpha-blend over them. A
        // run of only spaces still paints its rect (masking a hairline) with no
        // glyphs to follow.
        if self.rect_count > 0 {
            render_pass.set_pipeline(&self.rect_pipeline);
            render_pass.set_bind_group(0, &self.static_globals_bind_group, &[0]);
            render_pass.set_vertex_buffer(0, self.rect_instances.slice(..));
            render_pass.draw(0..6, 0..self.rect_count);
        }

        if self.text_run_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.static_globals_bind_group, &[0]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.text_run_instances.slice(..));
        render_pass.draw(0..6, 0..self.text_run_count);
    }

    /// Rasterize the perf HUD readout `lines` at pixel `anchor`, one line per
    /// entry stacked downward, into the screen-anchored HUD glyph buffer.
    ///
    /// Glyphs go through the shared atlas at `scale` relative to the body font
    /// and alpha-blend over the HUD panel already drawn beneath them, so the
    /// readout blends onto the HUD rather than the grid behind it.
    #[cfg(feature = "perf")]
    pub fn set_hud_text(
        &mut self,
        device: &Device,
        queue: &Queue,
        anchor: [f32; 2],
        scale: f32,
        lines: &[String],
    ) {
        const READOUT_FG: [f32; 3] = [0.85, 0.85, 0.9];

        let mut instances = Vec::new();
        for (line, text) in lines.iter().enumerate() {
            let line_top = anchor[1] + line as f32 * self.metrics.height * scale;
            let baseline_y = line_top + self.baseline * scale;
            for (index, ch) in text.chars().enumerate() {
                if ch == ' ' {
                    continue;
                }
                let Some(key) = self.glyph_key(ch, scale, Weight::NORMAL) else {
                    continue;
                };
                let Some(info) = self.atlas.get_or_insert(
                    device,
                    queue,
                    &mut self.font_system,
                    &mut self.swash_cache,
                    key,
                ) else {
                    continue;
                };
                let pen_x = anchor[0] + index as f32 * scale * self.metrics.width;
                instances.push(TextInstance {
                    pos: [
                        pen_x + info.placement[0] as f32,
                        baseline_y - info.placement[1] as f32,
                    ],
                    dim: [info.size[0] as f32, info.size[1] as f32],
                    uv: info.uv,
                    fg: READOUT_FG,
                    kind: kind_flag(info.kind),
                    seq: UNOCCLUDED_SEQ,
                    // A readout is placed absolutely rather than per grid row.
                    row: 0,
                });
            }
        }

        self.hud_count = instances.len() as u32;
        upload_instances(
            device,
            queue,
            &instances,
            &mut self.hud_instances,
            &mut self.hud_capacity,
            "hud text instances",
        );
    }

    /// Record the HUD readout draw into `render_pass`, screen-anchored like the
    /// text runs. A no-op when there is no readout text.
    #[cfg(feature = "perf")]
    pub fn draw_hud_text(&self, render_pass: &mut RenderPass<'_>) {
        if self.hud_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.static_globals_bind_group, &[0]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.hud_instances.slice(..));
        render_pass.draw(0..6, 0..self.hud_count);
    }

    /// Record the scroll-region glyph draw into `render_pass`, scissored to the
    /// region so its scrolled content is clipped to the rectangle.
    ///
    /// A no-op when no scroll region is present. Leaves the scissor rect set, so
    /// the caller must restore the full surface before any later full-screen
    /// draw.
    pub fn draw_region_text(&self, render_pass: &mut RenderPass<'_>) {
        if self.region_count == 0 {
            return;
        }

        if let Some([x, y, w, h]) = self.region_scissor {
            render_pass.set_scissor_rect(x, y, w, h);
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.region_globals_bind_group, &[0]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.region_instances.slice(..));
        render_pass.draw(0..6, 0..self.region_count);
    }

    /// Record each overlay's content glyph draw into `render_pass`, scissored to
    /// that overlay's box so its scrolled content is clipped to it.
    ///
    /// One scissored sub-range draw per overlay, so several popovers clip and
    /// scroll independently. A no-op when no overlay carries content. Run after
    /// the overlay boxes so the content sits inside them, on top of the fill.
    /// Must be the pass's last draw, since it leaves the scissor rect set. An
    /// overlay whose box clips to no area is skipped rather than drawn unclipped.
    pub fn draw_overlay_text(&self, render_pass: &mut RenderPass<'_>) {
        if self.overlay_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.static_globals_bind_group, &[0]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.overlay_instances.slice(..));

        for draw in &self.overlay_draws {
            let Some([x, y, w, h]) = draw.scissor else {
                continue;
            };
            if draw.count == 0 {
                continue;
            }
            render_pass.set_scissor_rect(x, y, w, h);
            render_pass.draw(0..6, draw.start..draw.start + draw.count);
        }
    }

    /// Phase one: shape and rasterize every visible cell glyph, returning their
    /// placements.
    ///
    /// Adjacent same-style cells the primary font covers are shaped together as
    /// one run, so the font's ligatures form across cells. Each resulting glyph
    /// maps back to the column it begins at. Scaled glyphs and characters outside
    /// the primary font are shaped on their own.
    ///
    /// Rasterizing here may grow the atlas, so the returned glyphs carry only
    /// their cache key; the caller reads each atlas sub-rect afterward, once the
    /// atlas has reached its final size and normalized coordinates are stable.
    fn rasterize_visible(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        cursor_cell: Option<(usize, usize)>,
        damage: &Damage,
        decoration_damage: &Damage,
    ) -> Vec<usize> {
        let rows = grid.rows();
        let cols = grid.cols();

        // Reuse the face resolved once at construction, so each cell's coverage
        // check is a charmap lookup rather than a per-frame font-database query.
        // The charmap is built once here as well. Constructing one parses the
        // font's cmap table directory, which is far more work than the lookup.
        let primary_name = self.family.clone();
        let primary = font::shape_family(primary_name.as_deref());
        let primary_font = self.primary_font.clone();
        let charmap = primary_font.as_ref().map(|font| font.as_swash().charmap());
        let covers = |ch: char| charmap.as_ref().is_some_and(|map| map.map(ch) != 0);

        // The per-row cache holds the previous frame's glyphs. Every row rebuilds
        // when the grid was resized or the terminal reported full damage;
        // otherwise a row rebuilds when its cells changed (VT damage), when an APC
        // scale covering it changed (decoration damage), or when the cursor
        // entered or left it.
        let rebuild_all = self.glyph_row_cache.len() != rows
            || self.glyph_cache_cols != cols
            || matches!(damage, Damage::Full);
        if self.glyph_row_cache.len() != rows {
            self.glyph_row_cache = vec![Vec::new(); rows];
        }
        self.glyph_cache_cols = cols;

        let cursor_moved = cursor_cell != self.last_cursor_cell;
        let left_row = self.last_cursor_cell.map(|(row, _)| row);
        let entered_row = cursor_cell.map(|(row, _)| row);
        self.last_cursor_cell = cursor_cell;

        let shaping = RowShaping {
            primary,
            covers: &covers,
            cursor_cell,
        };
        let rotation = self.grid_rotation();
        let mut rebuilt = mem::take(&mut self.rebuilt_scratch);
        rebuilt.clear();
        for row in 0..rows {
            let cursor_touched =
                cursor_moved && (left_row == Some(row) || entered_row == Some(row));
            if rebuild_all
                || damage.is_dirty(row)
                || decoration_damage.is_dirty(row)
                || cursor_touched
            {
                let slot = rotation.slot(row);
                let mut row_glyphs = mem::take(&mut self.glyph_row_cache[slot]);
                row_glyphs.clear();
                self.rasterize_row(device, queue, grid, row, &shaping, &mut row_glyphs);
                self.glyph_row_cache[slot] = row_glyphs;
                rebuilt.push(row);
            }
        }

        rebuilt
    }

    /// The cached glyphs of every row, concatenated in display-row order, each
    /// stamped with the row it is currently on.
    ///
    /// Walks the rows rather than the cache, since the cache is in slot order
    /// and two passes at different rotations hold the same screen in different
    /// slots. The stamp is what [`Self::rebuild_plain_row`] applies when it
    /// resolves a slot, so this reports what a rebuild would see rather than
    /// the row each glyph happened to be rasterized for.
    #[cfg(test)]
    fn collect_grid_glyphs(&self) -> Vec<PendingGlyph> {
        let rotation = self.grid_rotation();
        (0..self.glyph_row_cache.len())
            .flat_map(|row| {
                self.glyph_row_cache[rotation.slot(row)]
                    .iter()
                    .map(move |glyph| PendingGlyph { row, ..*glyph })
            })
            .collect()
    }

    /// Resolve the cached grid glyphs into instance buffers against the atlas as
    /// it currently stands.
    ///
    /// Idempotent, so `prepare` can run it a second time when the resolve itself
    /// moved the atlas. A `region` splits each row's glyphs across the two buffers,
    /// which are patched per row the same way an unsplit one is.
    fn build_grid_instances(
        &mut self,
        device: &Device,
        queue: &Queue,
        region: Option<ScrollRegion>,
        rebuilt: &[usize],
        rebuild_all: bool,
    ) {
        self.patch_rows(device, queue, region, rebuilt, rebuild_all);
    }

    /// Carry the per-row caches across a scroll of `scrolled` rows on a grid
    /// `grid_rows` tall, so the rows it only moved keep the shaping and
    /// rasterizing already done for them.
    ///
    /// Nothing is moved and no instance is rewritten. Advancing the rotation by
    /// what the screen scrolled sends a kept row to the slot it already
    /// occupies, since `(r + off) + by` and `(r + by) + off` are the same slot,
    /// so its cache entry and its bytes in the buffer are both already correct.
    /// Only the slots the scroll exposed are emptied, and they are what is left
    /// to rebuild.
    ///
    /// A changed row count leaves the rotation at zero. The caches are
    /// reallocated and every row rebuilt below, so there is nothing to carry.
    fn carry_caches_across_scroll(&mut self, grid_rows: usize, scrolled: isize) {
        if grid_rows != self.grid_rows {
            self.grid_rows = grid_rows;
            self.row_offset = 0;
            self.exposed_from = None;
            return;
        }

        self.exposed_from = None;
        if scrolled == 0 || grid_rows == 0 {
            return;
        }

        self.row_offset =
            (self.row_offset + scrolled.rem_euclid(grid_rows as isize) as u32) % grid_rows as u32;

        let rotation = self.grid_rotation();
        for row in exposed_rows(Some(scrolled), grid_rows) {
            let slot = rotation.slot(row);
            self.glyph_row_cache[slot].clear();
            self.plain_row_instances[slot].clear();
            self.region_row_instances[slot].clear();
            self.underline_row_instances[slot].clear();

            self.exposed_from = Some(self.exposed_from.map_or(slot, |held| held.min(slot)));
        }
    }

    /// Rebuild and re-upload only the changed rows' glyph instances.
    ///
    /// Unchanged rows reuse last frame's [`Self::plain_row_instances`] and
    /// [`Self::region_row_instances`]. The rows in `rebuilt` rebuild from their
    /// cached glyphs, and `rebuild_all` rebuilds every row (an atlas grow moved
    /// every UV, or text runs may grow it). A buffer that must grow is fully
    /// re-uploaded.
    ///
    /// Slots are patched where they sit, contiguous ones sharing a write, up to
    /// the lowest slot that came back a different length. That slot moved every
    /// later one's place in the buffer, so from there on the rest is rewritten
    /// as one span. Each buffer finds that slot for itself, and both start no
    /// later than a slot this frame's scroll emptied.
    ///
    /// A `region` whose rectangle differs from the one the caches were split
    /// against rebuilds every row, since the split moved.
    fn patch_rows(
        &mut self,
        device: &Device,
        queue: &Queue,
        region: Option<ScrollRegion>,
        rebuilt: &[usize],
        rebuild_all: bool,
    ) {
        let rows = self.glyph_row_cache.len();
        let split = region_split(region);
        let stale = self.plain_row_instances.len() != rows
            || self.region_row_instances.len() != rows
            || self.region_split != split;
        if stale {
            self.plain_row_instances = vec![Vec::new(); rows];
            self.region_row_instances = vec![Vec::new(); rows];
            self.region_split = split;
        }

        let (plain_from, region_from) = if rebuild_all || stale {
            for row in 0..rows {
                self.rebuild_plain_row(device, queue, row, region);
            }
            let all = (rows > 0).then_some(0);
            (all, all)
        } else {
            let rotation = self.grid_rotation();
            let mut plain_from = self.exposed_from;
            let mut region_from = self.exposed_from;
            for &row in rebuilt {
                let resized = self.rebuild_plain_row(device, queue, row, region);
                let slot = rotation.slot(row);
                if resized.plain {
                    plain_from = Some(plain_from.map_or(slot, |held| held.min(slot)));
                }
                if resized.region {
                    region_from = Some(region_from.map_or(slot, |held| held.min(slot)));
                }
            }
            (plain_from, region_from)
        };
        if plain_from.is_none() && region_from.is_none() && rebuilt.is_empty() {
            return;
        }

        let mut patched = mem::take(&mut self.patched_slots_scratch);
        slots_of_rows(self.grid_rotation(), rebuilt, &mut patched);

        let mut scratch = mem::take(&mut self.plain_upload_scratch);
        self.upload_row_buffer(device, queue, &mut scratch, &patched, plain_from, false);
        self.upload_row_buffer(device, queue, &mut scratch, &patched, region_from, true);
        self.plain_upload_scratch = scratch;
        self.patched_slots_scratch = patched;
    }

    /// Upload one of the two row buffers, growing it when its rows outgrow it.
    ///
    /// `into_region` picks which buffer, since the two are patched identically and
    /// differ only in which rows and which GPU buffer they touch.
    fn upload_row_buffer(
        &mut self,
        device: &Device,
        queue: &Queue,
        scratch: &mut Vec<TextInstance>,
        rebuilt: &[usize],
        rewrite_from: Option<usize>,
        into_region: bool,
    ) {
        let rows = if into_region {
            &self.region_row_instances
        } else {
            &self.plain_row_instances
        };
        let total = row_len(rows);
        let row_count = rows.len();

        if into_region {
            self.region_count = total as u32;
        } else {
            self.count = total as u32;
        }
        if total == 0 {
            return;
        }

        let capacity = if into_region {
            self.region_capacity
        } else {
            self.capacity
        };
        if total > capacity {
            // Growing the buffer drops its contents, so re-upload every row.
            let grown = total.next_power_of_two();
            let label = if into_region {
                "scroll region text instances"
            } else {
                "text instances"
            };
            let buffer = alloc_instances(device, label, instance_bytes::<TextInstance>(grown));
            if into_region {
                self.region_capacity = grown;
                self.region_instances = buffer;
            } else {
                self.capacity = grown;
                self.instances = buffer;
            }
            self.upload_rows(queue, scratch, 0, 0..row_count, into_region);
            return;
        }

        let rows = if into_region {
            &self.region_row_instances
        } else {
            &self.plain_row_instances
        };
        for (offset, run) in row_uploads(rows, rebuilt, rewrite_from) {
            self.upload_rows(queue, scratch, offset, run, into_region);
        }
    }

    /// Write rows `range` of one per-row cache into its instance buffer, at
    /// `offset` instances from the buffer's start.
    fn upload_rows(
        &self,
        queue: &Queue,
        scratch: &mut Vec<TextInstance>,
        offset: usize,
        range: Range<usize>,
        into_region: bool,
    ) {
        let (rows, buffer) = if into_region {
            (&self.region_row_instances, &self.region_instances)
        } else {
            (&self.plain_row_instances, &self.instances)
        };

        scratch.clear();
        scratch.extend(rows[range].iter().flatten().copied());
        if scratch.is_empty() {
            return;
        }

        queue.write_buffer(
            buffer,
            (offset * size_of::<TextInstance>()) as u64,
            bytemuck::cast_slice(scratch),
        );
    }

    /// Rebuild one plain row's text instances from its cached glyphs, leaving
    /// the glyph cache intact for the next frame.
    ///
    /// Reports whether the row's instance count changed. A row that grew or
    /// shrank displaces every later row in the buffer, so the caller can no
    /// longer patch those rows where they were.
    fn rebuild_plain_row(
        &mut self,
        device: &Device,
        queue: &Queue,
        row: usize,
        region: Option<ScrollRegion>,
    ) -> RowResized {
        let rotation = self.grid_rotation();
        let slot = rotation.slot(row);
        let mut glyphs = mem::take(&mut self.glyph_row_cache[slot]);
        // A scroll leaves a kept row's glyphs in the slot they were rasterized
        // into and moves the rotation instead, so what they name is the row they
        // were rasterized for. The slot is the authority on where they are now.
        // Both the region split and the instance's own slot are read off the row
        // below, which is why they are stamped here.
        for glyph in &mut glyphs {
            glyph.row = row;
        }

        let mut plain = mem::take(&mut self.plain_row_instances[slot]);
        let mut inside = mem::take(&mut self.region_row_instances[slot]);
        let (held_plain, held_region) = (plain.len(), inside.len());

        match region {
            // Splitting the row is what lets a region frame patch rows like any
            // other, rather than rebuilding every glyph on screen.
            Some(region) => {
                let mut outside_pending = mem::take(&mut self.plain_pending_scratch);
                let mut inside_pending = mem::take(&mut self.region_pending_scratch);
                outside_pending.clear();
                inside_pending.clear();
                for glyph in &glyphs {
                    if region.contains(glyph.row, glyph.col) {
                        inside_pending.push(*glyph);
                    } else {
                        outside_pending.push(*glyph);
                    }
                }

                self.build_text_instances_into(
                    device,
                    queue,
                    &outside_pending,
                    rotation,
                    &mut plain,
                );
                self.build_text_instances_into(
                    device,
                    queue,
                    &inside_pending,
                    rotation,
                    &mut inside,
                );
                self.plain_pending_scratch = outside_pending;
                self.region_pending_scratch = inside_pending;
            },
            None => {
                self.build_text_instances_into(device, queue, &glyphs, rotation, &mut plain);
                inside.clear();
            },
        }

        let resized = RowResized {
            plain: plain.len() != held_plain,
            region: inside.len() != held_region,
        };
        self.plain_row_instances[slot] = plain;
        self.region_row_instances[slot] = inside;
        self.glyph_row_cache[slot] = glyphs;
        resized
    }

    /// Shape and rasterize one grid row's glyphs, returning its placements.
    ///
    /// The per-row body of [`Self::rasterize_visible`]: same-style primary-covered
    /// runs shape together so ligatures form, while scaled glyphs, characters the
    /// primary font lacks, and the cursor cell shape on their own. `shaping`
    /// carries the primary family, its coverage test, and the cursor cell,
    /// resolved once by the caller.
    fn rasterize_row(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        row: usize,
        shaping: &RowShaping<'_>,
        pending: &mut Vec<PendingGlyph>,
    ) {
        let mut run = mem::take(&mut self.run_scratch);
        let mut run_text = mem::take(&mut self.run_text_scratch);
        let mut run_cols = mem::take(&mut self.run_cols_scratch);

        let mut col = 0;
        while col < grid.cols() {
            let cell = *grid.get(row, col);
            let Some(scale) = cell_glyph_scale(&cell) else {
                col += 1;
                continue;
            };

            // With ligatures off, every cell is shaped on its own. A scaled glyph
            // or a character the primary font lacks (icon, CJK) always is, through
            // the single-char path that keeps the symbols-font fallback. A
            // cell-fill codepoint is too, so its quad scales to the cell box on
            // its own. The cursor cell is too, so a ligature never spans it and
            // the character under the cursor stays visible. Only same-size
            // primary-covered cells run-shape, where ligatures form.
            if !self.ligatures
                || scale != 1
                || !(shaping.covers)(cell.ch)
                || is_cell_fill(cell.ch)
                || shaping.cursor_cell == Some((row, col))
            {
                if let Some(glyph) =
                    self.single_glyph(device, queue, &cell, row, col, f32::from(scale))
                {
                    pending.push(glyph);
                }
                col += 1;
                continue;
            }

            run.clear();
            run.push((col, cell.ch));
            let mut end = col + 1;
            while end < grid.cols() {
                let next = *grid.get(row, end);
                let groups = cell_glyph_scale(&next) == Some(1)
                    && next.fg == cell.fg
                    && next.bg == cell.bg
                    && next.flags == cell.flags
                    && (shaping.covers)(next.ch)
                    && !is_cell_fill(next.ch)
                    && shaping.cursor_cell != Some((row, end));
                if !groups {
                    break;
                }
                run.push((end, next.ch));
                end += 1;
            }

            let (fg, _) = cell.draw_colors();
            font::run_text_and_columns_into(&run, &mut run_text, &mut run_cols);
            let shaped = font::shape_run_cached(
                &mut self.run_shape_cache,
                &mut self.font_system,
                &run_text,
                self.metrics,
                shaping.primary,
            );
            for &(offset, key) in shaped {
                let Some(&glyph_col) = run_cols.get(offset) else {
                    continue;
                };
                if let Some(info) = self.atlas.get_or_insert(
                    device,
                    queue,
                    &mut self.font_system,
                    &mut self.swash_cache,
                    key,
                ) {
                    pending.push(PendingGlyph {
                        row,
                        col: glyph_col,
                        source: GlyphSource::Font(key),
                        fg,
                        scale: 1.0,
                        cell_fill: false,
                        info,
                        resolved_epoch: self.atlas.content_epoch(),
                    });
                }
            }
            col = end;
        }

        self.run_scratch = run;
        self.run_text_scratch = run_text;
        self.run_cols_scratch = run_cols;
    }

    /// Shape and rasterize one cell's glyph on its own, returning its placement.
    ///
    /// A geometric powerline separator is drawn procedurally to fill the exact
    /// cell box; every other character, including the box-drawing, block, and
    /// stylized powerline cell-fill codepoints, is shaped from the font and (for
    /// cell-fill codepoints) scaled to the cell by [`fill_cell_box`].
    fn single_glyph(
        &mut self,
        device: &Device,
        queue: &Queue,
        cell: &Cell,
        row: usize,
        col: usize,
        scale: f32,
    ) -> Option<PendingGlyph> {
        let (fg, _) = cell.draw_colors();
        let cp = u32::from(cell.ch);
        if powerline::is_geometric(cp) {
            let (width, height) = cell_fill_pixels(scale, self.metrics);
            let info =
                self.atlas
                    .get_or_insert_procedural(device, queue, cp, width, height, || {
                        powerline::rasterize(cp, width, height).unwrap_or_default()
                    })?;
            return Some(PendingGlyph {
                row,
                col,
                source: GlyphSource::Procedural { cp, width, height },
                fg,
                scale,
                cell_fill: false,
                info,
                resolved_epoch: self.atlas.content_epoch(),
            });
        }

        let key = self.glyph_key(cell.ch, scale, Weight::NORMAL)?;
        let info = self.atlas.get_or_insert(
            device,
            queue,
            &mut self.font_system,
            &mut self.swash_cache,
            key,
        )?;
        Some(PendingGlyph {
            row,
            col,
            source: GlyphSource::Font(key),
            fg,
            scale,
            cell_fill: is_cell_fill(cell.ch),
            info,
            resolved_epoch: self.atlas.content_epoch(),
        })
    }

    /// Shape and rasterize each overlay's content glyphs, returning one group of
    /// placements per overlay in overlay order.
    ///
    /// Content is laid out line by line down from the overlay's top-left at the
    /// overlay's scale and clipped to the box and the grid. The glyph color is
    /// the overlay's content color and it composites over the overlay fill.
    /// Grouping by overlay lets each popover's content be drawn in its own
    /// scissored sub-range.
    fn rasterize_overlays(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
    ) -> Vec<OverlayContent> {
        let mut groups = Vec::with_capacity(grid.overlays().len());
        let mut cells = mem::take(&mut self.overlay_cell_scratch);
        let mut cell_lines = mem::take(&mut self.overlay_cell_line_scratch);

        for overlay in grid.overlays() {
            let scale = overlay.scale.max(1);
            let weight = if overlay.bold {
                Weight::BOLD
            } else {
                Weight::NORMAL
            };
            let mut content = OverlayContent::default();

            overlay_content_cells(overlay, scale as usize, &mut cells, &mut cell_lines);

            // The cell index a line starts at is not the glyph index it starts at,
            // since blanks and off-grid cells shape to nothing. So the line index is
            // rebuilt against the glyphs as they are pushed.
            for line in cell_lines.windows(2) {
                content.starts.push(content.glyphs.len() as u32);

                for &(col, row, ch) in &cells[line[0] as usize..line[1] as usize] {
                    // No bound on the row. Content lines are laid out at
                    // increasing rows below the box's top, so a box holding more
                    // lines than the screen has rows puts its later ones past
                    // the bottom, and the popover's scroll is what brings them
                    // back up. Culling them here would leave a scrolled box
                    // drawing nothing. The column is bounded, since nothing
                    // slides a box sideways.
                    if col >= grid.cols() || ch == ' ' {
                        continue;
                    }

                    let Some(key) = self.glyph_key(ch, f32::from(scale), weight) else {
                        continue;
                    };

                    if let Some(info) = self.atlas.get_or_insert(
                        device,
                        queue,
                        &mut self.font_system,
                        &mut self.swash_cache,
                        key,
                    ) {
                        content.glyphs.push(PendingGlyph {
                            row,
                            col,
                            source: GlyphSource::Font(key),
                            fg: overlay.content_fg,
                            scale: f32::from(scale),
                            cell_fill: false,
                            info,
                            resolved_epoch: self.atlas.content_epoch(),
                        });
                    }
                }
            }

            content.starts.push(content.glyphs.len() as u32);
            groups.push(content);
        }

        self.overlay_cell_scratch = cells;
        self.overlay_cell_line_scratch = cell_lines;
        groups
    }

    /// The cached glyph cache key for `ch` at `scale`, shaping it on first use.
    /// `None` for a character that produces no glyph. The key is distinct per
    /// scale, so the atlas rasterizes each scale of a character separately.
    fn glyph_key(&mut self, ch: char, scale: f32, weight: Weight) -> Option<CacheKey> {
        let cache_key = (ch, scale.to_bits(), weight.0);
        if let Some(key) = self.shape_cache.get(&cache_key) {
            return *key;
        }

        let key = font::shape_char(
            &mut self.font_system,
            ch,
            scale,
            self.metrics,
            font::shape_family(self.family.as_deref()),
            weight,
        );
        self.shape_cache.insert(cache_key, key);
        key
    }
}

/// The per-frame shaping context [`TextPass::rasterize_row`] needs, resolved
/// once per frame and shared across rows: the primary family, a coverage test
/// for the face it resolves to, and the cursor cell that breaks ligatures.
struct RowShaping<'a> {
    primary: Family<'a>,
    covers: &'a dyn Fn(char) -> bool,
    cursor_cell: Option<(usize, usize)>,
}

/// Where a pending glyph's bitmap comes from, so
/// [`TextPass::build_text_instances`] re-resolves the same atlas entry the glyph
/// was rasterized into once every glyph this frame is packed.
///
/// A [`GlyphSource::Font`] glyph is shaped from the font and keyed by its
/// [`CacheKey`]; a [`GlyphSource::Procedural`] glyph is a powerline separator
/// drawn to fill the cell, keyed by its codepoint and cell pixel size.
#[derive(Clone, Copy, PartialEq, Debug)]
enum GlyphSource {
    Font(CacheKey),
    Procedural { cp: u32, width: u32, height: u32 },
}

/// One overlay's shaped glyphs, indexed by the content line they came from.
///
/// A box shows a window of its content rather than all of it, and the window moves
/// as the popover scrolls. Keeping the line index alongside the glyphs lets a frame
/// slice out the window it needs without walking or re-shaping the rest, which is
/// what makes the per-frame cost follow the box instead of the content.
#[derive(Default)]
struct OverlayContent {
    glyphs: Vec<PendingGlyph>,
    /// Where each content line's glyphs start in [`Self::glyphs`], with a trailing
    /// entry holding the end. The extra entry is what lets a line range slice
    /// directly, with no branch on the last line.
    starts: Vec<u32>,
}

impl OverlayContent {
    fn lines(&self) -> usize {
        self.starts.len().saturating_sub(1)
    }

    /// The glyphs belonging to a range of content lines.
    ///
    /// A window computed against a line count that has since changed yields fewer
    /// glyphs rather than panicking.
    fn window(&self, lines: Range<usize>) -> &[PendingGlyph] {
        // A start past the index means the window has fallen off the end entirely.
        // Returning here is also what leaves the end's clamp well-ordered below, by
        // bounding it from beneath with a start already known to be in range.
        let Some(&start) = self.starts.get(lines.start) else {
            return &[];
        };
        let end = self.starts[lines.end.clamp(lines.start, self.lines())] as usize;

        &self.glyphs[start as usize..end]
    }
}

/// A glyph that has been rasterized into the atlas, awaiting its final atlas
/// sub-rect once every glyph this frame is packed.
#[derive(Clone, Copy, PartialEq, Debug)]
struct PendingGlyph {
    row: usize,
    col: usize,
    source: GlyphSource,
    fg: Rgb,
    /// Multiple of the cell size this glyph is rasterized and drawn at. Integer
    /// for cell and overlay glyphs; the text-run path uses fractional scales.
    scale: f32,
    /// Whether a [`GlyphSource::Font`] cell-fill codepoint (box-drawing or
    /// block) has its quad scaled to the cell box by [`fill_cell_box`] rather
    /// than drawn at its bitmap size. A [`GlyphSource::Procedural`] glyph already
    /// fills its cell, so this stays false for one.
    cell_fill: bool,
    /// Where the glyph landed in the atlas when it was rasterized, so building its
    /// instance needs no second lookup.
    ///
    /// Only good while [`Self::resolved_epoch`] still matches the atlas, since a
    /// grow or an eviction moves every glyph's sub-rect.
    info: GlyphInfo,
    /// The atlas content epoch [`Self::info`] was read at. A build compares it
    /// before trusting the placement.
    ///
    /// Carried per glyph rather than per build because [`TextPass::glyph_row_cache`]
    /// holds these across frames, so one list can mix placements of different ages.
    resolved_epoch: u64,
}

/// What a scroll region's split of the row caches depends on, or `None` for no
/// region.
///
/// The rectangle alone. A region's `offset` moves on every scroll tick and rides the
/// globals uniform, shifting where the region's rows are drawn without changing which
/// cells fall inside it, so folding the offset in here would rebuild every row on
/// every tick for nothing.
fn region_split(region: Option<ScrollRegion>) -> Option<[u16; 4]> {
    region.map(|region| [region.top, region.left, region.width, region.height])
}

/// Which of a row's two instance lists changed length when it was rebuilt.
///
/// A row that grew or shrank displaces every later row in that buffer, so each
/// buffer's rewrite point is found independently. A row can change on one side
/// alone, when a glyph crosses the region's edge.
struct RowResized {
    plain: bool,
    region: bool,
}

/// One overlay's scissored slice of the shared overlay-content instance buffer.
///
/// `start` and `count` index [`TextPass::overlay_instances`]; `scissor` is the
/// overlay's box clamped to the surface, or `None` when the box clips to no
/// area and the content is skipped.
struct OverlayDraw {
    start: u32,
    count: u32,
    scissor: Option<[u32; 4]>,
}

/// What a region-free grid frame must do with last frame's cached plain-row
/// glyph instances.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum GridBuild {
    /// No row was damaged and the atlas is unchanged, so the cached instances
    /// still point at the right pixels and the build is skipped.
    Reuse,
    /// Only the damaged rows changed. Patch those in place and keep the rest.
    Patch,
    /// The atlas content epoch moved, so re-resolve every row.
    RebuildAll,
}

/// Decide how to rebuild the plain grid instances from whether any row was
/// damaged and how the atlas content epoch compares to the one the cached
/// instances were built against.
///
/// A changed epoch means an eviction moved some glyph, so every cached row --
/// even an undamaged one -- now points at the wrong pixels and the whole grid
/// must be re-resolved. An eviction reuses a slot without resizing the texture,
/// so only the epoch reveals it; a grow, which does resize it, leaves every
/// glyph where it was.
///
/// A moved scroll-region rectangle rebuilds for a different reason. It changes
/// which cells belong to the region buffer, and that buffer is drawn with the
/// region's scroll applied, so a cell left on the wrong side is drawn at the
/// wrong height. Patching the damaged rows would not move the undamaged ones
/// the rectangle now covers, so this is a full rebuild rather than a patch.
///
/// A scroll that emptied a slot is a patch even with nothing rebuilt. The rows
/// it kept are already where they belong and need no write, but the emptied
/// slot's instances are still in the buffer and would keep painting, now as
/// whichever row the rotation sends that slot to.
fn grid_build(
    rebuilt_empty: bool,
    slots_emptied: bool,
    region_moved: bool,
    current_epoch: u64,
    cached_epoch: u64,
) -> GridBuild {
    if current_epoch != cached_epoch || region_moved {
        GridBuild::RebuildAll
    } else if rebuilt_empty && !slots_emptied {
        GridBuild::Reuse
    } else {
        GridBuild::Patch
    }
}

fn texture_entry(binding: u32) -> BindGroupLayoutEntry {
    BindGroupLayoutEntry {
        binding,
        visibility: ShaderStages::FRAGMENT,
        ty: BindingType::Texture {
            sample_type: TextureSampleType::Float { filterable: true },
            view_dimension: TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn alloc_instances(device: &Device, label: &str, bytes: u64) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some(label),
        size: bytes,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn instance_bytes<T>(capacity: usize) -> u64 {
    (capacity * size_of::<T>()) as u64
}

/// Rows a composite has to shape for itself after sliding its caches by `by`.
///
/// A slide keeps the rows it moved and empties the ones it carried past the end,
/// and those emptied rows are what is left to shape. `None` keeps nothing, and a
/// slide of at least the row count carries everything past the end, so both
/// leave every row to shape.
fn exposed_rows(by: Option<isize>, rows: usize) -> Range<usize> {
    let Some(by) = by else {
        return 0..rows;
    };
    let vacated = by.unsigned_abs().min(rows);
    if by > 0 {
        rows - vacated..rows
    } else {
        0..vacated
    }
}

/// Empty buffers for a pool being composited for the first time.
fn new_slot(device: &Device) -> TextCompositeSlot {
    TextCompositeSlot {
        glyphs: alloc_slot::<TextInstance>(device, "composite text instances"),
        underlines: alloc_slot::<UnderlineInstance>(device, "composite underline instances"),
        text_runs: alloc_slot::<TextInstance>(device, "composite text run instances"),
        rects: alloc_slot::<RectInstance>(device, "composite text run rect instances"),
        epoch: 0,
        glyph_rows: Vec::new(),
        underline_rows: Vec::new(),
    }
}

fn alloc_slot<T>(device: &Device, label: &str) -> CompositeSlot {
    CompositeSlot {
        instances: alloc_instances(device, label, instance_bytes::<T>(INITIAL_CAPACITY)),
        capacity: INITIAL_CAPACITY,
        count: 0,
    }
}

/// Upload `instances` into `buffer`, growing it (and `capacity`) when the count
/// outgrows it. A no-op for an empty set, leaving the prior buffer in place.
fn upload_instances<T: Pod>(
    device: &Device,
    queue: &Queue,
    instances: &[T],
    buffer: &mut Buffer,
    capacity: &mut usize,
    label: &str,
) {
    if instances.is_empty() {
        return;
    }

    if instances.len() > *capacity {
        *capacity = instances.len().next_power_of_two();
        *buffer = alloc_instances(device, label, instance_bytes::<T>(*capacity));
    }
    queue.write_buffer(buffer, 0, bytemuck::cast_slice(instances));
}

/// Build the underline decoration pipeline sharing `shader` with the glyph pass.
///
/// Binds only the globals (it does not sample the atlas) and alpha-blends so the
/// painted underline shape composites over the glyphs already drawn.
fn build_underline_pipeline(
    device: &Device,
    shader: &ShaderModule,
    globals_layout: &BindGroupLayout,
    format: TextureFormat,
) -> RenderPipeline {
    let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some("underline"),
        bind_group_layouts: &[Some(globals_layout)],
        immediate_size: 0,
    });

    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some("underline"),
        layout: Some(&layout),
        vertex: VertexState {
            module: shader,
            entry_point: Some("vs_underline"),
            compilation_options: Default::default(),
            buffers: &[VertexBufferLayout {
                array_stride: size_of::<UnderlineInstance>() as u64,
                step_mode: VertexStepMode::Instance,
                attributes: &vertex_attr_array![
                    0 => Float32x2,
                    1 => Float32x3,
                    2 => Uint32,
                    3 => Uint32,
                ],
            }],
        },
        fragment: Some(FragmentState {
            module: shader,
            entry_point: Some("fs_underline"),
            compilation_options: Default::default(),
            targets: &[Some(ColorTargetState {
                format,
                blend: Some(BlendState::ALPHA_BLENDING),
                write_mask: ColorWrites::ALL,
            })],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// Build the run-background rect pipeline sharing `shader` with the glyph pass.
///
/// Binds only the globals and writes opaquely, so each scaled text run's
/// background box replaces whatever it sits over before the run's glyphs
/// alpha-blend on top.
fn build_rect_pipeline(
    device: &Device,
    shader: &ShaderModule,
    globals_layout: &BindGroupLayout,
    format: TextureFormat,
) -> RenderPipeline {
    let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some("text run rect"),
        bind_group_layouts: &[Some(globals_layout)],
        immediate_size: 0,
    });

    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some("text run rect"),
        layout: Some(&layout),
        vertex: VertexState {
            module: shader,
            entry_point: Some("vs_rect"),
            compilation_options: Default::default(),
            buffers: &[VertexBufferLayout {
                array_stride: size_of::<RectInstance>() as u64,
                step_mode: VertexStepMode::Instance,
                attributes: &vertex_attr_array![
                    0 => Float32x2,
                    1 => Float32x2,
                    2 => Float32x3,
                    3 => Uint32,
                ],
            }],
        },
        fragment: Some(FragmentState {
            module: shader,
            entry_point: Some("fs_rect"),
            compilation_options: Default::default(),
            targets: &[Some(ColorTargetState {
                format,
                blend: Some(BlendState::REPLACE),
                write_mask: ColorWrites::ALL,
            })],
        }),
        primitive: Default::default(),
        depth_stencil: None,
        multisample: Default::default(),
        multiview_mask: None,
        cache: None,
    })
}

/// Build a [`TextGlobals`] uniform buffer and its bind group over `layout`.
///
/// The pass keeps three: one per distinct per-draw `scroll_y`, all sharing the
/// group-0 layout and the shared `occluders` storage buffer at binding 1.
fn make_globals(
    device: &Device,
    layout: &BindGroupLayout,
    occluders: &Buffer,
    label: &str,
    slots: usize,
) -> (Buffer, BindGroup) {
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some(label),
        size: slots as u64 * GLOBALS_SLOT_STRIDE,
        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group = make_globals_bind_group(device, layout, &buffer, occluders, label);
    (buffer, bind_group)
}

/// Bind a globals uniform (binding 0) and the shared panel-occluder storage
/// buffer (binding 1) over `layout`. Rebuilt for each of the three globals
/// buffers whenever the occluder buffer reallocates.
fn make_globals_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    globals: &Buffer,
    occluders: &Buffer,
    label: &str,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[
            BindGroupEntry {
                binding: 0,
                // Bound to one slot's worth, so a dynamic offset selects a slot
                // rather than sliding a window over the whole buffer.
                resource: BindingResource::Buffer(BufferBinding {
                    buffer: globals,
                    offset: 0,
                    size: BufferSize::new(size_of::<TextGlobals>() as u64),
                }),
            },
            BindGroupEntry {
                binding: 1,
                resource: occluders.as_entire_binding(),
            },
        ],
    })
}

fn alloc_occluders(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("text occluders"),
        size: (capacity * size_of::<Occluder>()) as u64,
        usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn create_atlas_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    sampler: &Sampler,
    mask: &TextureView,
    color: &TextureView,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("text atlas"),
        layout,
        entries: &[
            BindGroupEntry {
                binding: 0,
                resource: BindingResource::TextureView(mask),
            },
            BindGroupEntry {
                binding: 1,
                resource: BindingResource::TextureView(color),
            },
            BindGroupEntry {
                binding: 2,
                resource: BindingResource::Sampler(sampler),
            },
        ],
    })
}

/// Screen position of a glyph bitmap's top-left in physical pixels.
///
/// The pen sits at the cell's left edge on the row baseline; `placement` is the
/// swash bitmap offset from that pen (`left` rightward, `top` upward from the
/// baseline).
///
/// The cell origin is snapped to whole pixels (the cell metrics are fractional)
/// so glyphs land on the same integer grid the background pass snaps its cells
/// to. The within-cell baseline offset is left unrounded.
fn glyph_origin(
    col: usize,
    row: usize,
    placement: [i32; 2],
    baseline: f32,
    metrics: CellMetrics,
) -> [f32; 2] {
    let pen_x = (col as f32 * metrics.width).round();
    let baseline_y = (row as f32 * metrics.height).round() + baseline;
    [
        pen_x + placement[0] as f32,
        baseline_y - placement[1] as f32,
    ]
}

/// Whether `ch` is a cell-fill codepoint: box-drawing (U+2500-257F), block
/// elements (U+2580-259F), or powerline (U+E0B0-E0D4).
///
/// These are designed to fill the cell box rather than sit on the text baseline.
/// Codepoints flagged here that are not drawn procedurally (see
/// [`powerline::is_geometric`]) have their font glyph scaled to the cell by
/// [`fill_cell_box`] instead of drawn at their bitmap size; the geometric
/// powerline separators bypass that and fill the cell exactly via
/// [`cell_box_rect`].
fn is_cell_fill(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{257F}' | '\u{2580}'..='\u{259F}' | '\u{E0B0}'..='\u{E0D4}')
}

/// Scale a cell-fill glyph's quad vertically so its em-height design fills the
/// taller cell, leaving the horizontal extent unchanged.
///
/// The glyph is rasterized at em `font_size`, but the cell is `height` (1.2x em)
/// tall, so a full-em glyph sits short of the cell with a gap above and below.
/// Scaling by `height / font_size` about the glyph's baseline maps the em box
/// onto the cell box: a full-em shape (a powerline separator, a full block)
/// fills the cell, while a line keeps its shape rather than stretching to a
/// solid fill. `scale` is the glyph's cell multiple, so a scaled block fills its
/// whole block.
fn fill_cell_box(
    pos: [f32; 2],
    dim: [f32; 2],
    row: usize,
    scale: f32,
    baseline: f32,
    metrics: CellMetrics,
) -> ([f32; 2], [f32; 2]) {
    let scale_y = metrics.height / metrics.font_size;
    let baseline_y = (row as f32 * metrics.height).round() + baseline * scale;
    (
        [pos[0], baseline_y + (pos[1] - baseline_y) * scale_y],
        [dim[0], dim[1] * scale_y],
    )
}

/// Integer pixel size to rasterize a procedural cell-fill glyph at, covering a
/// `scale`-cell block so the coverage mask matches the cell rect it fills.
fn cell_fill_pixels(scale: f32, metrics: CellMetrics) -> (u32, u32) {
    (
        (metrics.width * scale).round().max(1.0) as u32,
        (metrics.height * scale).round().max(1.0) as u32,
    )
}

/// The pixel-snapped rectangle of the `scale`-cell block at (`row`, `col`), as
/// `(top-left, [width, height])` in physical pixels.
///
/// Each edge is rounded to a whole pixel exactly as the background pass snaps
/// its cells, so a procedural cell-fill glyph shares an integer boundary with
/// the neighbouring cell backgrounds and leaves no seam.
fn cell_box_rect(row: usize, col: usize, scale: f32, metrics: CellMetrics) -> ([f32; 2], [f32; 2]) {
    let left = (col as f32 * metrics.width).round();
    let top = (row as f32 * metrics.height).round();
    let right = ((col as f32 + scale) * metrics.width).round();
    let bottom = ((row as f32 + scale) * metrics.height).round();
    ([left, top], [right - left, bottom - top])
}

/// Screen position of glyph `index` in a fractional, vertically-centered text
/// run, in physical pixels.
///
/// The run anchors at fractional cell (`col`, `row`) and advances one scaled
/// cell width per glyph. Its scaled line is centered within the target row's
/// height, so a run smaller than the grid sits aligned with full-size rows.
/// `baseline` is the unscaled cell baseline; the run scales it. At `scale ==
/// 1.0`, glyph 0 lands exactly where [`glyph_origin`] places the same cell.
///
/// The pen and the centered top are rounded to whole pixels, which is the same
/// pair [`glyph_origin`] rounds. The atlas sampler is nearest with no subpixel
/// variants, so a quad on a fractional pixel samples the bitmap off-center. A
/// scaled advance is routinely fractional, so without the rounding every other
/// glyph quantizes the other way and the run reads unevenly spaced.
///
/// Each pen is derived from `index` rather than accumulated, so the rounding
/// never compounds along the run.
fn text_run_origin(
    col: f32,
    row: f32,
    index: usize,
    scale: f32,
    placement: [i32; 2],
    baseline: f32,
    metrics: CellMetrics,
) -> [f32; 2] {
    let pen_x = ((col + index as f32 * scale) * metrics.width).round();
    let centered_top =
        (row * metrics.height + (metrics.height - metrics.height * scale) / 2.0).round();
    let baseline_y = centered_top + baseline * scale;
    [
        pen_x + placement[0] as f32,
        baseline_y - placement[1] as f32,
    ]
}

fn rgb_f32(color: Rgb) -> [f32; 3] {
    [
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
    ]
}

fn kind_flag(kind: AtlasKind) -> u32 {
    match kind {
        AtlasKind::Mask => KIND_MASK,
        AtlasKind::Color => KIND_COLOR,
    }
}

/// The integer scale to rasterize a cell's glyph at, or `None` to draw no glyph.
///
/// A blank cell and a [`Scale::Covered`] cell (inside a scaled glyph's block but
/// not its origin) draw nothing; every other cell draws at its own scale, with
/// [`Scale::Single`] meaning the normal cell size.
fn cell_glyph_scale(cell: &Cell) -> Option<u8> {
    if cell.ch == ' ' {
        return None;
    }
    match cell.scale {
        Scale::Single => Some(1),
        Scale::Origin(scale) => Some(scale),
        Scale::Covered => None,
    }
}

/// The grid cell the cursor block sits on as `(row, col)`, or `None` when the
/// cursor is hidden.
///
/// `cursor` is the eased block position in fractional cell coordinates
/// (`[col, row]`). Rounding to the nearest cell tracks the cell the block mostly
/// covers, so the break follows the visible block as it eases.
fn cursor_cell(cursor: Option<[f32; 2]>) -> Option<(usize, usize)> {
    let [col, row] = cursor?;
    Some((row.round() as usize, col.round() as usize))
}

/// The `(col, row, char)` cells an overlay's content occupies, laid out at
/// `scale` times the cell size.
///
/// Content is laid out line by line down the box from its top-left: each
/// The content is inset one cell from the box edge, matching the cell fallback,
/// so it does not jam against the border. A `\n`-separated line starts a new
/// row, its characters running rightward from that inset left edge. Each glyph
/// occupies a `scale` by `scale` cell block, so chars advance `scale` columns
/// and lines advance `scale` rows, and a line fits `(width - 2) / scale` chars
/// before the box clips it.
///
/// Every line is emitted, including those past the box height, so they can
/// scroll into view. The overlay-text draw scissors to the box to clip the
/// vertical overflow. `scale` must be at least 1.
///
/// `cells` and `line_starts` are cleared and refilled, so a caller holding them
/// across calls walks the content without allocating. `line_starts` carries where
/// each line begins in `cells`, plus a trailing entry for the end.
fn overlay_content_cells(
    overlay: &Overlay,
    scale: usize,
    cells: &mut Vec<(usize, usize, char)>,
    line_starts: &mut Vec<u32>,
) {
    cells.clear();
    line_starts.clear();

    let left = overlay.left as usize + 1;
    let top = overlay.top as usize + 1;
    let cols = (overlay.width as usize).saturating_sub(2) / scale;

    for (row, line) in overlay.content.lines().enumerate() {
        line_starts.push(cells.len() as u32);
        cells.extend(
            line.chars()
                .take(cols)
                .enumerate()
                .map(|(col, ch)| (left + col * scale, top + row * scale, ch)),
        );
    }

    line_starts.push(cells.len() as u32);
}

/// Collect into `out` the rows whose underlines rebuild this frame.
///
/// Ascending and free of duplicates, which is what lets the caller hand the same
/// list to [`row_uploads`] as the rows to patch.
///
/// `stale` says the per-row cache was just resized and holds nothing worth keeping,
/// so every row rebuilds. Full damage needs no case of its own, since it reports
/// every row dirty.
fn underline_rows_to_build(damage: &Damage, rows: usize, stale: bool, out: &mut Vec<usize>) {
    out.clear();
    if stale {
        out.extend(0..rows);
    } else {
        out.extend((0..rows).filter(|&row| damage.is_dirty(row)));
    }
}

/// The content lines `overlay`'s box can show at `scroll`, as a range over
/// `content`'s lines.
fn overlay_window(overlay: &Overlay, scroll: f32, content: &OverlayContent) -> Range<usize> {
    visible_lines(
        overlay.height,
        overlay.scale.max(1) as usize,
        scroll,
        content.lines(),
    )
}

/// The content lines an overlay's box can show, as a range over its `lines`.
///
/// A bound on work, not a clip. The scissor is what hides a glyph outside the box, so
/// this only has to cover every line that can show. It takes a line of slack on each
/// side, since a fractional `scroll` or a glyph `scale` cells tall can straddle an
/// edge, and a line included needlessly costs one line of work where a line left out
/// would go missing mid-scroll.
///
/// `scroll` is the content's offset in cells, growing as the box scrolls down.
fn visible_lines(height: u16, scale: usize, scroll: f32, lines: usize) -> Range<usize> {
    let scale = (scale.max(1)) as f32;
    let rows = f32::from(height);

    // Line `n`'s glyphs are drawn `1 + n * scale - scroll` cells below the box's top,
    // inset by the top border, and stand `scale` cells tall. They show when that span
    // meets the box's own `rows`, which bounds `n` from both sides. Both bounds are
    // strict, so the low one steps past its floor and the high one rounds up.
    let first = ((scroll - 1.0 - scale) / scale).floor() + 1.0;
    let end = ((rows - 1.0 + scroll) / scale).ceil();

    let first = if first > 0.0 { first as usize } else { 0 };
    let end = if end > 0.0 { end as usize } else { 0 };

    let first = first.min(lines);
    first..end.clamp(first, lines)
}

/// The pixel rect `[x, y, w, h]` to scissor a draw to a `width` by `height` cell
/// rectangle anchored at (`top`, `left`).
///
/// The rect is clamped to the surface, which a scissor rect requires. `None`
/// when the clamped rectangle has no area, which a zero-size scissor would
/// reject.
fn cell_rect_scissor(
    top: u16,
    left: u16,
    width: u16,
    height: u16,
    offset: [f32; 2],
    resolution: [f32; 2],
    metrics: CellMetrics,
) -> Option<[u32; 4]> {
    let res_w = resolution[0] as u32;
    let res_h = resolution[1] as u32;

    let x = ((left as f32 * metrics.width + offset[0]).max(0.0) as u32).min(res_w);
    let y = ((top as f32 * metrics.height + offset[1]).max(0.0) as u32).min(res_h);
    let w = ((width as f32 * metrics.width) as u32).min(res_w - x);
    let h = ((height as f32 * metrics.height) as u32).min(res_h - y);

    (w > 0 && h > 0).then_some([x, y, w, h])
}

/// The instance-buffer slot holding display `row` of a grid `rows` tall.
///
/// The inverse of what the vertex stages compute from a slot, so a row written
/// where this says is the row read back there. A height of zero marks a draw
/// that is not rotated, where the row is its own slot and nothing wraps.
fn row_slot(row: usize, row_offset: u32, rows: usize) -> usize {
    if rows == 0 {
        return row;
    }
    (row + row_offset as usize) % rows
}

/// The slots holding display rows `rows` under `rotation`, ascending.
///
/// The upload plan walks its rows once, advancing through the buffer as it goes,
/// so it needs them in buffer order and a descending pair would ask it to count
/// backwards. A run of display rows wraps in slot space, so rows that arrive
/// ascending do not leave that way.
fn slots_of_rows(rotation: RowRotation, rows: &[usize], out: &mut Vec<usize>) {
    out.clear();
    out.extend(rows.iter().map(|&row| rotation.slot(row)));
    out.sort_unstable();
}

/// One underline instance per underlined cell in `row`, in column order.
#[cfg(test)]
fn build_underline_row(
    grid: &Grid,
    row: usize,
    metrics: CellMetrics,
    rotation: RowRotation,
) -> Vec<UnderlineInstance> {
    let mut out = Vec::new();
    build_underline_row_into(grid, row, metrics, rotation, &mut out);
    out
}

/// Append one underline instance per underlined cell in `row` to `out`, in
/// column order. `out` is not cleared, so a caller reusing one buffer across
/// rows accumulates them and a single-row caller clears first.
///
/// `rotation` must be the one the globals of the draw these are bound to
/// carry, since each instance stores its slot rather than its row.
fn build_underline_row_into(
    grid: &Grid,
    row: usize,
    metrics: CellMetrics,
    rotation: RowRotation,
    out: &mut Vec<UnderlineInstance>,
) {
    for col in 0..grid.cols() {
        let cell = grid.get(row, col);
        let Some(style) = underline_style_flag(cell.underline) else {
            continue;
        };

        out.push(UnderlineInstance {
            cell_pos: [col as f32 * metrics.width, 0.0],
            color: rgb_f32(cell.underline_color),
            style,
            row: rotation.slot(row) as u32,
        });
    }
}

/// The shader style constant for `style`, or `None` for an un-underlined cell.
fn underline_style_flag(style: UnderlineStyle) -> Option<u32> {
    match style {
        UnderlineStyle::None => None,
        UnderlineStyle::Straight => Some(STYLE_STRAIGHT),
        UnderlineStyle::Double => Some(STYLE_DOUBLE),
        UnderlineStyle::Curly => Some(STYLE_CURLY),
        UnderlineStyle::Dotted => Some(STYLE_DOTTED),
        UnderlineStyle::Dashed => Some(STYLE_DASHED),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_underline_row, cell_glyph_scale, cell_rect_scissor, cursor_cell, exposed_rows,
        fill_cell_box, font, glyph_origin, grid_build, is_cell_fill, overlay_content_cells,
        region_split, row_len, row_slot, slots_of_rows, text_run_origin, underline_rows_to_build,
        visible_lines, GlyphSource, GridBuild, OverlayContent, PendingGlyph, RectInstance,
        RowRotation, RowShaping, TextGlobals, TextInstance, TextPass, UnderlineInstance,
        STYLE_DOTTED,
    };
    use crate::{
        atlas::{AtlasKind, GlyphInfo},
        gpu::headless_device,
        render::{row_uploads, CellMetrics, Frame, Scroll},
    };
    use stoatty_term::{
        grid::{whole_row, Cell, Grid, Overlay, Rgb, Scale, ScrollRegion, TextRun, UnderlineStyle},
        term::Damage,
    };
    use wgpu::{
        naga::{
            front::wgsl,
            valid::{Capabilities, ValidationFlags, Validator},
        },
        TextureFormat,
    };

    /// A partial damage marking each flagged row over its whole width, for a
    /// test that has row granularity and no column bounds to express.
    fn dirty_rows(flags: &[bool], cols: usize) -> Damage {
        Damage::Partial(
            flags
                .iter()
                .map(|&dirty| if dirty { whole_row(cols) } else { None })
                .collect(),
        )
    }

    #[test]
    fn grid_build_rebuilds_all_when_the_atlas_epoch_moves() {
        assert_eq!(grid_build(true, false, false, 7, 7), GridBuild::Reuse);
        assert_eq!(grid_build(false, false, false, 7, 7), GridBuild::Patch);
        assert_eq!(
            grid_build(true, false, false, 8, 7),
            GridBuild::RebuildAll,
            "an eviction moves UVs, so undamaged rows must still rebuild"
        );
        assert_eq!(grid_build(false, false, false, 8, 7), GridBuild::RebuildAll);
    }

    /// A moved region rectangle rebuilds whatever the damage says, because the
    /// cells it gained or lost have to change buffers and a patch only reaches
    /// the damaged ones.
    #[test]
    fn grid_build_rebuilds_all_when_the_region_rectangle_moves() {
        assert_eq!(grid_build(true, false, true, 7, 7), GridBuild::RebuildAll);
        assert_eq!(grid_build(false, false, true, 7, 7), GridBuild::RebuildAll);
    }

    /// An emptied slot still holds the instances of the row that scrolled off,
    /// and the rotation now sends that slot to a different row, so a frame that
    /// rebuilt nothing still has a write to make.
    #[test]
    fn grid_build_patches_when_a_scroll_emptied_a_slot() {
        assert_eq!(grid_build(true, true, false, 7, 7), GridBuild::Patch);
        assert_eq!(grid_build(false, true, false, 7, 7), GridBuild::Patch);
    }

    /// What `slot_row` computes in text.wgsl, transcribed. A rotation is only
    /// correct if writing through [`row_slot`] and reading through this round
    /// trips, and nothing here runs the shader to find out.
    fn shader_row(slot: usize, row_offset: u32, rows: usize) -> usize {
        if rows == 0 {
            return slot;
        }

        (slot + rows - row_offset as usize % rows) % rows
    }

    #[test]
    fn a_row_is_read_back_from_the_slot_it_was_written_to() {
        let rows = 5;
        for row_offset in 0..(2 * rows as u32 + 3) {
            let round_tripped: Vec<usize> = (0..rows)
                .map(|row| shader_row(row_slot(row, row_offset, rows), row_offset, rows))
                .collect();

            assert_eq!(
                round_tripped,
                (0..rows).collect::<Vec<_>>(),
                "at offset {row_offset} every row must land where the shader looks for it"
            );
        }
    }

    #[test]
    fn every_slot_holds_exactly_one_row() {
        let rows = 5;
        for row_offset in 0..(2 * rows as u32 + 3) {
            let mut slots: Vec<usize> = (0..rows)
                .map(|row| row_slot(row, row_offset, rows))
                .collect();
            slots.sort_unstable();

            assert_eq!(
                slots,
                (0..rows).collect::<Vec<_>>(),
                "at offset {row_offset} the rows must cover the buffer without two sharing a slot"
            );
        }
    }

    /// The rows a scroll kept are already where the advanced offset looks for
    /// them, which is what leaves only the exposed ones to write.
    #[test]
    fn a_scroll_leaves_the_rows_it_kept_where_the_shader_will_find_them() {
        let rows = 5;
        let scrolled = 2;
        let before = 3;
        let after = (before + scrolled) % rows as u32;

        for row in 0..rows - scrolled as usize {
            assert_eq!(
                row_slot(row, after, rows),
                row_slot(row + scrolled as usize, before, rows),
                "row {row} after the scroll reads the slot row {} was written to",
                row + scrolled as usize
            );
        }
    }

    /// The upload plan walks its rows once, advancing through the buffer as it
    /// goes, so a descending pair would ask it to count backwards. A run of
    /// display rows wraps in slot space, which is exactly where that arises.
    #[test]
    fn the_slots_of_a_wrapped_run_of_rows_come_back_ascending() {
        let rows = 6;
        let every_row: Vec<usize> = (0..rows).collect();

        for offset in 0..rows as u32 {
            let rotation = RowRotation { offset, rows };

            let mut slots = Vec::new();
            slots_of_rows(rotation, &every_row, &mut slots);
            assert_eq!(
                slots, every_row,
                "at offset {offset} every slot must appear once, ascending"
            );

            // A pair straddling the wrap, which is where ascending rows arrive
            // descending and the plan would be asked to count backwards.
            slots_of_rows(rotation, &[1, 4], &mut slots);
            assert!(
                slots[0] < slots[1],
                "at offset {offset} rows 1 and 4 land at {slots:?}"
            );
        }
    }

    /// An overlay taller than the screen names content rows past the bottom,
    /// and the popover's scroll is what brings them back up. A wrap would fold
    /// them over the box, so the screen-anchored draws rotate by nothing and
    /// the height they carry is zero.
    #[test]
    fn an_unrotated_draw_leaves_a_row_past_the_bottom_alone() {
        let unrotated = RowRotation::unrotated();

        for row in [0usize, 5, 41, 4000] {
            assert_eq!(unrotated.slot(row), row, "row {row} is its own slot");
            assert_eq!(
                shader_row(unrotated.slot(row), unrotated.offset, unrotated.rows),
                row,
                "row {row} must come back unwrapped"
            );
        }
    }

    #[test]
    fn text_shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_occlusion(include_str!(
            "../shaders/text.wgsl"
        )))
        .expect("parse text.wgsl");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate text.wgsl");
    }

    /// The vertex stage's half of the split position, mirrored so the round trip
    /// can be checked without a device.
    fn shader_pixel_y(pos_y: f32, row: u32, cell_height: f32) -> f32 {
        pos_y + row as f32 * cell_height
    }

    /// An instance stores its position measured from the top of its row, and the
    /// shader puts the row back. What the two halves must add up to is the
    /// absolute position the builder started from.
    #[test]
    fn a_row_relative_position_recombines_to_the_absolute_one() {
        // The height is fractional on purpose. The builder snaps the cell origin
        // to whole pixels, so a height that does not divide evenly is where a
        // mismatched scale between the two halves would show.
        let metrics = CellMetrics {
            font_size: 10.0,
            width: 6.0,
            height: 12.5,
            scale_factor: 1.0,
        };

        for row in 0..5usize {
            let absolute = glyph_origin(3, row, [1, 9], 10.0, metrics);
            let stored = absolute[1] - row as f32 * metrics.height;

            assert_eq!(
                shader_pixel_y(stored, row as u32, metrics.height),
                absolute[1],
                "row {row} must come back where the builder put it",
            );
        }
    }

    #[test]
    fn glyph_origin_offsets_from_cell_pen_and_baseline() {
        let metrics = CellMetrics::from_font_size(30, 1.0);
        let baseline = 14.0;

        let origin = glyph_origin(3, 2, [1, 10], baseline, metrics);
        assert_eq!(
            origin,
            [
                3.0 * metrics.width + 1.0,
                2.0 * metrics.height + baseline - 10.0
            ]
        );

        let origin = glyph_origin(0, 0, [-2, -3], baseline, metrics);
        assert_eq!(origin, [-2.0, baseline + 3.0]);
    }

    #[test]
    fn glyph_origin_snaps_the_cell_origin_to_whole_pixels() {
        // font_size 13 -> width 7.8, height 15.6, so cell origins are fractional.
        let metrics = CellMetrics::from_font_size(13, 1.0);

        // col 3 -> round(23.4) = 23, row 2 -> round(31.2) = 31; unsnapped the
        // origin would be the fractional [24.4, 39.2].
        let origin = glyph_origin(3, 2, [1, 2], 10.0, metrics);
        assert_eq!(origin, [24.0, 39.0]);
    }

    #[test]
    fn is_cell_fill_covers_box_block_and_powerline_ranges() {
        assert!(is_cell_fill('\u{2500}'), "box-drawing start");
        assert!(is_cell_fill('\u{257F}'), "box-drawing end");
        assert!(is_cell_fill('\u{2580}'), "block start");
        assert!(is_cell_fill('\u{259F}'), "block end");
        assert!(is_cell_fill('\u{E0B0}'), "powerline separator");
        assert!(is_cell_fill('\u{E0D4}'), "powerline end");

        assert!(!is_cell_fill('\u{24FF}'), "just below box-drawing");
        assert!(!is_cell_fill('\u{25A0}'), "just above block");
        assert!(!is_cell_fill('\u{E0AF}'), "just below powerline");
        assert!(!is_cell_fill('\u{E0D5}'), "just above powerline");
        assert!(!is_cell_fill('A'), "letter");
        assert!(!is_cell_fill('='), "ligature char");
    }

    #[test]
    fn fill_cell_box_scales_a_full_em_glyph_onto_the_cell() {
        // font_size 30 -> width 18, height 36, em 30, so scale_y = 1.2.
        let metrics = CellMetrics::from_font_size(30, 1.0);
        let baseline = 30.0;
        let approx =
            |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3;

        // A full-em glyph spanning [5, 35] at row 0 scales to fill the 36px cell.
        let (pos, dim) = fill_cell_box([2.0, 5.0], [8.0, 30.0], 0, 1.0, baseline, metrics);
        assert!(
            approx(pos, [2.0, 0.0]),
            "x unchanged, top at cell top: {pos:?}"
        );
        assert!(
            approx(dim, [8.0, 36.0]),
            "width kept, height fills cell: {dim:?}"
        );

        // A scaled 2x glyph fills its two-cell block.
        let (pos, dim) = fill_cell_box([0.0, 10.0], [8.0, 60.0], 0, 2.0, baseline, metrics);
        assert!(approx(pos, [0.0, 0.0]), "{pos:?}");
        assert!(approx(dim, [8.0, 72.0]), "fills two cells: {dim:?}");
    }

    #[test]
    fn text_run_origin_matches_glyph_origin_at_unit_scale() {
        let metrics = CellMetrics::from_font_size(30, 1.0);
        let baseline = 14.0;

        // The first glyph of a unit-scale run lands exactly on the cell grid, so
        // a run at scale 1 is indistinguishable from cell text.
        assert_eq!(
            text_run_origin(3.0, 2.0, 0, 1.0, [1, 10], baseline, metrics),
            glyph_origin(3, 2, [1, 10], baseline, metrics)
        );
    }

    #[test]
    fn text_run_origin_scales_advance_and_centers_in_row() {
        let metrics = CellMetrics::from_font_size(30, 1.0);
        let baseline = 14.0;

        let origin = text_run_origin(0.0, 0.0, 2, 0.5, [0, 0], baseline, metrics);

        // Two half-scale glyphs advance one cell, and the shorter line is
        // centered within the full row's height above its scaled baseline.
        assert_eq!(
            origin,
            [
                metrics.width,
                (metrics.height - metrics.height * 0.5) / 2.0 + baseline * 0.5
            ]
        );
    }

    /// The atlas sampler is nearest with no subpixel variants, so a quad off a
    /// whole pixel samples the bitmap off-center and the glyph comes out a
    /// different shape than the same glyph beside it.
    #[test]
    fn a_fractional_run_snaps_every_glyph_to_a_whole_pixel() {
        let metrics = CellMetrics::from_font_size(30, 1.0);
        let baseline = 14.0;

        // Three quarters of an 18px cell advances 13.5px, so every other pen
        // lands mid-pixel before the rounding.
        let pens: Vec<f32> = (0..5)
            .map(|index| text_run_origin(0.0, 0.0, index, 0.75, [0, 0], baseline, metrics)[0])
            .collect();

        assert!(
            pens.iter().all(|pen| pen.fract() == 0.0),
            "every glyph starts on a whole pixel: {pens:?}"
        );

        let gaps: Vec<f32> = pens.windows(2).map(|pair| pair[1] - pair[0]).collect();
        let (min, max) = gaps.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &gap| {
            (lo.min(gap), hi.max(gap))
        });
        assert!(min >= 0.0, "the run never steps backwards: {gaps:?}");
        assert!(
            max - min <= 1.0,
            "and no two gaps differ by more than a pixel: {gaps:?}"
        );
    }

    #[test]
    fn underline_instances_cover_styled_cells_only() {
        let mut grid = Grid::new(1, 3);
        grid.get_mut(0, 1).underline = UnderlineStyle::Dotted;
        grid.get_mut(0, 1).underline_color = Rgb::new(255, 0, 0);

        let metrics = CellMetrics::from_font_size(30, 1.0);
        let instances = build_underline_row(&grid, 0, metrics, RowRotation::unrotated());

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].cell_pos, [metrics.width, 0.0]);
        assert_eq!(instances[0].color, [1.0, 0.0, 0.0]);
        assert_eq!(instances[0].style, STYLE_DOTTED);
    }

    #[test]
    fn cell_glyph_scale_skips_blank_and_covered() {
        let glyph = |scale| Cell {
            ch: 'a',
            scale,
            ..Cell::default()
        };

        assert_eq!(cell_glyph_scale(&glyph(Scale::Single)), Some(1));
        assert_eq!(cell_glyph_scale(&glyph(Scale::Origin(2))), Some(2));
        assert_eq!(
            cell_glyph_scale(&glyph(Scale::Covered)),
            None,
            "covered cell draws no glyph"
        );
        assert_eq!(cell_glyph_scale(&Cell::default()), None, "blank cell");
    }

    #[test]
    fn cursor_cell_rounds_position_to_row_col() {
        assert_eq!(cursor_cell(None), None, "a hidden cursor breaks no run");
        assert_eq!(
            cursor_cell(Some([3.0, 5.0])),
            Some((5, 3)),
            "the [col, row] position maps to a (row, col) cell"
        );
        assert_eq!(
            cursor_cell(Some([3.4, 5.6])),
            Some((6, 3)),
            "a position mid-ease rounds to the nearest cell"
        );
    }

    #[test]
    fn overlay_content_cells_clip_to_box_width() {
        let overlay = Overlay {
            top: 2,
            left: 5,
            width: 3,
            height: 1,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: "Hello".to_owned(),
        };

        // Inset one cell to (6, 3), and the 3-wide box holds one char after the
        // inset trims a cell from each side.
        assert_eq!(content_cells(&overlay, 1).0, [(6, 3, 'H')]);
    }

    #[test]
    fn overlay_content_cells_space_and_clip_by_scale() {
        let overlay = Overlay {
            top: 2,
            left: 4,
            width: 6,
            height: 4,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(255, 255, 255),
            scale: 2,
            offset: [0, 0],
            bold: false,
            content: "abcd\nef".to_owned(),
        };

        // At scale 2 each glyph owns a 2x2 block, so chars advance two columns
        // and lines advance two rows. Inset one cell to (5, 3), the 6-cell box
        // holds two chars once the inset trims a cell from each side.
        assert_eq!(
            content_cells(&overlay, 2).0,
            [(5, 3, 'a'), (7, 3, 'b'), (5, 5, 'e'), (7, 5, 'f')]
        );
    }

    #[test]
    fn overlay_content_cells_emit_all_lines_clipped_to_width() {
        let overlay = Overlay {
            top: 2,
            left: 5,
            width: 3,
            height: 2,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: "abcd\nef\nXY".to_owned(),
        };

        // Every line is emitted and width-clipped. Inset one cell to (6, 3), the
        // 3-wide box holds one char per line, and all three lines still emit
        // since the scissor clips vertical overflow rather than the box height.
        let (cells, starts) = content_cells(&overlay, 1);
        assert_eq!(cells, [(6, 3, 'a'), (6, 4, 'e'), (6, 5, 'X')]);

        // One start per line plus the trailing end, so a line range slices the
        // cells directly.
        assert_eq!(starts, [0, 1, 2, 3], "each line's start, then the end");
    }

    /// The buffers are refilled rather than appended to, so a caller holding them
    /// across overlays sees only the overlay it asked about.
    #[test]
    fn overlay_content_cells_refill_the_buffers_they_are_given() {
        let overlay = |content: &str| Overlay {
            top: 2,
            left: 5,
            width: 4,
            height: 2,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: content.to_owned(),
        };

        let (mut cells, mut starts) = (Vec::new(), Vec::new());
        overlay_content_cells(&overlay("ab\ncd\nef"), 1, &mut cells, &mut starts);
        overlay_content_cells(&overlay("xy"), 1, &mut cells, &mut starts);

        assert_eq!(cells, [(6, 3, 'x'), (7, 3, 'y')], "only the second overlay");
        assert_eq!(starts, [0, 2], "one line, so one start and the end");
    }

    #[test]
    fn cell_rect_scissor_clamps_to_surface() {
        let metrics = CellMetrics::from_font_size(30, 1.0);
        let resolution = [metrics.width * 10.0, metrics.height * 5.0];

        assert_eq!(
            cell_rect_scissor(1, 2, 3, 2, [0.0, 0.0], resolution, metrics),
            Some([
                (2.0 * metrics.width) as u32,
                metrics.height as u32,
                (3.0 * metrics.width) as u32,
                (2.0 * metrics.height) as u32,
            ]),
            "a rectangle inside the surface maps cells to pixels directly"
        );

        assert_eq!(
            cell_rect_scissor(1, 2, 3, 2, [4.0, -metrics.height], resolution, metrics),
            Some([
                (2.0 * metrics.width) as u32 + 4,
                0,
                (3.0 * metrics.width) as u32,
                (2.0 * metrics.height) as u32,
            ]),
            "the offset shifts the rect and clamps a negative origin to zero"
        );

        let [x, y, w, h] = cell_rect_scissor(4, 8, 6, 4, [0.0, 0.0], resolution, metrics).unwrap();
        assert_eq!(x + w, resolution[0] as u32, "width clamps to the surface");
        assert_eq!(y + h, resolution[1] as u32, "height clamps to the surface");

        assert_eq!(
            cell_rect_scissor(5, 0, 2, 2, [0.0, 0.0], resolution, metrics),
            None,
            "an anchor at the bottom edge has no area"
        );
    }

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_occlusion(include_str!(
            "../shaders/text.wgsl"
        )))
        .expect("parse text.wgsl");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate text.wgsl");
    }

    #[test]
    fn bg_shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_occlusion(include_str!(
            "../shaders/bg.wgsl"
        )))
        .expect("parse bg.wgsl");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate bg.wgsl");
    }

    /// [`overlay_content_cells`] into fresh buffers, as (cells, line starts).
    fn content_cells(overlay: &Overlay, scale: usize) -> (Vec<(usize, usize, char)>, Vec<u32>) {
        let (mut cells, mut starts) = (Vec::new(), Vec::new());
        overlay_content_cells(overlay, scale, &mut cells, &mut starts);
        (cells, starts)
    }

    /// A text pass on the headless device, or `None` when no adapter is present.
    fn headless_text_pass() -> Option<(wgpu::Device, wgpu::Queue, TextPass)> {
        headless_text_pass_font(16)
    }

    /// A text pass at `font_size` on the headless device, or `None` when no
    /// adapter is present. A large size makes a small glyph burst overflow the
    /// initial atlas and force a grow.
    fn headless_text_pass_font(font_size: u32) -> Option<(wgpu::Device, wgpu::Queue, TextPass)> {
        let (device, queue) = headless_device()?;
        let pass = TextPass::new(
            &device,
            TextureFormat::Rgba8Unorm,
            CellMetrics::from_font_size(font_size, 1.0),
            font::build_font_system(),
            &["JetBrains Mono".to_owned()],
            true,
        );
        Some((device, queue, pass))
    }

    fn fill_row(grid: &mut Grid, row: usize, text: &str) {
        for (col, ch) in text.chars().enumerate() {
            grid.get_mut(row, col).ch = ch;
        }
    }

    /// A glyph's stored placement builds the same instance a fresh lookup would,
    /// and a stale epoch is what makes the build go and look again.
    ///
    /// The first half is the whole premise. Rasterizing already read where the glyph
    /// landed, so building from that must agree with asking the atlas a second time.
    ///
    /// The second half shows the epoch guard is load-bearing rather than decorative,
    /// by poisoning a placement and watching each epoch decide whether it is used.
    #[test]
    fn a_stored_placement_builds_what_a_fresh_lookup_would() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            eprintln!("stored placement test: no wgpu adapter available, skipping");
            return;
        };
        let mut grid = Grid::new(2, 12);
        fill_row(&mut grid, 0, "glyphs");

        let mut pending = Vec::new();
        let covers = |_: char| true;
        let family = Some("JetBrains Mono".to_owned());
        let shaping = RowShaping {
            primary: font::shape_family(family.as_deref()),
            covers: &covers,
            cursor_cell: None,
        };
        pass.rasterize_row(&device, &queue, &grid, 0, &shaping, &mut pending);
        assert!(!pending.is_empty(), "the row rasterized some glyphs");

        let mut from_stored = Vec::new();
        pass.build_text_instances_into(
            &device,
            &queue,
            &pending,
            RowRotation::unrotated(),
            &mut from_stored,
        );

        // A mismatched epoch sends every glyph back to the atlas, so this build
        // ignores what is stored and resolves each one.
        let stale: Vec<PendingGlyph> = pending
            .iter()
            .map(|glyph| PendingGlyph {
                resolved_epoch: glyph.resolved_epoch.wrapping_add(1),
                ..*glyph
            })
            .collect();
        let mut from_lookup = Vec::new();
        pass.build_text_instances_into(
            &device,
            &queue,
            &stale,
            RowRotation::unrotated(),
            &mut from_lookup,
        );

        let bytes = |instances: &[TextInstance]| {
            bytemuck::cast_slice::<TextInstance, u8>(instances).to_vec()
        };
        assert_eq!(
            bytes(&from_stored),
            bytes(&from_lookup),
            "the placement read at rasterize time matches what a second lookup gives"
        );

        // Poisoning a placement changes the instance only while its epoch still
        // matches, which is what the guard decides.
        let poison = GlyphInfo {
            kind: AtlasKind::Mask,
            uv: [0.25, 0.5, 0.75, 1.0],
            size: [3, 4],
            placement: [5, 6],
        };
        let poisoned: Vec<PendingGlyph> = pending
            .iter()
            .map(|glyph| PendingGlyph {
                info: poison,
                ..*glyph
            })
            .collect();
        let mut trusted = Vec::new();
        pass.build_text_instances_into(
            &device,
            &queue,
            &poisoned,
            RowRotation::unrotated(),
            &mut trusted,
        );

        let poisoned_stale: Vec<PendingGlyph> = poisoned
            .iter()
            .map(|glyph| PendingGlyph {
                resolved_epoch: glyph.resolved_epoch.wrapping_add(1),
                ..*glyph
            })
            .collect();
        let mut refused = Vec::new();
        pass.build_text_instances_into(
            &device,
            &queue,
            &poisoned_stale,
            RowRotation::unrotated(),
            &mut refused,
        );

        assert_ne!(
            bytes(&trusted),
            bytes(&from_stored),
            "a matching epoch means the stored placement is the one used"
        );
        assert_eq!(
            bytes(&refused),
            bytes(&from_stored),
            "a moved epoch means it is looked up again instead"
        );
    }

    #[test]
    fn run_rect_carries_its_run_occlusion_seq() {
        let Some((_device, _queue, pass)) = headless_text_pass() else {
            return;
        };
        let mut grid = Grid::new(2, 12);
        grid.set_text_runs(vec![TextRun {
            col: 0,
            row: 0,
            scale: 256,
            color: Rgb::new(1, 2, 3),
            bg: Some(Rgb::new(4, 5, 6)),
            text: "42".into(),
            seq: 42,
        }]);

        let rects = pass.build_run_rects(&grid);

        assert_eq!(rects.len(), 1);
        assert_eq!(
            rects[0].seq, 42,
            "the run rect carries its run's occlusion seq"
        );
    }

    #[test]
    fn run_without_bg_builds_no_rect() {
        let Some((_device, _queue, pass)) = headless_text_pass() else {
            return;
        };
        let mut grid = Grid::new(2, 12);
        grid.set_text_runs(vec![TextRun {
            col: 0,
            row: 0,
            scale: 256,
            color: Rgb::new(1, 2, 3),
            bg: None,
            text: "42".into(),
            seq: 42,
        }]);

        assert!(
            pass.build_run_rects(&grid).is_empty(),
            "a run with no background paints no backing rect"
        );
    }

    #[test]
    fn build_run_rects_into_clears_prior_scratch() {
        let Some((_device, _queue, pass)) = headless_text_pass() else {
            return;
        };
        let mut grid = Grid::new(2, 12);
        grid.set_text_runs(vec![TextRun {
            col: 0,
            row: 0,
            scale: 256,
            color: Rgb::new(1, 2, 3),
            bg: Some(Rgb::new(4, 5, 6)),
            text: "42".into(),
            seq: 7,
        }]);

        let fresh = pass.build_run_rects(&grid);
        assert_eq!(fresh.len(), 1, "the one backed run builds one rect");

        // A scratch buffer carrying stale rects is cleared before the rebuild, so
        // reuse yields exactly the fresh result rather than accumulating.
        let mut scratch = pass.build_run_rects(&grid);
        scratch.extend(pass.build_run_rects(&grid));
        pass.build_run_rects_into(&grid, &mut scratch);

        assert_eq!(
            bytemuck::cast_slice::<RectInstance, u8>(&scratch),
            bytemuck::cast_slice::<RectInstance, u8>(&fresh),
            "reuse clears the stale rects and rebuilds only the run's rect"
        );
    }

    /// A one-row text run whose `text` is every printable ASCII glyph, enough
    /// distinct masks at a large font to overflow the initial atlas.
    fn ascii_burst_run() -> TextRun {
        TextRun {
            col: 0,
            row: 0,
            scale: 256,
            color: Rgb::new(255, 255, 255),
            bg: None,
            text: (0x21u32..=0x7e)
                .filter_map(char::from_u32)
                .collect::<String>()
                .into(),
            seq: 0,
        }
    }

    /// Assert the composite run instances match a fresh resolve against the
    /// current atlas, so none were left frozen at a pre-grow atlas size.
    fn assert_composite_runs_healed(
        pass: &mut TextPass,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        grid: &Grid,
    ) {
        let fresh = pass.build_text_run_instances(device, queue, grid);
        assert!(!fresh.is_empty(), "the run contributes glyph instances");
        assert_eq!(
            bytemuck::cast_slice::<TextInstance, u8>(&pass.composite_run_scratch),
            bytemuck::cast_slice::<TextInstance, u8>(&fresh),
            "composite run instances must resolve against the grown atlas, not a pre-grow size"
        );
    }

    #[test]
    fn composite_runs_reresolve_when_the_run_build_grows_the_atlas() {
        let Some((device, queue, mut pass)) = headless_text_pass_font(60) else {
            return;
        };
        let mut grid = Grid::new(2, 4);
        grid.set_text_runs(vec![ascii_burst_run()]);

        let (initial, _) = pass.atlas.texture_dims();
        pass.prepare_composite(
            &device,
            &queue,
            &grid,
            &[],
            [640.0, 480.0],
            0.0,
            [0.0; 2],
            true,
            None,
            0,
            0,
        );
        let (grown, _) = pass.atlas.texture_dims();
        assert!(
            grown > initial,
            "the run's glyph burst must grow the atlas mid-build: {initial} -> {grown}"
        );

        assert_composite_runs_healed(&mut pass, &device, &queue, &grid);

        // A reuse composite returns early and leaves the healed instances intact.
        let healed = pass.composite_run_scratch.clone();
        pass.prepare_composite(
            &device,
            &queue,
            &grid,
            &[],
            [640.0, 480.0],
            0.0,
            [0.0; 2],
            false,
            None,
            0,
            0,
        );
        assert_eq!(
            bytemuck::cast_slice::<TextInstance, u8>(&pass.composite_run_scratch),
            bytemuck::cast_slice::<TextInstance, u8>(&healed),
            "a reuse composite must not disturb the healed run instances"
        );
    }

    #[test]
    fn composite_runs_reresolve_when_the_row_pack_grows_the_atlas() {
        let Some((device, queue, mut pass)) = headless_text_pass_font(60) else {
            return;
        };

        // A small run packs first, then a cell glyph burst grows the atlas
        // during the row pack, after the run instances were built. The run must
        // still land against the grown atlas, not the size it was built at.
        let mut grid = Grid::new(10, 10);
        for row in 0..10 {
            for col in 0..10 {
                let idx = (row * 10 + col) as u32;
                grid.get_mut(row, col).ch = char::from_u32(0x21 + idx % 94).unwrap_or('#');
            }
        }
        grid.set_text_runs(vec![TextRun {
            col: 0,
            row: 0,
            scale: 256,
            color: Rgb::new(255, 255, 255),
            bg: None,
            text: "42".into(),
            seq: 0,
        }]);

        let (initial, _) = pass.atlas.texture_dims();
        pass.prepare_composite(
            &device,
            &queue,
            &grid,
            &[],
            [640.0, 480.0],
            0.0,
            [0.0; 2],
            true,
            None,
            0,
            0,
        );
        let (grown, _) = pass.atlas.texture_dims();
        assert!(
            grown > initial,
            "the cell glyph burst must grow the atlas during the row pack: {initial} -> {grown}"
        );

        assert_composite_runs_healed(&mut pass, &device, &queue, &grid);
    }

    #[test]
    fn caches_clean_rows_and_rebuilds_damaged() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };
        let mut grid = Grid::new(3, 12);
        fill_row(&mut grid, 0, "a => b == c");
        fill_row(&mut grid, 1, "hello world");

        pass.rasterize_visible(
            &device,
            &queue,
            &grid,
            None,
            &Damage::Full,
            &Damage::Partial(Vec::new()),
        );

        // Change one row, then rebuild only it; the other rows come from the cache.
        fill_row(&mut grid, 1, "GOODBYE all");
        pass.rasterize_visible(
            &device,
            &queue,
            &grid,
            None,
            &dirty_rows(&[false, true, false], grid.cols()),
            &Damage::Partial(Vec::new()),
        );
        let incremental = pass.collect_grid_glyphs();

        pass.rasterize_visible(
            &device,
            &queue,
            &grid,
            None,
            &Damage::Full,
            &Damage::Partial(Vec::new()),
        );
        let full = pass.collect_grid_glyphs();

        assert_eq!(
            incremental, full,
            "rebuilding only the damaged row and reusing the rest matches a full rebuild"
        );
    }

    #[test]
    fn scaled_cells_reshape_only_damaged_rows() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };
        let mut grid = Grid::new(4, 12);
        fill_row(&mut grid, 0, "alpha");
        fill_row(&mut grid, 1, "bravo");
        fill_row(&mut grid, 2, "charlie");
        grid.place_scaled(2, 0, 2);

        // Warm the per-row cache.
        pass.rasterize_visible(
            &device,
            &queue,
            &grid,
            None,
            &Damage::Full,
            &Damage::Partial(Vec::new()),
        );

        // VT damage marks row 0; decoration damage (a scale change) marks row 1.
        // The scaled cell on row 2 must no longer force a whole-grid reshape.
        let rebuilt = pass.rasterize_visible(
            &device,
            &queue,
            &grid,
            None,
            &dirty_rows(&[true, false, false, false], grid.cols()),
            &dirty_rows(&[false, true, false, false], grid.cols()),
        );
        assert_eq!(
            rebuilt,
            vec![0, 1],
            "only the VT- and decoration-damaged rows reshape, not the scaled grid"
        );
    }

    #[test]
    fn routes_cell_fill_codepoints_by_kind() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };
        let mut grid = Grid::new(1, 4);
        grid.get_mut(0, 0).ch = '\u{E0B0}'; // geometric powerline separator
        grid.get_mut(0, 1).ch = 'M'; // ordinary glyph
        grid.get_mut(0, 2).ch = '\u{2500}'; // box-drawing, a font cell-fill glyph

        pass.rasterize_visible(
            &device,
            &queue,
            &grid,
            None,
            &Damage::Full,
            &Damage::Partial(Vec::new()),
        );
        let glyphs = pass.collect_grid_glyphs();
        let glyph = |col| glyphs.iter().find(|g| g.col == col).expect("glyph");

        assert!(
            matches!(glyph(0).source, GlyphSource::Procedural { cp: 0xE0B0, .. }),
            "a geometric powerline separator is drawn procedurally"
        );
        assert!(
            !glyph(0).cell_fill,
            "a procedural separator scales no font bitmap"
        );

        assert!(
            matches!(glyph(1).source, GlyphSource::Font(_)) && !glyph(1).cell_fill,
            "an ordinary letter shapes from the font and is not cell-fill"
        );

        assert!(
            matches!(glyph(2).source, GlyphSource::Font(_)) && glyph(2).cell_fill,
            "box-drawing stays on the font path and scales its glyph to the cell"
        );
    }

    /// One damaged row under an active region leaves both buffers holding what a
    /// full rebuild would have produced.
    ///
    /// A region is active for as long as a full-screen program is on screen, so
    /// patching its rows is the common path rather than a rare one. Patching splits
    /// each row across the two buffers, and a row's glyphs have to land on the same
    /// side and in the same order a from-scratch split puts them.
    #[test]
    fn a_patched_region_frame_matches_one_built_from_scratch() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            eprintln!("region patch test: no wgpu adapter available, skipping");
            return;
        };
        let resolution = [640.0, 480.0];
        let rows = 5;

        let region = ScrollRegion {
            top: 1,
            left: 2,
            width: 6,
            height: 3,
            offset: 0,
        };
        let build = |text: &str| {
            let mut grid = Grid::new(rows, 20);
            for row in 0..rows {
                fill_row(&mut grid, row, text);
            }
            grid.set_scroll_region(Some(region));
            grid
        };
        fn frame(damage: &Damage) -> Frame<'_> {
            Frame {
                cursor: None,
                cursor_corners: None,
                scroll: Scroll {
                    grid: 0.0,
                    document: 0.0,
                    scrollback: 0.0,
                    region: 0.0,
                    popovers: &[],
                },
                damage,
                decoration_damage: damage,
                scrolled_rows: 0,
            }
        }

        // A full frame, then one row changed and only that row damaged.
        let first = build("aaaaaaaaaa");
        pass.prepare(
            &device,
            &queue,
            &first,
            resolution,
            &frame(&Damage::Full),
            &[],
        );

        let mut second = build("aaaaaaaaaa");
        fill_row(&mut second, 2, "bbbbbbbbbb");
        let mut row_two = vec![None; rows];
        row_two[2] = whole_row(second.cols());
        pass.prepare(
            &device,
            &queue,
            &second,
            resolution,
            &frame(&Damage::Partial(row_two)),
            &[],
        );
        let patched = (
            pass.count,
            pass.region_count,
            row_len(&pass.plain_row_instances),
            row_len(&pass.region_row_instances),
        );

        // The same screen reached in one full frame, every row split from scratch.
        let Some((device, queue, mut fresh)) = headless_text_pass() else {
            return;
        };
        fresh.prepare(
            &device,
            &queue,
            &second,
            resolution,
            &frame(&Damage::Full),
            &[],
        );

        assert_eq!(
            patched,
            (
                fresh.count,
                fresh.region_count,
                row_len(&fresh.plain_row_instances),
                row_len(&fresh.region_row_instances),
            ),
            "a patched region frame splits into the same two buffers a rebuild does",
        );
        assert!(
            patched.1 > 0 && patched.0 > 0,
            "the region has to hold some glyphs and leave some outside: {patched:?}"
        );
    }

    /// Two dirty underline rows with a clean row between them travel as two writes,
    /// not one span reaching from the first of them to the end of the buffer.
    ///
    /// Driven through the same two calls the pass makes, since the coalescing is only
    /// as good as the row list handed to it. A list that included clean rows, or came
    /// out unsorted, would collapse the runs back into one.
    #[test]
    fn disjoint_dirty_underline_rows_upload_separately() {
        let underline = UnderlineInstance {
            cell_pos: [0.0, 0.0],
            color: [0.0; 3],
            style: STYLE_DOTTED,
            row: 0,
        };
        // Rows of differing length, so a write placed by summing the rows before it
        // lands somewhere a fixed stride would not.
        let cache: Vec<Vec<UnderlineInstance>> = [2, 1, 3, 1, 2]
            .iter()
            .map(|&len| vec![underline; len])
            .collect();

        let mut rows = Vec::new();
        let dirty = dirty_rows(&[false, true, false, true, false], 1);
        underline_rows_to_build(&dirty, cache.len(), false, &mut rows);
        assert_eq!(rows, [1, 3], "only the dirty rows, ascending");

        assert_eq!(
            row_uploads(&cache, &rows, None).collect::<Vec<_>>(),
            [(2, 1..2), (6, 3..4)],
            "one write per dirty row, each offset past the rows before it"
        );

        // The row that came back a different length displaced the rows after it, so
        // from there the buffer has to be rewritten rather than patched.
        assert_eq!(
            row_uploads(&cache, &rows, Some(3)).collect::<Vec<_>>(),
            [(2, 1..2), (6, 3..5)],
            "the resized row's write runs to the end and absorbs the rows after it"
        );

        underline_rows_to_build(&Damage::Full, cache.len(), false, &mut rows);
        assert_eq!(rows, [0, 1, 2, 3, 4], "full damage rebuilds every row");

        underline_rows_to_build(&dirty, cache.len(), true, &mut rows);
        assert_eq!(
            rows,
            [0, 1, 2, 3, 4],
            "a resized cache rebuilds every row whatever the damage says"
        );
    }

    /// A line range slices straight to its glyphs, and a range past the end clamps
    /// rather than indexing off the end of the index.
    ///
    /// The lines here hold two, zero, and one glyph, since a blank or off-grid line
    /// shapes to nothing while still occupying a line. A slice that assumed a glyph
    /// per line, or that let an empty line collapse, would return the wrong rows.
    #[test]
    fn overlay_content_slices_a_range_of_lines() {
        let glyph = |row: usize| PendingGlyph {
            row,
            col: 0,
            source: GlyphSource::Procedural {
                cp: 0,
                width: 1,
                height: 1,
            },
            fg: Rgb::new(0, 0, 0),
            scale: 1.0,
            cell_fill: false,
            info: GlyphInfo {
                kind: AtlasKind::Mask,
                uv: [0.0; 4],
                size: [0; 2],
                placement: [0; 2],
            },
            resolved_epoch: 0,
        };
        let content = OverlayContent {
            glyphs: vec![glyph(0), glyph(1), glyph(2)],
            starts: vec![0, 2, 2, 3],
        };
        let rows = |start, end| {
            content
                .window(start..end)
                .iter()
                .map(|glyph| glyph.row)
                .collect::<Vec<_>>()
        };

        assert_eq!(content.lines(), 3, "the trailing entry is not a line");
        assert_eq!(rows(0, 1), [0, 1], "the first line's two glyphs");
        assert_eq!(rows(1, 2), [0usize; 0], "the line that shaped to nothing");
        assert_eq!(rows(1, 3), [2], "spanning the empty line and the last");
        assert_eq!(rows(0, 3), [0, 1, 2], "every line");
        assert_eq!(rows(2, 9), [2], "an end past the last line clamps to it");
        assert_eq!(
            rows(9, 9),
            [0usize; 0],
            "a start past the last line is empty"
        );
        assert_eq!(
            rows(2, 0),
            [0usize; 0],
            "an end below the start is empty, not an inverted slice"
        );

        let empty = OverlayContent::default();
        assert_eq!(empty.lines(), 0, "no lines are held");
        assert!(
            empty.window(0..4).is_empty(),
            "an overlay with no line index at all yields no glyphs"
        );
    }

    /// The window never leaves out a line the box can show, at any scroll or scale.
    ///
    /// Brute-forced rather than spot-checked. The failure this guards is an off-by-one
    /// at a box edge during a smooth scroll, where a glyph that should have drawn goes
    /// missing for the few frames it straddles the boundary. Every scroll offset in
    /// quarter cells is checked against where each line actually lands.
    #[test]
    fn the_visible_window_covers_every_line_the_box_can_show() {
        let lines = 40;

        for scale in [1usize, 2, 4] {
            for height in [3u16, 10, 25] {
                for step in 0..(lines * scale * 4) {
                    let scroll = step as f32 / 4.0;
                    let window = visible_lines(height, scale, scroll, lines);

                    for line in 0..lines {
                        // The line's glyphs are drawn this many cells below the box's
                        // top, being its content offset inset past the border and
                        // shifted by the scroll. They show when that span meets the
                        // box's rows.
                        let top = 1.0 + (line * scale) as f32 - scroll;
                        let shows = top + scale as f32 > 0.0 && top < f32::from(height);
                        assert!(
                            !shows || window.contains(&line),
                            "line {line} shows at scroll {scroll} (scale {scale}, \
                             height {height}) but the window is {window:?}"
                        );
                    }
                }
            }
        }
    }

    /// The window stays close to the box's own size however much content sits behind
    /// it, which is the whole point of computing one.
    ///
    /// The bound is asserted tightly, within a line or two of what the box holds. The
    /// coverage test above can only catch a window that came out too narrow, since a
    /// wider one still contains every line that shows, so it takes an upper bound to
    /// notice a stray `scale` term or a missing divide.
    #[test]
    fn the_visible_window_is_sized_by_the_box_not_the_content() {
        for scale in [1usize, 2, 4] {
            for height in [3u16, 10, 25] {
                for step in 0..80 {
                    let window = visible_lines(height, scale, step as f32 / 4.0, 4000);
                    let holds = (height as usize / scale) + 2;
                    assert!(
                        window.len() <= holds,
                        "a {height}-row box at scale {scale} reads at most {holds} of \
                         4000 lines, not {}",
                        window.len()
                    );
                }
            }
        }

        assert_eq!(
            visible_lines(10, 1, 0.0, 6),
            0..6,
            "content shorter than its box is read whole"
        );
        assert_eq!(
            visible_lines(10, 1, 0.0, 0),
            0..0,
            "an empty overlay yields an empty window"
        );
        assert!(
            visible_lines(10, 1, 9_999.0, 20).is_empty(),
            "scrolled past the end there is nothing to read"
        );
    }

    /// The split the row caches are keyed on tracks the rectangle and ignores the
    /// offset.
    ///
    /// This is what keeps a scrolling region from rebuilding every row each tick.
    /// The offset moves constantly and rides the globals uniform, so it changes where
    /// the region's rows are drawn, not which cells belong to it.
    #[test]
    fn a_region_split_follows_the_rectangle_not_the_offset() {
        let rect = ScrollRegion {
            top: 1,
            left: 2,
            width: 3,
            height: 4,
            offset: 0,
        };

        assert_eq!(
            region_split(Some(ScrollRegion { offset: 99, ..rect })),
            region_split(Some(rect)),
            "a scrolled region keeps the split its rows were built against"
        );
        assert_ne!(
            region_split(Some(ScrollRegion { width: 4, ..rect })),
            region_split(Some(rect)),
            "a resized region does not"
        );
        assert_eq!(region_split(None), None, "no region has no split");
    }

    /// A moved region rectangle re-splits the rows. A moved offset does not.
    ///
    /// The offset changes on every scroll tick and rides the globals uniform, so
    /// invalidating on it would rebuild every row constantly and gain nothing. The
    /// rectangle is what decides which buffer a cell's glyph goes to.
    /// Each buffer's globals have to carry the rotation its instances were
    /// built under, since the vertex stage reads the slot back through them.
    /// The screen-anchored buffer carries none, because an overlay's content
    /// rows run past the bottom of the screen and a wrap would fold them back
    /// over the box.
    #[test]
    fn only_the_grid_rows_buffers_carry_a_rotation() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            eprintln!("globals rotation test: no wgpu adapter available, skipping");
            return;
        };
        let mut grid = Grid::new(4, 20);
        fill_row(&mut grid, 0, "cells");

        pass.prepare(
            &device,
            &queue,
            &grid,
            [640.0, 480.0],
            &Frame {
                cursor: None,
                cursor_corners: None,
                scroll: Scroll {
                    grid: 0.0,
                    document: 0.0,
                    scrollback: 0.0,
                    region: 0.0,
                    popovers: &[],
                },
                damage: &Damage::Full,
                decoration_damage: &Damage::Full,
                scrolled_rows: 0,
            },
            &[],
        );

        let rows_of = |globals: Option<TextGlobals>| globals.expect("globals written").rows;

        assert_eq!(rows_of(pass.last_globals), 4, "grid draws rotate by rows");
        assert_eq!(
            rows_of(pass.last_region_globals),
            4,
            "region draws carry grid rows too"
        );
        assert_eq!(
            rows_of(pass.last_static_globals),
            0,
            "screen-anchored draws must not wrap"
        );
    }

    /// A scroll leaves a kept row's glyphs in the slot they were rasterized
    /// into, naming the row they were rasterized for. A moved region rectangle
    /// then rebuilds every row's instances from those cached glyphs without
    /// re-rasterizing any, and the split reads the row off each glyph, so a
    /// stale one sends the glyph to the wrong buffer.
    #[test]
    fn a_scroll_then_a_moved_region_splits_like_a_build_that_never_scrolled() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            eprintln!("scrolled region split test: no wgpu adapter available, skipping");
            return;
        };
        let Some((_, _, mut fresh)) = headless_text_pass() else {
            return;
        };
        let resolution = [640.0, 480.0];
        let rows = 5;
        let lines = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

        let screen = |from: usize, region: ScrollRegion| {
            let mut grid = Grid::new(rows, 20);
            for row in 0..rows {
                fill_row(&mut grid, row, lines[from + row]);
            }
            grid.set_scroll_region(Some(region));
            grid
        };
        fn frame(damage: &Damage, scrolled_rows: isize) -> Frame<'_> {
            Frame {
                cursor: None,
                cursor_corners: None,
                scroll: Scroll {
                    grid: 0.0,
                    document: 0.0,
                    scrollback: 0.0,
                    region: 0.0,
                    popovers: &[],
                },
                damage,
                decoration_damage: damage,
                scrolled_rows,
            }
        }

        let before = ScrollRegion {
            top: 0,
            left: 0,
            width: 20,
            height: 2,
            offset: 0,
        };
        let after = ScrollRegion { top: 2, ..before };

        // Scroll by one, so every row but the last is carried rather than
        // rasterized, then move the rectangle under those carried rows.
        pass.prepare(
            &device,
            &queue,
            &screen(0, before),
            resolution,
            &frame(&Damage::Full, 0),
            &[],
        );
        let mut last_row_only = vec![None; rows];
        last_row_only[rows - 1] = whole_row(20);
        pass.prepare(
            &device,
            &queue,
            &screen(1, before),
            resolution,
            &frame(&Damage::Partial(last_row_only), 1),
            &[],
        );
        pass.prepare(
            &device,
            &queue,
            &screen(1, after),
            resolution,
            &frame(&Damage::Partial(vec![None; rows]), 0),
            &[],
        );

        fresh.prepare(
            &device,
            &queue,
            &screen(1, after),
            resolution,
            &frame(&Damage::Full, 0),
            &[],
        );

        assert_eq!(
            (
                row_len(&pass.plain_row_instances),
                row_len(&pass.region_row_instances)
            ),
            (
                row_len(&fresh.plain_row_instances),
                row_len(&fresh.region_row_instances)
            ),
            "the carried rows split by the rows they are on now"
        );
    }

    #[test]
    fn only_a_moved_region_rectangle_resplits_the_rows() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            eprintln!("region split test: no wgpu adapter available, skipping");
            return;
        };
        let resolution = [640.0, 480.0];
        let frame = Frame {
            cursor: None,
            cursor_corners: None,
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers: &[],
            },
            damage: &Damage::Partial(vec![None; 4]),
            decoration_damage: &Damage::Partial(vec![None; 4]),
            scrolled_rows: 0,
        };
        let build = |region: ScrollRegion| {
            let mut grid = Grid::new(4, 20);
            for row in 0..4 {
                fill_row(&mut grid, row, "cells");
            }
            grid.set_scroll_region(Some(region));
            grid
        };
        let rect = ScrollRegion {
            top: 1,
            left: 0,
            width: 3,
            height: 2,
            offset: 0,
        };

        let full = Frame {
            damage: &Damage::Full,
            decoration_damage: &Damage::Full,
            ..frame
        };
        pass.prepare(&device, &queue, &build(rect), resolution, &full, &[]);
        let split = (
            row_len(&pass.plain_row_instances),
            row_len(&pass.region_row_instances),
        );

        // An idle frame whose region only scrolled keeps the split it had.
        let scrolled = ScrollRegion { offset: 5, ..rect };
        pass.prepare(&device, &queue, &build(scrolled), resolution, &frame, &[]);
        assert_eq!(
            (
                row_len(&pass.plain_row_instances),
                row_len(&pass.region_row_instances)
            ),
            split,
            "a moved offset leaves the split alone"
        );

        // A wider rectangle takes cells from the plain side.
        let wider = ScrollRegion { width: 5, ..rect };
        pass.prepare(&device, &queue, &build(wider), resolution, &frame, &[]);
        let resplit = (
            row_len(&pass.plain_row_instances),
            row_len(&pass.region_row_instances),
        );
        assert!(
            resplit.1 > split.1 && resplit.0 < split.0,
            "a moved rectangle re-splits even on an idle frame: {split:?} then {resplit:?}"
        );
    }

    /// A scroll moves the rows above it without changing them, so sliding the
    /// caches has to leave those rows holding exactly what a full rebuild would
    /// have produced for their new positions.
    #[test]
    fn a_rotated_frame_matches_one_built_from_scratch() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };
        let resolution = [640.0, 480.0];
        let rows = 5;
        let lines = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

        fn fill(grid: &mut Grid, rows: usize, lines: &[&str], from: usize) {
            for row in 0..rows {
                fill_row(grid, row, lines[from + row]);
            }
        }
        fn frame(damage: &Damage, scrolled_rows: isize) -> Frame<'_> {
            Frame {
                cursor: None,
                cursor_corners: None,
                scroll: Scroll {
                    grid: 0.0,
                    document: 0.0,
                    scrollback: 0.0,
                    region: 0.0,
                    popovers: &[],
                },
                damage,
                decoration_damage: damage,
                scrolled_rows,
            }
        }

        // Build the pre-scroll screen, then scroll it by one row: the content
        // slides up and only the last row is new.
        let mut grid = Grid::new(rows, 20);
        fill(&mut grid, rows, &lines, 0);
        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&Damage::Full, 0),
            &[],
        );

        let mut scrolled = Grid::new(rows, 20);
        fill(&mut scrolled, rows, &lines, 1);
        let mut last_row_only = vec![None; rows];
        last_row_only[rows - 1] = whole_row(20);
        pass.prepare(
            &device,
            &queue,
            &scrolled,
            resolution,
            &frame(&Damage::Partial(last_row_only), 1),
            &[],
        );
        let rotated = pass.collect_grid_glyphs();

        // The same screen reached without a scroll, every row rebuilt.
        let Some((device, queue, mut fresh_pass)) = headless_text_pass() else {
            return;
        };
        fresh_pass.prepare(
            &device,
            &queue,
            &scrolled,
            resolution,
            &frame(&Damage::Full, 0),
            &[],
        );
        let fresh = fresh_pass.collect_grid_glyphs();

        assert_eq!(
            rotated.len(),
            fresh.len(),
            "a rotated screen holds as many glyphs as a rebuilt one",
        );
        for (got, want) in rotated.iter().zip(&fresh) {
            assert_eq!(
                (got.row, got.col, got.source),
                (want.row, want.col, want.source),
                "every glyph lands on the cell a rebuild would put it on, and is \
                 the glyph a rebuild would put there",
            );
        }
    }

    /// Which rows a slide leaves for the composite to shape.
    ///
    /// The sibling comparison against a from-scratch build needs a device, so on
    /// a machine without one it returns before asserting anything. This is the
    /// same arithmetic with the device taken out of it, and it is where the
    /// off-by-one lives. Naming one row too few leaves a stale row on screen,
    /// and one too many gives back the saving.
    #[test]
    fn a_slide_leaves_the_rows_it_carried_past_the_end() {
        assert_eq!(
            exposed_rows(Some(1), 5),
            4..5,
            "one row down exposes the last"
        );
        assert_eq!(exposed_rows(Some(3), 5), 2..5, "three down exposes three");
        assert_eq!(
            exposed_rows(Some(-1), 5),
            0..1,
            "one row up exposes the first"
        );
        assert_eq!(exposed_rows(Some(-3), 5), 0..3, "three up exposes three");

        assert_eq!(
            exposed_rows(Some(5), 5),
            0..5,
            "a slide of the whole height keeps nothing",
        );
        assert_eq!(
            exposed_rows(Some(9), 5),
            0..5,
            "and neither does a longer one",
        );
        assert_eq!(exposed_rows(Some(-9), 5), 0..5, "in either direction",);

        assert_eq!(exposed_rows(None, 5), 0..5, "no slide shapes every row");
        assert_eq!(exposed_rows(None, 0), 0..0, "an empty grid shapes nothing");
        assert_eq!(exposed_rows(Some(1), 0), 0..0, "nor does sliding one");
    }

    /// A pool composited after a scroll holds what one built from scratch holds.
    ///
    /// The scrolled frame slides its per-row caches and shapes only the rows the
    /// scroll exposed, so every other row keeps glyphs shaped against the frame
    /// before and the row each was shaped for is repaired rather than recomputed.
    /// A repair that is off by any amount lands those glyphs on the wrong cells,
    /// which the comparison against a rebuild catches.
    #[test]
    fn a_scrolled_composite_matches_one_built_from_scratch() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };
        let resolution = [640.0, 480.0];
        let rows = 5;
        let lines = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

        fn fill(grid: &mut Grid, rows: usize, lines: &[&str], from: usize) {
            for row in 0..rows {
                fill_row(grid, row, lines[from + row]);
            }
        }

        let mut grid = Grid::new(rows, 20);
        fill(&mut grid, rows, &lines, 0);
        pass.prepare_composite(
            &device,
            &queue,
            &grid,
            &[],
            resolution,
            0.0,
            [0.0; 2],
            true,
            None,
            0,
            0,
        );

        // The content slid up by one, so only the last row is new.
        let mut scrolled = Grid::new(rows, 20);
        fill(&mut scrolled, rows, &lines, 1);
        pass.prepare_composite(
            &device,
            &queue,
            &scrolled,
            &[],
            resolution,
            0.0,
            [0.0; 2],
            true,
            Some(1),
            0,
            0,
        );
        let carried = pass.composite_pending_scratch.clone();

        // The same rows reached without a scroll, every one of them shaped here.
        let Some((device, queue, mut fresh_pass)) = headless_text_pass() else {
            return;
        };
        fresh_pass.prepare_composite(
            &device,
            &queue,
            &scrolled,
            &[],
            resolution,
            0.0,
            [0.0; 2],
            true,
            None,
            0,
            0,
        );
        let fresh = fresh_pass.composite_pending_scratch.clone();

        assert!(!fresh.is_empty(), "the fixture has to shape something");
        assert_eq!(
            carried.len(),
            fresh.len(),
            "a scrolled composite holds as many glyphs as a rebuilt one",
        );
        for (got, want) in carried.iter().zip(&fresh) {
            assert_eq!(
                (got.row, got.col),
                (want.row, want.col),
                "every carried glyph lands on the cell a rebuild would put it on",
            );
        }
    }

    /// The scroll below stays inside the overlay's single line of content.
    ///
    /// A whole-line scroll over one line moves the window off the content
    /// entirely, which `visible_lines` answers with an empty range, so the box
    /// would correctly build nothing and the reshift path this pins would never
    /// run.
    #[test]
    fn overlays_reshift_cached_bases_and_rebuild_on_content_change() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };
        let mut grid = Grid::new(6, 20);
        let overlay = |left| Overlay {
            top: 0,
            left,
            width: 6,
            height: 3,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: "ab".to_owned(),
        };
        grid.set_overlays(vec![overlay(0)]);

        let resolution = [640.0, 480.0];
        let idle = Damage::Partial(vec![None; 6]);
        fn frame<'a>(idle: &'a Damage, popovers: &'a [f32]) -> Frame<'a> {
            Frame {
                cursor: None,
                cursor_corners: None,
                scroll: Scroll {
                    grid: 0.0,
                    document: 0.0,
                    scrollback: 0.0,
                    region: 0.0,
                    popovers,
                },
                damage: idle,
                decoration_damage: idle,
                scrolled_rows: 0,
            }
        }

        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&idle, &[0.0]),
            &[],
        );
        assert_eq!(
            pass.overlay_count, 2,
            "one two-glyph overlay builds two instances"
        );
        assert_eq!(
            pass.overlay_draws.len(),
            1,
            "one overlay records one draw range"
        );
        let scissor = pass.overlay_draws[0].scissor;

        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&idle, &[0.0]),
            &[],
        );
        assert_eq!(
            pass.overlay_count, 2,
            "an unchanged frame reuses the cached bases"
        );
        assert_eq!(pass.overlay_draws.len(), 1);

        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&idle, &[0.5]),
            &[],
        );
        assert_eq!(
            pass.overlay_count, 2,
            "a scroll-only frame re-shifts rather than rebuilds"
        );
        assert_eq!(pass.overlay_draws.len(), 1);
        assert_eq!(
            pass.overlay_draws[0].scissor, scissor,
            "the scissor is derived from geometry, so scrolling leaves it unchanged"
        );

        grid.set_overlays(vec![overlay(0), overlay(10)]);
        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&idle, &[0.0, 0.0]),
            &[],
        );
        assert_eq!(
            pass.overlay_count, 4,
            "the added overlay rebuilds to four instances"
        );
        assert_eq!(
            pass.overlay_draws.len(),
            2,
            "each overlay records its own draw range"
        );
    }

    /// An overlay taller than its box builds the box's worth of instances, not the
    /// content's.
    ///
    /// Asserted by growing the content behind an unchanged box and demanding the
    /// instance count not follow, which holds regardless of how wide the window
    /// rounds. Then the box is scrolled to a middle line, where the instances must
    /// be different ones, since a window that stayed at the top would also be
    /// small and would otherwise pass.
    #[test]
    fn an_overlay_taller_than_its_box_builds_only_the_visible_window() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };

        let lines = |count: usize| {
            (0..count)
                .map(|line| char::from(b'a' + (line % 26) as u8).to_string())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let overlay = |content: String| Overlay {
            top: 0,
            left: 0,
            width: 4,
            height: 5,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [0, 0],
            bold: false,
            content,
        };

        let resolution = [640.0, 480.0];
        let idle = Damage::Partial(vec![None; 40]);
        fn frame<'a>(idle: &'a Damage, popovers: &'a [f32]) -> Frame<'a> {
            Frame {
                cursor: None,
                cursor_corners: None,
                scroll: Scroll {
                    grid: 0.0,
                    document: 0.0,
                    scrollback: 0.0,
                    region: 0.0,
                    popovers,
                },
                damage: idle,
                decoration_damage: idle,
                scrolled_rows: 0,
            }
        }

        // One grid across the three builds. A fresh one per call would start its
        // popovers epoch back at zero every time, so the pass would see an
        // unmoved epoch and keep the first call's shaped content, and a window
        // computed for the longer content would clamp to the stale line count.
        let mut grid = Grid::new(40, 20);
        let mut built = |pass: &mut TextPass, count: usize, scroll: f32| {
            grid.set_overlays(vec![overlay(lines(count))]);

            let popovers = [scroll];
            pass.prepare(
                &device,
                &queue,
                &grid,
                resolution,
                &frame(&idle, &popovers),
                &[],
            );
            pass.overlay_instance_scratch
                .iter()
                .map(|instance| instance.pos[1])
                .collect::<Vec<_>>()
        };

        let short = built(&mut pass, 12, 0.0);
        let long = built(&mut pass, 400, 0.0);

        assert!(
            short.len() < 12,
            "a 5-row box reads fewer than the 12 lines behind it, not {}",
            short.len()
        );
        assert_eq!(
            long, short,
            "400 lines behind the same box build what 12 did"
        );

        // A window that ignored the scroll would sit at the top and be just as
        // small, so the instances have to have moved on as well as stayed few.
        //
        // Box-sized rather than equal to the unscrolled count. The window takes
        // a line of slack on each side for a glyph straddling an edge, and at
        // the top there is no line above the first to take, so an unscrolled
        // box reads one fewer than a scrolled one.
        let scrolled = built(&mut pass, 400, 200.0);
        let box_holds = 5 + 2;
        assert!(
            scrolled.len() <= box_holds,
            "the box reads a box's worth wherever it sits, not {}",
            scrolled.len()
        );
        assert!(
            scrolled.iter().all(|top| !short.contains(top)),
            "scrolled 200 lines down, the box draws different rows: {scrolled:?} \
             against {short:?}"
        );
    }

    #[test]
    fn a_rescrolled_overlay_holds_only_this_frame_s_instances() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };
        let mut grid = Grid::new(6, 20);
        grid.set_overlays(vec![Overlay {
            top: 0,
            left: 0,
            width: 6,
            height: 3,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [0, 0],
            bold: false,
            content: "ab".to_owned(),
        }]);

        let resolution = [640.0, 480.0];
        let idle = Damage::Partial(vec![None; 6]);
        fn frame<'a>(idle: &'a Damage, popovers: &'a [f32]) -> Frame<'a> {
            Frame {
                cursor: None,
                cursor_corners: None,
                scroll: Scroll {
                    grid: 0.0,
                    document: 0.0,
                    scrollback: 0.0,
                    region: 0.0,
                    popovers,
                },
                damage: idle,
                decoration_damage: idle,
                scrolled_rows: 0,
            }
        }
        let tops = |pass: &TextPass| {
            pass.overlay_instance_scratch
                .iter()
                .map(|instance| instance.pos[1])
                .collect::<Vec<_>>()
        };

        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&idle, &[0.0]),
            &[],
        );
        let unscrolled = tops(&pass);
        assert_eq!(
            unscrolled.len(),
            2,
            "the two-glyph overlay builds two instances"
        );

        // Three scroll frames in a row, so the buffers the second and third
        // build into are ones an earlier frame already filled. The offsets stay
        // within a cell, keeping the one line inside the box's window, since a
        // line scrolled out of the box builds no instances at all.
        for scroll in [0.25, 0.5, 0.75] {
            pass.prepare(
                &device,
                &queue,
                &grid,
                resolution,
                &frame(&idle, &[scroll]),
                &[],
            );
        }

        assert_eq!(
            pass.overlay_count, 2,
            "a reused instance buffer rebuilds rather than accumulates"
        );
        assert_eq!(
            pass.overlay_draws.len(),
            1,
            "a reused draw buffer holds the one overlay's range"
        );
        assert_eq!(
            (pass.overlay_draws[0].start, pass.overlay_draws[0].count),
            (0, 2),
            "the range covers this frame's instances from the buffer's start"
        );

        let want: Vec<f32> = unscrolled
            .iter()
            .map(|top| top - 0.75 * pass.metrics.height)
            .collect();
        assert_eq!(
            tops(&pass),
            want,
            "the instances are the last scroll offset's, not an earlier frame's"
        );
    }

    #[test]
    #[ignore = "timing benchmark; run with: cargo test -p stoatty_render --lib -- --ignored caches"]
    fn caching_skips_reshaping_clean_rows() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };
        let (rows, cols) = (50, 200);
        let mut grid = Grid::new(rows, cols);
        for row in 0..rows {
            let text: String = (0..cols)
                .map(|col| char::from(b'a' + (col % 26) as u8))
                .collect();
            fill_row(&mut grid, row, &text);
        }

        // Warm the per-row cache and the atlas before timing.
        pass.rasterize_visible(
            &device,
            &queue,
            &grid,
            None,
            &Damage::Full,
            &Damage::Partial(Vec::new()),
        );

        let one_dirty = {
            let mut dirty = vec![None; rows];
            dirty[rows / 2] = whole_row(cols);
            Damage::Partial(dirty)
        };

        let iterations = 50;
        let full_start = std::time::Instant::now();
        for _ in 0..iterations {
            pass.rasterize_visible(
                &device,
                &queue,
                &grid,
                None,
                &Damage::Full,
                &Damage::Partial(Vec::new()),
            );
        }
        let full = full_start.elapsed();

        let dirty_start = std::time::Instant::now();
        for _ in 0..iterations {
            pass.rasterize_visible(
                &device,
                &queue,
                &grid,
                None,
                &one_dirty,
                &Damage::Partial(Vec::new()),
            );
        }
        let dirty = dirty_start.elapsed();

        eprintln!("rasterize_visible {rows}x{cols}: full {full:?}, one dirty row {dirty:?}");
        assert!(
            dirty * 2 < full,
            "rebuilding one of {rows} rows ({dirty:?}) should beat a full rebuild ({full:?}) by over 2x"
        );
    }

    #[test]
    #[ignore = "timing benchmark; run with: cargo test -p stoatty_render --lib -- --ignored prepare_skips_unchanged_grid"]
    fn prepare_skips_unchanged_grid() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };
        let (rows, cols) = (50, 200);
        let mut grid = Grid::new(rows, cols);
        for row in 0..rows {
            let text: String = (0..cols)
                .map(|col| char::from(b'a' + (col % 26) as u8))
                .collect();
            fill_row(&mut grid, row, &text);
        }
        let resolution = [1280.0, 800.0];
        let full_damage = Damage::Full;
        let idle_damage = Damage::Partial(vec![None; rows]);
        let frame = |damage| Frame {
            cursor: None,
            cursor_corners: None,
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers: &[],
            },
            damage,
            decoration_damage: &idle_damage,
            scrolled_rows: 0,
        };

        // Warm the cache and atlas.
        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&full_damage),
            &[],
        );

        let iterations = 50;
        let full_start = std::time::Instant::now();
        for _ in 0..iterations {
            pass.prepare(
                &device,
                &queue,
                &grid,
                resolution,
                &frame(&full_damage),
                &[],
            );
        }
        let full = full_start.elapsed();

        let idle_start = std::time::Instant::now();
        for _ in 0..iterations {
            pass.prepare(
                &device,
                &queue,
                &grid,
                resolution,
                &frame(&idle_damage),
                &[],
            );
        }
        let idle = idle_start.elapsed();

        eprintln!("prepare() {rows}x{cols}: full rebuild {full:?}, unchanged grid {idle:?}");
        assert!(
            idle * 4 < full,
            "an unchanged-grid frame ({idle:?}) should beat a full rebuild ({full:?}) by over 4x"
        );
    }

    #[test]
    #[ignore = "timing measurement; run with: cargo test -p stoatty_render --lib -- --ignored cache_lookup_cost"]
    fn cache_lookup_cost() {
        let Some((device, queue, mut pass)) = headless_text_pass() else {
            return;
        };
        let (rows, cols) = (50, 200);
        let mut grid = Grid::new(rows, cols);
        for row in 0..rows {
            let text: String = (0..cols)
                .map(|col| char::from(b'a' + (col % 26) as u8))
                .collect();
            fill_row(&mut grid, row, &text);
        }
        let resolution = [1280.0, 800.0];
        let idle_damage = Damage::Partial(vec![None; rows]);
        let frame = |scroll, damage| Frame {
            cursor: None,
            cursor_corners: None,
            scroll,
            damage,
            decoration_damage: &idle_damage,
            scrolled_rows: 0,
        };
        let no_scroll = Scroll {
            grid: 0.0,
            document: 0.0,
            scrollback: 0.0,
            region: 0.0,
            popovers: &[],
        };

        // Warm the cache and atlas with a full build.
        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(no_scroll, &Damage::Full),
            &[],
        );

        // A changing scroll forces the full grid-glyph build -- every glyph's atlas
        // lookup -- each frame, but reshapes no row, isolating the cache lookups
        // from harfbuzz.
        let iterations = 100;
        let start = std::time::Instant::now();
        for i in 0..iterations {
            let scroll = Scroll {
                grid: i as f32 * 0.01,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers: &[],
            };
            pass.prepare(
                &device,
                &queue,
                &grid,
                resolution,
                &frame(scroll, &idle_damage),
                &[],
            );
        }
        let per_call = start.elapsed() / iterations;

        eprintln!("grid build without reshape {rows}x{cols}: {per_call:?} per frame");
    }
}
