//! Instanced color-bar pass.
//!
//! Fills each [`Bar`] as a solid sub-cell rectangle off the cell grid, above the
//! grid with its own z-order. Bars are not cell attributes: like the overlays
//! and icons they float over the grid, so a gutter can pack thin status bars and
//! a hairline separator into a fraction of a cell. The rectangle rides in
//! cell-fraction units and the vertex shader scales it by the live cell size, so
//! bars track font zoom.

use crate::render::CellMetrics;
use bytemuck::{Pod, Zeroable};
use stoatty_term::grid::{Bar, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingType, BlendState, Buffer, BufferBindingType, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor,
    Queue, RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in bars, allocated up front. Grows by doubling when
/// a frame exceeds it.
const INITIAL_CAPACITY: usize = 16;

/// Sixteenths of a cell per whole cell, the unit a [`Bar`] is declared in.
const SIXTEENTHS: f32 = 16.0;

/// Per-bar instance: the top-left and the size in cell-fraction units, and the
/// fill color.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BarInstance {
    origin: [f32; 2],
    size: [f32; 2],
    color: [f32; 3],
}

/// Uniform shared by every instance: the surface resolution and cell size the
/// vertex shader maps cell-fraction coordinates through.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
}

/// The instanced color-bar pipeline and its per-frame buffers.
pub struct BarPass {
    pipeline: RenderPipeline,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    count: u32,
    metrics: CellMetrics,
}

impl BarPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> BarPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("bar"),
            source: ShaderSource::Wgsl(include_str!("../shaders/bar.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("bar globals"),
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
            label: Some("bar"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("bar"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<BarInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x3,
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
            label: Some("bar globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("bar globals"),
            layout: &bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        BarPass {
            pipeline,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            metrics,
        }
    }

    /// Replace the cell metrics so the next frame lays out bars at the new size.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    /// Upload the frame's uniform and one instance per grid bar.
    ///
    /// `resolution` is the surface size in physical pixels. Reallocates the
    /// instance buffer only when the bar count outgrows the current capacity.
    pub fn prepare(&mut self, device: &Device, queue: &Queue, bars: &[Bar], resolution: [f32; 2]) {
        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let instances = build_bar_instances(bars);
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

    /// Record the bar draw into `render_pass`.
    ///
    /// A no-op when the grid carries no bar. Run after the grid text so a bar
    /// sits over the cells; the caller restores the full scissor first, since the
    /// region-text draw leaves one set.
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
        label: Some("bar instances"),
        size: (capacity * size_of::<BarInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// One instance per bar, in draw order, converting the sixteenth-cell wire units
/// to the cell-fraction units the shader scales by the cell size.
fn build_bar_instances(bars: &[Bar]) -> Vec<BarInstance> {
    bars.iter()
        .map(|bar| BarInstance {
            origin: [f32::from(bar.x) / SIXTEENTHS, f32::from(bar.y) / SIXTEENTHS],
            size: [
                f32::from(bar.width) / SIXTEENTHS,
                f32::from(bar.height) / SIXTEENTHS,
            ],
            color: rgb_f32(bar.color),
        })
        .collect()
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
    use super::build_bar_instances;
    use stoatty_term::grid::{Bar, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(include_str!("../shaders/bar.wgsl")).expect("parse bar");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate bar");
    }

    #[test]
    fn bar_instance_maps_sixteenths_to_cell_fractions() {
        let bars = [Bar {
            x: 8,
            y: 16,
            width: 3,
            height: 24,
            color: Rgb::new(220, 50, 47),
        }];

        let instances = build_bar_instances(&bars);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].origin, [0.5, 1.0]);
        assert_eq!(instances[0].size, [3.0 / 16.0, 1.5]);
        assert_eq!(
            instances[0].color,
            [220.0 / 255.0, 50.0 / 255.0, 47.0 / 255.0]
        );
    }
}
