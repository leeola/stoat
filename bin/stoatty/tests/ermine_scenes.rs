//! Renders stoat fixtures headlessly on the `ermine_night` theme and checks
//! that their frames show the Ermine colors.
//!
//! A scene is 160 by 50 cells at font 15, which fills 1440 by 900 pixels, the
//! size of an Ermine design board. A check reads the color that most pixels of
//! a region hold. A glyph covers less of a text cell than its background does,
//! so the dominant color of a cell is its background.
//!
//! The token constants restate the `ermine_night` theme block in config.stcfg.

use std::{
    collections::{HashMap, HashSet},
    ops::Range,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use stoatty_bin::{
    config,
    headless::{script, HeadlessSession, Scene, Snapshot},
};
use stoatty_render::render;
use tempfile::TempDir;

#[path = "support/scene.rs"]
mod support;

const COLS: u16 = 160;
const ROWS: u16 = 50;
const FONT_SIZE: u32 = 15;

/// The px a status row's elements draw above their grid row, which centers
/// them in the 24 px status band. The island stops twice this far above the
/// status row, where the band's ground starts.
const STATUS_ROW_PAD: u32 = 3;

/// The px the tab bar's elements draw below its grid row, which centers them
/// in the 32 px tab bar band. The island under the bar starts twice this far
/// below the bar's row, where the band ends.
const TAB_BAR_PAD: u32 = 7;

/// The px the active tab's pill stands outside the bar's row at the top and
/// the bottom inside the band, which makes the pill 22 px on the 18 px row.
const TAB_PILL_OUTSET: u32 = 2;

const ACCENT: [u8; 3] = [0x7f, 0xb0, 0xff];
const BASE: [u8; 3] = [0x17, 0x19, 0x1e];
const CHROME: [u8; 3] = [0x12, 0x14, 0x18];
const GROUND: [u8; 3] = [0x0c, 0x0e, 0x11];
const HOVER: [u8; 3] = [0x2c, 0x30, 0x39];
const OVERLAY: [u8; 3] = [0x25, 0x28, 0x30];
const RAISED: [u8; 3] = [0x1e, 0x21, 0x27];
const HATCH: [u8; 3] = [0x1b, 0x1d, 0x22];
const FOLD_BUTTON: [u8; 3] = [0x1f, 0x2a, 0x3d];
const TEXT: [u8; 3] = [0xdf, 0xe2, 0xe8];
const TEXT_DIM: [u8; 3] = [0xa3, 0xa9, 0xb5];
const HAIRLINE: [u8; 3] = [0x2e, 0x32, 0x3b];
const DIVIDER: [u8; 3] = [0x22, 0x25, 0x2b];
const STRONG: [u8; 3] = [0x3e, 0x43, 0x4e];
const DISABLED: [u8; 3] = [0x5b, 0x61, 0x6d];
const TINT_ACCENT: [u8; 3] = [0x22, 0x30, 0x49];
const DANGER: [u8; 3] = [0xf2, 0x77, 0x7f];
const SUCCESS: [u8; 3] = [0x8f, 0xd1, 0x8a];
const WASH: [u8; 3] = [0x2a, 0x3c, 0x5a];
const TAB_WASH: [u8; 3] = [0x24, 0x35, 0x52];
const CURSOR_SECONDARY: [u8; 3] = [0x8a, 0x90, 0x9c];
const READONLY_FRAME: [u8; 3] = [0x8a, 0x76, 0x50];
const TINT_OURS: [u8; 3] = [0x18, 0x24, 0x1b];
const TINT_THEIRS: [u8; 3] = [0x23, 0x1d, 0x2e];
const TINT_CONFLICT: [u8; 3] = [0x2a, 0x1b, 0x1d];
const MERGED_HEAD: [u8; 3] = [0x1b, 0x1d, 0x22];
const CHIP_CONFLICT: [u8; 3] = [0x33, 0x20, 0x1f];
const CONFLICT: [u8; 3] = [0xf5, 0x9a, 0xa0];
const TINT_MERGE: [u8; 3] = [0x2a, 0x1d, 0x21];
const INFO: [u8; 3] = [0x6c, 0xc7, 0xd6];
const MUTED: [u8; 3] = [0x83, 0x8a, 0x98];
const BLAME_SURFACE: [u8; 3] = [0x14, 0x16, 0x1a];
const BLAME_SELECTED: [u8; 3] = [0x1b, 0x22, 0x30];
const BLAME_AGE_1: [u8; 3] = [0x4f, 0x5a, 0x6e];
const BLAME_AGE_2: [u8; 3] = [0x5d, 0x79, 0xa8];
const BLAME_AGE_4: [u8; 3] = [0xa9, 0xc8, 0xff];
const TINT_WARNING: [u8; 3] = [0x2e, 0x2a, 0x1e];
const STRIP_TEXT: [u8; 3] = [0x3a, 0x3f, 0x49];

/// The primary cursor's opaque block, the terminal palette's cursor color. The
/// color comes from stoatty's own palette rather than a theme scope.
const CURSOR_PRIMARY: [u8; 3] = [0xdf, 0xe2, 0xe8];

/// The minimap's viewport band, the accent at 18 percent over the surface. The
/// band draws over the strip's runs, so a run under it takes the same 18
/// percent of the accent.
const BAND_FILL: [u8; 3] = [0x2a, 0x34, 0x47];

/// A diff strip's gray text under the viewport band.
const BAND_STRIP_TEXT: [u8; 3] = [0x46, 0x53, 0x6a];

/// [`SUCCESS`] under the viewport band, as an added Change Mark draws there.
const BAND_MARK_ADDED: [u8; 3] = [0x8c, 0xcb, 0x9f];

/// The wait for rust-analyzer to load the fixture crate and answer, the bound
/// the live LSP tests allow.
const LSP_DEADLINE: Duration = Duration::from_secs(90);

/// A pixel rectangle as `(x, y, width, height)`.
type PixelRect = (u32, u32, u32, u32);

/// The normal-mode chip fills with the accent, and a blank text cell fills
/// with the base surface.
///
/// The boards draw a 16 by 34 px chip on the 18 px row. Its box is the five
/// cells from column 1. The status row draws [`STATUS_ROW_PAD`] px up in its
/// band. The chip stands one pixel inside that row's top and bottom, five
/// inside the box's start, and six inside its end.
///
/// The checks read the pixel row or column on each side of each edge, ground
/// outside and accent inside. An edge one pixel off then fails a check. The
/// column checks read the rows between the chip's 4 px corners.
#[test]
fn the_basic_diff_scene_draws_on_the_ermine_palette() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");

    let (x, row_top, width, height) = cell_rect(2, ROWS - 1, 1, 1);
    let y = row_top - STATUS_ROW_PAD;
    expect_region(
        &shot,
        &mut home,
        (x, y + 1, width, height - 2),
        ACCENT,
        "mode_chip",
    );
    for (row, want, what) in [
        (y, GROUND, "mode_chip_top"),
        (y + 1, ACCENT, "mode_chip_top_row"),
        (y + height - 2, ACCENT, "mode_chip_bottom_row"),
        (y + height - 1, GROUND, "mode_chip_bottom"),
    ] {
        expect_region(&shot, &mut home, (x, row, width, 1), want, what);
    }

    let (start, ..) = cell_rect(1, ROWS - 1, 1, 1);
    let (end, ..) = cell_rect(6, ROWS - 1, 1, 1);
    for (column, want, what) in [
        (start + 4, GROUND, "mode_chip_start"),
        (start + 5, ACCENT, "mode_chip_first_column"),
        (end - 7, ACCENT, "mode_chip_last_column"),
        (end - 6, GROUND, "mode_chip_end"),
    ] {
        let rect = (column, y + 5, 1, height - 10);
        expect_region(&shot, &mut home, rect, want, what);
    }

    expect_region(
        &shot,
        &mut home,
        cell_rect(80, 11, 1, 1),
        BASE,
        "blank_text_cell",
    );
}

/// The modified row's git lanes sit left of its number, the hunk lane in the
/// modified color and the state lane in the unstaged color, one pixel apart.
///
/// The pane starts one column in, past the window's chrome margin. At 9-pixel
/// cells the hunk lane's 6/16 snaps to the first three pixel columns of that
/// cell, the boards' 3 px. The state lane starts one sixteenth later, which
/// leaves the fourth column on the base surface and takes the fifth to the
/// seventh. The tab bar's band takes rows 0 and 1, so the changed row is row
/// 5, `4 four changed`.
#[test]
fn the_gutter_draws_the_hunk_and_state_lanes_left_of_the_number() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    let (x, y, _, height) = cell_rect(1, 5, 1, 1);

    expect_region(&shot, &mut home, (x, y, 3, height), ACCENT, "hunk_lane");
    expect_region(&shot, &mut home, (x + 3, y, 1, height), BASE, "lane_gap");
    expect_region(
        &shot,
        &mut home,
        (x + 4, y, 3, height),
        DISABLED,
        "state_lane",
    );
}

/// A modified row's git bar reads removed on the base side and added on the
/// live side of the split diff, and the base side outlines the cell beside
/// the cursor.
///
/// The changed row is row 5, `4 four` on the left and `4 four changed` on the
/// right. A bar is too narrow to lead its cell's colors, so the check finds the
/// pixel columns the color fills over the row's whole height, which a glyph
/// never does.
///
/// The base side's removed lane stands 3 px in from the base half's left edge,
/// as the boards pad the base half by 3 px. It fills pixel columns 3 to 5 of
/// the pane's first cell, column 1.
///
/// The cursor opens on the row's first column, so the ghost outlines the base
/// text's first cell, column 7. Its dashed outline rounds its corners at 4 px
/// inside the cell, so the cell's top row holds none of its dim pixels, its
/// left side holds some, and the next cell holds none.
#[test]
fn a_modified_row_reads_removed_on_the_base_side_and_added_on_the_live_side() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    let (_, y, _, height) = cell_rect(0, 5, 1, 1);
    let [removed, added, modified] =
        [DANGER, SUCCESS, ACCENT].map(|color| bar_columns(&shot, (y, height), color));

    let apart = removed
        .last()
        .zip(added.first())
        .is_some_and(|(left, right)| left < right);
    if !apart || !modified.is_empty() {
        let frame = keep_frame(&shot, &mut home, "diff_git_bars");
        panic!(
            "the bars sit at {removed:?} removed, {added:?} added, and {modified:?} modified; the \
             frame is {}",
            frame.display(),
        );
    }

    let lane: Vec<u32> = {
        let (x, _, _, _) = cell_rect(1, 5, 1, 1);
        (x + 3..x + 6).collect()
    };
    if removed != lane {
        let frame = keep_frame(&shot, &mut home, "diff_removed_lane");
        panic!(
            "the removed lane fills pixel columns {removed:?}, not {lane:?} past the base \
             half's pad; the frame is {}",
            frame.display(),
        );
    }

    let row_dims = |col: u16, dy: u32| {
        let (x, _, width, _) = cell_rect(col, 5, 1, 1);
        pixels_of(&shot, (x, y + dy, width, 1), TEXT_DIM)
    };
    let side_dims = {
        let (x, _, _, _) = cell_rect(7, 5, 1, 1);
        pixels_of(&shot, (x, y, 1, height), TEXT_DIM)
    };
    let ghost = (row_dims(7, 0), side_dims > 0, row_dims(8, 1));
    if ghost != (0, true, 0) {
        let frame = keep_frame(&shot, &mut home, "diff_ghost_cursor");
        panic!(
            "the ghost's dim pixels read {ghost:?} on the cell top, its left side, and the next \
             cell; the frame is {}",
            frame.display(),
        );
    }
}

/// The split diff's two gutter lanes sit 2 px apart, as the DiffSplit board
/// draws them, where the plain editor's sit 1 px apart.
///
/// Row 5 is a modified row. The live side's gutter starts in cell 76, right of
/// the divider, where the added lane takes the cell's pixel columns 0 to 2 and
/// the unstaged state lane its columns 5 to 7. The base side's state lane
/// takes the same color, so the check reads the pixels of cell 76 alone.
#[test]
fn the_split_diff_s_gutter_lanes_sit_two_pixels_apart() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    let (x, y, width, height) = cell_rect(76, 5, 1, 1);
    let lanes = [SUCCESS, DISABLED].map(|color| {
        bar_columns(&shot, (y, height), color)
            .into_iter()
            .filter(|column| (x..x + width).contains(column))
            .map(|column| column - x)
            .collect::<Vec<_>>()
    });

    if lanes != [vec![0, 1, 2], vec![5, 6, 7]] {
        let frame = keep_frame(&shot, &mut home, "diff_gutter_lanes");
        panic!(
            "the live side's added and state lanes fill pixel columns {lanes:?} of cell 76, not \
             0 to 2 and 5 to 7; the frame is {}",
            frame.display(),
        );
    }
}

