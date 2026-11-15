//! The Commit Preview's box, painted as the BlamePreview board draws it.
//!
//! The board places the box 120 px in from each side of the window, 60 px under
//! its top, and 48 px over its bottom. At the scene's 9 by 18 px cell that is
//! 13 columns in and 3 rows in, and each edge stands off its cells by the
//! pixels left over. A window under 60 by 14 cells keeps a margin of 2 columns
//! and 1 row on the cell grid instead.
//!
//! Inside the box, a header row names the commit with a read-only chip at its
//! right, a muted meta line follows, and a rule divides them from the patch.
//! The footer on the box's bottom row lists the preview's keys.

use crate::{
    blame,
    commit_preview::CommitPreview,
    render::{
        self, chrome, commit_patch,
        commits::PreviewDials,
        paint,
        picker::{self, Footer},
        text,
    },
    theme::{scope, PanelRole, Theme, TypeRole},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Modifier,
    widgets::{Block, Borders, StatefulWidget},
};
use std::mem;
use stoat_widgets::{
    panel::Panel,
    text_run::{self, TextRun},
    ApcScene,
};
use stoatty_protocol::command::{BorderStyle, Edge, PanelShadow};

/// The columns the board's box stands in from each side of the window.
const BOARD_INSET_X: u16 = 13;

/// The rows the board's box stands in from the top and the bottom of the
/// window.
const BOARD_INSET_Y: u16 = 3;

/// The smallest window, in columns and rows, that keeps the board's margins.
const BOARD_MIN: (u16, u16) = (60, 14);

/// Where the board's box edges stand against the cells inside its border.
///
/// The box is 120 px in from each side, which is 13 columns and 3 px, so each
/// side crosses the border column and stops 3 px short of its far side. It is
/// 60 px down, which is 3 rows and 6 px, so the top crosses the border row and
/// stops 6 px short of its top. It is 48 px up, which is 3 rows less 6 px, so
/// the bottom stands 6 px under the footer row.
const BOARD_EDGES: [Edge; 4] = [
    Edge::Margin(6),
    Edge::Margin(3),
    Edge::Outset(6),
    Edge::Margin(3),
];

/// The rows above the patch, which hold the header, the meta line, and the
/// rule.
const HEADER_ROWS: u16 = 3;

/// The read-only chip's text, with its padding.
const CHIP: &str = " preview \u{b7} read-only ";

/// The word the patch's rows show while the patch builds.
const LOADING: &str = "loading\u{2026}";

/// Where the preview's parts go in a window.
pub(crate) struct PreviewLayout {
    /// The cells of the box, with its border rows and columns.
    area: Rect,
    /// Where the box's edges stand against the cells inside the border, or
    /// `None` for a box on the cell grid.
    edges: Option<[Edge; 4]>,
    /// The rows inside the border.
    inner: Rect,
    /// The rows the patch scrolls in.
    pub(crate) body: Rect,
    /// The footer's row, which is the box's bottom border row.
    footer: Rect,
}

/// The preview's layout in the window `full`, or `None` for a window with no
/// row for the patch.
pub(crate) fn commit_preview_layout(full: Rect) -> Option<PreviewLayout> {
    let (area, edges) = if full.width >= BOARD_MIN.0 && full.height >= BOARD_MIN.1 {
        let area = Rect::new(
            full.x + BOARD_INSET_X,
            full.y + BOARD_INSET_Y,
            full.width - 2 * BOARD_INSET_X,
            full.height - 2 * BOARD_INSET_Y,
        );
        (area, Some(BOARD_EDGES))
    } else {
        let area = Rect::new(
            full.x + 2,
            full.y + 1,
            full.width.saturating_sub(4),
            full.height.saturating_sub(2),
        );
        (area, None)
    };

    let inner = Block::default().borders(Borders::ALL).inner(area);
    if inner.width == 0 || inner.height <= HEADER_ROWS {
        return None;
    }
    Some(PreviewLayout {
        area,
        edges,
        inner,
        body: Rect::new(
            inner.x,
            inner.y + HEADER_ROWS,
            inner.width,
            inner.height - HEADER_ROWS,
        ),
        footer: Rect::new(inner.x, area.bottom() - 1, inner.width, 1),
    })
}

