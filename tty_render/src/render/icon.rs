//! Instanced status-icon pass.
//!
//! Draws each [`Icon`] as a fixed signed-distance silhouette -- a disc, triangle,
//! or square by [`IconKind`] -- over its cell block, above the grid with its own
//! z-order. Icons are not cell attributes: like overlays they float over the
//! grid, so this pass runs after the overlays and alpha-blends its shapes on top.

use crate::render::{
    globals_offset, rgb_f32, CellMetrics, CompositeSlot, CompositeSlots, Occluder, OccluderBuffer,
    PoolOccluders, RowAnchor, GLOBALS_SLOTS, GLOBALS_SLOT_STRIDE,
};
use bytemuck::{Pod, Zeroable};
use std::mem;
use stoatty_term::grid::{Icon, IconKind};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBinding, BufferBindingType, BufferDescriptor, BufferSize, BufferUsages,
    ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor, Queue,
    RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource,
    ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
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
/// cell size the vertex shader maps cell coordinates through, the
/// panel-occluder count the fragment shader loops over, the `occlude_all` flag
/// that bypasses the seq test for a pool composite beneath every box, and the
/// text band an icon is placed against. Padded to 64 bytes to match the WGSL
/// uniform layout.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    panel_count: u32,
    occlude_all: u32,
    /// Baseline offset from a cell's top, in physical pixels.
    baseline: f32,
    /// Height of a capital above that baseline, in physical pixels.
    cap_height: f32,
    /// Rows every icon is shifted down by, which the vertex stage adds to each
    /// anchor.
    ///
    /// Carried here rather than baked into the instances, so a gliding pool
    /// reuses the instances it built when its content last changed. A glide moves
    /// every icon by the same amount, so nothing per-instance has to change.
    shift_rows: f32,
    _pad0: u32,
    /// Cell the grid's own (0, 0) is drawn at, which the vertex stage adds to
    /// each anchor.
    ///
    /// A pool composite hands over icons positioned within its region rather
    /// than the viewport, so the region's origin is what puts them on the
    /// screen. Zero for the live grid.
    origin_cells: [f32; 2],
    /// The cell size the occluder rects scale by, which is the live grid's.
    ///
    /// A pool composited at its own font size lays its icons out at
    /// [`Self::cell_size`], but the boxes over it are cells of the live grid.
    occluder_cell: [f32; 2],
    /// The [`RowAnchor`] the icon rows and the occluder rows are placed by.
    bottom_row_y: f32,
    last_row: u32,
}