/// The base side of the split diff recedes as a whole behind the live side.
///
/// Row 7, `6 six`, is context on both sides. The left text starts at column 7
/// and the right at column 82. The base side paints the context color, which
/// alone reaches about two thirds of the live text, so only the fade brings the
/// left text's peak below three fifths of the right's.
#[test]
fn the_base_side_of_the_split_diff_recedes_behind_the_live_side() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    let base = brightest(&shot, cell_rect(7, 7, 5, 1));
    let live = brightest(&shot, cell_rect(82, 7, 5, 1));

    if u16::from(base) * 5 >= u16::from(live) * 3 {
        let frame = keep_frame(&shot, &mut home, "diff_base_side");
        panic!(
            "the base text peaks at {base} beside the live text's {live}; the frame is {}",
            frame.display(),
        );
    }
}

/// The split diff's status row sits on the window ground under both halves
/// and on the divider's cell, as the revised DiffSplit and DiffChanges boards
/// draw the footer with no fill behind its halves. The mode chip leads the
/// target half, which is the pane's own row, so the base half shows no chip.
///
/// Row 49 is the status row. Column 30 lies in the base half and column 130 in
/// the target half. Column 75 is the diff's divider, and every pixel of its
/// 9 px cell keeps the ground. The chip's box takes cells 76 to 80, and its
/// panel's fill shows in each of cells 77 to 80.
#[test]
fn the_split_diff_s_status_halves_sit_on_the_ground() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(30, 49, 1, 1),
        GROUND,
        "diff_status_base_half",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(130, 49, 1, 1),
        GROUND,
        "diff_status_target_half",
    );

    let rect = cell_rect(75, 49, 1, 1);
    let (_, _, width, height) = rect;
    let ground = pixels_of(&shot, rect, GROUND);
    if ground != (width * height) as usize {
        let frame = keep_frame(&shot, &mut home, "diff_status_divider");
        panic!(
            "{ground} of the {} pixels of the divider cell read {}, not all of them; the frame \
             is {}",
            width * height,
            hex(GROUND),
            frame.display(),
        );
    }

    let accent = |col: u16, cols: u16| pixels_of(&shot, cell_rect(col, 49, cols, 1), ACCENT);
    let chip = (77..=80).map(|col| accent(col, 1)).collect::<Vec<_>>();
    let base = accent(0, 75);
    if chip.contains(&0) || base > 0 {
        let frame = keep_frame(&shot, &mut home, "diff_status_chip");
        panic!(
            "the chip's fill reads {chip:?} pixels in cells 77 to 80 and {base} left of the \
             divider, not some in each and none left of it; the frame is {}",
            frame.display(),
        );
    }
}

/// The split diff draws no frame on its base half, as the DiffSplit board pads
/// the half by 3 px and draws no border around it.
///
/// Row 4, `3 three`, is context with no hunk mark. The pane's first cell,
/// column 1, reads the editor surface on its first three pixel columns. The
/// divider cell, column 75, holds the hairline on one pixel column and the
/// surface on the others.
#[test]
fn the_split_diff_draws_no_frame_on_its_base_half() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    let (x, y, _, height) = cell_rect(1, 4, 1, 1);
    let divider = cell_rect(75, 4, 1, 1);
    let got = [
        pixels_of(&shot, (x, y, 3, height), BASE),
        pixels_of(&shot, divider, HAIRLINE),
        pixels_of(&shot, divider, BASE),
    ];

    let (_, _, width, _) = divider;
    let want = [3 * height, height, (width - 1) * height].map(|pixels| pixels as usize);
    if got != want {
        let frame = keep_frame(&shot, &mut home, "diff_no_base_frame");
        panic!(
            "the pad's surface, the divider's hairline, and the divider's surface read {got:?} \
             pixels, not {want:?}; the frame is {}",
            frame.display(),
        );
    }
}

/// The split diff's mid divider draws in the hairline color, as the DiffSplit
/// board draws the rule between its halves.
///
/// Row 4, `3 three`, is context on both sides. The divider is column 75, and
/// its hairline takes the cell's pixel column 5. The check reads the row's
/// middle pixel row.
#[test]
fn the_split_diff_s_mid_divider_draws_in_the_hairline_color() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    let (x, y, _, height) = cell_rect(75, 4, 1, 1);
    let at = (((y + height / 2) * shot.width + x + 5) * 4) as usize;
    let pixel = [shot.rgba[at], shot.rgba[at + 1], shot.rgba[at + 2]];

    if pixel
        .into_iter()
        .zip(HAIRLINE)
        .any(|(got, want)| got.abs_diff(want) > 6)
    {
        let frame = keep_frame(&shot, &mut home, "diff_mid_divider");
        panic!(
            "the mid divider's pixel at x {} reads {}, not within 6 of {}; the frame is {}",
            x + 5,
            hex(pixel),
            hex(HAIRLINE),
            frame.display(),
        );
    }
}

/// The split diff ends in its buffer's minimap strip on the editor surface, as
/// a plain editor does.
///
/// The single band keeps its seven columns over the split diff, as
/// [`expect_strip_band`] reads them. The file's lines draw their text in the
/// strip's gray on the band's first row.
#[test]
fn the_split_diff_ends_in_the_minimap_strip_on_the_editor_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    expect_strip_band(&shot, &mut home, "diff_strip");
}

/// The changes view ends in its stack's minimap strip on the editor surface,
/// in the band the split diff ends in.
///
/// The band keeps its seven columns, as [`expect_strip_band`] reads them. The
/// stack's first file header draws its bar across the band's first row, and
/// the file's lines draw their text, all in the strip's gray.
#[test]
fn the_changes_view_ends_in_the_minimap_strip_on_the_editor_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>dc<Settle>",
    );
    expect_strip_band(&shot, &mut home, "changes_strip");
}

/// The split diff's strip draws a green Change Mark at least 12 px wide over
/// the changed part of a row, and no lane and no red, as the Minimap boards
/// draw a diff's strip.
///
/// The file's line 4 gains ` changed`, a modification that pairs with its
/// base line, so display row 3 marks the added text alone and no row marks a
/// removal. The strip's lines start at row 2's top at a 3 px pitch, so the
/// mark's full pixel rows are 9 and 10 below that top. The 12 px mark starts
/// at a fractional x, so it covers 11 pixels whole. The viewport band covers
/// the whole file, so the mark reads [`BAND_MARK_ADDED`]. The strip's third
/// and fourth pixel columns, where a lane drew, read the band's fill over the
/// file's lines.
#[test]
fn the_split_diff_s_strip_marks_its_changed_rows() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    let (x, y, width, height) = cell_rect(152, 2, 7, 2);

    let marked: Vec<u32> = (y..y + height)
        .filter(|&row| longest_run(&shot, (x, row, width), BAND_MARK_ADDED) >= 11)
        .map(|row| row - y)
        .collect();
    let red = (y..y + height)
        .flat_map(|row| (x..x + width).map(move |col| (col, row)))
        .filter(|&(col, row)| {
            let [red, green, _] = pixel(&shot, col, row);
            red >= green.saturating_add(0x30)
        })
        .count();
    let lane = (y + 2..=y + 18)
        .flat_map(|row| [x + 2, x + 3].map(|col| pixel(&shot, col, row)))
        .filter(|&color| color != BAND_FILL)
        .count();
    if (marked.as_slice(), red, lane) == (&[9, 10][..], 0, 0) {
        return;
    }

    let frame = keep_frame(&shot, &mut home, "diff_strip_marks");
    panic!(
        "the strip's pixel rows with 11 px of {} read {marked:?}, not [9, 10], with {red} \
         red pixels and {lane} lane pixels off {}; the frame is {}",
        hex(BAND_MARK_ADDED),
        hex(BAND_FILL),
        frame.display(),
    );
}

/// The single band rounds its viewport band's corners at 4 px and outlines
/// the band in the accent at 90 percent, as the Editor board draws the band.
///
/// The band's last cell is column 158 on row 2, where the thumb starts at the
/// file's top. The band's own island starts 4 px above that row, and its 6 px
/// top-left arc clips the thumb's top-left corner, so the check reads the
/// thumb's top-right corner. On the band's last pixel column, the thumb's top
/// pixel row lies past the arc and keeps the editor surface. The fifth pixel
/// row, the first past the 4 px arc, reads the outline, the accent at 90
/// percent over the band's color. Two pixels in, past the end of every strip
/// line, that row reads the band's color, the accent at 18 percent over the
/// surface.
#[test]
fn the_viewport_band_rounds_its_corners() {
    const BAND: [u8; 3] = [0x2a, 0x34, 0x47];
    const OUTLINE: [u8; 3] = [0x77, 0xa4, 0xed];

    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    let (x, y, width, _) = cell_rect(158, 2, 1, 1);
    let x = x + width - 1;

    expect_region(&shot, &mut home, (x, y, 1, 1), BASE, "band_corner");
    expect_region(
        &shot,
        &mut home,
        (x, y + 4, 1, 1),
        OUTLINE,
        "band_outline_past_the_arc",
    );
    expect_region(
        &shot,
        &mut home,
        (x - 2, y + 4, 1, 1),
        BAND,
        "band_inside_the_outline",
    );
}

/// The changes view heads each file with a row on the raised surface, between
/// two hairlines, and the divide's hairline runs through it.
///
/// Row 2 is the first file's header. Its counts end left of the divider,
/// column 75, and its path starts right of it, so the cells from column 2 to
/// column 31 hold no text, and the cells from column 10 to column 40 hold no
/// pixel of the text color. The hairlines take the top and bottom pixel rows
/// of the header row. The path's runs take no background, so the top hairline
/// runs unbroken over cells 77 to 86, where the path's name shows. The divide
/// takes one pixel column of the divider's cell.
///
/// The file's File Kind Tag reads `modified` on its box left of the counts.
/// The box is 14 sixteenths of the row tall and wider than its text, so the
/// cells left of the divider hold at least 14 by 30 pixels of its color.
#[test]
fn the_changes_view_heads_each_file_on_the_raised_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>dc<Settle>",
    );

    let (x, y, width, height) = cell_rect(2, 2, 30, 1);
    expect_region(
        &shot,
        &mut home,
        (x, y, width, height),
        RAISED,
        "changes_header",
    );
    expect_region(
        &shot,
        &mut home,
        (x, y, width, 1),
        HAIRLINE,
        "changes_header_top",
    );
    expect_region(
        &shot,
        &mut home,
        (x, y + height - 1, width, 1),
        HAIRLINE,
        "changes_header_bottom",
    );

    let counts_half_text = pixels_of(&shot, cell_rect(10, 2, 31, 1), TEXT);
    let path = cell_rect(77, 2, 10, 1);
    let (path_x, _, path_width, _) = path;
    let path_top = pixels_of(&shot, (path_x, y, path_width, 1), HAIRLINE);
    let path_text = pixels_of(&shot, path, TEXT);
    let divide = {
        let (cell_x, _, cell_width, _) = cell_rect(75, 2, 1, 1);
        bar_columns(&shot, (y, height), HAIRLINE)
            .into_iter()
            .filter(|column| (cell_x..cell_x + cell_width).contains(column))
            .count()
    };
    let tag_box = pixels_of(&shot, cell_rect(1, 2, 74, 1), FOLD_BUTTON);
    if counts_half_text > 0
        || path_top != path_width as usize
        || path_text == 0
        || divide != 1
        || tag_box < 14 * 30
    {
        let frame = keep_frame(&shot, &mut home, "changes_header_layout");
        panic!(
            "the header reads {counts_half_text} text pixels in cells 10 to 40, {path_top} of \
             {path_width} hairline pixels over the path, {path_text} text pixels in the path's \
             cells, {divide} divide columns in cell 75, and {tag_box} pixels of the tag's box \
             left of the divider; the frame is {}",
            frame.display(),
        );
    }
}

