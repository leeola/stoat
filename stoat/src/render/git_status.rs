//! The git status panel, painted as the VcsStatus board draws it.
//!
//! The panel is a popout box at the window's top right. A header row names the
//! branch and how far it has moved, a banner names a merge or a rebase in
//! progress, and each section lists its files under a head row.
//!
//! Under a live scene the bars, the rules, the tag chip, and the small text are
//! scene components placed in sixteenths of a cell, as the board places them
//! in pixels. Without a scene each one falls back to cells. A bar becomes a
//! `▎`, the chip becomes plain text, and the small text draws at full size.

use crate::{
    git_status::{
        BodyLine, Divergence, FileRow, GitStatusModel, GitStatusPanel, Note, OpBanner, Section,
    },
    render::{chrome, clear_modal, paint::style_rgb, text::write_str},
    theme::{scope, Theme, TypeRole},
};
use ratatui::{
    buffer::Buffer,
    layout::{Position, Rect},
    style::{Modifier, Style},
    widgets::{Block, Borders, StatefulWidget},
};
use std::path::Path;
use stoat_widgets::{
    bar::Bar,
    panel::Panel,
    text_run::{self, TextRun},
    ApcScene,
};
use stoatty_protocol::command::{BorderStyle, PanelShadow};

/// The box's width in cells, the board's 460 px.
const BOX_WIDTH: u16 = 51;

/// The row the box's top edge sits on, under the tab bar and a banner row.
const BOX_TOP: u16 = 2;

/// Cells between the box's right edge and the window's right edge.
const BOX_MARGIN: u16 = 1;

/// Rows the operation banner takes, which hold the tag and the head, then the
/// detail, then the keys.
const BANNER_ROWS: u16 = 3;

/// The board's 8 px padding and gaps, in sixteenths.
const PAD: u16 = 8;

/// The 6 px gap between a section head's title and its count, in sixteenths.
const HEAD_GAP: u16 = 6;

/// The 12 px gap between two of the banner's keys, in sixteenths.
const KEY_GAP: u16 = 12;

/// The tag chip's `padding: 0 6px`, in sixteenths.
const CHIP_PAD: u16 = 6;

/// The banner bar's width, the board's `inset 2px 0 0`, in sixteenths.
const BANNER_BAR: u16 = 2;

/// A file row's bar, the board's `margin-left: 4px; width: 3px`, in
/// sixteenths.
const ROW_BAR_X: i16 = 4;
const ROW_BAR_WIDTH: u16 = 3;

/// Where the kind letter's field starts, after the row bar and an 8 px gap,
/// in sixteenths.
const KIND_X: u16 = 15;

/// The cell a row's text starts in, after the row bar and the kind letter,
/// and after the header's branch glyph.
const TEXT_CELL: u16 = 2;

/// The glyph a cell paint draws in place of a scene bar.
const BAR_GLYPH: &str = "\u{258e}";

/// Lay the panel out within `area`, returning its box and the box's inner
/// rect, or [`None`] when `area` is too small to hold one row of it.
///
/// The box hangs from [`BOX_TOP`] at the window's right, and its height fits
/// its rows. It stops one row short of the window's bottom, which the status
/// bar holds, and rows past that are clipped.
pub(crate) fn git_status_layout(
    area: Rect,
    model: Option<&GitStatusModel>,
) -> Option<(Rect, Rect)> {
    if area.width < BOX_WIDTH + BOX_MARGIN {
        return None;
    }
    let height = (content_rows(model) + 2).min(area.height.saturating_sub(BOX_TOP + 1));
    if height < 3 {
        return None;
    }

    let rect = Rect::new(
        area.right() - BOX_MARGIN - BOX_WIDTH,
        area.y + BOX_TOP,
        BOX_WIDTH,
        height,
    );
    Some((rect, Block::default().borders(Borders::ALL).inner(rect)))
}

