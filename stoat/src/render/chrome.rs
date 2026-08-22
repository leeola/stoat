use super::TEXT_SCALE_FULL;
use crate::{render::paint::style_rgb, theme::Theme};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, StatefulWidget, Widget},
};
use stoat_widgets::{bar::Bar, panel::Panel, text_run::TextRun, ApcScene};
use stoatty_protocol::command::{BorderStyle, PanelShadow};

/// Cells left bare between a modal box and the edge of the area it floats over,
/// split evenly across the two opposing edges.
const MODAL_MARGIN: u16 = 4;

/// The same margin at its thinnest, which is what a zoomed-out-to-nothing or
/// zoomed-all-the-way-up box is allowed to shrink the surround to.
const MODAL_MARGIN_MIN: u16 = 2;

/// Denominator of one zoom step. Each step moves a dimension by this fraction of
/// the area, so a step feels the same size on a small screen and a large one.
const ZOOM_STEP_DIVISOR: u16 = 10;

/// Size and center a modal box over `area` for content of `content` cells,
/// returning `None` when `area` is too small to host `min`.
///
/// This is the sizing rule behind every content-sized modal. Per dimension the
/// box is `content` bounded below by `recommended` and above by `area` less
/// [`MODAL_MARGIN`]. Small content therefore keeps the recommended size a
/// fixed-size modal always had, and only content that outgrows it expands. A
/// dimension of [`u16::MAX`] is how a data-heavy modal asks for the largest box
/// the area allows without having to measure anything.
///
/// `zoom` then moves each dimension by that many tenths of the area, positive to
/// grow, and the result is clamped to between `min` and `area` less
/// [`MODAL_MARGIN_MIN`]. The clamp is what bounds the caller's zoom steps, so a
/// caller need not pre-limit them.
///
/// `content` is expected to be measured once when the modal opens rather than
/// per frame. The box's position is derived from its size, so re-measuring
/// against a filtered list would move the box while the user types into it.
///
/// See also:
/// - [`modal_frame`] to draw the returned rect's border and title.
pub(crate) fn modal_box(
    area: Rect,
    content: (u16, u16),
    recommended: (u16, u16),
    min: (u16, u16),
    zoom: i8,
) -> Option<Rect> {
    let ceiling = (
        area.width.saturating_sub(MODAL_MARGIN_MIN),
        area.height.saturating_sub(MODAL_MARGIN_MIN),
    );
    if ceiling.0 < min.0 || ceiling.1 < min.1 {
        return None;
    }

    let width = zoomed(area.width, content.0, recommended.0, min.0, ceiling.0, zoom);
    let height = zoomed(
        area.height,
        content.1,
        recommended.1,
        min.1,
        ceiling.1,
        zoom,
    );

    Some(Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    ))
}

/// Size one dimension of a [`modal_box`], fitting `content` between
/// `recommended` and the margin, applying `zoom` steps, then clamping to
/// `min..=ceiling`.
///
/// The zoom arithmetic runs in [`i32`] because a negative step can carry the
/// dimension below zero before the clamp pulls it back up to `min`.
fn zoomed(area: u16, content: u16, recommended: u16, min: u16, ceiling: u16, zoom: i8) -> u16 {
    let base = content
        .max(recommended)
        .min(area.saturating_sub(MODAL_MARGIN));
    let step = i32::from(area / ZOOM_STEP_DIVISOR) * i32::from(zoom);

    (i32::from(base) + step).clamp(i32::from(min), i32::from(ceiling)) as u16
}