const _: () = assert!(size_of::<Globals>() as u64 <= GLOBALS_SLOT_STRIDE);

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
    /// keeps spares an allocation that the frame otherwise discards.
    built: Vec<IconInstance>,
    /// Where a composited pool's icons are built, separate from [`Self::built`]
    /// so a pool draw leaves the live comparison intact.
    ///
    /// Rebuilt only on a frame whose content changed, the eased shift riding
    /// the globals instead, so a glide leaves this alone.
    composite_built: Vec<IconInstance>,
    count: u32,
    /// Per-pool icons of the pools composited over the live grid, one slot per
    /// pool, so the frame prepares every pool before any of them draws. Separate
    /// from [`Self::instances`] so a pool draw leaves the live icons intact.
    composite_slots: CompositeSlots<CompositeSlot>,
    /// One occluder per live panel, read by the fragment shader to discard icon
    /// fragments a later box covers. Bound alongside the globals, and rebuilt
    /// into a new bind group whenever it reallocates.
    occluders: OccluderBuffer,
    /// The occluders the composited pools read, bound by
    /// [`Self::composite_bind_group`].
    ///
    /// A pool occludes against a different set of panels than the live grid
    /// does, and a frame prepares every pass before any of them draws, so the
    /// pools need a buffer of their own.
    composite_occluders: OccluderBuffer,
    /// Bound by the composite draws, over the same globals as
    /// [`Self::bind_group`] and [`Self::composite_occluders`].
    composite_bind_group: BindGroup,
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
                crate::render::with_rows(&crate::render::with_occlusion(include_str!(
                    "../shaders/icon.wgsl"
                )))
                .into(),
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
                buffers: &[Some(VertexBufferLayout {
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
                })],
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
            size: GLOBALS_SLOTS as u64 * GLOBALS_SLOT_STRIDE,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let occluders = OccluderBuffer::new(device, "icon occluders", INITIAL_CAPACITY);
        let composite_occluders =
            OccluderBuffer::new(device, "icon composite occluders", INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &occluders.buffer);
        let composite_bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &globals,
            &composite_occluders.buffer,
        );

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
            composite_built: Vec::new(),
            composite_slots: CompositeSlots::new(),
            occluders,
            composite_occluders,
            composite_bind_group,
            last_globals: None,
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
    /// `anchor` places the icon rows.
    ///
    /// Reallocates the instance or occluder buffer only when its count outgrows
    /// the current capacity, and skips the instance upload when they match what
    /// was last sent.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        icons: &[Icon],
        occluders: &[Occluder],
        resolution: [f32; 2],
        band: [f32; 2],
        anchor: RowAnchor,
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
            occlude_all: 0,
            baseline: band[0],
            cap_height: band[1],
            // The live grid does not glide, so its icons sit where they are given.
            shift_rows: 0.0,
            _pad0: 0,
            origin_cells: [0.0; 2],
            occluder_cell: [self.metrics.width, self.metrics.height],
            bottom_row_y: anchor.bottom_row_y,
            last_row: anchor.last_row,
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

    /// Upload the live grid's panel occluders, rebuilding the bind group when
    /// the list outgrows the buffer and it has to be replaced.
    ///
    /// See also:
    /// - [`Self::upload_composite_occluders`] for the pools' list, which is a different one and
    ///   needs a buffer of its own.
    fn upload_occluders(&mut self, device: &Device, queue: &Queue, occluders: &[Occluder]) {
        if self.occluders.upload(device, queue, occluders) {
            self.bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals,
                &self.occluders.buffer,
            );
        }
    }

    /// Upload the composited pools' panel occluders. See
    /// [`Self::upload_occluders`].
    fn upload_composite_occluders(
        &mut self,
        device: &Device,
        queue: &Queue,
        occluders: &[Occluder],
    ) {
        if self.composite_occluders.upload(device, queue, occluders) {
            self.composite_bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals,
                &self.composite_occluders.buffer,
            );
        }
    }

    /// Upload one instance per icon of a pool grid being composited.
    ///
    /// The pool's eased `shift_rows` goes to the globals, which the vertex stage
    /// adds to every anchor, so a frame that only glides writes the uniform and
    /// keeps the instances `content_changed` last rebuilt.
    ///
    /// Writes `slot`'s buffer, separate from the live [`Self::prepare`] and from
    /// the other pools', and allocates the slot itself on first use. The icons
    /// are occluded against `occluders` with the seq test bypassed, so an icon
    /// gliding beneath a modal is hidden by it.
    ///
    /// `metrics` is the cell box the icons lay out in, which a pool at its own
    /// font size does not share with the live grid. `band` is the live grid's
    /// text band, which a pool at the live font size shares.
    ///
    /// See also:
    /// - [`PoolOccluders`] for why every pool of a frame reads one list.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        icons: &[Icon],
        metrics: CellMetrics,
        occluders: PoolOccluders<'_>,
        resolution: [f32; 2],
        band: [f32; 2],
        shift_rows: f32,
        origin_cells: [f32; 2],
        content_changed: bool,
        pool: u32,
        slot: usize,
    ) {
        self.upload_composite_occluders(device, queue, occluders.all);
        let (panel_count, occlude_all) = occluders.globals();

        let globals = Globals {
            resolution,
            cell_size: [metrics.width, metrics.height],
            panel_count,
            occlude_all,
            baseline: band[0],
            cap_height: band[1],
            shift_rows,
            _pad0: 0,
            origin_cells,
            occluder_cell: [self.metrics.width, self.metrics.height],
            bottom_row_y: RowAnchor::UNIFORM.bottom_row_y,
            last_row: RowAnchor::UNIFORM.last_row,
        };
        queue.write_buffer(
            &self.globals,
            u64::from(globals_offset(slot)),
            bytemuck::bytes_of(&globals),
        );

        // A glide moves every icon by the same amount, which the globals write
        // above has just re-applied, so unchanged content reuses last frame's
        // instances rather than rebuilding and re-uploading all of them.
        if !content_changed {
            return;
        }

        build_icon_instances_into(icons, &mut self.composite_built);

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

    /// Record the icon draw into `render_pass`.
    ///
    /// A no-op when the grid carries no icon. Run after the overlays, so an icon
    /// sits over a popover. The caller restores the full scissor first, since the
    /// overlay-content draw leaves one set.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[0]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        render_pass.draw(0..6, 0..self.count);
    }

    /// Record the composited pool's icon draw into `render_pass`.
    ///
    /// A no-op until [`Self::prepare_composite`] has run for `slot`. Reads that
    /// slot's buffer, so a pool draw leaves both the live icons a prior
    /// [`Self::prepare`] uploaded and the other pools' slots untouched. Inherits
    /// the pool pass's scissor.
    pub fn draw_composite(&self, render_pass: &mut RenderPass<'_>, pool: u32, slot: usize) {
        let Some(target) = self.composite_slots.get(pool).filter(|s| s.count > 0) else {
            return;
        };

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.composite_bind_group, &[globals_offset(slot)]);
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
        label: Some("icon instances"),
        size: (capacity * size_of::<IconInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
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
/// icons. Anchors are given as declared. A pool composite glides its icons
/// through the `shift_rows` uniform, so instances built here outlive every
/// frame that only moves the pool.
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

#[cfg(test)]
mod tests {
    use super::{
        build_icon_instances, build_icon_instances_into, IconInstance, IconPass, KIND_WARNING,
    };
    use crate::{
        render::{pool_occluders_into, CellMetrics, PoolOccluders, RowAnchor},
        test_support::{read_back, require_headless_device},
    };
    use stoatty_term::grid::{BorderStyle, Icon, IconKind, Panel, PanelShadow, Rgb};
    use wgpu::{
        naga::{
            front::wgsl,
            valid::{Capabilities, ValidationFlags, Validator},
        },
        Color, CommandEncoderDescriptor, Device, Extent3d, LoadOp, Operations, Queue, RenderPass,
        RenderPassColorAttachment, RenderPassDescriptor, StoreOp, TextureDescriptor,
        TextureDimension, TextureFormat, TextureUsages, TextureViewDescriptor,
    };

    /// The square readback target's edge, in pixels. Four bytes a texel makes a
    /// row exactly the 256-byte copy alignment, so the readback needs no stride
    /// padding.
    const TARGET: u32 = 64;

    /// A whole-pixel cell, so an icon moved by whole cells and half a row lands
    /// on exact pixels.
    const METRICS: CellMetrics = CellMetrics {
        font_size: 16.0,
        width: 16.0,
        height: 16.0,
        scale_factor: 1.0,
    };

    /// A band that centers the shape in its cell.
    const BAND: [f32; 2] = [12.0, 8.0];

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_rows(&crate::render::with_occlusion(
            include_str!("../shaders/icon.wgsl"),
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

    /// A composited pool's icon lands at the pool's origin and its eased
    /// shift, where the live draw of the same icon lands at its own anchor.
    /// A box over the pool hides it.
    #[test]
    fn a_composited_icon_glides_with_its_pool() {
        let (device, queue) = require_headless_device();
        let resolution = [TARGET as f32, TARGET as f32];
        let icon = Icon {
            top: 0,
            left: 0,
            kind: IconKind::Error,
            color: Rgb::new(255, 0, 0),
            size: 1,
            offset: [0, 0],
            seq: 0,
        };

        let mut live = IconPass::new(&device, TextureFormat::Rgba8Unorm, METRICS);
        live.prepare(
            &device,
            &queue,
            &[icon],
            &[],
            resolution,
            BAND,
            RowAnchor::UNIFORM,
        );
        let at_rest = red_corner(&device, &queue, |pass| live.draw(pass));

        let composite = |occluders: PoolOccluders<'_>| {
            let mut pool = IconPass::new(&device, TextureFormat::Rgba8Unorm, METRICS);
            pool.prepare_composite(
                &device,
                &queue,
                &[icon],
                METRICS,
                occluders,
                resolution,
                BAND,
                0.5,
                [1.0, 1.0],
                true,
                0,
                0,
            );
            red_corner(&device, &queue, |pass| pool.draw_composite(pass, 0, 0))
        };
        let gliding = composite(PoolOccluders::new(&[], 0, true));

        let mut boxes = Vec::new();
        let above = pool_occluders_into(&[covering_panel()], &[], &mut boxes);
        let covered = composite(PoolOccluders::new(&boxes, above, true));

        let (x, y) = at_rest.expect("the live icon paints");
        assert_eq!(
            (gliding, covered),
            (Some((x + 16, y + 24)), None),
            "one cell right, one and a half rows down, and nothing under a box",
        );
    }

    /// The boards draw an error as a filled disc, and a warning and info with
    /// their glyph cut out. So the middle of an icon takes the icon color for
    /// an error alone.
    #[test]
    fn only_an_error_icon_fills_its_middle() {
        let (device, queue) = require_headless_device();

        // A three-cell icon strokes its glyph wider than a pixel, so the middle
        // of its 48 px block reads either the whole color or the bare ground.
        let middle = |kind: IconKind| {
            let icon = Icon {
                top: 0,
                left: 0,
                kind,
                color: Rgb::new(255, 0, 0),
                size: 3,
                offset: [0, 0],
                seq: 0,
            };
            let mut pass = IconPass::new(&device, TextureFormat::Rgba8Unorm, METRICS);
            pass.prepare(
                &device,
                &queue,
                &[icon],
                &[],
                [TARGET as f32, TARGET as f32],
                BAND,
                RowAnchor::UNIFORM,
            );
            let rgba = render_rgba(&device, &queue, |render_pass| pass.draw(render_pass));
            rgba[((24 * TARGET + 24) * 4) as usize]
        };

        assert_eq!(
            [IconKind::Error, IconKind::Warning, IconKind::Info].map(middle),
            [255, 0, 0],
            "the error disc is whole, and the bang and the i clear the other middles",
        );
    }

    /// A box over the whole target, declared before the icon, so only the
    /// composite's occlude-all reaches it.
    fn covering_panel() -> Panel {
        Panel {
            top: 0,
            left: 0,
            width: 4,
            height: 4,
            style: BorderStyle::None_,
            border: Rgb::new(0, 0, 0),
            corner_radius: 0,
            fill: None,
            shadow: PanelShadow::None_,
            inset_x: 0,
            inset_y: 0,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        }
    }

    /// The leftmost column and topmost row of red that `record` paints over a
    /// black [`TARGET`]-square target, or `None` when it paints none.
    fn red_corner(
        device: &Device,
        queue: &Queue,
        record: impl FnOnce(&mut RenderPass<'_>),
    ) -> Option<(u32, u32)> {
        let rgba = render_rgba(device, queue, record);
        let red = |col: u32, row: u32| rgba[((row * TARGET + col) * 4) as usize] > 0;
        let col = (0..TARGET).find(|&col| (0..TARGET).any(|row| red(col, row)))?;
        let row = (0..TARGET).find(|&row| (0..TARGET).any(|col| red(col, row)))?;
        Some((col, row))
    }

    /// What `record` painted over black, read back as rgba texels.
    fn render_rgba(
        device: &Device,
        queue: &Queue,
        record: impl FnOnce(&mut RenderPass<'_>),
    ) -> Vec<u8> {
        let size = Extent3d {
            width: TARGET,
            height: TARGET,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&TextureDescriptor {
            label: Some("icon glide target"),
            size,
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format: TextureFormat::Rgba8Unorm,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&TextureViewDescriptor::default());

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("icon glide"),
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
            record(&mut render_pass);
        }
        queue.submit(Some(encoder.finish()));

        read_back(device, queue, &target, TARGET, TARGET)
    }
}
