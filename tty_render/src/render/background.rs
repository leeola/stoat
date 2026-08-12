//! Instanced per-cell background fill.
//!
//! Draws one solid colored quad per grid cell, reading each [`Cell`]'s
//! background from [`stoatty_term`]'s [`Grid`]. The vertex shader derives the
//! quad corners from the vertex index and the cell coordinate from the instance
//! index, so the instance stream carries nothing but each cell's packed color.
//! A uniform supplies the screen resolution, cell size, and column count used to
//! map cells to clip space, along with the rotation a scrolled frame reads the
//! rows through.
//!
//! [`Cell`]: stoatty_term::grid::Cell

use crate::render::{
    globals_offset, occlusion_globals, CellMetrics, CompositeSlot, CompositeSlots, Occluder,
    GLOBALS_SLOTS, GLOBALS_SLOT_STRIDE,
};
use bytemuck::{Pod, Zeroable};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::Damage,
};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBinding, BufferBindingType, BufferDescriptor, BufferSize, BufferUsages,
    ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor, Queue,
    RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModule, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in cells, allocated up front. Grows by doubling
/// when a grid exceeds it; 2048 covers a default 24x80 grid without reallocating.
const INITIAL_CAPACITY: usize = 2048;

/// Cursor block blend alpha. The cursor's RGB is the theme's cursor color; this
/// translucency is renderer policy so the block tints the cell beneath it.
const CURSOR_ALPHA: f32 = 0.55;

/// Byte offset of the cursor pipeline's globals, one slot past the pool slots.
///
/// The cursor needs a slot of its own because a frame compositing pools draws it
/// after them, over the cell they cover, and so rewrites its corners after the
/// live cell globals are already in slot 0. Sharing that slot would leave the
/// cell draws reading the cursor's write, whose column count is zero.
const CURSOR_GLOBALS_OFFSET: u32 = (GLOBALS_SLOTS as u64 * GLOBALS_SLOT_STRIDE) as u32;

/// Slots this pass's globals buffer holds: the shared per-pool set plus the
/// cursor's.
const BG_GLOBALS_SLOTS: usize = GLOBALS_SLOTS + 1;

/// One grid cell's background color, as the bytes the GPU normalizes.
///
/// Carries no grid coordinate. The buffer is row-major over the grid and both
/// draws bind it from instance zero, so the shader recovers the coordinate by
/// dividing the instance index by the column count. A coordinate is the one
/// thing in the stream the GPU can derive for free.
///
/// Deriving it is also what lets a scroll rotate the rows rather than rewrite
/// them, since an instance says nothing about where it sits.
///
/// Alpha is always 255, since the cell fill is opaque. The field exists because
/// a 3-byte vertex format is not one the GPU offers.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BgInstance {
    color: [u8; 4],
}

/// Uniform shared by the cell and cursor pipelines.
///
/// Carries the screen resolution and cell size that map cell coordinates to
/// clip space, the cursor block's four eased corners (two `vec4`s holding
/// [TL, TR] then [BL, BR] in fractional cell coordinates), the cursor color,
/// and the grid's eased vertical scroll offset in pixels.
///
/// `scroll_y`, `panel_count`, `occlude_all`, and `cols` fill one 16-byte slot,
/// and the rotation pair with its padding fills another, so `cursor_color` lands
/// on the 16-byte offset the uniform layout requires. The `vec4` corner pairs
/// already sit on 16-byte boundaries.
///
/// Two pipelines share this uniform, so each write site zeroes the fields its own
/// pipeline does not read. `panel_count` and `occlude_all` are non-zero only on an
/// occludable pool composite, so the live cell fill and the cursor draw skip the
/// occluder loop. `cols` is read only by the cell fill, which divides the instance
/// index by it to recover the cell coordinate, so the cursor writes zero.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    cursor_corners_01: [f32; 4],
    cursor_corners_23: [f32; 4],
    scroll_y: f32,
    panel_count: u32,
    occlude_all: u32,
    cols: u32,
    /// Rows the instance buffer is rotated by, and the grid height that rotation
    /// wraps at. Display row `r` lives at slot `(r + row_offset) % rows`.
    row_offset: u32,
    rows: u32,
    /// Pads the pair above to a whole slot, keeping `cursor_color` 16-byte
    /// aligned the way the shader declares it.
    pad: [u32; 2],
    cursor_color: [f32; 4],
}

