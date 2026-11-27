use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier},
};
use std::{path::PathBuf, sync::Arc};
use stoat::{
    action_handlers::{conflict_view, dispatch, focused_editor_mut},
    merge_view::Side,
    render::{
        conflict_view::{
            grid::{number_scale, GridColumns, HEADER_ROWS},
            render_conflict_view, ConflictHover,
        },
        paint::{dim_rgb, style_rgb},
    },
    test_fixture::render_conflict_view_grid::{
        rgb, AREA, CHIP_CONFLICT, CHIP_OURS, CHIP_THEIRS, DANGER, DIVIDER, MUTED, SPECIAL, SUCCESS,
        TEXT,
    },
    test_harness::TestHarness,
    theme::{scope, Theme},
};
use stoat_action::{Action, Conflict, ConflictPickOurs};
use stoat_widgets::ApcScene;
use stoatty_protocol::command::{decode_stream, Command};

const BASE: [u8; 3] = [0x17, 0x19, 0x1e];

const HAIRLINE: [u8; 3] = [0x2e, 0x32, 0x3b];

const TINT_OURS: [u8; 3] = [0x18, 0x24, 0x1b];

const TINT_THEIRS: [u8; 3] = [0x23, 0x1d, 0x2e];

const TINT_CONFLICT: [u8; 3] = [0x2a, 0x1b, 0x1d];

const CHIP_BOTH: [u8; 3] = [0x1f, 0x2a, 0x3d];

const ACCENT: [u8; 3] = [0x7f, 0xb0, 0xff];

const CHROME: [u8; 3] = [0x12, 0x14, 0x18];

/// The first sixteenth under the column header and the File Header, where the
/// grid's rows start.
const BODY_TOP: i16 = HEADER_ROWS as i16 * 16;

/// The one-chunk file most tests open.
///
/// It holds a context line, a conflict on the second line, and a context line
/// after it.
const ONE_CHUNK: [&str; 3] = ["a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n"];

#[test]
fn the_rows_start_below_the_header() {
    let mut h = conflict_harness(ONE_CHUNK, &[]);
    let shot = paint(&mut h);
    let editor = focused_editor_mut(&mut h.stoat).expect("center editor");
    let first_numbers: Vec<_> = shot
        .runs()
        .into_iter()
        .filter(|&(_, row, ..)| row == 32)
        .map(|(_, _, text, _)| text)
        .collect();

    assert_eq!(
        (editor.text_rect, editor.viewport_rows, first_numbers),
        (
            Some(Rect::new(
                shot.cols.merged.text_x,
                2,
                shot.cols.merged.right() - shot.cols.merged.text_x,
                AREA.height - 2,
            )),
            Some(u32::from(AREA.height) - 2),
            vec!["1".to_string(), "1".to_string(), "1".to_string()],
        ),
    );
}

#[test]
fn an_unresolved_chunk_tints_each_column_and_ghosts_its_markers() {
    let shot = grid_shot(ONE_CHUNK, &[]);
    let cols = &shot.cols;
    let row = |y: u16| {
        [cols.ours.x, cols.merged.x, cols.theirs.x].map(|x| style_rgb(Some(shot.buf[(x, y)].bg)))
    };
    let absent = style_rgb(shot.theme.get(scope::DIFF_ABSENT).bg);
    let ghost = &shot.buf[(cols.merged.text_x, 3)];

    assert_eq!(
        (
            [row(2), row(3), row(4), row(9)],
            (
                ghost.symbol(),
                ghost.fg,
                ghost.modifier.contains(Modifier::ITALIC)
            ),
        ),
        (
            [
                [None, None, None],
                [Some(TINT_OURS), Some(TINT_CONFLICT), Some(TINT_THEIRS)],
                [absent, Some(TINT_CONFLICT), absent],
                [None, None, None],
            ],
            ("<", rgb(MUTED), true),
        ),
        "the context row stays on the surface, a side with no line on a band row \
         fills with the absent color, and the empty line after the last newline \
         fills nothing",
    );
}

