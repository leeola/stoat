//! The Job Tray of the tab bar, a Tray Chip for each Job of the workspace at
//! the right end of the bar, newest at the right.
//!
//! A chip keeps a Cmd that left its Run pane in sight from every tab. It names
//! the Job, shows the state of its Cmd, and reads its progress or its time.
//! Under a live scene the chip is the pill of the active tab with a dot or a
//! mark and two runs of text, and a running Job with a total draws the
//! Progress Edge along the bottom of the pill. A plain terminal draws the chip
//! in cells.

use crate::{
    jobs::{self, JobState, JobView},
    render::{
        paint::style_rgb,
        pane::{tab_pill_panel, CHIP_PAD},
        text, FrameCtx, TypeScale,
    },
    theme::{scope, Theme, TypeRole},
};
use ratatui::{buffer::Buffer, layout::Rect, style::Color, widgets::StatefulWidget};
use std::ops::Range;
use stoat_widgets::{
    bar::Bar,
    text_run::{self, TextRun},
    ApcScene,
};
use stoatty_protocol::command::Edge;

/// The space between the parts of a chip in sixteenths, the board's
/// `gap: 6px` at two sixteenths to the px.
const TRAY_GAP: u16 = 12;

/// The width and the height of a running Job's dot in sixteenths.
///
/// The board draws the dot 6 by 6 px. A 9 by 18 px cell draws 11 sixteenths
/// across and 6 down at 6 px each way.
const DOT_SIZE: (u16, u16) = (11, 6);

/// The top of the dot in sixteenths down the row, which a 9 by 18 px cell
/// draws on pixel rows 6 to 11 of its row.
const DOT_TOP: i16 = 5;

/// The space that a chip keeps at its right before a newer chip, in
/// sixteenths, the board's `gap: 3px` between the chips.
const TRAY_SPACE: u16 = 6;

/// The height of the Progress Edge in sixteenths, the board's 2 px.
const EDGE_HEIGHT: u16 = 2;

/// The top of the Progress Edge in sixteenths down the row in a band, the
/// last 2 px of the box, which stands under the row there.
const EDGE_TOP_BAND: i16 = 16;

/// The top of the Progress Edge in sixteenths down the row with no band, the
/// last 2 px of the box inside the row.
const EDGE_TOP_ROW: i16 = 13;

/// The colors that the chips draw in under a live scene, each as RGB.
pub(super) struct TrayColors {
    /// The box of a chip.
    fill: [u8; 3],
    /// The name of a Job.
    name: [u8; 3],
    /// The name of a failed Job, which reads stronger than the others.
    strong: [u8; 3],
    /// The progress or the time of a Job.
    meta: [u8; 3],
    running: [u8; 3],
    ok: [u8; 3],
    failed: [u8; 3],
    stopped: [u8; 3],
}

impl TrayColors {
    /// The color of `state`, which draws its mark and its Progress Edge.
    fn state(&self, state: JobState) -> [u8; 3] {
        match state {
            JobState::Running => self.running,
            JobState::Ok => self.ok,
            JobState::Failed => self.failed,
            JobState::Stopped => self.stopped,
        }
    }
}

/// One chip of the tray, the Job it shows and the cells it takes.
struct Chip<'a> {
    view: &'a JobView,
    rect: Rect,
    /// Whether a newer chip follows, so the chip keeps the space between the
    /// two at its right.
    spaced: bool,
}