/// Draw a modal frame around `area` and return the inner content rect.
///
/// This is the single chrome primitive behind every stoat modal and cursor
/// popup. The fallback -- taken when `scene` is dead, or when `style`'s
/// foreground does not resolve to RGB -- draws a ratatui [`Block`] with [`Borders::ALL`], `style`
/// on the border, and `title` styled the same, which is exactly what the sites drew
/// before, so their snapshots stay identical.
///
/// When the border colour resolves to RGB it instead emits a hairline `panel` APC frame with
/// rounded corners and a drop shadow, plus, for `Some(title)`, a full-size title [`TextRun`] over
/// the top edge. The hairline runs unbroken through the title span, and the title run
/// carries no background box, so its glyphs blend directly over the grid cells
/// behind them. No box-drawing glyphs are written in this arm.
///
/// The caller owns background clearing, and a titled caller must clear the
/// border-row cells to the surface color so the title glyphs blend over a clean
/// surface rather than stale content. Sites that masked what was behind the
/// modal call [`Clear`](ratatui::widgets::Clear) before this; sites that paint
/// every cell themselves clear implicitly. This draws only the frame.
///
/// The returned rect is the area inset by the one-cell border, matching the
/// layout the sites lay their content out over regardless of arm.
pub(crate) fn modal_frame(
    buf: &mut Buffer,
    area: Rect,
    title: Option<&str>,
    style: Style,
    theme: &Theme,
    scene: &mut ApcScene,
) -> Rect {
    modal_frame_inner(buf, area, title, style, theme, scene, false)
}

/// Draw a modal frame that survives a pool glide, otherwise identical to
/// [`modal_frame`].
///
/// A pool composite paints after the main pass, scissored to its region, so
/// chrome layered with the grid inside that region is covered while the pool
/// glides and reappears when it settles. This variant marks its panel as floating
/// above every pooled surface, which punches the box's rect out of those
/// composites and leaves the main-pass result showing through.
///
/// Only chrome drawn *over* a pooled surface needs it. A box the pool is content
/// of should keep [`modal_frame`], or it would float above the very surface it
/// belongs to.
///
/// The flag rides the APC frame alone. The box's hline and text runs need nothing,
/// because the punched hole exposes the main-pass cells that already carry them.
pub(crate) fn modal_frame_above_pools(
    buf: &mut Buffer,
    area: Rect,
    title: Option<&str>,
    style: Style,
    theme: &Theme,
    scene: &mut ApcScene,
) -> Rect {
    modal_frame_inner(buf, area, title, style, theme, scene, true)
}

/// Draw a modal frame that rides a gliding pool, otherwise identical to
/// [`modal_frame`].
///
/// `anchor` names the host pool and the document top row this layout assumed,
/// so the terminal carries the frame along with the text beneath it instead of
/// leaving it parked until the scroll settles. `fill` paints the interior
/// opaque, which an anchored frame needs: the shifted draw exposes cells the
/// base pass wrote for a different scroll, and an unfilled box shows them.
///
/// Falls back to [`modal_frame`] when there is no anchor, or when the scene is
/// dead and only the degraded cell border is drawn.
pub(crate) fn modal_frame_anchored(
    buf: &mut Buffer,
    area: Rect,
    style: Style,
    theme: &Theme,
    scene: &mut ApcScene,
    anchor: Option<(u32, f32)>,
    fill: Option<[u8; 3]>,
) -> Rect {
    let Some(anchor) = anchor.filter(|_| scene.live()) else {
        return modal_frame(buf, area, None, style, theme, scene);
    };
    let Some(border) = style_rgb(style.fg) else {
        return modal_frame(buf, area, None, style, theme, scene);
    };

    Panel {
        style: BorderStyle::Rounded,
        border,
        corner_radius: 6,
        fill,
        shadow: PanelShadow::Drop,
        inset_x: 0,
        above_pools: true,
        anchor: Some(anchor),
    }
    .draw_components(area, scene);

    Block::default().borders(Borders::ALL).inner(area)
}

