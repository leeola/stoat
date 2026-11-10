//! Instanced floating-overlay pass.
//!
//! Draws each [`Overlay`] as a filled box in the corner radius, border width,
//! and drop shadow its chrome states, above the grid with its own z-order. Overlays are not cell
//! attributes: they float over the grid and occlude the cells beneath, so this pass runs last and
//! writes opaque pixels rather than alpha-blending into the text.
//!
//! This is the compositing layer for popovers and completion menus; the region
//! is the box itself, with any text inside it drawn separately.

use crate::render::{rgb_f32, CellMetrics, RowAnchor};
use bytemuck::{Pod, Zeroable};
use std::mem;
use stoatty_term::grid::{Grid, Overlay};
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingType, BlendState, Buffer, BufferBindingType, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor,
    Queue, RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in overlays, allocated up front. Grows by doubling
/// when a frame exceeds it.
const INITIAL_CAPACITY: usize = 16;

/// The data one overlay hands the vertex stage, a field to each vertex
/// attribute in order.
///
/// The cell and the size count cells. Every other length is in physical
/// pixels, from the overlay's chrome at the display's scale factor, and the
/// shadow's peak alpha is a fraction of 1.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct OverlayInstance {
    cell: [f32; 2],
    size: [f32; 2],
    fill: [f32; 3],
    border: [f32; 3],
    shadow_offset: [f32; 2],
    shadow_margin: f32,
    corner_radius: f32,
    anchor_offset: [f32; 2],
    border_width: f32,
    shadow_alpha: f32,
}

/// The uniform every instance shares, the surface resolution and cell size the
/// vertex shader maps cell coordinates through, and the display density.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    /// Physical pixels per logical pixel. Each instance's lengths are scaled
    /// by it before upload, so the shader reads no length through it.
    scale_factor: f32,
    _pad: f32,
}

/// The instanced overlay pipeline and its per-frame buffers.
pub struct OverlayPass {
    pipeline: RenderPipeline,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    /// The instances last uploaded, so an unchanged frame skips the write.
    last_instances: Vec<OverlayInstance>,
    /// Where each frame's instances are built, before being compared against
    /// [`Self::last_instances`] and traded with it. Chrome rebuilds every frame
    /// and changes on almost none of them, so building into a buffer the pass
    /// keeps spares an allocation the frame would otherwise discard.
    built: Vec<OverlayInstance>,
    count: u32,
    /// The uniform last written, so an unchanged frame skips that write too.
    last_globals: Option<Globals>,
    metrics: CellMetrics,
}