/// Paint the preview over the window `full`.
///
/// The paint clamps the preview's scroll to the patch's last page, since only
/// a built patch knows how many rows it has.
pub(crate) fn render_commit_preview_modal(
    preview: &mut CommitPreview,
    now_epoch: i64,
    theme: &Theme,
    dials: PreviewDials,
    full: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let Some(layout) = commit_preview_layout(full) else {
        return;
    };
    paint_box(&layout, theme, buf, scene);

    let inner = layout.inner;
    let row = |offset: u16| Rect::new(inner.x, inner.y + offset, inner.width, 1);
    paint_header(preview, theme, row(0), buf, scene);
    paint_meta(preview, now_epoch, theme, row(1), buf);
    chrome::hline(
        buf,
        inner.x,
        inner.y + 2,
        inner.width,
        theme.get(scope::UI_DIVIDER),
        scene,
    );
    paint_body(preview, theme, dials, layout.body, buf, scene);

    let hints: Vec<(&str, &str)> = preview
        .hints
        .iter()
        .map(|(key, label)| (key.as_str(), label.as_str()))
        .collect();
    let footer = Footer {
        hints: &hints,
        ..Footer::default()
    };
    picker::modal_footer(buf, layout.footer, &footer, theme, scene);
}

/// Paint the box's surface and its frame.
///
/// Under a live scene the board's box stands off the cell grid by a few
/// pixels, and a cell's background colors the whole cell. So the cells inside
/// the border carry the surface, and a backdrop panel fills the ring between
/// those cells and the box's edges. A backdrop's fill leaves its own cells
/// alone, so the patch keeps the tints its cells carry. Otherwise the box is a
/// modal frame over cells that carry the surface.
fn paint_box(layout: &PreviewLayout, theme: &Theme, buf: &mut Buffer, scene: &mut ApcScene) {
    let style = theme.get(scope::UI_MODAL_PICKER);
    let board = layout.edges.filter(|_| scene.live()).and_then(|edges| {
        Some((
            edges,
            paint::style_rgb(style.fg)?,
            render::modal_fill(theme, style)?,
        ))
    });
    let Some((edges, border, surface)) = board else {
        render::clear_modal(layout.area, buf, theme, style);
        chrome::modal_frame_above_pools(buf, layout.area, None, style, theme, scene);
        return;
    };

    let area = layout.area;
    let cells = Rect::new(area.x + 1, area.y + 1, area.width - 2, area.height - 1);
    render::clear_modal(cells, buf, theme, style);
    clear_border_glyphs(area, buf);

    let frame = theme.panel(PanelRole::Overlay);
    let box_panel = |style, fill, shadow, backdrop: bool| Panel {
        style,
        border,
        corner_radius: frame.corner_radius,
        fill,
        shadow,
        edges,
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: !backdrop,
        backdrop,
        anchor: None,
    };
    // A backdrop hides no other pass's components and holds no pool composite
    // back, so a plain panel over the same box does both. The plain panel goes
    // first, since a panel hides every earlier panel inside its box.
    box_panel(BorderStyle::None_, None, PanelShadow::None_, false).draw_components(cells, scene);
    box_panel(BorderStyle::Rounded, Some(surface), frame.shadow, true)
        .draw_components(cells, scene);
}

