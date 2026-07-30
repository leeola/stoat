//! Headless GPU check that compositing a pool leaves the live grid's instance
//! buffers intact.
//!
//! `composite_pool` builds a pool into dedicated composite buffers, not the
//! live grid's, so a live frame that reuses its cached instances (empty damage)
//! must still render the live grid after a pool composited over it. This renders
//! a glyph on a black grid, composites a gray pool over the whole surface, then
//! renders the live grid again with no damage and reads back to confirm the
//! glyph and the black background survive rather than the pool's gray and blank
//! cells.
//! Skips when no GPU adapter is present, so a GPU-less CI stays green.

use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{Grid, Rgb},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

#[test]
fn pool_composite_keeps_live_instances() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("pool_keeps_live_grid: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let cell_h = cell_size(font_size, 1.0)[1].round() as u32;
    let (width, height) = (128u32, cell_h * 3);

    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);
    let gray = Rgb::new(80, 80, 80);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("pool keeps live target"),
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
        black,
        white,
    );

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 3 && cols >= 1, "grid too small: {rows}x{cols}");

    let mut live = Grid::new(rows, cols);
    live.get_mut(0, 0).ch = 'M';
    live.get_mut(0, 0).fg = white;
    live.get_mut(0, 0).bg = black;

    // The pool differs from the live grid in both background (gray) and glyphs
    // (none), so a live buffer the composite clobbered would show through.
    let mut pool = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            pool.get_mut(r, c).bg = gray;
        }
    }

    let no_decoration = Damage::Partial(Vec::new());
    let frame = |damage| Frame {
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
        decoration_damage: &no_decoration,
        scrolled_rows: 0,
    };

    // A full live frame establishes the cached instances.
    renderer.render_into(&device, &queue, &view, &live, frame(&Damage::Full));
    let base = read_back(&device, &queue, &target, width, height);

    // Composite the gray pool over the whole surface, then render the live grid
    // again with no damage so it reuses its cached instances. A composite that
    // rebuilt those would leave the pool's gray and blank glyph showing.
    renderer.composite_pool(
        &device,
        &queue,
        &view,
        &pool,
        &[],
        [0, 0, width, height],
        0.0,
        true,
        true,
        0,
        0,
    );
    let idle = Damage::Partial(vec![false; rows]);
    renderer.render_into(&device, &queue, &view, &live, frame(&idle));
    let after = read_back(&device, &queue, &target, width, height);

    let lit = |pixels: &[u8], band: u32| {
        let mut count = 0usize;
        for y in (band * cell_h)..((band + 1) * cell_h) {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                let (r, g, b) = (pixels[i] as u32, pixels[i + 1] as u32, pixels[i + 2] as u32);
                if r + g + b > 120 {
                    count += 1;
                }
            }
        }
        count
    };

    assert!(
        lit(&base, 0) > 0,
        "the glyph should render in the base frame"
    );
    assert_eq!(
        lit(&after, 0),
        lit(&base, 0),
        "the live glyph must survive a pool composite under empty damage"
    );
    assert_eq!(
        lit(&after, 2),
        0,
        "a blank live row must stay black, not the pool's gray, after a composite"
    );
}