/// Paint the panel over `area`, the window.
///
/// The panel's top moves first, so the selected row shows in the box at the
/// height the window leaves it. Until the first read lands, the box holds one
/// blank row.
pub(crate) fn render_git_status(
    panel: &mut GitStatusPanel,
    theme: &Theme,
    area: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let Some((rect, inner)) = git_status_layout(area, panel.model.as_ref()) else {
        return;
    };
    let body_top = body_top(inner, panel.model.as_ref());
    panel.scroll_into_view(usize::from(inner.bottom().saturating_sub(body_top)));
    let panel = &*panel;
    let model = panel.model.as_ref();

    clear_modal(rect, buf, theme, theme.get(scope::UI_BACKGROUND));
    let border = Style {
        fg: theme.get(scope::UI_MODAL_PICKER).fg,
        ..Style::default()
    };
    chrome::modal_frame_above_pools(buf, rect, None, border, theme, scene);
    let Some(model) = model else {
        return;
    };

    let live = scene.live() && style_rgb(border.fg).is_some();
    let mut painter = Painter {
        theme,
        buf,
        scene,
        live,
        caption: theme.type_scale(TypeRole::Caption),
        small: theme.type_scale(TypeRole::Small),
    };
    let row = |y: u16| Rect::new(inner.x, y, inner.width, 1);

    let mut y = inner.y;
    painter.header(row(y), model);
    y += 1;
    if let Some(op) = &model.op {
        if y < inner.bottom() {
            painter.banner(
                Rect::new(inner.x, y, inner.width, BANNER_ROWS.min(inner.bottom() - y)),
                op,
            );
        }
        y += BANNER_ROWS;
    }
    if model.sections.is_empty() {
        if y < inner.bottom() {
            painter.clean(row(y));
        }
        return;
    }

    for line in panel.body_lines() {
        if y >= inner.bottom() {
            return;
        }
        match line {
            BodyLine::Head(section) => painter.section_head(row(y), section),
            BodyLine::Row(section, index, file) => {
                painter.file_row(row(y), section, file, index == panel.selected, &model.root);
            },
        }
        y += 1;
    }
}

/// The index among [`GitStatusPanel::rows`] of the file row that the cell at
/// `column` and `row` shows, or `None` off every file row.
///
/// The answer reads the layout and the panel's [`GitStatusPanel::top`] that
/// the last paint drew with, so a press names the row drawn under it. A
/// section head, the header, and the banner name no row.
pub(crate) fn git_status_row_at(
    panel: &GitStatusPanel,
    area: Rect,
    column: u16,
    row: u16,
) -> Option<usize> {
    let model = panel.model.as_ref()?;
    let (_, inner) = git_status_layout(area, Some(model))?;
    if !inner.contains(Position::new(column, row)) {
        return None;
    }
    let line = usize::from(row.checked_sub(body_top(inner, Some(model)))?);
    match panel.body_lines().nth(line)? {
        BodyLine::Row(_, index, _) => Some(index),
        BodyLine::Head(_) => None,
    }
}

/// The first row of the body inside `inner`, under the header and any
/// banner.
fn body_top(inner: Rect, model: Option<&GitStatusModel>) -> u16 {
    let banner = if model.is_some_and(|model| model.op.is_some()) {
        BANNER_ROWS
    } else {
        0
    };
    inner.y + 1 + banner
}

/// Rows the panel's content takes, before the box clips it.
///
/// Before the first read lands the content is one blank row, so the box opens
/// at once rather than when the read lands.
fn content_rows(model: Option<&GitStatusModel>) -> u16 {
    let Some(model) = model else {
        return 1;
    };
    let banner = if model.op.is_some() { BANNER_ROWS } else { 0 };
    let body = match model.sections.len() {
        0 => 1,
        _ => model
            .sections
            .iter()
            .map(|section| 1 + section.rows.len())
            .sum(),
    };
    u16::try_from(body)
        .unwrap_or(u16::MAX)
        .saturating_add(1 + banner)
}

/// The painting context of one frame of the panel.
struct Painter<'a> {
    theme: &'a Theme,
    buf: &'a mut Buffer,
    scene: &'a mut ApcScene,
    /// Whether the scene components draw, rather than their cell fallbacks.
    live: bool,
    caption: u16,
    small: u16,
}

