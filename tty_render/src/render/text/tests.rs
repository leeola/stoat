use super::{
    build_underline_row, cell_box, cell_box_rect, cell_glyph_scale, cell_rect_scissor, cursor_cell,
    fill_cell_box, fit_glyph_box, follow_slot, font, glyph_origin, grid_build, inset_scissor,
    is_cell_fill, overlay_content_cells, pack_dim, pack_fg, region_split, row_len, row_slot,
    slots_of_rows, text_run_origin, underline_rows_to_build, visible_lines, GlyphSource, GridBuild,
    OverlayContent, PendingGlyph, RectInstance, RowRotation, RowShaping, TextGlobals, TextInstance,
    TextPass, UnderlineInstance, DIM_FRACTION_BITS, FOLLOW_SHIFT, KIND_COLOR, KIND_MASK,
    OVERLAY_RING_PX, STYLE_DOTTED,
};
use crate::{
    atlas::{AtlasKind, GlyphInfo},
    render::{row_uploads, CellMetrics, Frame, HostRide, PoolOccluders, Scroll, SketchReveal},
    test_support::require_headless_device,
};
use stoatty_protocol::command::{
    SketchBounds, SketchCommand, SketchEasing, SketchPhase, SketchShape, SketchStyle, SketchTiming,
};
use stoatty_term::{
    grid::{
        whole_row, Cell, Grid, Overlay, Rgb, Scale, ScrollRegion, Sketch, TextRun, UnderlineStyle,
    },
    term::Damage,
};
use wgpu::{
    naga::{
        front::wgsl,
        valid::{Capabilities, ValidationFlags, Validator},
    },
    Device, Queue, TextureFormat,
};

/// A partial damage marking each flagged row over its whole width, for a
/// test that has row granularity and no column bounds to express.
fn dirty_rows(flags: &[bool], cols: usize) -> Damage {
    Damage::Partial(
        flags
            .iter()
            .map(|&dirty| if dirty { whole_row(cols) } else { None })
            .collect(),
    )
}

#[test]
fn grid_build_rebuilds_all_when_the_atlas_epoch_moves() {
    assert_eq!(grid_build(true, false, false, 7, 7), GridBuild::Reuse);
    assert_eq!(grid_build(false, false, false, 7, 7), GridBuild::Patch);
    assert_eq!(
        grid_build(true, false, false, 8, 7),
        GridBuild::RebuildAll,
        "an eviction moves UVs, so undamaged rows must still rebuild"
    );
    assert_eq!(grid_build(false, false, false, 8, 7), GridBuild::RebuildAll);
}

/// A moved region rectangle rebuilds whatever the damage says, because the
/// cells it gained or lost have to change buffers and a patch only reaches
/// the damaged ones.
#[test]
fn grid_build_rebuilds_all_when_the_region_rectangle_moves() {
    assert_eq!(grid_build(true, false, true, 7, 7), GridBuild::RebuildAll);
    assert_eq!(grid_build(false, false, true, 7, 7), GridBuild::RebuildAll);
}

/// An emptied slot still holds the instances of the row that scrolled off,
/// and the rotation now sends that slot to a different row, so a frame that
/// rebuilt nothing still has a write to make.
#[test]
fn grid_build_patches_when_a_scroll_emptied_a_slot() {
    assert_eq!(grid_build(true, true, false, 7, 7), GridBuild::Patch);
    assert_eq!(grid_build(false, true, false, 7, 7), GridBuild::Patch);
}

/// What `slot_row` computes in text.wgsl, transcribed. A rotation is only
/// correct if writing through [`row_slot`] and reading through this round
/// trips, and nothing here runs the shader to find out.
fn shader_row(slot: usize, row_offset: u32, rows: usize) -> usize {
    if rows == 0 {
        return slot;
    }

    (slot + rows - row_offset as usize % rows) % rows
}

#[test]
fn a_row_is_read_back_from_the_slot_it_was_written_to() {
    let rows = 5;
    for row_offset in 0..(2 * rows as u32 + 3) {
        let round_tripped: Vec<usize> = (0..rows)
            .map(|row| shader_row(row_slot(row, row_offset, rows), row_offset, rows))
            .collect();

        assert_eq!(
            round_tripped,
            (0..rows).collect::<Vec<_>>(),
            "at offset {row_offset} every row must land where the shader looks for it"
        );
    }
}

#[test]
fn every_slot_holds_exactly_one_row() {
    let rows = 5;
    for row_offset in 0..(2 * rows as u32 + 3) {
        let mut slots: Vec<usize> = (0..rows)
            .map(|row| row_slot(row, row_offset, rows))
            .collect();
        slots.sort_unstable();

        assert_eq!(
            slots,
            (0..rows).collect::<Vec<_>>(),
            "at offset {row_offset} the rows must cover the buffer without two sharing a slot"
        );
    }
}

/// The rows a scroll kept are already where the advanced offset looks for
/// them, which is what leaves only the exposed ones to write.
#[test]
fn a_scroll_leaves_the_rows_it_kept_where_the_shader_will_find_them() {
    let rows = 5;
    let scrolled = 2;
    let before = 3;
    let after = (before + scrolled) % rows as u32;

    for row in 0..rows - scrolled as usize {
        assert_eq!(
            row_slot(row, after, rows),
            row_slot(row + scrolled as usize, before, rows),
            "row {row} after the scroll reads the slot row {} was written to",
            row + scrolled as usize
        );
    }
}

/// The upload plan walks its rows once, advancing through the buffer as it
/// goes, so a descending pair would ask it to count backwards. A run of
/// display rows wraps in slot space, which is exactly where that arises.
#[test]
fn the_slots_of_a_wrapped_run_of_rows_come_back_ascending() {
    let rows = 6;
    let every_row: Vec<usize> = (0..rows).collect();

    for offset in 0..rows as u32 {
        let rotation = RowRotation { offset, rows };

        let mut slots = Vec::new();
        slots_of_rows(rotation, &every_row, &mut slots);
        assert_eq!(
            slots, every_row,
            "at offset {offset} every slot must appear once, ascending"
        );

        // A pair straddling the wrap, which is where ascending rows arrive
        // descending and the plan would be asked to count backwards.
        slots_of_rows(rotation, &[1, 4], &mut slots);
        assert!(
            slots[0] < slots[1],
            "at offset {offset} rows 1 and 4 land at {slots:?}"
        );
    }
}

/// An overlay taller than the screen names content rows past the bottom,
/// and the popover's scroll is what brings them back up. A wrap would fold
/// them over the box, so the screen-anchored draws rotate by nothing and
/// the height they carry is zero.
#[test]
fn an_unrotated_draw_leaves_a_row_past_the_bottom_alone() {
    let unrotated = RowRotation::unrotated();

    for row in [0usize, 5, 41, 4000] {
        assert_eq!(unrotated.slot(row), row, "row {row} is its own slot");
        assert_eq!(
            shader_row(unrotated.slot(row), unrotated.offset, unrotated.rows),
            row,
            "row {row} must come back unwrapped"
        );
    }
}

#[test]
fn text_shader_is_valid_wgsl() {
    let module = wgsl::parse_str(&crate::render::with_occlusion(&crate::render::with_cover(
        include_str!("../../shaders/text.wgsl"),
    )))
    .expect("parse text.wgsl");
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .expect("validate text.wgsl");
}

/// The vertex stage's half of the split position, mirrored so the round trip
/// can be checked without a device.
fn shader_pixel_y(pos_y: f32, row: u32, cell_height: f32) -> f32 {
    pos_y + row as f32 * cell_height
}

/// An instance stores its position measured from the top of its row, and the
/// shader puts the row back. What the two halves must add up to is the
/// absolute position the builder started from.
#[test]
fn a_row_relative_position_recombines_to_the_absolute_one() {
    // The height is fractional on purpose. The builder snaps the cell origin
    // to whole pixels, so a height that does not divide evenly is where a
    // mismatched scale between the two halves would show.
    let metrics = CellMetrics {
        font_size: 10.0,
        width: 6.0,
        height: 12.5,
        scale_factor: 1.0,
    };

    for row in 0..5usize {
        let absolute = glyph_origin(3, row, [1, 9], 10.0, metrics, [0.0; 2]);
        let stored = absolute[1] - row as f32 * metrics.height;

        assert_eq!(
            shader_pixel_y(stored, row as u32, metrics.height),
            absolute[1],
            "row {row} must come back where the builder put it",
        );
    }
}

/// A pooled glyph and a live-grid glyph on the same screen cell must land on
/// the same pixel, or the frame visibly shifts when a glide starts or
/// settles and the renderer swaps between the two paths.
///
/// The builder returns a region-relative position and the shader adds the
/// unrounded region origin back, so what has to match the live grid is the
/// sum. Metrics are fractional here on purpose: at integer metrics both
/// formulas agree whatever they do, which is why the bug never showed on
/// configs whose cells land on whole pixels.
#[test]
fn a_pooled_glyph_lands_on_the_same_pixel_as_the_live_grid() {
    let metrics = CellMetrics {
        font_size: 16.0,
        width: 9.5,
        height: 12.5,
        scale_factor: 1.0,
    };
    let baseline = 10.0;
    let region = [1.0, 1.0];

    // Region cell (1, 1) is screen cell (2, 2). Rounding the region-relative
    // cell instead gives round(9.5) + 9.5 = 19.5 across, half a pixel off the
    // live grid's round(19.0).
    for (col, row) in [(0, 0), (1, 1), (3, 2), (7, 5)] {
        let pooled = glyph_origin(col, row, [1, 2], baseline, metrics, region);
        let live = glyph_origin(col + 1, row + 1, [1, 2], baseline, metrics, [0.0; 2]);

        assert_eq!(
            [
                pooled[0] + region[0] * metrics.width,
                pooled[1] + region[1] * metrics.height
            ],
            live,
            "pooled cell ({col}, {row}) must land where the live grid puts it"
        );
    }

    // A pooled row carrying a scaled text run snaps the same way, so a
    // pooled gutter's runs sit where the screen-anchored ones do.
    for index in 0..4 {
        let pooled = text_run_origin(2.0, 3.0, index, 0.625, [1, 2], baseline, metrics, region);
        let live = text_run_origin(
            2.0 + region[0],
            3.0 + region[1],
            index,
            0.625,
            [1, 2],
            baseline,
            metrics,
            [0.0; 2],
        );

        assert_eq!(
            [
                pooled[0] + region[0] * metrics.width,
                pooled[1] + region[1] * metrics.height
            ],
            live,
            "pooled run glyph {index} must land where a screen-anchored run puts it"
        );
    }

    // A procedural separator spans the same whole pixels as the cell
    // backgrounds beside it, which the background pass snaps absolutely.
    let (pooled_pos, pooled_dim) = cell_box_rect(2, 3, 1.0, metrics, region);
    let (live_pos, live_dim) = cell_box_rect(3, 4, 1.0, metrics, [0.0; 2]);
    assert_eq!(
        [
            pooled_pos[0] + region[0] * metrics.width,
            pooled_pos[1] + region[1] * metrics.height
        ],
        live_pos,
        "a pooled cell box starts on the live grid's pixel"
    );
    assert_eq!(pooled_dim, live_dim, "and spans the same whole pixels");
}

#[test]
fn glyph_origin_offsets_from_cell_pen_and_baseline() {
    let metrics = CellMetrics::from_font_size(30, 1.0);
    let baseline = 14.0;

    let origin = glyph_origin(3, 2, [1, 10], baseline, metrics, [0.0; 2]);
    assert_eq!(
        origin,
        [
            3.0 * metrics.width + 1.0,
            2.0 * metrics.height + baseline - 10.0
        ]
    );

    let origin = glyph_origin(0, 0, [-2, -3], baseline, metrics, [0.0; 2]);
    assert_eq!(origin, [-2.0, baseline + 3.0]);
}

/// A pen lands on a whole physical pixel, so a glyph's bitmap is not
/// resampled across two of them.
///
/// The cell rectangle is whole pixels, so a region parked on a cell
/// boundary puts every cell of it on one already. A region parked part way
/// into a cell does not, and the shader adds `origin * size` back
/// unrounded, so the pen snaps the sum rather than its own offset.
#[test]
fn glyph_origin_snaps_the_cell_origin_to_whole_pixels() {
    // font_size 11 -> a 7 by 13 cell, so half a cell is half a pixel.
    let metrics = CellMetrics::from_font_size(11, 1.0);

    let origin = glyph_origin(3, 2, [1, 2], 10.0, metrics, [0.0; 2]);
    assert_eq!(origin, [22.0, 34.0]);

    let parked = [0.5, 0.5];
    let pen = glyph_origin(3, 2, [1, 2], 10.0, metrics, parked);
    let absolute = [
        pen[0] + parked[0] * metrics.width,
        pen[1] + parked[1] * metrics.height,
    ];
    assert_eq!(absolute, [26.0, 41.0], "the shader's sum lands on a pixel");
}

#[test]
fn is_cell_fill_covers_box_block_and_powerline_ranges() {
    assert!(is_cell_fill('\u{2500}'), "box-drawing start");
    assert!(is_cell_fill('\u{257F}'), "box-drawing end");
    assert!(is_cell_fill('\u{2580}'), "block start");
    assert!(is_cell_fill('\u{259F}'), "block end");
    assert!(is_cell_fill('\u{E0B0}'), "powerline separator");
    assert!(is_cell_fill('\u{E0D4}'), "powerline end");

    assert!(!is_cell_fill('\u{24FF}'), "just below box-drawing");
    assert!(!is_cell_fill('\u{25A0}'), "just above block");
    assert!(!is_cell_fill('\u{E0AF}'), "just below powerline");
    assert!(!is_cell_fill('\u{E0D5}'), "just above powerline");
    assert!(!is_cell_fill('A'), "letter");
    assert!(!is_cell_fill('='), "ligature char");
}

