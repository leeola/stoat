use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};
use std::sync::Arc;
use stoat::{
    display_map::DisplayPoint,
    render::{
        paint::{dim_rgb, luma, style_rgb, FAINT_CONTRAST_FLOOR},
        review::{
            diff_rich_gutter_scale, diff_soften_scale, render_diff_view, BufferNumbering,
            DiffColumns, DiffDials, DiffLayout, BASE_SIDE_SOFTEN, CONTEXT_SOFTEN, DIFF_SOFTEN_MAX,
            DIFF_SOFTEN_MIN, DIFF_TWO_COLUMN_MIN, DIVIDER_RULE_X, MODIFIED_ROW_SOFTEN, SOFTEN_CAP,
        },
    },
    test_fixture::render_review::{
        buffer_text, continued, diff_editor, hunk_fold_row_editor, left_num, left_text, line_text,
        long_line, number_cells, paint_cells, right_text, row_holding, wide_columns,
    },
    theme::{scope as s, scope::UI_SELECTION_EDITOR, Theme},
    workspace::diff::WorktreeBase,
};
use stoat_config::LineNumbers;
use stoat_text::Point;
use stoat_widgets::ApcScene;
use stoatty_protocol::command::{decode_stream, BorderStyle, Command};

/// The columns a harness's frame lays a diff view of files under a hundred
/// lines out in, rich as the harness's theme resolves it under stoatty.
fn harness_columns(h: &stoat::test_harness::TestHarness) -> DiffColumns {
    let theme = &h.stoat.theme;
    let rich = diff_rich_gutter_scale(
        h.stoat.stoatty,
        theme,
        theme.get(stoat::theme::scope::UI_TEXT),
    );
    let area = h.rendered_buffer().area;
    DiffColumns::compute(area, DiffLayout::diff_view(2, rich))
}

/// The cells the right line number shows in.
fn right_num(cols: &DiffColumns) -> std::ops::Range<u16> {
    number_cells(cols, cols.right_x, cols.right_num_x)
}

/// A view scrolled to the row of line 17, past a fold row over a modified
/// hunk's rows, paints its rows as the view from the top paints them from
/// that row on, in both layouts. The unified layout's unchanged rows show
/// their base lines, so a base count one short shows as a shifted text.
#[test]
fn a_view_scrolled_past_a_fold_row_in_a_hunk_paints_as_from_the_top() {
    for area in [Rect::new(0, 0, 120, 24), Rect::new(0, 0, 60, 24)] {
        let rows = |buf: &Buffer| -> Vec<String> {
            (0..buf.area.height)
                .map(|y| buffer_text(buf, y).trim_end().to_string())
                .collect()
        };
        let mut editor = hunk_fold_row_editor();
        let from_top = rows(&paint_cells(&mut editor, area));
        let scroll = editor
            .display_map
            .snapshot()
            .buffer_to_display(Point::new(16, 0))
            .row as usize;
        stoat::test_harness::editor::place_cursor(&mut editor, 16, 0);
        editor.scroll_row = scroll as u32;
        let scrolled = rows(&paint_cells(&mut editor, area));

        assert_eq!(
            scrolled[..from_top.len() - scroll],
            from_top[scroll..],
            "width {}",
            area.width,
        );
    }
}

#[test]
fn typing_in_diff_view_edits_the_real_buffer() {
    use stoat::{action_handlers::focused_editor_mut, test_harness::TestHarness};

    let mut h = TestHarness::with_size(40, 8);
    let path = h.write_file("a.txt", "abc\n");
    h.open_file(&path);
    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);

    h.type_keys("i");
    h.type_text("X");

    let text = focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .display_map
        .snapshot()
        .buffer_snapshot()
        .rope()
        .to_string();
    assert!(
        text.starts_with('X'),
        "inserting in the diff view lands in the real buffer: {text:?}"
    );
}

#[test]
fn diff_view_right_column_carries_syntax_colors() {
    use stoat::{action_handlers::focused_editor_mut, test_harness::TestHarness};

    let mut h = TestHarness::with_size(120, 10);
    let path = h.write_file("a.rs", "fn main() {}\n");
    h.open_file(&path);
    h.stoat.drive_background();
    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    h.snapshot();

    // A syntax-highlighted right column paints more than one foreground color
    // across the row's tokens.
    let buf = h.rendered_buffer();
    let mut colors = std::collections::HashSet::new();
    for x in right_text(&harness_columns(&h)) {
        let cell = &buf[(x, 0)];
        if cell.symbol().trim().is_empty() {
            continue;
        }
        colors.insert(format!("{:?}", cell.style().fg));
    }
    assert!(
        colors.len() >= 2,
        "the right column is syntax highlighted with distinct token colors: {colors:?}"
    );
}

