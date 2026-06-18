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
    atlas::{AtlasKind, GlyphAtlas},
    render::{CellMetrics, Scroll},
};
use bytemuck::{Pod, Zeroable};
use cosmic_text::{
    fontdb::{Query, Weight},
    Attrs, Buffer as CosmicBuffer, CacheKey, Family, Font, FontSystem, Metrics, Shaping,
    SwashCache,
};
use std::collections::HashMap;
use stoatty_term::grid::{Cell, Grid, Overlay, Rgb, Scale, UnderlineStyle};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites,
    Device, FragmentState, PipelineLayoutDescriptor, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, Sampler, SamplerBindingType, SamplerDescriptor, ShaderModule,
    ShaderModuleDescriptor, ShaderSource, ShaderStages, TextureFormat, TextureSampleType,
    TextureView, TextureViewDimension, VertexBufferLayout, VertexState, VertexStepMode,
};

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
}

/// The instanced glyph pipeline together with the font system, glyph atlas, and
/// per-frame buffers it draws [`stoatty_term`]'s cell glyphs from.
///
/// Owns the cosmic-text [`FontSystem`]/[`SwashCache`] and the [`GlyphAtlas`]
/// because shaping, rasterization, and packing all happen inside
/// [`Self::prepare`].
pub struct TextPass {
    pipeline: RenderPipeline,
    globals: Buffer,
    globals_bind_group: BindGroup,
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
    swash_cache: SwashCache,
    /// Keyed by the scale's bit pattern, so a fractional text-run scale caches
    /// alongside the integer cell scales.
    shape_cache: HashMap<(char, u32), Option<CacheKey>>,
    baseline: f32,
    metrics: CellMetrics,
}

impl TextPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    ///
    /// Loads the system fonts (cosmic-text [`FontSystem::new`]) plus the bundled
    /// JetBrains Mono default, resolves `font_family` against them to pick the
    /// shaping primary, and creates the glyph atlas, so this is the heavy part of
    /// renderer startup. `format` must
    /// be the non-sRGB surface format the text pass composites into; the shader
    /// does its own sRGB encoding.
    pub(crate) fn new(
        device: &Device,
        format: TextureFormat,
        metrics: CellMetrics,
        font_family: &[String],
    ) -> TextPass {
        let mut font_system = FontSystem::new();
        load_bundled_fonts(&mut font_system);
        let family = resolve_primary_family(&font_system, font_family);
        let baseline = probe_baseline(&mut font_system, metrics, shape_family(&family));
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

        let globals = device.create_buffer(&BufferDescriptor {
            label: Some("text globals"),
            size: size_of::<TextGlobals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("text globals"),
            layout: &globals_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });

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
            swash_cache,
            shape_cache: HashMap::new(),
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
        scroll: Scroll<'_>,
    ) {
        let globals = TextGlobals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        // Underlines are built first, before the glyph path can return early on
        // an all-blank grid: an underlined space has no glyph but still draws.
        self.prepare_underlines(device, queue, grid, scroll.grid);

        self.atlas.begin_frame();
        let grid_pending = self.rasterize_visible(device, queue, grid);
        let overlay_groups = self.rasterize_overlays(device, queue, grid);

        let region = grid.scroll_region();
        let (region_pending, plain_pending): (Vec<PendingGlyph>, Vec<PendingGlyph>) = grid_pending
            .into_iter()
            .partition(|glyph| region.is_some_and(|region| region.contains(glyph.row, glyph.col)));

        let mut plain_instances = self.build_text_instances(device, queue, plain_pending);
        let mut region_instances = self.build_text_instances(device, queue, region_pending);

        let grid_scroll_px = scroll.grid * self.metrics.height;
        for instance in &mut plain_instances {
            instance.pos[1] += grid_scroll_px;
        }

        let region_scroll_px = scroll.region * self.metrics.height;
        for instance in &mut region_instances {
            instance.pos[1] += region_scroll_px;
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

        self.count = plain_instances.len() as u32;
        self.region_count = region_instances.len() as u32;
        self.overlay_count = overlay_instances.len() as u32;
        self.text_run_count = text_run_instances.len() as u32;
        if plain_instances.is_empty()
            && region_instances.is_empty()
            && overlay_instances.is_empty()
            && text_run_instances.is_empty()
        {
            return;
        }

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

        self.atlas_bind_group = create_atlas_bind_group(
            device,
            &self.atlas_layout,
            &self.sampler,
            self.atlas.mask_view(),
            self.atlas.color_view(),
        );
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
            let Some(info) = self.atlas.get_or_insert(
                device,
                queue,
                &mut self.font_system,
                &mut self.swash_cache,
                glyph.key,
            ) else {
                continue;
            };
            instances.push(TextInstance {
                pos: glyph_origin(
                    glyph.col,
                    glyph.row,
                    info.placement,
                    self.baseline * glyph.scale,
                    self.metrics,
                ),
                dim: [info.size[0] as f32, info.size[1] as f32],
                uv: info.uv,
                fg: rgb_f32(glyph.fg),
                bg: rgb_f32(glyph.bg),
                kind: kind_flag(info.kind),
            });
        }
        instances
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

    /// Build and upload the frame's underline-decoration instances for `grid`,
    /// offset down by `grid_scroll` rows so they scroll with the grid.
    ///
    /// Independent of the glyph path: it runs over every cell (spaces included,
    /// since a blank cell can still be underlined) and reallocates only when the
    /// underlined-cell count outgrows the current capacity.
    fn prepare_underlines(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        grid_scroll: f32,
    ) {
        let mut instances = build_underline_instances(grid, self.metrics);

        let grid_scroll_px = grid_scroll * self.metrics.height;
        for instance in &mut instances {
            instance.cell_pos[1] += grid_scroll_px;
        }

        self.underline_count = instances.len() as u32;
        if instances.is_empty() {
            return;
        }

        if instances.len() > self.underline_capacity {
            self.underline_capacity = instances.len().next_power_of_two();
            self.underline_instances = alloc_instances(
                device,
                "underline instances",
                instance_bytes::<UnderlineInstance>(self.underline_capacity),
            );
        }
        queue.write_buffer(
            &self.underline_instances,
            0,
            bytemuck::cast_slice(&instances),
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
        render_pass.set_bind_group(0, &self.globals_bind_group, &[]);
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
        render_pass.set_bind_group(0, &self.globals_bind_group, &[]);
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
        render_pass.set_bind_group(0, &self.globals_bind_group, &[]);
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
    ) -> Vec<PendingGlyph> {
        let mut pending = Vec::new();

        // Resolve the primary face once, so each cell's coverage check is a
        // charmap lookup rather than a font-database query.
        let primary_name = self.family.clone();
        let primary = shape_family(&primary_name);
        let primary_font = {
            let id = self.font_system.db().query(&Query {
                families: &[primary],
                ..Default::default()
            });
            id.and_then(|id| self.font_system.get_font(id, Weight::NORMAL))
        };
        let covers = |ch: char| {
            primary_font
                .as_ref()
                .is_some_and(|font| font_covers(font, ch))
        };

        for row in 0..grid.rows() {
            let mut col = 0;
            while col < grid.cols() {
                let cell = *grid.get(row, col);
                let Some(scale) = cell_glyph_scale(&cell) else {
                    col += 1;
                    continue;
                };

                // A scaled glyph or a character the primary font lacks (icon,
                // CJK) is shaped on its own through the single-char path, which
                // keeps the symbols-font fallback. Only same-size primary-covered
                // cells run-shape, where ligatures form.
                if scale != 1 || !covers(cell.ch) {
                    if let Some(key) = self.glyph_key(cell.ch, f32::from(scale))
                        && self
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
                            col,
                            key,
                            fg: cell.fg,
                            bg: cell.bg,
                            scale: f32::from(scale),
                        });
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
                        && covers(next.ch);
                    if !groups {
                        break;
                    }
                    run.push((end, next.ch));
                    end += 1;
                }

                let (text, col_of_byte) = run_text_and_columns(&run);
                for (offset, key) in shape_run(&mut self.font_system, &text, self.metrics, primary)
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
                            key,
                            fg: cell.fg,
                            bg: cell.bg,
                            scale: 1.0,
                        });
                    }
                }
                col = end;
            }
        }

        pending
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
                        key,
                        fg: overlay.content_fg,
                        bg: overlay.fill,
                        scale: f32::from(scale),
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

