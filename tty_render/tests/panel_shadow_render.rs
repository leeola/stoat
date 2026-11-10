//! Headless pixel check that a panel's drop shadow stays outside its box.
//!
//! An unfilled panel (`fill: None`, what stoat's modals emit) must not wash its
//! own interior with the drop shadow. This renders one such panel over a
//! non-black clear off-screen and reads the pixels back, asserting the panel's
//! interior center keeps the clear color while a pixel just past the box's
//! bottom-right edge is darkened by the shadow.
//! Fails without a GPU adapter, since a test that draws nothing proves nothing.

use stoatty_protocol::command::Edge;
use stoatty_render::{
    gpu::{HostRide, PoolComposite},
    render::cell_size,
    test_support::{
        self, offscreen_target, pixel, read_back, require_headless_device, still_frame,
    },
};
use stoatty_term::{
    grid::{BorderStyle, Grid, Overlay, Panel, PanelShadow, PopoverChrome, Rgb},
    term::Damage,
};

/// The surface (clear) color the panel is rendered over, so darkening or fill is
/// measurable against a known non-black background.
const SURFACE: [u8; 3] = [120, 120, 120];

/// Rendered pixels of one panel over the [`SURFACE`] clear, plus the geometry
/// needed to index into them.
struct Rendered {
    pixels: Vec<u8>,
    width: u32,
    cell: [f32; 2],
    rows: usize,
    cols: usize,
}

impl Rendered {
    fn px(&self, x: u32, y: u32) -> [u8; 3] {
        pixel(&self.pixels, self.width, x, y)
    }
}

/// Render the panel `build` produces (given the grid size) over the clear and
/// read the pixels back.
fn render_panel(build: impl FnOnce(usize, usize) -> Panel) -> Rendered {
    render_scene(1.0, |grid, rows, cols| {
        grid.set_panels(vec![build(rows, cols)]);
    })
}

/// Render the chrome `build` puts on the grid over the clear at `scale_factor`,
/// and read the pixels back.
fn render_scene(scale_factor: f32, build: impl FnOnce(&mut Grid, usize, usize)) -> Rendered {
    render_scene_over(scale_factor, None, build)
}

/// [`render_scene`] with `pool` composited over the live grid.
///
/// A pool is one grid pane pool across the whole surface, of cells in the
/// color it names. With its flag set, its composite leaves the cells of the
/// clear color undrawn, as a terminal pool's does. A panel anchored to host 0
/// rides the pool with no shift.
fn render_scene_over(
    scale_factor: f32,
    pool: Option<(Rgb, bool)>,
    build: impl FnOnce(&mut Grid, usize, usize),
) -> Rendered {
    let (device, queue) = require_headless_device();

    let font_size = 24;
    let cell = cell_size(font_size, scale_factor);
    let (width, height) = (256u32, (cell[1] * 8.0).round() as u32);
    let surface = Rgb::new(SURFACE[0], SURFACE[1], SURFACE[2]);

    let (target, view) = offscreen_target(&device, width, height);

    let mut renderer =
        test_support::renderer(&device, [width, height], font_size, scale_factor, surface);

    let (rows, cols) = renderer.grid_size();
    assert!(rows >= 5 && cols >= 6, "grid too small: {rows}x{cols}");

    let filled = |color: Rgb| {
        let mut grid = Grid::new(rows, cols);
        for r in 0..rows {
            for c in 0..cols {
                grid.get_mut(r, c).bg = color;
            }
        }
        grid
    };
    let mut grid = filled(surface);
    build(&mut grid, rows, cols);

    let (full, partial) = (Damage::Full, Damage::Partial(Vec::new()));
    let frame = still_frame(&full, &partial);
    match pool {
        None => renderer.render_into(&device, &queue, &view, &grid, frame),
        Some((color, skip_clear)) => {
            let pool_grid = filled(color);
            let pool = PoolComposite {
                id: 0,
                grid: &pool_grid,
                font_size: None,
                origin_cells: [0.0; 2],
                scissor: [0, 0, width, height],
                shift_rows: 0.0,
                content_changed: true,
                scrolled_rows: None,
                occludable: true,
                skip_clear,
                changed_rows: None,
                dim: 0.0,
            };
            let ride = HostRide {
                host: 0,
                top_rows: 0.0,
                scissor: [0, 0, width, height],
            };
            renderer.render_pools_into(&device, &queue, &view, &grid, frame, &[pool], &[ride]);
        },
    }

    let pixels = read_back(&device, &queue, &target, width, height);
    Rendered {
        pixels,
        width,
        cell,
        rows,
        cols,
    }
}