#[test]
fn fill_cell_box_scales_a_full_em_glyph_onto_the_cell() {
    // font_size 30 -> width 18, height 36, em 30, so scale_y = 1.2.
    let metrics = CellMetrics::from_font_size(30, 1.0);
    let baseline = 30.0;
    let approx =
        |a: [f32; 2], b: [f32; 2]| (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3;

    // A full-em glyph spanning [5, 35] at row 0 scales to fill the 36px cell.
    let (pos, dim) = fill_cell_box([2.0, 5.0], [8.0, 30.0], 0, 1.0, baseline, metrics, [0.0; 2]);
    assert!(
        approx(pos, [2.0, 0.0]),
        "x unchanged, top at cell top: {pos:?}"
    );
    assert!(
        approx(dim, [8.0, 36.0]),
        "width kept, height fills cell: {dim:?}"
    );

    // A scaled 2x glyph fills its two-cell block.
    let (pos, dim) = fill_cell_box(
        [0.0, 10.0],
        [8.0, 60.0],
        0,
        2.0,
        baseline,
        metrics,
        [0.0; 2],
    );
    assert!(approx(pos, [0.0, 0.0]), "{pos:?}");
    assert!(approx(dim, [8.0, 72.0]), "fills two cells: {dim:?}");
}

/// The instance is uploaded once per visible glyph, so a frame that
/// rewrites the whole grid pays its size thousands of times over. Pinning
/// the size states that budget rather than leaving a field widened back
/// out of it unnoticed.
#[test]
fn a_glyph_instance_packs_to_thirty_two_bytes() {
    assert_eq!(size_of::<TextInstance>(), 32);
}

/// A quad size travels in eighths of a pixel, which is finer than a
/// nearest-sampled quad resolves but coarser than the f32 it replaced.
/// What it must not do is wrap. An absurd size saturates at the field's
/// ceiling and still draws something.
#[test]
fn a_quad_size_round_trips_through_its_fixed_point() {
    let pixels = |dim: [u16; 2]| {
        let unit = (1 << DIM_FRACTION_BITS) as f32;
        [f32::from(dim[0]) / unit, f32::from(dim[1]) / unit]
    };

    assert_eq!(
        pixels(pack_dim([12.0, 36.0])),
        [12.0, 36.0],
        "whole pixels survive exactly"
    );
    assert_eq!(
        pixels(pack_dim([8.0, 36.25])),
        [8.0, 36.25],
        "so does an eighth, which is what a cell-fill quad lands on"
    );

    let [_, height] = pixels(pack_dim([8.0, 36.1]));
    assert!(
        (height - 36.1).abs() <= 0.0625,
        "a finer fraction lands within half a step: {height}"
    );

    assert_eq!(
        pack_dim([1.0e9, -4.0]),
        [u16::MAX, 0],
        "a size past either end saturates"
    );
}

/// One vertex stage reads the color word two ways. `unpack4x8unorm` takes
/// the channels off the low bytes, and a shift takes the atlas selector off
/// the top one. Both readings have to find what was packed.
#[test]
fn the_color_word_carries_the_channels_and_the_atlas_selector() {
    let packed = pack_fg(Rgb::new(10, 20, 30), AtlasKind::Color);

    assert_eq!(
        packed.to_le_bytes(),
        [10, 20, 30, KIND_COLOR as u8],
        "channels in the order the unpack reads them, selector above"
    );
    assert_eq!(
        pack_fg(Rgb::new(255, 255, 255), AtlasKind::Mask) >> 24,
        KIND_MASK,
        "a fully white glyph leaves the selector byte alone"
    );
}

#[test]
fn text_run_origin_matches_glyph_origin_at_unit_scale() {
    let metrics = CellMetrics::from_font_size(30, 1.0);
    let baseline = 14.0;

    // The first glyph of a unit-scale run lands exactly on the cell grid, so
    // a run at scale 1 is indistinguishable from cell text.
    assert_eq!(
        text_run_origin(3.0, 2.0, 0, 1.0, [1, 10], baseline, metrics, [0.0; 2]),
        glyph_origin(3, 2, [1, 10], baseline, metrics, [0.0; 2])
    );
}

#[test]
fn text_run_origin_scales_advance_and_centers_in_row() {
    let metrics = CellMetrics::from_font_size(30, 1.0);
    let baseline = 14.0;

    let origin = text_run_origin(0.0, 0.0, 2, 0.5, [0, 0], baseline, metrics, [0.0; 2]);

    // Two half-scale glyphs advance one cell, and the shorter line is
    // centered within the full row's height above its scaled baseline.
    assert_eq!(
        origin,
        [
            metrics.width,
            (metrics.height - metrics.height * 0.5) / 2.0 + baseline * 0.5
        ]
    );
}

/// The atlas sampler is nearest with no subpixel variants, so a quad off a
/// whole pixel samples the bitmap off-center and the glyph comes out a
/// different shape than the same glyph beside it.
#[test]
fn a_fractional_run_snaps_every_glyph_to_a_whole_pixel() {
    let metrics = CellMetrics::from_font_size(30, 1.0);
    let baseline = 14.0;

    // Three quarters of an 18px cell advances 13.5px, so every other pen
    // lands mid-pixel before the rounding.
    let pens: Vec<f32> = (0..5)
        .map(|index| text_run_origin(0.0, 0.0, index, 0.75, [0, 0], baseline, metrics, [0.0; 2])[0])
        .collect();

    assert!(
        pens.iter().all(|pen| pen.fract() == 0.0),
        "every glyph starts on a whole pixel: {pens:?}"
    );

    let gaps: Vec<f32> = pens.windows(2).map(|pair| pair[1] - pair[0]).collect();
    let (min, max) = gaps.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &gap| {
        (lo.min(gap), hi.max(gap))
    });
    assert!(min >= 0.0, "the run never steps backwards: {gaps:?}");
    assert!(
        max - min <= 1.0,
        "and no two gaps differ by more than a pixel: {gaps:?}"
    );
}

#[test]
fn underline_instances_cover_styled_cells_only() {
    let mut grid = Grid::new(1, 3);
    grid.get_mut(0, 1).underline = UnderlineStyle::Dotted;
    grid.get_mut(0, 1).underline_color = Rgb::new(255, 0, 0);

    let metrics = CellMetrics::from_font_size(30, 1.0);
    let instances = build_underline_row(&grid, 0, metrics, RowRotation::unrotated());

    assert_eq!(instances.len(), 1);
    assert_eq!(instances[0].cell_pos, [metrics.width, 0.0]);
    assert_eq!(instances[0].color, [1.0, 0.0, 0.0]);
    assert_eq!(instances[0].style, STYLE_DOTTED);
}

#[test]
fn cell_glyph_scale_skips_blank_and_covered() {
    let glyph = |scale| Cell {
        ch: 'a',
        scale,
        ..Cell::default()
    };

    assert_eq!(cell_glyph_scale(&glyph(Scale::Single)), Some(1));
    assert_eq!(cell_glyph_scale(&glyph(Scale::Origin(2))), Some(2));
    assert_eq!(
        cell_glyph_scale(&glyph(Scale::Covered)),
        None,
        "covered cell draws no glyph"
    );
    assert_eq!(cell_glyph_scale(&Cell::default()), None, "blank cell");
}

#[test]
fn cursor_cell_rounds_position_to_row_col() {
    assert_eq!(cursor_cell(None), None, "a hidden cursor breaks no run");
    assert_eq!(
        cursor_cell(Some([3.0, 5.0])),
        Some((5, 3)),
        "the [col, row] position maps to a (row, col) cell"
    );
    assert_eq!(
        cursor_cell(Some([3.4, 5.6])),
        Some((6, 3)),
        "a position mid-ease rounds to the nearest cell"
    );
}

#[test]
fn overlay_content_cells_clip_to_box_width() {
    let overlay = Overlay {
        top: 2,
        left: 5,
        width: 3,
        height: 1,
        fill: Rgb::new(0, 0, 0),
        border: Rgb::new(0, 0, 0),
        content_fg: Rgb::new(255, 255, 255),
        scale: 1,
        offset: [0, 0],
        bold: false,
        content: "Hello".to_owned(),
    };

    // Inset one cell to (6, 3), and the 3-wide box holds one char after the
    // inset trims a cell from each side.
    assert_eq!(content_cells(&overlay, 1).0, [(6, 3, 'H')]);
}

#[test]
fn overlay_content_cells_space_and_clip_by_scale() {
    let overlay = Overlay {
        top: 2,
        left: 4,
        width: 6,
        height: 4,
        fill: Rgb::new(0, 0, 0),
        border: Rgb::new(0, 0, 0),
        content_fg: Rgb::new(255, 255, 255),
        scale: 2,
        offset: [0, 0],
        bold: false,
        content: "abcd\nef".to_owned(),
    };

    // At scale 2 each glyph owns a 2x2 block, so chars advance two columns
    // and lines advance two rows. Inset one cell to (5, 3), the 6-cell box
    // holds two chars once the inset trims a cell from each side.
    assert_eq!(
        content_cells(&overlay, 2).0,
        [(5, 3, 'a'), (7, 3, 'b'), (5, 5, 'e'), (7, 5, 'f')]
    );
}

#[test]
fn overlay_content_cells_emit_all_lines_clipped_to_width() {
    let overlay = Overlay {
        top: 2,
        left: 5,
        width: 3,
        height: 2,
        fill: Rgb::new(0, 0, 0),
        border: Rgb::new(0, 0, 0),
        content_fg: Rgb::new(255, 255, 255),
        scale: 1,
        offset: [0, 0],
        bold: false,
        content: "abcd\nef\nXY".to_owned(),
    };

    // Every line is emitted and width-clipped. Inset one cell to (6, 3), the
    // 3-wide box holds one char per line, and all three lines still emit
    // since the scissor clips vertical overflow rather than the box height.
    let (cells, starts) = content_cells(&overlay, 1);
    assert_eq!(cells, [(6, 3, 'a'), (6, 4, 'e'), (6, 5, 'X')]);

    // One start per line plus the trailing end, so a line range slices the
    // cells directly.
    assert_eq!(starts, [0, 1, 2, 3], "each line's start, then the end");
}

/// The buffers are refilled rather than appended to, so a caller holding them
/// across overlays sees only the overlay it asked about.
#[test]
fn overlay_content_cells_refill_the_buffers_they_are_given() {
    let overlay = |content: &str| Overlay {
        top: 2,
        left: 5,
        width: 4,
        height: 2,
        fill: Rgb::new(0, 0, 0),
        border: Rgb::new(0, 0, 0),
        content_fg: Rgb::new(255, 255, 255),
        scale: 1,
        offset: [0, 0],
        bold: false,
        content: content.to_owned(),
    };

    let (mut cells, mut starts) = (Vec::new(), Vec::new());
    overlay_content_cells(&overlay("ab\ncd\nef"), 1, &mut cells, &mut starts);
    overlay_content_cells(&overlay("xy"), 1, &mut cells, &mut starts);

    assert_eq!(cells, [(6, 3, 'x'), (7, 3, 'y')], "only the second overlay");
    assert_eq!(starts, [0, 2], "one line, so one start and the end");
}

#[test]
fn cell_rect_scissor_clamps_to_surface() {
    let metrics = CellMetrics::from_font_size(30, 1.0);
    let resolution = [metrics.width * 10.0, metrics.height * 5.0];

    assert_eq!(
        cell_rect_scissor(1, 2, 3, 2, [0.0, 0.0], resolution, metrics),
        Some([
            (2.0 * metrics.width) as u32,
            metrics.height as u32,
            (3.0 * metrics.width) as u32,
            (2.0 * metrics.height) as u32,
        ]),
        "a rectangle inside the surface maps cells to pixels directly"
    );

    assert_eq!(
        cell_rect_scissor(1, 2, 3, 2, [4.0, -metrics.height], resolution, metrics),
        Some([
            (2.0 * metrics.width) as u32 + 4,
            0,
            (3.0 * metrics.width) as u32,
            (2.0 * metrics.height) as u32,
        ]),
        "the offset shifts the rect and clamps a negative origin to zero"
    );

    let [x, y, w, h] = cell_rect_scissor(4, 8, 6, 4, [0.0, 0.0], resolution, metrics).unwrap();
    assert_eq!(x + w, resolution[0] as u32, "width clamps to the surface");
    assert_eq!(y + h, resolution[1] as u32, "height clamps to the surface");

    assert_eq!(
        cell_rect_scissor(5, 0, 2, 2, [0.0, 0.0], resolution, metrics),
        None,
        "an anchor at the bottom edge has no area"
    );
}

#[test]
fn shader_is_valid_wgsl() {
    let module = wgsl::parse_str(&crate::render::with_occlusion(&crate::render::with_cover(
        include_str!("../../shaders/text.wgsl"),
    )))
    .expect("parse text.wgsl");
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .expect("validate text.wgsl");
}

#[test]
fn bg_shader_is_valid_wgsl() {
    let module = wgsl::parse_str(&crate::render::with_occlusion(&crate::render::with_cover(
        include_str!("../../shaders/bg.wgsl"),
    )))
    .expect("parse bg.wgsl");
    Validator::new(ValidationFlags::all(), Capabilities::all())
        .validate(&module)
        .expect("validate bg.wgsl");
}

/// [`overlay_content_cells`] into fresh buffers, as (cells, line starts).
fn content_cells(overlay: &Overlay, scale: usize) -> (Vec<(usize, usize, char)>, Vec<u32>) {
    let (mut cells, mut starts) = (Vec::new(), Vec::new());
    overlay_content_cells(overlay, scale, &mut cells, &mut starts);
    (cells, starts)
}

/// A text pass on the headless device.
fn headless_text_pass() -> (Device, Queue, TextPass) {
    headless_text_pass_font(16)
}

/// A text pass at `font_size` on the headless device.
///
/// A large size makes a small glyph burst overflow the initial atlas and
/// force a grow.
fn headless_text_pass_font(font_size: u32) -> (Device, Queue, TextPass) {
    let (device, queue) = require_headless_device();
    let pass = TextPass::new(
        &device,
        TextureFormat::Rgba8Unorm,
        CellMetrics::from_font_size(font_size, 1.0),
        font::build_font_system(),
        &["JetBrains Mono".to_owned()],
        true,
    );
    (device, queue, pass)
}

/// A text pass over the bundled faces alone, at a fixed locale so a test
/// never reads the environment's.
fn bundled_text_pass() -> (Device, Queue, TextPass) {
    let (device, queue) = require_headless_device();
    let pass = TextPass::new(
        &device,
        TextureFormat::Rgba8Unorm,
        CellMetrics::from_font_size(16, 1.0),
        font::bundled_font_system_with_locale("en-US".into()),
        &["JetBrains Mono".to_owned()],
        true,
    );
    (device, queue, pass)
}

fn fill_row(grid: &mut Grid, row: usize, text: &str) {
    for (col, ch) in text.chars().enumerate() {
        grid.get_mut(row, col).ch = ch;
    }
}

/// Rasterize `rows` as a grid wide enough to hold the longest of them, so a
/// caller can then read what the pass's run cache was filled with.
fn rasterize_rows(pass: &mut TextPass, device: &Device, queue: &Queue, rows: &[&str]) {
    let cols = rows
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0);
    let mut grid = Grid::new(rows.len().max(1), cols.max(1));
    for (row, text) in rows.iter().enumerate() {
        fill_row(&mut grid, row, text);
    }

    let covers = |_: char| true;
    let family = Some("JetBrains Mono".to_owned());
    let shaping = RowShaping {
        primary: font::shape_family(family.as_deref()),
        covers: &covers,
        reshapes: &|_| true,
        cursor_cell: None,
    };
    let mut pending = Vec::new();
    for row in 0..rows.len() {
        pass.rasterize_row(device, queue, &grid, row, &shaping, &mut pending);
    }
}

/// Rasterize `rows` with the pass's own substitution coverage, so a run the
/// face reshapes takes the shaper and one it leaves alone does not.
///
/// [`rasterize_rows`] forces every run onto the shaper, which is what the
/// run-splitting tests above are about. This is for reading which runs
/// reach it at all.
fn rasterize_rows_by_coverage(pass: &mut TextPass, device: &Device, queue: &Queue, rows: &[&str]) {
    let cols = rows
        .iter()
        .map(|row| row.chars().count())
        .max()
        .unwrap_or(0);
    let mut grid = Grid::new(rows.len().max(1), cols.max(1));
    for (row, text) in rows.iter().enumerate() {
        fill_row(&mut grid, row, text);
    }

    let font = pass.primary_font.clone();
    let charmap = font.as_ref().map(|font| font.as_swash().charmap());
    let substitutable = std::mem::take(&mut pass.substitutable);
    let covers = |_: char| true;
    let reshapes = |run: &[(usize, char)]| {
        let Some(map) = charmap.as_ref() else {
            return false;
        };
        let glyphs: Vec<u16> = run.iter().map(|&(_, ch)| map.map(ch)).collect();
        substitutable.reshapes(&glyphs)
    };
    let family = Some("JetBrains Mono".to_owned());
    let shaping = RowShaping {
        primary: font::shape_family(family.as_deref()),
        covers: &covers,
        reshapes: &reshapes,
        cursor_cell: None,
    };
    let mut pending = Vec::new();
    for row in 0..rows.len() {
        pass.rasterize_row(device, queue, &grid, row, &shaping, &mut pending);
    }
    pass.substitutable = substitutable;
}

