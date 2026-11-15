//! The Blame Card, which names the commit of the cursor's line in a blamed
//! pane under that line.
//!
//! The BlameView board draws the card as a box 520 px wide on the raised
//! surface, 13 px left of the code and 4 px under the cursor's row. Its text
//! starts 10 px inside the box. The rows hold the short sha and the subject, a
//! meta line, the first paragraph of the message, and the keys that act on the
//! commit.

use crate::{
    app::Stoat,
    blame::{self, BlameCard},
    commit_picker, keymap_state,
    render::{paint::style_rgb, popout, screen},
    theme::{scope, PanelRole, Theme, TypeRole},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::StatefulWidget,
};
use stoat_text::Point;
use stoat_widgets::{
    panel::Panel,
    text_run::{self, TextRun},
    ApcScene,
};
use stoatty_protocol::{
    command::{BorderStyle, Edge},
    NUDGE_PROTOCOL,
};

/// The most cells the card spans, from two columns left of the text.
///
/// The board's 520 px box is 58 columns at the scene's 9 px cell, less the
/// 5 px the box stands inside its first column, plus the 3 px it stands past
/// its last.
const CARD_COLS: u16 = 58;

/// Where each side of the box stands against its rows and columns, as the
/// board places it at the scene size.
///
/// The box stands 6 px over the first text row, 3 px past the last column,
/// 6 px under the last text row, and 5 px inside the first column.
const CARD_EDGES: [Edge; 4] = [
    Edge::Outset(6),
    Edge::Outset(3),
    Edge::Outset(6),
    Edge::Inset(5),
];

/// The logical px the card's text starts right of its first column, the
/// box's 5 px inset plus the board's 10 px padding.
const TEXT_LEFT: i16 = 15;

/// The sixteenths of the card's columns that its padding keeps free of text,
/// the board's 15 px left of the text and 7 px right of it at the scene's
/// 9 px cell.
const ROOM_TRIM: u16 = 39;

/// The logical px the card draws below its rows when it opens under the line,
/// which puts the box's top 4 px under the line.
const NUDGE_BELOW: i16 = 10;

/// The logical px the card draws below its rows when it opens over the line,
/// which puts the box's bottom 4 px over the line.
const NUDGE_ABOVE: i16 = 8;

/// One row of the card, as its parts in order and the role whose scale its
/// text draws at.
struct CardRow {
    parts: Vec<(String, Style)>,
    role: TypeRole,
}

/// Where the card goes, against the line it names.
struct Spot {
    /// The editor's text area, which bounds the card's rows.
    text: Rect,
    /// The first and the last screen row of the card's line.
    line: (u16, u16),
    /// The card's first column.
    x: u16,
    /// The card's width in columns.
    width: u16,
}

/// Paint the Blame Card under the cursor's line, or over it when the pane has
/// no room under it, and answer whether it painted.
///
/// The card paints nothing while a modal is open or when its line is off
/// screen. Under the nudge protocol with a live scene it draws as a panel and
/// text runs nudged off the cell grid, over cells whose glyphs it clears.
/// Otherwise its rows are plain cells inside a popout card.
pub(crate) fn render_blame_card(stoat: &mut Stoat, buf: &mut Buffer, scene: &mut ApcScene) -> bool {
    if keymap_state::active_modal(stoat).is_some() {
        return false;
    }
    let now_epoch = commit_picker::now_epoch(&stoat.executor);
    let nudged = scene.live() && stoat.stoatty_protocol >= NUDGE_PROTOCOL;
    let Stoat {
        blame_card,
        workspaces,
        active_workspace,
        theme,
        ..
    } = stoat;
    let Some(card) = blame_card.as_ref() else {
        return false;
    };
    let Some(editor) = workspaces[*active_workspace].editors.get_mut(card.editor) else {
        return false;
    };
    let Some(text) = editor.text_rect.filter(|area| !area.is_empty()) else {
        return false;
    };
    let range = {
        let snapshot = editor.display_map.buffer_snapshot();
        let rope = snapshot.rope();
        let start = rope.point_to_offset(Point::new(card.row, 0));
        start..=rope.point_to_offset(Point::new(card.row, rope.line_len(card.row)))
    };
    let Some(span) = screen::visible_span(editor, range) else {
        return false;
    };

    let rows = card_rows(card, theme, now_epoch);
    let x = text.x.saturating_sub(2);
    let spot = Spot {
        text,
        line: (*span.rows.start(), *span.rows.end()),
        x,
        width: CARD_COLS.min(text.right().saturating_sub(x)),
    };
    let fill = theme
        .try_get(scope::UI_SURFACE_RAISED)
        .and_then(|style| style.bg);
    match (nudged, style_rgb(fill)) {
        (true, Some(fill)) => paint_nudged(&rows, &spot, fill, theme, buf, scene),
        _ => paint_cells(&rows, &spot, theme, buf, scene),
    }
}