/// The changes view pins the File Header of the file under the stack's top
/// edge to the stack's first row while the stack scrolls under it.
///
/// The scene is 22 rows tall, and the tab bar's band and the status band take
/// two rows each, so the stack shows 18. The two deleted files above
/// hunks.txt fold to their headers, so hunks.txt takes stack rows 5 to 19 of
/// 35. Two pages put the cursor on the stack's last row and the scroll on the
/// last page, row 17, which lies inside hunks.txt, past its header and before
/// its last row. The stack lists the staged move renamed-to.txt after
/// hunks.txt, so its header row and fold row add to the page. Row 2 then shows hunks.txt's header
/// on the raised surface over its bottom hairline. Where the cover fails, the resident pool's
/// composite shows a stack row there.
///
/// The pinned header casts its shadow on row 3, which the stack's pool
/// composites. A strip 2 px under that row's top edge takes about 0.38 of
/// black and a strip 16 px under it none. The two strips share the row's
/// surface, so a diff tint or a hatch there cancels out.
#[test]
fn the_changes_view_pins_the_file_header_over_its_scrolling_stack() {
    let mut home = support::scratch_home();
    let mut session = spawn_scene(Scene {
        rows: 22,
        ..ermine_scene("diff-kinds", "hunks.txt", home.path())
    });
    play(&mut session, "<Space>dc<Settle><C-f><C-f><Settle>");
    let shot = session.snapshot().expect("a snapshot");
    drop(session);

    let (x, y, width, height) = cell_rect(2, 2, 30, 1);
    expect_region(
        &shot,
        &mut home,
        (x, y, width, height),
        RAISED,
        "pinned_header",
    );
    expect_region(
        &shot,
        &mut home,
        (x, y + height - 1, width, 1),
        HAIRLINE,
        "pinned_header_bottom",
    );

    let (x, y, width, _) = cell_rect(20, 3, 12, 1);
    let [near, far] = [2, 16].map(|down| dominant_color(&shot, (x, y + down, width, 1)));
    if !near.iter().zip(far).all(|(&near, far)| near < far) {
        let frame = keep_frame(&shot, &mut home, "pinned_header_shadow");
        panic!(
            "the row under the pinned header reads {} 2 px under its top edge and {} 16 px \
             under it, not darker near the header; the frame is {}",
            hex(near),
            hex(far),
            frame.display(),
        );
    }
}

/// The changes view folds the unchanged lines between two hunks into a fold
/// row under the boards' hatch, and the divide's hairline runs through it.
///
/// The diff-kinds stack lists hunks.txt fourth, after the two deleted files,
/// which fold to their headers. Its fold row is row 14, under the row of its
/// line 6 on the base surface. The fold hides seven
/// lines, so the row carries one Fold Button, whose box covers most of columns
/// 1 and 2. The count and the range end against the divide, so the count's
/// muted text shows in the cells from column 61 to column 74. The divide takes
/// one pixel column of the divider's cell, column 75. A text file's language
/// has no outline query, so the row names no Scope. The cells from column 4 to
/// column 60, and from column 77 to column 148 right of the divide, hold no
/// text. They take the base surface under the hatch's lines, which the stack's
/// resident pool composites from its pages.
#[test]
fn the_changes_view_folds_unchanged_lines_into_a_hatched_row() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "diff-kinds", "hunks.txt", "<Space>dc<Settle>");

    expect_region(
        &shot,
        &mut home,
        cell_rect(2, 13, 30, 1),
        BASE,
        "changes_row_above_fold",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(1, 14, 2, 1),
        FOLD_BUTTON,
        "changes_fold_button",
    );
    for (rect, what) in [
        (cell_rect(4, 14, 57, 1), "changes_fold_row"),
        (cell_rect(77, 14, 72, 1), "changes_fold_row_right"),
    ] {
        expect_hatch(&shot, &mut home, rect, (BASE, HATCH), what);
    }

    let (_, y, _, height) = cell_rect(0, 14, 1, 1);
    let count = pixels_of(&shot, cell_rect(61, 14, 14, 1), MUTED);
    let divide = {
        let (cell_x, _, cell_width, _) = cell_rect(75, 14, 1, 1);
        bar_columns(&shot, (y, height), HAIRLINE)
            .into_iter()
            .filter(|column| (cell_x..cell_x + cell_width).contains(column))
            .count()
    };
    if count == 0 || divide != 1 {
        let frame = keep_frame(&shot, &mut home, "changes_fold_row_divide");
        panic!(
            "the fold row reads {count} muted pixels in cells 61 to 74 and {divide} divide \
             columns in cell 75; the frame is {}",
            frame.display(),
        );
    }
}

/// The panes stand 8 px in from each window edge, as every board draws its
/// island, and a blank text cell inside them stays on the base surface.
///
/// Each margin column is 9 px wide, and the island reaches 1 px into it. On
/// row 4, pixel columns 0 to 7 keep the ground and column 8 takes the
/// base surface. On the right the minimap band's own island reaches pixel
/// column 1431, the margin column's first, so 1432 to 1439 keep the ground.
#[test]
fn the_panes_sit_eight_pixels_in_from_each_window_edge() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    expect_region(
        &shot,
        &mut home,
        cell_rect(80, 4, 1, 1),
        BASE,
        "blank_text_cell",
    );

    let (_, y, _, height) = cell_rect(0, 4, 1, 1);
    let row = |from: u32, to: u32| {
        (from..to)
            .map(|x| pixel(&shot, x, y + height / 2))
            .collect::<Vec<_>>()
    };
    let (left, right) = (row(0, 9), row(1431, 1440));
    let want = (
        [vec![GROUND; 8], vec![BASE]].concat(),
        [vec![BASE], vec![GROUND; 8]].concat(),
    );
    if (left.clone(), right.clone()) != want {
        let frame = keep_frame(&shot, &mut home, "window_margins");
        panic!(
            "row 4 reads {:?} at pixel columns 0 to 8 and {:?} at 1431 to 1439, not the ground \
             up to the island's edge 8 px in; the frame is {}",
            left.into_iter().map(hex).collect::<Vec<_>>(),
            right.into_iter().map(hex).collect::<Vec<_>>(),
            frame.display(),
        );
    }
}

/// Two panes side by side stand 8 px apart, as the StartSplit board draws
/// them.
///
/// The gap column, 75, is 9 px wide. The left island reaches its first pixel
/// column, 675, and the right island none of it, so 676 to 683 keep the
/// ground. Pixel row 189 lies on row 10, past the file's lines, and the check
/// reads no pixel of the right pane's own cells, where a changed row's gutter
/// lane draws.
#[test]
fn side_by_side_panes_stand_eight_pixels_apart() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>as<Settle>",
    );
    let gap: Vec<[u8; 3]> = (675..684).map(|x| pixel(&shot, x, 189)).collect();

    if gap != [vec![BASE], vec![GROUND; 8]].concat() {
        let frame = keep_frame(&shot, &mut home, "side_by_side_gap");
        panic!(
            "pixel row 189 reads {:?} across the gap column, not the left island's last pixel \
             and then the ground; the frame is {}",
            gap.into_iter().map(hex).collect::<Vec<_>>(),
            frame.display(),
        );
    }
}

/// Two stacked panes meet at the upper pane's status row, whose 24 px band is
/// the gap between them, as the PaneStatus board stacks its islands.
///
/// The band takes the last 6 px of the spare row 24 and all of the status row
/// 25, from pixel row 444 to 467. The upper island stops at pixel row 443, and
/// the lower island starts at 468. Pixel column 720 lies in the middle of both
/// panes, where the row holds no text. Down that column, pixel row 443 reads
/// the base surface, the band's 24 pixel rows read the ground, and 468 reads
/// the base surface again. The upper pane's name draws in the status row's
/// first cells.
#[test]
fn stacked_panes_meet_at_the_upper_pane_s_status_row() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>av<Settle>",
    );
    let column: Vec<[u8; 3]> = (443..469).map(|y| pixel(&shot, 720, y)).collect();
    let name = brightest(&shot, cell_rect(1, 25, 12, 1));

    if column != [vec![BASE], vec![GROUND; 24], vec![BASE]].concat() || name <= 0x80 {
        let frame = keep_frame(&shot, &mut home, "stacked_status_row");
        panic!(
            "pixel column 720 reads {:?} down pixel rows 443 to 468, not the upper island's \
             base, the ground down the status band, and then the lower island's base, or the \
             row's first cells peak at {name:#04x}, not the pane's name; the frame is {}",
            column.into_iter().map(hex).collect::<Vec<_>>(),
            frame.display(),
        );
    }
}

/// An island's corners round at 6 px against the ground, as every board draws
/// a pane.
///
/// `<Space>av` stacks two panes. Each island starts at pixel column 8, a pixel
/// into the margin. The upper island starts at pixel row 32, where the tab
/// bar's band ends, so its top-left arc centers on (14, 38). The lower island
/// starts at pixel row 468, under the upper pane's status band, so its arc
/// centers on (14, 474). Each arc leaves the pixel at its island's top-left
/// corner and the pixel right of it on the ground. 8 px along, the top edge
/// runs straight, with the ground on the pixel row over it and the base
/// surface on its first pixel row. The pixel 8 px down the left edge reads the
/// base surface too.
///
/// The lower arc lies inside the lower pane's first cell, where only the
/// ground the island paints over its corner clears the cell.
#[test]
fn an_island_rounds_its_corners_against_the_ground() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>av<Settle>",
    );
    let corners = [32, 468].map(|top| {
        [(8, top), (9, top), (16, top - 1), (16, top), (8, top + 8)]
            .map(|(x, y)| pixel(&shot, x, y))
    });

    if corners != [[GROUND, GROUND, GROUND, BASE, BASE]; 2] {
        let frame = keep_frame(&shot, &mut home, "island_corner");
        panic!(
            "the islands' top-left corners read {:?} at (8, top), (9, top), (16, top - 1), \
             (16, top), and (8, top + 8) for the tops 32 and 468, not the ground outside each \
             island and the base surface inside it; the frame is {}",
            corners.map(|corner| corner.map(hex)),
            frame.display(),
        );
    }
}

/// Each pane's island draws no band inside its bottom edge and shades nothing
/// on the margin beside it, as every revised board draws its panes.
///
/// The island reaches 1 px into the one-column margin and stops 8 px from the
/// window's edge, so pixel column 3 keeps the ground. Row 47 is the pane's
/// last content row. The island reaches 12 px into row 48, the spare row of
/// the status band, and stops 6 px above the status row. Every pixel of cell
/// 80 from row 47's top down to that edge, far from the island's 6 px corner
/// arcs, reads the base surface. The pixel row under the edge keeps the
/// ground.
#[test]
fn the_island_draws_no_band_inside_its_bottom_edge() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    let (_, y, _, height) = cell_rect(0, 4, 1, 1);

    expect_region(
        &shot,
        &mut home,
        (3, y, 1, height),
        GROUND,
        "margin_beside_the_pane",
    );

    let (x, top, width, _) = cell_rect(80, ROWS - 3, 1, 1);
    let (_, status_top, ..) = cell_rect(80, ROWS - 1, 1, 1);
    let height = status_top - 2 * STATUS_ROW_PAD - top;
    let base = pixels_of(&shot, (x, top, width, height), BASE);
    let ground = pixels_of(&shot, (x, top + height, width, 1), GROUND);
    if base != (width * height) as usize || ground != width as usize {
        let frame = keep_frame(&shot, &mut home, "bottom_edge");
        panic!(
            "{base} of the {} pixels of cell 80 from the pane's last row down to the island's \
             bottom edge read {}, and {ground} of the {width} under that edge read {}, not all \
             of them; the frame is {}",
            width * height,
            hex(BASE),
            hex(GROUND),
            frame.display(),
        );
    }
}

