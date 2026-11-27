//! The Glance of a Job, a box under the Job's Tray Chip that reads the Job at a
//! glance.
//!
//! It names the Job's line with its state and its time, shows its newest
//! output line, and lists the keys that act on it.
//!
//! A host that lays rows out by pixels draws the box at the board's px, which
//! end on the last pixel of the chip, and sets its lines 16 px apart.

use crate::{
    jobs::{self, TraySpan},
    render::{job_tray, paint::style_rgb, pixel_rows::RowMetrics, text, TypeScale},
    run::Cmd,
    theme::{scope, Theme, TypeRole},
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style, widgets::StatefulWidget};
use std::ops::Range;
use stoat_widgets::{
    panel::Panel,
    text_run::{self, TextRun},
    ApcScene,
};
use stoatty_protocol::command::{BorderStyle, Edge, PanelShadow};

/// The width of the Glance in cells, the board's 340 px at the 9 px cell.
const GLANCE_WIDTH: u16 = 38;

/// The rows of the Glance, one for each of its lines.
const GLANCE_ROWS: u16 = 3;

/// The sixteenths before the text of each line, the board's 10 px pad.
const GLANCE_PAD: u16 = 18;

/// The px that the box stands inside its first row in a band, which sets its
/// top 6 px under the 32 px band.
const BAND_TOP_INSET: u8 = 2;

/// The px that the box reaches past its last row in a band, which makes it
/// the board's 60 px tall, its three 16 px lines and its two 6 px pads.
///
/// The box stands by its edges rather than by a nudge, because a pane's pool
/// composite keeps clear of a panel's cells and edges alone, and a nudge
/// would move the box past what the pool keeps clear.
const BAND_BOTTOM_OUTSET: u8 = 8;

/// The logical px of the box's width at the board's pixels, the board's
/// `width: 340px`.
const GLANCE_PX: u16 = 340;

/// The logical px between each side of the box and its text, the board's
/// `padding: 6px 10px`.
const PAD_PX: u16 = 10;

/// The logical px between the box's top and its first line in a band, the
/// board's `padding: 6px 10px`.
const TOP_PX: u16 = 6;

/// The logical px of each line, the board's `line-height: 16px`.
const LINE_PX: u16 = 16;

/// The logical px that the box keeps inside the window at either side, the
/// bar's `padding: 0 8px`.
const EDGE_PX: u16 = 8;

/// The colors of the Glance under a live scene, each as RGB.
struct GlanceColors {
    /// The box, which takes the fill of the Tray Chip.
    fill: [u8; 3],
    title: [u8; 3],
    output: [u8; 3],
    key: [u8; 3],
    word: [u8; 3],
}

/// Where the box and the lines of a Glance under a live scene draw.
struct GlanceLayout {
    /// The cells that the box covers.
    rect: Rect,
    edges: [Edge; 4],
    /// The sixteenths from the left of `rect` to the start of each line.
    col: u16,
    /// The chars that a line holds.
    capacity: usize,
    /// The nudge in logical px of each line at the board's pixels, or `None`
    /// for lines on their rows.
    nudges: Option<[i16; 3]>,
}

impl GlanceLayout {
    /// The layout of a Glance over the cells of `rect`, where one char
    /// advances `advance` sixteenths.
    fn in_cells(rect: Rect, band: bool, advance: u16) -> GlanceLayout {
        GlanceLayout {
            rect,
            edges: cell_edges(band),
            col: GLANCE_PAD,
            capacity: (usize::from(rect.width) * 16).saturating_sub(36)
                / usize::from(advance.max(1)),
            nudges: None,
        }
    }

