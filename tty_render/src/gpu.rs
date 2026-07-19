//! The wgpu rendering context: owns the surface, device, and queue, and
//! drives a frame.
//!
//! Windowing-toolkit-agnostic. The surface is created from any handle the
//! app supplies via the raw-window-handle traits, so this crate never links
//! the windowing library; the app owns the window and hands its handle in.
//!
//! [`Renderer`] is the surface-free render core: it builds the grid passes and
//! draws into any texture view, so a frame can target an off-screen texture as
//! well as the window surface that [`GpuContext`] wraps.

pub use crate::render::{text::build_font_system, Frame, Scroll};
use crate::{
    perf::FrameProfiler,
    render::{
        background::{BackgroundPass, CursorState},
        bar::BarPass,
        decoration::DecorationPass,
        icon::IconPass,
        minimap::MinimapPass,
        overlay::OverlayPass,
        panel::PanelPass,
        text::TextPass,
        CellMetrics,
    },
};
use cosmic_text::FontSystem;
use futures::executor;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::{thread, time::Instant};
use stoatty_term::grid::{Grid, Panel, Rgb};
use wgpu::{
    Adapter, Color, CommandEncoderDescriptor, CompositeAlphaMode, CurrentSurfaceTexture, Device,
    DeviceDescriptor, Instance, InstanceDescriptor, LoadOp, Operations, PowerPreference,
    PresentMode, Queue, RenderPassColorAttachment, RenderPassDescriptor, RequestAdapterOptions,
    StoreOp, Surface, SurfaceConfiguration, TextureFormat, TextureUsages, TextureView,
    TextureViewDescriptor,
};
#[cfg(feature = "perf")]
use {
    crate::{
        perf::{FrameSample, FrameStats},
        render::hud::{self, HudPass},
    },
    std::{
        sync::{
            atomic::{AtomicBool, Ordering},
            Arc,
        },
        time::Duration,
    },
    wgpu::{
        Buffer, BufferDescriptor, BufferUsages, CommandEncoder, Features, MapMode, PollType,
        QuerySet, QuerySetDescriptor, QueryType, RenderPassTimestampWrites,
    },
};

/// Slots in the timestamp readback ring. Three lets a frame's timing be read
/// back when its slot cycles around, so `present` never waits on the GPU.
#[cfg(feature = "perf")]
const TIMER_SLOTS: usize = 3;

/// Bytes for one frame's two `u64` timestamp ticks.
#[cfg(feature = "perf")]
const TIMESTAMP_BYTES: u64 = 16;

/// GPU-side frame timing via a two-query timestamp set and a never-blocking
/// readback ring. Compiled only under the `perf` feature.
///
/// Each timed frame writes a begin/end timestamp around the frame render pass,
/// resolves the pair into a free ring slot, and maps that slot for read. The
/// result is picked up [`TIMER_SLOTS`] frames later when the slot cycles back,
/// so the present path never stalls on GPU completion. A slot whose map has
/// not landed by the time its turn returns is skipped for that frame.
#[cfg(feature = "perf")]
struct GpuTimer {
    query_set: QuerySet,
    period_ns: f32,
    slots: Vec<TimerSlot>,
    frame: usize,
}

#[cfg(feature = "perf")]
struct TimerSlot {
    resolve: Buffer,
    map: Buffer,
    ready: Arc<AtomicBool>,
    in_flight: bool,
}

#[cfg(feature = "perf")]
impl GpuTimer {
    fn new(device: &Device, queue: &Queue) -> GpuTimer {
        let query_set = device.create_query_set(&QuerySetDescriptor {
            label: Some("frame-timestamps"),
            ty: QueryType::Timestamp,
            count: 2,
        });
        let slots = (0..TIMER_SLOTS)
            .map(|_| TimerSlot {
                resolve: device.create_buffer(&BufferDescriptor {
                    label: Some("timestamp-resolve"),
                    size: TIMESTAMP_BYTES,
                    usage: BufferUsages::QUERY_RESOLVE | BufferUsages::COPY_SRC,
                    mapped_at_creation: false,
                }),
                map: device.create_buffer(&BufferDescriptor {
                    label: Some("timestamp-map"),
                    size: TIMESTAMP_BYTES,
                    usage: BufferUsages::MAP_READ | BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                }),
                ready: Arc::new(AtomicBool::new(false)),
                in_flight: false,
            })
            .collect();
        GpuTimer {
            query_set,
            period_ns: queue.get_timestamp_period(),
            slots,
            frame: 0,
        }
    }

    fn current_slot(&self) -> usize {
        self.frame % TIMER_SLOTS
    }