impl Chip<'_> {
    /// Draw the chip as the pill of the active tab, its text in runs at the
    /// Caption scale.
    ///
    /// The cells under the pill take the bar's style, so no glyph that the bar
    /// left there shows through it.
    fn draw_rich(
        &self,
        colors: &TrayColors,
        frame: FrameCtx<'_>,
        buf: &mut Buffer,
        scene: &mut ApcScene,
    ) {
        let base = frame.theme.get(scope::UI_TABBAR_INACTIVE);
        for x in self.rect.x..self.rect.right() {
            text::write_cell(buf, x, self.rect.y, ' ', base);
        }
        let mut panel = tab_pill_panel(colors.fill, frame.chrome_bands);
        if self.spaced {
            panel.edges[1] = Edge::Inset(3);
        }
        panel.draw_components(self.rect, scene);

        let scale = frame.theme.type_scale(TypeRole::Caption);
        let view = self.view;
        let state = colors.state(view.state);
        let failed = view.state == JobState::Failed;
        let mut col = CHIP_PAD;
        match view.state {
            JobState::Running => {
                Bar {
                    x: col as i16,
                    y: DOT_TOP,
                    width: DOT_SIZE.0,
                    height: DOT_SIZE.1,
                    color: state,
                    radius: 3,
                    corners: 0,
                }
                .render(self.rect, buf, scene);
                col += DOT_SIZE.0;
            },
            ended => {
                let mark = ended.mark().to_string();
                caption_run(col, &mark, state, true, scale).render(self.rect, buf, scene);
                col += text_run::advance_sixteenths(1, scale);
            },
        }

        col += TRAY_GAP;
        let name = match failed {
            true => colors.strong,
            false => colors.name,
        };
        caption_run(col, &view.name, name, failed, scale).render(self.rect, buf, scene);
        col += text_run::advance_sixteenths(view.name.chars().count(), scale);

        let meta = chip_meta(view);
        if !meta.is_empty() {
            let color = match failed {
                true => state,
                false => colors.meta,
            };
            caption_run(col + TRAY_GAP, &meta, color, false, scale).render(self.rect, buf, scene);
        }

        if let Some((done, total)) = view.progress.filter(|&(_, total)| total > 0) {
            let span = u32::from(self.rect.width) * 16 - u32::from(trailing_space(self.spaced));
            let width = done.min(total) * span / total;
            Bar {
                x: 0,
                y: match frame.chrome_bands {
                    true => EDGE_TOP_BAND,
                    false => EDGE_TOP_ROW,
                },
                width: width as u16,
                height: EDGE_HEIGHT,
                color: state,
                radius: 1,
                corners: 0,
            }
            .render(self.rect, buf, scene);
        }
    }

    /// Draw the chip in cells, for a terminal with no scene or a theme with a
    /// color outside RGB.
    ///
    /// The cells read a pad cell, the mark, a space, the name, a space, the
    /// meta, and a pad cell, each part in the color that the rich look draws
    /// it in, on the fill of the box. The cell that a spaced chip keeps at
    /// its right holds the bar's ground.
    fn draw_cells(&self, frame: FrameCtx<'_>, buf: &mut Buffer) {
        let theme = frame.theme;
        let base = theme.get(scope::UI_TABBAR_INACTIVE);
        let fill = fill_bg(theme).map_or(base, |bg| base.bg(bg));
        let tinted = |color: Option<Color>| color.map_or(fill, |fg| fill.fg(fg));
        let view = self.view;
        let failed = view.state == JobState::Failed;
        let state = tinted(jobs::state_style(theme, view.state).fg);
        let name = match failed {
            true => tinted(theme.get(scope::UI_TEXT).fg),
            false => tinted(theme.get(scope::UI_TEXT_DIM).fg),
        };
        let meta_style = match failed {
            true => state,
            false => tinted(theme.get(scope::UI_TEXT_MUTED).fg),
        };

        let Rect { x, y, .. } = self.rect;
        for col in x..self.rect.right() - u16::from(self.spaced) {
            text::write_cell(buf, col, y, ' ', fill);
        }
        text::write_cell(buf, x + 1, y, view.state.mark(), state);
        text::write_str(buf, x + 3, y, &view.name, name);
        let meta = chip_meta(view);
        if !meta.is_empty() {
            let meta_x = x + 4 + view.name.chars().count() as u16;
            text::write_str(buf, meta_x, y, &meta, meta_style);
        }
    }
}

/// The colors of the chips under `theme`, or `None` when one of them has no
/// RGB value, which leaves the chips to draw in cells.
///
/// The box takes the background of `ui.job.chip`, the name `ui.text.dim`, the
/// name of a failed Job `ui.text`, the meta `ui.text.muted`, and each state
/// the color of [`jobs::state_style`].
pub(super) fn tray_colors(theme: &Theme) -> Option<TrayColors> {
    let state = |state| style_rgb(jobs::state_style(theme, state).fg);
    Some(TrayColors {
        fill: style_rgb(fill_bg(theme))?,
        name: style_rgb(theme.get(scope::UI_TEXT_DIM).fg)?,
        strong: style_rgb(theme.get(scope::UI_TEXT).fg)?,
        meta: style_rgb(theme.get(scope::UI_TEXT_MUTED).fg)?,
        running: state(JobState::Running)?,
        ok: state(JobState::Ok)?,
        failed: state(JobState::Failed)?,
        stopped: state(JobState::Stopped)?,
    })
}

