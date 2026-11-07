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
    collections::HashMap,
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
const HAIRLINE: [u8; 3] = [0x2e, 0x32, 0x3b];
const SELECT: [u8; 3] = [0x23, 0x32, 0x4a];
const DISABLED: [u8; 3] = [0x5b, 0x61, 0x6d];
const TINT_ACCENT: [u8; 3] = [0x22, 0x30, 0x49];
const DANGER: [u8; 3] = [0xf2, 0x77, 0x7f];
const SUCCESS: [u8; 3] = [0x8f, 0xd1, 0x8a];
const WARNING: [u8; 3] = [0xe8, 0xbd, 0x6a];
const SIDE_BASE: [u8; 3] = [0x2a, 0x1d, 0x20];
const SIDE_TARGET: [u8; 3] = [0x1d, 0x2a, 0x1f];
const WASH: [u8; 3] = [0x2a, 0x3c, 0x5a];
const CURSOR_SECONDARY: [u8; 3] = [0x8a, 0x90, 0x9c];
const READONLY_FRAME: [u8; 3] = [0x8a, 0x76, 0x50];
const TINT_OURS: [u8; 3] = [0x18, 0x24, 0x1b];
const TINT_THEIRS: [u8; 3] = [0x23, 0x1d, 0x2e];
const TINT_CONFLICT: [u8; 3] = [0x2a, 0x1b, 0x1d];
const MERGED_HEAD: [u8; 3] = [0x1b, 0x1d, 0x22];

/// The primary cursor's opaque block, the terminal palette's cursor color. The
/// color comes from stoatty's own palette rather than a theme scope.
const CURSOR_PRIMARY: [u8; 3] = [0xdf, 0xe2, 0xe8];

/// The wait for rust-analyzer to load the fixture crate and answer, the bound
/// the live LSP tests allow.
const LSP_DEADLINE: Duration = Duration::from_secs(90);

/// A pixel rectangle as `(x, y, width, height)`.
type PixelRect = (u32, u32, u32, u32);

/// The normal-mode chip fills with the accent, and a blank text cell with the
/// base surface.
#[test]
fn the_basic_diff_scene_draws_on_the_ermine_palette() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");

    expect_region(
        &shot,
        &mut home,
        cell_rect(2, ROWS - 1, 1, 1),
        ACCENT,
        "mode_chip",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(80, 10, 1, 1),
        BASE,
        "blank_text_cell",
    );
}

/// The modified row's git lanes sit left of its number, the hunk lane in the
/// modified color and the state lane in the unstaged color.
///
/// The pane starts one column in, past the window's chrome margin. At 9-pixel
/// cells the two 3/16 lanes snap to the first two pixel columns of that cell,
/// and the next two. The changed row is row 3, `4 four changed`.
#[test]
fn the_gutter_draws_the_hunk_and_state_lanes_left_of_the_number() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    let (x, y, _, height) = cell_rect(1, 3, 1, 1);

    expect_region(&shot, &mut home, (x, y, 2, height), WARNING, "hunk_lane");
    expect_region(
        &shot,
        &mut home,
        (x + 2, y, 2, height),
        DISABLED,
        "state_lane",
    );
}

/// A modified row's git bar reads removed on the base side and added on the
/// live side of the split diff.
///
/// The changed row is row 3, `4 four` on the left and `4 four changed` on the
/// right. A bar is too narrow to lead its cell's colors, so the check finds the
/// pixel columns the color fills over the row's whole height, which a glyph
/// never does.
#[test]
fn a_modified_row_reads_removed_on_the_base_side_and_added_on_the_live_side() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    let (_, y, _, height) = cell_rect(0, 3, 1, 1);
    let [removed, added, modified] =
        [DANGER, SUCCESS, WARNING].map(|color| bar_columns(&shot, (y, height), color));

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
}

/// The base side of the split diff recedes as a whole behind the live side.
///
/// Row 5, `6 six`, is context on both sides. The left text starts at column 6
/// and the right at column 85. The base side paints the context color, which
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
    let base = brightest(&shot, cell_rect(6, 5, 5, 1));
    let live = brightest(&shot, cell_rect(85, 5, 5, 1));

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
/// column.
///
/// Row 2, `3 three`, is context with no hunk mark, so its only base-tint
/// pixels are the frame's two sides.
#[test]
fn the_split_diff_frames_its_base_column() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>df<Settle>",
    );
    expect_base_frame_sides(&shot, &mut home, 2, "diff_base_frame");
}

/// The split diff ends in a one-cell scroll overview on the window ground.
///
/// The single band narrows to the overview at column 158, the last column
/// inside the island margin. Its change marks and the viewport band cover few
/// of the column's pixels, so the column reads as chrome. A text strip reads
/// as the editor surface.
#[test]
fn the_split_diff_ends_in_a_scroll_overview_on_chrome() {
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
        cell_rect(158, 0, 1, 49),
        CHROME,
        "diff_overview",
    );
}

