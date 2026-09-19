//! Instanced image pass, drawing the placements a client put on the grid.
//!
//! One instance per placement, carrying a pixel rectangle and the source
//! rectangle to sample. Quads ride in absolute pixels rather than cell-fraction
//! units, as the minimap's do: a placement's box is measured out in cells, but
//! scaling and the intra-cell offset put its edges wherever they land.
//!
//! Each draw binds one texture, so instances group by image. Textures are keyed
//! by image and generation together, since a client that re-transmits an id has
//! new pixels under the old name and a cache keyed on the id alone would keep
//! drawing what it replaced.
//!
//! The z-index sorts into two buckets rather than against the text, because
//! order across passes is fixed by the record chain. Negative z records under
//! the glyphs and the rest over them, which is the whole of what a z-index means
//! to a terminal that draws all its text in one pass.
//!
//! Pools and aux windows draw no images. A client places against the live grid,
//! and an aux window owns a separate device whose textures are not these.

use crate::render::CellMetrics;
use bytemuck::{Pod, Zeroable};
use std::collections::HashMap;
use stoatty_term::grid::{Grid, PlacedImage};
use wgpu::{
    vertex_attr_array, AddressMode, BindGroup, BindGroupDescriptor, BindGroupEntry,
    BindGroupLayout, BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType,
    BlendState, Buffer, BufferBindingType, BufferDescriptor, BufferUsages, Color, ColorTargetState,
    ColorWrites, CommandEncoder, CommandEncoderDescriptor, Device, Extent3d, FilterMode,
    FragmentState, LoadOp, MipmapFilterMode, Operations, PipelineLayoutDescriptor, Queue,
    RenderPass, RenderPassColorAttachment, RenderPassDescriptor, RenderPipeline,
    RenderPipelineDescriptor, SamplerBindingType, SamplerDescriptor, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, StoreOp, TexelCopyBufferLayout, Texture, TextureDescriptor,
    TextureDimension, TextureFormat, TextureSampleType, TextureUsages, TextureView,
    TextureViewDescriptor, TextureViewDimension, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in quads, allocated up front. Grows by doubling.
const INITIAL_CAPACITY: usize = 64;

/// The format of every image texture and of the mip blits that fill it.
///
/// The two have to match, because a blit pipeline fails validation against a
/// level of any other format.
const IMAGE_FORMAT: TextureFormat = TextureFormat::Rgba8Unorm;

/// Which image a cached texture holds, and which transmission of it.
///
/// The generation is half the key because a re-transmission replaces an id's
/// pixels, and a cache that ignored it would draw the image the client meant to
/// replace.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct TextureKey {
    image: u32,
    generation: u64,
}

/// The per-quad instance data: where to draw, and what part of the image to
/// sample there.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct ImageInstance {
    origin: [f32; 2],
    size: [f32; 2],
    uv_min: [f32; 2],
    uv_max: [f32; 2],
}

/// The uniform shared by every instance.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    _pad: [f32; 2],
}

/// One image's draw: the texture to bind and the instances that sample it.
struct ImageDraw {
    key: TextureKey,
    start: u32,
    count: u32,
}

/// An uploaded image, held until a frame stops placing it.
struct CachedTexture {
    bind_group: BindGroup,
    /// Whether the frame being prepared placed this image. An entry no frame
    /// placed is dropped at the end of the prepare rather than aged out: the
    /// terminal still holds the pixels, so showing it again costs an upload
    /// rather than a decode.
    live: bool,
    _texture: Texture,
    _view: TextureView,
}

/// The instanced image pipeline, its per-frame buffers, and its texture cache.
pub struct ImagePass {
    pipeline: RenderPipeline,
    /// Fills an image's base level from its raw texels, premultiplied.
    premultiply_blit: RenderPipeline,
    /// Fills each later mip level from the level above it.
    copy_blit: RenderPipeline,
    globals: Buffer,
    globals_bind_group: BindGroup,
    texture_layout: BindGroupLayout,
    sampler: wgpu::Sampler,
    instances: Buffer,
    capacity: usize,
    textures: HashMap<TextureKey, CachedTexture>,
    /// Draws for the placements sitting behind the grid text, and those in front
    /// of it. Split because the record chain, not a sort, decides which side of
    /// the text a pass draws on.
    under: Vec<ImageDraw>,
    over: Vec<ImageDraw>,
    scratch: Vec<ImageInstance>,
    metrics: CellMetrics,
}

