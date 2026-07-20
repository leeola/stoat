//! Instanced modal-chrome panel pass.
//!
//! Draws each [`Panel`] as a soft drop shadow confined to the box exterior, an
//! optional interior fill, and a hairline stroke frame with rounded corners.
//! Unlike the opaque overlay pass, a panel is chrome layered with the grid
//! rather than over it, so this pass runs before the grid text. The framed cells
//! render over the fill, and text outside the frame renders over the shadow. An
//! unfilled panel leaves its interior showing the grid beneath it.

use crate::render::CellMetrics;
use bytemuck::{Pod, Zeroable};
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

/// Drop-shadow blur radius in physical pixels. The shadow's alpha fades to zero
/// across this distance past the shadow rectangle.
const SHADOW_MARGIN: f32 = 16.0;

/// Drop-shadow displacement in physical pixels, down and to the right, so a
/// panel reads as floating above the grid rather than pasted onto it.
const SHADOW_OFFSET: [f32; 2] = [5.0, 7.0];

/// Blur radius in physical pixels for a tucked shadow. Tighter than
/// [`SHADOW_MARGIN`] so the undisplaced halo reads as a seam rather than a float.
const SHADOW_MARGIN_TUCKED: f32 = 6.0;

/// Height in physical pixels of an overhang shadow's interior bottom band. Small,
/// so it reads as a faint shadow cast onto the panel by whatever overhangs it.
const SHADOW_MARGIN_OVERHANG: f32 = 5.0;

/// The per-panel instance data. Carries the anchor cell, the size in cells, the
/// fill and stroke colors, the shadow displacement and blur radius, the corner
/// radius, a flag selecting whether the fill is painted, the border style code,
/// and the shadow mode (0 drop, 1 tucked, 2 overhang).
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
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
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    cell_size: [f32; 2],
    count: u32,
    _pad: [u32; 3],
}

/// The instanced panel pipeline and its per-frame buffers.
pub struct PanelPass {
    pipeline: RenderPipeline,
    bind_group_layout: BindGroupLayout,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    count: u32,
    metrics: CellMetrics,
}

impl PanelPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(device: &Device, format: TextureFormat, metrics: CellMetrics) -> PanelPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("panel"),
            source: ShaderSource::Wgsl(include_str!("../shaders/panel.wgsl").into()),
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

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("panel"),
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
            label: Some("panel globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let instances = alloc_instances(device, INITIAL_CAPACITY);
        let bind_group = make_bind_group(device, &bind_group_layout, &globals, &instances);

        PanelPass {
            pipeline,
            bind_group_layout,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
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
    pub fn prepare(&mut self, device: &Device, queue: &Queue, grid: &Grid, resolution: [f32; 2]) {
        let instances = build_panel_instances(grid.panels());
        self.count = instances.len() as u32;

        let globals = Globals {
            resolution,
            cell_size: [self.metrics.width, self.metrics.height],
            count: self.count,
            _pad: [0; 3],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        if instances.is_empty() {
            return;
        }

        if instances.len() > self.capacity {
            self.capacity = instances.len().next_power_of_two();
            self.instances = alloc_instances(device, self.capacity);
            self.bind_group = make_bind_group(
                device,
                &self.bind_group_layout,
                &self.globals,
                &self.instances,
            );
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
    }

    /// Record the panel draw into `render_pass`.
    ///
    /// A no-op when the grid carries no panel. Run before the grid text, so the
    /// framed cells render over the fill.
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

/// One instance per panel, in draw order. A panel with no fill leaves the
/// interior transparent. A panel with no shadow zeroes the shadow, so the pass
/// draws only the stroke.
fn build_panel_instances(panels: &[Panel]) -> Vec<PanelInstance> {
    panels
        .iter()
        .map(|panel| {
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
                shadow_offset,
                shadow_margin,
                corner_radius: panel.corner_radius as f32,
                fill_flag: if panel.fill.is_some() { 1.0 } else { 0.0 },
                style: style_code(panel.style),
                inset_x: panel.inset_x as f32,
                shadow_mode,
            }
        })
        .collect()
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
    use super::{build_panel_instances, style_code};
    use stoatty_term::grid::{BorderStyle, Panel, PanelShadow, Rgb};
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(include_str!("../shaders/panel.wgsl")).expect("parse panel");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate panel");
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
            seq: 0,
        }];

        let instances = build_panel_instances(&panels);

        assert_eq!(instances[0].shadow_offset, [0.0, 0.0], "no displacement");
        assert_eq!(instances[0].shadow_margin, super::SHADOW_MARGIN_TUCKED);
        assert_eq!(instances[0].shadow_mode, 1.0, "clipped below the box");
    }
}
