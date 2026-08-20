//! Instanced color-bar pass.
//!
//! Fills each [`Bar`] as a solid sub-cell rectangle off the cell grid, above the
//! grid with its own z-order. Bars are not cell attributes: like the overlays
//! and icons they float over the grid, so a gutter can pack thin status bars and
//! a hairline separator into a fraction of a cell. The rectangle rides in
//! cell-fraction units and the vertex shader scales it by the live cell size, so
//! bars track font zoom.

use crate::render::{
    globals_offset, CellMetrics, CompositeSlot, CompositeSlots, Occluder, OccluderBuffer,
    PoolOccluders, GLOBALS_SLOTS, GLOBALS_SLOT_STRIDE,
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
/// Padded to 48 bytes to match the WGSL uniform layout.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    panel_count: u32,
    occlude_all: u32,
    /// Rows every bar is shifted down by, in cell fractions, which the vertex
    /// stage adds to each origin.
    ///
    /// Carried here rather than baked into the instances so a gliding pool can
    /// reuse the instances it built when its content last changed. A glide moves
    /// every bar by the same amount, so nothing per-instance has to change.
    shift_rows: f32,
    _pad0: u32,
    /// Cell the grid's own (0, 0) is drawn at, which the vertex stage adds to
    /// each origin.
    ///
    /// A pool composite hands over bars positioned within its region rather
    /// than the viewport, so the region's origin is what puts them on the
    /// screen. Zero for the live grid.
    origin_cells: [f32; 2],
    /// Rounds the struct to the 48 bytes a uniform's 16-byte alignment wants.
    _pad1: [u32; 2],
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
    /// Rebuilt only on a frame whose content changed, the eased shift riding the
    /// globals instead, so a glide leaves this alone. Holding the buffer is what
    /// keeps a rebuild that does happen from allocating.
    composite_built: Vec<BarInstance>,
    count: u32,
    /// Per-pool bars of the pools composited over the live grid, one slot per
    /// pool so every pool can be prepared before any of them draws. Separate from
    /// [`Self::instances`] so a pool draw leaves the live bars intact.
    composite_slots: CompositeSlots<CompositeSlot>,
    /// One occluder per live panel, read by the fragment shader to discard bar
    /// fragments a later box covers. Bound alongside the globals, and rebuilt
    /// into a new bind group whenever it reallocates.
    occluders: OccluderBuffer,
    /// The occluders the composited pools read, bound by
    /// [`Self::composite_bind_group`].
    ///
    /// A pool occludes against a different set of panels than the live grid
    /// does, and a frame prepares every pass before any of them draws, so one
    /// buffer would hold whichever list was written last and hand it to every
    /// draw. The dedup makes that worse rather than better: two lists that
    /// differ never recognize each other's bytes, so each frame uploads twice.
    composite_occluders: OccluderBuffer,
    /// Bound by the composite draws, over the same globals as
    /// [`Self::bind_group`] and [`Self::composite_occluders`].
    composite_bind_group: BindGroup,
    /// The uniform last written, so an unchanged frame skips that write too.
    last_globals: Option<Globals>,
    metrics: CellMetrics,
}

