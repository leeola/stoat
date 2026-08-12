//! Instanced stroked-path pass.
//!
//! Draws each [`Polyline`] as anti-aliased capsule segments off the cell grid,
//! above the grid with its own z-order. The protocol's only non-axis-aligned
//! primitive, added so the commit graph can draw lane and merge lines. Endpoints
//! ride in cell-fraction units and the vertex shader scales them by the live
//! cell size, so a path tracks font zoom.

use crate::render::{
    globals_offset, occlusion_globals, CellMetrics, CompositeSlot, CompositeSlots, Occluder,
    GLOBALS_SLOTS, GLOBALS_SLOT_STRIDE,
};
use bytemuck::{Pod, Zeroable};
use std::mem;
use stoatty_term::grid::{Polyline, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBinding, BufferBindingType, BufferDescriptor, BufferSize, BufferUsages,
    ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor, Queue,
    RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource,
    ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in segments, allocated up front. Grows by doubling
/// when a frame exceeds it.
const INITIAL_CAPACITY: usize = 16;

/// Sixteenths of a cell per whole cell, the unit a [`Polyline`] is declared in.
const SIXTEENTHS: f32 = 16.0;

/// The per-segment instance data.
///
/// One instance covers one segment rather than one whole path, which keeps the
/// vertex stage to a fixed six-vertex quad. A path of N points expands to N-1
/// instances, and a single point expands to one zero-length instance that the
/// capsule SDF resolves as a dot.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct PolylineInstance {
    p0: [f32; 2],
    p1: [f32; 2],
    half_width: f32,
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
    /// Rows every endpoint is shifted down by, in cell fractions, which the
    /// vertex stage adds before converting to pixels.
    ///
    /// Carried here rather than baked into the instances so a gliding pool can
    /// reuse the segments it built when its content last changed. A glide moves
    /// every endpoint by the same amount, so nothing per-instance has to change.
    shift_rows: f32,
    _pad: u32,
}