/// A run holding nothing the face reshapes is cached like one that does.
/// Its entry is built from the per-character glyph cache rather than from
/// the shaper, so a repaint of either costs one hash of the run text.
///
/// Novel prose is almost all the first kind, and shaping a word costs some
/// twenty-five times what looking its characters up does. Leaving those runs
/// uncached made a styled screen pay the per-character path every frame.
#[test]
fn only_a_run_the_face_reshapes_reaches_the_shaper() {
    let (device, queue, mut pass) = headless_text_pass();

    rasterize_rows_by_coverage(&mut pass, &device, &queue, &["4f2a b91c 0e7d"]);
    assert_eq!(
        pass.run_shape_cache.cached_texts(),
        ["0e7d", "4f2a", "b91c"],
        "every run is cached, so a repaint asks the cache and nothing else",
    );
    assert_eq!(
        pass.run_shape_cache.shaped_chars(),
        0,
        "hex tokens hold nothing the face reshapes, so none reached the shaper",
    );

    rasterize_rows_by_coverage(&mut pass, &device, &queue, &["a => b"]);
    assert_eq!(
        pass.run_shape_cache.shaped_chars(),
        "=>".len(),
        "and the one run that ligates is the only one the shaper laid out",
    );

    rasterize_rows_by_coverage(&mut pass, &device, &queue, &["fn handle(x) { list[i] }"]);
    assert_eq!(
        pass.run_shape_cache.shaped_chars(),
        "=>".len(),
        "ordinary code holds no rule the face could fire, so it adds nothing",
    );
}

/// A run is one word, so a row of several words fills the cache with each of
/// them rather than with the row.
///
/// This is the whole point of the split: keyed on the row, one novel token
/// would miss the line, and a scrollback line is novel as a whole far more
/// often than its words are.
#[test]
fn a_row_caches_one_run_per_word() {
    let (device, queue, mut pass) = headless_text_pass();
    rasterize_rows(&mut pass, &device, &queue, &["a => b => a"]);

    assert_eq!(
        pass.run_shape_cache.cached_texts(),
        ["=>", "a", "b"],
        "the row cached its distinct words, and the repeats reused them"
    );
}

/// A ligature still forms inside a word, so the split cost none. The `=>`
/// glyphs a mixed row shapes are the ones a row holding only `=>` shapes.
#[test]
fn a_word_shapes_its_ligature_the_same_beside_other_words() {
    let (device, queue, mut pass) = headless_text_pass();
    rasterize_rows(&mut pass, &device, &queue, &["a => b => a"]);
    let beside_words = pass
        .run_shape_cache
        .cached_glyphs("=>")
        .expect("the arrow was cached")
        .to_vec();

    let (device, queue, mut alone) = headless_text_pass();
    rasterize_rows(&mut alone, &device, &queue, &["=>"]);

    assert_eq!(
        beside_words,
        alone
            .run_shape_cache
            .cached_glyphs("=>")
            .expect("the lone arrow was cached"),
        "the arrow shapes to the same glyphs with or without words beside it"
    );
}

/// The work a screen of prose costs, counted rather than timed: every
/// distinct run is shaped exactly once, so the characters the cache holds
/// are the characters that were shaped.
///
/// Scrollback lines repeat each other's words and not each other, which is
/// why keying on words is worth doing at all. Twenty lines differing only in
/// the number that leads them shape their shared words once.
#[test]
fn a_screen_of_prose_shapes_only_its_distinct_words() {
    let (device, queue, mut pass) = headless_text_pass();
    let rows: Vec<String> = (0..20)
        .map(|row| format!("line {row:04} fn handle_event(ev) -> Result<()>"))
        .collect();
    let borrowed: Vec<&str> = rows.iter().map(String::as_str).collect();
    rasterize_rows(&mut pass, &device, &queue, &borrowed);

    let shaped = pass.run_shape_cache.shaped_chars();
    let on_screen: usize = rows.iter().map(|row| row.chars().count()).sum();
    assert!(
        shaped * 4 < on_screen,
        "{shaped} characters shaped for {on_screen} on screen, which is no better than \
             shaping every row whole"
    );
}

/// A glyph's stored placement builds the same instance a fresh lookup would,
/// and a stale epoch is what makes the build go and look again.
///
/// The first half is the whole premise. Rasterizing already read where the glyph
/// landed, so building from that must agree with asking the atlas a second time.
///
/// The second half shows the epoch guard is load-bearing rather than decorative,
/// by poisoning a placement and watching each epoch decide whether it is used.
#[test]
fn a_stored_placement_builds_what_a_fresh_lookup_would() {
    let (device, queue, mut pass) = headless_text_pass();
    let mut grid = Grid::new(2, 12);
    fill_row(&mut grid, 0, "glyphs");

    let mut pending = Vec::new();
    let covers = |_: char| true;
    let family = Some("JetBrains Mono".to_owned());
    let shaping = RowShaping {
        primary: font::shape_family(family.as_deref()),
        covers: &covers,
        reshapes: &|_| true,
        cursor_cell: None,
    };
    pass.rasterize_row(&device, &queue, &grid, 0, &shaping, &mut pending);
    assert!(!pending.is_empty(), "the row rasterized some glyphs");

    let mut from_stored = Vec::new();
    pass.build_text_instances_into(
        &device,
        &queue,
        &pending,
        RowRotation::unrotated(),
        [0.0; 2],
        &mut from_stored,
    );

    // A mismatched epoch sends every glyph back to the atlas, so this build
    // ignores what is stored and resolves each one.
    let stale: Vec<PendingGlyph> = pending
        .iter()
        .map(|glyph| PendingGlyph {
            resolved_epoch: glyph.resolved_epoch.wrapping_add(1),
            ..*glyph
        })
        .collect();
    let mut from_lookup = Vec::new();
    pass.build_text_instances_into(
        &device,
        &queue,
        &stale,
        RowRotation::unrotated(),
        [0.0; 2],
        &mut from_lookup,
    );

    let bytes =
        |instances: &[TextInstance]| bytemuck::cast_slice::<TextInstance, u8>(instances).to_vec();
    assert_eq!(
        bytes(&from_stored),
        bytes(&from_lookup),
        "the placement read at rasterize time matches what a second lookup gives"
    );

    // Poisoning a placement changes the instance only while its epoch still
    // matches, which is what the guard decides.
    let poison = GlyphInfo {
        kind: AtlasKind::Mask,
        uv: [0.25, 0.5, 0.75, 1.0],
        size: [3, 4],
        placement: [5, 6],
    };
    let poisoned: Vec<PendingGlyph> = pending
        .iter()
        .map(|glyph| PendingGlyph {
            info: poison,
            ..*glyph
        })
        .collect();
    let mut trusted = Vec::new();
    pass.build_text_instances_into(
        &device,
        &queue,
        &poisoned,
        RowRotation::unrotated(),
        [0.0; 2],
        &mut trusted,
    );

    let poisoned_stale: Vec<PendingGlyph> = poisoned
        .iter()
        .map(|glyph| PendingGlyph {
            resolved_epoch: glyph.resolved_epoch.wrapping_add(1),
            ..*glyph
        })
        .collect();
    let mut refused = Vec::new();
    pass.build_text_instances_into(
        &device,
        &queue,
        &poisoned_stale,
        RowRotation::unrotated(),
        [0.0; 2],
        &mut refused,
    );

    assert_ne!(
        bytes(&trusted),
        bytes(&from_stored),
        "a matching epoch means the stored placement is the one used"
    );
    assert_eq!(
        bytes(&refused),
        bytes(&from_stored),
        "a moved epoch means it is looked up again instead"
    );
}

/// The instance hands the vertex stage an atlas origin and a size, and the
/// stage adds them back into the rect the atlas cut. This is that agreement,
/// stated on the CPU side where a change to either half is visible.
///
/// It also pins why the size is carried at all. A procedural glyph draws at
/// the pixel-snapped cell rect, not at its bitmap's extent, so a quad
/// derived from the atlas rect takes the wrong shape for it.
#[test]
fn a_built_glyph_decodes_to_its_atlas_rect_and_color() {
    let (device, queue, mut pass) = headless_text_pass();

    let info = GlyphInfo {
        kind: AtlasKind::Color,
        uv: [64.0, 32.0, 80.0, 40.0],
        size: [16, 8],
        placement: [0, 0],
    };
    let pending = [PendingGlyph {
        row: 0,
        col: 0,
        source: GlyphSource::Procedural {
            cp: 0,
            width: 16,
            height: 8,
        },
        fg: Rgb::new(10, 20, 30),
        scale: 1.0,
        cell_fill: false,
        wide: false,
        info,
        resolved_epoch: pass.atlas.content_epoch(),
    }];

    let mut built = Vec::new();
    pass.build_text_instances_into(
        &device,
        &queue,
        &pending,
        RowRotation::unrotated(),
        [0.0; 2],
        &mut built,
    );
    let [instance] = built[..] else {
        panic!("one pending glyph builds one instance, got {}", built.len());
    };

    let origin = instance.texel_origin.map(f32::from);
    let far = [
        origin[0] + f32::from(instance.texel_size[0]),
        origin[1] + f32::from(instance.texel_size[1]),
    ];
    assert_eq!(
        [origin[0], origin[1], far[0], far[1]],
        info.uv,
        "origin plus size rebuilds the rect the atlas cut"
    );
    assert_eq!(
        instance.fg.to_le_bytes(),
        [10, 20, 30, KIND_COLOR as u8],
        "the glyph's color and the atlas it samples share one word"
    );

    let (_, cell) = cell_box_rect(0, 0, 1.0, CellMetrics::from_font_size(16, 1.0), [0.0; 2]);
    assert_eq!(
        instance.dim,
        pack_dim(cell),
        "a procedural glyph's quad is the snapped cell rect"
    );
    assert_ne!(
        instance.dim,
        pack_dim([16.0, 8.0]),
        "and not its bitmap's extent, which is why the two travel apart"
    );
}

/// One mark, at the slot the run's declared id resolves to.
fn followed(id: u32, follow: u32) -> Grid {
    let mut grid = Grid::new(2, 12);
    grid.set_sketches(vec![test_sketch(7), test_sketch(id), test_sketch(9)]);
    grid.set_text_runs(vec![TextRun {
        col: 0,
        row: 0,
        scale: 256,
        color: Rgb::new(1, 2, 3),
        bg: Some(Rgb::new(4, 5, 6)),
        follow,
        anchor: None,
        text: "42".into(),
        seq: 42,
    }]);
    grid
}