#[test]
fn an_unresolved_chunk_bars_its_edited_sides_and_chips_its_first_row() {
    let shot = grid_shot(ONE_CHUNK, &[]);
    let cols = &shot.cols;
    let change_bars: Vec<_> = shot
        .bars()
        .into_iter()
        .filter(|&(_, _, width, ..)| width == 6)
        .collect();
    // The chip's 16 characters advance 256 sixteenths at the test scale, and
    // its box adds 12 of padding on each side.
    let chip_x = cols.merged.right() as i16 * 16 - 12 - 280;

    assert_eq!(
        (change_bars, shot.chips()),
        (
            vec![
                bar(cols.ours.x, 3, SUCCESS),
                bar(cols.theirs.x, 3, SPECIAL),
                bar(cols.merged.x, 3, DANGER),
                bar(cols.merged.x, 4, DANGER),
                bar(cols.merged.x, 5, DANGER),
                bar(cols.merged.x, 6, DANGER),
                bar(cols.merged.x, 7, DANGER),
            ],
            vec![(
                (chip_x, 49, 280, 14, CHIP_CONFLICT),
                "conflict \u{b7} o t b".to_string(),
                DANGER,
            )],
        ),
        "each band row bars the merged column, and the chip ends 12 sixteenths \
         before the column's edge",
    );
}

/// A chip draws as one rounded bar in its tint, 14 sixteenths tall, ahead of
/// the run that names it. The run starts 12 sixteenths in and takes no box of
/// its own. The scene holds no panel, so the chip rides a pool page with its
/// row.
#[test]
fn a_chip_draws_as_a_rounded_bar_under_its_run() {
    let shot = grid_shot(ONE_CHUNK, &[]);
    let at = |found: fn(&Command) -> bool| shot.commands.iter().position(found);
    let rounded: Vec<_> = shot
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Bar(bar) if bar.radius > 0 && bar.y >= BODY_TOP => {
                Some((bar.x, bar.height, bar.radius, bar.color))
            },
            _ => None,
        })
        .collect();
    let run = shot.commands.iter().find_map(|command| match command {
        Command::TextRun(run) if run.row >= BODY_TOP && run.text.starts_with("conflict") => {
            Some((run.col, run.bg))
        },
        _ => None,
    });
    let bar_at =
        at(|command| matches!(command, Command::Bar(bar) if bar.radius > 0 && bar.y >= BODY_TOP));
    let run_at = at(
        |command| matches!(command, Command::TextRun(run) if run.row >= BODY_TOP && run.text.starts_with("conflict")),
    );
    let box_x = rounded.first().map_or(0, |&(x, ..)| x);

    assert_eq!(
        (
            rounded
                .iter()
                .map(|&(_, height, radius, color)| (height, radius, color))
                .collect::<Vec<_>>(),
            run.map(|(col, bg)| (col - box_x, bg)),
            bar_at < run_at,
            at(|command| matches!(command, Command::Panel(_))),
        ),
        (vec![(14, 4, CHIP_CONFLICT)], Some((12, None)), true, None),
    );
}

/// An open chip menu draws three rounded buttons in the chip's place on the
/// chunk's first row, in the ours, theirs, and both tones. Each is 14
/// sixteenths tall under its label, 12 sixteenths in, with 4 sixteenths
/// between buttons, and the last ends where the chip's box ended.
#[test]
fn an_open_chip_menu_draws_three_buttons_where_the_chip_stood() {
    let closed = grid_shot(ONE_CHUNK, &[]).chips();
    let mut h = conflict_harness(ONE_CHUNK, &[]);
    conflict_view::set_chip_menu(&mut h.stoat, Some(0));
    let open = paint(&mut h).chips();

    let [((x, y, width, ..), ..)] = &closed[..] else {
        panic!("one chip, got {closed:?}");
    };
    let end = x + *width as i16;
    assert_eq!(
        open,
        [
            (
                (end - 304, *y, 88, 14, CHIP_OURS),
                "ours".to_string(),
                SUCCESS
            ),
            (
                (end - 212, *y, 120, 14, CHIP_THEIRS),
                "theirs".to_string(),
                SPECIAL
            ),
            (
                (end - 88, *y, 88, 14, CHIP_BOTH),
                "both".to_string(),
                ACCENT
            ),
        ],
    );
}

