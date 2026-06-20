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
    render::{CellMetrics, Frame},
};
use bytemuck::{Pod, Zeroable};
use cosmic_text::{
    fontdb::{Query, Weight},
    Attrs, Buffer as CosmicBuffer, CacheKey, Family, Font, FontSystem, Metrics, Shaping,
    SwashCache,
};
use rustc_hash::FxHashMap;
use std::sync::Arc;
use stoatty_term::{
    grid::{Cell, Grid, Overlay, Rgb, Scale, UnderlineStyle},
    term::Damage,
};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites,
    Device, FragmentState, PipelineLayoutDescriptor, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, Sampler, SamplerBindingType, SamplerDescriptor, ShaderModule,
    ShaderModuleDescriptor, ShaderSource, ShaderStages, TextureFormat, TextureSampleType,
    TextureView, TextureViewDimension, VertexBufferLayout, VertexState, VertexStepMode,
};

mod powerline;

/// Instance buffer capacity, in glyphs, allocated up front. Grows by doubling
/// when a frame exceeds it.
const INITIAL_CAPACITY: usize = 2048;

/// Atlas selector packed into each instance, matching the shader's constants.
const KIND_MASK: u32 = 0;
const KIND_COLOR: u32 = 1;

/// Underline style packed into each decoration instance, matching the shader's
/// constants.
const STYLE_STRAIGHT: u32 = 0;
const STYLE_DOUBLE: u32 = 1;
const STYLE_CURLY: u32 = 2;
const STYLE_DOTTED: u32 = 3;
const STYLE_DASHED: u32 = 4;

/// Family name of the bundled Nerd Font, registered by [`load_bundled_fonts`].
///
/// Carries the Private-Use-Area powerline separators and icon glyphs that
/// programming fonts omit, so it serves as the symbol fallback ahead of any
/// system font (see [`glyph_family`]).
const SYMBOLS_FAMILY: &str = "Symbols Nerd Font Mono";

/// Per-glyph instance: where to draw it, where to sample it, and how to color it.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TextInstance {
    /// Top-left of the glyph bitmap in physical pixels.
    pos: [f32; 2],
    /// Glyph bitmap size in physical pixels.
    dim: [f32; 2],
    /// Atlas sub-rect as `[u_min, v_min, u_max, v_max]`, normalized.
    uv: [f32; 4],
    /// Foreground color, normalized sRGB.
    fg: [f32; 3],
    /// Cell background color, normalized sRGB, the glyph composites over.
    bg: [f32; 3],
    /// Atlas to sample: [`KIND_MASK`] or [`KIND_COLOR`].
    kind: u32,
}

/// Per-underlined-cell decoration instance.
///
/// One quad per underlined cell, covering the whole cell; the fragment paints
/// only the underline shape selected by `style`.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct UnderlineInstance {
    /// Top-left of the cell in physical pixels.
    cell_pos: [f32; 2],
    /// Underline color, normalized sRGB.
    color: [f32; 3],
    /// Underline shape: one of the `STYLE_*` constants.
    style: u32,
}

