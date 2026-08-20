//! Headless GPU checks that a row reaches the screen through the slot its
//! instances were written to.
//!
//! The text pass stores each instance's buffer slot rather than the display row
//! it paints, keeps its caches in slot order, and advances a rotation instead of
//! moving anything when the screen scrolls. text.wgsl takes the slot back to a
//! row. Both halves are pinned without a device by transcribing the shader's
//! arithmetic, which checks the round trip against the transcription but never
//! against the shader, and says nothing about the caches or the upload plan
//! agreeing on where a row's bytes live.
//!
//! These render instances whose rows are far enough apart to read off the
//! surface and assert each lands in its own band. Skips when no GPU adapter is
//! present, so a GPU-less CI stays green.

use std::ops::Range;
use stoatty_render::{
    gpu::{build_font_system, headless_device, FontConfig, Frame, Renderer, Scroll},
    render::cell_size,
};
use stoatty_term::{
    grid::{whole_row, Grid, Overlay, Rgb, TextRun, UnderlineStyle},
    term::Damage,
};
use wgpu::{
    BufferDescriptor, BufferUsages, CommandEncoderDescriptor, Device, Extent3d, MapMode, Origin3d,
    PollType, Queue, TexelCopyBufferInfo, TexelCopyBufferLayout, TexelCopyTextureInfo, Texture,
    TextureAspect, TextureDescriptor, TextureDimension, TextureFormat, TextureUsages,
    TextureViewDescriptor,
};

const ROWS: u32 = 6;
/// Columns the harnesses whose damage vectors name whole rows are built at.
const COLS: usize = 8;

/// One color per absolute line, so which line a row holds is readable off the
/// surface.
///
/// The palette cycles wider than the screen is tall, so no two lines visible at
/// once share a color and a row off by any amount reads wrong.
fn line_color(line: u32) -> Rgb {
    Rgb::new(40 + (line % 8) as u8 * 25, 240, 60)
}

/// A screen whose top row is absolute line `top`, each row marked in its line's
/// color.
///
/// Marked twice over, an underline and a filled block, because the two travel in
/// different buffers with their own upload plans and only the block goes through
/// the glyph cache.
fn line_screen(top: u32) -> Grid {
    let mut grid = Grid::new(ROWS as usize, 8);
    for row in 0..ROWS {
        let color = line_color(top + row);
        let cell = grid.get_mut(row as usize, 0);
        cell.underline = UnderlineStyle::Straight;
        cell.underline_color = color;

        let block = grid.get_mut(row as usize, 3);
        block.ch = '\u{2588}';
        block.fg = color;
    }
    grid
}

/// Underlines are grid-row instances, so each one's slot is rotated and the
/// shader has to take it back. Two rows apart in one frame catch a stage that
/// dropped the row term or folded every slot onto one row.
#[test]
fn an_underline_paints_on_the_row_its_slot_names() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("row_slot_render: no wgpu adapter available, skipping");
        return;
    };

    let color = Rgb::new(240, 40, 40);
    let mut grid = Grid::new(ROWS as usize, 8);
    for row in [1usize, 4] {
        let cell = grid.get_mut(row, 0);
        cell.underline = UnderlineStyle::Straight;
        cell.underline_color = color;
    }

    let mut harness = Harness::new(&device, 8);
    let pixels = harness.render(&device, &queue, &grid, &Damage::Full, 0);

    assert_eq!(
        harness.rows_painting(&pixels, color),
        vec![1, 4],
        "each underline must paint in the row its slot names"
    );
}

/// A scroll advances the rotation instead of moving anything, so a row it kept
/// is already in the slot the new rotation looks for. Nothing re-uploads it, and
/// the only thing between its bytes and the right place on screen is the two
/// halves of the slot arithmetic agreeing.
///
/// Walked far enough to wrap the rotation twice, with an interior row damaged
/// alongside the exposed one so more than one slot is patched per frame. The
/// slots of two ascending rows straddle the wrap, and the upload plan counts
/// through the buffer once, so it has to receive them in slot order.
#[test]
fn a_scrolled_screen_keeps_every_row_where_the_shader_looks_for_it() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("row_slot_render: no wgpu adapter available, skipping");
        return;
    };

    let mut harness = Harness::new(&device, 8);
    harness.render(&device, &queue, &line_screen(0), &Damage::Full, 0);

    for top in 1..=(2 * ROWS + 1) {
        // The exposed row is new, and an interior row is redrawn alongside it.
        let mut damage = vec![None; ROWS as usize];
        damage[ROWS as usize - 1] = whole_row(COLS);
        damage[1] = whole_row(COLS);

        let pixels = harness.render(
            &device,
            &queue,
            &line_screen(top),
            &Damage::Partial(damage),
            1,
        );

        for row in 0..ROWS {
            let color = line_color(top + row);
            assert_eq!(
                harness.rows_underlined(&pixels, color),
                vec![row],
                "after scrolling to line {top}, row {row} must be underlined for line {}",
                top + row
            );
            assert_eq!(
                harness.rows_blocked(&pixels, color),
                vec![row],
                "after scrolling to line {top}, row {row} must be blocked for line {}",
                top + row
            );
        }
    }
}