impl ImagePass {
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> ImagePass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("image"),
            source: ShaderSource::Wgsl(include_str!("../shaders/image.wgsl").into()),
        });

        let globals_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("image globals"),
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

        let texture_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("image texture"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("image"),
            bind_group_layouts: &[Some(&globals_layout), Some(&texture_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("image"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<ImageInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x2,
                        3 => Float32x2,
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

        let blit_shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("image blit"),
            source: ShaderSource::Wgsl(include_str!("../shaders/image_blit.wgsl").into()),
        });
        let blit_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("image blit"),
            bind_group_layouts: &[Some(&texture_layout)],
            immediate_size: 0,
        });
        let blit_pipeline = |fragment_entry: &str| {
            device.create_render_pipeline(&RenderPipelineDescriptor {
                label: Some("image blit"),
                layout: Some(&blit_layout),
                vertex: VertexState {
                    module: &blit_shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[],
                },
                fragment: Some(FragmentState {
                    module: &blit_shader,
                    entry_point: Some(fragment_entry),
                    compilation_options: Default::default(),
                    targets: &[Some(ColorTargetState {
                        format: IMAGE_FORMAT,
                        blend: None,
                        write_mask: ColorWrites::ALL,
                    })],
                }),
                primitive: Default::default(),
                depth_stencil: None,
                multisample: Default::default(),
                multiview_mask: None,
                cache: None,
            })
        };
        let premultiply_blit = blit_pipeline("fs_premultiply");
        let copy_blit = blit_pipeline("fs_copy");

        let globals = device.create_buffer(&BufferDescriptor {
            label: Some("image globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let globals_bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("image globals"),
            layout: &globals_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });

        // Linear, unlike the glyph atlas's nearest. An image is scaled into a
        // cell box the client chose, so resampling is the point rather than an
        // artifact to keep out.
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("image"),
            address_mode_u: AddressMode::ClampToEdge,
            address_mode_v: AddressMode::ClampToEdge,
            address_mode_w: AddressMode::ClampToEdge,
            mag_filter: FilterMode::Linear,
            min_filter: FilterMode::Linear,
            // A placement often draws an image at a fraction of its size, so
            // the sampler reads a level near the size it draws and blends the
            // two it falls between.
            mipmap_filter: MipmapFilterMode::Linear,
            ..Default::default()
        });

        ImagePass {
            pipeline,
            premultiply_blit,
            copy_blit,
            globals,
            globals_bind_group,
            texture_layout,
            sampler,
            instances: alloc_instances(device, INITIAL_CAPACITY),
            capacity: INITIAL_CAPACITY,
            textures: HashMap::new(),
            under: Vec::new(),
            over: Vec::new(),
            scratch: Vec::new(),
            metrics,
        }
    }

    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    /// Build this frame's instances from the grid's placements, uploading any
    /// image the cache does not already hold.
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        resolution: [f32; 2],
    ) {
        queue.write_buffer(
            &self.globals,
            0,
            bytemuck::bytes_of(&Globals {
                resolution,
                _pad: [0.0; 2],
            }),
        );

        self.under.clear();
        self.over.clear();
        self.scratch.clear();
        for cached in self.textures.values_mut() {
            cached.live = false;
        }

        // Sorted so the buckets are contiguous and, within one, a higher z and
        // then a later placement id draws last. Grouping by image after that
        // keeps each draw to one texture bind.
        let mut placements: Vec<&PlacedImage> = grid.images().iter().collect();
        placements.sort_by_key(|placed| (placed.z, placed.placement, placed.image));

        for placed in placements {
            let key = TextureKey {
                image: placed.image,
                generation: placed.generation,
            };
            if placed.width == 0 || placed.height == 0 {
                continue;
            }
            self.ensure_texture(device, queue, key, placed);

            let start = self.scratch.len() as u32;
            self.scratch.push(self.instance_for(placed));

            let bucket = match placed.z < 0 {
                true => &mut self.under,
                false => &mut self.over,
            };
            // One draw per contiguous run of the same image, so a client placing
            // one image many times binds its texture once.
            match bucket.last_mut() {
                Some(last) if last.key == key && last.start + last.count == start => {
                    last.count += 1
                },
                _ => bucket.push(ImageDraw {
                    key,
                    start,
                    count: 1,
                }),
            }
        }

        self.textures.retain(|_, cached| cached.live);

        if self.scratch.is_empty() {
            return;
        }
        if self.scratch.len() > self.capacity {
            self.capacity = self.scratch.len().next_power_of_two();
            self.instances = alloc_instances(device, self.capacity);
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&self.scratch));
    }

    /// The quad for one placement, in pixels, with the source rectangle it
    /// samples.
    fn instance_for(&self, placed: &PlacedImage) -> ImageInstance {
        let (cell_w, cell_h) = (self.metrics.width, self.metrics.height);
        let origin = [
            placed.col as f32 * cell_w + placed.offset_x as f32,
            placed.row as f32 * cell_h + placed.offset_y as f32,
        ];
        let size = [placed.cols as f32 * cell_w, placed.rows as f32 * cell_h];
        let (uv_min, uv_max) = crop_uv(placed);

        ImageInstance {
            origin,
            size,
            uv_min,
            uv_max,
        }
    }

    /// Upload `placed`'s pixels unless the cache already holds that generation.
    ///
    /// The raw texels go up in one copy. The GPU then premultiplies them into
    /// the base level and averages each mip level from the one above it. Work
    /// per texel on this thread blocks input and frames while it runs, which
    /// reaches tens of milliseconds at the largest image the terminal accepts.
    ///
    /// The upload fills every mip level along with the base one, so a
    /// placement that scales the image down reads a level near the size it
    /// draws rather than every eighth texel of the full-size one.
    fn ensure_texture(
        &mut self,
        device: &Device,
        queue: &Queue,
        key: TextureKey,
        placed: &PlacedImage,
    ) {
        if let Some(cached) = self.textures.get_mut(&key) {
            cached.live = true;
            return;
        }

        let size = Extent3d {
            width: placed.width,
            height: placed.height,
            depth_or_array_layers: 1,
        };
        let source = device.create_texture(&TextureDescriptor {
            label: Some("image source"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: IMAGE_FORMAT,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            source.as_image_copy(),
            &placed.rgba,
            TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(placed.width * 4),
                rows_per_image: Some(placed.height),
            },
            size,
        );

        let levels = u32::BITS - placed.width.max(placed.height).max(1).leading_zeros();
        let usage = TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT;
        // A test reads mip levels back, which only a copy source allows.
        #[cfg(test)]
        let usage = usage | TextureUsages::COPY_SRC;
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("image"),
            size,
            mip_level_count: levels,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: IMAGE_FORMAT,
            usage,
            view_formats: &[],
        });

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
            label: Some("image mips"),
        });
        let source_view = source.create_view(&TextureViewDescriptor::default());
        self.blit(
            device,
            &mut encoder,
            &self.premultiply_blit,
            &source_view,
            &level_view(&texture, 0),
        );
        for level in 1..levels {
            self.blit(
                device,
                &mut encoder,
                &self.copy_blit,
                &level_view(&texture, level - 1),
                &level_view(&texture, level),
            );
        }
        queue.submit([encoder.finish()]);

        let view = texture.create_view(&TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("image texture"),
            layout: &self.texture_layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(&view),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        self.textures.insert(
            key,
            CachedTexture {
                bind_group,
                live: true,
                _texture: texture,
                _view: view,
            },
        );
    }

    /// Record a pass that draws `source` over all of `target` through
    /// `pipeline`.
    fn blit(
        &self,
        device: &Device,
        encoder: &mut CommandEncoder,
        pipeline: &RenderPipeline,
        source: &TextureView,
        target: &TextureView,
    ) {
        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("image blit"),
            layout: &self.texture_layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: BindingResource::TextureView(source),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::Sampler(&self.sampler),
                },
            ],
        });

        let mut pass = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("image blit"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(Color::TRANSPARENT),
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(pipeline);
        pass.set_bind_group(0, &bind_group, &[]);
        pass.draw(0..3, 0..1);
    }

    /// Record the placements that sit behind the grid text.
    pub fn draw_under(&self, render_pass: &mut RenderPass<'_>) {
        self.draw_bucket(render_pass, &self.under);
    }

    /// Record the placements that sit in front of the grid text.
    pub fn draw_over(&self, render_pass: &mut RenderPass<'_>) {
        self.draw_bucket(render_pass, &self.over);
    }

    fn draw_bucket(&self, render_pass: &mut RenderPass<'_>, bucket: &[ImageDraw]) {
        if bucket.is_empty() {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.globals_bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        for draw in bucket {
            let Some(cached) = self.textures.get(&draw.key) else {
                continue;
            };
            render_pass.set_bind_group(1, &cached.bind_group, &[]);
            render_pass.draw(0..6, draw.start..draw.start + draw.count);
        }
    }
}