/// The status row sits on the ground, and its hairline takes the bar's own
/// color, so no line shows above the bar. Its branch glyph draws as a glyph.
///
/// The hairline is the top pixel row of the status row, which draws
/// [`STATUS_ROW_PAD`] px up in its band. The check reads it over a blank cell
/// of the bar. The branch glyph, U+2387, draws in column 109, the
/// first glyph cell of the right-aligned repo segment. A glyph's strokes blend
/// through many colors there, and the empty box that a missing glyph draws
/// keeps to a few along its straight edges.
#[test]
fn the_status_row_sits_on_the_ground_with_no_hairline() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    let (x, y, width, _) = cell_rect(80, ROWS - 1, 1, 1);

    let colors = distinct_colors(&shot, cell_rect(109, ROWS - 1, 1, 1));
    if colors <= 12 {
        let frame = keep_frame(&shot, &mut home, "branch_glyph");
        panic!(
            "the branch glyph's cell holds {colors} colors, as few as an empty box draws; the \
             frame is {}",
            frame.display(),
        );
    }

    expect_region(
        &shot,
        &mut home,
        cell_rect(20, ROWS - 1, 1, 1),
        GROUND,
        "status_text_cell",
    );
    expect_region(
        &shot,
        &mut home,
        (x, y - STATUS_ROW_PAD, width, 1),
        GROUND,
        "status_hairline",
    );
}

/// The tab bar sits on the ground over a lone tab, and the active tab draws as
/// a 22 px pill in the accent wash, with its corners rounded at 4 px.
///
/// The bar leads with the workspace name, whose length follows the session's
/// random name, so the check reads row 0 as runs of color rather than at a
/// fixed column. The bar's row draws [`TAB_BAR_PAD`] px down in its 32 px
/// band, and the pill stands [`TAB_PILL_OUTSET`] px outside that row at the
/// top and the bottom, from pixel row 5 to 26. The tab's left edge is the
/// first wash pixel of the row's middle, where the edge runs straight. Pixel
/// column 8 of the tab lies in its pad cell, past the arc. It reads the ground
/// on the pixel rows over and under the pill, and the wash on the pill's first
/// and last pixel rows. On the pill's second pixel row the arc takes the wash
/// out of the tab's first pixel column.
#[test]
fn the_tab_bar_sits_on_the_ground_with_the_active_tab_in_a_pill() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "many-files", "src/alpha.txt", "");

    expect_row_runs(&shot, &mut home, 0, &[GROUND, TAB_WASH, GROUND], "tab_bar");

    let (_, row_top, _, height) = cell_rect(0, 0, 1, 1);
    let middle = row_top + TAB_BAR_PAD + height / 2;
    let Some(left) = (0..shot.width).find(|&x| pixel(&shot, x, middle) == TAB_WASH) else {
        let frame = keep_frame(&shot, &mut home, "tab_bar_middle");
        panic!(
            "no pixel of the bar's middle pixel row {middle} reads {}; the frame is {}",
            hex(TAB_WASH),
            frame.display(),
        );
    };

    let top = row_top + TAB_BAR_PAD - TAB_PILL_OUTSET;
    let bottom = row_top + TAB_BAR_PAD + height + TAB_PILL_OUTSET - 1;
    let column = [top - 1, top, bottom, bottom + 1].map(|y| pixel(&shot, left + 8, y));
    let corner = pixel(&shot, left, top + 1);
    if column != [GROUND, TAB_WASH, TAB_WASH, GROUND] || corner == TAB_WASH {
        let frame = keep_frame(&shot, &mut home, "tab_pill");
        panic!(
            "the tab's pixel column {} reads {:?} on pixel rows {}, {top}, {bottom}, and {}, not \
             the ground, the wash, the wash, and the ground, or its first column's second pixel \
             reads {}, not rounded off; the frame is {}",
            left + 8,
            column.map(hex),
            top - 1,
            bottom + 1,
            hex(corner),
            frame.display(),
        );
    }
}

/// The split diff's tab bar ends in its layout chips as the DiffSplit board
/// draws them. The focused `files` chip is a box on the hover surface one
/// pixel inside the bar's row, and the `changes` chip is text on the ground.
///
/// The bar's row draws [`TAB_BAR_PAD`] px down in its band, so the checks read
/// the row there. The bar ends in a pad cell at column 159, so the `changes`
/// chip takes columns 150 to 158 and the `files` chip columns 143 to 149.
#[test]
fn the_tab_bar_boxes_the_focused_layout_s_chip_inside_its_row() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    let (_, row_top, _, height) = cell_rect(0, 0, 1, 1);
    let top = row_top + TAB_BAR_PAD;
    let band = |col: u16, (dy, rows): (u32, u32)| {
        let (x, _, width, _) = cell_rect(col, 0, 1, 1);
        dominant_color(&shot, (x, top + dy, width, rows))
    };

    let files: Vec<[[u8; 3]; 3]> = (143..150)
        .map(|col| [(0, 1), (1, height - 2), (height - 1, 1)].map(|rows| band(col, rows)))
        .collect();
    let changes: Vec<[u8; 3]> = (150..159).map(|col| band(col, (0, height))).collect();
    if files != [[GROUND, HOVER, GROUND]; 7] || changes != [GROUND; 9] {
        let frame = keep_frame(&shot, &mut home, "tab_bar_layout_chips");
        panic!(
            "the files chip's columns read {:?} on their first, middle, and last pixel rows, \
             and the changes chip's columns read {:?}; the frame is {}",
            files.iter().map(|rows| rows.map(hex)).collect::<Vec<_>>(),
            changes.iter().map(|&color| hex(color)).collect::<Vec<_>>(),
            frame.display(),
        );
    }
}

/// The finder's list and preview Islands stand 8 px apart.
///
/// The list ends before cell 67, the separator column, and the preview starts
/// at cell 68. Cell row 25 is blank in both panes, and on its middle pixel row
/// the two Islands stand 8 px apart across the separator.
#[test]
fn the_finder_islands_stand_eight_pixels_apart() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>p<Settle>",
    );

    expect_islands_apart(&shot, &mut home, 67, 25, "finder_islands");
}

/// The finder plate casts the boards' modal shadow, `0 18px 40px
/// rgba(0,0,0,0.6)`, which reaches past 60 px under the plate.
///
/// The box's modal rect is the 128 by 32 cells from cell (16, 9), and the
/// plate's bottom edge stands 10 px into its last row, cell row 40. The pixel
/// 30 px under that edge at the plate's middle column lies 12 px past the
/// shadow rect's bottom, where the blur still lays about a sixth of black over
/// the editor surface.
#[test]
fn the_file_finder_box_casts_the_modal_shadow() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>p<Settle>",
    );

    let (x, _, width, _) = cell_rect(16, 9, 128, 32);
    let (_, last_row, _, _) = cell_rect(16, 40, 1, 1);
    let plate_bottom = last_row + 10;
    let under = dominant_color(&shot, (x + width / 2, plate_bottom + 30, 1, 1));
    if under.iter().zip(BASE).any(|(&found, base)| found >= base) {
        let frame = keep_frame(&shot, &mut home, "finder_shadow");
        panic!(
            "the pixel 30 px under the finder plate reads {}, not darker than {} in every \
             channel; the frame is {}",
            hex(under),
            hex(BASE),
            frame.display(),
        );
    }
}

/// The file finder's box is the boards' ground plate, `background: #0c0e11`,
/// with no hairline, and its preview pane sits on the base surface.
///
/// The box's modal rect is the 128 by 32 cells from cell (16, 9), so its inner
/// rect runs from cell (17, 10) up to cell (143, 40). The plate's top is the top
/// of the header row, cell row 10. Its sides stand 11 px outside the inner
/// rect, and its bottom stands 10 px into the modal rect's last row, cell row
/// 40. Each edge reads one pixel on each side, the ground inside and the
/// shadowed panes outside. The header row reads the ground where no text
/// covers it, and no pixel along the plate's sides takes the hairline color.
///
/// The base cell sits in the preview, under the six lines of the file it shows.
///
/// The rule under the preview's path header runs along the last pixel row of
/// cell row 12, the header's first row, in the divider color, as a rule on the
/// editor surface draws. The pixel row above it reads the base surface.
///
/// The preview numbers its lines in a column from cell 68. On the first row,
/// cell row 14, the column's first four cells hold the number in the muted
/// color and no text, so they peak above the base surface and at most at the
/// muted color. Cell row 21 is the first row past the file's seven lines, and
/// its column holds no number.
#[test]
fn the_file_finder_box_draws_as_the_ground_plate() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>p<Settle>",
    );

    let (left, top, _, _) = cell_rect(17, 10, 1, 1);
    let (right, last_row, _, _) = cell_rect(143, 40, 1, 1);
    let (left, right, bottom) = (left - 11, right + 11, last_row + 10);
    let (middle_x, middle_y) = ((left + right) / 2, (top + bottom) / 2);
    let edges = [
        ("left", (left - 1, middle_y), (left, middle_y)),
        ("right", (right, middle_y), (right - 1, middle_y)),
        ("top", (middle_x, top - 1), (middle_x, top)),
        ("bottom", (middle_x, bottom), (middle_x, bottom - 1)),
    ];
    for (side, (outside_x, outside_y), (inside_x, inside_y)) in edges {
        let outside = pixel(&shot, outside_x, outside_y);
        let inside = pixel(&shot, inside_x, inside_y);
        if outside == GROUND || inside != GROUND {
            let frame = keep_frame(&shot, &mut home, "finder_plate");
            panic!(
                "the plate's {side} edge reads {} outside and {} inside, where only the \
                 inside belongs to the ground {}; the frame is {}",
                hex(outside),
                hex(inside),
                hex(GROUND),
                frame.display(),
            );
        }
    }

    expect_region(
        &shot,
        &mut home,
        cell_rect(60, 10, 20, 1),
        GROUND,
        "finder_header",
    );
    let sides = [
        (left, top, 9, bottom - top),
        (right - 9, top, 9, bottom - top),
    ];
    let hairline: usize = sides
        .into_iter()
        .map(|side| pixels_of(&shot, side, STRONG))
        .sum();
    if hairline > 0 {
        let frame = keep_frame(&shot, &mut home, "finder_hairline");
        panic!(
            "{hairline} pixels on the plate's sides take the hairline color {}; the frame is {}",
            hex(STRONG),
            frame.display(),
        );
    }

    expect_region(
        &shot,
        &mut home,
        cell_rect(111, 27, 1, 1),
        BASE,
        "finder_preview",
    );

    let (x, y, width, height) = cell_rect(111, 12, 1, 1);
    expect_region(
        &shot,
        &mut home,
        (x, y + height - 2, width, 1),
        BASE,
        "preview_header_over_rule",
    );
    expect_region(
        &shot,
        &mut home,
        (x, y + height - 1, width, 1),
        DIVIDER,
        "preview_header_rule",
    );

    let peak = |color: [u8; 3]| color.into_iter().max().expect("a color has channels");
    let first = brightest(&shot, cell_rect(68, 14, 4, 1));
    let past_end = brightest(&shot, cell_rect(68, 21, 5, 1));
    if !(peak(BASE) + 1..=peak(MUTED)).contains(&first) || past_end != peak(BASE) {
        let frame = keep_frame(&shot, &mut home, "preview_numbers");
        panic!(
            "the number column peaks at {first:#04x} on the first row and at {past_end:#04x} \
             past the file's end; the frame is {}",
            frame.display(),
        );
    }
}

/// The Pane Picker draws its badge as the board draws it.
///
/// The badge is an accent box of 9 by 6 cells with 10 px corners and a 2 px
/// border in the window ground, and a shadow falls under it.
///
/// `<Space>as` splits the window and moves the focus to the right pane, so the
/// one badge sits in the left pane. That pane takes columns 1 to 74, and rows
/// 2 to 47 between the tab bar's band and its status band, so the box centers
/// with its top-left cell at (33, 22).
/// The cell one in from that corner lies inside the box and left of the digit.
///
/// The pixel on the box's left edge four rows down lies outside the 10 px
/// arc, so it shows neither the fill nor the border. The pixel 40 px along the
/// top edge and one row down lies on the border. The shadow falls 10 px down,
/// so the blank pane 14 px under the box's middle darkens, and the pane 14 px
/// over it keeps the base.
#[test]
fn the_pane_picker_badge_draws_the_accent_box() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>as<Settle><Space>ae<Settle>",
    );
    let (col, row) = (33, 22);

    expect_region(
        &shot,
        &mut home,
        cell_rect(col + 1, row + 1, 1, 1),
        ACCENT,
        "pane_badge_fill",
    );

    let (x, y, width, height) = cell_rect(col, row, 9, 6);
    let pixel = |x: u32, y: u32| dominant_color(&shot, (x, y, 1, 1));
    let (corner, border) = (pixel(x, y + 4), pixel(x + 40, y + 1));
    let middle = x + width / 2;
    let (under, over) = (pixel(middle, y + height + 14), pixel(middle, y - 14));
    let shaded = under
        .iter()
        .zip(BASE)
        .all(|(&channel, base)| channel < base);
    if [ACCENT, CHROME].contains(&corner) || border != CHROME || !shaded || over != BASE {
        let frame = keep_frame(&shot, &mut home, "pane_badge_chrome");
        panic!(
            "pane_badge_chrome: the corner pixel reads {}, the border pixel {}, and the pixels \
             under and over the box {} and {}; want a corner on neither the fill nor the \
             border, a {} border, a shadow darker than {} in each channel under the box, and \
             {} over it; the frame is {}",
            hex(corner),
            hex(border),
            hex(under),
            hex(over),
            hex(CHROME),
            hex(BASE),
            hex(BASE),
            frame.display(),
        );
    }
}