/// The cursor block's eased corners and color for the frame.
#[derive(Clone, Copy)]
pub struct CursorState {
    /// The block's four corners [TL, TR, BL, BR] in fractional cell
    /// coordinates, or `None` when the cursor is hidden.
    pub corners: Option<[[f32; 2]; 4]>,
    /// Block color. The pass applies its own blend alpha.
    pub color: Rgb,
}

/// The instanced background-fill pipeline and its per-frame buffers, plus a
/// single-quad cursor pipeline sharing the same globals uniform.
pub struct BackgroundPass {
    pipeline: RenderPipeline,
    globals: Buffer,
    bind_group: BindGroup,
    /// The group-0 layout the globals bind group uses, kept so the bind group
    /// can be rebuilt when [`Self::occluders`] reallocates.
    bind_group_layout: BindGroupLayout,
    instances: Buffer,
    capacity: usize,
    count: u32,
    /// Per-pool cell instances of the pools composited over the live grid, one
    /// slot per pool so a pool reusing last frame's instances cannot read a
    /// sibling's. Separate from [`Self::instances`] so a pool draw leaves the live
    /// grid's damage-tracked instances intact.
    composite_slots: CompositeSlots<CompositeSlot>,
    /// One occluder per live panel at binding 1, read by the cell fragment
    /// shader on an occludable pool composite to discard a page cell a box
    /// covers. Unused by the live cell fill and the cursor, which leave the
    /// panel count at zero.
    occluders: Buffer,
    /// The occluder list last written to [`Self::occluders`], so a frame whose
    /// panels have not moved skips the upload. Panels change on layout events, not
    /// per frame, so most frames match.
    last_occluders: Vec<Occluder>,
    occluder_capacity: usize,
    cursor_pipeline: RenderPipeline,
    cursor_visible: bool,
    /// The value last written to the cell slot, so an unchanged frame skips that
    /// write.
    last_globals: Option<Globals>,
    /// The value last written to the cursor slot, tracked apart from
    /// [`Self::last_globals`] because the two slots hold different values. A live
    /// frame seeds this slot with the cell globals, while a pool frame overwrites it
    /// through [`Self::prepare_cursor`] with the column count zeroed.
    last_cursor_globals: Option<Globals>,
    metrics: CellMetrics,
    /// Scratch reused each frame to build the cell instances for upload, so a
    /// full rebuild, a damaged row, and a composite frame each allocate none.
    scratch: Vec<BgInstance>,
    /// Rows the instance buffer is rotated by, so a scrolled frame moves this
    /// rather than re-uploading every cell.
    ///
    /// Display row `r` lives at slot `(r + row_offset) % rows`. Reset wherever
    /// the buffer is rebuilt whole, so the two cannot drift apart.
    row_offset: u32,
}