/// The changes view frames its base column once, across its file headers.
///
/// Row 0 is the first file's header, which carries no hunk mark, so its only
/// base-tint pixels are the frame's two sides. A frame per excerpt would leave
/// the header outside every frame.
#[test]
fn the_changes_view_frames_its_base_column_across_the_headers() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>dc<Settle>",
    );
    expect_base_frame_sides(&shot, &mut home, 0, "changes_base_frame");
}

/// The changes view ends in the same one-cell scroll overview on the window
/// ground that the split diff ends in.
///
/// The single band narrows to the overview at column 158, as it does over the
/// split diff, so the column reads as chrome.
#[test]
fn the_changes_view_ends_in_a_scroll_overview_on_chrome() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>dc<Settle>",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(158, 0, 1, 49),
        CHROME,
        "changes_overview",
    );
}

/// The changes view heads each file with a row on the raised surface, between
/// two hairlines.
///
/// Row 0 is the first file's header. Its text is centered, so the cells from
/// column 2 to column 31 hold none. The hairlines take the top and bottom
/// pixel rows of the header row.
#[test]
fn the_changes_view_heads_each_file_on_the_raised_surface() {
    let mut home = support::scratch_home();
    let shot = settled_frame(
        home.path(),
        "basic-diff",
        "unstaged.txt",
        "<Space>dc<Settle>",
    );

    let (x, y, width, height) = cell_rect(2, 0, 30, 1);
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
}

/// The changes view folds the unchanged lines between two hunks into a fold
/// row on the raised surface.
///
/// The diff-kinds stack lists hunks.txt fourth, and its fold row is row 18,
/// under the row of its line 6 on the base surface. The fold row's count is
/// centered, so the cells from column 2 to column 31 hold none.
#[test]
fn the_changes_view_folds_unchanged_lines_into_a_raised_row() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "diff-kinds", "hunks.txt", "<Space>dc<Settle>");

    expect_region(
        &shot,
        &mut home,
        cell_rect(2, 17, 30, 1),
        BASE,
        "changes_row_above_fold",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(2, 18, 30, 1),
        RAISED,
        "changes_fold_row",
    );
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
        expect_region(&shot, &mut home, cell_rect(col, 3, 1, 1), want, what);
    }
}

/// Each pane's island halo shades the margin beside it, darkest at the pane's
/// edge and gone a few pixels out.
///
/// The pane starts at pixel column 9, past the one-column margin. The tucked
/// halo reaches 6 pixels out, so pixel column 8 takes a shade darker than the
/// ground in every channel, and columns 0 and 1 keep the chrome ground.
#[test]
fn the_island_halo_shades_the_margin_beside_the_pane() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    let (_, y, _, height) = cell_rect(0, 3, 1, 1);

    expect_region(
        &shot,
        &mut home,
        (0, y, 2, height),
        CHROME,
        "margin_past_the_halo",
    );
    let shade = dominant_color(&shot, (8, y, 1, height));
    if shade
        .iter()
        .zip(CHROME)
        .any(|(&shade, chrome)| shade >= chrome)
    {
        let frame = keep_frame(&shot, &mut home, "halo_edge");
        panic!(
            "the margin's last pixel column reads {}, not darker than {}; the frame is {}",
            hex(shade),
            hex(CHROME),
            frame.display(),
        );
    }
}

/// The status row sits on chrome, and its hairline takes the bar's own color,
/// so no line shows above the bar.
///
/// The hairline is the top pixel row of the status row, read here over a
/// blank cell of the bar.
#[test]
fn the_status_row_sits_on_chrome_with_no_hairline() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "basic-diff", "unstaged.txt", "");
    let (x, y, width, _) = cell_rect(80, ROWS - 1, 1, 1);

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

/// A second tab shows the tab bar on chrome, with the active tab on the base
/// surface.
///
/// The bar leads with the workspace name, whose length follows the session's
/// random name, so the check reads row 0 as runs of color rather than at a
/// fixed column.
#[test]
fn the_tab_bar_sits_on_chrome_with_the_active_tab_on_base() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "many-files", "src/alpha.txt", "<C-a>c<Settle>");

    expect_row_runs(&shot, &mut home, 0, &[CHROME, BASE, CHROME], "tab_bar");
}

