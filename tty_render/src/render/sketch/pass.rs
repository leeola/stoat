//! Instanced hand-drawn mark pass.
//!
//! Draws each [`Sketch`] as anti-aliased strokes and convex fills over the cell
//! grid, above the stroked paths and below the minimap. A mark reveals itself
//! along its own arc length, so a walkthrough draws a circle on as the
//! narration reaches it.
//!
//! The geometry is generated rather than sent. A frame that only advances the
//! reveal rebuilds no points, because the cache is keyed on the sketch list and
//! the cell size, and neither moves while a mark draws itself.

use crate::render::{
    sketch::rough, CellMetrics, GridVersion, Occluder, OccluderBuffer, GLOBALS_SLOT_STRIDE,
};
use bytemuck::{Pod, Zeroable};
use std::mem;
use stoatty_protocol::command::{SketchFillStyle, SketchShape, SketchSide};
use stoatty_term::grid::{Grid, Sketch};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBinding, BufferBindingType, BufferDescriptor, BufferSize, BufferUsages,
    ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor, Queue,
    RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource,
    ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in marks, allocated up front. Grows by doubling
/// when a frame declares more.
const INITIAL_CAPACITY: usize = 64;

/// Point buffer capacity, in points, allocated up front. A single wobbling
/// ellipse runs to a few hundred, so this holds a handful of marks before the
/// first grow.
const INITIAL_POINTS: usize = 4096;

/// The instance kind that strokes a revealed span of a path.
const KIND_STROKE: u32 = 0;

/// The instance kind that fills a convex quad.
const KIND_FILL: u32 = 1;

/// How far outside a component's outline a connector's end sits, in pixels, so
/// the line stops short of the stroke it points at instead of crossing it.
const COMPONENT_GAP: f32 = 4.0;

/// The per-stroke instance data.
///
/// One instance covers a whole stroke rather than one segment, for the reason
/// [`crate::render::polyline`] gives: two capsules meeting at a shared endpoint
/// composite their anti-aliased fringes twice, which beads every joint. A
/// generated stroke has dozens of joints, so the bead becomes the whole look.
///
/// The points sit in a shared storage buffer this indexes, unlike a polyline's,
/// which ride inline. That pass binds one group across the live grid and every
/// composited pool. This one never draws on a pool, so a single arena works
/// and a stroke is free to run to any length.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
struct SketchInstance {
    /// The mark's bounding box in physical pixels, as `[min_x, min_y, max_x,
    /// max_y]`, which the vertex stage grows by the stroke reach to size the
    /// quad.
    bounds: [f32; 4],
    /// Straight color and alpha, the alpha already carrying a fill's fade.
    color: [f32; 4],
    half_width: f32,
    /// How far along the segment after the revealed run the pen sits, so the
    /// stroke grows smoothly instead of snapping point to point.
    reveal_t: f32,
    /// Pixels this mark is shifted down by, for one riding a gliding pane.
    dy: f32,
    _pad: f32,
    point_offset: u32,
    seq: u32,
    /// Whole points of this stroke that are revealed, or 4 for a fill.
    reveal_count: u32,
    kind: u32,
}

/// The uniform shared by every instance.
///
/// Padded to 32 bytes to match the WGSL uniform layout.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    /// Occluders arrive in whole-cell units, so hiding a mark under a box needs
    /// the live cell rectangle even though nothing else in this pass does.
    cell_size: [f32; 2],
    panel_count: u32,
    _pad: [u32; 3],
}

/// One generated stroke's span in the shared point buffer, with the arc lengths
/// a reveal searches.
struct StrokeSpan {
    point_offset: u32,
    count: u32,
    /// Distance along the stroke at each of its points. Held rather than
    /// recomputed because a reveal binary-searches it every frame.
    prefix: Vec<f32>,
    total: f32,
}

/// One sketch's generated geometry, as the frame reads it.
struct MarkGeometry {
    strokes: Vec<StrokeSpan>,
    /// The four corners of a filled box, already in the point buffer, and the
    /// span they occupy.
    fill: Option<(u32, [f32; 4])>,
    /// The mark's own pixel bounds, which the vertex stage sizes its quad from.
    bounds: [f32; 4],
}

