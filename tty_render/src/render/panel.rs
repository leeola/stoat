//! Instanced modal-chrome panel pass.
//!
//! Draws each [`Panel`] as a soft drop shadow confined to the box exterior, an
//! optional interior fill, and a hairline stroke frame with rounded corners.
//! Unlike the opaque overlay pass, a panel is chrome layered with the grid
//! rather than over it, so the body runs before the grid text. The framed cells
//! render over the fill, and text outside the frame renders over the shadow. An
//! unfilled panel leaves its interior showing the grid beneath it.
//!
//! The frame stroke is the exception, and the reason this pass draws in two
//! halves. A frame surrounds the cells it frames, so glyph ink reaching a cell
//! edge would break the line around it. The stroke records after the text, and
//! in a frame compositing pools after those composites too.

use crate::render::{rgb_f32, CellMetrics, HostRide, RowAnchor};
use bytemuck::{Pod, Zeroable};
use std::mem;
use stoatty_protocol::command;
use stoatty_term::grid::{BorderStyle, Grid, Panel, PanelShadow};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout,
    BindGroupLayoutDescriptor, BindGroupLayoutEntry, BindingType, BlendState, Buffer,
    BufferBindingType, BufferDescriptor, BufferUsages, ColorTargetState, ColorWrites, Device,
    FragmentState, PipelineLayoutDescriptor, Queue, RenderPass, RenderPipeline,
    RenderPipelineDescriptor, ShaderModuleDescriptor, ShaderSource, ShaderStages, TextureFormat,
    VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in panels, allocated up front. Grows by doubling
/// when a frame exceeds it.
const INITIAL_CAPACITY: usize = 16;

/// Vertices each panel draws per pass, six for each of four edge bands around
/// an interior rectangle and six for that rectangle.
///
/// The interior is most of a modal's quad, and the stroke pass and an unfilled
/// body paint nothing there. So those passes leave it out rather than launch a
/// fragment for every pixel of it.
const REGION_VERTICES: u32 = 30;

/// Drop-shadow blur radius in logical pixels, three sigma of the 20 logical
/// pixel sigma that the boards' 40 px CSS blur stands for.
///
/// The boards draw every modal with `box-shadow: 0 18px 40px rgba(0,0,0,0.6)`,
/// and one drop shadow serves every overlay. The shadow's alpha fades to zero
/// across this distance past the shadow rectangle.
const SHADOW_MARGIN: f32 = 60.0;

/// Drop-shadow displacement in logical pixels, straight down, the boards'
/// `box-shadow: 0 18px 40px` under a modal, so a panel reads as floating above
/// the grid rather than pasted onto it.
const SHADOW_OFFSET: [f32; 2] = [0.0, 18.0];

/// Height in logical pixels of a tucked shadow's interior bottom band, the band
/// a raised card darkens inside its bottom edge.
///
/// The band is a gaussian edge whose sigma is a third of this height, 4 logical
/// pixels as the boards blur it. It lays 0.27 of black on the bottom logical
/// pixel row and fades out as it rises.
const SHADOW_MARGIN_TUCKED: f32 = 12.0;

/// Height in logical pixels of an overhang shadow's interior bottom band. Shorter
/// than a tucked band and fainter, at 0.14, so it reads as a faint shadow cast
/// onto the panel by whatever overhangs it.
const SHADOW_MARGIN_OVERHANG: f32 = 5.0;

/// Blur radius in logical pixels for a halo shadow, three sigma of the 7
/// logical pixel sigma that the boards' 14 px CSS blur stands for.
///
/// The boards shade a docked box above and left of it with two shadows whose
/// spread cancels their offset, so only the blur reaches out of the box. The
/// halo is undisplaced for the same reason.
const SHADOW_MARGIN_HALO: f32 = 21.0;

/// Cast-shadow displacement in logical pixels, straight down, the boards'
/// `box-shadow: 0 4px 8px` under a pinned row.
const SHADOW_OFFSET_CAST: [f32; 2] = [0.0, 4.0];

/// Blur radius in logical pixels for a cast shadow, three sigma of the 4
/// logical pixel sigma that the boards' 8 px CSS blur stands for.
const SHADOW_MARGIN_CAST: f32 = 12.0;

/// The bit of an instance's shape word that marks a panel no pane pool takes
/// as an occluder, which a backdrop and a panel riding a pool this frame are.
///
/// A pool's composite paints over such a panel's box, so the shadow drawn
/// again over the composite falls inside it too. The shader's `PASSES_POOLS`
/// reads the same bit.
const PASSES_POOLS: u32 = 1 << 6;

/// The per-panel instance data. Carries the anchor cell, the size in cells, the
/// fill and stroke colors, the shadow displacement and blur radius, the corner
/// radius, a flag selecting how the fill paints, the border style code, the
/// shadow mode (0 drop, 1 tucked, 2 overhang, 3 halo, 4 cast), the shape bits
/// (the rounded corners in bits 0-3, the fillets in bits 4-5, and
/// [`PASSES_POOLS`] in bit 6), the hatch color with a flag selecting whether
/// it draws, each edge's pixels with the packed edge modes, and the fillet
/// radius.
///
/// The fill flag is 0 for no fill and 1 for a fill. A backdrop's fill takes 2,
/// which paints the fill only outside the panel's own cells.
///
/// The shader's occlusion test reads the cell, the size, the radius, the shape,
/// and the edges of every later panel by their place in this struct, so fields
/// join at the end.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct PanelInstance {
    cell: [f32; 2],
    size: [f32; 2],
    fill: [f32; 3],
    border: [f32; 3],
    shadow_offset: [f32; 2],
    shadow_margin: f32,
    corner_radius: f32,
    fill_flag: f32,
    style: u32,
    shadow_mode: f32,
    shape: u32,
    hatch: [f32; 3],
    hatch_flag: f32,
    edge_px: [f32; 4],
    edge_modes: u32,
    fillet_radius: f32,
}

/// The uniform shared by every instance. Carries the surface resolution, the
/// cell size the vertex shader maps cell coordinates through, and the panel
/// count the fragment shader loops over for self-occlusion. Padded to 32 bytes
/// so the layout matches the WGSL uniform.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    count: u32,
    /// Physical pixels per logical pixel, which the stroke weights scale by.
    scale_factor: f32,
    _pad: [u32; 2],
}

/// The instanced panel pipeline and its per-frame buffers.
pub struct PanelPass {
    /// The shadow, fill, and overhang, recorded beneath the text a panel frames.
    pipeline_under: RenderPipeline,
    /// The frame stroke alone, so it can be recorded above that text.
    pipeline_stroke: RenderPipeline,
    /// The exterior shadow alone, recorded again over a pane pool's composite,
    /// which paints over the shadow the under pass drew.
    pipeline_shadow: RenderPipeline,
    /// Whether a panel that rides no pool casts an exterior shadow this frame,
    /// so the shadow draw over a composite has anything to draw.
    casts: bool,
    bind_group_layout: BindGroupLayout,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    /// The instances last uploaded, so an unchanged frame skips the write.
    last_instances: Vec<PanelInstance>,
    /// Where each frame's instances are built, before being compared against
    /// [`Self::last_instances`] and traded with it. Chrome rebuilds every frame
    /// and changes on almost none of them, so building into a buffer the pass
    /// keeps spares an allocation the frame would otherwise discard.
    built: Vec<PanelInstance>,
    count: u32,
    /// Instance slots the base draw skips, with the scissor each rides under.
    ///
    /// Rebuilt every frame from the panels carrying an anchor whose host
    /// composites. Empty on a frame with no glide, where the base draw covers
    /// every panel as it always did.
    riding: Vec<(u32, [u32; 4])>,
    /// The uniform last written, so an unchanged frame skips that write too.
    last_globals: Option<Globals>,
    metrics: CellMetrics,
}