    /// The layout of a Glance at the board's pixels under `m` on the rows of
    /// `rect`, under `chip` in the window `window`, where one char advances
    /// `advance` sixteenths.
    ///
    /// The box ends on the chip's last pixel, or [`EDGE_PX`] inside the
    /// window with no chip, as [`glance_px`] places it. It covers the cells
    /// that hold its pixels and stands in from their outer edges by the px
    /// that it leaves of them. A line holds the chars that fit the box less
    /// its two pads.
    fn at_px(
        rect: Rect,
        chip: Option<&TraySpan>,
        band: bool,
        window: Rect,
        m: RowMetrics,
        advance: u16,
    ) -> GlanceLayout {
        let width = u32::from(m.cell[0]);
        let full_px = u32::from(window.x) * width..u32::from(window.right()) * width;
        let end = match chip {
            Some(TraySpan { px: Some(px), .. }) => px.end,
            Some(span) => u32::from(span.cells.end) * width,
            None => full_px.end.saturating_sub(m.px(EDGE_PX)),
        };
        let px = glance_px(full_px, end, m);

        let first = (px.start / width) as u16;
        let last = (px.end.saturating_sub(1) / width) as u16;
        let rect = Rect {
            x: first,
            width: last + 1 - first,
            ..rect
        };
        let off = px.start - u32::from(first) * width;
        let rest = u32::from(rect.right()) * width - px.end;
        let logical = |px: u32| (px as f32 / m.scale).round() as u8;
        let mut edges = cell_edges(band);
        edges[1] = Edge::Inset(logical(rest));
        edges[3] = Edge::Inset(logical(off));

        let room = (px.end - px.start).saturating_sub(2 * m.px(PAD_PX));
        GlanceLayout {
            rect,
            edges,
            col: m.px_cols(off + m.px(PAD_PX)),
            capacity: (room * 16 / (u32::from(advance) * width).max(1)) as usize,
            nudges: Some([0, 1, 2].map(|line| line_nudge(line, band, m))),
        }
    }

    /// Send the nudge of line `line`, where the lines stand at the board's
    /// pixels.
    fn nudge(&self, line: usize, scene: &mut ApcScene) {
        if let Some(nudges) = self.nudges {
            scene.nudge([0, nudges[line]]);
        }
    }
}

/// The rect of the Glance, under the rows of the tab bar with its right edge
/// at the window column `right`, or `None` for a window with no room for its
/// rows.
///
/// The Glance takes 38 columns, or the whole width of a narrower window, and
/// starts no further left than `full`. Its three rows start `top_rows` rows
/// down `full`.
pub(crate) fn glance_rect(full: Rect, top_rows: u16, right: u16) -> Option<Rect> {
    let y = full.y + top_rows;
    if y + GLANCE_ROWS > full.bottom() {
        return None;
    }
    let x = right.saturating_sub(GLANCE_WIDTH).max(full.x);
    Some(Rect::new(x, y, right.saturating_sub(x), GLANCE_ROWS))
}