/// A scroll empties the slots it carried past the end whether or not anything
/// rebuilds them, and an emptied slot is shorter than the buffer was written
/// with, which moves every slot packed after it. Nothing else reports that, so
/// the buffer has to rewrite from the emptied slot on its own account.
///
/// Repeated from every rotation, because an emptied slot that happens to be the
/// last one displaces nothing and would hide the omission.
#[test]
fn a_scroll_that_rebuilds_nothing_still_moves_the_rows_it_kept() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("row_slot_render: no wgpu adapter available, skipping");
        return;
    };

    for settled in 0..ROWS {
        let mut harness = Harness::new(&device, 8);
        harness.render(&device, &queue, &line_screen(0), &Damage::Full, 0);

        // Scroll to each rotation in turn, repainting as it goes.
        let mut last_row = vec![None; ROWS as usize];
        last_row[ROWS as usize - 1] = whole_row(COLS);
        for top in 1..=settled {
            harness.render(
                &device,
                &queue,
                &line_screen(top),
                &Damage::Partial(last_row.clone()),
                1,
            );
        }

        // One more scroll that reports no damage at all. The rows it kept still
        // have to move up, and the row it exposed has nothing to show.
        let top = settled + 1;
        let pixels = harness.render(
            &device,
            &queue,
            &line_screen(top),
            &Damage::Partial(vec![None; ROWS as usize]),
            1,
        );

        for row in 0..ROWS - 1 {
            let color = line_color(top + row);
            assert_eq!(
                harness.rows_underlined(&pixels, color),
                vec![row],
                "from rotation {settled}, row {row} must be underlined for the line it carried"
            );
            assert_eq!(
                harness.rows_blocked(&pixels, color),
                vec![row],
                "from rotation {settled}, row {row} must be blocked for the line it carried"
            );
        }
        // Against the whole palette, not just the line that would have been
        // there. A slot left holding the row that scrolled off still paints, in
        // that row's color rather than this one's.
        for line in 0..8 {
            let color = line_color(line);
            assert!(
                !harness
                    .rows_underlined(&pixels, color)
                    .contains(&(ROWS - 1)),
                "from rotation {settled}, the exposed row is still underlined for line {line}"
            );
            assert!(
                !harness.rows_blocked(&pixels, color).contains(&(ROWS - 1)),
                "from rotation {settled}, the exposed row is still blocked for line {line}"
            );
        }
    }
}

/// An overlay's content rows are positions its builder chose, not grid rows, so
/// the screen-anchored draws carry no rotation at all. A stage that wrapped them
/// at the grid height would fold a box's later lines back over its first.
#[test]
fn overlay_content_lines_do_not_wrap_at_the_grid_height() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("row_slot_render: no wgpu adapter available, skipping");
        return;
    };

    let content_fg = Rgb::new(20, 240, 60);
    let mut grid = Grid::new(ROWS as usize, 12);
    grid.set_overlays(vec![Overlay {
        top: 0,
        left: 0,
        width: 10,
        height: 5,
        fill: Rgb::new(0, 0, 0),
        border: Rgb::new(0, 0, 0),
        content_fg,
        scale: 1,
        offset: [0, 0],
        bold: false,
        // Three lines, laid out from the row below the box's top edge.
        content: "MMM\nMMM\nMMM".to_owned(),
    }]);

    let mut harness = Harness::new(&device, 12);
    let pixels = harness.render(&device, &queue, &grid, &Damage::Full, 0);

    assert_eq!(
        harness.rows_painting(&pixels, content_fg),
        vec![1, 2, 3],
        "each content line must keep its own row"
    );
}

/// A composited pool declares its text runs absolutely, not per grid row, so
/// nothing about the rotation the live grid is carrying may reach them. A run
/// baked at slot zero under rotating globals lands where the rotation maps zero
/// back to, which is most of the grid height away from where it was declared.
#[test]
fn a_pool_text_run_paints_on_its_own_row_after_the_live_grid_scrolls() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("row_slot_render: no wgpu adapter available, skipping");
        return;
    };

    let mut harness = Harness::new(&device, COLS as u32);
    harness.render(&device, &queue, &line_screen(0), &Damage::Full, 0);
    harness.render(&device, &queue, &line_screen(1), &Damage::Full, 1);

    let run_color = Rgb::new(240, 40, 200);
    let mut pool = Grid::new(ROWS as usize, COLS);
    pool.set_text_runs(vec![TextRun {
        col: 0,
        row: 2 * 16,
        scale: 256,
        color: run_color,
        bg: None,
        text: "MMM".into(),
        seq: 0,
    }]);

    let pixels = harness.composite(&device, &queue, &pool, 0.0, None);
    assert_eq!(
        harness.rows_painting(&pixels, run_color),
        vec![2],
        "the run must paint on the row it declared",
    );
}

