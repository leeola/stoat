//! Instanced floating-overlay pass.
//!
//! Draws each [`Overlay`] as a filled box with a one-pixel border, above the
//! grid with its own z-order. Overlays are not cell attributes: they float over
//! the grid and occlude the cells beneath, so this pass runs last and writes
//! opaque pixels rather than alpha-blending into the text.
//!
//! This is the compositing layer for popovers and completion menus; the region
//! is the box itself, with any text inside it drawn separately.

use crate::render::CellMetrics;
use bytemuck::{Pod, Zeroable};
use stoatty_term::grid::{Grid, Overlay, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingType, BlendState, Buffer, BufferBindingType, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor,
    Queue, RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in overlays, allocated up front. Grows by doubling
/// when a frame exceeds it.
const INITIAL_CAPACITY: usize = 16;

/// Drop-shadow blur radius in physical pixels: the distance over which the
/// shadow's alpha fades to zero past the shadow rectangle.
const SHADOW_MARGIN: f32 = 16.0;

/// Drop-shadow displacement in physical pixels, down and to the right, so an
/// overlay reads as floating above the grid rather than pasted onto it.
const SHADOW_OFFSET: [f32; 2] = [5.0, 7.0];

/// Corner radius in physical pixels for the rounded box, so a popover reads like
/// an IDE tooltip rather than a sharp block. The shader clamps it to the box's
/// half-extent, so a small box rounds proportionally.
const CORNER_RADIUS: f32 = 6.0;

/// Per-overlay instance: the anchor cell, the size in cells, the two colors, the
/// drop-shadow displacement and blur radius, and the corner radius.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct OverlayInstance {
    cell: [f32; 2],
    size: [f32; 2],
    fill: [f32; 3],
    border: [f32; 3],
    shadow_offset: [f32; 2],
    shadow_margin: f32,
    corner_radius: f32,
    anchor_offset: [f32; 2],
}

/// Uniform shared by every instance: the surface resolution and cell size the
/// vertex shader maps cell coordinates through.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
}

/// The instanced overlay pipeline and its per-frame buffers.
pub struct OverlayPass {
    pipeline: RenderPipeline,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    count: u32,
    metrics: CellMetrics,
}

impl OverlayPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> OverlayPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("overlay"),
            source: ShaderSource::Wgsl(include_str!("../shaders/overlay.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("overlay globals"),
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
            label: Some("overlay"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("overlay"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<OverlayInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x3,
                        3 => Float32x3,
                        4 => Float32x2,
                        5 => Float32,
                        6 => Float32,
                        7 => Float32x2,
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
            label: Some("overlay globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("overlay globals"),
            layout: &bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        OverlayPass {
            pipeline,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            metrics,
        }
    }

    /// Replace the cell metrics so the next frame lays out cells at the new size.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    /// Upload the frame's uniform and one instance per grid overlay.
    ///
    /// `resolution` is the surface size in physical pixels. Reallocates the
    /// instance buffer only when the overlay count outgrows the current
    /// capacity.
    pub fn prepare(&mut self, device: &Device, queue: &Queue, grid: &Grid, resolution: [f32; 2]) {
        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let instances = build_overlay_instances(grid.overlays());
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

    /// Record the overlay draw into `render_pass`.
    ///
    /// A no-op when the grid carries no overlay. Run last, after the grid and
    /// cursor, so overlays sit on top.
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
        label: Some("overlay instances"),
        size: (capacity * size_of::<OverlayInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// One instance per overlay, in draw order.
fn build_overlay_instances(overlays: &[Overlay]) -> Vec<OverlayInstance> {
    overlays
        .iter()
        .map(|overlay| OverlayInstance {
            cell: [overlay.left as f32, overlay.top as f32],
            size: [overlay.width as f32, overlay.height as f32],
            fill: rgb_f32(overlay.fill),
            border: rgb_f32(overlay.border),
            shadow_offset: SHADOW_OFFSET,
            shadow_margin: SHADOW_MARGIN,
            corner_radius: CORNER_RADIUS,
            anchor_offset: [overlay.offset[0] as f32, overlay.offset[1] as f32],
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
    use super::build_overlay_instances;
    use stoatty_term::grid::{Overlay, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn shader_is_valid_wgsl() {
        let module =
            wgsl::parse_str(include_str!("../shaders/overlay.wgsl")).expect("parse overlay");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate overlay");
    }

    #[test]
    fn overlay_instance_maps_anchor_size_and_colors() {
        let overlays = [Overlay {
            top: 3,
            left: 5,
            width: 8,
            height: 4,
            fill: Rgb::new(255, 0, 0),
            border: Rgb::new(0, 255, 0),
            content_fg: Rgb::new(0, 0, 255),
            scale: 1,
            offset: [-4, 6],
            content: "x".to_owned(),
        }];

        let instances = build_overlay_instances(&overlays);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].cell, [5.0, 3.0]);
        assert_eq!(instances[0].size, [8.0, 4.0]);
        assert_eq!(instances[0].fill, [1.0, 0.0, 0.0]);
        assert_eq!(instances[0].border, [0.0, 1.0, 0.0]);
        assert_eq!(instances[0].shadow_offset, super::SHADOW_OFFSET);
        assert_eq!(instances[0].shadow_margin, super::SHADOW_MARGIN);
        assert_eq!(instances[0].corner_radius, super::CORNER_RADIUS);
        assert_eq!(instances[0].anchor_offset, [-4.0, 6.0]);
    }
}