/// A hovered side draws a ring over the chunk's rows in its column, a stroke
/// in the side's color at radius 2 with no fill. On the ring's last row a pill
/// in the chrome fill names the pick, 12 sixteenths in from its edge. The
/// hovered cells wash 3 percent toward white.
///
/// The chunk's band takes rows 3 to 7, and the ours column cells 0 to 49. The
/// pill's 21 characters, its pad, and the board's 6 px from the ring's right
/// edge take 24 cells.
#[test]
fn a_hovered_side_draws_a_ring_and_a_pill() {
    let shot = hovered_shot(ConflictHover {
        chunk: 0,
        side: Side::Ours,
        merge_row: None,
    });

    assert_eq!(
        (shot.panels(), shot.pill(), shot.buf[(10, 3)].bg),
        (
            vec![
                (0, 3, 50, 5, SUCCESS, 2, None),
                (26, 7, 24, 1, SUCCESS, 4, Some(CHROME)),
            ],
            Some((
                26 * 16 + 12,
                "click \u{b7} take ours \u{b7} o".to_string(),
                SUCCESS
            )),
            rgb(dim_rgb(TINT_OURS, [0xff, 0xff, 0xff], 0.03)),
        ),
    );
}

/// With Alt held the ring covers the one theirs line under the pointer, and
/// the pill names the line pick. The theirs column starts at cell 108.
#[test]
fn an_alt_hover_rings_one_line() {
    let shot = hovered_shot(ConflictHover {
        chunk: 0,
        side: Side::Theirs,
        merge_row: Some(1),
    });

    assert_eq!(
        (shot.panels(), shot.pill()),
        (
            vec![
                (108, 3, 50, 1, SPECIAL, 2, None),
                (130, 3, 28, 1, SPECIAL, 4, Some(CHROME)),
            ],
            Some((
                130 * 16 + 12,
                "alt-click \u{b7} take line \u{b7} T".to_string(),
                SPECIAL
            )),
        ),
    );
}

/// A chunk a pick settled takes no pick from the pointer, so a hover left over
/// it draws no ring and no pill.
#[test]
fn a_settled_chunk_draws_no_hover() {
    let mut h = conflict_harness(ONE_CHUNK, &[&ConflictPickOurs]);
    focused_editor_mut(&mut h.stoat)
        .expect("center editor")
        .conflict_frame
        .hover = Some(ConflictHover {
        chunk: 0,
        side: Side::Ours,
        merge_row: None,
    });
    let shot = paint(&mut h);

    assert_eq!((shot.panels(), shot.pill()), (Vec::new(), None));
}

#[test]
fn the_cursor_row_lifts_its_merged_number() {
    let shot = grid_shot(ONE_CHUNK, &[]);
    let merged = shot.cols.merged;
    let mut merged_numbers: Vec<_> = shot
        .runs()
        .into_iter()
        .filter(|(col, _, text, _)| {
            (merged.x as i16 * 16..merged.right() as i16 * 16).contains(col)
                && text.parse::<u32>().is_ok()
        })
        .map(|(_, row, text, color)| (row / 16, text, color))
        .collect();
    // The numbers batch by color and backing, so the scene groups them out of
    // row order.
    merged_numbers.sort();

    assert_eq!(
        merged_numbers,
        (1..=8)
            .map(|line| {
                let color = if line == 2 { TEXT } else { MUTED };
                (line as i16 + 1, line.to_string(), color)
            })
            .collect::<Vec<_>>(),
        "the open lands the cursor on the chunk's first row, the second line",
    );
}