/// Where the chips of `views`, oldest first, stand on the bar, from left to
/// right, each with the index of its view and its columns.
///
/// The newest chip ends at `right`, and each older chip ends where the chip
/// after it starts. Its text measures at `scale`, or in cells with no scale.
/// The walk from the newest stops at the first chip that would start left of
/// `floor`, so that chip and each older one stay off the bar.
pub(super) fn tray_layout(
    views: &[JobView],
    right: u16,
    floor: u16,
    scale: Option<u16>,
) -> Vec<(usize, Range<u16>)> {
    let mut placed = Vec::new();
    let mut end = right;
    for (index, view) in views.iter().enumerate().rev() {
        let spaced = index + 1 < views.len();
        let Some(start) = end
            .checked_sub(chip_cells(view, scale, spaced))
            .filter(|&start| start >= floor)
        else {
            break;
        };
        placed.push((index, start..end));
        end = start;
    }
    placed.reverse();
    placed
}

/// Draw each chip of `placed` on the tab bar's `row`, the Job of `views`
/// that its index names.
///
/// With `colors` each chip draws as the pill of the active tab, which stands
/// outside the row in a band as the tab's does. With none it draws in cells.
pub(super) fn draw_tray(
    views: &[JobView],
    placed: &[(usize, Range<u16>)],
    colors: Option<&TrayColors>,
    row: Rect,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    for (index, cells) in placed {
        let chip = Chip {
            view: &views[*index],
            rect: Rect::new(cells.start, row.y, cells.end - cells.start, 1),
            spaced: index + 1 < views.len(),
        };
        match colors {
            Some(colors) => chip.draw_rich(colors, frame, buf, scene),
            None => chip.draw_cells(frame, buf),
        }
    }
}

/// The background that fills the box of a chip, from `ui.job.chip`, else from
/// `ui.surface.raised`, else from `ui.chip`.
pub(super) fn fill_bg(theme: &Theme) -> Option<Color> {
    [scope::UI_JOB_CHIP, scope::UI_SURFACE_RAISED]
        .into_iter()
        .find_map(|scope| theme.try_get(scope).and_then(|style| style.bg))
        .or(theme.get(scope::UI_CHIP).bg)
}

/// The meta of the chip of `view`, the progress such as `212/480` while it
/// has one, and else the time of the Job.
fn chip_meta(view: &JobView) -> String {
    match view.progress {
        Some((done, total)) => format!("{done}/{total}"),
        None => view.time.clone(),
    }
}

/// The whole cells that the chip of `view` takes, its text measured at
/// `scale`, or in cells with no scale.
///
/// With a scale the chip counts its pad, its mark, a gap, its name, a gap,
/// its meta, and its pad in sixteenths, where the mark of a running Job is its
/// dot and each other mark one glyph. An empty meta drops its gap. A `spaced`
/// chip counts [`TRAY_SPACE`] too.
///
/// In cells it counts a pad cell at each end, the mark, a space, and the name,
/// then a space and the meta when the meta holds any, and one more cell for a
/// `spaced` chip.
fn chip_cells(view: &JobView, scale: Option<u16>, spaced: bool) -> u16 {
    let name = view.name.chars().count();
    let meta = chip_meta(view).chars().count();
    let Some(scale) = scale else {
        let meta = match meta {
            0 => 0,
            chars => 1 + chars as u16,
        };
        return 4 + name as u16 + meta + u16::from(spaced);
    };

    let mark = match view.state {
        JobState::Running => DOT_SIZE.0,
        _ => text_run::advance_sixteenths(1, scale),
    };
    let meta = match meta {
        0 => 0,
        chars => TRAY_GAP + text_run::advance_sixteenths(chars, scale),
    };
    let name = text_run::advance_sixteenths(name, scale);
    (2 * CHIP_PAD + mark + TRAY_GAP + name + meta + trailing_space(spaced)).div_ceil(16)
}

