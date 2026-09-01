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

use crate::render::{AnchoredPanel, CellMetrics};
use bytemuck::{Pod, Zeroable};
use std::mem;
use stoatty_term::grid::{BorderStyle, Grid, Panel, PanelShadow, Rgb};
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

/// Drop-shadow blur radius in logical pixels. The shadow's alpha fades to zero
/// across this distance past the shadow rectangle.
const SHADOW_MARGIN: f32 = 16.0;

/// Drop-shadow displacement in logical pixels, down and to the right, so a
/// panel reads as floating above the grid rather than pasted onto it.
const SHADOW_OFFSET: [f32; 2] = [5.0, 7.0];

/// Blur radius in logical pixels for a tucked shadow. Tighter than
/// [`SHADOW_MARGIN`] so the undisplaced halo reads as a seam rather than a float.
const SHADOW_MARGIN_TUCKED: f32 = 6.0;

/// Height in logical pixels of an overhang shadow's interior bottom band. Small,
/// so it reads as a faint shadow cast onto the panel by whatever overhangs it.
const SHADOW_MARGIN_OVERHANG: f32 = 5.0;

/// The per-panel instance data. Carries the anchor cell, the size in cells, the
/// fill and stroke colors, the shadow displacement and blur radius, the corner
/// radius, a flag selecting whether the fill is painted, the border style code,
/// and the shadow mode (0 drop, 1 tucked, 2 overhang).
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
    inset_x: f32,
    shadow_mode: f32,
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
                crate::render::with_occlusion(include_str!("../shaders/panel.wgsl")).into(),
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

        // One module, two fragment stages. The stroke draws separately from the
        // rest so it can be recorded above the text the frame surrounds.
        let build = |label: &str, entry: &str| {
            device.create_render_pipeline(&RenderPipelineDescriptor {
                label: Some(label),
                layout: Some(&layout),
                vertex: VertexState {
                    module: &shader,
                    entry_point: Some("vs_main"),
                    compilation_options: Default::default(),
                    buffers: &[VertexBufferLayout {
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
                            10 => Float32,
                        ],
                    }],
                },
                fragment: Some(FragmentState {
                    module: &shader,
                    entry_point: Some(entry),
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
        let pipeline_under = build("panel under", "fs_under");
        let pipeline_stroke = build("panel stroke", "fs_stroke");

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
    /// `resolution` is the surface size in physical pixels. Reallocates the
    /// instance buffer only when the panel count outgrows the current capacity.
    pub fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        anchored: &[AnchoredPanel],
        resolution: [f32; 2],
    ) {
        // With no panel to draw now and none drawn last frame, nothing reads this
        // pass's buffers, so the frame skips it without touching the GPU. The frame
        // that empties the list still runs, which is what drops the count to zero
        // and stops the draw.
        if grid.panels().is_empty() && self.count == 0 {
            return;
        }

        build_panel_instances_into(grid.panels(), self.metrics.scale_factor, &mut self.built);

        // A ridden panel keeps its slot in the buffer, because the fragment
        // shader self-occludes against every later instance, so a reorder changes
        // which box hides which. Only its drawn position moves, and the base draw
        // skips its slot so it lands after the composites instead.
        self.riding.clear();
        for (index, panel) in grid.panels().iter().enumerate() {
            let Some((host, _)) = panel.anchor else {
                continue;
            };
            let Some(ride) = anchored.iter().find(|ride| ride.host == host) else {
                continue;
            };
            self.built[index].cell[1] += ride.dy_px / self.metrics.height;
            self.riding.push((index as u32, ride.scissor));
        }

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
                render_pass.draw(0..6, next..index);
            }
            next = index + 1;
        }
        if next < self.count {
            render_pass.draw(0..6, next..self.count);
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
            render_pass.draw(0..6, index..index + 1);
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
/// The shadow geometry, the corner radius, and the inset are all stated in
/// logical pixels and multiplied by `scale_factor` here. Left physical, they
/// hold their pixel count against a box that doubled, so the chrome reads half
/// weight on a 2x display.
///
/// `out` is cleared first, so a reused scratch buffer holds only this frame's
/// panels.
fn build_panel_instances_into(panels: &[Panel], scale_factor: f32, out: &mut Vec<PanelInstance>) {
    out.clear();
    out.extend(panels.iter().map(|panel| {
        let (shadow_offset, shadow_margin, shadow_mode) = match panel.shadow {
            PanelShadow::Drop => (SHADOW_OFFSET, SHADOW_MARGIN, 0.0),
            PanelShadow::Tucked => ([0.0, 0.0], SHADOW_MARGIN_TUCKED, 1.0),
            PanelShadow::Overhang => ([0.0, 0.0], SHADOW_MARGIN_OVERHANG, 2.0),
            PanelShadow::None_ => ([0.0, 0.0], 0.0, 0.0),
        };
        PanelInstance {
            cell: [panel.left as f32, panel.top as f32],
            size: [panel.width as f32, panel.height as f32],
            fill: panel.fill.map(rgb_f32).unwrap_or([0.0, 0.0, 0.0]),
            border: rgb_f32(panel.border),
            shadow_offset: [
                shadow_offset[0] * scale_factor,
                shadow_offset[1] * scale_factor,
            ],
            shadow_margin: shadow_margin * scale_factor,
            corner_radius: panel.corner_radius as f32 * scale_factor,
            fill_flag: if panel.fill.is_some() { 1.0 } else { 0.0 },
            style: style_code(panel.style),
            inset_x: panel.inset_x as f32 * scale_factor,
            shadow_mode,
        }
    }));
}

fn style_code(style: BorderStyle) -> u32 {
    match style {
        BorderStyle::Light => 0,
        BorderStyle::Heavy => 1,
        BorderStyle::Double => 2,
        BorderStyle::Rounded => 3,
    }
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
    use super::{
        build_panel_instances, build_panel_instances_into, style_code, PanelInstance, PanelPass,
    };
    use crate::{gpu::headless_device, render::CellMetrics};
    use stoatty_term::grid::{BorderStyle, Grid, Panel, PanelShadow, Rgb};
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
    /// row exactly the 256-byte copy alignment, so the readback needs no stride
    /// padding.
    const TARGET: u32 = 64;

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_occlusion(include_str!(
            "../shaders/panel.wgsl"
        )))
        .expect("parse panel");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate panel");
    }

    /// Draw `grid`'s panels alone onto a [`TARGET`]-square target cleared to
    /// `clear`, and read the whole surface back.
    fn render_rgba(
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        metrics: CellMetrics,
        clear: Color,
    ) -> Vec<u8> {
        let mut pass = PanelPass::new(device, TextureFormat::Rgba8Unorm, metrics);
        pass.prepare(device, queue, grid, &[], [TARGET as f32, TARGET as f32]);

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
        let readback = device.create_buffer(&BufferDescriptor {
            label: Some("panel weight readback"),
            size: u64::from(TARGET) * u64::from(TARGET) * 4,
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

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
            // Both halves, in the order the frame chain records them, so this
            // measures the composite a real frame produces.
            pass.draw_under(&mut render_pass);
            pass.draw_stroke(&mut render_pass);
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

    /// The red the panels painted over black, one byte a pixel.
    ///
    /// The fixtures stroke in pure red, so the byte at a pixel is what reached
    /// the target there.
    fn render_red(device: &Device, queue: &Queue, grid: &Grid, metrics: CellMetrics) -> Vec<u8> {
        render_rgba(device, queue, grid, metrics, Color::BLACK)
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
        render_rgba(device, queue, grid, metrics, Color::WHITE)
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
        let Some((device, queue)) = headless_device() else {
            eprintln!("panel composite test: no wgpu adapter, skipping");
            return;
        };

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
            corner_radius: 0,
            // Unfilled, so the stroke's fringe meets the shadow with nothing
            // opaque between them.
            fill: None,
            shadow,
            inset_x: 0,
            above_pools: false,
            anchor: None,
            seq: 0,
        };
        let coverage = |shadow| {
            let mut grid = Grid::new(4, 4);
            grid.set_panels(vec![panel(shadow)]);
            render_coverage(&device, &queue, &grid, metrics)
        };

        // A tucked shadow paints nothing below the box's bottom edge while a
        // drop shadow is at full strength there, so the pair isolates the
        // stroke's outer fringe from the shadow under it. A shadowless panel
        // serves no better. Its quad carries no shadow padding, so it never
        // rasterizes the fringe outside the box at all.
        let tucked = coverage(PanelShadow::Tucked);
        let dropped = coverage(PanelShadow::Drop);

        // A shadow alone never reaches past SHADOW_ALPHA. So a coverage above
        // that which also exceeds the tucked panel's is neither of the two
        // inputs, and only compositing them produces it.
        let ceiling = (0.22 * 255.0) as u8;
        let composited = dropped.iter().zip(&tucked).position(|(dropped, tucked)| {
            (1..255).contains(tucked) && dropped > tucked && *dropped > ceiling
        });

        assert!(
            composited.is_some(),
            "the stroke's fringe and the shadow beneath it compose"
        );
    }

    /// A chrome weight left in physical pixels holds its pixel count while the
    /// box it frames doubles, so the frame reads half as heavy on a 2x display.
    #[test]
    fn a_heavy_stroke_doubles_its_width_at_twice_the_density() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("panel weight test: no wgpu adapter, skipping");
            return;
        };

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
            inset_x: 0,
            above_pools: false,
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
            let red = render_red(&device, &queue, &grid, metrics(scale_factor));
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
            inset_x: 0,
            above_pools: false,
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
            inset_x: 0,
            above_pools: false,
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
            inset_x: 0,
            above_pools: false,
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
            inset_x: 0,
            above_pools: false,
            anchor: None,
            seq: 0,
        }];

        let instances = build_panel_instances(&panels);

        assert_eq!(instances[0].fill_flag, 0.0);
        assert_eq!(instances[0].shadow_offset, [0.0, 0.0]);
        assert_eq!(instances[0].shadow_margin, 0.0);
        assert_eq!(instances[0].shadow_mode, 0.0);
    }

    #[test]
    fn tucked_panel_undisplaces_and_clips_the_shadow() {
        let panels = [Panel {
            top: 2,
            left: 2,
            width: 6,
            height: 3,
            style: BorderStyle::Light,
            border: Rgb::new(1, 2, 3),
            corner_radius: 0,
            fill: Some(Rgb::new(4, 5, 6)),
            shadow: PanelShadow::Tucked,
            inset_x: 4,
            above_pools: false,
            anchor: None,
            seq: 0,
        }];

        let instances = build_panel_instances(&panels);

        assert_eq!(instances[0].shadow_offset, [0.0, 0.0], "no displacement");
        assert_eq!(instances[0].shadow_margin, super::SHADOW_MARGIN_TUCKED);
        assert_eq!(instances[0].shadow_mode, 1.0, "clipped below the box");
    }
}