/// Paint the Glance of the Job `cmd` over `rect`, as [`glance_rect`] places
/// it, under `chip`, the span of the Job's Tray Chip where one is on show.
///
/// Under a live scene with RGB colors the box is a panel in the fill of the
/// Tray Chip, rounded at 6 px, above the pools. It casts no shadow, as the
/// board draws none, so its bottom edge stays crisp where it crosses a pane. In a
/// `band` it stands 2 px inside its first row and 8 px past its last, which
/// sets it 6 px under the band and makes it 60 px tall, and the glyphs of the
/// row it reaches into clear. Its lines draw at the Caption scale. A plain terminal draws the lines
/// in cells on the fill.
///
/// Where the scene lays rows out by pixels, the box takes the rows of `rect`
/// and the board's 340 px. It ends on the last pixel of `chip`, or 8 px inside
/// the window with no chip, and keeps 8 px inside the window's left. Its lines
/// stand 16 px apart, each on its own row under a nudge.
pub(crate) fn render_job_glance(
    cmd: &Cmd,
    rect: Rect,
    chip: Option<&TraySpan>,
    band: bool,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let running = !cmd.finished;
    let word = jobs::state_of(cmd).word();
    let time = jobs::time_text(cmd);
    let output = newest_line(cmd);

    let Some(colors) = scene.live().then(|| glance_colors(theme)).flatten() else {
        let fill = job_tray::fill_bg(theme).map_or(Style::default(), |bg| Style::default().bg(bg));
        let tinted = |scope: &str| theme.get(scope).fg.map_or(fill, |fg| fill.fg(fg));
        for y in rect.y..rect.bottom() {
            for x in rect.x..rect.right() {
                text::write_cell(buf, x, y, ' ', fill);
            }
        }
        let capacity = usize::from(rect.width.saturating_sub(2));
        let title = glance_title(&cmd.command, word, &time, capacity);
        text::write_str(buf, rect.x + 1, rect.y, &title, tinted(scope::UI_TEXT));
        let output = cut(&output, capacity);
        text::write_str(
            buf,
            rect.x + 1,
            rect.y + 1,
            &output,
            tinted(scope::UI_TEXT_DIM),
        );
        let keys = key_line(&glance_keys(running, capacity));
        text::write_str(
            buf,
            rect.x + 1,
            rect.y + 2,
            &keys,
            tinted(scope::UI_TEXT_MUTED),
        );
        return;
    };

    let scale = theme.type_scale(TypeRole::Caption);
    let advance = |chars: usize| text_run::advance_sixteenths(chars, scale);
    let layout = match scene.pixel_rows() {
        Some((cell, px_scale)) => {
            let m = RowMetrics {
                cell,
                scale: px_scale,
            };
            GlanceLayout::at_px(rect, chip, band, buf.area, m, advance(1))
        },
        None => GlanceLayout::in_cells(rect, band, advance(1)),
    };
    let rect = layout.rect;

    for y in rect.y..(rect.bottom() + u16::from(band)).min(buf.area.bottom()) {
        for x in rect.x..rect.right().min(buf.area.right()) {
            buf[(x, y)].set_symbol(" ");
        }
    }
    Panel {
        style: BorderStyle::None_,
        border: colors.fill,
        corner_radius: 6,
        fill: Some(colors.fill),
        shadow: PanelShadow::None_,
        edges: layout.edges,
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: true,
        backdrop: false,
        anchor: None,
    }
    .draw_components(rect, scene);

    let capacity = layout.capacity;
    let title = glance_title(&cmd.command, word, &time, capacity);
    layout.nudge(0, scene);
    caption_run(layout.col, 0, &title, colors.title, scale).render(rect, buf, scene);
    let output = cut(&output, capacity);
    layout.nudge(1, scene);
    caption_run(layout.col, 16, &output, colors.output, scale).render(rect, buf, scene);

    layout.nudge(2, scene);
    let mut col = layout.col;
    for (index, (key, action)) in glance_keys(running, capacity).into_iter().enumerate() {
        if index > 0 {
            caption_run(col, 32, " \u{b7} ", colors.word, scale).render(rect, buf, scene);
            col += advance(3);
        }
        caption_run(col, 32, key, colors.key, scale).render(rect, buf, scene);
        col += advance(key.chars().count());
        let action = format!(" {action}");
        caption_run(col, 32, &action, colors.word, scale).render(rect, buf, scene);
        col += advance(action.chars().count());
    }
    if layout.nudges.is_some() {
        scene.nudge([0, 0]);
    }
}

/// The colors of the Glance under `theme`, or `None` when one of them has no
/// RGB value, which leaves the Glance to draw in cells.
///
/// The box takes the fill of the Tray Chip, the first line `ui.text`, the
/// output `ui.text.dim`, a key `ui.key_label`, and the words `ui.text.muted`.
fn glance_colors(theme: &Theme) -> Option<GlanceColors> {
    let fg = |scope: &str| style_rgb(theme.get(scope).fg);
    Some(GlanceColors {
        fill: style_rgb(job_tray::fill_bg(theme))?,
        title: fg(scope::UI_TEXT)?,
        output: fg(scope::UI_TEXT_DIM)?,
        key: fg(scope::UI_KEY_LABEL)?,
        word: fg(scope::UI_TEXT_MUTED)?,
    })
}

/// The first line of the Glance, the Cmd's `line`, U+00B7, the `word` of its
/// state, and its `time`, in `capacity` characters at most.
///
/// A line with no room cuts its end with U+2026 and keeps the state and the
/// time whole.
fn glance_title(line: &str, word: &str, time: &str, capacity: usize) -> String {
    let tail = format!(" \u{b7} {word} {time}");
    let room = capacity.saturating_sub(tail.chars().count());
    format!("{}{tail}", cut(line, room))
}