#[test]
fn diff_view_left_column_carries_base_token_colors() {
    use stoat::{action_handlers::focused_editor_mut, test_harness::TestHarness};

    let mut h = TestHarness::with_size(120, 10);
    // The base carries rust keywords. The buffer differs, so the base line
    // renders as a deleted block in the left column.
    h.stage_review_scenario("/repo", &[("a.rs", "fn main() {}\n", "fn other() {}\n")]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(std::path::Path::new("/repo/a.rs"));
    h.settle_diff_jobs();
    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    h.snapshot();

    let buf = h.rendered_buffer();
    let left = left_text(&harness_columns(&h));
    let mut colors = std::collections::HashSet::new();
    for y in 0..buf.area.height {
        for x in left.clone() {
            let cell = &buf[(x, y)];
            if cell.symbol().trim().is_empty() {
                continue;
            }
            colors.insert(format!("{:?}", cell.style().fg));
        }
    }
    assert!(
        colors.len() >= 2,
        "the base column carries token colors plus the text fallback: {colors:?}"
    );
}

/// A removed call whose argument moved into a new line keeps its row in the
/// left column, between the lines that stand around it in the base.
#[test]
fn a_removed_line_with_a_moved_argument_keeps_its_left_row() {
    let base = concat!(
        "fn push(&mut self, keys: Keys) {\n",
        "    let page = &mut self.page;\n",
        "    D::insert(&mut page.indexes, keys, page.households.len() as u32);\n",
        "    if page.last_keys != Some(keys) {\n",
        "        D::insert(&mut self.page_indexes, keys, self.pages.len() as u32);\n",
        "        page.last_keys = Some(keys);\n",
        "    }\n",
        "}\n",
    );
    let buffer = concat!(
        "fn push(&mut self, keys: Keys) {\n",
        "    let page = &mut self.page;\n",
        "    if page.last_keys != Some(keys) {\n",
        "        let row = page.households.len() as u32;\n",
        "        if let Some(last_keys) = page.last_keys {\n",
        "            D::insert_range(&mut page.indexes, last_keys, page.run_start..row);\n",
        "        }\n",
        "        page.run_start = row;\n",
        "        D::insert(&mut self.page_indexes, keys, self.pages.len() as u32);\n",
        "        page.last_keys = Some(keys);\n",
        "    }\n",
        "}\n",
    );
    let h = diff_harness_sized(200, base, buffer);
    let left = left_text(&harness_columns(&h));
    let buf = h.rendered_buffer();

    let top = row_holding(buf, left.clone(), "fn push");
    let rows: Vec<String> = (top..top + 4)
        .map(|y| line_text(buf, y, left.clone()).trim().to_string())
        .collect();
    assert_eq!(
        rows,
        [
            "fn push(&mut self, keys: Keys) {",
            "let page = &mut self.page;",
            "D::insert(&mut page.indexes, keys, page.households.len() as u32);",
            "if page.last_keys != Some(keys) {",
        ],
    );
}

/// A width-120 diff-view harness over one `.rs` file diffed `base` ->
/// `buffer`, rendered once. The default theme resolves diff colors to RGB, so
/// the change washes engage.
///
/// Width 120 stays above the two-column threshold, with the separator at col
/// 59. [`harness_columns`] gives the columns either side of it.
fn diff_harness(base: &str, buffer: &str) -> stoat::test_harness::TestHarness {
    diff_harness_sized(120, base, buffer)
}

/// [`diff_harness`] with the tint dial off, for a soften check. The tint
/// drains the color out of unchanged rows, which recolors what the check
/// reads.
fn untinted_diff_harness(base: &str, buffer: &str) -> stoat::test_harness::TestHarness {
    let mut h = diff_harness(base, buffer);
    h.stoat.diff_tint = 0;
    h.snapshot();
    h
}

/// [`diff_harness`] at `width` columns. Below [`DIFF_TWO_COLUMN_MIN`] both
/// sides share one column.
fn diff_harness_sized(width: u16, base: &str, buffer: &str) -> stoat::test_harness::TestHarness {
    use stoat::{action_handlers::focused_editor_mut, test_harness::TestHarness};

    let mut h = TestHarness::with_size(width, 10);
    // The wash scans below address fixed side-by-side columns, so the pane must
    // span the full width. The single-minimap strip would reserve the right
    // edge and shift the columns.
    h.stoat.minimap_override = Some(false);
    // The diff view keeps its hints box up over the lower right, across the
    // rows the checks below read.
    h.stoat.screen_hints_hidden = true;
    // The row checks below read each live number as the row's own line.
    // Relative numbering counts the numbers from the cursor instead.
    h.stoat.settings.editor_line_numbers = Some(LineNumbers::Absolute);
    h.stage_review_scenario("/repo", &[("a.rs", base, buffer)]);
    h.stoat.set_diff_warm_auto(true);
    h.open_file(std::path::Path::new("/repo/a.rs"));
    h.settle_diff_jobs();
    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    h.snapshot();
    h
}

/// The status row reads the diff's metrics left of the divider and the
/// pane's own row right of it, with the cursor on the top row, and again
/// once the base switches to the index.
#[test]
fn the_diff_view_s_status_row_splits_into_metrics_and_a_pane_row() {
    let mut h = diff_harness("a\nb\nc\nold\n", "a\nb\nc\nnew\n");
    let head = status_row_in_both_frames(&mut h);

    h.stoat
        .active_workspace_mut()
        .set_worktree_base(WorktreeBase::Index);
    h.settle_diff_jobs();
    let index = status_row_in_both_frames(&mut h);

    // The divider is column 59 of the 120-wide body, and its cell holds
    // neither half's words.
    let halves = |row: &String| {
        let cells: Vec<char> = row.chars().collect();
        let words = |cells: &[char]| {
            let text: String = cells.iter().collect();
            text.replace('─', " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        };
        [words(&cells[..59]), words(&cells[60..])]
    };
    let expected = |base: &str| {
        [
            format!("vs {base} \u{b7} structural diff \u{b7} hunk - / 1 +1 \u{2212}1"),
            "NOR repo a.rs 1:1".to_string(),
        ]
    };
    assert_eq!(
        (head.each_ref().map(halves), index.each_ref().map(halves)),
        (
            [expected("HEAD"), expected("HEAD")],
            [expected("index"), expected("index")]
        ),
    );
}

/// The halves sit on the window ground on both sides of the diff's divider
/// column and on it, as the revised boards draw the footer. Only the mode
/// chip, which leads the target half from column 60, takes a fill of its
/// own.
#[test]
fn the_status_halves_sit_on_the_window_ground() {
    use ratatui::style::Color;

    let mut h = diff_harness("a\nb\nc\nold\n", "a\nb\nc\nnew\n");
    let fills = [true, false].map(|stoatty| {
        h.stoat.stoatty = stoatty;
        h.snapshot();
        let buf = h.rendered_buffer();
        let y = buf.area.height - 1;
        [0..60, 60..65, 65..120].map(|xs| bg_runs(buf, y, xs))
    });

    let rgb = |hex: u32| Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
    let expected = [0x0c0e11, 0x7fb0ff, 0x0c0e11].map(|hex| vec![rgb(hex)]);
    assert_eq!(fills, [expected.clone(), expected], "in both frames");
}

/// A diff view's minimap strip narrows the diff body, so the body's
/// divider sits at column 56 of 121, left of the content's middle. The
/// status halves meet there, under the divider's hairline. The base half
/// takes the divider's cell, and the target half's mode chip starts past
/// it.
#[test]
fn the_status_halves_meet_at_the_divider_beside_the_strip() {
    use ratatui::style::Color;

    let mut h = diff_harness_sized(121, "a\nb\nc\nold\n", "a\nb\nc\nnew\n");
    h.stoat.minimap_override = None;
    h.stoat.settings.editor_minimap = Some(stoat_config::MinimapMode::PerPane);
    h.snapshot();
    let strip = stoat::action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .minimap_rect;
    let divider = (56 * 16 + DIVIDER_RULE_X) as i16;
    let hairlines = content_bars(&mut h)
        .into_iter()
        .filter(|bar| bar.0 == divider && bar.2 == 1)
        .count();

    let buf = h.rendered_buffer();
    let y = buf.area.height - 1;
    let rgb = |hex: u32| Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
    assert_eq!(
        (
            strip.map(|strip| strip.width),
            [55, 56, 57].map(|x| buf[(x, y)].bg),
            hairlines,
        ),
        (Some(7), [0x0c0e11, 0x0c0e11, 0x7fb0ff].map(rgb), 1),
    );
}

/// A diff too narrow for two columns keeps one status bar on the window
/// ground past the mode chip, and reads the diff's metrics there after the
/// file name.
#[test]
fn a_unified_diff_reads_its_metrics_in_one_bar_on_the_ground() {
    use ratatui::style::Color;

    let mut h = diff_harness_sized(80, "a\nb\nc\nold\n", "a\nb\nc\nnew\n");
    let rgb = |hex: u32| Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
    let runs = [true, false].map(|stoatty| {
        h.stoat.stoatty = stoatty;
        h.snapshot();
        let buf = h.rendered_buffer();
        bg_runs(buf, buf.area.height - 1, 0..buf.area.width)
    });
    let words: Vec<String> = status_row_in_both_frames(&mut h)
        .iter()
        .map(|row| {
            row.replace('─', " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect();

    let expected = [0x7fb0ff, 0x0c0e11].map(rgb);
    let text = "NOR repo a.rs vs HEAD \u{b7} structural diff \u{b7} hunk - / 1 +1 \u{2212}1 1:1";
    assert_eq!(
        (runs, words),
        (
            [expected, expected].map(Vec::from),
            vec![text.to_string(); 2]
        ),
        "in both frames",
    );
}

#[test]
fn a_plain_pane_names_no_diff_sides() {
    let mut h = diff_harness("a\nb\nc\nold\n", "a\nb\nc\nnew\n");
    stoat::action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(false);
    h.type_keys("j");
    h.snapshot();
    assert!(
        !h.rendered_text().contains("working tree"),
        "{}",
        h.rendered_text()
    );
}

/// Render a frame and return every bar the focused pane's content lane
/// draws, in draw order, as `(x, y, width, height, color, occludes)` with the
/// rect in sixteenths.
fn content_bars(
    h: &mut stoat::test_harness::TestHarness,
) -> Vec<(i16, i16, u16, u16, [u8; 3], bool)> {
    let _ = h.stoat.render();
    let ws = h.stoat.active_workspace();
    let index = ws.panes.pane(ws.panes.focus()).index;
    decode_stream(
        h.stoat
            .apc_scene
            .lane_bytes(stoat::render::content_lane(index)),
    )
    .into_iter()
    .flat_map(|command| match command {
        Command::Bar(bar) => {
            vec![(bar.x, bar.y, bar.width, bar.height, bar.color, bar.occludes)]
        },
        Command::Bars(group) => group
            .bars
            .into_iter()
            .map(|(x, y, width, height)| (x, y, width, height, group.color, false))
            .collect(),
        _ => Vec::new(),
    })
    .collect()
}

/// The status row of the stoatty frame and of the fallback frame, in that
/// order.
fn status_row_in_both_frames(h: &mut stoat::test_harness::TestHarness) -> [String; 2] {
    [true, false].map(|stoatty| {
        h.stoat.stoatty = stoatty;
        h.snapshot();
        let buf = h.rendered_buffer();
        line_text(buf, buf.area.height - 1, 0..buf.area.width)
    })
}

/// Row `y`'s trimmed glyphs across each of `cols`.
///
/// A rich side's lanes and removal tick replay as `│` and `─` in the side's
/// first cell, ahead of its line number. A leading one is left out, so a
/// range over the number reads the number alone.
fn cells<const N: usize>(buf: &Buffer, y: u16, cols: [std::ops::Range<u16>; N]) -> [String; N] {
    cols.map(|cols| {
        line_text(buf, y, cols)
            .trim()
            .trim_start_matches(['│', '─'])
            .trim()
            .to_string()
    })
}

/// At width 120 the right column wraps at 51, the narrower side's width,
/// and the tail's row takes no line number.
#[test]
fn a_long_live_line_wraps_into_the_right_column() {
    let h = diff_harness("keep\n", &format!("keep\n{}\n", long_line(45, 44)));
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
    assert_eq!(
        [y, y + 1].map(|y| cells(buf, y, [right_num(&cols), right_text(&cols)])),
        [
            ["2".to_string(), "a".repeat(45)],
            [String::new(), continued("b".repeat(44))],
        ],
    );
}

/// The wrap fit sizes the gutters as the paint does, so the widest line the
/// wrap keeps whole keeps one row, whichever gutter the harness paints. The
/// wrap stops one cell short of the narrower text column.
#[test]
fn a_line_one_cell_short_of_the_text_column_keeps_one_row() {
    let width = {
        let cols = harness_columns(&diff_harness("old\n", "new\n"));
        cols.left_content_w.min(cols.right_content_w) - 1
    };
    let line = "a".repeat(width);
    let h = diff_harness("keep\ntail\n", &format!("keep\n{line}\ntail\n"));
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, right_text(&cols), &line);
    assert_eq!(
        cells(buf, y + 1, [right_text(&cols)]),
        ["tail".to_string()],
        "the line's next row is the next line, not a continuation"
    );
}

#[test]
fn an_unchanged_long_line_mirrors_its_wrapped_segments() {
    let long = long_line(45, 44);
    let h = diff_harness(&format!("{long}\nold\n"), &format!("{long}\nnew\n"));
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
    assert_eq!(
        [y, y + 1].map(|y| cells(buf, y, [left_text(&cols), right_text(&cols)])),
        [
            ["a".repeat(45), "a".repeat(45)],
            [continued("b".repeat(44)), continued("b".repeat(44))],
        ],
        "each left row holds the segment the right row beside it holds",
    );
}

#[test]
fn a_wrapped_row_keeps_the_base_numbers_in_step() {
    let long = long_line(45, 44);
    let h = diff_harness(&format!("{long}\nold\n"), &format!("{long}\nnew\n"));
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
    assert_eq!(
        (y..y + 3)
            .map(|y| cells(buf, y, [left_num(&cols), left_text(&cols)]))
            .collect::<Vec<_>>(),
        [
            ["1".to_string(), "a".repeat(45)],
            [String::new(), continued("b".repeat(44))],
            ["2".to_string(), "old".to_string()],
        ],
        "the continuation takes no base line, so the paired row reads line 2",
    );
}

/// The underline dial marks every span cell on any theme, which makes the
/// span's cells readable off the grid.
#[test]
fn a_change_span_on_a_continuation_row_marks_only_its_own_cells() {
    // 44 a's wrap the line before `TWO`, whichever gutter the harness
    // paints, so the changed word starts the continuation row's text, 4
    // cells in.
    let a = "a".repeat(44);
    let mut h = diff_harness(
        &format!("// {a} one two three\n"),
        &format!("// {a} one TWO three\n"),
    );
    h.stoat.diff_underline = true;
    h.snapshot();
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, right_text(&cols), "  \u{21b3} TWO three");
    let underlined = |y| {
        (right_text(&cols))
            .filter(|&x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        [underlined(y - 1), underlined(y)],
        [
            vec![],
            (cols.right_text_x + 4..cols.right_text_x + 7).collect()
        ]
    );
}

#[test]
fn the_review_cursor_lands_on_the_wrapped_segment() {
    let long = long_line(45, 44);
    let mut editor = diff_editor(&format!("{long}\n"), &format!("{long}\n"));
    editor.display_map.set_wrap_width(Some(51));
    stoat::test_harness::editor::place_cursor(&mut editor, 0, 50);
    let area = Rect::new(0, 0, 120, 8);
    let mut buf = Buffer::empty(area);
    render_diff_view(
        &mut editor,
        area,
        Style::default(),
        &Theme::empty(),
        &mut buf,
        true,
        None,
        Some(&mut ApcScene::new()),
        true,
        DiffDials::shipped(),
        BufferNumbering::Absolute,
        &[],
        false,
    );
    assert_eq!(
        editor.cursor_screen_cell,
        Some((wide_columns().right_text_x + 8, 1)),
        "byte 50 is the fifth text cell of the continuation row, past its 4-cell indent",
    );
}

#[test]
fn a_cursor_paints_below_a_deleted_block_on_the_top_row() {
    let mut editor = diff_editor("a\nd1\nd2\nd3\nb\nc\n", "a\nb\nc\n");
    editor.scroll_row = 1;
    stoat::test_harness::editor::place_cursor(&mut editor, 1, 0);
    let area = Rect::new(0, 0, 120, 8);
    let mut buf = Buffer::empty(area);
    render_diff_view(
        &mut editor,
        area,
        Style::default(),
        &Theme::empty(),
        &mut buf,
        true,
        None,
        Some(&mut ApcScene::new()),
        true,
        DiffDials::shipped(),
        BufferNumbering::Absolute,
        &[],
        false,
    );

    assert!(
        editor
            .display_map
            .snapshot()
            .display_to_buffer(DisplayPoint::new(1, 0))
            .is_none(),
        "the pane's top row is a deleted line",
    );
    assert_eq!(
        editor.cursor_screen_cell,
        Some((wide_columns().right_text_x, 3)),
        "the cursor on `b` paints three rows below a top row that is a deleted line",
    );
}

/// Press `keys` on the refined `fn other() {}` line of a diff, and return the
/// row's columns in the selection wash and in the cursor block.
///
/// stoatty draws the primary cursor itself, so the harness turns it off and
/// the cell fallback paints the block onto the grid. The shipped theme styles
/// `ui.cursor` as a filled block, so the block is the cells on its background.
fn wash_and_cursor_after(keys: &str) -> (Vec<u16>, Vec<u16>) {
    let mut h = diff_harness("fn main() {}\n", "fn other() {}\n");
    h.stoat.stoatty = false;
    h.type_keys(keys);
    h.snapshot();

    let sel_bg = h
        .stoat
        .theme
        .get(UI_SELECTION_EDITOR)
        .bg
        .expect("a selection background");
    let cursor_bg = h
        .stoat
        .theme
        .cursor_style()
        .bg
        .expect("a cursor block background");
    let buf = h.rendered_buffer();
    let row = row_holding(buf, right_text(&harness_columns(&h)), "fn other");
    let washed = (0..buf.area.width)
        .filter(|&x| buf[(x, row)].bg == sel_bg)
        .collect();
    let cursor = (0..buf.area.width)
        .filter(|&x| buf[(x, row)].bg == cursor_bg)
        .collect();
    (washed, cursor)
}

#[test]
fn a_word_end_motion_washes_the_selected_word() {
    assert_eq!(
        wash_and_cursor_after("e"),
        {
            let x = wide_columns().right_text_x;
            (vec![x], vec![x + 1])
        },
        "`e` selects `fn` with the cursor on the `n`",
    );
}

/// The base text beside the line is not the buffer the selection belongs
/// to, so the wash stays on the live column.
#[test]
fn a_line_selection_washes_only_the_live_column() {
    assert_eq!(
        wash_and_cursor_after("x"),
        {
            let x = wide_columns().right_text_x;
            ((x..x + 13).collect(), vec![x + 13])
        },
        "`x` washes the thirteen glyphs and puts the cursor on the newline",
    );
}

/// A line selection keeps the receded `fn` of a refined row below its
/// changed `other`, with syntax colors off and the recede at its deepest.
///
/// With syntax off the recede is the only mark of unchanged text. A wash
/// that lifts the faint `fn` past the floor makes it as bright as the
/// change, and the reader does not know which of the two is under it.
#[test]
fn a_selection_keeps_receded_text_dimmer_than_a_change() {
    let mut h = diff_harness("fn main() {}\n", "fn other() {}\n");
    h.stoat.stoatty = false;
    h.stoat.diff_syntax = false;
    h.stoat.diff_soften = DIFF_SOFTEN_MAX;
    h.type_keys("x");
    h.snapshot();

    let wash = style_rgb(h.stoat.theme.get(UI_SELECTION_EDITOR).bg).expect("an rgb wash");
    let cols = harness_columns(&h);
    let buf = h.rendered_buffer();
    let row = row_holding(buf, right_text(&cols), "fn other");
    let lift = |x: u16| {
        let fg = style_rgb(buf[(x, row)].style().fg).expect("an rgb foreground");
        luma(fg) - luma(wash)
    };
    let (receded, changed) = (lift(cols.right_text_x), lift(cols.right_text_x + 3));

    assert!(
        (receded - FAINT_CONTRAST_FLOOR).abs() < 0.02,
        "the receded `f` lifts to the floor above the wash and no further, not {receded}",
    );
    assert!(
        changed > receded,
        "the changed `o` reads brighter than the receded `f`: {changed} against {receded}",
    );
}

/// Two words take one-letter labels, `a` for `fn` and `b` for `other`. A
/// label covers the first letter of its word, so the row is found by the
/// base text beside it.
#[test]
fn goto_word_labels_mark_each_word_of_the_live_column() {
    let mut h = diff_harness("fn main() {}\n", "fn other() {}\n");
    h.type_keys("g w");
    h.snapshot();

    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let row = row_holding(buf, left_text(&cols), "fn main");
    let labels = (0..buf.area.width)
        .filter(|&x| {
            buf[(x, row)]
                .modifier
                .contains(Modifier::REVERSED | Modifier::BOLD)
        })
        .map(|x| (x, buf[(x, row)].symbol()))
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        [(cols.right_text_x, "a"), (cols.right_text_x + 3, "b")],
        "each word of the live column carries its label, and the base column none",
    );
}

#[test]
fn the_wrap_override_turns_the_diff_view_wrap_off() {
    let mut h = diff_harness("keep\n", &format!("keep\n{}\ntail\n", long_line(45, 44)));
    h.stoat.toggle_wrap();
    h.snapshot();
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
    assert_eq!(
        [y, y + 1].map(|y| cells(buf, y, [right_num(&cols), right_text(&cols)])),
        [
            [
                "2".to_string(),
                long_line(45, 44)[..cols.right_content_w].to_string()
            ],
            ["3".to_string(), "tail".to_string()],
        ],
        "the line keeps one row, cut at the column edge",
    );
}

/// A removed lone blank line takes a numbered row of its own on the left,
/// and so does the blank line that ends a removed run.
#[test]
fn a_removed_blank_line_at_a_runs_edge_takes_a_row() {
    let rows = |base: &str, count: u16| {
        let h = diff_harness(base, "keep\ntail\n");
        let buf = h.rendered_buffer();
        let cols = harness_columns(&h);
        let y = row_holding(buf, right_text(&cols), "keep");
        (y..y + count)
            .map(|y| {
                cells(
                    buf,
                    y,
                    [
                        left_num(&cols),
                        left_text(&cols),
                        right_num(&cols),
                        right_text(&cols),
                    ],
                )
            })
            .collect::<Vec<_>>()
    };
    let row = |cells: [&str; 4]| cells.map(String::from);

    assert_eq!(
        rows("keep\n\ntail\n", 3),
        [
            row(["1", "keep", "1", "keep"]),
            row(["2", "", "", ""]),
            row(["3", "tail", "2", "tail"]),
        ],
        "a lone blank line",
    );
    assert_eq!(
        rows("keep\ngone\n\ntail\n", 4),
        [
            row(["1", "keep", "1", "keep"]),
            row(["2", "gone", "", ""]),
            row(["3", "", "", ""]),
            row(["4", "tail", "2", "tail"]),
        ],
        "a run that ends in a blank line",
    );
}

/// The removed line's block takes a row per segment, and only its first row
/// takes the base number, so the context below it reads line 3.
#[test]
fn a_removed_long_line_spans_two_block_rows() {
    let long = long_line(45, 44);
    let h = diff_harness(&format!("keep\n{long}\ntail\n"), "keep\ntail\n");
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, left_text(&cols), &"a".repeat(45));
    assert_eq!(
        (y..y + 3)
            .map(|y| cells(buf, y, [left_num(&cols), left_text(&cols)]))
            .collect::<Vec<_>>(),
        [
            ["2".to_string(), "a".repeat(45)],
            [String::new(), continued("b".repeat(44))],
            ["3".to_string(), "tail".to_string()],
        ],
    );
}

/// A removed line's block hangs below every row of the line above it, so a
/// wrapped line above a deletion stays whole.
#[test]
fn a_removed_line_below_a_wrapped_line_follows_its_last_row() {
    let long = long_line(45, 44);
    let h = diff_harness(&format!("{long}\ngone\ntail\n"), &format!("{long}\ntail\n"));
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
    assert_eq!(
        (y..y + 4)
            .map(|y| cells(
                buf,
                y,
                [
                    left_num(&cols),
                    left_text(&cols),
                    right_num(&cols),
                    right_text(&cols)
                ]
            ))
            .collect::<Vec<_>>(),
        [
            ["1".into(), "a".repeat(45), "1".into(), "a".repeat(45)],
            [
                String::new(),
                continued("b".repeat(44)),
                String::new(),
                continued("b".repeat(44))
            ],
            ["2".into(), "gone".into(), String::new(), String::new()],
            ["3".into(), "tail".into(), "2".into(), "tail".into()],
        ],
    );
}

/// Two removed lines paint their rows one after the other, and the second
/// breaks into its own segments rather than the first line's.
#[test]
fn each_removed_line_breaks_into_its_own_segments() {
    let h = diff_harness(
        &format!("keep\nshort\n{}\ntail\n", long_line(45, 44)),
        "keep\ntail\n",
    );
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, left_text(&cols), "short");
    assert_eq!(
        (y..y + 4)
            .map(|y| cells(buf, y, [left_num(&cols), left_text(&cols)]))
            .collect::<Vec<_>>(),
        [
            ["2".to_string(), "short".to_string()],
            ["3".to_string(), "a".repeat(45)],
            [String::new(), continued("b".repeat(44))],
            ["4".to_string(), "tail".to_string()],
        ],
    );
}

/// At 120 columns the base text wraps at 51 and the line fits. At 110 it
/// wraps at 46, and both widths keep the two columns, so the wrap width
/// alone is what moves.
#[test]
fn a_resize_resplits_the_standing_blocks() {
    let long = long_line(40, 7);
    let mut h = diff_harness(&format!("keep\n{long}\ntail\n"), "keep\ntail\n");
    let rows = |h: &stoat::test_harness::TestHarness| {
        let buf = h.rendered_buffer();
        let left = left_text(&harness_columns(h));
        let y = row_holding(buf, left.clone(), &"a".repeat(40));
        [y, y + 1].map(|y| line_text(buf, y, left.clone()).trim().to_string())
    };
    assert_eq!(rows(&h), [long.clone(), "tail".to_string()]);

    // The 48-char line fits the left column at width 120 and not at 104,
    // whichever gutter the harness paints.
    h.resize(104, 10);
    h.snapshot();
    assert_eq!(
        rows(&h),
        ["a".repeat(40), continued("b".repeat(7))],
        "the standing block grows a row at the narrower width",
    );
}

/// A modified line and its base both outrun the columns, so each wraps
/// beside the other row for row, with the numbers on the first row alone.
#[test]
fn a_paired_base_line_wraps_beside_its_live_line() {
    let live = format!("{} {}", "c".repeat(45), "d".repeat(44));
    let h = diff_harness(
        &format!("keep\n{}\ntail\n", long_line(45, 44)),
        &format!("keep\n{live}\ntail\n"),
    );
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, left_text(&cols), &"a".repeat(45));
    assert_eq!(
        (y..y + 3)
            .map(|y| cells(
                buf,
                y,
                [
                    left_num(&cols),
                    left_text(&cols),
                    right_num(&cols),
                    right_text(&cols)
                ]
            ))
            .collect::<Vec<_>>(),
        [
            ["2".into(), "a".repeat(45), "2".into(), "c".repeat(45)],
            [
                String::new(),
                continued("b".repeat(44)),
                String::new(),
                continued("d".repeat(44))
            ],
            ["3".into(), "tail".into(), "3".into(), "tail".into()],
        ],
        "the base fits beside the live rows, so no block row follows",
    );
}

/// The base breaks into three segments beside a live line of one row, so
/// its last two sit in block rows below that live row.
#[test]
fn a_taller_base_line_pads_below_its_live_row() {
    let base = format!("{} {} {}", "a".repeat(45), "b".repeat(45), "c".repeat(20));
    let h = diff_harness(
        &format!("keep\n{base}\ntail\n"),
        &format!("keep\n{}\ntail\n", "x".repeat(40)),
    );
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, left_text(&cols), &"a".repeat(45));
    assert_eq!(
        (y..y + 4)
            .map(|y| cells(
                buf,
                y,
                [
                    left_num(&cols),
                    left_text(&cols),
                    right_num(&cols),
                    right_text(&cols)
                ]
            ))
            .collect::<Vec<_>>(),
        [
            ["2".into(), "a".repeat(45), "2".into(), "x".repeat(40)],
            [
                String::new(),
                continued("b".repeat(45)),
                String::new(),
                String::new()
            ],
            [
                String::new(),
                continued("c".repeat(20)),
                String::new(),
                String::new()
            ],
            ["3".into(), "tail".into(), "3".into(), "tail".into()],
        ],
    );
}