/// The instanced hand-drawn mark pipeline and its per-frame buffers.
pub struct SketchPass {
    pipeline: RenderPipeline,
    bind_group_layout: BindGroupLayout,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    /// The points of every generated stroke, end to end. Rebuilt only when the
    /// sketch list or the cell size changes, never for a reveal step.
    points: Buffer,
    points_capacity: usize,
    /// The instances last uploaded, so an unchanged frame skips the write.
    last_instances: Vec<SketchInstance>,
    /// Where each frame's instances are built, before being compared against
    /// [`Self::last_instances`] and traded with it. A reveal rebuilds these
    /// every frame, so holding the buffer spares an allocation per frame.
    built: Vec<SketchInstance>,
    /// The generated geometry the instances are built from, one entry per
    /// [`Grid::sketches`] index.
    geometry: Vec<MarkGeometry>,
    /// What [`Self::geometry`] was generated from, or `None` before the first
    /// generation.
    ///
    /// An `Option` rather than a version starting at zero, because a fresh pass
    /// and a grid that has never declared a mark both start there, and the
    /// first frame must generate rather than trust a counter it never read.
    last_generated: Option<(GridVersion, CellMetrics)>,
    count: u32,
    /// Instances of marks riding a compositing pool, each with its host's
    /// scissor. Drawn after that pool's composite instead of with the rest, or
    /// the composite paints over them.
    riding: Vec<(u32, [u32; 4])>,
    occluders: OccluderBuffer,
    /// The uniform last written, so an unchanged frame skips that write too.
    last_globals: Option<Globals>,
    metrics: CellMetrics,
}

