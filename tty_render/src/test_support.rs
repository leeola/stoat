//! Helpers the renderer's tests share.
//!
//! Compiled for the crate's own unit tests, and for its integration tests
//! through the `test-support` feature.

use crate::gpu;
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages, TextureView,
    TextureViewDescriptor,
};

/// A headless device and queue for a test that draws.
///
/// Panics when no adapter answers. A renderer test that returns early without
/// a device builds no pipeline and draws no pixel, so its pass proves nothing
/// about the renderer.
pub fn require_headless_device() -> (Device, Queue) {
    gpu::headless_device().expect(
        "no wgpu adapter answered; the renderer tests draw on a real device. Enter the devshell \
         with `nix develop`, which registers lavapipe, or install a Vulkan driver",
    )
}

/// An `Rgba8Unorm` texture of `width` by `height` for a test to render into and
/// read back, with its view.
///
/// Size it so `4 * width` is a multiple of 256, the row alignment
/// [`read_back`] copies at.
pub fn offscreen_target(device: &Device, width: u32, height: u32) -> (Texture, TextureView) {
    let target = device.create_texture(&TextureDescriptor {
        label: Some("test target"),
        size: Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format: TextureFormat::Rgba8Unorm,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&TextureViewDescriptor::default());
    (target, view)
}

/// Copy `texture` into a mappable buffer and return its RGBA bytes, row-major
/// with no padding.
///
/// The copy writes whole rows, so the caller must size the texture so
/// `4 * width` is 256-aligned. Blocks until the device finishes the copy, and
/// panics when the poll fails.
pub fn read_back(
    device: &Device,
    queue: &Queue,
    texture: &Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some("test readback"),
        size: u64::from(width) * u64::from(height) * 4,
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