/// Clear the glyphs of the cells the board's box border crosses, which are the
/// box's top row and side columns and the row under the box.
///
/// The backgrounds stay, so the part of each cell outside the box shows the
/// panes. The box's fill covers the part inside, but the glyphs draw over a
/// fill.
fn clear_border_glyphs(area: Rect, buf: &mut Buffer) {
    let below = Some(area.bottom()).filter(|&y| y < buf.area.bottom());
    for y in [Some(area.y), below].into_iter().flatten() {
        for x in area.x..area.right() {
            buf[(x, y)].set_char(' ');
        }
    }
    for y in area.y + 1..area.bottom() {
        buf[(area.x, y)].set_char(' ');
        buf[(area.right() - 1, y)].set_char(' ');
    }
}

/// Paint the header row, which holds the short sha, the bold subject, and the
/// read-only chip at the right.
///
/// The chip draws when the row holds it after the sha. A theme with no chip
/// style draws its text plain in the muted color.
fn paint_header(
    preview: &CommitPreview,
    theme: &Theme,
    row: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let info = &preview.commit.info;
    let sha_x = row.x + 1;
    let subject_x = sha_x + info.short_sha.chars().count() as u16 + 1;
    let chip_width = CHIP.chars().count() as u16;
    let chip_x = row
        .right()
        .checked_sub(chip_width + 1)
        .filter(|&x| x > subject_x);

    let text_end = chip_x.map_or(row.right() - 1, |x| x - 1);
    text::write_str_clipped(
        buf,
        sha_x,
        row.y,
        &info.short_sha,
        theme.get(scope::VCS_COMMIT_SHA),
        text_end,
    );
    text::write_str_clipped(
        buf,
        subject_x,
        row.y,
        &info.summary,
        theme.get(scope::UI_TEXT).add_modifier(Modifier::BOLD),
        text_end,
    );

    let Some(chip_x) = chip_x else {
        return;
    };
    let chip = Rect::new(chip_x, row.y, chip_width, 1);
    let style = match theme.try_get(scope::UI_PREVIEW_CHIP) {
        Some(style) => chrome::chip_cells(chip, style, scene),
        None => theme.get(scope::UI_TEXT_MUTED),
    };
    text::write_str(buf, chip_x, row.y, CHIP, style);
}

/// Paint the meta line, which names the author, the date, the parent the patch
/// is against, and the change size once the build lands.
fn paint_meta(preview: &CommitPreview, now_epoch: i64, theme: &Theme, row: Rect, buf: &mut Buffer) {
    let info = &preview.commit.info;
    let muted = theme.get(scope::UI_TEXT_MUTED);
    let dot = || (" \u{b7} ".to_owned(), muted);
    let mut parts = vec![
        (info.author_name.clone(), muted),
        dot(),
        (
            blame::date_time(info.time, preview.commit.utc_offset_minutes, now_epoch),
            muted,
        ),
        dot(),
    ];
    match info.parents.get(preview.parent) {
        Some(parent) => {
            parts.push(("against parent ".to_owned(), muted));
            parts.push((
                parent.chars().take(7).collect(),
                theme.get(scope::VCS_COMMIT_SHA),
            ));
        },
        None => parts.push(("root commit".to_owned(), muted)),
    }
    if let Some((files, added, removed)) = preview.summary {
        let files = match files {
            1 => "1 file".to_owned(),
            files => format!("{files} files"),
        };
        parts.extend([
            dot(),
            (format!("{files} "), muted),
            (format!("+{added}"), theme.get(scope::DIFF_ADDED)),
            (" ".to_owned(), muted),
            (format!("\u{2212}{removed}"), theme.get(scope::DIFF_DELETED)),
        ]);
    }

    let end = row.right() - 1;
    let mut x = row.x + 1;
    for (part, style) in &parts {
        text::write_str_clipped(buf, x, row.y, part, *style, end);
        x = x.saturating_add(part.chars().count() as u16);
    }
}