impl SketchPass {
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> SketchPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("sketch"),
            source: ShaderSource::Wgsl(
                crate::render::with_occlusion(include_str!("../../shaders/sketch.wgsl")).into(),
            ),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("sketch bind group layout"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::VERTEX_FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: BufferSize::new(size_of::<Globals>() as u64),
                    },
                    count: None,
                },
                storage_entry(1),
                storage_entry(2),
            ],
        });

        let globals = device.create_buffer(&BufferDescriptor {
            label: Some("sketch globals"),
            // One slot, because a sketch never draws on a composited pool and
            // so never needs a second set of globals at a dynamic offset.
            size: GLOBALS_SLOT_STRIDE,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let occluders = OccluderBuffer::new(device, "sketch occluders", 16);
        let points = alloc_points(device, INITIAL_POINTS);
        let bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &globals,
            &occluders.buffer,
            &points,
        );

        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("sketch pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("sketch pipeline"),
            layout: Some(&pipeline_layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<SketchInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    // The bounds, the color with its alpha, the half width
                    // paired with the reveal fraction and the ride shift, then
                    // the point span with the seq and the kind.
                    attributes: &vertex_attr_array![
                        0 => Float32x4,
                        1 => Float32x4,
                        2 => Float32x4,
                        3 => Uint32x4,
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

        SketchPass {
            pipeline,
            bind_group_layout,
            globals,
            bind_group,
            instances: alloc_instances(device, INITIAL_CAPACITY),
            capacity: INITIAL_CAPACITY,
            points,
            points_capacity: INITIAL_POINTS,
            last_instances: Vec::new(),
            built: Vec::new(),
            geometry: Vec::new(),
            last_generated: None,
            count: 0,
            riding: Vec::new(),
            occluders,
            last_globals: None,
            metrics,
        }
    }

    /// Replace the cell metrics, so the next frame regenerates every mark at the
    /// new size.
    ///
    /// A mark is wobbled in pixels rather than scaled, so a font-size change is
    /// a full regeneration rather than a different multiplier. The seed keeps
    /// the new geometry recognizably the same mark.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    #[allow(clippy::too_many_arguments)]
    /// Upload the frame's uniform, occluders, generated points, and one
    /// instance per revealed stroke and fill.
    ///
    /// `progress` carries one reveal fraction per [`Grid::sketches`] entry, in
    /// order. A short slice leaves the marks past its end complete, so a caller
    /// with no clock passes `&[]` and every mark draws whole.
    ///
    /// `anchored` names the pools compositing this frame, so a mark anchored to
    /// one is shifted and held back for [`Self::draw_riding`].
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        progress: &[f32],
        anchored: &[crate::render::AnchoredPanel],
        occluders: &[Occluder],
        resolution: [f32; 2],
    ) {
        // With no mark to draw now and none drawn last frame, nothing reads this
        // pass's buffers, so the frame skips it without touching the GPU. The
        // frame that empties the list still runs, which is what drops the count
        // to zero and stops the draw.
        if grid.sketches().is_empty() && self.count == 0 {
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

        self.regenerate(device, queue, grid);
        build_instances(
            grid.sketches(),
            &self.geometry,
            progress,
            anchored,
            self.metrics,
            &mut self.built,
            &mut self.riding,
        );
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

    /// Record every non-riding mark.
    ///
    /// A no-op on a frame with no mark. Run after the stroked paths, so a mark
    /// sits over the chrome it annotates.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));

        // A riding slot is skipped here and drawn by [`Self::draw_riding`] after
        // the composites, so the base pass leaves a gap where it sits.
        let mut next = 0;
        for &(index, _) in &self.riding {
            if index > next {
                render_pass.draw(0..6, next..index);
            }
            next = index + 1;
        }
        if next < self.count {
            render_pass.draw(0..6, next..self.count);
        }
    }

    /// Record every mark riding a compositing pool, each clipped to its host.
    ///
    /// Recorded after the host's composite rather than with the rest of the
    /// chrome, so the mark lands over the pooled surface it annotates instead
    /// of being painted over by it. A no-op on a frame with no ride.
    pub fn draw_riding(&self, render_pass: &mut RenderPass<'_>) {
        if self.riding.is_empty() {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        for &(index, [x, y, w, h]) in &self.riding {
            if w == 0 || h == 0 {
                continue;
            }
            render_pass.set_scissor_rect(x, y, w, h);
            render_pass.draw(0..6, index..index + 1);
        }
    }

    /// Regenerate every mark's geometry, when the list or the cell size moved.
    ///
    /// A mark is wobbled in pixels rather than scaled, so the same list at a new
    /// font size is different geometry. A cache keyed on the list alone keeps
    /// serving the old size's points, which is why the metrics are in the key.
    fn regenerate(&mut self, device: &Device, queue: &Queue, grid: &Grid) {
        let key = (GridVersion::new(grid, grid.sketches_epoch()), self.metrics);
        if self.last_generated == Some(key) {
            return;
        }
        self.last_generated = Some(key);

        let points = generate_marks(grid.sketches(), self.metrics, &mut self.geometry);

        if points.is_empty() {
            return;
        }
        if points.len() > self.points_capacity {
            self.points_capacity = points.len().next_power_of_two();
            self.points = alloc_points(device, self.points_capacity);
            self.bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals,
                &self.occluders.buffer,
                &self.points,
            );
        }
        queue.write_buffer(&self.points, 0, bytemuck::cast_slice(&points));
    }

    fn upload_occluders(&mut self, device: &Device, queue: &Queue, occluders: &[Occluder]) {
        if self.occluders.upload(device, queue, occluders) {
            self.bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals,
                &self.occluders.buffer,
                &self.points,
            );
        }
    }
}

/// Build one instance per revealed stroke and one per faded fill.
///
/// Runs every frame, because the reveal moves every frame while the geometry
/// behind it does not. A stroke the reveal has not reached contributes no
/// instance at all, rather than an empty one the GPU still rasterizes.
///
/// `riding` collects the slots of marks anchored to a pool compositing this
/// frame, so [`SketchPass::draw`] skips them and [`SketchPass::draw_riding`]
/// picks them up after that pool's composite.
#[allow(clippy::too_many_arguments)]
fn build_instances(
    sketches: &[Sketch],
    geometry: &[MarkGeometry],
    progress: &[f32],
    anchored: &[crate::render::AnchoredPanel],
    metrics: CellMetrics,
    built: &mut Vec<SketchInstance>,
    riding: &mut Vec<(u32, [u32; 4])>,
) {
    built.clear();
    riding.clear();

    for (index, sketch) in sketches.iter().enumerate() {
        let Some(mark) = geometry.get(index) else {
            continue;
        };
        let revealed = progress.get(index).copied().unwrap_or(1.0).clamp(0.0, 1.0);
        let ride = ride_shift(sketch, anchored);
        let dy = ride.map_or(0.0, |(dy, _)| dy);

        let mut push = |instance: SketchInstance| {
            if let Some((_, scissor)) = ride {
                riding.push((built.len() as u32, scissor));
            }
            built.push(instance);
        };

        if let Some((offset, quad_bounds)) = mark.fill {
            let (color, alpha) = fill_style(&sketch.command.shape);
            // The fill eases in over the back half of the reveal, so the box
            // fills behind the stroke rather than ahead of it.
            let faded = smoothstep(0.5, 1.0, revealed);
            push(SketchInstance {
                bounds: quad_bounds,
                color: rgba(color, alpha, faded),
                half_width: 0.0,
                reveal_t: 0.0,
                dy,
                _pad: 0.0,
                point_offset: offset,
                seq: sketch.seq,
                reveal_count: 4,
                kind: KIND_FILL,
            });
        }

        let style = &sketch.command.style;
        let half_width = rough::stroke_width(style, metrics) / 2.0;
        for stroke in &mark.strokes {
            let (reveal_count, reveal_t) = reveal_at(stroke, revealed);
            if reveal_count < 2 && reveal_t <= 0.0 {
                continue;
            }
            push(SketchInstance {
                bounds: mark.bounds,
                color: rgba(style.color, style.alpha, 1.0),
                half_width,
                reveal_t,
                dy,
                _pad: 0.0,
                point_offset: stroke.point_offset,
                seq: sketch.seq,
                reveal_count,
                kind: KIND_STROKE,
            });
        }
    }
}

/// A protocol color and alpha as the straight float the shader blends with,
/// scaled by `fade`.
fn rgba(color: [u8; 3], alpha: u8, fade: f32) -> [f32; 4] {
    [
        f32::from(color[0]) / 255.0,
        f32::from(color[1]) / 255.0,
        f32::from(color[2]) / 255.0,
        f32::from(alpha) / 255.0 * fade,
    ]
}

/// Generate every mark's geometry into `out`, returning the points they share.
///
/// The points of every stroke and every fill land end to end in one buffer, and
/// each record names its own span, because the pass binds a single arena that
/// every instance indexes.
fn generate_marks(
    sketches: &[Sketch],
    metrics: CellMetrics,
    out: &mut Vec<MarkGeometry>,
) -> Vec<[f32; 2]> {
    out.clear();
    let mut points: Vec<[f32; 2]> = Vec::new();

    for sketch in sketches {
        let resolve = |id: u32, side: SketchSide| resolve_component(sketches, id, side, metrics);
        let generated = rough::geometry(&sketch.command, metrics, &resolve);

        let mut strokes = Vec::with_capacity(generated.strokes.len());
        for stroke in &generated.strokes {
            let point_offset = points.len() as u32;
            points.extend_from_slice(&stroke.points);
            strokes.push(StrokeSpan {
                point_offset,
                count: stroke.points.len() as u32,
                total: stroke.lengths.last().copied().unwrap_or(0.0),
                prefix: stroke.lengths.clone(),
            });
        }

        let fill = generated.fill.map(|corners| {
            let at = points.len() as u32;
            points.extend_from_slice(&corners);
            (at, bounds_of(&corners))
        });

        out.push(MarkGeometry {
            bounds: geometry_bounds(&generated),
            strokes,
            fill,
        });
    }

    points
}

/// Where a mark rides, when its anchor names a pool compositing this frame.
fn ride_shift(
    sketch: &Sketch,
    anchored: &[crate::render::AnchoredPanel],
) -> Option<(f32, [u32; 4])> {
    let (host, _) = sketch.command.anchor?;
    let ride = anchored.iter().find(|ride| ride.host == host)?;
    Some((ride.dy_px, ride.scissor))
}

/// Resolve a connector's component end against the mark it names.
///
/// The end lands on the named side's midpoint, pushed out by
/// [`COMPONENT_GAP`] so the line stops short of the outline rather than
/// crossing its stroke. An unknown id yields `None`, which drops the connector:
/// a line to nowhere points at the wrong thing.
fn resolve_component(
    sketches: &[Sketch],
    id: u32,
    side: SketchSide,
    metrics: CellMetrics,
) -> Option<[f32; 2]> {
    let target = sketches.iter().find(|s| s.command.id == id)?;
    let bounds = shape_bounds(&target.command.shape, metrics)?;
    let [min_x, min_y, max_x, max_y] = bounds;
    let center = [(min_x + max_x) / 2.0, (min_y + max_y) / 2.0];

    let side = match side {
        SketchSide::Auto => SketchSide::Right,
        named => named,
    };
    Some(match side {
        SketchSide::Left => [min_x - COMPONENT_GAP, center[1]],
        SketchSide::Right => [max_x + COMPONENT_GAP, center[1]],
        SketchSide::Top => [center[0], min_y - COMPONENT_GAP],
        SketchSide::Bottom | SketchSide::Auto => [center[0], max_y + COMPONENT_GAP],
    })
}

/// A boxed shape's pixel rectangle, or `None` for a connector, which has no box
/// of its own to point at.
fn shape_bounds(shape: &SketchShape, metrics: CellMetrics) -> Option<[f32; 4]> {
    let bounds = match shape {
        SketchShape::Ellipse(bounds) => bounds,
        SketchShape::Rect { bounds, .. } => bounds,
        SketchShape::Line { .. } => return None,
    };
    let (cw, ch) = (metrics.width, metrics.height);
    let x = f32::from(bounds.x) / 16.0 * cw;
    let y = f32::from(bounds.y) / 16.0 * ch;
    Some([
        x,
        y,
        x + f32::from(bounds.w) / 16.0 * cw,
        y + f32::from(bounds.h) / 16.0 * ch,
    ])
}

/// The color and alpha a filled box paints with, or an invisible black for an
/// open one, whose instance is never built.
fn fill_style(shape: &SketchShape) -> ([u8; 3], u8) {
    match shape {
        SketchShape::Rect {
            fill: Some(fill), ..
        } => match fill.style {
            SketchFillStyle::Solid => (fill.color, fill.alpha),
        },
        _ => ([0; 3], 0),
    }
}

/// How much of `stroke` a reveal fraction has drawn, as whole points plus how
/// far along the segment after them the pen sits.
///
/// The search is over arc length rather than point count, so the pen moves at an
/// even speed through a stroke whose points crowd where it wobbles most.
fn reveal_at(stroke: &StrokeSpan, revealed: f32) -> (u32, f32) {
    if revealed >= 1.0 || stroke.total <= 0.0 {
        return (stroke.count, 0.0);
    }
    if revealed <= 0.0 {
        return (0, 0.0);
    }

    let target = revealed * stroke.total;
    let at = stroke
        .prefix
        .partition_point(|&length| length <= target)
        .max(1);
    if at as u32 >= stroke.count {
        return (stroke.count, 0.0);
    }

    let (from, to) = (stroke.prefix[at - 1], stroke.prefix[at]);
    let span = to - from;
    let t = match span > 0.0 {
        true => ((target - from) / span).clamp(0.0, 1.0),
        false => 0.0,
    };
    (at as u32, t)
}

/// The classic smoothstep, easing a fill in over the back half of the reveal so
/// the box fills behind the stroke rather than ahead of it.
fn smoothstep(from: f32, to: f32, at: f32) -> f32 {
    let t = ((at - from) / (to - from)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The pixel box every point of `geometry` falls inside.
fn geometry_bounds(geometry: &rough::Geometry) -> [f32; 4] {
    let mut bounds = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for point in geometry.strokes.iter().flat_map(|stroke| &stroke.points) {
        bounds[0] = bounds[0].min(point[0]);
        bounds[1] = bounds[1].min(point[1]);
        bounds[2] = bounds[2].max(point[0]);
        bounds[3] = bounds[3].max(point[1]);
    }
    match bounds[0] <= bounds[2] {
        true => bounds,
        false => [0.0; 4],
    }
}

fn bounds_of(corners: &[[f32; 2]; 4]) -> [f32; 4] {
    let mut bounds = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for corner in corners {
        bounds[0] = bounds[0].min(corner[0]);
        bounds[1] = bounds[1].min(corner[1]);
        bounds[2] = bounds[2].max(corner[0]);
        bounds[3] = bounds[3].max(corner[1]);
    }
    bounds
}

fn storage_entry(binding: u32) -> BindGroupLayoutEntry {
    BindGroupLayoutEntry {
        binding,
        visibility: ShaderStages::FRAGMENT,
        ty: BindingType::Buffer {
            ty: BufferBindingType::Storage { read_only: true },
            has_dynamic_offset: false,
            min_binding_size: None,
        },
        count: None,
    }
}

fn make_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    globals: &Buffer,
    occluders: &Buffer,
    points: &Buffer,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("sketch bind group"),
        layout,
        entries: &[
            BindGroupEntry {
                binding: 0,
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
            BindGroupEntry {
                binding: 2,
                resource: points.as_entire_binding(),
            },
        ],
    })
}

fn alloc_instances(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("sketch instances"),
        size: (capacity * size_of::<SketchInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn alloc_points(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("sketch points"),
        size: (capacity * size_of::<[f32; 2]>()) as u64,
        usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests;