/// Uniform shared by every instance: the surface resolution the vertex shader
/// maps pixel coordinates through, and the cell box the underline pass draws in.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TextGlobals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    /// Vertical scroll offset in pixels, added to each glyph's Y in the vertex
    /// shader so a scroll-only frame rewrites this uniform instead of rebuilding
    /// the glyph instances. Differs per draw: grid scroll for plain glyphs,
    /// region scroll for region glyphs, zero for screen-anchored runs/overlays.
    scroll_y: f32,
    /// Pads the struct to the 32-byte (16-aligned) size a uniform requires.
    _pad: [f32; 3],
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
    atlas_layout: BindGroupLayout,
    sampler: Sampler,
    atlas_bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    count: u32,
    overlay_instances: Buffer,
    overlay_capacity: usize,
    overlay_count: u32,
    /// One scissored sub-range of [`Self::overlay_instances`] per overlay, in
    /// overlay order, so each popover's content is clipped to its own box and
    /// several can scroll independently.
    overlay_draws: Vec<OverlayDraw>,
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
    underline_pipeline: RenderPipeline,
    underline_instances: Buffer,
    underline_capacity: usize,
    underline_count: u32,
    atlas: GlyphAtlas,
    font_system: FontSystem,
    /// The resolved primary shaping family: the first configured `font_family`
    /// entry present in the font db, or `None` to shape with the generic
    /// monospace fallback.
    family: Option<String>,
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
    shape_cache: FxHashMap<(char, u32), Option<CacheKey>>,
    /// The shaped glyphs of each grid row from the previous frame, indexed by
    /// row, so an unchanged row reuses them instead of re-shaping. Rebuilt for
    /// damaged rows, the cursor's old and new rows, and (wholesale) on resize or
    /// when scaled cells are present. Holds [`CacheKey`]s, not atlas rects, so it
    /// survives atlas growth.
    glyph_row_cache: Vec<Vec<PendingGlyph>>,
    /// The built plain-glyph instances of each row from the previous frame, so a
    /// damaged frame rebuilds and re-uploads only the rows that changed rather
    /// than every glyph on screen. Holds resolved atlas rects, so an atlas grow
    /// (which moves every UV) rebuilds all rows. Used only when no scroll region
    /// is active; a region falls back to the whole-grid build and clears this.
    plain_row_instances: Vec<Vec<TextInstance>>,
    /// The built underline instances of each row from the previous frame, so a
    /// damaged frame rebuilds and re-uploads only the changed rows. Underline is
    /// a VT cell attribute, so VT damage tracks it; scroll rides the globals
    /// uniform, so this survives a scroll-only frame.
    underline_row_instances: Vec<Vec<UnderlineInstance>>,
    /// The grid width [`Self::glyph_row_cache`] was built at; a change invalidates
    /// every cached row since columns shift.
    glyph_cache_cols: usize,
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
        let family = resolve_primary_family(&font_system, font_family);
        let baseline = probe_baseline(&mut font_system, metrics, shape_family(&family));
        let primary_font = resolve_primary_font(&mut font_system, &family);
        let swash_cache = SwashCache::new();
        let atlas = GlyphAtlas::new(device);

        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("text"),
            source: ShaderSource::Wgsl(include_str!("../shaders/text.wgsl").into()),
        });

        let globals_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("text globals"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                // The underline pipeline shares this layout and reads
                // globals.cell_size in its fragment to place the underline, so
                // globals must be visible to the fragment stage.
                visibility: ShaderStages::VERTEX_FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
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
                        4 => Float32x3,
                        5 => Uint32,
                    ],
                }],
            },
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
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
        });

        let underline_pipeline = build_underline_pipeline(device, &shader, &globals_layout, format);

        // Three globals buffers share one layout but carry a different scroll_y,
        // so the plain, region, and screen-anchored draws each scroll correctly
        // within a single render pass.
        let (globals, globals_bind_group) = make_globals(device, &globals_layout, "text globals");
        let (region_globals, region_globals_bind_group) =
            make_globals(device, &globals_layout, "text region globals");
        let (static_globals, static_globals_bind_group) =
            make_globals(device, &globals_layout, "text static globals");

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
            atlas_layout,
            sampler,
            atlas_bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            overlay_instances,
            overlay_capacity: INITIAL_CAPACITY,
            overlay_count: 0,
            overlay_draws: Vec::new(),
            region_instances,
            region_capacity: INITIAL_CAPACITY,
            region_count: 0,
            region_scissor: None,
            text_run_instances,
            text_run_capacity: INITIAL_CAPACITY,
            text_run_count: 0,
            underline_pipeline,
            underline_instances,
            underline_capacity: INITIAL_CAPACITY,
            underline_count: 0,
            atlas,
            font_system,
            family,
            primary_font,
            ligatures,
            swash_cache,
            shape_cache: FxHashMap::default(),
            glyph_row_cache: Vec::new(),
            plain_row_instances: Vec::new(),
            underline_row_instances: Vec::new(),
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
        self.baseline = probe_baseline(&mut self.font_system, metrics, shape_family(&self.family));
        self.shape_cache.clear();
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
    /// Runs in two phases: every visible glyph is rasterized first (which may
    /// grow the atlas), then each glyph's atlas sub-rect is read once the atlas
    /// has reached its final size, so normalized coordinates stay valid.
    /// Reallocates the instance buffer only when the glyph count outgrows the
    /// current capacity.
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        resolution: [f32; 2],
        frame: &Frame<'_>,
    ) {
        let cursor = frame.cursor;
        let scroll = frame.scroll;
        let damage = frame.damage;
        let decoration_damage = frame.decoration_damage;

        // Write each globals buffer with its own scroll: grid scroll for the
        // plain glyphs, region scroll for the region glyphs, none for the
        // screen-anchored runs and overlays. Done every frame so a scroll-only
        // frame refreshes the uniforms without rebuilding instances.
        let cell_size = [self.metrics.width, self.metrics.height];
        let write_globals = |buffer: &Buffer, scroll_y: f32| {
            queue.write_buffer(
                buffer,
                0,
                bytemuck::bytes_of(&TextGlobals {
                    resolution,
                    cell_size,
                    scroll_y,
                    _pad: [0.0; 3],
                }),
            );
        };
        write_globals(&self.globals, scroll.grid * self.metrics.height);
        write_globals(&self.region_globals, scroll.region * self.metrics.height);
        write_globals(&self.static_globals, 0.0);

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
        let overlay_groups = self.rasterize_overlays(device, queue, grid);

        let region = grid.scroll_region();

        // The grid-glyph instances from last frame stay valid when nothing that
        // feeds them changed: no row was rebuilt, the atlas did not grow (its size
        // drives every UV), and no text runs are present (those rasterize below,
        // after the grid instances are built, so a text-run grow could not be seen
        // here). Scroll no longer counts -- it rides the globals uniform.
        let atlas_grew = self.atlas.texture_dims() != atlas_dims;
        let grid_unchanged = rebuilt.is_empty() && !atlas_grew && grid.text_runs().is_empty();

        if !grid_unchanged {
            match region {
                // A scroll region splits each row's glyphs across the plain and
                // region buffers, so build the whole grid and drop the per-row
                // cache; the next region-free frame rebuilds it. Regions are rare,
                // so this falls back rather than tracking the split per row.
                Some(region) => {
                    let (region_pending, plain_pending): (Vec<PendingGlyph>, Vec<PendingGlyph>) =
                        self.collect_grid_glyphs()
                            .into_iter()
                            .partition(|glyph| region.contains(glyph.row, glyph.col));

                    let plain_instances = self.build_text_instances(device, queue, plain_pending);
                    let region_instances = self.build_text_instances(device, queue, region_pending);

                    self.count = plain_instances.len() as u32;
                    self.region_count = region_instances.len() as u32;
                    upload_instances(
                        device,
                        queue,
                        &plain_instances,
                        &mut self.instances,
                        &mut self.capacity,
                        "text instances",
                    );
                    upload_instances(
                        device,
                        queue,
                        &region_instances,
                        &mut self.region_instances,
                        &mut self.region_capacity,
                        "scroll region text instances",
                    );
                    self.plain_row_instances.clear();
                },
                None => {
                    let rebuild_all = atlas_grew || !grid.text_runs().is_empty();
                    self.patch_plain_rows(device, queue, &rebuilt, rebuild_all);
                },
            }
        }

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

        // Each overlay's content is concatenated into one buffer but recorded as
        // its own draw range, shifted by its own scroll offset and scissored to
        // its own box, so several popovers scroll and clip independently.
        let metrics = self.metrics;
        let mut overlay_instances = Vec::new();
        self.overlay_draws = Vec::with_capacity(overlay_groups.len());
        for (index, (overlay, group)) in grid.overlays().iter().zip(overlay_groups).enumerate() {
            let start = overlay_instances.len() as u32;
            let mut group_instances = self.build_text_instances(device, queue, group);

            // The sub-cell pixel offset shifts the content with the box, and the
            // scroll offset slides it within the box.
            let anchor = [overlay.offset[0] as f32, overlay.offset[1] as f32];
            let scroll_px = scroll.popovers.get(index).copied().unwrap_or(0.0) * metrics.height;
            for instance in &mut group_instances {
                instance.pos[0] += anchor[0];
                instance.pos[1] += anchor[1] - scroll_px;
            }

            let count = group_instances.len() as u32;
            overlay_instances.extend(group_instances);
            self.overlay_draws.push(OverlayDraw {
                start,
                count,
                scissor: cell_rect_scissor(
                    overlay.top,
                    overlay.left,
                    overlay.width,
                    overlay.height,
                    anchor,
                    resolution,
                    metrics,
                ),
            });
        }

        // Off-grid text runs are screen-anchored: no grid or region scroll
        // offset is applied, so they sit at their declared position.
        let text_run_instances = self.build_text_run_instances(device, queue, grid);

        // The plain and region grid glyphs are built and uploaded above, only
        // when changed. Overlays and text runs change independently, so they
        // rebuild every frame.
        self.overlay_count = overlay_instances.len() as u32;
        self.text_run_count = text_run_instances.len() as u32;
        upload_instances(
            device,
            queue,
            &overlay_instances,
            &mut self.overlay_instances,
            &mut self.overlay_capacity,
            "overlay text instances",
        );
        upload_instances(
            device,
            queue,
            &text_run_instances,
            &mut self.text_run_instances,
            &mut self.text_run_capacity,
            "text run instances",
        );

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
    }

    /// Build the glyph instances for `pending`, reading each glyph's final atlas
    /// sub-rect. Shared by the grid and overlay-content glyph paths.
    fn build_text_instances(
        &mut self,
        device: &Device,
        queue: &Queue,
        pending: Vec<PendingGlyph>,
    ) -> Vec<TextInstance> {
        let mut instances = Vec::with_capacity(pending.len());
        for glyph in pending {
            let Some(info) = self.resolve_glyph(device, queue, glyph.source) else {
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

            instances.push(TextInstance {
                pos,
                dim,
                uv: info.uv,
                fg: rgb_f32(glyph.fg),
                bg: rgb_f32(glyph.bg),
                kind: kind_flag(info.kind),
            });
        }
        instances
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
            GlyphSource::Procedural { cp, width, height } => self.atlas.get_or_insert_procedural(
                device,
                queue,
                &mut self.font_system,
                &mut self.swash_cache,
                cp,
                width,
                height,
                || powerline::rasterize(cp, width, height).unwrap_or_default(),
            ),
        }
    }

    /// Build the glyph instances for the grid's off-grid text runs.
    ///
    /// Each run is shaped at its fractional scale and laid out by
    /// [`text_run_origin`]: screen-anchored (no grid scroll), advancing one
    /// scaled cell width per glyph, vertically centered in its row. A
    /// non-positive scale draws nothing.
    fn build_text_run_instances(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
    ) -> Vec<TextInstance> {
        let mut instances = Vec::new();
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
                let Some(key) = self.glyph_key(ch, scale) else {
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

                instances.push(TextInstance {
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
                    bg: rgb_f32(run.bg),
                    kind: kind_flag(info.kind),
                });
            }
        }
        instances
    }

    /// Rebuild and re-upload only the damaged rows' underline instances.
    ///
    /// Underline is a VT cell attribute, so `damage` tracks it: unchanged rows
    /// reuse last frame's [`Self::underline_row_instances`], damaged rows (and
    /// every row on `Damage::Full` or a resize) rebuild, and the upload runs from
    /// the first changed row to the end. Scroll rides the globals uniform, so a
    /// scroll-only frame reuses the buffer untouched.
    fn prepare_underlines(&mut self, device: &Device, queue: &Queue, grid: &Grid, damage: &Damage) {
        let rows = grid.rows();
        let stale = self.underline_row_instances.len() != rows;
        if stale {
            self.underline_row_instances = vec![Vec::new(); rows];
        }

        let rows_to_build: Vec<usize> = if matches!(damage, Damage::Full) || stale {
            (0..rows).collect()
        } else {
            (0..rows).filter(|&row| damage.is_dirty(row)).collect()
        };
        let Some(&first) = rows_to_build.iter().min() else {
            return;
        };

        for &row in &rows_to_build {
            self.underline_row_instances[row] = build_underline_row(grid, row, self.metrics);
        }

        let offset: usize = self.underline_row_instances[..first]
            .iter()
            .map(Vec::len)
            .sum();
        let tail_len: usize = self.underline_row_instances[first..]
            .iter()
            .map(Vec::len)
            .sum();
        self.underline_count = (offset + tail_len) as u32;
        if offset + tail_len == 0 {
            return;
        }

        if offset + tail_len > self.underline_capacity {
            // Growing the buffer drops its contents, so re-upload every row.
            self.underline_capacity = (offset + tail_len).next_power_of_two();
            self.underline_instances = alloc_instances(
                device,
                "underline instances",
                instance_bytes::<UnderlineInstance>(self.underline_capacity),
            );
            let all: Vec<UnderlineInstance> = self
                .underline_row_instances
                .iter()
                .flatten()
                .copied()
                .collect();
            queue.write_buffer(&self.underline_instances, 0, bytemuck::cast_slice(&all));
        } else {
            let tail: Vec<UnderlineInstance> = self.underline_row_instances[first..]
                .iter()
                .flatten()
                .copied()
                .collect();
            let byte_offset = (offset * size_of::<UnderlineInstance>()) as u64;
            queue.write_buffer(
                &self.underline_instances,
                byte_offset,
                bytemuck::cast_slice(&tail),
            );
        }
    }

    /// Record the glyph draw, then the underline draw, into `render_pass`.
    ///
    /// A no-op until [`Self::prepare`] has run. Must run after the background
    /// pass in the same render pass: each glyph quad composites over the cell
    /// background painted underneath, and underlines alpha-blend over the glyphs.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.count > 0 {
            render_pass.set_pipeline(&self.pipeline);
            render_pass.set_bind_group(0, &self.globals_bind_group, &[]);
            render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
            render_pass.set_vertex_buffer(0, self.instances.slice(..));
            render_pass.draw(0..6, 0..self.count);
        }

        if self.underline_count > 0 {
            render_pass.set_pipeline(&self.underline_pipeline);
            render_pass.set_bind_group(0, &self.globals_bind_group, &[]);
            render_pass.set_vertex_buffer(0, self.underline_instances.slice(..));
            render_pass.draw(0..6, 0..self.underline_count);
        }
    }

    /// Record the off-grid text-run glyph draw into `render_pass`.
    ///
    /// Reuses the glyph pipeline and atlas. The runs are screen-anchored, so no
    /// scissor is set; run it after the grid text so the runs sit on top. A
    /// no-op when no text run is present.
    pub fn draw_text_runs(&self, render_pass: &mut RenderPass<'_>) {
        if self.text_run_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.static_globals_bind_group, &[]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.text_run_instances.slice(..));
        render_pass.draw(0..6, 0..self.text_run_count);
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
        render_pass.set_bind_group(0, &self.region_globals_bind_group, &[]);
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
        render_pass.set_bind_group(0, &self.static_globals_bind_group, &[]);
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
        let primary_name = self.family.clone();
        let primary = shape_family(&primary_name);
        let primary_font = self.primary_font.clone();
        let covers = |ch: char| {
            primary_font
                .as_ref()
                .is_some_and(|font| font_covers(font, ch))
        };

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
        let mut rebuilt = Vec::new();
        for row in 0..rows {
            let cursor_touched =
                cursor_moved && (left_row == Some(row) || entered_row == Some(row));
            if rebuild_all
                || damage.is_dirty(row)
                || decoration_damage.is_dirty(row)
                || cursor_touched
            {
                let row_glyphs = self.rasterize_row(device, queue, grid, row, &shaping);
                self.glyph_row_cache[row] = row_glyphs;
                rebuilt.push(row);
            }
        }

        rebuilt
    }

    /// The cached glyphs of every row, concatenated in row order, ready for
    /// [`Self::build_text_instances`].
    fn collect_grid_glyphs(&self) -> Vec<PendingGlyph> {
        self.glyph_row_cache.iter().flatten().copied().collect()
    }

    /// Rebuild and re-upload only the changed rows' plain-glyph instances.
    ///
    /// Unchanged rows reuse last frame's [`Self::plain_row_instances`]; the rows
    /// in `rebuilt` rebuild from their cached glyphs, and `rebuild_all` rebuilds
    /// every row (an atlas grow moved every UV, or text runs may grow it). Only
    /// the buffer from the first changed row to the end is uploaded, since the
    /// rows before it keep their bytes; a buffer that must grow is fully
    /// re-uploaded. Used only with no scroll region, so every glyph is plain.
    fn patch_plain_rows(
        &mut self,
        device: &Device,
        queue: &Queue,
        rebuilt: &[usize],
        rebuild_all: bool,
    ) {
        self.region_count = 0;

        let rows = self.glyph_row_cache.len();
        let stale = self.plain_row_instances.len() != rows;
        if stale {
            self.plain_row_instances = vec![Vec::new(); rows];
        }

        let rows_to_build: Vec<usize> = if rebuild_all || stale {
            (0..rows).collect()
        } else {
            rebuilt.to_vec()
        };
        let Some(&first) = rows_to_build.iter().min() else {
            return;
        };

        for &row in &rows_to_build {
            let glyphs = self.glyph_row_cache[row].clone();
            self.plain_row_instances[row] = self.build_text_instances(device, queue, glyphs);
        }

        let offset: usize = self.plain_row_instances[..first].iter().map(Vec::len).sum();
        let tail_len: usize = self.plain_row_instances[first..].iter().map(Vec::len).sum();
        self.count = (offset + tail_len) as u32;
        if offset + tail_len == 0 {
            return;
        }

        if offset + tail_len > self.capacity {
            // Growing the buffer drops its contents, so re-upload every row.
            self.capacity = (offset + tail_len).next_power_of_two();
            self.instances = alloc_instances(
                device,
                "text instances",
                instance_bytes::<TextInstance>(self.capacity),
            );
            let all: Vec<TextInstance> =
                self.plain_row_instances.iter().flatten().copied().collect();
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&all));
        } else {
            let tail: Vec<TextInstance> = self.plain_row_instances[first..]
                .iter()
                .flatten()
                .copied()
                .collect();
            let byte_offset = (offset * size_of::<TextInstance>()) as u64;
            queue.write_buffer(&self.instances, byte_offset, bytemuck::cast_slice(&tail));
        }
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
    ) -> Vec<PendingGlyph> {
        let mut pending = Vec::new();
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

            let mut run = vec![(col, cell.ch)];
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

            let (text, col_of_byte) = run_text_and_columns(&run);
            for (offset, key) in
                shape_run(&mut self.font_system, &text, self.metrics, shaping.primary)
            {
                let Some(&glyph_col) = col_of_byte.get(offset) else {
                    continue;
                };
                if self
                    .atlas
                    .get_or_insert(
                        device,
                        queue,
                        &mut self.font_system,
                        &mut self.swash_cache,
                        key,
                    )
                    .is_some()
                {
                    pending.push(PendingGlyph {
                        row,
                        col: glyph_col,
                        source: GlyphSource::Font(key),
                        fg: cell.fg,
                        bg: cell.bg,
                        scale: 1.0,
                        cell_fill: false,
                    });
                }
            }
            col = end;
        }

        pending
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
        let cp = u32::from(cell.ch);
        if powerline::is_geometric(cp) {
            let (width, height) = cell_fill_pixels(scale, self.metrics);
            self.atlas.get_or_insert_procedural(
                device,
                queue,
                &mut self.font_system,
                &mut self.swash_cache,
                cp,
                width,
                height,
                || powerline::rasterize(cp, width, height).unwrap_or_default(),
            )?;
            return Some(PendingGlyph {
                row,
                col,
                source: GlyphSource::Procedural { cp, width, height },
                fg: cell.fg,
                bg: cell.bg,
                scale,
                cell_fill: false,
            });
        }

        let key = self.glyph_key(cell.ch, scale)?;
        self.atlas.get_or_insert(
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
            fg: cell.fg,
            bg: cell.bg,
            scale,
            cell_fill: is_cell_fill(cell.ch),
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
    ) -> Vec<Vec<PendingGlyph>> {
        let mut groups = Vec::with_capacity(grid.overlays().len());

        for overlay in grid.overlays() {
            let scale = overlay.scale.max(1);
            let mut group = Vec::new();

            for (col, row, ch) in overlay_content_cells(overlay, scale as usize) {
                if row >= grid.rows() || col >= grid.cols() || ch == ' ' {
                    continue;
                }

                let Some(key) = self.glyph_key(ch, f32::from(scale)) else {
                    continue;
                };

                if self
                    .atlas
                    .get_or_insert(
                        device,
                        queue,
                        &mut self.font_system,
                        &mut self.swash_cache,
                        key,
                    )
                    .is_some()
                {
                    group.push(PendingGlyph {
                        row,
                        col,
                        source: GlyphSource::Font(key),
                        fg: overlay.content_fg,
                        bg: overlay.fill,
                        scale: f32::from(scale),
                        cell_fill: false,
                    });
                }
            }

            groups.push(group);
        }

        groups
    }

    /// The cached glyph cache key for `ch` at `scale`, shaping it on first use.
    /// `None` for a character that produces no glyph. The key is distinct per
    /// scale, so the atlas rasterizes each scale of a character separately.
    fn glyph_key(&mut self, ch: char, scale: f32) -> Option<CacheKey> {
        let cache_key = (ch, scale.to_bits());
        if let Some(key) = self.shape_cache.get(&cache_key) {
            return *key;
        }

        let key = shape_char(
            &mut self.font_system,
            ch,
            scale,
            self.metrics,
            shape_family(&self.family),
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

/// A glyph that has been rasterized into the atlas, awaiting its final atlas
/// sub-rect once every glyph this frame is packed.
#[derive(Clone, Copy, PartialEq, Debug)]
struct PendingGlyph {
    row: usize,
    col: usize,
    source: GlyphSource,
    fg: Rgb,
    bg: Rgb,
    /// Multiple of the cell size this glyph is rasterized and drawn at. Integer
    /// for cell and overlay glyphs; the text-run path uses fractional scales.
    scale: f32,
    /// Whether a [`GlyphSource::Font`] cell-fill codepoint (box-drawing or
    /// block) has its quad scaled to the cell box by [`fill_cell_box`] rather
    /// than drawn at its bitmap size. A [`GlyphSource::Procedural`] glyph already
    /// fills its cell, so this stays false for one.
    cell_fill: bool,
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

/// Upload `instances` into `buffer`, growing it (and `capacity`) when the count
/// outgrows it. A no-op for an empty set, leaving the prior buffer in place.
fn upload_instances(
    device: &Device,
    queue: &Queue,
    instances: &[TextInstance],
    buffer: &mut Buffer,
    capacity: &mut usize,
    label: &str,
) {
    if instances.is_empty() {
        return;
    }

    if instances.len() > *capacity {
        *capacity = instances.len().next_power_of_two();
        *buffer = alloc_instances(device, label, instance_bytes::<TextInstance>(*capacity));
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

/// Build a [`TextGlobals`] uniform buffer and its bind group over `layout`.
///
/// The pass keeps three: one per distinct per-draw `scroll_y`, all sharing the
/// group-0 layout.
fn make_globals(device: &Device, layout: &BindGroupLayout, label: &str) -> (Buffer, BindGroup) {
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some(label),
        size: size_of::<TextGlobals>() as u64,
        usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let bind_group = device.create_bind_group(&BindGroupDescriptor {
        label: Some(label),
        layout,
        entries: &[BindGroupEntry {
            binding: 0,
            resource: buffer.as_entire_binding(),
        }],
    });
    (buffer, bind_group)
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

/// Shape `ch` on its own at `scale` times the cell size and return its glyph
/// cache key, or `None` if it produces no glyph.
///
/// One character maps to one cell, so each is shaped independently rather than
/// through proportional line layout. The cache key encodes the rasterization
/// size, so each scale of a character keys a distinct atlas entry.
///
/// `primary` is the preferred family; glyphs it lacks are shaped with the
/// bundled symbols font instead (see [`glyph_family`]).
fn shape_char(
    font_system: &mut FontSystem,
    ch: char,
    scale: f32,
    metrics: CellMetrics,
    primary: Family<'_>,
) -> Option<CacheKey> {
    let family = glyph_family(font_system, ch, primary);
    let size = scale;
    let mut buffer = CosmicBuffer::new(
        font_system,
        Metrics::new(metrics.font_size * size, metrics.height * size),
    );
    let mut encoded = [0u8; 4];
    let text = ch.encode_utf8(&mut encoded);
    buffer.set_text(
        font_system,
        text,
        &Attrs::new().family(family),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);

    let run = buffer.layout_runs().next()?;
    let glyph = run.glyphs.first()?;
    Some(glyph.physical((0.0, 0.0), 1.0).cache_key)
}

/// Shape `text` as one run with `family`, returning each glyph's source byte
/// offset paired with its cache key.
///
/// Shaping the run as a single string lets the font's contextual alternates
/// merge adjacent characters into ligature glyphs. The returned byte offset is
/// the start of each glyph's source cluster, which maps the glyph back to the
/// column it begins at. A ligature glyph maps to the column of its first
/// character. Each glyph is keyed at subpixel bin zero, since the grid draws it
/// at an integer cell origin.
fn shape_run(
    font_system: &mut FontSystem,
    text: &str,
    metrics: CellMetrics,
    family: Family<'_>,
) -> Vec<(usize, CacheKey)> {
    let mut buffer =
        CosmicBuffer::new(font_system, Metrics::new(metrics.font_size, metrics.height));
    buffer.set_text(
        font_system,
        text,
        &Attrs::new().family(family),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);

    let Some(run) = buffer.layout_runs().next() else {
        return Vec::new();
    };
    run.glyphs
        .iter()
        .map(|glyph| {
            let pixel_aligned = (-(glyph.x + glyph.font_size * glyph.x_offset), 0.0);
            (glyph.start, glyph.physical(pixel_aligned, 1.0).cache_key)
        })
        .collect()
}

/// The run's shaping string and a per-byte map from string offset to grid column.
///
/// Each cell contributes its character. Every byte of that character maps to the
/// cell's column, so a shaped glyph's [`start`](cosmic_text::LayoutGlyph::start)
/// byte resolves to the column it originates at, even across multi-byte
/// characters.
fn run_text_and_columns(cells: &[(usize, char)]) -> (String, Vec<usize>) {
    let mut text = String::new();
    let mut col_of_byte = Vec::new();
    for &(col, ch) in cells {
        text.push(ch);
        col_of_byte.resize(text.len(), col);
    }
    (text, col_of_byte)
}

/// The cosmic-text family to shape `ch` with: `primary` when it carries the
/// glyph, otherwise the bundled symbols font so Private-Use-Area icons resolve
/// to it ahead of cosmic-text's system fallback.
fn glyph_family<'a>(font_system: &mut FontSystem, ch: char, primary: Family<'a>) -> Family<'a> {
    if family_covers(font_system, primary, ch) {
        primary
    } else {
        Family::Name(SYMBOLS_FAMILY)
    }
}

/// Whether the face that `family` resolves to in `font_system` has a glyph for
/// `ch`.
///
/// Checks the resolved face's character map directly, so the answer reflects the
/// face that would actually shape `ch` rather than cosmic-text's fallback chain.
fn family_covers(font_system: &mut FontSystem, family: Family<'_>, ch: char) -> bool {
    let Some(id) = font_system.db().query(&Query {
        families: &[family],
        ..Default::default()
    }) else {
        return false;
    };

    font_system
        .get_font(id, Weight::NORMAL)
        .is_some_and(|font| font_covers(&font, ch))
}

/// Whether `font` has a glyph for `ch`, read from its character map.
fn font_covers(font: &Font, ch: char) -> bool {
    font.as_swash().charmap().map(ch) != 0
}

/// Resolve the primary shaping `family` to its face, for the per-cell coverage
/// test. `None` when no family resolves, so coverage falls through to the
/// fallback font. Looked up once when the family is set, since it is fixed for
/// the pass's lifetime.
fn resolve_primary_font(
    font_system: &mut FontSystem,
    family: &Option<String>,
) -> Option<Arc<Font>> {
    let id = font_system.db().query(&Query {
        families: &[shape_family(family)],
        ..Default::default()
    })?;
    font_system.get_font(id, Weight::NORMAL)
}

/// Build the [`FontSystem`] a [`TextPass`] shapes with: cosmic-text's system
/// font enumeration plus the bundled fonts.
///
/// Enumerating the system fonts dominates renderer startup, and this needs no
/// window or GPU, so it is run on a background thread (see
/// [`GpuContext::new`](crate::gpu::GpuContext::new)) concurrently with the
/// main-thread surface and device setup.
pub fn build_font_system() -> FontSystem {
    let mut font_system = FontSystem::new();
    load_bundled_fonts(&mut font_system);
    font_system
}

/// Register the bundled faces into `font_system`'s font database so they resolve
/// regardless of which fonts are installed system-wide: the JetBrains Mono
/// variable faces (the `JetBrains Mono` family) and the Symbols Nerd Font Mono
/// symbol face ([`SYMBOLS_FAMILY`]) that backs the Private-Use-Area fallback.
fn load_bundled_fonts(font_system: &mut FontSystem) {
    const REGULAR: &[u8] =
        include_bytes!("../../assets/fonts/JetBrainsMono/JetBrainsMono[wght].ttf");
    const ITALIC: &[u8] =
        include_bytes!("../../assets/fonts/JetBrainsMono/JetBrainsMono-Italic[wght].ttf");
    const SYMBOLS: &[u8] =
        include_bytes!("../../assets/fonts/SymbolsNerdFont/SymbolsNerdFontMono-Regular.ttf");

    let db = font_system.db_mut();
    db.load_font_data(REGULAR.to_vec());
    db.load_font_data(ITALIC.to_vec());
    db.load_font_data(SYMBOLS.to_vec());
}

/// The first family in `cascade` present in `font_system`'s db, or `None` when
/// none are installed so shaping falls back to the generic monospace.
fn resolve_primary_family(font_system: &FontSystem, cascade: &[String]) -> Option<String> {
    let db = font_system.db();
    cascade
        .iter()
        .find(|name| {
            db.query(&Query {
                families: &[Family::Name(name.as_str())],
                ..Default::default()
            })
            .is_some()
        })
        .cloned()
}

/// The cosmic-text family to shape with: the resolved primary by name, or the
/// generic monospace when no configured family was present.
fn shape_family(family: &Option<String>) -> Family<'_> {
    family.as_deref().map_or(Family::Monospace, Family::Name)
}

/// Baseline offset from a cell's top, in physical pixels, measured once from the
/// font so glyphs sit on a consistent baseline within their cell.
fn probe_baseline(font_system: &mut FontSystem, metrics: CellMetrics, family: Family<'_>) -> f32 {
    let mut buffer =
        CosmicBuffer::new(font_system, Metrics::new(metrics.font_size, metrics.height));
    buffer.set_text(
        font_system,
        "M",
        &Attrs::new().family(family),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);
    buffer
        .layout_runs()
        .next()
        .map(|run| run.line_y)
        .unwrap_or(metrics.height * 0.8)
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
fn text_run_origin(
    col: f32,
    row: f32,
    index: usize,
    scale: f32,
    placement: [i32; 2],
    baseline: f32,
    metrics: CellMetrics,
) -> [f32; 2] {
    let pen_x = (col + index as f32 * scale) * metrics.width;
    let centered_top = row * metrics.height + (metrics.height - metrics.height * scale) / 2.0;
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
/// `\n`-separated line starts a new row, its characters running rightward from
/// the left edge. Each glyph occupies a `scale` by `scale` cell block, so chars
/// advance `scale` columns and lines advance `scale` rows, and a line fits
/// `width / scale` chars before the box clips it. Every line is emitted,
/// including those past the box height, so they can scroll into view; the
/// overlay-text draw scissors to the box to clip the vertical overflow. `scale`
/// must be at least 1.
fn overlay_content_cells(overlay: &Overlay, scale: usize) -> Vec<(usize, usize, char)> {
    let left = overlay.left as usize;
    let top = overlay.top as usize;
    let cols = overlay.width as usize / scale;

    overlay
        .content
        .lines()
        .enumerate()
        .flat_map(|(row, line)| {
            line.chars()
                .take(cols)
                .enumerate()
                .map(move |(col, ch)| (left + col * scale, top + row * scale, ch))
        })
        .collect()
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

/// One underline instance per underlined cell in `row`, in column order.
fn build_underline_row(grid: &Grid, row: usize, metrics: CellMetrics) -> Vec<UnderlineInstance> {
    (0..grid.cols())
        .filter_map(|col| {
            let cell = grid.get(row, col);
            let style = underline_style_flag(cell.underline)?;
            Some(UnderlineInstance {
                cell_pos: [col as f32 * metrics.width, row as f32 * metrics.height],
                color: rgb_f32(cell.underline_color),
                style,
            })
        })
        .collect()
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
        build_font_system, build_underline_row, cell_glyph_scale, cell_rect_scissor, cursor_cell,
        fill_cell_box, glyph_family, glyph_origin, is_cell_fill, load_bundled_fonts,
        overlay_content_cells, resolve_primary_family, run_text_and_columns, shape_family,
        shape_run, text_run_origin, GlyphSource, TextPass, STYLE_DOTTED, SYMBOLS_FAMILY,
    };
    use crate::{
        gpu::headless_device,
        render::{CellMetrics, Frame, Scroll},
    };
    use cosmic_text::{
        fontdb::{Database, Query},
        Family, FontSystem,
    };
    use stoatty_term::{
        grid::{Cell, Grid, Overlay, Rgb, Scale, UnderlineStyle},
        term::Damage,
    };
    use wgpu::{
        naga::{
            front::wgsl,
            valid::{Capabilities, ValidationFlags, Validator},
        },
        TextureFormat,
    };

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

    #[test]
    fn underline_instances_cover_styled_cells_only() {
        let mut grid = Grid::new(1, 3);
        grid.get_mut(0, 1).underline = UnderlineStyle::Dotted;
        grid.get_mut(0, 1).underline_color = Rgb::new(255, 0, 0);

        let metrics = CellMetrics::from_font_size(30, 1.0);
        let instances = build_underline_row(&grid, 0, metrics);

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
            content: "Hello".to_owned(),
        };

        assert_eq!(
            overlay_content_cells(&overlay, 1),
            [(5, 2, 'H'), (6, 2, 'e'), (7, 2, 'l')]
        );
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
            content: "abcd\nef".to_owned(),
        };

        // At scale 2 each glyph owns a 2x2 block: chars advance two columns,
        // lines advance two rows, and the 6-cell-wide box fits three chars.
        assert_eq!(
            overlay_content_cells(&overlay, 2),
            [
                (4, 2, 'a'),
                (6, 2, 'b'),
                (8, 2, 'c'),
                (4, 4, 'e'),
                (6, 4, 'f')
            ]
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
            content: "abcd\nef\nXY".to_owned(),
        };

        // Every line is emitted and width-clipped. The box height no longer
        // drops the third line, since the scissor now clips vertical overflow.
        assert_eq!(
            overlay_content_cells(&overlay, 1),
            [
                (5, 2, 'a'),
                (6, 2, 'b'),
                (7, 2, 'c'),
                (5, 3, 'e'),
                (6, 3, 'f'),
                (5, 4, 'X'),
                (6, 4, 'Y'),
            ]
        );
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
    fn bundled_fonts_make_jetbrains_mono_resolvable() {
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);

        assert!(
            font_system
                .db()
                .query(&Query {
                    families: &[Family::Name("JetBrains Mono")],
                    ..Default::default()
                })
                .is_some(),
            "bundled faces resolve JetBrains Mono in an otherwise empty font db"
        );
    }

    #[test]
    fn glyph_family_falls_back_to_symbols_font_for_uncovered_glyphs() {
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);
        let primary = Family::Name("JetBrains Mono");

        assert_eq!(
            glyph_family(&mut font_system, 'A', primary),
            primary,
            "a glyph the primary family carries shapes with the primary"
        );
        assert_eq!(
            glyph_family(&mut font_system, '\u{e0b6}', primary),
            Family::Name(SYMBOLS_FAMILY),
            "a Private-Use-Area powerline glyph the primary lacks routes to the symbols font"
        );
    }

    #[test]
    fn shape_run_forms_ligatures_and_maps_clusters() {
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);
        let metrics = CellMetrics::from_font_size(16, 1.0);
        let jbm = Family::Name("JetBrains Mono");

        let offsets: Vec<usize> = shape_run(&mut font_system, "ab", metrics, jbm)
            .iter()
            .map(|(offset, _)| *offset)
            .collect();
        assert_eq!(
            offsets,
            [0, 1],
            "non-ligating characters map to their source byte offsets"
        );

        let alone = shape_run(&mut font_system, "=", metrics, jbm);
        let ligated = shape_run(&mut font_system, "=>", metrics, jbm);
        assert_eq!(alone.len(), 1, "a lone = shapes to one glyph");
        assert_ne!(
            alone[0].1.glyph_id, ligated[0].1.glyph_id,
            "shaping => as a run substitutes the = via calt, so the ligature forms across cells"
        );
        assert_eq!(
            ligated[0].0, 0,
            "the ligature's first glyph maps back to the run's first column"
        );
    }

    #[test]
    fn run_text_and_columns_maps_each_byte_to_its_cell() {
        let (text, col_of_byte) = run_text_and_columns(&[(3, 'a'), (4, '世'), (5, 'b')]);

        assert_eq!(text, "a世b");
        assert_eq!(
            col_of_byte,
            [3, 4, 4, 4, 5],
            "the three bytes of 世 all map to its column, so a glyph's start byte resolves correctly"
        );
    }

    #[test]
    fn resolve_primary_family_picks_first_present_then_falls_back() {
        let mut font_system = FontSystem::new_with_locale_and_db("en-US".into(), Database::new());
        load_bundled_fonts(&mut font_system);

        assert_eq!(
            resolve_primary_family(
                &font_system,
                &["Nonexistent Face".to_owned(), "JetBrains Mono".to_owned()],
            ),
            Some("JetBrains Mono".to_owned()),
            "skips the missing family and resolves the first present one"
        );
        assert_eq!(
            resolve_primary_family(&font_system, &["Nonexistent Face".to_owned()]),
            None,
            "a cascade with no present family resolves to None"
        );
        assert_eq!(
            resolve_primary_family(&font_system, &[]),
            None,
            "an empty cascade resolves to None"
        );
    }

    #[test]
    fn shape_family_maps_resolved_name_else_monospace() {
        assert_eq!(
            shape_family(&Some("JetBrains Mono".to_owned())),
            Family::Name("JetBrains Mono")
        );
        assert_eq!(shape_family(&None), Family::Monospace);
    }

    #[test]
    fn shader_is_valid_wgsl() {
        let module =
            wgsl::parse_str(include_str!("../shaders/text.wgsl")).expect("parse text.wgsl");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate text.wgsl");
    }

    #[test]
    fn bg_shader_is_valid_wgsl() {
        let module = wgsl::parse_str(include_str!("../shaders/bg.wgsl")).expect("parse bg.wgsl");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate bg.wgsl");
    }

    /// A text pass on the headless device, or `None` when no adapter is present.
    fn headless_text_pass() -> Option<(wgpu::Device, wgpu::Queue, TextPass)> {
        let (device, queue) = headless_device()?;
        let pass = TextPass::new(
            &device,
            TextureFormat::Rgba8Unorm,
            CellMetrics::from_font_size(16, 1.0),
            build_font_system(),
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
            &Damage::Partial(vec![false, true, false]),
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
            &Damage::Partial(vec![true, false, false, false]),
            &Damage::Partial(vec![false, true, false, false]),
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
            let mut dirty = vec![false; rows];
            dirty[rows / 2] = true;
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
        let idle_damage = Damage::Partial(vec![false; rows]);
        let frame = |damage| Frame {
            cursor: None,
            scroll: Scroll {
                grid: 0.0,
                region: 0.0,
                popovers: &[],
            },
            damage,
            decoration_damage: &idle_damage,
        };

        // Warm the cache and atlas.
        pass.prepare(&device, &queue, &grid, resolution, &frame(&full_damage));

        let iterations = 50;
        let full_start = std::time::Instant::now();
        for _ in 0..iterations {
            pass.prepare(&device, &queue, &grid, resolution, &frame(&full_damage));
        }
        let full = full_start.elapsed();

        let idle_start = std::time::Instant::now();
        for _ in 0..iterations {
            pass.prepare(&device, &queue, &grid, resolution, &frame(&idle_damage));
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
        let idle_damage = Damage::Partial(vec![false; rows]);
        let frame = |scroll, damage| Frame {
            cursor: None,
            scroll,
            damage,
            decoration_damage: &idle_damage,
        };
        let no_scroll = Scroll {
            grid: 0.0,
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
        );

        // A changing scroll forces the full grid-glyph build -- every glyph's atlas
        // lookup -- each frame, but reshapes no row, isolating the cache lookups
        // from harfbuzz.
        let iterations = 100;
        let start = std::time::Instant::now();
        for i in 0..iterations {
            let scroll = Scroll {
                grid: i as f32 * 0.01,
                region: 0.0,
                popovers: &[],
            };
            pass.prepare(
                &device,
                &queue,
                &grid,
                resolution,
                &frame(scroll, &idle_damage),
            );
        }
        let per_call = start.elapsed() / iterations;

        eprintln!("grid build without reshape {rows}x{cols}: {per_call:?} per frame");
    }
}