/// The instanced stroked-path pipeline and its per-frame buffers.
pub struct PolylinePass {
    pipeline: RenderPipeline,
    bind_group_layout: BindGroupLayout,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    /// The instances last uploaded, so an unchanged frame skips the write.
    last_instances: Vec<PolylineInstance>,
    /// Where each frame's instances are built, before being compared against
    /// [`Self::last_instances`] and traded with it. Chrome rebuilds every frame
    /// and changes on almost none of them, so building into a buffer the pass
    /// keeps spares an allocation the frame would otherwise discard.
    built: Vec<PolylineInstance>,
    /// Where a composited pool's segments are built, separate from [`Self::built`] so
    /// a pool draw leaves the live comparison intact.
    ///
    /// Rebuilt only on a frame whose content changed, the eased shift riding the
    /// globals instead, so a glide leaves this alone. Holding the buffer is what
    /// keeps a rebuild that does happen from allocating.
    composite_built: Vec<PolylineInstance>,
    count: u32,
    /// Per-pool segments of the pools composited over the live grid, one slot per
    /// pool so every pool can be prepared before any of them draws. Separate from
    /// [`Self::instances`] so a pool draw leaves the live paths intact.
    composite_slots: CompositeSlots<CompositeSlot>,
    /// One occluder per live panel, read by the fragment shader to discard path
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

impl PolylinePass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(
        device: &Device,
        format: TextureFormat,
        metrics: CellMetrics,
    ) -> PolylinePass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("polyline"),
            source: ShaderSource::Wgsl(
                crate::render::with_occlusion(include_str!("../shaders/polyline.wgsl")).into(),
            ),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("polyline globals"),
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
            label: Some("polyline"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("polyline"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<PolylineInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32,
                        3 => Float32x3,
                        4 => Uint32,
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
            label: Some("polyline globals"),
            size: GLOBALS_SLOTS as u64 * GLOBALS_SLOT_STRIDE,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let occluders = alloc_occluders(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &occluders);

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        PolylinePass {
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

    /// Replace the cell metrics so the next frame lays out paths at the new size.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    /// Upload the frame's uniform, one occluder per live panel, and one instance
    /// per path segment.
    ///
    /// `resolution` is the surface size in physical pixels. `occluders` are the
    /// live panels' rects, built once per frame and shared with the other
    /// off-grid passes. Reallocates the instance or occluder buffer only when
    /// its count outgrows the current capacity, and skips the instance upload
    /// when they match what was last sent.
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        polylines: &[Polyline],
        occluders: &[Occluder],
        resolution: [f32; 2],
    ) {
        // With no path to draw now and none drawn last frame, nothing reads this
        // pass's buffers, so the frame skips it without touching the GPU. The frame
        // that empties the list still runs, which is what drops the count to zero
        // and stops the draw.
        if polylines.is_empty() && self.count == 0 {
            return;
        }

        self.upload_occluders(device, queue, occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count: occluders.len() as u32,
            occlude_all: 0,
            // The live grid does not glide, so its paths sit where they are given.
            shift_rows: 0.0,
            _pad: 0,
        };
        crate::render::upload_globals(queue, &self.globals, 0, globals, &mut self.last_globals);

        build_polyline_instances_into(polylines, &mut self.built);
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

    /// Upload one instance per segment of a pool grid being composited.
    ///
    /// The pool's eased `shift_rows` goes to the globals, which the vertex stage
    /// adds to every endpoint, so a frame that only glides writes the uniform and
    /// keeps the segments `content_changed` last rebuilt.
    ///
    /// Writes `slot`'s buffer, separate from the live [`Self::prepare`] and from
    /// the other pools', reusing the shared globals uniform the live pass already
    /// wrote this frame. The slot itself is allocated on first use.
    ///
    /// The paths are occluded against `occluders` with the seq test bypassed, so a
    /// line gliding beneath a modal is hidden by it. Which panels reach that list is
    /// the caller's decision, since all four of a pool's composite passes share it.
    ///
    /// See also:
    /// - [`pool_occluders_into`](crate::render::pool_occluders_into) for how a pool's list is
    ///   narrowed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        polylines: &[Polyline],
        occluders: &[Occluder],
        resolution: [f32; 2],
        shift_rows: f32,
        content_changed: bool,
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
            shift_rows,
            _pad: 0,
        };
        queue.write_buffer(
            &self.globals,
            u64::from(globals_offset(slot)),
            bytemuck::bytes_of(&globals),
        );

        // A glide moves every endpoint by the same amount, which the globals
        // write above has just re-applied, so unchanged content reuses last
        // frame's segments rather than rebuilding and re-uploading all of them.
        if !content_changed {
            return;
        }

        build_polyline_instances_into(polylines, &mut self.composite_built);

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

    /// Record the path draw into `render_pass`.
    ///
    /// A no-op when the grid carries no path. Run after the grid text so a line
    /// sits over the cells. The caller restores the full scissor first, since
    /// the region-text draw leaves one set.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[0]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        render_pass.draw(0..6, 0..self.count);
    }

    /// Record the composited pool's path draw into `render_pass`.
    ///
    /// A no-op until [`Self::prepare_composite`] has run for `slot`. Reads that
    /// slot's buffer, so a pool draw leaves both the live paths a prior
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
        label: Some("polyline instances"),
        size: (capacity * size_of::<PolylineInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn alloc_occluders(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("polyline occluders"),
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
        label: Some("polyline globals"),
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

/// Flatten each path into `out` as one instance per segment, in draw order,
/// converting the sixteenth-cell wire units to the cell-fraction units the shader
/// scales by the cell size.
///
/// `out` is cleared first, so a caller holding it across frames sees only this
/// frame's segments.
///
/// A single-point path yields one zero-length segment, which the capsule SDF
/// draws as a dot. An empty path yields nothing.
///
/// Endpoints are given as declared. A pool composite glides its paths through the
/// `shift_rows` uniform rather than through their endpoints, so instances built
/// here outlive every frame that only moves the pool.
fn build_polyline_instances_into(polylines: &[Polyline], out: &mut Vec<PolylineInstance>) {
    out.clear();
    for polyline in polylines {
        let point = |[x, y]: [i16; 2]| [f32::from(x) / SIXTEENTHS, f32::from(y) / SIXTEENTHS];
        let half_width = f32::from(polyline.width) / SIXTEENTHS / 2.0;
        let color = rgb_f32(polyline.color);

        match polyline.points.as_slice() {
            [] => {},
            [only] => out.push(PolylineInstance {
                p0: point(*only),
                p1: point(*only),
                half_width,
                color,
                seq: polyline.seq,
            }),
            points => out.extend(points.windows(2).map(|pair| PolylineInstance {
                p0: point(pair[0]),
                p1: point(pair[1]),
                half_width,
                color,
                seq: polyline.seq,
            })),
        }
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
    use super::{build_polyline_instances_into, PolylineInstance, PolylinePass};
    use crate::{gpu::headless_device, render::CellMetrics};
    use stoatty_term::grid::{Polyline, Rgb};
    use wgpu::{
        naga::{
            front::wgsl,
            valid::{Capabilities, ValidationFlags, Validator},
        },
        TextureFormat,
    };

    fn path(points: &[[i16; 2]]) -> Polyline {
        Polyline {
            points: points.to_vec(),
            width: 8,
            color: Rgb::new(220, 50, 47),
            seq: 7,
        }
    }

    /// The vertex stage's shift, mirrored here so a test can check the endpoints it
    /// builds still land where the baked-in shift used to put them.
    fn shader_point(point: [f32; 2], shift_rows: f32) -> [f32; 2] {
        [point[0], point[1] + shift_rows]
    }

    /// [`build_polyline_instances_into`] into a fresh buffer, for the assertions that
    /// only want one frame's instances and have no buffer to reuse.
    fn build_polyline_instances(polylines: &[Polyline]) -> Vec<PolylineInstance> {
        let mut instances = Vec::new();
        build_polyline_instances_into(polylines, &mut instances);
        instances
    }

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_occlusion(include_str!(
            "../shaders/polyline.wgsl"
        )))
        .expect("parse polyline");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate polyline");
    }

    #[test]
    fn a_reused_polyline_scratch_holds_only_this_frame_s_segments() {
        let paths = [path(&[[0, 0], [0, 16], [16, 32]])];

        let mut scratch = build_polyline_instances(&paths);
        scratch.extend(build_polyline_instances(&paths));
        build_polyline_instances_into(&paths, &mut scratch);

        assert_eq!(
            bytemuck::cast_slice::<PolylineInstance, u8>(&scratch),
            bytemuck::cast_slice::<PolylineInstance, u8>(&build_polyline_instances(&paths)),
            "reuse clears the stale segments and rebuilds only the frame's own"
        );
    }

    #[test]
    fn instances_map_sixteenths_to_cell_fractions() {
        let instances = build_polyline_instances(&[path(&[[8, 16], [24, 32]])]);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].p0, [0.5, 1.0]);
        assert_eq!(instances[0].p1, [1.5, 2.0]);
        assert_eq!(instances[0].half_width, 0.25, "8/16 of a cell, halved");
        assert_eq!(
            instances[0].color,
            [220.0 / 255.0, 50.0 / 255.0, 47.0 / 255.0]
        );
        assert_eq!(instances[0].seq, 7, "the occlusion seq is carried");
    }

    #[test]
    fn a_path_becomes_one_instance_per_segment() {
        let instances = build_polyline_instances(&[path(&[[0, 0], [0, 16], [16, 32]])]);

        assert_eq!(instances.len(), 2, "three points span two segments");
        assert_eq!(instances[0].p0, [0.0, 0.0]);
        assert_eq!(instances[0].p1, [0.0, 1.0]);
        assert_eq!(
            instances[1].p0, instances[0].p1,
            "segments chain end to start"
        );
        assert_eq!(instances[1].p1, [1.0, 2.0]);
    }

    #[test]
    fn a_single_point_becomes_one_zero_length_segment() {
        let instances = build_polyline_instances(&[path(&[[8, 8]])]);

        assert_eq!(instances.len(), 1);
        assert_eq!(
            instances[0].p0, instances[0].p1,
            "a dot is a capsule with no length"
        );
    }

    #[test]
    fn an_empty_path_yields_no_instances() {
        assert!(build_polyline_instances(&[path(&[])]).is_empty());
    }

    #[test]
    fn composite_shift_offsets_both_endpoints_by_whole_cells() {
        let instances = build_polyline_instances(&[path(&[[0, 16], [0, 32]])]);

        assert_eq!(
            (instances[0].p0, instances[0].p1),
            ([0.0, 1.0], [0.0, 2.0]),
            "the built endpoints are where the path was declared, shift or no shift",
        );
        assert_eq!(
            (
                shader_point(instances[0].p0, -0.5),
                shader_point(instances[0].p1, -0.5),
            ),
            ([0.0, 0.5], [0.0, 1.5]),
            "both ends shift equally once the shader applies it",
        );
    }

    /// A pool composite glides its paths by writing `shift_rows` into the globals.
    /// What a shift-only frame must leave alone is the segments the last content
    /// change built, which is the one part of that a test can observe.
    #[test]
    fn a_shift_only_composite_frame_rebuilds_no_segments() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("polyline composite test: no wgpu adapter, skipping");
            return;
        };
        let mut pass = PolylinePass::new(
            &device,
            TextureFormat::Rgba8Unorm,
            CellMetrics {
                font_size: 10.0,
                width: 6.0,
                height: 12.0,
                scale_factor: 1.0,
            },
        );

