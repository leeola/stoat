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

const ACCENT: [u8; 3] = [0x7f, 0xb0, 0xff];
const BASE: [u8; 3] = [0x17, 0x19, 0x1e];
const CHROME: [u8; 3] = [0x12, 0x14, 0x18];
const HOVER: [u8; 3] = [0x2c, 0x30, 0x39];
const OVERLAY: [u8; 3] = [0x25, 0x28, 0x30];
const RAISED: [u8; 3] = [0x1e, 0x21, 0x27];
const HATCH: [u8; 3] = [0x1b, 0x1d, 0x22];
const FOLD_BUTTON: [u8; 3] = [0x1f, 0x2a, 0x3d];
const TEXT_DIM: [u8; 3] = [0xa3, 0xa9, 0xb5];
const HAIRLINE: [u8; 3] = [0x2e, 0x32, 0x3b];
const SELECT: [u8; 3] = [0x23, 0x32, 0x4a];
const DISABLED: [u8; 3] = [0x5b, 0x61, 0x6d];
const TINT_ACCENT: [u8; 3] = [0x22, 0x30, 0x49];
const DANGER: [u8; 3] = [0xf2, 0x77, 0x7f];
const SUCCESS: [u8; 3] = [0x8f, 0xd1, 0x8a];
const SIDE_BASE: [u8; 3] = [0x2a, 0x1d, 0x20];
const SIDE_TARGET: [u8; 3] = [0x1d, 0x2a, 0x1f];
const WASH: [u8; 3] = [0x2a, 0x3c, 0x5a];
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

/// The primary cursor's opaque block, the terminal palette's cursor color. The
/// color comes from stoatty's own palette rather than a theme scope.
const CURSOR_PRIMARY: [u8; 3] = [0xdf, 0xe2, 0xe8];

/// The wait for rust-analyzer to load the fixture crate and answer, the bound
/// the live LSP tests allow.
const LSP_DEADLINE: Duration = Duration::from_secs(90);

/// A pixel rectangle as `(x, y, width, height)`.
type PixelRect = (u32, u32, u32, u32);

/// The normal-mode chip fills with the accent one pixel inside the status
/// row's top and bottom edges, and a blank text cell with the base surface.
///
/// The chip's cell keeps the row's chrome in its top and bottom pixel rows,
/// as the Editor board draws a 16 px chip on its row.
#[test]
fn the_basic_diff_scene_draws_on_the_ermine_palette() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");

    let (x, y, width, height) = cell_rect(2, ROWS - 1, 1, 1);
    expect_region(
        &shot,
        &mut home,
        (x, y + 1, width, height - 2),
        ACCENT,
        "mode_chip",
    );
    for (edge, what) in [(y, "mode_chip_top"), (y + height - 1, "mode_chip_bottom")] {
        expect_region(&shot, &mut home, (x, edge, width, 1), CHROME, what);
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
/// seventh. The tab bar takes row 0, so the changed row is row 4, `4 four
/// changed`.
#[test]
fn the_gutter_draws_the_hunk_and_state_lanes_left_of_the_number() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    let (x, y, _, height) = cell_rect(1, 4, 1, 1);

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
/// The changed row is row 4, `4 four` on the left and `4 four changed` on the
/// right. A bar is too narrow to lead its cell's colors, so the check finds the
/// pixel columns the color fills over the row's whole height, which a glyph
/// never does.
///
/// The cursor opens on the row's first column, so the ghost outlines the base
/// text's first cell, column 6. Its dashed top edge lies one pixel below the
/// cell's top, in the dim text color, and the next cell holds none of it.
#[test]
fn a_modified_row_reads_removed_on_the_base_side_and_added_on_the_live_side() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    let (_, y, _, height) = cell_rect(0, 4, 1, 1);
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

    let row_dims = |col: u16, dy: u32| {
        let (x, _, width, _) = cell_rect(col, 4, 1, 1);
        pixels_of(&shot, (x, y + dy, width, 1), TEXT_DIM)
    };
    let ghost = (row_dims(6, 0), row_dims(6, 1) > 0, row_dims(7, 1));
    if ghost != (0, true, 0) {
        let frame = keep_frame(&shot, &mut home, "diff_ghost_cursor");
        panic!(
            "the ghost's dim pixels read {ghost:?} on the cell top, its edge, and the next cell; \
             the frame is {}",
            frame.display(),
        );
    }
}

