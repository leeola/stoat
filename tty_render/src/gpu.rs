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
    background::BackgroundPass, decoration::DecorationPass, overlay::OverlayPass, text::TextPass,
    CELL_HEIGHT, CELL_WIDTH,
};
use futures::executor;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use stoatty_term::grid::{Grid, Rgb};
use wgpu::{
    Color, CommandEncoderDescriptor, CompositeAlphaMode, CurrentSurfaceTexture, Device,
    DeviceDescriptor, Instance, InstanceDescriptor, LoadOp, Operations, PowerPreference,
    PresentMode, Queue, RenderPassColorAttachment, RenderPassDescriptor, RequestAdapterOptions,
    StoreOp, Surface, SurfaceConfiguration, TextureFormat, TextureUsages, TextureView,
    TextureViewDescriptor,
};

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
    width: u32,
    height: u32,
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
    /// Build the grid passes for `format` at `width`x`height` physical pixels,
    /// clearing to `background` and drawing the cursor block in `cursor`.
    pub fn new(
        device: &Device,
        format: TextureFormat,
        width: u32,
        height: u32,
        background: Rgb,
        cursor: Rgb,
    ) -> Renderer {
        Renderer {
            background: BackgroundPass::new(device, format),
            decoration: DecorationPass::new(device, format),
            text: TextPass::new(device, format),
            overlay: OverlayPass::new(device, format),
            width,
            height,
            clear_color: rgb_to_color(background),
            cursor_color: cursor,
        }
    }

    /// The (rows, cols) cell grid that fills the target at the current size.
    ///
    /// Divides the pixel size by the fixed cell metrics, flooring with a
    /// one-cell minimum so a sliver still yields a usable grid.
    pub fn grid_size(&self) -> (usize, usize) {
        let rows = (self.height as f32 / CELL_HEIGHT).floor().max(1.0) as usize;
        let cols = (self.width as f32 / CELL_WIDTH).floor().max(1.0) as usize;
        (rows, cols)
    }

    /// Draw a frame for `grid` into `view`: clear to the default background,
    /// fill each cell background, composite glyphs and decorations, tint the
    /// cursor cell, then draw overlays and their content on top.
    ///
    /// `cursor` is the cursor's position in fractional cell coordinates, or
    /// `None` when it is hidden. Submits the frame but does not present or poll;
    /// the caller drives whichever it needs.
    pub fn render_into(
        &mut self,
        device: &Device,
        queue: &Queue,
        view: &TextureView,
        grid: &Grid,
        cursor: Option<[f32; 2]>,
    ) {
        let resolution = [self.width as f32, self.height as f32];
        self.background
            .prepare(device, queue, grid, resolution, cursor, self.cursor_color);
        self.decoration.prepare(device, queue, grid, resolution);
        self.text.prepare(device, queue, grid, resolution);
        self.overlay.prepare(device, queue, grid, resolution);

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
            self.background.draw_cursor(&mut render_pass);
            self.overlay.draw(&mut render_pass);
            self.text.draw_overlay_text(&mut render_pass);
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
    renderer: Renderer,
}

impl GpuContext {
    /// Build the context for `window`, sized to `width`x`height` physical
    /// pixels, clearing to `background` and drawing the cursor block in
    /// `cursor`.
    ///
    /// `window` is anything carrying window and display handles; the surface
    /// takes ownership of it, so it must outlive the context (pass an
    /// `Arc`-wrapped window). Blocks on adapter and device acquisition.
    ///
    /// Panics if no GPU adapter is available, device creation fails, or the
    /// surface cannot be created. All three are unrecoverable at startup.
    pub fn new<W>(window: W, width: u32, height: u32, background: Rgb, cursor: Rgb) -> GpuContext
    where
        W: HasWindowHandle + HasDisplayHandle + Send + Sync + 'static,
    {
        let instance = Instance::new(InstanceDescriptor::new_without_display_handle());
        let surface = instance
            .create_surface(window)
            .expect("create wgpu surface");

        let adapter = executor::block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::HighPerformance,
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .expect("request wgpu adapter");

        let (device, queue) =
            executor::block_on(adapter.request_device(&DeviceDescriptor::default()))
                .expect("request wgpu device");

        // A non-sRGB surface: the text pass encodes its gamma-correct composite
        // to sRGB in the shader, and the background pass writes already-encoded
        // colors, so the surface must store written values verbatim rather than
        // re-encoding them.
        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .copied()
            .find(|format| !format.is_srgb())
            .unwrap_or(caps.formats[0]);

        let config = SurfaceConfiguration {
            usage: TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: PresentMode::Fifo,
            alpha_mode: CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let renderer = Renderer::new(&device, format, width, height, background, cursor);

        GpuContext {
            surface,
            device,
            queue,
            config,
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

    /// Draw a frame of `grid` to the window surface. `cursor` is the cursor's
    /// position in fractional cell coordinates, or `None` when it is hidden.
    ///
    /// Skips the frame when the surface is transiently unavailable (timed
    /// out, occluded, or a validation error already raised elsewhere) and
    /// re-configures on an outdated or lost surface so the next frame
    /// recovers.
    pub fn render(&mut self, grid: &Grid, cursor: Option<[f32; 2]>) {
        let frame = match self.surface.get_current_texture() {
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

        let view = frame.texture.create_view(&TextureViewDescriptor::default());
        self.renderer
            .render_into(&self.device, &self.queue, &view, grid, cursor);
        frame.present();
    }
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
