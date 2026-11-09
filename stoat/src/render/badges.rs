use crate::{
    agent_status::AgentStatus,
    badge::{Anchor, Badge, BadgeSource, BadgeState, BadgeTray, StackDirection},
    code_index::nav::TrailState,
    render::{
        paint::{blend, style_rgb},
        text::{write_cell, write_str},
    },
    theme::TypeRole,
    walkthrough::run::WalkthroughRun,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::StatefulWidget,
};
use std::borrow::Cow;
use stoat_widgets::{panel::Panel, text_run::TextRun, ApcScene};
use stoatty_protocol::command::{BorderStyle, Edge, PanelShadow};

/// The mark an active badge's text leads with.
const ACTIVE_MARK: &str = "\u{25cf} ";

/// The percent of a badge's semantic color in its chip's fill.
///
/// The rest is the chrome surface, so a chip reads as a tint of the bar it sits
/// on rather than as a block of the semantic color.
const TINT_PERCENT: u16 = 12;

/// Paint each visible badge of the workspace and global trays over `area`.
///
/// Returns whether it painted a badge.
pub(crate) fn render_badges(
    workspace: &BadgeTray,
    global: &BadgeTray,
    area: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) -> bool {
    if workspace.is_empty() && global.is_empty() {
        return false;
    }

    let mut painted = false;
    for anchor in Anchor::ALL {
        for (badge, rect) in placements(workspace, global, anchor, area) {
            render_single_badge(badge, rect.x, rect.y, theme, buf, scene);
            painted = true;
        }
    }
    painted
}

/// The columns the bottom-right badges cover on `area`'s bottom row, as a
/// one-row rect.
///
/// A bottom-right badge draws its chip on the window's last row, which is where
/// the bottom panes draw their status bars. A status bar keeps its segments
/// left of this span, since the chip paints over whatever the bar put there.
/// `None` when no badge reaches the row.
pub(crate) fn bottom_right_cover(
    workspace: &BadgeTray,
    global: &BadgeTray,
    area: Rect,
) -> Option<Rect> {
    let row = area.y + area.height.checked_sub(1)?;
    placements(workspace, global, Anchor::BottomRight, area)
        .into_iter()
        .map(|(_, rect)| rect)
        .filter(|rect| rect.y <= row && row < rect.y + rect.height)
        .reduce(Rect::union)
        .map(|rect| Rect {
            y: row,
            height: 1,
            ..rect
        })
}

/// Where each visible badge at `anchor` draws inside `area`, in paint order.
///
/// [`render_badges`] paints from this and [`bottom_right_cover`] measures from
/// it, so the columns a status bar keeps clear are the ones the badges fill.
fn placements<'t>(
    workspace: &'t BadgeTray,
    global: &'t BadgeTray,
    anchor: Anchor,
    area: Rect,
) -> Vec<(&'t Badge, Rect)> {
    let tray = workspace.tray(anchor);
    let mut placed: Vec<(&Badge, Rect)> = workspace
        .at_anchor(anchor)
        .chain(global.at_anchor(anchor))
        .take(tray.max_visible as usize)
        .map(|(_, badge)| {
            let (width, height) = badge_size(badge);
            (badge, Rect::new(0, 0, width, height))
        })
        .collect();

    let (origin_x, origin_y) = anchor_origin(anchor, area);
    let grows_left = matches!(
        anchor,
        Anchor::TopRight | Anchor::MidRight | Anchor::BottomRight
    );
    let grows_up = matches!(
        anchor,
        Anchor::BottomLeft | Anchor::BottomCenter | Anchor::BottomRight
    );
    let centered = matches!(anchor, Anchor::TopCenter | Anchor::BottomCenter);

    let (mut cx, mut cy) = (origin_x, origin_y);

    if centered && tray.stack == StackDirection::Horizontal {
        let total_w: u16 = placed.iter().map(|(_, rect)| rect.width).sum::<u16>()
            + placed.len().saturating_sub(1) as u16;
        cx = origin_x.saturating_sub(total_w / 2);
    }

    for (_, rect) in &mut placed {
        let (bw, bh) = (rect.width, rect.height);

        rect.x = if grows_left {
            cx.saturating_sub(bw)
        } else if centered && tray.stack == StackDirection::Vertical {
            cx.saturating_sub(bw / 2)
        } else {
            cx
        };
        rect.y = if grows_up {
            cy.saturating_sub(bh - 1)
        } else {
            cy
        };

        match tray.stack {
            StackDirection::Horizontal => {
                if grows_left {
                    cx = cx.saturating_sub(bw + 1);
                } else {
                    cx += bw + 1;
                }
            },
            StackDirection::Vertical => {
                if grows_up {
                    cy = cy.saturating_sub(bh);
                } else {
                    cy += bh;
                }
            },
        }
    }

    placed
}