/// A base of three segments beside a live line of two rows overflows by
/// one, and that block row follows the live line's last row.
#[test]
fn a_base_line_overflows_below_the_last_row_of_a_wrapped_live_line() {
    let base = format!("{} {} {}", "p".repeat(45), "q".repeat(45), "r".repeat(20));
    let h = diff_harness(
        &format!("keep\n{base}\ntail\n"),
        &format!("keep\n{}\ntail\n", long_line(45, 44)),
    );
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, left_text(&cols), &"p".repeat(45));
    assert_eq!(
        (y..y + 4)
            .map(|y| cells(
                buf,
                y,
                [
                    left_num(&cols),
                    left_text(&cols),
                    right_num(&cols),
                    right_text(&cols)
                ]
            ))
            .collect::<Vec<_>>(),
        [
            ["2".into(), "p".repeat(45), "2".into(), "a".repeat(45)],
            [
                String::new(),
                continued("q".repeat(45)),
                String::new(),
                continued("b".repeat(44))
            ],
            [
                String::new(),
                continued("r".repeat(20)),
                String::new(),
                String::new()
            ],
            ["3".into(), "tail".into(), "3".into(), "tail".into()],
        ],
    );
}

#[test]
fn a_shorter_base_line_leaves_the_live_continuation_blank_on_the_left() {
    let h = diff_harness(
        &format!("keep\n{}\ntail\n", "x".repeat(40)),
        &format!("keep\n{}\ntail\n", long_line(45, 44)),
    );
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, right_text(&cols), &"a".repeat(45));
    assert_eq!(
        (y..y + 3)
            .map(|y| cells(
                buf,
                y,
                [
                    left_num(&cols),
                    left_text(&cols),
                    right_num(&cols),
                    right_text(&cols)
                ]
            ))
            .collect::<Vec<_>>(),
        [
            ["2".into(), "x".repeat(40), "2".into(), "a".repeat(45)],
            [
                String::new(),
                String::new(),
                String::new(),
                continued("b".repeat(44))
            ],
            ["3".into(), "tail".into(), "3".into(), "tail".into()],
        ],
    );
}

