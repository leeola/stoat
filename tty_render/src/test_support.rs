//! Helpers the renderer's tests share.
//!
//! Compiled for the crate's own unit tests, and for its integration tests
//! through the `test-support` feature.

use crate::gpu::{self, build_font_system, FontConfig, Frame, Renderer, Scroll, SharedGpu};
use stoatty_term::{grid::Rgb, term::Damage};
use wgpu::{Device, Queue, Texture, TextureFormat, TextureView};

/// The panic message of a test that draws when no wgpu adapter answers.
const NO_ADAPTER: &str = "no wgpu adapter answered; the renderer tests draw on a real device. \
                          Enter the devshell with `nix develop`, which registers lavapipe, or \
                          install a Vulkan driver";

/// A headless device and queue for a test that draws.
///
/// Panics when no adapter answers. A renderer test that returns early without
/// a device builds no pipeline and draws no pixel, so its pass proves nothing
/// about the renderer.
pub fn require_headless_device() -> (Device, Queue) {
    gpu::headless_device().expect(NO_ADAPTER)
}

/// The GPU handles for a test that draws through an offscreen
/// [`gpu::GpuContext`].
///
/// Panics when no adapter answers, for the reason [`require_headless_device`]
/// does.
pub fn require_headless_gpu() -> SharedGpu {
    gpu::headless_gpu(false).expect(NO_ADAPTER)
}

/// An `Rgba8Unorm` texture of `width` by `height` for a test to render into and
/// read back, with its view.
pub fn offscreen_target(device: &Device, width: u32, height: u32) -> (Texture, TextureView) {
    gpu::offscreen_texture(device, width, height)
}

/// Copy `texture` into a mappable buffer and return its RGBA bytes, row-major
/// with no padding, as [`gpu::read_back_texture`] does for any width.
pub fn read_back(
    device: &Device,
    queue: &Queue,
    texture: &Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    gpu::read_back_texture(device, queue, texture, width, height)
}

/// A frame with no cursor, scroll, or sketch reveals, which rebuilds the rows
/// `damage` names and repaints the decorations `decoration_damage` names.
pub fn still_frame<'a>(damage: &'a Damage, decoration_damage: &'a Damage) -> Frame<'a> {
    Frame {
        cursor: None,
        cursor_corners: None,
        scroll: Scroll {
            grid: 0.0,
            document: 0.0,
            scrollback: 0.0,
            region: 0.0,
            popovers: &[],
        },
        damage,
        decoration_damage,
        scrolled_rows: 0,
        sketch_reveals: &[],
    }
}

/// The RGB of the pixel at `x`, `y` in a [`read_back`] of a `width`-wide
/// target.
pub fn pixel(shot: &[u8], width: u32, x: u32, y: u32) -> [u8; 3] {
    let at = ((y * width + x) * 4) as usize;
    [shot[at], shot[at + 1], shot[at + 2]]
}

/// A renderer of `size` over JetBrains Mono at `font_size` and `scale_factor`,
/// for an `Rgba8Unorm` target on a `surface` background.
///
/// The cursor is white and ligatures are on, as most renderer tests draw. A
/// test that needs another format, cursor, or ligature setting calls
/// [`Renderer::new`] itself.
pub fn renderer(
    device: &Device,
    size: [u32; 2],
    font_size: u32,
    scale_factor: f32,
    surface: Rgb,
) -> Renderer {
    Renderer::new(
        device,
        TextureFormat::Rgba8Unorm,
        size,
        build_font_system(),
        FontConfig {
            size: font_size,
            scale_factor,
            family: &["JetBrains Mono".to_owned()],
            ligatures: true,
        },
        surface,
        Rgb::new(255, 255, 255),
    )
}