/// The which-key box docks on the status bar as a card on the raised surface.
///
/// The card's cells end above the two rows of the status band, so its footer
/// row is three rows above the status row. The cell two columns left of the
/// window's right edge sits in that footer row.
#[test]
fn the_space_box_docks_as_a_card_on_the_raised_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "<Space><Settle>");

    expect_region(
        &shot,
        &mut home,
        cell_rect(157, 46, 1, 1),
        RAISED,
        "hints_card",
    );
}

/// The commits view lists the rebase fixture's history under the graph
/// header's row, with the lane graph left of its rows. The header row stays on
/// the base. A blank cell on the selected first row, the row under the header,
/// fills with the tab wash, and one two rows below stays on the base.
///
/// The selected commit adds `ast.txt`, so the preview's one row under its label
/// is an added line, tinted whole past its text.
#[test]
fn the_commits_view_selects_its_first_row_beside_the_lane_graph() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "rebase",
        "base.txt",
        "<C-;>OpenCommits<Enter><Settle>",
    );

    expect_region(
        &shot,
        &mut home,
        cell_rect(30, 2, 1, 1),
        BASE,
        "graph_header_row",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(30, 3, 1, 1),
        TAB_WASH,
        "selected_commit_row",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(30, 5, 1, 1),
        BASE,
        "unselected_commit_row",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(120, 9, 1, 1),
        TINT_OURS,
        "added_preview_row",
    );
}

/// The diff view keeps the which-key box docked as the same card with no key
/// held, so the cell in the card's footer row is still on the raised surface.
///
/// The card's cells take columns 107 to 159 and rows 39 to 47, above the two
/// rows of the status band, and the card takes the HintBox board's docked
/// shape. Its fill reaches into row 48, the band's spare row, and stops 6 px
/// above the status row, on the band's ground. Its top-left corner alone
/// rounds, so that corner's pixel leaves the raised surface and the top-right
/// corner's keeps it. A fillet curves out from its bottom-left corner, so the
/// pixel left of that corner on the card's last pixel row takes the raised
/// surface too.
#[test]
fn the_diff_view_keeps_its_box_docked_on_the_raised_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>di<Settle>",
    );

    expect_region(
        &shot,
        &mut home,
        cell_rect(157, 46, 1, 1),
        RAISED,
        "diff_hints_card",
    );

    let pixel = |x: u32, y: u32| {
        let at = ((y * shot.width + x) * 4) as usize;
        [shot.rgba[at], shot.rgba[at + 1], shot.rgba[at + 2]]
    };
    let (left, top, _, _) = cell_rect(107, 39, 1, 1);
    let (right, _, width, _) = cell_rect(159, 39, 1, 1);
    let (_, status_top, ..) = cell_rect(0, ROWS - 1, 1, 1);
    let bottom = status_top - 2 * STATUS_ROW_PAD;
    let corners = [
        pixel(left, top),
        pixel(right + width - 1, top),
        pixel(left - 1, bottom - 1),
    ];
    if corners[0] == RAISED || corners[1..] != [RAISED; 2] {
        let frame = keep_frame(&shot, &mut home, "diff_hints_card_corners");
        panic!(
            "the card's top-left, top-right, and fillet pixels read {:?}, not a rounded top-left \
             and {} at the other two; the frame is {}",
            corners.map(hex),
            hex(RAISED),
            frame.display(),
        );
    }
}

/// The finder's selected row is one box in the tab wash with 4 px corners, and
/// no accent edge runs inside it.
///
/// The selected row is the list's first, cell row 12, and its first cell is
/// cell 17, from pixel column 153. The box stands 1 px inside that cell, so
/// the row's first pixel column keeps the list's Island, on the base surface.
/// The box spans pixel columns 154 to 599, 3 px inside the row's last cell,
/// and its 4 px arcs leave the base at the four corner pixels. A blank pixel
/// in the row's middle reads the tab wash, and no pixel of the row's first
/// cell takes the accent.
#[test]
fn the_finder_marks_its_selected_row_with_a_rounded_wash() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "many-files",
        "src/alpha.txt",
        "<Space>p<Settle>",
    );

    expect_region(
        &shot,
        &mut home,
        (500, 225, 1, 1),
        TAB_WASH,
        "selected_row_middle",
    );
    expect_region(
        &shot,
        &mut home,
        (153, 216, 1, 18),
        BASE,
        "selected_row_first_column",
    );
    for (x, y) in [(154, 216), (599, 216), (154, 233), (599, 233)] {
        let what = format!("selected_row_corner_{x}_{y}");
        expect_region(&shot, &mut home, (x, y, 1, 1), BASE, &what);
    }

    let accent = (153..162)
        .flat_map(|x| (216..234).map(move |y| (x, y)))
        .filter(|&(x, y)| pixel(&shot, x, y) == ACCENT)
        .count();
    if accent > 0 {
        let frame = keep_frame(&shot, &mut home, "selected_row_accent");
        panic!(
            "{accent} pixels of the selected row's first cell take the accent; the frame is {}",
            frame.display(),
        );
    }
}

/// The finder's header ends in its scope chip, a box on the tab wash.
///
/// The box takes whole cells, the fewest that hold `S-Tab · git only` with
/// 6 px of padding at each end. Those are the 15 cells that end at the inner
/// rect's right edge, cell 143. The header row is cell row 10, and the box
/// draws 1 px right and 7 px down of its cells. So the tab wash fills the
/// pixels from (1153, 187) to (1287, 204), and no other pixel of the header's
/// 32 px band, pixel rows 180 to 211, takes it.
#[test]
fn the_finder_header_ends_in_a_chip_on_the_tab_wash() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "many-files",
        "src/alpha.txt",
        "<Space>p<Settle>",
    );

    expect_color_box(
        &shot,
        &mut home,
        TAB_WASH,
        180..212,
        (1153, 187, 1287, 204),
        "header_chip",
    );
}

/// The editor behind the finder fades toward its own ground.
///
/// The fade blends a glyph toward its cell's background, so the backdrop keeps
/// its surface and its text dims. The brightest pixel of the first line's text
/// falls below half of what it was before the finder opened.
#[test]
fn the_editor_behind_the_finder_fades() {
    let mut home = support::scratch_home();
    let mut session = spawn_settled(home.path(), "many-files", "src/alpha.txt");
    let text = cell_rect(6, 2, 17, 1);
    let before = session.snapshot().expect("a snapshot");
    play(&mut session, "<Space>p<Settle>");
    let after = session.snapshot().expect("a snapshot");

    let (lit, faded) = (brightest(&before, text), brightest(&after, text));
    if u16::from(faded) * 2 >= u16::from(lit) {
        let frame = keep_frame(&after, &mut home, "modal_backdrop");
        panic!(
            "the backdrop text peaks at {faded} under the finder and at {lit} before it; the \
             frame is {}",
            frame.display(),
        );
    }
}

/// The finder ends in its key line on the plate's ground, in the inner rect's
/// last row, with no raised fill and no rule above it.
///
/// The box's modal rect is the 128 by 32 cells from cell (16, 9), so the inner
/// rect runs from cell 17 to cell 142 and its last row is cell row 39. The
/// line's keys end well short of pixel column 1200. No cell of the frame is
/// raised, and the docked key hints box waits for its toggle under the finder.
#[test]
fn the_finder_ends_in_a_key_line_on_the_ground() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "many-files",
        "src/alpha.txt",
        "<Space>p<Settle>",
    );

    let raised = cells_of(&shot, RAISED);
    if !raised.is_empty() {
        let frame = keep_frame(&shot, &mut home, "finder_raised");
        panic!(
            "the cells {raised:?} take the raised surface; the frame is {}",
            frame.display(),
        );
    }
    expect_key_line(&shot, &mut home, 39, (17, 143), 1200, "finder_key_line");
}

/// The session picker's ground plate runs from the top of its header row to
/// its key line, which ends the plate 10 px into the modal rect's last row.
///
/// The box is the S size, 80 columns and seven rows, a third of the way down
/// the 43 rows the 50-row window leaves around it. So it starts at cell
/// (40, 14), the inner rect runs from cell (41, 15) to cell 118, and the key
/// line is its last row, cell row 19. The top edge reads one pixel on each
/// side, the ground inside and the shadowed panes outside, in the inner rect's
/// first cell, which the plate's corner arcs leave whole. The line's keys end
/// and its legend starts clear of pixel column 840.
#[test]
fn the_session_picker_plate_ends_in_its_key_line() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<C-a><C-f><Settle>",
    );

    let (column, top, _, _) = cell_rect(41, 15, 1, 1);
    let (outside, inside) = (
        pixel(&shot, column + 2, top - 1),
        pixel(&shot, column + 2, top),
    );
    if outside == GROUND || inside != GROUND {
        let frame = keep_frame(&shot, &mut home, "session_plate");
        panic!(
            "the plate's top edge reads {} outside and {} inside, where only the inside \
             belongs to the ground {}; the frame is {}",
            hex(outside),
            hex(inside),
            hex(GROUND),
            frame.display(),
        );
    }
    expect_key_line(&shot, &mut home, 19, (41, 119), 840, "session_key_line");
}

/// The help header names both scopes. The active `applicable` sits in a chip
/// box on the tab wash, and `all` follows as text on the plate's ground.
///
/// The box takes the fewest whole cells that hold `applicable` with 6 px of
/// padding at each end, which are the 10 cells from cell 129. They end on the
/// last cell edge left of `all` and its gap. The header row is cell row 3, and
/// the box draws 1 px right and 7 px down of its cells. So the tab wash fills
/// the pixels from (1162, 61) to (1251, 78), and no other pixel of the header's
/// 32 px band, pixel rows 54 to 85, takes it. In that band, the 30 px from x 1258 hold `all` with
/// no box, so most of them keep the ground.
#[test]
fn the_help_header_names_both_scopes() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "?<Settle>");

    expect_color_box(
        &shot,
        &mut home,
        TAB_WASH,
        54..86,
        (1162, 61, 1251, 78),
        "help_active_scope",
    );
    expect_region(
        &shot,
        &mut home,
        (1258, 61, 30, 18),
        GROUND,
        "help_inactive_scope",
    );
}

/// The help modal's detail pane is an Island on the base surface, 8 px right of
/// the list's Island.
///
/// The list ends before cell 82, the separator column, and the detail starts at
/// cell 83. Cell row 30 is blank in both panes, and on its middle pixel row the
/// two Islands stand 8 px apart across the separator. A blank cell low in the
/// detail reads the base surface.
#[test]
fn the_help_detail_draws_on_the_editor_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "?<Settle>");

    expect_islands_apart(&shot, &mut home, 82, 30, "help_islands");
    expect_region(
        &shot,
        &mut home,
        cell_rect(110, 38, 1, 1),
        BASE,
        "help_detail",
    );
}

