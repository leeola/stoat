//! Instanced minimap pass.
//!
//! Renders each declared [`Minimap`] strip as a background quad, one sub-pixel
//! quad per run block of the visible line slice, and a viewport thumb. Unlike the
//! bar pass, quads ride in absolute pixels rather than cell-fraction units, since
//! a minimap column is a fraction of a pixel.
//!
//! Cost is bounded by the visible strip slice. Only the lines under the strip are
//! walked, so a large file is no more work than a small one. The pure layout math
//! ([`minimap_top`], [`thumb_geometry`], [`build_strip`]) is unit-tested without a
//! GPU.

use crate::render::{CellMetrics, GridVersion, Occluder};
use bytemuck::{Pod, Zeroable};
use stoatty_term::grid::{Grid, LineSummary, Minimap, MinimapStrip, MinimapView, Rgb, Rgba};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, Buffer,
    BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites, Device,
    FragmentState, PipelineLayoutDescriptor, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, TextureFormat,
    VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in quads, allocated up front. Grows by doubling.
const INITIAL_CAPACITY: usize = 256;

/// Minimum viewport-thumb height in pixels, so the thumb stays grabbable on a
/// large file where the proportional height would collapse to a sliver.
const MIN_THUMB_PX: f32 = 12.0;

/// A run quad's height as a fraction of the line height, leaving a hairline gap
/// between lines so the run blocks read as distinct rows.
const RUN_HEIGHT_RATIO: f32 = 0.75;

/// The per-quad instance data. It carries an absolute-pixel rectangle, an rgba
/// fill, and the strip's declaration-order seq the fragment shader occludes by.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct MinimapInstance {
    origin: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
    seq: u32,
}

/// The uniform shared by every instance, matching the bar pass layout so the
/// occluder test maps a panel's cell rect to pixels the same way.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    panel_count: u32,
    occlude_all: u32,
    _pad: [u32; 2],
}

/// A strip's pixel geometry, derived fresh from its declaration and the live cell
/// metrics.
struct StripLayout {
    strip_x: f32,
    strip_y: f32,
    strip_w: f32,
    strip_h: f32,
    line_h: f32,
    col_w: f32,
    /// How many minimap lines fit the strip height, the window the thumb rides in
    /// and the slice of the file the strip renders.
    visible_lines: f32,
}

/// One strip's draw: its scissor rect in pixels and the instance range to draw.
struct StripDraw {
    scissor: [u32; 4],
    start: u32,
    count: u32,
}

/// One strip's built instances, beside what they were built from.
///
/// A frame rebuilds a strip only when [`Self::declared`] or
/// [`Self::content_version`] moved. Holding the declaration itself rather than a
/// digest of it means a field added to a strip is compared without anyone
/// remembering to add it here.
struct StripState {
    declared: Minimap,
    /// The store's write count when [`Self::instances`] was built. The slice's
    /// address and length answer nothing, since a one-line edit splices the same
    /// count into the same allocation.
    content_version: u64,
    scissor: [u32; 4],
    instances: Vec<MinimapInstance>,
}

/// The instanced minimap pipeline and its per-frame buffers.
pub struct MinimapPass {
    pipeline: RenderPipeline,
    bind_group_layout: BindGroupLayout,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    strips: Vec<StripDraw>,
    /// What each strip was built from and the instances it built, so a frame
    /// that moved one strip leaves the rest alone.
    ///
    /// Indexed alongside [`Self::strips`], in the grid's declaration order.
    built: Vec<StripState>,
    /// Where a rebuild gathers every strip's instances before the single upload,
    /// reused so a rebuild does not open a fresh list and discard it.
    instance_scratch: Vec<MinimapInstance>,
    occluders: Buffer,
    /// The occluder list last written to [`Self::occluders`], so a frame whose
    /// panels have not moved skips the upload. Panels change on layout events, not
    /// per frame, so most frames match.
    last_occluders: Vec<Occluder>,
    occluder_capacity: usize,
    /// The uniform last written, so an unchanged frame skips that write too.
    last_globals: Option<Globals>,
    metrics: CellMetrics,
    /// The grid, its minimap epoch, and the resolution the current
    /// [`Self::strips`] and instance buffer were built against. While all hold,
    /// the strips are unchanged, so the rebuild and upload are skipped. `None`
    /// forces a rebuild, set at construction and whenever the cell metrics
    /// change.
    last_build: Option<(GridVersion, [f32; 2])>,
    /// How many strips this pass has built since it was made.
    ///
    /// A build is a pure function of its inputs, so a kept strip's instances and
    /// the ones a rebuild would produce are the same bytes. Only a count tells
    /// the two apart, and telling them apart is the whole claim the per-strip
    /// cache makes.
    #[cfg(test)]
    builds: usize,
    /// How many times this pass has re-packed and sent the instance buffer,
    /// counted for the same reason [`Self::builds`] is.
    #[cfg(test)]
    uploads: usize,
}

