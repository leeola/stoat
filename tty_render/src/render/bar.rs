//! Instanced color-bar pass.
//!
//! Fills each [`Bar`] as a solid sub-cell rectangle off the cell grid, above the
//! grid with its own z-order. Bars are not cell attributes: like the overlays
//! and icons they float over the grid, so a gutter can pack thin status bars and
//! a hairline separator into a fraction of a cell. The rectangle rides in
//! cell-fraction units and the vertex shader scales it by the live cell size, so
//! bars track font zoom.

use crate::render::{build_occluders, composite_occlusion, CellMetrics, Occluder};
use bytemuck::{Pod, Zeroable};
use stoatty_term::grid::{Bar, Panel, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, Buffer,
    BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites, Device,
    FragmentState, PipelineLayoutDescriptor, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, TextureFormat,
    VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in bars, allocated up front. Grows by doubling when
/// a frame exceeds it.
const INITIAL_CAPACITY: usize = 16;

/// Sixteenths of a cell per whole cell, the unit a [`Bar`] is declared in.
const SIXTEENTHS: f32 = 16.0;

/// The per-bar instance data. Carries the top-left and the size in
/// cell-fraction units, the fill color, and the declaration-order seq the
/// fragment shader occludes by.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BarInstance {
    origin: [f32; 2],
    size: [f32; 2],
    color: [f32; 3],
    seq: u32,
}

/// The uniform shared by every instance. Carries the surface resolution and
/// cell size the vertex shader maps cell-fraction coordinates through, the
/// panel-occluder count the fragment shader loops over, and the `occlude_all`
/// flag that bypasses the seq test for a pool composite beneath every box.
/// Padded to 32 bytes to match the WGSL uniform layout.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    panel_count: u32,
    occlude_all: u32,
    _pad: [u32; 2],
}

/// The instanced color-bar pipeline and its per-frame buffers.
pub struct BarPass {
    pipeline: RenderPipeline,
    bind_group_layout: BindGroupLayout,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    count: u32,
    /// Bars of a pool grid being composited over the live grid, built by
    /// [`Self::prepare_composite`] into a buffer separate from
    /// [`Self::instances`] so a pool draw leaves the live bars intact.
    composite_instances: Buffer,
    composite_capacity: usize,
    composite_count: u32,
    /// One occluder per live panel, read by the fragment shader to discard bar
    /// fragments a later box covers. Bound alongside the globals, and rebuilt
    /// into a new bind group whenever it reallocates.
    occluders: Buffer,
    occluder_capacity: usize,
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
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Storage { read_only: true },
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
            ],
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
            label: Some("bar globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let occluders = alloc_occluders(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &occluders);

        let instances = alloc_instances(device, INITIAL_CAPACITY);
        let composite_instances = alloc_instances(device, INITIAL_CAPACITY);

        BarPass {
            pipeline,
            bind_group_layout,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            composite_instances,
            composite_capacity: INITIAL_CAPACITY,
            composite_count: 0,
            occluders,
            occluder_capacity: INITIAL_CAPACITY,
            metrics,
        }
    }

    /// Replace the cell metrics so the next frame lays out bars at the new size.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    /// Upload the frame's uniform, one occluder per live panel, and one instance
    /// per grid bar.
    ///
    /// `resolution` is the surface size in physical pixels. `panels` are the live
    /// panels the bars occlude against. Reallocates the instance or occluder
    /// buffer only when its count outgrows the current capacity.
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        bars: &[Bar],
        panels: &[Panel],
        resolution: [f32; 2],
    ) {
        let occluders = build_occluders(panels);
        self.upload_occluders(device, queue, &occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count: occluders.len() as u32,
            occlude_all: 0,
            _pad: [0; 2],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let instances = build_bar_instances(bars, 0.0);
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

    /// Upload the panel occluders, reallocating the buffer and rebuilding the
    /// bind group when the panel count outgrows the current capacity.
    fn upload_occluders(&mut self, device: &Device, queue: &Queue, occluders: &[Occluder]) {
        if occluders.len() > self.occluder_capacity {
            self.occluder_capacity = occluders.len().next_power_of_two();
            self.occluders = alloc_occluders(device, self.occluder_capacity);
            self.bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals,
                &self.occluders,
            );
        }
        if !occluders.is_empty() {
            queue.write_buffer(&self.occluders, 0, bytemuck::cast_slice(occluders));
        }
    }

    /// Upload one instance per bar of a pool grid being composited, offset down
    /// by the pool's eased `shift_rows` so the bars glide with the page cells.
    ///
    /// Writes a buffer separate from the live [`Self::prepare`], reusing the
    /// shared globals uniform the live pass already wrote this frame. Reallocates
    /// only when the bar count outgrows the composite capacity.
    ///
    /// `occludable` marks a pane pool that sits under every box. Its bars are
    /// then occluded against `panels` with the seq test bypassed, so a gutter
    /// bar gliding beneath a modal is hidden by it. A non-pane pool passes
    /// `false` and its bars never occlude, since they are a box's own content.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        bars: &[Bar],
        panels: &[Panel],
        resolution: [f32; 2],
        shift_rows: f32,
        occludable: bool,
    ) {
        let occluders = build_occluders(panels);
        self.upload_occluders(device, queue, &occluders);
        let (panel_count, occlude_all) = composite_occlusion(occludable, &occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count,
            occlude_all,
            _pad: [0; 2],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let instances = build_bar_instances(bars, shift_rows);
        self.composite_count = instances.len() as u32;
        if instances.is_empty() {
            return;
        }

        if instances.len() > self.composite_capacity {
            self.composite_capacity = instances.len().next_power_of_two();
            self.composite_instances = alloc_instances(device, self.composite_capacity);
        }
        queue.write_buffer(
            &self.composite_instances,
            0,
            bytemuck::cast_slice(&instances),
        );
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

    /// Record the composited pool's bar draw into `render_pass`.
    ///
    /// A no-op until [`Self::prepare_composite`] has run. Reads the composite
    /// buffer, so a pool draw leaves the live bars a prior [`Self::prepare`]
    /// uploaded untouched. Inherits the pool pass's scissor.
    pub fn draw_composite(&self, render_pass: &mut RenderPass<'_>) {
        if self.composite_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.composite_instances.slice(..));
        render_pass.draw(0..6, 0..self.composite_count);
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

fn alloc_occluders(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("bar occluders"),
        size: (capacity * size_of::<Occluder>()) as u64,
        usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Bind the globals uniform (binding 0) and the panel-occluder storage buffer
/// (binding 1). Rebuilt whenever the occluder buffer reallocates, since the bind
/// group holds a reference to the specific buffer.
fn make_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    globals: &Buffer,
    occluders: &Buffer,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("bar globals"),
        layout,
        entries: &[
            BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            },
            BindGroupEntry {
                binding: 1,
                resource: occluders.as_entire_binding(),
            },
        ],
    })
}

