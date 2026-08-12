//! Instanced status-icon pass.
//!
//! Draws each [`Icon`] as a fixed signed-distance silhouette -- a disc, triangle,
//! or square by [`IconKind`] -- over its cell block, above the grid with its own
//! z-order. Icons are not cell attributes: like overlays they float over the
//! grid, so this pass runs after the overlays and alpha-blends its shapes on top.

use crate::render::{CellMetrics, Occluder};
use bytemuck::{Pod, Zeroable};
use std::mem;
use stoatty_term::grid::{Icon, IconKind, Rgb};
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
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
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
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
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
    /// The instances last uploaded, so an unchanged frame skips the write.
    last_instances: Vec<IconInstance>,
    /// Where each frame's instances are built, before being compared against
    /// [`Self::last_instances`] and traded with it. Chrome rebuilds every frame
    /// and changes on almost none of them, so building into a buffer the pass
    /// keeps spares an allocation the frame would otherwise discard.
    built: Vec<IconInstance>,
    count: u32,
    /// One occluder per live panel, read by the fragment shader to discard icon
    /// fragments a later box covers. Bound alongside the globals, and rebuilt
    /// into a new bind group whenever it reallocates.
    occluders: Buffer,
    /// The occluder list last written to [`Self::occluders`], so a frame whose
    /// panels have not moved skips the upload. Panels change on layout events, not
    /// per frame, so most frames match.
    last_occluders: Vec<Occluder>,
    occluder_capacity: usize,
    /// The uniform last written, so an unchanged frame skips that write too.
    last_globals: Option<Globals>,
    metrics: CellMetrics,
}

impl IconPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> IconPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("icon"),
            source: ShaderSource::Wgsl(
                crate::render::with_occlusion(include_str!("../shaders/icon.wgsl")).into(),
            ),
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
            last_instances: Vec::new(),
            built: Vec::new(),
            occluders,
            last_occluders: Vec::new(),
            last_globals: None,
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
    /// `resolution` is the surface size in physical pixels. `occluders` are the
    /// live panels' rects, built once per frame and shared with the bar pass.
    /// Reallocates the instance or occluder buffer only when its count outgrows
    /// the current capacity, and skips the instance upload when they match what
    /// was last sent.
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        icons: &[Icon],
        occluders: &[Occluder],
        resolution: [f32; 2],
    ) {
        // With no icon to draw now and none drawn last frame, nothing reads this
        // pass's buffers, so the frame skips it without touching the GPU. The frame
        // that empties the list still runs, which is what drops the count to zero
        // and stops the draw.
        if icons.is_empty() && self.count == 0 {
            return;
        }

        self.upload_occluders(device, queue, occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count: occluders.len() as u32,
            _pad: [0; 3],
        };
        crate::render::upload_globals(queue, &self.globals, 0, globals, &mut self.last_globals);

        build_icon_instances_into(icons, &mut self.built);
        self.count = self.built.len() as u32;
        if self.built.is_empty() {
            return;
        }

        if !crate::render::upload_needed(&self.built, &self.last_instances) {
            return;
        }

        if self.built.len() > self.capacity {
            self.capacity = self.built.len().next_power_of_two();
            self.instances = alloc_instances(device, self.capacity);
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&self.built));
        mem::swap(&mut self.built, &mut self.last_instances);
    }

    /// Upload the panel occluders, reallocating the buffer and rebuilding the
    /// bind group when the panel count outgrows the current capacity.
    ///
    /// A list matching the one already in the buffer is not re-sent. Panels move on
    /// layout events rather than per frame, so most frames land here, including the
    /// idle ones a blinking cursor drives.
    fn upload_occluders(&mut self, device: &Device, queue: &Queue, occluders: &[Occluder]) {
        if !crate::render::upload_needed(occluders, &self.last_occluders) {
            return;
        }

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

        self.last_occluders.clear();
        self.last_occluders.extend_from_slice(occluders);
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

/// One instance per icon, in draw order, into a vector of the caller's.
#[cfg(test)]
fn build_icon_instances(icons: &[Icon]) -> Vec<IconInstance> {
    let mut instances = Vec::new();
    build_icon_instances_into(icons, &mut instances);
    instances
}

/// Build into `out` one instance per icon, in draw order.
///
/// `out` is cleared first, so a reused scratch buffer holds only this frame's
/// icons.
fn build_icon_instances_into(icons: &[Icon], out: &mut Vec<IconInstance>) {
    out.clear();
    out.extend(icons.iter().map(|icon| IconInstance {
        cell: [icon.left as f32, icon.top as f32],
        size: icon.size.max(1) as f32,
        color: rgb_f32(icon.color),
        kind: kind_code(icon.kind),
        offset: [icon.offset[0] as f32, icon.offset[1] as f32],
        seq: icon.seq,
    }));
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
    use super::{build_icon_instances, build_icon_instances_into, IconInstance, KIND_WARNING};
    use stoatty_term::grid::{Icon, IconKind, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_occlusion(include_str!(
            "../shaders/icon.wgsl"
        )))
        .expect("parse icon");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate icon");
    }

    #[test]
    fn a_reused_icon_scratch_holds_only_this_frame_s_icons() {
        let icons = [Icon {
            top: 3,
            left: 5,
            kind: IconKind::Warning,
            color: Rgb::new(255, 200, 0),
            size: 2,
            offset: [3, 6],
            seq: 9,
        }];

        let mut scratch = build_icon_instances(&icons);
        scratch.extend(build_icon_instances(&icons));
        build_icon_instances_into(&icons, &mut scratch);

        assert_eq!(
            bytemuck::cast_slice::<IconInstance, u8>(&scratch),
            bytemuck::cast_slice::<IconInstance, u8>(&build_icon_instances(&icons)),
            "reuse clears the stale icons and rebuilds only the frame's own"
        );
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
