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

use crate::render::{
    background::{BackgroundPass, CursorState},
    bar::BarPass,
    decoration::DecorationPass,
    icon::IconPass,
    overlay::OverlayPass,
    text::TextPass,
    CellMetrics,
};
pub use crate::render::{text::build_font_system, Frame, Scroll};
use cosmic_text::FontSystem;
use futures::executor;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use std::thread;
use stoatty_term::grid::{Grid, Rgb};
use wgpu::{
    Color, CommandEncoderDescriptor, CompositeAlphaMode, CurrentSurfaceTexture, Device,
    DeviceDescriptor, Instance, InstanceDescriptor, LoadOp, Operations, PowerPreference,
    PresentMode, Queue, RenderPassColorAttachment, RenderPassDescriptor, RequestAdapterOptions,
    StoreOp, Surface, SurfaceConfiguration, TextureFormat, TextureUsages, TextureView,
    TextureViewDescriptor,
};

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
    decoration: DecorationPass,
    text: TextPass,
    overlay: OverlayPass,
    icon: IconPass,
    bar: BarPass,
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
            width: size[0],
            height: size[1],
            metrics,
            clear_color: rgb_to_color(background),
            cursor_color: cursor,
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
        self.decoration.set_metrics(metrics);
        self.text.set_metrics(metrics);
        self.overlay.set_metrics(metrics);
        self.icon.set_metrics(metrics);
        self.bar.set_metrics(metrics);
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
                pos: frame.cursor,
                color: self.cursor_color,
            },
            frame.scroll.grid,
        );
        self.decoration
            .prepare(device, queue, grid, resolution, frame.scroll.grid);
        self.text.prepare(device, queue, grid, resolution, &frame);
        self.overlay.prepare(device, queue, grid, resolution);
        self.icon.prepare(device, queue, grid.icons(), resolution);
        self.bar.prepare(device, queue, grid.bars(), resolution);

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
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });

            self.background.draw(&mut render_pass);
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
            self.text.draw_text_runs(&mut render_pass);
            self.background.draw_cursor(&mut render_pass);
            self.overlay.draw(&mut render_pass);
            self.text.draw_overlay_text(&mut render_pass);
            // The overlay-content draw leaves its scissor set, so restore the
            // full surface before the icons draw on top of the overlays.
            render_pass.set_scissor_rect(0, 0, self.width, self.height);
            self.icon.draw(&mut render_pass);
        }

        queue.submit([encoder.finish()]);
    }

    fn set_size(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
    }
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

        let (device, queue) =
            executor::block_on(adapter.request_device(&DeviceDescriptor::default()))
                .expect("GPU device creation failed on the selected adapter");

        // The text pass encodes its gamma-correct composite to sRGB in the
        // shader and the background pass writes already-encoded colors, so the
        // passes must render to a linear-store (non-sRGB) view. When only an
        // sRGB surface format is available, the surface keeps it but views
        // render through the non-sRGB sibling, so the hardware does not encode
        // sRGB a second time on top of the shader.
        let caps = surface.get_capabilities(&adapter);
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
            present_mode: PresentMode::Fifo,
            alpha_mode: CompositeAlphaMode::Auto,
            view_formats,
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let font_system = font_load.join();
        let renderer = Renderer::new(
            &device,
            view_format,
            [width, height],
            font_system,
            font,
            background,
            cursor,
        );

        GpuContext {
            surface,
            device,
            queue,
            config,
            view_format,
            renderer,
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
    pub fn render(&mut self, grid: &Grid, frame: Frame<'_>) {
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

        let view = surface_frame.texture.create_view(&TextureViewDescriptor {
            format: Some(self.view_format),
            ..Default::default()
        });
        self.renderer
            .render_into(&self.device, &self.queue, &view, grid, frame);
        surface_frame.present();
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

    executor::block_on(adapter.request_device(&DeviceDescriptor::default())).ok()
}

#[cfg(test)]
mod tests {
    use super::{grid_dims, surface_formats, TextureFormat};
    use crate::render::CellMetrics;

    #[test]
    fn grid_dims_shrink_as_font_grows() {
        let dims = |font| grid_dims(800, 600, CellMetrics::from_font_size(font, 1.0));
        assert_eq!(dims(15), (33, 88));
        assert_eq!(dims(30), (16, 44));
        assert_eq!(dims(60), (8, 22));
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