/// Reflect the live [`AgentStatus`] into `tray` under [`BadgeSource::Agent`],
/// replacing any agent badge left from a previous frame. Run each frame so the
/// overlay tracks the status the render process reads on paint. A cleanly
/// ended or absent session leaves no agent badge.
///
/// A badge equal to the one in the tray stays in place. An exited session's
/// error badge stands for many frames, and [`BadgeTray::insert`] logs each
/// error badge it takes.
pub(crate) fn sync_agent_badge(tray: &mut BadgeTray, agent: Option<&AgentStatus>) {
    let badge = agent.and_then(AgentStatus::badge);
    let shown = tray
        .find_by_source(BadgeSource::Agent)
        .and_then(|id| tray.get(id));
    if shown == badge.as_ref() {
        return;
    }

    tray.remove_by_source(BadgeSource::Agent);
    if let Some(badge) = badge {
        tray.insert(badge);
    }
}

/// Reflect the marked trail into `tray` under [`BadgeSource::Trail`], replacing
/// any trail badge left from a previous frame.
///
/// Mirrors [`sync_agent_badge`], and re-derives per frame for the same reason:
/// the label carries the position along the trail, which every step moves. No
/// trail leaves no badge, which is what a `TrailClear` shows up as.
pub(crate) fn sync_trail_badge(tray: &mut BadgeTray, trail: Option<&TrailState>) {
    tray.remove_by_source(BadgeSource::Trail);
    if let Some(badge) = trail.map(TrailState::badge) {
        tray.insert(badge);
    }
}

/// Reflect the walkthrough being played into `tray` under
/// [`BadgeSource::Walkthrough`], replacing any left from a previous frame.
///
/// Mirrors [`sync_trail_badge`], and re-derives per frame for the same reason:
/// the label carries the stop the reader is on, which every step moves. No run
/// leaves no badge, which is what `WalkthroughDone` shows up as.
pub(crate) fn sync_walkthrough_badge(tray: &mut BadgeTray, run: Option<&WalkthroughRun>) {
    tray.remove_by_source(BadgeSource::Walkthrough);
    if let Some(badge) = run.map(WalkthroughRun::badge) {
        tray.insert(badge);
    }
}

/// Reflect a held chord into `tray` under [`BadgeSource::PinnedMode`],
/// replacing any left from a previous frame.
///
/// Mirrors [`sync_trail_badge`], but the label is the constant `pinned`, since
/// the mode segment already names the chord. The per-frame re-derive is what
/// drops the badge once a mode switch releases the pin.
pub(crate) fn sync_pinned_badge(tray: &mut BadgeTray, pinned: bool) {
    tray.remove_by_source(BadgeSource::PinnedMode);
    if pinned {
        tray.insert(Badge {
            source: BadgeSource::PinnedMode,
            anchor: Anchor::BottomRight,
            state: BadgeState::Active,
            label: "pinned".into(),
            detail: None,
        });
    }
}

