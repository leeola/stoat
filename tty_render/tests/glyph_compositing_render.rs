//! Headless pixel check that grid glyphs blend over the framebuffer.
//!
//! A glyph's partially-covered edge pixels must show whatever was drawn beneath
//! them, not an assumed cell background baked into the glyph instance. This
//! paints one white glyph over an opaque magenta panel fill while the glyph's
//! own cell background is a neutral gray, then reads the pixels back. Because
//! the glyph blends premultiplied over the magenta already in the framebuffer,
//! its anti-aliased edges are white-over-magenta mixes -- red and blue pinned
//! at full, green partial. A glyph that instead stamped its cell background
//! would leave only flat gray edges and pure magenta, never such a mix.
//! Fails without a GPU adapter, since a test that draws nothing proves nothing.

use stoatty_render::{
    gpu::{build_font_system, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
    test_support::{offscreen_target, read_back, require_headless_device},
};
use stoatty_term::{
    grid::{BorderStyle, Grid, Panel, PanelShadow, Rgb},
    term::Damage,
};
use wgpu::TextureFormat;

#[test]
fn grid_glyph_blends_over_the_framebuffer_not_an_assumed_bg() {
    let (device, queue) = require_headless_device();

    let format = TextureFormat::Rgba8Unorm;
    let font_size = 24;
    let cell = cell_size(font_size, 1.0);
    let (cell_w, cell_h) = (cell[0], cell[1]);
    let (width, height) = (256u32, (cell_h * 10.0).round() as u32);

    // The glyph's own cell background, distinct from the magenta beneath it so a
    // stamped-in background would be detectable as gray rather than a mix.
    let surface = Rgb::new(120, 120, 120);
    let fill = Rgb::new(255, 0, 255);

    let (target, view) = offscreen_target(&device, width, height);

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
        surface,
        Rgb::new(255, 255, 255),
    );

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 6 && cols >= 6, "grid too small: {rows}x{cols}");

    // A filled panel covering the interior, so the framebuffer beneath the glyph
    // is opaque magenta rather than the cell background.
    let panel = Panel {
        top: 1,
        left: 1,
        width: cols as u16 - 2,
        height: rows as u16 - 2,
        style: BorderStyle::Rounded,
        border: Rgb::new(200, 100, 50),
        corner_radius: 6,
        fill: Some(fill),
        shadow: PanelShadow::None_,
        inset_x: 0,
        above_pools: false,
        anchor: None,
        seq: 0,
    };

    // A white glyph deep in the panel interior, its cell background left neutral.
    let (grow, gcol) = (rows / 2, cols / 2);
    let mut grid = Grid::new(rows, cols);
    for r in 0..rows {
        for c in 0..cols {
            grid.get_mut(r, c).bg = surface;
        }
    }
    {
        let cell = grid.get_mut(grow, gcol);
        cell.ch = 'X';
        cell.fg = Rgb::new(255, 255, 255);
        cell.bg = surface;
    }
    grid.set_panels(vec![panel]);

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
            sketch_reveals: &[],
        },
    );

    let pixels = read_back(&device, &queue, &target, width, height);
    let px = |x: u32, y: u32| -> [u8; 3] {
        let i = ((y * width + x) * 4) as usize;
        [pixels[i], pixels[i + 1], pixels[i + 2]]
    };

    let x0 = (gcol as f32 * cell_w).floor() as u32;
    let x1 = ((gcol as f32 + 1.0) * cell_w).ceil() as u32;
    let y0 = (grow as f32 * cell_h).floor() as u32;
    let y1 = ((grow as f32 + 1.0) * cell_h).ceil() as u32;

    let (mut ink, mut magenta, mut blend) = (false, false, false);
    for y in y0..y1.min(height) {
        for x in x0..x1.min(width) {
            let [r, g, b] = px(x, y);
            ink |= r >= 250 && g >= 250 && b >= 250;
            magenta |= r >= 250 && b >= 250 && g <= 6;
            blend |= r >= 250 && b >= 250 && (8..=247).contains(&g);
        }
    }

    assert!(
        magenta,
        "panel fill should reach the glyph cell as opaque magenta"
    );
    assert!(ink, "the white glyph should render at full coverage");
    assert!(
        blend,
        "the glyph's anti-aliased edges should be white-over-magenta mixes \
         (red/blue full, green partial), proving it blended over the framebuffer \
         rather than stamping its cell background"
    );
}