/// The source rectangle a placement samples, as texture coordinates.
///
/// A zero crop width or height means the rest of the image from that edge,
/// which is what a client that wants the whole image sends. The result is
/// clamped into the image, since a crop reaching past it would sample the edge
/// texel repeatedly and stretch it across the difference.
fn crop_uv(placed: &PlacedImage) -> ([f32; 2], [f32; 2]) {
    let (width, height) = (placed.width as f32, placed.height as f32);
    let x0 = (placed.crop.x as f32).min(width);
    let y0 = (placed.crop.y as f32).min(height);
    let x1 = match placed.crop.width {
        0 => width,
        w => (x0 + w as f32).min(width),
    };
    let y1 = match placed.crop.height {
        0 => height,
        h => (y0 + h as f32).min(height),
    };

    ([x0 / width, y0 / height], [x1 / width, y1 / height])
}

/// A view of mip `level` of `texture` alone.
///
/// A blit samples one level while it draws into the next, and each view covers
/// a single level for two reasons. A pass rejects a texture level bound as both
/// its source and its target. A source view over the whole chain also lets the
/// sampler choose a level from the draw's scale, and a draw half the size of
/// its source picks the level of its own size, which is the target.
fn level_view(texture: &Texture, level: u32) -> TextureView {
    texture.create_view(&TextureViewDescriptor {
        label: Some("image level"),
        base_mip_level: level,
        mip_level_count: Some(1),
        ..Default::default()
    })
}

