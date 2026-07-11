//! Instanced per-cell background fill.
//!
//! Draws one solid colored quad per grid cell, reading each [`Cell`]'s
//! background from [`stoatty_term`]'s [`Grid`]. The quad corners are generated
//! in the vertex shader from the vertex index, so the only vertex buffer is
//! the per-cell instance stream; a uniform supplies the screen resolution and
//! cell size used to map cells to clip space.
//!
//! [`Cell`]: stoatty_term::grid::Cell

use crate::render::{build_occluders, composite_occlusion, CellMetrics, Occluder};
use bytemuck::{Pod, Zeroable};
use stoatty_term::{
    grid::{Grid, Panel, Rgb},
    term::Damage,
};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, Buffer,
    BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites, Device,
    FragmentState, PipelineLayoutDescriptor, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, ShaderModule, ShaderModuleDescriptor, ShaderSource, ShaderStages,
    TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in cells, allocated up front. Grows by doubling
/// when a grid exceeds it; 2048 covers a default 24x80 grid without reallocating.
const INITIAL_CAPACITY: usize = 2048;

/// Cursor block blend alpha. The cursor's RGB is the theme's cursor color; this
/// translucency is renderer policy so the block tints the cell beneath it.
const CURSOR_ALPHA: f32 = 0.55;

/// Per-cell instance: grid coordinate and normalized background color.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct BgInstance {
    cell: [f32; 2],
    color: [f32; 3],
}

/// Uniform shared by the cell and cursor pipelines.
///
/// Carries the screen resolution and cell size that map cell coordinates to
/// clip space, the cursor block's four eased corners (two `vec4`s holding
/// [TL, TR] then [BL, BR] in fractional cell coordinates), the cursor color,
/// and the grid's eased vertical scroll offset in pixels.
///
/// `scroll_y`, `panel_count`, `occlude_all`, and `pad3` fill one 16-byte slot
/// so the following `cursor_color` lands on the 16-byte offset the uniform
/// layout requires. The `vec4` corner pairs already sit on 16-byte boundaries.
///
/// `panel_count` and `occlude_all` are non-zero only on an occludable pool
/// composite, so the live cell fill and the cursor draw skip the occluder loop.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    cursor_corners_01: [f32; 4],
    cursor_corners_23: [f32; 4],
    scroll_y: f32,
    panel_count: u32,
    occlude_all: u32,
    pad3: f32,
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
    composite_instances: Buffer,
    composite_capacity: usize,
    composite_count: u32,
    /// One occluder per live panel at binding 1, read by the cell fragment
    /// shader on an occludable pool composite to discard a page cell a box
    /// covers. Unused by the live cell fill and the cursor, which leave the
    /// panel count at zero.
    occluders: Buffer,
    occluder_capacity: usize,
    cursor_pipeline: RenderPipeline,
    cursor_visible: bool,
    metrics: CellMetrics,
    /// Scratch reused each frame to build the cell instances for upload, so a
    /// full rebuild, a damaged row, and a composite frame each allocate none.
    scratch: Vec<BgInstance>,
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
            source: ShaderSource::Wgsl(include_str!("../shaders/bg.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("background globals"),
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
                    attributes: &vertex_attr_array![0 => Float32x2, 1 => Float32x3],
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
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let occluders = alloc_occluders(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &occluders);

        let instances = alloc_instances(device, INITIAL_CAPACITY);
        let composite_instances = alloc_instances(device, INITIAL_CAPACITY);

        BackgroundPass {
            pipeline,
            globals,
            bind_group,
            bind_group_layout,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            composite_instances,
            composite_capacity: INITIAL_CAPACITY,
            composite_count: 0,
            occluders,
            occluder_capacity: INITIAL_CAPACITY,
            cursor_pipeline,
            cursor_visible: false,
            metrics,
            scratch: Vec::new(),
        }
    }

    /// Replace the cell metrics so the next frame lays out cells at the new size.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    /// Upload the panel occluders, reallocating the buffer and rebuilding the
    /// globals bind group when the panel count outgrows the current capacity.
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
            pad3: 0.0,
            cursor_color: [
                cursor.color.r as f32 / 255.0,
                cursor.color.g as f32 / 255.0,
                cursor.color.b as f32 / 255.0,
                CURSOR_ALPHA,
            ],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));
        self.cursor_visible = cursor.corners.is_some();

        let cols = grid.cols();
        let total = grid.rows() * cols;

        // A resize changes the cell count and a grow reallocates (dropping the
        // buffer's contents), so both rebuild every cell; otherwise rewrite only
        // the damaged rows. Each cell is one instance, so row r is the fixed slice
        // [r*cols, (r+1)*cols) and can be patched in place.
        let full =
            matches!(damage, Damage::Full) || total != self.count as usize || total > self.capacity;
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
        } else {
            for row in 0..grid.rows() {
                if damage.is_dirty(row) {
                    self.scratch.clear();
                    build_row_instances(grid, row, &mut self.scratch);
                    let offset = (row * cols * size_of::<BgInstance>()) as u64;
                    queue.write_buffer(
                        &self.instances,
                        offset,
                        bytemuck::cast_slice(&self.scratch),
                    );
                }
            }
        }
    }

    /// Upload the uniform and per-cell instances for a pool grid being
    /// composited over the live grid, into buffers separate from the live ones.
    ///
    /// A pool composite paints a pooled page over the live grid mid-glide.
    /// Building its cells into [`Self::instances`] would erase the live grid's
    /// damage-tracked instances, so the pool builds into a dedicated buffer that
    /// [`Self::draw_composite`] reads, leaving the live buffer intact for the
    /// next live frame.
    ///
    /// `grid_scroll` shifts the grid up by that many rows. The pool grid changes
    /// wholesale each frame, so every cell is rebuilt with no per-row damage
    /// path. No cursor draws over a composite, so the shared globals carry none.
    ///
    /// `occludable` marks a pane pool that sits under every box. Its page cells
    /// are then occluded against `panels` with the seq test bypassed, so a
    /// pooled cell gliding beneath a modal is hidden by it. A non-pane pool
    /// passes `false` and its cells never occlude, since they are box content.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        panels: &[Panel],
        resolution: [f32; 2],
        grid_scroll: f32,
        content_changed: bool,
        occludable: bool,
    ) {
        let occluders = build_occluders(panels);
        self.upload_occluders(device, queue, &occluders);
        let (panel_count, occlude_all) = composite_occlusion(occludable, &occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            cursor_corners_01: [0.0; 4],
            cursor_corners_23: [0.0; 4],
            scroll_y: grid_scroll * self.metrics.height,
            panel_count,
            occlude_all,
            pad3: 0.0,
            cursor_color: [0.0; 4],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        // Cell quads carry no atlas UVs, so a sub-cell glide over unchanged rows
        // reuses last frame's instances once the globals write above has
        // re-applied the shift.
        if !content_changed {
            return;
        }

        self.scratch.clear();
        build_instances(grid, &mut self.scratch);
        self.composite_count = self.scratch.len() as u32;
        if self.scratch.is_empty() {
            return;
        }
        if self.scratch.len() > self.composite_capacity {
            self.composite_capacity = self.scratch.len().next_power_of_two();
            self.composite_instances = alloc_instances(device, self.composite_capacity);
        }
        queue.write_buffer(
            &self.composite_instances,
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
            pad3: 0.0,
            cursor_color: [
                cursor.color.r as f32 / 255.0,
                cursor.color.g as f32 / 255.0,
                cursor.color.b as f32 / 255.0,
                CURSOR_ALPHA,
            ],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));
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
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        render_pass.draw(0..6, 0..self.count);
    }

    /// Record a composited pool's background draw into `render_pass`.
    ///
    /// A no-op until [`Self::prepare_composite`] has run with a non-empty grid.
    /// Reads the composite instance buffer, so drawing a pool leaves the live
    /// cell instances a prior [`Self::prepare`] uploaded untouched.
    pub fn draw_composite(&self, render_pass: &mut RenderPass<'_>) {
        if self.composite_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.composite_instances.slice(..));
        render_pass.draw(0..6, 0..self.composite_count);
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
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.draw(0..6, 0..1);
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
                resource: globals.as_entire_binding(),
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

