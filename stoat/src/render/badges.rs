use crate::{
    agent_status::AgentStatus,
    badge::{Anchor, Badge, BadgeSource, BadgeState, BadgeTray, StackDirection},
    render::text::{write_cell, write_str},
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};

pub(crate) fn render_badges(
    workspace: &BadgeTray,
    global: &BadgeTray,
    area: Rect,
    render_tick: u64,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
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

            render_single_badge(badge, draw_x, draw_y, render_tick, theme, buf);

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

fn render_single_badge(
    badge: &Badge,
    x: u16,
    y: u16,
    render_tick: u64,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    let (w, h) = badge_size(badge);
    let border_style = badge_border_style(badge.state, theme);

    let perimeter_len = 2 * (w as usize) + 2 * (h as usize) - 4;
    let spinner_pos = if badge.state == BadgeState::Active {
        Some(render_tick as usize % perimeter_len)
    } else {
        None
    };

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
}