/// The card as a panel and text runs, under the nudge that moves it off the
/// cell grid.
///
/// The box touches one row more than its text, so it needs one more row of
/// the pane than it has text rows.
fn paint_nudged(
    rows: &[CardRow],
    spot: &Spot,
    fill: [u8; 3],
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) -> bool {
    let &Spot {
        text,
        line: (line_top, line_bottom),
        x,
        width,
    } = spot;
    let count = rows.len() as u16;
    let (top, nudge) = if line_bottom + count + 2 <= text.bottom() {
        (line_bottom + 1, NUDGE_BELOW)
    } else if line_top > text.y + count {
        (line_top - count - 1, NUDGE_ABOVE)
    } else {
        return false;
    };

    // A panel's fill covers the cells' backgrounds and not their glyphs, so
    // the code under the box goes first.
    let clear_end = (x + width + 1).min(text.right());
    for row in top..=top + count {
        for col in x..clear_end {
            buf[(col, row)].set_char(' ');
        }
    }

    let frame = theme.panel(PanelRole::Popout);
    scene.nudge([0, nudge]);
    Panel {
        style: BorderStyle::None_,
        border: fill,
        corner_radius: frame.corner_radius,
        fill: Some(fill),
        shadow: frame.shadow,
        edges: CARD_EDGES,
        corners: 0,
        fillets: 0,
        fillet_radius: 0,
        hatch: None,
        above_pools: false,
        backdrop: false,
        anchor: None,
    }
    .draw_components(Rect::new(x, top, width, count), scene);

    scene.nudge([TEXT_LEFT, nudge]);
    let room = (width * 16).saturating_sub(ROOM_TRIM);
    for (index, row) in rows.iter().enumerate() {
        let scale = theme.type_scale(row.role);
        let mut col = 0;
        for (part, style) in fit(
            &row.parts,
            usize::from(room) * 16 / usize::from(scale.max(1)),
        ) {
            let Some(color) = style_rgb(style.fg) else {
                continue;
            };
            TextRun {
                col: col as i16,
                row: 0,
                scale,
                color,
                bg: Some(fill),
                bold: style.add_modifier.contains(Modifier::BOLD),
                italic: false,
                text: &part,
                follow: 0,
                anchor: None,
            }
            .render(Rect::new(x, top + index as u16, 1, 1), buf, scene);
            col += text_run::advance_sixteenths(part.chars().count(), scale);
        }
    }
    scene.nudge([0, 0]);
    true
}

/// The card as plain rows of cells inside a popout card, for a terminal that
/// draws no nudge or no components.
///
/// The rows fill with the raised surface, and with the popout card's own
/// background in a theme that sets none.
fn paint_cells(
    rows: &[CardRow],
    spot: &Spot,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) -> bool {
    let &Spot {
        text,
        line: (line_top, line_bottom),
        x,
        width,
    } = spot;
    let count = rows.len() as u16;
    let top = if line_bottom + count < text.bottom() {
        line_bottom + 1
    } else if line_top >= text.y + count {
        line_top - count
    } else {
        return false;
    };

    let bg = theme
        .try_get(scope::UI_SURFACE_RAISED)
        .and_then(|style| style.bg)
        .unwrap_or_else(|| popout::popout_card_bg(theme));
    let content =
        popout::paint_popout_card(buf, Rect::new(x, top, width, count), bg, bg, theme, scene);
    for (index, row) in rows.iter().enumerate() {
        let y = content.y + index as u16;
        let mut col = content.x;
        for (part, style) in fit(&row.parts, usize::from(content.width)) {
            let room = usize::from(content.right().saturating_sub(col));
            col = buf.set_stringn(col, y, &part, room, style.bg(bg)).0;
        }
    }
    true
}

