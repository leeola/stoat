//! Helpers the renderer's tests share.
//!
//! Compiled for the crate's own unit tests, and for its integration tests
//! through the `test-support` feature.

use crate::gpu;
use wgpu::{Device, Queue};

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