fn test_sketch(id: u32) -> Sketch {
    Sketch {
        command: SketchCommand {
            id,
            style: SketchStyle {
                color: [255, 0, 0],
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
            shape: SketchShape::Ellipse {
                bounds: SketchBounds {
                    x: 0,
                    y: 0,
                    w: 32,
                    h: 32,
                },
                fill: None,
            },
            anchor: None,
        },
        seq: id,
    }
}

/// The shader indexes the alpha array by this, so the slot must be the
/// mark's position in the grid's own list, biased so zero can mean "none".
#[test]
fn a_followed_run_resolves_to_its_mark_s_slot() {
    assert_eq!(follow_slot(&followed(5, 5), 5), 2, "the second of three");
    assert_eq!(follow_slot(&followed(5, 7), 7), 1, "and the first");
    assert_eq!(follow_slot(&followed(5, 9), 9), 3, "and the last");
}

/// A run that follows nothing, and one naming an id no mark declared, both
/// draw at full alpha. A label whose subject never appeared should be
/// readable rather than invisible.
#[test]
fn a_run_following_nothing_or_a_stranger_takes_no_slot() {
    assert_eq!(follow_slot(&followed(5, 0), 0), 0, "following nothing");
    assert_eq!(follow_slot(&followed(5, 99), 99), 0, "an undeclared id");
}

/// The slot rides the high half of the row word, so a glyph carries both
/// without widening the instance. Reading either back wrong puts the run on
/// the wrong row or fades it with the wrong mark.
#[test]
fn a_run_glyph_packs_its_follow_slot_above_its_row() {
    let (device, queue, mut pass) = headless_text_pass();
    let grid = followed(5, 5);

    let mut instances = Vec::new();
    pass.build_text_run_instances_into(&device, &queue, &grid, [0.0; 2], 3, &mut instances);

    assert!(!instances.is_empty(), "the run builds glyphs");
    for instance in &instances {
        assert_eq!(instance.row & 0xFFFF, 3, "the row slot it was given");
        assert_eq!(instance.row >> FOLLOW_SHIFT, 2, "and its mark's slot");
    }
}

/// The rect backs the run's glyphs, so it has to fade with them or an
/// opaque box appears before the label it backs.
#[test]
fn a_run_rect_follows_the_mark_its_glyphs_do() {
    let (_device, _queue, pass) = headless_text_pass();

    assert_eq!(pass.build_run_rects(&followed(5, 5))[0].follow, 2);
    assert_eq!(pass.build_run_rects(&followed(5, 0))[0].follow, 0);
}

/// A run's slot is an index into the mark list, so a mark declared or
/// dropped renumbers every run after it. Reusing instances across that
/// change fades a label with the wrong mark.
#[test]
fn a_changed_mark_list_rebuilds_the_run_slots() {
    let (_device, _queue, pass) = headless_text_pass();
    let mut grid = followed(5, 5);

    assert_eq!(pass.build_run_rects(&grid)[0].follow, 2);

    // The mark it follows is now first, so its slot moved.
    grid.set_sketches(vec![test_sketch(5), test_sketch(9)]);
    assert_eq!(
        pass.build_run_rects(&grid)[0].follow,
        1,
        "the same run resolves to the slot the new list puts its mark at",
    );
}

#[test]
fn run_rect_carries_its_run_occlusion_seq() {
    let (_device, _queue, pass) = headless_text_pass();
    let mut grid = Grid::new(2, 12);
    grid.set_text_runs(vec![TextRun {
        col: 0,
        row: 0,
        scale: 256,
        color: Rgb::new(1, 2, 3),
        bg: Some(Rgb::new(4, 5, 6)),
        follow: 0,
        anchor: None,
        text: "42".into(),
        seq: 42,
    }]);

    let rects = pass.build_run_rects(&grid);

    assert_eq!(rects.len(), 1);
    assert_eq!(
        rects[0].seq, 42,
        "the run rect carries its run's occlusion seq"
    );
}

#[test]
fn run_without_bg_builds_no_rect() {
    let (_device, _queue, pass) = headless_text_pass();
    let mut grid = Grid::new(2, 12);
    grid.set_text_runs(vec![TextRun {
        col: 0,
        row: 0,
        scale: 256,
        color: Rgb::new(1, 2, 3),
        bg: None,
        follow: 0,
        anchor: None,
        text: "42".into(),
        seq: 42,
    }]);

    assert!(
        pass.build_run_rects(&grid).is_empty(),
        "a run with no background paints no backing rect"
    );
}

#[test]
fn build_run_rects_into_clears_prior_scratch() {
    let (_device, _queue, pass) = headless_text_pass();
    let mut grid = Grid::new(2, 12);
    grid.set_text_runs(vec![TextRun {
        col: 0,
        row: 0,
        scale: 256,
        color: Rgb::new(1, 2, 3),
        bg: Some(Rgb::new(4, 5, 6)),
        follow: 0,
        anchor: None,
        text: "42".into(),
        seq: 7,
    }]);

    let fresh = pass.build_run_rects(&grid);
    assert_eq!(fresh.len(), 1, "the one backed run builds one rect");

    // A scratch buffer carrying stale rects is cleared before the rebuild, so
    // reuse yields exactly the fresh result rather than accumulating.
    let mut scratch = pass.build_run_rects(&grid);
    scratch.extend(pass.build_run_rects(&grid));
    pass.build_run_rects_into(&grid, &mut scratch);

    assert_eq!(
        bytemuck::cast_slice::<RectInstance, u8>(&scratch),
        bytemuck::cast_slice::<RectInstance, u8>(&fresh),
        "reuse clears the stale rects and rebuilds only the run's rect"
    );
}

/// A backing rect covers whole pixels, so it lands on the pixel its first
/// glyph starts on and two runs at one column back the same pixels.
///
/// A run declares its column in sixteenths of a cell and its scale in
/// 256ths, so the raw product is fractional on both axes whenever either
/// one is off a whole cell.
#[test]
fn a_run_rect_covers_whole_pixels() {
    let (_device, _queue, pass) = headless_text_pass();
    let mut grid = Grid::new(2, 12);
    grid.set_text_runs(vec![TextRun {
        col: 3 * 16 + 5,
        row: 0,
        scale: 200,
        color: Rgb::new(1, 2, 3),
        bg: Some(Rgb::new(4, 5, 6)),
        follow: 0,
        anchor: None,
        text: "42".into(),
        seq: 7,
    }]);

    let rects = pass.build_run_rects(&grid);
    let [pos, dim] = [rects[0].pos, rects[0].dim];

    assert_eq!(
        [
            pos[0].fract(),
            pos[1].fract(),
            dim[0].fract(),
            dim[1].fract()
        ],
        [0.0; 4],
        "rect at {pos:?} sized {dim:?}"
    );
}

/// A one-row text run whose `text` is every printable ASCII glyph, enough
/// distinct masks at a large font to overflow the initial atlas.
fn ascii_burst_run() -> TextRun {
    TextRun {
        col: 0,
        row: 0,
        scale: 256,
        color: Rgb::new(255, 255, 255),
        bg: None,
        follow: 0,
        anchor: None,
        text: (0x21u32..=0x7e)
            .filter_map(char::from_u32)
            .collect::<String>()
            .into(),
        seq: 0,
    }
}

/// Assert the composite run instances match a fresh resolve against the
/// current atlas, so none were left frozen at a pre-grow atlas size.
fn assert_composite_runs_healed(pass: &mut TextPass, device: &Device, queue: &Queue, grid: &Grid) {
    let fresh = pass.build_text_run_instances(device, queue, grid, [0.0; 2], 0);
    assert!(!fresh.is_empty(), "the run contributes glyph instances");
    assert_eq!(
        bytemuck::cast_slice::<TextInstance, u8>(&pass.composite_run_scratch),
        bytemuck::cast_slice::<TextInstance, u8>(&fresh),
        "composite run instances must resolve against the grown atlas, not a pre-grow size"
    );
}

#[test]
fn composite_runs_reresolve_when_the_run_build_grows_the_atlas() {
    let (device, queue, mut pass) = headless_text_pass_font(160);
    let mut grid = Grid::new(2, 4);
    grid.set_text_runs(vec![ascii_burst_run()]);

    let (initial, _) = pass.atlas.texture_dims();
    pass.prepare_composite(
        &device,
        &queue,
        &grid,
        PoolOccluders::new(&[], 0, true),
        [640.0, 480.0],
        0.0,
        [0.0; 2],
        true,
        None,
        0,
        0,
    );
    let (grown, _) = pass.atlas.texture_dims();
    assert!(
        grown > initial,
        "the run's glyph burst must grow the atlas mid-build: {initial} -> {grown}"
    );

    assert_composite_runs_healed(&mut pass, &device, &queue, &grid);

    // A reuse composite returns early and leaves the healed instances intact.
    let healed = pass.composite_run_scratch.clone();
    pass.prepare_composite(
        &device,
        &queue,
        &grid,
        PoolOccluders::new(&[], 0, true),
        [640.0, 480.0],
        0.0,
        [0.0; 2],
        false,
        None,
        0,
        0,
    );
    assert_eq!(
        bytemuck::cast_slice::<TextInstance, u8>(&pass.composite_run_scratch),
        bytemuck::cast_slice::<TextInstance, u8>(&healed),
        "a reuse composite must not disturb the healed run instances"
    );
}

#[test]
fn composite_runs_reresolve_when_the_row_pack_grows_the_atlas() {
    let (device, queue, mut pass) = headless_text_pass_font(160);

    // A small run packs first, then a cell glyph burst grows the atlas
    // during the row pack, after the run instances were built. The run must
    // still land against the grown atlas, not the size it was built at.
    let mut grid = Grid::new(10, 10);
    for row in 0..10 {
        for col in 0..10 {
            let idx = (row * 10 + col) as u32;
            grid.get_mut(row, col).ch = char::from_u32(0x21 + idx % 94).unwrap_or('#');
        }
    }
    grid.set_text_runs(vec![TextRun {
        col: 0,
        row: 0,
        scale: 256,
        color: Rgb::new(255, 255, 255),
        bg: None,
        follow: 0,
        anchor: None,
        text: "42".into(),
        seq: 0,
    }]);

    let (initial, _) = pass.atlas.texture_dims();
    pass.prepare_composite(
        &device,
        &queue,
        &grid,
        PoolOccluders::new(&[], 0, true),
        [640.0, 480.0],
        0.0,
        [0.0; 2],
        true,
        None,
        0,
        0,
    );
    let (grown, _) = pass.atlas.texture_dims();
    assert!(
        grown > initial,
        "the cell glyph burst must grow the atlas during the row pack: {initial} -> {grown}"
    );

    assert_composite_runs_healed(&mut pass, &device, &queue, &grid);
}

#[test]
fn caches_clean_rows_and_rebuilds_damaged() {
    let (device, queue, mut pass) = headless_text_pass();
    let mut grid = Grid::new(3, 12);
    fill_row(&mut grid, 0, "a => b == c");
    fill_row(&mut grid, 1, "hello world");

    pass.rasterize_visible(
        &device,
        &queue,
        &grid,
        None,
        &Damage::Full,
        &Damage::Partial(Vec::new()),
    );

    // Change one row, then rebuild only it; the other rows come from the cache.
    fill_row(&mut grid, 1, "GOODBYE all");
    pass.rasterize_visible(
        &device,
        &queue,
        &grid,
        None,
        &dirty_rows(&[false, true, false], grid.cols()),
        &Damage::Partial(Vec::new()),
    );
    let incremental = pass.collect_grid_glyphs();

    pass.rasterize_visible(
        &device,
        &queue,
        &grid,
        None,
        &Damage::Full,
        &Damage::Partial(Vec::new()),
    );
    let full = pass.collect_grid_glyphs();

    assert_eq!(
        incremental, full,
        "rebuilding only the damaged row and reusing the rest matches a full rebuild"
    );
}

#[test]
fn scaled_cells_reshape_only_damaged_rows() {
    let (device, queue, mut pass) = headless_text_pass();
    let mut grid = Grid::new(4, 12);
    fill_row(&mut grid, 0, "alpha");
    fill_row(&mut grid, 1, "bravo");
    fill_row(&mut grid, 2, "charlie");
    grid.place_scaled(2, 0, 2);

    // Warm the per-row cache.
    pass.rasterize_visible(
        &device,
        &queue,
        &grid,
        None,
        &Damage::Full,
        &Damage::Partial(Vec::new()),
    );

    // VT damage marks row 0; decoration damage (a scale change) marks row 1.
    // The scaled cell on row 2 must no longer force a whole-grid reshape.
    let rebuilt = pass.rasterize_visible(
        &device,
        &queue,
        &grid,
        None,
        &dirty_rows(&[true, false, false, false], grid.cols()),
        &dirty_rows(&[false, true, false, false], grid.cols()),
    );
    assert_eq!(
        rebuilt,
        vec![0, 1],
        "only the VT- and decoration-damaged rows reshape, not the scaled grid"
    );
}

#[test]
fn routes_cell_fill_codepoints_by_kind() {
    let (device, queue, mut pass) = headless_text_pass();
    let mut grid = Grid::new(1, 4);
    grid.get_mut(0, 0).ch = '\u{E0B0}'; // geometric powerline separator
    grid.get_mut(0, 1).ch = 'M'; // ordinary glyph
    grid.get_mut(0, 2).ch = '\u{2500}'; // box-drawing, a font cell-fill glyph

    pass.rasterize_visible(
        &device,
        &queue,
        &grid,
        None,
        &Damage::Full,
        &Damage::Partial(Vec::new()),
    );
    let glyphs = pass.collect_grid_glyphs();
    let glyph = |col| glyphs.iter().find(|g| g.col == col).expect("glyph");

    assert!(
        matches!(glyph(0).source, GlyphSource::Procedural { cp: 0xE0B0, .. }),
        "a geometric powerline separator is drawn procedurally"
    );
    assert!(
        !glyph(0).cell_fill,
        "a procedural separator scales no font bitmap"
    );

    assert!(
        matches!(glyph(1).source, GlyphSource::Font(_)) && !glyph(1).cell_fill,
        "an ordinary letter shapes from the font and is not cell-fill"
    );

    assert!(
        matches!(glyph(2).source, GlyphSource::Font(_)) && glyph(2).cell_fill,
        "box-drawing stays on the font path and scales its glyph to the cell"
    );
}

/// One damaged row under an active region leaves both buffers holding what a
/// full rebuild would have produced.
///
/// A region is active for as long as a full-screen program is on screen, so
/// patching its rows is the common path rather than a rare one. Patching splits
/// each row across the two buffers, and a row's glyphs have to land on the same
/// side and in the same order a from-scratch split puts them.
#[test]
fn a_patched_region_frame_matches_one_built_from_scratch() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let rows = 5;

    let region = ScrollRegion {
        top: 1,
        left: 2,
        width: 6,
        height: 3,
        offset: 0,
    };
    let build = |text: &str| {
        let mut grid = Grid::new(rows, 20);
        for row in 0..rows {
            fill_row(&mut grid, row, text);
        }
        grid.set_scroll_region(Some(region));
        grid
    };
    fn frame(damage: &Damage) -> Frame<'_> {
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
            decoration_damage: damage,
            scrolled_rows: 0,
            sketch_reveals: &[],
        }
    }

    // A full frame, then one row changed and only that row damaged.
    let first = build("aaaaaaaaaa");
    pass.prepare(
        &device,
        &queue,
        &first,
        resolution,
        &frame(&Damage::Full),
        &[],
        &[],
        &[],
    );

    let mut second = build("aaaaaaaaaa");
    fill_row(&mut second, 2, "bbbbbbbbbb");
    let mut row_two = vec![None; rows];
    row_two[2] = whole_row(second.cols());
    pass.prepare(
        &device,
        &queue,
        &second,
        resolution,
        &frame(&Damage::Partial(row_two)),
        &[],
        &[],
        &[],
    );
    let patched = (
        pass.count,
        pass.region_count,
        row_len(&pass.plain_row_instances),
        row_len(&pass.region_row_instances),
    );

    // The same screen reached in one full frame, every row split from scratch.
    let (device, queue, mut fresh) = headless_text_pass();
    fresh.prepare(
        &device,
        &queue,
        &second,
        resolution,
        &frame(&Damage::Full),
        &[],
        &[],
        &[],
    );

    assert_eq!(
        patched,
        (
            fresh.count,
            fresh.region_count,
            row_len(&fresh.plain_row_instances),
            row_len(&fresh.region_row_instances),
        ),
        "a patched region frame splits into the same two buffers a rebuild does",
    );
    assert!(
        patched.1 > 0 && patched.0 > 0,
        "the region has to hold some glyphs and leave some outside: {patched:?}"
    );
}

/// Two dirty underline rows with a clean row between them travel as two writes,
/// not one span reaching from the first of them to the end of the buffer.
///
/// Driven through the same two calls the pass makes, since the coalescing is only
/// as good as the row list handed to it. A list that included clean rows, or came
/// out unsorted, would collapse the runs back into one.
#[test]
fn disjoint_dirty_underline_rows_upload_separately() {
    let underline = UnderlineInstance {
        cell_pos: [0.0, 0.0],
        color: [0.0; 3],
        style: STYLE_DOTTED,
        row: 0,
    };
    // Rows of differing length, so a write placed by summing the rows before it
    // lands somewhere a fixed stride would not.
    let cache: Vec<Vec<UnderlineInstance>> = [2, 1, 3, 1, 2]
        .iter()
        .map(|&len| vec![underline; len])
        .collect();

    let mut rows = Vec::new();
    let dirty = dirty_rows(&[false, true, false, true, false], 1);
    underline_rows_to_build(&dirty, cache.len(), false, &mut rows);
    assert_eq!(rows, [1, 3], "only the dirty rows, ascending");

    assert_eq!(
        row_uploads(&cache, &rows, None).collect::<Vec<_>>(),
        [(2, 1..2), (6, 3..4)],
        "one write per dirty row, each offset past the rows before it"
    );

    // The row that came back a different length displaced the rows after it, so
    // from there the buffer has to be rewritten rather than patched.
    assert_eq!(
        row_uploads(&cache, &rows, Some(3)).collect::<Vec<_>>(),
        [(2, 1..2), (6, 3..5)],
        "the resized row's write runs to the end and absorbs the rows after it"
    );

    underline_rows_to_build(&Damage::Full, cache.len(), false, &mut rows);
    assert_eq!(rows, [0, 1, 2, 3, 4], "full damage rebuilds every row");

    underline_rows_to_build(&dirty, cache.len(), true, &mut rows);
    assert_eq!(
        rows,
        [0, 1, 2, 3, 4],
        "a resized cache rebuilds every row whatever the damage says"
    );
}