/// The base side of the split diff recedes as a whole behind the live side.
///
/// Row 6, `6 six`, is context on both sides. The left text starts at column 6
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
    let base = brightest(&shot, cell_rect(6, 6, 5, 1));
    let live = brightest(&shot, cell_rect(82, 6, 5, 1));

    if u16::from(base) * 5 >= u16::from(live) * 3 {
        let frame = keep_frame(&shot, &mut home, "diff_base_side");
        panic!(
            "the base text peaks at {base} beside the live text's {live}; the frame is {}",
            frame.display(),
        );
    }
}

/// The split diff's status row tints its base half toward removal and its
/// target half toward addition.
///
/// Row 49 is the status row. Column 30 lies in the base half and column 130 in
/// the target half.
#[test]
fn the_split_diff_s_status_halves_take_their_side_tints() {
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
        SIDE_BASE,
        "diff_status_base_half",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(130, 49, 1, 1),
        SIDE_TARGET,
        "diff_status_target_half",
    );
}

/// A frame in the base tint runs down both sides of the split diff's base
/// column and along its top, inside the pane's first row.
///
/// Row 3, `3 three`, is context with no hunk mark, so its only base-tint
/// pixels are the frame's two sides. The top bar fills the top three pixel
/// rows of row 1, under the tab bar.
#[test]
fn the_split_diff_frames_its_base_column() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    expect_base_frame_sides(&shot, &mut home, 3, "diff_base_frame");
    expect_base_frame_top(&shot, &mut home, 1, "diff_base_frame_top");
}

/// The split diff ends in its buffer's minimap strip on the editor surface, as
/// a plain editor does.
///
/// The single band keeps its seven columns over the split diff, as
/// [`expect_strip_band`] reads them. The file's lines draw their runs on the
/// band's first row.
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

/// The changes view frames its base column once, across its file headers.
///
/// Row 1 is the first file's header, which carries no hunk mark, so its only
/// base-tint pixels down the whole row are the frame's two sides. A frame for
/// each excerpt leaves the header outside every frame. The top bar fills the
/// row's top three pixel rows over the header's hairline, although the view's
/// stack stays composited at rest.
#[test]
fn the_changes_view_frames_its_base_column_across_the_headers() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>dc<Settle>",
    );
    expect_base_frame_sides(&shot, &mut home, 1, "changes_base_frame");
    expect_base_frame_top(&shot, &mut home, 1, "changes_base_frame_top");
}

/// The changes view ends in its stack's minimap strip on the editor surface,
/// in the band the split diff ends in.
///
/// The band keeps its seven columns, as [`expect_strip_band`] reads them. The
/// stack's first file header draws its run across the band's first row,
/// beside the runs of the file's lines.
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

/// The changes view heads each file with a row on the raised surface, between
/// two hairlines.
///
/// Row 1 is the first file's header. Its text is centered, so the cells from
/// column 2 to column 31 and from column 120 to column 149 hold none. The
/// hairlines take the top and bottom pixel rows of the header row. Over the
/// base column the frame's top bar covers the top one, so the check reads it
/// over the live column.
#[test]
fn the_changes_view_heads_each_file_on_the_raised_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>dc<Settle>",
    );

    let (x, y, width, height) = cell_rect(2, 1, 30, 1);
    expect_region(
        &shot,
        &mut home,
        (x, y, width, height),
        RAISED,
        "changes_header",
    );
    let (live_x, ..) = cell_rect(120, 1, 30, 1);
    expect_region(
        &shot,
        &mut home,
        (live_x, y, width, 1),
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
}

