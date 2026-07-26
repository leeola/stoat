//! Headless GPU check that stroked paths paint, anti-alias, occlude, and glide.
//!
//! The polyline pass is the only one drawing non-axis-aligned geometry, so the
//! things worth pinning are the ones a unit test over instance data cannot see:
//! that a diagonal's edge blends rather than stair-steps, that a zero-length
//! segment fills a disc, that a later panel hides a line beneath it, and that a
//! pooled path moves with the composite's eased shift. Skips when no GPU adapter
//! is present, so a GPU-less CI stays green.

use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{BorderStyle, Grid, Panel, PanelShadow, Polyline, Rgb},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

/// Sixteenths of a cell, the unit a polyline's coordinates are declared in.
const SIXTEENTHS: i16 = 16;

struct Harness {
    device: Device,
    queue: Queue,
    renderer: Renderer,
    target: Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
    cell_w: u32,
    cell_h: u32,
    rows: usize,
    cols: usize,
}

#[test]
fn a_diagonal_paints_with_blended_edges() {
    let Some(mut h) = Harness::new() else { return };

    let line = Rgb::new(220, 50, 47);
    let mut grid = h.filled_grid();
    // A diagonal across two cells. Snapped geometry would stair-step. The
    // capsule SDF has to leave partly-covered pixels along the edge instead.
    grid.set_polylines(vec![Polyline {
        points: vec![[0, 0], [2 * SIXTEENTHS, 2 * SIXTEENTHS]],
        width: 6,
        color: line,
        seq: 0,
    }]);

    let pixels = h.render_live(&grid);

    let center = h.pixel(&pixels, h.cell_w, h.cell_h);
    assert_eq!(
        center,
        (line.r, line.g, line.b),
        "the line covers its spine"
    );

    // Walk out from the spine perpendicular to it. A hard edge would jump from
    // the line color straight to the background. An anti-aliased one blends.
    let blended = (1..=8).any(|d| {
        let p = h.pixel(&pixels, h.cell_w + d, h.cell_h - d);
        p != (line.r, line.g, line.b) && p != BG
    });
    assert!(blended, "a diagonal's edge blends toward the background");
}

#[test]
fn a_zero_length_path_paints_a_disc() {
    let Some(mut h) = Harness::new() else { return };

    let dot = Rgb::new(50, 200, 90);
    let mut grid = h.filled_grid();
    // One point, at the center of cell (1, 1), a third of a cell across.
    grid.set_polylines(vec![Polyline {
        points: vec![[SIXTEENTHS + 8, SIXTEENTHS + 8]],
        width: 10,
        color: dot,
        seq: 0,
    }]);

    let pixels = h.render_live(&grid);

    let center = h.pixel(&pixels, h.cell_w + h.cell_w / 2, h.cell_h + h.cell_h / 2);
    assert_eq!(center, (dot.r, dot.g, dot.b), "the dot fills its center");
    assert_eq!(
        h.pixel(&pixels, h.cell_w / 4, h.cell_h / 4),
        BG,
        "and does not bleed a cell away"
    );
}

#[test]
fn a_later_panel_hides_the_line_beneath_it() {
    let Some(mut h) = Harness::new() else { return };

    let line = Rgb::new(220, 50, 47);
    let mut grid = h.filled_grid();
    grid.set_polylines(vec![Polyline {
        points: vec![[0, SIXTEENTHS + 8], [4 * SIXTEENTHS, SIXTEENTHS + 8]],
        width: 8,
        color: line,
        seq: 0,
    }]);
    // Declared after the path, covering columns 2 and 3 of row 1.
    grid.set_panels(vec![Panel {
        top: 1,
        left: 2,
        width: 2,
        height: 1,
        style: BorderStyle::Light,
        border: Rgb::new(90, 90, 120),
        corner_radius: 0,
        fill: Some(Rgb::new(90, 90, 120)),
        shadow: PanelShadow::None_,
        inset_x: 0,
        seq: 100,
    }]);

    let pixels = h.render_live(&grid);

    assert_eq!(
        h.pixel(&pixels, h.cell_w / 2, h.cell_h + h.cell_h / 2),
        (line.r, line.g, line.b),
        "the line paints where no box covers it"
    );
    assert_ne!(
        h.pixel(
            &pixels,
            2 * h.cell_w + h.cell_w / 2,
            h.cell_h + h.cell_h / 2
        ),
        (line.r, line.g, line.b),
        "and is discarded under the later-declared box"
    );
}