/// The rows of `card` in `theme`, with its dates read against `now_epoch`.
///
/// The body row shows only once the details land with a body, and the key row
/// only while a labeled blame key binds, so a card has two to four rows.
fn card_rows(card: &BlameCard, theme: &Theme, now_epoch: i64) -> Vec<CardRow> {
    let style = |name: &str| theme.get(name);
    let info = &card.commit.info;
    let muted = style(scope::UI_TEXT_MUTED);
    let dot = || (" \u{b7} ".to_owned(), muted);

    let title = CardRow {
        parts: vec![
            (info.short_sha.clone(), style(scope::VCS_COMMIT_SHA)),
            (" ".to_owned(), muted),
            (
                info.summary.clone(),
                style(scope::UI_TEXT).add_modifier(Modifier::BOLD),
            ),
        ],
        role: TypeRole::Ui,
    };

    let mut meta = vec![
        (info.author_name.clone(), muted),
        dot(),
        (
            blame::date_time(info.time, card.commit.utc_offset_minutes, now_epoch),
            muted,
        ),
        dot(),
        (commit_picker::age_words(now_epoch, info.time), muted),
        dot(),
    ];
    match info.parents.as_slice() {
        [] => meta.push(("root commit".to_owned(), muted)),
        [parent] => {
            meta.push(("parent ".to_owned(), muted));
            meta.push((
                parent.chars().take(7).collect(),
                style(scope::VCS_COMMIT_SHA),
            ));
        },
        parents => meta.push((format!("merge \u{b7} {} parents", parents.len()), muted)),
    }
    if let Some(details) = &card.details {
        let files = match details.files {
            1 => "1 file".to_owned(),
            files => format!("{files} files"),
        };
        meta.extend([
            dot(),
            (format!("+{}", details.added), style(scope::DIFF_ADDED)),
            (" ".to_owned(), muted),
            (
                format!("\u{2212}{}", details.removed),
                style(scope::DIFF_DELETED),
            ),
            (format!(" in {files}"), muted),
        ]);
    }

    let mut rows = vec![
        title,
        CardRow {
            parts: meta,
            role: TypeRole::Ui,
        },
    ];
    if let Some(body) = card
        .details
        .as_ref()
        .and_then(|details| details.body.clone())
    {
        rows.push(CardRow {
            parts: vec![(body, style(scope::UI_TEXT_DIM))],
            role: TypeRole::Ui,
        });
    }

    let mut hints = Vec::new();
    for (index, (key, label)) in card.hints.iter().enumerate() {
        if index > 0 {
            hints.push(("  ".to_owned(), muted));
        }
        hints.push((key.clone(), style(scope::UI_KEY_LABEL)));
        hints.push((format!(" {label}"), muted));
    }
    if !hints.is_empty() {
        rows.push(CardRow {
            parts: hints,
            role: TypeRole::Caption,
        });
    }
    rows
}

/// The parts of `parts` that fit in `room` characters, the last that fits
/// cut short and the rest left out.
fn fit(parts: &[(String, Style)], room: usize) -> Vec<(String, Style)> {
    let mut used = 0;
    let mut fitted = Vec::new();
    for (text, style) in parts {
        let len = text.chars().count();
        if used + len <= room {
            used += len;
            fitted.push((text.clone(), *style));
            continue;
        }

        let kept: String = text.chars().take(room - used).collect();
        if !kept.is_empty() {
            fitted.push((kept, *style));
        }
        break;
    }
    fitted
}

#[cfg(test)]
mod tests {
    use super::CARD_EDGES;
    use crate::{
        action_handlers, commit_picker,
        render::OVERLAY_LANE,
        test_harness::{CommitSpec, TestHarness},
        theme::PanelRole,
    };
    use ratatui::layout::Rect;
    use std::path::{Path, PathBuf};
    use stoatty_protocol::{
        command::{decode_stream, BorderStyle, Command, Edge},
        PROTOCOL_VERSION,
    };

    /// A nudge of the overlay lane as its offset, or a panel of it as its
    /// cells, its border, its radius, and its edges.
    type CardCommand = Result<(Rect, BorderStyle, u8, [Edge; 4]), [i16; 2]>;

    /// Without components the card is plain rows under the cursor's line,
    /// from the card's second column to its last but one. The rows read the
    /// sha and the subject, the meta line cut to the card, the body, and the
    /// card's keys, over the frame's border on the first and last rows.
    #[test]
    fn the_cell_card_reads_its_rows_under_the_line() {
        let mut h = card_open();
        h.stoat.stoatty = false;
        h.snapshot();

        let text = text_rect(&mut h);
        let rows: Vec<String> = h
            .rendered_text()
            .lines()
            .skip(usize::from(text.y) + 2)
            .take(4)
            .map(|row| row.chars().skip(usize::from(text.x) - 1).take(56).collect())
            .collect();

        let meta: String = {
            let card = h.stoat.blame_card.as_ref().expect("the card opened");
            let now = commit_picker::now_epoch(&h.stoat.executor);
            let age = commit_picker::age_words(now, card.commit.info.time);
            format!("fake \u{b7} 2023/11/14 22:13 \u{b7} {age} \u{b7} parent c1 \u{b7} +1 \u{2212}0 in 1 file")
                .chars()
                .take(56)
                .collect()
        };
        assert_eq!(
            rows,
            [
                format!("{:\u{2500}<56}", "c2 two"),
                meta,
                format!("{:<56}", "the body of two"),
                format!("{:\u{2500}<56}", "y copy sha  Esc close"),
            ],
        );
    }

