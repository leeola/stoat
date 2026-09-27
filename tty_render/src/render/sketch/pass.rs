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
    sketch::rough::{self, ComponentBox, Rounding},
    CellMetrics, GridVersion, HostRide, Occluder, OccluderBuffer, SketchReveal,
    GLOBALS_SLOT_STRIDE,
};
use bytemuck::{Pod, Zeroable};
use std::{mem, ops::Range};
use stoatty_protocol::command::{SketchFillStyle, SketchShape};
use stoatty_term::grid::{Grid, Sketch};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingResource, BindingType, BlendState,
    Buffer, BufferBinding, BufferBindingType, BufferDescriptor, BufferSize, BufferUsages,
    ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor, Queue,
    RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource,
    ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in instances, allocated up front. Grows by
/// doubling when a frame builds more.
const INITIAL_CAPACITY: usize = 64;

/// Point buffer capacity, in points, allocated up front. A single wobbling
/// ellipse runs to a few hundred, so this holds a handful of marks before the
/// first grow.
const INITIAL_POINTS: usize = 4096;

/// The instance kind that fills a convex quad with rounded corners.
const KIND_FILL: u32 = 1;

/// The instance kind that strokes the part of a run of spans inside one tile.
const KIND_STROKE_TILE: u32 = 2;

/// The edge of the square tiles a run of spans draws through, in physical
/// pixels.
///
/// A hollow mark's quad is mostly interior, and every fragment of it walks the
/// spans. Drawing only the tiles that meet the ink leaves the interior unshaded.
const STROKE_TILE: f32 = 32.0;

/// Pixels past the stroke's half width that a fragment still reads coverage in,
/// the same margin as `AA_MARGIN` in sketch.wgsl.
///
/// The tiles have to reach as far as the distance field ramps, or the rim of
/// the anti-aliased edge falls outside every tile.
const AA_MARGIN: f32 = 1.0;

/// Segments one span holds at most.
///
/// A fragment walks every segment of each span whose box reaches it, and a
/// ring or a bowed path is one stroke around the whole shape. Cutting a stroke
/// into chunks this long keeps the walk to the segments that pass nearby.
const CHUNK_SEGMENTS: u32 = 16;

/// The geometry every sketch list generates within.
///
/// The walkthrough's largest marks take 16 spans for the card and 12 for the
/// ring. A list budget of about 170 times its scene holds the point buffer to
/// 2 MiB and the span buffer to 512 KiB.
const BUDGET: Budget = Budget {
    mark_spans: 64,
    list_points: 262_144,
    list_spans: 16_384,
};

/// The per-mark instance data.
///
/// A mark's strokes resolve together in each fragment rather than one instance
/// per stroke, because the target blends each instance over the last: a base
/// pass and the overlay that doubles it run the same path a pixel apart, so two
/// instances composite twice along their whole length and read as a dark core
/// inside a paler halo. The reference strokes a whole path in one call for the
/// same reason. Resolving every stroke inside one fragment blends the union
/// once.
///
/// A run of strokes draws as one tile instance per [`STROKE_TILE`] square its
/// ink meets. Every tile names the whole run, and the tiles of a run never
/// overlap, so each pixel still blends the union once.
///
/// The strokes themselves ride [`SpanInstance`], which this names a run of,
/// so the fragment stage still skips a chunk of stroke whose box is nowhere
/// near it.
///
/// The points sit in a shared storage buffer the spans index, unlike a
/// polyline's, which ride inline. That pass binds one group across the live
/// grid and every composited pool. This one never draws on a pool, so a single
/// arena works and a stroke is free to run to any length.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
struct SketchInstance {
    /// The quad's box in physical pixels, as `[min_x, min_y, max_x, max_y]`.
    ///
    /// A fill's box holds its outer corners, and the vertex stage grows it by
    /// the reach. A stroke tile's box is the tile, which the vertex stage draws
    /// as it is.
    bounds: [f32; 4],
    /// Straight color and alpha, the alpha already carrying a fill's fade.
    color: [f32; 4],
    /// Half a stroke's width in pixels, or a fill's corner radius.
    ///
    /// A fill's point buffer holds its inset quad, and the shader grows that
    /// quad back out by this radius to round its corners.
    half_width: f32,
    _pad0: f32,
    /// Pixels this mark is shifted down by, for one riding a gliding pane.
    dy: f32,
    _pad1: f32,
    /// The first of this mark's entries in the span buffer.
    ///
    /// A fill has no span. It names the first of its four inset corners in the
    /// point buffer here instead, which is what its zero [`Self::span_count`]
    /// tells the fragment stage to read.
    span_first: u32,
    seq: u32,
    /// Revealed spans this mark carries, or 0 for a fill.
    span_count: u32,
    kind: u32,
}