#[test]
fn unfilled_panel_shadow_stays_outside_the_box() {
    // A panel inset one cell from the top-left, leaving a couple cells of margin
    // at the bottom-right for the [5,7]px shadow to fall into.
    let r = render_panel(|rows, cols| Panel {
        top: 1,
        left: 1,
        width: cols as u16 - 3,
        height: rows as u16 - 3,
        style: BorderStyle::Rounded,
        border: Rgb::new(200, 100, 50),
        corner_radius: 6,
        fill: None,
        shadow: PanelShadow::Drop,
        edges: Edge::symmetric(0, 0),
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: false,
        backdrop: false,
        anchor: None,
        seq: 0,
    });

    let box_left = 1.0 * r.cell[0];
    let box_right = (1.0 + (r.cols as f32 - 3.0)) * r.cell[0];
    let box_bottom = (1.0 + (r.rows as f32 - 3.0)) * r.cell[1];

    let center = r.px((box_right * 0.5) as u32, (box_bottom * 0.5) as u32);
    // A few px past the box's bottom edge, inside the offset shadow rect and
    // clear of the corner, where a blurred rect is at its darkest.
    let exterior = r.px(box_right as u32 - 3, box_bottom as u32 + 3);

    assert!(
        center
            .iter()
            .zip(SURFACE)
            .all(|(&got, want)| got.abs_diff(want) <= 1),
        "panel interior center should keep the clear color, got {center:?}"
    );
    assert!(
        exterior[0] < center[0].saturating_sub(20),
        "a pixel past the box's bottom edge should be shadow-darkened, \
         got exterior {exterior:?} vs center {center:?}"
    );

    // A blurred rect falls off with distance from its edge. A sharp rect faded
    // by its exterior distance alone is flat everywhere inside itself, so these
    // two read the same there.
    let mid_y = (box_bottom * 0.5) as u32;
    let near = r.px(box_right as u32 + 1, mid_y);
    let far = r.px(box_right as u32 + 4, mid_y);
    assert!(
        far[0] > near[0] + 2,
        "the shadow lightens away from the box's right edge, got near {near:?} then far {far:?}"
    );

    // The blur follows the rect's corner, so the corner casts less than the run
    // that leaves it. A rect faded by its exterior distance casts a square
    // plateau that reads the same at both.
    let corner = r.px(box_left as u32 + 3, box_bottom as u32 + 3);
    let along = r.px(box_left as u32 + 12, box_bottom as u32 + 3);
    assert!(
        corner[0] > along[0] + 4,
        "the shadow's corner is lighter than its bottom run, got corner {corner:?} then {along:?}"
    );
}

