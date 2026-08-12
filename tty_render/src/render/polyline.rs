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

/// Points one instance carries, which is the longest path drawn in a single
/// blend.
///
/// The commit graph is the sizing case. A bending edge runs a stub, eight curve
/// steps, and a stub, so eleven points, and a straight edge two. Twelve covers
/// every path it draws without a split.
const MAX_PATH_POINTS: usize = 12;

/// The per-path instance data.
///
/// One instance covers a whole path rather than one segment, because two
/// capsules meeting at a shared endpoint overlap and composite their
/// anti-aliased fringes twice, which beads every joint. The fragment stage takes
/// the minimum distance over [`Self::point_count`] points instead, so a path
/// blends once.
///
/// The points ride in the instance rather than a storage buffer the instance
/// indexes, because the pass binds one bind group across the live grid and every
/// composited pool while each keeps its own instance buffer. A shared arena has
/// no frame boundary to reset against, and a per-slot arena needs a bind group
/// per slot.
///
/// Slots past the count repeat the last point, so the loop bound is the only
/// thing that decides how much of the array is read.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct PolylineInstance {
    points: [[f32; 2]; MAX_PATH_POINTS],
    /// The path's bounding box in cell fractions, as `[min_x, min_y, max_x,
    /// max_y]`, which the vertex stage grows by the stroke reach to size the
    /// quad. A per-path quad has no single segment to orient along, unlike the
    /// one capsule's it replaces.
    bounds: [f32; 4],
    color: [f32; 3],
    half_width: f32,
    seq: u32,
    point_count: u32,
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
                    // Six vec4s carry the twelve points, then the bounds, the
                    // color paired with the half width, and the seq paired with
                    // the point count.
                    attributes: &vertex_attr_array![
                        0 => Float32x4,
                        1 => Float32x4,
                        2 => Float32x4,
                        3 => Float32x4,
                        4 => Float32x4,
                        5 => Float32x4,
                        6 => Float32x4,
                        7 => Float32x4,
                        8 => Uint32x2,
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
        let half_width = f32::from(polyline.width) / SIXTEENTHS / 2.0;
        let color = rgb_f32(polyline.color);
        let points: Vec<[f32; 2]> = polyline
            .points
            .iter()
            .map(|&[x, y]| [f32::from(x) / SIXTEENTHS, f32::from(y) / SIXTEENTHS])
            .collect();

        match points.as_slice() {
            [] => {},
            // A dot is the degenerate segment the capsule distance already
            // resolves to a disc, so it needs no case of its own downstream.
            [only] => out.push(path_instance(
                &[*only, *only],
                half_width,
                color,
                polyline.seq,
            )),
            // Chunks overlap by their joint point, so the split path stays
            // continuous and only that one joint blends twice.
            points => out.extend(
                points
                    .chunks(MAX_PATH_POINTS - 1)
                    .enumerate()
                    .map(|(index, chunk)| {
                        let start = index * (MAX_PATH_POINTS - 1);
                        let end = (start + chunk.len() + 1).min(points.len());
                        path_instance(&points[start..end], half_width, color, polyline.seq)
                    })
                    .filter(|instance| instance.point_count > 1),
            ),
        }
    }
}