/// One revealed chunk of a stroke, as the fragment stage reads it.
///
/// Held apart from [`SketchInstance`] rather than inline, because a mark draws
/// as one instance and carries however many chunks its strokes cut into. A
/// card runs to sixteen strokes.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Debug, Pod, Zeroable)]
struct SpanInstance {
    /// This chunk's own pixel box.
    ///
    /// Per chunk rather than per mark or per stroke, so a fragment runs the
    /// distance field of the few segments whose ink is near it instead of every
    /// stroke the mark carries.
    bounds: [f32; 4],
    point_offset: u32,
    /// Whole points of this chunk that are revealed.
    reveal_count: u32,
    /// How far along the segment after the revealed run the pen sits, so the
    /// stroke grows smoothly instead of snapping point to point.
    reveal_t: f32,
    _pad: u32,
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
    /// The stroke cut into chunks of at most [`CHUNK_SEGMENTS`] segments, in
    /// order along it.
    ///
    /// A revealed chunk is one span, so a fragment walks the few segments near
    /// it rather than the whole stroke, however long the stroke runs.
    chunks: Vec<Chunk>,
    /// Multiplier on the mark's stroke weight, and the color to draw in.
    ///
    /// A hatch line is half weight in the fill's own color, so one mark carries
    /// strokes of two kinds. `None` takes the mark's own color.
    weight: f32,
    color: Option<[u8; 4]>,
}

/// A stretch of one stroke's points, which is one span once the pen reaches it.
struct Chunk {
    /// The chunk's first and last point, as indices into the stroke, inclusive.
    /// Adjacent chunks share their boundary point, so no segment falls between
    /// two chunks.
    first: u32,
    last: u32,
    /// The pixel box of the chunk's points, which the vertex stage sizes its
    /// quad from.
    bounds: [f32; 4],
}

/// How much geometry one sketch list generates at most.
///
/// A frame carries up to 255 points a path and a list holds up to 4,096 marks,
/// so a flood of zigzag paths or elbows generates points and spans past the
/// device's buffer and storage binding limits, and the device's error handler
/// panics the render thread. A mark over `mark_spans` generates nothing, and
/// so does every mark from the first one that takes the list past
/// `list_points` or `list_spans`.
#[derive(Clone, Copy)]
struct Budget {
    mark_spans: usize,
    list_points: usize,
    list_spans: usize,
}