#[allow(clippy::too_many_arguments)]
fn modal_frame_inner(
    buf: &mut Buffer,
    area: Rect,
    title: Option<&str>,
    style: Style,
    _theme: &Theme,
    scene: &mut ApcScene,
    above_pools: bool,
) -> Rect {
    let inner = Block::default().borders(Borders::ALL).inner(area);

    match style_rgb(style.fg).filter(|_| scene.live()) {
        Some(border) => {
            Panel {
                style: BorderStyle::Rounded,
                border,
                corner_radius: 6,
                fill: None,
                shadow: PanelShadow::Drop,
                inset_x: 0,
                above_pools,
                anchor: None,
            }
            .draw_components(area, scene);
            if let Some(title) = title {
                TextRun {
                    col: 16,
                    row: 0,
                    scale: TEXT_SCALE_FULL,
                    color: border,
                    bg: None,
                    text: title,
                }
                .render(area, buf, scene);
            }
        },
        None => {
            let mut block = Block::default().borders(Borders::ALL).border_style(style);
            if let Some(title) = title {
                block = block.title(title.to_string()).title_style(style);
            }
            block.render(area, buf);
        },
    }

    inner
}

/// Device pixels shaved off each horizontal edge of a popout card's panel, so the
/// card draws a touch narrower than its cell rect and the editor background shows
/// in the thin strip beside it.
const POPOUT_INSET_PX: u8 = 4;

/// Draw a popout card frame around `area`.
///
/// The frame is a filled, square-cornered, drop-shadowed panel inset a few pixels
/// from its cell rect. This draws only the frame, so the caller owns the interior.
///
/// The rich arm -- taken when `scene` is live and both `bg` and `border` resolve to RGB -- emits a
/// `panel` APC frame with `fill` set to `bg`, a [`POPOUT_INSET_PX`] horizontal inset, a drop
/// shadow, and a square light hairline in `border`. Square corners match the status bar the card
/// extends, so the card reads as part of it. The inset and shadow are what make the card read
/// as tucked behind the bar, and a plain terminal cannot draw them.
///
/// The fallback draws a ratatui [`Block`] with [`Borders::ALL`] in `border`. It
/// draws only when `area` spans at least two rows, because a one-row card has no
/// room for a box border and degrades to the bare background cells the caller
/// already painted.
pub(crate) fn popout_frame(
    buf: &mut Buffer,
    area: Rect,
    bg: Color,
    border: Color,
    _theme: &Theme,
    scene: &mut ApcScene,
) {
    match style_rgb(Some(bg))
        .zip(style_rgb(Some(border)))
        .filter(|_| scene.live())
    {
        Some((bg, border)) => {
            Panel {
                style: BorderStyle::Light,
                border,
                corner_radius: 0,
                fill: Some(bg),
                shadow: PanelShadow::Overhang,
                inset_x: POPOUT_INSET_PX,
                above_pools: false,
                anchor: None,
            }
            .draw_components(area, scene);
        },
        None => {
            if area.height >= 2 {
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(border))
                    .render(area, buf);
            }
        },
    }
}

/// Draw a horizontal separator across `width` cells at row `y`, starting at
/// column `x`.
///
/// The fallback -- taken when `scene` is dead, or when `style`'s foreground
/// does not resolve to RGB -- writes `─` glyphs styled with `style`, as the separator sites did
/// before. Otherwise it emits one hairline [`Bar`] a sixteenth of a cell thick
/// centered in the row, and writes no glyphs.
pub(crate) fn hline(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    width: u16,
    style: Style,
    scene: &mut ApcScene,
) {
    match style_rgb(style.fg).filter(|_| scene.live()) {
        Some(color) => {
            Bar {
                x: 0,
                y: 8,
                width: width.saturating_mul(16),
                height: 1,
                color,
            }
            .render(Rect::new(x, y, width, 1), buf, scene);
        },
        None => {
            for col in x..x + width {
                buf[(col, y)].set_char('─').set_style(style);
            }
        },
    }
}