/// The three column names, the File Header's file name, and the cursor row's
/// merged number draw bold, as the MergeGrid board draws them, and no other
/// run of the grid does.
#[test]
fn the_column_names_and_the_cursor_row_s_number_draw_bold() {
    let shot = grid_shot(ONE_CHUNK, &[]);
    let bold: Vec<String> = shot
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::TextRun(run) if run.bold => Some(run.text.clone()),
            _ => None,
        })
        .collect();

    assert_eq!(bold, ["ours", "merged", "theirs", "f.txt", "2"]);
}

#[test]
fn a_taken_side_bars_the_merged_column_and_fades_the_other_side() {
    let shot = grid_shot(ONE_CHUNK, &[&ConflictPickOurs]);
    let cols = &shot.cols;
    let change_bars: Vec<_> = shot
        .bars()
        .into_iter()
        .filter(|&(_, _, width, ..)| width == 6)
        .collect();
    let theirs = &shot.buf[(cols.theirs.text_x, 3)];
    let [r, g, b] = dim_rgb(SPECIAL, BASE, 0.55);

    assert_eq!(
        (
            change_bars,
            shot.chips()
                .into_iter()
                .map(|((.., tint), text, fg)| (text, fg, tint))
                .collect::<Vec<_>>(),
            (theirs.symbol(), theirs.fg),
            style_rgb(Some(shot.buf[(cols.ours.x, 3)].bg)),
        ),
        (
            vec![bar(cols.ours.x, 3, SUCCESS), bar(cols.merged.x, 3, SUCCESS)],
            vec![("\u{2713} took ours".to_string(), SUCCESS, CHIP_OURS)],
            ("T", Color::Rgb(r, g, b)),
            Some(TINT_OURS),
        ),
        "the taken side tints and bars, its chip takes the ours chip's tint, and \
         the untaken side's text fades",
    );
}

#[test]
fn one_sided_edits_tint_their_side_and_chip_the_first_row_of_a_run() {
    let shot = grid_shot(
        [
            "a\nb\nc\nkeep\nbase\nz\ntail\n",
            "a\nB\nC\nkeep\nOURS\nz\ntail\n",
            "a\nb\nc\nkeep\nTHEIRS\nz\nTAIL\n",
        ],
        &[],
    );
    let cols = &shot.cols;
    let bg = |x: u16, y: u16| style_rgb(Some(shot.buf[(x, y)].bg));
    let chips: Vec<_> = shot
        .chips()
        .into_iter()
        .map(|((_, y, .., tint), text, fg)| (y / 16, text, fg, tint))
        .collect();

    assert_eq!(
        (
            [
                bg(cols.ours.x, 3),
                bg(cols.merged.x, 3),
                bg(cols.theirs.x, 3)
            ],
            [
                bg(cols.ours.x, 4),
                bg(cols.merged.x, 4),
                bg(cols.theirs.x, 4)
            ],
            chips,
        ),
        (
            [Some(TINT_OURS), None, None],
            [Some(TINT_OURS), None, None],
            vec![
                (3, "auto \u{b7} ours".to_string(), SUCCESS, CHIP_OURS),
                (
                    6,
                    "conflict \u{b7} o t b".to_string(),
                    DANGER,
                    CHIP_CONFLICT
                ),
                (12, "auto \u{b7} theirs".to_string(), SPECIAL, CHIP_THEIRS),
            ],
        ),
        "only ours tints an ours edit, its run of two rows takes one chip, and \
         each chip takes the tint of its kind",
    );
}