/// One sketch's generated geometry, as the frame reads it.
struct MarkGeometry {
    strokes: Vec<StrokeSpan>,
    /// Where a filled box's inset quad starts in the point buffer, the box's
    /// outer bounds, and the corner radius that grows the quad back out.
    fill: Option<(u32, [f32; 4], f32)>,
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
    /// The revealed strokes of every mark, end to end, which each instance
    /// names a run of. Rebuilt every frame, because the reveal moves every
    /// frame while the points behind it do not.
    spans: Buffer,
    spans_capacity: usize,
    /// The spans last uploaded, so an unchanged frame skips the write.
    last_spans: Vec<SpanInstance>,
    /// Where each frame's spans are built, before being compared against
    /// [`Self::last_spans`] and traded with it.
    built_spans: Vec<SpanInstance>,
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
    /// Runs of instances of marks riding a compositing pool, each with its
    /// host's scissor. Drawn after that pool's composite instead of with the
    /// rest, or the composite paints over them.
    riding: Vec<(Range<u32>, [u32; 4])>,
    occluders: OccluderBuffer,
    /// The uniform last written, so an unchanged frame skips that write too.
    last_globals: Option<Globals>,
    metrics: CellMetrics,
    /// Whether a mark dropped for the [`BUDGET`] has been reported, so a flood
    /// logs once rather than on every regeneration.
    warned_budget: bool,
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
                storage_entry(3),
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
        let spans = alloc_spans(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(
            device,
            &bind_group_layout,
            &globals,
            &occluders.buffer,
            &points,
            &spans,
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
                    // paired with the ride shift, then the span run with the
                    // seq and the kind.
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
            spans,
            spans_capacity: INITIAL_CAPACITY,
            last_spans: Vec::new(),
            built_spans: Vec::new(),
            last_instances: Vec::new(),
            built: Vec::new(),
            geometry: Vec::new(),
            last_generated: None,
            count: 0,
            riding: Vec::new(),
            occluders,
            last_globals: None,
            metrics,
            warned_budget: false,
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
    /// Upload the frame's uniform, occluders, generated points, and instances:
    /// one per fill, and one per tile a run of revealed strokes meets.
    ///
    /// `reveals` carries one entry per [`Grid::sketches`] entry, in order. A
    /// short slice leaves the marks past its end complete, at the style their
    /// commands declare, so a caller with no clock passes `&[]`.
    ///
    /// `anchored` names the pools compositing this frame, so a mark anchored to
    /// one is shifted and held back for [`Self::draw_riding`].
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        reveals: &[SketchReveal],
        anchored: &[HostRide],
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
            reveals,
            anchored,
            self.metrics,
            resolution,
            &mut self.built,
            &mut self.built_spans,
            &mut self.riding,
        );
        self.count = self.built.len() as u32;

        if self.built.is_empty() {
            return;
        }

        // The two buffers are compared apart, because a reveal step moves every
        // span while leaving the instance that names them alone.
        if crate::render::upload_needed(&self.built_spans, &self.last_spans) {
            if self.built_spans.len() > self.spans_capacity {
                self.spans_capacity = self.built_spans.len().next_power_of_two();
                self.spans = alloc_spans(device, self.spans_capacity);
                self.rebuild_bind_group(device);
            }
            queue.write_buffer(&self.spans, 0, bytemuck::cast_slice(&self.built_spans));
            mem::swap(&mut self.built_spans, &mut self.last_spans);
        }

        if crate::render::upload_needed(&self.built, &self.last_instances) {
            if self.built.len() > self.capacity {
                self.capacity = self.built.len().next_power_of_two();
                self.instances = alloc_instances(device, self.capacity);
            }
            queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&self.built));
            mem::swap(&mut self.built, &mut self.last_instances);
        }
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

        // A riding run is skipped here and drawn by [`Self::draw_riding`] after
        // the composites, so the base pass leaves a gap where it sits.
        let mut next = 0;
        for (run, _) in &self.riding {
            if run.start > next {
                render_pass.draw(0..6, next..run.start);
            }
            next = run.end;
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
        for (run, scissor) in &self.riding {
            let [x, y, w, h] = *scissor;
            if w == 0 || h == 0 {
                continue;
            }
            render_pass.set_scissor_rect(x, y, w, h);
            render_pass.draw(0..6, run.clone());
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

        let points = generate_marks(
            grid.sketches(),
            self.metrics,
            BUDGET,
            &mut self.geometry,
            &mut self.warned_budget,
        );

        if points.is_empty() {
            return;
        }
        if points.len() > self.points_capacity {
            self.points_capacity = points.len().next_power_of_two();
            self.points = alloc_points(device, self.points_capacity);
            self.rebuild_bind_group(device);
        }
        queue.write_buffer(&self.points, 0, bytemuck::cast_slice(&points));
    }

    fn upload_occluders(&mut self, device: &Device, queue: &Queue, occluders: &[Occluder]) {
        if self.occluders.upload(device, queue, occluders) {
            self.rebuild_bind_group(device);
        }
    }

    /// Rebind the group after one of its buffers was replaced by a larger one.
    ///
    /// A grown buffer is a new handle, so the old group points at the freed one.
    fn rebuild_bind_group(&mut self, device: &Device) {
        self.bind_group = make_bind_group(
            device,
            &self.bind_group_layout,
            &self.globals,
            &self.occluders.buffer,
            &self.points,
            &self.spans,
        );
    }
}

