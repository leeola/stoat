//! Headless GPU check that an oversized pool, cursor, or riding scissor is
//! clamped to the render target instead of tripping a validation error.
//!
//! During a live resize the app sometimes hands `composite_pool` and
//! `draw_cursor_over` a scissor sized to a stale grid larger than the freshly
//! shrunk drawable, and a panel riding a pool takes its scissor from that pool's
//! region. wgpu aborts
//! the process when a scissor exceeds the render target, so the renderer clamps
//! every caller-supplied scissor. These drive each entry point with a scissor
//! twice the offscreen target's size inside a validation error scope and assert
//! no error is raised.
//! Fails without a GPU adapter, since a test that draws nothing proves nothing.

use futures::executor;
use stoatty_render::{
    gpu::{build_font_system, FontConfig, Frame, Renderer, Scroll},
    render::{cell_size, HostRide},
    test_support::require_headless_device,
};
use stoatty_term::{
    grid::{BorderStyle, Grid, Panel, PanelShadow, Rgb},
    term::Damage,
};
use wgpu::{
    Device, ErrorFilter, Extent3d, PollType, Texture, TextureDescriptor, TextureDimension,
    TextureFormat, TextureUsages, TextureView, TextureViewDescriptor,
};

/// An offscreen target three rows tall and a renderer sized to it.
struct Setup {
    /// Held so the target outlives every draw into `view`.
    _target: Texture,
    view: TextureView,
    renderer: Renderer,
    width: u32,
    height: u32,
}

fn setup(device: &Device) -> Setup {
    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let [_, cell_h] = cell_size(font_size, 1.0);
    let (width, height) = (128u32, cell_h.round() as u32 * 3);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("scissor clamp target"),
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

    let renderer = Renderer::new(
        device,
        format,
        [width, height],
        build_font_system(),
        FontConfig {
            size: font_size,
            scale_factor: 1.0,
            family: &["JetBrains Mono".to_owned()],
            ligatures: true,
        },
        Rgb::new(0, 0, 0),
        Rgb::new(255, 255, 255),
    );

    Setup {
        _target: target,
        view,
        renderer,
        width,
        height,
    }
}

/// A frame with no cursor, no scroll, and the whole grid damaged.
fn still_frame(no_decoration: &Damage) -> Frame<'_> {
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
        damage: &Damage::Full,
        decoration_damage: no_decoration,
        scrolled_rows: 0,
        sketch_reveals: &[],
    }
}

#[test]
fn oversized_scissors_are_clamped_not_validated() {
    let (device, queue) = require_headless_device();
    let Setup {
        view,
        mut renderer,
        width,
        height,
        ..
    } = setup(&device);

    let (rows, cols) = renderer.grid_size();
    let base = Grid::new(rows, cols);
    let pool = Grid::new(rows, cols);
    let no_decoration = Damage::Partial(Vec::new());
    let resolution = [width as f32, height as f32];
    let corners = [[0.0, 1.0], [1.0, 1.0], [0.0, 2.0], [1.0, 2.0]];

    // Twice the target in each axis. Without clamping, encoding this scissor
    // raises the validation error that aborts the process in the live app.
    let oversized = [0, 0, width * 2, height * 2];

    let scope = device.push_error_scope(ErrorFilter::Validation);

    renderer.render_into(&device, &queue, &view, &base, still_frame(&no_decoration));
    renderer.composite_pool(
        &device,
        &queue,
        &view,
        &pool,
        None,
        &[],
        oversized,
        0.0,
        [0.0; 2],
        true,
        None,
        true,
        0,
        0,
    );
    renderer.draw_cursor_over(
        &device,
        &queue,
        &view,
        resolution,
        Some(corners),
        0.0,
        Some(oversized),
    );

    let error_future = scope.pop();
    device.poll(PollType::wait_indefinitely()).expect("poll");
    let error = executor::block_on(error_future);

    assert!(
        error.is_none(),
        "a scissor larger than the target must be clamped, not validated: {error:?}"
    );
}

/// A panel anchored to a gliding pool draws inside the scissor its ride carries,
/// which a pool region past the window makes larger than the target.
#[test]
fn an_oversized_riding_scissor_is_clamped_not_validated() {
    let (device, queue) = require_headless_device();
    let Setup {
        view,
        mut renderer,
        width,
        height,
        ..
    } = setup(&device);

    let (rows, cols) = renderer.grid_size();
    let mut grid = Grid::new(rows, cols);
    grid.set_panels(vec![Panel {
        top: 0,
        left: 0,
        width: 2,
        height: 1,
        style: BorderStyle::Rounded,
        border: Rgb::new(200, 100, 50),
        corner_radius: 6,
        fill: None,
        shadow: PanelShadow::None_,
        inset_x: 0,
        above_pools: false,
        anchor: Some((7, 0.0)),
        seq: 0,
    }]);
    let no_decoration = Damage::Partial(Vec::new());
    let ride = HostRide {
        host: 7,
        top_rows: 0.0,
        scissor: [0, 0, width * 2, height * 2],
    };

    let scope = device.push_error_scope(ErrorFilter::Validation);

    renderer.render_pools_into(
        &device,
        &queue,
        &view,
        &grid,
        still_frame(&no_decoration),
        &[],
        &[ride],
    );

    let error_future = scope.pop();
    device.poll(PollType::wait_indefinitely()).expect("poll");
    let error = executor::block_on(error_future);

    assert!(
        error.is_none(),
        "a riding scissor larger than the target must be clamped, not validated: {error:?}"
    );
}