        let first = [path(&[[0, 16], [0, 32]])];
        pass.prepare_composite(&device, &queue, &first, &[], [64.0, 64.0], 0.0, true, 0, 0);
        let built = pass.composite_built.clone();

        // A different path on purpose. A rebuild that ran anyway would put its
        // segments in the buffer, so matching proves the gate and not just the
        // arithmetic.
        let moved = [path(&[[0, 48], [0, 64], [16, 80]])];
        pass.prepare_composite(
            &device,
            &queue,
            &moved,
            &[],
            [64.0, 64.0],
            -0.5,
            false,
            0,
            0,
        );

        assert_eq!(
            bytemuck::cast_slice::<PolylineInstance, u8>(&pass.composite_built),
            bytemuck::cast_slice::<PolylineInstance, u8>(&built),
            "a frame that only glides leaves the segments where the last one left them",
        );
    }

    /// The pass skips its GPU write when the rebuilt instances match the last
    /// upload, so an unchanged graph must compare equal across rebuilds and a
    /// real change must not.
    #[test]
    fn rebuilt_paths_compare_equal_until_one_changes() {
        use crate::render::upload_needed;

        let paths = [path(&[[0, 0], [0, 16]])];
        let first = build_polyline_instances(&paths);
        assert!(
            !upload_needed(&build_polyline_instances(&paths), &first),
            "an unchanged path rebuilds to the same bytes, so no upload is needed",
        );

        let moved = [path(&[[1, 0], [0, 16]])];
        assert!(
            upload_needed(&build_polyline_instances(&moved), &first),
            "a moved endpoint must reach the GPU",
        );
        assert!(
            upload_needed(&build_polyline_instances(&[]), &first),
            "so must dropping the path entirely",
        );
    }
}