/// At width 80 both sides share one column, which wraps at the width past
/// its one gutter.
#[test]
fn a_narrow_diff_view_wraps_at_its_single_column() {
    let h = diff_harness_sized(80, "keep\n", &format!("keep\n{}\n", long_line(65, 24)));
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let y = row_holding(buf, right_text(&cols), &"a".repeat(65));
    assert_eq!(
        [y, y + 1].map(|y| cells(buf, y, [left_num(&cols), right_text(&cols)])),
        [
            ["2".to_string(), "a".repeat(65)],
            [String::new(), continued("b".repeat(24))],
        ],
    );
}

/// The changed word is marked by everything around it receding, not by a
/// color behind it, so a refined row is one flat stretch of the editor
/// background from end to end. See
/// `diff_view_softens_the_unchanged_gaps_of_a_refined_row` for the receding
/// half of that.
#[test]
fn diff_view_paints_no_background_on_a_refined_row() {
    // `main` becomes `other`, so only that one word changed on the line.
    let h = diff_harness("fn main() {}\n", "fn other() {}\n");
    let canvas = h
        .stoat
        .theme
        .try_get(stoat::theme::scope::UI_BACKGROUND)
        .and_then(|s| s.bg)
        .expect("an rgb editor background");
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    let row = (0..buf.area.height)
        .find(|&y| line_text(buf, y, right_text(&cols)).contains("other"))
        .expect("the changed line rendered on the right");
    assert!(
        line_text(buf, row, left_text(&cols)).contains("main"),
        "and its base word beside it: {:?}",
        line_text(buf, row, left_text(&cols))
    );

    let painted = (0..buf.area.width)
        .filter(|&x| buf[(x, row)].bg != canvas)
        .collect::<Vec<_>>();
    assert_eq!(
        painted,
        Vec::<u16>::new(),
        "no cell of the row carries a background of its own",
    );
}