impl PanelPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> PanelPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("panel"),
            source: ShaderSource::Wgsl(
                crate::render::with_shadow(&crate::render::with_occlusion(include_str!(
                    "../shaders/panel.wgsl"
                )))
                .into(),
            ),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("panel globals"),
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
            label: Some("panel"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        // One module, two pairs of stages. The stroke draws separately from the
        // rest, so the frame records above the text it surrounds. Each vertex
        // stage leaves out the part of the box its fragment stage paints nothing
        // over.
        let build = |label: &str, vertex: &str, fragment: &str| {
            device.create_render_pipeline(&RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: VertexState {
                    module: &shader,
                    entry_point: Some(vertex),
                    compilation_options: Default::default(),
                    buffers: &[Some(VertexBufferLayout {
                        array_stride: size_of::<PanelInstance>() as u64,
                        step_mode: VertexStepMode::Instance,
                        attributes: &vertex_attr_array![
                            0 => Float32x2,
                            1 => Float32x2,
                            2 => Float32x3,
                            3 => Float32x3,
                            4 => Float32x2,
                            5 => Float32,
                            6 => Float32,
                            7 => Float32,
                            8 => Uint32,
                            9 => Float32,
                            10 => Uint32,
                            11 => Float32x3,
                            12 => Float32,
                            13 => Float32x4,
                            14 => Uint32,
                            15 => Float32,
                        ],
                    })],
                },
                fragment: Some(FragmentState {
                    module: &shader,
                    entry_point: Some(fragment),
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
        };
        let pipeline_under = build("panel under", "vs_under", "fs_under");
        let pipeline_stroke = build("panel stroke", "vs_stroke", "fs_stroke");
        // The stroke's vertex stage draws the four bands around the box, which
        // hold every pixel an exterior shadow reaches.
        let pipeline_shadow = build("panel shadow", "vs_stroke", "fs_shadow");

        let globals = device.create_buffer(&BufferDescriptor {
            label: Some("panel globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let instances = alloc_instances(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &instances);

        PanelPass {
            pipeline_under,
            pipeline_stroke,
            pipeline_shadow,
            casts: false,
            bind_group_layout,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
            riding: Vec::new(),
            last_instances: Vec::new(),
            last_globals: None,
            built: Vec::new(),
            metrics,
        }
    }

    /// Replace the cell metrics so the next frame lays out cells at the new size.
    pub(crate) fn set_metrics(&mut self, metrics: CellMetrics) {
        self.metrics = metrics;
    }

    /// Upload the frame's uniform and one instance per grid panel.
    ///
    /// `resolution` is the surface size in physical pixels. `anchor` places the
    /// boxes' rows, which the shader scales by the uniform pitch. Reallocates
    /// the instance buffer only when the panel count outgrows the current
    /// capacity.
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        anchored: &[HostRide],
        resolution: [f32; 2],
        anchor: RowAnchor,
    ) {
        // With no panel to draw now and none drawn last frame, nothing reads this
        // pass's buffers, so the frame skips it without touching the GPU. The frame
        // that empties the list still runs, which is what drops the count to zero
        // and stops the draw.
        if grid.panels().is_empty() && self.count == 0 {
            return;
        }

        build_panel_instances_into(grid.panels(), self.metrics.scale_factor, &mut self.built);
        for instance in &mut self.built {
            let (top, height) =
                anchor.place_span(instance.cell[1], instance.size[1], self.metrics.height);
            instance.cell[1] = top;
            instance.size[1] = height;
        }

        // A ridden panel keeps its slot in the buffer, because the fragment
        // shader self-occludes against every later instance, so a reorder changes
        // which box hides which. Only its drawn position moves, and the base draw
        // skips its slot so it lands after the composites instead.
        self.riding.clear();
        for (index, panel) in grid.panels().iter().enumerate() {
            let Some((host, top_rows)) = panel.anchor else {
                continue;
            };
            let Some(ride) = anchored.iter().find(|ride| ride.host == host) else {
                continue;
            };
            self.built[index].cell[1] +=
                ride.shift_px(top_rows, self.metrics.height) / self.metrics.height;
            self.built[index].shape |= PASSES_POOLS;
            self.riding.push((index as u32, ride.scissor));
        }
        self.casts = grid.panels().iter().enumerate().any(|(index, panel)| {
            let exterior = matches!(
                panel.shadow,
                PanelShadow::Drop | PanelShadow::Halo | PanelShadow::Cast
            );
            exterior
                && !self
                    .riding
                    .iter()
                    .any(|&(ridden, _)| ridden == index as u32)
        });

        self.count = self.built.len() as u32;

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            count: self.count,
            scale_factor: self.metrics.scale_factor,
            _pad: [0; 2],
        };
        crate::render::upload_globals(queue, &self.globals, 0, globals, &mut self.last_globals);

        if self.built.is_empty() {
            return;
        }

        if !crate::render::upload_needed(&self.built, &self.last_instances) {
            return;
        }

        if self.built.len() > self.capacity {
            self.capacity = self.built.len().next_power_of_two();
            self.instances = alloc_instances(device, self.capacity);
            self.bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals,
                &self.instances,
            );
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&self.built));
        mem::swap(&mut self.built, &mut self.last_instances);
    }

    /// Record the shadow, fill, and overhang of every non-riding panel.
    ///
    /// A no-op when the grid carries no panel. Run before the grid text, so the
    /// framed cells render over the fill. The frame stroke that belongs to this
    /// body is [`Self::draw_stroke`], recorded after that text.
    pub fn draw_under(&self, render_pass: &mut RenderPass<'_>) {
        self.draw_slots(render_pass, &self.pipeline_under);
    }

    /// Record the frame stroke of every non-riding panel.
    ///
    /// Split from [`Self::draw_under`] so a caller can put the stroke above the
    /// text the frame surrounds while the rest stays below it.
    pub fn draw_stroke(&self, render_pass: &mut RenderPass<'_>) {
        self.draw_slots(render_pass, &self.pipeline_stroke);
    }

    /// Record the exterior shadow of every non-riding panel again, clipped to
    /// `scissor`, the region of a pane pool composited over the live grid.
    ///
    /// The composite repaints its region opaque, so the shadow that
    /// [`Self::draw_under`] drew there shows nowhere in it. This draw lands
    /// outside every box the composite leaves to the live grid, where the first
    /// shadow still shows, and outside every later box, as the first draw does.
    /// A no-op on a frame where no such panel casts an exterior shadow. Leaves
    /// `scissor` set.
    pub(crate) fn draw_shadows_over(&self, render_pass: &mut RenderPass<'_>, scissor: [u32; 4]) {
        let [x, y, width, height] = scissor;
        if !self.casts || width == 0 || height == 0 {
            return;
        }
        render_pass.set_scissor_rect(x, y, width, height);
        self.draw_slots(render_pass, &self.pipeline_shadow);
    }

    fn draw_slots(&self, render_pass: &mut RenderPass<'_>, pipeline: &RenderPipeline) {
        if self.count == 0 {
            return;
        }

        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));

        // A ridden slot is skipped here and drawn by [`Self::draw_riding`] after
        // the composites, so the base pass leaves a gap where it sits.
        let mut next = 0;
        for &(index, _) in &self.riding {
            if index > next {
                render_pass.draw(0..REGION_VERTICES, next..index);
            }
            next = index + 1;
        }
        if next < self.count {
            render_pass.draw(0..REGION_VERTICES, next..self.count);
        }
    }

    /// Record the shadow, fill, and overhang of every riding panel.
    ///
    /// Recorded after the host's composite rather than with the rest of the
    /// chrome, so the body lands over the pooled surface it belongs to instead
    /// of being painted over by it. A no-op on a frame with no ride.
    pub fn draw_riding_under(&self, render_pass: &mut RenderPass<'_>) {
        self.draw_riding_slots(render_pass, &self.pipeline_under);
    }

    /// Record the frame stroke of every riding panel, split for
    /// [`Self::draw_stroke`]'s reason.
    pub fn draw_riding_stroke(&self, render_pass: &mut RenderPass<'_>) {
        self.draw_riding_slots(render_pass, &self.pipeline_stroke);
    }

    fn draw_riding_slots(&self, render_pass: &mut RenderPass<'_>, pipeline: &RenderPipeline) {
        if self.riding.is_empty() {
            return;
        }

        render_pass.set_pipeline(pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        for &(index, [x, y, w, h]) in &self.riding {
            if w == 0 || h == 0 {
                continue;
            }
            render_pass.set_scissor_rect(x, y, w, h);
            render_pass.draw(0..REGION_VERTICES, index..index + 1);
        }
    }
}