    /// The timestamp writes to hang on this frame's render pass.
    fn timestamp_writes(&self) -> RenderPassTimestampWrites<'_> {
        RenderPassTimestampWrites {
            query_set: &self.query_set,
            beginning_of_pass_write_index: Some(0),
            end_of_pass_write_index: Some(1),
        }
    }

    /// Read back this frame's slot if its map has landed, returning the GPU
    /// duration and freeing the slot to be written again.
    ///
    /// `None` when the slot's map has not completed yet or the slot was never
    /// written. After this returns, [`Self::slot_free`] reports whether the
    /// slot can take this frame's timestamps.
    fn take_ready(&mut self) -> Option<Duration> {
        let period = self.period_ns;
        let slot = &mut self.slots[self.frame % TIMER_SLOTS];
        if !slot.in_flight || !slot.ready.load(Ordering::Acquire) {
            return None;
        }
        let ticks: [u64; 2] = {
            let view = slot.map.slice(..).get_mapped_range();
            bytemuck::pod_read_unaligned(&view[..TIMESTAMP_BYTES as usize])
        };
        slot.map.unmap();
        slot.in_flight = false;
        slot.ready.store(false, Ordering::Release);
        let elapsed = ticks[1].saturating_sub(ticks[0]);
        Some(Duration::from_nanos(
            (elapsed as f64 * period as f64) as u64,
        ))
    }

    /// Whether this frame's slot is free to resolve into and re-map.
    fn slot_free(&self) -> bool {
        !self.slots[self.current_slot()].in_flight
    }

    /// Resolve this frame's two timestamps into its slot's buffers.
    fn resolve(&self, encoder: &mut CommandEncoder) {
        let slot = &self.slots[self.current_slot()];
        encoder.resolve_query_set(&self.query_set, 0..2, &slot.resolve, 0);
        encoder.copy_buffer_to_buffer(&slot.resolve, 0, &slot.map, 0, TIMESTAMP_BYTES);
    }

    /// Begin the async map of this frame's slot. A later device poll completes
    /// it, and the read happens when the slot cycles back.
    fn begin_map(&mut self) {
        let slot = &mut self.slots[self.frame % TIMER_SLOTS];
        let ready = slot.ready.clone();
        ready.store(false, Ordering::Release);
        slot.map.slice(..).map_async(MapMode::Read, move |result| {
            if result.is_ok() {
                ready.store(true, Ordering::Release);
            }
        });
        slot.in_flight = true;
    }

    /// Advance to the next slot. Called once per frame so each slot is written
    /// every [`TIMER_SLOTS`] frames and read back the next time around.
    fn advance(&mut self) {
        self.frame += 1;
    }
}

/// The device descriptor for `adapter`. Under the `perf` feature, requests
/// `TIMESTAMP_QUERY` when the adapter supports it so the renderer can measure
/// GPU frame time. Otherwise it is the default descriptor, requesting no
/// features.
fn device_descriptor(adapter: &Adapter) -> DeviceDescriptor<'static> {
    #[cfg(feature = "perf")]
    if adapter.features().contains(Features::TIMESTAMP_QUERY) {
        return DeviceDescriptor {
            required_features: Features::TIMESTAMP_QUERY,
            ..Default::default()
        };
    }
    let _ = adapter;
    DeviceDescriptor::default()
}

/// How the renderer renders text, passed to [`Renderer::new`] and
/// [`GpuContext::new`].
///
/// `size` is the logical font size in points; the physical rasterization size
/// is `size * scale_factor`, so a given `size` keeps its apparent size across
/// displays of different density. `family` is an ordered cascade whose first
/// entry present in the font db becomes the shaping primary, cosmic-text
/// falling back per glyph for codepoints it lacks.
#[derive(Clone, Copy)]
pub struct FontConfig<'a> {
    pub size: u32,
    pub scale_factor: f32,
    pub family: &'a [String],
    /// Whether the text pass shapes contiguous same-style cell runs together so
    /// the font's ligatures form. When false, each cell is shaped on its own.
    pub ligatures: bool,
}

/// The grid render passes and the target size, independent of any window.
///
/// [`Self::render_into`] draws a frame into any texture view, so the same render
/// path serves the window surface (via [`GpuContext`]) and an off-screen
/// texture. It does not own the device or queue; the caller passes them in,
/// which lets a test keep them to poll for completion.
pub struct Renderer {
    background: BackgroundPass,
    panel: PanelPass,
    decoration: DecorationPass,
    text: TextPass,
    overlay: OverlayPass,
    icon: IconPass,
    bar: BarPass,
    minimap: MinimapPass,
    /// Perf HUD overlay pass, composited topmost. Present only under `perf`.
    #[cfg(feature = "perf")]
    hud: HudPass,
    width: u32,
    height: u32,
    metrics: CellMetrics,
    /// Color cleared behind the grid each frame. Must equal the terminal's
    /// default cell background so the floored-grid gutter (the up-to-one-cell
    /// remainder on the right and bottom edges that no cell quad covers) stays
    /// indistinguishable from the grid.
    clear_color: Color,
    /// Cursor block color. The cursor pass applies its own blend alpha, so this
    /// is the opaque RGB only.
    cursor_color: Rgb,
    /// GPU frame timer, created lazily on the first render when the device was
    /// built with `TIMESTAMP_QUERY`. `None` until then or when unsupported.
    #[cfg(feature = "perf")]
    gpu_timer: Option<GpuTimer>,
    /// The most recent GPU duration read back from the timer, taken by
    /// [`GpuContext`] each frame to attach to the profiler sample.
    #[cfg(feature = "perf")]
    last_gpu: Option<Duration>,
}

impl Renderer {
    /// Build the grid passes for `format` at `size` (`[width, height]`) physical
    /// pixels, with cells sized and text shaped per `font`, clearing to
    /// `background` and drawing the cursor block in `cursor`.
    pub fn new(
        device: &Device,
        format: TextureFormat,
        size: [u32; 2],
        font_system: FontSystem,
        font: FontConfig<'_>,
        background: Rgb,
        cursor: Rgb,
    ) -> Renderer {
        let metrics = CellMetrics::from_font_size(font.size, font.scale_factor);
        Renderer {
            background: BackgroundPass::new(device, format, metrics),
            panel: PanelPass::new(device, format, metrics),
            decoration: DecorationPass::new(device, format, metrics),
            text: TextPass::new(
                device,
                format,
                metrics,
                font_system,
                font.family,
                font.ligatures,
            ),
            overlay: OverlayPass::new(device, format, metrics),
            icon: IconPass::new(device, format, metrics),
            bar: BarPass::new(device, format, metrics),
            minimap: MinimapPass::new(device, format, metrics),
            #[cfg(feature = "perf")]
            hud: HudPass::new(device, format),
            width: size[0],
            height: size[1],
            metrics,
            clear_color: rgb_to_color(background),
            cursor_color: cursor,
            #[cfg(feature = "perf")]
            gpu_timer: None,
            #[cfg(feature = "perf")]
            last_gpu: None,
        }
    }

