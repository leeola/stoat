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

pub use crate::render::{
    text::{build_font_system, shape_words},
    AnchoredPanel, Frame, Scroll,
};
use crate::{
    perf::FrameProfiler,
    render::{
        background::{BackgroundPass, CursorState},
        bar::BarPass,
        decoration::DecorationPass,
        grid_dims,
        icon::IconPass,
        image::ImagePass,
        minimap::MinimapPass,
        overlay::OverlayPass,
        panel::PanelPass,
        polyline::PolylinePass,
        sketch::SketchPass,
        text::TextPass,
        CellMetrics, Occluder, PoolOccluders,
    },
};
use cosmic_text::{fontdb, FontSystem};
use futures::executor;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::{mem, sync::Arc, thread, time::Instant};
use stoatty_term::{
    grid::{Grid, Panel, Rgb},
    term::Damage,
};
use wgpu::{
    Adapter, Color, CommandEncoder, CommandEncoderDescriptor, CompositeAlphaMode,
    CurrentSurfaceTexture, Device, DeviceDescriptor, Instance, InstanceDescriptor, LoadOp,
    Operations, PowerPreference, PresentMode, Queue, RenderPass, RenderPassColorAttachment,
    RenderPassDescriptor, RequestAdapterOptions, StoreOp, Surface, SurfaceConfiguration,
    TextureFormat, TextureUsages, TextureView, TextureViewDescriptor,
};
#[cfg(feature = "perf")]
use {
    crate::{
        perf::{FrameSample, FrameStats},
        render::hud::{self, HudPass},
    },
    std::{
        sync::atomic::{AtomicBool, Ordering},
        time::Duration,
    },
    wgpu::{
        Buffer, BufferDescriptor, BufferUsages, Features, MapMode, PollType, QuerySet,
        QuerySetDescriptor, QueryType, RenderPassTimestampWrites,
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

/// Where a frame's cursor block and panel frame strokes draw.
///
/// A plain frame draws them among the grid's layers, beneath the overlays. A
/// frame compositing pools cannot, because the pools paint over the cells they
/// sit on. Both have to follow the composites, so they are recorded after them.
///
/// That leaves the deferred strokes above the overlays and icons the frame
/// recorded earlier, where an inline frame puts them below. The cursor has
/// carried that same asymmetry since pools existed.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum CursorLayer {
    /// Among the grid's layers, below the overlays and icons.
    Inline,
    /// Not at all, leaving the caller to record it once the pools are down.
    Deferred,
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
    polyline: PolylinePass,
    sketch: SketchPass,
    minimap: MinimapPass,
    image: ImagePass,
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
    /// The frame's panel occluders, lent to every pass that occludes. Held here
    /// so a frame builds the list once and reuses the allocation across frames.
    occluders: Vec<Occluder>,
    /// The occluders of the pool being composited, lent to all four of its composite
    /// passes. Separate from [`Self::occluders`] because a pool's list is filtered to
    /// what covers that pool, while the live grid's is every panel, and a frame that
    /// composites pools prepares the live grid too.
    pool_occluders: Vec<Occluder>,
    /// The pools this frame's panels ride, built beside [`Self::occluders`] from
    /// the same anchors and reused for the same reason.
    ///
    /// A ridden panel draws shifted, after the composites, so its rect on the
    /// live grid is stale for the whole glide. Every occluder list drops it
    /// rather than punch a hole where it no longer is.
    riding: Vec<u32>,
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
            polyline: PolylinePass::new(device, format, metrics),
            sketch: SketchPass::new(device, format, metrics),
            minimap: MinimapPass::new(device, format, metrics),
            image: ImagePass::new(device, format, metrics),
            #[cfg(feature = "perf")]
            hud: HudPass::new(device, format),
            width: size[0],
            height: size[1],
            metrics,
            clear_color: rgb_to_color(background),
            cursor_color: cursor,
            occluders: Vec::new(),
            pool_occluders: Vec::new(),
            riding: Vec::new(),
            #[cfg(feature = "perf")]
            gpu_timer: None,
            #[cfg(feature = "perf")]
            last_gpu: None,
        }
    }

    /// The font database this renderer's font system holds, for a second one to
    /// build its own from without a second scan of the system's fonts.
    pub fn fonts(&self) -> SharedFonts {
        self.text.fonts()
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
        self.polyline.set_metrics(metrics);
        self.sketch.set_metrics(metrics);
        self.minimap.set_metrics(metrics);
        self.image.set_metrics(metrics);
    }

    /// Re-point the two colors baked in at construction, the surface clear and
    /// the cursor tint.
    ///
    /// Every other color arrives per frame with the grid, so a theme change only
    /// has to reach these two to take effect on the next draw.
    pub fn set_theme_colors(&mut self, background: Rgb, cursor: Rgb) {
        self.clear_color = rgb_to_color(background);
        self.cursor_color = cursor;
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
        self.prepare_frame(device, queue, grid, &frame, &[]);
        self.record_into(device, queue, view);
    }

    /// Record and submit a frame whose buffers are already prepared, into
    /// `view`.
    ///
    /// Split out of [`Self::render_into`] because a caller waiting on a surface
    /// drawable prepares before it holds a view, and still has to record
    /// through the same path afterward.
    pub(crate) fn record_into(&mut self, device: &Device, queue: &Queue, view: &TextureView) {
        // Time this frame's GPU work when the timer's current slot is free.
        #[cfg(feature = "perf")]
        let timing = self.prepare_gpu_timing(device, queue);
        #[cfg(not(feature = "perf"))]
        let timing = false;

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = self.begin_frame_pass(&mut encoder, view, timing);
            self.record_frame(&mut render_pass, CursorLayer::Inline);
        }
        self.finish_frame(device, queue, encoder, timing);
    }

    /// Draw a frame for `grid` with `pools` composited over it, into `view`.
    ///
    /// The headless twin of [`GpuContext::render_with_pools`]. Every pass
    /// prepares before any of them draws, which is what a frame carrying pools
    /// needs and what compositing them one at a time through
    /// [`Self::composite_pool`] cannot give. Submits the frame but does not
    /// present or poll.
    ///
    /// The deferred cursor is not drawn. A caller that wants one runs
    /// [`Self::draw_cursor_over`] afterward, as it does after a composite.
    #[allow(clippy::too_many_arguments)]
    pub fn render_pools_into(
        &mut self,
        device: &Device,
        queue: &Queue,
        view: &TextureView,
        grid: &Grid,
        frame: Frame<'_>,
        pools: &[PoolComposite<'_>],
        anchored: &[AnchoredPanel],
    ) {
        self.prepare_frame(device, queue, grid, &frame, anchored);

        let panels = grid.panels();
        let riding = mem::take(&mut self.riding);
        for (slot, pool) in pools.iter().enumerate() {
            if self.pool_scissor(pool.scissor).is_none() {
                continue;
            }
            self.prepare_pool(
                device,
                queue,
                pool.grid,
                panels,
                &riding,
                pool.shift_rows,
                pool.origin_cells,
                pool.content_changed,
                pool.scrolled_rows,
                pool.occludable,
                pool.id,
                slot,
            );
        }
        self.riding = riding;

        #[cfg(feature = "perf")]
        let timing = self.prepare_gpu_timing(device, queue);
        #[cfg(not(feature = "perf"))]
        let timing = false;

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = self.begin_frame_pass(&mut encoder, view, timing);
            self.record_frame(&mut render_pass, CursorLayer::Deferred);

            // A ridden panel goes over the pane composites it floats above, and
            // under the non-pane composites, which are the box content it
            // frames, exactly as the surface path orders them.
            for (slot, pool) in pools.iter().enumerate() {
                if pool.occludable
                    && let Some(scissor) = self.pool_scissor(pool.scissor)
                {
                    self.record_pool(&mut render_pass, scissor, pool.id, slot);
                }
            }
            self.record_riding_panels(&mut render_pass);
            for (slot, pool) in pools.iter().enumerate() {
                if !pool.occludable
                    && let Some(scissor) = self.pool_scissor(pool.scissor)
                {
                    self.record_pool(&mut render_pass, scissor, pool.id, slot);
                }
            }
        }
        self.finish_frame(device, queue, encoder, timing);
    }

    /// Upload every live-grid pass's buffers for `frame`, touching no encoder.
    ///
    /// Split from the recording so a caller compositing pools can prepare the live
    /// grid and the pools before any of them draws, which is what lets the whole
    /// frame share one render pass. [`Self::record_frame`] issues the draws these
    /// buffers back, and must run before the next prepare overwrites them.
    pub(crate) fn prepare_frame(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        frame: &Frame<'_>,
        anchored: &[AnchoredPanel],
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
            frame.scrolled_rows,
        );
        self.decoration.prepare(
            device,
            queue,
            grid,
            resolution,
            frame.scroll.grid + frame.scroll.document + frame.scroll.scrollback,
            frame.decoration_damage,
            frame.scrolled_rows,
        );
        // Built once here rather than per pass, since every pass that occludes
        // derives the same list from the same panels.
        self.riding.clear();
        self.riding.extend(anchored.iter().map(|ride| ride.host));
        crate::render::build_occluders_into(grid.panels(), &self.riding, &mut self.occluders);

        self.text
            .prepare(device, queue, grid, resolution, frame, &self.occluders);
        self.panel
            .prepare(device, queue, grid, anchored, resolution);
        self.overlay.prepare(device, queue, grid, resolution);
        self.icon
            .prepare(device, queue, grid.icons(), &self.occluders, resolution);
        self.bar
            .prepare(device, queue, grid.bars(), &self.occluders, resolution);
        self.polyline
            .prepare(device, queue, grid, &self.occluders, resolution);
        self.sketch.prepare(
            device,
            queue,
            grid,
            frame.sketch_progress,
            anchored,
            &self.occluders,
            resolution,
        );
        self.minimap
            .prepare(device, queue, grid, &self.occluders, resolution);
        self.image.prepare(device, queue, grid, resolution);
    }

    /// Open the frame's render pass on `encoder`, clearing `view` to the theme
    /// background and hanging this frame's timestamp writes on it when `timing`.
    ///
    /// The only pass a frame opens, so everything drawn over the live grid records
    /// into it rather than reloading the attachment in a pass of its own.
    fn begin_frame_pass<'pass>(
        &self,
        encoder: &'pass mut CommandEncoder,
        view: &'pass TextureView,
        timing: bool,
    ) -> RenderPass<'pass> {
        let _ = timing;
        encoder.begin_render_pass(&RenderPassDescriptor {
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
        })
    }

    /// Issue the live grid's draws into `render_pass`, in layer order.
    ///
    /// Leaves the full-surface scissor set, so whatever records next starts from a
    /// clean clip rather than the last scissored draw's.
    /// Record the panels riding a compositing pool, each clipped to its host.
    ///
    /// [`Self::record_frame`] left their slots out, so this is where they land.
    /// The scissor is restored to the full surface afterward, since the draws
    /// that follow set their own.
    pub(crate) fn record_riding_panels(&self, render_pass: &mut RenderPass<'_>) {
        self.panel.draw_riding_under(render_pass);
        self.sketch.draw_riding(render_pass);
        render_pass.set_scissor_rect(0, 0, self.width, self.height);
    }

    /// Record every panel's frame stroke, for a frame that deferred them.
    ///
    /// Run after the pool composites. They paint over the cells a frame sits on,
    /// so a stroke recorded before them is a stroke a gliding pane erases.
    ///
    /// Leaves the full-surface scissor set, since the riding strokes each apply
    /// their own and the draws that follow set theirs.
    pub(crate) fn record_panel_strokes(&self, render_pass: &mut RenderPass<'_>) {
        self.panel.draw_stroke(render_pass);
        self.panel.draw_riding_stroke(render_pass);
        render_pass.set_scissor_rect(0, 0, self.width, self.height);
    }

    pub(crate) fn record_frame(&self, render_pass: &mut RenderPass<'_>, cursor: CursorLayer) {
        self.background.draw(render_pass);
        // A placement at a negative z sits behind the text, which is one of the
        // two sides a z-index can name to a terminal that draws its text in one
        // pass.
        self.image.draw_under(render_pass);
        // The panel's body only. Its frame stroke records after the text below,
        // so glyph ink reaching a cell edge cannot break the line around it.
        self.panel.draw_under(render_pass);
        self.text.draw(render_pass);
        self.text.draw_region_text(render_pass);
        // The region draw leaves its scissor set, so restore the full
        // surface before the decoration, cursor, and overlay draws that follow.
        render_pass.set_scissor_rect(0, 0, self.width, self.height);
        // The other side of the z-index. Stoatty's own chrome records later
        // still, so a panel or popover stays over an image a client placed.
        self.image.draw_over(render_pass);
        // Cell borders frame the cells they surround, so they draw over the
        // glyphs rather than under them. Ink that reaches a cell edge would
        // otherwise break the line it sits inside.
        self.decoration.draw(render_pass);
        // Off-grid color bars and text runs sit above the grid text but
        // below floating popovers and icons, like a gutter beneath a
        // tooltip. The bars fill behind the runs.
        self.bar.draw(render_pass);
        // Stroked paths draw over the bars they share a layer with, so a
        // commit graph's lines sit above any gutter fill behind them.
        self.polyline.draw(render_pass);
        // A hand-drawn mark annotates what the chrome beneath it shows, so it
        // draws over the stroked paths rather than under them.
        self.sketch.draw(render_pass);
        // The minimap strip sits over the bars and below the cursor. It
        // scissors to each strip, so restore the full surface before the
        // text runs and cursor that follow.
        self.minimap.draw(render_pass);
        render_pass.set_scissor_rect(0, 0, self.width, self.height);
        self.text.draw_text_runs(render_pass);
        // The frame the body above belongs to, over everything it surrounds. A
        // deferred frame leaves it to the caller for the cursor's reason: the
        // pool composites paint over the cells it sits on.
        if cursor == CursorLayer::Inline {
            self.panel.draw_stroke(render_pass);
            self.background.draw_cursor(render_pass);
        }
        self.overlay.draw(render_pass);
        self.text.draw_overlay_text(render_pass);
        // The overlay-content draw leaves its scissor set, so restore the
        // full surface before the icons draw on top of the overlays.
        render_pass.set_scissor_rect(0, 0, self.width, self.height);
        self.icon.draw(render_pass);
    }

    /// Resolve this frame's timestamps, submit `encoder`, and advance the timer.
    ///
    /// `timing` must be what [`Self::begin_frame_pass`] was given, since the
    /// resolve is only valid for a pass that carried the writes.
    fn finish_frame(
        &mut self,
        device: &Device,
        queue: &Queue,
        encoder: CommandEncoder,
        timing: bool,
    ) {
        // Only the perf build reads these, discarded here rather than threading a
        // cfg through the signature.
        let _ = (device, timing);

        #[cfg(feature = "perf")]
        let mut encoder = encoder;

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
    ///
    /// `scrolled_rows` is how far the pool's content moved since the frame that
    /// last composited it, which lets the passes carry the rows it kept. `None`
    /// means nothing carries and every row is shaped again.
    ///
    /// `pool` is the terminal's id for this pool, under which each pass keeps its
    /// composite buffers. Two entries sharing an id would have the later one's
    /// instances drawn for both, so callers pass each pool its own.
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
        origin_cells: [f32; 2],
        content_changed: bool,
        scrolled_rows: Option<isize>,
        occludable: bool,
        pool: u32,
        slot: usize,
    ) {
        let Some(scissor) = clamp_scissor(scissor, self.width, self.height) else {
            return;
        };

        self.prepare_pool(
            device,
            queue,
            pool_grid,
            panels,
            // A standalone composite has no frame of rides around it.
            &[],
            shift_rows,
            origin_cells,
            content_changed,
            scrolled_rows,
            occludable,
            pool,
            slot,
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
            self.record_pool(&mut render_pass, scissor, pool, slot);
        }

        queue.submit([encoder.finish()]);
    }

    /// Upload every composite pass's buffers for one pool, touching no encoder.
    ///
    /// Split from the recording for the same reason as [`Self::prepare_frame`]: a
    /// frame compositing several pools prepares them all before any draws, so they
    /// can share one render pass. The buffers a pool prepares are its own, keyed by
    /// `pool`, so preparing the next one does not disturb them.
    ///
    /// `riding` names the pools this frame's panels are anchored to. A panel
    /// riding one of them draws shifted, after the composites, so the rect it
    /// declared is not where it is and this pool must not occlude against it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn prepare_pool(
        &mut self,
        device: &Device,
        queue: &Queue,
        pool_grid: &Grid,
        panels: &[Panel],
        riding: &[u32],
        shift_rows: f32,
        origin_cells: [f32; 2],
        content_changed: bool,
        scrolled_rows: Option<isize>,
        occludable: bool,
        pool: u32,
        slot: usize,
    ) {
        let resolution = [self.width as f32, self.height as f32];

        // All four passes occlude this pool against the same panels, so the list
        // is built once here rather than in each of them. It cannot share
        // `self.occluders`: that holds the live grid's list, which the live
        // passes still read on a frame that also composites pools.
        //
        // Every pool of a frame builds the same list and reads as much of it as
        // its own occludability names, so a second pool leaves the first one's
        // bytes alone and the upload dedup recognizes them.
        let above = crate::render::pool_occluders_into(panels, riding, &mut self.pool_occluders);
        let occluders = PoolOccluders::new(&self.pool_occluders, above, occludable);

        self.background.prepare_composite(
            device,
            queue,
            pool_grid,
            occluders,
            resolution,
            shift_rows,
            origin_cells,
            content_changed,
            pool,
            slot,
        );
        self.text.prepare_composite(
            device,
            queue,
            pool_grid,
            occluders,
            resolution,
            shift_rows,
            origin_cells,
            content_changed,
            scrolled_rows,
            pool,
            slot,
        );
        self.bar.prepare_composite(
            device,
            queue,
            pool_grid.bars(),
            occluders,
            resolution,
            shift_rows,
            origin_cells,
            content_changed,
            pool,
            slot,
        );
        self.polyline.prepare_composite(
            device,
            queue,
            pool_grid.polylines(),
            occluders,
            resolution,
            shift_rows,
            origin_cells,
            content_changed,
            pool,
            slot,
        );
    }

    /// A pool's clip rectangle trimmed to the render target, or `None` when it
    /// falls entirely outside and the pool has nothing to draw.
    pub(crate) fn pool_scissor(&self, scissor: [u32; 4]) -> Option<[u32; 4]> {
        clamp_scissor(scissor, self.width, self.height)
    }

    /// Issue one pool's composite draws into `render_pass`, clipped to `scissor`.
    ///
    /// `scissor` must already be clamped to the render target, and `pool` and `slot`
    /// must be what [`Self::prepare_pool`] was given. Leaves the scissor set, so a
    /// caller recording anything else afterward restores the full surface first.
    pub(crate) fn record_pool(
        &self,
        render_pass: &mut RenderPass<'_>,
        scissor: [u32; 4],
        pool: u32,
        slot: usize,
    ) {
        render_pass.set_scissor_rect(scissor[0], scissor[1], scissor[2], scissor[3]);
        self.background.draw_composite(render_pass, pool, slot);
        self.text.draw_composite(render_pass, pool, slot);
        // Off-grid gutter chrome sits above the page glyphs but below the
        // cursor. Bars fill behind the scaled run text.
        self.bar.draw_composite(render_pass, pool, slot);
        self.polyline.draw_composite(render_pass, pool, slot);
        self.text.draw_composite_text_runs(render_pass, pool, slot);
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

        self.prepare_cursor_over(queue, resolution, corners, grid_scroll);

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
            self.record_cursor_over(&mut render_pass, scissor);
        }

        queue.submit([encoder.finish()]);
    }

    /// Upload the cursor block's globals, touching no encoder.
    ///
    /// Writes the same globals slot the live grid's draws read, so it must run after
    /// the live prepare rather than before it.
    pub(crate) fn prepare_cursor_over(
        &mut self,
        queue: &Queue,
        resolution: [f32; 2],
        corners: Option<[[f32; 2]; 4]>,
        grid_scroll: f32,
    ) {
        self.background.prepare_cursor(
            queue,
            resolution,
            CursorState {
                corners,
                color: self.cursor_color,
            },
            grid_scroll,
        );
    }

    /// Issue the cursor block's draw into `render_pass`, clipped to `scissor` when
    /// one is given. Already-clamped, as [`Self::draw_cursor_over`] leaves it.
    pub(crate) fn record_cursor_over(
        &self,
        render_pass: &mut RenderPass<'_>,
        scissor: Option<[u32; 4]>,
    ) {
        if let Some(s) = scissor {
            render_pass.set_scissor_rect(s[0], s[1], s[2], s[3]);
        }
        self.background.draw_cursor(render_pass);
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
        self.prepare_hud_over(device, queue, stats, samples, resolution);

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
            self.record_hud_over(&mut render_pass);
        }

        queue.submit([encoder.finish()]);
    }

    /// Upload the HUD's series and readout text, touching no encoder.
    #[cfg(feature = "perf")]
    pub(crate) fn prepare_hud_over(
        &mut self,
        device: &Device,
        queue: &Queue,
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
    }

    /// Issue the HUD's draws into `render_pass`, over whatever it already holds.
    #[cfg(feature = "perf")]
    pub(crate) fn record_hud_over(&self, render_pass: &mut RenderPass<'_>) {
        self.hud.draw(render_pass);
        self.text.draw_hud_text(render_pass);
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
    /// The pool this entry draws, as the terminal declared it. Identifies the
    /// pool's composite buffers across frames, whose set of gliding pools changes
    /// shape as pools settle and start.
    pub id: u32,
    /// The grid holding the pool's composed page rows, sized to its region
    /// rather than to the viewport.
    pub grid: &'a Grid,
    /// The screen cell [`Self::grid`]'s own (0, 0) draws at, which is the
    /// region's top-left. The passes add it in their vertex stages, so the grid
    /// never has to be copied into a viewport-sized one to be placed.
    pub origin_cells: [f32; 2],
    /// The pool's region in screen space, as the clip rectangle
    /// `[x, y, width, height]` in physical pixels.
    pub scissor: [u32; 4],
    /// The sub-cell document scroll, in rows; a negative value shifts the rows
    /// up.
    pub shift_rows: f32,
    /// Whether the pool's composed rows differ from the previous frame. `false`
    /// during a pure sub-cell glide, letting the composite reuse the instances
    /// it built last frame and only re-apply the shift, rather than reshape and
    /// re-upload identical rows.
    pub content_changed: bool,
    /// Rows the composed content moved by since the previous frame, when moving
    /// is all it did.
    ///
    /// Lets the text composite slide its per-row shaping caches and re-shape
    /// only the rows the move exposed, which during an eased scroll is one to
    /// three of them. `None` when nothing carries over, so the composite rebuilds
    /// every row as it always has.
    pub scrolled_rows: Option<isize>,
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
    /// The handles another window's context can build on rather than requesting
    /// its own, held past their use here for [`GpuContext::shared`]. The device
    /// and queue below are the same two, kept beside the surface because that is
    /// where this context reaches for them.
    shared: SharedGpu,
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

/// What a render call did with the frame it was handed.
///
/// A caller builds a frame by taking the terminal's damage, which reports the
/// rows a projection rewrote and then forgets them. Both answers here name a
/// way that frame can leave the GPU behind the grid, and both are repaired by
/// one more frame rather than by anything the caller has to remember.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FrameOutcome {
    /// Whether the frame reached the screen.
    ///
    /// A surface that timed out, went outdated or lost, or belongs to an
    /// occluded window yields no drawable, so the call draws nothing. The rows
    /// the caller's damage named never reach the GPU buffers and stay stale
    /// until something else happens to change them.
    pub presented: bool,
    /// Whether the glyph atlas moved while the frame was being recorded,
    /// leaving the buffers just presented pointing at where glyphs used to be.
    pub atlas_stale: bool,
}