/// A screen like [`line_screen`] whose lines differ in how much they hold and
/// in where they hold it.
///
/// A row that comes back a different length displaces every row after it in the
/// buffer, which is where the upload plan has to stop patching. A line and the
/// line a screen below it therefore mark different numbers of cells, so every
/// scroll resizes the slot it exposes.
///
/// They also mark cells that do not overlap, so a slot that kept what it held
/// paints the old line beside the new one rather than under it.
fn varied_screen(top: u32) -> Grid {
    let mut grid = Grid::new(ROWS as usize, COLS);
    for row in 0..ROWS {
        let line = top + row;
        let color = line_color(line);
        for col in 0..=(line % 5).min(2) as usize {
            let cell = grid.get_mut(row as usize, col);
            cell.underline = UnderlineStyle::Straight;
            cell.underline_color = color;
        }

        let from = 3 + (line % 5) as usize;
        for col in from..(from + (line % 5) as usize + 1).min(COLS) {
            let block = grid.get_mut(row as usize, col);
            block.ch = '\u{2588}';
            block.fg = color;
        }
    }
    grid
}

/// A composited pool that carried its rows across a scroll paints them where a
/// pool built from nothing paints them.
///
/// The pool keeps each row in the slot it shaped the row into and advances a
/// rotation, then uploads only the rows the scroll exposed. A plan that mislaid
/// a row leaves it drawn from another row's bytes, or drops it. Marked twice
/// over, since the underlines and the glyphs travel in separate buffers with
/// their own plans.
///
/// The underline strip is compared byte for byte, which it can be: an underline
/// instance carries no row in its geometry, only the slot the shader reads a row
/// back from. The glyph columns are not. A carried glyph keeps the pixel snap of
/// the row it was shaped for, so a row that moved sits up to half a pixel off the
/// grid until something reshapes it, which is the row cache's standing trade and
/// the live grid's too.
#[test]
fn a_scrolled_composite_paints_what_a_rebuilt_one_paints() {
    let Some((device, queue)) = headless_device() else {
        eprintln!("row_slot_render: no wgpu adapter available, skipping");
        return;
    };

    let blank = Grid::new(ROWS as usize, COLS);

    // The rotation walks a row at a time, then jumps far enough that the slots
    // the jump exposes run off the end of the buffer and wrap.
    let mut carried = Harness::new(&device, COLS as u32);
    carried.render(&device, &queue, &blank, &Damage::Full, 0);
    carried.composite(&device, &queue, &varied_screen(0), 0.0, None);
    for top in 1..=5 {
        carried.composite(&device, &queue, &varied_screen(top), 0.0, Some(1));
    }
    let scrolled = carried.composite(&device, &queue, &varied_screen(9), 0.0, Some(4));

    let mut fresh = Harness::new(&device, COLS as u32);
    fresh.render(&device, &queue, &blank, &Damage::Full, 0);
    let rebuilt = fresh.composite(&device, &queue, &varied_screen(9), 0.0, None);

    for row in 0..ROWS {
        let color = line_color(9 + row);
        assert_eq!(
            carried.rows_underlined(&rebuilt, color),
            vec![row],
            "the rebuilt pool has to underline row {row} for the line it holds",
        );
        assert_eq!(
            carried.rows_underlined(&scrolled, color),
            vec![row],
            "the carried pool must underline row {row} for the line it holds",
        );
        assert_eq!(
            carried.rows_blocked(&scrolled, color),
            vec![row],
            "the carried pool must block row {row} for the line it holds",
        );
    }

    // Nothing left over from the line that scrolled off, nor from any line the
    // palette holds that this screen never showed.
    for line in [0, 7] {
        let color = line_color(line);
        assert_eq!(
            carried.rows_underlined(&scrolled, color),
            Vec::new(),
            "no row may keep line {line}, which is off the screen",
        );
        assert_eq!(
            carried.rows_blocked(&scrolled, color),
            Vec::new(),
            "no row may block line {line}, which is off the screen",
        );
    }

    let strip = carried.strip(&scrolled, 0..carried.cell_w);
    let differs = strip
        .chunks_exact(4)
        .zip(carried.strip(&rebuilt, 0..carried.cell_w).chunks_exact(4))
        .position(|(got, want)| got != want);
    assert_eq!(
        differs, None,
        "a carried underline must land on the pixels a rebuilt one lands on",
    );
}