#[test]
fn diff_view_softens_the_unchanged_gaps_of_a_refined_row() {
    // `main` becomes `other` on the first line, so that line is refined.
    // The second is a pure insertion, which nothing refines, so its
    // identical `fn` keyword stays at full strength to compare against.
    let h = untinted_diff_harness("fn main() {}\n", "fn other() {}\nfn extra() {}\n");

    let bg = style_rgb(
        h.stoat
            .theme
            .try_get(stoat::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg),
    )
    .expect("rgb background");
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    let row_with = |text: &str| {
        (0..buf.area.height)
            .find(|&y| line_text(buf, y, right_text(&cols)).contains(text))
            .unwrap_or_else(|| panic!("{text:?} rendered in the right column"))
    };
    let fg_of = |y: u16, glyph: &str| {
        let x = right_text(&cols)
            .find(|&x| buf[(x, y)].symbol() == glyph)
            .unwrap_or_else(|| panic!("{glyph:?} on row {y}"));
        buf[(x, y)].style().fg.expect("an fg")
    };

    let refined = row_with("fn other");
    let added = row_with("fn extra");

    let full_keyword = fg_of(added, "f");
    let [r, g, b] = dim_rgb(
        style_rgb(Some(full_keyword)).expect("rgb fg on the added row"),
        bg,
        MODIFIED_ROW_SOFTEN,
    );

    assert_eq!(
        (fg_of(refined, "f"), fg_of(refined, "o")),
        (Color::Rgb(r, g, b), fg_of(added, "e")),
        "the gaps blend toward the background while the changed word stays full"
    );
    assert_ne!(
        fg_of(refined, "f"),
        full_keyword,
        "the gap blend moves the color off full strength"
    );
}

