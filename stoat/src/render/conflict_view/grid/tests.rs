use super::{number_scale, row_kind, GridColors, GridColumns, MergeRowKind, Tone};
use crate::{
    action_handlers::{dispatch, focused_editor_mut},
    app::DEFAULT_KEYMAP,
    merge_view::{ChunkState, MergeDoc},
    render::{
        conflict_view::render_conflict_view,
        paint::{dim_rgb, style_rgb},
    },
    test_harness::TestHarness,
    theme::{scope, Theme},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
};
use std::{path::PathBuf, sync::Arc};
use stoat_action::{Action, Conflict, ConflictPickOurs};
use stoat_widgets::ApcScene;
use stoatty_protocol::command::{decode_stream, BorderStyle, Command, PanelCommand, PanelShadow};

const SUCCESS: [u8; 3] = [0x8f, 0xd1, 0x8a];
const SPECIAL: [u8; 3] = [0xc5, 0x9a, 0xf0];
const DANGER: [u8; 3] = [0xf2, 0x77, 0x7f];
const MUTED: [u8; 3] = [0x83, 0x8a, 0x98];
const TEXT: [u8; 3] = [0xdf, 0xe2, 0xe8];
const BASE: [u8; 3] = [0x17, 0x19, 0x1e];
const RAISED: [u8; 3] = [0x1e, 0x21, 0x27];
const HAIRLINE: [u8; 3] = [0x2e, 0x32, 0x3b];
const DIVIDER: [u8; 3] = [0x22, 0x25, 0x2b];
const TINT_OURS: [u8; 3] = [0x18, 0x24, 0x1b];
const TINT_THEIRS: [u8; 3] = [0x23, 0x1d, 0x2e];
const TINT_CONFLICT: [u8; 3] = [0x2a, 0x1b, 0x1d];
const CHIP_CONFLICT: [u8; 3] = [0x33, 0x20, 0x1f];
const MERGED_HEAD: [u8; 3] = [0x1b, 0x1d, 0x22];

/// The one-chunk file most tests open.
///
/// It holds a context line, a conflict on the second line, and a context line
/// after it.
const ONE_CHUNK: [&str; 3] = ["a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n"];

/// The paint area, wide enough that the merged column ends past its chip.
const AREA: Rect = Rect::new(0, 0, 158, 12);

#[test]
fn the_columns_split_the_width_one_to_one_point_one_two_to_one() {
    let layout = |width, digits, scale| {
        let cols = GridColumns::compute(Rect::new(1, 0, width, 10), digits, scale);
        [cols.ours, cols.merged, cols.theirs].map(|col| (col.x, col.width, col.text_x))
    };

    assert_eq!(
        [
            layout(158, 2, 208),
            layout(151, 2, 208),
            layout(158, 4, 256)
        ],
        [
            [(1, 50, 4), (51, 58, 54), (109, 50, 112)],
            [(1, 48, 4), (49, 55, 52), (104, 48, 107)],
            [(1, 50, 7), (51, 58, 57), (109, 50, 115)],
        ],
        "the sides take 100 / 312 each, and the text starts on the cell past the \
         bar, the number field, and the divider's margins",
    );
}

#[test]
fn rows_classify_by_the_side_the_merge_takes() {
    let doc = MergeDoc::build(
        "keep\nmine\nyours\nboth\ntail\n",
        "keep\nMINE\nyours\nOURS\ntail\n",
        "keep\nmine\nYOURS\nTHEIRS\ntail\n",
        None,
    );
    let plan = doc.align(&[]);
    let kinds = |state| {
        plan.iter()
            .map(|row| row_kind(row, &doc, &[state]))
            .collect::<Vec<_>>()
    };

    use MergeRowKind::{Conflict, Context, Ours, Settled, Theirs, TookOurs, TookTheirs};
    assert_eq!(
        kinds(ChunkState::Unresolved),
        [Context, Ours, Theirs, Conflict, Conflict, Conflict, Conflict, Conflict, Context],
        "a one-sided edit takes its side, and the marker block is the conflict",
    );
    assert_eq!(
        [
            ChunkState::Ours,
            ChunkState::Theirs,
            ChunkState::Both,
            ChunkState::Picked,
            ChunkState::AutoIndent,
            ChunkState::Manual,
        ]
        .map(|state| kinds(state)[3]),
        [
            TookOurs,
            TookTheirs,
            Settled("both"),
            Settled("picked"),
            Settled("auto \u{b7} indent"),
            Settled("edited"),
        ],
        "a settled chunk's rows take the kind of its resolution",
    );
}

#[test]
fn inserted_lines_classify_by_the_side_that_added_them() {
    let doc = MergeDoc::build("keep\n", "keep\nmine\n", "theirs\nkeep\n", None);
    let kinds: Vec<_> = doc
        .align(&[])
        .iter()
        .map(|row| row_kind(row, &doc, &[]))
        .collect();

    assert_eq!(
        kinds,
        [
            MergeRowKind::Theirs,
            MergeRowKind::Context,
            MergeRowKind::Ours
        ]
    );
}