/// Ring `row`, the row of the patch that holds the origin's line, in the
/// focused border's color.
///
/// Without a live scene the row takes no ring. The ring is a panel, and a
/// panel hides the scene components declared before it inside its box, so it
/// goes before the row's lane, numbers, and rule.
fn paint_origin_ring(row: Rect, theme: &Theme, scene: &mut ApcScene) {
    let Some(border) =
        paint::style_rgb(theme.get(scope::UI_BORDER_FOCUSED).fg).filter(|_| scene.live())
    else {
        return;
    };
    Panel {
        style: BorderStyle::Light,
        border,
        corner_radius: 0,
        fill: None,
        shadow: PanelShadow::None_,
        edges: Edge::symmetric(0, 0),
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: true,
        backdrop: false,
        anchor: None,
    }
    .draw_components(row, scene);
}

/// Name where the reader came from at the right end of `row`, in the focused
/// border's color, as `label` after a left-pointing triangle.
///
/// Under a live scene the name is a Caption-scale run that ends a cell short
/// of the row's right edge, on the background of the cell under it, so it
/// covers the code it overlaps. Without one it is cells that end at the same
/// place.
fn paint_origin_label(
    label: &str,
    row: Rect,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let text = format!("\u{25c2} {label}");
    let style = theme.get(scope::UI_BORDER_FOCUSED);
    let last = row.right() - 1;
    let Some(color) = paint::style_rgb(style.fg).filter(|_| scene.live()) else {
        let width = text.chars().count() as u16;
        text::write_str(buf, last.saturating_sub(width), row.y, &text, style);
        return;
    };

    let scale = theme.type_scale(TypeRole::Caption);
    let advance = text_run::advance_sixteenths(text.chars().count(), scale);
    TextRun {
        col: -(advance as i16),
        row: 0,
        scale,
        color,
        bg: paint::style_rgb(Some(buf[(last, row.y)].bg)),
        text: &text,
        bold: false,
        italic: false,
        follow: 0,
        anchor: None,
    }
    .render(Rect::new(last, row.y, 1, 1), buf, scene);
}

/// Paint the patch into `body`, or a word in its place: [`LOADING`] while the
/// patch builds, and `no changes` for a commit with no diff.
fn paint_body(
    preview: &mut CommitPreview,
    theme: &Theme,
    dials: PreviewDials,
    body: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let word = match &preview.document {
        Some(Some(doc)) => {
            let height = usize::from(body.height);
            if mem::take(&mut preview.seek)
                && let Some(origin) = preview.origin_row
            {
                preview.scroll = commit_patch::origin_scroll(&preview.rows, origin, height);
            }
            let last_page = preview.rows.len().saturating_sub(height);
            preview.scroll = preview.scroll.min(last_page);

            let origin = preview
                .origin_row
                .and_then(|row| row.checked_sub(preview.scroll))
                .filter(|&row| row < height)
                .map(|row| Rect::new(body.x, body.y + row as u16, body.width, 1));
            if let Some(row) = origin {
                paint_origin_ring(row, theme, scene);
            }
            commit_patch::render_commit_patch(
                doc,
                &preview.rows[preview.scroll..],
                theme,
                body,
                dials,
                buf,
                scene,
            );
            if let Some(row) = origin {
                paint_origin_label(&preview.origin.label, row, theme, buf, scene);
            }
            return;
        },
        Some(None) => "no changes",
        None => LOADING,
    };

    let width = word.chars().count() as u16;
    text::write_str_clipped(
        buf,
        body.x + body.width.saturating_sub(width) / 2,
        body.y + body.height.saturating_sub(1) / 2,
        word,
        theme.get(scope::UI_TEXT_MUTED),
        body.right(),
    );
}

#[cfg(test)]
mod tests {
    use super::{commit_preview_layout, BOARD_EDGES};
    use crate::{
        action_handlers::dispatch,
        render::{self, paint, OVERLAY_LANE},
        test_harness::{CommitSpec, TestHarness},
        theme::{scope, PanelRole},
    };
    use ratatui::{buffer::Buffer, layout::Rect, style::Color};
    use std::path::{Path, PathBuf};
    use stoat_action::BlamePreview;
    use stoat_widgets::panel::Panel;
    use stoatty_protocol::{
        command::{decode_stream, BorderStyle, Command, Edge, PanelShadow},
        PROTOCOL_VERSION,
    };

