//! The wgpu rendering context: owns the surface, device, and queue, and
//! drives a frame.
//!
//! Windowing-toolkit-agnostic. The surface is created from any handle the
//! app supplies via the raw-window-handle traits, so this crate never links
//! the windowing library; the app owns the window and hands its handle in.

use crate::render::{
    background::BackgroundPass, decoration::DecorationPass, overlay::OverlayPass, text::TextPass,
    CELL_HEIGHT, CELL_WIDTH,
};
use futures::executor;
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use stoatty_term::grid::Grid;
use wgpu::{
    Color, CommandEncoderDescriptor, CompositeAlphaMode, CurrentSurfaceTexture, Device,
    DeviceDescriptor, Instance, InstanceDescriptor, LoadOp, Operations, PowerPreference,
    PresentMode, Queue, RenderPassColorAttachment, RenderPassDescriptor, RequestAdapterOptions,
    StoreOp, Surface, SurfaceConfiguration, TextureUsages, TextureViewDescriptor,
};

/// Solid color the surface is cleared to each frame until the cell-grid
/// passes land. A dark slate, distinct from an uninitialized black window so
/// a successful clear is visible.
const BACKGROUND: Color = Color {
    r: 0.08,
    g: 0.09,
    b: 0.12,
    a: 1.0,
};

/// The GPU swapchain plus the device and queue that feed it.
///
/// Holds the surface configuration so [`Self::resize`] and the surface-loss
/// recovery in [`Self::render`] can re-`configure` without re-querying
/// capabilities.
pub struct GpuContext {
    surface: Surface<'static>,
    device: Device,
    queue: Queue,
    config: SurfaceConfiguration,
    background: BackgroundPass,
    decoration: DecorationPass,
    text: TextPass,
    overlay: OverlayPass,
}

impl GpuContext {
    /// Build the context for `window`, sized to `width`x`height` physical
    /// pixels.
    ///
    /// `window` is anything carrying window and display handles; the surface
    /// takes ownership of it, so it must outlive the context (pass an
    /// `Arc`-wrapped window). Blocks on adapter and device acquisition.
    ///
    /// Panics if no GPU adapter is available, device creation fails, or the
    /// surface cannot be created. All three are unrecoverable at startup.
    pub fn new<W>(window: W, width: u32, height: u32) -> GpuContext
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

        let background = BackgroundPass::new(&device, format);
        let decoration = DecorationPass::new(&device, format);
        let text = TextPass::new(&device, format);
        let overlay = OverlayPass::new(&device, format);

        GpuContext {
            surface,
            device,
            queue,
            config,
            background,
            decoration,
            text,
            overlay,
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
    }

    /// The (rows, cols) cell grid that fills the current surface.
    ///
    /// Divides the surface's physical pixel size by the fixed cell metrics,
    /// flooring with a one-cell minimum so a sliver of a window still yields a
    /// usable grid. The app sizes the terminal and PTY to this so the shell's
    /// view matches what the renderer draws.
    pub fn grid_size(&self) -> (usize, usize) {
        let rows = (self.config.height as f32 / CELL_HEIGHT).floor().max(1.0) as usize;
        let cols = (self.config.width as f32 / CELL_WIDTH).floor().max(1.0) as usize;
        (rows, cols)
    }

    /// Draw a frame: clear to [`BACKGROUND`], fill each cell of `grid` with its
    /// background color, composite each cell's glyph over it, then tint the
    /// cursor cell. `cursor` is the cursor's position in fractional cell
    /// coordinates, or `None` when it is hidden.
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

        let resolution = [self.config.width as f32, self.config.height as f32];
        self.background
            .prepare(&self.device, &self.queue, grid, resolution, cursor);
        self.decoration
            .prepare(&self.device, &self.queue, grid, resolution);
        self.text
            .prepare(&self.device, &self.queue, grid, resolution);
        self.overlay
            .prepare(&self.device, &self.queue, grid, resolution);

        let mut encoder = self
            .device
            .create_command_encoder(&CommandEncoderDescriptor::default());

        {
            let mut render_pass = encoder.begin_render_pass(&RenderPassDescriptor {
                label: Some("frame"),
                color_attachments: &[Some(RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: Operations {
                        load: LoadOp::Clear(BACKGROUND),
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
        }

        self.queue.submit([encoder.finish()]);
        frame.present();
    }
}