impl BackgroundPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(
        device: &Device,
        format: TextureFormat,
        metrics: CellMetrics,
    ) -> BackgroundPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("background"),
            source: ShaderSource::Wgsl(
                crate::render::with_occlusion(include_str!("../shaders/bg.wgsl")).into(),
            ),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("background globals"),
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
            label: Some("background"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("background"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<BgInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![0 => Unorm8x4],
                }],
            },
            fragment: Some(FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: Default::default(),
                targets: &[Some(ColorTargetState {
                    format,
                    blend: Some(BlendState::REPLACE),
                    write_mask: ColorWrites::ALL,
                })],
            }),
            primitive: Default::default(),
            depth_stencil: None,
            multisample: Default::default(),
            multiview_mask: None,
            cache: None,
        });

        let cursor_pipeline = build_cursor_pipeline(device, &shader, &bind_group_layout, format);

        let globals = device.create_buffer(&BufferDescriptor {
            label: Some("background globals"),
            size: BG_GLOBALS_SLOTS as u64 * GLOBALS_SLOT_STRIDE,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let occluders = alloc_occluders(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &occluders);

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        BackgroundPass {
            pipeline,
            globals,
            bind_group,
            bind_group_layout,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            composite_slots: CompositeSlots::new(),
            occluders,
            last_occluders: Vec::new(),
            occluder_capacity: INITIAL_CAPACITY,
            cursor_pipeline,
            cursor_visible: false,
            last_globals: None,
            last_cursor_globals: None,
            metrics,
            scratch: Vec::new(),
            row_offset: 0,
        }
    }

    /// Replace the cell metrics so the next frame lays out cells at the new size.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    /// Upload the panel occluders, reallocating the buffer and rebuilding the
    /// globals bind group when the panel count outgrows the current capacity.
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

    /// Upload the frame's uniform and per-cell instances for `grid`.
    ///
    /// `resolution` is the surface size in physical pixels. `cursor` carries the
    /// cursor block's eased corners and color. `grid_scroll` shifts the whole
    /// grid up by that many rows.
    ///
    /// Reallocates the instance buffer only when the grid outgrows the current
    /// capacity. With partial `damage`, only the damaged rows' cells are rewritten.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        resolution: [f32; 2],
        cursor: CursorState,
        grid_scroll: f32,
        damage: &Damage,
        scrolled_rows: isize,
    ) {
        let cols = grid.cols();
        let rows = grid.rows();
        let total = rows * cols;

        // A resize changes the cell count and a grow reallocates (dropping the
        // buffer's contents), so both rebuild every cell; otherwise rewrite only
        // the damaged rows. Each cell is one instance, so a row is a fixed slice
        // of `cols` and can be patched in place.
        //
        // A scroll is not among the reasons to rebuild. The instance carries
        // only a colour and the shader derives the cell from the instance index,
        // so the rows are rotated under it instead. The scroll advances
        // `row_offset` and only the rows it exposed are written.
        let full =
            matches!(damage, Damage::Full) || total != self.count as usize || total > self.capacity;

        // Settled before the globals below, which carry the offset to the
        // shader. Deciding it after them would leave the uniform describing a
        // rotation the buffer no longer has, for one frame.
        if full {
            self.row_offset = 0;
        } else if scrolled_rows != 0 && rows != 0 {
            let advance = scrolled_rows.rem_euclid(rows as isize) as u32;
            self.row_offset = (self.row_offset + advance) % rows as u32;
        }

        let c = cursor.corners.unwrap_or([[0.0; 2]; 4]);
        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            cursor_corners_01: [c[0][0], c[0][1], c[1][0], c[1][1]],
            cursor_corners_23: [c[2][0], c[2][1], c[3][0], c[3][1]],
            scroll_y: grid_scroll * self.metrics.height,
            panel_count: 0,
            occlude_all: 0,
            cols: cols as u32,
            row_offset: self.row_offset,
            rows: grid.rows() as u32,
            pad: [0; 2],
            cursor_color: [
                cursor.color.r as f32 / 255.0,
                cursor.color.g as f32 / 255.0,
                cursor.color.b as f32 / 255.0,
                CURSOR_ALPHA,
            ],
        };
        crate::render::upload_globals(queue, &self.globals, 0, globals, &mut self.last_globals);
        // The cursor reads its own slot, so a live frame seeds it here with the same
        // globals. A pool frame overwrites it via [`Self::prepare_cursor`] after the
        // pools have their slots, leaving slot 0's column count intact.
        crate::render::upload_globals(
            queue,
            &self.globals,
            u64::from(CURSOR_GLOBALS_OFFSET),
            globals,
            &mut self.last_cursor_globals,
        );
        self.cursor_visible = cursor.corners.is_some();

        if full {
            self.scratch.clear();
            build_instances(grid, &mut self.scratch);
            self.count = self.scratch.len() as u32;
            if self.scratch.is_empty() {
                return;
            }
            if self.scratch.len() > self.capacity {
                self.capacity = self.scratch.len().next_power_of_two();
                self.instances = alloc_instances(device, self.capacity);
            }
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&self.scratch));
            return;
        }

        // The rows a scroll kept already sit where the advanced offset points,
        // which is the whole of what it costs. The ones it uncovered have no
        // content behind them, and damage names those.
        let mut row = 0;
        while row < rows {
            if !damage.is_dirty(row) {
                row += 1;
                continue;
            }

            // Written one row at a time rather than in runs. A run of rows is
            // contiguous on screen but wraps in the buffer, so the slice it
            // would write is not one range once the rotation is past the end.
            self.scratch.clear();
            build_row_instances(grid, row, &mut self.scratch);
            let slot = row_slot(row, self.row_offset, rows);
            let offset = (slot * cols * size_of::<BgInstance>()) as u64;
            queue.write_buffer(&self.instances, offset, bytemuck::cast_slice(&self.scratch));
            row += 1;
        }
    }

    /// Upload the uniform and per-cell instances for a pool grid being
    /// composited over the live grid, into buffers separate from the live ones.
    ///
    /// A pool composite paints a pooled page over the live grid mid-glide.
    /// Building its cells into [`Self::instances`] would erase the live grid's
    /// damage-tracked instances, so the pool builds into its own slot that
    /// [`Self::draw_composite`] reads, leaving the live buffer intact for the
    /// next live frame.
    ///
    /// `pool` is the terminal's id for the pool, under which its instances are
    /// kept across frames. `slot` is its position among this frame's pools, naming
    /// the globals slot the matching [`Self::draw_composite`] binds. The two differ
    /// because instances persist and globals do not.
    ///
    /// `grid_scroll` shifts the grid up by that many rows. The pool grid changes
    /// wholesale each frame, so every cell is rebuilt with no per-row damage
    /// path. No cursor draws over a composite, so the shared globals carry none.
    ///
    /// The page cells are occluded against `occluders` with the seq test bypassed,
    /// so a pooled cell gliding beneath a modal is hidden by it. Which panels reach
    /// that list is the caller's decision, since all four of a pool's composite
    /// passes share it.
    ///
    /// See also:
    /// - [`pool_occluders_into`](crate::render::pool_occluders_into) for how a pool's list is
    ///   narrowed.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        occluders: &[Occluder],
        resolution: [f32; 2],
        grid_scroll: f32,
        content_changed: bool,
        pool: u32,
        slot: usize,
    ) {
        self.upload_occluders(device, queue, occluders);
        let (panel_count, occlude_all) = occlusion_globals(occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            cursor_corners_01: [0.0; 4],
            cursor_corners_23: [0.0; 4],
            scroll_y: grid_scroll * self.metrics.height,
            panel_count,
            occlude_all,
            cols: grid.cols() as u32,
            // A pool composite builds its instances fresh into its own buffer, so
            // they are never rotated.
            row_offset: 0,
            rows: grid.rows() as u32,
            pad: [0; 2],
            cursor_color: [0.0; 4],
        };
        queue.write_buffer(
            &self.globals,
            u64::from(globals_offset(slot)),
            bytemuck::bytes_of(&globals),
        );

        // Cell quads carry no atlas UVs, so a sub-cell glide over unchanged rows
        // reuses last frame's instances once the globals write above has
        // re-applied the shift.
        if !content_changed {
            return;
        }

        self.scratch.clear();
        build_instances(grid, &mut self.scratch);

        let target = self.composite_slots.entry(pool, || new_slot(device));
        target.count = self.scratch.len() as u32;
        if self.scratch.is_empty() {
            return;
        }

        if self.scratch.len() > target.capacity {
            target.capacity = self.scratch.len().next_power_of_two();
            target.instances = alloc_instances(device, target.capacity);
        }
        queue.write_buffer(&target.instances, 0, bytemuck::cast_slice(&self.scratch));
    }

    /// Upload the cursor block's corners and scroll offset, leaving the cell
    /// instances a prior [`Self::prepare`] uploaded in place.
    ///
    /// Draws the cursor over content another pass already composited, where the
    /// cell instances must not be rebuilt. `grid_scroll` shifts the cursor up by
    /// that many rows to match the cell passes.
    pub(crate) fn prepare_cursor(
        &mut self,
        queue: &Queue,
        resolution: [f32; 2],
        cursor: CursorState,
        grid_scroll: f32,
    ) {
        let c = cursor.corners.unwrap_or([[0.0; 2]; 4]);
        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            cursor_corners_01: [c[0][0], c[0][1], c[1][0], c[1][1]],
            cursor_corners_23: [c[2][0], c[2][1], c[3][0], c[3][1]],
            scroll_y: grid_scroll * self.metrics.height,
            panel_count: 0,
            occlude_all: 0,
            // Written by a cursor-only draw, which reads none of the three.
            cols: 0,
            row_offset: 0,
            rows: 0,
            pad: [0; 2],
            cursor_color: [
                cursor.color.r as f32 / 255.0,
                cursor.color.g as f32 / 255.0,
                cursor.color.b as f32 / 255.0,
                CURSOR_ALPHA,
            ],
        };
        // The cursor's own slot, so this can run after the cell globals are placed
        // without disturbing them.
        crate::render::upload_globals(
            queue,
            &self.globals,
            u64::from(CURSOR_GLOBALS_OFFSET),
            globals,
            &mut self.last_cursor_globals,
        );
        self.cursor_visible = cursor.corners.is_some();
    }

    /// Record the background draw into `render_pass`.
    ///
    /// A no-op until [`Self::prepare`] has run with a non-empty grid.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[0]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        render_pass.draw(0..6, 0..self.count);
    }

    /// Record a composited pool's background draw into `render_pass`.
    ///
    /// A no-op until [`Self::prepare_composite`] has run for `pool` with a
    /// non-empty grid. Reads that pool's instances, so drawing it leaves both the
    /// live cell instances a prior [`Self::prepare`] uploaded and the other pools'
    /// untouched.
    ///
    /// `slot` must be the one that prepare was given, since it selects the globals
    /// the draw reads.
    pub fn draw_composite(&self, render_pass: &mut RenderPass<'_>, pool: u32, slot: usize) {
        let Some(target) = self.composite_slots.get(pool).filter(|s| s.count > 0) else {
            return;
        };

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[globals_offset(slot)]);
        render_pass.set_vertex_buffer(0, target.instances.slice(..));
        render_pass.draw(0..6, 0..target.count);
    }

    /// Record the cursor-block draw into `render_pass`.
    ///
    /// A no-op when the cursor is hidden. Draw it after the glyph pass so the
    /// translucent block tints the cell and its glyph as it slides.
    pub fn draw_cursor(&self, render_pass: &mut RenderPass<'_>) {
        if !self.cursor_visible {
            return;
        }

        render_pass.set_pipeline(&self.cursor_pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[CURSOR_GLOBALS_OFFSET]);
        render_pass.draw(0..6, 0..1);
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
        label: Some("background instances"),
        size: (capacity * size_of::<BgInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn alloc_occluders(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("background occluders"),
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
        label: Some("background globals"),
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

/// Build the cursor pipeline sharing `globals_layout` with the cell pass.
///
/// It has no vertex buffer. The single quad reads the cursor's four corners
/// from the globals uniform, and alpha blends so the block tints what it covers.
fn build_cursor_pipeline(
    device: &Device,
    shader: &ShaderModule,
    globals_layout: &BindGroupLayout,
    format: TextureFormat,
) -> RenderPipeline {
    let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
        label: Some("cursor"),
        bind_group_layouts: &[Some(globals_layout)],
        immediate_size: 0,
    });

    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some("cursor"),
        layout: Some(&layout),
        vertex: VertexState {
            module: shader,
            entry_point: Some("vs_cursor"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        fragment: Some(FragmentState {
            module: shader,
            entry_point: Some("fs_cursor"),
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
    })
}

/// The instance-buffer slot holding display `row` of a grid `rows` tall.
///
/// The inverse of what the shader computes from a slot, so a row written where
/// this says is the row read back there. A grid of no rows has no slots.
fn row_slot(row: usize, row_offset: u32, rows: usize) -> usize {
    if rows == 0 {
        return 0;
    }
    (row + row_offset as usize) % rows
}

fn build_instances(grid: &Grid, out: &mut Vec<BgInstance>) {
    for row in 0..grid.rows() {
        build_row_instances(grid, row, out);
    }
}

fn build_row_instances(grid: &Grid, row: usize, out: &mut Vec<BgInstance>) {
    out.extend((0..grid.cols()).map(|col| {
        let (_, bg) = grid.get(row, col).draw_colors();
        BgInstance {
            color: [bg.r, bg.g, bg.b, 255],
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::{build_instances, build_row_instances, row_slot, BackgroundPass, BgInstance};
    use crate::{gpu::headless_device, render::CellMetrics};
    use stoatty_term::grid::{Flags, Grid, Rgb};
    use wgpu::{
        naga::{
            front::wgsl,
            valid::{Capabilities, ValidationFlags, Validator},
        },
        TextureFormat,
    };

    /// What `vs_main` computes from a slot, transcribed. A rotation is only
    /// correct if writing through [`row_slot`] and reading through this round
    /// trips, and nothing here runs the shader to find out.
    fn shader_row(slot: usize, row_offset: u32, rows: usize) -> usize {
        let height = rows.max(1);
        (slot + height - row_offset as usize % height) % height
    }

    #[test]
    fn a_row_is_read_back_from_the_slot_it_was_written_to() {
        let rows = 5;
        for row_offset in 0..(2 * rows as u32 + 3) {
            let round_tripped: Vec<usize> = (0..rows)
                .map(|row| shader_row(row_slot(row, row_offset, rows), row_offset, rows))
                .collect();

            assert_eq!(
                round_tripped,
                (0..rows).collect::<Vec<_>>(),
                "at offset {row_offset} every row must land where the shader looks for it"
            );
        }
    }

    #[test]
    fn every_slot_holds_exactly_one_row() {
        let rows = 5;
        for row_offset in 0..(2 * rows as u32 + 3) {
            let mut slots: Vec<usize> = (0..rows)
                .map(|row| row_slot(row, row_offset, rows))
                .collect();
            slots.sort_unstable();

            assert_eq!(
                slots,
                (0..rows).collect::<Vec<_>>(),
                "at offset {row_offset} the rows must cover the buffer without two sharing a slot"
            );
        }
    }

    /// The rows a scroll kept are already where the advanced offset looks for
    /// them, which is what leaves only the exposed ones to write.
    #[test]
    fn a_scroll_leaves_the_rows_it_kept_where_the_shader_will_find_them() {
        let rows = 5;
        let scrolled = 2;
        let before = 3;
        let after = (before + scrolled) % rows as u32;

        for row in 0..rows - scrolled as usize {
            assert_eq!(
                row_slot(row, after, rows),
                row_slot(row + scrolled as usize, before, rows),
                "row {row} after the scroll reads the slot row {} was written to",
                row + scrolled as usize
            );
        }
    }

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_occlusion(include_str!(
            "../shaders/bg.wgsl"
        )))
        .expect("parse bg.wgsl");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate bg.wgsl");
    }

    #[test]
    fn instances_cover_every_cell_with_its_opaque_bg() {
        let mut grid = Grid::new(2, 2);
        grid.get_mut(0, 0).bg = Rgb::new(255, 0, 0);
        grid.get_mut(1, 1).bg = Rgb::new(0, 0, 255);

        let mut instances = Vec::new();
        build_instances(&grid, &mut instances);

        assert_eq!(instances.len(), 4);
        assert_eq!(instances[0].color, [255, 0, 0, 255]);
        assert_eq!(instances[3].color, [0, 0, 255, 255]);
    }

    #[test]
    fn inverse_cell_draws_foreground_as_background() {
        let mut grid = Grid::new(1, 1);
        grid.get_mut(0, 0).fg = Rgb::new(255, 0, 0);
        grid.get_mut(0, 0).bg = Rgb::new(0, 0, 255);
        grid.get_mut(0, 0).flags = Flags::INVERSE;

        let mut instances = Vec::new();
        build_instances(&grid, &mut instances);

        assert_eq!(instances[0].color, [255, 0, 0, 255]);
    }

    /// The instances carry no coordinate, so the shader recovers each cell's from
    /// `instance_index % cols` and `instance_index / cols`. That only holds while
    /// the build stays row-major over the whole grid with no gaps, which nothing
    /// else here would catch if it changed.
    #[test]
    fn instances_are_row_major_over_the_whole_grid() {
        let (rows, cols) = (3, 4);
        let mut grid = Grid::new(rows, cols);
        for row in 0..rows {
            for col in 0..cols {
                grid.get_mut(row, col).bg = Rgb::new(row as u8, col as u8, 0);
            }
        }

        let mut instances = Vec::new();
        build_instances(&grid, &mut instances);

        let coords: Vec<[u8; 2]> = instances
            .iter()
            .map(|inst| [inst.color[0], inst.color[1]])
            .collect();
        let expected: Vec<[u8; 2]> = (0..rows * cols)
            .map(|i| [(i / cols) as u8, (i % cols) as u8])
            .collect();
        assert_eq!(
            coords, expected,
            "instance i holds cell (i / cols, i % cols)"
        );
    }

    /// The instance stream declares one 4-byte `Unorm8x4` attribute where the
    /// shader takes a `vec4<f32>`, an agreement only pipeline creation checks.
    /// Validating the WGSL alone would not catch a stride or format that no longer
    /// matches what `vs_main` reads.
    #[test]
    fn the_pipeline_accepts_the_packed_instance_layout() {
        let Some((device, _queue)) = headless_device() else {
            eprintln!("background pipeline test: no wgpu adapter, skipping");
            return;
        };

        BackgroundPass::new(
            &device,
            TextureFormat::Rgba8Unorm,
            CellMetrics {
                font_size: 10.0,
                width: 6.0,
                height: 12.0,
                scale_factor: 1.0,
            },
        );
    }

    /// A damaged row is patched in place at `row * cols * size_of::<BgInstance>()`,
    /// so the row's instances have to be exactly the slice that offset names.
    #[test]
    fn a_row_patch_covers_exactly_its_row_of_the_buffer() {
        let (rows, cols) = (4, 5);
        let grid = Grid::new(rows, cols);

        let mut instances = Vec::new();
        build_row_instances(&grid, 2, &mut instances);

        let bytes = size_of::<BgInstance>();
        assert_eq!(
            (instances.len() * bytes, 2 * cols * bytes),
            (cols * 4, 40),
            "a row spans 4 bytes per cell, at four times its row-major start",
        );
    }
}