/// A hover ends in a footer row on the raised surface, inside the overlay box.
///
/// The server loads the crate before it answers, so the hover key plays again
/// until a raised row shows. The server's progress spinner keeps the screen
/// moving, so each try waits a fixed time in place of a settle. The raised
/// cells form one run in one row, and the box's fill surrounds that run.
#[test]
fn a_hover_ends_in_a_footer_on_the_raised_surface() {
    let mut home = support::scratch_home();
    let mut session = spawn_settled(home.path(), "rust-lsp", "src/main.rs");
    play(&mut session, "/Greeter<Enter>");

    let deadline = Instant::now() + LSP_DEADLINE;
    let (shot, raised) = loop {
        play(&mut session, "<Esc><Space>li<Wait:2000>");
        let shot = session.snapshot().expect("a snapshot");
        let raised = cells_of(&shot, RAISED);
        if !raised.is_empty() {
            break (shot, raised);
        }
        assert!(
            Instant::now() < deadline,
            "no hover opened before the deadline"
        );
    };

    let (first, row) = raised[0];
    let last = raised[raised.len() - 1].0;
    if raised != (first..=last).map(|col| (col, row)).collect::<Vec<_>>() {
        let frame = keep_frame(&shot, &mut home, "footer_run");
        panic!(
            "the raised cells {raised:?} are not one run in one row; the frame is {}",
            frame.display(),
        );
    }

    let middle = (first + last) / 2;
    for (col, row, what) in [
        (first - 1, row, "box_left_edge"),
        (last + 1, row, "box_right_edge"),
        (middle, row - 1, "box_body"),
        (middle, row + 1, "box_bottom_edge"),
    ] {
        expect_region(&shot, &mut home, cell_rect(col, row, 1, 1), OVERLAY, what);
    }
}

/// A completion row of a function leads with its badge on the function tint,
/// and the selected first row is a box in the tab wash with 4 px corners.
///
/// The server loads the crate before it answers, so the completion key plays
/// again until the tint shows. The popup opens with no row selected, so Tab
/// then selects the first row. The prefix `mai` names the fixture's `main`
/// function. Every badge sits in the box's first column, so the tinted cells
/// all share one column.
///
/// The badge's box stands 1 px inside its row, so the first tinted cell's
/// first pixel row shows the row's box. Its pixel column 0 lies outside the
/// box's corner arc and keeps the overlay, and its column 4 lies on the box's
/// straight top edge.
#[test]
fn a_function_completion_leads_with_its_badge_on_the_function_tint() {
    let mut home = support::scratch_home();
    let mut session = spawn_settled(home.path(), "rust-lsp", "src/main.rs");
    play(&mut session, "/unused<Enter>omai");

    let deadline = Instant::now() + LSP_DEADLINE;
    loop {
        play(&mut session, "<C-Space><Wait:2000>");
        let shot = session.snapshot().expect("a snapshot");
        if !cells_of(&shot, TINT_ACCENT).is_empty() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no function completion showed before the deadline"
        );
    }

    play(&mut session, "<Tab><Settle>");
    let shot = session.snapshot().expect("a snapshot");
    let tinted = cells_of(&shot, TINT_ACCENT);

    if tinted.is_empty() || tinted.iter().any(|&(col, _)| col != tinted[0].0) {
        let frame = keep_frame(&shot, &mut home, "completion_badges");
        panic!(
            "the function tint cells {tinted:?} are not one badge column; the frame is {}",
            frame.display(),
        );
    }

    let (col, row) = tinted[0];
    let (x, y, _, _) = cell_rect(col, row, 1, 1);
    expect_region(
        &shot,
        &mut home,
        (x, y, 1, 1),
        OVERLAY,
        "selected_row_corner",
    );
    expect_region(
        &shot,
        &mut home,
        (x + 4, y, 1, 1),
        TAB_WASH,
        "selected_row_top_edge",
    );
}

/// A selection washes its range, the primary cursor is the terminal's own
/// opaque block, and a secondary cursor takes the theme's cursor block.
///
/// The text starts in cell 6, and the tab bar's band takes rows 0 and 1.
/// `jjvll` selects `3 t` on row 4, so the wash covers the `3` and the blank
/// cell after it, and the primary cursor sits on the `t` in cell 8. `<Esc>CC`
/// copies the selection onto the next two rows, and the newest, on row 6,
/// takes the primary, which leaves the cursor on row 4 secondary.
#[test]
fn the_selection_scene_paints_the_wash_and_both_cursor_kinds() {
    let mut home = support::scratch_home();
    let mut session = spawn_settled(home.path(), "basic-diff", "unstaged.txt");

    play(&mut session, "jjvll<Settle>");
    let selected = session.snapshot().expect("a snapshot");
    expect_region(
        &selected,
        &mut home,
        cell_rect(7, 4, 1, 1),
        WASH,
        "selection_wash",
    );
    expect_region(
        &selected,
        &mut home,
        cell_rect(8, 4, 1, 1),
        CURSOR_PRIMARY,
        "primary_cursor",
    );

    play(&mut session, "<Esc>CC<Settle>");
    let copied = session.snapshot().expect("a snapshot");
    expect_region(
        &copied,
        &mut home,
        cell_rect(8, 4, 1, 1),
        CURSOR_SECONDARY,
        "secondary_cursor",
    );
    expect_region(
        &copied,
        &mut home,
        cell_rect(8, 6, 1, 1),
        CURSOR_PRIMARY,
        "copied_primary_cursor",
    );
}

/// A gzip file's pane rings its text in the dashed read-only hairline.
///
/// The tab bar's band takes rows 0 and 1, so the first content row is row 2,
/// and the ring's top edge is that row's first pixel row. Three pixels in
/// five carry the dashed stroke, so the stroke's color leads a run of them,
/// and the run two pixels down lies under the stroke on the editor surface.
/// Cells 20 to 60 lie clear of the ring's rounded corners and of the label at
/// its right end.
#[test]
fn a_read_only_pane_draws_the_dashed_ring_over_its_text() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "read-only", "CHANGELOG.md.gz", "");
    let (x, y, width, _) = cell_rect(20, 2, 40, 1);

    expect_region(
        &shot,
        &mut home,
        (x, y, width, 1),
        READONLY_FRAME,
        "ring_top_edge",
    );
    expect_region(
        &shot,
        &mut home,
        (x, y + 2, width, 1),
        BASE,
        "under_the_ring",
    );
}

/// The conflict view paints as the MergeGrid on the Ermine palette.
///
/// The fixture pauses a merge with one conflict in `file.txt`. The tab bar's
/// band takes rows 0 and 1, and the minimap band takes the window's last seven
/// columns before its margin, past a gap column. So the view spans 150 cells,
/// with sides of 48 cells at x 1 and 103 and the merged column of 54 cells at
/// x 49. The column header is row 2 and the File Header row 3, the File Header
/// on the head color across all three columns. `1 shared header` is row 4, and
/// the chunk's first row is row 5, where each column takes its side's tint.
/// The ours bar sits 7/16 into its column and is 6/16 wide, so at 9-pixel
/// cells it snaps to pixel columns 4 to 6 of the column's first cell, the
/// board's `margin-left: 4px; width: 3px`.
///
/// The File Header's tag and counts end before cell 49, its path starts past
/// cell 50, and its place starts past cell 140, so the checked spans of row 3
/// read the head color. The chunk's chip ends 12/16 before the merged column's
/// right edge at cell 103, and its box reaches back past cell 91. The check
/// reads the 10 cells from cell 91, which the box covers but for a pixel row at
/// its top and its bottom, so they read the chip's fill.
#[test]
fn the_conflict_view_paints_the_merge_grid() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "conflict",
        "file.txt",
        "<C-;>Conflict<Enter><Settle>",
    );

    for (col, row, want, what) in [
        (5, 5, TINT_OURS, "ours_conflict_row"),
        (55, 5, TINT_CONFLICT, "merged_conflict_row"),
        (113, 5, TINT_THEIRS, "theirs_conflict_row"),
        (55, 2, MERGED_HEAD, "merged_header"),
        (5, 3, MERGED_HEAD, "ours_file_header"),
        (55, 3, MERGED_HEAD, "merged_file_header"),
        (113, 3, MERGED_HEAD, "theirs_file_header"),
        (5, 4, BASE, "context_row"),
    ] {
        expect_region(&shot, &mut home, cell_rect(col, row, 20, 1), want, what);
    }
    let (x, y, _, height) = cell_rect(1, 5, 1, 1);
    expect_region(&shot, &mut home, (x + 4, y, 3, height), SUCCESS, "ours_bar");
    expect_region(
        &shot,
        &mut home,
        cell_rect(103 - 12, 5, 10, 1),
        CHIP_CONFLICT,
        "conflict_chip",
    );
}

/// The tab bar ends in the conflict chip while the conflict view is focused.
///
/// The chip reads `1 unresolved · 1 / 1 chunks` at the Caption scale, 24
/// cells that end one cell before the bar's right edge, so cells 140 to 152
/// of the bar's row lie inside it on its fill. The row draws [`TAB_BAR_PAD`]
/// px down in its band, so the check reads it there.
#[test]
fn the_tab_bar_ends_in_the_conflict_chip() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "conflict",
        "file.txt",
        "<C-;>Conflict<Enter><Settle>",
    );

    let (x, y, width, height) = cell_rect(140, 0, 12, 1);
    expect_region(
        &shot,
        &mut home,
        (x, y + TAB_BAR_PAD, width, height),
        CHIP_CONFLICT,
        "conflict_chip",
    );
}

/// The git status panel names the merge in progress in its banner and leads
/// its sections with the conflicted file.
///
/// The box hangs from row 4, two rows under the tab bar's band, with its right
/// edge one cell in from the window's, so its inner cells span columns 109 to
/// 157 from row 5. Row 5 is the header, whose rule is the row's last pixel
/// row. Rows 6 to 8 are the banner, whose bar is the first pixel column of
/// column 109. Row 9 heads the conflicted section, and row 10 holds its file,
/// whose bar sits 4/16 into column 109.
#[test]
fn the_git_status_panel_names_the_merge_in_its_banner() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "conflict",
        "file.txt",
        ":git status<Enter><Settle>",
    );

    expect_region(
        &shot,
        &mut home,
        cell_rect(157, 5, 1, 1),
        BASE,
        "inner_top_right",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(140, 6, 15, 3),
        TINT_MERGE,
        "banner_rows",
    );
    let (x, y, width, _) = cell_rect(130, 5, 25, 1);
    expect_region(
        &shot,
        &mut home,
        (x, y + 17, width, 1),
        RAISED,
        "header_rule",
    );
    let (x, y, _, height) = cell_rect(109, 6, 1, 3);
    expect_region(&shot, &mut home, (x, y, 1, height), CONFLICT, "banner_bar");
    let (x, y, _, height) = cell_rect(109, 10, 1, 1);
    expect_region(
        &shot,
        &mut home,
        (x + 2, y, 2, height),
        DANGER,
        "conflicted_bar",
    );
}

/// The git status panel colors each file's bar by its section, the staged
/// file's in info and the unstaged file's in muted.
///
/// The box hangs from row 4 with its header on row 5. With no banner, row 6
/// heads the staged section over its file on row 7, and row 8 heads the
/// unstaged section over its file on row 9.
#[test]
fn the_git_status_panel_bars_each_file_in_its_section_color() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        ":git status<Enter><Settle>",
    );

    for (row, want, what) in [(7, INFO, "staged_bar"), (9, MUTED, "unstaged_bar")] {
        let (x, y, _, height) = cell_rect(109, row, 1, 1);
        expect_region(&shot, &mut home, (x + 2, y, 2, height), want, what);
    }
}