/// A glyph that has been rasterized into the atlas, awaiting its final atlas
/// sub-rect once every glyph this frame is packed.
struct PendingGlyph {
    row: usize,
    col: usize,
    key: CacheKey,
    fg: Rgb,
    bg: Rgb,
    /// Multiple of the cell size this glyph is rasterized and drawn at. Integer
    /// for cell and overlay glyphs; the text-run path uses fractional scales.
    scale: f32,
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
fn glyph_origin(
    col: usize,
    row: usize,
    placement: [i32; 2],
    baseline: f32,
    metrics: CellMetrics,
) -> [f32; 2] {
    let pen_x = col as f32 * metrics.width;
    let baseline_y = row as f32 * metrics.height + baseline;
    [
        pen_x + placement[0] as f32,
        baseline_y - placement[1] as f32,
    ]
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

/// One decoration instance per underlined cell, in row-major order.
fn build_underline_instances(grid: &Grid, metrics: CellMetrics) -> Vec<UnderlineInstance> {
    let mut instances = Vec::new();

    for row in 0..grid.rows() {
        for col in 0..grid.cols() {
            let cell = grid.get(row, col);
            let Some(style) = underline_style_flag(cell.underline) else {
                continue;
            };
            instances.push(UnderlineInstance {
                cell_pos: [col as f32 * metrics.width, row as f32 * metrics.height],
                color: rgb_f32(cell.underline_color),
                style,
            });
        }
    }

    instances
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
        build_underline_instances, cell_glyph_scale, cell_rect_scissor, glyph_family, glyph_origin,
        load_bundled_fonts, overlay_content_cells, resolve_primary_family, run_text_and_columns,
        shape_family, shape_run, text_run_origin, STYLE_DOTTED, SYMBOLS_FAMILY,
    };
    use crate::render::CellMetrics;
    use cosmic_text::{
        fontdb::{Database, Query},
        Family, FontSystem,
    };
    use stoatty_term::grid::{Cell, Grid, Overlay, Rgb, Scale, UnderlineStyle};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
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
        let instances = build_underline_instances(&grid, metrics);

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
}
