use crate::{
    agent_status::AgentStatus,
    badge::{Anchor, Badge, BadgeSource, BadgeState, BadgeTray, StackDirection},
    render::{
        review::style_rgb,
        text::{write_cell, write_str},
    },
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};
use stoatty_protocol::command::{BorderStyle, PanelShadow};
use stoatty_widgets::{panel::Panel, ApcScene};

pub(crate) fn render_badges(
    workspace: &BadgeTray,
    global: &BadgeTray,
    area: Rect,
    render_tick: u64,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    if workspace.is_empty() && global.is_empty() {
        return;
    }

    for anchor in Anchor::ALL {
        let tray = workspace.tray(anchor);
        let visible: Vec<&Badge> = workspace
            .at_anchor(anchor)
            .chain(global.at_anchor(anchor))
            .map(|(_, b)| b)
            .take(tray.max_visible as usize)
            .collect();
        if visible.is_empty() {
            continue;
        }

        let sizes: Vec<(u16, u16)> = visible.iter().map(|b| badge_size(b)).collect();
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
            let total_w: u16 =
                sizes.iter().map(|(w, _)| w).sum::<u16>() + sizes.len().saturating_sub(1) as u16;
            cx = origin_x.saturating_sub(total_w / 2);
        }

        for (i, badge) in visible.iter().enumerate() {
            let (bw, bh) = sizes[i];

            let draw_x = if grows_left {
                cx.saturating_sub(bw)
            } else if centered && tray.stack == StackDirection::Vertical {
                cx.saturating_sub(bw / 2)
            } else {
                cx
            };
            let draw_y = if grows_up {
                cy.saturating_sub(bh - 1)
            } else {
                cy
            };

            render_single_badge(badge, draw_x, draw_y, render_tick, theme, buf, scene);

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
    }
}

/// Reflect the live [`AgentStatus`] into `tray` under [`BadgeSource::Agent`],
/// replacing any agent badge left from a previous frame. Run each frame so the
/// overlay tracks the status the render process reads on paint. A cleanly
/// ended or absent session leaves no agent badge.
pub(crate) fn sync_agent_badge(tray: &mut BadgeTray, agent: Option<&AgentStatus>) {
    tray.remove_by_source(BadgeSource::Agent);
    if let Some(badge) = agent.and_then(AgentStatus::badge) {
        tray.insert(badge);
    }
}

#[allow(clippy::too_many_arguments)]
fn render_single_badge(
    badge: &Badge,
    x: u16,
    y: u16,
    render_tick: u64,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let (w, h) = badge_size(badge);
    let border_style = badge_border_style(badge.state, theme);

    match style_rgb(border_style.fg).filter(|_| scene.live()) {
        // A badge anchored to a screen edge overhangs the pane behind it, and
        // that pane's pool composite would erase the overhanging rows on every
        // glide frame. A panel over the rect punches it out of the composite.
        //
        // above_pools stays false because a pane pool occludes against every
        // panel regardless of the flag, which is all this needs, while leaving
        // it false is what keeps a modal's own pooled surfaces painting over
        // badges.
        //
        // The panel's seq occludes lower-seq main-pass runs and bars inside the
        // rect, so a rich status bar's scaled text no longer draws through the
        // badge's bottom row where the two overlap. That is accepted.
        Some(border) => {
            Panel {
                style: BorderStyle::Rounded,
                border,
                corner_radius: 6,
                fill: None,
                shadow: PanelShadow::None_,
                inset_x: 0,
                above_pools: false,
            }
            .draw_components(Rect::new(x, y, w, h), scene);
        },
        None => {
            for col in x..x + w {
                write_cell(buf, col, y, border_char_at(col - x, 0, w, h), border_style);
            }
            for col in x..x + w {
                write_cell(
                    buf,
                    col,
                    y + h - 1,
                    border_char_at(col - x, h - 1, w, h),
                    border_style,
                );
            }
            for row in y + 1..y + h - 1 {
                write_cell(buf, x, row, border_char_at(0, row - y, w, h), border_style);
                write_cell(
                    buf,
                    x + w - 1,
                    row,
                    border_char_at(w - 1, row - y, w, h),
                    border_style,
                );
            }
        },
    }

    let perimeter_len = 2 * (w as usize) + 2 * (h as usize) - 4;
    let spinner_pos = if badge.state == BadgeState::Active {
        Some(render_tick as usize % perimeter_len)
    } else {
        None
    };

    if let Some(pos) = spinner_pos {
        let (sc, sr) = perimeter_position(pos, w, h);
        let ch = spinner_char_at(sc, sr, w, h);
        write_cell(buf, x + sc, y + sr, ch, border_style);
    }

    let content_style = theme.get(crate::theme::scope::UI_TEXT);
    write_str(buf, x + 1, y + 1, &badge.label, content_style);
}