impl MinimapPass {
    /// Build the pipeline targeting `format`, with empty buffers.
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> MinimapPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("minimap"),
            source: ShaderSource::Wgsl(
                crate::render::with_occlusion(include_str!("../shaders/minimap.wgsl")).into(),
            ),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("minimap globals"),
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
            label: Some("minimap"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("minimap"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<MinimapInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x4,
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
            label: Some("minimap globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let occluders = alloc_occluders(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &occluders);
        let instances = alloc_instances(device, INITIAL_CAPACITY);

        MinimapPass {
            pipeline,
            bind_group_layout,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            strips: Vec::new(),
            built: Vec::new(),
            occluders,
            last_occluders: Vec::new(),
            last_globals: None,
            instance_scratch: Vec::new(),
            occluder_capacity: INITIAL_CAPACITY,
            metrics,
            last_build: None,
            #[cfg(test)]
            builds: 0,
            #[cfg(test)]
            uploads: 0,
        }
    }

    /// Replace the cell metrics so the next frame lays strips out at the new size.
    ///
    /// Invalidates the rebuild cache, since the strip layout is derived from the
    /// metrics and every cached instance now sits at the wrong pixel size.
    ///
    /// The draws packed from that cache go with it. They are the same state one
    /// stage on, and the repack that would replace them runs only on a frame
    /// with a strip to rebuild, so a grid that declares none would otherwise
    /// leave them standing at the size the change just left behind.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
        self.last_build = None;
        self.built.clear();
        self.strips.clear();
    }

    /// Upload the frame's uniform, panel occluders, and one instance per strip
    /// background, run block, and thumb.
    ///
    /// Reads the grid's declared strips and their content stores, walking only
    /// each strip's visible line slice. Reallocates a buffer only when its count
    /// outgrows the current capacity.
    ///
    /// `occluders` are the live panels' rects, built once per frame and shared
    /// with the other passes that occlude.
    ///
    /// The panel occluders and the globals uniform are rewritten every frame,
    /// since panels move independently of the strips. The strip instances rebuild
    /// only when the resolution changed, or the grid did, or its minimap epoch
    /// moved. The epoch bumps whenever the projection re-applies the strip list
    /// or their content ([`Grid::minimap_epoch`]), so an unchanged epoch on the
    /// same grid means the strips would build byte-for-byte identically, and the
    /// reused buffer still holds.
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        occluders: &[Occluder],
        resolution: [f32; 2],
    ) {
        // With no strip to draw now and none drawn last frame, nothing reads this
        // pass's buffers, so the frame skips it without touching the GPU. The frame
        // that empties the list still runs, which is what clears the strips and stops
        // the draw. Leaving `last_build` unstamped is safe because a strip appearing
        // bumps the minimap epoch, so the rebuild below cannot be skipped for it.
        if grid.minimaps().is_empty() && self.strips.is_empty() {
            return;
        }

        self.upload_occluders(device, queue, occluders);

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            panel_count: occluders.len() as u32,
            occlude_all: 0,
            _pad: [0; 2],
        };
        crate::render::upload_globals(queue, &self.globals, 0, globals, &mut self.last_globals);

        let built_from = GridVersion::new(grid, grid.minimap_epoch());
        if self.last_build == Some((built_from, resolution)) {
            return;
        }

        // A cached strip's content version is its own grid's write count, so it
        // says nothing about the store another grid keeps under the same content
        // id. Nothing per-strip carries over when the grid does not.
        let grid_changed = !self
            .last_build
            .is_some_and(|(last, _)| last.same_grid(built_from));
        self.last_build = Some((built_from, resolution));

        // The epoch moved, but it moves for any minimap change anywhere. Which
        // strips it moved for is what decides how much of this frame costs
        // anything: with a strip per pane, one thumb drag would otherwise
        // rebuild every pane on screen.
        let declared = grid.minimaps();
        let mut rebuilt = self.built.len() != declared.len();
        self.built.truncate(declared.len());
        for (index, strip) in declared.iter().enumerate() {
            let version = grid.minimap_content_version(strip.strip.content_id);
            if !grid_changed
                && let Some(state) = self.built.get(index)
                && state.declared == *strip
                && state.content_version == version
            {
                continue;
            }

            let content = grid.minimap_content(strip.strip.content_id);
            let (instances, rect) = build_strip(strip, content, self.metrics);
            #[cfg(test)]
            {
                self.builds += 1;
            }
            let state = StripState {
                declared: strip.clone(),
                content_version: version,
                scissor: clamp_scissor(rect, resolution),
                instances,
            };
            match self.built.get_mut(index) {
                Some(slot) => *slot = state,
                None => self.built.push(state),
            }
            rebuilt = true;
        }

        if !rebuilt {
            return;
        }

        // A rebuilt strip whose instance count moved shifts every later strip's
        // range, so the buffer is re-packed and sent once rather than patched
        // per strip.
        #[cfg(test)]
        {
            self.uploads += 1;
        }
        let instances = &mut self.instance_scratch;
        instances.clear();
        self.strips.clear();
        for state in &self.built {
            if state.instances.is_empty() {
                continue;
            }
            self.strips.push(StripDraw {
                scissor: state.scissor,
                start: instances.len() as u32,
                count: state.instances.len() as u32,
            });
            instances.extend_from_slice(&state.instances);
        }