/// The file finder's box fills with the overlay surface where no text covers
/// it. The cell sits in the list column, under the fixture's two file rows.
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
        cell_rect(30, 0, 1, 1),
        BASE,
        "graph_header_row",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(30, 1, 1, 1),
        SELECT,
        "selected_commit_row",
    );
    expect_region(
        &shot,
        &mut home,
        cell_rect(30, 3, 1, 1),
        BASE,
        "unselected_commit_row",
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
    let text = cell_rect(6, 0, 17, 1);
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
/// The text starts in cell 6. `jjvll` selects `3 t` on row 2, so the wash
/// covers the `3` and the blank cell after it, and the primary cursor sits on
/// the `t` in cell 8. `<Esc>CC` copies the selection onto the next two rows,
/// and the newest, on row 4, takes the primary, which leaves the cursor on row
/// 2 secondary.
#[test]
fn the_selection_scene_paints_the_wash_and_both_cursor_kinds() {
    let mut home = support::scratch_home();
    let mut session = spawn_settled(home.path(), "basic-diff", "unstaged.txt");

    play(&mut session, "jjvll<Settle>");
    let selected = session.snapshot().expect("a snapshot");
    expect_region(
        &selected,
        &mut home,
        cell_rect(7, 2, 1, 1),
        WASH,
        "selection_wash",
    );
    expect_region(
        &selected,
        &mut home,
        cell_rect(8, 2, 1, 1),
        CURSOR_PRIMARY,
        "primary_cursor",
    );

    play(&mut session, "<Esc>CC<Settle>");
    let copied = session.snapshot().expect("a snapshot");
    expect_region(
        &copied,
        &mut home,
        cell_rect(8, 2, 1, 1),
        CURSOR_SECONDARY,
        "secondary_cursor",
    );
    expect_region(
        &copied,
        &mut home,
        cell_rect(8, 4, 1, 1),
        CURSOR_PRIMARY,
        "copied_primary_cursor",
    );
}

/// A gzip file's pane rings its text in the dashed read-only hairline.
///
/// One tab shows no tab bar, so the first content row is row 0, and the
/// ring's top edge is that row's first pixel row. Three pixels in five carry
/// the dashed stroke, so the stroke's color leads a run of them, and the run
/// two pixels down lies under the stroke on the editor surface. Cells 20 to
/// 60 lie clear of the ring's rounded corners and of the label at its right
/// end.
#[test]
fn a_read_only_pane_draws_the_dashed_ring_over_its_text() {
    let mut home = support::scratch_home();
    let shot = settled_frame(home.path(), "read-only", "CHANGELOG.md.gz", "");
    let (x, y, width, _) = cell_rect(20, 0, 40, 1);

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
/// The fixture pauses a merge with one conflict in `file.txt`. One tab shows
/// no tab bar, and the minimap band takes the window's last seven columns
/// before its margin. So the view spans 151 cells, with sides of 48 cells at x
/// 1 and 104 and the merged column of 55 cells at x 49. The header is row 0,
/// `1 shared header` is row 1, and the chunk's first row is row 2, where each
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
        (5, 2, TINT_OURS, "ours_conflict_row"),
        (55, 2, TINT_CONFLICT, "merged_conflict_row"),
        (113, 2, TINT_THEIRS, "theirs_conflict_row"),
        (55, 0, MERGED_HEAD, "merged_header"),
        (5, 1, BASE, "context_row"),
    ] {
        expect_region(&shot, &mut home, cell_rect(col, row, 20, 1), want, what);
    }
    let (x, y, _, height) = cell_rect(1, 2, 1, 1);
    expect_region(&shot, &mut home, (x + 2, y, 2, height), SUCCESS, "ours_bar");
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
    let mut session =
        HeadlessSession::spawn(ermine_scene(fixture, file, home)).unwrap_or_else(|error| {
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

/// Panics unless `row` of `shot` holds the two sides of the frame around a
/// split diff's base column, and no other base-tint pixel.
///
/// The left side lies in the pane's first cell, column 1. The right one lies
/// in the divider column, 79, left of the hairline, which both diff screens
/// place beside their one-cell overview. A miss saves the frame as
/// [`expect_region`] does.
fn expect_base_frame_sides(shot: &Snapshot, home: &mut TempDir, row: u16, what: &str) {
    let (_, y, _, height) = cell_rect(0, row, 1, 1);
    let columns = bar_columns(shot, (y, height), SIDE_BASE);

    let cell = |col| {
        let (x, _, width, _) = cell_rect(col, row, 1, 1);
        x..x + width
    };
    let (left, right) = (cell(1), cell(79));
    let sides = [&left, &right].map(|side| columns.iter().any(|x| side.contains(x)));
    let stray = columns
        .iter()
        .any(|x| !left.contains(x) && !right.contains(x));
    if sides == [true, true] && !stray {
        return;
    }

    let frame = keep_frame(shot, home, what);
    panic!(
        "{what}: the base tint fills pixel columns {columns:?} of row {row}, not one side in \
         each of {left:?} and {right:?}; the frame is {}",
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

fn hex([red, green, blue]: [u8; 3]) -> String {
    format!("#{red:02x}{green:02x}{blue:02x}")
}