#[test]
fn a_pooled_path_glides_with_the_composite_shift() {
    let Some(mut h) = Harness::new() else { return };

    let line = Rgb::new(220, 50, 47);
    let base = h.filled_grid();
    let mut pool = h.filled_grid();
    // A horizontal run across row 1, so a whole-cell shift moves it to row 0.
    pool.set_polylines(vec![Polyline {
        points: vec![[0, SIXTEENTHS + 8], [4 * SIXTEENTHS, SIXTEENTHS + 8]],
        width: 8,
        color: line,
        seq: 0,
    }]);

    let unshifted = h.render_pool(&base, &pool, 0.0);
    assert_eq!(
        h.pixel(&unshifted, h.cell_w / 2, h.cell_h + h.cell_h / 2),
        (line.r, line.g, line.b),
        "the path paints row 1 unshifted"
    );

    let shifted = h.render_pool(&base, &pool, -1.0);
    assert_eq!(
        h.pixel(&shifted, h.cell_w / 2, h.cell_h / 2),
        (line.r, line.g, line.b),
        "and glides up to row 0"
    );
    assert_eq!(
        h.pixel(&shifted, h.cell_w / 2, h.cell_h + h.cell_h / 2),
        BG,
        "leaving row 1 as page background"
    );
}

/// The page background every grid is filled with, as a readback tuple.
const BG: (u8, u8, u8) = (10, 20, 30);

impl Harness {
    /// Build a headless renderer over a 4-row target, or `None` when the
    /// machine has no usable adapter.
    fn new() -> Option<Harness> {
        let Some((device, queue)) = headless_device() else {
            eprintln!("polyline_render: no wgpu adapter available, skipping");
            return None;
        };

        let format = TextureFormat::Rgba8Unorm;
        let font_size = 30;
        let [cell_w, cell_h] = cell_size(font_size, 1.0);
        let (cell_w, cell_h) = (cell_w.round() as u32, cell_h.round() as u32);
        let (width, height) = (128u32, cell_h * 4);

        let target = device.create_texture(&TextureDescriptor {
            label: Some("polyline target"),
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

        let black = Rgb::new(0, 0, 0);
        let renderer = Renderer::new(
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
            black,
        );

        let (rows, cols) = renderer.grid_size();
        assert!(rows >= 4 && cols >= 6, "grid too small: {rows}x{cols}");

        Some(Harness {
            device,
            queue,
            renderer,
            target,
            view,
            width,
            height,
            cell_w,
            cell_h,
            rows,
            cols,
        })
    }

    /// A grid of the renderer's size with every cell on the page background.
    fn filled_grid(&self) -> Grid {
        let mut grid = Grid::new(self.rows, self.cols);
        for r in 0..self.rows {
            for c in 0..self.cols {
                grid.get_mut(r, c).bg = Rgb::new(BG.0, BG.1, BG.2);
            }
        }
        grid
    }

    fn render_live(&mut self, grid: &Grid) -> Vec<u8> {
        self.renderer
            .render_into(&self.device, &self.queue, &self.view, grid, plain_frame());
        read_back(
            &self.device,
            &self.queue,
            &self.target,
            self.width,
            self.height,
        )
    }

    fn render_pool(&mut self, base: &Grid, pool: &Grid, shift: f32) -> Vec<u8> {
        let full = [0, 0, self.width, self.height];
        self.renderer
            .render_into(&self.device, &self.queue, &self.view, base, plain_frame());
        self.renderer.composite_pool(
            &self.device,
            &self.queue,
            &self.view,
            pool,
            &[],
            full,
            shift,
            true,
            true,
        );
        read_back(
            &self.device,
            &self.queue,
            &self.target,
            self.width,
            self.height,
        )
    }

    /// The color at physical pixel (`x`, `y`).
    fn pixel(&self, pixels: &[u8], x: u32, y: u32) -> (u8, u8, u8) {
        let i = ((y * self.width + x) * 4) as usize;
        (pixels[i], pixels[i + 1], pixels[i + 2])
    }
}

static FULL_DAMAGE: Damage = Damage::Full;
static NO_DECORATION: Damage = Damage::Partial(Vec::new());

/// A frame with no cursor and no scroll, which is all these draws need.
fn plain_frame() -> Frame<'static> {
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
        damage: &FULL_DAMAGE,
        decoration_damage: &NO_DECORATION,
    }
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
        label: Some("polyline readback"),
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
    queue.submit([encoder.finish()]);

    buffer.slice(..).map_async(MapMode::Read, |_| {});
    device
        .poll(PollType::wait_indefinitely())
        .expect("poll readback");
    buffer.slice(..).get_mapped_range().to_vec()
}
