//! The Commit Preview's box, painted as the BlamePreview board draws it.
//!
//! The box is the boards' L modal. It takes the window less one column on each
//! side, from the first row under the tab bar to the window's last row, so it
//! covers the panes and the status rows and leaves the tab bar in view.
//!
//! Inside the box, a header row names the commit, with its meta and a
//! read-only chip at its right. One Island holds the patch, and the key line
//! lists the preview's keys. A terminal without the plate look draws the same
//! cells inside a bordered frame.

use crate::{
    blame,
    commit_preview::CommitPreview,
    render::{
        chrome::{self, ISLAND_BOTTOM, ISLAND_SIDE, ISLAND_TOP},
        commit_patch,
        commits::PreviewDials,
        paint,
        picker::{self, Footer, PlateHeader},
        text, TypeScale,
    },
    theme::{scope, Theme, TypeRole},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, StatefulWidget},
};
use std::mem;
use stoat_widgets::{
    panel::Panel,
    text_run::{self, TextRun},
    ApcScene,
};
use stoatty_protocol::command::{BorderStyle, Edge, PanelShadow};

/// The modal's name, which leads the header.
const LABEL: &str = "Commit";

/// The read-only chip's text.
const CHIP: &str = "preview \u{b7} read-only";

/// The word the patch's rows show while the patch builds.
const LOADING: &str = "loading\u{2026}";

/// Where the preview's parts go in a window.
pub(crate) struct PreviewLayout {
    /// The cells of the box, with its border rows and columns.
    modal: Rect,
    /// The rows inside the border.
    inner: Rect,
    /// The rows the patch scrolls in.
    pub(crate) body: Rect,
    /// The key line's rows, the inner rect's last row and the border row under
    /// it.
    footer: Rect,
}

/// The preview's layout in the window `full` under `tab_rows` rows of tab bar,
/// or `None` for a window with no row for the patch.
///
/// The header takes the inner rect's first two rows, and the key line takes
/// its last row, so the patch scrolls in the rows between them.
pub(crate) fn commit_preview_layout(full: Rect, tab_rows: u16) -> Option<PreviewLayout> {
    let modal = Rect::new(
        full.x + 1,
        full.y + tab_rows,
        full.width.saturating_sub(2),
        full.height.saturating_sub(tab_rows),
    );
    let inner = Block::default().borders(Borders::ALL).inner(modal);
    if inner.width == 0 || inner.height <= 3 {
        return None;
    }
    Some(PreviewLayout {
        modal,
        inner,
        body: Rect::new(inner.x, inner.y + 2, inner.width, inner.height - 3),
        footer: picker::footer_rect(modal),
    })
}

/// Paint the preview over the window `full`, under `tab_rows` rows of tab bar.
///
/// The paint clamps the preview's scroll to the patch's last page, since only
/// a built patch knows how many rows it has.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_commit_preview_modal(
    preview: &mut CommitPreview,
    now_epoch: i64,
    theme: &Theme,
    dials: PreviewDials,
    full: Rect,
    tab_rows: u16,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let Some(layout) = commit_preview_layout(full, tab_rows) else {
        return;
    };
    let style = theme.get(scope::UI_MODAL_PICKER);
    chrome::list_plate(buf, layout.modal, style, theme, scene);
    let plate = chrome::plate_colors(theme, scene);
    if let Some(plate) = plate {
        let edges = [ISLAND_TOP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE];
        chrome::modal_island(layout.body, edges, plate, theme, buf, scene);
    }

    let meta = meta_spans(preview, now_epoch, theme);
    let header = PlateHeader {
        label: LABEL,
        title: &preview.commit.info.summary,
        meta: &meta,
        chip: Some(CHIP),
    };
    picker::plate_header(buf, layout.inner, &header, theme, scene);
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
    picker::modal_footer(buf, layout.footer, &footer, plate, theme, scene);
}