    /// The (rows, cols) cell grid that fills the target at the current size.
    ///
    /// Divides the pixel size by the cell metrics, flooring with a one-cell
    /// minimum so a sliver still yields a usable grid.
    pub fn grid_size(&self) -> (usize, usize) {
        grid_dims(self.width, self.height, self.metrics)
    }

    /// Re-derive every pass's cell metrics from the logical `font_size` and
    /// `scale_factor`, so the next frame lays out and rasterizes the grid at the
    /// new size.
    ///
    /// The surface is untouched: only the cell rectangle changes, so a later
    /// [`Self::grid_size`] yields fewer cells for a larger font and more for a
    /// smaller one at the same pixel size.
    pub fn set_font_size(&mut self, font_size: u32, scale_factor: f32) {
        let metrics = CellMetrics::from_font_size(font_size, scale_factor);
        self.metrics = metrics;
        self.background.set_metrics(metrics);
        self.panel.set_metrics(metrics);
        self.decoration.set_metrics(metrics);
        self.text.set_metrics(metrics);
        self.overlay.set_metrics(metrics);
        self.icon.set_metrics(metrics);
        self.bar.set_metrics(metrics);
        self.minimap.set_metrics(metrics);
    }

    /// Draw a frame for `grid` into `view`: clear to the default background,
    /// fill each cell background, composite glyphs and decorations, tint the
    /// cursor cell, then draw overlays and their content on top.
    ///
    /// `cursor` is the cursor's position in fractional cell coordinates, or
    /// `None` when it is hidden. `scroll` carries the eased whole-grid and
    /// scroll-region offsets; `popover_scrolls` carries one content offset per
    /// overlay, in overlay order. Submits the frame but does not present or poll;
    /// the caller drives whichever it needs.
    pub fn render_into(
        &mut self,
        device: &Device,
        queue: &Queue,
        view: &TextureView,
        grid: &Grid,
        frame: Frame<'_>,
    ) {
        let resolution = [self.width as f32, self.height as f32];
        self.background.prepare(
            device,
            queue,
            grid,
            resolution,
            CursorState {
                corners: frame.cursor_corners,
                color: self.cursor_color,
            },
            frame.scroll.grid + frame.scroll.document + frame.scroll.scrollback,
            frame.damage,
        );
        self.decoration.prepare(
            device,
            queue,
            grid,
            resolution,
            frame.scroll.grid + frame.scroll.document + frame.scroll.scrollback,
            frame.decoration_damage,
        );
        self.text.prepare(device, queue, grid, resolution, &frame);
        self.panel.prepare(device, queue, grid, resolution);
        self.overlay.prepare(device, queue, grid, resolution);
        self.icon
            .prepare(device, queue, grid.icons(), grid.panels(), resolution);
        self.bar
            .prepare(device, queue, grid.bars(), grid.panels(), resolution);
        self.minimap.prepare(device, queue, grid, resolution);

        // Time this frame's GPU work when the timer's current slot is free.
        #[cfg(feature = "perf")]
        let timing = self.prepare_gpu_timing(device, queue);

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());

        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(self.clear_color),
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                #[cfg(feature = "perf")]
                timestamp_writes: timing.then(|| {
                    self.gpu_timer
                        .as_ref()
                        .expect("timer present when timing")
                        .timestamp_writes()
                }),
                #[cfg(not(feature = "perf"))]
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            self.background.draw(&mut render_pass);
            self.panel.draw(&mut render_pass);
            self.decoration.draw(&mut render_pass);
            self.text.draw(&mut render_pass);
            self.text.draw_region_text(&mut render_pass);
            // The region draw leaves its scissor set, so restore the full
            // surface before the cursor and overlay draws that follow.
            render_pass.set_scissor_rect(0, 0, self.width, self.height);
            // Off-grid color bars and text runs sit above the grid text but
            // below floating popovers and icons, like a gutter beneath a
            // tooltip; the bars fill behind the runs.
            self.bar.draw(&mut render_pass);
            // The minimap strip sits over the bars and below the cursor. It
            // scissors to each strip, so restore the full surface before the
            // text runs and cursor that follow.
            self.minimap.draw(&mut render_pass);
            render_pass.set_scissor_rect(0, 0, self.width, self.height);
            self.text.draw_text_runs(&mut render_pass);
            self.background.draw_cursor(&mut render_pass);
            self.overlay.draw(&mut render_pass);
            self.text.draw_overlay_text(&mut render_pass);
            // The overlay-content draw leaves its scissor set, so restore the
            // full surface before the icons draw on top of the overlays.
            render_pass.set_scissor_rect(0, 0, self.width, self.height);
            self.icon.draw(&mut render_pass);
        }

        #[cfg(feature = "perf")]
        if timing {
            self.gpu_timer
                .as_ref()
                .expect("timer present when timing")
                .resolve(&mut encoder);
        }

        queue.submit([encoder.finish()]);

        #[cfg(feature = "perf")]
        {
            if let Some(timer) = self.gpu_timer.as_mut() {
                if timing {
                    timer.begin_map();
                }
                timer.advance();
            }
            if timing {
                let _ = device.poll(PollType::Poll);
            }
        }
    }

    /// Ready the GPU timer for this frame. Creates it lazily when the device
    /// carries `TIMESTAMP_QUERY`, reads any completed measurement back into
    /// `last_gpu`, and returns whether this frame's slot is free to time.
    #[cfg(feature = "perf")]
    fn prepare_gpu_timing(&mut self, device: &Device, queue: &Queue) -> bool {
        if !device.features().contains(Features::TIMESTAMP_QUERY) {
            return false;
        }
        let timer = self
            .gpu_timer
            .get_or_insert_with(|| GpuTimer::new(device, queue));
        let gpu = timer.take_ready();
        let free = timer.slot_free();
        self.last_gpu = gpu;
        free
    }

    /// Take the most recent GPU frame duration the timer read back, if one
    /// landed. [`GpuContext`] consumes it each frame to attach to the profiler
    /// sample it belongs to.
    #[cfg(feature = "perf")]
    pub fn take_gpu_time(&mut self) -> Option<Duration> {
        self.last_gpu.take()
    }

    /// The glyph atlas content epoch, which changes on a grow or eviction.
    ///
    /// A caller compositing pools over a just-drawn live grid can compare this
    /// before and after to tell whether a pool pass moved the atlas UVs, leaving
    /// the live buffers it already drew stale.
    pub fn content_epoch(&self) -> u64 {
        self.text.content_epoch()
    }

    /// Composite `pool_grid`'s backgrounds and text over an already-rendered
    /// `view`, clipped to `scissor` and shifted up by `shift_rows` rows.
    ///
    /// Loads (does not clear) `view`, so it overwrites only the scissor
    /// rectangle with the pool's cells, leaving the live grid drawn elsewhere by
    /// a prior [`Self::render_into`] intact. `scissor` is `[x, y, width,
    /// height]` in physical pixels. `shift_rows` is the sub-cell document scroll
    /// applied to both passes so the composed pool glides pixel-by-pixel; pass a
    /// negative value to shift the rows up.
    ///
    /// Draws only the background and text passes: no cursor, decorations,
    /// regions, overlays, icons, or bars, since the pool carries plain composed
    /// page rows.
    #[allow(clippy::too_many_arguments)]
    pub fn composite_pool(
        &mut self,
        device: &Device,
        queue: &Queue,
        view: &TextureView,
        pool_grid: &Grid,
        panels: &[Panel],
        scissor: [u32; 4],
        shift_rows: f32,
        content_changed: bool,
        occludable: bool,
    ) {
        let Some(scissor) = clamp_scissor(scissor, self.width, self.height) else {
            return;
        };

        let resolution = [self.width as f32, self.height as f32];
        self.background.prepare_composite(
            device,
            queue,
            pool_grid,
            panels,
            resolution,
            shift_rows,
            content_changed,
            occludable,
        );
        self.text.prepare_composite(
            device,
            queue,
            pool_grid,
            panels,
            resolution,
            shift_rows,
            content_changed,
            occludable,
        );
        self.bar.prepare_composite(
            device,
            queue,
            pool_grid.bars(),
            panels,
            resolution,
            shift_rows,
            occludable,
        );

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());

        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("pool composite"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Load,
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            render_pass.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
            self.background.draw_composite(&mut render_pass);
            self.text.draw_composite(&mut render_pass);
            // Off-grid gutter chrome sits above the page glyphs but below the
            // cursor. Bars fill behind the scaled run text.
            self.bar.draw_composite(&mut render_pass);
            self.text.draw_composite_text_runs(&mut render_pass);
        }

        queue.submit([encoder.finish()]);
    }

    /// Draw the cursor block over an already-composited `view`, clipped to
    /// `scissor` when set.
    ///
    /// Loads `view` and draws only the cursor quad, so the block sits above the
    /// pool composites [`Self::composite_pool`] painted over the cursor's cell.
    /// `corners` is the eased block and `grid_scroll` matches the cell passes'
    /// offset. `scissor` is the cursor's pool region in physical pixels, holding
    /// the block to that surface as it sweeps.
    #[allow(clippy::too_many_arguments)]
    pub fn draw_cursor_over(
        &mut self,
        device: &Device,
        queue: &Queue,
        view: &TextureView,
        resolution: [f32; 2],
        corners: Option<[[f32; 2]; 4]>,
        grid_scroll: f32,
        scissor: Option<[u32; 4]>,
    ) {
        let scissor = if let Some(s) = scissor {
            let Some(clamped) = clamp_scissor(s, self.width, self.height) else {
                return;
            };
            Some(clamped)
        } else {
            None
        };

        self.background.prepare_cursor(
            queue,
            resolution,
            CursorState {
                corners,
                color: self.cursor_color,
            },
            grid_scroll,
        );

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());

        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("cursor over pools"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Load,
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            if let Some(s) = scissor {
                render_pass.set_scissor_rect(s[0], s[1], s[2], s[3]);
            }
            self.background.draw_cursor(&mut render_pass);
        }

        queue.submit([encoder.finish()]);
    }

    /// Composite the perf HUD topmost via a load-not-clear pass.
    ///
    /// Draws the previous frame's sample series over everything, including pool
    /// composites and the cursor, in its own encoder so the HUD's cost lands
    /// outside the timed grid pass rather than inflating the numbers it shows.
    #[cfg(feature = "perf")]
    pub fn draw_hud_over(
        &mut self,
        device: &Device,
        queue: &Queue,
        view: &TextureView,
        stats: &FrameStats,
        samples: &[FrameSample],
        resolution: [f32; 2],
    ) {
        self.hud.prepare(device, queue, samples, resolution);
        self.text.set_hud_text(
            device,
            queue,
            hud::readout_anchor(resolution),
            hud::READOUT_SCALE,
            &hud::readout_lines(stats),
        );

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("perf hud"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Load,
                        store: StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            self.hud.draw(&mut render_pass);
            self.text.draw_hud_text(&mut render_pass);
        }

        queue.submit([encoder.finish()]);
    }

    fn set_size(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }
}