impl Painter<'_> {
    /// The branch glyph, the branch, its divergence, and the upstream at the
    /// right, over a rule.
    fn header(&mut self, row: Rect, model: &GitStatusModel) {
        let branch = model.branch.as_deref().unwrap_or("HEAD");
        let muted = self.theme.get(scope::UI_TEXT_MUTED);
        write_str(
            self.buf,
            row.x,
            row.y,
            "\u{2387}",
            self.theme.get(scope::UI_TEXT_DIM),
        );
        write_str(
            self.buf,
            row.x + TEXT_CELL,
            row.y,
            branch,
            self.theme.get(scope::UI_TEXT).add_modifier(Modifier::BOLD),
        );

        let after_branch = (TEXT_CELL + char_count(branch)) * 16 + PAD;
        self.caption(
            row,
            after_branch,
            &divergence_text(&model.divergence),
            muted,
        );
        self.caption_right(row, &model.upstream, muted);
        self.rule(row);
    }

    /// The operation's tag chip, head, and message, then its detail, then its
    /// keys, on the banner's tint behind its bar, over a rule.
    fn banner(&mut self, rect: Rect, op: &OpBanner) {
        let tint = self.theme.get(op.kind.scope());
        let text = self.theme.get(scope::UI_TEXT);
        let dim = self.theme.get(scope::UI_TEXT_DIM);
        let muted = self.theme.get(scope::UI_TEXT_MUTED);
        let line = |dy: u16| Rect::new(rect.x, rect.y + dy, rect.width, 1);

        if let Some(bg) = tint.bg {
            for y in rect.top()..rect.bottom() {
                for x in rect.left()..rect.right() {
                    self.buf[(x, y)].set_bg(bg);
                }
            }
        }
        self.bar(rect, 0, BANNER_BAR, tint);

        let tag = op.kind.tag();
        let chip = Rect::new(
            rect.x + 1,
            rect.y,
            (12 + 13 * char_count(tag)).div_ceil(16),
            1,
        );
        self.chip(chip, tag, tint);
        let after_chip = (chip.right() - rect.x) * 16 + PAD;
        let after_head = self.caption(line(0), after_chip, &op.head, text) + PAD;
        self.caption(line(0), after_head, &op.message, dim);

        if rect.height > 1 {
            self.caption(line(1), 16, &op.detail, muted);
        }
        if rect.height > 2 {
            let mut col = 16;
            for (key, label, key_scope) in &op.keys {
                col = self.caption(line(2), col, key, self.theme.get(key_scope));
                col = self.caption(line(2), col, &format!(" {label}"), muted) + KEY_GAP;
            }
            self.rule(line(2));
        }
    }

    /// The section's glyph and title in its color, its count, and its keys at
    /// the right.
    fn section_head(&mut self, row: Rect, section: &Section) {
        let color = self.theme.get(section.kind.scope());
        let muted = self.theme.get(scope::UI_TEXT_MUTED);
        let title = section.kind.title();
        write_str(self.buf, row.x, row.y, "\u{25be}", color);
        write_str(
            self.buf,
            row.x + TEXT_CELL,
            row.y,
            title,
            color.add_modifier(Modifier::BOLD),
        );

        let after_title = (TEXT_CELL + char_count(title)) * 16 + HEAD_GAP;
        self.caption(row, after_title, &section.rows.len().to_string(), muted);
        self.caption_right(row, section.kind.keys(), muted);
    }

    /// A file's bar in its section's color, its kind letter, its path as a
    /// muted directory and the name, and its note at the right.
    fn file_row(
        &mut self,
        row: Rect,
        section: &Section,
        file: &FileRow,
        selected: bool,
        root: &Path,
    ) {
        if selected && let Some(bg) = self.theme.get(scope::UI_LIST_SELECTED).bg {
            for x in row.left()..row.right() {
                self.buf[(x, row.y)].set_bg(bg);
            }
        }
        self.bar(
            row,
            ROW_BAR_X,
            ROW_BAR_WIDTH,
            self.theme.get(section.kind.scope()),
        );
        // FIXME: The kind letter and the tag chip draw at regular weight, since
        // a text run carries no weight.
        let letter = file.kind.letter().to_string();
        let kind_style = self
            .theme
            .get(file.kind.scope())
            .add_modifier(Modifier::BOLD);
        if self.live {
            self.run(row, KIND_X, self.small, &letter, kind_style, None);
        } else {
            write_str(self.buf, row.x + 1, row.y, &letter, kind_style);
        }

        let note = note_text(file.note);
        let note_cells = note.as_ref().map_or(0, |(text, _)| char_count(text) + 1);
        let room = usize::from(row.width.saturating_sub(TEXT_CELL + note_cells));
        let rel = file.path.strip_prefix(root).unwrap_or(&file.path);
        let name = rel
            .file_name()
            .map_or_else(|| rel.to_string_lossy(), |name| name.to_string_lossy());
        let dir = rel
            .parent()
            .filter(|dir| !dir.as_os_str().is_empty())
            .map(|dir| format!("{}/", dir.display()))
            .unwrap_or_default();
        let dir: String = dir.chars().take(room).collect();
        let name: String = name.chars().take(room - dir.chars().count()).collect();
        let x = row.x + TEXT_CELL;
        write_str(
            self.buf,
            x,
            row.y,
            &dir,
            self.theme.get(scope::UI_TEXT_MUTED),
        );
        write_str(
            self.buf,
            x + char_count(&dir),
            row.y,
            &name,
            self.theme.get(scope::UI_TEXT),
        );

        if let Some((text, note_scope)) = note {
            self.caption_right(row, &text, self.theme.get(note_scope));
        }
    }

    /// The clean tree's mark.
    fn clean(&mut self, row: Rect) {
        let style = self.theme.get(scope::UI_STATUSBAR_CLEAN);
        self.caption(row, PAD, "\u{2713} working tree clean", style);
    }

    /// Write `text` at the Caption scale, starting `col` sixteenths into
    /// `row`, and return where it ends.
    ///
    /// Without a scene it is full-size cell text from the first cell at or
    /// past `col`.
    fn caption(&mut self, row: Rect, col: u16, text: &str, style: Style) -> u16 {
        if self.live {
            self.run(row, col, self.caption, text, style, None);
            return col + text_run::advance_sixteenths(text.chars().count(), self.caption);
        }
        let cell = col.div_ceil(16);
        write_str(self.buf, row.x + cell, row.y, text, style);
        (cell + char_count(text)) * 16
    }

    /// Write `text` at the Caption scale so it ends [`PAD`] short of `row`'s
    /// right edge, or as cell text ending in the row's last cell.
    fn caption_right(&mut self, row: Rect, text: &str, style: Style) {
        if self.live {
            let width = text_run::advance_sixteenths(text.chars().count(), self.caption);
            let col = (row.width * 16).saturating_sub(PAD + width);
            self.run(row, col, self.caption, text, style, None);
            return;
        }
        let x = row.right().saturating_sub(char_count(text)).max(row.x);
        write_str(self.buf, x, row.y, text, style);
    }

    /// The tag chip over `chip`, its tag in the chrome color on `tint`'s
    /// foreground.
    fn chip(&mut self, chip: Rect, tag: &str, tint: Style) {
        if !self.live {
            write_str(
                self.buf,
                chip.x,
                chip.y,
                tag,
                tint.add_modifier(Modifier::BOLD),
            );
            return;
        }
        let fill = rgb(tint);
        Panel {
            style: BorderStyle::None_,
            border: fill,
            corner_radius: 1,
            fill: Some(fill),
            shadow: PanelShadow::None_,
            inset_x: 0,
            above_pools: false,
            backdrop: false,
            anchor: None,
        }
        .draw_components(chip, self.scene);
        let chrome = Style {
            fg: self.theme.get(scope::UI_SURFACE_CHROME).bg,
            ..Style::default()
        };
        self.run(chip, CHIP_PAD, self.caption, tag, chrome, Some(fill));
    }

    /// A bar `width` sixteenths wide at `x`, down the whole of `rect`, in
    /// `style`'s foreground, or a bar glyph in the cell holding `x` of each
    /// row.
    fn bar(&mut self, rect: Rect, x: i16, width: u16, style: Style) {
        if self.live {
            Bar {
                x,
                y: 0,
                width,
                height: rect.height * 16,
                color: rgb(style),
            }
            .render(rect, self.buf, self.scene);
            return;
        }
        let col = rect.x + x.unsigned_abs() / 16;
        for y in rect.top()..rect.bottom() {
            write_str(self.buf, col, y, BAR_GLYPH, style);
        }
    }

    /// A rule 1/16 high along the bottom of `row` in the raised surface's
    /// color. A cell paint has no line that thin, so it draws none.
    fn rule(&mut self, row: Rect) {
        if !self.live {
            return;
        }
        Bar {
            x: 0,
            y: 15,
            width: row.width * 16,
            height: 1,
            color: rgb(Style {
                fg: self.theme.get(scope::UI_SURFACE_RAISED).bg,
                ..Style::default()
            }),
        }
        .render(row, self.buf, self.scene);
    }

    /// A text run at `scale`, `col` sixteenths into `row`.
    fn run(
        &mut self,
        row: Rect,
        col: u16,
        scale: u16,
        text: &str,
        style: Style,
        bg: Option<[u8; 3]>,
    ) {
        TextRun {
            col: i16::try_from(col).unwrap_or(i16::MAX),
            row: 0,
            scale,
            color: rgb(style),
            bg,
            text,
            follow: 0,
            anchor: None,
        }
        .render(row, self.buf, self.scene);
    }
}