    /// A panel of the overlay lane as its cells, its border style, its fill,
    /// its shadow, its radius, its edges, and whether it floats above the pools
    /// and whether it is a backdrop.
    type PanelParts = (
        Rect,
        BorderStyle,
        Option<[u8; 3]>,
        PanelShadow,
        u8,
        [Edge; 4],
        bool,
        bool,
    );

    /// The two commits [`preview_open`] previews the second of.
    const TWO_COMMITS: &[CommitSpec<'static>] = &[
        ("c1", "one", &[("a.rs", "one\n")]),
        ("c2", "two", &[("a.rs", "one\ntwo\n")]),
    ];

    /// A window of 60 by 14 cells or more holds the box 13 columns and 3 rows
    /// in, at the board's edges, and a smaller window holds it 2 columns and
    /// 1 row in, on the cell grid. The patch takes the rows under the header,
    /// the meta line, and the rule, and the footer takes the bottom border row.
    #[test]
    fn the_box_keeps_the_board_margins_from_60_by_14_cells() {
        let layouts = [(160, 50), (60, 14), (59, 14), (60, 13)].map(|(width, height)| {
            commit_preview_layout(Rect::new(0, 0, width, height))
                .map(|layout| (layout.area, layout.edges, layout.body, layout.footer))
        });
        assert_eq!(
            layouts,
            [
                Some((
                    Rect::new(13, 3, 134, 44),
                    Some(BOARD_EDGES),
                    Rect::new(14, 7, 132, 39),
                    Rect::new(14, 46, 132, 1),
                )),
                Some((
                    Rect::new(13, 3, 34, 8),
                    Some(BOARD_EDGES),
                    Rect::new(14, 7, 32, 3),
                    Rect::new(14, 10, 32, 1),
                )),
                Some((
                    Rect::new(2, 1, 55, 12),
                    None,
                    Rect::new(3, 5, 53, 7),
                    Rect::new(3, 12, 53, 1),
                )),
                Some((
                    Rect::new(2, 1, 56, 11),
                    None,
                    Rect::new(3, 5, 54, 6),
                    Rect::new(3, 11, 54, 1),
                )),
            ],
        );
    }