/// One pool's contribution to a multi-pool frame: the grid to draw, the pixel
/// rectangle to clip it to, and the sub-cell row shift to glide it by.
///
/// [`GpuContext::render_with_pools`] composites these over the live grid in
/// slice order, so an earlier entry sits beneath a later one.
pub struct PoolComposite<'a> {
    /// The viewport-sized grid whose region cells hold the pool's composed page
    /// rows; only the [`Self::scissor`] rectangle is drawn.
    pub grid: &'a Grid,
    /// The clip rectangle `[x, y, width, height]` in physical pixels: the pool's
    /// region in screen space.
    pub scissor: [u32; 4],
    /// The sub-cell document scroll, in rows; a negative value shifts the rows
    /// up.
    pub shift_rows: f32,
    /// Whether the pool's composed rows differ from the previous frame. `false`
    /// during a pure sub-cell glide, letting the composite reuse the instances
    /// it built last frame and only re-apply the shift, rather than reshape and
    /// re-upload identical rows.
    pub content_changed: bool,
    /// Whether this pool sits under the modal boxes, so its composite is
    /// occluded by them. True for an editor-pane pool, which glides beneath any
    /// box. False for a pool that is itself a box's content, such as a finder or
    /// palette list easing.
    ///
    /// A false pool is never occluded, so a non-pane pool easing under a later
    /// box (a hints box over a still-easing palette list) can still bleed for
    /// the frames of the glide.
    pub occludable: bool,
}