fn alloc_instances(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("image instances"),
        size: (capacity * size_of::<ImageInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests {
    use super::{crop_uv, ImagePass, TextureKey};
    use crate::{gpu::headless_device, render::CellMetrics};
    use std::sync::Arc;
    use stoatty_term::grid::{ImageCrop, PlacedImage};
    use wgpu::{
        naga::{
            front::wgsl,
            valid::{Capabilities, ValidationFlags, Validator},
        },
        BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode,
        Origin3d, PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout,
        TexelCopyTextureInfo, Texture, TextureAspect, TextureFormat, COPY_BYTES_PER_ROW_ALIGNMENT,
    };

    fn placed(width: u32, height: u32, crop: ImageCrop) -> PlacedImage {
        PlacedImage {
            image: 1,
            placement: 0,
            generation: 1,
            rgba: Arc::from(vec![0u8; (width * height * 4) as usize]),
            width,
            height,
            row: 0,
            col: 0,
            cols: 1,
            rows: 1,
            crop,
            offset_x: 0,
            offset_y: 0,
            z: 0,
        }
    }

    /// A zero crop dimension is how a client says "the rest of the image", which
    /// is what every client sending a whole image sends.
    #[test]
    fn an_unset_crop_samples_the_whole_image() {
        assert_eq!(
            crop_uv(&placed(20, 10, ImageCrop::default())),
            ([0.0, 0.0], [1.0, 1.0]),
        );
    }

    #[test]
    fn a_crop_maps_to_the_fraction_of_the_image_it_names() {
        let crop = ImageCrop {
            x: 5,
            y: 2,
            width: 10,
            height: 4,
        };

        assert_eq!(crop_uv(&placed(20, 8, crop)), ([0.25, 0.25], [0.75, 0.75]),);
    }

    /// A crop reaching past the image would sample its edge texel over and over,
    /// stretching one row of pixels across the difference.
    #[test]
    fn a_crop_past_the_edge_stops_at_it() {
        let crop = ImageCrop {
            x: 8,
            y: 0,
            width: 100,
            height: 100,
        };

        assert_eq!(crop_uv(&placed(10, 10, crop)), ([0.8, 0.0], [1.0, 1.0]));
    }

    /// An origin past the image leaves nothing to sample, and the range has to
    /// stay ordered rather than inverting.
    #[test]
    fn a_crop_starting_past_the_image_is_empty_rather_than_inverted() {
        let crop = ImageCrop {
            x: 40,
            y: 40,
            width: 0,
            height: 0,
        };

        let (min, max) = crop_uv(&placed(10, 10, crop));
        assert_eq!((min, max), ([1.0, 1.0], [1.0, 1.0]));
    }

    #[test]
    fn blit_shader_is_valid_wgsl() {
        let module =
            wgsl::parse_str(include_str!("../shaders/image_blit.wgsl")).expect("parse image blit");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate image blit");
    }

    /// A 4 by 4 image is opaque red, except for its fourth column and its
    /// fourth row, which are transparent white.
    ///
    /// The base level holds the image premultiplied, so its transparent texels
    /// read as zero. Level 1 averages each 2 by 2 block of it. A block with
    /// half its texels transparent reads half-covered red, and the corner block
    /// reads quarter-covered red. An average taken ahead of the premultiply
    /// reads about `[255, 128, 128, 128]` for a half-covered block instead,
    /// pulled toward the white that the transparent texels carry.
    ///
    /// The transparent texels sit on one side of each axis, so a flip on either
    /// axis moves them. The base level is part of the check, because a flip in
    /// the blit flips level 1 back.
    #[test]
    fn mip_levels_average_the_premultiplied_image() {
        const RED: [u8; 4] = [255, 0, 0, 255];
        const HALF_RED: [u8; 4] = [128, 0, 0, 128];
        const QUARTER_RED: [u8; 4] = [64, 0, 0, 64];

        let Some((device, queue)) = headless_device() else {
            return;
        };
        let metrics = CellMetrics {
            font_size: 10.0,
            width: 6.0,
            height: 12.0,
            scale_factor: 1.0,
        };
        let mut pass = ImagePass::new(&device, TextureFormat::Rgba8Unorm, metrics);

        let covered = |texel: usize| texel % 4 != 3 && texel / 4 != 3;
        let rgba: Vec<u8> = (0..16)
            .flat_map(|texel| {
                if covered(texel) {
                    RED
                } else {
                    [255, 255, 255, 0]
                }
            })
            .collect();
        let image = PlacedImage {
            rgba: Arc::from(rgba),
            ..placed(4, 4, ImageCrop::default())
        };
        let key = TextureKey {
            image: image.image,
            generation: image.generation,
        };
        pass.ensure_texture(&device, &queue, key, &image);

        let texture = &pass.textures[&key]._texture;
        let levels = [(0, 4), (1, 2), (2, 1)]
            .map(|(level, side)| read_level(&device, &queue, texture, level, side));
        let base: Vec<[u8; 4]> = (0..16)
            .map(|texel| if covered(texel) { RED } else { [0; 4] })
            .collect();
        let expected = [
            base,
            vec![RED, HALF_RED, HALF_RED, QUARTER_RED],
            vec![[144, 0, 0, 144]],
        ];
        assert!(
            levels
                .iter()
                .zip(&expected)
                .all(|(got, want)| within_one(got, want)),
            "the levels read {levels:?}"
        );
    }

    /// The texels of mip `level` of `texture`, row by row, for a level `side`
    /// texels square.
    fn read_level(
        device: &Device,
        queue: &Queue,
        texture: &Texture,
        level: u32,
        side: u32,
    ) -> Vec<[u8; 4]> {
        let readback = device.create_buffer(&BufferDescriptor {
            label: Some("image level readback"),
            size: u64::from(COPY_BYTES_PER_ROW_ALIGNMENT * side),
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            TexelCopyTextureInfo {
                texture,
                mip_level: level,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            TexelCopyBufferInfo {
                buffer: &readback,
                layout: TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(COPY_BYTES_PER_ROW_ALIGNMENT),
                    rows_per_image: None,
                },
            },
            Extent3d {
                width: side,
                height: side,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));

        readback.slice(..).map_async(MapMode::Read, |_| {});
        device
            .poll(PollType::wait_indefinitely())
            .expect("poll readback");
        let bytes = readback.slice(..).get_mapped_range().to_vec();
        bytes
            .chunks(COPY_BYTES_PER_ROW_ALIGNMENT as usize)
            .flat_map(|row| row.as_chunks::<4>().0[..side as usize].iter().copied())
            .collect()
    }

    /// Whether `got` holds as many texels as `want`, each channel within one of
    /// its counterpart, the rounding a GPU filter takes either way.
    fn within_one(got: &[[u8; 4]], want: &[[u8; 4]]) -> bool {
        got.len() == want.len()
            && got
                .iter()
                .zip(want)
                .all(|(got, want)| got.iter().zip(want).all(|(g, w)| g.abs_diff(*w) <= 1))
    }
}