/// The Blame Column draws its surface 158 px wide from the pane's left edge
/// and each run's 2 px bar in the color of its commit's age, as the BlameView
/// board draws them. A divider takes the top pixel row of each run's first
/// line past the file's first line.
///
/// The cursor rests on the uncommitted line 34, so no commit is selected. File
/// line n shows on row n + 1, and the pane starts at x 9. The bar takes x 9
/// and 10, the surface x 11 to 166, and the editor surface shows from x 167.
/// Lines 2, 5, 15, and 27 belong to the four commits from the oldest, and line
/// 34 is uncommitted. Line 4 starts the second commit's run on row 5.
#[test]
fn the_blame_column_draws_its_surface_and_each_run_in_its_age_color() {
    let mut home = support::scratch_home();
    let shot = blamed_frame(home.path(), "34G<Settle>");

    let runs = [
        (3, BLAME_AGE_1),
        (6, BLAME_AGE_2),
        (16, ACCENT),
        (28, BLAME_AGE_4),
        (35, MUTED),
    ];
    let (_, y, _, _) = cell_rect(0, 5, 1, 1);
    let found = (
        runs.map(|(row, color)| column_bars(&shot, row, color)),
        runs.map(|(row, _)| surface_color(&shot, row)),
        column_bars(&shot, 3, BLAME_SURFACE),
        pixel(&shot, 167, y + 9),
        [y - 1, y, y + 1].map(|y| pixel(&shot, 13, y)),
    );

    let want = (
        runs.map(|_| vec![9, 10]),
        runs.map(|_| BLAME_SURFACE),
        (11..=166).collect::<Vec<u32>>(),
        BASE,
        [BLAME_SURFACE, DIVIDER, BLAME_SURFACE],
    );
    if found != want {
        let frame = keep_frame(&shot, &mut home, "blame_column");
        panic!(
            "the bars, the surfaces, row 3's surface columns, the pixel past the surface, and \
             the divider's three pixels read {found:?}, not {want:?}; the frame is {}",
            frame.display(),
        );
    }
}

/// Every run of the cursor's commit draws the selected surface, as the
/// BlameView board tints both runs of its cursor's commit.
///
/// The cursor rests on line 18, in the second run of the third commit. That
/// commit holds lines 14 to 16 and 18 to 20, which show on rows 15 to 17 and
/// 19 to 21. Rows 14, 18, and 22 show lines of the first commit.
#[test]
fn the_cursor_commit_tints_every_run_of_it() {
    let mut home = support::scratch_home();
    let shot = blamed_frame(home.path(), "18G<Settle>");

    let found: Vec<[u8; 3]> = (14..=22).map(|row| surface_color(&shot, row)).collect();
    let (plain, tint) = (BLAME_SURFACE, BLAME_SELECTED);
    if found != [plain, tint, tint, tint, plain, tint, tint, tint, plain] {
        let frame = keep_frame(&shot, &mut home, "blame_selection");
        let colors = found.iter().map(|&color| hex(color)).collect::<Vec<_>>();
        panic!(
            "rows 14 to 22 of the column read {colors:?}; the frame is {}",
            frame.display(),
        );
    }
}

/// A blame checkout draws its banner under the tab bar until the return, as
/// the BlameCheckout board draws it.
///
/// The scene checks out the third commit from line 18, and the confirm stashes
/// the uncommitted line. The banner's band takes rows 2 and 3, pixel rows 36
/// to 71. Its box draws 2 px down and stands 4 px outside row 2, so it fills
/// pixel rows 34 to 59, with the ground on pixel rows 33 and 62. The island
/// under the banner reaches 4 px into row 3, so its base surface starts at
/// pixel row 68. Pixel column 720 lies clear of the banner's texts.
#[test]
fn a_blame_checkout_draws_its_banner_under_the_tab_bar() {
    let mut home = support::scratch_home();
    let shot = checked_out_frame(home.path());

    let found = [33, 47, 62, 70].map(|y| pixel(&shot, 720, y));
    if found != [GROUND, TINT_WARNING, GROUND, BASE] {
        let frame = keep_frame(&shot, &mut home, "checkout_banner");
        panic!(
            "pixel column 720 reads {:?} at pixel rows 33, 47, 62, and 70, not the ground, the \
             banner's tint, the ground, and the island's base; the frame is {}",
            found.map(hex),
            frame.display(),
        );
    }
}

/// The frame the `fixture` scene with `file` open settles on after `keys` play.
///
/// The session ends before this returns, so the child is gone before its
/// scratch home is.
fn settled_frame(home: &Path, fixture: &str, file: &str, keys: &str) -> Snapshot {
    let mut session = spawn_settled(home, fixture, file);
    play(&mut session, keys);
    session.snapshot().expect("a snapshot")
}

/// The frame the blame scene settles on with its Blame Column on, once the
/// blame lands and `keys` play.
///
/// The session ends before this returns, as in [`settled_frame`].
fn blamed_frame(home: &Path, keys: &str) -> Snapshot {
    let mut session = spawn_settled(home, "blame", "src/chrome.rs");
    play(&mut session, "<Space>ha");
    wait_for_blame(&mut session);
    play(&mut session, keys);
    session.snapshot().expect("a snapshot")
}

/// The frame the blame scene settles on once the checkout of the third commit
/// from line 18 lands, past the confirm that stashes the uncommitted line.
///
/// The confirm opens once a read of the tree lands, so the `<Enter>` waits for
/// the plate's ground on the confirm's title row. The session ends before this returns,
/// as in [`settled_frame`].
fn checked_out_frame(home: &Path) -> Snapshot {
    let mut session = spawn_settled(home, "blame", "src/chrome.rs");
    play(&mut session, "<Space>ha");
    wait_for_blame(&mut session);

    play(&mut session, "18Gc");
    let title_row = cell_rect(95, 22, 10, 1);
    wait_for_color(&mut session, title_row, GROUND, "the checkout confirm");
    play(&mut session, "<Enter>");
    wait_for_color(
        &mut session,
        (720, 47, 1, 1),
        TINT_WARNING,
        "the checkout banner",
    );
    session.snapshot().expect("a snapshot")
}

/// A session of the `fixture` scene with `file` open, once its first frame
/// settles.
///
/// The basic-diff scene also waits for its file's diff, which lands after the
/// first frame. Under load, a settle sometimes ends before the diff lands, and
/// the frame then shows no gutter lanes and no file counts in the status row.
fn spawn_settled(home: &Path, fixture: &str, file: &str) -> HeadlessSession {
    let mut session = spawn_scene(ermine_scene(fixture, file, home));
    if fixture == "basic-diff" {
        wait_for_diff(&mut session);
    }
    session
}

/// Wait until the basic-diff scene's modified row, row 5, shows its hunk lane
/// in the accent, and then until the frame settles.
fn wait_for_diff(session: &mut HeadlessSession) {
    let (x, y, _, height) = cell_rect(1, 5, 1, 1);
    let deadline = Instant::now() + support::STARTUP_TIMEOUT;
    while dominant_color(&session.snapshot().expect("a snapshot"), (x, y, 3, height)) != ACCENT {
        assert!(
            Instant::now() < deadline,
            "the basic-diff scene's diff did not land"
        );
        play(session, "<Wait:100>");
    }

    session
        .wait_settled(support::SETTLE, support::STARTUP_TIMEOUT)
        .expect("the frame with the diff settles");
}

/// Wait until the blame scene's row 3 draws the selected surface, and then
/// until the frame settles.
///
/// Row 3 shows line 2, a line of the commit of the cursor's line 1. The column
/// draws its plain surface until the blame lands, so only the selection marks
/// the landed blame.
fn wait_for_blame(session: &mut HeadlessSession) {
    let deadline = Instant::now() + support::STARTUP_TIMEOUT;
    while surface_color(&session.snapshot().expect("a snapshot"), 3) != BLAME_SELECTED {
        assert!(
            Instant::now() < deadline,
            "the blame scene's blame did not land"
        );
        play(session, "<Wait:100>");
    }

    session
        .wait_settled(support::SETTLE, support::STARTUP_TIMEOUT)
        .expect("the frame with the blame settles");
}

/// Wait until most pixels of `rect` read `color`, where `what` shows, and then
/// until the frame settles.
fn wait_for_color(session: &mut HeadlessSession, rect: PixelRect, color: [u8; 3], what: &str) {
    let deadline = Instant::now() + support::STARTUP_TIMEOUT;
    while dominant_color(&session.snapshot().expect("a snapshot"), rect) != color {
        assert!(Instant::now() < deadline, "{what} did not show");
        play(session, "<Wait:100>");
    }

    session
        .wait_settled(support::SETTLE, support::STARTUP_TIMEOUT)
        .expect("the frame settles");
}

/// A session of `scene`, once its first frame settles.
fn spawn_scene(scene: Scene) -> HeadlessSession {
    let mut session = HeadlessSession::spawn(scene).unwrap_or_else(|error| {
        panic!(
            "{error}. With no wgpu adapter, enter the devshell with `nix develop`, which \
             registers lavapipe"
        )
    });

    session
        .wait_settled(support::SETTLE, support::STARTUP_TIMEOUT)
        .expect("the first frame settles; a stoat built without its fixture feature exits at once");
    session
}

/// Play the key script `keys` into `session`.
fn play(session: &mut HeadlessSession, keys: &str) {
    session
        .type_keys(&script::parse(keys).expect("a key script"))
        .expect("the keys play");
}

/// The `fixture` scene with `file` open on the `ermine_night` theme, at
/// [`COLS`] by [`ROWS`] cells and [`FONT_SIZE`] on the software adapter, with
/// config, data, and state under `home`.
///
/// The scene names its theme rather than taking the embedded config's default,
/// so the checks keep their palette when the default changes. A fixture opens
/// no file of its own, so the scene names one.
fn ermine_scene(fixture: &str, file: &str, home: &Path) -> Scene {
    let mut config = config::embedded_default();
    config.theme = "ermine_night".to_owned();

    Scene {
        program: support::stoat_binary().to_string_lossy().into_owned(),
        args: ["--fixture", fixture, file].map(String::from).to_vec(),
        working_directory: Some(home.to_owned()),
        stoat_dir: None,
        cols: COLS,
        rows: ROWS,
        font_size: FONT_SIZE,
        terminal_font_size: config.terminal_font_size,
        scale_factor: 1.0,
        theme: config.resolve_theme(),
        theme_name: config.theme,
        font_family: config.font_family,
        ligatures: config.ligatures,
        cursor_animation: config.cursor_animation,
        software: true,
        extra_env: support::scratch_env(home),
    }
}

/// The pixels that `cols` by `rows` cells cover, from the cell at `col` and
/// `row`.
fn cell_rect(col: u16, row: u16, cols: u16, rows: u16) -> PixelRect {
    let [width, height] = render::cell_size(FONT_SIZE, 1.0).map(|side| side as u32);
    (
        u32::from(col) * width,
        u32::from(row) * height,
        u32::from(cols) * width,
        u32::from(rows) * height,
    )
}

/// Panics unless the dominant color of `rect` in `shot` is `want`.
///
/// A miss saves the frame as `<what>.png` in `home` and keeps `home` past the
/// test, so the frame the panic names is there to open.
fn expect_region(shot: &Snapshot, home: &mut TempDir, rect: PixelRect, want: [u8; 3], what: &str) {
    let found = dominant_color(shot, rect);
    if found == want {
        return;
    }

    let frame = keep_frame(shot, home, what);
    panic!(
        "{what}: most pixels of {rect:?} are {}, not {}; the frame is {}",
        hex(found),
        hex(want),
        frame.display(),
    );
}

/// Panics unless cell row `row` holds a plate's key line on the ground, with
/// the plate ending 10 px into the row under it.
///
/// Across the cells `cols`, from their first cell to the cell before their
/// last, no pixel row from 2 px above the row to 6 px into it takes the
/// hairline, where the cell look rules its footer off. At the line's middle
/// pixel row, `blank_x` reads the ground behind the line. Under it the pixel
/// 1 px above the plate's bottom edge reads the ground, and the pixel at the
/// edge does not. A miss saves the frame as [`expect_region`] does.
fn expect_key_line(
    shot: &Snapshot,
    home: &mut TempDir,
    row: u16,
    cols: (u16, u16),
    blank_x: u32,
    what: &str,
) {
    let (left, top, _, height) = cell_rect(cols.0, row, 1, 1);
    let (right, _, _, _) = cell_rect(cols.1, row, 1, 1);
    let ruled: Vec<u32> = (top - 2..top + 6)
        .filter(|&y| (left..right).any(|x| pixel(shot, x, y) == HAIRLINE))
        .collect();
    let bottom = top + height + 10;
    let reads = [
        pixel(shot, blank_x, top + height / 2),
        pixel(shot, blank_x, bottom - 1),
        pixel(shot, blank_x, bottom),
    ];
    if ruled.is_empty() && reads[0] == GROUND && reads[1] == GROUND && reads[2] != GROUND {
        return;
    }

    let frame = keep_frame(shot, home, what);
    panic!(
        "{what}: the hairline takes the pixel rows {ruled:?}, and column {blank_x} reads {} \
         in the line, {} above the plate's bottom edge, and {} at it; the frame is {}",
        hex(reads[0]),
        hex(reads[1]),
        hex(reads[2]),
        frame.display(),
    );
}