/// Build one span per revealed chunk, one tile instance per [`STROKE_TILE`]
/// square a run of those spans meets, and one instance per faded fill.
///
/// Runs every frame, because the reveal moves every frame while the geometry
/// behind it does not. A stroke the reveal has not reached contributes no span
/// at all, and a mark with no revealed stroke contributes no instance, rather
/// than an empty one the GPU still rasterizes. The tiles follow the revealed
/// chunks and this frame's weight, so a partial reveal shades only the tiles
/// its ink has reached.
///
/// A run of a mark's strokes draws through tiles that never overlap, so the
/// target blends their union once. Drawing each stroke as its own instance
/// composites the overlaps twice, which reads as a dark core inside a paler
/// halo at any alpha below opaque.
///
/// The reveal walks the mark's units in declaration order rather than advancing
/// every stroke at once. A mark whose strokes all grow together materializes;
/// one whose units follow each other reads as being drawn. See [`unit_length`]
/// for what a unit is.
///
/// `resolution` bounds the tiles to the ones on the target. `riding` collects
/// the runs of instances of marks anchored to a pool compositing this frame, so
/// [`SketchPass::draw`] skips them and [`SketchPass::draw_riding`] picks them
/// up after that pool's composite.
#[allow(clippy::too_many_arguments)]
fn build_instances(
    sketches: &[Sketch],
    geometry: &[MarkGeometry],
    reveals: &[SketchReveal],
    anchored: &[HostRide],
    metrics: CellMetrics,
    resolution: [f32; 2],
    built: &mut Vec<SketchInstance>,
    spans: &mut Vec<SpanInstance>,
    riding: &mut Vec<(Range<u32>, [u32; 4])>,
) {
    built.clear();
    spans.clear();
    riding.clear();
    let mut tiles = Vec::new();

    for (index, sketch) in sketches.iter().enumerate() {
        let Some(mark) = geometry.get(index) else {
            continue;
        };
        // Past the slice's end a mark is whole, at the style its command
        // declares, which is what a caller with no clock relies on.
        let style = &sketch.command.style;
        let reveal = reveals.get(index).copied().unwrap_or(SketchReveal {
            revealed: 1.0,
            width: f32::from(style.width),
            alpha: f32::from(style.alpha) / 255.0,
        });
        let revealed = reveal.revealed.clamp(0.0, 1.0);
        let ride = ride_shift(sketch, anchored, metrics);
        let dy = ride.map_or(0.0, |(dy, _)| dy);

        let mut push = |instance: SketchInstance| {
            if let Some((_, scissor)) = ride {
                let at = built.len() as u32;
                match riding.last_mut() {
                    Some((run, held)) if run.end == at && *held == scissor => run.end += 1,
                    _ => riding.push((at..at + 1, scissor)),
                }
            }
            built.push(instance);
        };

        // The tiles on the target, in the unshifted coordinates the chunk boxes
        // are in. A tile past the target draws nothing, and a chunk box far
        // larger than the target, such as a long diagonal line's, holds a great
        // many of them.
        let on_target = [
            0,
            tile_of(-dy),
            tile_of(resolution[0]),
            tile_of(resolution[1] - dy),
        ];

        if let Some((offset, quad_bounds, radius)) = mark.fill {
            let (color, alpha) = fill_style(&sketch.command.shape);
            // The fill eases in over the back half of the reveal, so the box
            // fills behind the stroke rather than ahead of it.
            let faded = smoothstep(0.5, 1.0, revealed);
            push(SketchInstance {
                bounds: quad_bounds,
                color: rgba(color, f32::from(alpha) / 255.0 * faded),
                half_width: radius,
                _pad0: 0.0,
                dy,
                _pad1: 0.0,
                span_first: offset,
                seq: sketch.seq,
                span_count: 0,
                kind: KIND_FILL,
            });
        }

        let half_width = rough::stroke_width(reveal.width, metrics) / 2.0;
        // The pen walks the mark one unit at a time, so a box draws around its
        // perimeter and an arrowhead follows its shaft.
        let target = revealed * mark.strokes.chunks(2).map(unit_length).sum::<f32>();
        let mut unit_start = 0.0;
        let mut groups: Vec<SpanGroup> = Vec::new();

        for unit in mark.strokes.chunks(2) {
            let unit_len = unit_length(unit);
            // A unit with no length is already whole, which is also what draws
            // a degenerate mark rather than leaving it blank forever.
            let local = match unit_len > 0.0 {
                true => ((target - unit_start) / unit_len).clamp(0.0, 1.0),
                false => 1.0,
            };
            unit_start += unit_len;

            for stroke in unit {
                let (reveal_count, reveal_t) = reveal_at(stroke, local);
                if reveal_count < 2 && reveal_t <= 0.0 {
                    continue;
                }

                // The pen stands on this point of the stroke, reveal_t along the
                // segment after it.
                let pen = reveal_count - 1;
                let reached = stroke
                    .chunks
                    .iter()
                    .map_while(|chunk| Some((chunk, chunk_reveal(chunk, pen, reveal_t)?)))
                    .filter(|&(_, (count, t))| count >= 2 || t > 0.0);
                for (chunk, (count, t)) in reached {
                    // A mark's hatch and its outline differ in weight and color,
                    // so each run of one kind takes its own tiles. An unhatched
                    // mark has a single run.
                    match groups.last_mut() {
                        Some(last)
                            if (last.weight, last.color) == (stroke.weight, stroke.color) =>
                        {
                            last.count += 1;
                        },
                        _ => groups.push(SpanGroup {
                            weight: stroke.weight,
                            color: stroke.color,
                            first: spans.len() as u32,
                            count: 1,
                        }),
                    }
                    spans.push(SpanInstance {
                        bounds: chunk.bounds,
                        point_offset: stroke.point_offset + chunk.first,
                        reveal_count: count,
                        reveal_t: t,
                        _pad: 0,
                    });
                }
            }
        }

        for group in groups {
            let color = match group.color {
                Some([r, g, b, a]) => rgba([r, g, b], f32::from(a) / 255.0),
                None => rgba(style.color, reveal.alpha),
            };
            let half_width = half_width * group.weight;
            let run = &spans[group.first as usize..(group.first + group.count) as usize];
            stroke_tiles(run, half_width + AA_MARGIN, on_target, &mut tiles);

            for &(row, col) in &tiles {
                let (x, y) = (col as f32 * STROKE_TILE, row as f32 * STROKE_TILE);
                push(SketchInstance {
                    bounds: [x, y, x + STROKE_TILE, y + STROKE_TILE],
                    color,
                    half_width,
                    _pad0: 0.0,
                    dy,
                    _pad1: 0.0,
                    span_first: group.first,
                    seq: sketch.seq,
                    span_count: group.count,
                    kind: KIND_STROKE_TILE,
                });
            }
        }
    }
}