/// The key pairs of the last line of the Glance, as a key and the action that
/// it takes, that fit `capacity` characters joined by U+00B7.
///
/// A running Job reads `Enter open`, `C-r run again`, and `C-c stop`, and an
/// ended one `x dismiss` in place of the last. A pair with no room is left
/// out.
fn glance_keys(running: bool, capacity: usize) -> Vec<(&'static str, &'static str)> {
    let last = match running {
        true => ("C-c", "stop"),
        false => ("x", "dismiss"),
    };
    let mut used = 0;
    let mut keys = Vec::new();
    for (key, action) in [("Enter", "open"), ("C-r", "run again"), last] {
        let sep = match keys.is_empty() {
            true => 0,
            false => 3,
        };
        let width = sep + key.chars().count() + 1 + action.chars().count();
        if used + width > capacity {
            continue;
        }
        used += width;
        keys.push((key, action));
    }
    keys
}

/// The pairs `keys` as one line of text, for the cell look.
fn key_line(keys: &[(&str, &str)]) -> String {
    keys.iter()
        .map(|(key, action)| format!("{key} {action}"))
        .collect::<Vec<_>>()
        .join(" \u{b7} ")
}

/// The newest output line of `cmd`, the last row of its grid that holds text,
/// with its trailing blanks dropped, or the empty text for a Cmd with no
/// output.
fn newest_line(cmd: &Cmd) -> String {
    let width = usize::from(cmd.grid.width());
    (0..cmd.grid.rendered_line_count())
        .rev()
        .map(|row| cmd.grid.text_in(0..width, row..row + 1))
        .map(|text| text.trim_end().to_owned())
        .find(|text| !text.is_empty())
        .unwrap_or_default()
}

/// `text` in `room` characters at most, its end cut with U+2026 when it has
/// no room.
fn cut(text: &str, room: usize) -> String {
    if text.chars().count() <= room {
        return text.to_owned();
    }
    let kept: String = text.chars().take(room.saturating_sub(1)).collect();
    format!("{kept}\u{2026}")
}

/// A run of `text` at `col` sixteenths in and `row` sixteenths down the
/// Glance, at `scale` in `color`.
fn caption_run(col: u16, row: i16, text: &str, color: [u8; 3], scale: u16) -> TextRun<'_> {
    TextRun {
        col: col as i16,
        row,
        scale,
        color,
        bg: None,
        text,
        bold: false,
        italic: false,
        follow: 0,
        anchor: None,
    }
}

/// The physical px columns of the box of a Glance at the board's pixels under
/// `m`, in a window of the px columns `full_px`, for a box that ends at the
/// pixel column `end`.
///
/// The box is [`GLANCE_PX`] wide and starts no further left than
/// [`EDGE_PX`] inside the window.
fn glance_px(full_px: Range<u32>, end: u32, m: RowMetrics) -> Range<u32> {
    let start = end
        .saturating_sub(m.px(GLANCE_PX))
        .max(full_px.start + m.px(EDGE_PX));
    start.min(end)..end
}

/// The nudge in logical px that sets line `line` of a Glance at the board's
/// pixels on its 16 px line, from the top of the box's row `line`.
///
/// Each line stands [`LINE_PX`] under the line before it, and the box of its
/// run, a cell high, centers on it. In a `band` the first line stands
/// [`TOP_PX`] under the box's top, which stands [`BAND_TOP_INSET`] px into its
/// first row. With no band the box is its three rows, and the lines center in
/// them.
fn line_nudge(line: u16, band: bool, m: RowMetrics) -> i16 {
    let cell = i32::from(m.cell[1]);
    let lines = m.px(LINE_PX) as i32;
    let first = match band {
        true => (m.px(u16::from(BAND_TOP_INSET)) + m.px(TOP_PX)) as i32,
        false => i32::from(GLANCE_ROWS) * (cell - lines) / 2,
    };
    let run = first + i32::from(line) * (lines - cell) - (cell - lines) / 2;
    (run as f32 / m.scale).round() as i16
}

/// The top and the bottom edge of the box of a Glance, with its sides on its
/// cells.
///
/// In a `band` the box stands 2 px inside its first row and 8 px past its
/// last. With no band it stands on its rows.
fn cell_edges(band: bool) -> [Edge; 4] {
    match band {
        true => [
            Edge::Inset(BAND_TOP_INSET),
            Edge::Inset(0),
            Edge::Outset(BAND_BOTTOM_OUTSET),
            Edge::Inset(0),
        ],
        false => Edge::symmetric(0, 0),
    }
}