fn badge_size(badge: &Badge) -> (u16, u16) {
    let label_w = badge.label.chars().count() as u16;
    (label_w + 2, 3)
}

fn border_char_at(col: u16, row: u16, w: u16, h: u16) -> char {
    let top = row == 0;
    let bot = row == h - 1;
    let left = col == 0;
    let right = col == w - 1;
    match (top, bot, left, right) {
        (true, _, true, _) => '\u{256d}',
        (true, _, _, true) => '\u{256e}',
        (_, true, true, _) => '\u{2570}',
        (_, true, _, true) => '\u{256f}',
        (true, _, _, _) | (_, true, _, _) => '\u{2500}',
        _ => '\u{2502}',
    }
}

/// Braille character that visually traces the box-drawing line at this
/// border position. Dot placement matches the line direction:
///
/// ```text
///   braille grid        used for
///   1 4                 ╭ → ⣰  (bottom-right quadrant: right then down)
///   2 5                 ╮ → ⣆  (bottom-left quadrant: left then down)
///   3 6                 ╰ → ⠙  (top-right quadrant: right then up)
///   7 8                 ╯ → ⠋  (top-left quadrant: left then up)
///                       ─ top  → ⠉  (dots 1,4)
///                       ─ bot  → ⣀  (dots 7,8)
///                       │ left → ⡇  (dots 1,2,3,7)
///                       │ right→ ⢸  (dots 4,5,6,8)
/// ```
fn spinner_char_at(col: u16, row: u16, w: u16, h: u16) -> char {
    let top = row == 0;
    let bot = row == h - 1;
    let left = col == 0;
    let right = col == w - 1;
    match (top, bot, left, right) {
        (true, _, true, _) => '\u{28f0}',
        (true, _, _, true) => '\u{28c6}',
        (_, true, true, _) => '\u{2819}',
        (_, true, _, true) => '\u{280b}',
        (true, _, _, _) => '\u{2809}',
        (_, true, _, _) => '\u{28c0}',
        (_, _, true, _) => '\u{2847}',
        _ => '\u{28b8}',
    }
}