impl FrameOutcome {
    /// The outcome of a call that found no drawable and drew nothing.
    pub const SKIPPED: FrameOutcome = FrameOutcome {
        presented: false,
        atlas_stale: false,
    };
}

/// The GPU handles one window's context was built on, for a second window to
/// build on rather than requesting its own.
///
/// Every one of these is a handle onto state wgpu already holds, so a clone
/// costs a refcount. What it saves the second window is the asking: an instance,
/// an adapter request, and a device request, each of which talks to the driver.
///
/// The pipelines, atlases, and buffers stay per-window. They are what a
/// [`Renderer`] builds, and two windows drawing different grids need their own.
#[derive(Clone)]
pub struct SharedGpu {
    instance: Instance,
    adapter: Adapter,
    device: Device,
    queue: Queue,
}

/// The font database one window's context enumerated, for a second window to
/// build its font system from.
///
/// Enumerating the installed fonts reads the system's font directories and
/// costs hundreds of milliseconds. The database is what that reading produces,
/// and it clones, so a second window copies the answer rather than asking the
/// disk the same question. The bundled faces come with it, since they are loaded
/// into the same database.
///
/// Held behind an [`Arc`] because a font system owns its database, so the copy
/// has to happen once per window that builds one. Carrying this around costs a
/// refcount instead of a second one.
#[derive(Clone)]
pub struct SharedFonts {
    locale: String,
    db: Arc<fontdb::Database>,
}

