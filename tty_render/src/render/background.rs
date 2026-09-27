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
    exposed_rows, globals_offset, CellMetrics, CompositeSlot, CompositeSlots, Cover, Occluder,
    OccluderBuffer, PoolOccluders, GLOBALS_SLOTS, GLOBALS_SLOT_STRIDE,
};
use bytemuck::{Pod, Zeroable};
use std::{
    iter,
    ops::{Range, RangeInclusive},
};
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

/// One grid cell's background color, as the bytes the vertex stage unpacks.
///
/// Carries no grid coordinate. The buffer is row-major over the grid and both
/// draws bind it from instance zero, so the shader recovers the coordinate by
/// dividing the instance index by the column count. A coordinate is the one
/// thing in the stream the GPU derives for free.
///
/// Deriving it is also what lets a scroll rotate the rows rather than rewrite
/// them, since an instance says nothing about where it sits.
///
/// The vertex stage reads the four bytes as one `u32`, so it compares a cell
/// against [`Globals::skip_color`] exactly before it unpacks the color.
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
/// already sit on 16-byte boundaries. `skip_color` and its padding fill one more
/// slot, which puts [`Cover`] on the 16-byte boundary its rect array requires.
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
    /// Cell the grid's own (0, 0) is drawn at, which the vertex stage adds to
    /// every cell coordinate.
    ///
    /// A pool composite hands over a grid sized to its region rather than to
    /// the viewport, so the region's origin is what puts its cells on the
    /// screen. Carried here rather than baked into the instances, because every
    /// cell shares it and the instances outlive a frame that only glides. Zero
    /// for the live grid, which starts at the screen's own origin.
    origin_cells: [f32; 2],
    cursor_color: [f32; 4],
    /// The packed cell color whose quad draws nothing, being the color the
    /// frame cleared to.
    ///
    /// Such a cell repaints what the clear already painted, and on a screen of
    /// blank cells that is most of the frame's fill. Zero culls no cell, since
    /// every instance carries alpha 255. A pool composite writes zero, because
    /// it covers the live grid and must paint its default cells.
    skip_color: u32,
    _pad: [u32; 3],
    /// The pool regions the live cell fill skips. A pool composite writes
    /// [`Cover::NONE`], since its own cells are what covers the live grid.
    cover: Cover,
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
    /// Binds [`Self::globals`] with [`Self::live_occluders`], for the live cell
    /// fill and the cursor.
    live_bind_group: BindGroup,
    /// Binds [`Self::globals`] with [`Self::composite_occluders`], for the pool
    /// composites.
    composite_bind_group: BindGroup,
    /// The group-0 layout both bind groups use, kept to rebuild a bind group
    /// when its occluder buffer reallocates.
    bind_group_layout: BindGroupLayout,
    /// The frame's live occluder list, which the live cell fill's vertex stage
    /// reads so that a cell under a panel inside a pool region keeps drawing.
    ///
    /// A buffer apart from [`Self::composite_occluders`], which the pool
    /// composites fill later in the same frame. A frame prepares every pass
    /// before any draw, so with one shared buffer the live draw reads the pools'
    /// list.
    live_occluders: OccluderBuffer,
    /// The occluders the pool composites read. The cell fragment shader
    /// discards a page cell a box covers on an occludable pool composite.
    composite_occluders: OccluderBuffer,
    instances: Buffer,
    capacity: usize,
    count: u32,
    /// Per-pool cell instances of the pools composited over the live grid, one
    /// slot per pool so a pool reusing last frame's instances cannot read a
    /// sibling's. Separate from [`Self::instances`] so a pool draw leaves the live
    /// grid's damage-tracked instances intact.
    composite_slots: CompositeSlots<BackgroundSlot>,
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
                crate::render::with_occlusion(&crate::render::with_cover(include_str!(
                    "../shaders/bg.wgsl"
                )))
                .into(),
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
                    // The composite's fragment stage discards under a box, and
                    // the live fill's vertex stage keeps a cell under one.
                    visibility: ShaderStages::VERTEX_FRAGMENT,
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
                    attributes: &vertex_attr_array![0 => Uint32],
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

        let live_occluders =
            OccluderBuffer::new(device, "background live occluders", INITIAL_CAPACITY);
        let live_bind_group =
            make_bind_group(device, &bind_group_layout, &globals, &live_occluders.buffer);
        let composite_occluders =
            OccluderBuffer::new(device, "background composite occluders", INITIAL_CAPACITY);
        let composite_bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &globals,
            &composite_occluders.buffer,
        );

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        BackgroundPass {
            pipeline,
            globals,
            live_bind_group,
            composite_bind_group,
            bind_group_layout,
            live_occluders,
            composite_occluders,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            composite_slots: CompositeSlots::new(),
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

    /// Upload the frame's uniform and per-cell instances for `grid`.
    ///
    /// `resolution` is the surface size in physical pixels. `cursor` carries the
    /// cursor block's eased corners and color. `clear` is the color the frame
    /// cleared to, and a cell of that color draws no quad, since the clear has
    /// already painted it. `grid_scroll` shifts the whole grid up by that many
    /// rows.
    ///
    /// `covered` holds the scissors of the pools composited over this frame, as
    /// `[x, y, width, height]` in pixels. A cell inside one draws no quad, since
    /// the pool repaints it, unless it meets one of `occluders`, the frame's live
    /// list.
    ///
    /// Reallocates the instance buffer only when the grid outgrows the current
    /// capacity. With partial `damage`, only the damaged rows' cells are rewritten.
    /// Adjacent damaged rows share a write.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        resolution: [f32; 2],
        cursor: CursorState,
        clear: Rgb,
        grid_scroll: f32,
        damage: &Damage,
        scrolled_rows: isize,
        occluders: &[Occluder],
        covered: &[[u32; 4]],
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
            origin_cells: [0.0; 2],
            cursor_color: [
                cursor.color.r as f32 / 255.0,
                cursor.color.g as f32 / 255.0,
                cursor.color.b as f32 / 255.0,
                CURSOR_ALPHA,
            ],
            skip_color: packed_color(clear),
            _pad: [0; 3],
            cover: Cover::new(covered, occluders.len()),
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

        if self.live_occluders.upload(device, queue, occluders) {
            self.live_bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals,
                &self.live_occluders.buffer,
            );
        }

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
        let Some(last_col) = cols.checked_sub(1) else {
            return;
        };
        for (slot, run) in damaged_row_runs(|row| damage.is_dirty(row), rows, self.row_offset) {
            // Each write stages its bytes in a buffer of its own, which costs
            // more than a whole row of instances, so a run of rows goes up in
            // one write. A lone row is bounded by its damaged columns instead.
            // The instance is fixed size, so the column's byte offset into the
            // row's slice is exact, and a cell blinking in place costs one
            // instance rather than the row holding it.
            self.scratch.clear();
            let first = if run.len() == 1 {
                let (left, right) = damage.columns(run.start, cols).unwrap_or((0, last_col));
                build_row_instances(grid, run.start, left..=right, &mut self.scratch);
                slot * cols + left
            } else {
                for row in run {
                    build_row_instances(grid, row, 0..=last_col, &mut self.scratch);
                }
                slot * cols
            };
            let offset = (first * size_of::<BgInstance>()) as u64;
            queue.write_buffer(&self.instances, offset, bytemuck::cast_slice(&self.scratch));
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
    /// `grid_scroll` shifts the grid up by that many rows. No cursor draws over a
    /// composite, so the shared globals carry none.
    ///
    /// `scrolled_rows` is how far the pool's content moved since the frame that
    /// last composited it. When the pool's slot holds a grid of the same shape,
    /// the move rotates the slot's instances and only the rows it exposed are
    /// written, as the live pass does for a scroll. `None`, or a grid of another
    /// shape, rebuilds every cell.
    ///
    /// The page cells are occluded against `occluders` with the seq test bypassed,
    /// so a pooled cell gliding beneath a modal is hidden by it. `occluders`
    /// carries the frame's whole list and how much of it covers this pool, and
    /// all four of a pool's composite passes are handed the same one.
    ///
    /// See also:
    /// - [`PoolOccluders`] for why every pool of a frame reads one list.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        occluders: PoolOccluders<'_>,
        resolution: [f32; 2],
        grid_scroll: f32,
        origin_cells: [f32; 2],
        content_changed: bool,
        scrolled_rows: Option<isize>,
        pool: u32,
        slot: usize,
    ) {
        if self
            .composite_occluders
            .upload(device, queue, occluders.all)
        {
            self.composite_bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals,
                &self.composite_occluders.buffer,
            );
        }
        let (panel_count, occlude_all) = occluders.globals();

        // The pool scrolls on its own clock, so its slot carries its own
        // rotation. It is settled here, ahead of the globals write below,
        // because the globals are what tell the shader which display row a slot
        // holds.
        let (rows, cols) = (grid.rows(), grid.cols());
        let held = self.composite_slots.get(pool);
        let held_offset = held.map_or(0, |held| held.row_offset);
        let rotate_by = scrolled_rows.filter(|&by| by != 0).filter(|_| {
            rows > 0 && held.is_some_and(|held| (held.rows, held.cols) == (rows, cols))
        });
        let row_offset = match (content_changed, rotate_by) {
            (false, _) => held_offset,
            (true, Some(by)) => (held_offset + by.rem_euclid(rows as isize) as u32) % rows as u32,
            (true, None) => 0,
        };

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            cursor_corners_01: [0.0; 4],
            cursor_corners_23: [0.0; 4],
            scroll_y: grid_scroll * self.metrics.height,
            panel_count,
            occlude_all,
            cols: cols as u32,
            row_offset,
            rows: rows as u32,
            origin_cells,
            cursor_color: [0.0; 4],
            skip_color: 0,
            _pad: [0; 3],
            cover: Cover::NONE,
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

        let target = self.composite_slots.entry(pool, || new_slot(device));
        target.row_offset = row_offset;
        if let Some(by) = rotate_by {
            // The rows the scroll kept already sit where the advanced offset
            // points, so only the rows it exposed are written.
            let Some(last_col) = cols.checked_sub(1) else {
                return;
            };
            let exposed = exposed_rows(Some(by), rows);
            for (at, run) in damaged_row_runs(|row| exposed.contains(&row), rows, row_offset) {
                self.scratch.clear();
                for row in run {
                    build_row_instances(grid, row, 0..=last_col, &mut self.scratch);
                }
                let offset = (at * cols * size_of::<BgInstance>()) as u64;
                queue.write_buffer(
                    &target.cells.instances,
                    offset,
                    bytemuck::cast_slice(&self.scratch),
                );
            }
            return;
        }

        self.scratch.clear();
        build_instances(grid, &mut self.scratch);
        (target.rows, target.cols) = (rows, cols);
        target.cells.count = self.scratch.len() as u32;
        if self.scratch.is_empty() {
            return;
        }

        if self.scratch.len() > target.cells.capacity {
            target.cells.capacity = self.scratch.len().next_power_of_two();
            target.cells.instances = alloc_instances(device, target.cells.capacity);
        }
        queue.write_buffer(
            &target.cells.instances,
            0,
            bytemuck::cast_slice(&self.scratch),
        );
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
            origin_cells: [0.0; 2],
            cursor_color: [
                cursor.color.r as f32 / 255.0,
                cursor.color.g as f32 / 255.0,
                cursor.color.b as f32 / 255.0,
                CURSOR_ALPHA,
            ],
            skip_color: 0,
            _pad: [0; 3],
            cover: Cover::NONE,
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
        render_pass.set_bind_group(0, &self.live_bind_group, &[0]);
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
        let Some(target) = self.composite_slots.get(pool).filter(|s| s.cells.count > 0) else {
            return;
        };

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.composite_bind_group, &[globals_offset(slot)]);
        render_pass.set_vertex_buffer(0, target.cells.instances.slice(..));
        render_pass.draw(0..6, 0..target.cells.count);
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
        render_pass.set_bind_group(0, &self.live_bind_group, &[CURSOR_GLOBALS_OFFSET]);
        render_pass.draw(0..6, 0..1);
    }
}