/// One instance per bar, in draw order, converting the sixteenth-cell wire units
/// to the cell-fraction units the shader scales by the cell size.
///
/// `shift_rows` offsets each bar down by that many cells, baked into the origin.
/// The live path passes zero. A pool composite passes the eased sub-cell scroll
/// so slot-bound bars glide with the page, since the bar shader carries no
/// scroll uniform of its own.
fn build_bar_instances(bars: &[Bar], shift_rows: f32) -> Vec<BarInstance> {
    bars.iter()
        .map(|bar| BarInstance {
            origin: [
                f32::from(bar.x) / SIXTEENTHS,
                f32::from(bar.y) / SIXTEENTHS + shift_rows,
            ],
            size: [
                f32::from(bar.width) / SIXTEENTHS,
                f32::from(bar.height) / SIXTEENTHS,
            ],
            color: rgb_f32(bar.color),
            seq: bar.seq,
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
            seq: 7,
        }];

        let instances = build_bar_instances(&bars, 0.0);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].origin, [0.5, 1.0]);
        assert_eq!(instances[0].size, [3.0 / 16.0, 1.5]);
        assert_eq!(
            instances[0].color,
            [220.0 / 255.0, 50.0 / 255.0, 47.0 / 255.0]
        );
        assert_eq!(instances[0].seq, 7, "the bar's occlusion seq is carried");
    }

    #[test]
    fn composite_shift_offsets_the_bar_origin_by_whole_cells() {
        let bars = [Bar {
            x: 0,
            y: 16,
            width: 2,
            height: 16,
            color: Rgb::new(1, 2, 3),
            seq: 0,
        }];

        let instances = build_bar_instances(&bars, -0.5);

        assert_eq!(
            instances[0].origin,
            [0.0, 0.5],
            "row 1 shifted up half a cell lands at 0.5"
        );
    }
}