fn alloc_instances(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("panel instances"),
        size: (capacity * size_of::<PanelInstance>()) as u64,
        // STORAGE so the fragment shader can read every instance's box rect for
        // self-occlusion, alongside the per-instance vertex fetch.
        usage: BufferUsages::VERTEX | BufferUsages::STORAGE | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// Bind the globals uniform (binding 0) and the instance storage buffer
/// (binding 1). Rebuilt whenever the instance buffer is reallocated, since the
/// bind group holds a reference to the specific buffer.
fn make_bind_group(
    device: &Device,
    layout: &BindGroupLayout,
    globals: &Buffer,
    instances: &Buffer,
) -> BindGroup {
    device.create_bind_group(&BindGroupDescriptor {
        label: Some("panel globals"),
        layout,
        entries: &[
            BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            },
            BindGroupEntry {
                binding: 1,
                resource: instances.as_entire_binding(),
            },
        ],
    })
}

/// One instance per panel, in draw order, into a vector of the caller's.
#[cfg(test)]
fn build_panel_instances(panels: &[Panel]) -> Vec<PanelInstance> {
    let mut instances = Vec::new();
    build_panel_instances_into(panels, 1.0, &mut instances);
    instances
}

/// Build into `out` one instance per panel, in draw order.
///
/// A panel with no fill leaves the interior transparent. A panel with no shadow
/// zeroes the shadow, so the pass draws only the stroke.
///
/// The shadow geometry, the corner radius, the edges, and the fillet radius are
/// all stated in logical pixels and multiplied by `scale_factor` here. Left
/// physical, they hold their pixel count against a box that doubled, so the
/// chrome reads half weight on a 2x display.
///
/// Each product rounds, so the box edges land on the whole pixels the cell
/// rectangle already uses. A 1.25 or a 1.5 display density otherwise puts a
/// fraction back on an edge, and the stroke over it covers two pixels at
/// partial opacity rather than one.
///
/// `out` is cleared first, so a reused scratch buffer holds only this frame's
/// panels.
fn build_panel_instances_into(panels: &[Panel], scale_factor: f32, out: &mut Vec<PanelInstance>) {
    let physical = |logical: u8| (f32::from(logical) * scale_factor).round();

    out.clear();
    out.extend(panels.iter().map(|panel| {
        let (shadow_offset, shadow_margin, shadow_mode) = match panel.shadow {
            PanelShadow::Drop => (SHADOW_OFFSET, SHADOW_MARGIN, 0.0),
            PanelShadow::Tucked => ([0.0, 0.0], SHADOW_MARGIN_TUCKED, 1.0),
            PanelShadow::Overhang => ([0.0, 0.0], SHADOW_MARGIN_OVERHANG, 2.0),
            PanelShadow::Halo => ([0.0, 0.0], SHADOW_MARGIN_HALO, 3.0),
            PanelShadow::Cast => (SHADOW_OFFSET_CAST, SHADOW_MARGIN_CAST, 4.0),
            PanelShadow::None_ => ([0.0, 0.0], 0.0, 0.0),
        };
        PanelInstance {
            cell: [panel.left as f32, panel.top as f32],
            size: [panel.width as f32, panel.height as f32],
            fill: panel.fill.map(rgb_f32).unwrap_or([0.0, 0.0, 0.0]),
            border: rgb_f32(panel.border),
            shadow_offset: [
                (shadow_offset[0] * scale_factor).round(),
                (shadow_offset[1] * scale_factor).round(),
            ],
            shadow_margin: (shadow_margin * scale_factor).round(),
            corner_radius: physical(panel.corner_radius),
            fill_flag: match (panel.fill, panel.backdrop) {
                (None, _) => 0.0,
                (Some(_), false) => 1.0,
                (Some(_), true) => 2.0,
            },
            style: style_code(panel.style),
            shadow_mode,
            shape: u32::from(corner_mask(panel.corners))
                | u32::from(panel.fillets & 0b11) << 4
                | if panel.backdrop { PASSES_POOLS } else { 0 },
            hatch: panel.hatch.map(rgb_f32).unwrap_or([0.0, 0.0, 0.0]),
            hatch_flag: if panel.hatch.is_some() { 1.0 } else { 0.0 },
            edge_px: panel.edges.map(|edge| physical(edge.px())),
            edge_modes: u32::from(command::edge_modes(panel.edges)),
            fillet_radius: physical(panel.fillet_radius),
        }
    }));
}

/// The corners a panel or a bar rounds, as four bits from top-left clockwise.
/// A mask that names no corner rounds all four.
pub(crate) fn corner_mask(corners: u8) -> u8 {
    match corners & 0b1111 {
        0 => 0b1111,
        mask => mask,
    }
}