/// Draw a vertical separator down `height` cells at column `x`, starting at row
/// `y`.
///
/// The fallback -- taken when `scene` is dead, or when `style`'s foreground
/// does not resolve to RGB -- writes `│` glyphs styled with `style`, as the separator sites did
/// before. Otherwise it emits one hairline [`Bar`] a sixteenth of a cell thick
/// centered in the column, and writes no glyphs.
pub(crate) fn vline(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    height: u16,
    style: Style,
    scene: &mut ApcScene,
) {
    match style_rgb(style.fg).filter(|_| scene.live()) {
        Some(color) => {
            Bar {
                x: 8,
                y: 0,
                width: 1,
                height: height.saturating_mul(16),
                color,
            }
            .render(Rect::new(x, y, 1, height), buf, scene);
        },
        None => {
            for row in y..y + height {
                buf[(x, row)].set_char('│').set_style(style);
            }
        },
    }
}

/// Draw `content` at cell `(x, y)`, clipped before column `end_x`.
///
/// The fallback -- taken when `scene` is dead, `style`'s foreground does not
/// resolve to RGB, or `bg` is `None` -- writes glyphs cell-by-cell styled with `style`, stopping
/// before `end_x`, exactly as the text sites did before. Otherwise it emits one [`TextRun`] at
/// `scale` (256ths of a cell) anchored at the cell, with `bg` as its background box and no grid
/// glyphs.
///
/// `bg` is the run's own background. The renderer paints it as one opaque box
/// behind the alpha-blended glyphs, so it need not match the surface beneath.
#[allow(clippy::too_many_arguments)]
pub(crate) fn text(
    buf: &mut Buffer,
    x: u16,
    y: u16,
    end_x: u16,
    content: &str,
    style: Style,
    bg: Option<[u8; 3]>,
    scale: u16,
    scene: &mut ApcScene,
) {
    match (style_rgb(style.fg).filter(|_| scene.live()), bg) {
        (Some(color), Some(bg)) => {
            TextRun {
                col: 0,
                row: 0,
                scale,
                color,
                bg: Some(bg),
                text: content,
            }
            .render(Rect::new(x, y, 1, 1), buf, scene);
        },
        _ => {
            for (j, ch) in content.chars().enumerate() {
                let col = x + j as u16;
                if col >= end_x {
                    break;
                }
                buf[(col, y)].set_char(ch).set_style(style);
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{
        hline, modal_box, modal_frame, modal_frame_above_pools, popout_frame, text, vline,
        POPOUT_INSET_PX,
    };
    use crate::theme::Theme;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Style},
    };
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{
        encode_bar, encode_panel, encode_text_run, BarCommand, BorderStyle, PanelCommand,
        PanelShadow, TextRunCommand,
    };

    fn rgb_style() -> Style {
        Style::default().fg(Color::Rgb(1, 2, 3))
    }

    /// A style whose foreground does not resolve to RGB, which is what drops
    /// these helpers to their cell fallback now that a scene is always present.
    fn plain_style() -> Style {
        Style::default().fg(Color::Reset)
    }

    /// Recommended and minimum sizes roughly matching the file finder's, over an
    /// area sized so one zoom step is a round 20 columns by 6 rows.
    const RECOMMENDED: (u16, u16) = (120, 32);
    const MIN: (u16, u16) = (40, 12);

    fn sized(content: (u16, u16), zoom: i8) -> Option<Rect> {
        modal_box(Rect::new(0, 0, 200, 60), content, RECOMMENDED, MIN, zoom)
    }

    #[test]
    fn small_content_keeps_the_recommended_box_centered() {
        assert_eq!(sized((10, 5), 0), Some(Rect::new(40, 14, 120, 32)));
        assert_eq!(
            modal_box(Rect::new(10, 5, 200, 60), (10, 5), RECOMMENDED, MIN, 0),
            Some(Rect::new(50, 19, 120, 32)),
            "centered within the area's own origin"
        );
    }

    #[test]
    fn content_past_the_recommended_size_expands_to_the_margin() {
        let expanded = Some(Rect::new(2, 2, 196, 56));
        assert_eq!(sized((400, 100), 0), expanded);
        assert_eq!(
            sized((u16::MAX, u16::MAX), 0),
            expanded,
            "u16::MAX asks for the largest box the area allows"
        );
    }

    #[test]
    fn zoom_steps_the_box_by_a_tenth_of_the_area() {
        assert_eq!(sized((10, 5), 1), Some(Rect::new(30, 11, 140, 38)));
        assert_eq!(sized((10, 5), -1), Some(Rect::new(50, 17, 100, 26)));
    }

    #[test]
    fn zoom_clamps_between_the_minimum_and_the_thin_margin() {
        assert_eq!(sized((10, 5), 8), Some(Rect::new(1, 1, 198, 58)));
        assert_eq!(
            sized((10, 5), -8),
            Some(Rect::new(80, 24, 40, 12)),
            "a step past zero clamps up to the minimum rather than underflowing"
        );
    }

    #[test]
    fn an_area_too_small_for_the_minimum_has_no_box() {
        assert_eq!(
            modal_box(Rect::new(0, 0, 41, 60), (10, 5), RECOMMENDED, MIN, 0),
            None
        );
        assert_eq!(
            modal_box(Rect::new(0, 0, 200, 13), (10, 5), RECOMMENDED, MIN, 0),
            None
        );
        assert_eq!(
            modal_box(Rect::new(0, 0, 42, 14), (10, 5), RECOMMENDED, MIN, 0),
            Some(Rect::new(1, 1, 40, 12)),
            "the smallest hostable area yields the minimum box"
        );
    }

    #[test]
    fn fallback_draws_a_box_border_and_returns_the_inner_rect() {
        let area = Rect::new(0, 0, 8, 4);
        let mut buf = Buffer::empty(area);
        let theme = Theme::empty();

        let mut scene = ApcScene::new();

        let inner = modal_frame(
            &mut buf,
            area,
            Some(" hi "),
            plain_style(),
            &theme,
            &mut scene,
        );

        assert_eq!(inner, Rect::new(1, 1, 6, 2));
        assert_eq!(buf.cell((0, 0)).unwrap().symbol(), "┌");
        assert_eq!(buf.cell((7, 3)).unwrap().symbol(), "┘");
        // The title glyphs land on the top border.
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), " ");
        assert_eq!(buf.cell((2, 0)).unwrap().symbol(), "h");
    }

    #[test]
    fn stoatty_arm_emits_a_panel_and_no_border_glyphs() {
        let area = Rect::new(2, 1, 8, 4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 6));
        let mut scene = ApcScene::new();
        let theme = Theme::empty();
        // The rich arm engages purely on the border color (style.fg) being RGB.
        let style = rgb_style();

        let inner = modal_frame(&mut buf, area, Some(" hi "), style, &theme, &mut scene);

        assert_eq!(inner, Rect::new(3, 2, 6, 2));
        // No box-drawing glyph is painted. The panel is off-grid.
        assert_eq!(buf.cell((2, 1)).unwrap().symbol(), " ");

        let panel = encode_panel(&PanelCommand {
            top: 1,
            left: 2,
            width: 8,
            height: 4,
            style: BorderStyle::Rounded,
            border: [1, 2, 3],
            corner_radius: 6,
            fill: None,
            shadow: PanelShadow::Drop,
            inset_x: 0,
            above_pools: false,
            anchor: None,
        });
        // The title run carries no background box and anchors one cell into the
        // modal (area.x * 16 + 16 = 48, area.y * 16 = 16), so the hairline draws
        // unbroken and the glyphs blend over the caller-cleared cells.
        let title = encode_text_run(&TextRunCommand {
            col: 48,
            row: 16,
            scale: 256,
            color: [1, 2, 3],
            bg: None,
            follow: 0,
            anchor: None,
            text: " hi ".to_owned(),
        });
        assert_eq!(scene.buffer(), &[panel, title].concat());
    }

    /// Chrome drawn over a pooled surface has to say so, or the pool composite
    /// paints over it for the length of every glide.
    #[test]
    fn the_above_pools_variant_flags_only_its_panel() {
        let area = Rect::new(2, 1, 8, 4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 6));
        let mut scene = ApcScene::new();
        let theme = Theme::empty();

        let inner = modal_frame_above_pools(&mut buf, area, None, rgb_style(), &theme, &mut scene);

        assert_eq!(inner, Rect::new(3, 2, 6, 2), "same layout as modal_frame");
        assert_eq!(
            scene.buffer(),
            &encode_panel(&PanelCommand {
                top: 1,
                left: 2,
                width: 8,
                height: 4,
                style: BorderStyle::Rounded,
                border: [1, 2, 3],
                corner_radius: 6,
                fill: None,
                shadow: PanelShadow::Drop,
                inset_x: 0,
                above_pools: true,
                anchor: None,
            }),
            "the flag is the only difference from modal_frame's frame"
        );
    }

    /// Every other modal is content a pool may own, so the plain entry point must
    /// not opt them in.
    #[test]
    fn modal_frame_leaves_its_panel_layered_with_the_grid() {
        let area = Rect::new(2, 1, 8, 4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 6));
        let mut scene = ApcScene::new();
        let theme = Theme::empty();

        modal_frame(&mut buf, area, None, rgb_style(), &theme, &mut scene);

        assert_eq!(
            scene.buffer(),
            &encode_panel(&PanelCommand {
                top: 1,
                left: 2,
                width: 8,
                height: 4,
                style: BorderStyle::Rounded,
                border: [1, 2, 3],
                corner_radius: 6,
                fill: None,
                shadow: PanelShadow::Drop,
                inset_x: 0,
                above_pools: false,
                anchor: None,
            })
        );
    }

    #[test]
    fn popout_arm_emits_a_square_cornered_panel() {
        let area = Rect::new(2, 1, 8, 4);
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 6));
        let mut scene = ApcScene::new();
        let theme = Theme::empty();

        popout_frame(
            &mut buf,
            area,
            Color::Rgb(4, 5, 6),
            Color::Rgb(1, 2, 3),
            &theme,
            &mut scene,
        );

        // No box-drawing glyph is painted. The panel is off-grid.
        assert_eq!(buf.cell((2, 1)).unwrap().symbol(), " ");

        // Square corners and a light hairline match the status bar the card
        // extends, filled with the card background and inset so it tucks behind
        // the bar.
        assert_eq!(
            scene.buffer(),
            &encode_panel(&PanelCommand {
                top: 1,
                left: 2,
                width: 8,
                height: 4,
                style: BorderStyle::Light,
                border: [1, 2, 3],
                corner_radius: 0,
                fill: Some([4, 5, 6]),
                shadow: PanelShadow::Overhang,
                inset_x: POPOUT_INSET_PX,
                above_pools: false,
                anchor: None,
            }),
        );
    }

    /// An RGB theme says nothing about whether the host can draw a hairline. A
    /// foreign terminal has to get the glyph forms, which is what a dead scene
    /// selects even though every color here resolves.
    #[test]
    fn a_dead_scene_takes_every_cell_arm_despite_rgb_colors() {
        let area = Rect::new(0, 0, 10, 6);
        let mut buf = Buffer::empty(area);
        let mut scene = ApcScene::new();
        scene.set_live(false);
        let theme = Theme::empty();

        let inner = modal_frame(
            &mut buf,
            Rect::new(0, 0, 6, 3),
            Some(" hi "),
            rgb_style(),
            &theme,
            &mut scene,
        );
        assert_eq!(inner, Rect::new(1, 1, 4, 1));
        assert_eq!(buf.cell((0, 0)).expect("in bounds").symbol(), "┌");

        popout_frame(
            &mut buf,
            Rect::new(0, 3, 6, 2),
            Color::Rgb(4, 5, 6),
            Color::Rgb(1, 2, 3),
            &theme,
            &mut scene,
        );
        assert_eq!(buf.cell((0, 3)).expect("in bounds").symbol(), "┌");

        hline(&mut buf, 6, 0, 3, rgb_style(), &mut scene);
        assert_eq!(buf.cell((6, 0)).expect("in bounds").symbol(), "─");

        vline(&mut buf, 9, 1, 3, rgb_style(), &mut scene);
        assert_eq!(buf.cell((9, 1)).expect("in bounds").symbol(), "│");

        text(
            &mut buf,
            6,
            5,
            10,
            "ab",
            rgb_style(),
            Some([9, 9, 9]),
            218,
            &mut scene,
        );
        assert_eq!(buf.cell((6, 5)).expect("in bounds").symbol(), "a");

        assert!(
            scene.bytes().is_empty(),
            "and no component frame is built for a host that cannot draw one"
        );
    }

    #[test]
    fn hline_fallback_draws_dashes_and_stoatty_emits_a_centered_bar() {
        let mut fallback = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut fallback_scene = ApcScene::new();
        hline(&mut fallback, 2, 3, 4, plain_style(), &mut fallback_scene);
        assert_eq!(fallback.cell((2, 3)).unwrap().symbol(), "─");
        assert_eq!(fallback.cell((5, 3)).unwrap().symbol(), "─");
        assert_eq!(fallback.cell((6, 3)).unwrap().symbol(), " ");

        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut scene = ApcScene::new();
        hline(&mut buf, 2, 3, 4, rgb_style(), &mut scene);
        assert_eq!(buf.cell((2, 3)).unwrap().symbol(), " ");
        assert_eq!(
            scene.buffer(),
            &encode_bar(&BarCommand {
                x: 32,
                y: 56,
                width: 64,
                height: 1,
                color: [1, 2, 3],
            })
        );
    }

    #[test]
    fn vline_fallback_draws_bars_and_stoatty_emits_a_centered_bar() {
        let mut fallback = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut fallback_scene = ApcScene::new();
        vline(&mut fallback, 2, 1, 3, plain_style(), &mut fallback_scene);
        assert_eq!(fallback.cell((2, 1)).unwrap().symbol(), "│");
        assert_eq!(fallback.cell((2, 3)).unwrap().symbol(), "│");
        assert_eq!(fallback.cell((2, 0)).unwrap().symbol(), " ");

        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 4));
        let mut scene = ApcScene::new();
        vline(&mut buf, 2, 1, 3, rgb_style(), &mut scene);
        assert_eq!(buf.cell((2, 1)).unwrap().symbol(), " ");
        assert_eq!(
            scene.buffer(),
            &encode_bar(&BarCommand {
                x: 40,
                y: 16,
                width: 1,
                height: 48,
                color: [1, 2, 3],
            })
        );
    }

    #[test]
    fn text_fallback_writes_clipped_glyphs_and_stoatty_emits_a_scaled_run() {
        let mut fallback = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut fallback_scene = ApcScene::new();
        text(
            &mut fallback,
            1,
            0,
            5,
            "hello",
            plain_style(),
            Some([9, 9, 9]),
            218,
            &mut fallback_scene,
        );
        assert_eq!(fallback.cell((1, 0)).unwrap().symbol(), "h");
        assert_eq!(fallback.cell((4, 0)).unwrap().symbol(), "l");
        // The 'o' would land on column 5, which is clipped at end_x.
        assert_eq!(fallback.cell((5, 0)).unwrap().symbol(), " ");

        let mut buf = Buffer::empty(Rect::new(0, 0, 8, 2));
        let mut scene = ApcScene::new();
        text(
            &mut buf,
            1,
            0,
            5,
            "hi",
            rgb_style(),
            Some([9, 9, 9]),
            218,
            &mut scene,
        );
        assert_eq!(buf.cell((1, 0)).unwrap().symbol(), " ");
        assert_eq!(
            scene.buffer(),
            &encode_text_run(&TextRunCommand {
                col: 16,
                row: 0,
                scale: 218,
                color: [1, 2, 3],
                bg: Some([9, 9, 9]),
                follow: 0,
                anchor: None,
                text: "hi".to_owned(),
            })
        );
    }
}