#[test]
fn the_grid_draws_dividers_and_hairlines_down_its_rows() {
    let shot = grid_shot(ONE_CHUNK, &[]);
    let cols = &shot.cols;
    let mut rules: Vec<_> = shot
        .bars()
        .into_iter()
        .filter(|&(_, _, width, ..)| width == 1)
        .collect();
    rules.sort();
    let divider = |x: u16| (x as i16 * 16 + 68, 32, 1, 128, DIVIDER);
    let hairline = |x: i16, y: i16, height: u16| (x, y, 1, height, HAIRLINE);
    let mut want = vec![
        divider(cols.ours.x),
        divider(cols.merged.x),
        divider(cols.theirs.x),
        hairline(cols.merged.x as i16 * 16 - 1, 0, 16),
        hairline(cols.merged.right() as i16 * 16, 0, 16),
        hairline(cols.merged.x as i16 * 16 - 1, 16, 16),
        hairline(cols.merged.right() as i16 * 16, 16, 16),
        hairline(cols.merged.x as i16 * 16 - 1, 32, 128),
        hairline(cols.merged.right() as i16 * 16, 32, 128),
    ];
    want.sort();

    assert_eq!(
        rules, want,
        "the dividers run down the eight painted rows, and the hairlines through \
         the header and the rows",
    );
}

#[test]
fn no_rgb_colors_or_no_row_under_the_header_keeps_the_cell_layout() {
    let mut h = conflict_harness(ONE_CHUNK, &[]);
    let ermine = h.stoat.theme.clone();
    let cases = [
        (AREA, Arc::new(Theme::empty())),
        (Rect::new(0, 0, AREA.width, HEADER_ROWS), ermine),
    ];
    let layouts = cases.map(|(area, theme)| {
        let shot = paint_over(&mut h, area, theme);
        let text_y = focused_editor_mut(&mut h.stoat)
            .and_then(|editor| editor.text_rect)
            .map(|text| text.y);
        (
            shot.commands.is_empty(),
            text_y,
            shot.buf[(area.width / 3, 0)].symbol().to_string(),
        )
    });

    assert_eq!(
        layouts,
        [(); 2].map(|()| (true, Some(0), "\u{2502}".to_string())),
        "no scene component draws, the rows start at the top, and a cell rule \
         splits the columns",
    );
}

/// A conflict view painted with a live scene.
///
/// It keeps the cells, the scene's commands, the theme of the paint, and the
/// columns the paint laid out.
struct GridShot {
    buf: Buffer,
    commands: Vec<Command>,
    theme: Arc<Theme>,
    cols: GridColumns,
}

/// A bar's `(x, y, width, height, color)`, in sixteenths.
type BarFields = (i16, i16, u16, u16, [u8; 3]);

/// A panel's `(left, top, width, height, border, radius, fill)`, in cells.
type PanelFields = (u16, u16, u16, u16, [u8; 3], u8, Option<[u8; 3]>);

impl GridShot {
    /// Every bar, in the order the scene holds them.
    fn bars(&self) -> Vec<BarFields> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                Command::Bar(bar) => Some((bar.x, bar.y, bar.width, bar.height, bar.color)),
                _ => None,
            })
            .collect()
    }

    /// Every text run's `(col, row, text, color)`, in sixteenths, in the order
    /// the scene holds them.
    fn runs(&self) -> Vec<(i16, i16, String, [u8; 3])> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some((run.col, run.row, run.text.clone(), run.color)),
                _ => None,
            })
            .collect()
    }

    /// Each chip's box as `(x, y, width, height, tint)`, in sixteenths, with
    /// the text and the color of the run that starts on the box, in the order
    /// the scene holds them.
    ///
    /// A chip's box is the one kind of rounded bar in the grid's rows. The File
    /// Header's tag above them is rounded too.
    fn chips(&self) -> Vec<(BarFields, String, [u8; 3])> {
        let runs = self.runs();
        self.commands
            .iter()
            .filter_map(|command| match command {
                Command::Bar(bar) if bar.radius > 0 && bar.y >= BODY_TOP => {
                    Some((bar.x, bar.y, bar.width, bar.height, bar.color))
                },
                _ => None,
            })
            .filter_map(|chip| {
                let (x, y, width, ..) = chip;
                let (_, _, text, color) = runs
                    .iter()
                    .find(|&&(col, row, ..)| {
                        row / 16 == y / 16 && (x..x + width as i16).contains(&col)
                    })?
                    .clone();
                Some((chip, text, color))
            })
            .collect()
    }

    /// Each panel as `(left, top, width, height, border, radius, fill)`, in
    /// cells, in the order the scene holds them.
    fn panels(&self) -> Vec<PanelFields> {
        self.commands
            .iter()
            .filter_map(|command| match command {
                Command::Panel(panel) => Some((
                    panel.left,
                    panel.top,
                    panel.width,
                    panel.height,
                    panel.border,
                    panel.corner_radius,
                    panel.fill,
                )),
                _ => None,
            })
            .collect()
    }

    /// The hover pill's run as `(col, text, color)`, the one run that names a
    /// pick.
    fn pill(&self) -> Option<(i16, String, [u8; 3])> {
        self.runs()
            .into_iter()
            .find(|(_, _, text, _)| text.contains("take"))
            .map(|(col, _, text, color)| (col, text, color))
    }
}