    /// Without components the box is cells. Its header reads the sha, the
    /// subject, and the read-only chip at the right, its meta line names the
    /// parent and the change size, a rule closes the header, and the footer on
    /// the bottom border reads the keys.
    #[test]
    fn the_cell_box_reads_its_header_meta_line_and_footer() {
        let mut h = preview_open();
        h.stoat.stoatty = false;
        h.snapshot();

        let rows: Vec<String> = h
            .rendered_text()
            .lines()
            .skip(3)
            .take(18)
            .map(|row| row.chars().skip(13).take(94).collect())
            .collect();
        let meta =
            "fake \u{b7} 2023/11/14 22:13 \u{b7} against parent c1 \u{b7} 1 file +1 \u{2212}0";
        assert_eq!(
            [&rows[0], &rows[1], &rows[2], &rows[3], &rows[17]],
            [
                &format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(92)),
                &format!(
                    "\u{2502}{:<70} preview \u{b7} read-only  \u{2502}",
                    " c2 two"
                ),
                &format!("\u{2502} {meta:<91}\u{2502}"),
                &format!("\u{2502}{}\u{2502}", "\u{2500}".repeat(92)),
                &format!(
                    "\u{2514} {:<91}\u{2518}",
                    "j/k scroll  y copy sha  Esc back to blame"
                ),
            ],
        );
    }

    /// Under a live scene the box is two panels over the cells inside its
    /// border, at the board's edges. A plain panel that draws nothing goes
    /// first and holds the pools back. A backdrop panel then fills the ring
    /// outside those cells, strokes the frame, and casts the shadow. The
    /// read-only chip draws in its own box, and a light square ring marks the
    /// origin row, the added line the preview opened from.
    #[test]
    fn the_live_box_draws_a_backdrop_ring_at_the_board_edges() {
        let mut h = preview_open();
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        h.stoat.render();

        let theme = &h.stoat.theme;
        let frame = theme.panel(PanelRole::Overlay);
        let surface = render::modal_fill(theme, theme.get(scope::UI_MODAL_PICKER));
        let chip = Panel::chip(
            paint::style_rgb(theme.get(scope::UI_PREVIEW_CHIP).bg).expect("an RGB chip"),
        );
        let cells = Rect::new(14, 4, 92, 17);
        let edges = [
            Edge::Margin(6),
            Edge::Margin(3),
            Edge::Outset(6),
            Edge::Margin(3),
        ];
        assert_eq!(
            box_panels(&h),
            [
                (
                    cells,
                    BorderStyle::None_,
                    None,
                    PanelShadow::None_,
                    frame.corner_radius,
                    edges,
                    true,
                    false,
                ),
                (
                    cells,
                    BorderStyle::Rounded,
                    surface,
                    frame.shadow,
                    frame.corner_radius,
                    edges,
                    false,
                    true,
                ),
                (
                    Rect::new(84, 4, 21, 1),
                    chip.style,
                    chip.fill,
                    chip.shadow,
                    chip.corner_radius,
                    chip.edges,
                    chip.above_pools,
                    chip.backdrop,
                ),
                (
                    Rect::new(14, 9, 92, 1),
                    BorderStyle::Light,
                    None,
                    PanelShadow::None_,
                    0,
                    Edge::symmetric(0, 0),
                    true,
                    false,
                ),
            ],
        );
    }

    /// Under a live scene the cells the box's border crosses hold no glyphs,
    /// since the box's fill covers their backgrounds and not their glyphs.
    /// They keep the pane's backgrounds rather than the box's surface, which
    /// would show past the border. Before the preview opens, the same cells
    /// hold the pane's text.
    #[test]
    fn the_live_box_clears_the_glyphs_its_border_crosses() {
        let text = format!("{}\n", "x".repeat(110)).repeat(30);
        let mut h = blamed_pane(&[("c1", "one", &[("a.rs", &text)])], &text);
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        let before = border_cells(&h.stoat.render());

        h.type_keys("enter");
        h.settle();
        let surface = h.stoat.theme.get(scope::UI_MODAL_PICKER).bg;
        let after: Vec<(String, bool)> = border_cells(&h.stoat.render())
            .into_iter()
            .map(|(symbol, bg)| (symbol, Some(bg) == surface))
            .collect();
        assert_eq!(
            (before.iter().any(|(symbol, _)| symbol == "x"), after),
            (true, vec![(" ".to_string(), false); before.len()]),
        );
    }

    /// A root commit's meta line says so where a commit names its parent, and
    /// a commit of two files counts them in the plural.
    #[test]
    fn a_root_commit_meta_line_reads_root_commit() {
        let mut h = blamed_pane(
            &[("c1", "one", &[("a.rs", "one\n"), ("b.rs", "b\n")])],
            "one\n",
        );
        h.stoat.stoatty = false;
        h.type_keys("enter");
        h.settle();
        h.snapshot();

        let meta: String = h
            .rendered_text()
            .lines()
            .nth(5)
            .map(|row| row.chars().skip(15).take(91).collect())
            .unwrap_or_default();
        let want = "fake \u{b7} 2023/11/14 22:13 \u{b7} root commit \u{b7} 2 files +2 \u{2212}0";
        assert_eq!(meta, format!("{want:<91}"));
    }

    /// The patch's rows read the loading word, centered, until the build
    /// lands.
    #[test]
    fn the_patch_reads_loading_until_its_build_lands() {
        let mut h = blamed_pane(TWO_COMMITS, "one\ntwo\n");
        h.type_keys("j");
        dispatch(&mut h.stoat, &BlamePreview);

        let buf = h.stoat.render();
        let row: String = (14..106).map(|x| buf[(x, 13)].symbol()).collect();
        assert_eq!(row, format!("{:^92}", "loading\u{2026}"));
    }

    /// A scrolled preview paints its patch from the scroll. One step down puts
    /// the patch's context line in the body's first row, where the File Header
    /// was.
    #[test]
    fn a_scrolled_preview_paints_its_patch_from_the_scroll() {
        let text = {
            let lines: String = (0..40).map(|i| format!("line {i}\n")).collect();
            format!("one\n{lines}")
        };
        let mut h = blamed_pane(
            &[
                ("c1", "one", &[("a.rs", "one\n")]),
                ("c2", "two", &[("a.rs", &text)]),
            ],
            &text,
        );
        h.stoat.stoatty = false;
        h.type_keys("j enter");
        h.settle();
        h.type_keys("j");
        h.snapshot();

        let row: String = h
            .rendered_text()
            .lines()
            .nth(7)
            .map(|row| row.chars().skip(14).take(92).collect())
            .unwrap_or_default();
        assert_eq!(row, format!("{:<92}", "   1   1 \u{2502}one"));
    }

    /// Without a live scene the origin row takes no ring, and its label is
    /// cells that end a cell short of the row's right edge.
    #[test]
    fn the_cell_label_names_the_origin_at_the_row_end() {
        let mut h = preview_open();
        h.stoat.stoatty = false;
        h.snapshot();

        let row: String = h
            .rendered_text()
            .lines()
            .nth(9)
            .map(|row| row.chars().skip(14).take(92).collect())
            .unwrap_or_default();
        assert_eq!(
            row,
            format!("{:<83}\u{25c2} a.rs:2 ", "\u{258e}      2 \u{2502}two"),
        );
    }

    /// The open preview hides the terminal cursor, which otherwise draws at the
    /// cursor of the pane under the box. The cursor shows again once the
    /// preview closes.
    #[test]
    fn the_open_preview_hides_the_terminal_cursor() {
        let mut h = preview_open();
        h.stoat.render();
        let open = h.stoat.primary_cursor_screen_pos();
        h.type_keys("escape");
        assert_eq!(
            (open, h.stoat.primary_cursor_screen_pos().is_some()),
            (None, true),
        );
    }

    /// The overlay lane's panels that start in the box's cells.
    fn box_panels(h: &TestHarness) -> Vec<PanelParts> {
        let area = Rect::new(13, 3, 94, 18);
        decode_stream(h.stoat.apc_scene.lane_bytes(OVERLAY_LANE))
            .iter()
            .filter_map(|command| match command {
                Command::Panel(panel) => Some((
                    Rect::new(panel.left, panel.top, panel.width, panel.height),
                    panel.style,
                    panel.fill,
                    panel.shadow,
                    panel.corner_radius,
                    panel.edges,
                    panel.above_pools,
                    panel.backdrop,
                )),
                _ => None,
            })
            .filter(|(cells, ..)| area.contains(cells.as_position()))
            .collect()
    }

    /// The glyph and the background of each cell the box's border crosses in
    /// `buf`: its top row, its side columns, and the row under it.
    fn border_cells(buf: &Buffer) -> Vec<(String, Color)> {
        let rows = [3, 21]
            .into_iter()
            .flat_map(|y| (13..107).map(move |x| (x, y)));
        let sides = (4..21).flat_map(|y| [(13, y), (106, y)]);
        rows.chain(sides)
            .map(|(x, y)| (buf[(x, y)].symbol().to_owned(), buf[(x, y)].bg))
            .collect()
    }

    /// A 120 by 24 window with the Commit Preview open on the second commit of
    /// a repository, with the preview's patch built.
    fn preview_open() -> TestHarness {
        let mut h = blamed_pane(TWO_COMMITS, "one\ntwo\n");
        h.type_keys("j enter");
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
}