/// Panics unless the pixels of `color` in the pixel rows `rows` fill a box
/// that spans exactly `want`, the inclusive `(left, top, right, bottom)`.
///
/// Every pixel of the color in those rows counts, so a second element in it
/// widens the box and fails. A miss saves the frame as [`expect_region`] does.
fn expect_color_box(
    shot: &Snapshot,
    home: &mut TempDir,
    color: [u8; 3],
    rows: Range<u32>,
    want: (u32, u32, u32, u32),
    what: &str,
) {
    let found = rows
        .flat_map(|y| (0..shot.width).map(move |x| (x, y)))
        .filter(|&(x, y)| pixel(shot, x, y) == color)
        .fold(None, |found: Option<(u32, u32, u32, u32)>, (x, y)| {
            Some(match found {
                None => (x, y, x, y),
                Some((left, top, right, bottom)) => {
                    (left.min(x), top.min(y), right.max(x), bottom.max(y))
                },
            })
        });
    if found == Some(want) {
        return;
    }

    let frame = keep_frame(shot, home, what);
    panic!(
        "{what}: the pixels of {} span {found:?}, not {want:?}; the frame is {}",
        hex(color),
        frame.display(),
    );
}

/// Panics unless two Islands stand 8 px apart across the separator column
/// `separator`, on the middle pixel row of cell row `row`.
///
/// The left Island reaches the separator's first pixel, the next 8 px read the
/// ground, and the right Island starts at the next column's first pixel. Each
/// of the ten pixels is read alone, so a gap one pixel off fails. A miss saves
/// the frame as [`expect_region`] does.
fn expect_islands_apart(shot: &Snapshot, home: &mut TempDir, separator: u16, row: u16, what: &str) {
    let (x, y, _, height) = cell_rect(separator, row, 1, 1);
    let y = y + height / 2;
    let found: Vec<[u8; 3]> = (x..x + 10).map(|x| pixel(shot, x, y)).collect();
    let mut want = vec![BASE];
    want.extend([GROUND; 8]);
    want.push(BASE);
    if found == want {
        return;
    }

    let frame = keep_frame(shot, home, what);
    panic!(
        "{what}: the pixels from ({x}, {y}) read {:?}, not the base, 8 px of the ground, and \
         the base; the frame is {}",
        found.iter().map(|&color| hex(color)).collect::<Vec<_>>(),
        frame.display(),
    );
}

/// Panics unless `rect` of `shot` reads as the boards' hatch, the `ground` of
/// `colors` under 1 px lines of its `lines` every 7 px at 135 degrees.
///
/// The lines are placed by the screen pixel, so the check takes their phase
/// from the first line pixel. It then wants exactly the pixels on that phase's
/// diagonals in `lines` and every other pixel in `ground`. A miss saves the
/// frame as [`expect_region`] does.
fn expect_hatch(
    shot: &Snapshot,
    home: &mut TempDir,
    rect: PixelRect,
    (ground, lines): ([u8; 3], [u8; 3]),
    what: &str,
) {
    let (x, y, width, height) = rect;
    let at = |px: u32, py: u32| {
        let i = ((py * shot.width + px) * 4) as usize;
        [shot.rgba[i], shot.rgba[i + 1], shot.rgba[i + 2]]
    };
    let mut phase = None;
    'find: for py in y..y + height {
        for px in x..x + width {
            if at(px, py) == lines {
                phase = Some((px + py) % 7);
                break 'find;
            }
        }
    }
    let hatched = phase.is_some_and(|phase| {
        (y..y + height).all(|py| {
            (x..x + width).all(|px| {
                let want = match (px + py) % 7 == phase {
                    true => lines,
                    false => ground,
                };
                at(px, py) == want
            })
        })
    });
    if hatched {
        return;
    }

    let frame = keep_frame(shot, home, what);
    panic!(
        "{what}: {rect:?} is not {} under lines of {} every 7 px; the frame is {}",
        hex(ground),
        hex(lines),
        frame.display(),
    );
}

/// Panics unless each of the single band's seven columns, 152 to 158, reads
/// as the editor surface from row 2 under the tab bar's band to row 47 above
/// the status band, and a diff strip draws its text on row 2.
///
/// The strip's runs, its marks, and its viewport band cover few of a column's
/// pixels, so each column reads as the surface that a text strip sits on. A
/// short file's diff strip rows fill row 2 and draw their text in the strip's
/// gray, as [`strip_text_pixels`] counts it. A miss saves the frame as
/// [`expect_region`] does.
fn expect_strip_band(shot: &Snapshot, home: &mut TempDir, what: &str) {
    for col in 152..=158 {
        expect_region(shot, home, cell_rect(col, 2, 1, 46), BASE, what);
    }

    let text = strip_text_pixels(shot, cell_rect(152, 2, 7, 1));
    if text > 0 {
        return;
    }

    let frame = keep_frame(shot, home, what);
    panic!(
        "{what}: the band's first row holds no pixel of the strip's gray text; the frame is {}",
        frame.display(),
    );
}

/// Panics unless the dominant colors of `row`'s cells, merged into runs of one
/// color, read `want` from left to right.
///
/// A miss saves the frame as [`expect_region`] does.
fn expect_row_runs(shot: &Snapshot, home: &mut TempDir, row: u16, want: &[[u8; 3]], what: &str) {
    let mut runs: Vec<[u8; 3]> = Vec::new();
    for col in 0..COLS {
        let color = dominant_color(shot, cell_rect(col, row, 1, 1));
        if runs.last() != Some(&color) {
            runs.push(color);
        }
    }
    if runs == want {
        return;
    }

    let frame = keep_frame(shot, home, what);
    let colors = |colors: &[[u8; 3]]| colors.iter().map(|&color| hex(color)).collect::<Vec<_>>();
    panic!(
        "{what}: row {row} reads {:?}, not {:?}; the frame is {}",
        colors(&runs),
        colors(want),
        frame.display(),
    );
}

/// Save `shot` as `<what>.png` in `home` and keep `home` past the test, so the
/// frame a failure names is there to open.
fn keep_frame(shot: &Snapshot, home: &mut TempDir, what: &str) -> PathBuf {
    let frame = home.path().join(format!("{what}.png"));
    support::save(shot, &frame);
    home.disable_cleanup(true);
    frame
}

/// The `(col, row)` of every cell whose dominant color in `shot` is `color`,
/// row by row.
fn cells_of(shot: &Snapshot, color: [u8; 3]) -> Vec<(u16, u16)> {
    (0..ROWS)
        .flat_map(|row| (0..COLS).map(move |col| (col, row)))
        .filter(|&(col, row)| dominant_color(shot, cell_rect(col, row, 1, 1)) == color)
        .collect()
}

/// The pixel columns of `shot` that `color` fills over the whole band of rows
/// from `y`, left to right.
fn bar_columns(shot: &Snapshot, (y, height): (u32, u32), color: [u8; 3]) -> Vec<u32> {
    (0..shot.width)
        .filter(|&x| {
            (y..y + height).all(|row| {
                let at = ((row * shot.width + x) * 4) as usize;
                shot.rgba[at..at + 3] == color
            })
        })
        .collect()
}

/// The pixel columns of the Blame Column's 18 cells from column 1 that `color`
/// fills over the whole band of `row`, left to right.
fn column_bars(shot: &Snapshot, row: u16, color: [u8; 3]) -> Vec<u32> {
    let (x, y, width, height) = cell_rect(1, row, 18, 1);
    bar_columns(shot, (y, height), color)
        .into_iter()
        .filter(|column| (x..x + width).contains(column))
        .collect()
}

/// The dominant color of the Blame Column's surface on `row`, right of the
/// run's bar and under a divider.
///
/// A label's glyphs cover less of the row than the surface under them does.
fn surface_color(shot: &Snapshot, row: u16) -> [u8; 3] {
    let (_, y, _, height) = cell_rect(1, row, 1, 1);
    dominant_color(shot, (11, y + 1, 156, height - 1))
}

/// How many pixels of `rect` in `shot` are exactly `color`.
fn pixels_of(shot: &Snapshot, (x, y, width, height): PixelRect, color: [u8; 3]) -> usize {
    (y..y + height)
        .flat_map(|row| {
            let start = ((row * shot.width + x) * 4) as usize;
            let end = start + (width * 4) as usize;
            shot.rgba[start..end].chunks_exact(4)
        })
        .filter(|pixel| pixel[..3] == color)
        .count()
}

/// The largest channel of any pixel in `rect` of `shot`.
fn brightest(shot: &Snapshot, (x, y, width, height): PixelRect) -> u8 {
    (y..y + height)
        .flat_map(|row| {
            let start = ((row * shot.width + x) * 4) as usize;
            let end = start + (width * 4) as usize;
            shot.rgba[start..end]
                .chunks_exact(4)
                .map(|pixel| pixel[0].max(pixel[1]).max(pixel[2]))
        })
        .max()
        .expect("the rectangle holds pixels")
}

/// The pixels of `rect` in `shot` that draw a diff strip's text, within 6 of
/// [`STRIP_TEXT`] or of [`BAND_STRIP_TEXT`] under the viewport band.
///
/// A pixel that a run covers in part blends with the surface or the band and
/// misses both, so the count is the runs' fully covered pixels.
fn strip_text_pixels(shot: &Snapshot, (x, y, width, height): PixelRect) -> usize {
    let near = |pixel: &[u8], color: [u8; 3]| {
        pixel
            .iter()
            .zip(color)
            .all(|(&got, want)| got.abs_diff(want) <= 6)
    };
    (y..y + height)
        .flat_map(|row| {
            let start = ((row * shot.width + x) * 4) as usize;
            let end = start + (width * 4) as usize;
            shot.rgba[start..end].chunks_exact(4)
        })
        .filter(|pixel| near(&pixel[..3], STRIP_TEXT) || near(&pixel[..3], BAND_STRIP_TEXT))
        .count()
}

/// The longest run of pixels within 6 of `color` on pixel row `y` of `shot`,
/// in the `width` pixels from `x`.
fn longest_run(shot: &Snapshot, (x, y, width): (u32, u32, u32), color: [u8; 3]) -> u32 {
    let (mut longest, mut run) = (0, 0);
    for col in x..x + width {
        let near = pixel(shot, col, y)
            .into_iter()
            .zip(color)
            .all(|(got, want)| got.abs_diff(want) <= 6);
        run = if near { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    longest
}

/// The RGB color that the most pixels of `rect` in `shot` hold.
///
/// A tie in the count goes to the larger color, so the answer does not depend
/// on the order of the count map.
fn dominant_color(shot: &Snapshot, (x, y, width, height): PixelRect) -> [u8; 3] {
    let mut counts: HashMap<[u8; 3], u32> = HashMap::new();
    for row in y..y + height {
        let start = ((row * shot.width + x) * 4) as usize;
        let end = start + (width * 4) as usize;
        for pixel in shot.rgba[start..end].as_chunks::<4>().0 {
            *counts.entry([pixel[0], pixel[1], pixel[2]]).or_default() += 1;
        }
    }

    counts
        .into_iter()
        .max_by_key(|&(color, count)| (count, color))
        .map(|(color, _)| color)
        .expect("the rectangle holds pixels")
}

/// How many distinct RGB colors the pixels of `rect` in `shot` hold.
fn distinct_colors(shot: &Snapshot, (x, y, width, height): PixelRect) -> usize {
    let mut colors: HashSet<[u8; 3]> = HashSet::new();
    for row in y..y + height {
        let start = ((row * shot.width + x) * 4) as usize;
        let end = start + (width * 4) as usize;
        for pixel in shot.rgba[start..end].as_chunks::<4>().0 {
            colors.insert([pixel[0], pixel[1], pixel[2]]);
        }
    }
    colors.len()
}

/// The RGB color of pixel `(x, y)` in `shot`.
fn pixel(shot: &Snapshot, x: u32, y: u32) -> [u8; 3] {
    let at = ((y * shot.width + x) * 4) as usize;
    [shot.rgba[at], shot.rgba[at + 1], shot.rgba[at + 2]]
}

fn hex([red, green, blue]: [u8; 3]) -> String {
    format!("#{red:02x}{green:02x}{blue:02x}")
}