/// A line range slices straight to its glyphs, and a range past the end clamps
/// rather than indexing off the end of the index.
///
/// The lines here hold two, zero, and one glyph, since a blank or off-grid line
/// shapes to nothing while still occupying a line. A slice that assumed a glyph
/// per line, or that let an empty line collapse, would return the wrong rows.
#[test]
fn overlay_content_slices_a_range_of_lines() {
    let glyph = |row: usize| PendingGlyph {
        row,
        col: 0,
        source: GlyphSource::Procedural {
            cp: 0,
            width: 1,
            height: 1,
        },
        fg: Rgb::new(0, 0, 0),
        scale: 1.0,
        cell_fill: false,
        wide: false,
        info: GlyphInfo {
            kind: AtlasKind::Mask,
            uv: [0.0; 4],
            size: [0; 2],
            placement: [0; 2],
        },
        resolved_epoch: 0,
    };
    let content = OverlayContent {
        glyphs: vec![glyph(0), glyph(1), glyph(2)],
        starts: vec![0, 2, 2, 3],
    };
    let rows = |start, end| {
        content
            .window(start..end)
            .iter()
            .map(|glyph| glyph.row)
            .collect::<Vec<_>>()
    };

    assert_eq!(content.lines(), 3, "the trailing entry is not a line");
    assert_eq!(rows(0, 1), [0, 1], "the first line's two glyphs");
    assert_eq!(rows(1, 2), [0usize; 0], "the line that shaped to nothing");
    assert_eq!(rows(1, 3), [2], "spanning the empty line and the last");
    assert_eq!(rows(0, 3), [0, 1, 2], "every line");
    assert_eq!(rows(2, 9), [2], "an end past the last line clamps to it");
    assert_eq!(
        rows(9, 9),
        [0usize; 0],
        "a start past the last line is empty"
    );
    assert_eq!(
        rows(2, 0),
        [0usize; 0],
        "an end below the start is empty, not an inverted slice"
    );

    let empty = OverlayContent::default();
    assert_eq!(empty.lines(), 0, "no lines are held");
    assert!(
        empty.window(0..4).is_empty(),
        "an overlay with no line index at all yields no glyphs"
    );
}

/// The window never leaves out a line the box can show, at any scroll or scale.
///
/// Brute-forced rather than spot-checked. The failure this guards is an off-by-one
/// at a box edge during a smooth scroll, where a glyph that should have drawn goes
/// missing for the few frames it straddles the boundary. Every scroll offset in
/// quarter cells is checked against where each line actually lands.
#[test]
fn the_visible_window_covers_every_line_the_box_can_show() {
    let lines = 40;

    for scale in [1usize, 2, 4] {
        for height in [3u16, 10, 25] {
            for step in 0..(lines * scale * 4) {
                let scroll = step as f32 / 4.0;
                let window = visible_lines(height, scale, scroll, lines);

                for line in 0..lines {
                    // The line's glyphs are drawn this many cells below the box's
                    // top, being its content offset inset past the border and
                    // shifted by the scroll. They show when that span meets the
                    // box's rows.
                    let top = 1.0 + (line * scale) as f32 - scroll;
                    let shows = top + scale as f32 > 0.0 && top < f32::from(height);
                    assert!(
                        !shows || window.contains(&line),
                        "line {line} shows at scroll {scroll} (scale {scale}, \
                             height {height}) but the window is {window:?}"
                    );
                }
            }
        }
    }
}

/// The window stays close to the box's own size however much content sits behind
/// it, which is the whole point of computing one.
///
/// The bound is asserted tightly, within a line or two of what the box holds. The
/// coverage test above can only catch a window that came out too narrow, since a
/// wider one still contains every line that shows, so it takes an upper bound to
/// notice a stray `scale` term or a missing divide.
#[test]
fn the_visible_window_is_sized_by_the_box_not_the_content() {
    for scale in [1usize, 2, 4] {
        for height in [3u16, 10, 25] {
            for step in 0..80 {
                let window = visible_lines(height, scale, step as f32 / 4.0, 4000);
                let holds = (height as usize / scale) + 2;
                assert!(
                    window.len() <= holds,
                    "a {height}-row box at scale {scale} reads at most {holds} of \
                         4000 lines, not {}",
                    window.len()
                );
            }
        }
    }

    assert_eq!(
        visible_lines(10, 1, 0.0, 6),
        0..6,
        "content shorter than its box is read whole"
    );
    assert_eq!(
        visible_lines(10, 1, 0.0, 0),
        0..0,
        "an empty overlay yields an empty window"
    );
    assert!(
        visible_lines(10, 1, 9_999.0, 20).is_empty(),
        "scrolled past the end there is nothing to read"
    );
}

/// The split the row caches are keyed on tracks the rectangle and ignores the
/// offset.
///
/// This is what keeps a scrolling region from rebuilding every row each tick.
/// The offset moves constantly and rides the globals uniform, so it changes where
/// the region's rows are drawn, not which cells belong to it.
#[test]
fn a_region_split_follows_the_rectangle_not_the_offset() {
    let rect = ScrollRegion {
        top: 1,
        left: 2,
        width: 3,
        height: 4,
        offset: 0,
    };

    assert_eq!(
        region_split(Some(ScrollRegion { offset: 99, ..rect })),
        region_split(Some(rect)),
        "a scrolled region keeps the split its rows were built against"
    );
    assert_ne!(
        region_split(Some(ScrollRegion { width: 4, ..rect })),
        region_split(Some(rect)),
        "a resized region does not"
    );
    assert_eq!(region_split(None), None, "no region has no split");
}

/// A moved region rectangle re-splits the rows. A moved offset does not.
///
/// The offset changes on every scroll tick and rides the globals uniform, so
/// invalidating on it would rebuild every row constantly and gain nothing. The
/// rectangle is what decides which buffer a cell's glyph goes to.
/// Each buffer's globals have to carry the rotation its instances were
/// built under, since the vertex stage reads the slot back through them.
/// The screen-anchored buffer carries none, because an overlay's content
/// rows run past the bottom of the screen and a wrap would fold them back
/// over the box.
#[test]
fn only_the_grid_rows_buffers_carry_a_rotation() {
    let (device, queue, mut pass) = headless_text_pass();
    let mut grid = Grid::new(4, 20);
    fill_row(&mut grid, 0, "cells");

    pass.prepare(
        &device,
        &queue,
        &grid,
        [640.0, 480.0],
        &Frame {
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
            decoration_damage: &Damage::Full,
            scrolled_rows: 0,
            sketch_reveals: &[],
        },
        &[],
        &[],
        &[],
    );

    let rows_of = |globals: Option<TextGlobals>| globals.expect("globals written").rows;

    assert_eq!(rows_of(pass.last_globals), 4, "grid draws rotate by rows");
    assert_eq!(
        rows_of(pass.last_region_globals),
        4,
        "region draws carry grid rows too"
    );
    assert_eq!(
        rows_of(pass.last_static_globals),
        0,
        "screen-anchored draws must not wrap"
    );
}

/// A scroll leaves a kept row's glyphs in the slot they were rasterized
/// into, naming the row they were rasterized for. A moved region rectangle
/// then rebuilds every row's instances from those cached glyphs without
/// re-rasterizing any, and the split reads the row off each glyph, so a
/// stale one sends the glyph to the wrong buffer.
#[test]
fn a_scroll_then_a_moved_region_splits_like_a_build_that_never_scrolled() {
    let (device, queue, mut pass) = headless_text_pass();
    let (_, _, mut fresh) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let rows = 5;
    let lines = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

    let screen = |from: usize, region: ScrollRegion| {
        let mut grid = Grid::new(rows, 20);
        for row in 0..rows {
            fill_row(&mut grid, row, lines[from + row]);
        }
        grid.set_scroll_region(Some(region));
        grid
    };
    fn frame(damage: &Damage, scrolled_rows: isize) -> Frame<'_> {
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
            decoration_damage: damage,
            scrolled_rows,
            sketch_reveals: &[],
        }
    }

    let before = ScrollRegion {
        top: 0,
        left: 0,
        width: 20,
        height: 2,
        offset: 0,
    };
    let after = ScrollRegion { top: 2, ..before };

    // Scroll by one, so every row but the last is carried rather than
    // rasterized, then move the rectangle under those carried rows.
    pass.prepare(
        &device,
        &queue,
        &screen(0, before),
        resolution,
        &frame(&Damage::Full, 0),
        &[],
        &[],
        &[],
    );
    let mut last_row_only = vec![None; rows];
    last_row_only[rows - 1] = whole_row(20);
    pass.prepare(
        &device,
        &queue,
        &screen(1, before),
        resolution,
        &frame(&Damage::Partial(last_row_only), 1),
        &[],
        &[],
        &[],
    );
    pass.prepare(
        &device,
        &queue,
        &screen(1, after),
        resolution,
        &frame(&Damage::Partial(vec![None; rows]), 0),
        &[],
        &[],
        &[],
    );

    fresh.prepare(
        &device,
        &queue,
        &screen(1, after),
        resolution,
        &frame(&Damage::Full, 0),
        &[],
        &[],
        &[],
    );

    assert_eq!(
        (
            row_len(&pass.plain_row_instances),
            row_len(&pass.region_row_instances)
        ),
        (
            row_len(&fresh.plain_row_instances),
            row_len(&fresh.region_row_instances)
        ),
        "the carried rows split by the rows they are on now"
    );
}

/// A frame whose grid declares `text_runs` and `overlays` and nothing else,
/// for the gate tests, which read what a prepare rebuilt rather than what it
/// drew.
fn chrome_grid(text_runs: Vec<TextRun>, overlays: Vec<Overlay>) -> Grid {
    let mut grid = Grid::new(4, 20);
    grid.set_text_runs(text_runs);
    grid.set_overlays(overlays);
    grid
}

/// An idle whole-screen frame over a four-row grid.
fn chrome_frame() -> Frame<'static> {
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
        decoration_damage: &Damage::Full,
        scrolled_rows: 0,
        sketch_reveals: &[],
    }
}

fn chrome_run(text: &str) -> TextRun {
    TextRun {
        col: 0,
        row: 0,
        scale: 256,
        color: Rgb::new(200, 200, 200),
        bg: None,
        follow: 0,
        anchor: None,
        text: text.into(),
        seq: 0,
    }
}

fn chrome_overlay(content: &str) -> Overlay {
    Overlay {
        top: 0,
        left: 0,
        width: 12,
        height: 3,
        fill: Rgb::new(0, 0, 0),
        border: Rgb::new(0, 0, 0),
        content_fg: Rgb::new(255, 255, 255),
        scale: 1,
        offset: [0, 0],
        bold: false,
        content: content.to_owned(),
    }
}

/// The chrome the off-grid runs and the popovers back changes far more
/// rarely than frames are drawn, so a frame whose grid reports neither
/// moved must rebuild neither. Both builds are pure, so a count is the only
/// thing that separates a reuse from a rebuild producing the same bytes.
#[test]
fn an_unchanged_grid_rebuilds_neither_the_runs_nor_the_popovers() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let frame = chrome_frame();
    let grid = chrome_grid(vec![chrome_run("x")], vec![chrome_overlay("x")]);

    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
    assert_eq!(
        (pass.run_builds, pass.overlay_builds),
        (1, 1),
        "the second frame reuses what the first built",
    );

    let moved = chrome_grid(vec![chrome_run("xx")], vec![chrome_overlay("xx")]);
    pass.prepare(&device, &queue, &moved, resolution, &frame, &[], &[], &[]);
    assert_eq!(
        (pass.run_builds, pass.overlay_builds),
        (2, 2),
        "a grid the pass has not seen rebuilds both",
    );
}

/// A run anchored to a gliding pane draws after that pane composites,
/// shifted by its glide and clipped to it. The base draw leaves it out, or
/// the composite paints over it and the label vanishes mid-glide.
#[test]
fn an_anchored_run_leaves_the_base_draw_and_rides_shifted() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let frame = chrome_frame();

    let mut anchored_run = chrome_run("x");
    anchored_run.anchor = Some((3, 10.0));
    anchored_run.bg = Some(Rgb::new(4, 5, 6));
    let mut fixed_run = chrome_run("y");
    fixed_run.row = 2 * 16;
    let grid = chrome_grid(vec![anchored_run, fixed_run], Vec::new());

    // Nothing composites, so both runs draw with the rest.
    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
    let settled = pass.text_run_count;
    assert!(settled > 0, "both runs build glyphs");
    assert!(pass.riding_runs.is_empty(), "and nothing rides");
    assert_eq!(pass.rect_count, 1, "and the run's backing rect is drawn");
    // The anchored run is declared first, so its glyphs lead the base
    // buffer while nothing glides. Held to compare against the shifted one.
    let unshifted = pass.text_run_build_scratch[0].pos[1];
    let rect_unshifted = pass.run_rect_build_scratch[0].pos[1];

    // The host eased half a row past the top the run was laid out at, which
    // is 9.5 pixels on this pass's 19-pixel cell.
    let anchored = [HostRide {
        host: 3,
        top_rows: 10.5,
        scissor: [0, 0, 40, 40],
    }];
    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame,
        &[],
        &anchored,
        &[],
    );

    assert!(
        pass.text_run_count < settled,
        "the base draw lost the anchored run's glyphs, {} of {settled}",
        pass.text_run_count,
    );
    let [host] = pass.riding_runs.as_slice() else {
        panic!(
            "one gliding host takes one span, got {}",
            pass.riding_runs.len()
        );
    };
    assert_eq!(host.scissor, [0, 0, 40, 40], "clipped to its host");
    assert!(!host.glyphs.is_empty(), "and carries the run's glyphs");
    assert_eq!(host.rects.len(), 1, "and its backing rect");
    assert_eq!(
        pass.rect_count, 0,
        "which the base draw gave up, or the backing paints twice",
    );

    let rect_riding = pass.riding_rect_scratch[host.rects.start as usize].pos[1];
    assert_eq!(
        rect_riding - rect_unshifted,
        -9.5,
        "the backing rides with the glyphs it backs",
    );

    let riding = pass.riding_glyph_scratch[host.glyphs.start as usize].pos[1];
    assert_eq!(
        riding - unshifted,
        -9.5,
        "the same run, shifted from its own top row to the host's eased top",
    );
}

/// A glide moves where its riding runs draw and nothing else about them, so
/// a second glide frame shifts the runs the first one built instead of
/// walking their characters through the atlas again. New runs, or another
/// host gliding in the place of the first, build them again.
#[test]
fn a_second_glide_frame_rebuilds_no_riding_run() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let frame = chrome_frame();
    // The run's layout put its host's top at row 10, on a 19-pixel cell.
    let grid_of = |text: &str| {
        let mut run = chrome_run(text);
        run.anchor = Some((3, 10.0));
        chrome_grid(vec![run], Vec::new())
    };
    let ride = |host, top_rows| {
        [HostRide {
            host,
            top_rows,
            scissor: [0, 0, 40, 40],
        }]
    };
    let (grid, changed) = (grid_of("x"), grid_of("xx"));

    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame,
        &[],
        &ride(3, 10.5),
        &[],
    );
    let first = pass.riding_glyph_scratch[0].pos[1];
    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame,
        &[],
        &ride(3, 10.25),
        &[],
    );
    assert_eq!(
        (
            pass.riding_builds,
            pass.riding_glyph_scratch[0].pos[1] - first
        ),
        (1, 4.75),
        "(builds, shift gained) as the glide eases a quarter row further",
    );

    pass.prepare(
        &device,
        &queue,
        &changed,
        resolution,
        &frame,
        &[],
        &ride(3, 10.25),
        &[],
    );
    assert_eq!(pass.riding_builds, 2, "new runs build again");

    pass.prepare(
        &device,
        &queue,
        &changed,
        resolution,
        &frame,
        &[],
        &ride(9, 0.5),
        &[],
    );
    assert_eq!(
        (pass.riding_builds, pass.riding_runs.len()),
        (3, 0),
        "(builds, riding hosts) once another host glides in its place",
    );
}

