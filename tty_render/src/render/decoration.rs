//! Instanced cell-edge border pass.
//!
//! Draws each cell border edge as a renderer primitive: one quad per present
//! edge, the fragment painting a line along that edge in the border color and
//! weight. Borders are decoration over the cell backgrounds, so the pass
//! alpha-blends and runs after the background fill.

use crate::render::{CELL_HEIGHT, CELL_WIDTH};
use bytemuck::{Pod, Zeroable};
use stoatty_term::grid::{Border, BorderStyle, Grid, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingType, BlendState, Buffer, BufferBindingType, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor,
    Queue, RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in edges, allocated up front. Grows by doubling
/// when a frame exceeds it.
const INITIAL_CAPACITY: usize = 256;

/// Edge selector packed into each instance, matching the shader's constants.
const EDGE_TOP: u32 = 0;
const EDGE_RIGHT: u32 = 1;
const EDGE_BOTTOM: u32 = 2;
const EDGE_LEFT: u32 = 3;

/// Border weight packed into each instance, matching the shader's constants.
const STYLE_LIGHT: u32 = 0;
const STYLE_HEAVY: u32 = 1;
const STYLE_DOUBLE: u32 = 2;

/// Per-edge instance: the cell, which edge, its color, and its weight.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BorderInstance {
    cell: [f32; 2],
    color: [f32; 3],
    edge: u32,
    style: u32,
}

/// Uniform shared by every instance: the screen resolution and cell size the
/// vertex shader maps cell coordinates through.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
}

/// The instanced cell-edge border pipeline and its per-frame buffers.
pub struct DecorationPass {
    pipeline: RenderPipeline,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    count: u32,
}

impl DecorationPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub fn new(device: &Device, format: TextureFormat) -> DecorationPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("decoration"),
            source: ShaderSource::Wgsl(include_str!("../shaders/decoration.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("decoration globals"),
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

        let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("decoration"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("decoration"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<BorderInstance>() as u64,
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
                module: &shader,
                entry_point: Some("fs_main"),
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
        });

        let globals = device.create_buffer(&BufferDescriptor {
            label: Some("decoration globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("decoration globals"),
            layout: &bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        DecorationPass {
            pipeline,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
        }
    }

    /// Upload the frame's uniform and one instance per bordered cell edge.
    ///
    /// `resolution` is the surface size in physical pixels. Reallocates the
    /// instance buffer only when the edge count outgrows the current capacity.
    pub fn prepare(&mut self, device: &Device, queue: &Queue, grid: &Grid, resolution: [f32; 2]) {
        let globals = Globals {
            resolution,
            cell_size: [CELL_WIDTH, CELL_HEIGHT],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let instances = build_border_instances(grid);
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

    /// Record the border draw into `render_pass`.
    ///
    /// A no-op when no cell carries a border. Run after the background fill so
    /// the borders sit over the cell backgrounds.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        render_pass.draw(0..6, 0..self.count);
    }
}

fn alloc_instances(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("decoration instances"),
        size: (capacity * size_of::<BorderInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// One instance per present border edge across the grid, in row-major order.
fn build_border_instances(grid: &Grid) -> Vec<BorderInstance> {
    let mut instances = Vec::new();

    for row in 0..grid.rows() {
        for col in 0..grid.cols() {
            let borders = grid.get(row, col).borders;
            let cell = [col as f32, row as f32];

            push_edge(&mut instances, cell, EDGE_TOP, borders.top);
            push_edge(&mut instances, cell, EDGE_RIGHT, borders.right);
            push_edge(&mut instances, cell, EDGE_BOTTOM, borders.bottom);
            push_edge(&mut instances, cell, EDGE_LEFT, borders.left);
        }
    }

    instances
}

fn push_edge(
    instances: &mut Vec<BorderInstance>,
    cell: [f32; 2],
    edge: u32,
    border: Option<Border>,
) {
    if let Some(border) = border {
        instances.push(BorderInstance {
            cell,
            color: rgb_f32(border.color),
            edge,
            style: style_flag(border.style),
        });
    }
}

fn style_flag(style: BorderStyle) -> u32 {
    match style {
        BorderStyle::Light => STYLE_LIGHT,
        BorderStyle::Heavy => STYLE_HEAVY,
        BorderStyle::Double => STYLE_DOUBLE,
    }
}

fn rgb_f32(color: Rgb) -> [f32; 3] {
    [
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::{build_border_instances, EDGE_BOTTOM, EDGE_TOP, STYLE_HEAVY};
    use stoatty_term::grid::{Border, BorderStyle, Grid, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn shader_is_valid_wgsl() {
        let module =
            wgsl::parse_str(include_str!("../shaders/decoration.wgsl")).expect("parse decoration");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate decoration");
    }

    #[test]
    fn border_instances_cover_present_edges_only() {
        let mut grid = Grid::new(1, 2);
        grid.get_mut(0, 1).borders.top = Some(Border {
            style: BorderStyle::Heavy,
            color: Rgb::new(255, 0, 0),
        });
        grid.get_mut(0, 1).borders.bottom = Some(Border {
            style: BorderStyle::Light,
            color: Rgb::new(0, 255, 0),
        });

        let instances = build_border_instances(&grid);

        assert_eq!(instances.len(), 2);
        assert_eq!(instances[0].cell, [1.0, 0.0]);
        assert_eq!(instances[0].edge, EDGE_TOP);
        assert_eq!(instances[0].style, STYLE_HEAVY);
        assert_eq!(instances[0].color, [1.0, 0.0, 0.0]);
        assert_eq!(instances[1].edge, EDGE_BOTTOM);
        assert_eq!(instances[1].color, [0.0, 1.0, 0.0]);
    }
}