#[cfg(test)]
mod tests {
    use super::{glance_keys, glance_title};
    use crate::{
        app::Stoat,
        jobs::TraySpan,
        render::{pixel_rows::RowMetrics, OVERLAY_LANE},
        run::CmdId,
        test_harness::TestHarness,
    };
    use ratatui::{buffer::Buffer, layout::Rect};
    use stoat_config::TabBarMode;
    use stoat_widgets::ApcScene;
    use stoatty_protocol::{
        command::{self, Command, Edge},
        PROTOCOL_VERSION,
    };

    const RAISED: [u8; 3] = [0x1e, 0x21, 0x27];
    const TEXT: [u8; 3] = [0xdf, 0xe2, 0xe8];
    const INFO: [u8; 3] = [0x6c, 0xc7, 0xd6];
    const DIM: [u8; 3] = [0xa3, 0xa9, 0xb5];

    /// The board's 9 by 18 px cell at the scale 1.
    const M1: RowMetrics = RowMetrics {
        cell: [9, 18],
        scale: 1.0,
    };

    /// The board's cell at the scale 2, 18 by 36 physical px.
    const M2: RowMetrics = RowMetrics {
        cell: [18, 36],
        scale: 2.0,
    };

    /// A panel of the Glance as its left, its top, its width, its height, its
    /// edges, and whether it stands above the pools.
    type PanelSpec = (u16, u16, u16, u16, [Edge; 4], bool);

    /// A run of the Glance as its text, its col, its row, and its color.
    type Run = (String, i16, i16, [u8; 3]);

    /// The panels of the Glance that a snapshot of `h` draws, and the runs on
    /// the rows of its first panel whose text `wanted` names, by row.
    fn glance(h: &mut TestHarness, wanted: &[&str]) -> (Vec<PanelSpec>, Vec<Run>) {
        h.snapshot();
        let mut panels = Vec::new();
        let mut runs = Vec::new();
        let wanted = |text: &str| wanted.contains(&text);
        for command in command::decode_stream(&h.stoat.apc_scene.all_bytes()) {
            match command {
                Command::Panel(panel) if panel.corner_radius == 6 && panel.fill == Some(RAISED) => {
                    panels.push((
                        panel.left,
                        panel.top,
                        panel.width,
                        panel.height,
                        panel.edges,
                        panel.above_pools,
                    ))
                },
                Command::TextRun(run) if wanted(&run.text) => {
                    runs.push((run.text, run.col, run.row, run.color))
                },
                Command::TextRuns(group) => runs.extend(
                    group
                        .runs
                        .into_iter()
                        .filter(|(.., text)| wanted(text))
                        .map(|(col, row, text)| (text, col, row, group.color)),
                ),
                _ => {},
            }
        }
        let rows = panels.first().map_or(0..0, |&(_, top, _, height, ..)| {
            top as i16 * 16..(top + height) as i16 * 16
        });
        runs.retain(|run| rows.contains(&run.2));
        runs.sort_by_key(|run| run.2);
        (panels, runs)
    }

    /// `h` under a stoatty that lays rows out by pixels, with a 9 by 18 px cell
    /// at the scale 1, whose tab bar takes a band.
    fn in_pixels(mut h: TestHarness) -> TestHarness {
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        h.stoat.cell_pixels = Some((9, 18));
        h.stoat.display_scale = Some(1.0);
        h
    }

    /// The edges of a pixel Glance in a band, standing in `right` and `left`
    /// px from the outer edges of its last and its first cell.
    fn pixel_edges(right: u8, left: u8) -> [Edge; 4] {
        [
            Edge::Inset(2),
            Edge::Inset(right),
            Edge::Outset(8),
            Edge::Inset(left),
        ]
    }