fn style_code(style: BorderStyle) -> u32 {
    match style {
        BorderStyle::Light => 0,
        BorderStyle::Heavy => 1,
        BorderStyle::Double => 2,
        BorderStyle::Rounded => 3,
        BorderStyle::None_ => 4,
        BorderStyle::Dashed => 5,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        build_panel_instances, build_panel_instances_into, style_code, PanelInstance, PanelPass,
    };
    use crate::{
        render::{CellMetrics, GridLayout, RowAnchor},
        test_support::{read_back, require_headless_device},
    };
    use stoatty_protocol::command::Edge;
    use stoatty_term::grid::{BorderStyle, Grid, Panel, PanelShadow, Rgb};
    use wgpu::{
        naga::{
            front::wgsl,
            valid::{Capabilities, ValidationFlags, Validator},
        },
        Color, CommandEncoderDescriptor, Device, Extent3d, LoadOp, Operations, Queue,
        RenderPassColorAttachment, RenderPassDescriptor, StoreOp, TextureDescriptor,
        TextureDimension, TextureFormat, TextureUsages, TextureViewDescriptor,
    };

    /// The square readback target's edge, in pixels. Four bytes a texel makes a
    /// row exactly the 256-byte copy alignment, so the readback needs no stride
    /// padding.
    const TARGET: u32 = 64;

    /// The ground [`panel_pixel`] draws its panel over.
    const GROUND: [u8; 3] = [51, 153, 102];

    /// Which halves of the panel pass a fixture draws.
    ///
    /// The frame is recorded above the text it surrounds, so a fixture reading
    /// what lies under the text draws `Under` and leaves the frame off.
    #[derive(Clone, Copy, PartialEq)]
    enum Halves {
        Under,
        Both,
    }

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_shadow(&crate::render::with_occlusion(
            include_str!("../shaders/panel.wgsl"),
        )))
        .expect("parse panel");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate panel");
    }

    /// The color at pixel `(x, y)` once a `cells`-square panel at cell (1, 1) in
    /// `fill` and `shadow` draws over [`GROUND`].
    ///
    /// Cells are 12 pixels at scale 1, so a two-cell box spans pixels 12 to 36
    /// on each axis and a one-cell box spans 12 to 24. The panel's corner
    /// radius is 6 and its frame is light, and the fixture leaves the frame off.
    fn panel_pixel(
        device: &Device,
        queue: &Queue,
        cells: u16,
        fill: Option<Rgb>,
        shadow: PanelShadow,
        at: (u32, u32),
    ) -> [u8; 3] {
        let panel = Panel {
            top: 1,
            left: 1,
            width: cells,
            height: cells,
            style: BorderStyle::Light,
            border: Rgb::new(255, 0, 0),
            corner_radius: 6,
            fill,
            shadow,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        };
        render_pixel(device, queue, 1.0, panel, at)
    }

    /// The color at pixel `(x, y)` once `panel` draws over [`GROUND`], with
    /// 12-pixel cells at `scale_factor`. The fixture leaves the frame off.
    fn render_pixel(
        device: &Device,
        queue: &Queue,
        scale_factor: f32,
        panel: Panel,
        (x, y): (u32, u32),
    ) -> [u8; 3] {
        let metrics = CellMetrics {
            font_size: 10.0,
            width: 12.0,
            height: 12.0,
            scale_factor,
        };
        let ground = Color {
            r: f64::from(GROUND[0]) / 255.0,
            g: f64::from(GROUND[1]) / 255.0,
            b: f64::from(GROUND[2]) / 255.0,
            a: 1.0,
        };
        let mut grid = Grid::new(4, 4);
        grid.set_panels(vec![panel]);
        let rgba = render_rgba(device, queue, &grid, metrics, ground, Halves::Under);
        let at = ((y * TARGET + x) * 4) as usize;
        [rgba[at], rgba[at + 1], rgba[at + 2]]
    }

    /// Draw `grid`'s panels alone onto a [`TARGET`]-square target cleared to
    /// `clear`, and read the whole surface back.
    fn render_rgba(
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        metrics: CellMetrics,
        clear: Color,
        halves: Halves,
    ) -> Vec<u8> {
        let mut pass = PanelPass::new(device, TextureFormat::Rgba8Unorm, metrics);
        pass.prepare(
            device,
            queue,
            grid,
            &[],
            [TARGET as f32, TARGET as f32],
            RowAnchor::UNIFORM,
        );

        let size = Extent3d {
            width: TARGET,
            height: TARGET,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&TextureDescriptor {
            label: Some("panel weight target"),
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
                label: Some("panel weight"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(clear),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            // In the order the frame chain records them, so a fixture asking
            // for both measures the composite a real frame produces.
            pass.draw_under(&mut render_pass);
            if halves == Halves::Both {
                pass.draw_stroke(&mut render_pass);
            }
        }
        queue.submit(Some(encoder.finish()));

        read_back(device, queue, &target, TARGET, TARGET)
    }

    /// The red the panels painted over black, one byte a pixel.
    ///
    /// The fixtures paint in pure red, so the byte at a pixel is what reached
    /// the target there.
    fn render_red(
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        metrics: CellMetrics,
        halves: Halves,
    ) -> Vec<u8> {
        render_rgba(device, queue, grid, metrics, Color::BLACK, halves)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|texel| texel[0])
            .collect()
    }

    /// The coverage the panels resolved, one byte a pixel.
    ///
    /// A panel's layers are red and black, so none of them writes green. Over a
    /// white ground the green channel reads back as the ground that survived,
    /// and the coverage is what is left of it.
    fn render_coverage(
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        metrics: CellMetrics,
    ) -> Vec<u8> {
        render_rgba(device, queue, grid, metrics, Color::WHITE, Halves::Both)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|texel| 255 - texel[1])
            .collect()
    }

    /// Two partly covered layers cover more together than either does alone.
    /// Taking the larger of their alphas understates that, and because the
    /// pipeline blends unpremultiplied, the understatement weakens the stroke's
    /// own color and lets the ground behind show through.
    #[test]
    fn a_stroke_over_a_shadow_covers_more_than_either() {
        let (device, queue) = require_headless_device();

        let metrics = CellMetrics {
            font_size: 10.0,
            width: 12.0,
            height: 12.0,
            scale_factor: 1.0,
        };
        let panel = |shadow| Panel {
            top: 1,
            left: 1,
            width: 2,
            height: 2,
            style: BorderStyle::Light,
            border: Rgb::new(255, 0, 0),
            // A rounded corner is where the frame's own coverage is partial, so
            // it is where the frame meets the shadow with neither of them whole.
            corner_radius: 6,
            // Unfilled, so the frame meets the shadow with nothing opaque
            // between them.
            fill: None,
            shadow,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        };
        let coverage = |shadow| {
            let mut grid = Grid::new(4, 4);
            grid.set_panels(vec![panel(shadow)]);
            render_coverage(&device, &queue, &grid, metrics)
        };

        // A shadowless panel carries the frame alone, so it is what the frame
        // covers with nothing under it.
        let alone = coverage(PanelShadow::None_);
        let dropped = coverage(PanelShadow::Drop);

        // A shadow alone never reaches past SHADOW_ALPHA. So a coverage above
        // that which also exceeds the shadowless panel's is neither of the two
        // inputs, and only compositing them produces it.
        let ceiling = (0.6 * 255.0) as u8;
        let composited = dropped.iter().zip(&alone).position(|(dropped, alone)| {
            (1..255).contains(alone) && dropped > alone && *dropped > ceiling
        });

        assert!(
            composited.is_some(),
            "the frame's rounded corner and the shadow beneath it compose"
        );
    }

    /// An opaque box meets what surrounds it at a crisp edge, so the fill
    /// covers the pixel inside that edge whole and the pixel outside not at
    /// all.
    ///
    /// A ramp two pixels wide instead lays 84 percent over the inner pixel and
    /// 16 percent over the outer one, which reads as a soft edge on every side
    /// of every box.
    ///
    /// The frame stays off here. It straddles the perimeter, so in the
    /// composite it covers both of the pixels this reads.
    #[test]
    fn a_box_fill_covers_the_pixel_inside_its_edge_whole() {
        let (device, queue) = require_headless_device();

        let metrics = CellMetrics {
            font_size: 10.0,
            width: 12.0,
            height: 12.0,
            scale_factor: 1.0,
        };
        let mut grid = Grid::new(4, 4);
        grid.set_panels(vec![Panel {
            top: 1,
            left: 1,
            width: 2,
            height: 2,
            style: BorderStyle::Light,
            border: Rgb::new(255, 0, 0),
            corner_radius: 0,
            fill: Some(Rgb::new(255, 0, 0)),
            // A shadow pads the quad, so the pixel outside the box rasterizes
            // at all. Its color is black, which leaves the red channel alone.
            shadow: PanelShadow::Tucked,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        }]);

        // The box's left edge falls on x 12, and row 24 crosses its vertical
        // middle, clear of the corners.
        let red = render_red(&device, &queue, &grid, metrics, Halves::Under);
        let at = |x: u32| red[(24 * TARGET + x) as usize];

        assert_eq!(
            (at(12), at(11)),
            (255, 0),
            "the fill covers the pixel inside the left edge and none of the one outside"
        );
    }

    /// Deep inside its box a panel paints its fill alone, since the drop shadow
    /// stops at the box edge. An unfilled panel leaves the ground there as it
    /// was, and a filled one covers it with the fill whole.
    ///
    /// A box too small for an interior rectangle gets its middle from the edge
    /// bands, which meet there, so its fill covers that middle whole too.
    ///
    /// A tucked panel and an overhang are the exceptions. Each casts a band onto
    /// the fill from inside the bottom edge, so a pixel just above that edge
    /// reads darker than the fill, the tucked band the darker of the two.
    #[test]
    fn a_panel_interior_paints_the_fill_alone() {
        let (device, queue) = require_headless_device();
        let blue = Some(Rgb::new(0, 0, 255));
        // Pixel (24, 24) sits twelve pixels inside every edge of a two-cell box,
        // and (18, 18) is the middle of a one-cell box. Pixel (24, 23) sits past
        // the twelve pixels a tucked band rises from the bottom edge.
        let pixel = |cells, fill, shadow, at| panel_pixel(&device, &queue, cells, fill, shadow, at);

        assert_eq!(
            (
                pixel(2, None, PanelShadow::Drop, (24, 24)),
                pixel(2, blue, PanelShadow::Drop, (24, 24)),
                pixel(1, blue, PanelShadow::Drop, (18, 18)),
                pixel(2, blue, PanelShadow::Tucked, (24, 23)),
            ),
            (GROUND, [0, 0, 255], [0, 0, 255], [0, 0, 255]),
            "the ground shows through an unfilled interior, and a fill covers it whole, \
             the one-cell box's middle and a tucked box above its band included",
        );
        // Row 33 sits two and a half pixels above the bottom edge. The band lays
        // 0.07 of black there, which is 237 of the fill's 255 blue, give or take
        // the rounding a driver does.
        let overhung = pixel(2, blue, PanelShadow::Overhang, (24, 33));
        assert!(
            matches!(overhung, [0, 0, 230..=245]),
            "the overhang band darkens the fill above the bottom edge, got {overhung:?}",
        );
        // Row 35 sits half a pixel above the bottom edge, where the tucked band
        // lays 0.27 of black, which is 186 of the fill's 255 blue. Row 33 lies
        // two and a half pixels inside the edge, where the band lays 0.16, which
        // is 214 of the blue and keeps 84 percent of an unfilled box's ground.
        let tucked = [
            pixel(2, blue, PanelShadow::Tucked, (24, 35)),
            pixel(2, blue, PanelShadow::Tucked, (24, 33)),
        ];
        assert!(
            matches!(tucked, [[0, 0, 183..=189], [0, 0, 211..=217]]),
            "the tucked band darkens the fill's last rows, deepest at the edge, got {tucked:?}",
        );
        let ground = pixel(2, None, PanelShadow::Tucked, (24, 33));
        assert!(
            matches!(ground, [40..=46, 126..=132, 83..=89]),
            "the tucked band darkens the ground under an unfilled box, got {ground:?}",
        );
    }

    /// A tucked panel shades nothing outside its box, so the ground beside,
    /// above, and below it keeps its color. A halo shades the ground left of
    /// the box and above it, and stops at its bottom and right edges.
    #[test]
    fn a_tucked_box_shades_nothing_outside_and_a_halo_stops_at_the_bottom() {
        let (device, queue) = require_headless_device();
        let blue = Some(Rgb::new(0, 0, 255));
        // A two-cell box spans pixels 12 to 36. The tucked margin pads its quad
        // by twelve pixels and the halo's pads by 21, so every pixel read here
        // rasterizes.
        let tucked = |at| panel_pixel(&device, &queue, 2, blue, PanelShadow::Tucked, at);
        let halo = |at| panel_pixel(&device, &queue, 2, blue, PanelShadow::Halo, at);

        assert_eq!(
            [
                tucked((10, 24)),
                tucked((24, 10)),
                tucked((24, 38)),
                halo((24, 38)),
                halo((38, 24)),
            ],
            [GROUND; 5],
            "no tucked shade beside, above, or below the box, and no halo below it or right of it",
        );
        let darker = |[r, g, b]: [u8; 3]| r < GROUND[0] && g < GROUND[1] && b < GROUND[2];
        let (beside, above) = (halo((10, 24)), halo((24, 5)));
        assert!(
            darker(beside) && darker(above),
            "the halo darkens the ground left of the box and 7 px above it, got {beside:?} and \
             {above:?}",
        );
    }

    /// A chip shorter than its row, a tab rounded at the top alone, a fillet
    /// beside a bottom corner, and a hatched fill each land on whole pixels.
    ///
    /// At scale 2 a two-cell box spans pixels 12 to 36. An inset of 3 shaves 6
    /// pixels, and a radius of 3 rounds 6. A fillet of radius 2 reaches 4 pixels
    /// past the box, and one of radius 5 reaches 10. The hatch lines run 14
    /// pixels apart along x plus y.
    #[test]
    fn a_panel_takes_its_inset_corner_mask_fillets_and_hatch() {
        let (device, queue) = require_headless_device();
        let (blue, red) = ([0, 0, 255], [255, 0, 0]);
        let pixel = |shape: fn(&mut Panel), at| {
            let mut panel = Panel {
                top: 1,
                left: 1,
                width: 2,
                height: 2,
                style: BorderStyle::None_,
                border: Rgb::new(255, 0, 0),
                corner_radius: 0,
                fill: Some(Rgb::new(0, 0, 255)),
                shadow: PanelShadow::None_,
                edges: Edge::symmetric(0, 0),
                corners: 0,
                fillets: 0,
                fillet_radius: 2,
                hatch: None,
                above_pools: false,
                backdrop: false,
                anchor: None,
                seq: 0,
            };
            shape(&mut panel);
            render_pixel(&device, &queue, 2.0, panel, at)
        };

        let chip = |panel: &mut Panel| panel.edges = Edge::symmetric(0, 3);
        assert_eq!(
            [pixel(chip, (24, 15)), pixel(chip, (24, 20))],
            [GROUND, blue],
            "the inset leaves the row's top strip bare and fills below it",
        );

        let tab = |panel: &mut Panel| {
            panel.corner_radius = 3;
            panel.corners = 0b0011;
        };
        assert_eq!(
            [
                pixel(tab, (12, 12)),
                pixel(tab, (12, 35)),
                pixel(tab, (35, 35))
            ],
            [GROUND, blue, blue],
            "the top-left corner rounds, and both bottom corners stay square",
        );

        let filleted = |panel: &mut Panel| panel.fillets = 0b11;
        assert_eq!(
            [
                pixel(filleted, (11, 35)),
                pixel(filleted, (9, 33)),
                pixel(filleted, (36, 35)),
                pixel(|_| {}, (11, 35)),
            ],
            [blue, GROUND, blue, GROUND],
            "a fillet fills beside each bottom corner and curves away above it",
        );

        let wide_fillet = |panel: &mut Panel| {
            panel.fillets = 0b11;
            panel.fillet_radius = 5;
        };
        assert_eq!(
            [pixel(wide_fillet, (11, 31)), pixel(filleted, (11, 31))],
            [blue, GROUND],
            "a fillet takes its own radius, so the wider one reaches further up the side",
        );

        let hatched = |panel: &mut Panel| panel.hatch = Some(Rgb::new(255, 0, 0));
        assert_eq!(
            [
                pixel(hatched, (14, 14)),
                pixel(hatched, (15, 14)),
                pixel(hatched, (28, 14)),
                pixel(hatched, (12, 15)),
            ],
            [red, blue, red, red],
            "the hatch lines repeat every 14 pixels, on the edge pixels as inside",
        );
    }

    /// A later panel hides an earlier panel's fill only inside the box it draws.
    /// The fill shows in the bare strip that the chip's vertical inset leaves,
    /// and at the chip's rounded corner. It stays hidden under the chip's middle
    /// and its square corner.
    ///
    /// The fill covers the whole grid. The chip covers columns 1 to 3 of rows 1
    /// and 2, which is pixels 12 to 48 across and 12 to 36 down. It shaves 4
    /// pixels off the top and the bottom, so its box spans pixels 16 to 32
    /// down. Its top corners round at 6, and its bottom corners stay square.
    #[test]
    fn a_chip_hides_the_panel_beneath_only_inside_its_box() {
        let (device, queue) = require_headless_device();

        let beneath = Panel {
            top: 0,
            left: 0,
            width: 5,
            height: 5,
            style: BorderStyle::None_,
            border: Rgb::new(255, 0, 0),
            corner_radius: 0,
            fill: Some(Rgb::new(255, 0, 0)),
            shadow: PanelShadow::None_,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        };
        let chip = Panel {
            top: 1,
            left: 1,
            width: 3,
            height: 2,
            corner_radius: 6,
            fill: None,
            edges: Edge::symmetric(0, 4),
            corners: 0b0011,
            seq: 1,
            ..beneath
        };
        let mut grid = Grid::new(5, 5);
        grid.set_panels(vec![beneath, chip]);

        let metrics = CellMetrics {
            font_size: 10.0,
            width: 12.0,
            height: 12.0,
            scale_factor: 1.0,
        };
        let red = render_red(&device, &queue, &grid, metrics, Halves::Under);
        let at = |x: u32, y: u32| red[(y * TARGET + x) as usize];

        assert_eq!(
            [at(30, 13), at(13, 17), at(30, 24), at(13, 30)],
            [255, 255, 0, 0],
            "the fill shows in the bare strip and at the rounded top corner, \
             and hides under the chip's middle and its square bottom corner",
        );
    }

    /// A backdrop's edges reach across the cells beside it, and its fill paints
    /// only there, so its own cells keep whatever they draw.
    ///
    /// The cells are 9 by 18 pixels, and the panel covers columns 2 and 3 of
    /// row 1, which is pixels 18 to 36 across. Its left edge is a margin of 4,
    /// so the box crosses column 1 and stops 4 pixels short of its far side, at
    /// pixel 13. Its right edge is a gap of 4. That reaches half of the
    /// margin's 5 pixels into column 4, rounded up to 3, so the box ends at
    /// pixel 39.
    #[test]
    fn a_backdrop_fills_only_the_cells_its_edges_reach_across() {
        let (device, queue) = require_headless_device();
        let panel = Panel {
            top: 1,
            left: 2,
            width: 2,
            height: 1,
            style: BorderStyle::None_,
            border: Rgb::new(255, 0, 0),
            corner_radius: 0,
            fill: Some(Rgb::new(255, 0, 0)),
            shadow: PanelShadow::None_,
            edges: [
                Edge::Inset(0),
                Edge::Gap(4),
                Edge::Inset(0),
                Edge::Margin(4),
            ],
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: true,
            anchor: None,
            seq: 0,
        };
        let mut grid = Grid::new(3, 7);
        grid.set_panels(vec![panel]);

        let metrics = CellMetrics {
            font_size: 15.0,
            width: 9.0,
            height: 18.0,
            scale_factor: 1.0,
        };
        let red = render_red(&device, &queue, &grid, metrics, Halves::Under);
        let row = |y: u32| red[(y * TARGET) as usize..((y + 1) * TARGET) as usize].to_vec();

        let reached = |x: u32| (13..18).contains(&x) || (36..39).contains(&x);
        let expected = (0..TARGET)
            .map(|x| if reached(x) { 255 } else { 0 })
            .collect::<Vec<u8>>();
        assert_eq!(
            [row(18), row(27)],
            [expected.clone(), expected],
            "on the row's top pixel row and its middle, the fill covers pixels 13 to 17 \
             and 36 to 38, and nothing in the panel's cells",
        );
    }

    /// An outset edge stands its px outside the cell edge.
    ///
    /// The cells are 9 by 18 pixels, and the panel covers columns 1 to 4 of
    /// row 1, which is pixels 18 to 35 down. Its top edge is an outset of 2 and
    /// its bottom edge an outset of 9, so its fill covers pixels 16 to 44 down.
    #[test]
    fn an_outset_edge_stands_outside_its_cell_edge() {
        let (device, queue) = require_headless_device();
        let mut grid = Grid::new(3, 7);
        grid.set_panels(vec![Panel {
            top: 1,
            left: 1,
            width: 4,
            height: 1,
            style: BorderStyle::None_,
            border: Rgb::new(255, 0, 0),
            corner_radius: 0,
            fill: Some(Rgb::new(255, 0, 0)),
            shadow: PanelShadow::None_,
            edges: [
                Edge::Outset(2),
                Edge::Inset(0),
                Edge::Outset(9),
                Edge::Inset(0),
            ],
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        }]);

        let metrics = CellMetrics {
            font_size: 15.0,
            width: 9.0,
            height: 18.0,
            scale_factor: 1.0,
        };
        let red = render_red(&device, &queue, &grid, metrics, Halves::Under);
        let column = (0..TARGET)
            .map(|y| red[(y * TARGET + 20) as usize])
            .collect::<Vec<u8>>();

        let expected = (0..TARGET)
            .map(|y| if (16..45).contains(&y) { 255 } else { 0 })
            .collect::<Vec<u8>>();
        assert_eq!(
            column, expected,
            "down pixel column 20, the fill covers pixels 16 to 44 and nothing above or below",
        );
    }

    /// A backdrop with a fill paints its ground, its border color, over the
    /// part of its own cells that a rounded corner leaves outside the box. The
    /// ground draws in the stroke pass, above the text, and the rest of the
    /// cells keep what they draw. A backdrop with no fill paints no ground.
    ///
    /// The cells are 9 by 18 pixels, and the panel covers columns 2 and 3 of
    /// row 1, pixels 18 to 35 across and down, rounded at 6.
    #[test]
    fn a_filled_backdrop_paints_its_ground_over_the_corners_inside_its_cells() {
        let (device, queue) = require_headless_device();
        let metrics = CellMetrics {
            font_size: 15.0,
            width: 9.0,
            height: 18.0,
            scale_factor: 1.0,
        };
        let red = |fill: Option<Rgb>, halves| {
            let mut grid = Grid::new(3, 7);
            grid.set_panels(vec![Panel {
                top: 1,
                left: 2,
                width: 2,
                height: 1,
                style: BorderStyle::None_,
                border: Rgb::new(255, 0, 0),
                corner_radius: 6,
                fill,
                shadow: PanelShadow::None_,
                edges: Edge::symmetric(0, 0),
                corners: 0,
                fillets: 0,
                fillet_radius: 2,
                hatch: None,
                above_pools: false,
                backdrop: true,
                anchor: None,
                seq: 0,
            }]);
            render_red(&device, &queue, &grid, metrics, halves)
        };
        let at = |red: &[u8], (x, y): (u32, u32)| red[(y * TARGET + x) as usize];

        let black = Some(Rgb::new(0, 0, 0));
        let (both, under, unfilled) = (
            red(black, Halves::Both),
            red(black, Halves::Under),
            red(None, Halves::Both),
        );
        assert_eq!(
            (
                [(18, 18), (35, 18), (18, 35), (35, 35)].map(|pixel| at(&both, pixel)),
                [(24, 24), (27, 18), (18, 27)].map(|pixel| at(&both, pixel)),
                at(&under, (18, 18)),
                at(&unfilled, (18, 18)),
            ),
            ([255; 4], [0; 3], 0, 0),
            "the ground at the four corners, none inside the box, none in the under pass, \
             and none from an unfilled backdrop",
        );
    }

    /// A frame is a band inside the box edge, so it lands on the same pixels
    /// whether or not a shadow pads the quad around it.
    ///
    /// A stroke centered on the perimeter puts half its ink outside the box,
    /// and a shadowless panel's quad stops at the box rect, which clips that
    /// half away. One style then drew a thinner frame ending in a hard step
    /// than the same style drew with a shadow behind it.
    #[test]
    fn a_frame_band_sits_inside_the_box_edge() {
        let (device, queue) = require_headless_device();

        let metrics = CellMetrics {
            font_size: 10.0,
            width: 12.0,
            height: 12.0,
            scale_factor: 1.0,
        };
        // The box's top edge falls on y 12, and the column at x 24 crosses it
        // clear of the corners.
        let column = |shadow| {
            let mut grid = Grid::new(4, 4);
            grid.set_panels(vec![Panel {
                top: 1,
                left: 1,
                width: 2,
                height: 2,
                style: BorderStyle::Light,
                border: Rgb::new(255, 0, 0),
                corner_radius: 0,
                fill: None,
                shadow,
                edges: Edge::symmetric(0, 0),
                corners: 0,
                fillets: 0,
                fillet_radius: 2,
                hatch: None,
                above_pools: false,
                backdrop: false,
                anchor: None,
                seq: 0,
            }]);
            let red = render_red(&device, &queue, &grid, metrics, Halves::Both);
            let at = |y: u32| red[(y * TARGET + 24) as usize];
            (at(11), at(12), at(13))
        };

        assert_eq!(
            (column(PanelShadow::None_), column(PanelShadow::Tucked)),
            ((0, 255, 0), (0, 255, 0)),
            "one full pixel inside the top edge, nothing on either side of it"
        );
    }

    /// A dashed frame lights three pixels in every five along each side, where
    /// a light frame lights them all.
    ///
    /// The dashes run clockwise from the top-left corner, and a corner pixel
    /// takes its place on the top or the bottom side. A dash and a gap are
    /// logical lengths, so a denser display draws them over more pixels.
    #[test]
    fn a_dashed_frame_lights_three_pixels_in_five() {
        let (device, queue) = require_headless_device();

        // The box spans x 12 to 48 and y 12 to 36 at either density, so its
        // sides' inner pixel rows and columns hold 36 and 24 pixels.
        let metrics = |scale_factor| CellMetrics {
            font_size: 10.0,
            width: 12.0,
            height: 12.0,
            scale_factor,
        };
        let sides = |style, scale_factor| {
            let mut grid = Grid::new(4, 4);
            grid.set_panels(vec![Panel {
                top: 1,
                left: 1,
                width: 3,
                height: 2,
                style,
                border: Rgb::new(255, 0, 0),
                corner_radius: 0,
                fill: None,
                shadow: PanelShadow::None_,
                edges: Edge::symmetric(0, 0),
                corners: 0,
                fillets: 0,
                fillet_radius: 2,
                hatch: None,
                above_pools: false,
                backdrop: false,
                anchor: None,
                seq: 0,
            }]);
            let red = render_red(&device, &queue, &grid, metrics(scale_factor), Halves::Both);
            let lit = |x: u32, y: u32| match red[(y * TARGET + x) as usize] {
                255 => '#',
                _ => '.',
            };
            [
                (12..48).map(|x| lit(x, 12)).collect::<String>(),
                (12..36).map(|y| lit(47, y)).collect::<String>(),
                (12..48).map(|x| lit(x, 35)).collect::<String>(),
                (12..36).map(|y| lit(12, y)).collect::<String>(),
            ]
        };

        assert_eq!(
            sides(BorderStyle::Light, 1.0),
            [
                "#".repeat(36),
                "#".repeat(24),
                "#".repeat(36),
                "#".repeat(24)
            ],
            "a light frame's top, right, bottom, and left sides whole",
        );
        assert_eq!(
            sides(BorderStyle::Dashed, 1.0),
            [
                "###..".repeat(7) + "#",
                "##..###..###..###..###.#".to_string(),
                "#".to_string() + &"..###".repeat(7),
                "#.###..###..###..###..##".to_string(),
            ],
            "a dashed frame's top, right, bottom, and left sides",
        );
        assert_eq!(
            sides(BorderStyle::Dashed, 2.0)[0],
            "######....".repeat(3) + "######",
            "at twice the density a dash and a gap span twice the pixels",
        );
    }

    /// A chrome weight left in physical pixels holds its pixel count while the
    /// box it frames doubles, so the frame reads half as heavy on a 2x display.
    #[test]
    fn a_heavy_stroke_doubles_its_width_at_twice_the_density() {
        let (device, queue) = require_headless_device();

        let mut grid = Grid::new(4, 4);
        grid.set_panels(vec![Panel {
            top: 1,
            left: 1,
            width: 2,
            height: 2,
            style: BorderStyle::Heavy,
            border: Rgb::new(255, 0, 0),
            corner_radius: 0,
            fill: None,
            shadow: PanelShadow::None_,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        }]);

        // One cell rectangle, two densities. The box lands on the same pixels
        // either way, so the only thing that moves is the stroke weight.
        let metrics = |scale_factor| CellMetrics {
            font_size: 10.0,
            width: 12.0,
            height: 12.0,
            scale_factor,
        };
        // The row through the box's vertical middle crosses the left and right
        // strokes and nothing else.
        let lit = |scale_factor| {
            let red = render_red(&device, &queue, &grid, metrics(scale_factor), Halves::Both);
            (0..TARGET)
                .filter(|x| red[(24 * TARGET + x) as usize] > 0)
                .count()
        };

        let single = lit(1.0);
        let double = lit(2.0);

        assert!(single > 0, "the heavy stroke paints something at 1x");
        assert!(
            double.abs_diff(single * 2) <= 2,
            "and about twice as much at 2x: {single} then {double}"
        );
    }

    #[test]
    fn a_reused_panel_scratch_holds_only_this_frame_s_panels() {
        let panels = [Panel {
            top: 3,
            left: 5,
            width: 8,
            height: 4,
            style: BorderStyle::Heavy,
            border: Rgb::new(0, 255, 0),
            corner_radius: 6,
            fill: Some(Rgb::new(255, 0, 0)),
            shadow: PanelShadow::Drop,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        }];

        let mut scratch = build_panel_instances(&panels);
        scratch.extend(build_panel_instances(&panels));
        build_panel_instances_into(&panels, 1.0, &mut scratch);

        assert_eq!(
            bytemuck::cast_slice::<PanelInstance, u8>(&scratch),
            bytemuck::cast_slice::<PanelInstance, u8>(&build_panel_instances(&panels)),
            "reuse clears the stale panels and rebuilds only the frame's own"
        );
    }

    /// The pass skips its GPU write when the rebuilt instances match the last
    /// upload, so unchanged chrome must compare equal across rebuilds and a
    /// real change must not.
    #[test]
    fn rebuilt_panels_compare_equal_until_one_changes() {
        use crate::render::upload_needed;

        let panels = [Panel {
            top: 3,
            left: 5,
            width: 8,
            height: 4,
            style: BorderStyle::Heavy,
            border: Rgb::new(0, 255, 0),
            corner_radius: 6,
            fill: Some(Rgb::new(255, 0, 0)),
            shadow: PanelShadow::Drop,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        }];
        let first = build_panel_instances(&panels);
        assert!(
            !upload_needed(&build_panel_instances(&panels), &first),
            "an unchanged panel rebuilds to the same bytes, so no upload is needed",
        );

        let recolored = [Panel {
            border: Rgb::new(0, 254, 0),
            ..panels[0]
        }];
        assert!(
            upload_needed(&build_panel_instances(&recolored), &first),
            "a one-channel color change must still reach the GPU",
        );
    }

    #[test]
    fn filled_panel_maps_geometry_colors_and_shadow() {
        let panels = [Panel {
            top: 3,
            left: 5,
            width: 8,
            height: 4,
            style: BorderStyle::Heavy,
            border: Rgb::new(0, 255, 0),
            corner_radius: 6,
            fill: Some(Rgb::new(255, 0, 0)),
            shadow: PanelShadow::Drop,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        }];

        let instances = build_panel_instances(&panels);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].cell, [5.0, 3.0]);
        assert_eq!(instances[0].size, [8.0, 4.0]);
        assert_eq!(instances[0].fill, [1.0, 0.0, 0.0]);
        assert_eq!(instances[0].border, [0.0, 1.0, 0.0]);
        assert_eq!(instances[0].shadow_offset, super::SHADOW_OFFSET);
        assert_eq!(instances[0].shadow_margin, super::SHADOW_MARGIN);
        assert_eq!(instances[0].shadow_mode, 0.0);
        assert_eq!(instances[0].corner_radius, 6.0);
        assert_eq!(instances[0].fill_flag, 1.0);
        assert_eq!(instances[0].style, style_code(BorderStyle::Heavy));
    }

    #[test]
    fn unfilled_shadowless_panel_zeroes_fill_and_shadow() {
        let panels = [Panel {
            top: 0,
            left: 0,
            width: 4,
            height: 2,
            style: BorderStyle::Light,
            border: Rgb::new(10, 20, 30),
            corner_radius: 0,
            fill: None,
            shadow: PanelShadow::None_,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        }];

        let instances = build_panel_instances(&panels);

        assert_eq!(instances[0].fill_flag, 0.0);
        assert_eq!(instances[0].shadow_offset, [0.0, 0.0]);
        assert_eq!(instances[0].shadow_margin, 0.0);
        assert_eq!(instances[0].shadow_mode, 0.0);
    }

    /// A tucked band and a halo both stay on the box, so neither displaces its
    /// shadow. Each reaches the shader as its own mode and margin.
    #[test]
    fn tucked_and_halo_panels_keep_their_shadows_on_the_box() {
        assert_eq!(
            [
                shadow_instance(PanelShadow::Tucked),
                shadow_instance(PanelShadow::Halo)
            ],
            [
                ([0.0, 0.0], super::SHADOW_MARGIN_TUCKED, 1.0),
                ([0.0, 0.0], super::SHADOW_MARGIN_HALO, 3.0),
            ],
        );
    }

    /// A cast shadow falls 4 logical px under the box with a 12 px margin, three
    /// sigma of the boards' 8 px blur, and reaches the shader as mode 4.
    #[test]
    fn a_cast_panel_drops_its_shadow_under_the_box() {
        assert_eq!(
            shadow_instance(PanelShadow::Cast),
            ([0.0, 4.0], super::SHADOW_MARGIN_CAST, 4.0),
        );
    }

    /// The shadow displacement, margin, and mode that a filled panel with
    /// `shadow` reaches the shader with at scale 1.
    fn shadow_instance(shadow: PanelShadow) -> ([f32; 2], f32, f32) {
        let panels = [Panel {
            top: 2,
            left: 2,
            width: 6,
            height: 3,
            style: BorderStyle::Light,
            border: Rgb::new(1, 2, 3),
            corner_radius: 0,
            fill: Some(Rgb::new(4, 5, 6)),
            shadow,
            edges: Edge::symmetric(4, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        }];
        let instance = build_panel_instances(&panels)[0];
        (
            instance.shadow_offset,
            instance.shadow_margin,
            instance.shadow_mode,
        )
    }

    /// A panel that reaches the last row arrives with that row's place in its
    /// cells, which the shader scales by the uniform pitch, and a panel above
    /// it keeps its own.
    #[test]
    fn a_panel_reaching_the_last_row_is_placed_on_it() {
        let (device, queue) = require_headless_device();
        let metrics = CellMetrics::from_font_size(17, 1.0);
        let mut pass = PanelPass::new(&device, TextureFormat::Rgba8Unorm, metrics);
        let panel = |top, height| Panel {
            top,
            left: 1,
            width: 4,
            height,
            style: BorderStyle::Light,
            border: Rgb::new(255, 0, 0),
            corner_radius: 0,
            fill: None,
            shadow: PanelShadow::None_,
            edges: Edge::symmetric(0, 0),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
            seq: 0,
        };
        let mut grid = Grid::new(10, 40);
        grid.set_panels(vec![panel(7, 3), panel(2, 3)]);
        // Ten 20-pixel rows whose last row rose 6 pixels.
        let anchor = GridLayout {
            rows: 10,
            cols: 40,
            bottom_row_y: 174.0,
        }
        .row_anchor();

        pass.prepare(&device, &queue, &grid, &[], [800.0, 194.0], anchor);
        assert_eq!(
            pass.last_instances
                .iter()
                .map(|instance| (instance.cell[1], instance.size[1]))
                .collect::<Vec<_>>(),
            [(7.0, 2.7), (2.0, 3.0)],
            "the box over rows 7 to 9 ends at the bottom edge, 54 pixels down",
        );
    }
}
