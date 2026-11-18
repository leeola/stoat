use super::{
    number_scale, paint_file_header, paint_header, row_kind, FileFacts, Grid, GridColors,
    GridColumns, MergeRowKind, RowLook, Tone, HEADER_ROWS,
};
use crate::{
    action_handlers::{dispatch, focused_editor_mut},
    app::DEFAULT_KEYMAP,
    conflict_session::{ConflictLabels, ConflictPosition},
    host::git::MergeState,
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
use stoatty_protocol::command::{decode_stream, Command};

const SUCCESS: [u8; 3] = [0x8f, 0xd1, 0x8a];
const SPECIAL: [u8; 3] = [0xc5, 0x9a, 0xf0];
const DANGER: [u8; 3] = [0xf2, 0x77, 0x7f];
const MUTED: [u8; 3] = [0x83, 0x8a, 0x98];
const TEXT: [u8; 3] = [0xdf, 0xe2, 0xe8];
const BASE: [u8; 3] = [0x17, 0x19, 0x1e];
const HAIRLINE: [u8; 3] = [0x2e, 0x32, 0x3b];
const DIVIDER: [u8; 3] = [0x22, 0x25, 0x2b];
const TINT_OURS: [u8; 3] = [0x18, 0x24, 0x1b];
const TINT_THEIRS: [u8; 3] = [0x23, 0x1d, 0x2e];
const TINT_CONFLICT: [u8; 3] = [0x2a, 0x1b, 0x1d];
const CHIP_CONFLICT: [u8; 3] = [0x33, 0x20, 0x1f];
const CHIP_OURS: [u8; 3] = [0x1d, 0x2a, 0x1f];
const CHIP_THEIRS: [u8; 3] = [0x2a, 0x22, 0x36];
const MERGED_HEAD: [u8; 3] = [0x1b, 0x1d, 0x22];
const CONFLICT: [u8; 3] = [0xf5, 0x9a, 0xa0];
const TAG_CONFLICT: [u8; 3] = [0x3a, 0x20, 0x26];
const DISABLED: [u8; 3] = [0x5b, 0x61, 0x6d];

/// The first sixteenth under the column header and the File Header, where the
/// grid's rows start.
const BODY_TOP: i16 = HEADER_ROWS as i16 * 16;

/// The one-chunk file most tests open.
///
/// It holds a context line, a conflict on the second line, and a context line
/// after it.
const ONE_CHUNK: [&str; 3] = ["a\nbase\nz\n", "a\nOURS\nz\n", "a\nTHEIRS\nz\n"];

/// The paint area, wide enough that the merged column ends past its chip, and
/// tall enough to hold the eleven rows of the one-sided edits test under the
/// two header rows.
const AREA: Rect = Rect::new(0, 0, 158, 13);

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
            [(1, 50, 6), (51, 58, 56), (109, 50, 114)],
            [(1, 48, 6), (49, 55, 54), (104, 48, 109)],
            [(1, 50, 8), (51, 58, 58), (109, 50, 116)],
        ],
        "the sides take 100 / 312 each, the text starts on the cell past the bar, \
         the number field, and the divider's margins, and a number too long for \
         the field moves the text out",
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

/// The header names each side's commit and the merged column's working tree,
/// each name and label at the Ui scale and the keys at the Caption scale, all
/// 20 sixteenths in from their column's edges. The merged cell fills with the
/// head color, and a rule in the divider's color underlines the row.
///
/// At the Ui scale of 232 a character advances 14.5 sixteenths, and at the
/// Caption scale of 216 it advances 13.5. So the ours label starts after the
/// name's 58 and a gap of 16, and the 18 characters of its keys end 20 short
/// of the column's 800.
#[test]
fn the_header_names_each_side_and_the_merged_column() {
    let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    let theme = Theme::from_config(&config.expect("the shipped config parses"), "ermine_night")
        .expect("ermine_night loads");
    let grid = Grid {
        cols: GridColumns::compute(AREA, 2, 208),
        colors: GridColors::resolve(&theme).expect("ermine_night gives RGB colors"),
    };
    let merge = MergeState {
        head_sha: "f00dfeed".to_string(),
        head_name: Some("feature".to_string()),
        message: String::new(),
        conflicted: 1,
    };
    let labels = ConflictLabels::new(Some("main".to_string()), Some(&merge));
    let mut buf = Buffer::empty(AREA);
    let mut scene = ApcScene::new();
    paint_header(
        Rect { height: 1, ..AREA },
        &grid,
        &labels,
        232,
        216,
        &mut buf,
        &mut scene,
    );

    let commands = decode_stream(scene.buffer());
    let runs: Vec<_> = commands
        .iter()
        .filter_map(|command| match command {
            Command::TextRun(run) => Some((run.col, run.text.clone(), run.scale, run.color)),
            _ => None,
        })
        .collect();
    let rule = commands.iter().find_map(|command| match command {
        Command::Bar(bar) if bar.y == 15 => Some((bar.x, bar.width, bar.height, bar.color)),
        _ => None,
    });
    let bg = |x: u16| style_rgb(Some(buf[(x, 0)].bg));

    assert_eq!(
        (runs, bg(grid.cols.merged.x), bg(grid.cols.ours.x), rule),
        (
            [
                (20, "ours", 232, SUCCESS),
                (94, "HEAD \u{b7} main", 232, MUTED),
                (537, "click \u{b7} o \u{b7} O line", 216, MUTED),
                (820, "merged", 232, TEXT),
                (923, "working tree \u{b7} you edit here", 232, MUTED),
                (1748, "theirs", 232, SPECIAL),
                (1851, "feature \u{b7} f00dfee", 232, MUTED),
                (2265, "click \u{b7} t \u{b7} T line", 216, MUTED),
            ]
            .map(|(col, text, scale, color)| (col, text.to_string(), scale, color))
            .to_vec(),
            Some(MERGED_HEAD),
            None,
            Some((0, AREA.width * 16, 1, DIVIDER)),
        ),
    );
}