    /// The commands of the Glance of the Job `cmd` of `h`, painted over the
    /// cells (41, 1, 38, 3) of 80 columns into a scene that lays rows out by
    /// pixels under `m`, under `chip`, in a `band` or not.
    fn pixel_paint(
        h: &TestHarness,
        cmd: CmdId,
        chip: Option<&TraySpan>,
        band: bool,
        m: RowMetrics,
    ) -> Vec<Command> {
        let mut scene = ApcScene::new();
        scene.set_live(true);
        scene.set_nudge_protocol(true);
        scene.set_pixel_rows(Some((m.cell, m.scale)));
        let mut buf = Buffer::empty(Rect::new(0, 0, 80, 24));
        let cmd = h
            .stoat
            .active_workspace()
            .cmds
            .get(cmd)
            .expect("the Job's Cmd");
        let rect = Rect::new(41, 1, 38, 3);
        super::render_job_glance(cmd, rect, chip, band, &h.stoat.theme, &mut buf, &mut scene);
        command::decode_stream(&scene.all_bytes())
    }

    /// The left, the width, and the edges of each panel of `commands`.
    fn panels_of(commands: Vec<Command>) -> Vec<(u16, u16, [Edge; 4])> {
        commands
            .into_iter()
            .filter_map(|command| match command {
                Command::Panel(panel) => Some((panel.left, panel.width, panel.edges)),
                _ => None,
            })
            .collect()
    }

    /// The Glance of the one Job on 80 columns hangs under the bar, its right
    /// edge on its chip's, with the first line and the first key at the pad.
    #[test]
    fn the_glance_hangs_under_its_chip() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.inject_cmd_output(job.cmd, b"one\r\ntwo\r\n");
        h.type_keys("escape space J k");