/// Each riding run shifts from the top row its own layout assumed to its own
/// host's eased top, whatever it shares with the run before it.
#[test]
fn each_riding_run_shifts_by_its_own_host_and_top_row() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let frame = chrome_frame();
    let anchored_run = |row: i16, host: u32, top_rows: f32| {
        let mut run = chrome_run("x");
        run.row = row * 16;
        run.anchor = Some((host, top_rows));
        run
    };
    // The second run shares a host with the first and a top row with the
    // third, and a group that ignores either shifts it wrong.
    let grid = chrome_grid(
        vec![
            anchored_run(0, 3, 10.0),
            anchored_run(1, 3, 12.0),
            anchored_run(2, 4, 12.0),
        ],
        Vec::new(),
    );

    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
    let unshifted: Vec<f32> = pass
        .text_run_build_scratch
        .iter()
        .map(|glyph| glyph.pos[1])
        .collect();
    let rides = [(3, 10.5, 0), (4, 9.0, 40)].map(|(host, top_rows, left)| HostRide {
        host,
        top_rows,
        scissor: [left, 0, 40, 40],
    });
    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &rides, &[]);

    let shifts: Vec<f32> = pass
        .riding_glyph_scratch
        .iter()
        .zip(&unshifted)
        .map(|(glyph, y)| glyph.pos[1] - y)
        .collect();
    let spans: Vec<_> = pass
        .riding_runs
        .iter()
        .map(|host| (host.scissor[0], host.glyphs.clone()))
        .collect();
    assert_eq!(
        (shifts, spans),
        (vec![-9.5, 28.5, 57.0], vec![(0, 0..2), (40, 2..3)]),
        "(each run's shift on a 19-pixel cell, each host's clip and glyph span)",
    );
}

/// A run anchored to a pool that is not compositing this frame draws with
/// the rest, unshifted. Only a glide splits it out.
#[test]
fn a_run_whose_host_is_still_does_not_ride() {
    let (device, queue, mut pass) = headless_text_pass();
    let mut run = chrome_run("x");
    run.anchor = Some((3, 0.0));
    let grid = chrome_grid(vec![run], Vec::new());

    // A different pool glides, so this run's host is not among them.
    let elsewhere = [HostRide {
        host: 9,
        top_rows: 0.5,
        scissor: [0, 0, 40, 40],
    }];
    pass.prepare(
        &device,
        &queue,
        &grid,
        [640.0, 480.0],
        &chrome_frame(),
        &[],
        &elsewhere,
        &[],
    );

    assert!(pass.riding_runs.is_empty(), "nothing rides");
    assert!(pass.text_run_count > 0, "and the run draws with the rest");
}

/// A host that starts or stops gliding moves runs between the two sets, so
/// the base instances have to be rebuilt. Reusing them leaves the run drawn
/// twice, or not at all.
#[test]
fn a_host_starting_to_glide_rebuilds_the_base_runs() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let frame = chrome_frame();
    let mut run = chrome_run("x");
    run.anchor = Some((3, 0.0));
    let grid = chrome_grid(vec![run], Vec::new());
    let anchored = [HostRide {
        host: 3,
        top_rows: 0.5,
        scissor: [0, 0, 40, 40],
    }];

    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
    assert_eq!(pass.run_builds, 1, "a still frame reuses");

    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame,
        &[],
        &anchored,
        &[],
    );
    assert_eq!(pass.run_builds, 2, "the glide starting rebuilds");

    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame,
        &[],
        &anchored,
        &[],
    );
    assert_eq!(pass.run_builds, 2, "and mid-glide frames reuse again");

    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
    assert_eq!(pass.run_builds, 3, "the glide ending rebuilds too");
}

/// A label arrives as the box around it closes, not with the first pen
/// stroke, which would name a shape not yet recognizable. The array the
/// fragment stage reads is what carries that, one entry per declared mark.
#[test]
fn a_label_fades_in_over_the_tail_of_its_mark_s_reveal() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let mut grid = chrome_grid(vec![chrome_run("x")], vec![chrome_overlay("x")]);
    grid.set_sketches(vec![test_sketch(5), test_sketch(7)]);

    let alphas_at = |pass: &mut TextPass, progress: &[(f32, f32)]| {
        let reveals: Vec<SketchReveal> = progress
            .iter()
            .map(|&(revealed, alpha)| SketchReveal {
                revealed,
                width: 64.0,
                alpha,
            })
            .collect();
        let frame = Frame {
            sketch_reveals: &reveals,
            ..chrome_frame()
        };
        pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
        pass.last_sketch_alpha.clone()
    };

    assert_eq!(
        alphas_at(&mut pass, &[(0.0, 1.0), (0.5, 1.0)]),
        [0.0, 0.0],
        "nothing shows through the first half of a reveal",
    );
    assert_eq!(
        alphas_at(&mut pass, &[(1.0, 1.0), (0.55, 1.0)]),
        [1.0, 0.0],
        "a finished mark carries its label whole, and the curve starts at 0.55",
    );

    let [rising, _] = alphas_at(&mut pass, &[(0.8, 1.0), (0.0, 1.0)])
        .try_into()
        .expect("two marks");
    assert!(
        rising > 0.0 && rising < 1.0,
        "and eases between, got {rising}",
    );

    assert_eq!(
        alphas_at(&mut pass, &[(1.0, 0.5), (1.0, 1.0)]),
        [0.5, 1.0],
        "a dimmed mark carries its label at the same opacity",
    );

    assert_eq!(
        alphas_at(&mut pass, &[]),
        [1.0, 1.0],
        "a caller with no clock draws every label whole",
    );
}

/// A run's follow slot is an index into the mark list, so a mark declared
/// or dropped renumbers the slot of every run after it. A gate watching
/// only the run list reuses instances that now name the wrong mark's fade.
#[test]
fn a_changed_mark_list_rebuilds_the_runs_it_renumbers() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let frame = chrome_frame();

    // One grid throughout, since the rebuild key names the grid as well as
    // its epochs and a fresh one rebuilds for that reason alone.
    let mut grid = chrome_grid(vec![chrome_run("x")], vec![chrome_overlay("x")]);
    grid.set_sketches(vec![test_sketch(7), test_sketch(5)]);

    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
    assert_eq!(pass.run_builds, 1, "an unchanged mark list reuses");

    grid.set_sketches(vec![test_sketch(5)]);
    pass.prepare(&device, &queue, &grid, resolution, &frame, &[], &[], &[]);
    assert_eq!(
        pass.run_builds, 2,
        "dropping the mark ahead of it moved every later slot",
    );
}

/// The live screen and the scrollback window take turns through one pass,
/// and each grid counts only its own changes, so two of them meet on a count
/// routinely. A pass reading the count alone would hold one grid's off-grid
/// runs over the other for as long as the two agreed.
#[test]
fn a_second_grid_holding_the_same_run_count_is_not_the_first() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let frame = chrome_frame();

    // Declared by the same calls, so both counters land in the same place
    // over runs of different lengths.
    let first = chrome_grid(vec![chrome_run("x")], Vec::new());
    let second = chrome_grid(vec![chrome_run("xxxxxxxx")], Vec::new());
    assert_eq!(
        first.text_runs_epoch(),
        second.text_runs_epoch(),
        "the fixture needs two grids that agree on their count",
    );

    pass.prepare(&device, &queue, &first, resolution, &frame, &[], &[], &[]);
    pass.prepare(&device, &queue, &second, resolution, &frame, &[], &[], &[]);
    assert_eq!(
        pass.text_run_count, 8,
        "the second grid's runs are the ones drawn",
    );
}

/// The overlay glyphs and their bases are cached the same way the runs are,
/// and against a grid that likewise takes turns with another. See
/// [`a_second_grid_holding_the_same_run_count_is_not_the_first`].
#[test]
fn a_second_grid_holding_the_same_popover_count_is_not_the_first() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let frame = chrome_frame();

    // One overlay each, so the length check the reuse also makes cannot be
    // what tells them apart.
    let first = chrome_grid(Vec::new(), vec![chrome_overlay("x")]);
    let second = chrome_grid(Vec::new(), vec![chrome_overlay("xxxxxxxx")]);
    assert_eq!(
        first.popovers_epoch(),
        second.popovers_epoch(),
        "the fixture needs two grids that agree on their count",
    );

    pass.prepare(&device, &queue, &first, resolution, &frame, &[], &[], &[]);
    let one_glyph = pass.overlay_count;
    pass.prepare(&device, &queue, &second, resolution, &frame, &[], &[], &[]);
    assert_eq!(
        (one_glyph, pass.overlay_count),
        (1, 8),
        "the second grid's overlay content is the one drawn",
    );
}

#[test]
fn only_a_moved_region_rectangle_resplits_the_rows() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let frame = Frame {
        cursor: None,
        cursor_corners: None,
        scroll: Scroll {
            grid: 0.0,
            document: 0.0,
            scrollback: 0.0,
            region: 0.0,
            popovers: &[],
        },
        damage: &Damage::Partial(vec![None; 4]),
        decoration_damage: &Damage::Partial(vec![None; 4]),
        scrolled_rows: 0,
        sketch_reveals: &[],
    };
    let build = |region: ScrollRegion| {
        let mut grid = Grid::new(4, 20);
        for row in 0..4 {
            fill_row(&mut grid, row, "cells");
        }
        grid.set_scroll_region(Some(region));
        grid
    };
    let rect = ScrollRegion {
        top: 1,
        left: 0,
        width: 3,
        height: 2,
        offset: 0,
    };

    let full = Frame {
        damage: &Damage::Full,
        decoration_damage: &Damage::Full,
        ..frame
    };
    pass.prepare(
        &device,
        &queue,
        &build(rect),
        resolution,
        &full,
        &[],
        &[],
        &[],
    );
    let split = (
        row_len(&pass.plain_row_instances),
        row_len(&pass.region_row_instances),
    );

    // An idle frame whose region only scrolled keeps the split it had.
    let scrolled = ScrollRegion { offset: 5, ..rect };
    pass.prepare(
        &device,
        &queue,
        &build(scrolled),
        resolution,
        &frame,
        &[],
        &[],
        &[],
    );
    assert_eq!(
        (
            row_len(&pass.plain_row_instances),
            row_len(&pass.region_row_instances)
        ),
        split,
        "a moved offset leaves the split alone"
    );

    // A wider rectangle takes cells from the plain side.
    let wider = ScrollRegion { width: 5, ..rect };
    pass.prepare(
        &device,
        &queue,
        &build(wider),
        resolution,
        &frame,
        &[],
        &[],
        &[],
    );
    let resplit = (
        row_len(&pass.plain_row_instances),
        row_len(&pass.region_row_instances),
    );
    assert!(
        resplit.1 > split.1 && resplit.0 < split.0,
        "a moved rectangle re-splits even on an idle frame: {split:?} then {resplit:?}"
    );
}

/// A scroll moves the rows above it without changing them, so sliding the
/// caches has to leave those rows holding exactly what a full rebuild would
/// have produced for their new positions.
#[test]
fn a_rotated_frame_matches_one_built_from_scratch() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let rows = 5;
    let lines = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

    fn fill(grid: &mut Grid, rows: usize, lines: &[&str], from: usize) {
        for row in 0..rows {
            fill_row(grid, row, lines[from + row]);
        }
    }
    fn frame(damage: &Damage, scrolled_rows: isize) -> Frame<'_> {
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
            decoration_damage: damage,
            scrolled_rows,
            sketch_reveals: &[],
        }
    }

    // Build the pre-scroll screen, then scroll it by one row: the content
    // slides up and only the last row is new.
    let mut grid = Grid::new(rows, 20);
    fill(&mut grid, rows, &lines, 0);
    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame(&Damage::Full, 0),
        &[],
        &[],
        &[],
    );

    let mut scrolled = Grid::new(rows, 20);
    fill(&mut scrolled, rows, &lines, 1);
    let mut last_row_only = vec![None; rows];
    last_row_only[rows - 1] = whole_row(20);
    pass.prepare(
        &device,
        &queue,
        &scrolled,
        resolution,
        &frame(&Damage::Partial(last_row_only), 1),
        &[],
        &[],
        &[],
    );
    let rotated = pass.collect_grid_glyphs();

    // The same screen reached without a scroll, every row rebuilt.
    let (device, queue, mut fresh_pass) = headless_text_pass();
    fresh_pass.prepare(
        &device,
        &queue,
        &scrolled,
        resolution,
        &frame(&Damage::Full, 0),
        &[],
        &[],
        &[],
    );
    let fresh = fresh_pass.collect_grid_glyphs();

    assert_eq!(
        rotated.len(),
        fresh.len(),
        "a rotated screen holds as many glyphs as a rebuilt one",
    );
    for (got, want) in rotated.iter().zip(&fresh) {
        assert_eq!(
            (got.row, got.col, got.source),
            (want.row, want.col, want.source),
            "every glyph lands on the cell a rebuild would put it on, and is \
                 the glyph a rebuild would put there",
        );
    }
}

/// A pool composited after a scroll holds what one built from scratch holds.
///
/// The scrolled frame slides its per-row caches and shapes only the rows the
/// scroll exposed, so every other row keeps glyphs shaped against the frame
/// before and the row each was shaped for is repaired rather than recomputed.
/// A repair that is off by any amount lands those glyphs on the wrong cells,
/// which the comparison against a rebuild catches.
#[test]
fn a_scrolled_composite_matches_one_built_from_scratch() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let rows = 5;
    let lines = ["alpha", "bravo", "charlie", "delta", "echo", "foxtrot"];

    fn fill(grid: &mut Grid, rows: usize, lines: &[&str], from: usize) {
        for row in 0..rows {
            fill_row(grid, row, lines[from + row]);
        }
    }

    let mut grid = Grid::new(rows, 20);
    fill(&mut grid, rows, &lines, 0);
    pass.prepare_composite(
        &device,
        &queue,
        &grid,
        PoolOccluders::new(&[], 0, true),
        resolution,
        0.0,
        [0.0; 2],
        true,
        None,
        0,
        0,
    );

    // The content slid up by one, so only the last row is new.
    let mut scrolled = Grid::new(rows, 20);
    fill(&mut scrolled, rows, &lines, 1);
    pass.prepare_composite(
        &device,
        &queue,
        &scrolled,
        PoolOccluders::new(&[], 0, true),
        resolution,
        0.0,
        [0.0; 2],
        true,
        Some(1),
        0,
        0,
    );
    let carried = pass.composite_glyphs(0);

    // The same rows reached without a scroll, every one of them shaped here.
    let (device, queue, mut fresh_pass) = headless_text_pass();
    fresh_pass.prepare_composite(
        &device,
        &queue,
        &scrolled,
        PoolOccluders::new(&[], 0, true),
        resolution,
        0.0,
        [0.0; 2],
        true,
        None,
        0,
        0,
    );
    let fresh = fresh_pass.composite_glyphs(0);

    assert!(!fresh.is_empty(), "the fixture has to shape something");
    assert_eq!(
        carried.len(),
        fresh.len(),
        "a scrolled composite holds as many glyphs as a rebuilt one",
    );
    for (got, want) in carried.iter().zip(&fresh) {
        assert_eq!(
            (got.row, got.col),
            (want.row, want.col),
            "every carried glyph lands on the cell a rebuild would put it on",
        );
    }
}