/// One renderer and one surface across a run of frames, so the caches and the
/// rotation carry between them the way they do in a live terminal.
struct Harness {
    renderer: Renderer,
    target: Texture,
    view: wgpu::TextureView,
    width: u32,
    height: u32,
    cell_w: u32,
    cell_h: u32,
}

impl Harness {
    fn new(device: &Device, cols: u32) -> Self {
        let format = TextureFormat::Rgba8Unorm;
        let font_size = 30;
        let [cell_w, cell_h] = cell_size(font_size, 1.0);
        let (cell_w, cell_h) = (cell_w.round() as u32, cell_h.round() as u32);

        // Widened to a whole number of readback rows. A texture copy's bytes per
        // row must be 256-aligned, and the surplus columns paint the background.
        let width = (cell_w * cols).next_multiple_of(64);
        let height = cell_h * ROWS;

        let target = device.create_texture(&TextureDescriptor {
            label: Some("row slot target"),
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
            Rgb::new(0, 0, 0),
        );

        Harness {
            renderer,
            target,
            view,
            width,
            height,
            cell_w,
            cell_h,
        }
    }

    fn render(
        &mut self,
        device: &Device,
        queue: &Queue,
        grid: &Grid,
        damage: &Damage,
        scrolled_rows: isize,
    ) -> Vec<u8> {
        self.renderer.render_into(
            device,
            queue,
            &self.view,
            grid,
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
                decoration_damage: &Damage::Partial(Vec::new()),
                scrolled_rows,
            },
        );

        read_back(device, queue, &self.target, self.width, self.height)
    }

    /// Composite `pool` over what the surface already holds, and read it back.
    fn composite(
        &mut self,
        device: &Device,
        queue: &Queue,
        pool: &Grid,
        shift_rows: f32,
        scrolled_rows: Option<isize>,
    ) -> Vec<u8> {
        self.renderer.composite_pool(
            device,
            queue,
            &self.view,
            pool,
            &[],
            [0, 0, self.width, self.height],
            shift_rows,
            [0.0; 2],
            true,
            scrolled_rows,
            false,
            1,
            0,
        );

        read_back(device, queue, &self.target, self.width, self.height)
    }

    /// The pixels of `columns`, every row of them, in row-major order.
    fn strip(&self, pixels: &[u8], columns: Range<u32>) -> Vec<u8> {
        (0..self.height)
            .flat_map(|y| {
                columns.clone().flat_map(move |x| {
                    let i = ((y * self.width + x) * 4) as usize;
                    i..i + 4
                })
            })
            .map(|i| pixels[i])
            .collect()
    }

    /// The grid rows carrying a pixel within tolerance of `color`, ascending.
    ///
    /// Sampled across each row's whole band rather than at one point, since a
    /// glyph covers only part of a cell and its coverage is blended against
    /// whatever is behind it.
    fn rows_painting(&self, pixels: &[u8], color: Rgb) -> Vec<u32> {
        self.rows_matching(pixels, color, 0..self.width, |band| band)
    }

    /// The rows whose underline is within tolerance of `color`, ascending.
    ///
    /// Read from the first column, which is the only one [`line_screen`] marks,
    /// so nothing else lands in the strip. An underline's quad is exactly its
    /// cell, so its whole band can be swept without catching a neighbour.
    fn rows_underlined(&self, pixels: &[u8], color: Rgb) -> Vec<u32> {
        self.rows_matching(pixels, color, 0..self.cell_w, |band| band)
    }

    /// The rows whose filled block is within tolerance of `color`, ascending.
    ///
    /// Read from one scanline down the middle of each row. A block glyph is
    /// rasterized to its own bitmap rather than clipped to the cell, so it can
    /// reach a few pixels into the row above and sweeping the whole band would
    /// report a row twice over.
    fn rows_blocked(&self, pixels: &[u8], color: Rgb) -> Vec<u32> {
        self.rows_matching(pixels, color, 0..self.width, |band| {
            let middle = (band.start + band.end) / 2;
            middle..middle + 1
        })
    }

    fn rows_matching(
        &self,
        pixels: &[u8],
        color: Rgb,
        columns: Range<u32>,
        rows_of: impl Fn(Range<u32>) -> Range<u32>,
    ) -> Vec<u32> {
        let near = |i: usize| {
            let channels = [
                (pixels[i], color.r),
                (pixels[i + 1], color.g),
                (pixels[i + 2], color.b),
            ];
            channels.iter().all(|(got, want)| got.abs_diff(*want) <= 6)
        };

        (0..ROWS)
            .filter(|row| {
                let band = (row * self.cell_h)..((row + 1) * self.cell_h).min(self.height);
                rows_of(band)
                    .flat_map(|y| {
                        columns
                            .clone()
                            .map(move |x| ((y * self.width + x) * 4) as usize)
                    })
                    .any(near)
            })
            .collect()
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
        label: Some("row slot readback"),
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