/// The changes view pins the File Header of the file under the stack's top
/// edge to the stack's first row while the stack scrolls under it.
///
/// The scene is 20 rows tall, so the stack shows 18. Two pages put the cursor
/// on the stack's last row and the scroll on the last page, which starts
/// inside hunks.txt, past its header and before its last row. Row 1 then shows
/// hunks.txt's header on the raised surface over its bottom hairline. Where
/// the cover fails, the resident pool's composite shows a stack row there.
///
/// The pinned header casts its shadow on row 2, which the stack's pool
/// composites. A strip 2 px under that row's top edge takes about 0.38 of
/// black and a strip 16 px under it none. The two strips share the row's
/// surface, so a diff tint or a hatch there cancels out.
#[test]
fn the_changes_view_pins_the_file_header_over_its_scrolling_stack() {
    let mut home = support::scratch_home();
    let mut session = spawn_scene(Scene {
        rows: 20,
        ..ermine_scene("diff-kinds", "hunks.txt", home.path())
    });
    play(&mut session, "<Space>dc<Settle><C-f><C-f><Settle>");
    let shot = session.snapshot().expect("a snapshot");
    drop(session);

    let (x, y, width, height) = cell_rect(2, 1, 30, 1);
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

    let (x, y, width, _) = cell_rect(20, 2, 12, 1);
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
/// row under the boards' hatch.
///
/// The diff-kinds stack lists hunks.txt fourth, and its fold row is row 19,
/// under the row of its line 6 on the base surface. The fold hides seven
/// lines, so the row carries one Fold Button, whose box covers most of columns
/// 1 and 2. The count starts one cell past the buttons and ends before column
/// 18. A text file's language has no outline query, so the row centers no
/// Scope. The cells from column 18 to the divider, and from column 84 to
/// column 139 right of it, hold no text. They take the base surface under the
/// hatch's lines, which the stack's resident pool composites from its pages.
#[test]
fn the_changes_view_folds_unchanged_lines_into_a_hatched_row() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "diff-kinds", "hunks.txt", "<Space>dc<Settle>");

    expect_region(
        &shot,
        &mut home,
        cell_rect(2, 18, 30, 1),
        BASE,
        "changes_row_above_fold",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(1, 19, 2, 1),
        FOLD_BUTTON,
        "changes_fold_button",
    );
    for (rect, what) in [
        (cell_rect(18, 19, 52, 1), "changes_fold_row"),
        (cell_rect(84, 19, 56, 1), "changes_fold_row_center"),
    ] {
        expect_hatch(&shot, &mut home, rect, (BASE, HATCH), what);
    }
}

/// The panes sit one chrome column in from each window edge, and a blank text
/// cell inside them stays on the base surface.
#[test]
fn the_panes_sit_one_chrome_column_in_from_each_window_edge() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");

    for (col, want, what) in [
        (0, CHROME, "left_margin"),
        (COLS - 1, CHROME, "right_margin"),
        (80, BASE, "blank_text_cell"),
    ] {
        expect_region(&shot, &mut home, cell_rect(col, 4, 1, 1), want, what);
    }
}

/// Each pane's island darkens a band inside its bottom edge and shades nothing
/// on the margin beside it.
///
/// The pane starts at pixel column 9, past the one-column margin, so pixel
/// column 8 keeps the chrome ground. Row 48 is the pane's last content row,
/// above the status row. The band rises twelve pixels from the pane's bottom
/// edge, so row 47 keeps the base surface. Row 48's last pixel row reads the
/// boards' bottom row within one in every channel, and the fifth from its
/// bottom reads darker than the base surface in every channel. The fifth lies
/// past the island's two-pixel corner radius, so it reads the band only where
/// the band reaches past the corner arcs.
#[test]
fn the_island_s_bottom_band_darkens_inside_the_pane() {
    // The base surface under 0.27 of black, the boards' bottom pixel row.
    const BAND_EDGE: [u8; 3] = [0x11, 0x12, 0x16];

    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    let (_, y, _, height) = cell_rect(0, 4, 1, 1);

    expect_region(
        &shot,
        &mut home,
        (8, y, 1, height),
        CHROME,
        "margin_beside_the_pane",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(80, ROWS - 3, 1, 1),
        BASE,
        "row_above_the_band",
    );

    let (x, y, width, height) = cell_rect(80, ROWS - 2, 1, 1);
    let edge = dominant_color(&shot, (x, y + height - 1, width, 1));
    let fifth = dominant_color(&shot, (x, y + height - 5, width, 1));
    let at_edge = edge
        .iter()
        .zip(BAND_EDGE)
        .all(|(&edge, board)| edge.abs_diff(board) <= 1);
    let darker = fifth.iter().zip(BASE).all(|(&fifth, base)| fifth < base);
    if at_edge && darker {
        return;
    }

    let frame = keep_frame(&shot, &mut home, "bottom_band");
    panic!(
        "the pane's bottom pixel row reads {}, not within one of {}, or the fifth from the \
         bottom reads {}, not darker than {}; the frame is {}",
        hex(edge),
        hex(BAND_EDGE),
        hex(fifth),
        hex(BASE),
        frame.display(),
    );
}