    /// Under the nudge protocol the card is one borderless panel in the popout
    /// role's radius over the four rows under the cursor's line, 58 columns
    /// from two left of the text, with the board's edges. The panel draws
    /// 10 px down, its text 15 px right of that, and the lane's nudge resets
    /// after the card.
    #[test]
    fn the_nudged_card_draws_one_borderless_panel() {
        let mut h = card_open();
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        h.stoat.render();

        let text = text_rect(&mut h);
        let radius = h.stoat.theme.panel(PanelRole::Popout).corner_radius;
        assert_eq!(
            card_commands(&h),
            [
                Err([0, 10]),
                Ok((
                    Rect::new(text.x - 2, text.y + 2, 58, 4),
                    BorderStyle::None_,
                    radius,
                    CARD_EDGES
                )),
                Err([15, 10]),
                Err([0, 0]),
            ],
        );
    }

    /// A card with no room under its line opens over it, 8 px down, so the box
    /// ends 4 px over the line. The second commit's message has no body, so
    /// the card has three rows.
    #[test]
    fn a_card_near_the_pane_bottom_opens_over_the_line() {
        let base: String = (0..40).map(|i| format!("line {i}\n")).collect();
        let text = base.replace("line 39\n", "LINE 39\n");
        let mut h = blamed_pane(
            &[
                ("c1", "one", &[("a.rs", &base)]),
                ("c2", "two", &[("a.rs", &text)]),
            ],
            &text,
        );
        h.type_keys("4 0 G K");
        h.settle();
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        h.stoat.render();

        let (area, line) = {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            let area = editor.text_rect.expect("the text painted");
            (area, area.y + (39 - editor.scroll_row) as u16)
        };
        let radius = h.stoat.theme.panel(PanelRole::Popout).corner_radius;
        assert_eq!(
            card_commands(&h),
            [
                Err([0, 8]),
                Ok((
                    Rect::new(area.x - 2, line - 4, 58, 3),
                    BorderStyle::None_,
                    radius,
                    CARD_EDGES
                )),
                Err([15, 8]),
                Err([0, 0]),
            ],
        );
    }

    /// The overlay lane's nudges and panels from its first nudge on, which is
    /// where the card starts.
    fn card_commands(h: &TestHarness) -> Vec<CardCommand> {
        decode_stream(h.stoat.apc_scene.lane_bytes(OVERLAY_LANE))
            .iter()
            .skip_while(|command| !matches!(command, Command::Nudge(_)))
            .filter_map(|command| match command {
                Command::Nudge(offset) => Some(Err(*offset)),
                Command::Panel(panel) => Some(Ok((
                    Rect::new(panel.left, panel.top, panel.width, panel.height),
                    panel.style,
                    panel.corner_radius,
                    panel.edges,
                ))),
                _ => None,
            })
            .collect()
    }

    /// A blamed pane over `a.rs` of a two-commit repository, with the card
    /// open on line 2 and its details landed.
    fn card_open() -> TestHarness {
        let mut h = blamed_pane(
            &[
                ("c1", "one", &[("a.rs", "one\n")]),
                ("c2", "two\n\nthe body\nof two\n", &[("a.rs", "one\ntwo\n")]),
            ],
            "one\ntwo\n",
        );
        h.type_keys("j K");
        h.settle();
        h
    }

    /// A 120 by 24 window over `a.rs` of a repository at `/repo` that holds
    /// `history`, with `text` in the working tree and the Blame Column on.
    fn blamed_pane(history: &[CommitSpec<'_>], text: &str) -> TestHarness {
        let mut h = TestHarness::with_size(120, 24);
        h.seed_linear_history("/repo", history);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_fs().insert_file("/repo/a.rs", text);
        h.open_file(Path::new("/repo/a.rs"));
        h.type_keys("space h a");
        h.settle();
        h
    }

    /// The focused editor's text rect as the last paint left it.
    fn text_rect(h: &mut TestHarness) -> Rect {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.text_rect.expect("the text painted")
    }
}
