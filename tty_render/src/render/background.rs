//! Instanced per-cell background fill.
//!
//! Draws one solid colored quad per grid cell, reading each [`Cell`]'s
//! background from [`stoatty_term`]'s [`Grid`]. The quad corners are generated
//! in the vertex shader from the vertex index, so the only vertex buffer is
//! the per-cell instance stream; a uniform supplies the screen resolution and
//! cell size used to map cells to clip space.
//!
//! [`Cell`]: stoatty_term::grid::Cell

use crate::render::CellMetrics;
use bytemuck::{Pod, Zeroable};
use stoatty_term::grid::{Grid, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, Buffer,
    BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites, Device,
    FragmentState, PipelineLayoutDescriptor, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, ShaderModule, ShaderModuleDescriptor, ShaderSource, ShaderStages,
    TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in cells, allocated up front. Grows by doubling
/// when a grid exceeds it; 2048 covers a default 24x80 grid without reallocating.
const INITIAL_CAPACITY: usize = 2048;

/// Cursor block blend alpha. The cursor's RGB is the theme's cursor color; this
/// translucency is renderer policy so the block tints the cell beneath it.
const CURSOR_ALPHA: f32 = 0.55;

/// Per-cell instance: grid coordinate and normalized background color.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BgInstance {
    cell: [f32; 2],
    color: [f32; 3],
}

/// Uniform shared by the cell and cursor pipelines: the screen resolution and
/// cell size that map cell coordinates to clip space, the cursor's eased
/// position in (fractional) cell coordinates and its color, and the grid's eased
/// vertical scroll offset in pixels.
///
/// `pad` aligns `cursor_color` to a 16-byte offset for the uniform layout.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    cursor_pos: [f32; 2],
    scroll_y: f32,
    pad: f32,
    cursor_color: [f32; 4],
}

/// The cursor block's eased position and color for the frame.
#[derive(Clone, Copy)]
pub struct CursorState {
    /// Fractional cell position, or `None` when the cursor is hidden.
    pub pos: Option<[f32; 2]>,
    /// Block color. The pass applies its own blend alpha.
    pub color: Rgb,
}

/// The instanced background-fill pipeline and its per-frame buffers, plus a
/// single-quad cursor pipeline sharing the same globals uniform.
pub struct BackgroundPass {
    pipeline: RenderPipeline,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    count: u32,
    cursor_pipeline: RenderPipeline,
    cursor_visible: bool,
    metrics: CellMetrics,
}

impl BackgroundPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(
        device: &Device,
        format: TextureFormat,
        metrics: CellMetrics,
    ) -> BackgroundPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("background"),
            source: ShaderSource::Wgsl(include_str!("../shaders/bg.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("background globals"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("background"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("background"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<BgInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![0 => Float32x2, 1 => Float32x3],
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

        let cursor_pipeline = build_cursor_pipeline(device, &shader, &bind_group_layout, format);

        let globals = device.create_buffer(&BufferDescriptor {
            label: Some("background globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("background globals"),
            layout: &bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        BackgroundPass {
            pipeline,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            cursor_pipeline,
            cursor_visible: false,
            metrics,
        }
    }

    /// Replace the cell metrics so the next frame lays out cells at the new size.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    /// Upload the frame's uniform and per-cell instances for `grid`.
    ///
    /// `resolution` is the surface size in physical pixels. `cursor` carries the
    /// cursor block's eased position and color. `grid_scroll` shifts the whole
    /// grid up by that many rows.
    ///
    /// Reallocates the instance buffer only when the grid outgrows the current
    /// capacity.
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        resolution: [f32; 2],
        cursor: CursorState,
        grid_scroll: f32,
    ) {
        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            cursor_pos: cursor.pos.unwrap_or([0.0, 0.0]),
            scroll_y: grid_scroll * self.metrics.height,
            pad: 0.0,
            cursor_color: [
                cursor.color.r as f32 / 255.0,
                cursor.color.g as f32 / 255.0,
                cursor.color.b as f32 / 255.0,
                CURSOR_ALPHA,
            ],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));
        self.cursor_visible = cursor.pos.is_some();

        let instances = build_instances(grid);
        self.count = instances.len() as u32;
        if instances.is_empty() {
            return;
        }

        if instances.len() > self.capacity {
            self.capacity = instances.len().next_power_of_two();
            self.instances = alloc_instances(device, self.capacity);
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
    }

    /// Record the background draw into `render_pass`.
    ///
    /// A no-op until [`Self::prepare`] has run with a non-empty grid.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        render_pass.draw(0..6, 0..self.count);
    }

    /// Record the cursor-block draw into `render_pass`.
    ///
    /// A no-op when the cursor is hidden. Draw it after the glyph pass so the
    /// translucent block tints the cell and its glyph as it slides.
    pub fn draw_cursor(&self, render_pass: &mut RenderPass<'_>) {
        if !self.cursor_visible {
            return;
        }

        render_pass.set_pipeline(&self.cursor_pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.draw(0..6, 0..1);
    }
}

fn alloc_instances(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("background instances"),
        size: (capacity * size_of::<BgInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Build the cursor pipeline sharing `globals_layout` with the cell pass.
///
/// It has no vertex buffer: the single quad reads the cursor position from the
/// globals uniform, and alpha blends so the block tints whatever it covers.
fn build_cursor_pipeline(
    device: &Device,
    shader: &ShaderModule,
    globals_layout: &BindGroupLayout,
    format: TextureFormat,
) -> RenderPipeline {
    let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some("cursor"),
        bind_group_layouts: &[Some(globals_layout)],
        immediate_size: 0,
    });

    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some("cursor"),
        layout: Some(&layout),
        vertex: VertexState {
            module: shader,
            entry_point: Some("vs_cursor"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(FragmentState {
            module: shader,
            entry_point: Some("fs_cursor"),
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

fn build_instances(grid: &Grid) -> Vec<BgInstance> {
    let mut instances = Vec::with_capacity(grid.rows() * grid.cols());

    for row in 0..grid.rows() {
        for col in 0..grid.cols() {
            let bg = grid.get(row, col).bg;
            instances.push(BgInstance {
                cell: [col as f32, row as f32],
                color: [
                    bg.r as f32 / 255.0,
                    bg.g as f32 / 255.0,
                    bg.b as f32 / 255.0,
                ],
            });
        }
    }

    instances
}

#[cfg(test)]
mod tests {
    use super::build_instances;
    use stoatty_term::grid::{Grid, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(include_str!("../shaders/bg.wgsl")).expect("parse bg.wgsl");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate bg.wgsl");
    }

    #[test]
    fn instances_cover_every_cell_with_normalized_bg() {
        let mut grid = Grid::new(2, 2);
        grid.get_mut(0, 0).bg = Rgb::new(255, 0, 0);
        grid.get_mut(1, 1).bg = Rgb::new(0, 0, 255);

        let instances = build_instances(&grid);

        assert_eq!(instances.len(), 4);
        assert_eq!(instances[0].cell, [0.0, 0.0]);
        assert_eq!(instances[0].color, [1.0, 0.0, 0.0]);
        assert_eq!(instances[3].cell, [1.0, 1.0]);
        assert_eq!(instances[3].color, [0.0, 0.0, 1.0]);
    }
}