/// One instance covering `points`, which must hold at least two and at most
/// [`MAX_PATH_POINTS`] of them.
fn path_instance(
    points: &[[f32; 2]],
    half_width: f32,
    color: [f32; 3],
    seq: u32,
) -> PolylineInstance {
    let last = *points.last().expect("a path instance holds a point");
    let mut slots = [last; MAX_PATH_POINTS];
    slots[..points.len()].copy_from_slice(points);

    let bounds = points.iter().fold(
        [f32::MAX, f32::MAX, f32::MIN, f32::MIN],
        |[min_x, min_y, max_x, max_y], &[x, y]| {
            [min_x.min(x), min_y.min(y), max_x.max(x), max_y.max(y)]
        },
    );

    PolylineInstance {
        points: slots,
        bounds,
        color,
        half_width,
        seq,
        point_count: points.len() as u32,
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
        BufferDescriptor, BufferUsages, Color, CommandEncoderDescriptor, Device, Extent3d, LoadOp,
        MapMode, Operations, Origin3d, PollType, Queue, RenderPassColorAttachment,
        RenderPassDescriptor, StoreOp, TexelCopyBufferInfo, TexelCopyBufferLayout,
        TexelCopyTextureInfo, TextureAspect, TextureDescriptor, TextureDimension, TextureFormat,
        TextureUsages, TextureViewDescriptor,
    };

    /// The square readback target's edge, in pixels. Four bytes a texel makes a
    /// row exactly the 256-byte copy alignment, so the readback needs no stride
    /// padding.
    const TARGET: u32 = 64;

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
        assert_eq!(instances[0].points[..2], [[0.5, 1.0], [1.5, 2.0]]);
        assert_eq!(instances[0].half_width, 0.25, "8/16 of a cell, halved");
        assert_eq!(
            instances[0].color,
            [220.0 / 255.0, 50.0 / 255.0, 47.0 / 255.0]
        );
        assert_eq!(instances[0].seq, 7, "the occlusion seq is carried");
    }

    #[test]
    fn a_path_becomes_one_instance() {
        let instances = build_polyline_instances(&[path(&[[0, 0], [0, 16], [16, 32]])]);

        assert_eq!(instances.len(), 1, "every segment blends together or beads");
        assert_eq!(instances[0].point_count, 3);
        assert_eq!(
            instances[0].points[..3],
            [[0.0, 0.0], [0.0, 1.0], [1.0, 2.0]]
        );
        assert_eq!(
            instances[0].bounds,
            [0.0, 0.0, 1.0, 2.0],
            "the quad bounds every point"
        );
        assert!(
            instances[0].points[3..].iter().all(|&p| p == [1.0, 2.0]),
            "the slots past the count repeat the last point"
        );
    }

    /// A path past the cap splits, and the chunks share the point they meet at
    /// so the stroke stays continuous across the split.
    #[test]
    fn a_path_past_the_cap_splits_on_a_shared_point() {
        let points: Vec<[i16; 2]> = (0..16).map(|step| [0, step * 16]).collect();
        let instances = build_polyline_instances(&[path(&points)]);

        assert_eq!(instances.len(), 2, "sixteen points need a second instance");
        assert_eq!(instances[0].point_count, 12);
        assert_eq!(instances[1].point_count, 5);
        assert_eq!(
            instances[1].points[0], instances[0].points[11],
            "the chunks meet on one shared point"
        );
    }

    #[test]
    fn a_single_point_becomes_one_zero_length_segment() {
        let instances = build_polyline_instances(&[path(&[[8, 8]])]);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].point_count, 2);
        assert_eq!(
            instances[0].points[0], instances[0].points[1],
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
            instances[0].points[..2],
            [[0.0, 1.0], [0.0, 2.0]],
            "the built endpoints are where the path was declared, shift or no shift",
        );
        assert_eq!(
            (
                shader_point(instances[0].points[0], -0.5),
                shader_point(instances[0].points[1], -0.5),
            ),
            ([0.0, 0.5], [0.0, 1.5]),
            "both ends shift equally once the shader applies it",
        );
    }

    /// Draw `paths` alone onto a black [`TARGET`]-square target and read the red
    /// channel back, one byte a pixel.
    ///
    /// Red alone because the fixture strokes in pure red over black, so the byte
    /// at a pixel is the coverage the path resolved there.
    fn render_red(device: &Device, queue: &Queue, paths: &[Polyline]) -> Vec<u8> {
        let mut pass = PolylinePass::new(
            device,
            TextureFormat::Rgba8Unorm,
            CellMetrics {
                font_size: 10.0,
                width: 12.0,
                height: 12.0,
                scale_factor: 1.0,
            },
        );
        pass.prepare_composite(
            device,
            queue,
            paths,
            &[],
            [TARGET as f32, TARGET as f32],
            0.0,
            true,
            0,
            0,
        );

        let size = Extent3d {
            width: TARGET,
            height: TARGET,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&TextureDescriptor {
            label: Some("polyline joint target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&TextureViewDescriptor::default());
        let readback = device.create_buffer(&BufferDescriptor {
            label: Some("polyline joint readback"),
            size: u64::from(TARGET) * u64::from(TARGET) * 4,
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("polyline joint"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(Color::BLACK),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.draw_composite(&mut render_pass, 0, 0);
        }
        encoder.copy_texture_to_buffer(
            TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            TexelCopyBufferInfo {
                buffer: &readback,
                layout: TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(TARGET * 4),
                    rows_per_image: None,
                },
            },
            size,
        );
        queue.submit(Some(encoder.finish()));

        readback.slice(..).map_async(MapMode::Read, |_| {});
        device
            .poll(PollType::wait_indefinitely())
            .expect("poll readback");
        let rgba = readback.slice(..).get_mapped_range().to_vec();

        rgba.chunks_exact(4).map(|texel| texel[0]).collect()
    }

    /// Two capsules meeting at a joint overlap, so drawing them apart composites
    /// their anti-aliased fringes twice and beads the joint.
    ///
    /// Splitting a straight run at its midpoint covers exactly the same shape as
    /// the unsplit run, so the two render identically or the joint beads.
    #[test]
    fn a_joint_blends_no_heavier_than_the_run_it_splits() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("polyline joint test: no wgpu adapter, skipping");
            return;
        };

        // An odd width puts the fringe on fractional coverage, where a second
        // blend shows. An even one saturates it and hides the bead.
        let run = |points: &[[i16; 2]]| Polyline {
            points: points.to_vec(),
            width: 7,
            color: Rgb::new(255, 0, 0),
            seq: 0,
        };

        let unsplit = render_red(&device, &queue, &[run(&[[32, 32], [32, 64]])]);
        let split = render_red(&device, &queue, &[run(&[[32, 32], [32, 48], [32, 64]])]);

        assert!(
            unsplit.iter().any(|&byte| byte > 0 && byte < 255),
            "the fixture paints a partly covered fringe to compare"
        );

        let bead = split
            .iter()
            .zip(&unsplit)
            .position(|(split, unsplit)| split != unsplit)
            .map(|at| {
                let index = at as u32;
                (index % TARGET, index / TARGET, split[at], unsplit[at])
            });
        assert_eq!(
            bead, None,
            "the joint adds no coverage of its own, at (x, y, split, unsplit)"
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
