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
    render::{CELL_HEIGHT, CELL_WIDTH},
};
use bytemuck::{Pod, Zeroable};
use cosmic_text::{
    Attrs, Buffer as CosmicBuffer, CacheKey, Family, FontSystem, Metrics, Shaping, SwashCache,
};
use std::collections::HashMap;
use stoatty_term::grid::{Grid, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites,
    Device, FragmentState, PipelineLayoutDescriptor, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, Sampler, SamplerBindingType, SamplerDescriptor,
    ShaderModuleDescriptor, ShaderSource, ShaderStages, TextureFormat, TextureSampleType,
    TextureView, TextureViewDimension, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Font size in physical pixels glyphs are rasterized at.
///
/// A fixed placeholder paired with the cell metrics until both are derived from
/// the font.
const FONT_SIZE: f32 = 15.0;

/// Instance buffer capacity, in glyphs, allocated up front. Grows by doubling
/// when a frame exceeds it.
const INITIAL_CAPACITY: usize = 2048;

/// Atlas selector packed into each instance, matching the shader's constants.
const KIND_MASK: u32 = 0;
const KIND_COLOR: u32 = 1;

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

/// Uniform shared by every instance: the surface resolution the vertex shader
/// maps pixel coordinates through.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct TextGlobals {
    resolution: [f32; 2],
    pad: [f32; 2],
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
    atlas: GlyphAtlas,
    font_system: FontSystem,
    swash_cache: SwashCache,
    shape_cache: HashMap<char, Option<CacheKey>>,
    baseline: f32,
}

