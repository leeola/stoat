//! Instanced perf-HUD pass.
//!
//! Draws the frame-time HUD from solid pixel-space rectangles. A translucent
//! panel backs one bar per retained [`FrameSample`], scaled so the 60 Hz and
//! 120 Hz frame budgets land at fixed heights, with a hairline at each budget.
//! The rectangles are in physical pixels, not cell fractions, so the HUD is a
//! fixed overlay that ignores font zoom. Compiled only under the `perf` feature.

use crate::perf::{FrameSample, FrameStats};
use bytemuck::{Pod, Zeroable};
use std::time::Duration;
use wgpu::{
    vertex_attr_array, BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingType, BlendState, Buffer, BufferBindingType, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, Device, FragmentState, PipelineLayoutDescriptor,
    Queue, RenderPass, RenderPipeline, RenderPipelineDescriptor, ShaderModuleDescriptor,
    ShaderSource, ShaderStages, TextureFormat, VertexBufferLayout, VertexState, VertexStepMode,
};

/// Instance buffer capacity, in rectangles, allocated up front. Grows by
/// doubling when a frame exceeds it.
const INITIAL_CAPACITY: usize = 256;

/// Outer margin from the surface's top-right corner, in pixels.
const MARGIN: f32 = 12.0;
/// Panel width and height in pixels. The height holds the graph plus the two
/// readout lines below it.
const PANEL_W: f32 = 260.0;
const PANEL_H: f32 = 128.0;
/// Inner padding between the panel edge and the graph, in pixels.
const PAD: f32 = 8.0;
/// Graph height in pixels. The readout occupies the panel below it.
const GRAPH_H: f32 = 56.0;
/// Gap in pixels between the graph bottom and the first readout line.
const READOUT_GAP: f32 = 6.0;
/// Text scale for the readout, relative to the body font.
pub(crate) const READOUT_SCALE: f32 = 0.7;
/// 60 Hz frame budget in milliseconds, drawn at the full graph height.
const BUDGET_60: f32 = 1000.0 / 60.0;
/// 120 Hz frame budget in milliseconds, drawn at half the graph height.
const BUDGET_120: f32 = 1000.0 / 120.0;

/// One HUD rectangle in physical pixels, with a pixel top-left, a pixel size,
/// and an rgba fill.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct HudInstance {
    origin: [f32; 2],
    size: [f32; 2],
    color: [f32; 4],
}

/// The surface resolution every instance's vertex shader maps pixel coordinates
/// through, shared as a uniform. Padded to a 16-byte uniform slot.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Globals {
    resolution: [f32; 2],
    _pad: [f32; 2],
}

/// The instanced HUD pipeline and its per-frame buffers.
pub struct HudPass {
    pipeline: RenderPipeline,
    globals: Buffer,
    bind_group: BindGroup,
    instances: Buffer,
    capacity: usize,
    count: u32,
}

