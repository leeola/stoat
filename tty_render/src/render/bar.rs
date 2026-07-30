//! Instanced color-bar pass.
//!
//! Fills each [`Bar`] as a solid sub-cell rectangle off the cell grid, above the
//! grid with its own z-order. Bars are not cell attributes: like the overlays
//! and icons they float over the grid, so a gutter can pack thin status bars and
//! a hairline separator into a fraction of a cell. The rectangle rides in
//! cell-fraction units and the vertex shader scales it by the live cell size, so
//! bars track font zoom.

use crate::render::{
    globals_offset, occlusion_globals, CellMetrics, CompositeSlot, CompositeSlots, Occluder,
    GLOBALS_SLOTS, GLOBALS_SLOT_STRIDE,
};
use bytemuck::{Pod, Zeroable};
use std::mem;
use stoatty_term::grid::{Bar, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBinding, BufferBindingType, BufferDescriptor, BufferSize, BufferUsages,
    ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor, Queue,
    RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource,
    ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
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
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
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
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
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
    /// The instances last uploaded, so an unchanged frame skips the write.
    last_instances: Vec<BarInstance>,
    /// Where each frame's instances are built, before being compared against
    /// [`Self::last_instances`] and traded with it. Chrome rebuilds every frame
    /// and changes on almost none of them, so building into a buffer the pass
    /// keeps spares an allocation the frame would otherwise discard.
    built: Vec<BarInstance>,
    /// Where a composited pool's bars are built, separate from [`Self::built`] so a
    /// pool draw leaves the live comparison intact.
    ///
    /// A pool's bars carry its eased shift in their origins, so they are rebuilt on
    /// every frame the pool glides rather than only when its content changes. Holding
    /// the buffer is what keeps that from allocating per tick.
    composite_built: Vec<BarInstance>,
    count: u32,
    /// Per-pool bars of the pools composited over the live grid, one slot per
    /// pool so every pool can be prepared before any of them draws. Separate from
    /// [`Self::instances`] so a pool draw leaves the live bars intact.
    composite_slots: CompositeSlots<CompositeSlot>,
    /// One occluder per live panel, read by the fragment shader to discard bar
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
                        // One slot per composited pool plus the live grid's, so
                        // every pool's globals coexist and a draw selects its own.
                        has_dynamic_offset: true,
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
            size: GLOBALS_SLOTS as u64 * GLOBALS_SLOT_STRIDE,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let occluders = alloc_occluders(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &occluders);

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        BarPass {
            pipeline,
            bind_group_layout,
            globals,
            bind_group,
            instances,
            last_instances: Vec::new(),
            built: Vec::new(),
            composite_built: Vec::new(),
            capacity: INITIAL_CAPACITY,
            count: 0,
            composite_slots: CompositeSlots::new(),
            occluders,
            last_occluders: Vec::new(),
            last_globals: None,
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
    /// `resolution` is the surface size in physical pixels. `occluders` are the
    /// live panels' rects, built once per frame and shared with the icon pass.
    /// Reallocates the instance or occluder buffer only when its count outgrows
    /// the current capacity, and skips the instance upload when they match what
    /// was last sent.
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        bars: &[Bar],
        occluders: &[Occluder],
        resolution: [f32; 2],
    ) {
        // With no bar to draw now and none drawn last frame, nothing reads this
        // pass's buffers, so the frame skips it without touching the GPU. The frame
        // that empties the list still runs, which is what drops the count to zero
        // and stops the draw.
        if bars.is_empty() && self.count == 0 {
            return;
        }

        self.upload_occluders(device, queue, occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count: occluders.len() as u32,
            occlude_all: 0,
            _pad: [0; 2],
        };
        crate::render::upload_globals(queue, &self.globals, 0, globals, &mut self.last_globals);

        build_bar_instances_into(bars, 0.0, &mut self.built);
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

    /// Upload one instance per bar of a pool grid being composited, offset down
    /// by the pool's eased `shift_rows` so the bars glide with the page cells.
    ///
    /// Writes `slot`'s buffer, separate from the live [`Self::prepare`] and from
    /// the other pools', reusing the shared globals uniform the live pass already
    /// wrote this frame. Reallocates only when the bar count outgrows that slot's
    /// capacity, and allocates the slot itself on first use.
    ///
    /// The bars are occluded against `occluders` with the seq test bypassed, so a
    /// gutter bar gliding beneath a modal is hidden by it. Which panels reach that
    /// list is the caller's decision, since all four of a pool's composite passes
    /// share it.
    ///
    /// See also:
    /// - [`pool_occluders_into`](crate::render::pool_occluders_into) for how a pool's list is
    ///   narrowed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        bars: &[Bar],
        occluders: &[Occluder],
        resolution: [f32; 2],
        shift_rows: f32,
        pool: u32,
        slot: usize,
    ) {
        self.upload_occluders(device, queue, occluders);
        let (panel_count, occlude_all) = occlusion_globals(occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count,
            occlude_all,
            _pad: [0; 2],
        };
        queue.write_buffer(
            &self.globals,
            u64::from(globals_offset(slot)),
            bytemuck::bytes_of(&globals),
        );

        build_bar_instances_into(bars, shift_rows, &mut self.composite_built);

        let target = self.composite_slots.entry(pool, || new_slot(device));
        target.count = self.composite_built.len() as u32;
        if self.composite_built.is_empty() {
            return;
        }

        if self.composite_built.len() > target.capacity {
            target.capacity = self.composite_built.len().next_power_of_two();
            target.instances = alloc_instances(device, target.capacity);
        }
        queue.write_buffer(
            &target.instances,
            0,
            bytemuck::cast_slice(&self.composite_built),
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
        render_pass.set_bind_group(0, &self.bind_group, &[0]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        render_pass.draw(0..6, 0..self.count);
    }

    /// Record the composited pool's bar draw into `render_pass`.
    ///
    /// A no-op until [`Self::prepare_composite`] has run for `slot`. Reads that
    /// slot's buffer, so a pool draw leaves both the live bars a prior
    /// [`Self::prepare`] uploaded and the other pools' slots untouched. Inherits
    /// the pool pass's scissor.
    pub fn draw_composite(&self, render_pass: &mut RenderPass<'_>, pool: u32, slot: usize) {
        let Some(target) = self.composite_slots.get(pool).filter(|s| s.count > 0) else {
            return;
        };

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[globals_offset(slot)]);
        render_pass.set_vertex_buffer(0, target.instances.slice(..));
        render_pass.draw(0..6, 0..target.count);
    }
}

/// An empty composite slot at the initial capacity, for a pool being composited
/// for the first time.
fn new_slot(device: &Device) -> CompositeSlot {
    CompositeSlot {
        instances: alloc_instances(device, INITIAL_CAPACITY),
        capacity: INITIAL_CAPACITY,
        count: 0,
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
                // Bound to one slot's worth, so a dynamic offset selects a slot
                // rather than sliding a window over the whole buffer.
                resource: BindingResource::Buffer(BufferBinding {
                    buffer: globals,
                    offset: 0,
                    size: BufferSize::new(size_of::<Globals>() as u64),
                }),
            },
            BindGroupEntry {
                binding: 1,
                resource: occluders.as_entire_binding(),
            },
        ],
    })
}