impl OverlayPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> OverlayPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("overlay"),
            source: ShaderSource::Wgsl(
                crate::render::with_shadow(include_str!("../shaders/overlay.wgsl")).into(),
            ),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("overlay globals"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX_FRAGMENT,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("overlay"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("overlay"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[Some(VertexBufferLayout {
                    array_stride: size_of::<OverlayInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x3,
                        3 => Float32x3,
                        4 => Float32x2,
                        5 => Float32,
                        6 => Float32,
                        7 => Float32x2,
                        8 => Float32,
                        9 => Float32,
                    ],
                })],
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
            label: Some("overlay globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("overlay globals"),
            layout: &bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        OverlayPass {
            pipeline,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
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

    /// Upload the frame's uniform and one instance per grid overlay.
    ///
    /// `resolution` is the surface size in physical pixels. `anchor` places the
    /// grid's last row, which a box that reaches it moves onto. Reallocates the
    /// instance buffer only when the overlay count outgrows the current
    /// capacity.
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        resolution: [f32; 2],
        anchor: RowAnchor,
    ) {
        // With no overlay to draw now and none drawn last frame, nothing reads this
        // pass's buffers, so the frame skips it without touching the GPU. The frame
        // that empties the list still runs, which is what drops the count to zero
        // and stops the draw.
        if grid.overlays().is_empty() && self.count == 0 {
            return;
        }

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            scale_factor: self.metrics.scale_factor,
            _pad: 0.0,
        };
        crate::render::upload_globals(queue, &self.globals, 0, globals, &mut self.last_globals);

        build_overlay_instances_into(
            grid.overlays(),
            self.metrics.scale_factor,
            anchor,
            self.metrics.height,
            &mut self.built,
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

    /// Record the overlay draw into `render_pass`.
    ///
    /// A no-op when the grid carries no overlay. Run last, after the grid and
    /// cursor, so overlays sit on top.
    pub fn draw(&self, render_pass: &mut RenderPass<'_>) {
        if self.count == 0 {
            return;
        }

        render_pass.set_pipeline(&self.pipeline);
        render_pass.set_bind_group(0, &self.bind_group, &[]);
        render_pass.set_vertex_buffer(0, self.instances.slice(..));
        render_pass.draw(0..6, 0..self.count);
    }
}

fn alloc_instances(device: &Device, capacity: usize) -> Buffer {
    device.create_buffer(&BufferDescriptor {
        label: Some("overlay instances"),
        size: (capacity * size_of::<OverlayInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// One instance per overlay, in draw order, into a vector of the caller's.
#[cfg(test)]
fn build_overlay_instances(overlays: &[Overlay], scale_factor: f32) -> Vec<OverlayInstance> {
    let mut instances = Vec::new();
    build_overlay_instances_into(
        overlays,
        scale_factor,
        RowAnchor::UNIFORM,
        1.0,
        &mut instances,
    );
    instances
}

/// Build into `out` one instance per overlay, in draw order.
///
/// Each overlay's chrome states its shadow geometry, corner radius, and border
/// width in logical pixels, as the anchor offset is, and all of them are
/// multiplied by `scale_factor` here. Left physical, they hold their pixel
/// count against a box that doubled, so a popover's chrome reads half weight
/// beside the panel chrome on a 2x display.
///
/// Each product rounds, so the box edges land on the whole pixels the cell
/// rectangle already uses. The border keeps at least one physical pixel, and
/// the shadow's peak alpha passes through as a fraction of 1.
///
/// A box that reaches the last row takes [`last_row_shift`] in its anchor
/// offset, for rows `cell_h` pixels tall that `anchor` places.
///
/// `out` is cleared first, so a reused scratch buffer holds only this frame's
/// overlays.
fn build_overlay_instances_into(
    overlays: &[Overlay],
    scale_factor: f32,
    anchor: RowAnchor,
    cell_h: f32,
    out: &mut Vec<OverlayInstance>,
) {
    out.clear();
    out.extend(overlays.iter().map(|overlay| {
        let chrome = overlay.chrome;
        let px = |logical: f32| (logical * scale_factor).round();
        OverlayInstance {
            cell: [overlay.left as f32, overlay.top as f32],
            size: [overlay.width as f32, overlay.height as f32],
            fill: rgb_f32(overlay.fill),
            border: rgb_f32(overlay.border),
            shadow_offset: [
                px(f32::from(chrome.shadow_offset[0])),
                px(f32::from(chrome.shadow_offset[1])),
            ],
            shadow_margin: px(f32::from(chrome.shadow_margin)),
            corner_radius: px(f32::from(chrome.corner_radius)),
            anchor_offset: [
                px(overlay.offset[0] as f32),
                px(overlay.offset[1] as f32) + last_row_shift(overlay, anchor, cell_h),
            ],
            border_width: px(f32::from(chrome.border_width)).max(1.0),
            shadow_alpha: f32::from(chrome.shadow_alpha) / 255.0,
        }
    }));
}

/// How far `overlay` moves to sit on the anchored last row, in pixels for rows
/// `cell_h` tall that `anchor` places.
///
/// A popover keeps the uniform pitch inside its box, so one that reaches the
/// last row moves as a whole rather than mapping each of its rows. Its rows
/// never overlap one another, and a content scroll never jumps at the
/// boundary. The box and its text both take this shift, which keeps the text
/// on the box.
pub(crate) fn last_row_shift(overlay: &Overlay, anchor: RowAnchor, cell_h: f32) -> f32 {
    anchor.last_row_shift(overlay.top as f32 + overlay.height as f32, cell_h)
}

#[cfg(test)]
mod tests {
    use super::{build_overlay_instances, build_overlay_instances_into, OverlayInstance};
    use crate::render::{GridLayout, RowAnchor};
    use stoatty_term::grid::{Overlay, PopoverChrome, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(&crate::render::with_shadow(include_str!(
            "../shaders/overlay.wgsl"
        )))
        .expect("parse overlay");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate overlay");
    }

    #[test]
    fn a_reused_overlay_scratch_holds_only_this_frame_s_overlays() {
        let overlays = [Overlay {
            top: 3,
            left: 5,
            width: 8,
            height: 4,
            fill: Rgb::new(255, 0, 0),
            border: Rgb::new(0, 255, 0),
            content_fg: Rgb::new(0, 0, 255),
            scale: 1,
            offset: [-4, 6],
            bold: false,
            chrome: PopoverChrome::CARD,
            center: false,
            content: "x".to_owned(),
            spans: Vec::new(),
        }];

        let mut scratch = build_overlay_instances(&overlays, 1.0);
        scratch.extend(build_overlay_instances(&overlays, 1.0));
        build_overlay_instances_into(&overlays, 1.0, RowAnchor::UNIFORM, 1.0, &mut scratch);

        assert_eq!(
            bytemuck::cast_slice::<OverlayInstance, u8>(&scratch),
            bytemuck::cast_slice::<OverlayInstance, u8>(&build_overlay_instances(&overlays, 1.0)),
            "reuse clears the stale overlays and rebuilds only the frame's own"
        );
    }

    #[test]
    fn overlay_instance_maps_anchor_size_and_colors() {
        let overlays = [Overlay {
            top: 3,
            left: 5,
            width: 8,
            height: 4,
            fill: Rgb::new(255, 0, 0),
            border: Rgb::new(0, 255, 0),
            content_fg: Rgb::new(0, 0, 255),
            scale: 1,
            offset: [-4, 6],
            bold: false,
            chrome: PopoverChrome::CARD,
            center: false,
            content: "x".to_owned(),
            spans: Vec::new(),
        }];

        let instances = build_overlay_instances(&overlays, 1.0);

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].cell, [5.0, 3.0]);
        assert_eq!(instances[0].size, [8.0, 4.0]);
        assert_eq!(instances[0].fill, [1.0, 0.0, 0.0]);
        assert_eq!(instances[0].border, [0.0, 1.0, 0.0]);
        assert_eq!(instances[0].shadow_offset, [5.0, 7.0]);
        assert_eq!(instances[0].shadow_margin, 16.0);
        assert_eq!(instances[0].corner_radius, 2.0);
        assert_eq!(instances[0].anchor_offset, [-4.0, 6.0]);
        assert_eq!(instances[0].border_width, 1.0);
        assert_eq!(instances[0].shadow_alpha, 82.0 / 255.0);

        // The chrome is stated in logical pixels, so a denser display draws the
        // same popover with proportionally larger chrome. The cell rectangle it
        // sits on grew by the same factor, and the alpha holds.
        let dense = build_overlay_instances(&overlays, 2.0);

        assert_eq!(dense[0].cell, [5.0, 3.0], "cells do not scale");
        assert_eq!(dense[0].shadow_offset, [10.0, 14.0]);
        assert_eq!(dense[0].shadow_margin, 32.0);
        assert_eq!(dense[0].corner_radius, 4.0);
        assert_eq!(dense[0].anchor_offset, [-8.0, 12.0]);
        assert_eq!(dense[0].border_width, 2.0);
        assert_eq!(dense[0].shadow_alpha, 82.0 / 255.0);
    }

    /// An overlay draws in the chrome it carries, the Pane Picker badge's here:
    /// a 10 px radius, a 2 px border, and a shadow 10 px down that fades over
    /// 36 px from a peak alpha of 140.
    #[test]
    fn an_overlay_takes_its_own_chrome() {
        let overlays = [Overlay {
            top: 0,
            left: 0,
            width: 9,
            height: 6,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(0, 0, 0),
            scale: 6,
            offset: [0, 0],
            bold: true,
            chrome: PopoverChrome {
                corner_radius: 10,
                border_width: 2,
                shadow_offset: [0, 10],
                shadow_margin: 36,
                shadow_alpha: 140,
            },
            center: true,
            content: "4".to_owned(),
            spans: Vec::new(),
        }];

        let instance = build_overlay_instances(&overlays, 1.0)[0];

        assert_eq!(
            (
                instance.corner_radius,
                instance.border_width,
                instance.shadow_offset,
                instance.shadow_margin,
                instance.shadow_alpha
            ),
            (10.0, 2.0, [0.0, 10.0], 36.0, 140.0 / 255.0),
        );
    }

    /// A popover that reaches the last row moves up onto it whole, and one that
    /// ends above that row stays.
    #[test]
    fn a_box_reaching_the_last_row_moves_onto_it() {
        let anchor = GridLayout {
            rows: 10,
            cols: 40,
            bottom_row_y: 172.0,
        }
        .row_anchor();
        let rows_tall = |height| Overlay {
            top: 5,
            left: 0,
            width: 8,
            height,
            fill: Rgb::new(0, 0, 0),
            border: Rgb::new(0, 0, 0),
            content_fg: Rgb::new(0, 0, 0),
            scale: 1,
            offset: [0, 6],
            bold: false,
            chrome: PopoverChrome::CARD,
            center: false,
            content: String::new(),
            spans: Vec::new(),
        };

        let mut built = Vec::new();
        build_overlay_instances_into(&[rows_tall(4), rows_tall(5)], 1.0, anchor, 20.0, &mut built);
        assert_eq!(
            built
                .iter()
                .map(|instance| instance.anchor_offset)
                .collect::<Vec<_>>(),
            [[0.0, 6.0], [0.0, -2.0]],
            "the box over rows 5 to 9 moves up the 8 pixels the last row rose",
        );
    }
}