/// The header's account of `divergence`, such as `↑7 ↓2` or `= origin/main`.
///
/// A side with no commits is left out, so a branch only ahead reads `↑1`.
fn divergence_text(divergence: &Divergence) -> String {
    match divergence {
        Divergence::Equal(name) => format!("= {name}"),
        Divergence::AheadBehind(ahead, behind) => [('\u{2191}', ahead), ('\u{2193}', behind)]
            .into_iter()
            .filter(|(_, count)| **count > 0)
            .map(|(arrow, count)| format!("{arrow}{count}"))
            .collect::<Vec<_>>()
            .join(" "),
        Divergence::Detached(sha) => format!("detached at {sha}"),
        Divergence::Rebase { done, total, onto } => format!("{done} / {total} onto {onto}"),
        Divergence::Unknown => String::new(),
    }
}

/// The text of `note` and the scope it paints in, or `None` for no note.
///
/// A line count leaves out a side that changes no line, so an addition alone
/// reads `+6`.
fn note_text(note: Note) -> Option<(String, &'static str)> {
    match note {
        Note::Open { open, total } => Some((
            format!("{open} / {total} open"),
            scope::VCS_STATUS_CONFLICTED,
        )),
        Note::Resolved => Some(("resolved".to_string(), scope::VCS_STATUS_STAGED)),
        Note::TookOurs => Some(("took ours".to_string(), scope::VCS_STATUS_STAGED)),
        Note::TookTheirs => Some(("took theirs".to_string(), scope::VCS_STATUS_STAGED)),
        Note::Lines { added, deleted } => {
            let text = [('+', added), ('\u{2212}', deleted)]
                .into_iter()
                .filter(|(_, count)| *count > 0)
                .map(|(sign, count)| format!("{sign}{count}"))
                .collect::<Vec<_>>()
                .join(" ");
            Some((text, scope::UI_TEXT_MUTED))
        },
        Note::None => None,
    }
}

/// The foreground of `style` as a scene color. A style with no RGB foreground
/// reads as white, since a scene component needs a color to draw at all.
fn rgb(style: Style) -> [u8; 3] {
    style_rgb(style.fg).unwrap_or([0xff, 0xff, 0xff])
}

fn char_count(text: &str) -> u16 {
    u16::try_from(text.chars().count()).unwrap_or(u16::MAX)
}

#[cfg(test)]
mod tests;