/// Build into `out` one instance per bar, in draw order, converting the
/// sixteenth-cell wire units to the cell-fraction units the shader scales by the
/// cell size.
///
/// `out` is cleared first, so a caller holding it across frames sees only this
/// frame's bars.
///
/// `shift_rows` offsets each bar down by that many cells, baked into the origin.
/// The live path passes zero. A pool composite passes the eased sub-cell scroll
/// so slot-bound bars glide with the page, since the bar shader carries no
/// scroll uniform of its own. A gliding pool therefore rebuilds every frame,
/// since the shift lives in the instances rather than in a uniform.
fn build_bar_instances_into(bars: &[Bar], shift_rows: f32, out: &mut Vec<BarInstance>) {
    out.clear();
    out.extend(bars.iter().map(|bar| BarInstance {
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
    }));
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
    use super::{build_bar_instances_into, BarInstance};
    use stoatty_term::grid::{Bar, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    /// [`build_bar_instances_into`] into a fresh buffer, for the assertions that only
    /// want one frame's instances and have no buffer to reuse.
    fn build_bar_instances(bars: &[Bar], shift_rows: f32) -> Vec<BarInstance> {
        let mut instances = Vec::new();
        build_bar_instances_into(bars, shift_rows, &mut instances);
        instances
    }

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(include_str!("../shaders/bar.wgsl")).expect("parse bar");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate bar");
    }

    #[test]
    fn a_reused_bar_scratch_holds_only_this_frame_s_bars() {
        let bars = [Bar {
            x: 8,
            y: 16,
            width: 3,
            height: 24,
            color: Rgb::new(220, 50, 47),
            seq: 7,
        }];

        let mut scratch = build_bar_instances(&bars, 0.0);
        scratch.extend(build_bar_instances(&bars, 0.0));
        build_bar_instances_into(&bars, 0.0, &mut scratch);

        assert_eq!(
            bytemuck::cast_slice::<BarInstance, u8>(&scratch),
            bytemuck::cast_slice::<BarInstance, u8>(&build_bar_instances(&bars, 0.0)),
            "reuse clears the stale bars and rebuilds only the frame's own"
        );
    }

    /// The pass skips its GPU write when the rebuilt instances match the last
    /// upload, so unchanged chrome must compare equal across rebuilds and a
    /// real change must not.
    #[test]
    fn rebuilt_bars_compare_equal_until_one_changes() {
        use crate::render::upload_needed;

        let bars = [Bar {
            x: 8,
            y: 16,
            width: 3,
            height: 24,
            color: Rgb::new(220, 50, 47),
            seq: 7,
        }];
        let first = build_bar_instances(&bars, 0.0);
        assert!(
            !upload_needed(&build_bar_instances(&bars, 0.0), &first),
            "an unchanged bar rebuilds to the same bytes, so no upload is needed",
        );

        let moved = [Bar { x: 9, ..bars[0] }];
        assert!(
            upload_needed(&build_bar_instances(&moved, 0.0), &first),
            "a moved bar must reach the GPU",
        );
        assert!(
            upload_needed(&build_bar_instances(&[], 0.0), &first),
            "so must dropping the bar entirely",
        );
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