/// One pool's composite cell instances, with the rotation and the grid shape
/// they were written at.
///
/// A glide crosses a row on most of its composited frames. A slot that knows
/// its rotation advances it by the rows the pool scrolled and writes only the
/// rows the scroll exposed, the way the live pass rotates its own buffer.
struct BackgroundSlot {
    cells: CompositeSlot,
    /// Display row `r` lives at slot `(r + row_offset) % rows`.
    row_offset: u32,
    /// The grid shape [`Self::cells`] holds. A scroll carries rows only into a
    /// buffer laid out for the same shape.
    rows: usize,
    cols: usize,
}

/// An empty composite slot at the initial capacity, for a pool being composited
/// for the first time.
fn new_slot(device: &Device) -> BackgroundSlot {
    BackgroundSlot {
        cells: CompositeSlot {
            instances: alloc_instances(device, INITIAL_CAPACITY),
            capacity: INITIAL_CAPACITY,
            count: 0,
        },
        row_offset: 0,
        rows: 0,
        cols: 0,
    }
}

fn alloc_instances(device: &Device, capacity: usize) -> Buffer {
    let usage = BufferUsages::VERTEX | BufferUsages::COPY_DST;
    // A test reads the instances back, which only a copy source allows.
    #[cfg(test)]
    let usage = usage | BufferUsages::COPY_SRC;
    device.create_buffer(&BufferDescriptor {
        label: Some("background instances"),
        size: (capacity * size_of::<BgInstance>()) as u64,
        usage,
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

/// The runs of the rows `is_damaged` names that fill consecutive buffer slots,
/// each as its first slot and the display rows it covers.
///
/// A run ends at a row it does not name, and where the rotation wraps the next
/// row's slot back to the start of the buffer. Each run is then one range of
/// the buffer, which one write covers.
fn damaged_row_runs(
    is_damaged: impl Fn(usize) -> bool,
    rows: usize,
    row_offset: u32,
) -> impl Iterator<Item = (usize, Range<usize>)> {
    let mut row = 0;
    iter::from_fn(move || {
        while row < rows && !is_damaged(row) {
            row += 1;
        }
        if row == rows {
            return None;
        }

        let start = row;
        let slot = row_slot(start, row_offset, rows);
        row += 1;
        while row < rows
            && is_damaged(row)
            && row_slot(row, row_offset, rows) == slot + (row - start)
        {
            row += 1;
        }
        Some((slot, start..row))
    })
}

fn build_instances(grid: &Grid, out: &mut Vec<BgInstance>) {
    let Some(last_col) = grid.cols().checked_sub(1) else {
        return;
    };
    for row in 0..grid.rows() {
        build_row_instances(grid, row, 0..=last_col, out);
    }
}

fn build_row_instances(
    grid: &Grid,
    row: usize,
    columns: RangeInclusive<usize>,
    out: &mut Vec<BgInstance>,
) {
    out.extend(columns.map(|col| {
        let (_, bg) = grid.get(row, col).draw_colors();
        BgInstance {
            color: instance_bytes(bg),
        }
    }));
}

/// `rgb` as the opaque bytes of one [`BgInstance`].
fn instance_bytes(rgb: Rgb) -> [u8; 4] {
    [rgb.r, rgb.g, rgb.b, 255]
}

/// `rgb` as the `u32` the vertex stage reads from an instance of that color.
///
/// Built from [`instance_bytes`], so the skip compare and the instances always
/// pack a color the same way. Vertex attributes are little-endian.
fn packed_color(rgb: Rgb) -> u32 {
    u32::from_le_bytes(instance_bytes(rgb))
}

#[cfg(test)]
mod tests {
    use super::{
        build_instances, build_row_instances, damaged_row_runs, row_slot, BackgroundPass,
        BgInstance, CursorState,
    };
    use crate::{
        render::{self, CellMetrics, Occluder, PoolOccluders},
        test_support::require_headless_device,
    };
    use std::ops::Range;
    use stoatty_term::{
        grid::{BorderStyle, Flags, Grid, Panel, PanelShadow, Rgb},
        term::Damage,
    };
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
        let module = wgsl::parse_str(&render::with_occlusion(&render::with_cover(include_str!(
            "../shaders/bg.wgsl"
        ))))
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

    /// The instance stream declares one 4-byte `Uint32` attribute where the
    /// shader takes a `u32`, an agreement only pipeline creation checks.
    /// Validating the WGSL alone would not catch a stride or format that no longer
    /// matches what `vs_main` reads.
    #[test]
    fn the_pipeline_accepts_the_packed_instance_layout() {
        let (device, _queue) = require_headless_device();

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
        build_row_instances(&grid, 2, 0..=cols - 1, &mut instances);

        let bytes = size_of::<BgInstance>();
        assert_eq!(
            (instances.len() * bytes, 2 * cols * bytes),
            (cols * 4, 40),
            "a row spans 4 bytes per cell, at four times its row-major start",
        );
    }

    /// A cell changing in place damages its own columns, and the instance is
    /// fixed size, so what the patch writes is that column's slice rather than
    /// the row holding it. A spinner or a clock is the case this is for.
    #[test]
    fn a_one_cell_change_patches_only_that_cell() {
        let (rows, cols) = (4, 5);
        let mut grid = Grid::new(rows, cols);
        grid.get_mut(2, 3).bg = Rgb::new(10, 20, 30);

        let mut instances = Vec::new();
        build_row_instances(&grid, 2, 3..=3, &mut instances);

        assert_eq!(
            instances.iter().map(|i| i.color).collect::<Vec<_>>(),
            [[10, 20, 30, 255]],
            "the bounded build carries the one cell the damage named",
        );

        let bytes = size_of::<BgInstance>();
        let slot = row_slot(2, 0, rows);
        assert_eq!(
            (slot * cols + 3) * bytes,
            13 * bytes,
            "written at the cell's own offset, not its row's start",
        );
    }

    /// Damage over `rows` rows, naming the rows in `damaged` across a width of
    /// three.
    fn partial(rows: usize, damaged: &[usize]) -> Damage {
        Damage::Partial(
            (0..rows)
                .map(|row| damaged.contains(&row).then_some((0, 2)))
                .collect(),
        )
    }

    #[test]
    fn contiguous_damaged_rows_share_one_write() {
        let damage = partial(10, &[3, 4, 5, 6, 8]);
        let runs: Vec<(usize, Range<usize>)> =
            damaged_row_runs(|row| damage.is_dirty(row), 10, 0).collect();
        assert_eq!(runs, [(3, 3..7), (8, 8..9)], "a clean row ends a run");
    }

    #[test]
    fn a_run_splits_where_the_rotation_wraps() {
        let damage = partial(5, &[0, 1, 2, 3, 4]);
        let runs: Vec<(usize, Range<usize>)> =
            damaged_row_runs(|row| damage.is_dirty(row), 5, 3).collect();
        assert_eq!(
            runs,
            [(3, 0..2), (0, 2..5)],
            "row 2 wraps from slot 4 back to slot 0"
        );
    }

    /// A flood frame names every row, and after a scroll its rows wrap in the
    /// buffer, so it goes up as two runs. A later one-cell change goes up alone.
    /// Every slot has to hold the row the shader reads there, both sides of the
    /// wrap included.
    #[test]
    fn a_rotated_flood_lands_every_row_where_the_shader_reads_it() {
        let (device, queue) = require_headless_device();
        let (rows, cols) = (5, 3);
        let metrics = CellMetrics {
            font_size: 10.0,
            width: 6.0,
            height: 12.0,
            scale_factor: 1.0,
        };
        let mut pass = BackgroundPass::new(&device, TextureFormat::Rgba8Unorm, metrics);
        let cursor = CursorState {
            corners: None,
            color: Rgb::new(0, 0, 0),
        };
        let mut prepare = |grid: &Grid, damage: &Damage, scrolled_rows: isize| {
            pass.prepare(
                &device,
                &queue,
                grid,
                [64.0, 64.0],
                cursor,
                SKIP,
                0.0,
                damage,
                scrolled_rows,
                &[],
                &[],
            );
        };

        let mut grid = Grid::new(rows, cols);
        let paint = |grid: &mut Grid, base: u8| {
            for row in 0..rows {
                for col in 0..cols {
                    grid.get_mut(row, col).bg = Rgb::new(base + row as u8, col as u8, 0);
                }
            }
        };
        paint(&mut grid, 0);
        prepare(&grid, &Damage::Full, 0);
        paint(&mut grid, 10);
        prepare(&grid, &partial(rows, &[0, 1, 2, 3, 4]), 2);
        grid.get_mut(4, 1).bg = Rgb::new(99, 99, 99);
        prepare(
            &grid,
            &Damage::Partial(vec![None, None, None, None, Some((1, 1))]),
            0,
        );

        let expected: Vec<[u8; 4]> = (0..rows * cols)
            .map(|index| {
                let (row, col) = (shader_row(index / cols, 2, rows), index % cols);
                match (row, col) {
                    (4, 1) => [99, 99, 99, 255],
                    _ => [10 + row as u8, col as u8, 0, 255],
                }
            })
            .collect();
        assert_eq!(
            read_instances(&device, &queue, &pass, rows * cols),
            expected
        );
    }

    /// The first `count` instances `pass` holds, read back from the GPU.
    fn read_instances(
        device: &Device,
        queue: &Queue,
        pass: &BackgroundPass,
        count: usize,
    ) -> Vec<[u8; 4]> {
        let size = (count * size_of::<BgInstance>()) as u64;
        let readback = device.create_buffer(&BufferDescriptor {
            label: Some("background readback"),
            size,
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        encoder.copy_buffer_to_buffer(&pass.instances, 0, &readback, 0, size);
        queue.submit(Some(encoder.finish()));

        readback.slice(..).map_async(MapMode::Read, |_| {});
        device
            .poll(PollType::wait_indefinitely())
            .expect("poll readback");
        let bytes = readback.slice(..).get_mapped_range().to_vec();
        bytes.as_chunks::<4>().0.to_vec()
    }

    /// The color a frame's cells skip, and the clear the readback tests set
    /// apart from it. In a real frame the two are equal, so a skipped cell and
    /// a painted one look the same. Apart, a skipped cell shows the clear.
    const SKIP: Rgb = Rgb::new(10, 20, 30);
    const CLEARED: Rgb = Rgb::new(250, 0, 250);
    const OTHER: Rgb = Rgb::new(200, 100, 50);

    /// The readback target's edge, in pixels. Four bytes a texel makes a row
    /// exactly the 256-byte copy alignment, so the readback needs no stride
    /// padding.
    const TARGET: u32 = 64;

    /// Sixteen-pixel cells, so a four-by-four grid covers [`TARGET`] exactly.
    const CELLS: usize = 4;
    const CELL_METRICS: CellMetrics = CellMetrics {
        font_size: 10.0,
        width: 16.0,
        height: 16.0,
        scale_factor: 1.0,
    };

    /// A cell of the skip color draws no quad, so the clear shows through it,
    /// while a cell of any other color paints itself.
    #[test]
    fn a_cell_of_the_clear_color_draws_nothing() {
        let (device, queue) = require_headless_device();
        let mut grid = filled_grid(SKIP);
        grid.get_mut(1, 2).bg = OTHER;
        let mut pass = BackgroundPass::new(&device, TextureFormat::Rgba8Unorm, CELL_METRICS);

        prepare_live(&device, &queue, &mut pass, &grid, &[], &[]);
        let rgba = render_rgba(&device, &queue, CLEARED, |render_pass| {
            pass.draw(render_pass)
        });

        let expected: Vec<[u8; 3]> = (0..CELLS * CELLS)
            .map(|cell| match cell {
                6 => rgb(OTHER),
                _ => rgb(CLEARED),
            })
            .collect();
        assert_eq!(
            cell_centers(&rgba),
            expected,
            "only the cell of another color paints over the clear",
        );
    }

    /// A live cell inside a pool's scissor draws no quad, since the pool
    /// repaints it, while a cell a panel meets still draws, since an occludable
    /// pool leaves the pixels under a panel to the live grid.
    ///
    /// The first scissor runs along cell edges. The one-pixel inset leaves each
    /// of its cells a pixel short of it, so none of them is culled.
    #[test]
    fn a_live_cell_under_a_pool_draws_nothing_unless_a_panel_meets_it() {
        let (device, queue) = require_headless_device();
        let grid = filled_grid(OTHER);
        let mut occluders = Vec::new();
        render::build_occluders_into(
            &[panel_at(2, 2)],
            &[],
            &[],
            CELL_METRICS.width,
            &mut occluders,
        );
        let mut pass = BackgroundPass::new(&device, TextureFormat::Rgba8Unorm, CELL_METRICS);

        let covered = [[0, 48, 64, 16], [15, 15, 34, 34]];
        prepare_live(&device, &queue, &mut pass, &grid, &occluders, &covered);
        let rgba = render_rgba(&device, &queue, CLEARED, |render_pass| {
            pass.draw(render_pass)
        });

        let expected: Vec<[u8; 3]> = (0..CELLS * CELLS)
            .map(|cell| match cell {
                5 | 6 | 9 => rgb(CLEARED),
                _ => rgb(OTHER),
            })
            .collect();
        assert_eq!(
            cell_centers(&rgba),
            expected,
            "the middle cells the panel misses show the clear, and every other cell paints",
        );
    }

    /// A pool composite covers the live grid under it, so it paints every cell,
    /// the cells of the clear color included, while the live frame skips them.
    #[test]
    fn a_composite_paints_every_cell_of_the_clear_color() {
        let (device, queue) = require_headless_device();
        let grid = filled_grid(SKIP);
        let mut pass = BackgroundPass::new(&device, TextureFormat::Rgba8Unorm, CELL_METRICS);

        prepare_live(&device, &queue, &mut pass, &grid, &[], &[]);
        assert_eq!(
            composite_cells(&device, &queue, &mut pass, &grid, None, true),
            vec![rgb(SKIP); CELLS * CELLS],
            "every composite cell paints its own color",
        );
    }

    /// A composite that scrolled draws what one built from scratch draws, which
    /// is its grid's own colors. It rotates the rows the scroll kept and writes
    /// the ones the scroll exposed, down and up and across the end of its
    /// buffer. A frame that keeps its rows draws them at the rotation they sit
    /// at. A grid of another shape rebuilds whatever the scroll says, and so does
    /// a change that moved nothing.
    #[test]
    fn a_scrolled_composite_matches_one_built_from_scratch() {
        let (device, queue) = require_headless_device();
        let mut pass = BackgroundPass::new(&device, TextureFormat::Rgba8Unorm, CELL_METRICS);
        // Four rows, so three down keeps one row, two more writes rows that wrap
        // the end of the buffer, and three up writes the top three, wrapping
        // again.
        let steps = [
            (document_rows(0, 4), None, true),
            (document_rows(3, 4), Some(3), true),
            (document_rows(5, 4), Some(2), true),
            (document_rows(2, 4), Some(-3), true),
            (document_rows(2, 4), Some(0), false),
            (document_rows(9, 3), Some(1), true),
            (document_rows(1, 3), Some(0), true),
        ];

        let (drawn, built): (Vec<_>, Vec<_>) = steps
            .iter()
            .map(|(grid, scrolled, changed)| {
                (
                    composite_cells(&device, &queue, &mut pass, grid, *scrolled, *changed),
                    grid_cells(grid),
                )
            })
            .unzip();
        assert_eq!(drawn, built, "each step draws its grid's own colors");
    }

    /// A scroll writes only the rows it exposed. A kept row stays as the earlier
    /// composite wrote it, so a kept row that changed without a word to the pass
    /// still draws its earlier colors.
    #[test]
    fn a_scrolled_composite_writes_only_the_rows_it_exposed() {
        let (device, queue) = require_headless_device();
        let mut pass = BackgroundPass::new(&device, TextureFormat::Rgba8Unorm, CELL_METRICS);
        composite_cells(&device, &queue, &mut pass, &document_rows(0, 4), None, true);

        // One row down keeps display rows 0 to 2 and exposes row 3.
        let mut repainted = document_rows(1, 4);
        for col in 0..CELLS {
            repainted.get_mut(0, col).bg = OTHER;
        }
        assert_eq!(
            composite_cells(&device, &queue, &mut pass, &repainted, Some(1), true),
            grid_cells(&document_rows(1, 4)),
            "the kept row draws what the first composite wrote",
        );
    }

    /// A one-cell panel at `row` and `col`, with no fill and no shadow.
    fn panel_at(row: u16, col: u16) -> Panel {
        Panel {
            top: row,
            left: col,
            width: 1,
            height: 1,
            style: BorderStyle::Light,
            border: OTHER,
            corner_radius: 0,
            fill: None,
            shadow: PanelShadow::None_,
            inset_x: 0,
            above_pools: false,
            anchor: None,
            seq: 0,
        }
    }

    /// A [`CELLS`]-square grid with every cell's background `color`.
    fn filled_grid(color: Rgb) -> Grid {
        let mut grid = Grid::new(CELLS, CELLS);
        for row in 0..CELLS {
            for col in 0..CELLS {
                grid.get_mut(row, col).bg = color;
            }
        }
        grid
    }

    /// A [`CELLS`]-wide grid of `rows` rows whose row `r` holds document row
    /// `top + r`, each cell colored by its document row and its column.
    fn document_rows(top: u8, rows: usize) -> Grid {
        let mut grid = Grid::new(rows, CELLS);
        for row in 0..rows {
            for col in 0..CELLS {
                grid.get_mut(row, col).bg = Rgb::new(20 * (top + row as u8), 40 * col as u8, 7);
            }
        }
        grid
    }

    /// The cell colors `pass` draws for `grid` composited as pool 1, whose
    /// content moved `scrolled` rows since its last composite and whose rows
    /// `changed` or held.
    fn composite_cells(
        device: &Device,
        queue: &Queue,
        pass: &mut BackgroundPass,
        grid: &Grid,
        scrolled: Option<isize>,
        changed: bool,
    ) -> Vec<[u8; 3]> {
        pass.prepare_composite(
            device,
            queue,
            grid,
            PoolOccluders::new(&[], 0, false),
            [TARGET as f32; 2],
            0.0,
            [0.0; 2],
            changed,
            scrolled,
            1,
            0,
        );
        cell_centers(&render_rgba(device, queue, CLEARED, |render_pass| {
            pass.draw_composite(render_pass, 1, 0)
        }))
    }

    /// The colors [`cell_centers`] reads where `grid` covers the target, and the
    /// clear everywhere else.
    fn grid_cells(grid: &Grid) -> Vec<[u8; 3]> {
        (0..CELLS * CELLS)
            .map(|cell| {
                let (row, col) = (cell / CELLS, cell % CELLS);
                match row < grid.rows() && col < grid.cols() {
                    true => rgb(grid.get(row, col).bg),
                    false => rgb(CLEARED),
                }
            })
            .collect()
    }

    /// Upload `grid` as a whole live frame that skips [`SKIP`], with the cursor
    /// hidden, under the pools at `covered` and the panels in `occluders`.
    fn prepare_live(
        device: &Device,
        queue: &Queue,
        pass: &mut BackgroundPass,
        grid: &Grid,
        occluders: &[Occluder],
        covered: &[[u32; 4]],
    ) {
        let cursor = CursorState {
            corners: None,
            color: OTHER,
        };
        pass.prepare(
            device,
            queue,
            grid,
            [TARGET as f32; 2],
            cursor,
            SKIP,
            0.0,
            &Damage::Full,
            0,
            occluders,
            covered,
        );
    }

    /// The color at the center of each cell, in row-major order.
    fn cell_centers(rgba: &[u8]) -> Vec<[u8; 3]> {
        let edge = TARGET as usize / CELLS;
        (0..CELLS * CELLS)
            .map(|cell| {
                let (x, y) = (
                    (cell % CELLS) * edge + edge / 2,
                    (cell / CELLS) * edge + edge / 2,
                );
                let at = (y * TARGET as usize + x) * 4;
                [rgba[at], rgba[at + 1], rgba[at + 2]]
            })
            .collect()
    }

    fn rgb(color: Rgb) -> [u8; 3] {
        [color.r, color.g, color.b]
    }

    /// What `record` painted over a [`TARGET`]-square target cleared to `clear`,
    /// read back as rgba texels.
    fn render_rgba(
        device: &Device,
        queue: &Queue,
        clear: Rgb,
        record: impl FnOnce(&mut RenderPass<'_>),
    ) -> Vec<u8> {
        let size = Extent3d {
            width: TARGET,
            height: TARGET,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&TextureDescriptor {
            label: Some("background target"),
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
            label: Some("background pixel readback"),
            size: u64::from(TARGET) * u64::from(TARGET) * 4,
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("background"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(Color {
                            r: f64::from(clear.r) / 255.0,
                            g: f64::from(clear.g) / 255.0,
                            b: f64::from(clear.b) / 255.0,
                            a: 1.0,
                        }),
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
        readback.slice(..).get_mapped_range().to_vec()
    }
}