/// Clamp `scissor` (`[x, y, width, height]` in physical pixels) to a
/// `width`x`height` render target, or `None` when nothing of it remains inside.
///
/// Pool and cursor scissors are sized from the app's grid, which can lag a live
/// resize and describe a rectangle larger than the freshly shrunk drawable.
/// wgpu aborts the process when a scissor exceeds the render target, so the
/// origin is pulled back to the target and the extent trimmed to what is left.
/// An origin at or past an edge, or a zero input extent, leaves an empty
/// rectangle the caller skips instead of encoding.
fn clamp_scissor(scissor: [u32; 4], width: u32, height: u32) -> Option<[u32; 4]> {
    let [x, y, w, h] = scissor;
    let x = x.min(width);
    let y = y.min(height);
    let w = w.min(width - x);
    let h = h.min(height - y);

    if w == 0 || h == 0 {
        return None;
    }

    Some([x, y, w, h])
}

/// The GPU swapchain wrapping a [`Renderer`] for an on-screen window.
///
/// Holds the surface configuration so [`Self::resize`] and the surface-loss
/// recovery in [`Self::render`] can re-`configure` without re-querying
/// capabilities.
pub struct GpuContext {
    surface: Surface<'static>,
    device: Device,
    queue: Queue,
    config: SurfaceConfiguration,
    /// The non-sRGB format the frame view is created with, so the passes'
    /// in-shader sRGB encoding is stored verbatim. Equals the surface format
    /// when that is already non-sRGB, or its non-sRGB sibling when only an sRGB
    /// surface format is available.
    view_format: TextureFormat,
    renderer: Renderer,
    perf: FrameProfiler,
    /// Whether to composite the perf HUD topmost. Toggled from the app.
    #[cfg(feature = "perf")]
    show_perf_hud: bool,
}

/// A [`FontSystem`] being built on a background thread, handed to
/// [`GpuContext::new`].
///
/// Enumerating the system fonts dominates startup and needs no window or GPU,
/// so the app starts it via [`Self::spawn`] before creating the window; the
/// font build then runs concurrently with the main-thread window and GPU setup
/// instead of after it.
pub struct FontLoad(thread::JoinHandle<FontSystem>);

impl FontLoad {
    /// Start building the font system on a background thread.
    pub fn spawn() -> FontLoad {
        FontLoad(thread::spawn(build_font_system))
    }

    /// Block until the font system is ready.
    fn join(self) -> FontSystem {
        self.0.join().expect("font system thread panicked")
    }
}

impl GpuContext {
    /// Build the context for `window`, sized to `width`x`height` physical
    /// pixels, with cells sized and text shaped per `font`, clearing to
    /// `background` and drawing the cursor block in `cursor`.
    ///
    /// `window` is anything carrying window and display handles; the surface
    /// takes ownership of it, so it must outlive the context (pass an
    /// `Arc`-wrapped window). Blocks on adapter and device acquisition, while the
    /// font system loads concurrently on a background thread, so startup costs
    /// the slower of the two rather than their sum.
    ///
    /// Panics if no GPU adapter is available even with the software fallback,
    /// device creation fails, or the surface cannot be created. All three are
    /// unrecoverable at startup.
    pub fn new<W>(
        window: W,
        width: u32,
        height: u32,
        font_load: FontLoad,
        font: FontConfig<'_>,
        background: Rgb,
        cursor: Rgb,
    ) -> GpuContext
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let instance = Instance::new(InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window)
            .expect("create wgpu surface");

