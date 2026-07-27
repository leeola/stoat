//! Instanced stroked-path pass.
//!
//! Draws each [`Polyline`] as anti-aliased capsule segments off the cell grid,
//! above the grid with its own z-order. The protocol's only non-axis-aligned
//! primitive, added so the commit graph can draw lane and merge lines. Endpoints
//! ride in cell-fraction units and the vertex shader scales them by the live
//! cell size, so a path tracks font zoom.

use crate::render::{occlusion_globals, pool_occluders, CellMetrics, Occluder};
use bytemuck::{Pod, Zeroable};
use stoatty_term::grid::{Panel, Polyline, Rgb};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, Buffer,
    BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites, Device,
    FragmentState, PipelineLayoutDescriptor, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, TextureFormat,
    VertexBufferLayout, VertexState, VertexStepMode,
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
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    panel_count: u32,
    occlude_all: u32,
    _pad: [u32; 2],
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
    count: u32,
    /// Segments of a pool grid being composited over the live grid, built by
    /// [`Self::prepare_composite`] into a buffer separate from
    /// [`Self::instances`] so a pool draw leaves the live paths intact.
    composite_instances: Buffer,
    composite_capacity: usize,
    composite_count: u32,
    /// One occluder per live panel, read by the fragment shader to discard path
    /// fragments a later box covers. Bound alongside the globals, and rebuilt
    /// into a new bind group whenever it reallocates.
    occluders: Buffer,
    occluder_capacity: usize,
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
            source: ShaderSource::Wgsl(include_str!("../shaders/polyline.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("polyline globals"),
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
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let occluders = alloc_occluders(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &occluders);

        let instances = alloc_instances(device, INITIAL_CAPACITY);
        let composite_instances = alloc_instances(device, INITIAL_CAPACITY);

        PolylinePass {
            pipeline,
            bind_group_layout,
            globals,
            bind_group,
            instances,
            last_instances: Vec::new(),
            capacity: INITIAL_CAPACITY,
            count: 0,
            composite_instances,
            composite_capacity: INITIAL_CAPACITY,
            composite_count: 0,
            occluders,
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
        self.upload_occluders(device, queue, occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count: occluders.len() as u32,
            occlude_all: 0,
            _pad: [0; 2],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let instances = build_polyline_instances(polylines, 0.0);
        self.count = instances.len() as u32;
        if instances.is_empty() {
            return;
        }

        if !crate::render::upload_needed(&instances, &self.last_instances) {
            return;
        }

        if instances.len() > self.capacity {
            self.capacity = instances.len().next_power_of_two();
            self.instances = alloc_instances(device, self.capacity);
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
        self.last_instances = instances;
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

    /// Upload one instance per segment of a pool grid being composited, offset
    /// down by the pool's eased `shift_rows` so the paths glide with the page
    /// cells.
    ///
    /// Writes a buffer separate from the live [`Self::prepare`], reusing the
    /// shared globals uniform the live pass already wrote this frame.
    ///
    /// `occludable` marks a pane pool that sits under every box. Its paths are
    /// then occluded against all of `panels` with the seq test bypassed, so a line
    /// gliding beneath a modal is hidden by it. A non-pane pool passes `false` and
    /// its paths occlude only against the panels that float above every pooled
    /// surface, since they are a box's own content.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        polylines: &[Polyline],
        panels: &[Panel],
        resolution: [f32; 2],
        shift_rows: f32,
        occludable: bool,
    ) {
        let occluders = pool_occluders(occludable, panels);
        self.upload_occluders(device, queue, &occluders);
        let (panel_count, occlude_all) = occlusion_globals(&occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count,
            occlude_all,
            _pad: [0; 2],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let instances = build_polyline_instances(polylines, shift_rows);
        self.composite_count = instances.len() as u32;
        if instances.is_empty() {
            return;
        }

        if instances.len() > self.composite_capacity {
            self.composite_capacity = instances.len().next_power_of_two();
            self.composite_instances = alloc_instances(device, self.composite_capacity);
        }
        queue.write_buffer(
            &self.composite_instances,
            0,
            bytemuck::cast_slice(&instances),
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
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        render_pass.draw(0..6, 0..self.count);
    }

    /// Record the composited pool's path draw into `render_pass`.
    ///
    /// A no-op until [`Self::prepare_composite`] has run. Reads the composite
    /// buffer, so a pool draw leaves the live paths a prior [`Self::prepare`]
    /// uploaded untouched. Inherits the pool pass's scissor.
    pub fn draw_composite(&self, render_pass: &mut RenderPass<'_>) {
        if self.composite_count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.composite_instances.slice(..));
        render_pass.draw(0..6, 0..self.composite_count);
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
                resource: globals.as_entire_binding(),
            },
            BindGroupEntry {
                binding: 1,
                resource: occluders.as_entire_binding(),
            },
        ],
    })
}

/// Flatten each path into one instance per segment, in draw order, converting
/// the sixteenth-cell wire units to the cell-fraction units the shader scales by
/// the cell size.
///
/// A single-point path yields one zero-length segment, which the capsule SDF
/// draws as a dot. An empty path yields nothing.
///
/// `shift_rows` offsets every endpoint down by that many cells, baked in here.
/// The live path passes zero. A pool composite passes the eased sub-cell scroll
/// so slot-bound paths glide with the page, since the shader carries no scroll
/// uniform of its own.
fn build_polyline_instances(polylines: &[Polyline], shift_rows: f32) -> Vec<PolylineInstance> {
    let mut out = Vec::new();
    for polyline in polylines {
        let point = |[x, y]: [i16; 2]| {
            [
                f32::from(x) / SIXTEENTHS,
                f32::from(y) / SIXTEENTHS + shift_rows,
            ]
        };
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
    out
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
    use super::build_polyline_instances;
    use stoatty_term::grid::{Polyline, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    fn path(points: &[[i16; 2]]) -> Polyline {
        Polyline {
            points: points.to_vec(),
            width: 8,
            color: Rgb::new(220, 50, 47),
            seq: 7,
        }
    }

    #[test]
    fn shader_is_valid_wgsl() {
        let module =
            wgsl::parse_str(include_str!("../shaders/polyline.wgsl")).expect("parse polyline");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate polyline");
    }

    #[test]
    fn instances_map_sixteenths_to_cell_fractions() {
        let instances = build_polyline_instances(&[path(&[[8, 16], [24, 32]])], 0.0);

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
        let instances = build_polyline_instances(&[path(&[[0, 0], [0, 16], [16, 32]])], 0.0);

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
        let instances = build_polyline_instances(&[path(&[[8, 8]])], 0.0);

        assert_eq!(instances.len(), 1);
        assert_eq!(
            instances[0].p0, instances[0].p1,
            "a dot is a capsule with no length"
        );
    }

    #[test]
    fn an_empty_path_yields_no_instances() {
        assert!(build_polyline_instances(&[path(&[])], 0.0).is_empty());
    }

    #[test]
    fn composite_shift_offsets_both_endpoints_by_whole_cells() {
        let instances = build_polyline_instances(&[path(&[[0, 16], [0, 32]])], -0.5);

        assert_eq!(instances[0].p0, [0.0, 0.5]);
        assert_eq!(instances[0].p1, [0.0, 1.5], "the far end shifts equally");
    }

    /// The pass skips its GPU write when the rebuilt instances match the last
    /// upload, so an unchanged graph must compare equal across rebuilds and a
    /// real change must not.
    #[test]
    fn rebuilt_paths_compare_equal_until_one_changes() {
        use crate::render::upload_needed;

        let paths = [path(&[[0, 0], [0, 16]])];
        let first = build_polyline_instances(&paths, 0.0);
        assert!(
            !upload_needed(&build_polyline_instances(&paths, 0.0), &first),
            "an unchanged path rebuilds to the same bytes, so no upload is needed",
        );

        let moved = [path(&[[1, 0], [0, 16]])];
        assert!(
            upload_needed(&build_polyline_instances(&moved, 0.0), &first),
            "a moved endpoint must reach the GPU",
        );
        assert!(
            upload_needed(&build_polyline_instances(&[], 0.0), &first),
            "so must dropping the path entirely",
        );
    }
}