impl TextPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    ///
    /// Loads the system fonts (cosmic-text [`FontSystem::new`]) and creates the
    /// glyph atlas, so this is the heavy part of renderer startup. `format` must
    /// be the non-sRGB surface format the text pass composites into; the shader
    /// does its own sRGB encoding.
    pub fn new(device: &Device, format: TextureFormat) -> TextPass {
        let mut font_system = FontSystem::new();
        let baseline = probe_baseline(&mut font_system);
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
                visibility: ShaderStages::VERTEX,
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

        let instances = alloc_instances(device, INITIAL_CAPACITY);

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
            atlas,
            font_system,
            swash_cache,
            shape_cache: HashMap::new(),
            baseline,
        }
    }

    /// Shape, rasterize, and upload the frame's glyph instances for `grid`.
    ///
    /// `resolution` is the surface size in physical pixels. Runs in two phases:
    /// every visible glyph is rasterized first (which may grow the atlas), then
    /// each glyph's atlas sub-rect is read once the atlas has reached its final
    /// size, so normalized coordinates stay valid. Reallocates the instance
    /// buffer only when the glyph count outgrows the current capacity.
    pub fn prepare(&mut self, device: &Device, queue: &Queue, grid: &Grid, resolution: [f32; 2]) {
        let globals = TextGlobals {
            resolution,
            pad: [0.0, 0.0],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        self.atlas.begin_frame();
        let pending = self.rasterize_visible(device, queue, grid);

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
                pos: glyph_origin(glyph.col, glyph.row, info.placement, self.baseline),
                dim: [info.size[0] as f32, info.size[1] as f32],
                uv: info.uv,
                fg: rgb_f32(glyph.fg),
                bg: rgb_f32(glyph.bg),
                kind: kind_flag(info.kind),
            });
        }

        self.count = instances.len() as u32;
        if instances.is_empty() {
            return;
        }

        if instances.len() > self.capacity {
            self.capacity = instances.len().next_power_of_two();
            self.instances = alloc_instances(device, self.capacity);
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));

        self.atlas_bind_group = create_atlas_bind_group(
            device,
            &self.atlas_layout,
            &self.sampler,
            self.atlas.mask_view(),
            self.atlas.color_view(),
        );
    }

    /// Record the glyph draw into `render_pass`.
    ///
    /// A no-op until [`Self::prepare`] has run with a grid that has visible
    /// glyphs. Must run after the background pass in the same render pass: each
    /// quad composites over the cell background already painted underneath.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.globals_bind_group, &[]);
        render_pass.set_bind_group(1, &self.atlas_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        render_pass.draw(0..6, 0..self.count);
    }

    /// Phase one: shape and rasterize every visible cell glyph, returning their
    /// placements.
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

        for row in 0..grid.rows() {
            for col in 0..grid.cols() {
                let cell = grid.get(row, col);
                if cell.ch == ' ' {
                    continue;
                }

                let Some(key) = self.glyph_key(cell.ch) else {
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
                        col,
                        key,
                        fg: cell.fg,
                        bg: cell.bg,
                    });
                }
            }
        }

        pending
    }

    /// The cached glyph cache key for `ch`, shaping it on first use. `None` for
    /// a character that produces no glyph.
    fn glyph_key(&mut self, ch: char) -> Option<CacheKey> {
        if let Some(key) = self.shape_cache.get(&ch) {
            return *key;
        }

        let key = shape_char(&mut self.font_system, ch);
        self.shape_cache.insert(ch, key);
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

fn alloc_instances(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("text instances"),
        size: (capacity * size_of::<TextInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
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

/// Shape `ch` on its own and return its glyph cache key, or `None` if it
/// produces no glyph. One character maps to one cell, so each is shaped
/// independently rather than through proportional line layout.
fn shape_char(font_system: &mut FontSystem, ch: char) -> Option<CacheKey> {
    let mut buffer = CosmicBuffer::new(font_system, Metrics::new(FONT_SIZE, CELL_HEIGHT));
    let mut encoded = [0u8; 4];
    let text = ch.encode_utf8(&mut encoded);
    buffer.set_text(
        font_system,
        text,
        &Attrs::new().family(Family::Monospace),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);

    let run = buffer.layout_runs().next()?;
    let glyph = run.glyphs.first()?;
    Some(glyph.physical((0.0, 0.0), 1.0).cache_key)
}

/// Baseline offset from a cell's top, in physical pixels, measured once from the
/// font so glyphs sit on a consistent baseline within their cell.
fn probe_baseline(font_system: &mut FontSystem) -> f32 {
    let mut buffer = CosmicBuffer::new(font_system, Metrics::new(FONT_SIZE, CELL_HEIGHT));
    buffer.set_text(
        font_system,
        "M",
        &Attrs::new().family(Family::Monospace),
        Shaping::Advanced,
        None,
    );
    buffer.shape_until_scroll(font_system, false);
    buffer
        .layout_runs()
        .next()
        .map(|run| run.line_y)
        .unwrap_or(CELL_HEIGHT * 0.8)
}

/// Screen position of a glyph bitmap's top-left in physical pixels.
///
/// The pen sits at the cell's left edge on the row baseline; `placement` is the
/// swash bitmap offset from that pen (`left` rightward, `top` upward from the
/// baseline).
fn glyph_origin(col: usize, row: usize, placement: [i32; 2], baseline: f32) -> [f32; 2] {
    let pen_x = col as f32 * CELL_WIDTH;
    let baseline_y = row as f32 * CELL_HEIGHT + baseline;
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

#[cfg(test)]
mod tests {
    use super::glyph_origin;
    use crate::render::{CELL_HEIGHT, CELL_WIDTH};

    #[test]
    fn glyph_origin_offsets_from_cell_pen_and_baseline() {
        let baseline = 14.0;

        let origin = glyph_origin(3, 2, [1, 10], baseline);
        assert_eq!(
            origin,
            [3.0 * CELL_WIDTH + 1.0, 2.0 * CELL_HEIGHT + baseline - 10.0]
        );

        let origin = glyph_origin(0, 0, [-2, -3], baseline);
        assert_eq!(origin, [-2.0, baseline + 3.0]);
    }
}