        // Prefer a hardware adapter, but retry with a software rasterizer
        // (llvmpipe) before giving up, so a driverless or headless box still
        // starts rather than panicking on the first request.
        let t_adapter = Instant::now();
        let adapter = {
            let request = |force_fallback_adapter: bool| {
                executor::block_on(instance.request_adapter(&RequestAdapterOptions {
                    power_preference: PowerPreference::HighPerformance,
                    compatible_surface: Some(&surface),
                    force_fallback_adapter,
                }))
            };

            request(false).or_else(|_| request(true)).expect(
                "no compatible GPU adapter found: stoatty needs a hardware GPU \
                 (Metal, Vulkan, or DX12) or a software fallback, and neither \
                 was available",
            )
        };
        let adapter_time = t_adapter.elapsed();

        // Before request_device so a device-creation panic still records which
        // adapter was selected. driver/driver_info are empty on backends that do
        // not report them.
        let adapter_info = adapter.get_info();
        tracing::info!(
            name = %adapter_info.name,
            backend = ?adapter_info.backend,
            device_type = ?adapter_info.device_type,
            driver = %adapter_info.driver,
            driver_info = %adapter_info.driver_info,
            vendor = adapter_info.vendor,
            device = adapter_info.device,
            "gpu adapter",
        );

        let t_device = Instant::now();
        let (device, queue) =
            executor::block_on(adapter.request_device(&device_descriptor(&adapter)))
                .expect("GPU device creation failed on the selected adapter");
        let device_time = t_device.elapsed();

        // The text pass encodes its gamma-correct composite to sRGB in the
        // shader and the background pass writes already-encoded colors, so the
        // passes must render to a linear-store (non-sRGB) view. When only an
        // sRGB surface format is available, the surface keeps it but views
        // render through the non-sRGB sibling, so the hardware does not encode
        // sRGB a second time on top of the shader.
        let caps = surface.get_capabilities(&adapter);

        // Fifo blocks the present until the display consumes a frame, which is
        // what paces the redraw-requested animation loop at the refresh rate.
        // Mailbox never blocks, so the loop would spin unthrottled, burning a
        // core to render frames the display drops. With a frame latency of 1
        // the Fifo latency cost over Mailbox is at most one refresh.
        let present_mode = PresentMode::Fifo;