/// A cast shadow falls only under the box's bottom edge and between its sides,
/// as a pane clips the shadow of a pinned row as wide as the pane. It darkens
/// the pixel 3 px under the bottom edge and fades out by 17 px under it. The
/// ground shows over the box and beside its sides.
#[test]
fn a_cast_shadow_falls_under_the_box_alone() {
    let r = render_panel(|_, cols| unframed(cols as u16 - 3, PanelShadow::Cast));
    let [left, right] = [1.0, r.cols as f32 - 2.0].map(|col| (col * r.cell[0]) as u32);
    let [top, bottom] = [1.0, 3.0].map(|row| (row * r.cell[1]) as u32);
    let middle = (left + right) / 2;
    let under = r.px(middle, bottom + 3);
    let grounds = [
        [middle, bottom + 17],
        [middle, top - 3],
        [left - 3, bottom + 3],
        [right + 3, bottom + 3],
    ]
    .map(|[x, y]| r.px(x, y));

    assert!(
        under.iter().zip(SURFACE).all(|(&got, ground)| got < ground),
        "the pixel under the bottom edge darkens, got {under:?}"
    );
    assert!(
        grounds
            .iter()
            .flatten()
            .zip(SURFACE.iter().cycle())
            .all(|(&got, &ground)| got.abs_diff(ground) <= 1),
        "far under the box, over it, and beside its sides the ground shows, got {grounds:?}"
    );
}

/// A panel's exterior shadow falls on a pane pool's composite as it falls on
/// the live grid.
///
/// The shadowless box comes first in the list, so the drop shadow beside it is
/// not occluded inside it. The live grid there holds one shadow, which the
/// composite leaves in place, and the pooled frame matches the frame with no
/// pool there, one shadow and not two. A pool paints over the box of a
/// backdrop and of a panel that rides it, so the shadow falls inside an
/// earlier one of each, and a later backdrop hides it as it does on the live
/// grid. A terminal pool leaves its cells of the clear color to the live grid,
/// so its frame takes no second shadow.
#[test]
fn a_panel_shadow_falls_on_a_pane_pool_composite() {
    let pool = Rgb::new(60, 140, 200);
    let surface = Rgb::new(SURFACE[0], SURFACE[1], SURFACE[2]);
    let backdrop = |left| Panel {
        top: 3,
        left,
        height: 1,
        backdrop: true,
        ..unframed(1, PanelShadow::None_)
    };
    let panels = [
        Panel {
            anchor: Some((0, 0.0)),
            backdrop: false,
            ..backdrop(4)
        },
        backdrop(1),
        Panel {
            left: 4,
            ..unframed(2, PanelShadow::None_)
        },
        unframed(3, PanelShadow::Drop),
        backdrop(3),
    ];
    let build = |grid: &mut Grid, _: usize, _: usize| grid.set_panels(panels.to_vec());
    let [plain, pooled, terminal] = [None, Some((pool, false)), Some((surface, true))]
        .map(|pool| render_scene_over(1.0, pool, build));

    let [cell_w, cell_h] = plain.cell;
    let under_at = |col: f32| ((col * cell_w) as u32, (3.0 * cell_h) as u32 + 4);
    let (early, under, late, riding) = (under_at(1.5), under_at(2.5), under_at(3.5), under_at(4.2));
    let inside = ((4.0 * cell_w) as u32 + 3, (2.0 * cell_h) as u32);
    let away = (plain.width - 20, (6.0 * cell_h) as u32);
    let at = |r: &Rendered, (x, y): (u32, u32)| r.px(x, y);
    let darker = |got: [u8; 3], than: [u8; 3]| got.iter().zip(than).all(|(&c, t)| c < t);
    let near = |got: [u8; 3], want: [u8; 3]| got.iter().zip(want).all(|(&c, w)| c.abs_diff(w) <= 1);
    let pool_rgb = [pool.r, pool.g, pool.b];

    assert!(
        darker(at(&pooled, under), pool_rgb),
        "the shadow darkens the composite under the box, got {:?}",
        at(&pooled, under)
    );
    assert!(
        near(at(&pooled, away), pool_rgb),
        "far from both boxes the composite shows, got {:?}",
        at(&pooled, away)
    );
    assert!(
        darker(at(&plain, inside), SURFACE),
        "the shadow reaches inside the shadowless box, got {:?}",
        at(&plain, inside)
    );
    assert!(
        near(at(&pooled, inside), at(&plain, inside)),
        "inside the shadowless box one shadow shows and not two, got {:?} over {:?}",
        at(&pooled, inside),
        at(&plain, inside)
    );
    assert!(
        darker(at(&pooled, early), pool_rgb)
            && darker(at(&pooled, riding), pool_rgb)
            && near(at(&pooled, late), pool_rgb),
        "the shadow falls in the earlier backdrop and the riding box and not in the later \
         backdrop, got {:?}, {:?}, and {:?}",
        at(&pooled, early),
        at(&pooled, riding),
        at(&pooled, late)
    );
    assert!(
        near(at(&terminal, under), at(&plain, under)),
        "a terminal pool takes no second shadow, got {:?} over {:?}",
        at(&terminal, under),
        at(&plain, under)
    );
}