/// Inside a string the whole literal carries one color, so the receding
/// around a changed char is all that separates it. Bold gives the eye a
/// mark that the color does not give. The underline shows where the changed
/// run starts and ends among the unchanged words.
#[test]
fn diff_view_bolds_a_changed_char_inside_a_string() {
    // The two literals share most of their text, so the search reads them
    // as one edited string. Bold marks the case the reader compares char by
    // char, which is what a pair means and a delete beside an add does not.
    let h = diff_harness(
        "fn f() {\n    g(\"one aaa three\");\n}\n",
        "fn f() {\n    g(\"one zzz three\");\n}\n",
    );
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    let changed_word = ("zzz".to_string(), "aaa".to_string());
    assert_eq!(
        (
            row_cells_with(buf, &cols, "zzz", Modifier::BOLD),
            row_cells_with(buf, &cols, "zzz", Modifier::UNDERLINED),
        ),
        (changed_word.clone(), changed_word),
        "both columns bold and underline their changed word and nothing else of the literal"
    );
}

/// The search reads two similar comments as a pair rather than as two
/// unrelated runs, and the pair has to reach the reader. Without it the row
/// arrives gutter-marked with nothing receding and nothing bold, saying
/// only that the line changed somewhere.
#[test]
fn diff_view_marks_the_changed_word_of_a_similar_comment() {
    let h = diff_harness("// step aaa here\n", "// step zzz here\n");
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    let changed_word = ("zzz".to_string(), "aaa".to_string());
    assert_eq!(
        (
            row_cells_with(buf, &cols, "zzz", Modifier::BOLD),
            row_cells_with(buf, &cols, "zzz", Modifier::UNDERLINED),
        ),
        (changed_word.clone(), changed_word),
        "each column marks its own changed word and leaves the rest of the comment alone"
    );
}

/// A comment whose every word changed keeps no text around the edit to
/// set it apart from, so it bolds as a replacement and takes no underline.
/// Its `//` marker survives the rewrite but carries no words of its own.
#[test]
fn a_comment_rewritten_as_a_whole_takes_no_underline() {
    let h = diff_harness("// aaa bbb\n", "// zzz yyy\n");
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    assert_eq!(
        (
            row_cells_with(buf, &cols, "zzz", Modifier::BOLD),
            row_cells_with(buf, &cols, "zzz", Modifier::UNDERLINED),
        ),
        (
            ("zzz yyy".to_string(), "aaa bbb".to_string()),
            (String::new(), String::new()),
        ),
        "both columns bold the rewritten words and underline nothing"
    );
}

/// Bold and the underline say "these chars differ from the ones beside them
/// in the other text". Added prose has no counterpart to differ from, so it
/// stays plain however unstructured it is.
#[test]
fn diff_view_neither_bolds_nor_underlines_an_added_comment() {
    // The buffer opens two changed runs against the base's one, so the
    // pairing pass claims `h` and leaves the comment with no counterpart.
    let h = diff_harness(
        "fn f() {\n    g();\n}\n",
        "fn f() {\n    h();\n    // note here\n}\n",
    );
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    assert_eq!(
        (
            row_cells_with(buf, &cols, "note here", Modifier::BOLD),
            row_cells_with(buf, &cols, "note here", Modifier::UNDERLINED),
        ),
        (
            (String::new(), String::new()),
            (String::new(), String::new()),
        ),
        "an added comment carries no bold and no underline"
    );
}

/// A renamed identifier already has a token boundary and a color change
/// beside it, so bolding it would mark what the reader can see.
#[test]
fn diff_view_keeps_a_renamed_identifier_unbolded() {
    let h = diff_harness("fn alpha() {}\n", "fn beta() {}\n");
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    let row = (0..buf.area.height)
        .find(|&y| line_text(buf, y, right_text(&cols)).contains("beta"))
        .expect("the renamed line rendered on the right");
    let bold = right_text(&cols)
        .filter(|&x| buf[(x, row)].modifier.contains(Modifier::BOLD))
        .map(|x| buf[(x, row)].symbol().to_string())
        .collect::<String>();
    assert_eq!(bold, "", "nothing on a code row bolds");
}

/// The bold dial marks a change by weight whatever its kind, so it bolds the
/// code rename that the prose rule leaves plain, and nothing else of the row.
#[test]
fn the_bold_dial_bolds_a_code_change_span() {
    assert_eq!(
        (
            renamed_row_cells_with(DiffDials::shipped(), Modifier::BOLD),
            renamed_row_cells_with(
                DiffDials {
                    bold: true,
                    ..DiffDials::shipped()
                },
                Modifier::BOLD,
            ),
        ),
        (
            (String::new(), String::new()),
            ("beta".to_string(), "alpha".to_string()),
        ),
        "the dial alone bolds the rename, in each column and on nothing else"
    );
}

/// On a theme that blends, only the receding marks a change span. The
/// underline dial adds an underline to every span there, and to nothing
/// else of the row.
#[test]
fn the_underline_dial_underlines_a_change_span_on_an_rgb_theme() {
    assert_eq!(
        (
            renamed_row_cells_with(DiffDials::shipped(), Modifier::UNDERLINED),
            renamed_row_cells_with(
                DiffDials {
                    underline: true,
                    ..DiffDials::shipped()
                },
                Modifier::UNDERLINED,
            ),
        ),
        (
            (String::new(), String::new()),
            ("beta".to_string(), "alpha".to_string()),
        ),
        "the dial alone underlines the rename, in each column and on nothing else"
    );
}

/// The glyphs that carry `modifier` on the changed row of an `alpha` to
/// `beta` rename, right column then left, painted with `dials`.
///
/// The harness diffs a `.rs` file, so the rename refines as code. A
/// plain-text editor refines as prose, which bolds whatever the dial says.
fn renamed_row_cells_with(dials: DiffDials, modifier: Modifier) -> (String, String) {
    let mut h = diff_harness("fn alpha() {}\n", "fn beta() {}\n");
    let theme = Arc::clone(&h.stoat.theme);
    let fallback = theme.get(stoat::theme::scope::UI_TEXT);
    let area = Rect::new(0, 0, 120, 4);
    let editor = stoat::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    let mut buf = Buffer::empty(area);
    render_diff_view(
        editor,
        area,
        fallback,
        &theme,
        &mut buf,
        true,
        None,
        None,
        true,
        dials,
        BufferNumbering::Absolute,
        &[],
        false,
    );
    row_cells_with(&buf, &wide_columns(), "beta", modifier)
}

/// The glyphs that carry `modifier` on the row whose right column shows
/// `needle`, right column then left, read across `cols`.
fn row_cells_with(
    buf: &Buffer,
    cols: &DiffColumns,
    needle: &str,
    modifier: Modifier,
) -> (String, String) {
    let row = (0..buf.area.height)
        .find(|&y| line_text(buf, y, right_text(cols)).contains(needle))
        .unwrap_or_else(|| panic!("{needle:?} rendered in the right column"));
    let marked = |cells: std::ops::Range<u16>| {
        cells
            .filter(|&x| buf[(x, row)].modifier.contains(modifier))
            .map(|x| buf[(x, row)].symbol().to_string())
            .collect::<String>()
    };
    (marked(right_text(cols)), marked(left_text(cols)))
}

/// A theme that cannot blend has no receding to lead a change with, so the
/// view asks for the one mark left to it and underlines the changed chars.
/// `paint_base_row_underlines_change_spans_on_a_theme_that_cannot_blend`
/// pins the paint; this pins that the view asks for it.
#[test]
fn diff_view_underlines_a_change_span_on_a_theme_that_cannot_blend() {
    // `main` becomes `other`, so only that one word changed on the line.
    let mut h = diff_harness("fn main() {}\n", "fn other() {}\n");
    h.stoat.theme = Arc::new(Theme::empty());
    // Both theme installs move the paint generation, which is what tells a
    // replayed pane to paint under the new theme.
    h.stoat.paint_generation += 1;
    h.snapshot();
    let buf = h.rendered_buffer();

    let underlined = (0..buf.area.height)
        .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
        .filter(|&pos| buf[pos].modifier.contains(Modifier::UNDERLINED))
        .map(|pos| buf[pos].symbol().to_string())
        .collect::<String>();
    assert_eq!(
        underlined, "mainother",
        "the changed word is underlined on both sides, and nothing else is"
    );
}