/// The sixteenths that a chip keeps at its right before a newer chip, none
/// for the newest.
fn trailing_space(spaced: bool) -> u16 {
    match spaced {
        true => TRAY_SPACE,
        false => 0,
    }
}

/// A run of `text` at `col` sixteenths into a chip, at `scale` in `color`.
fn caption_run(col: u16, text: &str, color: [u8; 3], bold: bool, scale: u16) -> TextRun<'_> {
    TextRun {
        col: col as i16,
        row: 0,
        scale,
        color,
        bg: None,
        text,
        bold,
        italic: false,
        follow: 0,
        anchor: None,
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        app::Stoat,
        badge::{Anchor, Badge, BadgeSource, BadgeState},
        jobs::{JobState, JobView},
        render::badges,
        run::CmdId,
        test_harness::TestHarness,
        theme::Theme,
    };
    use ratatui::{layout::Rect, style::Color};
    use std::time::Duration;
    use stoat_config::TabBarMode;
    use stoatty_protocol::{
        command::{self, Command, Edge},
        PROTOCOL_VERSION,
    };

    const RAISED: [u8; 3] = [0x1e, 0x21, 0x27];
    const SPECIAL: [u8; 3] = [0xc5, 0x9a, 0xf0];
    const DIM: [u8; 3] = [0xa3, 0xa9, 0xb5];
    const MUTED: [u8; 3] = [0x83, 0x8a, 0x98];
    const SUCCESS: [u8; 3] = [0x8f, 0xd1, 0x8a];
    const DANGER: [u8; 3] = [0xf2, 0x77, 0x7f];
    const TEXT: [u8; 3] = [0xdf, 0xe2, 0xe8];

    /// The edges of a chip's box inside the bar's row, as [`Panel::chip`]
    /// places them.
    ///
    /// [`Panel::chip`]: stoat_widgets::panel::Panel::chip
    const ROW_EDGES: [Edge; 4] = [
        Edge::Inset(1),
        Edge::Inset(0),
        Edge::Inset(1),
        Edge::Inset(0),
    ];

    /// A bar of a scene as its x, y, width, height, and color.
    type BarSpec = (i16, i16, u16, u16, [u8; 3]);

    /// A run of a scene as its text, its col, its color, and whether it is
    /// bold.
    type Run = (String, i16, [u8; 3], bool);

    /// What the last frame draws of the Job Tray on the tab bar's row.
    #[derive(Debug, Default)]
    struct Tray {
        /// Each box in the chip fill, as its left, its width, and its edges.
        boxes: Vec<(u16, u16, [Edge; 4])>,
        /// Each bar with the dot's radius of 3.
        dots: Vec<BarSpec>,
        /// Each bar with the Progress Edge's radius of 1.
        edges: Vec<BarSpec>,
        /// Each run from the first box's column on.
        runs: Vec<Run>,
    }

    /// The Job Tray that a snapshot of `h` draws.
    fn tray(h: &mut TestHarness) -> Tray {
        h.snapshot();
        let mut tray = Tray::default();
        let mut runs = Vec::new();
        for command in command::decode_stream(&h.stoat.apc_scene.all_bytes()) {
            match command {
                Command::Panel(panel) if panel.top == 0 && panel.fill == Some(RAISED) => {
                    tray.boxes.push((panel.left, panel.width, panel.edges))
                },
                Command::Bar(bar) if bar.y < 32 => {
                    let spec = (bar.x, bar.y, bar.width, bar.height, bar.color);
                    match bar.radius {
                        3 => tray.dots.push(spec),
                        1 => tray.edges.push(spec),
                        _ => {},
                    }
                },
                Command::TextRun(run) if run.row < 16 => {
                    runs.push((run.text, run.col, run.color, run.bold))
                },
                Command::TextRuns(group) => runs.extend(
                    group
                        .runs
                        .into_iter()
                        .filter(|&(_, row, _)| row < 16)
                        .map(|(col, _, text)| (text, col, group.color, group.bold)),
                ),
                _ => {},
            }
        }
        let start = tray
            .boxes
            .first()
            .map_or(i16::MAX, |&(left, ..)| left as i16 * 16);
        tray.runs = runs.into_iter().filter(|run| run.1 >= start).collect();
        tray.runs.sort_by_key(|run| run.1);
        tray
    }

    /// The text, the color, and the weight of each run of `tray`.
    fn styled(tray: &Tray) -> Vec<(String, [u8; 3], bool)> {
        tray.runs
            .iter()
            .map(|(text, _, color, bold)| (text.clone(), *color, *bold))
            .collect()
    }

    /// A chip on 80 columns ends one pad cell before the bar's right edge, and
    /// it draws its box, its dot in the run color, its name, and its time.
    #[test]
    fn the_chip_of_a_running_job_draws_its_box_dot_name_and_meta() {
        let mut h = Stoat::test();
        h.push_job("sleep 30");
        h.advance_clock(Duration::from_secs(41));
        let tray = tray(&mut h);

        assert_eq!(
            (tray.boxes, tray.dots, tray.runs),
            (
                vec![(62, 17, ROW_EDGES)],
                vec![(1008, 5, 11, 6, SPECIAL)],
                vec![
                    ("sleep 30".to_owned(), 1031, DIM, false),
                    ("0:41".to_owned(), 1171, MUTED, false),
                ],
            ),
        );
    }

    /// In a band the chip's box is the pill of the active tab, outside the
    /// row at the top and the bottom, and it draws under the band's nudge.
    #[test]
    fn a_chip_in_a_band_takes_the_pill_of_the_tab() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        h.stoat.settings.ui_tab_bar = Some(TabBarMode::Always);
        h.push_job("sleep 30");
        h.snapshot();

        let mut nudge = [0, 0];
        let mut boxes = Vec::new();
        for command in command::decode_stream(h.stoat.apc_scene.lane_bytes(0)) {
            match command {
                Command::Nudge(offset) => nudge = offset,
                Command::Panel(panel) if panel.top == 0 && panel.fill == Some(RAISED) => {
                    boxes.push((panel.left, panel.width, panel.edges, nudge))
                },
                _ => {},
            }
        }
        let pill = [
            Edge::Outset(2),
            Edge::Inset(0),
            Edge::Outset(2),
            Edge::Inset(0),
        ];
        assert_eq!(boxes, [(142, 17, pill, [0, 7])]);
    }

    /// A running Job with a total draws the Progress Edge along the bottom of
    /// its box, as wide as the share of its tests that finished. A Job with no
    /// total and a Job that ended draw none.
    #[test]
    fn the_progress_edge_spans_the_done_share() {
        let mut h = Stoat::test();
        let job = h.push_job("cargo test -p stoat");
        let results = "test a ... ok\r\n".repeat(212);
        h.inject_cmd_output(
            job.cmd,
            format!("running 480 tests\r\n{results}").as_bytes(),
        );
        let running = tray(&mut h);
        h.inject_cmd_done(job.cmd, 0);
        let ended = tray(&mut h).edges;

        let mut plain = Stoat::test();
        plain.push_job("sleep 30");
        let no_total = tray(&mut plain).edges;

        let meta = running.runs.last().map(|run| run.0.clone());
        assert_eq!(
            (running.boxes, meta, running.edges, ended, no_total),
            (
                vec![(57, 22, ROW_EDGES)],
                Some("212/480".to_owned()),
                vec![(912, 13, 155, 2, SPECIAL)],
                Vec::new(),
                Vec::new(),
            ),
        );
    }

    /// An ended chip leads with the mark of its state in the state's color in
    /// place of the dot. A failed Job reads its name bold in the text color
    /// and its exit code in the failed color.
    #[test]
    fn an_ended_chip_draws_the_mark_of_its_state() {
        let ended = |exit_code: i32| {
            let mut h = Stoat::test();
            let job = h.push_job("sleep 30");
            h.inject_cmd_done(job.cmd, exit_code);
            tray(&mut h)
        };
        let ok = ended(0);
        let failed = ended(101);

        let mut h = Stoat::test();
        h.open_run();
        let stopped = h.submit_run("sleep 30");
        h.type_keys("ctrl-c");
        h.type_keys("ctrl-j");
        h.inject_cmd_done(stopped.cmd, 130);
        let stopped = tray(&mut h);

        let name = |color: [u8; 3], bold: bool| ("sleep 30".to_owned(), color, bold);
        assert_eq!(
            ((styled(&ok), ok.dots), styled(&failed), styled(&stopped)),
            (
                (
                    vec![
                        ("\u{2713}".to_owned(), SUCCESS, true),
                        name(DIM, false),
                        ("0ms".to_owned(), MUTED, false),
                    ],
                    Vec::new(),
                ),
                vec![
                    ("\u{2717}".to_owned(), DANGER, true),
                    name(TEXT, true),
                    ("exit 101".to_owned(), DANGER, false),
                ],
                vec![
                    ("\u{25a0}".to_owned(), MUTED, true),
                    name(DIM, false),
                    ("0ms".to_owned(), MUTED, false),
                ],
            ),
        );
    }

    /// The newest chip stands at the right, and an older chip keeps 3 px
    /// before it, its box standing in from its right edge.
    #[test]
    fn chips_stand_three_pixels_apart_newest_at_the_right() {
        let mut h = Stoat::test();
        h.push_job("sleep 30");
        h.push_job("sleep 31");
        let tray = tray(&mut h);

        let spaced = [
            Edge::Inset(1),
            Edge::Inset(3),
            Edge::Inset(1),
            Edge::Inset(0),
        ];
        let names: Vec<String> = tray
            .runs
            .iter()
            .map(|run| run.0.clone())
            .filter(|text| text.starts_with("sleep"))
            .collect();
        assert_eq!(
            (tray.boxes, names),
            (
                vec![(45, 17, spaced), (62, 17, ROW_EDGES)],
                vec!["sleep 30".to_owned(), "sleep 31".to_owned()],
            ),
        );
    }

    /// The tray takes the right half of the bar at most, so the oldest chip
    /// that would start left of the middle drops.
    #[test]
    fn the_oldest_chips_drop_when_the_tray_passes_half_the_bar() {
        let mut h = Stoat::test();
        for line in ["sleep 30", "sleep 31", "sleep 32"] {
            h.push_job(line);
        }
        let lefts: Vec<u16> = tray(&mut h).boxes.iter().map(|&(left, ..)| left).collect();

        assert_eq!(lefts, [45, 62]);
    }

    /// The tabs lay out left of the tray, so no tab reaches past the start of
    /// the first chip, while with no Job the tabs run on past it.
    #[test]
    fn the_tabs_end_at_the_start_of_the_tray() {
        let mut h = Stoat::test();
        for _ in 0..7 {
            h.type_action("NewTab()");
        }
        h.snapshot();
        let free = h.stoat.tab_bar_spans.clone();
        h.push_job("sleep 30");
        h.snapshot();

        let past = |spans: &[std::ops::Range<u16>]| spans.iter().any(|span| span.end > 992);
        assert_eq!((past(&free), past(&h.stoat.tab_bar_spans)), (true, false));
    }

    /// A top-right badge on the bar's row keeps the tray one blank cell left of
    /// its chip, which takes the columns 154 to 158.
    #[test]
    fn a_top_right_badge_pushes_the_tray_left() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.settings.ui_tab_bar = Some(TabBarMode::Always);
        h.stoat.badges.insert(Badge {
            source: BadgeSource::Agent,
            anchor: Anchor::TopRight,
            state: BadgeState::Complete,
            label: "top".to_owned(),
            detail: None,
        });
        h.push_job("sleep 30");
        let tray = tray(&mut h);

        let cover = badges::top_right_cover(
            &h.stoat.active_workspace().badges,
            &h.stoat.badges,
            Rect::new(0, 0, 160, 50),
        );
        assert_eq!(
            (cover, tray.boxes),
            (Some(Rect::new(154, 0, 5, 1)), vec![(136, 17, ROW_EDGES)]),
        );
    }

    /// At the Caption scale a chip rounds its sixteenths up to whole cells, so
    /// the space that a spaced chip keeps can take one more cell, and the tray
    /// lays the older chip out that much wider. In cells a spaced chip takes
    /// one more cell.
    #[test]
    fn a_chip_counts_its_cells_at_the_caption_scale_and_in_cells() {
        let view = || JobView {
            id: CmdId::default(),
            state: JobState::Running,
            name: "cargo test".to_owned(),
            progress: None,
            time: "0:41".to_owned(),
        };
        let views = [view(), view()];
        let cells = |scale, spaced| super::chip_cells(&views[0], scale, spaced);

        assert_eq!(
            (
                [
                    cells(Some(216), false),
                    cells(Some(216), true),
                    cells(None, false),
                    cells(None, true),
                ],
                super::tray_layout(&views, 159, 80, Some(216)),
            ),
            ([16, 17, 19, 20], vec![(0, 126..143), (1, 143..159)]),
        );
    }

    /// In a band the Progress Edge stands under the bar's row, the last 2 px
    /// of the pill. A spaced chip's edge spans its box less the space before
    /// the newer chip, and a run that counts more results than it announced
    /// fills its box and no more.
    #[test]
    fn the_progress_edge_of_a_band_and_a_spaced_chip() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        h.stoat.settings.ui_tab_bar = Some(TabBarMode::Always);
        let older = h.push_job("cargo test");
        let results = "test a ... ok\r\n".repeat(212);
        h.inject_cmd_output(
            older.cmd,
            format!("running 480 tests\r\n{results}").as_bytes(),
        );
        let newer = h.push_job("cargo test");
        h.inject_cmd_output(
            newer.cmd,
            b"running 2 tests\r\ntest a ... ok\r\ntest b ... ok\r\ntest c ... ok\r\n",
        );

        assert_eq!(
            tray(&mut h).edges,
            [
                (119 * 16, 16, 152, 2, SPECIAL),
                (141 * 16, 16, 18 * 16, 2, SPECIAL),
            ],
        );
    }

    /// The box takes the background of `ui.job.chip`, else of
    /// `ui.surface.raised`, else of `ui.chip`.
    #[test]
    fn the_box_takes_the_job_chip_fill_or_its_fallbacks() {
        let fill = |scopes: &str| {
            let (config, _) = stoat_config::parse(&format!("theme t {{ {scopes} }}"));
            let theme =
                Theme::from_config(&config.expect("theme parses"), "t").expect("theme loads");
            super::fill_bg(&theme)
        };
        let raised = r##"ui.surface.raised.bg = "#000002"; ui.chip.bg = "#000003";"##;

        assert_eq!(
            [
                fill(&format!(r##"ui.job.chip.bg = "#000001"; {raised}"##)),
                fill(raised),
                fill(r##"ui.chip.bg = "#000003";"##),
            ],
            [
                Some(Color::Rgb(0, 0, 1)),
                Some(Color::Rgb(0, 0, 2)),
                Some(Color::Rgb(0, 0, 3)),
            ],
        );
    }

    /// In cells an older chip keeps the bar's ground in its last cell, the
    /// space before the newer chip, and a failed Job reads its name in the
    /// text color and its exit code in the failed color.
    #[test]
    fn a_plain_terminal_spaces_the_chips_and_marks_a_failure() {
        let mut h = Stoat::test();
        h.stoat.stoatty = false;
        let failed = h.push_job("sleep 30");
        h.inject_cmd_done(failed.cmd, 101);
        h.push_job("sleep 31");
        h.snapshot();

        let buf = h.rendered_buffer();
        let text: String = (41..60).map(|x| buf[(x, 0)].symbol()).collect();
        let [r, g, b] = RAISED;
        let filled: Vec<bool> = (40..62)
            .map(|x| buf[(x, 0)].bg == Color::Rgb(r, g, b))
            .collect();
        let rgb = |[r, g, b]: [u8; 3]| Color::Rgb(r, g, b);
        assert_eq!(
            (text, filled, buf[(43, 0)].fg, buf[(52, 0)].fg),
            (
                "\u{2717} sleep 30 exit 101".to_owned(),
                [vec![true; 21], vec![false]].concat(),
                rgb(TEXT),
                rgb(DANGER),
            ),
        );
    }

    /// A terminal with no scene draws the chip in cells on the fill of its box,
    /// one pad cell from the bar's right edge.
    #[test]
    fn a_plain_terminal_draws_the_chip_in_cells() {
        let mut h = Stoat::test();
        h.stoat.stoatty = false;
        h.push_job("sleep 30");
        h.snapshot();

        let buf = h.rendered_buffer();
        let text: String = (63..78).map(|x| buf[(x, 0)].symbol()).collect();
        let fills: Vec<Color> = (62..79).map(|x| buf[(x, 0)].bg).collect();
        let [r, g, b] = RAISED;
        assert_eq!(
            (text, fills),
            (
                "\u{25cf} sleep 30 0:00".to_owned(),
                vec![Color::Rgb(r, g, b); 17]
            ),
        );
    }
}