/// An unfilled, unframed panel two rows tall at row 1 and column 1, `width`
/// cells wide, that casts `shadow`.
fn unframed(width: u16, shadow: PanelShadow) -> Panel {
    Panel {
        top: 1,
        left: 1,
        width,
        height: 2,
        style: BorderStyle::None_,
        border: Rgb::new(200, 100, 50),
        corner_radius: 0,
        fill: None,
        shadow,
        edges: Edge::symmetric(0, 0),
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: false,
        backdrop: false,
        anchor: None,
        seq: 0,
    }
}

#[test]
fn a_horizontal_inset_leaves_the_cell_edge_strip_clear() {
    let inset = 6u8;
    let r = render_panel(|rows, cols| Panel {
        top: 1,
        left: 1,
        width: cols as u16 - 2,
        height: rows as u16 - 3,
        style: BorderStyle::Rounded,
        border: Rgb::new(200, 100, 50),
        corner_radius: 6,
        // Fill the box so the inset frame's interior is visibly not the clear.
        fill: Some(Rgb::new(20, 22, 30)),
        shadow: PanelShadow::None_,
        edges: Edge::symmetric(inset, 0),
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: false,
        backdrop: false,
        anchor: None,
        seq: 0,
    });

    let box_left = 1.0 * r.cell[0];
    let box_right = (1.0 + (r.cols as f32 - 2.0)) * r.cell[0];
    let mid_y = ((1.0 + (r.rows as f32 - 3.0) * 0.5) * r.cell[1]) as u32;

    // A couple pixels inside each cell-rect edge, within the inset strip, keeps
    // the clear color because the box is shaved off there.
    let left_strip = r.px(box_left as u32 + 2, mid_y);
    let right_strip = r.px(box_right as u32 - 3, mid_y);
    for (label, got) in [("left", left_strip), ("right", right_strip)] {
        assert!(
            got.iter().zip(SURFACE).all(|(&g, want)| g.abs_diff(want) <= 1),
            "the {label} inset strip at the cell rect edge should keep the clear color, got {got:?}"
        );
    }

    // Past the inset, inside the frame, the fill drew, so the pixel is not clear.
    let interior = r.px(box_left as u32 + inset as u32 + 6, mid_y);
    assert!(
        interior
            .iter()
            .zip(SURFACE)
            .any(|(&g, want)| g.abs_diff(want) > 1),
        "just inside the inset frame the fill should show, got {interior:?}"
    );
}

/// A popover rounds at the two logical pixels an overlay panel rounds at. The
/// pixel whose center sits one and a half pixels in from the box's corner
/// shows the box, where a wider rounding leaves the surface behind it.
#[test]
fn a_popover_rounds_at_the_overlay_panel_radius() {
    const FILL: [u8; 3] = [20, 22, 30];

    let r = render_scene(1.0, |grid, _rows, _cols| {
        grid.set_overlays(vec![Overlay {
            top: 0,
            left: 0,
            width: 4,
            height: 3,
            fill: Rgb::new(FILL[0], FILL[1], FILL[2]),
            border: Rgb::new(200, 100, 50),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [0, 0],
            bold: false,
            chrome: PopoverChrome::CARD,
            center: false,
            content: String::new(),
            spans: Vec::new(),
        }]);
    });

    let corner = r.px(1, 1);
    assert!(
        corner
            .iter()
            .zip(SURFACE)
            .all(|(&channel, surface)| channel + 40 < surface),
        "the pixel inside the corner shows the box, got {corner:?}",
    );
}