/// A run of a mark's spans sharing one weight and color, which is one set of
/// tiles.
struct SpanGroup {
    weight: f32,
    color: Option<[u8; 4]>,
    first: u32,
    count: u32,
}

/// Collect into `tiles`, sorted and each once, the `(row, column)` of every
/// [`STROKE_TILE`] tile that meets one of `run`'s chunk boxes grown by `reach`,
/// within the `[min_col, min_row, max_col, max_row]` tiles `on_target`.
///
/// A pixel belongs to the tile its center falls in, so the tiles from the one
/// holding a grown box's low edge to the one holding its high edge hold every
/// pixel the box reaches.
fn stroke_tiles(
    run: &[SpanInstance],
    reach: f32,
    on_target: [i32; 4],
    tiles: &mut Vec<(i32, i32)>,
) {
    tiles.clear();
    let [min_col, min_row, max_col, max_row] = on_target;
    for span in run {
        let [x0, y0, x1, y1] = span.bounds;
        let cols = tile_of(x0 - reach).max(min_col)..=tile_of(x1 + reach).min(max_col);
        for row in tile_of(y0 - reach).max(min_row)..=tile_of(y1 + reach).min(max_row) {
            tiles.extend(cols.clone().map(|col| (row, col)));
        }
    }
    tiles.sort_unstable();
    tiles.dedup();
}

/// The index of the [`STROKE_TILE`] tile holding the coordinate `at`.
fn tile_of(at: f32) -> i32 {
    (at / STROKE_TILE).floor() as i32
}

/// How far the pen travels through one unit of a mark.
///
/// A unit is a base stroke and the overlay that doubles it, which
/// [`rough::Geometry::strokes`] holds adjacent. The two run the same path at
/// slightly different wobbles, so the longer of them is the distance the unit
/// takes and both reach their ends together.
fn unit_length(unit: &[StrokeSpan]) -> f32 {
    unit.iter().map(|stroke| stroke.total).fold(0.0, f32::max)
}

/// A protocol color with an already-scaled alpha, as the straight float the
/// shader blends with.
fn rgba(color: [u8; 3], alpha: f32) -> [f32; 4] {
    [
        f32::from(color[0]) / 255.0,
        f32::from(color[1]) / 255.0,
        f32::from(color[2]) / 255.0,
        alpha,
    ]
}