/// The status row sits on chrome, and its hairline takes the bar's own color,
/// so no line shows above the bar. Its branch glyph draws as a glyph.
///
/// The hairline is the top pixel row of the status row, read here over a
/// blank cell of the bar. The branch glyph, U+2387, draws in column 117, the
/// first glyph cell of the right-aligned repo segment. A glyph's strokes blend
/// through many colors there, and the empty box that a missing glyph draws
/// keeps to a few along its straight edges.
#[test]
fn the_status_row_sits_on_chrome_with_no_hairline() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    let (x, y, width, _) = cell_rect(80, ROWS - 1, 1, 1);

    let colors = distinct_colors(&shot, cell_rect(117, ROWS - 1, 1, 1));
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
        CHROME,
        "status_text_cell",
    );
    expect_region(
        &shot,
        &mut home,
        (x, y, width, 1),
        CHROME,
        "status_hairline",
    );
}

/// The tab bar sits on chrome over a lone tab, with the active tab on the base
/// surface, its top corners rounded off.
///
/// The bar leads with the workspace name, whose length follows the session's
/// random name, so the check reads row 0 as runs of color rather than at a
/// fixed column. The tab's left edge is the first base pixel of the row's
/// middle, where the edge runs straight, and the rounding takes the base
/// out of that column's top pixel.
#[test]
fn the_tab_bar_sits_on_chrome_with_the_active_tab_on_base() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "many-files", "src/alpha.txt", "");

    expect_row_runs(&shot, &mut home, 0, &[CHROME, BASE, CHROME], "tab_bar");

    let (_, top, _, height) = cell_rect(0, 0, 1, 1);
    let pixel = |x: u32, y: u32| {
        let at = ((y * shot.width + x) * 4) as usize;
        [shot.rgba[at], shot.rgba[at + 1], shot.rgba[at + 2]]
    };
    let middle = top + height / 2;
    let Some(left) = (0..shot.width).find(|&x| pixel(x, middle) == BASE) else {
        let frame = keep_frame(&shot, &mut home, "tab_bar_middle");
        panic!(
            "no pixel of row 0's middle reads {}; the frame is {}",
            hex(BASE),
            frame.display(),
        );
    };
    if pixel(left, top) == BASE {
        let frame = keep_frame(&shot, &mut home, "tab_corner");
        panic!(
            "the active tab's top-left pixel at x {left} reads {}, not rounded off; the frame \
             is {}",
            hex(BASE),
            frame.display(),
        );
    }
}

/// The file finder's box fills with the overlay surface where no text covers
/// it, and its preview pane sits on the base surface.
///
/// The overlay cell sits in the list column, under the fixture's two file
/// rows. The base cell sits in the preview, under the six lines of the file it
/// shows.
#[test]
fn the_file_finder_box_draws_on_the_overlay_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>p<Settle>",
    );

    expect_region(
        &shot,
        &mut home,
        cell_rect(40, 25, 1, 1),
        OVERLAY,
        "finder_box",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(111, 27, 1, 1),
        BASE,
        "finder_preview",
    );
}

/// The which-key box docks on the status bar as a card on the raised surface.
///
/// The cell two columns left of the window's right edge, two rows above the
/// status row, sits in the card's footer row.
#[test]
fn the_space_box_docks_as_a_card_on_the_raised_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "<Space><Settle>");

    expect_region(
        &shot,
        &mut home,
        cell_rect(157, 47, 1, 1),
        RAISED,
        "hints_card",
    );
}

/// The commits view lists the rebase fixture's history under the graph
/// header's row, with the lane graph left of its rows. The header row stays on
/// the base. A blank cell on the selected first row, the row under the header,
/// fills with the select surface, and one two rows below stays on the base.
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
        cell_rect(30, 1, 1, 1),
        BASE,
        "graph_header_row",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(30, 2, 1, 1),
        SELECT,
        "selected_commit_row",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(30, 4, 1, 1),
        BASE,
        "unselected_commit_row",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(120, 8, 1, 1),
        TINT_OURS,
        "added_preview_row",
    );
}

/// The diff view keeps the which-key box docked as the same card with no key
/// held, so the cell in the card's footer row is still on the raised surface.
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
        cell_rect(157, 47, 1, 1),
        RAISED,
        "diff_hints_card",
    );
}

