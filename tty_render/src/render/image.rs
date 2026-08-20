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
    BlendState, Buffer, BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState,
    ColorWrites, Device, Extent3d, FilterMode, FragmentState, Origin3d, PipelineLayoutDescriptor,
    Queue, RenderPass, RenderPipeline, RenderPipelineDescriptor, SamplerBindingType,
    SamplerDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, TexelCopyBufferLayout,
    TexelCopyTextureInfo, Texture, TextureAspect, TextureDescriptor, TextureDimension,
    TextureFormat, TextureSampleType, TextureUsages, TextureView, TextureViewDescriptor,
    TextureViewDimension, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in quads, allocated up front. Grows by doubling.
const INITIAL_CAPACITY: usize = 64;

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
            ..Default::default()
        });

        ImagePass {
            pipeline,
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
        let texture = device.create_texture(&TextureDescriptor {
            label: Some("image"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::TEXTURE_BINDING | TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            &placed.rgba,
            TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(placed.width * 4),
                rows_per_image: Some(placed.height),
            },
            size,
        );

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
    use super::crop_uv;
    use std::sync::Arc;
    use stoatty_term::grid::{ImageCrop, PlacedImage};

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
}