/// Generate every mark's geometry into `out`, returning the points they share.
///
/// The points of every stroke and every fill land end to end in one buffer, and
/// each record names its own span, because the pass binds a single arena that
/// every instance indexes.
///
/// A mark past the `budget` gets empty geometry, which keeps `out` aligned
/// with `sketches` and draws nothing. The first such mark is reported through
/// `warned`, once.
fn generate_marks(
    sketches: &[Sketch],
    metrics: CellMetrics,
    budget: Budget,
    out: &mut Vec<MarkGeometry>,
    warned: &mut bool,
) -> Vec<[f32; 2]> {
    out.clear();
    let mut points: Vec<[f32; 2]> = Vec::new();
    let mut spans = 0;
    let mut list_full = false;

    for sketch in sketches {
        let empty = MarkGeometry {
            strokes: Vec::new(),
            fill: None,
        };
        if list_full {
            out.push(empty);
            continue;
        }

        let resolve = |id: u32| component_bounds(sketches, id, metrics);
        let generated = rough::geometry(&sketch.command, metrics, &resolve);
        let chunks: Vec<Vec<Chunk>> = generated
            .strokes
            .iter()
            .map(|stroke| stroke_chunks(&stroke.points))
            .collect();
        let inset = generated
            .fill
            .map(|fill| rough::inset_quad(fill.corners, fill.radius));

        let mark_spans: usize = chunks.iter().map(Vec::len).sum();
        let mark_points = generated
            .strokes
            .iter()
            .map(|stroke| stroke.points.len())
            .sum::<usize>()
            + inset.map_or(0, |quad| quad.len());

        // A mark over its own cap is dropped alone. One that fills the list ends
        // generation, since the newest marks are the ones a flood adds. Either
        // is dropped whole rather than cut short, because part of a drawing
        // points at the wrong thing, the same reason a connector to an unknown
        // mark draws nothing.
        let over_mark = mark_spans > budget.mark_spans;
        let over_list = points.len() + mark_points > budget.list_points
            || spans + mark_spans > budget.list_spans;
        list_full = !over_mark && over_list;
        if over_mark || list_full {
            warn_over_budget(budget, warned);
            out.push(empty);
            continue;
        }
        spans += mark_spans;

        let mut strokes = Vec::with_capacity(generated.strokes.len());
        for (stroke, chunks) in generated.strokes.iter().zip(chunks) {
            let point_offset = points.len() as u32;
            points.extend_from_slice(&stroke.points);
            strokes.push(StrokeSpan {
                point_offset,
                count: stroke.points.len() as u32,
                total: stroke.lengths.last().copied().unwrap_or(0.0),
                prefix: stroke.lengths.clone(),
                chunks,
                weight: stroke.weight,
                color: stroke.color,
            });
        }

        let fill = generated.fill.zip(inset).map(|(fill, quad)| {
            let at = points.len() as u32;
            points.extend_from_slice(&quad);
            // The rounded shape reaches back out to the outer corners, so they
            // bound it rather than the inset quad the buffer holds.
            (at, points_bounds(&fill.corners), fill.radius)
        });

        out.push(MarkGeometry { strokes, fill });
    }

    points
}

/// Report the first mark dropped for `budget`, the first time `warned` sees
/// one.
fn warn_over_budget(budget: Budget, warned: &mut bool) {
    if *warned {
        return;
    }
    *warned = true;
    tracing::warn!(
        mark_spans = budget.mark_spans,
        list_points = budget.list_points,
        list_spans = budget.list_spans,
        "sketch geometry over its budget, dropping marks",
    );
}

/// Cut a stroke's points into chunks of at most [`CHUNK_SEGMENTS`] segments.
///
/// A stroke of fewer than two points has no segment, and so no chunk.
fn stroke_chunks(points: &[[f32; 2]]) -> Vec<Chunk> {
    let last_point = points.len().saturating_sub(1);
    (0..last_point)
        .step_by(CHUNK_SEGMENTS as usize)
        .map(|first| {
            let last = (first + CHUNK_SEGMENTS as usize).min(last_point);
            Chunk {
                first: first as u32,
                last: last as u32,
                bounds: points_bounds(&points[first..=last]),
            }
        })
        .collect()
}