/// The base column recedes as a whole, so a removed line's text sits
/// [`BASE_SIDE_SOFTEN`] of the way from the removed color toward the
/// background. With syntax off, the removed color is the whole line's.
///
/// The dial's bottom turns the fade off, and its top leaves it at the
/// shipped amount.
#[test]
fn diff_view_fades_a_removed_base_row_toward_the_background() {
    let mut h = diff_harness("fn keep() {}\nfn gone() {}\n", "fn keep() {}\n");
    h.stoat.diff_syntax = false;
    let removed_fg = |h: &mut stoat::test_harness::TestHarness, soften| {
        h.stoat.diff_soften = soften;
        h.snapshot();
        let buf = h.rendered_buffer();
        let cols = harness_columns(h);
        let y = (0..buf.area.height)
            .find(|&y| line_text(buf, y, left_text(&cols)).contains("fn gone"))
            .expect("the removed line renders in the left column");
        let x = left_text(&cols)
            .find(|&x| !buf[(x, y)].symbol().trim().is_empty())
            .expect("a glyph on the removed row");
        buf[(x, y)].fg
    };
    let fgs = [DIFF_SOFTEN_MIN, 0, DIFF_SOFTEN_MAX].map(|soften| removed_fg(&mut h, soften));

    let theme = &h.stoat.theme;
    let bg = style_rgb(theme.get(stoat::theme::scope::UI_BACKGROUND).bg).expect("rgb bg");
    let deleted = style_rgb(theme.get(s::DIFF_DELETED).fg).expect("rgb deleted color");
    let faded = |amount| {
        let [r, g, b] = dim_rgb(deleted, bg, amount);
        Color::Rgb(r, g, b)
    };
    assert_eq!(fgs, [0.0, BASE_SIDE_SOFTEN, BASE_SIDE_SOFTEN].map(faded));
}

/// The deepest soften leaves a refined base row's changed word readable.
///
/// The row's unchanged text recedes further, and the changed word keeps
/// the side fade's shipped amount. `main` became `other`, so the `m` is a
/// changed char and the `f` is an unchanged one on the same row.
#[test]
fn the_deepest_soften_keeps_a_changed_base_word_readable() {
    let mut h = diff_harness(
        "fn keep() {}\nfn main() {}\n",
        "fn keep() {}\nfn other() {}\n",
    );
    h.stoat.diff_syntax = false;
    h.stoat.diff_soften = DIFF_SOFTEN_MAX;
    h.snapshot();

    let bg = style_rgb(h.stoat.theme.get(stoat::theme::scope::UI_BACKGROUND).bg).expect("rgb bg");
    let cols = harness_columns(&h);
    let buf = h.rendered_buffer();
    let row = row_holding(buf, left_text(&cols), "fn main");
    let distance = |x: u16| {
        let fg = style_rgb(buf[(x, row)].style().fg).expect("an rgb foreground");
        (luma(fg) - luma(bg)).abs()
    };
    let (unchanged, changed) = (distance(cols.left_text_x), distance(cols.left_text_x + 3));

    assert!(
        changed >= FAINT_CONTRAST_FLOOR,
        "the changed `m` stands {changed} from the background, under the floor",
    );
    assert!(
        changed > unchanged,
        "the changed `m` stands further from the background than the unchanged `f`: \
         {changed} against {unchanged}",
    );
}

/// The shipped dials are the ones a fresh session paints with, so a modal
/// preview and a plain pane paint as a fresh diff view does.
#[test]
fn the_shipped_dials_are_a_fresh_sessions() {
    let h = stoat::test_harness::TestHarness::with_size(40, 10);
    assert_eq!(DiffDials::from_stoat(&h.stoat), DiffDials::shipped());
}

#[test]
fn diff_view_softens_context_row_foregrounds() {
    // The first line is untouched, so it renders as a context row. The
    // second is a pure insertion, which nothing softens, so its identical
    // `fn` keyword is the full-strength reference.
    let h = untinted_diff_harness("fn keep() {}\n", "fn keep() {}\nfn add() {}\n");

    let bg = style_rgb(
        h.stoat
            .theme
            .try_get(stoat::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg),
    )
    .expect("rgb background");
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    let row_with = |text: &str| {
        (0..buf.area.height)
            .find(|&y| line_text(buf, y, right_text(&cols)).contains(text))
            .unwrap_or_else(|| panic!("{text:?} rendered in the right column"))
    };
    let leading_glyph = |y: u16| {
        let x = right_text(&cols)
            .find(|&x| !buf[(x, y)].symbol().trim().is_empty())
            .expect("a glyph on the row");
        (buf[(x, y)].symbol().to_string(), buf[(x, y)].style().fg)
    };

    let (added_glyph, added_fg) = leading_glyph(row_with("fn add"));
    let (context_glyph, context_fg) = leading_glyph(row_with("fn keep"));
    let [r, g, b] = dim_rgb(
        style_rgb(added_fg).expect("rgb fg on the added row"),
        bg,
        CONTEXT_SOFTEN,
    );

    assert_eq!(
        (added_glyph.as_str(), context_glyph.as_str(), context_fg),
        ("f", "f", Some(Color::Rgb(r, g, b))),
        "the context row's keyword blends the added row's color toward the background"
    );
    assert_ne!(
        context_fg, added_fg,
        "the blend moves the context color off the full-strength one"
    );
}

/// The base text and live text of the one-sided rows tests, a removed
/// line, a context line, and an added line.
const ONE_SIDED: (&str, &str) = ("keep\ngone\nstay\n", "keep\nstay\nadded\n");

/// The absent tint and the editor background of `h`'s theme.
fn absent_and_editor_bg(h: &stoat::test_harness::TestHarness) -> (Color, Color) {
    let theme = &h.stoat.theme;
    let bg = |scope| theme.try_get(scope).and_then(|style| style.bg);
    (
        bg(stoat::theme::scope::DIFF_ABSENT).expect("the theme tints an absent half"),
        bg(stoat::theme::scope::UI_BACKGROUND).expect("an editor background"),
    )
}

/// The backgrounds across cells `xs` of row `y`, one per run of equal
/// cells, left to right.
fn bg_runs(buf: &Buffer, y: u16, xs: std::ops::Range<u16>) -> Vec<Color> {
    let mut bgs: Vec<Color> = xs.map(|x| buf[(x, y)].bg).collect();
    bgs.dedup();
    bgs
}

/// The cell rects the frame's hatched bars cover, each with its fill and its
/// hatch color.
fn hatched_rects(h: &stoat::test_harness::TestHarness) -> Vec<(Rect, [u8; 3], [u8; 3])> {
    decode_stream(&h.stoat.apc_scene.all_bytes())
        .into_iter()
        .filter_map(|command| match command {
            Command::Bar(bar) => bar.hatch.map(|hatch| {
                let cells = Rect::new(
                    (bar.x / 16) as u16,
                    (bar.y / 16) as u16,
                    bar.width / 16,
                    bar.height / 16,
                );
                (cells, bar.color, hatch)
            }),
            _ => None,
        })
        .collect()
}