/// A column too narrow for its label cuts the label's tail at the label's own
/// scale. At 84 cells a side column is 26 cells. The ours column leaves 43
/// sixteenths between its name and its keys, so two characters of 14.5 fit
/// and a third does not. The longer name of the theirs column leaves 14, too
/// few for one character, so its label draws no run. The merged column's 32
/// cells leave room for 25 characters.
#[test]
fn a_narrow_column_cuts_its_label_at_the_label_scale() {
    let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    let theme = Theme::from_config(&config.expect("the shipped config parses"), "ermine_night")
        .expect("ermine_night loads");
    let area = Rect::new(0, 0, 84, 1);
    let grid = Grid {
        cols: GridColumns::compute(area, 2, 208),
        colors: GridColors::resolve(&theme).expect("ermine_night gives RGB colors"),
    };
    let merge = MergeState {
        head_sha: "f00dfeed".to_string(),
        head_name: Some("feature".to_string()),
        message: String::new(),
        conflicted: 1,
    };
    let labels = ConflictLabels::new(Some("main".to_string()), Some(&merge));
    let mut scene = ApcScene::new();
    paint_header(
        area,
        &grid,
        &labels,
        232,
        216,
        &mut Buffer::empty(area),
        &mut scene,
    );

    let cut: Vec<String> = decode_stream(scene.buffer())
        .into_iter()
        .filter_map(|command| match command {
            Command::TextRun(run) if run.color == MUTED && run.scale == 232 => Some(run.text),
            _ => None,
        })
        .collect();
    assert_eq!(cut, ["HE", "working tree \u{b7} you edit h"]);
}

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

/// The File Header runs a rule along its top and its bottom on the head color.
/// It tags the shown file's open conflicts and counts what theirs changes,
/// both against the merged column, names the file from the merged column's
/// pad, and ends in the file's place in the session against the theirs
/// column's right edge. The tag reads every open chunk as the total, some as
/// how many of the total stay left, and none as resolved, in the resolved
/// tag's colors. A zero count takes no run.
///
/// At the test scale every character advances 16 sixteenths. So the tag of
/// `conflict U+00B7 2` is 192 wide with 10 of pad each side, and the tag and
/// its two counts, 16 apart, end 20 short of the merged column's 800.
#[test]
fn the_file_header_tags_the_open_conflicts_and_counts_the_file() {
    let tags = [2, 1, 0].map(|open| {
        let (commands, _) = file_header(AREA, open, (9, 4));
        let tag = commands.iter().find_map(|command| match command {
            Command::Bar(bar) if bar.radius > 0 => {
                Some((bar.x, bar.y, bar.width, bar.height, bar.color))
            },
            _ => None,
        });
        let text = commands.iter().find_map(|command| match command {
            Command::TextRun(run) if run.bg.is_none() => {
                Some((run.col, run.text.clone(), run.color))
            },
            _ => None,
        });
        (tag, text)
    });
    let (commands, buf) = file_header(AREA, 2, (9, 4));
    let rules: Vec<_> = commands
        .iter()
        .filter_map(|command| match command {
            Command::Bar(bar) if bar.width == AREA.width * 16 => Some((bar.y, bar.color)),
            _ => None,
        })
        .collect();
    let runs: Vec<_> = commands
        .into_iter()
        .filter_map(|command| match command {
            Command::TextRun(run) if run.bg.is_some() => {
                Some((run.col, run.text, run.color, run.bold))
            },
            _ => None,
        })
        .collect();
    let one_count: Vec<_> = file_header(AREA, 2, (0, 3))
        .0
        .into_iter()
        .filter_map(|command| match command {
            Command::TextRun(run) if run.text.starts_with(['+', '\u{2212}']) => {
                Some((run.col, run.text))
            },
            _ => None,
        })
        .collect();

    assert_eq!(
        (
            tags,
            rules,
            runs,
            style_rgb(Some(buf[(0, 0)].bg)),
            one_count
        ),
        (
            [
                (
                    Some((472, 1, 212, 14, TAG_CONFLICT)),
                    Some((482, "conflict \u{b7} 2".to_string(), CONFLICT)),
                ),
                (
                    Some((312, 1, 372, 14, TAG_CONFLICT)),
                    Some((322, "conflict \u{b7} 1 of 2 left".to_string(), CONFLICT)),
                ),
                (
                    Some((504, 1, 180, 14, CHIP_OURS)),
                    Some((514, "\u{2713} resolved".to_string(), SUCCESS)),
                ),
            ],
            vec![(0, DIVIDER), (15, DIVIDER)],
            vec![
                (820, "src/".to_string(), MUTED, false),
                (884, "notes".to_string(), TEXT, true),
                (700, "+9".to_string(), SUCCESS, false),
                (748, "\u{2212}4".to_string(), DANGER, false),
                (2348, "file 1 / 1".to_string(), DISABLED, false),
            ],
            Some(MERGED_HEAD),
            vec![(748, "\u{2212}3".to_string())],
        ),
    );
}