fn perimeter_position(index: usize, w: u16, h: u16) -> (u16, u16) {
    let w = w as usize;
    let h = h as usize;
    let top = w;
    let right = top + h.saturating_sub(2);
    let bottom = right + w;
    if index < top {
        (index as u16, 0)
    } else if index < right {
        ((w - 1) as u16, (index - top + 1) as u16)
    } else if index < bottom {
        ((w - 1 - (index - right)) as u16, (h - 1) as u16)
    } else {
        (0, (h - 1 - (index - bottom + 1)) as u16)
    }
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
    use stoatty_protocol::command::{encode_panel, PanelCommand};

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
        assert_eq!(tray.get(id).unwrap().label, "claude: Bash");
        assert_eq!(tray.get(id).unwrap().state, BadgeState::Active);

        status.apply(AgentHookEvent::Notification);
        sync_agent_badge(&mut tray, Some(&status));
        let replaced = tray
            .find_by_source(BadgeSource::Agent)
            .expect("agent badge still present");
        assert_eq!(tray.get(replaced).unwrap().label, "claude: awaiting input");

        status.apply(AgentHookEvent::SessionEnd);
        sync_agent_badge(&mut tray, Some(&status));
        assert!(tray.find_by_source(BadgeSource::Agent).is_none());
    }

    /// A theme whose badge colors resolve to RGB, which is what selects the rich
    /// arm once the scene is live.
    fn rgb_badge_theme() -> crate::theme::Theme {
        let src = r##"theme rgbbadge {
            ui.badge.active.fg = "#010203";
            ui.badge.complete.fg = "#010203";
            ui.text.fg = "#c8ccd4";
        }"##;
        let (config, _) = stoat_config::parse(src);
        crate::theme::Theme::from_config(&config.expect("theme config parses"), "rgbbadge")
            .expect("rgb theme builds")
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
    /// surface. `live` picks the arm the way a stoatty host would.
    fn paint(state: BadgeState, render_tick: u64, live: bool) -> (Buffer, Vec<u8>) {
        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 5));
        let mut scene = ApcScene::new();
        scene.set_live(live);

        render_single_badge(
            &badge(state),
            0,
            0,
            render_tick,
            &rgb_badge_theme(),
            &mut buf,
            &mut scene,
        );

        let bytes = scene.bytes().to_vec();
        (buf, bytes)
    }

    fn box_glyphs(buf: &Buffer) -> Vec<String> {
        buf.content()
            .iter()
            .map(|cell| cell.symbol().to_owned())
            .filter(|s| "╭╮╰╯─│".contains(s.as_str()))
            .collect()
    }

    /// The badge's own w-by-h panel is what a pane pool occludes against, which
    /// is what keeps the badge painted through a glide.
    #[test]
    fn rich_arm_emits_a_panel_and_no_border_glyphs() {
        let (buf, bytes) = paint(BadgeState::Complete, 0, true);

        assert_eq!(
            bytes,
            encode_panel(&PanelCommand {
                top: 0,
                left: 0,
                width: 4,
                height: 3,
                style: BorderStyle::Rounded,
                border: [1, 2, 3],
                corner_radius: 6,
                fill: None,
                shadow: PanelShadow::None_,
                inset_x: 0,
                above_pools: false,
            }),
            "one panel over the badge rect, unflagged"
        );
        assert_eq!(
            box_glyphs(&buf),
            Vec::<String>::new(),
            "the panel is the border, so no glyph border is drawn"
        );
        assert_eq!(
            buf.cell((1, 1)).unwrap().symbol(),
            "a",
            "label still paints"
        );
    }

    /// A foreign terminal renders no panel, so the badge keeps the glyph border
    /// it has always drawn there.
    #[test]
    fn fallback_arm_paints_the_glyph_border_and_no_panel() {
        let (buf, bytes) = paint(BadgeState::Complete, 0, false);

        assert_eq!(bytes, Vec::<u8>::new(), "a dead scene emits nothing");
        assert_eq!(
            box_glyphs(&buf),
            ["╭", "─", "─", "╮", "│", "│", "╰", "─", "─", "╯"],
            "the full rounded perimeter"
        );
        assert_eq!(
            buf.cell((1, 1)).unwrap().symbol(),
            "a",
            "label still paints"
        );
    }

    /// The spinner is content rather than border, so it survives the arm that
    /// drops the border glyphs.
    #[test]
    fn both_arms_paint_the_active_spinner() {
        let (rich, _) = paint(BadgeState::Active, 0, true);
        let (fallback, _) = paint(BadgeState::Active, 0, false);

        assert_eq!(
            rich.cell((0, 0)).unwrap().symbol(),
            "⣰",
            "the spinner draws over the panel"
        );
        assert_eq!(
            fallback.cell((0, 0)).unwrap().symbol(),
            "⣰",
            "and over the glyph corner it replaces"
        );
    }
}