/// A split row that holds a line on one side alone gives the other half
/// the absent surface, gutter and all. Its cells take the absent fill, and
/// one hatched bar stripes them in the absent foreground. A context row
/// keeps the editor background on both halves and takes no hatch.
#[test]
fn a_one_sided_row_hatches_its_empty_half() {
    let h = diff_harness(ONE_SIDED.0, ONE_SIDED.1);
    let (absent, editor) = absent_and_editor_bg(&h);
    let hatch = style_rgb(h.stoat.theme.get(stoat::theme::scope::DIFF_ABSENT).fg)
        .expect("the theme hatches an absent half");
    let fill = style_rgb(Some(absent)).expect("an rgb absent fill");
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);
    let sep = cols.sep_x.expect("two columns");
    let (base, live) = (cols.left_x..sep, cols.right_x..buf.area.x + buf.area.width);
    let half = |cells: &std::ops::Range<u16>, y: u16| {
        Rect::new(cells.start, y, cells.end - cells.start, 1)
    };

    let gone = row_holding(buf, left_text(&cols), "gone");
    let added = row_holding(buf, right_text(&cols), "added");
    let stay = row_holding(buf, right_text(&cols), "stay");
    assert_eq!(
        (
            [
                bg_runs(buf, gone, live.clone()),
                bg_runs(buf, added, base.clone()),
                bg_runs(buf, stay, base.clone()),
                bg_runs(buf, stay, live.clone())
            ],
            hatched_rects(&h),
        ),
        (
            [vec![absent], vec![absent], vec![editor], vec![editor]],
            vec![
                (half(&live, gone), fill, hatch),
                (half(&base, added), fill, hatch)
            ],
        ),
    );
}

/// The dashed outlines in `h`'s last frame, each as its cell rect and the
/// pool anchor it rides.
fn ghost_outlines(h: &stoat::test_harness::TestHarness) -> Vec<(Rect, Option<(u32, f32)>)> {
    decode_stream(&h.stoat.apc_scene.all_bytes())
        .into_iter()
        .filter_map(|command| match command {
            Command::Panel(panel) if panel.style == BorderStyle::Dashed => Some((
                Rect::new(panel.left, panel.top, panel.width, panel.height),
                panel.anchor,
            )),
            _ => None,
        })
        .collect()
}

/// The base column outlines the cell beside the cursor, at the cursor's
/// column, on a row that shows a base line: an unchanged row and a paired
/// modified row. An added row shows none, so it takes no outline. The
/// outline rides the pane's pool from the scroll the paint assumed.
#[test]
fn the_base_column_outlines_the_cell_beside_the_cursor() {
    let mut h = diff_harness("keep\nold\nstay\n", "keep\nnew line\nstay\nadded\n");
    let cols = harness_columns(&h);
    let pane = {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).index
    };
    let row_of = |h: &stoat::test_harness::TestHarness, needle: &str| {
        row_holding(h.rendered_buffer(), right_text(&cols), needle)
    };
    let ghost = |y: u16| vec![(Rect::new(cols.left_text_x + 2, y, 1, 1), Some((pane, 0.0)))];

    h.type_keys("l l");
    let (context, context_row) = (ghost_outlines(&h), row_of(&h, "keep"));
    h.type_keys("j");
    let (paired, paired_row) = (ghost_outlines(&h), row_of(&h, "new line"));
    h.type_keys("j j");
    let added = ghost_outlines(&h);

    assert_eq!(
        (context, paired, added),
        (ghost(context_row), ghost(paired_row), Vec::new()),
    );
}

/// The unified layout shows a removed line in its one column, so no half
/// of the row is empty and the row keeps the editor background.
#[test]
fn a_unified_removed_row_takes_no_absent_tint() {
    let h = diff_harness_sized(DIFF_TWO_COLUMN_MIN - 1, ONE_SIDED.0, ONE_SIDED.1);
    let (_, editor) = absent_and_editor_bg(&h);
    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    let gone = row_holding(buf, left_text(&cols), "gone");
    assert_eq!(
        (
            cols.sep_x,
            bg_runs(buf, gone, buf.area.x..buf.area.x + buf.area.width)
        ),
        (None, vec![editor]),
    );
}

/// The level reaches the paint through the whole live chain, which is what
/// makes it a knob rather than a field.
#[test]
fn the_soften_level_scales_how_far_a_context_row_recedes() {
    let mut h = untinted_diff_harness("fn keep() {}\n", "fn keep() {}\nfn add() {}\n");

    let bg = style_rgb(
        h.stoat
            .theme
            .try_get(stoat::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg),
    )
    .expect("rgb background");

    let context_fg = |h: &mut stoat::test_harness::TestHarness| {
        h.snapshot();
        let buf = h.rendered_buffer();
        let cols = harness_columns(h);
        let y = (0..buf.area.height)
            .find(|&y| line_text(buf, y, right_text(&cols)).contains("fn keep"))
            .expect("the context row renders");
        let x = right_text(&cols)
            .find(|&x| !buf[(x, y)].symbol().trim().is_empty())
            .expect("a glyph on the row");
        buf[(x, y)].style().fg
    };

    let shipped = context_fg(&mut h);

    h.stoat.diff_soften = DIFF_SOFTEN_MIN;
    let stopped = context_fg(&mut h);

    h.stoat.diff_soften = DIFF_SOFTEN_MAX;
    let deepest = context_fg(&mut h);

    let blend = |amount: f32| {
        let full = style_rgb(stopped).expect("rgb fg with no soften applied");
        let [r, g, b] = dim_rgb(full, bg, amount);
        Some(Color::Rgb(r, g, b))
    };

    assert_eq!(
        (shipped, deepest),
        (
            blend(CONTEXT_SOFTEN),
            blend((CONTEXT_SOFTEN * diff_soften_scale(DIFF_SOFTEN_MAX)).min(SOFTEN_CAP)),
        ),
        "level 0 paints the shipped fraction and the top level lands on the floor",
    );
    assert_ne!(
        stopped, shipped,
        "the bottom level stops the blend rather than shrinking it",
    );
}

#[test]
fn diff_view_added_line_takes_no_line_wash() {
    // The second line is a pure insertion, so nothing is refined.
    let h = diff_harness("fn a() {}\n", "fn a() {}\nfn b() {}\n");

    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    let underlined = (0..buf.area.height)
        .any(|y| (0..buf.area.width).any(|x| buf[(x, y)].modifier.contains(Modifier::UNDERLINED)));
    assert!(!underlined, "a pure added line underlines nothing");

    let row = (0..buf.area.height)
        .find(|&y| line_text(buf, y, right_text(&cols)).contains("fn b"))
        .expect("added line rendered on the right");
    let context = (0..buf.area.height)
        .find(|&y| line_text(buf, y, right_text(&cols)).contains("fn a"))
        .expect("context line rendered on the right");
    assert!(
        (right_text(&cols)).all(|x| buf[(x, row)].bg == buf[(x, context)].bg),
        "the added line's cells carry the same background as an unchanged row"
    );
}

#[test]
fn diff_view_deleted_line_takes_no_line_wash() {
    // `old` is deleted, so it renders as a base-only block row on the left.
    let h = diff_harness("keep\nold\ntail\n", "keep\ntail\n");

    let buf = h.rendered_buffer();
    let cols = harness_columns(&h);

    let row = (0..buf.area.height)
        .find(|&y| line_text(buf, y, left_text(&cols)).contains("old"))
        .expect("deleted base line rendered on the left");
    let context = (0..buf.area.height)
        .find(|&y| line_text(buf, y, left_text(&cols)).contains("keep"))
        .expect("unchanged base line mirrored on the left");
    assert!(
        (left_text(&cols)).all(|x| buf[(x, row)].bg == buf[(x, context)].bg),
        "the deleted base line's cells carry the same background as a mirrored row"
    );
}

#[test]
fn diff_view_unchanged_mirrored_base_row_carries_no_background() {
    // `keep` is unchanged and mirrored into the left column.
    let h = diff_harness("keep\nold\ntail\n", "keep\ntail\n");
    let canvas = h
        .stoat
        .theme
        .try_get(stoat::theme::scope::UI_BACKGROUND)
        .and_then(|s| s.bg)
        .expect("an rgb editor background");
    let buf = h.rendered_buffer();
    // The gutter's last cell, where its rule sits, then the left text.
    let left = {
        let text = left_text(&harness_columns(&h));
        text.start - 1..text.end
    };

    let row = (0..buf.area.height)
        .find(|&y| line_text(buf, y, left.clone()).contains("keep"))
        .expect("unchanged base line mirrored on the left");
    assert!(
        left.clone().all(|x| buf[(x, row)].bg == canvas),
        "an unchanged mirrored base row paints no background of its own"
    );
}