impl SharedFonts {
    /// Take what `font_system` found, so another one can be built from it.
    pub(crate) fn of(font_system: &FontSystem) -> SharedFonts {
        SharedFonts {
            locale: font_system.locale().to_owned(),
            db: Arc::new(font_system.db().clone()),
        }
    }

    /// Build a font system holding this database, reading nothing.
    fn build(&self) -> FontSystem {
        FontSystem::new_with_locale_and_db(self.locale.clone(), (*self.db).clone())
    }
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
            shared: SharedGpu {
                instance,
                adapter,
                device: device.clone(),
                queue: queue.clone(),
            },
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

    /// Build a context for `window` on the handles another window's context
    /// already holds, or `None` when its adapter cannot present to this
    /// window's surface.
    ///
    /// Only what is really per-window is created: the surface, its
    /// configuration, and a [`Renderer`]. The instance, adapter, and device
    /// requests are the ones [`Self::new`] pays and this one skips, and the
    /// font system is built from `fonts` rather than from a second scan of the
    /// system's font directories.
    ///
    /// The adapter is asked because a surface on another window may be one it
    /// cannot present to, and wgpu aborts rather than errors when a device is
    /// used with such a surface. A `None` answer is a cue to fall back to
    /// [`Self::new`], which picks an adapter for this surface in particular.
    ///
    /// `window` is anything carrying window and display handles; the surface
    /// takes ownership of it, so it must outlive the context.
    pub fn with_shared<W>(
        shared: &SharedGpu,
        window: W,
        size: [u32; 2],
        fonts: &SharedFonts,
        font: FontConfig<'_>,
        background: Rgb,
        cursor: Rgb,
    ) -> Option<GpuContext>
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let [width, height] = size;
        let surface = shared.instance.create_surface(window).ok()?;
        if !shared.adapter.is_surface_supported(&surface) {
            return None;
        }