/// Paint `badge` as a one-row chip from the cell at `x` and `y`.
///
/// The chip fills with its tint and its text reads in the badge's semantic
/// color. Both arms blank the chip's cells in the tint first. A cell glyph
/// draws above a panel's fill, so a glyph left under the chip shows through it.
fn render_single_badge(
    badge: &Badge,
    x: u16,
    y: u16,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let (w, h) = badge_size(badge);
    let semantic = badge_border_style(badge.state, theme);
    let tint = chip_tint(semantic, theme);
    let text = chip_text(badge);

    let style = match tint {
        Some([r, g, b]) => semantic.bg(Color::Rgb(r, g, b)),
        None => semantic,
    };
    for col in x..x + w {
        write_cell(buf, col, y, ' ', style);
    }

    match style_rgb(semantic.fg).filter(|_| scene.live()) {
        // A badge anchored to a screen edge overhangs the pane behind it, and
        // that pane's pool composite erases the overhanging cells on every
        // glide frame. A panel over the rect punches it out of the composite.
        //
        // above_pools stays false because a pane pool occludes against every
        // panel regardless of the flag, which is all this needs, while leaving
        // it false is what keeps a modal's own pooled surfaces painting over
        // badges.
        //
        // The panel's seq occludes lower-seq main-pass runs and bars inside the
        // rect. A status bar keeps its segments out of a bottom-right chip's
        // row (see bottom_right_cover), so only its hairline runs under it.
        Some(color) => {
            Panel {
                style: BorderStyle::None_,
                border: color,
                corner_radius: 1,
                fill: tint,
                shadow: PanelShadow::None_,
                edges: Edge::symmetric(0, 0),
                corners: 0,
                fillets: 0,
                fillet_radius: 2,
                hatch: None,
                above_pools: false,
                backdrop: false,
                anchor: None,
            }
            .draw_components(Rect::new(x, y, w, h), scene);
            TextRun {
                col: 0,
                row: 0,
                scale: theme.type_scale(TypeRole::Ui),
                color,
                bg: None,
                text: &text,
                bold: false,
                follow: 0,
                anchor: None,
            }
            .render(Rect::new(x + 1, y, 1, 1), buf, scene);
        },
        None => write_str(buf, x + 1, y, &text, style),
    }
}

/// The cells a badge's chip takes: its text with one cell of padding at each
/// end, on one row.
fn badge_size(badge: &Badge) -> (u16, u16) {
    (chip_text(badge).chars().count() as u16 + 2, 1)
}

/// The text a badge's chip shows, which leads with [`ACTIVE_MARK`] while the
/// badge is active.
fn chip_text(badge: &Badge) -> Cow<'_, str> {
    match badge.state {
        BadgeState::Active => Cow::Owned(format!("{ACTIVE_MARK}{}", badge.label)),
        BadgeState::Complete | BadgeState::Error => Cow::Borrowed(&badge.label),
    }
}

/// A chip's fill, which is the chrome surface with [`TINT_PERCENT`] of the
/// badge's semantic color mixed in.
///
/// The surface is `ui.surface.chrome`, or `ui.background` where a theme sets no
/// chrome. `None` when either color has no RGB value, and the chip then keeps
/// the surface under it.
fn chip_tint(semantic: Style, theme: &crate::theme::Theme) -> Option<[u8; 3]> {
    let surface = theme
        .try_get(crate::theme::scope::UI_SURFACE_CHROME)
        .and_then(|style| style.bg)
        .or_else(|| {
            theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|style| style.bg)
        });
    Some(blend(
        style_rgb(surface)?,
        style_rgb(semantic.fg)?,
        TINT_PERCENT,
    ))
}

fn anchor_origin(anchor: Anchor, area: Rect) -> (u16, u16) {
    let x = match anchor {
        Anchor::TopLeft | Anchor::MidLeft | Anchor::BottomLeft => area.x,
        Anchor::TopCenter | Anchor::BottomCenter => area.x + area.width / 2,
        Anchor::TopRight | Anchor::MidRight | Anchor::BottomRight => {
            (area.x + area.width).saturating_sub(1)
        },
    };
    let y = match anchor {
        Anchor::TopLeft | Anchor::TopCenter | Anchor::TopRight => area.y,
        Anchor::MidLeft | Anchor::MidRight => area.y + area.height / 2,
        Anchor::BottomLeft | Anchor::BottomCenter | Anchor::BottomRight => {
            area.y + area.height.saturating_sub(1)
        },
    };
    (x, y)
}