        assert_eq!(
            glance(&mut h, &["sleep 30 \u{b7} running 0:00", "two", "Enter"]),
            (
                vec![(41, 1, 38, 3, Edge::symmetric(0, 0), true)],
                vec![
                    ("sleep 30 \u{b7} running 0:00".to_owned(), 674, 16, TEXT),
                    ("two".to_owned(), 674, 32, DIM),
                    ("Enter".to_owned(), 674, 48, INFO),
                ],
            ),
        );
    }

    /// A Glance over its cells holds the chars that fit its 38 cells less its
    /// two pads, 35 at the full cell, so a longer line cuts.
    #[test]
    fn a_glance_in_cells_holds_thirty_five_chars_a_line() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.inject_cmd_output(job.cmd, format!("{}\r\n", "x".repeat(40)).as_bytes());
        h.type_keys("escape space J k");
        let cut = format!("{}\u{2026}", "x".repeat(34));

        assert_eq!(glance(&mut h, &[&cut]).1, [(cut.clone(), 674, 32, DIM)]);
    }

    /// In a band the Glance stands 2 px inside its first row and 8 px past its
    /// last, 6 px under the band, by its edges and under no nudge, so that a
    /// pane's pool composite keeps clear of the whole box.
    #[test]
    fn a_glance_in_a_band_stands_six_pixels_under_the_bar() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        h.stoat.settings.ui_tab_bar = Some(TabBarMode::Always);
        h.push_job("sleep 30");
        h.type_keys("escape space J k");
        h.snapshot();

        let mut nudge = [0, 0];
        let mut glance = Vec::new();
        for command in command::decode_stream(h.stoat.apc_scene.lane_bytes(OVERLAY_LANE)) {
            match command {
                Command::Nudge(offset) => nudge = offset,
                Command::Panel(panel) if panel.corner_radius == 6 && panel.fill == Some(RAISED) => {
                    glance.push((panel.left, panel.top, panel.edges, nudge))
                },
                _ => {},
            }
        }
        let band = [
            Edge::Inset(2),
            Edge::Inset(0),
            Edge::Outset(8),
            Edge::Inset(0),
        ];
        assert_eq!((glance, nudge), (vec![(121, 2, band, [0, 0])], [0, 0]));
    }

    /// A first line with no room cuts the Cmd's line and keeps the state and
    /// the time whole.
    #[test]
    fn a_long_line_keeps_its_state_and_time() {
        let line = "x".repeat(60);

        assert_eq!(
            glance_title(&line, "running", "0:22", 42),
            format!("{}\u{2026} \u{b7} running 0:22", "x".repeat(26)),
        );
    }

    /// The Glance of an older Job hangs under that Job's chip, its right edge
    /// on the chip's, rather than at the window's right edge.
    #[test]
    fn the_glance_of_an_older_job_hangs_under_its_chip() {
        let mut h = Stoat::test();
        h.push_job("sleep 30");
        h.push_job("sleep 31");
        h.type_keys("escape space J k h");
        h.snapshot();

        let lefts: Vec<u16> = command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|command| match command {
                Command::Panel(panel) if panel.corner_radius == 6 && panel.fill == Some(RAISED) => {
                    Some(panel.left)
                },
                _ => None,
            })
            .collect();
        assert_eq!(lefts, [62 - 38]);
    }

    /// A terminal with no scene draws the Glance in cells on the chip's fill,
    /// each line one cell in, and the key line leaves out the pair with no
    /// room in the cells.
    #[test]
    fn a_plain_terminal_draws_the_glance_in_cells() {
        let mut h = Stoat::test();
        h.stoat.stoatty = false;
        h.push_job("sleep 30");
        h.type_keys("escape space J k");
        h.snapshot();

        let buf = h.rendered_buffer();
        let row = |y: u16| -> String {
            let text: String = (42..79).map(|x| buf[(x, y)].symbol()).collect();
            text.trim_end().to_owned()
        };
        let [r, g, b] = RAISED;
        assert_eq!(
            ([row(1), row(3)], buf[(41, 1)].bg),
            (
                [
                    "sleep 30 \u{b7} running 0:00".to_owned(),
                    "Enter open \u{b7} C-r run again".to_owned(),
                ],
                ratatui::style::Color::Rgb(r, g, b),
            ),
        );
    }

    /// The key line leaves out a pair with no room, and an ended Job ends in
    /// `x dismiss` in place of `C-c stop`.
    #[test]
    fn the_key_line_leaves_out_a_pair_with_no_room() {
        assert_eq!(
            [
                glance_keys(true, 42),
                glance_keys(true, 35),
                glance_keys(false, 42)
            ],
            [
                vec![("Enter", "open"), ("C-r", "run again"), ("C-c", "stop")],
                vec![("Enter", "open"), ("C-r", "run again")],
                vec![("Enter", "open"), ("C-r", "run again"), ("x", "dismiss")],
            ],
        );
    }

    /// At the board's pixels the Glance is 340 px wide and ends on the last
    /// pixel of its chip. The chip of `sleep 30` ends on pixel 711, so the box
    /// takes the pixels 372 to 711, 3 px into the cell 41 and 8 px short of
    /// the end of the cell 79, and each line starts 10 px into the box, at col
    /// 679. A line holds the 35 chars that fit the box less its two pads.
    #[test]
    fn the_glance_is_three_hundred_forty_pixels_wide_and_ends_on_its_chip() {
        let mut h = in_pixels(Stoat::test());
        let job = h.push_job("sleep 30");
        let long = "x".repeat(40);
        h.inject_cmd_output(job.cmd, format!("one\r\n{long}\r\n").as_bytes());
        h.type_keys("escape space J k");
        let cut = format!("{}\u{2026}", "x".repeat(34));

        assert_eq!(
            glance(&mut h, &["sleep 30 \u{b7} running 0:00", &cut, "Enter"]),
            (
                vec![(41, 2, 39, 3, pixel_edges(8, 3), true)],
                vec![
                    ("sleep 30 \u{b7} running 0:00".to_owned(), 679, 32, TEXT),
                    (cut.clone(), 679, 48, DIM),
                    ("Enter".to_owned(), 679, 64, INFO),
                ],
            ),
        );
    }

    /// At the board's pixels each line stands 16 px under the line before it,
    /// on its own row under a nudge, and the nudge ends after the last line.
    /// Each run's box is the 18 px cell, 1 px over its 16 px line. In a band
    /// the box stands 2 px into its first row and its first line 6 px under
    /// that, so the rows take the nudges 7, 5, and 3. With no band the lines
    /// center in the box's three rows, which take 2, 0, and -2. A nudge is in
    /// logical px, so the scale 2 takes the same nudges.
    #[test]
    fn the_glance_lines_stand_sixteen_pixels_apart() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.inject_cmd_output(job.cmd, b"two\r\n");
        let lines = |band: bool, m: RowMetrics| {
            let mut nudge = [0, 0];
            let mut lines = Vec::new();
            for command in pixel_paint(&h, job.cmd, None, band, m) {
                match command {
                    Command::Nudge(offset) => nudge = offset,
                    Command::TextRun(run) if run.col == 679 => lines.push((run.row, nudge)),
                    Command::TextRuns(group) => lines.extend(
                        group
                            .runs
                            .iter()
                            .filter(|(col, ..)| *col == 679)
                            .map(|(_, row, _)| (*row, nudge)),
                    ),
                    _ => {},
                }
            }
            (lines, nudge)
        };

        let row = (vec![(16, [0, 2]), (32, [0, 0]), (48, [0, -2])], [0, 0]);
        let band = (vec![(16, [0, 7]), (32, [0, 5]), (48, [0, 3])], [0, 0]);
        assert_eq!(
            [
                lines(false, M1),
                lines(true, M1),
                lines(false, M2),
                lines(true, M2)
            ],
            [row.clone(), band.clone(), row, band],
        );
    }

    /// At the scale 2 the Glance counts physical px and states its insets in
    /// logical px. With no chip on an 18 by 36 px cell its 680 px end 16 px
    /// inside the 1440 px window and start 6 px into the cell 41, so it stands
    /// on the cells of the scale 1.
    #[test]
    fn a_glance_at_the_scale_two_counts_physical_pixels() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");

        let edges = [
            Edge::Inset(0),
            Edge::Inset(8),
            Edge::Inset(0),
            Edge::Inset(3),
        ];
        assert_eq!(
            panels_of(pixel_paint(&h, job.cmd, None, false, M2)),
            [(41, 39, edges)],
        );
    }

    /// With no chip on show the Glance ends 8 px inside the window. The oldest
    /// of three Jobs on 80 columns has dropped off the tray, so its box takes
    /// the pixels 372 to 711.
    #[test]
    fn a_glance_with_no_chip_ends_eight_pixels_inside_the_window() {
        let mut h = in_pixels(Stoat::test());
        let oldest = h.push_job("sleep 30");
        h.push_job("sleep 31");
        h.push_job("sleep 32");
        h.type_keys("escape space J k h h");
        let (panels, _) = glance(&mut h, &[]);

        let shown = h.stoat.tray_spans.iter().any(|span| span.cmd == oldest.cmd);
        let glanced = h.stoat.job_glance.as_ref().map(|glance| glance.cmd);
        assert_eq!(
            (shown, glanced, panels),
            (
                false,
                Some(oldest.cmd),
                vec![(41, 2, 39, 3, pixel_edges(8, 3), true)],
            ),
        );
    }

    /// The Glance of an older Job ends on the last pixel of that Job's chip,
    /// 566, so its box takes the pixels 227 to 566, 2 px into the cell 25.
    #[test]
    fn the_glance_of_an_older_job_ends_on_its_pixel_chip() {
        let mut h = in_pixels(Stoat::test());
        h.push_job("sleep 30");
        h.push_job("sleep 31");
        h.type_keys("escape space J k h");
        let (panels, _) = glance(&mut h, &[]);

        assert_eq!(panels, [(25, 2, 38, 3, pixel_edges(0, 2), true)]);
    }

    /// A chip in cells ends on the edge of its last cell, so a pixel Glance
    /// under the chip of the cells 62 to 78 ends at the pixel 711 and covers
    /// the cells 41 to 78, 2 px in from the left of the first.
    #[test]
    fn a_pixel_glance_ends_on_a_chip_in_cells() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        let chip = TraySpan {
            cmd: job.cmd,
            cells: 62..79,
            px: None,
        };
        let edges = [
            Edge::Inset(0),
            Edge::Inset(0),
            Edge::Inset(0),
            Edge::Inset(2),
        ];
        assert_eq!(
            panels_of(pixel_paint(&h, job.cmd, Some(&chip), false, M1)),
            [(41, 38, edges)],
        );
    }

    /// The box keeps 8 px inside the window's left, so in a window of 351 px
    /// a Glance that ends at the pixel 343 starts at 8 rather than at 3. A
    /// window too narrow for those 8 px gives the box no width.
    #[test]
    fn a_narrow_window_keeps_the_glance_eight_pixels_in() {
        assert_eq!(
            [
                super::glance_px(0..720, 712, M1),
                super::glance_px(0..351, 343, M1),
                super::glance_px(0..4, 0, M1),
            ],
            [372..712, 8..343, 0..0],
        );
    }
}
