//! Headless pixel check that a text run following a hand-drawn mark fades in
//! with it.
//!
//! The alpha rides a storage array the fragment stage indexes by a slot the
//! instance carries. Every step of that stays right on the Rust side even when
//! the shader ignores the array, so only reading the drawn pixels tells the two
//! apart. Skips when no GPU adapter is present so a GPU-less CI stays green.

use stoatty_protocol::command::{
    SketchBounds, SketchCommand, SketchEasing, SketchPhase, SketchShape, SketchStyle, SketchTiming,
};
use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll, SketchReveal},
    render::cell_size,
};
use stoatty_term::{
    grid::{Grid, Rgb, Sketch, TextRun},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

/// The ground beneath the backed run, which a faded rect blends into and an
/// opaque one wipes away.
const GROUND: Rgb = Rgb { r: 0, g: 0, b: 255 };

/// A label arrives as the box around it closes, not with the first pen stroke,
/// which names a shape not yet recognizable.
#[test]
fn a_followed_run_is_clear_early_and_painted_once_its_mark_is_drawn() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("follow_fade_render: no wgpu adapter available, skipping");
        return;
    };

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (width, height) = (256u32, (cell[1] * 10.0).round() as u32);

    let target = device.create_texture(&TextureDescriptor {
        label: Some("follow fade target"),
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
        Rgb::new(0, 0, 0),
        Rgb::new(255, 255, 255),
    );

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 4 && cols >= 4, "grid too small: {rows}x{cols}");

    // Red on a black surface, so any ink the run paints is unambiguous. The
    // second run carries a background and no ink of its own, which separates
    // the rect's fade from the glyphs'.
    let mut grid = Grid::new(rows, cols);
    // The cells under that second run carry a blue ground, so a rect that
    // replaces what it sits over reads differently from one that blends into
    // it.
    for col in 0..2 {
        grid.get_mut(5, col).bg = GROUND;
    }
    grid.set_sketches(vec![mark(1)]);
    grid.set_text_runs(vec![
        TextRun {
            col: 0,
            row: 16,
            scale: 256,
            color: Rgb::new(255, 0, 0),
            bg: None,
            follow: 1,
            anchor: None,
            text: "MMMM".into(),
            seq: 1,
        },
        TextRun {
            col: 0,
            row: 80,
            scale: 256,
            color: Rgb::new(0, 0, 0),
            bg: Some(Rgb::new(255, 0, 0)),
            follow: 1,
            anchor: None,
            text: "  ".into(),
            seq: 2,
        },
    ]);

    let mut frame_at = |progress: f32, opacity: f32| {
        renderer.render_into(
            &device,
            &queue,
            &view,
            &grid,
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
                decoration_damage: &Damage::Partial(Vec::new()),
                scrolled_rows: 0,
                sketch_reveals: &[SketchReveal {
                    revealed: progress,
                    width: 64.0,
                    alpha: opacity,
                }],
            },
        );

        read_back(&device, &queue, &target, width, height)
    };
    let red_count = |pixels: &[u8]| {
        pixels
            .as_chunks::<4>()
            .0
            .iter()
            .filter(|texel| texel[0] > 0)
            .count()
    };
    // A texel through the middle of the second run's background rect.
    let bg_texel = |pixels: &[u8]| {
        let (x, y) = ((cell[0] * 0.5) as u32, (cell[1] * 5.5) as u32);
        let i = ((y * width + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };

    // The mark is a fifth drawn, well short of where the fade starts.
    let early = frame_at(0.2, 1.0);
    assert_eq!(
        red_count(&early),
        0,
        "no ink while the mark is still being drawn"
    );
    assert_eq!(
        bg_texel(&early),
        [GROUND.r, GROUND.g, GROUND.b],
        "and the background waits with the glyphs rather than arriving whole"
    );

    let full = frame_at(1.0, 1.0);
    let whole = red_count(&full);
    assert!(whole > 0, "and the label paints once the mark is finished");
    assert_eq!(
        bg_texel(&full),
        [255, 0, 0],
        "with its background at full strength"
    );

    let midway = red_count(&frame_at(0.8, 1.0));
    assert!(
        midway > 0 && midway <= whole,
        "easing in between, {midway} against {whole}",
    );

    // A walkthrough dims the callouts the reader has walked past, and the label
    // recedes with the box around it rather than standing at full strength
    // inside a faded outline.
    let dimmed = red_count(&frame_at(1.0, 0.4));
    assert!(
        dimmed > 0 && dimmed < whole,
        "a dimmed mark carries its label dimmed, {dimmed} against {whole}",
    );
}

fn mark(id: u32) -> Sketch {
    Sketch {
        command: SketchCommand {
            id,
            style: SketchStyle {
                color: [0, 255, 0],
                alpha: 255,
                width: 64,
                roughness: 64,
                seed: 1,
            },
            timing: SketchTiming {
                delay_ms: 0,
                duration_ms: 400,
                easing: SketchEasing::Linear,
                phase: SketchPhase::Enter,
            },
            // Off in a corner the run does not sit on, which separates the
            // mark's own green stroke from the label's red.
            shape: SketchShape::Ellipse {
                bounds: SketchBounds {
                    x: 0,
                    y: 96,
                    w: 32,
                    h: 32,
                },
                fill: None,
            },
            anchor: None,
        },
        seq: 0,
    }
}

fn read_back(
    device: &Device,
    queue: &Queue,
    texture: &Texture,
    width: u32,
    height: u32,
) -> Vec<u8> {
    let buffer = device.create_buffer(&BufferDescriptor {
        label: Some("follow fade readback"),
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
    let data = buffer.slice(..).get_mapped_range().to_vec();
    buffer.unmap();
    data
}