fn badge_border_style(state: BadgeState, theme: &crate::theme::Theme) -> Style {
    use crate::theme::scope;
    match state {
        BadgeState::Active => theme.get(scope::UI_BADGE_ACTIVE),
        BadgeState::Complete => theme.get(scope::UI_BADGE_COMPLETE),
        BadgeState::Error => theme.get(scope::UI_BADGE_ERROR),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{agent_status::AgentHookEvent, Stoat};
    use stoatty_protocol::command::{self, Command, PanelCommand, TextRunCommand};

    #[test]
    fn snapshot_agent_badge_active() {
        let mut h = Stoat::test();
        let mut status = AgentStatus::new();
        status.apply(AgentHookEvent::PreToolUse {
            tool: "Bash".into(),
        });
        h.stoat.active_workspace_mut().agent = Some(status);
        h.assert_snapshot("agent_badge_active");
    }

    /// With the tab bar up, the agent's top-right chip sits in the bar's row
    /// rather than on the first pane row under it.
    #[test]
    fn the_agent_chip_sits_in_the_tab_bar_row() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 24);
        h.type_action("NewTab()");
        let mut status = AgentStatus::new();
        status.apply(AgentHookEvent::PreToolUse {
            tool: "Bash".into(),
        });
        h.stoat.active_workspace_mut().agent = Some(status);

        let buf = h.render_composited();
        let chip_rows: Vec<u16> = (0..buf.area.height)
            .filter(|&y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .contains("claude")
            })
            .collect();
        assert_eq!((h.stoat.tab_bar_visible(), chip_rows), (true, vec![0]));
    }

    #[test]
    fn sync_replaces_then_clears_agent_badge() {
        let mut tray = BadgeTray::new();
        let mut status = AgentStatus::new();
        status.apply(AgentHookEvent::PreToolUse {
            tool: "Bash".into(),
        });

        sync_agent_badge(&mut tray, Some(&status));
        let id = tray
            .find_by_source(BadgeSource::Agent)
            .expect("agent badge present");
        assert_eq!(tray.get(id).unwrap().label, "claude \u{b7} Bash");
        assert_eq!(tray.get(id).unwrap().state, BadgeState::Active);

        status.apply(AgentHookEvent::Notification);
        sync_agent_badge(&mut tray, Some(&status));
        let replaced = tray
            .find_by_source(BadgeSource::Agent)
            .expect("agent badge still present");
        assert_eq!(
            tray.get(replaced).unwrap().label,
            "claude \u{b7} awaiting input"
        );

        status.apply(AgentHookEvent::SessionEnd);
        sync_agent_badge(&mut tray, Some(&status));
        assert!(tray.find_by_source(BadgeSource::Agent).is_none());
    }

    #[test]
    fn sync_keeps_an_unchanged_agent_badge() {
        let mut tray = BadgeTray::new();
        let mut status = AgentStatus::new();
        status.mark_exited();

        sync_agent_badge(&mut tray, Some(&status));
        let shown = tray
            .find_by_source(BadgeSource::Agent)
            .expect("exited agent badge");
        sync_agent_badge(&mut tray, Some(&status));

        assert_eq!(
            tray.find_by_source(BadgeSource::Agent),
            Some(shown),
            "agent badge id after a second sync"
        );
    }

    #[test]
    fn sync_pinned_badge_inserts_then_clears() {
        let mut tray = BadgeTray::new();
        let corner = |tray: &BadgeTray| {
            tray.at_anchor(Anchor::BottomRight)
                .map(|(_, badge)| (badge.source, badge.label.clone(), badge.state))
                .collect::<Vec<_>>()
        };

        sync_pinned_badge(&mut tray, true);
        sync_pinned_badge(&mut tray, true);
        let held = corner(&tray);
        sync_pinned_badge(&mut tray, false);

        assert_eq!(
            (held, corner(&tray)),
            (
                vec![(
                    BadgeSource::PinnedMode,
                    "pinned".to_string(),
                    BadgeState::Active
                )],
                vec![],
            ),
            "one badge per held frame, and none once released",
        );
    }

    #[test]
    fn pinned_chord_paints_the_badge_and_escape_clears_it() {
        let mut h = crate::test_harness::TestHarness::with_size(160, 60);
        h.type_keys("space g g");
        let held = shows_pinned(&mut h);
        h.type_keys("escape");
        assert_eq!(
            (held, shows_pinned(&mut h)),
            (true, false),
            "the badge shows while the chord holds"
        );
    }

    /// A config-named `*_pin` mode holds with no `PinMode` press, and the
    /// badge marks it all the same.
    #[test]
    fn a_pin_named_mode_paints_the_badge() {
        let mut h = crate::test_harness::TestHarness::with_size(160, 60);
        h.stoat.set_focused_mode("goto_pin".into());
        assert!(shows_pinned(&mut h), "the mode's name marks the hold");
    }

    /// Whether the composited frame holds the word `pinned` on any row.
    fn shows_pinned(h: &mut crate::test_harness::TestHarness) -> bool {
        let buf = h.render_composited();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
            .contains("pinned")
    }

    /// A theme whose badge colors resolve to RGB, which is what selects the rich
    /// arm once the scene is live. The badge color is (200, 100, 50) over a gray
    /// (100, 100, 100) chrome surface, so the tint works out by hand.
    fn rgb_badge_theme() -> crate::theme::Theme {
        let src = r##"theme rgbbadge {
            ui.badge.active.fg = "#c86432";
            ui.badge.complete.fg = "#c86432";
            ui.surface.chrome.bg = "#646464";
            ui.text.fg = "#c8ccd4";
        }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbbadge")
            .expect("rgb theme builds")
    }

    /// The badge color of [`rgb_badge_theme`].
    const BADGE_RGB: [u8; 3] = [200, 100, 50];

    /// The chip fill of [`rgb_badge_theme`], 88 percent of its chrome and 12
    /// percent of its badge color.
    const TINT: [u8; 3] = [112, 100, 94];

    /// The buffer a chip at the origin of [`paint`]'s buffer leaves, with `row`
    /// in the badge color over the tint.
    fn chip_buffer(row: &str) -> Buffer {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 5));
        let [r, g, b] = BADGE_RGB;
        let [tr, tg, tb] = TINT;
        let style = Style::default()
            .fg(Color::Rgb(r, g, b))
            .bg(Color::Rgb(tr, tg, tb));
        buf.set_string(0, 0, row, style);
        buf
    }

    fn badge(state: BadgeState) -> Badge {
        Badge {
            source: BadgeSource::Agent,
            anchor: Anchor::TopLeft,
            state,
            label: "ab".to_owned(),
            detail: None,
        }
    }

    /// Paint one badge at the buffer origin and hand back what it wrote to each
    /// surface. `live` picks the arm, as a stoatty host does.
    fn paint(state: BadgeState, live: bool) -> (Buffer, Vec<u8>) {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 5));
        let mut scene = ApcScene::new();
        scene.set_live(live);

        render_single_badge(
            &badge(state),
            0,
            0,
            &rgb_badge_theme(),
            &mut buf,
            &mut scene,
        );

        let bytes = scene.bytes().to_vec();
        (buf, bytes)
    }

    /// The rich arm declares one borderless panel filled with the tint, which a
    /// pane pool occludes against so the chip stays painted through a glide.
    /// Over it goes one run of the text in the badge color, and the cells under
    /// it go blank in the tint.
    #[test]
    fn rich_arm_emits_a_tinted_panel_and_the_text_run() {
        let (buf, bytes) = paint(BadgeState::Complete, true);

        assert_eq!(
            (command::decode_stream(&bytes), buf),
            (
                vec![
                    Command::Panel(PanelCommand {
                        top: 0,
                        left: 0,
                        width: 4,
                        height: 1,
                        style: BorderStyle::None_,
                        border: BADGE_RGB,
                        corner_radius: 1,
                        fill: Some(TINT),
                        shadow: PanelShadow::None_,
                        edges: Edge::symmetric(0, 0),
                        corners: 0,
                        fillets: 0,
                        fillet_radius: 2,
                        hatch: None,
                        above_pools: false,
                        backdrop: false,
                        anchor: None,
                    }),
                    Command::TextRun(TextRunCommand {
                        col: 16,
                        row: 0,
                        scale: 256,
                        color: BADGE_RGB,
                        bg: None,
                        text: "ab".to_string(),
                        bold: false,
                        follow: 0,
                        anchor: None,
                    }),
                    Command::TextRunEnd,
                ],
                chip_buffer("    "),
            ),
        );
    }

    /// A foreign terminal renders no panel, so the chip paints its text in the
    /// badge color over the tint in its one row of cells.
    #[test]
    fn cell_arm_paints_the_text_over_the_tint() {
        let (buf, bytes) = paint(BadgeState::Complete, false);

        assert_eq!((bytes, buf), (Vec::new(), chip_buffer(" ab ")));
    }

    /// The chip tints the chrome surface, or the background where a theme sets
    /// no chrome. A theme with neither leaves the chip untinted.
    #[test]
    fn the_tint_falls_back_from_chrome_to_the_background() {
        let tint = |settings: &str| {
            let src = format!("theme t {{ {settings} }}");
            let (config, _) = stoat_config::parse(&src);
            let theme = crate::theme::Theme::from_config(&config.expect("theme parses"), "t")
                .expect("theme builds");
            let [r, g, b] = BADGE_RGB;
            chip_tint(Style::default().fg(Color::Rgb(r, g, b)), &theme)
        };

        assert_eq!(
            [
                tint(r##"ui.surface.chrome.bg = "#646464"; ui.background.bg = "#000000";"##),
                tint(r##"ui.background.bg = "#646464";"##),
                tint(""),
            ],
            [Some(TINT), Some(TINT), None],
        );
    }

    /// An active badge's text leads with a dot in both arms, and the chip widens
    /// to hold it.
    #[test]
    fn an_active_badge_leads_with_a_dot() {
        let (_, rich) = paint(BadgeState::Active, true);
        let (cells, _) = paint(BadgeState::Active, false);
        let run_text = command::decode_stream(&rich)
            .into_iter()
            .find_map(|cmd| match cmd {
                Command::TextRun(run) => Some(run.text),
                _ => None,
            });

        assert_eq!(
            (run_text, cells),
            (
                Some("\u{25cf} ab".to_string()),
                chip_buffer(" \u{25cf} ab ")
            ),
        );
    }

    fn corner_badge(label: &str) -> Badge {
        Badge {
            source: BadgeSource::Walkthrough,
            anchor: Anchor::BottomRight,
            state: BadgeState::Complete,
            label: label.to_owned(),
            detail: None,
        }
    }

    /// A status bar clear of the cover has to be clear of the chip itself, so
    /// the cover is checked against the cells the chip tints.
    #[test]
    fn the_cover_is_the_row_a_corner_badge_paints() {
        let mut tray = BadgeTray::new();
        tray.insert(corner_badge("tour 1/6"));
        let area = Rect::new(0, 0, 80, 24);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_live(false);

        render_badges(
            &tray,
            &BadgeTray::new(),
            area,
            &rgb_badge_theme(),
            &mut buf,
            &mut scene,
        );

        let [r, g, b] = TINT;
        let tinted: Vec<u16> = (0..80)
            .filter(|&x| buf[(x, 23)].bg == Color::Rgb(r, g, b))
            .collect();
        assert_eq!(
            bottom_right_cover(&tray, &BadgeTray::new(), area),
            Some(Rect::new(69, 23, 10, 1)),
            "a 10-wide chip one column in from the right edge"
        );
        assert_eq!(tinted, (69..79).collect::<Vec<_>>());
    }

    /// The tray stacks bottom-right badges upward, so only the lowest one's box
    /// reaches the bottom row.
    #[test]
    fn only_the_lowest_stacked_badge_covers_the_bottom_row() {
        let mut tray = BadgeTray::new();
        tray.insert(corner_badge("ab"));
        tray.insert(corner_badge("a longer label"));

        let cover = bottom_right_cover(&tray, &BadgeTray::new(), Rect::new(0, 0, 80, 24));

        assert_eq!(cover, Some(Rect::new(75, 23, 4, 1)));
    }

    #[test]
    fn no_bottom_right_badge_covers_nothing() {
        let mut tray = BadgeTray::new();
        let area = Rect::new(0, 0, 80, 24);
        assert_eq!(
            bottom_right_cover(&tray, &BadgeTray::new(), area),
            None,
            "an empty tray"
        );

        tray.insert(Badge {
            anchor: Anchor::TopRight,
            ..corner_badge("ab")
        });
        assert_eq!(
            bottom_right_cover(&tray, &BadgeTray::new(), area),
            None,
            "a badge at another anchor"
        );
    }
}