        let (surface_format, view_format) = surface_formats(&caps.formats);
        let view_formats = if view_format == surface_format {
            vec![]
        } else {
            vec![view_format]
        };

        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
            present_mode,
            alpha_mode: CompositeAlphaMode::Auto,
            view_formats,
            desired_maximum_frame_latency: 1,
        };
        let t_surface = Instant::now();
        surface.configure(&device, &config);
        let surface_time = t_surface.elapsed();

        let t_font = Instant::now();
        let font_system = font_load.join();
        let font_time = t_font.elapsed();

        let t_renderer = Instant::now();
        let renderer = Renderer::new(
            &device,
            view_format,
            [width, height],
            font_system,
            font,
            background,
            cursor,
        );
        let renderer_time = t_renderer.elapsed();

        // Always-on single line so a real launch can attribute cold-start cost.
        // The font wait is the residual after the concurrent scan. A small value
        // means the scan finished before the GPU was ready.
        tracing::info!(
            adapter = ?adapter_time,
            device = ?device_time,
            surface = ?surface_time,
            font_wait = ?font_time,
            renderer = ?renderer_time,
            "gpu init phases",
        );

        GpuContext {
            surface,
            device,
            queue,
            config,
            view_format,
            renderer,
            perf: FrameProfiler::new(),
            #[cfg(feature = "perf")]
            show_perf_hud: false,
        }
    }

    /// Re-configure the surface to `width`x`height` physical pixels.
    ///
    /// Zero-area sizes (e.g. a minimized window) are ignored, since
    /// configuring a surface with a zero dimension is invalid.
    pub fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }

        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
        self.renderer.set_size(width, height);
    }

    /// The (rows, cols) cell grid that fills the current surface.
    ///
    /// The app sizes the terminal and PTY to this so the shell's view matches
    /// what the renderer draws.
    pub fn grid_size(&self) -> (usize, usize) {
        self.renderer.grid_size()
    }

    /// Re-derive the renderer's cell metrics from the logical `font_size` and
    /// `scale_factor` for live resizing.
    ///
    /// The surface is left as-is, so the caller must re-read [`Self::grid_size`]
    /// and resize the terminal and PTY to match.
    pub fn set_font_size(&mut self, font_size: u32, scale_factor: f32) {
        self.renderer.set_font_size(font_size, scale_factor);
    }

    /// Draw a frame of `grid` to the window surface. `cursor` is the cursor's
    /// position in fractional cell coordinates, or `None` when it is hidden.
    /// `scroll` carries the eased whole-grid and scroll-region offsets;
    /// `popover_scrolls` carries one content offset per overlay, in overlay order.
    ///
    /// Skips the frame when the surface is transiently unavailable (timed
    /// out, occluded, or a validation error already raised elsewhere) and
    /// re-configures on an outdated or lost surface so the next frame
    /// recovers.
    ///
    /// When the acquired drawable's size disagrees with the configured size,
    /// the frame adopts the drawable's size so a live resize cannot trip
    /// scissor validation.
    pub fn render(&mut self, grid: &Grid, frame: Frame<'_>) {
        self.perf.begin_frame();
        let surface_frame = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(frame) | CurrentSurfaceTexture::Suboptimal(frame) => {
                frame
            },
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return;
            },
            CurrentSurfaceTexture::Timeout
            | CurrentSurfaceTexture::Occluded
            | CurrentSurfaceTexture::Validation => return,
        };
        self.perf.mark_acquired();

        self.adopt_drawable_size(
            surface_frame.texture.width(),
            surface_frame.texture.height(),
        );

        let view = surface_frame.texture.create_view(&TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });
        self.renderer
            .render_into(&self.device, &self.queue, &view, grid, frame);

        #[cfg(feature = "perf")]
        if self.show_perf_hud
            && let Some(stats) = self.perf.stats()
        {
            let samples = self.perf.samples();
            let resolution = [self.config.width as f32, self.config.height as f32];
            self.renderer.draw_hud_over(
                &self.device,
                &self.queue,
                &view,
                &stats,
                &samples,
                resolution,
            );
        }

        self.perf.mark_submitted();
        surface_frame.present();
        self.perf.end_frame();
        #[cfg(feature = "perf")]
        if let Some(gpu) = self.renderer.take_gpu_time() {
            self.perf.attach_gpu(gpu);
        }
    }

    /// Draw `live_grid` to the window surface, then composite each pool in
    /// `pools` over its scissor sub-rectangle, in one presented frame.
    ///
    /// `live_grid` and `frame` render exactly as [`Self::render`] does -- the
    /// static chrome and its cursor. Each [`PoolComposite`] then drives
    /// [`Renderer::composite_pool`] over the same view in slice order, so several
    /// eased pools (split panes, a modal over an editor) each overwrite only
    /// their own region and stack earlier-under-later. Every pass loads rather
    /// than clears, so the live grid is drawn first and the pools over it; an
    /// empty slice renders just the live grid.
    ///
    /// Skips and re-configures on the same transient surface states as
    /// [`Self::render`], and adopts the acquired drawable's size the same way,
    /// so its pool and cursor scissors stay within the render target during a
    /// live resize.
    ///
    /// Returns `true` when a pool composite grew or evicted from the glyph atlas
    /// after the live grid was drawn, so the live buffers just presented hold
    /// stale UVs. The caller should schedule another frame, on which the live
    /// prepare rebuilds them. Without it, an idle screen keeps the stale frame.
    pub fn render_with_pools(
        &mut self,
        live_grid: &Grid,
        frame: Frame<'_>,
        pools: &[PoolComposite<'_>],
        cursor_scissor: Option<[u32; 4]>,
    ) -> bool {
        self.perf.begin_frame();
        let surface_frame = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(frame) | CurrentSurfaceTexture::Suboptimal(frame) => {
                frame
            },
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return false;
            },
            CurrentSurfaceTexture::Timeout
            | CurrentSurfaceTexture::Occluded
            | CurrentSurfaceTexture::Validation => return false,
        };
        self.perf.mark_acquired();

        self.adopt_drawable_size(
            surface_frame.texture.width(),
            surface_frame.texture.height(),
        );

        let view = surface_frame.texture.create_view(&TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });

        let resolution = [self.config.width as f32, self.config.height as f32];
        let cursor_corners = frame.cursor_corners;
        let cursor_scroll = frame.scroll.grid + frame.scroll.document + frame.scroll.scrollback;

        // The live buffers are drawn against the atlas as it stands now. A pool
        // composite below can grow or evict from it, moving every UV, so capture
        // the epoch first to tell afterward whether the buffers just drawn have
        // gone stale.
        let epoch_before = self.renderer.content_epoch();

        // The pool composites paint over the cursor's cell, so the base draws
        // without its cursor block and the block is drawn on top afterward. The
        // ligature-break cell (`frame.cursor`) stays, keeping the live grid's
        // glyph under a chrome cursor broken out of any ligature.
        self.renderer.render_into(
            &self.device,
            &self.queue,
            &view,
            live_grid,
            Frame {
                cursor_corners: None,
                ..frame
            },
        );
        let panels = live_grid.panels();
        for pool in pools {
            self.renderer.composite_pool(
                &self.device,
                &self.queue,
                &view,
                pool.grid,
                panels,
                pool.scissor,
                pool.shift_rows,
                pool.content_changed,
                pool.occludable,
            );
        }

        // The pool loop is the only atlas-touching work after the live draw, so
        // a moved epoch here means the live buffers predate the change. The
        // cursor and HUD draws below never grow the atlas the same way, so they
        // stay outside this compare.
        let atlas_changed = self.renderer.content_epoch() != epoch_before;

        if cursor_corners.is_some() {
            self.renderer.draw_cursor_over(
                &self.device,
                &self.queue,
                &view,
                resolution,
                cursor_corners,
                cursor_scroll,
                cursor_scissor,
            );
        }

        #[cfg(feature = "perf")]
        if self.show_perf_hud
            && let Some(stats) = self.perf.stats()
        {
            let samples = self.perf.samples();
            self.renderer.draw_hud_over(
                &self.device,
                &self.queue,
                &view,
                &stats,
                &samples,
                resolution,
            );
        }

        self.perf.mark_submitted();
        surface_frame.present();
        self.perf.end_frame();
        #[cfg(feature = "perf")]
        if let Some(gpu) = self.renderer.take_gpu_time() {
            self.perf.attach_gpu(gpu);
        }

        atlas_changed
    }

    /// Adopt a drawable's `width`x`height` when it disagrees with the
    /// configured surface size.
    ///
    /// macOS live resize can hand back a drawable already at the layer's new
    /// size before the app processes the pending `Resized`. The frame then
    /// adopts the drawable's real size rather than tripping scissor validation,
    /// so every scissor derived from the surface size stays within the render
    /// target until the queued `Resized` re-fits the grid and PTY a moment
    /// later.
    fn adopt_drawable_size(&mut self, width: u32, height: u32) {
        if width == self.config.width && height == self.config.height {
            return;
        }

        self.config.width = width;
        self.config.height = height;
        self.renderer.set_size(width, height);
    }

    /// The per-frame timing recorder, read by the perf HUD.
    #[cfg(feature = "perf")]
    pub fn perf(&self) -> &FrameProfiler {
        &self.perf
    }

    /// Toggle whether the perf HUD is composited over subsequent frames.
    #[cfg(feature = "perf")]
    pub fn set_perf_hud(&mut self, on: bool) {
        self.show_perf_hud = on;
    }
}