#[test]
fn the_header_names_each_side_and_the_merged_file() {
    let shot = grid_shot(ONE_CHUNK, &[]);
    let row = |cell: (u16, u16)| style_rgb(Some(shot.buf[cell].bg));
    let header_runs: Vec<_> = shot
        .runs()
        .into_iter()
        .filter(|&(_, row, ..)| row == 0)
        .map(|(_, _, text, color)| (text, color))
        .collect();
    let rule = shot.bars().into_iter().find(|&(.., color)| color == RAISED);

    assert_eq!(
        (
            header_runs,
            row((shot.cols.merged.x, 0)),
            row((shot.cols.ours.x, 0)),
            rule
        ),
        (
            [
                ("ours", SUCCESS),
                ("main", MUTED),
                ("o \u{b7} O line", MUTED),
                ("merged", TEXT),
                ("f.txt", MUTED),
                ("theirs", SPECIAL),
                ("feature", MUTED),
                ("t \u{b7} T line", MUTED),
            ]
            .map(|(text, color)| (text.to_string(), color))
            .to_vec(),
            Some(MERGED_HEAD),
            None,
            Some((0, 15, AREA.width * 16, 1, RAISED)),
        ),
        "the merged cell fills with the head color, and a rule underlines the row",
    );
}

#[test]
fn the_rows_start_below_the_header() {
    let mut h = conflict_harness(ONE_CHUNK, &[]);
    let shot = paint(&mut h);
    let editor = focused_editor_mut(&mut h.stoat).expect("center editor");
    let first_numbers: Vec<_> = shot
        .runs()
        .into_iter()
        .filter(|&(_, row, ..)| row == 16)
        .map(|(_, _, text, _)| text)
        .collect();

    assert_eq!(
        (editor.text_rect, editor.viewport_rows, first_numbers),
        (
            Some(Rect::new(
                shot.cols.merged.text_x,
                1,
                shot.cols.merged.right() - shot.cols.merged.text_x,
                AREA.height - 1,
            )),
            Some(u32::from(AREA.height) - 1),
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
    let ghost = &shot.buf[(cols.merged.text_x, 2)];

    assert_eq!(
        (
            [row(1), row(2), row(3), row(8)],
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
        .filter(|&(_, _, width, ..)| width == 3)
        .collect();
    let chip = shot.chip();

    assert_eq!(
        (change_bars, chip),
        (
            vec![
                bar(cols.ours.x, 2, SUCCESS),
                bar(cols.theirs.x, 2, SPECIAL),
                bar(cols.merged.x, 2, DANGER),
                bar(cols.merged.x, 3, DANGER),
                bar(cols.merged.x, 4, DANGER),
                bar(cols.merged.x, 5, DANGER),
                bar(cols.merged.x, 6, DANGER),
            ],
            Some((
                Rect::new(cols.merged.right() - 17, 2, 17, 1),
                "conflict \u{b7} o t b".to_string(),
                DANGER,
                CHIP_CONFLICT,
            )),
        ),
        "each band row bars the merged column, and the chip ends on the column's edge",
    );
}

/// A chip's panel is a backdrop, which no pool composite takes as an occluder.
/// A glide of the pane's pool then covers the live chip, and the chip that
/// the page paints travels with its row.
#[test]
fn a_chip_panel_sits_under_the_pane_pool() {
    let shot = grid_shot(ONE_CHUNK, &[]);
    let panels: Vec<PanelCommand> = shot
        .commands
        .iter()
        .filter_map(|command| match command {
            Command::Panel(panel) => Some(*panel),
            _ => None,
        })
        .collect();

    assert_eq!(
        panels,
        [PanelCommand {
            top: 2,
            left: shot.cols.merged.right() - 17,
            width: 17,
            height: 1,
            style: BorderStyle::None_,
            border: CHIP_CONFLICT,
            corner_radius: 1,
            fill: Some(CHIP_CONFLICT),
            shadow: PanelShadow::None_,
            inset_x: 0,
            inset_y: 0,
            corners: 0,
            fillets: 0,
            hatch: None,
            above_pools: false,
            backdrop: true,
            anchor: None,
        }],
    );
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
                (line as i16, line.to_string(), color)
            })
            .collect::<Vec<_>>(),
        "the open lands the cursor on the chunk's first row, the second line",
    );
}

#[test]
fn a_taken_side_bars_the_merged_column_and_fades_the_other_side() {
    let shot = grid_shot(ONE_CHUNK, &[&ConflictPickOurs]);
    let cols = &shot.cols;
    let change_bars: Vec<_> = shot
        .bars()
        .into_iter()
        .filter(|&(_, _, width, ..)| width == 3)
        .collect();
    let theirs = &shot.buf[(cols.theirs.text_x, 2)];
    let [r, g, b] = dim_rgb(SPECIAL, BASE, 0.55);

    assert_eq!(
        (
            change_bars,
            shot.chip().map(|(_, text, ..)| text),
            (theirs.symbol(), theirs.fg),
            style_rgb(Some(shot.buf[(cols.ours.x, 2)].bg)),
        ),
        (
            vec![bar(cols.ours.x, 2, SUCCESS), bar(cols.merged.x, 2, SUCCESS)],
            Some("\u{2713} took ours".to_string()),
            ("T", Color::Rgb(r, g, b)),
            Some(TINT_OURS),
        ),
        "the taken side tints and bars, and the untaken side's text fades",
    );
}

#[test]
fn one_sided_edits_tint_their_side_and_chip_the_first_row_of_a_run() {
    let shot = grid_shot(
        [
            "a\nb\nc\nkeep\nbase\nz\n",
            "a\nB\nC\nkeep\nOURS\nz\n",
            "a\nb\nc\nkeep\nTHEIRS\nz\n",
        ],
        &[],
    );
    let cols = &shot.cols;
    let bg = |x: u16, y: u16| style_rgb(Some(shot.buf[(x, y)].bg));
    let chips: Vec<_> = shot
        .runs()
        .into_iter()
        .filter(|(_, _, text, _)| text.contains('\u{b7}'))
        .filter(|&(_, row, ..)| row > 0)
        .map(|(_, row, text, _)| (row / 16, text))
        .collect();

    assert_eq!(
        (
            [
                bg(cols.ours.x, 2),
                bg(cols.merged.x, 2),
                bg(cols.theirs.x, 2)
            ],
            [
                bg(cols.ours.x, 3),
                bg(cols.merged.x, 3),
                bg(cols.theirs.x, 3)
            ],
            chips,
        ),
        (
            [Some(TINT_OURS), None, None],
            [Some(TINT_OURS), None, None],
            vec![
                (2, "auto \u{b7} ours".to_string()),
                (5, "conflict \u{b7} o t b".to_string()),
            ],
        ),
        "only ours tints an ours edit, and its run of two rows takes one chip",
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
        .filter(|&(.., color)| color != RAISED)
        .collect();
    rules.sort();
    let divider = |x: u16| (x as i16 * 16 + 45, 16, 1, 128, DIVIDER);
    let hairline = |x: i16, y: i16, height: u16| (x, y, 1, height, HAIRLINE);
    let mut want = vec![
        divider(cols.ours.x),
        divider(cols.merged.x),
        divider(cols.theirs.x),
        hairline(cols.merged.x as i16 * 16 - 1, 0, 16),
        hairline(cols.merged.right() as i16 * 16, 0, 16),
        hairline(cols.merged.x as i16 * 16 - 1, 16, 128),
        hairline(cols.merged.right() as i16 * 16, 16, 128),
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
        (Rect::new(0, 0, AREA.width, 1), ermine),
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

#[test]
fn a_theme_without_the_board_scopes_falls_back_to_their_neighbors() {
    let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    let theme = Theme::from_config(&config.expect("the shipped config parses"), "default_dark")
        .expect("default_dark loads");
    // default_dark's danger, #e06c75, washed 85% toward its surface, #282c34.
    let band = Tone {
        fg: [0xe0, 0x6c, 0x75],
        tint: [0x44, 0x36, 0x3e],
    };
    let colors = GridColors::resolve(&theme).expect("default_dark gives RGB colors");

    assert_eq!(
        (colors.band, colors.chip, colors.ghost, colors.head),
        (
            band,
            band,
            Style::default()
                .fg(rgb([0x63, 0x6d, 0x83]))
                .add_modifier(Modifier::ITALIC),
            None,
        ),
        "the band takes the conflict header's color, the chip the band, and the \
         ghost muted italic text",
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

    /// The one chip panel's cell rect, the text of the run inside it, the
    /// run's color, and the panel's fill.
    fn chip(&self) -> Option<(Rect, String, [u8; 3], [u8; 3])> {
        let panel = self.commands.iter().find_map(|command| match command {
            Command::Panel(panel) => Some(panel),
            _ => None,
        })?;
        let area = Rect::new(panel.left, panel.top, panel.width, panel.height);
        let (_, _, text, color) = self
            .runs()
            .into_iter()
            .find(|&(col, row, ..)| (col / 16) as u16 == area.x && (row / 16) as u16 == area.y)?;
        Some((area, text, color, panel.fill?))
    }
}

/// The 3/16 change bar of `column`'s cell on row `y`.
fn bar(column: u16, y: u16, color: [u8; 3]) -> BarFields {
    (column as i16 * 16 + 4, y as i16 * 16, 3, 16, color)
}

fn rgb([r, g, b]: [u8; 3]) -> Color {
    Color::Rgb(r, g, b)
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