/// A column too narrow for its part of the File Header drops that part whole.
/// At 66 cells the ours column has too little room for the tag and its counts,
/// and at 30 cells the theirs column has too little room for the place.
#[test]
fn a_narrow_column_drops_its_part_of_the_file_header() {
    let parts = [66, 30].map(|width| {
        let (commands, _) = file_header(Rect::new(0, 0, width, 13), 2, (9, 4));
        let tag = commands
            .iter()
            .any(|command| matches!(command, Command::Bar(bar) if bar.radius > 0));
        let place = commands.iter().any(
            |command| matches!(command, Command::TextRun(run) if run.text.starts_with("file")),
        );
        (tag, place)
    });

    assert_eq!(parts, [(false, true), (false, false)]);
}

/// Paint the File Header in `ermine_night` over the first row of `area` for
/// `src/notes` with `open` of its two chunks open and `counts` lines theirs
/// adds and removes, and answer the scene's commands and the cells.
fn file_header(area: Rect, open: usize, counts: (u32, u32)) -> (Vec<Command>, Buffer) {
    let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    let theme = Theme::from_config(&config.expect("the shipped config parses"), "ermine_night")
        .expect("ermine_night loads");
    let grid = Grid {
        cols: GridColumns::compute(area, 2, 256),
        colors: GridColors::resolve(&theme).expect("ermine_night gives RGB colors"),
    };
    let position = ConflictPosition {
        operation: "merge",
        chunk_at: None,
        chunk_total: 2,
        file_at: 1,
        file_total: 1,
        file_open: open,
        chunks_open: open,
        files_open: usize::from(open > 0),
    };
    let facts = FileFacts {
        path: "src/notes",
        position: Some(position),
        counts,
    };
    let mut buf = Buffer::empty(area);
    let mut scene = ApcScene::new();
    paint_file_header(
        Rect { height: 1, ..area },
        &grid,
        facts,
        &theme,
        &mut buf,
        &mut scene,
    );
    (decode_stream(scene.buffer()), buf)
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

/// Each chip takes the tone of its kind. A run or a chunk the merge takes from
/// ours takes the ours chip, one from theirs the theirs chip, and an unsettled
/// chunk the conflict chip.
#[test]
fn each_chip_takes_the_tone_of_its_kind() {
    let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    let theme = Theme::from_config(&config.expect("the shipped config parses"), "ermine_night")
        .expect("ermine_night loads");
    let colors = GridColors::resolve(&theme).expect("ermine_night gives RGB colors");
    let tint = |kind| {
        RowLook::of(kind, [false, false], &colors)
            .chip
            .map(|(_, tone)| tone.tint)
    };

    assert_eq!(
        [
            MergeRowKind::Ours,
            MergeRowKind::TookOurs,
            MergeRowKind::Theirs,
            MergeRowKind::TookTheirs,
            MergeRowKind::Conflict,
        ]
        .map(tint),
        [
            CHIP_OURS,
            CHIP_OURS,
            CHIP_THEIRS,
            CHIP_THEIRS,
            CHIP_CONFLICT
        ]
        .map(Some),
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
        (
            colors.band,
            colors.chip,
            [colors.chip_ours, colors.chip_theirs],
            [colors.tag, colors.tag_resolved],
            colors.ghost,
            colors.head
        ),
        (
            band,
            band,
            [colors.ours, colors.theirs],
            [band, colors.ours],
            Style::default()
                .fg(rgb([0x63, 0x6d, 0x83]))
                .add_modifier(Modifier::ITALIC),
            None,
        ),
        "the band takes the conflict header's color, the chip the band, each \
         side's chip its side, the tag the chip, the resolved tag the ours chip, \
         and the ghost muted italic text",
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
}

/// The 6/16 change bar of `column`'s cell on row `y`.
fn bar(column: u16, y: u16, color: [u8; 3]) -> BarFields {
    (column as i16 * 16 + 7, y as i16 * 16, 6, 16, color)
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