impl HudPass {
    /// Build the pipeline targeting `format`, with an empty instance buffer.
    pub(crate) fn new(device: &Device, format: TextureFormat) -> HudPass {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("hud"),
            source: ShaderSource::Wgsl(include_str!("../shaders/hud.wgsl").into()),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("hud globals"),
            entries: &[BindGroupLayoutEntry {
                binding: 0,
                visibility: ShaderStages::VERTEX,
                ty: BindingType::Buffer {
                    ty: BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("hud"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&RenderPipelineDescriptor {
            label: Some("hud"),
            layout: Some(&layout),
            vertex: VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: Default::default(),
                buffers: &[VertexBufferLayout {
                    array_stride: size_of::<HudInstance>() as u64,
                    step_mode: VertexStepMode::Instance,
                    attributes: &vertex_attr_array![
                        0 => Float32x2,
                        1 => Float32x2,
                        2 => Float32x4,
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
            label: Some("hud globals"),
            size: size_of::<Globals>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group = device.create_bind_group(&BindGroupDescriptor {
            label: Some("hud globals"),
            layout: &bind_group_layout,
            entries: &[BindGroupEntry {
                binding: 0,
                resource: globals.as_entire_binding(),
            }],
        });

        let instances = alloc_instances(device, INITIAL_CAPACITY);

        HudPass {
            pipeline,
            globals,
            bind_group,
            instances,
            capacity: INITIAL_CAPACITY,
            count: 0,
        }
    }

    /// Upload the frame's uniform and one instance per HUD rectangle.
    ///
    /// `resolution` is the surface size in physical pixels. The panel anchors to
    /// its top-right corner. Reallocates the instance buffer only when the
    /// rectangle count outgrows the current capacity.
    pub(crate) fn prepare(
        &mut self,
        device: &Device,
        queue: &Queue,
        samples: &[FrameSample],
        resolution: [f32; 2],
    ) {
        let globals = Globals {
            resolution,
            _pad: [0.0, 0.0],
        };
        queue.write_buffer(&self.globals, 0, bytemuck::bytes_of(&globals));

        let instances = build_hud_instances(samples, resolution);
        self.count = instances.len() as u32;
        if instances.is_empty() {
            return;
        }

        if instances.len() > self.capacity {
            self.capacity = instances.len().next_power_of_two();
            self.instances = alloc_instances(device, self.capacity);
        }
        queue.write_buffer(&self.instances, 0, bytemuck::cast_slice(&instances));
    }

    /// Record the HUD draw into `render_pass`. A no-op with no instances.
    pub(crate) fn draw(&self, render_pass: &mut RenderPass<'_>) {
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
        label: Some("hud instances"),
        size: (capacity * size_of::<HudInstance>()) as u64,
        usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
        mapped_at_creation: false,
    })
}

/// The panel, the per-sample bars, and the two budget hairlines, in draw order
/// (later rectangles blend over earlier ones).
///
/// Bars run oldest-to-newest left-to-right and are colored green, yellow, or red
/// by which frame budget the CPU time crosses. Heights scale so [`BUDGET_60`]
/// fills the graph and [`BUDGET_120`] reaches half, clamped to the graph.
fn build_hud_instances(samples: &[FrameSample], resolution: [f32; 2]) -> Vec<HudInstance> {
    let panel_x = resolution[0] - PANEL_W - MARGIN;
    let panel_y = MARGIN;
    let graph_x = panel_x + PAD;
    let graph_y = panel_y + PAD;
    let graph_w = PANEL_W - 2.0 * PAD;
    let graph_bottom = graph_y + GRAPH_H;

    let mut out = Vec::with_capacity(samples.len() + 3);

    out.push(HudInstance {
        origin: [panel_x, panel_y],
        size: [PANEL_W, PANEL_H],
        color: [0.05, 0.05, 0.08, 0.85],
    });

    if !samples.is_empty() {
        let bar_w = graph_w / samples.len() as f32;
        for (index, sample) in samples.iter().enumerate() {
            let ms = sample.cpu().as_secs_f32() * 1000.0;
            let height = (ms / BUDGET_60 * GRAPH_H).clamp(0.0, GRAPH_H);
            out.push(HudInstance {
                origin: [graph_x + index as f32 * bar_w, graph_bottom - height],
                size: [bar_w, height],
                color: bar_color(ms),
            });
        }
    }

    for budget in [BUDGET_60, BUDGET_120] {
        let y = graph_bottom - budget / BUDGET_60 * GRAPH_H;
        out.push(HudInstance {
            origin: [graph_x, y],
            size: [graph_w, 1.0],
            color: [0.6, 0.6, 0.65, 0.7],
        });
    }

    out
}

fn bar_color(ms: f32) -> [f32; 4] {
    if ms <= BUDGET_120 {
        [0.3, 0.85, 0.4, 0.9]
    } else if ms <= BUDGET_60 {
        [0.9, 0.8, 0.3, 0.9]
    } else {
        [0.9, 0.3, 0.3, 0.95]
    }
}

/// The two readout lines below the graph. The first is the last and p95 CPU
/// frame time, the second the GPU frame time when the timestamp path has one.
pub(crate) fn readout_lines(stats: &FrameStats) -> Vec<String> {
    let ms = |d: Duration| d.as_secs_f32() * 1000.0;
    let gpu = match stats.last.gpu {
        Some(gpu) => format!("gpu {:.1}", ms(gpu)),
        None => "gpu --".to_string(),
    };
    vec![
        format!("cpu {:.1} / {:.1}", ms(stats.last.cpu()), ms(stats.cpu.p95)),
        gpu,
    ]
}

/// Top-left pixel of the readout text block, below the graph inside the panel.
pub(crate) fn readout_anchor(resolution: [f32; 2]) -> [f32; 2] {
    let panel_x = resolution[0] - PANEL_W - MARGIN;
    let panel_y = MARGIN;
    [panel_x + PAD, panel_y + PAD + GRAPH_H + READOUT_GAP]
}

#[cfg(test)]
mod tests {
    use super::{bar_color, build_hud_instances, readout_lines, BUDGET_120, BUDGET_60};
    use crate::perf::{FrameSample, FrameStats, Percentiles};
    use std::time::Duration;
    use wgpu::naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    };

    fn sample(cpu_ms: f32) -> FrameSample {
        FrameSample {
            acquire: Duration::from_secs_f32(cpu_ms / 1000.0),
            encode: Duration::ZERO,
            present: Duration::ZERO,
            interval: Duration::ZERO,
            gpu: None,
        }
    }

    #[test]
    fn shader_is_valid_wgsl() {
        let module = wgsl::parse_str(include_str!("../shaders/hud.wgsl")).expect("parse hud");
        Validator::new(ValidationFlags::all(), Capabilities::all())
            .validate(&module)
            .expect("validate hud");
    }

    #[test]
    fn instances_are_panel_bars_and_two_hairlines() {
        let samples = [sample(4.0), sample(20.0)];
        let instances = build_hud_instances(&samples, [800.0, 600.0]);
        assert_eq!(instances.len(), 1 + samples.len() + 2);
    }

    #[test]
    fn empty_ring_draws_only_panel_and_hairlines() {
        let instances = build_hud_instances(&[], [800.0, 600.0]);
        assert_eq!(instances.len(), 3);
    }

    #[test]
    fn bar_color_tracks_the_frame_budget() {
        assert_eq!(bar_color(BUDGET_120 - 1.0), [0.3, 0.85, 0.4, 0.9]);
        assert_eq!(bar_color(BUDGET_60 - 1.0), [0.9, 0.8, 0.3, 0.9]);
        assert_eq!(bar_color(BUDGET_60 + 1.0), [0.9, 0.3, 0.3, 0.95]);
    }

    #[test]
    fn readout_lines_format_cpu_and_optional_gpu() {
        let flat = |ms: u64| Percentiles {
            p50: Duration::from_millis(ms),
            p95: Duration::from_millis(ms),
            worst: Duration::from_millis(ms),
        };
        let mut stats = FrameStats {
            frames: 10,
            last: sample(8.0),
            cpu: flat(14),
            interval: flat(16),
            gpu: None,
        };
        let lines = readout_lines(&stats);
        assert_eq!(lines[0], "cpu 8.0 / 14.0");
        assert_eq!(lines[1], "gpu --");

        stats.last.gpu = Some(Duration::from_millis(2));
        assert_eq!(readout_lines(&stats)[1], "gpu 2.0");
    }
}
