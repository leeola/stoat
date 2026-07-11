//! Instanced status-icon pass.
//!
//! Draws each [`Icon`] as a fixed signed-distance silhouette -- a disc, triangle,
//! or square by [`IconKind`] -- over its cell block, above the grid with its own
//! z-order. Icons are not cell attributes: like overlays they float over the
//! grid, so this pass runs after the overlays and alpha-blends its shapes on top.

use crate::render::{build_occluders, CellMetrics, Occluder};
use bytemuck::{Pod, Zeroable};
use stoatty_term::grid::{Icon, IconKind, Panel, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, Buffer,
    BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites, Device,
    FragmentState, PipelineLayoutDescriptor, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, TextureFormat,
    VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in icons, allocated up front. Grows by doubling
/// when a frame exceeds it.
const INITIAL_CAPACITY: usize = 16;

/// Kind codes packed into each instance, matching the shader's constants.
const KIND_ERROR: u32 = 0;
const KIND_WARNING: u32 = 1;
const KIND_INFO: u32 = 2;

/// The per-icon instance data. Carries the anchor cell, the block size in
/// cells, the color, the icon kind, the anchor offset, and the
/// declaration-order seq the fragment shader occludes by.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct IconInstance {
    cell: [f32; 2],
    size: f32,
    color: [f32; 3],
    kind: u32,
    offset: [f32; 2],
    seq: u32,
}

/// The uniform shared by every instance. Carries the surface resolution and
/// cell size the vertex shader maps cell coordinates through, and the
/// panel-occluder count the fragment shader loops over. Padded to 32 bytes to
/// match the WGSL uniform layout.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    panel_count: u32,
    _pad: [u32; 3],
}

/// The instanced icon pipeline and its per-frame buffers.
pub struct IconPass {
    pipeline: RenderPipeline,
    bind_group_layout: BindGroupLayout,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    count: u32,
    /// One occluder per live panel, read by the fragment shader to discard icon
    /// fragments a later box covers. Bound alongside the globals, and rebuilt
    /// into a new bind group whenever it reallocates.
    occluders: Buffer,
    occluder_capacity: usize,
    metrics: CellMetrics,
}

impl IconPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> IconPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("icon"),
            source: ShaderSource::Wgsl(include_str!("../shaders/icon.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("icon globals"),
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
            label: Some("icon"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("icon"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<IconInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32,
                        2 => Float32x3,
                        3 => Uint32,
                        4 => Float32x2,
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
            label: Some("icon globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let occluders = alloc_occluders(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &occluders);

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        IconPass {
            pipeline,
            bind_group_layout,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            occluders,
            occluder_capacity: INITIAL_CAPACITY,
            metrics,
        }
    }

    /// Replace the cell metrics so the next frame lays out icons at the new size.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    /// Upload the frame's uniform, one occluder per live panel, and one instance
    /// per grid icon.
    ///
    /// `resolution` is the surface size in physical pixels. `panels` are the live
    /// panels the icons occlude against. Reallocates the instance or occluder
    /// buffer only when its count outgrows the current capacity.
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        icons: &[Icon],
        panels: &[Panel],
        resolution: [f32; 2],
    ) {
        let occluders = build_occluders(panels);
        self.upload_occluders(device, queue, &occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count: occluders.len() as u32,
            _pad: [0; 3],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let instances = build_icon_instances(icons);
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

    /// Record the icon draw into `render_pass`.
    ///
    /// A no-op when the grid carries no icon. Run after the overlays so an icon
    /// can sit over a popover; the caller restores the full scissor first, since
    /// the overlay-content draw leaves one set.
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
        label: Some("icon instances"),
        size: (capacity * size_of::<IconInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn alloc_occluders(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("icon occluders"),
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
        label: Some("icon globals"),
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

/// One instance per icon, in draw order.
fn build_icon_instances(icons: &[Icon]) -> Vec<IconInstance> {
    icons
        .iter()
        .map(|icon| IconInstance {
            cell: [icon.left as f32, icon.top as f32],
            size: icon.size.max(1) as f32,
            color: rgb_f32(icon.color),
            kind: kind_code(icon.kind),
            offset: [icon.offset[0] as f32, icon.offset[1] as f32],
            seq: icon.seq,
        })
        .collect()
}

fn kind_code(kind: IconKind) -> u32 {
    match kind {
        IconKind::Error => KIND_ERROR,
        IconKind::Warning => KIND_WARNING,
        IconKind::Info => KIND_INFO,
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
    use super::{build_icon_instances, KIND_WARNING};
    use stoatty_term::grid::{Icon, IconKind, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(include_str!("../shaders/icon.wgsl")).expect("parse icon");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate icon");
    }

    #[test]
    fn icon_instance_maps_anchor_size_color_kind_and_offset() {
        let icons = [Icon {
            top: 3,
            left: 5,
            kind: IconKind::Warning,
            color: Rgb::new(255, 200, 0),
            size: 2,
            offset: [3, 6],
            seq: 9,
        }];

        let instances = build_icon_instances(&icons);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].cell, [5.0, 3.0]);
        assert_eq!(instances[0].size, 2.0);
        assert_eq!(instances[0].color, [1.0, 200.0 / 255.0, 0.0]);
        assert_eq!(instances[0].kind, KIND_WARNING);
        assert_eq!(instances[0].offset, [3.0, 6.0]);
        assert_eq!(instances[0].seq, 9, "the icon's occlusion seq is carried");
    }
}