        let caps = surface.get_capabilities(&shared.adapter);
        let (surface_format, view_format) = surface_formats(&caps.formats);
        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: surface_format,
            width,
            height,
            present_mode: PresentMode::Fifo,
            alpha_mode: CompositeAlphaMode::Auto,
            view_formats: match view_format == surface_format {
                true => vec![],
                false => vec![view_format],
            },
            desired_maximum_frame_latency: 1,
        };
        surface.configure(&shared.device, &config);

        let renderer = Renderer::new(
            &shared.device,
            view_format,
            size,
            fonts.build(),
            font,
            background,
            cursor,
        );

        Some(GpuContext {
            surface,
            shared: shared.clone(),
            device: shared.device.clone(),
            queue: shared.queue.clone(),
            config,
            view_format,
            renderer,
            perf: FrameProfiler::new(),
            #[cfg(feature = "perf")]
            show_perf_hud: false,
        })
    }

    /// The GPU handles this context was built on, for a second window to build
    /// on through [`Self::with_shared`].
    pub fn shared(&self) -> SharedGpu {
        self.shared.clone()
    }

    /// The font database this context's font system holds, for a second window
    /// to build its own from through [`Self::with_shared`].
    pub fn fonts(&self) -> SharedFonts {
        self.renderer.fonts()
    }

    /// Re-configure the surface to `width`x`height` physical pixels.
    ///
    /// A size the surface already has costs nothing. Reconfiguring reallocates
    /// the swapchain, and window managers repeat the current size freely, on a
    /// focus change or a move.
    ///
    /// Zero-area sizes (e.g. a minimized window) are ignored, since
    /// configuring a surface with a zero dimension is invalid.
    pub fn resize(&mut self, width: u32, height: u32) {
        if !needs_configure(&self.config, width, height) {
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

    /// Re-point the surface clear and cursor tint colors, so a theme change
    /// shows on the next draw.
    pub fn set_theme_colors(&mut self, background: Rgb, cursor: Rgb) {
        self.renderer.set_theme_colors(background, cursor);
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
    /// Every buffer is prepared before the drawable is acquired, so the uploads
    /// overlap the wait rather than following it. Only the recording needs a
    /// view.
    ///
    /// When the acquired drawable's size disagrees with the configured size,
    /// the frame adopts the drawable's size so a live resize cannot trip
    /// scissor validation, and prepares again against the size it settled on.
    pub fn render(&mut self, grid: &Grid, frame: Frame<'_>) -> FrameOutcome {
        self.perf.begin_frame();

        // Ahead of the acquire, so the buffer uploads run while the compositor
        // is still deciding to hand over a drawable. Nothing a prepare writes
        // needs the view.
        self.renderer
            .prepare_frame(&self.device, &self.queue, grid, &frame, &[]);
        self.perf.mark_prepared();

        let surface_frame = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(frame) | CurrentSurfaceTexture::Suboptimal(frame) => {
                frame
            },
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return FrameOutcome::SKIPPED;
            },
            CurrentSurfaceTexture::Timeout
            | CurrentSurfaceTexture::Occluded
            | CurrentSurfaceTexture::Validation => return FrameOutcome::SKIPPED,
        };
        self.perf.mark_acquired();

        // The drawable can arrive at a size the surface was not configured for,
        // and every pass just prepared against the old one. Preparing again is
        // what a resize frame costs; one that kept its size pays nothing.
        if self.adopt_drawable_size(
            surface_frame.texture.width(),
            surface_frame.texture.height(),
        ) {
            self.renderer
                .prepare_frame(&self.device, &self.queue, grid, &frame, &[]);
        }

        let view = surface_frame.texture.create_view(&TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });
        self.renderer.record_into(&self.device, &self.queue, &view);

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

        // A grow on this path lands in the prepare, before anything records
        // against the atlas, so nothing this frame drew can hold a stale UV.
        FrameOutcome {
            presented: true,
            atlas_stale: false,
        }
    }

    /// Draw `live_grid` to the window surface, then composite each pool in
    /// `pools` over its scissor sub-rectangle, in one presented frame.
    ///
    /// `live_grid` and `frame` render as [`Self::render`] does, the static chrome
    /// and its cursor. Each [`PoolComposite`] is then composited over its scissor
    /// sub-rectangle in slice order, so several eased pools (split panes, a modal
    /// over an editor) each overwrite only their own region and stack
    /// earlier-under-later. An empty slice renders just the live grid.
    ///
    /// The whole frame is one encoder and one submit. Every pass prepares its
    /// buffers first, then the live grid, the pools, and the cursor all record into
    /// a single render pass, so the surface is cleared once rather than reloaded per
    /// pool. The perf HUD keeps a pass of its own inside that encoder, to stay out
    /// of the timed one.
    ///
    /// That prepare runs before the drawable is acquired, so its uploads overlap
    /// the wait, and it runs again when the drawable settles on a different size.
    ///
    /// Skips and re-configures on the same transient surface states as
    /// [`Self::render`], and adopts the acquired drawable's size the same way,
    /// so its pool and cursor scissors stay within the render target during a
    /// live resize.
    ///
    /// The returned [`FrameOutcome`] reports an atlas that was still moving as
    /// the frame was recorded, leaving the buffers just presented with stale
    /// UVs. A pool that grows the atlas is healed within the frame by a second
    /// prepare sweep, so this reports only a grow during that sweep.
    pub fn render_with_pools(
        &mut self,
        live_grid: &Grid,
        frame: Frame<'_>,
        pools: &[PoolComposite<'_>],
        anchored: &[AnchoredPanel],
        cursor_scissor: Option<[u32; 4]>,
    ) -> FrameOutcome {
        self.perf.begin_frame();

        // Ahead of the acquire, so the buffer uploads run while the compositor
        // is still deciding to hand over a drawable. Nothing a prepare writes
        // needs the view.
        let mut atlas_changed = self.prepare_pooled(live_grid, &frame, pools, anchored);
        self.perf.mark_prepared();

        let surface_frame = match self.surface.get_current_texture() {
            CurrentSurfaceTexture::Success(frame) | CurrentSurfaceTexture::Suboptimal(frame) => {
                frame
            },
            CurrentSurfaceTexture::Outdated | CurrentSurfaceTexture::Lost => {
                self.surface.configure(&self.device, &self.config);
                return FrameOutcome::SKIPPED;
            },
            CurrentSurfaceTexture::Timeout
            | CurrentSurfaceTexture::Occluded
            | CurrentSurfaceTexture::Validation => return FrameOutcome::SKIPPED,
        };
        self.perf.mark_acquired();

        // The drawable can arrive at a size the surface was not configured for,
        // and every pass just prepared against the old one. Preparing again is
        // what a resize frame costs; one that kept its size pays nothing.
        if self.adopt_drawable_size(
            surface_frame.texture.width(),
            surface_frame.texture.height(),
        ) {
            atlas_changed = self.prepare_pooled(live_grid, &frame, pools, anchored);
        }

        let view = surface_frame.texture.create_view(&TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });

        let cursor_scissor = cursor_scissor.and_then(|s| self.renderer.pool_scissor(s));

        #[cfg(feature = "perf")]
        let hud = self.show_perf_hud.then(|| self.perf.stats()).flatten();
        #[cfg(feature = "perf")]
        if let Some(stats) = hud.as_ref() {
            let samples = self.perf.samples();
            let resolution = [self.config.width as f32, self.config.height as f32];
            self.renderer
                .prepare_hud_over(&self.device, &self.queue, stats, &samples, resolution);
        }

        #[cfg(feature = "perf")]
        let timing = self.renderer.prepare_gpu_timing(&self.device, &self.queue);
        #[cfg(not(feature = "perf"))]
        let timing = false;

        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = self.renderer.begin_frame_pass(&mut encoder, &view, timing);

            self.renderer
                .record_frame(&mut render_pass, CursorLayer::Deferred);

            // A ridden panel goes over the pane composites it floats above, and
            // under the non-pane composites, which are the box content it frames.
            // Splitting the loop on `occludable` puts it exactly there.
            for (slot, pool) in pools.iter().enumerate() {
                if pool.occludable
                    && let Some(scissor) = self.renderer.pool_scissor(pool.scissor)
                {
                    self.renderer
                        .record_pool(&mut render_pass, scissor, pool.id, slot);
                }
            }
            self.renderer.record_riding_panels(&mut render_pass);
            for (slot, pool) in pools.iter().enumerate() {
                if !pool.occludable
                    && let Some(scissor) = self.renderer.pool_scissor(pool.scissor)
                {
                    self.renderer
                        .record_pool(&mut render_pass, scissor, pool.id, slot);
                }
            }

            // A pool leaves its own scissor set, so the strokes start from the full
            // surface and the riding ones apply their own.
            render_pass.set_scissor_rect(0, 0, self.config.width, self.config.height);
            // Every frame stroke, now that nothing left will paint over it.
            self.renderer.record_panel_strokes(&mut render_pass);
            self.renderer
                .record_cursor_over(&mut render_pass, cursor_scissor);
        }

        // The HUD keeps a pass of its own, inside this encoder, so its cost lands
        // outside the timed one rather than inflating the numbers it reports.
        #[cfg(feature = "perf")]
        if hud.is_some() {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("perf hud"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
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
            self.renderer.record_hud_over(&mut render_pass);
        }

        self.renderer
            .finish_frame(&self.device, &self.queue, encoder, timing);

        self.perf.mark_submitted();
        surface_frame.present();
        self.perf.end_frame();
        #[cfg(feature = "perf")]
        if let Some(gpu) = self.renderer.take_gpu_time() {
            self.perf.attach_gpu(gpu);
        }

        FrameOutcome {
            presented: true,
            atlas_stale: atlas_changed,
        }
    }

    /// Prepare every buffer a pooled frame draws from, reporting whether the
    /// glyph atlas was still moving when the sweep finished.
    ///
    /// Nothing here touches a surface, so a caller runs it before acquiring the
    /// drawable and lets the uploads overlap that wait. A caller whose drawable
    /// then arrives at a different size runs it again, since every pass resolved
    /// against the old resolution.
    ///
    /// The HUD is not prepared here. It reads the resolution the frame ends up
    /// with, which only the acquire settles.
    fn prepare_pooled(
        &mut self,
        live_grid: &Grid,
        frame: &Frame<'_>,
        pools: &[PoolComposite<'_>],
        anchored: &[AnchoredPanel],
    ) -> bool {
        let resolution = [self.config.width as f32, self.config.height as f32];
        let cursor_corners = frame.cursor_corners;
        let cursor_scroll = frame.scroll.grid + frame.scroll.document + frame.scroll.scrollback;

        // The pool composites paint over the cursor's cell, so the base prepares
        // without its cursor block and the block is recorded after the pools. The
        // ligature-break cell (`frame.cursor`) stays, keeping the live grid's
        // glyph under a chrome cursor broken out of any ligature.
        let base = Frame {
            cursor_corners: None,
            ..*frame
        };

        // Every pass prepares before anything draws, so the whole frame shares one
        // render pass. A pool that grows the atlas partway through moves the UVs the
        // live grid and the pools before it already resolved, so the sweep runs
        // again when the epoch moved. Both prepare paths rebuild rather than reuse
        // on an epoch mismatch, so the second sweep re-resolves exactly what went
        // stale.
        let epoch_before = self.renderer.content_epoch();
        self.prepare_pool_frame(live_grid, &base, pools, anchored);
        let settled = self.renderer.content_epoch();
        let atlas_changed = if settled == epoch_before {
            false
        } else {
            // The heal rebuilds against the settled atlas rather than repeating the
            // first sweep. Full damage with no slide, because the passes that cache
            // per row already rotated those caches by this frame's scroll and must
            // not rotate them a second time.
            let full = Damage::Full;
            let heal = Frame {
                cursor: base.cursor,
                cursor_corners: None,
                scroll: base.scroll,
                damage: &full,
                decoration_damage: &full,
                scrolled_rows: 0,
                sketch_progress: &[],
            };
            self.prepare_pool_frame(live_grid, &heal, pools, anchored);
            // A grow during the healing sweep leaves it stale in turn, and the
            // caller's extra frame is the backstop for that.
            self.renderer.content_epoch() != settled
        };

        if cursor_corners.is_some() {
            self.renderer.prepare_cursor_over(
                &self.queue,
                resolution,
                cursor_corners,
                cursor_scroll,
            );
        }

        atlas_changed
    }

    /// Upload the live grid's buffers and every pool's, in the order they draw.
    ///
    /// Run twice by [`Self::render_with_pools`] when a pool grew the atlas during
    /// the first sweep, since that moves the UVs everything prepared before it
    /// resolved. Idempotent apart from that healing, because each pass reuses what
    /// it built when neither its content nor the atlas has moved.
    ///
    /// A pool whose clip rectangle falls outside the render target is skipped, so
    /// the recording pass can skip the same ones by the same test.
    fn prepare_pool_frame(
        &mut self,
        live_grid: &Grid,
        frame: &Frame<'_>,
        pools: &[PoolComposite<'_>],
        anchored: &[AnchoredPanel],
    ) {
        self.renderer
            .prepare_frame(&self.device, &self.queue, live_grid, frame, anchored);

        let panels = live_grid.panels();
        let riding = mem::take(&mut self.renderer.riding);
        for (slot, pool) in pools.iter().enumerate() {
            if self.renderer.pool_scissor(pool.scissor).is_none() {
                continue;
            }
            self.renderer.prepare_pool(
                &self.device,
                &self.queue,
                pool.grid,
                panels,
                &riding,
                pool.shift_rows,
                pool.origin_cells,
                pool.content_changed,
                pool.scrolled_rows,
                pool.occludable,
                pool.id,
                slot,
            );
        }
        self.renderer.riding = riding;
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
    ///
    /// Reports whether the size moved, since every pass prepared against the
    /// old one before the drawable existed and has to prepare again when it
    /// did.
    fn adopt_drawable_size(&mut self, width: u32, height: u32) -> bool {
        if width == self.config.width && height == self.config.height {
            return false;
        }

        self.config.width = width;
        self.config.height = height;
        self.renderer.set_size(width, height);
        true
    }

    /// Mark the top of the caller's redraw, before it assembles the frame.
    ///
    /// Everything between here and the surface acquire is the caller's own
    /// work, which the profiler cannot see from inside [`Self::render`]. A
    /// caller that never marks reports a zero pre-acquire span.
    ///
    /// A no-op without the `perf` feature.
    pub fn mark_redraw_start(&mut self) {
        self.perf.mark_redraw_start();
    }

    /// Mark when the output the next frame answers arrived from the child, so
    /// the frame reports how long that byte waited to reach the screen.
    ///
    /// A no-op without the `perf` feature.
    pub fn mark_ingest(&mut self, at: Instant) {
        self.perf.mark_ingest(at);
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

/// Whether a surface configured as `config` has to be reconfigured to show
/// `width`x`height` physical pixels.
///
/// A zero dimension is refused outright, a surface being invalid at one, which
/// is what a minimized window reports.
fn needs_configure(config: &SurfaceConfiguration, width: u32, height: u32) -> bool {
    width != 0 && height != 0 && (config.width != width || config.height != height)
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
    use super::{
        build_font_system, clamp_scissor, headless_device, needs_configure, surface_formats,
        CommandEncoderDescriptor, CursorLayer, FontConfig, FontSystem, Frame, PoolComposite,
        Renderer, Scroll, SharedFonts, SurfaceConfiguration, TextureFormat,
    };
    use stoatty_term::{
        grid::{Grid, Rgb},
        term::Damage,
    };
    use wgpu::{
        BufferDescriptor, BufferUsages, Extent3d, MapMode, Origin3d, PollType, TexelCopyBufferInfo,
        TexelCopyBufferLayout, TexelCopyTextureInfo, TextureAspect, TextureDescriptor,
        TextureDimension, TextureUsages, TextureViewDescriptor,
    };

    /// A second window builds its font system from what the first found rather
    /// than reading the system's font directories again, so what it carries has
    /// to be the same answer: the same faces under the same locale, bundled ones
    /// included.
    #[test]
    fn a_font_system_built_from_a_shared_database_holds_what_the_scan_found() {
        let scanned = build_font_system();
        let faces = |system: &FontSystem| {
            system
                .db()
                .faces()
                .map(|face| (face.id, face.families.clone()))
                .collect::<Vec<_>>()
        };
        let found = faces(&scanned);
        assert!(
            !found.is_empty(),
            "the bundled faces are always there, or this proves nothing",
        );

        let shared = SharedFonts::of(&scanned).build();
        assert_eq!(
            (faces(&shared), shared.locale()),
            (found, scanned.locale()),
            "the shared database is the scan's answer, carried whole",
        );
    }

    /// The live grid and every pool land in their own regions from a single render
    /// pass, which is the sequence [`GpuContext::render_with_pools`] records.
    ///
    /// That method needs a window surface, so it cannot be driven here. This drives
    /// the [`Renderer`] halves it is built from, in the same order, which covers the
    /// merge's own risks. One pass clears once, and each recorded region still lands
    /// where its scissor says.
    #[test]
    fn one_pass_lands_the_live_grid_and_every_pool_in_its_own_region() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("single-pass frame test: no wgpu adapter available, skipping");
            return;
        };

        let format = TextureFormat::Rgba8Unorm;
        let font_size = 30;
        let cell_h = crate::render::cell_size(font_size, 1.0)[1].round() as u32;
        let band = cell_h * 2;
        let (width, height) = (128u32, band * 3);
        let (live_bg, gray, white) = (
            Rgb::new(0, 0, 90),
            Rgb::new(80, 80, 80),
            Rgb::new(255, 255, 255),
        );

        let target = device.create_texture(&TextureDescriptor {
            label: Some("single pass frame target"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&TextureViewDescriptor::default());

        let mut renderer = Renderer::new(
            &device,
            format,
            [width, height],
            build_font_system(),
            FontConfig {
                size: font_size,
                scale_factor: 1.0,
                family: &["JetBrains Mono".to_owned()],
                ligatures: true,
            },
            live_bg,
            white,
        );

        let (rows, cols) = renderer.grid_size();
        let filled = |color: Rgb| {
            let mut grid = Grid::new(rows, cols);
            for r in 0..rows {
                for c in 0..cols {
                    grid.get_mut(r, c).bg = color;
                }
            }
            grid
        };
        let live = filled(live_bg);
        let gray_pool = filled(gray);
        let white_pool = filled(white);

        let no_decoration = Damage::Partial(Vec::new());
        let frame = Frame {
            cursor: None,
            cursor_corners: None,
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers: &[],
            },
            damage: &Damage::Full,
            decoration_damage: &no_decoration,
            scrolled_rows: 0,
            sketch_progress: &[],
        };

        // The two pools take the lower bands, leaving the top one to the live grid.
        let pools = [
            PoolComposite {
                id: 7,
                grid: &gray_pool,
                origin_cells: [0.0; 2],
                scissor: [0, band, width, band],
                shift_rows: 0.0,
                content_changed: true,
                scrolled_rows: None,
                occludable: true,
            },
            PoolComposite {
                id: 4,
                grid: &white_pool,
                origin_cells: [0.0; 2],
                scissor: [0, band * 2, width, band],
                shift_rows: 0.0,
                content_changed: true,
                scrolled_rows: None,
                occludable: true,
            },
        ];

        renderer.prepare_frame(&device, &queue, &live, &frame, &[]);
        for (slot, pool) in pools.iter().enumerate() {
            renderer.prepare_pool(
                &device,
                &queue,
                pool.grid,
                &[],
                &[],
                pool.shift_rows,
                pool.origin_cells,
                pool.content_changed,
                pool.scrolled_rows,
                pool.occludable,
                pool.id,
                slot,
            );
        }

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = renderer.begin_frame_pass(&mut encoder, &view, false);
            renderer.record_frame(&mut render_pass, CursorLayer::Deferred);
            for (slot, pool) in pools.iter().enumerate() {
                let scissor = renderer
                    .pool_scissor(pool.scissor)
                    .expect("pool scissor is inside the target");
                renderer.record_pool(&mut render_pass, scissor, pool.id, slot);
            }
        }
        renderer.finish_frame(&device, &queue, encoder, false);

        let shot = read_back(&device, &queue, &target, width, height);
        let pixel = |y: u32| {
            let at = ((y * width + width / 2) * 4) as usize;
            (shot[at], shot[at + 1], shot[at + 2])
        };
        let bands = (
            pixel(band / 2),
            pixel(band + band / 2),
            pixel(band * 2 + band / 2),
        );

        let near = |got: (u8, u8, u8), want: Rgb| {
            let close = |a: u8, b: u8| i16::from(a).abs_diff(i16::from(b)) <= 12;
            close(got.0, want.r) && close(got.1, want.g) && close(got.2, want.b)
        };
        assert!(
            near(bands.0, live_bg) && near(bands.1, gray) && near(bands.2, white),
            "one pass must leave the live grid where no pool covers it and each pool \
             inside its own scissor: got {bands:?}"
        );
    }

    /// A pool composite paints over the cells a panel frame sits on, so a
    /// stroke recorded before it is a stroke a gliding pane erases. The pools
    /// path defers the strokes for that reason, the same way it defers the
    /// cursor.
    #[test]
    fn a_deferred_panel_stroke_survives_the_pool_that_covers_its_cells() {
        let Some((device, queue)) = headless_device() else {
            eprintln!("deferred panel stroke test: no wgpu adapter available, skipping");
            return;
        };

        let format = TextureFormat::Rgba8Unorm;
        let font_size = 30;
        let cell = crate::render::cell_size(font_size, 1.0);
        let (cell_w, cell_h) = (cell[0], cell[1].round() as u32);
        let (width, height) = (128u32, cell_h * 4);
        let (live_bg, pool_bg) = (Rgb::new(0, 0, 90), Rgb::new(80, 80, 80));

        let target = device.create_texture(&TextureDescriptor {
            label: Some("deferred stroke target"),
            size: Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: TextureDimension::D2,
            format,
            usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = target.create_view(&TextureViewDescriptor::default());

        let mut renderer = Renderer::new(
            &device,
            format,
            [width, height],
            build_font_system(),
            FontConfig {
                size: font_size,
                scale_factor: 1.0,
                family: &["JetBrains Mono".to_owned()],
                ligatures: true,
            },
            live_bg,
            Rgb::new(255, 255, 255),
        );

        let (rows, cols) = renderer.grid_size();
        assert!(rows >= 4 && cols >= 6, "grid too small: {rows}x{cols}");

        // A panel whose left edge falls inside the pool's band, so the composite
        // covers the cells the frame is drawn on.
        let mut live = Grid::new(rows, cols);
        live.set_panels(vec![stoatty_term::grid::Panel {
            top: 0,
            left: 2,
            width: 3,
            height: rows as u16,
            style: stoatty_term::grid::BorderStyle::Heavy,
            border: Rgb::new(255, 0, 0),
            corner_radius: 0,
            fill: None,
            shadow: stoatty_term::grid::PanelShadow::None_,
            inset_x: 0,
            above_pools: false,
            anchor: None,
            seq: 0,
        }]);

        let mut pool = Grid::new(rows, cols);
        for row in 0..rows {
            for col in 0..cols {
                pool.get_mut(row, col).bg = pool_bg;
            }
        }

        let no_decoration = Damage::Partial(Vec::new());
        let frame = Frame {
            cursor: None,
            cursor_corners: None,
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers: &[],
            },
            damage: &Damage::Full,
            decoration_damage: &no_decoration,
            scrolled_rows: 0,
            sketch_progress: &[],
        };

        // Covers the middle band, which the panel's left edge runs through.
        let scissor = [0, cell_h, width, cell_h * 2];
        renderer.prepare_frame(&device, &queue, &live, &frame, &[]);
        renderer.prepare_pool(
            &device,
            &queue,
            &pool,
            &[],
            &[],
            0.0,
            [0.0; 2],
            true,
            None,
            true,
            3,
            0,
        );

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        {
            let mut render_pass = renderer.begin_frame_pass(&mut encoder, &view, false);
            renderer.record_frame(&mut render_pass, CursorLayer::Deferred);
            let scissor = renderer
                .pool_scissor(scissor)
                .expect("pool scissor is inside the target");
            renderer.record_pool(&mut render_pass, scissor, 3, 0);
            render_pass.set_scissor_rect(0, 0, width, height);
            renderer.record_panel_strokes(&mut render_pass);
        }
        renderer.finish_frame(&device, &queue, encoder, false);

        let shot = read_back(&device, &queue, &target, width, height);
        let at = |x: u32, y: u32| {
            let i = ((y * width + x) * 4) as usize;
            [shot[i], shot[i + 1], shot[i + 2]]
        };

        let edge_x = (2.0 * cell_w).round() as u32;
        let inside_pool = cell_h + cell_h / 2;
        let reddest = (edge_x.saturating_sub(1)..=edge_x + 1)
            .map(|x| at(x, inside_pool))
            .max_by_key(|[r, g, _]| i32::from(*r) - i32::from(*g))
            .expect("pixels across the edge");

        assert!(
            reddest[0] > 200 && reddest[1] < 100,
            "the frame survives the composite covering its cells, got {reddest:?}",
        );
    }

    /// Copy `texture` into a mappable buffer and return its RGBA bytes, row-major
    /// with no padding, so the caller must size the texture so `4 * width` is
    /// 256-aligned.
    fn read_back(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        width: u32,
        height: u32,
    ) -> Vec<u8> {
        let buffer = device.create_buffer(&BufferDescriptor {
            label: Some("single pass readback"),
            size: u64::from(width * height * 4),
            usage: BufferUsages::COPY_DST | BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor::default());
        encoder.copy_texture_to_buffer(
            TexelCopyTextureInfo {
                texture,
                mip_level: 0,
                origin: Origin3d::ZERO,
                aspect: TextureAspect::All,
            },
            TexelCopyBufferInfo {
                buffer: &buffer,
                layout: TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: None,
                },
            },
            Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(Some(encoder.finish()));

        buffer.slice(..).map_async(MapMode::Read, |_| {});
        device
            .poll(PollType::wait_indefinitely())
            .expect("poll readback");
        buffer.slice(..).get_mapped_range().to_vec()
    }

    /// Reconfiguring reallocates the swapchain, and a window manager repeats
    /// the current size freely, on a focus change or a move, so a size the
    /// surface already has has to cost nothing.
    #[test]
    fn a_surface_is_reconfigured_only_for_a_size_it_is_not_at() {
        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format: TextureFormat::Rgba8Unorm,
            width: 800,
            height: 600,
            present_mode: wgpu::PresentMode::Fifo,
            desired_maximum_frame_latency: 2,
            alpha_mode: wgpu::CompositeAlphaMode::Opaque,
            view_formats: Vec::new(),
        };

        assert!(!needs_configure(&config, 800, 600), "the size it is at");
        assert!(needs_configure(&config, 801, 600), "a wider window");
        assert!(needs_configure(&config, 800, 601), "a taller window");
        assert!(
            !needs_configure(&config, 0, 600) && !needs_configure(&config, 800, 0),
            "a minimized window reports a zero dimension, which no surface is valid at",
        );
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
