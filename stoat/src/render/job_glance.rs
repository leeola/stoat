//! The Glance of a Job, a box under the Job's Tray Chip that reads the Job at a
//! glance.
//!
//! It names the Job's line with its state and its time, shows its newest
//! output line, and lists the keys that act on it.

use crate::{
    jobs,
    render::{job_tray, paint::style_rgb, text, TypeScale},
    run::Cmd,
    theme::{scope, Theme, TypeRole},
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style, widgets::StatefulWidget};
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

/// The colors of the Glance under a live scene, each as RGB.
struct GlanceColors {
    /// The box, which takes the fill of the Tray Chip.
    fill: [u8; 3],
    title: [u8; 3],
    output: [u8; 3],
    key: [u8; 3],
    word: [u8; 3],
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
/// it.
///
/// Under a live scene with RGB colors the box is a panel in the fill of the
/// Tray Chip, rounded at 6 px, above the pools. It casts no shadow, as the
/// board draws none, so its bottom edge stays crisp where it crosses a pane. In a
/// `band` it stands 2 px inside its first row and 8 px past its last, which
/// sets it 6 px under the band and makes it 60 px tall, and the glyphs of the
/// row it reaches into clear. Its lines draw at the Caption scale. A plain terminal draws the lines
/// in cells on the fill.
pub(crate) fn render_job_glance(
    cmd: &Cmd,
    rect: Rect,
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
        edges: match band {
            true => [
                Edge::Inset(BAND_TOP_INSET),
                Edge::Inset(0),
                Edge::Outset(BAND_BOTTOM_OUTSET),
                Edge::Inset(0),
            ],
            false => Edge::symmetric(0, 0),
        },
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: true,
        backdrop: false,
        anchor: None,
    }
    .draw_components(rect, scene);

    let scale = theme.type_scale(TypeRole::Caption);
    let advance = |chars: usize| text_run::advance_sixteenths(chars, scale);
    let capacity =
        (usize::from(rect.width) * 16).saturating_sub(36) / usize::from(advance(1).max(1));
    let title = glance_title(&cmd.command, word, &time, capacity);
    caption_run(GLANCE_PAD, 0, &title, colors.title, scale).render(rect, buf, scene);
    let output = cut(&output, capacity);
    caption_run(GLANCE_PAD, 16, &output, colors.output, scale).render(rect, buf, scene);

    let mut col = GLANCE_PAD;
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

#[cfg(test)]
mod tests {
    use super::{glance_keys, glance_title};
    use crate::{app::Stoat, render::OVERLAY_LANE, test_harness::TestHarness};
    use stoat_config::TabBarMode;
    use stoatty_protocol::{
        command::{self, Command, Edge},
        PROTOCOL_VERSION,
    };

    const RAISED: [u8; 3] = [0x1e, 0x21, 0x27];
    const TEXT: [u8; 3] = [0xdf, 0xe2, 0xe8];
    const INFO: [u8; 3] = [0x6c, 0xc7, 0xd6];
    const DIM: [u8; 3] = [0xa3, 0xa9, 0xb5];

    /// The Glance of the one Job on 80 columns hangs under the bar, its right
    /// edge on its chip's, with the first line and the first key at the pad.
    #[test]
    fn the_glance_hangs_under_its_chip() {
        let mut h = Stoat::test();
        let job = h.push_job("sleep 30");
        h.inject_cmd_output(job.cmd, b"one\r\ntwo\r\n");
        h.type_keys("escape space J k");
        h.snapshot();

        let mut panels = Vec::new();
        let mut runs = Vec::new();
        let wanted = |text: &str| ["sleep 30 \u{b7} running 0:00", "two", "Enter"].contains(&text);
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
        runs.sort_by_key(|run| run.2);
        assert_eq!(
            (panels, runs),
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
}