/// The header's meta, which names the author, the date, the parent the patch
/// is against, and the change size once the build lands.
fn meta_spans(preview: &CommitPreview, now_epoch: i64, theme: &Theme) -> Vec<(String, Style)> {
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
    parts
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
    use super::commit_preview_layout;
    use crate::{
        action_handlers::dispatch,
        render::{
            chrome::{self, ISLAND_BOTTOM, ISLAND_SIDE, ISLAND_TOP},
            paint, OVERLAY_LANE,
        },
        test_harness::{CommitSpec, TestHarness},
        theme::{scope, PanelRole},
    };
    use ratatui::{layout::Rect, style::Color};
    use std::path::{Path, PathBuf};
    use stoat_action::BlamePreview;
    use stoat_config::TabBarMode;
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

    /// The box takes the window less one column on each side, from the first
    /// row under the tab bar to the window's last row. The patch takes the
    /// rows between the header's two rows and the key line, which takes the
    /// inner rect's last row and the border row under it. A window that leaves
    /// the patch no row has no layout.
    #[test]
    fn the_box_takes_the_window_less_a_column_each_side_under_the_tab_bar() {
        let layout = |width, height, tab_rows| {
            commit_preview_layout(Rect::new(0, 0, width, height), tab_rows)
                .map(|layout| (layout.modal, layout.body, layout.footer))
        };
        assert_eq!(
            [
                layout(160, 50, 2),
                layout(80, 24, 0),
                layout(80, 7, 1),
                layout(80, 6, 1),
            ],
            [
                Some((
                    Rect::new(1, 2, 158, 48),
                    Rect::new(2, 5, 156, 43),
                    Rect::new(2, 48, 156, 2),
                )),
                Some((
                    Rect::new(1, 0, 78, 24),
                    Rect::new(2, 3, 76, 19),
                    Rect::new(2, 22, 76, 2),
                )),
                Some((
                    Rect::new(1, 1, 78, 6),
                    Rect::new(2, 4, 76, 1),
                    Rect::new(2, 5, 76, 2),
                )),
                None,
            ],
        );
    }

    /// Without components the box is cells inside a frame, which starts on the
    /// window's first row while the tab bar hides. Its header row reads the
    /// label and the subject, and from the right the meta and the read-only
    /// chip, and a rule closes the header. The key line on the inner rect's
    /// last row reads every key, and the border row under it is blank between
    /// its corners.
    #[test]
    fn the_cell_box_reads_its_header_row_and_key_line() {
        let mut h = preview_open();
        h.stoat.stoatty = false;
        h.snapshot();

        let rows: Vec<String> = h
            .rendered_text()
            .lines()
            .map(|row| row.chars().skip(1).take(118).collect())
            .collect();
        let meta =
            "fake \u{b7} 2023/11/14 22:13 \u{b7} against parent c1 \u{b7} 1 file +1 \u{2212}0";
        let keys = "j/k scroll  ]/[ file  c check out  p blame parent  P other parent  \
                    y copy sha  Esc back to blame";
        assert_eq!(
            [&rows[0], &rows[1], &rows[2], &rows[22], &rows[23]],
            [
                &format!("\u{250c}{}\u{2510}", "\u{2500}".repeat(116)),
                &format!("\u{2502}Commit two{meta:>84}  preview \u{b7} read-only \u{2502}"),
                &format!("\u{2502}{}\u{2502}", "\u{2500}".repeat(116)),
                &format!("\u{2502} {keys:<115}\u{2502}"),
                &format!("\u{2514}{}\u{2518}", " ".repeat(116)),
            ],
        );
    }

    /// Under a live scene the box is the plate's two panels over the inner
    /// cells, a plain one and then a backdrop in the ground. One Island in the
    /// editor surface follows over the patch's rows. The read-only chip draws
    /// in its own box at the header row's right end, and a light square ring
    /// marks the origin row, the added line the preview opened from. The key
    /// line's row, the inner rect's last, blanks to the ground. The tab bar
    /// shows in two rows here, so the box starts on row 2.
    #[test]
    fn the_live_box_draws_a_plate_with_one_island() {
        let mut h = preview_open();
        h.stoat.stoatty_protocol = PROTOCOL_VERSION;
        h.stoat.settings.ui_tab_bar = Some(TabBarMode::Always);
        let key_line: Vec<Color> = {
            let buf = h.stoat.render();
            (2..118).map(|x| buf[(x, 22)].bg).collect()
        };

        let theme = &h.stoat.theme;
        let plate = chrome::plate_colors(theme, &h.stoat.apc_scene).expect("the plate look");
        let chip = Panel::chip(
            paint::style_rgb(theme.get(scope::UI_PREVIEW_CHIP).bg).expect("an RGB chip"),
        );
        let inner = Rect::new(2, 3, 116, 20);
        let [r, g, b] = plate.ground;
        let plate_edges = [
            Edge::Inset(0),
            Edge::Outset(11),
            Edge::Outset(10),
            Edge::Outset(11),
        ];
        assert_eq!(
            (box_panels(&h), key_line),
            (
                vec![
                    (
                        inner,
                        BorderStyle::None_,
                        None,
                        PanelShadow::None_,
                        8,
                        plate_edges,
                        false,
                        false,
                    ),
                    (
                        inner,
                        BorderStyle::None_,
                        Some(plate.ground),
                        theme.panel(PanelRole::Overlay).shadow,
                        8,
                        plate_edges,
                        false,
                        true,
                    ),
                    (
                        Rect::new(2, 5, 116, 17),
                        BorderStyle::None_,
                        Some(plate.surface),
                        PanelShadow::None_,
                        theme.panel(PanelRole::Island).corner_radius,
                        [ISLAND_TOP, ISLAND_SIDE, ISLAND_BOTTOM, ISLAND_SIDE],
                        false,
                        true,
                    ),
                    (
                        Rect::new(97, 3, 21, 1),
                        chip.style,
                        chip.fill,
                        chip.shadow,
                        chip.corner_radius,
                        Edge::symmetric(0, 0),
                        chip.above_pools,
                        chip.backdrop,
                    ),
                    (
                        Rect::new(2, 7, 116, 1),
                        BorderStyle::Light,
                        None,
                        PanelShadow::None_,
                        0,
                        Edge::symmetric(0, 0),
                        true,
                        false,
                    ),
                ],
                vec![Color::Rgb(r, g, b); 116],
            ),
        );
    }

    /// A root commit's meta says so where a commit names its parent, and a
    /// commit of two files counts them in the plural.
    #[test]
    fn a_root_commit_meta_reads_root_commit() {
        let mut h = blamed_pane(
            &[("c1", "one", &[("a.rs", "one\n"), ("b.rs", "b\n")])],
            "one\n",
        );
        h.stoat.stoatty = false;
        h.type_keys("enter");
        h.settle();
        h.snapshot();

        let header: String = h
            .rendered_text()
            .lines()
            .nth(1)
            .map(|row| row.chars().skip(2).take(116).collect())
            .unwrap_or_default();
        let meta = "fake \u{b7} 2023/11/14 22:13 \u{b7} root commit \u{b7} 2 files +2 \u{2212}0";
        assert_eq!(
            header,
            format!("Commit one{meta:>84}  preview \u{b7} read-only "),
        );
    }

    /// The patch's rows read the loading word, centered, until the build
    /// lands.
    #[test]
    fn the_patch_reads_loading_until_its_build_lands() {
        let mut h = blamed_pane(TWO_COMMITS, "one\ntwo\n");
        h.type_keys("j");
        dispatch(&mut h.stoat, &BlamePreview);

        let buf = h.stoat.render();
        let row: String = (2..118).map(|x| buf[(x, 12)].symbol()).collect();
        assert_eq!(row, format!("{:^116}", "loading\u{2026}"));
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
            .nth(3)
            .map(|row| row.chars().skip(2).take(116).collect())
            .unwrap_or_default();
        assert_eq!(row, format!("{:<116}", "   1   1 \u{2502}one"));
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
            .nth(5)
            .map(|row| row.chars().skip(2).take(116).collect())
            .unwrap_or_default();
        assert_eq!(
            row,
            format!("{:<107}\u{25c2} a.rs:2 ", "\u{258e}      2 \u{2502}two"),
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

    /// The overlay lane's panels that start in the box's inner cells, under
    /// the two rows of a tab bar that shows under a live scene.
    ///
    /// The strip band's Island beside the panes starts on the box's top row,
    /// so the inner cells leave it out.
    fn box_panels(h: &TestHarness) -> Vec<PanelParts> {
        let area = Rect::new(2, 3, 116, 20);
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