/// The finder's selected row fills with the select surface and carries the
/// accent bar at its left edge.
///
/// The select cells form one run in one row. At 9-pixel cells the 3/16 bar
/// snaps to the first two pixel columns of the run's first cell. Each column is
/// read alone, because the renderer keeps a thinner bar one pixel wide, and a
/// two-column read of that bar ties.
#[test]
fn the_finder_marks_its_selected_row_with_the_select_fill_and_the_accent_bar() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "many-files",
        "src/alpha.txt",
        "<Space>p<Settle>",
    );

    let selected = cells_of(&shot, SELECT);
    let first = selected.first().copied();
    let one_run = first.is_some_and(|(col, row)| {
        let run = col..col + selected.len() as u16;
        selected == run.map(|col| (col, row)).collect::<Vec<_>>()
    });
    let Some((col, row)) = first.filter(|_| one_run) else {
        let frame = keep_frame(&shot, &mut home, "selected_row");
        panic!(
            "the select cells {selected:?} are not one run in one row; the frame is {}",
            frame.display(),
        );
    };

    let (x, y, _, height) = cell_rect(col, row, 1, 1);
    for (offset, what) in [(0, "selection_bar_left"), (1, "selection_bar_right")] {
        expect_region(&shot, &mut home, (x + offset, y, 1, height), ACCENT, what);
    }
}

/// The finder's header ends in its scope chip on the hover surface.
///
/// The chip reads `S-Tab`, a middle dot, and `git only`, with one cell of
/// padding at each end. So its cells form one run of 18 in one row, and no
/// other cell of the frame takes the hover color.
#[test]
fn the_finder_header_ends_in_a_chip_on_the_hover_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "many-files",
        "src/alpha.txt",
        "<Space>p<Settle>",
    );

    let chip = cells_of(&shot, HOVER);
    let one_run = chip.first().is_some_and(|&(col, row)| {
        chip == (col..col + 18).map(|col| (col, row)).collect::<Vec<_>>()
    });
    if !one_run {
        let frame = keep_frame(&shot, &mut home, "header_chip");
        panic!(
            "the hover cells {chip:?} are not one 18-cell run in one row; the frame is {}",
            frame.display(),
        );
    }
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
    let text = cell_rect(6, 1, 17, 1);
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

/// The finder ends in a footer row on the raised surface.
///
/// No other surface of the frame is raised, so the raised cells form one run
/// in one row. The docked key hints box waits for its toggle under the finder,
/// so the run spans the 126 cells inside the box's border.
#[test]
fn the_finder_ends_in_a_footer_on_the_raised_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "many-files",
        "src/alpha.txt",
        "<Space>p<Settle>",
    );

    let raised = cells_of(&shot, RAISED);
    if !one_run_in_one_row(&raised) || raised.len() != 126 {
        let frame = keep_frame(&shot, &mut home, "finder_footer");
        panic!(
            "the raised cells {raised:?} are not one footer run in one row; the frame is {}",
            frame.display(),
        );
    }
}

/// The session picker ends in a footer row on the raised surface.
///
/// No other surface of the frame is raised, so the raised cells form one run
/// in one row. The box holds its recommended 112 columns, so the footer spans
/// the 110 cells inside its border.
#[test]
fn the_session_picker_ends_in_a_footer_on_the_raised_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<C-a><C-f><Settle>",
    );

    let raised = cells_of(&shot, RAISED);
    if !one_run_in_one_row(&raised) || raised.len() != 110 {
        let frame = keep_frame(&shot, &mut home, "session_footer");
        panic!(
            "the raised cells {raised:?} are not one footer run across the box; the frame is {}",
            frame.display(),
        );
    }
}

/// The help header names both scopes. The active `applicable` sits on the hover
/// surface, and `all` follows one column later as text on the box's own
/// surface.
///
/// The active chip reads `applicable` with one cell of padding at each end, so
/// its cells form one run of 12 in one row, and no other cell of the frame
/// takes the hover color. The inactive chip's five cells start one column past
/// that run and keep the overlay fill.
#[test]
fn the_help_header_names_both_scopes() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "?<Settle>");

    let chip = cells_of(&shot, HOVER);
    let run = chip
        .first()
        .copied()
        .filter(|&(col, row)| chip == (col..col + 12).map(|col| (col, row)).collect::<Vec<_>>());
    let Some((col, row)) = run else {
        let frame = keep_frame(&shot, &mut home, "help_scope_chips");
        panic!(
            "the hover cells {chip:?} are not one 12-cell run in one row; the frame is {}",
            frame.display(),
        );
    };
    expect_region(
        &shot,
        &mut home,
        cell_rect(col + 13, row, 5, 1),
        OVERLAY,
        "help_inactive_scope",
    );
}

