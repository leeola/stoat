//! Instanced cell border pass.
//!
//! Draws each cell's borders as a renderer primitive: one quad per bordered
//! cell, the fragment painting a line along every present edge in that edge's
//! color and weight. Holding a cell's four edges in one instance lets a
//! [`BorderStyle::Rounded`] corner arc the join where two adjacent edges meet,
//! which a per-edge instance could not coordinate. Borders are decoration over
//! the cell backgrounds, so the pass alpha-blends and runs after the background
//! fill.

use crate::render::{CELL_HEIGHT, CELL_WIDTH};
use bytemuck::{Pod, Zeroable};
use stoatty_term::grid::{Border, BorderStyle, Borders, Grid, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingType, BlendState, Buffer, BufferBindingType, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor,
    Queue, RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in cells, allocated up front. Grows by doubling
/// when a frame exceeds it.
const INITIAL_CAPACITY: usize = 256;

/// Edge-presence bits packed into [`BorderInstance::edges`], matching the
/// shader's constants.
const EDGE_TOP_BIT: u32 = 1;
const EDGE_RIGHT_BIT: u32 = 2;
const EDGE_BOTTOM_BIT: u32 = 4;
const EDGE_LEFT_BIT: u32 = 8;

/// Per-edge style codes packed into [`BorderInstance::styles`], matching the
/// shader's constants.
const STYLE_LIGHT: u32 = 0;
const STYLE_HEAVY: u32 = 1;
const STYLE_DOUBLE: u32 = 2;
const STYLE_ROUNDED: u32 = 3;

/// Per-cell instance: the cell, each edge's color, a presence bitmask, and the
/// four per-edge style codes.
///
/// `edges` is an OR of the `EDGE_*_BIT` flags. `styles` packs one 8-bit style
/// code per edge: top in the low byte, then right, bottom, and left. Absent
/// edges leave their color and style byte zeroed; the bitmask is the source of
/// truth for which edges to draw.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BorderInstance {
    cell: [f32; 2],
    top_color: [f32; 3],
    right_color: [f32; 3],
    bottom_color: [f32; 3],
    left_color: [f32; 3],
    edges: u32,
    styles: u32,
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
                // The fragment reads globals.cell_size for the rounded-corner
                // radius and edge distance, so it must be visible there too.
                visibility: ShaderStages::VERTEX_FRAGMENT,
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
                        2 => Float32x3,
                        3 => Float32x3,
                        4 => Float32x3,
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

/// One instance per bordered cell across the grid, in row-major order.
fn build_border_instances(grid: &Grid) -> Vec<BorderInstance> {
    let mut instances = Vec::new();

    for row in 0..grid.rows() {
        for col in 0..grid.cols() {
            let borders = grid.get(row, col).borders;
            if let Some(instance) = cell_instance([col as f32, row as f32], borders) {
                instances.push(instance);
            }
        }
    }

    instances
}

/// Pack a cell's borders into one instance, or `None` when no edge is set.
fn cell_instance(cell: [f32; 2], borders: Borders) -> Option<BorderInstance> {
    if borders == Borders::default() {
        return None;
    }

    let mut edges = 0;
    let mut styles = 0;
    if let Some(border) = borders.top {
        edges |= EDGE_TOP_BIT;
        styles |= style_flag(border.style);
    }
    if let Some(border) = borders.right {
        edges |= EDGE_RIGHT_BIT;
        styles |= style_flag(border.style) << 8;
    }
    if let Some(border) = borders.bottom {
        edges |= EDGE_BOTTOM_BIT;
        styles |= style_flag(border.style) << 16;
    }
    if let Some(border) = borders.left {
        edges |= EDGE_LEFT_BIT;
        styles |= style_flag(border.style) << 24;
    }

    Some(BorderInstance {
        cell,
        top_color: edge_color(borders.top),
        right_color: edge_color(borders.right),
        bottom_color: edge_color(borders.bottom),
        left_color: edge_color(borders.left),
        edges,
        styles,
    })
}

fn edge_color(border: Option<Border>) -> [f32; 3] {
    border.map_or([0.0; 3], |border| rgb_f32(border.color))
}

fn style_flag(style: BorderStyle) -> u32 {
    match style {
        BorderStyle::Light => STYLE_LIGHT,
        BorderStyle::Heavy => STYLE_HEAVY,
        BorderStyle::Double => STYLE_DOUBLE,
        BorderStyle::Rounded => STYLE_ROUNDED,
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
    use super::{
        build_border_instances, EDGE_BOTTOM_BIT, EDGE_LEFT_BIT, EDGE_TOP_BIT, STYLE_HEAVY,
        STYLE_LIGHT, STYLE_ROUNDED,
    };
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
    fn cell_instance_packs_present_edges() {
        let mut grid = Grid::new(1, 2);
        let cell = grid.get_mut(0, 1);
        cell.borders.top = Some(Border {
            style: BorderStyle::Heavy,
            color: Rgb::new(255, 0, 0),
        });
        cell.borders.bottom = Some(Border {
            style: BorderStyle::Light,
            color: Rgb::new(0, 255, 0),
        });

        let instances = build_border_instances(&grid);

        assert_eq!(instances.len(), 1, "one instance per bordered cell");
        let instance = instances[0];
        assert_eq!(instance.cell, [1.0, 0.0]);
        assert_eq!(instance.edges, EDGE_TOP_BIT | EDGE_BOTTOM_BIT);
        assert_eq!(instance.styles & 0xff, STYLE_HEAVY, "top style in low byte");
        assert_eq!(
            (instance.styles >> 16) & 0xff,
            STYLE_LIGHT,
            "bottom style in third byte"
        );
        assert_eq!(instance.top_color, [1.0, 0.0, 0.0]);
        assert_eq!(instance.bottom_color, [0.0, 1.0, 0.0]);
    }

    #[test]
    fn rounded_corner_packs_rounded_style_per_edge() {
        let mut grid = Grid::new(1, 1);
        let teal = Rgb::new(10, 20, 30);
        let cell = grid.get_mut(0, 0);
        cell.borders.top = Some(Border {
            style: BorderStyle::Rounded,
            color: teal,
        });
        cell.borders.left = Some(Border {
            style: BorderStyle::Rounded,
            color: teal,
        });

        let instances = build_border_instances(&grid);

        assert_eq!(instances.len(), 1);
        let instance = instances[0];
        assert_eq!(instance.edges, EDGE_TOP_BIT | EDGE_LEFT_BIT);
        assert_eq!(instance.styles & 0xff, STYLE_ROUNDED, "top style");
        assert_eq!((instance.styles >> 24) & 0xff, STYLE_ROUNDED, "left style");
    }
}