/// A pool that moved rebuilds, even though its rows say nothing changed.
///
/// Instances snap against the absolute screen pixel grid, so where the region
/// sits is folded into every position. The pool's content version tracks the
/// region's size and never its corner, so nothing but the recorded origin
/// tells a moved region from a still one, and reusing the old instances puts
/// the pooled text up to a pixel off the live grid.
#[test]
fn a_moved_composite_rebuilds_against_its_new_origin() {
    // A quarter cell is the move here, because the cell is whole pixels and
    // a whole-cell move therefore snaps to the same offsets it started on.
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let mut grid = Grid::new(3, 20);
    fill_row(&mut grid, 0, "alpha");
    fill_row(&mut grid, 1, "bravo");
    grid.set_text_runs(vec![TextRun {
        col: 32,
        row: 32,
        scale: 160,
        color: Rgb::new(255, 255, 255),
        bg: None,
        follow: 0,
        anchor: None,
        text: "charlie".into(),
        seq: 0,
    }]);

    let composite = |pass: &mut TextPass, origin, content_changed| {
        pass.prepare_composite(
            &device,
            &queue,
            &grid,
            PoolOccluders::new(&[], 0, true),
            resolution,
            0.0,
            origin,
            content_changed,
            None,
            0,
            0,
        );
        (
            pass.composite_instances(0),
            pass.composite_run_scratch.clone(),
        )
    };

    let (glyphs_at_rest, runs_at_rest) = composite(&mut pass, [0.0; 2], true);
    let (glyphs_moved, runs_moved) = composite(&mut pass, [0.25, 0.25], false);
    let (glyphs_rebuilt, runs_rebuilt) = composite(&mut pass, [0.25, 0.25], true);

    assert!(
        !glyphs_rebuilt.is_empty() && !runs_rebuilt.is_empty(),
        "the fixture has to build both grid glyphs and run glyphs"
    );
    assert_ne!(
        bytemuck::cast_slice::<TextInstance, u8>(&glyphs_at_rest),
        bytemuck::cast_slice::<TextInstance, u8>(&glyphs_rebuilt),
        "the move has to change the snap, or the reuse below proves nothing"
    );
    assert_eq!(
        bytemuck::cast_slice::<TextInstance, u8>(&glyphs_moved),
        bytemuck::cast_slice::<TextInstance, u8>(&glyphs_rebuilt),
        "a moved pool holds the grid glyphs a rebuild at its new origin holds"
    );
    assert_ne!(
        bytemuck::cast_slice::<TextInstance, u8>(&runs_at_rest),
        bytemuck::cast_slice::<TextInstance, u8>(&runs_rebuilt),
        "and the move has to change the run snap too"
    );
    assert_eq!(
        bytemuck::cast_slice::<TextInstance, u8>(&runs_moved),
        bytemuck::cast_slice::<TextInstance, u8>(&runs_rebuilt),
        "and the run glyphs a rebuild at its new origin holds"
    );
}

/// A pool riding another's glide drifts sub-cell every frame. The drift
/// travels in the shift rather than the origin, so the origin the reuse
/// gate keys on stands still.
///
/// This pins the half that makes the reuse safe. The shift never reaches
/// the instances, so the ones reused across a drift are the ones a rebuild
/// at that drift holds.
#[test]
fn a_sub_cell_drift_leaves_the_instances_a_rebuild_would_hold() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let mut grid = Grid::new(3, 20);
    fill_row(&mut grid, 0, "alpha");
    fill_row(&mut grid, 1, "bravo");

    let composite = |pass: &mut TextPass, shift, content_changed| {
        pass.prepare_composite(
            &device,
            &queue,
            &grid,
            PoolOccluders::new(&[], 0, true),
            resolution,
            shift,
            [2.0, 4.0],
            content_changed,
            None,
            0,
            0,
        );
        pass.composite_instances(0)
    };

    let at_rest = composite(&mut pass, 0.0, true);
    let drifted = composite(&mut pass, 0.375, false);
    let rebuilt = composite(&mut pass, 0.375, true);

    assert!(!rebuilt.is_empty(), "the fixture has to build glyphs");
    assert_eq!(
        bytemuck::cast_slice::<TextInstance, u8>(&drifted),
        bytemuck::cast_slice::<TextInstance, u8>(&rebuilt),
        "a drifted pool holds what a rebuild at that drift holds",
    );
    assert_eq!(
        bytemuck::cast_slice::<TextInstance, u8>(&at_rest),
        bytemuck::cast_slice::<TextInstance, u8>(&rebuilt),
        "because the shift rides the globals and never enters an instance",
    );
}

/// The chrome a pool's text runs back changes far more rarely than its
/// cells do, so a frame that moved neither the runs nor where they sit
/// builds none of them again.
///
/// Read off the build scratch, emptied between the two frames so a refill is
/// the only thing that can put anything back in it.
#[test]
fn a_composite_whose_runs_held_builds_none_of_them_again() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];
    let mut grid = Grid::new(3, 20);
    fill_row(&mut grid, 0, "alpha");
    let run = |col: i16| TextRun {
        col,
        row: 32,
        scale: 160,
        color: Rgb::new(255, 255, 255),
        bg: Some(Rgb::new(20, 20, 20)),
        follow: 0,
        anchor: None,
        text: "charlie".into(),
        seq: 0,
    };
    grid.set_text_runs(vec![run(32)]);

    let composite = |pass: &mut TextPass, grid: &Grid, scrolled| {
        pass.prepare_composite(
            &device,
            &queue,
            grid,
            PoolOccluders::new(&[], 0, true),
            resolution,
            0.0,
            [0.0; 2],
            true,
            scrolled,
            0,
            0,
        );
    };

    composite(&mut pass, &grid, None);
    assert!(
        !pass.composite_run_scratch.is_empty() && !pass.composite_rect_scratch.is_empty(),
        "the fixture has to build both run glyphs and a run rect",
    );

    pass.composite_run_scratch.clear();
    pass.composite_rect_scratch.clear();
    composite(&mut pass, &grid, None);
    assert!(
        pass.composite_run_scratch.is_empty(),
        "a run list that held must not be built again",
    );
    assert!(
        pass.composite_rect_scratch.is_empty(),
        "nor the rects backing it",
    );

    // A scroll leaves the runs where they were declared and moves the slot
    // that carries them there, so the same list has to be baked again.
    composite(&mut pass, &grid, Some(1));
    assert!(
        !pass.composite_run_scratch.is_empty(),
        "a rotation that moved the anchor has to bake the runs again",
    );

    pass.composite_run_scratch.clear();
    pass.composite_rect_scratch.clear();
    grid.set_text_runs(vec![run(48)]);
    composite(&mut pass, &grid, None);
    assert!(
        !pass.composite_run_scratch.is_empty() && !pass.composite_rect_scratch.is_empty(),
        "a run that moved has to be built again",
    );
}

/// An eviction hands the texels a pool's runs named to another glyph, so a
/// held run list is held only while the atlas has not moved under it.
#[test]
fn a_composite_bakes_its_runs_again_when_the_atlas_moved_under_them() {
    let (device, queue, mut pass) = headless_text_pass_font(160);
    let resolution = [640.0, 480.0];

    let mut grid = Grid::new(3, 10);
    fill_row(&mut grid, 0, "alpha");
    grid.set_text_runs(vec![TextRun {
        col: 0,
        row: 0,
        scale: 256,
        color: Rgb::new(255, 255, 255),
        bg: None,
        follow: 0,
        anchor: None,
        text: "42".into(),
        seq: 0,
    }]);

    let composite = |pass: &mut TextPass, grid: &Grid| {
        pass.prepare_composite(
            &device,
            &queue,
            grid,
            PoolOccluders::new(&[], 0, true),
            resolution,
            0.0,
            [0.0; 2],
            true,
            None,
            0,
            0,
        );
    };
    composite(&mut pass, &grid);

    // A later frame, so this pool's run glyphs are no longer protected, and
    // then more glyphs than the atlas grows to hold, so packing them evicts
    // rather than growing again.
    let before = pass.atlas.content_epoch();
    pass.atlas.begin_frame();
    let glyph = 128u32;
    let coverage = (glyph * glyph) as usize;
    for cp in 0..400u32 {
        pass.atlas
            .get_or_insert_procedural(&device, &queue, cp, glyph, glyph, || vec![255u8; coverage]);
    }
    assert_ne!(
        pass.atlas.content_epoch(),
        before,
        "the pack has to evict, or this proves nothing",
    );

    pass.composite_run_scratch.clear();
    composite(&mut pass, &grid);
    assert!(
        !pass.composite_run_scratch.is_empty(),
        "runs whose texels moved have to be baked again",
    );
}

/// A pool that reuses its instances still writes its own globals.
///
/// A globals slot is the pool's position in the frame rather than its id,
/// and pools enter a frame only while they glide, so which pool holds a slot
/// changes between frames. A reuse frame that wrote nothing leaves this pool
/// drawing against the rows, origin, and shift of whichever pool held the
/// slot before it.
#[test]
fn a_reusing_composite_still_writes_its_own_globals() {
    let (device, queue, mut pass) = headless_text_pass();
    let resolution = [640.0, 480.0];

    let mut mine = Grid::new(3, 8);
    fill_row(&mut mine, 0, "alpha");

    // The other pool's glyphs are a subset of this one's, so preparing it
    // packs nothing and leaves the atlas epoch the reuse gate reads.
    let mut theirs = Grid::new(5, 8);
    fill_row(&mut theirs, 0, "alpha");

    let composite = |pass: &mut TextPass, grid: &Grid, shift, origin, changed, pool| {
        pass.prepare_composite(
            &device,
            &queue,
            grid,
            PoolOccluders::new(&[], 0, true),
            resolution,
            shift,
            origin,
            changed,
            None,
            pool,
            0,
        );
    };

    composite(&mut pass, &mine, 0.0, [0.0; 2], true, 1);
    composite(&mut pass, &theirs, 0.25, [2.0, 4.0], true, 2);
    composite(&mut pass, &mine, -0.5, [0.0; 2], false, 1);

    let globals = pass.composite_globals[0].expect("slot 0 carries globals");
    assert_eq!(globals.rows, 3, "the reusing pool's own row count");
    assert_eq!(globals.origin_cells, [0.0; 2], "and its own region origin");
    assert_eq!(
        globals.scroll_y,
        -0.5 * pass.metrics.height,
        "and this frame's shift, which is the whole point of the frame",
    );
}

/// A pack that grows the atlas leaves the globals naming the grown size.
///
/// The instances built here are normalized by the atlas they resolved
/// against, so globals frozen at the pre-grow size draw the whole pool at
/// the wrong scale. The write before the reuse return cannot know this, so
/// the one after the pack is what corrects it.
#[test]
fn composite_globals_name_the_atlas_the_pack_grew_to() {
    let (device, queue, mut pass) = headless_text_pass_font(160);
    let mut grid = Grid::new(2, 4);
    grid.set_text_runs(vec![ascii_burst_run()]);

    let (before, _) = pass.atlas.texture_dims();
    pass.prepare_composite(
        &device,
        &queue,
        &grid,
        PoolOccluders::new(&[], 0, true),
        [640.0, 480.0],
        0.0,
        [0.0; 2],
        true,
        None,
        0,
        0,
    );

    let (mask, color) = pass.atlas.texture_dims();
    assert!(
        mask > before,
        "the burst has to grow the atlas mid-pack: {before} -> {mask}"
    );
    assert_eq!(
        pass.composite_globals[0]
            .expect("slot 0 carries globals")
            .atlas_size,
        [mask as f32, color as f32],
        "the globals must name the atlas the instances resolved against"
    );
}

/// The scroll below stays inside the overlay's single line of content.
///
/// A whole-line scroll over one line moves the window off the content
/// entirely, which `visible_lines` answers with an empty range, so the box
/// would correctly build nothing and the reshift path this pins would never
/// run.
#[test]
fn overlays_reshift_cached_bases_and_rebuild_on_content_change() {
    let (device, queue, mut pass) = headless_text_pass();
    let mut grid = Grid::new(6, 20);
    let overlay = |left| Overlay {
        top: 0,
        left,
        width: 6,
        height: 3,
        fill: Rgb::new(0, 0, 0),
        border: Rgb::new(0, 0, 0),
        content_fg: Rgb::new(255, 255, 255),
        scale: 1,
        offset: [0, 0],
        bold: false,
        content: "ab".to_owned(),
    };
    grid.set_overlays(vec![overlay(0)]);

    let resolution = [640.0, 480.0];
    let idle = Damage::Partial(vec![None; 6]);
    fn frame<'a>(idle: &'a Damage, popovers: &'a [f32]) -> Frame<'a> {
        Frame {
            cursor: None,
            cursor_corners: None,
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers,
            },
            damage: idle,
            decoration_damage: idle,
            scrolled_rows: 0,
            sketch_reveals: &[],
        }
    }

    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame(&idle, &[0.0]),
        &[],
        &[],
        &[],
    );
    assert_eq!(
        pass.overlay_count, 2,
        "one two-glyph overlay builds two instances"
    );
    assert_eq!(
        pass.overlay_draws.len(),
        1,
        "one overlay records one draw range"
    );
    let scissor = pass.overlay_draws[0].scissor;

    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame(&idle, &[0.0]),
        &[],
        &[],
        &[],
    );
    assert_eq!(
        pass.overlay_count, 2,
        "an unchanged frame reuses the cached bases"
    );
    assert_eq!(pass.overlay_draws.len(), 1);

    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame(&idle, &[0.5]),
        &[],
        &[],
        &[],
    );
    assert_eq!(
        pass.overlay_count, 2,
        "a scroll-only frame re-shifts rather than rebuilds"
    );
    assert_eq!(pass.overlay_draws.len(), 1);
    assert_eq!(
        pass.overlay_draws[0].scissor, scissor,
        "the scissor is derived from geometry, so scrolling leaves it unchanged"
    );

    grid.set_overlays(vec![overlay(0), overlay(10)]);
    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame(&idle, &[0.0, 0.0]),
        &[],
        &[],
        &[],
    );
    assert_eq!(
        pass.overlay_count, 4,
        "the added overlay rebuilds to four instances"
    );
    assert_eq!(
        pass.overlay_draws.len(),
        2,
        "each overlay records its own draw range"
    );
}