/// Where a mark rides, when its anchor names a pool compositing this frame.
fn ride_shift(
    sketch: &Sketch,
    anchored: &[HostRide],
    metrics: CellMetrics,
) -> Option<(f32, [u32; 4])> {
    let (host, top_rows) = sketch.command.anchor?;
    let ride = anchored.iter().find(|ride| ride.host == host)?;
    Some((ride.shift_px(top_rows, metrics.height), ride.scissor))
}

/// The pixel box of the mark `id` names, for a connector pointing at it.
///
/// A connector names the decoration it points at rather than a coordinate, so
/// it tracks that thing as it moves. Which side of the box the line meets is
/// decided by the geometry generator, which sees both ends; this only says
/// where the box is. An unknown id yields `None`, which drops the connector,
/// because a line to nowhere points at the wrong thing.
fn component_bounds(sketches: &[Sketch], id: u32, metrics: CellMetrics) -> Option<ComponentBox> {
    let target = sketches.iter().find(|sketch| sketch.command.id == id)?;
    shape_bounds(&target.command.shape, metrics)
}

/// A boxed shape's pixel rectangle and how its outline rounds inside it, or
/// `None` for a connector, which has no box of its own to point at.
fn shape_bounds(shape: &SketchShape, metrics: CellMetrics) -> Option<ComponentBox> {
    let (cw, ch) = (metrics.width, metrics.height);
    let (bounds, rounding) = match shape {
        SketchShape::Ellipse { bounds, .. } => (bounds, Rounding::Ellipse),
        SketchShape::Rect { bounds, radius, .. } => (
            bounds,
            Rounding::Rect {
                radius_px: f32::from(*radius) / 16.0 * cw,
            },
        ),
        SketchShape::Line { .. } | SketchShape::Path { .. } | SketchShape::Elbow { .. } => {
            return None;
        },
    };
    let x = f32::from(bounds.x) / 16.0 * cw;
    let y = f32::from(bounds.y) / 16.0 * ch;
    Some(ComponentBox {
        bounds: [
            x,
            y,
            x + f32::from(bounds.w) / 16.0 * cw,
            y + f32::from(bounds.h) / 16.0 * ch,
        ],
        rounding,
    })
}

/// The color and alpha a filled box paints with, or an invisible black for an
/// open one, whose instance is never built.
fn fill_style(shape: &SketchShape) -> ([u8; 3], u8) {
    match shape {
        SketchShape::Rect {
            fill: Some(fill), ..
        } => match fill.style {
            SketchFillStyle::Solid => (fill.color, fill.alpha),
            // A hatched fill is drawn as strokes, so it builds no fill instance.
            SketchFillStyle::Hachure | SketchFillStyle::CrossHatch => ([0; 3], 0),
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

/// How much of `chunk` the pen has drawn, standing on the stroke's point `pen`
/// with the tip `reveal_t` along the segment after it.
///
/// A chunk behind the pen is whole. The chunk the pen stands in carries the
/// tip. A chunk ahead of the pen answers `None`, and so does every chunk after
/// it.
fn chunk_reveal(chunk: &Chunk, pen: u32, reveal_t: f32) -> Option<(u32, f32)> {
    if chunk.last <= pen {
        return Some((chunk.last - chunk.first + 1, 0.0));
    }
    if chunk.first <= pen {
        return Some((pen - chunk.first + 1, reveal_t));
    }
    None
}

/// The classic smoothstep, easing a fill in over the back half of the reveal so
/// the box fills behind the stroke rather than ahead of it.
fn smoothstep(from: f32, to: f32, at: f32) -> f32 {
    let t = ((at - from) / (to - from)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The pixel box every point in `points` falls inside.
///
/// An empty run yields a zero box rather than the inverted one the fold starts
/// from. An inverted box sizes a quad the rasterizer drops, which is right, but
/// it reads as a bug wherever it surfaces.
fn points_bounds(points: &[[f32; 2]]) -> [f32; 4] {
    let mut bounds = [f32::MAX, f32::MAX, f32::MIN, f32::MIN];
    for point in points {
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
    spans: &Buffer,
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
            BindGroupEntry {
                binding: 3,
                resource: spans.as_entire_binding(),
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

fn alloc_spans(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("sketch spans"),
        size: (capacity * size_of::<SpanInstance>()) as u64,
        usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests;