/// Popover chrome is stated in logical pixels, so a 2x display draws it at
/// twice the size rather than at half the weight of the panel chrome beside it.
///
/// The border is one logical pixel, so it is two opaque rows deep here. The
/// shadow's blur scales with it, so it still reaches a pixel 24 physical pixels
/// past the box.
#[test]
fn popover_chrome_scales_with_the_display() {
    const FILL: [u8; 3] = [20, 22, 30];
    const BORDER: [u8; 3] = [200, 100, 50];

    let r = render_scene(2.0, |grid, _rows, _cols| {
        grid.set_overlays(vec![Overlay {
            top: 1,
            left: 1,
            width: 4,
            height: 3,
            fill: Rgb::new(FILL[0], FILL[1], FILL[2]),
            border: Rgb::new(BORDER[0], BORDER[1], BORDER[2]),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [0, 0],
            bold: false,
            chrome: PopoverChrome::CARD,
            center: false,
            content: String::new(),
            spans: Vec::new(),
        }]);
    });

    let box_top = r.cell[1];
    let box_bottom = 4.0 * r.cell[1];
    // The column through the box's horizontal middle, clear of both rounded
    // corners.
    let mid_x = (3.0 * r.cell[0]) as u32;

    let band = [0, 1, 2].map(|row| r.px(mid_x, box_top as u32 + row));
    assert_eq!(
        band,
        [BORDER, BORDER, FILL],
        "the border band is two rows deep at twice the density"
    );

    let below = r.px(mid_x, box_bottom as u32 + 24);
    assert!(
        below[0] + 3 < SURFACE[0],
        "the scaled blur still darkens 24 px past the box, got {below:?}"
    );
}

/// A popover draws in the chrome it carries. A 3 px border is three opaque
/// rows deep, and a shadow cast 10 px down at the full peak alpha darkens the
/// ground 5 px under the box past half, where the card's peak of 82 leaves
/// it lighter.
#[test]
fn a_popover_draws_in_its_own_chrome() {
    const FILL: [u8; 3] = [20, 22, 30];
    const BORDER: [u8; 3] = [200, 100, 50];

    let r = render_scene(1.0, |grid, _rows, _cols| {
        grid.set_overlays(vec![Overlay {
            top: 1,
            left: 1,
            width: 4,
            height: 3,
            fill: Rgb::new(FILL[0], FILL[1], FILL[2]),
            border: Rgb::new(BORDER[0], BORDER[1], BORDER[2]),
            content_fg: Rgb::new(255, 255, 255),
            scale: 1,
            offset: [0, 0],
            bold: false,
            chrome: PopoverChrome {
                corner_radius: 2,
                border_width: 3,
                shadow_offset: [0, 10],
                shadow_margin: 36,
                shadow_alpha: 255,
            },
            center: false,
            content: String::new(),
            spans: Vec::new(),
        }]);
    });

    let box_top = r.cell[1] as u32;
    let box_bottom = (4.0 * r.cell[1]) as u32;
    let mid_x = (3.0 * r.cell[0]) as u32;
    let band = [0, 1, 2, 3].map(|row| r.px(mid_x, box_top + row));
    let below = r.px(mid_x, box_bottom + 5);

    assert_eq!(
        band,
        [BORDER, BORDER, BORDER, FILL],
        "the border band is three rows deep"
    );
    assert!(
        below[0] < SURFACE[0] / 2,
        "the full-alpha shadow darkens the ground past half, got {below:?}"
    );
}