/// An overlay taller than its box builds the box's worth of instances, not the
/// content's.
///
/// Asserted by growing the content behind an unchanged box and demanding the
/// instance count not follow, which holds regardless of how wide the window
/// rounds. Then the box is scrolled to a middle line, where the instances must
/// be different ones, since a window that stayed at the top would also be
/// small and would otherwise pass.
#[test]
fn an_overlay_taller_than_its_box_builds_only_the_visible_window() {
    let (device, queue, mut pass) = headless_text_pass();

    let lines = |count: usize| {
        (0..count)
            .map(|line| char::from(b'a' + (line % 26) as u8).to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };
    let overlay = |content: String| Overlay {
        top: 0,
        left: 0,
        width: 4,
        height: 5,
        fill: Rgb::new(0, 0, 0),
        border: Rgb::new(0, 0, 0),
        content_fg: Rgb::new(255, 255, 255),
        scale: 1,
        offset: [0, 0],
        bold: false,
        content,
    };

    let resolution = [640.0, 480.0];
    let idle = Damage::Partial(vec![None; 40]);
    fn frame<'a>(idle: &'a Damage, popovers: &'a [f32]) -> Frame<'a> {
        Frame {
            cursor: None,
            cursor_corners: None,
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers,
            },
            damage: idle,
            decoration_damage: idle,
            scrolled_rows: 0,
            sketch_reveals: &[],
        }
    }

    // One grid across the three builds. A fresh one per call would start its
    // popovers epoch back at zero every time, so the pass would see an
    // unmoved epoch and keep the first call's shaped content, and a window
    // computed for the longer content would clamp to the stale line count.
    let mut grid = Grid::new(40, 20);
    let mut built = |pass: &mut TextPass, count: usize, scroll: f32| {
        grid.set_overlays(vec![overlay(lines(count))]);

        let popovers = [scroll];
        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&idle, &popovers),
            &[],
            &[],
            &[],
        );
        pass.overlay_instance_scratch
            .iter()
            .map(|instance| instance.pos[1])
            .collect::<Vec<_>>()
    };

    let short = built(&mut pass, 12, 0.0);
    let long = built(&mut pass, 400, 0.0);

    assert!(
        short.len() < 12,
        "a 5-row box reads fewer than the 12 lines behind it, not {}",
        short.len()
    );
    assert_eq!(
        long, short,
        "400 lines behind the same box build what 12 did"
    );

    // A window that ignored the scroll would sit at the top and be just as
    // small, so the instances have to have moved on as well as stayed few.
    //
    // Box-sized rather than equal to the unscrolled count. The window takes
    // a line of slack on each side for a glyph straddling an edge, and at
    // the top there is no line above the first to take, so an unscrolled
    // box reads one fewer than a scrolled one.
    let scrolled = built(&mut pass, 400, 200.0);
    let box_holds = 5 + 2;
    assert!(
        scrolled.len() <= box_holds,
        "the box reads a box's worth wherever it sits, not {}",
        scrolled.len()
    );
    assert!(
        scrolled.iter().all(|top| !short.contains(top)),
        "scrolled 200 lines down, the box draws different rows: {scrolled:?} \
             against {short:?}"
    );
}

#[test]
fn a_rescrolled_overlay_holds_only_this_frame_s_instances() {
    let (device, queue, mut pass) = headless_text_pass();
    let mut grid = Grid::new(6, 20);
    grid.set_overlays(vec![Overlay {
        top: 0,
        left: 0,
        width: 6,
        height: 3,
        fill: Rgb::new(0, 0, 0),
        border: Rgb::new(0, 0, 0),
        content_fg: Rgb::new(255, 255, 255),
        scale: 1,
        offset: [0, 0],
        bold: false,
        content: "ab".to_owned(),
    }]);

    let resolution = [640.0, 480.0];
    let idle = Damage::Partial(vec![None; 6]);
    fn frame<'a>(idle: &'a Damage, popovers: &'a [f32]) -> Frame<'a> {
        Frame {
            cursor: None,
            cursor_corners: None,
            scroll: Scroll {
                grid: 0.0,
                document: 0.0,
                scrollback: 0.0,
                region: 0.0,
                popovers,
            },
            damage: idle,
            decoration_damage: idle,
            scrolled_rows: 0,
            sketch_reveals: &[],
        }
    }
    let tops = |pass: &TextPass| {
        pass.overlay_instance_scratch
            .iter()
            .map(|instance| instance.pos[1])
            .collect::<Vec<_>>()
    };

    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame(&idle, &[0.0]),
        &[],
        &[],
        &[],
    );
    let unscrolled = tops(&pass);
    assert_eq!(
        unscrolled.len(),
        2,
        "the two-glyph overlay builds two instances"
    );

    // Three scroll frames in a row, so the buffers the second and third
    // build into are ones an earlier frame already filled. The offsets stay
    // within a cell, keeping the one line inside the box's window, since a
    // line scrolled out of the box builds no instances at all.
    for scroll in [0.25, 0.5, 0.75] {
        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&idle, &[scroll]),
            &[],
            &[],
            &[],
        );
    }

    assert_eq!(
        pass.overlay_count, 2,
        "a reused instance buffer rebuilds rather than accumulates"
    );
    assert_eq!(
        pass.overlay_draws.len(),
        1,
        "a reused draw buffer holds the one overlay's range"
    );
    assert_eq!(
        (pass.overlay_draws[0].start, pass.overlay_draws[0].count),
        (0, 2),
        "the range covers this frame's instances from the buffer's start"
    );

    let want: Vec<f32> = unscrolled
        .iter()
        .map(|top| top - 0.75 * pass.metrics.height)
        .collect();
    assert_eq!(
        tops(&pass),
        want,
        "the instances are the last scroll offset's, not an earlier frame's"
    );
}

#[test]
#[ignore = "timing benchmark; run with: cargo test -p stoatty_render --lib -- --ignored caches"]
fn caching_skips_reshaping_clean_rows() {
    let (device, queue, mut pass) = headless_text_pass();
    let (rows, cols) = (50, 200);
    let mut grid = Grid::new(rows, cols);
    for row in 0..rows {
        let text: String = (0..cols)
            .map(|col| char::from(b'a' + (col % 26) as u8))
            .collect();
        fill_row(&mut grid, row, &text);
    }

    // Warm the per-row cache and the atlas before timing.
    pass.rasterize_visible(
        &device,
        &queue,
        &grid,
        None,
        &Damage::Full,
        &Damage::Partial(Vec::new()),
    );

    let one_dirty = {
        let mut dirty = vec![None; rows];
        dirty[rows / 2] = whole_row(cols);
        Damage::Partial(dirty)
    };

    let iterations = 50;
    let full_start = std::time::Instant::now();
    for _ in 0..iterations {
        pass.rasterize_visible(
            &device,
            &queue,
            &grid,
            None,
            &Damage::Full,
            &Damage::Partial(Vec::new()),
        );
    }
    let full = full_start.elapsed();

    let dirty_start = std::time::Instant::now();
    for _ in 0..iterations {
        pass.rasterize_visible(
            &device,
            &queue,
            &grid,
            None,
            &one_dirty,
            &Damage::Partial(Vec::new()),
        );
    }
    let dirty = dirty_start.elapsed();

    eprintln!("rasterize_visible {rows}x{cols}: full {full:?}, one dirty row {dirty:?}");
    assert!(
            dirty * 2 < full,
            "rebuilding one of {rows} rows ({dirty:?}) should beat a full rebuild ({full:?}) by over 2x"
        );
}

#[test]
#[ignore = "timing benchmark; run with: cargo test -p stoatty_render --lib -- --ignored prepare_skips_unchanged_grid"]
fn prepare_skips_unchanged_grid() {
    let (device, queue, mut pass) = headless_text_pass();
    let (rows, cols) = (50, 200);
    let mut grid = Grid::new(rows, cols);
    for row in 0..rows {
        let text: String = (0..cols)
            .map(|col| char::from(b'a' + (col % 26) as u8))
            .collect();
        fill_row(&mut grid, row, &text);
    }
    let resolution = [1280.0, 800.0];
    let full_damage = Damage::Full;
    let idle_damage = Damage::Partial(vec![None; rows]);
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
        decoration_damage: &idle_damage,
        scrolled_rows: 0,
        sketch_reveals: &[],
    };

    // Warm the cache and atlas.
    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame(&full_damage),
        &[],
        &[],
        &[],
    );

    let iterations = 50;
    let full_start = std::time::Instant::now();
    for _ in 0..iterations {
        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&full_damage),
            &[],
            &[],
            &[],
        );
    }
    let full = full_start.elapsed();

    let idle_start = std::time::Instant::now();
    for _ in 0..iterations {
        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(&idle_damage),
            &[],
            &[],
            &[],
        );
    }
    let idle = idle_start.elapsed();

    eprintln!("prepare() {rows}x{cols}: full rebuild {full:?}, unchanged grid {idle:?}");
    assert!(
        idle * 4 < full,
        "an unchanged-grid frame ({idle:?}) should beat a full rebuild ({full:?}) by over 4x"
    );
}

#[test]
#[ignore = "timing measurement; run with: cargo test -p stoatty_render --lib -- --ignored cache_lookup_cost"]
fn cache_lookup_cost() {
    let (device, queue, mut pass) = headless_text_pass();
    let (rows, cols) = (50, 200);
    let mut grid = Grid::new(rows, cols);
    for row in 0..rows {
        let text: String = (0..cols)
            .map(|col| char::from(b'a' + (col % 26) as u8))
            .collect();
        fill_row(&mut grid, row, &text);
    }
    let resolution = [1280.0, 800.0];
    let idle_damage = Damage::Partial(vec![None; rows]);
    let frame = |scroll, damage| Frame {
        cursor: None,
        cursor_corners: None,
        scroll,
        damage,
        decoration_damage: &idle_damage,
        scrolled_rows: 0,
        sketch_reveals: &[],
    };
    let no_scroll = Scroll {
        grid: 0.0,
        document: 0.0,
        scrollback: 0.0,
        region: 0.0,
        popovers: &[],
    };

    // Warm the cache and atlas with a full build.
    pass.prepare(
        &device,
        &queue,
        &grid,
        resolution,
        &frame(no_scroll, &Damage::Full),
        &[],
        &[],
        &[],
    );

    // A changing scroll forces the full grid-glyph build -- every glyph's atlas
    // lookup -- each frame, but reshapes no row, isolating the cache lookups
    // from harfbuzz.
    let iterations = 100;
    let start = std::time::Instant::now();
    for i in 0..iterations {
        let scroll = Scroll {
            grid: i as f32 * 0.01,
            document: 0.0,
            scrollback: 0.0,
            region: 0.0,
            popovers: &[],
        };
        pass.prepare(
            &device,
            &queue,
            &grid,
            resolution,
            &frame(scroll, &idle_damage),
            &[],
            &[],
            &[],
        );
    }
    let per_call = start.elapsed() / iterations;

    eprintln!("grid build without reshape {rows}x{cols}: {per_call:?} per frame");
}

/// A color bitmap is rasterized at whatever size the font gives, which is
/// not the size of the cells it was placed in.
#[test]
fn an_oversize_bitmap_shrinks_into_its_box_and_centers() {
    // A 40x40 bitmap at the cell's origin, in a two-cell box 16 wide and 20
    // tall. Width runs out first, so it scales to 16x16 and centers down the
    // height it no longer fills.
    assert_eq!(
        fit_glyph_box([0.0, 0.0], [40.0, 40.0], [0.0, 0.0], [16.0, 20.0]),
        ([0.0, 2.0], [16.0, 16.0]),
    );
}

/// A bitmap smaller than its box is one the font supplied at that size, and
/// stretching it would blur a glyph that was already right.
#[test]
fn a_bitmap_that_fits_is_left_where_it_is() {
    let placed = ([3.0, 4.0], [8.0, 10.0]);

    assert_eq!(
        fit_glyph_box(placed.0, placed.1, [0.0, 0.0], [16.0, 20.0]),
        placed,
        "a narrow bitmap keeps its own placement",
    );
    assert_eq!(
        fit_glyph_box([0.0, 0.0], [16.0, 20.0], [0.0, 0.0], [16.0, 20.0]),
        ([0.0, 0.0], [16.0, 20.0]),
        "and one exactly the size of its box is untouched",
    );
}

/// Every zero here would divide, and a glyph with no extent has nothing to
/// fit anyway.
#[test]
fn nothing_with_no_extent_is_fitted() {
    let placed = ([1.0, 2.0], [0.0, 10.0]);
    assert_eq!(
        fit_glyph_box(placed.0, placed.1, [0.0, 0.0], [8.0, 8.0]),
        placed
    );

    let placed = ([1.0, 2.0], [10.0, 10.0]);
    assert_eq!(
        fit_glyph_box(placed.0, placed.1, [0.0, 0.0], [0.0, 8.0]),
        placed
    );
}

/// The box is the cells the glyph occupies, which for a wide character is
/// two across and always one down.
#[test]
fn a_wide_cell_box_is_two_across_and_one_down() {
    let metrics = CellMetrics::from_font_size(10, 1.0);
    let (single_origin, single) = cell_box(2, 3, 1.0, 1.0, metrics, [0.0, 0.0]);
    let (wide_origin, wide) = cell_box(2, 3, 1.0, 2.0, metrics, [0.0, 0.0]);

    assert_eq!(wide_origin, single_origin, "both start on the same cell");
    assert_eq!(wide[1], single[1], "and are the same height");
    assert!(
        (wide[0] - single[0] * 2.0).abs() <= 1.0,
        "the wide box spans two cells, within a pixel of snapping",
    );
}

/// The content is held off the ring on every side, so a line scrolled
/// against any edge stops short of it.
#[test]
fn a_scissor_insets_on_all_four_sides() {
    assert_eq!(
        inset_scissor([10, 20, 100, 50], 3),
        Some([13, 23, 94, 44]),
        "each origin moves in by the inset and each extent loses two of them",
    );
    assert_eq!(
        inset_scissor([0, 0, 8, 8], 3),
        Some([3, 3, 2, 2]),
        "a rect with exactly enough room keeps what is left",
    );
}

/// A box too small to hold anything once its frame is accounted for has no
/// content region, and clipping to nothing is not the same as not drawing.
#[test]
fn a_scissor_too_small_to_survive_the_inset_is_refused() {
    assert_eq!(inset_scissor([0, 0, 6, 40], 3), None, "width collapses");
    assert_eq!(inset_scissor([0, 0, 40, 6], 3), None, "height collapses");
    assert_eq!(
        inset_scissor([0, 0, 2, 2], 3),
        None,
        "and one smaller still"
    );
}

/// The ring's extent is physical, so the inset that clears it is a constant
/// rather than something scaled with the display.
#[test]
fn merging_the_scan_holds_the_text_band_and_drops_the_shaped_runs() {
    let (device, queue, mut pass) = bundled_text_pass();
    rasterize_rows(&mut pass, &device, &queue, &["hello"]);
    let band = pass.text_band();

    assert!(
        pass.run_shape_cache.cached_glyphs("hello").is_some(),
        "the run shaped against the bundled faces is cached",
    );

    pass.merge_fonts(font::scan_system_fonts(font::bundled_database()));

    assert_eq!(
        pass.text_band(),
        band,
        "the scan adds faces without moving the band the bundled family sets",
    );
    assert!(
        pass.run_shape_cache.cached_glyphs("hello").is_none(),
        "a run shaped before the scan reshapes against the full database",
    );
}

#[test]
fn the_ring_inset_covers_the_bands_outer_edge() {
    // The shader fades the border band out at border_px + 1.0, which is 2.5
    // pixels inside the box edge.
    assert!(
        f32::from(OVERLAY_RING_PX as u16) >= 2.5,
        "the inset must reach past where the ring stops fading",
    );
}