/// The help modal's detail pane fills with the raised surface.
///
/// The pane spans most rows of the box's right side, so its raised cells reach
/// many rows, where the footer alone raises only one.
#[test]
fn the_help_detail_draws_on_the_raised_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "?<Settle>");

    let mut rows: Vec<u16> = cells_of(&shot, RAISED)
        .into_iter()
        .map(|(_, row)| row)
        .collect();
    rows.dedup();
    if rows.len() < 10 {
        let frame = keep_frame(&shot, &mut home, "help_detail");
        panic!(
            "the raised cells reach rows {rows:?}, too few for a detail pane; the frame is {}",
            frame.display(),
        );
    }
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

/// A completion row of a function leads with its badge on the function tint.
///
/// The server loads the crate before it answers, so the completion key plays
/// again until the tint shows. The prefix `mai` names the fixture's `main`
/// function. Every badge sits in the box's first column, so the tinted cells
/// all share one column.
#[test]
fn a_function_completion_leads_with_its_badge_on_the_function_tint() {
    let mut home = support::scratch_home();
    let mut session = spawn_settled(home.path(), "rust-lsp", "src/main.rs");
    play(&mut session, "/unused<Enter>omai");

    let deadline = Instant::now() + LSP_DEADLINE;
    let (shot, tinted) = loop {
        play(&mut session, "<C-Space><Wait:2000>");
        let shot = session.snapshot().expect("a snapshot");
        let tinted = cells_of(&shot, TINT_ACCENT);
        if !tinted.is_empty() {
            break (shot, tinted);
        }
        assert!(
            Instant::now() < deadline,
            "no function completion showed before the deadline"
        );
    };

    if tinted.iter().any(|&(col, _)| col != tinted[0].0) {
        let frame = keep_frame(&shot, &mut home, "completion_badges");
        panic!(
            "the function tint cells {tinted:?} are not one badge column; the frame is {}",
            frame.display(),
        );
    }
}

/// A selection washes its range, the primary cursor is the terminal's own
/// opaque block, and a secondary cursor takes the theme's cursor block.
///
/// The text starts in cell 6, and the tab bar takes row 0. `jjvll` selects
/// `3 t` on row 3, so the wash covers the `3` and the blank cell after it, and
/// the primary cursor sits on the `t` in cell 8. `<Esc>CC` copies the
/// selection onto the next two rows, and the newest, on row 5, takes the
/// primary, which leaves the cursor on row 3 secondary.
#[test]
fn the_selection_scene_paints_the_wash_and_both_cursor_kinds() {
    let mut home = support::scratch_home();
    let mut session = spawn_settled(home.path(), "basic-diff", "unstaged.txt");

    play(&mut session, "jjvll<Settle>");
    let selected = session.snapshot().expect("a snapshot");
    expect_region(
        &selected,
        &mut home,
        cell_rect(7, 3, 1, 1),
        WASH,
        "selection_wash",
    );
    expect_region(
        &selected,
        &mut home,
        cell_rect(8, 3, 1, 1),
        CURSOR_PRIMARY,
        "primary_cursor",
    );

    play(&mut session, "<Esc>CC<Settle>");
    let copied = session.snapshot().expect("a snapshot");
    expect_region(
        &copied,
        &mut home,
        cell_rect(8, 3, 1, 1),
        CURSOR_SECONDARY,
        "secondary_cursor",
    );
    expect_region(
        &copied,
        &mut home,
        cell_rect(8, 5, 1, 1),
        CURSOR_PRIMARY,
        "copied_primary_cursor",
    );
}