/// A composite declared `content_changed = false` reuses the instances the prior
/// composite built instead of reshaping the grid it is handed.
///
/// The instances built from a gray pool must redraw when a white pool is
/// composited as a shift-only reuse, so the sampled cell stays gray: the white
/// grid is ignored, and the live grid's black does not show through.
#[test]
fn shift_only_composite_reuses_prior_rows() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("pool_keeps_live_grid: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let cell_h = cell_size(font_size, 1.0)[1].round() as u32;
    let (width, height) = (128u32, cell_h * 3);
    let (black, white, gray) = (
        Rgb::new(0, 0, 0),
        Rgb::new(255, 255, 255),
        Rgb::new(80, 80, 80),
    );

    let target = device.create_texture(&TextureDescriptor {
        label: Some("shift-only reuse target"),
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
        black,
        white,
    );

    let (rows, cols) = renderer.grid_size();
    let live = Grid::new(rows, cols);

    let filled = |color: Rgb| {
        let mut grid = Grid::new(rows, cols);
        for r in 0..rows {
            for c in 0..cols {
                grid.get_mut(r, c).bg = color;
            }
        }
        grid
    };
    let gray_pool = filled(gray);
    let white_pool = filled(white);

    let no_decoration = Damage::Partial(Vec::new());
    let frame = |damage| Frame {
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
        decoration_damage: &no_decoration,
        scrolled_rows: 0,
    };
    let full_surface = [0, 0, width, height];

    // Build the composite instances from the gray pool.
    renderer.render_into(&device, &queue, &view, &live, frame(&Damage::Full));
    renderer.composite_pool(
        &device,
        &queue,
        &view,
        &gray_pool,
        &[],
        full_surface,
        0.0,
        true,
        true,
        0,
        0,
    );

    // Reset the surface to the live grid, then composite the white pool as a
    // shift-only reuse, which must redraw the gray instances and ignore white.
    let idle = Damage::Partial(vec![false; rows]);
    renderer.render_into(&device, &queue, &view, &live, frame(&idle));
    renderer.composite_pool(
        &device,
        &queue,
        &view,
        &white_pool,
        &[],
        full_surface,
        0.0,
        false,
        true,
        0,
        0,
    );
    let reused = read_back(&device, &queue, &target, width, height);

    let center = (((cell_h + cell_h / 2) * width + width / 2) * 4) as usize;
    let (r, g, b) = (reused[center], reused[center + 1], reused[center + 2]);
    assert!(
        r < 160 && g < 160 && b < 160,
        "a shift-only reuse must redraw the prior gray pool, not the white grid it was handed: got ({r},{g},{b})"
    );
    assert!(
        r > 20 || g > 20 || b > 20,
        "the reuse must redraw the pool, not leave the live grid's black: got ({r},{g},{b})"
    );
}

/// Pools reusing prior rows each redraw their own, both when the frame carries the
/// same pools as the last and when one has dropped out.
///
/// `content_changed` is per pool, so one pool can reuse in a frame where another
/// rebuilds. Sharing a buffer between them would hand every reusing pool whatever
/// the last-prepared one built, painting a sibling's content inside its scissor.
///
/// Pools appear in a frame only while they glide, so one settling shifts the rest
/// down a position. Keying each pool's buffers by its own id rather than by that
/// position is what keeps the survivors reading their own instances.
#[test]
fn pools_reusing_prior_rows_keep_their_own_as_the_frame_changes_shape() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("pool_keeps_live_grid: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let cell_h = cell_size(font_size, 1.0)[1].round() as u32;
    let band = cell_h * 2;
    let (width, height) = (128u32, band * 3);
    let (black, white, gray, blue) = (
        Rgb::new(0, 0, 0),
        Rgb::new(255, 255, 255),
        Rgb::new(80, 80, 80),
        Rgb::new(0, 0, 200),
    );

    let target = device.create_texture(&TextureDescriptor {
        label: Some("pool reuse target"),
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
        black,
        white,
    );

    let (rows, cols) = renderer.grid_size();
    let live = Grid::new(rows, cols);

    let filled = |color: Rgb| {
        let mut grid = Grid::new(rows, cols);
        for r in 0..rows {
            for c in 0..cols {
                grid.get_mut(r, c).bg = color;
            }
        }
        grid
    };
    let gray_pool = filled(gray);
    let white_pool = filled(white);
    let blue_pool = filled(blue);

    let no_decoration = Damage::Partial(Vec::new());
    let frame = |damage| Frame {
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
        decoration_damage: &no_decoration,
        scrolled_rows: 0,
    };

    // Each pool owns a horizontal band, so a read-back tells them apart. The ids
    // are unordered and non-contiguous, as pane and modal ids are.
    let (gray_id, white_id, blue_id) = (7, 4, 9);
    let bands = [
        [0, 0, width, band],
        [0, band, width, band],
        [0, band * 2, width, band],
    ];

    // The pool id keys the instance buffers across frames. The slot is only this
    // frame's globals slot, so it moves when a pool leaves the frame.
    let composite = |renderer: &mut Renderer, grid: &Grid, scissor, changed, pool, slot| {
        renderer.composite_pool(
            &device,
            &queue,
            &view,
            grid,
            &[],
            scissor,
            0.0,
            changed,
            true,
            pool,
            slot,
        );
    };

    // Each pool builds its own instances.
    renderer.render_into(&device, &queue, &view, &live, frame(&Damage::Full));
    composite(&mut renderer, &gray_pool, bands[0], true, gray_id, 0);
    composite(&mut renderer, &white_pool, bands[1], true, white_id, 1);
    composite(&mut renderer, &blue_pool, bands[2], true, blue_id, 2);

    let idle = Damage::Partial(vec![false; rows]);
    let pixel = |shot: &[u8], y: u32| {
        let at = ((y * width + width / 2) * 4) as usize;
        (shot[at], shot[at + 1], shot[at + 2])
    };
    let is_gray = |(r, g, b): (u8, u8, u8)| (20..160).contains(&r) && g == r && b == r;
    let is_white = |(r, g, b): (u8, u8, u8)| r > 200 && g > 200 && b > 200;
    let is_blue = |(r, g, b): (u8, u8, u8)| r < 60 && g < 60 && b > 150;

    // Every pool reuses, each handed a sibling's grid, so reading its own prior
    // instances is the only way to come out with its own color.
    renderer.render_into(&device, &queue, &view, &live, frame(&idle));
    composite(&mut renderer, &white_pool, bands[0], false, gray_id, 0);
    composite(&mut renderer, &blue_pool, bands[1], false, white_id, 1);
    composite(&mut renderer, &gray_pool, bands[2], false, blue_id, 2);
    let held = read_back(&device, &queue, &target, width, height);
    let shape = (
        pixel(&held, band / 2),
        pixel(&held, band + band / 2),
        pixel(&held, band * 2 + band / 2),
    );
    assert!(
        is_gray(shape.0) && is_white(shape.1) && is_blue(shape.2),
        "each pool must redraw its own rows, not a sibling's: got {shape:?}"
    );

    // The gray pool settles and leaves the frame, so white and blue shift down a
    // position. Their buffers must follow their ids, not the positions.
    renderer.render_into(&device, &queue, &view, &live, frame(&idle));
    composite(&mut renderer, &gray_pool, bands[1], false, white_id, 0);
    composite(&mut renderer, &blue_pool, bands[2], false, blue_id, 1);
    let shifted = read_back(&device, &queue, &target, width, height);
    let survivors = (
        pixel(&shifted, band + band / 2),
        pixel(&shifted, band * 2 + band / 2),
    );
    assert!(
        is_white(survivors.0) && is_blue(survivors.1),
        "a pool leaving the frame must not hand its instances to the next one along: got {survivors:?}"
    );
}

/// Copy `texture` into a mappable buffer and return its RGBA bytes, row-major
/// with no padding (the caller sizes the texture so `4 * width` is 256-aligned).
fn read_back(
    device: &Device,
    queue: &Queue,
    texture: &Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some("pool keeps live readback"),
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

/// A pool composite whose glyph burst grows the atlas heals the live grid's
/// cached instances, so its glyph survives instead of freezing stale UVs.
///
/// The pool packs dozens of never-seen glyphs, overflowing the initial atlas and
/// doubling it. That rescales every normalized UV, so the live grid's cached
/// instances -- reused under empty damage -- must rebuild against the grown
/// atlas rather than sampling the pre-grow region.
#[test]
fn pool_grow_heals_live_instances() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("pool_keeps_live_grid: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 30;
    let cell_h = cell_size(font_size, 1.0)[1].round() as u32;
    // Wide and tall enough for the pool to pack enough distinct glyphs to
    // overflow the 256px atlas and force a grow.
    let (width, height) = (512u32, cell_h * 8);

    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("pool grow heals target"),
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
        black,
        white,
    );

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 1 && cols >= 1, "grid too small: {rows}x{cols}");

    let mut live = Grid::new(rows, cols);
    live.get_mut(0, 0).ch = 'M';
    live.get_mut(0, 0).fg = white;
    live.get_mut(0, 0).bg = black;

    // Every pool cell carries a distinct printable glyph, a burst of never-seen
    // masks that overflows the atlas and grows it.
    let mut pool = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            let idx = (r * cols + c) as u32;
            let ch = char::from_u32(0x21 + idx % 94).unwrap_or('#');
            let cell = pool.get_mut(r, c);
            cell.ch = ch;
            cell.fg = white;
            cell.bg = black;
        }
    }

    let no_decoration = Damage::Partial(Vec::new());
    let frame = |damage| Frame {
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
        decoration_damage: &no_decoration,
        scrolled_rows: 0,
    };

    renderer.render_into(&device, &queue, &view, &live, frame(&Damage::Full));
    let base = read_back(&device, &queue, &target, width, height);

    // Composite the distinct-glyph pool, growing the atlas, then render the live
    // grid again with no damage so it must heal its cached instances.
    renderer.composite_pool(
        &device,
        &queue,
        &view,
        &pool,
        &[],
        [0, 0, width, height],
        0.0,
        true,
        true,
        0,
        0,
    );
    let idle = Damage::Partial(vec![false; rows]);
    renderer.render_into(&device, &queue, &view, &live, frame(&idle));
    let after = read_back(&device, &queue, &target, width, height);

    let lit = |pixels: &[u8], band: u32| {
        let mut count = 0usize;
        for y in (band * cell_h)..((band + 1) * cell_h) {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                let (r, g, b) = (pixels[i] as u32, pixels[i + 1] as u32, pixels[i + 2] as u32);
                if r + g + b > 120 {
                    count += 1;
                }
            }
        }
        count
    };

    assert!(lit(&base, 0) > 0, "the glyph renders in the base frame");
    assert_eq!(
        lit(&after, 0),
        lit(&base, 0),
        "the live glyph must survive an atlas grow driven by a pool composite"
    );
}