/// [`grid_shot`] of [`ONE_CHUNK`] with the pointer resting over `hover`.
fn hovered_shot(hover: ConflictHover) -> GridShot {
    let mut h = conflict_harness(ONE_CHUNK, &[]);
    focused_editor_mut(&mut h.stoat)
        .expect("center editor")
        .conflict_frame
        .hover = Some(hover);
    paint(&mut h)
}

/// The 6/16 change bar of `column`'s cell on row `y`.
fn bar(column: u16, y: u16, color: [u8; 3]) -> BarFields {
    (column as i16 * 16 + 7, y as i16 * 16, 6, 16, color)
}

/// Open the conflict view on `f.txt` with the three `stages` (ancestor, ours,
/// theirs), mid-merge of `feature` into `main`, then dispatch `actions`, and
/// paint the view over [`AREA`] with a live scene.
fn grid_shot(stages: [&str; 3], actions: &[&dyn Action]) -> GridShot {
    let mut h = conflict_harness(stages, actions);
    paint(&mut h)
}

/// A harness with the conflict view open on `stages` as [`grid_shot`] opens it.
fn conflict_harness(stages: [&str; 3], actions: &[&dyn Action]) -> TestHarness {
    let [ancestor, ours, theirs] = stages;
    let mut h = TestHarness::with_size(AREA.width, AREA.height + 2);
    h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
    h.fake_git()
        .add_repo("/repo")
        .with_fs(h.fake_fs())
        .set_head_branch("main")
        .merge_in_progress("f00dfeed", Some("feature"), "Merge branch 'feature'")
        .conflicted_file("f.txt", Some(ancestor), Some(ours), Some(theirs));
    dispatch(&mut h.stoat, &Conflict);
    h.settle();
    for action in actions {
        dispatch(&mut h.stoat, *action);
        h.settle();
    }
    h
}

/// Paint the focused conflict view over [`AREA`] with a fresh live scene.
fn paint(h: &mut TestHarness) -> GridShot {
    let theme = h.stoat.theme.clone();
    paint_over(h, AREA, theme)
}

/// Paint the focused conflict view over `area` in `theme` with a fresh live
/// scene.
fn paint_over(h: &mut TestHarness, area: Rect, theme: Arc<Theme>) -> GridShot {
    let editor = focused_editor_mut(&mut h.stoat).expect("center editor");
    let mut buf = Buffer::empty(area);
    let mut scene = ApcScene::new();
    render_conflict_view(
        editor,
        area,
        theme.get(scope::UI_TEXT),
        &theme,
        &mut buf,
        Some(&mut scene),
        true,
    );

    GridShot {
        buf,
        commands: decode_stream(scene.buffer()),
        cols: GridColumns::compute(area, 2, number_scale(&theme)),
        theme,
    }
}