/// A gzip file's pane rings its text in the dashed read-only hairline.
///
/// The tab bar takes row 0, so the first content row is row 1, and the
/// ring's top edge is that row's first pixel row. Three pixels in five carry
/// the dashed stroke, so the stroke's color leads a run of them, and the run
/// two pixels down lies under the stroke on the editor surface. Cells 20 to
/// 60 lie clear of the ring's rounded corners and of the label at its right
/// end.
#[test]
fn a_read_only_pane_draws_the_dashed_ring_over_its_text() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "read-only", "CHANGELOG.md.gz", "");
    let (x, y, width, _) = cell_rect(20, 1, 40, 1);

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
/// The fixture pauses a merge with one conflict in `file.txt`. The tab bar
/// takes row 0, and the minimap band takes the window's last seven columns
/// before its margin. So the view spans 151 cells, with sides of 48 cells at x
/// 1 and 104 and the merged column of 55 cells at x 49. The header is row 1,
/// `1 shared header` is row 2, and the chunk's first row is row 3, where each
/// column takes its side's tint. The ours bar sits 4/16 into its column, so at
/// 9-pixel cells it snaps to the third and fourth pixel columns of the
/// column's first cell.
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
        (5, 3, TINT_OURS, "ours_conflict_row"),
        (55, 3, TINT_CONFLICT, "merged_conflict_row"),
        (113, 3, TINT_THEIRS, "theirs_conflict_row"),
        (55, 1, MERGED_HEAD, "merged_header"),
        (5, 2, BASE, "context_row"),
    ] {
        expect_region(&shot, &mut home, cell_rect(col, row, 20, 1), want, what);
    }
    let (x, y, _, height) = cell_rect(1, 3, 1, 1);
    expect_region(&shot, &mut home, (x + 2, y, 2, height), SUCCESS, "ours_bar");
}

/// The tab bar ends in the conflict chip while the conflict view is focused.
///
/// The chip reads `1 unresolved · 1 / 1 chunks` at the Caption scale, 24
/// cells that end one cell before the bar's right edge, so cells 140 to 152
/// of row 0 lie inside it on its fill.
#[test]
fn the_tab_bar_ends_in_the_conflict_chip() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "conflict",
        "file.txt",
        "<C-;>Conflict<Enter><Settle>",
    );

    expect_region(
        &shot,
        &mut home,
        cell_rect(140, 0, 12, 1),
        CHIP_CONFLICT,
        "conflict_chip",
    );
}