/// The (rows, cols) that fill `width`x`height` physical pixels at `metrics`.
///
/// Floors each axis with a one-cell minimum so a sub-cell sliver still yields a
/// usable grid. A larger font (bigger cell) yields fewer cells for the same
/// pixel size.
fn grid_dims(width: u32, height: u32, metrics: CellMetrics) -> (usize, usize) {
    let rows = (height as f32 / metrics.height).floor().max(1.0) as usize;
    let cols = (width as f32 / metrics.width).floor().max(1.0) as usize;
    (rows, cols)
}

/// Choose the surface format and the format its views render through, from a
/// surface's supported formats.
///
/// The passes write sRGB-encoded colors, so views must render to a non-sRGB
/// target. Prefers a non-sRGB surface format, in which case the view format
/// equals it. When only sRGB formats are available (some Linux/Vulkan drivers,
/// never macOS/Metal), the surface keeps the sRGB format but views render
/// through its non-sRGB sibling, so the hardware does not encode sRGB twice.
fn surface_formats(available: &[TextureFormat]) -> (TextureFormat, TextureFormat) {
    let surface = available
        .iter()
        .copied()
        .find(|format| !format.is_srgb())
        .unwrap_or(available[0]);
    (surface, surface.remove_srgb_suffix())
}

/// Convert an [`Rgb`] to a wgpu [`Color`], normalizing each channel to 0..1
/// with an opaque alpha.
fn rgb_to_color(rgb: Rgb) -> Color {
    Color {
        r: rgb.r as f64 / 255.0,
        g: rgb.g as f64 / 255.0,
        b: rgb.b as f64 / 255.0,
        a: 1.0,
    }
}

/// Request a wgpu adapter and device with no surface, for off-screen rendering.
///
/// `None` when no adapter is available, so a GPU-less caller (such as a test in
/// headless CI) can skip rather than fail. Uses the same power preference and
/// device descriptor as [`GpuContext::new`].
pub fn headless_device() -> Option<(Device, Queue)> {
    let instance = Instance::new(InstanceDescriptor::new_without_display_handle());

    let adapter = executor::block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::HighPerformance,
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .ok()?;

    executor::block_on(adapter.request_device(&device_descriptor(&adapter))).ok()
}

#[cfg(test)]
mod tests {
    use super::{clamp_scissor, grid_dims, surface_formats, TextureFormat};
    use crate::render::CellMetrics;

    #[test]
    fn grid_dims_shrink_as_font_grows() {
        let dims = |font| grid_dims(800, 600, CellMetrics::from_font_size(font, 1.0));
        assert_eq!(dims(15), (33, 88));
        assert_eq!(dims(30), (16, 44));
        assert_eq!(dims(60), (8, 22));
    }

    #[test]
    fn clamp_scissor_keeps_in_bounds_trims_overhang_and_drops_empty() {
        assert_eq!(
            clamp_scissor([10, 10, 20, 20], 100, 100),
            Some([10, 10, 20, 20]),
            "an in-bounds rect passes through unchanged"
        );
        assert_eq!(
            clamp_scissor([90, 90, 20, 20], 100, 100),
            Some([90, 90, 10, 10]),
            "an overhanging rect keeps its origin and trims its extent to the edge"
        );
        assert_eq!(
            clamp_scissor([100, 10, 20, 20], 100, 100),
            None,
            "an origin at the right edge leaves nothing inside"
        );
        assert_eq!(
            clamp_scissor([10, 10, 0, 20], 100, 100),
            None,
            "a zero-width input is empty"
        );
    }

    #[test]
    fn surface_formats_prefer_non_srgb_then_fall_back_to_the_sibling() {
        assert_eq!(
            surface_formats(&[TextureFormat::Bgra8UnormSrgb, TextureFormat::Bgra8Unorm]),
            (TextureFormat::Bgra8Unorm, TextureFormat::Bgra8Unorm),
            "a non-sRGB format becomes both the surface and the view format"
        );

        assert_eq!(
            surface_formats(&[TextureFormat::Bgra8UnormSrgb, TextureFormat::Rgba8UnormSrgb]),
            (TextureFormat::Bgra8UnormSrgb, TextureFormat::Bgra8Unorm),
            "an sRGB-only surface keeps its format but views through the non-sRGB sibling"
        );
    }
}