        if instances.is_empty() {
            return;
        }
        if instances.len() > self.capacity {
            self.capacity = instances.len().next_power_of_two();
            self.instances = alloc_instances(device, self.capacity);
        }
        queue.write_buffer(
            &self.instances,
            0,
            bytemuck::cast_slice(instances.as_slice()),
        );
    }

    /// Upload the panel occluders, reallocating and rebuilding the bind group when
    /// the panel count outgrows the current capacity.
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

    /// Record the minimap draw into `render_pass`, one scissored instanced draw
    /// per strip.
    ///
    /// A no-op when no strip is declared. Each strip scissors to its pixel rect so
    /// a run cannot bleed past the strip. The caller restores the full scissor
    /// afterward. Run after the bar pass and before the cursor.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.strips.is_empty() {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        for strip in &self.strips {
            let [x, y, w, h] = strip.scissor;
            if w == 0 || h == 0 {
                continue;
            }
            render_pass.set_scissor_rect(x, y, w, h);
            render_pass.draw(0..6, strip.start..strip.start + strip.count);
        }
    }
}

fn alloc_instances(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("minimap instances"),
        size: (capacity * size_of::<MinimapInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn alloc_occluders(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("minimap occluders"),
        size: (capacity * size_of::<Occluder>()) as u64,
        usage: BufferUsages::STORAGE | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

fn make_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    globals: &Buffer,
    occluders: &Buffer,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("minimap globals"),
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

/// The pixel geometry of `strip` under `metrics`.
fn strip_layout(strip: &MinimapStrip, metrics: CellMetrics) -> StripLayout {
    let strip_w = strip.width as f32 * metrics.width;
    let strip_h = strip.height as f32 * metrics.height;
    let line_h = metrics.height / strip.lines_per_cell.max(1) as f32;
    StripLayout {
        strip_x: strip.left as f32 * metrics.width,
        strip_y: strip.top as f32 * metrics.height,
        strip_w,
        strip_h,
        line_h,
        col_w: strip_w / strip.max_columns.max(1) as f32,
        visible_lines: strip_h / line_h,
    }
}

/// The first minimap line drawn at the strip top, in fractional lines.
///
/// Zero when the file fits the strip. Otherwise the viewport's proportional
/// position (top over the scrollable span `total - view_visible`, clamped to
/// `[0, 1]`) scaled across the minimap's own scrollable span `total - visible`,
/// so the whole file maps onto the strip and the visible window rides within it.
fn minimap_top(total: f32, visible_lines: f32, view_top: f32, view_visible: f32) -> f32 {
    if total <= visible_lines {
        return 0.0;
    }
    let scrollable = total - view_visible;
    let ratio = if scrollable > 0.0 {
        (view_top / scrollable).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ratio * (total - visible_lines)
}

/// The viewport thumb's top offset from the strip top and its height, in pixels.
///
/// The height floors at [`MIN_THUMB_PX`] so the thumb stays visible on a large
/// file where its proportional height would collapse.
///
/// That floor is what makes the offset clamp necessary. A purely proportional
/// thumb already ends flush with the strip at max scroll, since the offset gives
/// back exactly the pixels the height takes. Substituting the taller floored
/// height breaks that identity and pushes the thumb's bottom past `strip_h`,
/// where the strip's scissor crops it. Clamping against the floored height keeps
/// the whole thumb on the strip and leaves the proportional case untouched.
fn thumb_geometry(
    view_top: f32,
    top: f32,
    view_visible: f32,
    line_h: f32,
    strip_h: f32,
) -> (f32, f32) {
    let height = (view_visible * line_h).max(MIN_THUMB_PX);
    let offset = ((view_top - top) * line_h).min(strip_h - height).max(0.0);
    (offset, height)
}

/// One strip's instances (background, run blocks, thumb) and its pixel scissor
/// rect `[x, y, w, h]`.
///
/// Walks only the visible line slice `[top, top + visible_lines)`, so the cost is
/// bounded by the strip height, never the file size. A line index past the
/// content is skipped. A run whose class is outside the palette is skipped.
fn build_strip(
    strip: &Minimap,
    content: &[LineSummary],
    metrics: CellMetrics,
) -> (Vec<MinimapInstance>, [f32; 4]) {
    let layout = strip_layout(&strip.strip, metrics);
    let seq = strip.seq;
    let rect = [
        layout.strip_x,
        layout.strip_y,
        layout.strip_w,
        layout.strip_h,
    ];

    let mut instances = vec![MinimapInstance {
        origin: [layout.strip_x, layout.strip_y],
        size: [layout.strip_w, layout.strip_h],
        color: rgba_f32(strip.strip.bg),
        seq,
    }];

    let total = content.len() as f32;
    let (view_top, view_visible) = match strip.view {
        Some(MinimapView { top_256, visible }) => (top_256 as f32 / 256.0, visible as f32),
        None => (0.0, layout.visible_lines),
    };
    let top = minimap_top(total, layout.visible_lines, view_top, view_visible);

    let strip_right = layout.strip_x + layout.strip_w;
    let last = ((top + layout.visible_lines).ceil() as usize).min(content.len());
    let first = (top.max(0.0) as usize).min(last);
    for (line, runs) in (first..last).zip(&content[first..last]) {
        let y = layout.strip_y + (line as f32 - top) * layout.line_h;
        for run in runs.iter() {
            let Some(color) = strip
                .strip
                .palette
                .get(run.class as usize)
                .or_else(|| strip.strip.palette.first())
            else {
                continue;
            };
            let x = layout.strip_x + run.start_col as f32 * layout.col_w;
            let width = (run.len as f32 * layout.col_w)
                .min(strip_right - x)
                .max(0.0);
            if width == 0.0 {
                continue;
            }
            instances.push(MinimapInstance {
                origin: [x, y],
                size: [width, layout.line_h * RUN_HEIGHT_RATIO],
                color: rgb_opaque_f32(*color),
                seq,
            });
        }
    }

    let (thumb_offset, thumb_height) =
        thumb_geometry(view_top, top, view_visible, layout.line_h, layout.strip_h);
    instances.push(MinimapInstance {
        origin: [layout.strip_x, layout.strip_y + thumb_offset],
        size: [layout.strip_w, thumb_height],
        color: rgba_f32(strip.strip.thumb),
        seq,
    });

    (instances, rect)
}

/// Convert a strip's pixel rect to an integer scissor clamped to the surface, so
/// wgpu never rejects a rect that spills past the attachment edge.
fn clamp_scissor(rect: [f32; 4], resolution: [f32; 2]) -> [u32; 4] {
    let x = rect[0].max(0.0).min(resolution[0]);
    let y = rect[1].max(0.0).min(resolution[1]);
    let w = (rect[0] + rect[2]).min(resolution[0]) - x;
    let h = (rect[1] + rect[3]).min(resolution[1]) - y;
    [x as u32, y as u32, w.max(0.0) as u32, h.max(0.0) as u32]
}

fn rgba_f32(color: Rgba) -> [f32; 4] {
    [
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
        color.a as f32 / 255.0,
    ]
}

fn rgb_opaque_f32(color: Rgb) -> [f32; 4] {
    [
        color.r as f32 / 255.0,
        color.g as f32 / 255.0,
        color.b as f32 / 255.0,
        1.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::{build_strip, minimap_top, thumb_geometry, MinimapPass, MIN_THUMB_PX};
    use crate::{
        gpu::headless_device,
        render::{panel_occluder, CellMetrics, Occluder},
    };
    use std::collections::HashMap;
    use stoatty_term::grid::{
        BorderStyle, Grid, LineSummary, Minimap, MinimapRun, MinimapStrip, MinimapView, Panel,
        PanelShadow, Rgb, Rgba,
    };
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
    /// row exactly the 256-byte copy alignment, so the readback needs no
    /// stride padding.
    const TARGET: u32 = 64;

    /// The shared summaries a content store holds, for stating a fixture.
    fn summaries(lines: Vec<Vec<MinimapRun>>) -> Vec<LineSummary> {
        lines.into_iter().map(Into::into).collect()
    }

    fn metrics() -> CellMetrics {
        // width 6, height 12: a minimap line at lines_per_cell 8 is 1.5px tall.
        CellMetrics {
            font_size: 10.0,
            width: 6.0,
            height: 12.0,
            scale_factor: 1.0,
        }
    }

    fn command() -> MinimapStrip {
        MinimapStrip {
            top: 0,
            left: 10,
            width: 8,
            height: 10,
            strip_id: 1,
            content_id: 1,
            lines_per_cell: 8,
            max_columns: 120,
            bg: Rgba::new(0, 0, 0, 0),
            thumb: Rgba::new(200, 200, 200, 48),
            thumb_border: Rgb::new(255, 255, 255),
            palette: vec![
                Rgb::new(10, 20, 30),
                Rgb::new(40, 50, 60),
                Rgb::new(70, 80, 90),
            ],
        }
    }

    fn strip(view: Option<MinimapView>) -> Minimap {
        Minimap {
            strip: command(),
            seq: 3,
            view,
        }
    }

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_occlusion(include_str!(
            "../shaders/minimap.wgsl"
        )))
        .expect("parse minimap");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate minimap");
    }

    #[test]
    fn minimap_top_is_zero_when_file_fits() {
        // 40 lines fit a 120-line strip window, so there is nothing to scroll.
        assert_eq!(minimap_top(40.0, 120.0, 0.0, 30.0), 0.0);
    }

    #[test]
    fn minimap_top_scales_the_viewport_position_across_the_strip() {
        // Halfway down a 200-line file (top 85 over the 170-line scrollable span)
        // lands halfway across the 120-line minimap scrollable span (200 - 80).
        let top = minimap_top(200.0, 80.0, 85.0, 30.0);
        assert!((top - 60.0).abs() < 1e-4, "got {top}");
    }

    #[test]
    fn minimap_top_clamps_past_the_end() {
        // A view_top past the last page clamps to the bottom of the strip span.
        assert_eq!(minimap_top(200.0, 80.0, 1_000.0, 30.0), 120.0);
    }

    #[test]
    fn thumb_height_floors_at_the_minimum() {
        // The strip is 10 cells of 12px, so 120px tall.
        const STRIP_H: f32 = 120.0;

        // A one-line viewport at 1.5px per line would be a sliver, so it floors.
        let (_, height) = thumb_geometry(0.0, 0.0, 1.0, 1.5, STRIP_H);
        assert_eq!(height, MIN_THUMB_PX);

        let (offset, height) = thumb_geometry(20.0, 10.0, 40.0, 1.5, STRIP_H);
        assert_eq!(offset, 15.0, "thumb offset is (view_top - top) * line_h");
        assert_eq!(
            height, 60.0,
            "a tall viewport keeps its proportional height"
        );

        // A 5-line viewport over a 10000-line file scrolled to the bottom has a
        // 7.5px proportional thumb, which floors to 12. Its unclamped offset of
        // 112.5 would hang 4.5px below the strip.
        let top = minimap_top(10_000.0, 80.0, 9_995.0, 5.0);
        let (offset, height) = thumb_geometry(9_995.0, top, 5.0, 1.5, STRIP_H);
        assert_eq!(
            (offset, height, offset + height),
            (108.0, MIN_THUMB_PX, STRIP_H),
            "a bottom-scrolled floored thumb ends flush with the strip"
        );
    }

    #[test]
    fn build_strip_pins_run_geometry_from_the_palette() {
        let content = vec![vec![
            MinimapRun {
                start_col: 0,
                len: 4,
                class: 1,
            },
            MinimapRun {
                start_col: 6,
                len: 2,
                class: 2,
            },
        ]];
        let (instances, rect) = build_strip(&strip(None), &summaries(content), metrics());

        // strip: left 10 * width 6 = x 60; width 8 * 6 = 48; col_w = 48 / 120 = 0.4;
        // height 10 * 12 = 120.
        assert_eq!(rect, [60.0, 0.0, 48.0, 120.0]);

        // Background first, then the two runs, then the thumb.
        assert_eq!(instances.len(), 4);
        let first_run = instances[1];
        assert_eq!(first_run.origin, [60.0, 0.0], "class-1 run at start_col 0");
        assert_eq!(first_run.size[0], 4.0 * 0.4, "width is len * col_w");
        assert_eq!(
            first_run.color,
            [40.0 / 255.0, 50.0 / 255.0, 60.0 / 255.0, 1.0],
            "class 1 indexes the palette, opaque",
        );

        let second_run = instances[2];
        assert_eq!(second_run.origin[0], 60.0 + 6.0 * 0.4);
    }

    #[test]
    fn build_strip_skips_lines_past_the_content() {
        // The view is scrolled so the strip window runs off the end of a short
        // file. The missing lines contribute no run quads.
        let content = vec![vec![MinimapRun {
            start_col: 0,
            len: 1,
            class: 0,
        }]];
        let view = Some(MinimapView {
            top_256: 0,
            visible: 30,
        });
        let (instances, _) = build_strip(&strip(view), &summaries(content), metrics());

        // Background + one run (the single line) + thumb, nothing for the missing
        // lines the strip window covers.
        assert_eq!(instances.len(), 3);
    }

    #[test]
    fn strips_rebuild_only_when_the_epoch_or_resolution_changes() {
        let Some((device, queue)) = headless_device() else {
            return;
        };
        let mut pass = MinimapPass::new(&device, TextureFormat::Rgba8Unorm, metrics());

        let mut grid = Grid::new(12, 24);
        grid.set_minimaps(vec![strip(None)]);
        grid.set_minimap_contents(HashMap::from([(
            1,
            summaries(vec![vec![MinimapRun {
                start_col: 0,
                len: 4,
                class: 1,
            }]]),
        )]));

        let resolution = [640.0, 480.0];
        pass.prepare(&device, &queue, &grid, &[], resolution);
        assert_eq!(pass.strips.len(), 1, "the declared strip builds one draw");

        // A change to something the epoch does not cover leaves the strips as
        // they were, which is the skip this pass exists for.
        grid.set_text_runs(Vec::new());
        pass.prepare(&device, &queue, &grid, &[], resolution);
        assert_eq!(
            pass.strips.len(),
            1,
            "an unchanged epoch skips the rebuild and keeps the prior strips"
        );

        // The grid bumps the epoch itself, so emptying the list outside a
        // projection is a change this sees.
        grid.set_minimaps(Vec::new());
        pass.prepare(&device, &queue, &grid, &[], resolution);
        assert!(
            pass.strips.is_empty(),
            "a moved epoch rebuilds against the current grid"
        );

        grid.set_minimaps(vec![strip(None)]);
        pass.prepare(&device, &queue, &grid, &[], [800.0, 600.0]);
        assert_eq!(
            pass.strips.len(),
            1,
            "a resolution change rebuilds the strips against the current grid"
        );
    }

    /// The live screen and the scrollback window take turns through one pass,
    /// and each grid counts only its own changes, so two of them meet on a
    /// count routinely. A pass reading the count alone would hold one grid's
    /// strips over the other for as long as the two agreed.
    #[test]
    fn a_second_grid_holding_the_same_count_is_not_the_first() {
        let Some((device, queue)) = headless_device() else {
            return;
        };
        let mut pass = MinimapPass::new(&device, TextureFormat::Rgba8Unorm, metrics());

        // Built by the same calls in the same order, so both counters land in
        // the same place over different strip lists.
        let mut first = Grid::new(12, 24);
        first.set_minimaps(vec![strip(None)]);
        first.set_minimap_contents(HashMap::from([(1, summaries(lines(4)))]));
        let second = two_strip_grid(4, 4);
        assert_eq!(
            first.minimap_epoch(),
            second.minimap_epoch(),
            "the fixture needs two grids that agree on their count",
        );

        let resolution = [640.0, 480.0];
        pass.prepare(&device, &queue, &first, &[], resolution);
        pass.prepare(&device, &queue, &second, &[], resolution);
        assert_eq!(
            pass.strips.len(),
            2,
            "the second grid's strips are the ones drawn",
        );
    }

    /// A metrics change invalidates every cached instance, but the draws are
    /// packed from that cache one stage later. A grid declaring no strips gives
    /// the next prepare nothing to rebuild and so no reason to repack, and
    /// anything left in the draws is drawn scissored to where the strip sat at
    /// the old cell size.
    #[test]
    fn a_metrics_change_leaves_no_strip_to_draw() {
        let Some((device, queue)) = headless_device() else {
            return;
        };
        let mut pass = MinimapPass::new(&device, TextureFormat::Rgba8Unorm, metrics());

        let mut grid = Grid::new(12, 24);
        grid.set_minimaps(vec![strip(None)]);
        grid.set_minimap_contents(HashMap::from([(1, summaries(lines(4)))]));

        let resolution = [640.0, 480.0];
        pass.prepare(&device, &queue, &grid, &[], resolution);
        assert_eq!(pass.strips.len(), 1, "the declared strip builds one draw");

        pass.set_metrics(CellMetrics {
            font_size: 20.0,
            width: 12.0,
            height: 24.0,
            scale_factor: 1.0,
        });
        grid.set_minimaps(Vec::new());
        pass.prepare(&device, &queue, &grid, &[], resolution);

        assert!(
            pass.strips.is_empty(),
            "a strip laid out at the old cell size is not drawn",
        );
    }

    /// A grid that still declares its strip has to see it rebuilt at the new
    /// cell size, and the build key a metrics change clears is the only thing
    /// that sends the frame past the gate: the grid and its epoch are exactly
    /// where the last frame left them.
    #[test]
    fn a_metrics_change_rebuilds_a_strip_the_grid_still_declares() {
        let Some((device, queue)) = headless_device() else {
            return;
        };
        let mut pass = MinimapPass::new(&device, TextureFormat::Rgba8Unorm, metrics());

        let mut grid = Grid::new(12, 24);
        grid.set_minimaps(vec![strip(None)]);
        grid.set_minimap_contents(HashMap::from([(1, summaries(lines(4)))]));

        let resolution = [640.0, 480.0];
        pass.prepare(&device, &queue, &grid, &[], resolution);
        let before = pass.built[0].scissor;

        // Twice the cell box, so the strip's own box doubles with it.
        pass.set_metrics(CellMetrics {
            font_size: 20.0,
            width: 12.0,
            height: 24.0,
            scale_factor: 1.0,
        });
        pass.prepare(&device, &queue, &grid, &[], resolution);

        assert_eq!(
            (pass.builds, before, pass.built[0].scissor),
            (2, [60, 0, 48, 120], [120, 0, 96, 240]),
            "the strip is built again, on a box the new cell size sets",
        );
    }

    /// The per-strip cache under the epoch gate keys on the store's write count,
    /// which is the declaring grid's own. Two grids agree on a strip and on that
    /// count as readily as they agree on the epoch, so the cache has to go when
    /// the grid does or it hands one grid's lines to the other.
    #[test]
    fn a_second_grid_reuses_no_strip_of_the_first() {
        let Some((device, queue)) = headless_device() else {
            return;
        };
        let mut pass = MinimapPass::new(&device, TextureFormat::Rgba8Unorm, metrics());

        // The same strip over stores of different heights, declared by the same
        // calls, so the two grids agree on both counters the cache reads.
        let content = |count: usize| {
            let mut grid = Grid::new(12, 24);
            grid.set_minimaps(vec![strip(None)]);
            grid.set_minimap_contents(HashMap::from([(1, summaries(lines(count)))]));
            grid
        };
        let first = content(2);
        let second = content(6);
        assert_eq!(
            (
                first.minimap_epoch(),
                first.minimap_content_version(1),
                first.minimaps()[0].clone(),
            ),
            (
                second.minimap_epoch(),
                second.minimap_content_version(1),
                second.minimaps()[0].clone(),
            ),
            "the fixture needs two grids the cache cannot tell apart by its keys",
        );

        let resolution = [640.0, 480.0];
        pass.prepare(&device, &queue, &first, &[], resolution);
        let two_lines = pass.built[0].instances.len();
        pass.prepare(&device, &queue, &second, &[], resolution);

        // A strip's instances are its lines plus its background and its thumb.
        assert_eq!(
            (two_lines, pass.built[0].instances.len(), pass.builds),
            (4, 8, 2),
            "the second grid's lines are the ones built",
        );
    }

    /// A second strip beside the first, over its own content store, so a change
    /// to one can be seen not to touch the other.
    fn second_strip(view: Option<MinimapView>) -> Minimap {
        Minimap {
            strip: MinimapStrip {
                left: 2,
                strip_id: 2,
                content_id: 2,
                ..command()
            },
            seq: 4,
            view,
        }
    }

    /// One run per line, so a strip's instance count follows its line count.
    fn lines(count: usize) -> Vec<Vec<MinimapRun>> {
        (0..count)
            .map(|_| {
                vec![MinimapRun {
                    start_col: 0,
                    len: 4,
                    class: 1,
                }]
            })
            .collect()
    }

    /// Two strips over two stores, each with its own line count.
    fn two_strip_grid(first_lines: usize, second_lines: usize) -> Grid {
        let mut grid = Grid::new(12, 24);
        grid.set_minimaps(vec![strip(None), second_strip(None)]);
        grid.set_minimap_contents(HashMap::from([
            (1, summaries(lines(first_lines))),
            (2, summaries(lines(second_lines))),
        ]));
        grid
    }

    /// The epoch moves for any minimap change anywhere, so it cannot say which
    /// strip moved. With a strip per pane, rebuilding on it alone means one
    /// thumb drag rebuilds every pane on screen.
    #[test]
    fn a_view_move_rebuilds_only_the_strip_that_moved() {
        let Some((device, queue)) = headless_device() else {
            return;
        };
        let mut pass = MinimapPass::new(&device, TextureFormat::Rgba8Unorm, metrics());
        let mut grid = two_strip_grid(4, 7);
        let resolution = [640.0, 480.0];

        pass.prepare(&device, &queue, &grid, &[], resolution);
        let untouched = pass.built[1].instances.clone();
        let (built_once, uploaded_once) = (pass.builds, pass.uploads);

        grid.set_minimaps(vec![
            strip(Some(MinimapView {
                top_256: 512,
                visible: 3,
            })),
            second_strip(None),
        ]);
        pass.prepare(&device, &queue, &grid, &[], resolution);

        assert_ne!(
            pass.built[0].declared.view, None,
            "the moved strip took the new view"
        );
        assert!(
            pass.built[1].instances == untouched,
            "the strip nothing touched keeps the instances it already had"
        );
        assert_eq!(
            (pass.builds - built_once, pass.uploads - uploaded_once),
            (1, 1),
            "and the frame built the one strip that moved, then sent the frame"
        );
    }

    /// The epoch moves whenever the strips are declared, which a projection does
    /// every frame whether they moved or not. Re-packing and sending the same
    /// bytes on each of those is the idle cost the per-strip key removes.
    #[test]
    fn a_redeclared_but_unmoved_strip_set_sends_nothing() {
        let Some((device, queue)) = headless_device() else {
            return;
        };
        let mut pass = MinimapPass::new(&device, TextureFormat::Rgba8Unorm, metrics());
        let mut grid = two_strip_grid(4, 7);
        let resolution = [640.0, 480.0];

        pass.prepare(&device, &queue, &grid, &[], resolution);
        let (built, uploaded) = (pass.builds, pass.uploads);
        let drawn = pass.strips.len();

        // The same two strips again, which moves the epoch and nothing else.
        grid.set_minimaps(vec![strip(None), second_strip(None)]);
        pass.prepare(&device, &queue, &grid, &[], resolution);

        assert_eq!(
            (pass.builds, pass.uploads, pass.strips.len()),
            (built, uploaded, drawn),
            "nothing was built, nothing was sent, and the draws still stand"
        );
    }

    /// A one-line edit splices the same count into the same allocation, so the
    /// store's address and length both stand still while its bytes move. Only a
    /// version says the strip has to rebuild.
    #[test]
    fn a_same_length_content_edit_rebuilds_its_strip() {
        let Some((device, queue)) = headless_device() else {
            return;
        };
        let mut pass = MinimapPass::new(&device, TextureFormat::Rgba8Unorm, metrics());
        let mut grid = two_strip_grid(4, 7);
        let resolution = [640.0, 480.0];

        pass.prepare(&device, &queue, &grid, &[], resolution);
        let before = pass.built[0].instances.clone();
        let untouched = pass.built[1].instances.clone();
        let (built_once, uploaded_once) = (pass.builds, pass.uploads);

        // One line replaced by one line, over a run of a different width, so the
        // instances have to move while the line count does not.
        grid.splice_minimap_content(
            1,
            0,
            1,
            &summaries(vec![vec![MinimapRun {
                start_col: 0,
                len: 40,
                class: 2,
            }]]),
        );
        pass.prepare(&device, &queue, &grid, &[], resolution);

        assert!(
            pass.built[0].instances != before,
            "the edited store's strip rebuilds against its new content"
        );
        assert!(
            pass.built[1].instances == untouched,
            "and the store nothing edited leaves its strip alone"
        );
        assert_eq!(
            (pass.builds - built_once, pass.uploads - uploaded_once),
            (1, 1),
            "one store edited, one strip built, one frame sent"
        );
    }

    /// Draw `grid`'s strips alone onto a black [`TARGET`]-square target and read
    /// the red channel back, one byte a pixel.
    ///
    /// Red alone because every fixture paints in pure red over black, so the
    /// byte at a pixel is the coverage the shader resolved there.
    fn render_red(
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        occluders: &[Occluder],
        metrics: CellMetrics,
    ) -> Vec<u8> {
        let mut pass = MinimapPass::new(device, TextureFormat::Rgba8Unorm, metrics);
        pass.prepare(
            device,
            queue,
            grid,
            occluders,
            [TARGET as f32, TARGET as f32],
        );

        let size = Extent3d {
            width: TARGET,
            height: TARGET,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&TextureDescriptor {
            label: Some("minimap coverage target"),
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
            label: Some("minimap coverage readback"),
            size: u64::from(TARGET) * u64::from(TARGET) * 4,
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("minimap coverage"),
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
            pass.draw(&mut render_pass);
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

    /// A strip drawn in pure red over nothing, so a readback byte is coverage.
    ///
    /// The thumb is cleared along with the background. It spans the strip's full
    /// width, so a visible one paints over the very runs under measurement.
    fn red_strip(width: u16, height: u16, lines_per_cell: u8, max_columns: u8) -> MinimapStrip {
        MinimapStrip {
            top: 0,
            left: 0,
            width,
            height,
            strip_id: 1,
            content_id: 1,
            lines_per_cell,
            max_columns,
            bg: Rgba::new(0, 0, 0, 0),
            thumb: Rgba::new(0, 0, 0, 0),
            thumb_border: Rgb::new(0, 0, 0),
            palette: vec![Rgb::new(255, 0, 0)],
        }
    }

    fn red_grid(
        strip: MinimapStrip,
        content: Vec<Vec<MinimapRun>>,
        view: Option<MinimapView>,
    ) -> Grid {
        let mut grid = Grid::new(12, 24);
        grid.set_minimaps(vec![Minimap {
            strip,
            seq: 0,
            view,
        }]);
        grid.set_minimap_contents(HashMap::from([(1, summaries(content))]));
        grid
    }

    /// A minimap column is routinely a fraction of a pixel wide. Without
    /// coverage the run either misses every pixel center and vanishes, or takes
    /// a whole pixel and reads twice its weight.
    #[test]
    fn a_half_pixel_run_covers_half_a_pixel() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("minimap coverage test: no wgpu adapter, skipping");
            return;
        };

        // One cell over 12 columns puts a column, and so a one-column run, at
        // half a pixel. One line a cell keeps the run tall enough that only the
        // horizontal axis is fractional.
        let run = MinimapRun {
            start_col: 0,
            len: 1,
            class: 0,
        };
        let grid = red_grid(red_strip(1, 1, 1, 12), vec![vec![run]], None);

        let red = render_red(&device, &queue, &grid, &[], metrics());
        let at = |x: u32, y: u32| red[(y * TARGET + x) as usize];

        assert!(
            (120..=136).contains(&at(0, 4)),
            "half a pixel of red reads as half intensity, got {}",
            at(0, 4)
        );
        assert_eq!(
            at(1, 4),
            0,
            "and the run does not smear into the next column"
        );
    }

    /// A panel draws rounded corners, so the chrome beneath a corner is outside
    /// the body even while it sits inside the declared rect. Occluding by the
    /// rect notches a square hole out of that chrome.
    #[test]
    fn a_rounded_corner_keeps_the_chrome_beneath_it() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("minimap occlusion test: no wgpu adapter, skipping");
            return;
        };

        // A 4x2-cell strip is 24 by 24 pixels under a 6x12 cell, so a radius of
        // 8 rounds well inside its 12-pixel half-extent.
        let mut command = red_strip(4, 2, 1, 12);
        command.bg = Rgba::new(255, 0, 0, 255);
        let grid = red_grid(command, vec![Vec::new()], None);

        let panel = Panel {
            top: 0,
            left: 0,
            width: 4,
            height: 2,
            style: BorderStyle::Rounded,
            border: Rgb::new(0, 0, 0),
            corner_radius: 8,
            fill: None,
            shadow: PanelShadow::Drop,
            inset_x: 0,
            above_pools: false,
            anchor: None,
            // Above the strip's own seq, which is what makes it occlude.
            seq: 1,
        };
        let occluders: [Occluder; 1] = [panel_occluder(&panel)];

        let red = render_red(&device, &queue, &grid, &occluders, metrics());
        let at = |x: u32, y: u32| red[(y * TARGET + x) as usize];

        // A pixel a cell and a half in from the corner sits a clear pixel and a
        // half inside the declared rect, and a clear pixel outside the rounded
        // body. The half-pixel ring the threshold spares reaches neither.
        assert_eq!(
            at(1, 1),
            255,
            "the corner is rounded away, so the strip survives under it"
        );
        assert_eq!(at(12, 12), 0, "and the body itself still hides the strip");
    }

    /// A fractional scroll used to carry every block across pixel centers, so
    /// blocks popped in and out instead of sliding, which is the sparkle this
    /// replaces. Coverage hands the intensity from one row to the next and
    /// conserves the total.
    #[test]
    fn a_sub_pixel_scroll_hands_intensity_between_rows() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("minimap scroll test: no wgpu adapter, skipping");
            return;
        };

        // Cell 6x12 over 6 lines a cell puts a minimap line at 2px, and a strip
        // four cells tall shows 24 of them. With the viewport also 24 lines over
        // 100, minimap_top reduces to the declared top, so top_256 moves the
        // strip by a known sub-pixel amount.
        let mut content = vec![Vec::new(); 100];
        content[4] = vec![MinimapRun {
            start_col: 0,
            len: 12,
            class: 0,
        }];
        let strip_at = |top_256: u32| {
            let view = MinimapView {
                top_256,
                visible: 24,
            };
            let grid = red_grid(red_strip(1, 4, 6, 12), content.clone(), Some(view));
            let red = render_red(&device, &queue, &grid, &[], metrics());
            // Column 2 sits inside the full-width run, so only the vertical
            // axis is fractional there.
            [7u32, 8, 9].map(|y| u32::from(red[(y * TARGET + 2) as usize]))
        };

        // Line 4 spans y 8.0 to 9.5 at rest. A quarter line later it spans 7.5
        // to 9.0, so row 7 takes the half row 9 gives up.
        let rest = strip_at(0);
        let scrolled = strip_at(64);

        assert_eq!(rest[0], 0, "at rest nothing reaches the row above");
        assert!(
            (120..=136).contains(&scrolled[0]),
            "a quarter-line scroll lights it half way, got {}",
            scrolled[0]
        );
        assert!(
            (120..=136).contains(&rest[2]),
            "the row below starts half lit, got {}",
            rest[2]
        );
        assert_eq!(scrolled[2], 0, "and gives that up as the run moves off it");
        assert_eq!(
            rest.iter().sum::<u32>(),
            scrolled.iter().sum::<u32>(),
            "the run carries the same ink either way"
        );
    }
}