/// Compositing a pool page of never-seen glyphs that overflow the atlas bumps
/// the content epoch; recompositing the same page (glyphs now resident) leaves
/// it unchanged.
///
/// This is the detection `render_with_pools` reports back so the app schedules a
/// heal frame only when a pool pass actually moved the atlas UVs.
#[test]
fn composite_pool_bumps_content_epoch_only_on_atlas_change() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("pool_keeps_live_grid: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 60;
    let cell_h = cell_size(font_size, 1.0)[1].round() as u32;
    // Large glyphs plus a full ASCII spread overflow the 256px atlas and grow it.
    let (width, height) = (512u32, cell_h * 8);

    let black = Rgb::new(0, 0, 0);
    let white = Rgb::new(255, 255, 255);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("content epoch target"),
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
        black,
        white,
    );

    let (rows, cols) = renderer.grid_size();

    let mut pool = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            let idx = (r * cols + c) as u32;
            let cell = pool.get_mut(r, c);
            cell.ch = char::from_u32(0x21 + idx % 94).unwrap_or('#');
            cell.fg = white;
            cell.bg = black;
        }
    }

    let full = [0, 0, width, height];
    let before = renderer.content_epoch();
    renderer.composite_pool(
        &device,
        &queue,
        &view,
        &pool,
        &[],
        full,
        0.0,
        true,
        true,
        0,
        0,
    );
    let after_new = renderer.content_epoch();
    assert_ne!(
        after_new, before,
        "a burst of never-seen glyphs that overflows the atlas must bump the epoch"
    );

    renderer.composite_pool(
        &device,
        &queue,
        &view,
        &pool,
        &[],
        full,
        0.0,
        true,
        true,
        0,
        0,
    );
    let after_repeat = renderer.content_epoch();
    assert_eq!(
        after_repeat, after_new,
        "recompositing the same resident glyphs must not bump the epoch"
    );
}