impl BarPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> BarPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("bar"),
            source: ShaderSource::Wgsl(
                crate::render::with_occlusion(include_str!("../shaders/bar.wgsl")).into(),
            ),
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

        let occluders = OccluderBuffer::new(device, "bar occluders", INITIAL_CAPACITY);
        let composite_occluders =
            OccluderBuffer::new(device, "bar composite occluders", INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &occluders.buffer);
        let composite_bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &globals,
            &composite_occluders.buffer,
        );

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
            composite_occluders,
            composite_bind_group,
            last_globals: None,
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
            // The live grid does not glide, so its bars sit where they are given.
            shift_rows: 0.0,
            _pad0: 0,
            origin_cells: [0.0; 2],
            _pad1: [0; 2],
        };
        crate::render::upload_globals(queue, &self.globals, 0, globals, &mut self.last_globals);

        build_bar_instances_into(bars, &mut self.built);
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

    /// Upload one instance per bar of a pool grid being composited.
    ///
    /// The pool's eased `shift_rows` goes to the globals, which the vertex stage
    /// adds to every origin, so a frame that only glides writes the uniform and
    /// keeps the instances `content_changed` last rebuilt.
    ///
    /// Writes `slot`'s buffer, separate from the live [`Self::prepare`] and from
    /// the other pools', reusing the shared globals uniform the live pass already
    /// wrote this frame. Reallocates only when the bar count outgrows that slot's
    /// capacity, and allocates the slot itself on first use.
    ///
    /// The bars are occluded against `occluders` with the seq test bypassed, so a
    /// gutter bar gliding beneath a modal is hidden by it. `occluders` carries
    /// the frame's whole list and how much of it covers this pool, and all four
    /// of a pool's composite passes are handed the same one.
    ///
    /// See also:
    /// - [`PoolOccluders`] for why every pool of a frame reads one list.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        bars: &[Bar],
        occluders: PoolOccluders<'_>,
        resolution: [f32; 2],
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
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count,
            occlude_all,
            shift_rows,
            _pad0: 0,
            origin_cells,
            _pad1: [0; 2],
        };
        queue.write_buffer(
            &self.globals,
            u64::from(globals_offset(slot)),
            bytemuck::bytes_of(&globals),
        );

        // A glide moves every bar by the same amount, which the globals write
        // above has just re-applied, so unchanged content reuses last frame's
        // instances rather than rebuilding and re-uploading all of them.
        if !content_changed {
            return;
        }

        build_bar_instances_into(bars, &mut self.composite_built);

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
        label: Some("bar instances"),
        size: (capacity * size_of::<BarInstance>()) as u64,
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
/// Origins are given as declared. A pool composite glides its bars through the
/// `shift_rows` uniform rather than through their origins, so instances built
/// here outlive every frame that only moves the pool.
fn build_bar_instances_into(bars: &[Bar], out: &mut Vec<BarInstance>) {
    out.clear();
    out.extend(bars.iter().map(|bar| BarInstance {
        origin: [f32::from(bar.x) / SIXTEENTHS, f32::from(bar.y) / SIXTEENTHS],
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
    use super::{build_bar_instances_into, BarInstance, BarPass};
    use crate::{
        gpu::headless_device,
        render::{background::BackgroundPass, CellMetrics, PoolOccluders},
    };
    use stoatty_term::grid::{Bar, Grid, Rgb};
    use wgpu::{
        naga::{
            front::wgsl,
            valid::{Capabilities, ValidationFlags, Validator},
        },
        BufferDescriptor, BufferUsages, Color, CommandEncoderDescriptor, Device, Extent3d, LoadOp,
        MapMode, Operations, Origin3d, PollType, Queue, RenderPass, RenderPassColorAttachment,
        RenderPassDescriptor, StoreOp, TexelCopyBufferInfo, TexelCopyBufferLayout,
        TexelCopyTextureInfo, TextureAspect, TextureDescriptor, TextureDimension, TextureFormat,
        TextureUsages, TextureViewDescriptor,
    };

    /// The square readback target's edge, in pixels. Four bytes a texel makes a
    /// row exactly the 256-byte copy alignment, so the readback needs no stride
    /// padding.
    const TARGET: u32 = 64;

    /// Draw whatever `record` records onto a black [`TARGET`]-square target, and
    /// report the topmost row painting any red.
    ///
    /// Both fixtures paint pure red over black, so that row is the top edge the
    /// pass resolved.
    fn first_red_row(
        device: &Device,
        queue: &Queue,
        record: impl FnOnce(&mut RenderPass<'_>),
    ) -> Option<u32> {
        let size = Extent3d {
            width: TARGET,
            height: TARGET,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&TextureDescriptor {
            label: Some("bar glide target"),
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
            label: Some("bar glide readback"),
            size: u64::from(TARGET) * u64::from(TARGET) * 4,
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("bar glide"),
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

        (0..TARGET).find(|row| (0..TARGET).any(|col| rgba[((row * TARGET + col) * 4) as usize] > 0))
    }

    /// A gliding pool moves its cells and its bars by the same shift, so a bar
    /// that rounds the shifted origin crosses pixel boundaries at a different
    /// phase than the row it annotates and wobbles a pixel against it.
    #[test]
    fn a_gliding_bar_lands_on_the_row_it_annotates() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("bar glide test: no wgpu adapter, skipping");
            return;
        };

        // The cell height must be fractional, or rounding is the identity and
        // either ordering agrees. Row 1 sits at 12.5 and the shift is a further
        // 1.25 pixels, which is enough that snapping before and after the shift
        // land the top edge on different pixel rows.
        //
        // Which row is not asserted. The two passes rounding alike is the
        // property, and a pinned row pins the shader language's rounding mode
        // along with it.
        let metrics = CellMetrics {
            font_size: 10.0,
            width: 12.0,
            height: 12.5,
            scale_factor: 1.0,
        };
        let shift_rows = 1.25 / 12.5;
        let resolution = [TARGET as f32, TARGET as f32];
        let red = Rgb::new(255, 0, 0);

        let mut grid = Grid::new(4, 4);
        grid.get_mut(1, 0).bg = red;
        let mut cells = BackgroundPass::new(&device, TextureFormat::Rgba8Unorm, metrics);
        cells.prepare_composite(
            &device,
            &queue,
            &grid,
            PoolOccluders::new(&[], 0, true),
            resolution,
            shift_rows,
            [0.0; 2],
            true,
            0,
            0,
        );
        let cell_row = first_red_row(&device, &queue, |pass| cells.draw_composite(pass, 0, 0));

        let bar = Bar {
            x: 0,
            y: 16,
            width: 16,
            height: 16,
            color: red,
            seq: 0,
        };
        let mut bars = BarPass::new(&device, TextureFormat::Rgba8Unorm, metrics);
        bars.prepare_composite(
            &device,
            &queue,
            &[bar],
            PoolOccluders::new(&[], 0, true),
            resolution,
            shift_rows,
            [0.0; 2],
            true,
            0,
            0,
        );
        let bar_row = first_red_row(&device, &queue, |pass| bars.draw_composite(pass, 0, 0));

        assert!(cell_row.is_some(), "the shifted cell row paints something");
        assert_eq!(
            bar_row, cell_row,
            "and the bar on that row starts from the same pixel"
        );
    }

    /// The vertex stage's shift, mirrored here so a test can check the origins it
    /// builds still land where the baked-in shift used to put them.
    fn shader_origin(origin: [f32; 2], shift_rows: f32) -> [f32; 2] {
        [origin[0], origin[1] + shift_rows]
    }

    /// [`build_bar_instances_into`] into a fresh buffer, for the assertions that only
    /// want one frame's instances and have no buffer to reuse.
    fn build_bar_instances(bars: &[Bar]) -> Vec<BarInstance> {
        let mut instances = Vec::new();
        build_bar_instances_into(bars, &mut instances);
        instances
    }

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_occlusion(include_str!(
            "../shaders/bar.wgsl"
        )))
        .expect("parse bar");
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

        let mut scratch = build_bar_instances(&bars);
        scratch.extend(build_bar_instances(&bars));
        build_bar_instances_into(&bars, &mut scratch);

        assert_eq!(
            bytemuck::cast_slice::<BarInstance, u8>(&scratch),
            bytemuck::cast_slice::<BarInstance, u8>(&build_bar_instances(&bars)),
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
        let first = build_bar_instances(&bars);
        assert!(
            !upload_needed(&build_bar_instances(&bars), &first),
            "an unchanged bar rebuilds to the same bytes, so no upload is needed",
        );

        let moved = [Bar { x: 9, ..bars[0] }];
        assert!(
            upload_needed(&build_bar_instances(&moved), &first),
            "a moved bar must reach the GPU",
        );
        assert!(
            upload_needed(&build_bar_instances(&[]), &first),
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

        let instances = build_bar_instances(&bars);

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

        let instances = build_bar_instances(&bars);

        assert_eq!(
            instances[0].origin,
            [0.0, 1.0],
            "the built origin is where the bar was declared, shift or no shift",
        );
        assert_eq!(
            shader_origin(instances[0].origin, -0.5),
            [0.0, 0.5],
            "row 1 shifted up half a cell lands at 0.5 once the shader applies it",
        );
    }

    /// A pool composite glides its bars by writing `shift_rows` into the globals.
    /// What a shift-only frame must leave alone is the instances the last content
    /// change built, which is the one part of that a test can observe.
    #[test]
    fn a_shift_only_composite_frame_rebuilds_no_bars() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("bar composite test: no wgpu adapter, skipping");
            return;
        };
        let mut pass = BarPass::new(
            &device,
            TextureFormat::Rgba8Unorm,
            CellMetrics {
                font_size: 10.0,
                width: 6.0,
                height: 12.0,
                scale_factor: 1.0,
            },
        );
        let bar = |y: i16| Bar {
            x: 0,
            y,
            width: 2,
            height: 16,
            color: Rgb::new(1, 2, 3),
            seq: 0,
        };

        pass.prepare_composite(
            &device,
            &queue,
            &[bar(16)],
            PoolOccluders::new(&[], 0, true),
            [64.0, 64.0],
            0.0,
            [0.0; 2],
            true,
            0,
            0,
        );
        let built = pass.composite_built.clone();

        // Different bars on purpose. A rebuild that ran anyway would put these in
        // the buffer, so matching proves the gate and not just the arithmetic.
        pass.prepare_composite(
            &device,
            &queue,
            &[bar(32), bar(48)],
            PoolOccluders::new(&[], 0, true),
            [64.0, 64.0],
            -0.5,
            [0.0; 2],
            false,
            0,
            0,
        );

        assert_eq!(
            bytemuck::cast_slice::<BarInstance, u8>(&pass.composite_built),
            bytemuck::cast_slice::<BarInstance, u8>(&built),
            "a frame that only glides leaves the instances where the last one left them",
        );
    }
}