/// The git status panel names the merge in progress in its banner and leads
/// its sections with the conflicted file.
///
/// The box hangs from row 2 with its right edge one cell in from the window's,
/// so its inner cells span columns 109 to 157 from row 3. Row 3 is the header,
/// whose rule is the row's last pixel row. Rows 4 to 6 are the banner, whose
/// bar is the first pixel column of column 109. Row 7 heads the conflicted
/// section, and row 8 holds its file, whose bar sits 4/16 into column 109.
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
        cell_rect(157, 3, 1, 1),
        BASE,
        "inner_top_right",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(140, 4, 15, 3),
        TINT_MERGE,
        "banner_rows",
    );
    let (x, y, width, _) = cell_rect(130, 3, 25, 1);
    expect_region(
        &shot,
        &mut home,
        (x, y + 17, width, 1),
        RAISED,
        "header_rule",
    );
    let (x, y, _, height) = cell_rect(109, 4, 1, 3);
    expect_region(&shot, &mut home, (x, y, 1, height), CONFLICT, "banner_bar");
    let (x, y, _, height) = cell_rect(109, 8, 1, 1);
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
/// With no banner, row 4 heads the staged section over its file on row 5, and
/// row 6 heads the unstaged section over its file on row 7.
#[test]
fn the_git_status_panel_bars_each_file_in_its_section_color() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        ":git status<Enter><Settle>",
    );

    for (row, want, what) in [(5, INFO, "staged_bar"), (7, MUTED, "unstaged_bar")] {
        let (x, y, _, height) = cell_rect(109, row, 1, 1);
        expect_region(&shot, &mut home, (x + 2, y, 2, height), want, what);
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

/// A session of the `fixture` scene with `file` open, once its first frame
/// settles.
fn spawn_settled(home: &Path, fixture: &str, file: &str) -> HeadlessSession {
    spawn_scene(ermine_scene(fixture, file, home))
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

/// Panics unless `row` of `shot` holds the two sides of the frame around a
/// split diff's base column, and no other base-tint pixel.
///
/// The left side is the first three pixel columns of the pane's first cell,
/// column 1, the boards' 3 px border. The right one is three pixel columns of
/// the divider column, 76, the middle of the pane's 151 cells. It starts a
/// pixel into the cell and ends at the hairline, which the even column draws
/// on the cell's fifth pixel column. A miss saves the frame as
/// [`expect_region`] does.
fn expect_base_frame_sides(shot: &Snapshot, home: &mut TempDir, row: u16, what: &str) {
    let (_, y, _, height) = cell_rect(0, row, 1, 1);
    let columns = bar_columns(shot, (y, height), SIDE_BASE);

    let cell = |col| {
        let (x, _, width, _) = cell_rect(col, row, 1, 1);
        x..x + width
    };
    let (left, right) = (cell(1), cell(76));
    let sides: Vec<u32> = [0, 1, 2]
        .map(|dx| left.start + dx)
        .into_iter()
        .chain([1, 2, 3].map(|dx| right.start + dx))
        .collect();
    if columns == sides {
        return;
    }

    let frame = keep_frame(shot, home, what);
    panic!(
        "{what}: the base tint fills pixel columns {columns:?} of row {row}, not {sides:?}, the \
         first three of {left:?} and the three past the first of {right:?}; the frame is {}",
        frame.display(),
    );
}

/// Panics unless the top three pixel rows of `row` in `shot` hold the top bar
/// of the frame around a split diff's base column, and the pixel row under them
/// does not.
///
/// The bar is one run of the base tint from the left side's first pixel column
/// to the right side's last, where the sides are the columns the tint fills
/// down the whole row. Below the bar the tint fills the sides alone. A miss
/// saves the frame as [`expect_region`] does.
fn expect_base_frame_top(shot: &Snapshot, home: &mut TempDir, row: u16, what: &str) {
    let (_, y, _, height) = cell_rect(0, row, 1, 1);
    let sides = bar_columns(shot, (y, height), SIDE_BASE);
    let top = bar_columns(shot, (y, 3), SIDE_BASE);
    let below = bar_columns(shot, (y + 3, 1), SIDE_BASE);

    let span: Vec<u32> = match (sides.first(), sides.last()) {
        (Some(&first), Some(&last)) => (first..=last).collect(),
        _ => Vec::new(),
    };
    if !span.is_empty() && top == span && below == sides {
        return;
    }

    let frame = keep_frame(shot, home, what);
    panic!(
        "{what}: the top three pixel rows of row {row} hold the base tint in columns {top:?} \
         and the row under them in {below:?}, not one run across the sides {sides:?} and the \
         sides alone; the frame is {}",
        frame.display(),
    );
}

/// Panics unless each of the single band's seven columns, 152 to 158, reads
/// as the editor surface from row 1 under the tab bar to row 48 above the
/// status row, and the band draws text runs on row 1.
///
/// The strip's runs, its marks, and its viewport band cover few of a column's
/// pixels, so each column reads as the surface that a text strip sits on. A
/// short plain-text file's strip rows fill row 1 and draw their runs in grey,
/// as [`grey_run_pixels`] counts them. A miss saves the frame as
/// [`expect_region`] does.
fn expect_strip_band(shot: &Snapshot, home: &mut TempDir, what: &str) {
    for col in 152..=158 {
        expect_region(shot, home, cell_rect(col, 1, 1, 48), BASE, what);
    }

    let runs = grey_run_pixels(shot, cell_rect(152, 1, 7, 1));
    if runs > 0 {
        return;
    }

    let frame = keep_frame(shot, home, what);
    panic!(
        "{what}: the band's first row holds no grey run pixel; the frame is {}",
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

/// The pixels of `rect` in `shot` that draw a minimap text run of a plain-text
/// file, whose class takes a grey.
///
/// A grey keeps its channels within `0x20` of each other, and a run's peaks
/// above `0x70`. The blue change lane, the surface, the viewport band, a row's
/// change tint, and a file header's bar all miss one of the two tests.
fn grey_run_pixels(shot: &Snapshot, (x, y, width, height): PixelRect) -> usize {
    (y..y + height)
        .flat_map(|row| {
            let start = ((row * shot.width + x) * 4) as usize;
            let end = start + (width * 4) as usize;
            shot.rgba[start..end].chunks_exact(4)
        })
        .filter(|pixel| {
            let high = pixel[0].max(pixel[1]).max(pixel[2]);
            let low = pixel[0].min(pixel[1]).min(pixel[2]);
            high > 0x70 && high - low <= 0x20
        })
        .count()
}

/// Whether `cells`, row by row, form one unbroken run in one row.
fn one_run_in_one_row(cells: &[(u16, u16)]) -> bool {
    cells.first().is_some_and(|&(first, row)| {
        let run = first..first + cells.len() as u16;
        cells == run.map(|col| (col, row)).collect::<Vec<_>>()
    })
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

fn hex([red, green, blue]: [u8; 3]) -> String {
    format!("#{red:02x}{green:02x}{blue:02x}")
}