fn build_instances(grid: &Grid, out: &mut Vec<BgInstance>) {
    for row in 0..grid.rows() {
        build_row_instances(grid, row, out);
    }
}

fn build_row_instances(grid: &Grid, row: usize, out: &mut Vec<BgInstance>) {
    out.extend((0..grid.cols()).map(|col| {
        let (_, bg) = grid.get(row, col).draw_colors();
        BgInstance {
            cell: [col as f32, row as f32],
            color: [
                bg.r as f32 / 255.0,
                bg.g as f32 / 255.0,
                bg.b as f32 / 255.0,
            ],
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::build_instances;
    use stoatty_term::grid::{Flags, Grid, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(include_str!("../shaders/bg.wgsl")).expect("parse bg.wgsl");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate bg.wgsl");
    }

    #[test]
    fn instances_cover_every_cell_with_normalized_bg() {
        let mut grid = Grid::new(2, 2);
        grid.get_mut(0, 0).bg = Rgb::new(255, 0, 0);
        grid.get_mut(1, 1).bg = Rgb::new(0, 0, 255);

        let mut instances = Vec::new();
        build_instances(&grid, &mut instances);

        assert_eq!(instances.len(), 4);
        assert_eq!(instances[0].cell, [0.0, 0.0]);
        assert_eq!(instances[0].color, [1.0, 0.0, 0.0]);
        assert_eq!(instances[3].cell, [1.0, 1.0]);
        assert_eq!(instances[3].color, [0.0, 0.0, 1.0]);
    }

    #[test]
    fn inverse_cell_draws_foreground_as_background() {
        let mut grid = Grid::new(1, 1);
        grid.get_mut(0, 0).fg = Rgb::new(255, 0, 0);
        grid.get_mut(0, 0).bg = Rgb::new(0, 0, 255);
        grid.get_mut(0, 0).flags = Flags::INVERSE;

        let mut instances = Vec::new();
        build_instances(&grid, &mut instances);

        assert_eq!(instances[0].color, [1.0, 0.0, 0.0]);
    }
}
