use crate::{
    action_handlers::search::StatusPrompt,
    app::SPINNER_FRAMES,
    buffer::BufferId,
    buffer_registry::BufferRegistry,
    diff_changes::{DiffChangesId, DiffChangesState},
    diff_map::MarkKind,
    editor_state::{EditorId, EditorState},
    pane::{Divider, DividerOrientation, Pane, Placement, View},
    render::{
        chrome, commits, content_lane, diff_changes,
        editor::{editor_cursor_position, render_editor_with_overlay, split_minimap_strip},
        layout::split_pane_status,
        paint::{dim_rgb, style_rgb},
        popout::{
            paint_popout_card, popout_area, popout_card_bg, popout_inset, scaled_char_capacity,
            wrap_popout_lines,
        },
        review,
        run_pane::render_run_pane,
        status_lane,
        term_pane::render_term_pane,
        undercurl::UndercurlBatch,
        FrameCtx, MinimapChrome, PaneCtx,
    },
    term_session::{TermId, TermSession},
    theme::{PanelRole, TypeRole, FULL_CELL_SCALE},
    workspace::{
        diff::{self, DiffBase, HunkPosition},
        Workspace,
    },
};
use lsp_types::DiagnosticSeverity;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Text},
    widgets::{Paragraph, StatefulWidget, Widget},
};
use slotmap::SlotMap;
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    ops::Range,
    path::Path,
};
use stoat_widgets::{
    minimap::Minimap,
    panel::Panel,
    status_bar::{StatusBar, StatusSegment},
    text_run, ApcScene,
};
use stoatty_protocol::command::BorderStyle;

/// Buffer lines the minimap strip draws per vertical cell.
pub(super) const MINIMAP_LINES_PER_CELL: u8 = 8;

/// Widest line, in minimap columns, a strip renders before clipping.
pub(super) const MINIMAP_MAX_COLUMNS: u8 = 120;

/// How far a diff half's fill moves from the status bar's toward the side's
/// status color, for a theme that sets no `diff.sides` fill.
const SIDE_FILL_TINT: f32 = 0.12;

/// How far a diff half's text moves from the bar's toward the status color.
const SIDE_TEXT_TINT: f32 = 0.15;

/// How far a side's name lifts from its status color toward white.
const SIDE_NAME_LIFT: f32 = 0.4;

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_pane(
    pane: &Pane,
    is_focused: bool,
    ctx: PaneCtx<'_>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    undercurls: &mut UndercurlBatch,
    status: &StatusRow,
) {
    let theme = frame.theme;
    let text_style = theme.get(crate::theme::scope::UI_TEXT);
    let (content_area, status_area) = pane_areas(pane.area, frame.minimap_band);

    let PaneCtx {
        editors,
        buffers,
        runs,
        terms,
        term_pool_grids,
        commit_lists,
        diff_changes_views,
    } = ctx;

    // The frame blanks to the window ground, and a view paints only the cells
    // it has content for, so the content area clears to the editor surface
    // first.
    crate::render::clear_surface(content_area, buf, theme);
    if frame.islands && scene.live() {
        render_island(content_area, frame, scene);
    }

    match &pane.view {
        View::Image { path, px } => {
            render_image_pane(
                content_area,
                buf,
                path,
                *px,
                text_style,
                frame.images_capable,
            );
        },
        View::Label(label) => {
            Paragraph::new(Text::styled(label.clone(), text_style))
                .centered()
                .render(content_area, buf);
        },
        View::Editor(editor_id) => {
            if let Some(editor) = editors.get_mut(*editor_id) {
                let labels = if is_focused {
                    frame.goto_word_labels
                } else {
                    None
                };
                let diagnostic_info = buffers
                    .path_for(editor.buffer_id)
                    .map(|path| (path, frame.diagnostics));
                render_editor_with_overlay(
                    editor,
                    content_area,
                    text_style,
                    theme,
                    frame.chrome,
                    buf,
                    is_focused,
                    frame.minimap_enabled,
                    frame.line_numbers,
                    frame.mode == "insert",
                    if is_focused { frame.hover_cell } else { None },
                    labels,
                    diagnostic_info,
                    // The editor's rich surfaces -- gutter, minimap strip, review
                    // decorations -- fork on having a scene at all, so a dead one
                    // is withheld and each takes the cell arm it already carries.
                    match scene.live() {
                        true => Some(&mut *scene),
                        false => None,
                    },
                    frame.terminal_cursor_editor == Some(*editor_id),
                    Some(undercurls),
                    frame.pane_dim(is_focused),
                    frame.diff_dials,
                    frame.wrap_mode,
                    frame.wrap_column,
                    frame.scrolloff,
                    frame.reload_flashes,
                    false,
                    None,
                );

                if let (Some(strip), Some(chrome)) = (editor.minimap_rect, frame.minimap_chrome)
                    && let Some(content) = chrome.content.get(&(chrome.workspace, editor.buffer_id))
                {
                    pane_minimap(
                        pane,
                        chrome,
                        content.content_id(),
                        editor.diff_view,
                        is_focused,
                        frame,
                    )
                    .render(strip, buf, scene);
                }
            }
        },
        View::Run(run_id) => {
            if let Some(run_state) = runs.get(*run_id) {
                render_run_pane(
                    run_state,
                    editors,
                    theme,
                    frame.chrome,
                    frame.home,
                    content_area,
                    is_focused,
                    buf,
                );
            }
        },
        View::Agent(term_id) | View::Terminal(term_id) => {
            // stoatty draws a split pane's terminal as a pool at the terminal
            // font size once it reports the grid, which covers this rectangle.
            // The live grid stays blank, so a change crosses the wire once, in
            // the page.
            let pooled =
                pane.placement == Placement::Split && term_pool_grids.contains_key(term_id);
            if !pooled && let Some(term) = terms.get(*term_id) {
                render_term_pane(term, theme, content_area, is_focused, buf);
            }
        },
        // The list paints a status row of its own, so the pane status and the
        // LSP popout above it stay off this view.
        View::Commits(id) => {
            if let Some(state) = commit_lists.get_mut(*id) {
                commits::render_commits(pane, is_focused, state, frame, buf, &mut *scene);
            }
            dim_pane(split_pane_status(pane.area).0, is_focused, frame, buf);
            return;
        },
        // The changes view paints the pane status row through its focused
        // excerpt. The row rides the status lane, so a replay of the content
        // keeps it.
        View::DiffChanges(id) => {
            if let Some(state) = diff_changes_views.get_mut(*id) {
                diff_changes::render_diff_changes(
                    pane,
                    is_focused,
                    state,
                    editors,
                    buffers,
                    text_style,
                    undercurls,
                    frame,
                    buf,
                    &mut *scene,
                );
                if let (Some(strip), Some(chrome)) = (state.minimap_rect, frame.minimap_chrome)
                    && let Some(content_id) = chrome.diff_changes_content
                {
                    pane_minimap(pane, chrome, content_id, true, is_focused, frame)
                        .render(strip, buf, scene);
                }
            }
            dim_pane(split_pane_status(pane.area).0, is_focused, frame, buf);
            render_pane_status(pane, frame, editors, status, buf, scene);
            return;
        },
    }

    dim_pane(content_area, is_focused, frame, buf);

    render_pane_status(pane, frame, editors, status, buf, scene);
    scene.set_lane(content_lane(pane.index));

    let status_rows: u16 = if is_focused && frame.lsp_status_open {
        let mut rows: Vec<String> = frame
            .lsp_progress_entries
            .iter()
            .map(|entry| {
                format!(
                    " {} {} {}",
                    SPINNER_FRAMES[frame.spinner_phase as usize],
                    lsp_short_name(&entry.server),
                    lsp_progress_label(entry).trim()
                )
            })
            .collect();
        if rows.is_empty() {
            rows = frame
                .lsp_servers
                .iter()
                .map(|server| format!(" {} idle ", server.short))
                .collect();
        }
        rows.truncate(6);

        if !rows.is_empty()
            && let Some(area) = popout_area(status_area, content_area, rows.len() as u16, 0)
        {
            let bg = popout_card_bg(theme);
            let border = theme
                .get(crate::theme::scope::UI_BORDER_INACTIVE)
                .fg
                .unwrap_or_else(|| crate::render::themed_fg(theme));
            let content = paint_popout_card(buf, area, bg, border, theme, &mut *scene);

            let scale = theme.type_scale(TypeRole::Ui);
            let cap = scaled_char_capacity(content.width as usize, scale);
            let style = theme
                .get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
                .add_modifier(Modifier::ITALIC);
            for (i, row) in rows.iter().enumerate() {
                let text: String = row.chars().take(cap).collect();
                chrome::text(
                    buf,
                    content.x,
                    content.y + i as u16,
                    content.x + content.width,
                    &text,
                    style,
                    style_rgb(Some(bg)),
                    scale,
                    &mut *scene,
                );
            }
            rows.len() as u16
        } else {
            0
        }
    } else {
        0
    };

    if is_focused
        && let Some((typ, msg)) = frame.lsp_message
        && typ == lsp_types::MessageType::ERROR
    {
        let inset = popout_inset();
        let cell_width = status_area
            .width
            .saturating_sub(inset * 2)
            .saturating_sub(2) as usize;
        let scale = theme.type_scale(TypeRole::Ui);
        let width = scaled_char_capacity(cell_width, scale);
        let lines = wrap_popout_lines(msg, width, 4);
        if !lines.is_empty()
            && let Some(area) =
                popout_area(status_area, content_area, lines.len() as u16, status_rows)
        {
            let bg = popout_card_bg(theme);
            let border = theme
                .get(crate::theme::scope::UI_BORDER_INACTIVE)
                .fg
                .unwrap_or_else(|| crate::render::themed_fg(theme));
            let content = paint_popout_card(buf, area, bg, border, theme, &mut *scene);

            let style = theme
                .get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
                .patch(theme.get(crate::theme::scope::UI_ERROR));
            for (i, line) in lines.iter().enumerate() {
                chrome::text(
                    buf,
                    content.x,
                    content.y + i as u16,
                    content.x + content.width,
                    line,
                    style,
                    style_rgb(Some(bg)),
                    scale,
                    &mut *scene,
                );
            }
        }
    }
}

/// Declare the island halo of a pane whose content fills `content_area`.
///
/// The halo is a backdrop panel with no border and no fill over the content, so
/// it draws only the island role's shadow around the content, and the pane's
/// pool composite still draws inside it. A pane flush against the
/// single-minimap band, over the band's own rows, takes the band into its
/// island, so the band sits inside an island and not on the window ground.
pub(crate) fn render_island(content_area: Rect, frame: FrameCtx<'_>, scene: &mut ApcScene) {
    let theme = frame.theme;
    let Some(chrome) = style_rgb(
        theme
            .try_get(crate::theme::scope::UI_SURFACE_CHROME)
            .and_then(|style| style.bg),
    ) else {
        return;
    };
    let area = match frame.minimap_band {
        Some(band)
            if band.x == content_area.right()
                && band.y == content_area.y
                && band.height == content_area.height =>
        {
            Rect {
                width: content_area.width + band.width,
                ..content_area
            }
        },
        _ => content_area,
    };

    let role = theme.panel(PanelRole::Island);
    Panel {
        style: BorderStyle::None_,
        border: chrome,
        corner_radius: role.corner_radius,
        fill: None,
        shadow: role.shadow,
        inset_x: role.inset_x,
        above_pools: false,
        backdrop: true,
        anchor: None,
    }
    .draw_components(area, scene);
}

/// The per-pane minimap strip that `pane` declares over content store
/// `content_id`.
///
/// The strip takes the diff palette when `diff` holds, and the syntax palette
/// otherwise. A dimmed pane's palette and thumb blend toward the theme
/// background at the pane's dim, as the rest of the pane does.
fn pane_minimap<'a>(
    pane: &Pane,
    chrome: MinimapChrome<'a>,
    content_id: u32,
    diff: bool,
    is_focused: bool,
    frame: FrameCtx<'_>,
) -> Minimap<'a> {
    let dim = frame.pane_dim(is_focused);
    let dim_bg = (dim > 0.0)
        .then(|| {
            style_rgb(
                frame
                    .theme
                    .try_get(crate::theme::scope::UI_BACKGROUND)
                    .and_then(|s| s.bg),
            )
        })
        .flatten();
    let blend = |c: [u8; 3]| match dim_bg {
        Some(bg) => dim_rgb(c, bg, dim),
        None => c,
    };

    let [tr, tg, tb, ta] = chrome.thumb;
    let [tr, tg, tb] = blend([tr, tg, tb]);
    Minimap {
        strip_id: pane.index,
        content_id,
        lines_per_cell: MINIMAP_LINES_PER_CELL,
        max_columns: MINIMAP_MAX_COLUMNS,
        bg: [0, 0, 0, 0],
        thumb: [tr, tg, tb, ta],
        thumb_border: [tr, tg, tb],
        palette: match (diff, dim > 0.0) {
            (false, false) => chrome.palette,
            (false, true) => chrome.dimmed_palette,
            (true, false) => chrome.diff_palette,
            (true, true) => chrome.dimmed_diff_palette,
        },
    }
}

/// Dim `area` toward the theme background by the pane's fade.
///
/// An unfocused pane fades so the focused pane reads as the one keys reach,
/// and every pane fades while a modal paints its own box over them, per
/// [`FrameCtx::pane_dim`].
pub(crate) fn dim_pane(area: Rect, is_focused: bool, frame: FrameCtx<'_>, buf: &mut Buffer) {
    let dim = frame.pane_dim(is_focused);
    if dim > 0.0
        && let Some(bg) = style_rgb(
            frame
                .theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
    {
        dim_pane_content(buf, area, bg, dim);
    }
}

/// The names a diff view's status row gives its two sides for the file at
/// `path`.
///
/// A pair names the sides of its own file, and the workspace base names those
/// of every other file.
fn diff_sides_for<'a>(path: Option<&Path>, frame: FrameCtx<'a>) -> (&'a str, &'a str) {
    path.and_then(|path| frame.pair_sides.get(path))
        .map_or(frame.diff_sides, |(from, to)| (from.as_str(), to.as_str()))
}

/// Where a diff view's status row names the base and the target it compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidesLayout {
    /// The row splits at this column, the diff's divider, into a base half and
    /// a target half. The base name ends at the divider and the target name
    /// starts past it, so each name sits under its own column.
    Split(u16),
    /// The row runs whole and names both sides after its lead segments. A
    /// unified diff has no divider for the names to meet at.
    Inline,
}

/// How the status row of a pane showing `view` over `content` names the diff
/// sides, or `None` for a pane that shows no diff.
///
/// The divider is the column the diff body splits at. The body leaves out the
/// pane's minimap strip, so the divider does too.
fn sides_layout(
    view: &View,
    content: Rect,
    frame: FrameCtx<'_>,
    editors: &SlotMap<EditorId, EditorState>,
) -> Option<SidesLayout> {
    let diff = match view {
        View::Editor(id) => editors.get(*id).is_some_and(|editor| editor.diff_view),
        View::DiffChanges(_) => true,
        _ => false,
    };
    if !diff {
        return None;
    }

    let (inner, _) = split_minimap_strip(content, frame.minimap_enabled);
    Some(review::diff_divider_x(inner).map_or(SidesLayout::Inline, SidesLayout::Split))
}

/// The colors of both halves of a split diff view's status row.
///
/// The base half tints toward the removal color and the target half toward
/// the addition color, so each half reads as the side of the diff above it.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DiffSidesColors {
    base: DiffSideColors,
    target: DiffSideColors,
}

impl DiffSidesColors {
    /// The halves' colors in `theme` for a status bar styled `bar`.
    ///
    /// Each channel a `diff.sides` scope sets wins. An unset one tints the bar
    /// toward the side's status color, and an unset name lifts that color
    /// toward white.
    fn resolve(theme: &crate::theme::Theme, bar: Style) -> Self {
        use crate::theme::scope;
        Self {
            base: DiffSideColors::resolve(
                theme,
                bar,
                (scope::DIFF_SIDES_BASE, scope::DIFF_SIDES_BASE_NAME),
                scope::DIFF_DELETED,
            ),
            target: DiffSideColors::resolve(
                theme,
                bar,
                (scope::DIFF_SIDES_TARGET, scope::DIFF_SIDES_TARGET_NAME),
                scope::DIFF_ADDED,
            ),
        }
    }
}

/// The colors one half of a split diff view's status row paints in.
#[derive(Clone, Copy, Debug, PartialEq)]
struct DiffSideColors {
    /// The half's fill.
    bg: Color,
    /// The half's plain text, such as the hunk count.
    fg: Color,
    /// The side's name, and the other bright text of its half.
    name: Color,
    /// The `U+2212` or `+` ahead of the name, the side's status color itself.
    mark: Color,
}

impl DiffSideColors {
    /// The colors of one half, from its `(half, name)` scopes and its `status`
    /// scope.
    fn resolve(
        theme: &crate::theme::Theme,
        bar: Style,
        (half, name): (&str, &str),
        status: &str,
    ) -> Self {
        let bar_bg = bar.bg.unwrap_or_else(|| crate::render::themed_bg(theme));
        let bar_fg = bar.fg.unwrap_or_else(|| crate::render::themed_fg(theme));
        let mark = theme.get(status).fg.unwrap_or(bar_fg);
        let set = theme.try_get(half).unwrap_or_default();

        Self {
            bg: set
                .bg
                .unwrap_or_else(|| tint_color(bar_bg, mark, SIDE_FILL_TINT)),
            fg: set
                .fg
                .unwrap_or_else(|| tint_color(bar_fg, mark, SIDE_TEXT_TINT)),
            name: theme
                .try_get(name)
                .and_then(|style| style.fg)
                .unwrap_or_else(|| tint_color(mark, Color::Rgb(255, 255, 255), SIDE_NAME_LIFT)),
            mark,
        }
    }

    /// The half's fill, and the style its plain segments inherit, over the
    /// bar's own `style`.
    fn style(self, bar: Style) -> Style {
        bar.fg(self.fg).bg(self.bg)
    }

    /// The side's mark and name over `style`, in reading order.
    ///
    /// The name is bold where the bar paints cells. The rich bar carries no
    /// weight, so there the name reads by its color alone.
    fn pair(self, mark: char, name: &str, style: Style) -> [StatusSeg; 2] {
        [
            (format!(" {mark}"), style.fg(self.mark)),
            (
                format!(" {name} "),
                style.fg(self.name).add_modifier(Modifier::BOLD),
            ),
        ]
    }
}

/// `from` moved `amount` of the way to `to`, or `from` unchanged when either
/// color is not RGB.
fn tint_color(from: Color, to: Color, amount: f32) -> Color {
    match (style_rgb(Some(from)), style_rgb(Some(to))) {
        (Some(from), Some(to)) => {
            let [r, g, b] = review::tint_rgb(from, to, amount);
            Color::Rgb(r, g, b)
        },
        _ => from,
    }
}

/// A pane's content and status rectangles, given where the single-minimap band
/// sits.
///
/// The band stops one row above the bottom. A status bar on that freed row,
/// flush against the band's left edge, reclaims the band's width so it runs
/// edge to edge. Mid-window status rows sit beside strip rows and stay
/// pane-width.
///
/// Shared with the render site, which captures the content rectangle for the
/// replay cache.
pub(crate) fn pane_areas(pane_area: Rect, minimap_band: Option<Rect>) -> (Rect, Rect) {
    let (content_area, mut status_area) = split_pane_status(pane_area);
    if let Some(band) = minimap_band
        && status_area.y == band.y + band.height
        && status_area.x + status_area.width == band.x
    {
        status_area.width += band.width;
    }
    (content_area, status_area)
}

/// Blend every RGB cell in `area` toward `bg` by `amount`, dimming an unfocused
/// pane's live grid. A cell with a non-RGB color (an indexed-color terminal) is
/// left unchanged, so such terminals simply do not dim. `amount` is expected in
/// `0.0..=1.0`.
pub(crate) fn dim_pane_content(buf: &mut Buffer, area: Rect, bg: [u8; 3], amount: f32) {
    // With `bg` and `amount` fixed for the pass, the blend is a function of the
    // cell's own color, and themed text runs long stretches of one. A channel
    // keeps its own last answer, since a run of one foreground is not a run of
    // the same background.
    let mut last_fg = None;
    let mut last_bg = None;

    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            let cell = &mut buf[(x, y)];
            if let Color::Rgb(r, g, b) = cell.fg {
                let [r, g, b] = dim_memoized(&mut last_fg, [r, g, b], bg, amount);
                cell.set_fg(Color::Rgb(r, g, b));
            }
            if let Color::Rgb(r, g, b) = cell.bg {
                let [r, g, b] = dim_memoized(&mut last_bg, [r, g, b], bg, amount);
                cell.set_bg(Color::Rgb(r, g, b));
            }
        }
    }
}

/// [`dim_rgb`] answered from `last` when it holds the same input, and recorded
/// there when it does not.
///
/// A caller only reaches this for a color it is going to blend, so a cell left
/// alone never displaces the answer a run of blended ones is sharing.
fn dim_memoized(
    last: &mut Option<([u8; 3], [u8; 3])>,
    color: [u8; 3],
    bg: [u8; 3],
    amount: f32,
) -> [u8; 3] {
    if let Some((input, output)) = *last
        && input == color
    {
        return output;
    }

    let output = dim_rgb(color, bg, amount);
    *last = Some((color, output));
    output
}

/// Minimal status bar for overlay panes (commits/rebase/reword/conflict).
/// Does not know about editors or buffers. Shows mode + workspace + a short
/// label identifying the overlay on the left, and the pending count and the
/// status message on the right. Matches the visual style of
/// [`render_pane_status`] for a focused pane.
///
/// As on the pane bar, the count and the message show only while the pane has
/// focus, and the message is cut to the room the left segments leave.
///
/// `screen` picks the label. A commits pane names itself, because an
/// unfocused list is not the screen the frame reports.
pub(crate) fn render_overlay_status(
    area: Rect,
    is_focused: bool,
    screen: Option<&str>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let base_style = if is_focused {
        frame.theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        frame.theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };

    let y = area.y;
    let end_x = area.x + area.width;
    for x in area.x..end_x {
        buf[(x, y)].set_char(' ').set_style(base_style);
    }

    let segments = status_segments_area(area, frame.badge_cover);
    let (left, left_end) = overlay_status_segments(is_focused, segments, screen, frame);
    let mut right: Vec<StatusSeg> = Vec::new();
    let mut right_anchor = segments.x + segments.width;
    // The first right segment takes the bar's right edge, which is where the
    // pane bar paints the count.
    if is_focused && let Some(count) = frame.pending_count {
        let text = format!(" {count} ");
        let start = right_anchor.saturating_sub(text.chars().count() as u16);
        if start >= left_end {
            right.push((text, base_style.add_modifier(Modifier::BOLD)));
            right_anchor = start;
        }
    }
    if is_focused && let Some(message) = frame.status_message {
        let available = right_anchor.saturating_sub(left_end) as usize;
        if available > 0 {
            right.push((
                message.chars().take(available).collect(),
                status_message_style(base_style, frame.theme),
            ));
        }
    }
    let run = StatusRun {
        area,
        segments,
        style: base_style,
        left,
        right,
        hairline: true,
    };
    render_status_runs(&[run], frame, buf, scene, None);
}

/// Build the overlay status bar's left segments in paint order, with the column
/// where they end.
///
/// Mode and workspace show only when focused, then a screen label that shows
/// unconditionally, left-padded when it leads. The screen segment differs from
/// the pane status's focus-gated one. The end column stops at the right edge of
/// `area`, and the right segments fit between it and that edge.
///
/// A focused bar names an open picker modal in place of the screen, as the
/// pane status does. The label then agrees with the prompt chip beside it.
fn overlay_status_segments(
    is_focused: bool,
    area: Rect,
    screen: Option<&str>,
    frame: FrameCtx<'_>,
) -> (Vec<StatusSeg>, u16) {
    let theme = frame.theme;
    let base_style = if is_focused {
        theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };
    let end_x = area.x + area.width;

    let mut left: Vec<StatusSeg> = Vec::new();
    let mut cursor = area.x;
    if is_focused {
        let (mode_label, mode_bg) = frame.mode_label;
        let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!(" {mode_label} "),
            mode_style,
        );
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!(" {} ", frame.workspace_name),
            base_style.add_modifier(Modifier::BOLD),
        );
    }
    let label = match frame.modal_label.filter(|_| is_focused) {
        Some(modal_label) => Some((modal_label, base_style)),
        None => screen_segment(screen, theme)
            .map(|(screen_label, screen_color)| (screen_label, base_style.fg(screen_color))),
    };
    if let Some((label, style)) = label {
        let left_pad = if cursor == area.x { " " } else { "" };
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!("{left_pad}{label} "),
            style,
        );
    }
    (left, cursor)
}

pub(crate) fn render_pane_dividers(
    dividers: &[Divider],
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let dim = theme.get(crate::theme::scope::UI_BORDER_INACTIVE);
    let lit = theme.get(crate::theme::scope::UI_BORDER_FOCUSED);
    for d in dividers {
        let style = if d.touches_focus { lit } else { dim };
        let buf_end_x = buf.area.x + buf.area.width;
        let buf_end_y = buf.area.y + buf.area.height;
        match d.orientation {
            DividerOrientation::Vertical => {
                if d.x >= buf_end_x {
                    continue;
                }
                let height = d.y.saturating_add(d.len).min(buf_end_y).saturating_sub(d.y);
                chrome::vline(buf, d.x, d.y, height, style, &mut *scene);
            },
            DividerOrientation::Horizontal => {
                if d.y >= buf_end_y {
                    continue;
                }
                let width = d.x.saturating_add(d.len).min(buf_end_x).saturating_sub(d.x);
                chrome::hline(buf, d.x, d.y, width, style, &mut *scene);
            },
        }
    }
}

/// Paint `pane`'s status row into the pane's status lane, and leave that lane
/// open.
///
/// The row changes with every cursor move, so it rides a lane of its own and a
/// move leaves the content lane off the wire.
///
/// The replay cache holds no status row, so a pane whose content replays
/// paints its row through here after the replay. [`render_pane`] paints the
/// row between the content and the cards over the content. The cards touch
/// none of the row's cells, so a replay that paints the row last gives the
/// same frame.
pub(crate) fn render_pane_status(
    pane: &Pane,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    status: &StatusRow,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    scene.set_lane(status_lane(pane.index));
    if status.runs.is_empty() {
        return;
    }

    // The cell between two runs keeps the window ground the frame blanks to.
    for run in &status.runs {
        for x in run.area.left()..run.area.right() {
            buf[(x, run.area.y)].set_char(' ').set_style(run.style);
        }
    }

    let cache = match pane.view {
        View::Editor(id) => editors.get_mut(id).map(|e| &mut e.status_scene_cache),
        _ => None,
    };
    render_status_runs(&status.runs, frame, buf, scene, cache);
}

/// The segments of one pane's status row, built once a frame before the pane
/// paints or replays.
///
/// A replay puts back the pane's content alone, so the row paints from this on
/// every frame, and the frame reads the badge back whichever way the pane drew.
#[derive(Default)]
pub(crate) struct StatusRow {
    /// The row's runs from left to right, none for a pane that paints no row.
    runs: Vec<StatusRun>,
    /// The hover rect of the LSP badge the row shows, which the frame reads
    /// back for hit tests.
    pub(crate) badge: Option<Rect>,
}

/// One stretch of a status row, in one fill, with segments packed in from
/// each of its ends.
///
/// A plain row is one run across its width. A split diff view's row is two,
/// its base half and its target half, with a cell of window ground between
/// them at the diff's divider.
#[derive(Default, Hash)]
struct StatusRun {
    /// The cells the run fills.
    area: Rect,
    /// The part of `area` that holds the segments, left of any badge box that
    /// paints over the row.
    segments: Rect,
    /// Fill style of the run, and the style every plain segment inherits.
    style: Style,
    left: Vec<StatusSeg>,
    right: Vec<StatusSeg>,
    /// Whether the run draws the bar's hairline along its top. A half's tint
    /// already sets it apart from the content, so the halves draw none.
    hairline: bool,
}

/// The status row that [`render_pane`] paints for `pane`.
///
/// A changes view's row reads its focused excerpt, as an editor pane's row
/// reads its editor. Empty for a commits view, which paints a status row of
/// its own, and for a pane with no room for the row.
pub(crate) fn status_row(
    pane: &Pane,
    is_focused: bool,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    terms: &SlotMap<TermId, TermSession>,
    changes: &SlotMap<DiffChangesId, DiffChangesState>,
) -> StatusRow {
    let (content, area) = pane_areas(pane.area, frame.minimap_band);
    if area.width == 0 || area.height == 0 || matches!(pane.view, View::Commits(_)) {
        return StatusRow::default();
    }

    let sides = sides_layout(&pane.view, content, frame, editors);
    let mut badge = None;
    let runs = status_segments(
        &pane.view, is_focused, area, sides, frame, editors, buffers, terms, changes, &mut badge,
    );
    StatusRow { runs, badge }
}

/// The part of status row `row` that holds its segments, left of any badge
/// box that paints over the row.
///
/// A bottom-right badge draws the lower edge of its box on the window's last
/// row. A segment under the box is painted over, so the segments end where
/// the box starts. When the box starts left of the row, the row holds no
/// segments. The row's background and hairline still run the whole width.
fn status_segments_area(row: Rect, cover: Option<Rect>) -> Rect {
    let end = row.x + row.width;
    match cover {
        Some(cover) if cover.y == row.y && cover.x < end && cover.x + cover.width > row.x => Rect {
            width: cover.x.saturating_sub(row.x),
            ..row
        },
        _ => row,
    }
}

/// Everything a detached pane's status bar draws, assembled but not yet
/// painted.
///
/// The pair `(base_style, left, right)` determines the painted row exactly,
/// given the rectangle. A caller that caches the painted bytes can therefore
/// compare this instead of repainting and diffing, which is why the assembly is
/// exposed apart from [`paint_pane_status_cells`].
pub(crate) struct PaneStatusCells {
    /// Fill style of the row, and the style every plain segment inherits.
    pub(crate) base_style: Style,
    pub(crate) left: Vec<StatusSeg>,
    pub(crate) right: Vec<StatusSeg>,
}

/// Assemble a detached pane's status bar for [`paint_pane_status_cells`].
///
/// Shares [`status_segments`] with [`status_row`], adding the numeric
/// pane badge that only a detached pane needs, since the primary scene's digit
/// popover does not reach its window. The badge paints in the focused bar's
/// style on either bar, so its digit reads against its box. The focused window
/// gets none, since the chord starts from that pane and does not pick it.
///
/// The bar runs whole for every view. A detached diff view keeps the file name
/// and the staged label, and names no sides.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pane_status_cells(
    view: &View,
    is_focused: bool,
    area: Rect,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    terms: &SlotMap<TermId, TermSession>,
    changes: &SlotMap<DiffChangesId, DiffChangesState>,
    badge: Option<u32>,
) -> PaneStatusCells {
    let StatusRun {
        style: base_style,
        mut left,
        right,
        ..
    } = status_segments(
        view, is_focused, area, None, frame, editors, buffers, terms, changes, &mut None,
    )
    .into_iter()
    .next()
    .unwrap_or_default();

    if let Some(digit) = badge {
        let badge_style = frame
            .theme
            .get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
            .add_modifier(Modifier::BOLD);
        left.insert(0, (format!("[{digit}]"), badge_style));
    }

    PaneStatusCells {
        base_style,
        left,
        right,
    }
}

/// Paint assembled status segments into `buf` as plain cells, for a detached
/// pane's aux window where no rich APC scene is available.
pub(crate) fn paint_pane_status_cells(cells: &PaneStatusCells, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let y = area.y;
    let end_x = area.x + area.width;
    for x in area.x..end_x {
        buf[(x, y)].set_char(' ').set_style(cells.base_style);
    }

    paint_status_fallback(buf, area, &cells.left, &cells.right);
}

/// The APC frame a status bar last emitted, held so an unchanged repaint
/// splices it instead of encoding it again.
///
/// The bar is re-encoded every frame and then discarded unchanged by the
/// scene's own flush comparison, so a pane that repaints for a reason the bar
/// does not share pays a text run per segment for nothing.
///
/// `key` is `None` while nothing is recorded, which covers a bar that has only
/// ever taken the cell fallback. A frame is recorded only under a live scene,
/// since a dead one drops the appends and records nothing.
///
/// Only the editor pane's bar has somewhere to keep one. The overlay bar and
/// the tab bar are drawn from state no editor owns, so they encode every frame.
#[derive(Default)]
pub(crate) struct StatusSceneCache {
    key: Option<u64>,
    bytes: Vec<u8>,
}

/// Render the built status runs as rich APC components inside stoatty, or into
/// cells otherwise.
///
/// Rich mode needs a live scene and every segment color as RGB. A dead scene or
/// any color outside RGB drops the whole row to the cell fallback, so a foreign
/// terminal and a theme without RGB status colors both keep the cell rendering.
///
/// A run's segments go inside its `segments` rect, which [`status_segments_area`]
/// leaves them, while its hairline runs all of its `area`. The hairline takes
/// `ui.statusbar.separator`, or `ui.border.inactive` where the theme leaves that
/// unset.
///
/// With a `cache`, a repaint whose runs and colors all hold splices the
/// recorded frame. Splicing is only sound because
/// [`StatusBar::draw_components_within`] reads nothing outside those, and
/// writes nothing but the scene, so a skipped encode leaves no cell unpainted.
///
/// Returns whether the row painted as rich components, which tells a hit test
/// the scale the segments advance at.
fn render_status_runs(
    runs: &[StatusRun],
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    cache: Option<&mut StatusSceneCache>,
) -> bool {
    let separator = (|| {
        scene.live().then_some(())?;
        let theme = frame.theme;
        style_rgb(
            theme
                .try_get(crate::theme::scope::UI_STATUSBAR_SEPARATOR)
                .and_then(|style| style.fg)
                .or_else(|| theme.get(crate::theme::scope::UI_BORDER_INACTIVE).fg),
        )
    })();

    let Some(separator) = separator else {
        paint_runs_fallback(buf, runs);
        return false;
    };

    let key = status_scene_key(runs, separator);
    if let Some(cache) = &cache
        && cache.key == Some(key)
    {
        scene.buffer().extend_from_slice(&cache.bytes);
        return true;
    }

    let rich: Option<Vec<_>> = runs
        .iter()
        .map(|run| {
            let bg = style_rgb(run.style.bg)?;
            let left = resolve_rich_segments(&run.left, run.style)?;
            let right = resolve_rich_segments(&run.right, run.style)?;
            Some((bg, left, right))
        })
        .collect();

    let Some(rich) = rich else {
        paint_runs_fallback(buf, runs);
        return false;
    };

    let start = scene.bytes().len();
    let scale = frame.theme.type_scale(TypeRole::Ui);
    for (run, (bg, left, right)) in runs.iter().zip(&rich) {
        StatusBar {
            left,
            right,
            scale,
            separator: if run.hairline { separator } else { *bg },
            bg: *bg,
        }
        .draw_components_within(run.area, run.segments.width, buf, scene);
    }

    if let Some(cache) = cache {
        cache.bytes.clear();
        cache.bytes.extend_from_slice(&scene.bytes()[start..]);
        cache.key = Some(key);
    }
    true
}

/// Paint each run's segments into cells through [`paint_status_fallback`].
fn paint_runs_fallback(buf: &mut Buffer, runs: &[StatusRun]) {
    for run in runs {
        paint_status_fallback(buf, run.segments, &run.left, &run.right);
    }
}

/// Hash everything the status row's APC frame is encoded from into a cache key.
///
/// The segments go in as their pre-resolution text and style, which is what
/// they are compared as on the windowed path too. Resolving them to RGB is
/// itself part of the work the cache exists to skip, and the resolution is a
/// function of the runs hashed here.
fn status_scene_key(runs: &[StatusRun], separator: [u8; 3]) -> u64 {
    let mut hasher = DefaultHasher::new();
    runs.hash(&mut hasher);
    separator.hash(&mut hasher);
    hasher.finish()
}

/// Paint the tab bar across `area`, the workspace name first and then one label
/// per tab.
///
/// The name leads in bold, since the status row leaves it out while the bar
/// shows. A tab reads as its [`Workspace::tab_label`], and a tagged file's
/// extension follows it as a tag in the tag's color, joined by a muted dot.
/// Painting through the status-bar segment path means the bar picks up the
/// same scaled-run rendering under stoatty and the same cell fallback
/// elsewhere.
///
/// Leaves in `spans` the extent of each painted tab, in sixteenths of a cell
/// from the left edge of `area`, so a pointer press resolves against the
/// segments as painted.
pub(crate) fn render_tab_bar(
    ws: &Workspace,
    area: Rect,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    spans: &mut Vec<Range<u16>>,
) {
    spans.clear();
    if area.width == 0 || area.height == 0 {
        return;
    }

    let theme = frame.theme;
    let active = theme.get(crate::theme::scope::UI_TABBAR_ACTIVE);
    let inactive = theme.get(crate::theme::scope::UI_TABBAR_INACTIVE);
    for x in area.x..area.x + area.width {
        buf[(x, area.y)].set_char(' ').set_style(inactive);
    }

    let mut left: Vec<StatusSeg> = vec![
        (
            format!(" {} ", frame.workspace_name),
            inactive.add_modifier(Modifier::BOLD),
        ),
        (" ".to_string(), inactive),
    ];
    let mut owners = vec![None, None];
    for idx in 0..ws.tabs.len() {
        let style = if idx == ws.active_tab {
            active
        } else {
            inactive
        };
        let (label, tag) = ws.tab_label(idx);
        left.push((format!(" {label} "), style));
        owners.push(Some(idx));
        if let Some((tag, tag_scope)) = tag {
            left.push((
                "\u{b7}".to_string(),
                tinted(style, theme, crate::theme::scope::UI_TEXT_MUTED),
            ));
            left.push((format!(" {tag} "), tinted(style, theme, tag_scope)));
            owners.extend([Some(idx), Some(idx)]);
        }
    }

    let runs = [StatusRun {
        area,
        segments: area,
        style: inactive,
        left,
        right: Vec::new(),
        hairline: true,
    }];
    let rich = render_status_runs(&runs, frame, buf, scene, None);
    let scale = if rich {
        theme.type_scale(TypeRole::Ui)
    } else {
        FULL_CELL_SCALE
    };
    tab_spans(
        owners
            .into_iter()
            .zip(runs[0].left.iter().map(|(text, _)| text.chars().count())),
        scale,
        area.width,
        spans,
    );
}

/// Records the extent of each tab in the tab bar, in sixteenths of a cell from
/// the bar's left edge.
///
/// `segments` pairs each bar segment with the tab it belongs to and its
/// character count. A segment of no tab, such as the workspace name, takes room
/// but makes no span, so the list indexes the tabs alone. A tab cut at the
/// bar's right edge ends there, and a tab past the edge has no entry, so the
/// list holds only the tabs on screen.
fn tab_spans(
    segments: impl Iterator<Item = (Option<usize>, usize)>,
    scale: u16,
    width: u16,
    spans: &mut Vec<Range<u16>>,
) {
    spans.clear();
    let limit = (u32::from(width) * 16).min(u32::from(u16::MAX)) as u16;
    let mut cursor = 0u16;
    for (tab, count) in segments {
        if cursor >= limit {
            break;
        }
        let end = cursor
            .saturating_add(text_run::advance_sixteenths(count, scale))
            .min(limit);
        match tab {
            Some(tab) if tab < spans.len() => spans[tab].end = end,
            Some(_) => spans.push(cursor..end),
            None => {},
        }
        cursor = end;
    }
}

/// One built status-bar segment pairing painted text with its cell style.
type StatusSeg = (String, Style);

/// `style` with the foreground of the theme's `scope`, or `style` unchanged
/// when the scope sets no foreground.
fn tinted(style: Style, theme: &crate::theme::Theme, scope: &str) -> Style {
    match theme.get(scope).fg {
        Some(fg) => style.fg(fg),
        None => style,
    }
}

/// Resolve the style for the transient status message segment from the bar's
/// base style plus the theme's `ui.message.error` override.
///
/// Falls back to red when the theme leaves that scope undefined, so the message
/// always reads as an alert rather than blending into the bar.
///
/// That fallback is an RGB red rather than a named one because
/// [`render_status_segments`] drops the whole bar to the cell fallback on any
/// non-RGB segment color. A theme missing this scope must degrade only the
/// message's color, never the bar's rendering mode.
fn status_message_style(base_style: Style, theme: &crate::theme::Theme) -> Style {
    base_style.patch(
        theme
            .try_get(crate::theme::scope::UI_MESSAGE_ERROR)
            .unwrap_or_else(|| Style::default().fg(Color::Rgb(0xe0, 0x6c, 0x75))),
    )
}

/// Build the status row's runs, each with its left- and right-anchored segments
/// as `(text, style)` pairs in paint order.
///
/// Both the cell fallback and the rich status path consume these, so the two
/// renderings stay in lockstep. The left cursor and right anchor track the same
/// cell arithmetic [`paint_segment`] applies, so a segment enters the list only
/// where it paints, and the `lsp_message` truncation matches the paint.
///
/// `area` is the whole row. With `sides`, the row names the base and the
/// target of a diff. A split row is two runs. The lead segments fill the base
/// half, and the trailing ones fill the target half. A diff's row leaves out
/// the workspace name, the file name, and the staged label, since the tab
/// names the file, and its lead names the hunk the cursor stands in.
#[allow(clippy::too_many_arguments)]
fn status_segments(
    view: &View,
    is_focused: bool,
    area: Rect,
    sides: Option<SidesLayout>,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    terms: &SlotMap<TermId, TermSession>,
    changes: &SlotMap<DiffChangesId, DiffChangesState>,
    badge_rect: &mut Option<Rect>,
) -> Vec<StatusRun> {
    let theme = frame.theme;
    let bar_style = if is_focused {
        theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };
    let segments = status_segments_area(area, frame.badge_cover);
    let end_x = segments.x + segments.width;

    let colors = sides.map(|_| DiffSidesColors::resolve(theme, bar_style));
    let split = match sides {
        Some(SidesLayout::Split(divider)) => {
            colors.map(|colors| (split_halves(area, divider, frame.badge_cover), colors))
        },
        _ => None,
    };
    let lead_style = split.map_or(bar_style, |(_, colors)| colors.base.style(bar_style));
    let trail_style = split.map_or(bar_style, |(_, colors)| colors.target.style(bar_style));
    let lead_end = split.map_or(end_x, |([(_, base), _], _)| base.right());
    let trail_end = split.map_or(end_x, |([_, (_, target)], _)| target.right());

    let mut left: Vec<(String, Style)> = Vec::new();
    let mut cursor = area.x;
    // The prompt takes the identifying left segments for as long as it is open.
    // The typed text is what the user needs to read mid-type. The workspace,
    // screen and file names are context they already have.
    let prompt = frame.status_prompt.filter(|_| is_focused);
    if is_focused {
        let (label, mode_bg) = frame.mode_label;
        let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
        push_left(
            &mut left,
            &mut cursor,
            lead_end,
            format!(" {label} "),
            mode_style,
        );
        if let Some(prompt) = prompt {
            push_prompt(&mut left, &mut cursor, lead_end, prompt, lead_style);
        } else {
            if !frame.tab_bar_shown && sides.is_none() {
                push_left(
                    &mut left,
                    &mut cursor,
                    lead_end,
                    format!(" {} ", frame.workspace_name),
                    lead_style.add_modifier(Modifier::BOLD),
                );
            }
            if let Some(modal_label) = frame.modal_label {
                push_left(
                    &mut left,
                    &mut cursor,
                    lead_end,
                    format!(" {modal_label} "),
                    lead_style,
                );
            } else if let Some((screen_label, screen_color)) = screen_segment(frame.screen, theme) {
                let screen_color = split.map_or(screen_color, |(_, colors)| colors.base.name);
                push_left(
                    &mut left,
                    &mut cursor,
                    lead_end,
                    format!(" {screen_label} "),
                    lead_style.fg(screen_color),
                );
            }
            if let Some(position) = frame.hunk_position.as_ref().filter(|_| sides.is_some()) {
                let at = position
                    .hunk_at
                    .map_or_else(|| "-".to_string(), |k| k.to_string());
                push_left(
                    &mut left,
                    &mut cursor,
                    lead_end,
                    format!(" hunk {at} / {} ", position.hunk_total),
                    lead_style,
                );
            }
        }
    }

    let status = pane_status_info(
        view,
        frame.workspace_root,
        frame.home,
        editors,
        buffers,
        terms,
        changes,
    );
    let cursor_pos = status.cursor_pos;
    if let Some(name) = &status.filename
        && prompt.is_none()
        && sides.is_none()
    {
        let left_pad = if cursor == area.x { " " } else { "" };
        let read_only = if status.read_only { " [readonly]" } else { "" };
        let modified = if status.dirty { " [+]" } else { "" };
        let text = format!("{left_pad}{name}{read_only}{modified} ");
        push_left(&mut left, &mut cursor, lead_end, text, lead_style);
    }
    // A lone cursor paints no count, since the count only answers how many
    // places an edit lands once there is more than one.
    if let Some((_, count)) = status.selections
        && count > 1
        && prompt.is_none()
    {
        let muted = tinted(lead_style, theme, crate::theme::scope::UI_TEXT_MUTED);
        push_left(
            &mut left,
            &mut cursor,
            lead_end,
            format!(" {count} sel "),
            muted,
        );
    }
    if let Some((_, label)) = status
        .buffer_id
        .and_then(|id| frame.held_buffers.iter().find(|(held, _)| *held == id))
        && prompt.is_none()
    {
        push_left(
            &mut left,
            &mut cursor,
            lead_end,
            format!("[for {label}] "),
            lead_style.add_modifier(Modifier::BOLD),
        );
    }

    let mut base_right: Vec<StatusSeg> = Vec::new();
    let mut target_left: Vec<StatusSeg> = Vec::new();
    if let Some(colors) = colors {
        let path = status.buffer_id.and_then(|id| buffers.path_for(id));
        let (base_name, target_name) = diff_sides_for(path, frame);
        match split {
            // The trailing segments pack against the target half's lead from
            // here, since the base half's run ends at the divider.
            Some(([_, (_, target)], _)) => {
                (base_right, target_left, cursor) = split_side_segments(
                    (base_name, target_name),
                    colors,
                    (lead_style, trail_style),
                    (cursor, lead_end),
                    target,
                );
            },
            None => {
                let pairs = [
                    (colors.base, '\u{2212}', base_name),
                    (colors.target, '+', target_name),
                ];
                for (side, mark, name) in pairs {
                    for (text, style) in side.pair(mark, name, bar_style.bg(side.bg)) {
                        push_left(&mut left, &mut cursor, end_x, text, style);
                    }
                }
            },
        }
    }

    let mut right: Vec<(String, Style)> = Vec::new();
    let mut right_anchor = trail_end;
    if let Some((line, col)) = cursor_pos {
        let text = format!(" {line}:{col} ");
        let width = text.chars().count() as u16;
        let start = right_anchor.saturating_sub(width);
        if start >= cursor {
            let style = split.map_or(trail_style, |(_, colors)| {
                trail_style.fg(colors.target.name)
            });
            right.push((text, style));
            right_anchor = start;
        }
    }
    if is_focused {
        let badge_right = right_anchor;
        let muted = tinted(trail_style, theme, crate::theme::scope::UI_TEXT_MUTED);
        // The block reads left to right as the servers and then the sources the
        // file's diagnostics name, and right segments push leftward, so the
        // sources push first.
        let sources = focused_diagnostic_sources(view, editors, buffers, frame.diagnostics);
        let sources = (!sources.is_empty()).then(|| format!(" {} ", sources.join(" \u{b7} ")));
        let servers = frame
            .lsp_servers
            .iter()
            .rev()
            .map(|server| match server.busy {
                true => format!(
                    " {} {} ",
                    SPINNER_FRAMES[frame.spinner_phase as usize], server.name
                ),
                false => format!(" {} ", server.name),
            });
        for text in sources.into_iter().chain(servers) {
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, muted));
                right_anchor = start;
            }
        }
        if right_anchor < badge_right {
            // The bar paints the badge glyphs at its Ui scale, right-anchored at
            // badge_right, so they cover fewer cells than their char count.
            // Record the hover rect over that scaled extent (dropping the leftmost
            // pad space, which carries no glyph) so hover matches the drawn glyphs.
            let chars = u32::from(badge_right - right_anchor);
            let width = ((chars - 1) * u32::from(theme.type_scale(TypeRole::Ui)))
                .div_ceil(u32::from(FULL_CELL_SCALE)) as u16;
            *badge_rect = Some(Rect::new(badge_right - width, area.y, width, area.height));
        }
        if frame.diff_warm_busy {
            let text = format!(" {} diff ", SPINNER_FRAMES[frame.spinner_phase as usize]);
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, trail_style));
                right_anchor = start;
            }
        }
        if let Some(count) = frame.pending_count {
            let text = format!(" {count} ");
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, trail_style.add_modifier(Modifier::BOLD)));
                right_anchor = start;
            }
        }
        if let Some(register) = frame.recording_register {
            let text = format!(" REC {register} ");
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, trail_style.add_modifier(Modifier::BOLD)));
                right_anchor = start;
            }
        }
        // The severities push from hint to error, so they read error first.
        let severities = focused_diagnostic_label(
            view,
            editors,
            buffers,
            frame.diagnostics,
            trail_style,
            theme,
        );
        for (text, style) in severities.into_iter().rev() {
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start < cursor {
                break;
            }
            right.push((text, style));
            right_anchor = start;
        }
        // A status message answers what the reader just did and expires after a
        // few seconds, while the staged summary stays on the bar. The summary
        // takes a place only where the room left of it holds the whole message,
        // because a cut drops the message's tail, such as the on or off of a
        // toggle report.
        let message_chars = frame.status_message.map_or(0, |m| m.chars().count());
        let pair_lead = status
            .buffer_id
            .and_then(|id| buffers.path_for(id))
            .and_then(|path| frame.pair_sides.get(path))
            .map(|(from, _)| format!("diff vs {from}"));
        if sides.is_none()
            && let Some(segments) = focused_staged_label(
                status.mark_kind,
                pair_lead.as_deref().or(frame.diff_base),
                frame.repo_change_counts,
                status.staged_counts,
                frame.hunk_position.as_ref(),
                (trail_style, theme),
            )
        {
            let width: u16 = segments
                .iter()
                .map(|(text, _)| text.chars().count() as u16)
                .sum();
            let start = right_anchor.saturating_sub(width);
            if start >= cursor && usize::from(start - cursor) >= message_chars {
                right.extend(segments.into_iter().rev());
                right_anchor = start;
            }
        }
        if let Some(message) = frame.status_message {
            let available = right_anchor.saturating_sub(cursor) as usize;
            if available > 0 {
                let text: String = message.chars().take(available).collect();
                let width = text.chars().count() as u16;
                let start = right_anchor.saturating_sub(width);
                right.push((text, status_message_style(trail_style, theme)));
                right_anchor = start;
            }
        }
        if let Some((typ, message)) = frame.lsp_message
            && typ != lsp_types::MessageType::ERROR
        {
            let available = right_anchor.saturating_sub(cursor) as usize;
            if available > 0 {
                let text: String = message.chars().take(available).collect();
                let width = text.chars().count() as u16;
                let start = right_anchor.saturating_sub(width);
                right.push((text, trail_style));
                right_anchor = start;
            }
        }
        if let Some(label) = frame.lsp_pending {
            let available = right_anchor.saturating_sub(cursor) as usize;
            if available > 0 {
                let text: String = format!(" lsp: {label}... ")
                    .chars()
                    .take(available)
                    .collect();
                let width = text.chars().count() as u16;
                let start = right_anchor.saturating_sub(width);
                right.push((text, trail_style.add_modifier(Modifier::ITALIC)));
                right_anchor = start;
            }
        }
        #[cfg(feature = "perf")]
        if let Some(seg) = frame.perf {
            let text = crate::render::perf_label(seg);
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, trail_style.add_modifier(Modifier::DIM)));
            }
        }
    }
    let _ = cursor;
    let _ = right_anchor;

    match split {
        Some(([(base, base_segments), (target, target_segments)], _)) => vec![
            StatusRun {
                area: base,
                segments: base_segments,
                style: lead_style,
                left,
                right: base_right,
                hairline: false,
            },
            StatusRun {
                area: target,
                segments: target_segments,
                style: trail_style,
                left: target_left,
                right,
                hairline: false,
            },
        ],
        None => vec![StatusRun {
            area,
            segments,
            style: bar_style,
            left,
            right,
            hairline: true,
        }],
    }
}

/// The base half and the target half of status row `row` split at
/// `divider`, each as its whole rect and the part of it that holds segments.
///
/// The divider's own cell belongs to neither half. A divider outside the row
/// leaves one half empty.
fn split_halves(row: Rect, divider: u16, cover: Option<Rect>) -> [(Rect, Rect); 2] {
    let divider = divider.clamp(row.left(), row.right());
    let target_x = divider.saturating_add(1).min(row.right());
    let base = Rect {
        width: divider - row.x,
        ..row
    };
    let target = Rect {
        x: target_x,
        width: row.right() - target_x,
        ..row
    };
    [base, target].map(|half| (half, status_segments_area(half, cover)))
}

/// The side names of a row split at the divider, as the base half's right
/// segments and the target half's left segments.
///
/// The third value is the column the target's segments reach.
///
/// The base pair ends at `lead_end`, where the base half's segments stop, and
/// takes its place only clear of `cursor`, the column the lead segments
/// reached. Its mark and name show together or not at all. The target pair
/// packs from the left edge of `target`, the target half's segment rect.
fn split_side_segments(
    (base_name, target_name): (&str, &str),
    colors: DiffSidesColors,
    (lead_style, trail_style): (Style, Style),
    (cursor, lead_end): (u16, u16),
    target: Rect,
) -> (Vec<StatusSeg>, Vec<StatusSeg>, u16) {
    let base = colors.base.pair('\u{2212}', base_name, lead_style);
    let width: u16 = base
        .iter()
        .map(|(text, _)| text.chars().count() as u16)
        .sum();
    let base_right = match lead_end
        .checked_sub(width)
        .is_some_and(|start| start >= cursor)
    {
        true => base.into_iter().rev().collect(),
        false => Vec::new(),
    };

    let mut target_left = Vec::new();
    let mut target_cursor = target.x;
    for (text, style) in colors.target.pair('+', target_name, trail_style) {
        push_left(
            &mut target_left,
            &mut target_cursor,
            target.right(),
            text,
            style,
        );
    }
    (base_right, target_left, target_cursor)
}

/// Append the open status prompt as left-anchored segments.
///
/// Three of them, because a segment carries one style for its whole text and
/// the caret is a single inverted cell inside an otherwise uniform run. The
/// trailing segment always ends in a space, so the typed text never abuts
/// whatever the bar packs after it.
fn push_prompt(
    left: &mut Vec<StatusSeg>,
    cursor: &mut u16,
    end_x: u16,
    prompt: &StatusPrompt,
    base_style: Style,
) {
    let (before, rest) = prompt.text.split_at(prompt.cursor);
    let at_caret = rest.chars().next();

    push_left(
        left,
        cursor,
        end_x,
        format!("{}{before}", prompt.label),
        base_style,
    );
    push_left(
        left,
        cursor,
        end_x,
        at_caret.unwrap_or(' ').to_string(),
        prompt_caret_style(base_style),
    );

    let after = at_caret.map_or("", |ch| &rest[ch.len_utf8()..]);
    push_left(left, cursor, end_x, format!("{after} "), base_style);
}

/// Style for the one cell the status prompt's caret covers.
///
/// Swaps the bar's own color pair rather than setting [`Modifier::REVERSED`],
/// because [`resolve_rich_segments`] carries a segment across as an fg/bg pair
/// and drops its modifiers. A reversed caret therefore vanishes under stoatty
/// and shows only in the cell fallback. A bar style missing either color has no
/// pair to swap, so that case keeps the modifier the fallback honors.
fn prompt_caret_style(base_style: Style) -> Style {
    match (base_style.fg, base_style.bg) {
        (Some(fg), Some(bg)) => base_style.fg(bg).bg(fg),
        _ => base_style.add_modifier(Modifier::REVERSED),
    }
}

/// Append a left-anchored segment and advance `cursor` as [`paint_segment`]
/// would, clamping at `end_x`.
fn push_left(left: &mut Vec<StatusSeg>, cursor: &mut u16, end_x: u16, text: String, style: Style) {
    *cursor = cursor
        .saturating_add(text.chars().count() as u16)
        .min(end_x);
    left.push((text, style));
}

/// Paint the built status segments into cells through [`paint_segment`], the
/// graceful-degradation path for a terminal without the rich components.
fn paint_status_fallback(buf: &mut Buffer, area: Rect, left: &[StatusSeg], right: &[StatusSeg]) {
    let y = area.y;
    let end_x = area.x + area.width;

    let mut cursor = area.x;
    for (text, style) in left {
        cursor = paint_segment(buf, y, cursor, end_x, text, *style);
    }

    let mut right_anchor = end_x;
    for (text, style) in right {
        let width = text.chars().count() as u16;
        let start = right_anchor.saturating_sub(width);
        paint_segment(buf, y, start, right_anchor, text, *style);
        right_anchor = start;
    }
    let _ = cursor;
    let _ = right_anchor;
}

/// Resolve each segment's fg/bg to RGB for the rich path, defaulting a missing
/// channel to `base`'s.
///
/// Returns `None` if any resolved color is not RGB, since a theme that cannot
/// supply RGB status colors drives the cell fallback rather than the rich bar.
fn resolve_rich_segments(segments: &[StatusSeg], base: Style) -> Option<Vec<StatusSegment<'_>>> {
    segments
        .iter()
        .map(|(text, style)| {
            let fg = style_rgb(style.fg.or(base.fg))?;
            let bg = style_rgb(style.bg.or(base.bg))?;
            Some(StatusSegment {
                text: text.as_str(),
                fg,
                bg,
            })
        })
        .collect()
}

/// The status-bar diagnostic segments for the focused pane's editor, one per
/// severity it has, from error to hint.
///
/// Each reads a glyph and the count, ` ● 2`, ` ▲ 1`, ` i 3`, or ` · 4`, in the
/// severity's color over `base_style`, and a pad closes the run. The list is
/// empty when the pane is not an editor, has no path, or has no diagnostics.
fn focused_diagnostic_label(
    view: &View,
    editors: &SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    diagnostics: &crate::diagnostics::DiagnosticSet,
    base_style: Style,
    theme: &crate::theme::Theme,
) -> Vec<StatusSeg> {
    let Some(path) = focused_editor_path(view, editors, buffers) else {
        return Vec::new();
    };
    let summary = diagnostics.summarize(path);
    let mut segments: Vec<StatusSeg> = [
        (summary.error, '\u{25cf}', DiagnosticSeverity::ERROR),
        (summary.warning, '\u{25b2}', DiagnosticSeverity::WARNING),
        (summary.information, 'i', DiagnosticSeverity::INFORMATION),
        (summary.hint, '\u{b7}', DiagnosticSeverity::HINT),
    ]
    .into_iter()
    .filter(|&(count, _, _)| count > 0)
    .map(|(count, glyph, severity)| {
        let style = tinted(base_style, theme, diagnostic_severity_scope(severity));
        (format!(" {glyph} {count}"), style)
    })
    .collect();
    if !segments.is_empty() {
        segments.push((" ".to_string(), base_style));
    }
    segments
}

/// The distinct sources the focused pane's diagnostics name, such as a
/// compiler and a linter, in the order they first appear.
fn focused_diagnostic_sources<'a>(
    view: &View,
    editors: &SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    diagnostics: &'a crate::diagnostics::DiagnosticSet,
) -> Vec<&'a str> {
    let Some(path) = focused_editor_path(view, editors, buffers) else {
        return Vec::new();
    };
    let mut sources: Vec<&str> = Vec::new();
    for source in diagnostics
        .get(path)
        .iter()
        .filter_map(|diag| diag.source.as_deref())
    {
        if !sources.contains(&source) {
            sources.push(source);
        }
    }
    sources
}

/// The file the editor in `view` shows, or `None` for a pane that is no editor
/// or an editor with no file.
fn focused_editor_path<'a>(
    view: &View,
    editors: &SlotMap<EditorId, EditorState>,
    buffers: &'a BufferRegistry,
) -> Option<&'a Path> {
    let View::Editor(editor_id) = view else {
        return None;
    };
    buffers.path_for(editors.get(*editor_id)?.buffer_id)
}

/// Statusline label leading with the repo's hunk counts and appending the
/// focused buffer's, or `None` when neither has anything.
///
/// Both halves count hunks and differ only in scope: `repo` is every changed
/// file and `file` is the one on screen. The label names them apart rather than
/// running them together, and a repo with nothing changed still shows while the
/// focused file has hunks, since a tally the file contradicts is worth seeing.
///
/// With no repo pair the label falls back to the file's counts alone. That
/// covers a buffer outside a repo and the window before the first diff lands,
/// where leading with `repo 0 staged` would read as a clean repo.
///
/// A `base_lead` naming a base off the working tree replaces the repo pair
/// rather than joining it. The tally counts what git status reports, which
/// describes a working tree the pane is then not showing. The base always
/// shows, even for a file with no hunks, since it says what is under review.
///
/// A diff view's `position` names the focused file's place among the changed
/// files ahead of its counts, as `file F/M`, and closes the label with
/// ` · hunk K/N`, which says how far through the review the cursor stands. `K`
/// prints as `-` above the focused file's first hunk, where the walk has
/// nothing behind it. The position rides on a label the counts produced, so a
/// scope with nothing to count shows nothing at all.
///
/// The file's counts follow the text as two segments, the pencil count of
/// unstaged hunks in `ui.statusbar.unstaged` and the plus count of staged hunks
/// in `ui.statusbar.staged`, so each count reads in its own color. A range
/// review's counts keep their words inside the text, `approved / unapproved`,
/// since the glyphs name git states. The repo pair counts what git status
/// reports under any base and keeps `staged / unstaged`.
fn focused_staged_label(
    kind: MarkKind,
    base_lead: Option<&str>,
    repo: Option<(usize, usize)>,
    file: Option<(usize, usize)>,
    position: Option<&HunkPosition>,
    (base_style, theme): (Style, &crate::theme::Theme),
) -> Option<Vec<StatusSeg>> {
    let place = position.map(|position| (position.file_at, position.file_total));
    let StagedBody { text, counts } = staged_label_body(kind, base_lead, repo, file, place)?;

    let mut segments = Vec::new();
    if let Some(text) = text {
        let joint = if counts.is_some() { " \u{b7}" } else { "" };
        segments.push((format!(" {text}{joint}"), base_style));
    }
    if let Some((staged, unstaged)) = counts {
        segments.push((
            format!(" \u{270e}{unstaged}"),
            tinted(
                base_style,
                theme,
                crate::theme::scope::UI_STATUSBAR_UNSTAGED,
            ),
        ));
        segments.push((
            format!(" \u{271a}{staged}"),
            tinted(base_style, theme, crate::theme::scope::UI_STATUSBAR_STAGED),
        ));
    }
    let close = match position {
        Some(position) => {
            let at = position
                .hunk_at
                .map_or_else(|| "-".to_string(), |k| k.to_string());
            format!(" \u{b7} hunk {at}/{} ", position.hunk_total)
        },
        None => " ".to_string(),
    };
    segments.push((close, base_style));
    Some(segments)
}

/// What [`staged_label_body`] answers for [`focused_staged_label`].
struct StagedBody {
    /// The unpadded text, or `None` when only the counts show.
    text: Option<String>,
    /// The file's git hunk counts as `(staged, unstaged)`, which follow the
    /// text as glyph segments. `None` for a review, whose counts stay words in
    /// the text, and for a file with no hunks.
    counts: Option<(usize, usize)>,
}

/// The text half of [`focused_staged_label`], with the file's git counts that
/// follow it.
///
/// `place` is the focused file's `(at, total)` among the changed files. With
/// it, the text reads `file {at}/{total}` after the repo pair or the base. A
/// file with no counts keeps its place alone.
fn staged_label_body(
    kind: MarkKind,
    base_lead: Option<&str>,
    repo: Option<(usize, usize)>,
    file: Option<(usize, usize)>,
    place: Option<(usize, usize)>,
) -> Option<StagedBody> {
    let file = file.filter(|&(staged, unstaged)| staged > 0 || unstaged > 0);
    let (counts, worded) = match kind {
        MarkKind::Staged => (file, None),
        MarkKind::Approved => (None, file),
    };
    let words = |(approved, unapproved): (usize, usize)| {
        format!("{approved} approved / {unapproved} unapproved")
    };
    let file_part = match (place, worded) {
        (Some((at, total)), Some(pair)) => {
            Some(format!("file {at}/{total} \u{b7} {}", words(pair)))
        },
        (Some((at, total)), None) => Some(format!("file {at}/{total}")),
        (None, Some(pair)) => Some(format!("file {}", words(pair))),
        (None, None) => None,
    };
    let joined = |head: String| match &file_part {
        Some(part) => format!("{head} \u{b7} {part}"),
        None => head,
    };

    if let Some(lead) = base_lead {
        return Some(StagedBody {
            text: Some(joined(lead.to_string())),
            counts,
        });
    }

    let Some((repo_staged, repo_unstaged)) = repo else {
        let pair = file?;
        return Some(match kind {
            MarkKind::Staged => StagedBody {
                text: None,
                counts: Some(pair),
            },
            MarkKind::Approved => StagedBody {
                text: Some(words(pair)),
                counts: None,
            },
        });
    };
    if repo_staged == 0 && repo_unstaged == 0 && file.is_none() {
        return None;
    }

    let repo_part = format!("repo {repo_staged} staged / {repo_unstaged} unstaged");
    Some(StagedBody {
        text: Some(joined(repo_part)),
        counts,
    })
}

/// What the status bar names a displacing diff base, or `None` for HEAD.
///
/// HEAD and the index are the working tree's own picks, so the bar shows the
/// repo tally under either one, and a diff view's status row names the pick.
/// Every displacing base names itself, so the bar never leaves a reader
/// guessing which base the hunks on screen were measured against.
pub(crate) fn diff_base_lead(base: &DiffBase) -> Option<String> {
    match base {
        DiffBase::Head => None,
        base => Some(format!("diff vs {}", diff::diff_sides(Some(base)).0)),
    }
}

fn diagnostic_severity_scope(severity: DiagnosticSeverity) -> &'static str {
    use crate::theme::scope;
    match severity {
        DiagnosticSeverity::ERROR => scope::UI_DIAGNOSTIC_ERROR,
        DiagnosticSeverity::WARNING => scope::UI_DIAGNOSTIC_WARNING,
        DiagnosticSeverity::INFORMATION => scope::UI_DIAGNOSTIC_INFO,
        DiagnosticSeverity::HINT => scope::UI_DIAGNOSTIC_HINT,
        _ => scope::UI_DIAGNOSTIC_ERROR,
    }
}

/// Formats an LSP progress entry's `title: message pct%` body for a status
/// popout row. Padded with leading and trailing spaces so adjacent segments stay
/// separated. The server is attributed by the row's short-name badge, so the
/// body carries no server prefix.
fn lsp_progress_label(entry: &crate::lsp::progress::LspProgressEntry) -> String {
    let mut body = entry.title.clone();
    if let Some(message) = &entry.message {
        if !body.is_empty() {
            body.push_str(": ");
        }
        body.push_str(message);
    }
    if let Some(pct) = entry.percentage {
        if !body.is_empty() {
            body.push(' ');
        }
        body.push_str(&format!("{pct}%"));
    }
    if body.is_empty() {
        body.push_str("...");
    }
    format!(" {body} ")
}

/// A short uppercase abbreviation of an LSP server name for its status-bar badge.
///
/// A multi-token name takes the initial of each token ("rust-analyzer" -> "RA");
/// a single token takes its first two characters ("pyright" -> "PY"). Clamped to
/// three characters so a long name still reads as a compact fixture.
pub(crate) fn lsp_short_name(name: &str) -> String {
    let tokens: Vec<&str> = name.split(['-', '_']).filter(|t| !t.is_empty()).collect();
    let letters: String = if tokens.len() > 1 {
        tokens
            .iter()
            .filter_map(|token| token.chars().next())
            .collect()
    } else {
        name.chars().take(2).collect()
    };
    letters
        .chars()
        .take(3)
        .collect::<String>()
        .to_ascii_uppercase()
}

fn paint_segment(
    buf: &mut Buffer,
    y: u16,
    start_x: u16,
    end_x: u16,
    text: &str,
    style: Style,
) -> u16 {
    let mut x = start_x;
    for ch in text.chars() {
        if x >= end_x {
            break;
        }
        buf[(x, y)].set_char(ch).set_style(style);
        x += 1;
    }
    x
}

pub(crate) fn mode_segment<'a>(
    mode: &str,
    theme: &crate::theme::Theme,
    mode_badges: &'a std::collections::BTreeMap<String, String>,
) -> (&'a str, Color) {
    use crate::theme::scope;
    let (default_label, default, legacy_scope) = match mode {
        "normal" => ("NOR", Color::Blue, scope::UI_STATUSLINE_NORMAL),
        "insert" => ("INS", Color::Green, scope::UI_STATUSLINE_INSERT),
        "select" => ("SEL", Color::Yellow, scope::UI_STATUSLINE_SELECT),
        "prompt" => ("PMT", Color::Green, scope::UI_STATUSLINE_PROMPT),
        "run" => ("RUN", Color::Magenta, scope::UI_STATUSLINE_RUN),
        "goto" => ("GTO", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "z" => ("VWA", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "bracket_next" => ("BNX", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "bracket_prev" => ("BPV", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "match" => ("MAT", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "select_goto" => ("SLG", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "space" => ("SPC", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "space_workspace" => ("SWS", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "space_pane_nav" => ("SPN", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        "space_pane_nav_new" => ("SNN", Color::DarkGray, scope::UI_STATUSLINE_SUBMODE),
        _ => ("---", Color::Gray, scope::UI_STATUSLINE_DEFAULT),
    };
    let per_mode_scope = format!("ui.statusline.{mode}");
    let color = theme
        .try_get(&per_mode_scope)
        .and_then(|s| s.fg)
        .or_else(|| theme.try_get(legacy_scope).and_then(|s| s.fg))
        .unwrap_or(default);
    let label = mode_badges.get(mode).map_or(default_label, String::as_str);
    (label, color)
}

/// The status-bar label and color for the foreground app `screen`, or `None`
/// for a plain editor with no screen over it.
///
/// App screens are no longer editor modes, so they are labelled separately from
/// the mode cell. Color resolves through the same `ui.statusline.<screen>`
/// scopes [`mode_segment`] used, so a theme restyles both consistently.
pub(crate) fn screen_segment(
    screen: Option<&str>,
    theme: &crate::theme::Theme,
) -> Option<(&'static str, Color)> {
    use crate::theme::scope;
    let (label, default, screen_scope) = match screen? {
        "review" => ("review", Color::Cyan, scope::UI_STATUSLINE_REVIEW),
        "diff" => ("structural diff", Color::Cyan, scope::UI_STATUSLINE_REVIEW),
        "diff_changes" => ("changes", Color::Cyan, scope::UI_STATUSLINE_REVIEW),
        "commits" => ("commits", Color::Yellow, scope::UI_STATUSLINE_COMMITS),
        "rebase" => ("rebase", Color::Red, scope::UI_STATUSLINE_REBASE),
        "reword" => ("reword", Color::Red, scope::UI_STATUSLINE_REWORD),
        "rebase_conflict" => ("conflict", Color::LightRed, scope::UI_STATUSLINE_CONFLICT),
        _ => return None,
    };
    let color = theme
        .try_get(screen_scope)
        .and_then(|s| s.fg)
        .unwrap_or(default);
    Some((label, color))
}

fn pane_status_info(
    view: &View,
    workspace_root: &Path,
    home: Option<&Path>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    terms: &SlotMap<TermId, TermSession>,
    changes: &SlotMap<DiffChangesId, DiffChangesState>,
) -> PaneStatusInfo {
    let owned = |name: &str| PaneStatusInfo {
        filename: Some(name.to_string()),
        ..Default::default()
    };
    match view {
        View::Editor(editor_id) => {
            editor_status_info(*editor_id, workspace_root, home, editors, buffers)
        },
        View::Run(_) => owned("[run]"),
        View::Agent(id) => owned(
            terms
                .get(*id)
                .and_then(|session| session.term.title())
                .unwrap_or("[agent]"),
        ),
        View::Terminal(id) => owned(
            terms
                .get(*id)
                .and_then(|session| session.term.title())
                .unwrap_or("[term]"),
        ),
        View::Image { path, .. } => owned(&crate::action_handlers::file::display_name(path)),
        View::Commits(_) => owned("[commits]"),
        View::DiffChanges(id) => match changes.get(*id).and_then(|state| state.focused_excerpt()) {
            Some(excerpt) => {
                editor_status_info(excerpt.editor_id, workspace_root, home, editors, buffers)
            },
            None => owned("[diff-changes]"),
        },
        View::Label(label) => owned(label),
    }
}

/// What the status bar reads off editor `editor_id`, which an editor pane
/// shows and a changes view shows as its focused excerpt.
fn editor_status_info(
    editor_id: EditorId,
    workspace_root: &Path,
    home: Option<&Path>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
) -> PaneStatusInfo {
    let Some(editor) = editors.get_mut(editor_id) else {
        return PaneStatusInfo::default();
    };
    let buffer_id = editor.buffer_id;
    let filename =
        status_filename(editor, buffers.path_for(buffer_id), workspace_root, home).to_string();
    // The fields come off the same buffer at the same moment, and the bar
    // wants them together. One acquisition answers for all of them, so the
    // staged label takes no lock of its own.
    let shared = buffers.get(buffer_id);
    let (dirty, staged_counts, mark_kind) = shared
        .as_ref()
        .and_then(|b| b.read().ok())
        .map(|g| {
            let map = g.diff_map.as_ref();
            (
                g.dirty,
                map.map(|dm| dm.staged_counts()),
                map.map(|dm| dm.mark_kind()).unwrap_or_default(),
            )
        })
        .unwrap_or_default();
    PaneStatusInfo {
        filename: Some(filename),
        buffer_id: Some(buffer_id),
        read_only: buffers.read_only(buffer_id).is_some(),
        dirty,
        cursor_pos: editor_cursor_position(editor),
        selections: Some((
            editor.selections.primary_index() + 1,
            editor.selections.all_anchors().len(),
        )),
        staged_counts,
        mark_kind,
    }
}

/// What one pane's status bar reads off its view.
#[derive(Default)]
struct PaneStatusInfo {
    filename: Option<String>,
    /// The buffer an editor view shows, which the bar looks up in
    /// [`FrameCtx::held_buffers`].
    buffer_id: Option<BufferId>,
    /// Whether the buffer must not write back to its file, which the bar tags
    /// `[readonly]` between the name and the modified tag.
    read_only: bool,
    dirty: bool,
    cursor_pos: Option<(u32, u32)>,
    /// The primary cursor's 1-based place among the cursors in start order,
    /// and the cursor count. `None` for a view with no editor.
    selections: Option<(usize, usize)>,
    /// `(staged, unstaged)` hunk counts, `None` when the buffer has no diff.
    /// Read alongside `dirty` so the bar takes one lock, not two.
    staged_counts: Option<(usize, usize)>,
    /// What [`Self::staged_counts`] counts, from the same diff map. Under a
    /// range review the counts are approved and unapproved hunks.
    mark_kind: MarkKind,
}

/// The rendered filename for `editor`, reusing the last one when nothing it was
/// rendered from has changed.
///
/// Rendering walks the path and allocates, and the bar re-derives its segments
/// for every pane on every repaint, so a pane sitting still would otherwise
/// rebuild the same string every frame.
fn status_filename<'a>(
    editor: &'a mut EditorState,
    path: Option<&Path>,
    workspace_root: &Path,
    home: Option<&Path>,
) -> &'a str {
    let fresh = editor.status_filename.as_ref().is_some_and(|held| {
        held.path.as_deref() == path
            && held.workspace_root == workspace_root
            && held.home.as_deref() == home
    });
    if !fresh {
        let rendered = match path {
            Some(path) => crate::paths::display_relative_with_home(path, workspace_root, home),
            None => "[scratch]".to_string(),
        };
        editor.status_filename = Some(crate::editor_state::StatusFilename {
            path: path.map(Path::to_path_buf),
            workspace_root: workspace_root.to_path_buf(),
            home: home.map(Path::to_path_buf),
            rendered,
        });
    }
    &editor
        .status_filename
        .as_ref()
        .expect("filled just above")
        .rendered
}

/// Paint an image pane: the file's name, its pixel size, and, where the
/// terminal cannot draw images, what is missing.
///
/// The pixels are not here. Nothing has been asked to send them yet, and a
/// terminal without the capability would never receive them, so both cases show
/// the same label rather than one showing an empty pane.
fn render_image_pane(
    area: Rect,
    buf: &mut Buffer,
    path: &Path,
    px: (u32, u32),
    text_style: Style,
    capable: bool,
) {
    let (width, height) = px;
    let mut lines = vec![Line::styled(
        format!(
            "{} - {width}x{height} px",
            crate::action_handlers::file::display_name(path),
        ),
        text_style,
    )];
    if !capable {
        lines.push(Line::styled("image display needs stoatty", text_style));
    }

    // Centered in the pane rather than at its top, since the label stands in
    // for a picture that would fill it.
    let rows = lines.len() as u16;
    let band = Rect {
        y: area.y + area.height.saturating_sub(rows) / 2,
        height: rows.min(area.height),
        ..area
    };
    Paragraph::new(Text::from(lines))
        .centered()
        .render(band, buf);
}
#[cfg(test)]
mod tests {
    use super::{
        diff_base_lead, focused_staged_label, paint_status_fallback, split_halves,
        split_side_segments, status_filename, tab_spans, DiffSideColors, DiffSidesColors,
    };
    use crate::{
        action_handlers::{dispatch, focused_editor_mut},
        agent_ipc::AgentControl,
        badge::{Anchor, Badge, BadgeSource, BadgeState},
        buffer::{BufferId, TextBuffer},
        diff_map::{DiffMap, MarkKind},
        editor_state::EditorState,
        host::LspNotification,
        lsp::drain,
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
        rebase::RebaseState,
        term_session::TermId,
        workspace::diff::{DiffBase, HunkPosition, WorktreeBase},
        Stoat,
    };
    use lsp_types::{Diagnostic, DiagnosticSeverity, MessageType, Position, Range};
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Arc, RwLock},
    };
    use stoat_action::{
        AddSelectionAbove, AddSelectionBelow, Conflict, KeepPrimarySelection, MoveDown, OpenFile,
        OpenSearchInput,
    };
    use stoat_language::structural_diff;
    use tokio::sync::{mpsc, oneshot};

    /// The view a terminal session shows as, so one test covers each kind of
    /// pane a session fills.
    type SessionView = fn(TermId) -> View;

    /// The rendered name is reused only while everything it was rendered from
    /// holds still.
    ///
    /// The bar re-derives every pane's segments on every repaint, so a name
    /// rebuilt each time is an allocation per pane per frame. Each input is
    /// moved on its own because one missing from the key would leave a pane
    /// showing the name it had before a rename, a workspace switch, or a change
    /// of home.
    #[test]
    fn the_status_filename_is_reused_until_an_input_moves() {
        let executor =
            stoat_scheduler::Executor::new(Arc::new(stoat_scheduler::TestScheduler::new()));
        let shared = Arc::new(RwLock::new(TextBuffer::with_text(BufferId::new(0), "x\n")));
        let mut editor = EditorState::new(BufferId::new(0), shared, executor, crate::test_notify());

        let root = Path::new("/repo");
        let home = Path::new("/home/u");
        let a = PathBuf::from("/repo/src/a.rs");

        let first = status_filename(&mut editor, Some(&a), root, Some(home)).to_string();
        assert_eq!(first, "src/a.rs", "the name renders relative to the root");

        let held = editor
            .status_filename
            .as_ref()
            .expect("rendering fills the cache")
            .rendered
            .as_ptr();
        assert_eq!(
            status_filename(&mut editor, Some(&a), root, Some(home)).as_ptr(),
            held,
            "an unchanged pane hands back the string it already had",
        );

        let b = PathBuf::from("/repo/src/b.rs");
        assert_eq!(
            status_filename(&mut editor, Some(&b), root, Some(home)),
            "src/b.rs",
            "a renamed buffer re-renders",
        );
        assert_eq!(
            status_filename(&mut editor, Some(&b), Path::new("/repo/src"), Some(home)),
            "b.rs",
            "and so does a workspace that moved under it",
        );
        assert_eq!(
            status_filename(
                &mut editor,
                Some(Path::new("/home/u/n.md")),
                root,
                Some(home)
            ),
            "~/n.md",
            "and a home that now contains it",
        );
        assert_eq!(
            status_filename(&mut editor, Some(Path::new("/home/u/n.md")), root, None),
            "/home/u/n.md",
            "and a home that is no longer known",
        );
        assert_eq!(
            status_filename(&mut editor, None, root, Some(home)),
            "[scratch]",
            "a pathless buffer names itself",
        );
    }

    fn diag(severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 0,
                    character: 1,
                },
            },
            severity: Some(severity),
            message: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn lsp_short_name_abbreviates_server_names() {
        use super::lsp_short_name;
        assert_eq!(lsp_short_name("rust-analyzer"), "RA");
        assert_eq!(lsp_short_name("pyright"), "PY");
        assert_eq!(lsp_short_name("typescript-language-server"), "TLS");
        assert_eq!(lsp_short_name("gopls"), "GO");
    }

    fn open_rust(h: &mut crate::test_harness::TestHarness) {
        let root = PathBuf::from("/lsp");
        let path = root.join("a.rs");
        h.fake_fs().insert_file(&path, b"fn main() {}");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
    }

    fn bar_row(buf: &Buffer) -> String {
        let y = buf.area.height - 1;
        (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
    }

    /// A buffer outside the workspace falls back to `~`-abbreviating against the
    /// home the frame carries, which comes from the env host so a test controls
    /// it. Resolving it in the paint instead would read the real environment.
    #[test]
    fn a_pane_status_abbreviates_against_the_frames_home() {
        let paint = |home: Option<&str>| {
            let mut h = crate::test_harness::TestHarness::with_size(60, 8);
            if let Some(home) = home {
                h.fake_env().set("HOME", home);
                // The cached home resolves when the env host is set, so re-inject
                // it now that HOME is populated.
                h.stoat.set_env_host(h.fake_env().clone());
            }

            let path = PathBuf::from("/fixture-home/notes.txt");
            h.fake_fs().insert_file(&path, b"hello");
            h.stoat.active_workspace_mut().git_root = PathBuf::from("/elsewhere");
            dispatch(&mut h.stoat, &OpenFile { path });
            h.settle();
            bar_row(&h.render_composited())
        };

        assert!(
            paint(Some("/fixture-home")).contains("~/notes.txt"),
            "a path under the frame's home paints abbreviated, got {:?}",
            paint(Some("/fixture-home")),
        );
        assert!(
            paint(None).contains("/fixture-home/notes.txt"),
            "and stays whole with no home to measure against, got {:?}",
            paint(None),
        );
    }

    /// A shell or an agent in a pane names itself through its title, and the
    /// bar keeps the kind label until the child sets one.
    #[test]
    fn a_terminal_or_agent_pane_status_names_the_childs_title() {
        let kinds: [(SessionView, &str); 2] =
            [(View::Terminal, "[term]"), (View::Agent, "[agent]")];
        let bars: Vec<(bool, bool, bool)> = kinds
            .iter()
            .map(|&(view, label)| {
                let (before, after) = bar_before_and_after_a_title(view);
                (
                    before.contains(label),
                    after.contains("build"),
                    after.contains(label),
                )
            })
            .collect();
        assert_eq!(bars, [(true, true, false); 2]);
    }

    /// Show a fresh terminal session in the focused pane as `view`, and paint
    /// its bar before and after the child titles it `build`.
    fn bar_before_and_after_a_title(view: SessionView) -> (String, String) {
        let mut h = crate::test_harness::TestHarness::with_size(60, 8);
        dispatch(&mut h.stoat, &stoat_action::Terminal);
        let term_id = {
            let ws = h.stoat.active_workspace_mut();
            let focus = ws.panes.focus();
            let View::Terminal(term_id) = ws.panes.pane(focus).view else {
                panic!("the terminal action shows a terminal");
            };
            ws.panes.pane_mut(focus).view = view(term_id);
            term_id
        };
        let before = bar_row(&h.render_composited());
        h.stoat.active_workspace_mut().terms[term_id]
            .term
            .feed(b"\x1b]0;build\x07");
        (before, bar_row(&h.render_composited()))
    }

    /// Push a work-done progress begin so `fake`'s server reads as busy, painting
    /// its bar badge.
    fn mark_busy(h: &mut crate::test_harness::TestHarness, fake: &crate::host::FakeLsp) {
        use crate::host::LspNotification;
        use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};

        fake.push_notification(LspNotification::Progress {
            token: NumberOrString::Number(1),
            value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "indexing".into(),
                cancellable: None,
                message: None,
                percentage: None,
            }),
        });
        h.drain_lsp();
    }

    /// After the servers, the bar names each source the file's diagnostics come
    /// from, once, in the order they first appear.
    #[test]
    fn the_status_row_names_each_diagnostic_source_once() {
        let mut h = Stoat::test();
        h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);
        let sourced = |source: &str| Diagnostic {
            source: Some(source.to_string()),
            ..diag(DiagnosticSeverity::WARNING)
        };
        h.seed_diagnostics(
            "/lsp/a.rs",
            vec![sourced("rustc"), sourced("clippy"), sourced("rustc")],
        );

        let bar = bar_row(&h.render_composited());
        assert!(
            bar.contains(" rust-analyzer  rustc \u{b7} clippy "),
            "the server, then each source once:\n{bar}"
        );
    }

    /// An attached server shows its name while idle, with no spinner, and
    /// stamps the hover rect that opens its status.
    #[test]
    fn lsp_badge_names_an_idle_server() {
        let mut h = Stoat::test();
        h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);

        let buf = h.render_composited();
        let bar = bar_row(&buf);
        assert_eq!(
            (
                bar.contains(" rust-analyzer "),
                bar.contains('⠋'),
                h.stoat.lsp_badge_rect.is_some(),
            ),
            (true, false, true),
            "the name, no spinner, and the hover rect:\n{bar}"
        );
    }

    #[test]
    fn lsp_badge_shows_spinner_when_busy() {
        let mut h = Stoat::test();
        let fake = h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);
        mark_busy(&mut h, &fake);

        let buf = h.render_composited();
        let bar = bar_row(&buf);
        assert!(
            bar.contains("⠋ rust-analyzer "),
            "a busy server shows the spinner before its name:\n{bar}"
        );
    }

    #[test]
    fn diff_warm_shows_spinner_segment_while_busy() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 12);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "b\n")]);
        h.stoat.set_diff_warm_auto(true);
        // The warm is opt-in, so a spinner for it needs the option on.
        h.stoat.settings.review_precompute = Some(true);
        crate::diff_warm::ensure_diff_warm(&mut h.stoat);

        let buf = h.render_composited();
        let bar = bar_row(&buf);
        assert!(
            bar.contains("diff"),
            "a pending warm shows the diff segment:\n{bar}"
        );
        assert!(
            bar.contains('⠋'),
            "the diff segment shows the spinner glyph:\n{bar}"
        );

        h.settle();
        crate::diff_warm::install_finished(&mut h.stoat);
        let buf = h.render_composited();
        let bar = bar_row(&buf);
        assert!(
            !bar.contains("diff"),
            "the segment clears once the warm finishes:\n{bar}"
        );
    }

    /// The server list is held across frames, so installing a server has to
    /// invalidate it. Without that the bar would keep painting the list it
    /// derived before the server existed.
    #[test]
    fn a_server_installed_after_a_paint_reaches_the_bar() {
        let mut h = Stoat::test();
        open_rust(&mut h);

        let bar = bar_row(&h.render_composited());
        assert!(
            !bar.contains("rust-analyzer"),
            "no server is up yet:\n{bar}"
        );

        let fake = h.install_lsp_server("rust", "rust-analyzer");
        mark_busy(&mut h, &fake);
        let bar = bar_row(&h.render_composited());
        assert!(
            bar.contains("rust-analyzer"),
            "the newly installed server badges on the next paint:\n{bar}"
        );
    }

    /// The names are held across frames but the busy flags are not, since only
    /// they can move without the focus or the registry moving.
    #[test]
    fn a_server_going_busy_after_a_paint_shows_its_spinner() {
        let mut h = Stoat::test();
        let fake = h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);

        let bar = bar_row(&h.render_composited());
        assert!(
            bar.contains(" rust-analyzer ") && !bar.contains('⠋'),
            "an idle server paints its name with no spinner:\n{bar}"
        );

        mark_busy(&mut h, &fake);
        let bar = bar_row(&h.render_composited());
        assert!(
            bar.contains("⠋ rust-analyzer "),
            "the flag refreshes without the names being rebuilt:\n{bar}"
        );
    }

    #[test]
    fn lsp_badge_one_per_server() {
        let mut h = Stoat::test();
        let ra = h.install_lsp_server("rust", "rust-analyzer");
        let ss = h.install_lsp_server("rust", "second-server");
        open_rust(&mut h);
        mark_busy(&mut h, &ra);
        mark_busy(&mut h, &ss);

        let buf = h.render_composited();
        let bar = bar_row(&buf);
        assert!(
            bar.contains("rust-analyzer") && bar.contains("second-server"),
            "each running server gets its own badge:\n{bar}"
        );
    }

    /// The badge's hover rect comes from the status row, which a replayed
    /// focused pane still builds, so a hover over the badge still finds it.
    #[test]
    fn a_replayed_focused_pane_keeps_the_lsp_badge_rect() {
        let mut h = Stoat::test();
        let fake = h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);
        mark_busy(&mut h, &fake);
        // The first paint installs the wrap width, which moves the paint
        // version past the key it captured. The second captures the settled
        // key.
        let _ = h.stoat.render();
        let _ = h.stoat.render();
        let badge = h.stoat.lsp_badge_rect;
        assert!(badge.is_some(), "the busy server raises the badge");

        let painted_before = h.stoat.pane_paints;
        let _ = h.stoat.render();
        assert_eq!(
            (h.stoat.pane_paints - painted_before, h.stoat.lsp_badge_rect),
            (0, badge),
            "the replayed pane hands back the badge rect"
        );
    }

    #[test]
    fn lsp_badge_hidden_on_unfocused_pane() {
        let mut h = Stoat::test();
        let fake = h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);
        mark_busy(&mut h, &fake);
        h.type_action("SplitRight()");
        h.settle();

        let buf = h.render_composited();
        let rendered: String = (buf.area.y..buf.area.y + buf.area.height)
            .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_string())
            .collect();
        assert_eq!(
            rendered.matches("rust-analyzer").count(),
            1,
            "only the focused pane paints a badge:\n{rendered}"
        );
    }

    #[test]
    fn lsp_badge_rect_records_painted_columns() {
        let mut h = Stoat::test();
        let fake = h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);
        mark_busy(&mut h, &fake);

        let buf = h.render_composited();
        let rect = h.stoat.lsp_badge_rect.expect("badge rect stamped");
        let span: String = (rect.x..rect.x + rect.width)
            .map(|x| buf[(x, rect.y)].symbol())
            .collect();
        // Under cfg(test) every type role is a full cell, so the busy badge
        // (" ⠋ rust-analyzer ", 17 chars) covers 16 cells. The leftmost pad
        // space carries no glyph and is dropped.
        assert_eq!(
            rect.width, 16,
            "the rect is the scaled extent of the drawn glyphs, not their full char span"
        );
        assert!(
            span.contains("rust-analyzer"),
            "and covers the drawn badge glyph columns:\n{span}"
        );
    }

    fn full_render(h: &mut crate::test_harness::TestHarness) -> String {
        let buf = h.render_composited();
        (0..buf.area.height)
            .map(|y| {
                (0..buf.area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn toggle_lsp_status_shows_and_hides_the_card() {
        use crate::host::LspNotification;
        use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};

        let mut h = Stoat::test();
        let fake = h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);
        fake.push_notification(LspNotification::Progress {
            token: NumberOrString::Number(1),
            value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
                title: "indexing".into(),
                cancellable: None,
                message: None,
                percentage: None,
            }),
        });
        h.drain_lsp();

        assert!(
            !full_render(&mut h).contains("indexing"),
            "the card stays hidden until toggled open"
        );

        dispatch(&mut h.stoat, &stoat_action::ToggleLspStatus);
        assert!(
            full_render(&mut h).contains("indexing"),
            "toggling pins the card with the in-flight entry row"
        );

        dispatch(&mut h.stoat, &stoat_action::ToggleLspStatus);
        assert!(
            !full_render(&mut h).contains("indexing"),
            "toggling again hides the card"
        );
    }

    #[test]
    fn pinned_lsp_status_lists_idle_servers() {
        let mut h = Stoat::test();
        h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);
        dispatch(&mut h.stoat, &stoat_action::ToggleLspStatus);

        assert!(
            full_render(&mut h).contains("RA idle"),
            "pinned with no in-flight work lists each running server as idle"
        );
    }

    #[test]
    fn error_card_stacks_above_the_pinned_status_card() {
        use lsp_types::MessageType;

        let mut h = Stoat::test();
        h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);
        dispatch(&mut h.stoat, &stoat_action::ToggleLspStatus);
        h.stoat.lsp_message = Some((MessageType::ERROR, "workspace load failed".to_string()));

        let buf = h.render_composited();
        let rows: Vec<String> = (0..buf.area.height)
            .map(|y| (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect())
            .collect();
        let status_y = rows
            .iter()
            .position(|r| r.contains("RA idle"))
            .expect("pinned status card painted");
        let error_y = rows
            .iter()
            .position(|r| r.contains("workspace"))
            .expect("error card painted");
        assert!(
            error_y < status_y,
            "the error card auto-shows and stacks above the pinned status card"
        );
    }

    /// A server error of several lines paints as one line of the error card.
    /// A plain terminal takes the card text cell by cell, and flushing a frame
    /// with a raw newline in a cell trips ratatui's control-character check.
    #[test]
    fn a_multi_line_server_error_paints_as_one_line() {
        let mut h = Stoat::test();
        h.stoat.stoatty = false;
        let fake = h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);
        fake.push_notification(LspNotification::ShowMessage {
            typ: MessageType::ERROR,
            message: "failed to fetch workspace\n\nno Cargo.toml".to_string(),
        });
        drain::drain_lsp_notifications(&mut h.stoat);

        h.snapshot();
        let frame = h.rendered_buffer();
        // The same cell measure a terminal frontend runs when it flushes.
        Buffer::empty(frame.area).diff(frame);
        assert!(
            h.rendered_text()
                .contains("failed to fetch workspace no Cargo.toml"),
            "the error card joins the lines with one space",
        );
    }

    #[test]
    fn badge_hover_opens_the_status_card() {
        let mut h = Stoat::test();
        h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);

        assert!(
            !full_render(&mut h).contains("RA idle"),
            "no card without hover or pin"
        );

        h.stoat.lsp_badge_hovered = true;
        assert!(
            full_render(&mut h).contains("RA idle"),
            "hovering the badge opens the idle card"
        );

        h.stoat.lsp_badge_hovered = false;
        assert!(
            !full_render(&mut h).contains("RA idle"),
            "un-hovering closes the card"
        );

        h.stoat.lsp_status_pinned = true;
        assert!(
            full_render(&mut h).contains("RA idle"),
            "pinning keeps the card open regardless of hover"
        );
    }

    #[test]
    fn snapshot_status_bar_diagnostic_badge_warning_color() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-status");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.seed_diagnostics(path, vec![diag(DiagnosticSeverity::WARNING)]);
        h.assert_snapshot("status_bar_diagnostic_badge_warning_color");
    }

    /// The text the staged label's segments read, styles left out.
    fn staged_label(
        kind: MarkKind,
        base_lead: Option<&str>,
        repo: Option<(usize, usize)>,
        file: Option<(usize, usize)>,
        position: Option<&HunkPosition>,
    ) -> Option<String> {
        use ratatui::style::Style;

        let theme = crate::theme::Theme::empty();
        focused_staged_label(
            kind,
            base_lead,
            repo,
            file,
            position,
            (Style::default(), &theme),
        )
        .map(|segments| segments.into_iter().map(|(text, _)| text).collect())
    }

    /// The pencil count takes `ui.statusbar.unstaged` and the plus count
    /// `ui.statusbar.staged`, which the shipped theme reads as its accent and
    /// teal.
    #[test]
    fn the_staged_label_colors_each_count() {
        use ratatui::style::{Color, Style};

        let h = Stoat::test();
        let segments = focused_staged_label(
            MarkKind::Staged,
            None,
            None,
            Some((1, 4)),
            None,
            (Style::default(), &*h.stoat.theme),
        )
        .expect("a label");
        let colors: Vec<(String, Option<Color>)> = segments
            .into_iter()
            .map(|(text, style)| (text, style.fg))
            .collect();
        assert_eq!(
            colors,
            [
                (" \u{270e}4".to_string(), Some(Color::Rgb(0x7f, 0xb0, 0xff))),
                (" \u{271a}1".to_string(), Some(Color::Rgb(0x6c, 0xc7, 0xd6))),
                (" ".to_string(), None),
            ],
        );
    }

    #[test]
    fn the_staged_label_pairs_the_repo_hunks_with_the_focused_file() {
        assert_eq!(
            staged_label(MarkKind::Staged, None, Some((2, 3)), Some((1, 4)), None).as_deref(),
            Some(" repo 2 staged / 3 unstaged · \u{270e}4 \u{271a}1 "),
            "both pairs read side by side"
        );
        assert_eq!(
            staged_label(MarkKind::Staged, None, Some((2, 3)), None, None).as_deref(),
            Some(" repo 2 staged / 3 unstaged "),
            "a file with no diff map drops its half"
        );
        assert_eq!(
            staged_label(MarkKind::Staged, None, Some((2, 3)), Some((0, 0)), None).as_deref(),
            Some(" repo 2 staged / 3 unstaged "),
            "and so does a file whose diff map holds no hunks"
        );
        assert_eq!(
            staged_label(MarkKind::Staged, None, None, Some((1, 4)), None).as_deref(),
            Some(" \u{270e}4 \u{271a}1 "),
            "no repo tally yet falls back to the file's own counts"
        );
        assert_eq!(
            staged_label(MarkKind::Staged, None, None, None, None),
            None,
            "nothing to say without either"
        );
        assert_eq!(
            staged_label(MarkKind::Staged, None, Some((0, 0)), Some((0, 0)), None),
            None,
            "a clean repo and an unchanged file hide the segment"
        );
        assert_eq!(
            staged_label(MarkKind::Staged, None, Some((0, 0)), Some((1, 0)), None).as_deref(),
            Some(" repo 0 staged / 0 unstaged · \u{270e}0 \u{271a}1 "),
            "a stale zero tally still shows against a file that has hunks"
        );
    }

    /// A pair names its two files in its own pane's status row alone, and a git
    /// diff in another tab names the workspace base there. Neither row repeats
    /// the base as a lead, since its halves name it.
    #[test]
    fn each_diff_s_status_row_names_its_own_sides() {
        let mut h = crate::test_harness::TestHarness::with_size(120, 20);
        h.stage_review_scenario("/repo", &[("c.txt", "x\ny\nz\n", "x\ny\nZ\n")]);
        h.open_side_by_side(("/repo/a.txt", "one\ntwo\n"), ("/repo/b.txt", "one\nTWO\n"));
        dispatch(&mut h.stoat, &stoat_action::DiffPair);
        h.settle();
        dispatch(&mut h.stoat, &stoat_action::NewTab);
        h.open_file(Path::new("/repo/c.txt"));
        dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
        h.settle();

        let named = |h: &mut crate::test_harness::TestHarness| {
            h.snapshot();
            let text = h.rendered_text().replace('─', " ");
            [
                text.contains("\u{2212} HEAD") && text.contains("+ working tree"),
                text.contains("\u{2212} a.txt") && text.contains("+ b.txt"),
                text.contains("diff vs a.txt"),
            ]
        };
        let git_tab = named(&mut h);
        dispatch(&mut h.stoat, &stoat_action::PrevTab);
        h.settle();
        assert_eq!(
            [git_tab, named(&mut h)],
            [[true, false, false], [false, true, false]],
            "each tab's diff names its own sides",
        );
    }

    /// A split diff view's status row leads its base half with the mode chip,
    /// the screen label, and the hunk, and ends it with the base name at the
    /// divider. The target name and the position fill the target half.
    #[test]
    fn diff_view_status_halves() {
        let mut h = crate::test_harness::TestHarness::with_size(120, 8);
        h.stoat.minimap_override = Some(false);
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .set_diff_view(true);
        h.type_keys("j j j");
        h.assert_snapshot("diff_view_status_halves");
    }

    fn hunk_position(
        file_at: usize,
        file_total: usize,
        hunk_at: Option<usize>,
        hunk_total: usize,
    ) -> HunkPosition {
        HunkPosition {
            file_at,
            file_total,
            hunk_at,
            hunk_total,
        }
    }

    #[test]
    fn the_staged_label_closes_with_the_hunk_under_the_cursor() {
        let at_second_file = hunk_position(2, 4, Some(2), 7);
        assert_eq!(
            staged_label(
                MarkKind::Staged,
                None,
                Some((2, 3)),
                Some((1, 4)),
                Some(&at_second_file)
            )
            .as_deref(),
            Some(" repo 2 staged / 3 unstaged · file 2/4 · \u{270e}4 \u{271a}1 · hunk 2/7 "),
            "the file's place leads its counts, and the hunk position closes the label"
        );
        assert_eq!(
            staged_label(
                MarkKind::Staged,
                None,
                Some((2, 3)),
                None,
                Some(&at_second_file)
            )
            .as_deref(),
            Some(" repo 2 staged / 3 unstaged · file 2/4 · hunk 2/7 "),
            "a file with no hunks keeps its place and drops its counts"
        );
        assert_eq!(
            staged_label(
                MarkKind::Staged,
                None,
                Some((2, 3)),
                Some((1, 4)),
                Some(&hunk_position(2, 4, None, 7))
            )
            .as_deref(),
            Some(" repo 2 staged / 3 unstaged · file 2/4 · \u{270e}4 \u{271a}1 · hunk -/7 "),
            "a cursor above the first hunk stands at no position"
        );
        assert_eq!(
            staged_label(
                MarkKind::Staged,
                Some("diff vs abc1234"),
                None,
                Some((1, 4)),
                Some(&hunk_position(2, 4, Some(5), 13))
            )
            .as_deref(),
            Some(" diff vs abc1234 · file 2/4 · \u{270e}4 \u{271a}1 · hunk 5/13 "),
            "a named base carries the position too"
        );
        assert_eq!(
            staged_label(
                MarkKind::Staged,
                None,
                None,
                None,
                Some(&hunk_position(1, 1, Some(1), 3))
            ),
            None,
            "nothing to count hides the position with the rest of the label"
        );
    }

    #[test]
    fn a_diff_base_takes_the_repo_tally_s_place_in_the_label() {
        assert_eq!(
            staged_label(
                MarkKind::Staged,
                Some("diff vs abc1234"),
                Some((2, 3)),
                Some((1, 4)),
                None
            )
            .as_deref(),
            Some(" diff vs abc1234 · \u{270e}4 \u{271a}1 "),
            "the base replaces the repo pair rather than joining it"
        );
        assert_eq!(
            staged_label(
                MarkKind::Staged,
                Some("diff vs abc1234"),
                Some((2, 3)),
                None,
                None
            )
            .as_deref(),
            Some(" diff vs abc1234 "),
            "a file with no hunks still says what is under review"
        );
        assert_eq!(
            staged_label(
                MarkKind::Staged,
                Some("diff vs abc1234"),
                None,
                Some((0, 0)),
                None
            )
            .as_deref(),
            Some(" diff vs abc1234 "),
            "and so does one whose diff map holds none"
        );
    }

    /// Under a range review the file's counts are approvals, so each file part
    /// words them as approved and unapproved. The repo pair counts git status
    /// and keeps git's words.
    #[test]
    fn a_review_s_file_counts_read_as_approvals() {
        assert_eq!(
            [
                staged_label(
                    MarkKind::Approved,
                    Some("diff vs main base0"),
                    None,
                    Some((1, 4)),
                    None
                ),
                staged_label(
                    MarkKind::Approved,
                    None,
                    Some((2, 3)),
                    Some((1, 4)),
                    Some(&hunk_position(2, 4, Some(1), 5))
                ),
                staged_label(MarkKind::Approved, None, None, Some((1, 4)), None),
            ],
            [
                Some(" diff vs main base0 · file 1 approved / 4 unapproved ".to_string()),
                Some(
                    " repo 2 staged / 3 unstaged · file 2/4 · 1 approved / 4 unapproved · hunk 1/5 "
                        .to_string()
                ),
                Some(" 1 approved / 4 unapproved ".to_string()),
            ],
        );
    }

    #[test]
    fn every_displacing_base_names_itself() {
        assert_eq!(
            diff_base_lead(&DiffBase::Head),
            None,
            "the HEAD base is a working-tree pick, so the bar keeps the repo tally"
        );
        assert_eq!(
            diff_base_lead(&DiffBase::parent_of(
                "def5678abc1234",
                Some("abc1234def5678".into())
            )),
            Some("diff vs parent abc1234".to_string()),
            "a review names the parent it measures from"
        );
        assert_eq!(
            diff_base_lead(&DiffBase::parent_of("def5678abc1234", None)),
            Some("diff vs empty".to_string()),
            "a root commit's parent is the empty tree"
        );
        assert_eq!(
            diff_base_lead(&DiffBase::named("main", "abc1234def5678".into())),
            Some("diff vs main abc1234".to_string()),
            "a typed revision keeps its name"
        );
        assert_eq!(
            diff_base_lead(&DiffBase::Memory {
                files: HashMap::new()
            }),
            Some("diff vs original".to_string()),
            "a proposal has no revision, so it names what it replaced"
        );
    }

    /// Each kind of pair a diff's sides take reads at the divider, the base
    /// name flush against it on the left and the target name just past it.
    #[test]
    fn the_status_halves_name_each_kind_of_side_pair() {
        use ratatui::style::{Color, Style};

        let side = DiffSideColors {
            bg: Color::Black,
            fg: Color::Gray,
            name: Color::White,
            mark: Color::Red,
        };
        let colors = DiffSidesColors {
            base: side,
            target: side,
        };
        let row = Rect::new(0, 0, 120, 1);
        let painted = |names: (&str, &str)| {
            let [(base, _), (target, target_segments)] = split_halves(row, 59, None);
            let (base_right, target_left, _) = split_side_segments(
                names,
                colors,
                (Style::default(), Style::default()),
                (row.x, base.right()),
                target_segments,
            );
            let mut buf = Buffer::empty(row);
            paint_status_fallback(&mut buf, base, &[], &base_right);
            paint_status_fallback(&mut buf, target, &target_left, &[]);
            (0..row.width)
                .map(|x| buf[(x, 0)].symbol())
                .collect::<String>()
        };

        let pairs = [
            ("index", "working tree"),
            ("HEAD", "working tree"),
            ("main abc1234", "working tree"),
            ("abc1234", "working tree"),
            ("parent abc1234", "commit def5678"),
            ("empty", "commit def5678"),
            ("original", "proposal"),
            ("a.txt", "b.txt"),
            ("x/conf.toml", "y/conf.toml"),
        ];
        assert_eq!(
            pairs.map(painted),
            pairs.map(|(base, target)| format!(
                "{:>59} {:<60}",
                format!(" \u{2212} {base} "),
                format!(" + {target} ")
            )),
        );
    }

    /// The halves split around the divider's cell, and a badge box over the
    /// row's end takes the target half's segment room.
    #[test]
    fn the_status_halves_leave_the_divider_cell_between_them() {
        let row = Rect::new(2, 9, 120, 1);
        let badge = Rect::new(100, 9, 22, 1);
        assert_eq!(
            [
                split_halves(row, 61, None),
                split_halves(row, 61, Some(badge))
            ],
            [
                [
                    (Rect::new(2, 9, 59, 1), Rect::new(2, 9, 59, 1)),
                    (Rect::new(62, 9, 60, 1), Rect::new(62, 9, 60, 1)),
                ],
                [
                    (Rect::new(2, 9, 59, 1), Rect::new(2, 9, 59, 1)),
                    (Rect::new(62, 9, 60, 1), Rect::new(62, 9, 38, 1)),
                ],
            ],
        );
    }

    /// Ermine sets each half's colors through its `diff.sides` scopes, and a
    /// theme without them tints the status bar toward each side's status
    /// color.
    #[test]
    fn the_side_colors_take_the_theme_s_scopes_and_derive_the_rest() {
        use ratatui::style::Color;

        let (config, _) = stoat_config::parse(crate::app::DEFAULT_KEYMAP);
        let config = config.expect("the shipped config parses");
        let resolved = ["ermine_night", "default_dark"].map(|name| {
            let theme = crate::theme::Theme::from_config(&config, name).expect("theme loads");
            DiffSidesColors::resolve(&theme, theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED))
        });

        let rgb = |hex: u32| Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8);
        let side = |[bg, fg, name, mark]: [u32; 4]| DiffSideColors {
            bg: rgb(bg),
            fg: rgb(fg),
            name: rgb(name),
            mark: rgb(mark),
        };
        assert_eq!(
            resolved,
            [
                DiffSidesColors {
                    base: side([0x2a1d20, 0xa3898c, 0xf2b3b7, 0xf2777f]),
                    target: side([0x1d2a1f, 0x8ea78b, 0xbfe6bb, 0x8fd18a]),
                },
                DiffSidesColors {
                    base: side([0x726d81, 0xf6e5e6, 0xeca7ac, 0xe06c75]),
                    target: side([0x697782, 0xebf2e7, 0xc1dbaf, 0x98c379]),
                },
            ],
        );
    }

    #[test]
    fn statusline_shows_staged_and_unstaged_counts() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 12);
        h.stage_index_scenario(
            "/repo",
            &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
        );
        // The repo pair counts files off git status, where the file pair counts
        // hunks off the diff map, so the fixture registers a second file to
        // tell the two apart in the bar.
        h.fake_git().add_repo("/repo").staged_file("g.txt", "x\n");
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/f.txt"));
        h.settle_diff_jobs();
        h.snapshot();

        let buf = h.rendered_buffer();
        let rendered: String = (buf.area.y..buf.area.y + buf.area.height)
            .map(|y| {
                (buf.area.x..buf.area.x + buf.area.width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        // The composited bar joins segments with the status-line hairline rule, so
        // the inter-token spaces read back as `─`. Normalize them to recover the
        // logical segment text.
        assert!(
            rendered
                .replace('─', " ")
                .contains("repo 1 staged / 1 unstaged · \u{270e}1 \u{271a}1"),
            "statusline reports the repo's files and the focused file's hunks:\n{rendered}"
        );
    }

    /// Under a range review the focused file's map reads approvals, so the bar
    /// words its counts as approved and unapproved.
    #[test]
    fn statusline_words_a_review_s_counts_as_approvals() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 12);
        let root = PathBuf::from("/approvals");
        let path = root.join("f.txt");
        let (base, text) = ("a\nb\nc\nd\n", "a\nB\nc\nD\n");
        h.fake_fs().insert_file(&path, text.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        h.open_file(&path);
        let buffer_id = h.stoat.focused_editor_ids().expect("focused editor").1;
        let dm = DiffMap::from_structural_changes_staged(
            structural_diff::diff(base, text),
            Arc::new(base.to_string()),
            text,
            &std::iter::once(3..4).collect::<Vec<_>>(),
        )
        .with_mark_kind(MarkKind::Approved);
        h.stoat
            .active_workspace_mut()
            .install_test_diff_map(buffer_id, dm);
        h.snapshot();

        let rendered = h.rendered_text().replace('─', " ");
        assert_eq!(
            [
                rendered.contains("1 approved / 1 unapproved"),
                rendered.contains("1 staged / 1 unstaged"),
            ],
            [true, false],
            "the bar words the file's counts as approvals:\n{rendered}",
        );
    }

    /// A status message reports what the reader just did and leaves after a
    /// few seconds, while the staged summary stays. So the summary gives way
    /// while a message it leaves no room for is up. The two follow reports
    /// differ only in their last word, which a cut to the leftover room drops.
    ///
    /// The row is 70 cells wide, so the room left of the summary holds
    /// `saved` and not `follow changes off`.
    #[test]
    fn the_staged_summary_gives_way_to_a_message_it_leaves_no_room_for() {
        for stoatty in [true, false] {
            let mut h = crate::test_harness::TestHarness::with_size(70, 12);
            h.stoat.stoatty = stoatty;
            h.stage_index_scenario(
                "/repo",
                &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
            );
            h.stoat.set_diff_warm_auto(true);
            h.open_file(Path::new("/repo/f.txt"));
            h.stoat
                .active_workspace_mut()
                .set_worktree_base(WorktreeBase::Index);
            h.settle_diff_jobs();

            let mut bar = |message: &str| {
                h.stoat.set_status(message);
                let row = bar_row(&h.render_composited()).replace('─', " ");
                row.split_whitespace().collect::<Vec<_>>().join(" ")
            };
            assert_eq!(
                [bar("saved"), bar("follow changes off")],
                [
                    "NOR repo f.txt saved repo 0 staged / 1 unstaged · \u{270e}1 \u{271a}1 1:1",
                    "NOR repo f.txt follow changes off 1:1",
                ],
                "stoatty {stoatty}",
            );
        }
    }

    /// A changes view's status row is the diff view's row with the screen
    /// named `changes`, read through the focused excerpt. A walk puts the
    /// cursor on the first chunk, so the hunk reads `1 / 1`.
    #[test]
    fn the_changes_view_bar_reads_the_focused_excerpts_hunks() {
        let mut h = crate::test_harness::TestHarness::with_size(112, 12);
        h.stage_index_scenario(
            "/repo",
            &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/f.txt"));
        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Index);
        h.cover_with_diff_changes(false);
        h.settle_diff_jobs();
        h.type_keys("n");

        let row = bar_row(&h.render_composited()).replace('─', " ");
        let point = crate::test_harness::editor::focused_cursor_point(&mut h.stoat);
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>().join(" "),
            format!(
                "NOR changes hunk 1 / 1 \u{2212} index + working tree {}:{}",
                point.row + 1,
                point.column + 1,
            ),
        );
    }

    /// The changes view's status row reclaims the single-minimap band's width,
    /// as an editor pane's row does, so the cells under the band carry the
    /// row's target half.
    #[test]
    fn the_changes_view_bar_spans_the_minimap_band() {
        let mut h = crate::test_harness::TestHarness::with_size(200, 24);
        h.stoat.settings.editor_minimap = Some(stoat_config::MinimapMode::Single);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        h.cover_with_diff_changes(false);
        let buf = h.stoat.render();

        let band = h
            .stoat
            .single_minimap_rect
            .expect("single mode reserves a band");
        let target = h
            .stoat
            .theme
            .get(crate::theme::scope::DIFF_SIDES_TARGET)
            .bg
            .expect("the theme tints the target half");
        let row = band.y + band.height;
        assert_eq!(
            (band.x..band.x + band.width)
                .map(|x| buf[(x, row)].bg)
                .collect::<Vec<_>>(),
            vec![target; usize::from(band.width)],
        );
    }

    /// A changes view whose stack lists no file still names the sides it
    /// compares.
    #[test]
    fn the_changes_view_bar_names_an_empty_stack_s_sides() {
        let mut h = crate::test_harness::TestHarness::with_size(112, 12);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git().add_repo("/repo").head_file("a.txt", "a\n");
        h.fake_fs().insert_file("/repo/a.txt", "a\n");
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.txt"));
        h.cover_with_diff_changes(false);
        h.settle_diff_jobs();

        let row = bar_row(&h.render_composited()).replace('─', " ");
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>().join(" "),
            "NOR changes \u{2212} HEAD + working tree",
        );
    }

    /// Recording takes every keypress until it is toggled off, and the status
    /// bar is the only thing on screen that says so.
    ///
    /// The indicator has to leave with the recording too. One that stays
    /// claims the editor still takes keys it no longer takes.
    #[test]
    fn the_status_bar_shows_which_register_is_recording() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 12);
        h.seed_focused_buffer("hello");

        let bar_text = |h: &mut crate::test_harness::TestHarness| -> String {
            h.snapshot();
            let buf = h.rendered_buffer();
            (buf.area.y..buf.area.y + buf.area.height)
                .map(|y| {
                    (buf.area.x..buf.area.x + buf.area.width)
                        .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
                .replace('─', " ")
        };

        assert!(
            !bar_text(&mut h).contains("REC"),
            "nothing records yet, so nothing says so",
        );

        h.type_keys("\" a");
        h.type_keys("Q");
        let recording = bar_text(&mut h);
        assert!(
            recording.contains("REC a"),
            "the bar names the register being recorded into:\n{recording}",
        );

        h.type_keys("Q");
        let stopped = bar_text(&mut h);
        assert!(
            !stopped.contains("REC"),
            "and the indicator leaves with the recording:\n{stopped}",
        );
    }

    /// A command that waits on a buffer ends when the buffer leaves, and a quit
    /// over the buffer returns to a shell, so every pane that shows it names
    /// what waits.
    ///
    /// The mark has to leave with the waiter while the file stays on screen.
    #[test]
    fn the_status_bar_names_what_waits_on_a_held_buffer() {
        let mut h = crate::test_harness::TestHarness::with_size(160, 12);
        let path = PathBuf::from("/bridge/msg.txt");
        h.fake_fs().insert_file(&path, b"draft\n");
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/bridge");
        let uid = h.stoat.active_workspace().uid;
        let (hold, _outcome) = mpsc::unbounded_channel();
        h.stoat.handle_agent_control(AgentControl::Open {
            uid,
            client: 0,
            term: None,
            paths: vec![path],
            split: true,
            hold: Some(hold),
            done: oneshot::channel().0,
        });
        h.settle();
        dispatch(&mut h.stoat, &stoat_action::SplitRight);

        let names = |h: &mut crate::test_harness::TestHarness| {
            h.snapshot();
            let text = h.rendered_text();
            (
                text.matches("msg.txt").count(),
                text.matches("msg.txt [for agent] ").count(),
            )
        };
        assert_eq!(
            names(&mut h),
            (2, 2),
            "both panes on the file name its waiter"
        );

        h.stoat
            .handle_agent_control(AgentControl::ClientGone { uid, client: 0 });
        assert_eq!(names(&mut h), (2, 0), "the mark leaves with the waiter");
    }

    /// The search prompt is the only thing on screen that reports the query, so
    /// a user without it types blind.
    ///
    /// The sigil has to distinguish the two directions, and the prompt has to
    /// leave when the search does. A bar still showing `/foo` claims a prompt
    /// that no longer takes keys.
    #[test]
    fn the_status_bar_shows_the_search_query_while_it_is_typed() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 12);
        h.seed_focused_buffer("hello");

        let bar_text = |h: &mut crate::test_harness::TestHarness| -> String {
            h.snapshot();
            h.rendered_text().replace('─', " ")
        };

        h.type_keys("/ f o o");
        let typing = bar_text(&mut h);
        assert!(
            typing.contains("/foo"),
            "the bar reads back the forward query:\n{typing}",
        );

        // The caret has to invert the bar's colors rather than carry a REVERSED
        // modifier, because the rich status path resolves a segment to a color
        // pair and drops its modifiers.
        let buf = h.rendered_buffer();
        let row = buf.area.height - 1;
        let sigil = (0..buf.area.width)
            .find(|&x| buf[(x, row)].symbol() == "/")
            .expect("the prompt paints on the status row");
        let (query, caret) = (&buf[(sigil, row)], &buf[(sigil + 4, row)]);
        assert_eq!(
            (caret.fg, caret.bg),
            (query.bg, query.fg),
            "the caret cell past the query inverts the bar's own colors",
        );

        h.type_keys("escape");
        let cancelled = bar_text(&mut h);
        assert!(
            !cancelled.contains("/foo"),
            "and the prompt leaves with the search:\n{cancelled}",
        );

        h.type_keys("g ?");
        h.type_text("b");
        let reverse = bar_text(&mut h);
        assert!(
            reverse.contains("?b"),
            "a reverse search prompts with its own sigil:\n{reverse}",
        );
    }

    /// The focused pane's editor, whichever input holds focus.
    fn pane_editor(h: &crate::test_harness::TestHarness) -> &EditorState {
        let ws = h.stoat.active_workspace();
        let View::Editor(id) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the focused pane shows an editor");
        };
        &ws.editors[id]
    }

    /// Whether `buf` paints text column `col` of the focused pane's first text
    /// row as the theme's cursor block.
    fn block_at(h: &crate::test_harness::TestHarness, buf: &Buffer, col: u16) -> bool {
        let rect = pane_editor(h).text_rect.expect("the pane painted its text");
        let cell = &buf[(rect.x + col, rect.y)];
        let cursor = h.stoat.theme.cursor_style();
        cursor.bg.is_none_or(|bg| cell.bg == bg) && cell.modifier.contains(cursor.add_modifier)
    }

    /// Whether column zero paints as a block before a `/` search for `query`,
    /// and whether the match at column `col` does while the prompt is open and
    /// after Enter.
    fn block_triple(h: &mut crate::test_harness::TestHarness, query: &str, col: u16) -> [bool; 3] {
        h.snapshot();
        let before = block_at(h, h.rendered_buffer(), 0);
        h.type_keys("/");
        h.type_text(query);
        h.stoat.drive_background();
        h.snapshot();
        let during = block_at(h, h.rendered_buffer(), col);
        h.type_keys("enter");
        h.snapshot();
        [before, during, block_at(h, h.rendered_buffer(), col)]
    }

    /// The terminal draws the focused editor's primary cursor, so the pane
    /// leaves that cell alone. A search prompt takes the terminal cursor, so
    /// the previewed match paints its own block until the prompt closes.
    #[test]
    fn the_previewed_match_keeps_its_cursor_block_while_the_prompt_is_open() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        h.seed_focused_buffer("abc def");
        assert_eq!(block_triple(&mut h, "d", 4), [false, true, false]);
    }

    /// The diff view's right column takes the same cursor as a plain pane.
    #[test]
    fn a_diff_view_match_keeps_its_cursor_block_while_the_prompt_is_open() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 6);
        h.seed_focused_buffer("abc def");
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        assert_eq!(block_triple(&mut h, "d", 4), [false, true, false]);
    }

    /// The conflict view's center column takes the same cursor as a plain pane.
    #[test]
    fn a_conflict_view_match_keeps_its_cursor_block_while_the_prompt_is_open() {
        let mut h = crate::test_harness::TestHarness::with_size(90, 12);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .conflicted_file(
                "f.txt",
                Some("abc def\nbase\n"),
                Some("abc def\nours\n"),
                Some("abc def\ntheirs\n"),
            );
        dispatch(&mut h.stoat, &Conflict);
        h.settle();
        assert_eq!(block_triple(&mut h, "d", 4), [false, true, false]);
    }

    /// A prompt that opens over an insert-mode pane moves the terminal cursor
    /// off the pane and leaves the mode as it was. With no background pass, no
    /// preview runs either, so the terminal cursor is the one thing the next
    /// frame changes, and the pane repaints its primary cell as a block rather
    /// than replaying the paint that left the cell to the terminal.
    #[test]
    fn a_terminal_cursor_moving_off_the_pane_repaints_it() {
        let mut h = crate::test_harness::TestHarness::with_size(40, 6);
        h.seed_focused_buffer("abc def");
        h.type_keys("i");
        let delegated = h.stoat.render();
        dispatch(&mut h.stoat, &OpenSearchInput);
        let painted = h.stoat.render();
        assert_eq!(
            [block_at(&h, &delegated, 0), block_at(&h, &painted, 0)],
            [false, true],
        );
    }

    /// A bottom-right badge draws its chip on the status row, and whatever the
    /// bar put under the chip is painted over. A message loses its end there,
    /// and the cursor position goes entirely.
    ///
    /// The check runs for the rich bar under stoatty and for the cell fallback
    /// that a foreign terminal gets.
    #[test]
    fn the_right_segments_end_where_a_corner_badge_starts() {
        for stoatty in [true, false] {
            let mut h = crate::test_harness::TestHarness::with_size(100, 12);
            h.stoat.stoatty = stoatty;
            h.seed_focused_buffer("hello");
            h.snapshot();
            h.stoat.set_status("walkthrough 'empty' has no stops");
            corner_chip(&mut h);

            let buf = h.render_composited();

            // The 14-wide chip sits one column in from the right edge, so it
            // starts at column 85.
            let row = buf.area.height - 1;
            let expected = "walkthrough 'empty' has no stops 1:1 ";
            let start = 85 - expected.chars().count() as u16;
            let beside_the_box: String = (start..85)
                .map(|x| buf[(x, row)].symbol())
                .collect::<String>()
                .replace('─', " ");
            assert_eq!(beside_the_box, expected, "stoatty {stoatty}");
        }
    }

    /// A message longer than the room left of the badge is cut to that room.
    /// Cut to the room the whole row gives, it runs back over the mode and
    /// workspace segments in the cells, and the rich bar drops it for the
    /// overlap.
    #[test]
    fn a_long_message_is_cut_where_the_badge_starts() {
        for stoatty in [true, false] {
            let mut h = crate::test_harness::TestHarness::with_size(60, 12);
            h.stoat.stoatty = stoatty;
            h.seed_focused_buffer("hello");
            h.snapshot();
            h.stoat.set_status("walkthrough 'empty' has no stops");
            corner_chip(&mut h);

            let buf = h.render_composited();

            // The 14-wide chip starts at column 45 of 60.
            let row = buf.area.height - 1;
            let left_of_the_box: String = (0..45)
                .map(|x| buf[(x, row)].symbol())
                .collect::<String>()
                .replace('─', " ");
            assert_eq!(
                left_of_the_box, " NOR  (unnamed) [scratch] [+] walkthroug 1:1 ",
                "stoatty {stoatty}",
            );
        }
    }

    /// Put a 14-wide chip at the bottom right, from a 12-character label of a
    /// source no frame sync takes down.
    fn corner_chip(h: &mut crate::test_harness::TestHarness) {
        h.stoat.active_workspace_mut().badges.insert(Badge {
            source: BadgeSource::Review,
            anchor: Anchor::BottomRight,
            state: BadgeState::Complete,
            label: "reviewed 1/3".to_owned(),
            detail: None,
        });
    }

    /// A read-only buffer's status row reads `name [readonly] [+]`, the
    /// read-only tag between the name and the modified tag.
    #[test]
    fn the_status_row_tags_a_read_only_buffer() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 12);
        let root = PathBuf::from("/ro");
        let path = root.join("file.txt");
        h.fake_fs().insert_file(&path, b"text\n");
        h.fake_fs().set_read_only(&path, true);
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.edit_focused(4..4, "!");

        let buf = h.render_composited();

        let row = buf.area.height - 1;
        let bar = (0..buf.area.width)
            .map(|x| buf[(x, row)].symbol())
            .collect::<String>()
            .replace('─', " ");
        assert_eq!(
            bar,
            format!("{:<56}1:1 ", " NOR  ro file.txt [readonly] [+]")
        );
    }

    /// The bottom row of `area` in `buf`, with the composite's blank bar cells
    /// read as spaces.
    fn status_row(buf: &Buffer, area: Rect) -> String {
        let row = area.y + area.height - 1;
        (area.x..area.x + area.width)
            .map(|x| buf[(x, row)].symbol())
            .collect::<String>()
            .replace('─', " ")
    }

    /// Several cursors show as `{count} sel` after the file name, and a lone
    /// cursor shows no count.
    #[test]
    fn the_status_row_counts_the_cursors() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 12);
        h.seed_focused_buffer("foo\nbar\n");

        dispatch(&mut h.stoat, &AddSelectionBelow);
        let two = {
            let buf = h.render_composited();
            status_row(&buf, buf.area)
        };
        dispatch(&mut h.stoat, &KeepPrimarySelection);
        let buf = h.render_composited();
        assert_eq!(
            [two, status_row(&buf, buf.area)],
            [
                format!("{:<56}2:1 ", " NOR  (unnamed) [scratch] [+]  2 sel "),
                format!("{:<56}2:1 ", " NOR  (unnamed) [scratch] [+]"),
            ],
        );
    }

    /// An open picker shows the prompt chip and its own name where a screen
    /// label sits. A closed picker gives the row back to the mode. The search
    /// input has no name, so its row keeps the mode chip.
    #[test]
    fn the_status_row_names_an_open_picker() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 12);
        h.seed_focused_buffer("foo\nbar\n");
        let row = |h: &mut crate::test_harness::TestHarness| {
            let buf = h.render_composited();
            status_row(&buf, buf.area)
        };

        h.type_keys("space p");
        let open = row(&mut h);
        h.type_keys("escape");
        let closed = row(&mut h);
        dispatch(&mut h.stoat, &OpenSearchInput);
        assert_eq!(
            [open, closed, row(&mut h)],
            [
                format!("{:<56}1:1 ", " PMT  (unnamed)  find file [scratch] [+]"),
                format!("{:<56}1:1 ", " NOR  (unnamed) [scratch] [+]"),
                format!("{:<56}1:1 ", " INS /"),
            ],
        );
    }

    /// A focused overlay bar names an open picker in place of its screen, and
    /// names the screen again once the picker closes. An unfocused bar shows no
    /// prompt chip, so it keeps its screen under an open picker.
    #[test]
    fn the_overlay_bar_names_an_open_picker() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 12);
        h.stoat.active_workspace_mut().rebase = Some(RebaseState::new(
            PathBuf::from("/overlay"),
            "onto".into(),
            vec![],
        ));
        let row = |h: &mut crate::test_harness::TestHarness| {
            let buf = h.render_composited();
            status_row(&buf, buf.area)
        };

        h.type_keys(":");
        let open = row(&mut h);
        h.type_keys("escape");
        let closed = row(&mut h);

        h.type_keys(":");
        let ws = h.stoat.active_workspace_mut();
        let dock = ws.docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Hidden,
            default_width: 30,
            area: Rect::default(),
        });
        ws.focus = FocusTarget::Dock(dock);
        assert_eq!(
            [open, closed, row(&mut h)],
            [
                format!("{:<60}", " PMT  (unnamed) palette"),
                format!("{:<60}", " NOR  (unnamed) rebase"),
                format!("{:<60}", " rebase"),
            ],
        );
    }

    /// An unfocused pane keeps its cursors and shows its position, so it shows
    /// its cursor count too.
    #[test]
    fn an_unfocused_pane_counts_its_cursors() {
        let mut h = crate::test_harness::TestHarness::with_size(120, 12);
        h.seed_focused_buffer("foo\nbar\n");
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &AddSelectionAbove);
        let counted = h.stoat.active_workspace().panes.focus();
        h.type_action("SplitRight()");

        let buf = h.render_composited();
        let ws = h.stoat.active_workspace();
        let bar = status_row(&buf, ws.panes.pane(counted).area);
        let width = bar.chars().count() - "1:1 ".len();
        assert_eq!(
            (ws.panes.focus() != counted, bar.clone()),
            (true, format!("{:<width$}1:1 ", " [scratch] [+]  2 sel ")),
            "the split moves the focus away, and the pane keeps its count",
        );
    }

    #[test]
    fn dim_pane_content_blends_rgb_and_skips_indexed() {
        use super::dim_pane_content;
        use crate::render::paint::dim_rgb;
        use ratatui::{
            buffer::Buffer,
            layout::Rect,
            style::{Color, Style},
        };

        let area = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        buf[(0, 0)].set_style(
            Style::default()
                .fg(Color::Rgb(200, 100, 40))
                .bg(Color::Rgb(10, 20, 30)),
        );
        buf[(1, 0)].set_style(Style::default().fg(Color::Blue));

        let bg = [0, 0, 0];
        dim_pane_content(&mut buf, area, bg, 0.5);

        let [fr, fg_, fb] = dim_rgb([200, 100, 40], bg, 0.5);
        assert_eq!(
            buf[(0, 0)].fg,
            Color::Rgb(fr, fg_, fb),
            "rgb fg dims toward bg"
        );
        let [br, bgc, bb] = dim_rgb([10, 20, 30], bg, 0.5);
        assert_eq!(
            buf[(0, 0)].bg,
            Color::Rgb(br, bgc, bb),
            "rgb bg dims toward bg"
        );
        assert_eq!(buf[(1, 0)].fg, Color::Blue, "indexed color left unchanged");
    }

    /// The memo answers a repeat of the colour before it, so what it must not
    /// do is answer a repeat of one further back, or carry an answer across a
    /// cell it never blended.
    #[test]
    fn dim_pane_content_matches_blending_every_cell() {
        use super::dim_pane_content;
        use crate::render::paint::dim_rgb;
        use ratatui::{
            buffer::Buffer,
            layout::Rect,
            style::{Color, Style},
        };

        let area = Rect::new(0, 0, 6, 3);
        let bg = [20, 20, 30];
        let amount = 0.25;

        // Runs of one colour, alternation between two, a foreground repeating
        // under a changing background, and indexed cells breaking the runs.
        let palette = [
            Some([200, 100, 40]),
            Some([200, 100, 40]),
            Some([10, 200, 90]),
            None,
            Some([200, 100, 40]),
            Some([10, 200, 90]),
        ];
        let mut buf = Buffer::empty(area);
        for y in 0..area.height {
            for x in 0..area.width {
                let fg = palette[usize::from(x)];
                let cell_bg = palette[usize::from((x + y) % area.width)];
                let mut style = Style::default();
                style = match fg {
                    Some([r, g, b]) => style.fg(Color::Rgb(r, g, b)),
                    None => style.fg(Color::Blue),
                };
                style = match cell_bg {
                    Some([r, g, b]) => style.bg(Color::Rgb(r, g, b)),
                    None => style.bg(Color::Green),
                };
                buf[(x, y)].set_style(style);
            }
        }

        let mut expected = buf.clone();
        for y in area.top()..area.bottom() {
            for x in area.left()..area.right() {
                let cell = &mut expected[(x, y)];
                if let Color::Rgb(r, g, b) = cell.fg {
                    let [r, g, b] = dim_rgb([r, g, b], bg, amount);
                    cell.set_fg(Color::Rgb(r, g, b));
                }
                if let Color::Rgb(r, g, b) = cell.bg {
                    let [r, g, b] = dim_rgb([r, g, b], bg, amount);
                    cell.set_bg(Color::Rgb(r, g, b));
                }
            }
        }

        dim_pane_content(&mut buf, area, bg, amount);

        assert_eq!(
            buf, expected,
            "the memoized pass agrees with blending every cell"
        );
    }

    #[test]
    fn dim_pane_content_zero_amount_is_identity() {
        use super::dim_pane_content;
        use ratatui::{
            buffer::Buffer,
            layout::Rect,
            style::{Color, Style},
        };

        let area = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        buf[(0, 0)].set_style(
            Style::default()
                .fg(Color::Rgb(200, 100, 40))
                .bg(Color::Rgb(10, 20, 30)),
        );
        let before = buf.clone();
        dim_pane_content(&mut buf, area, [0, 0, 0], 0.0);
        assert_eq!(buf, before, "amount 0 leaves the pane byte-identical");
    }

    /// The pane stands in for a picture, so it has to say which file and how
    /// large, and say what is missing where nothing can draw it.
    #[test]
    fn an_image_pane_names_the_file_and_its_size() {
        use ratatui::{buffer::Buffer, layout::Rect, style::Style};
        use std::path::Path;

        let painted = |capable| {
            let area = Rect::new(0, 0, 40, 5);
            let mut buf = Buffer::empty(area);
            super::render_image_pane(
                area,
                &mut buf,
                Path::new("/w/pic.png"),
                (640, 480),
                Style::default(),
                capable,
            );
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .map(|x| buf[(x, y)].symbol().to_string())
                        .collect::<String>()
                        .trim()
                        .to_owned()
                })
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
        };

        assert_eq!(
            painted(true),
            ["pic.png - 640x480 px"],
            "a terminal that can draw one needs only the label",
        );
        assert_eq!(
            painted(false),
            ["pic.png - 640x480 px", "image display needs stoatty"],
            "and one that cannot is told why the picture is missing",
        );
    }

    /// The scale is a literal, because every type role answers a full cell
    /// under `cfg(test)`.
    #[test]
    fn tab_spans_follow_the_scale_and_stop_at_the_bar_edge() {
        let spans_at = |(scale, width): (u16, u16)| {
            let mut spans = Vec::new();
            tab_spans(
                [(Some(0), 5), (Some(1), 7)].into_iter(),
                scale,
                width,
                &mut spans,
            );
            spans
                .iter()
                .map(|span| (span.start, span.end))
                .collect::<Vec<_>>()
        };

        assert_eq!(
            [(160, 20), (256, 20), (160, 6), (160, 3)].map(spans_at),
            [
                vec![(0, 50), (50, 120)],
                vec![(0, 80), (80, 192)],
                vec![(0, 50), (50, 96)],
                vec![(0, 48)],
            ],
        );
    }

    /// The workspace name takes room but makes no span, and a tab's label, dot,
    /// and tag join into one span.
    #[test]
    fn tab_spans_skip_the_name_and_join_a_tabs_segments() {
        let mut spans = Vec::new();
        tab_spans(
            [(None, 4), (Some(0), 5), (Some(0), 1), (Some(1), 7)].into_iter(),
            160,
            30,
            &mut spans,
        );
        assert_eq!(spans, [40..100, 100..170]);
    }

    /// The bar leads with the workspace name and a one-cell gap. A tagged file's
    /// tab trades its extension for the tag after a dot.
    #[test]
    fn the_tab_bar_leads_with_the_workspace_and_tags_each_file() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 8);
        h.stoat.active_workspace_mut().name = "ws".to_string();
        let chrome = h.write_file("chrome.rs", "fn a() {}\n");
        h.open_file(&chrome);
        dispatch(&mut h.stoat, &stoat_action::NewTab);
        let panel = h.write_file("panel.rs", "fn b() {}\n");
        h.open_file(&panel);
        h.snapshot();

        let text = h.rendered_text();
        let bar = text.lines().next().expect("the tab bar row");
        assert_eq!(
            bar.trim_end_matches('─'),
            " ws   chrome \u{b7} rs  panel \u{b7} rs "
        );
    }

    /// The status row names the workspace until a second tab shows the tab
    /// bar, which then carries the name.
    #[test]
    fn the_status_row_leaves_the_workspace_name_to_the_tab_bar() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 8);
        h.stoat.active_workspace_mut().name = "ws".to_string();
        // The left segments, which end where the bar's hairline run starts.
        let status_left = |h: &mut crate::test_harness::TestHarness| {
            h.snapshot();
            let text = h.rendered_text();
            let row = text.lines().last().expect("the status row");
            row.split('─').next().unwrap_or_default().to_string()
        };

        let one_tab = status_left(&mut h);
        dispatch(&mut h.stoat, &stoat_action::NewTab);
        let two_tabs = status_left(&mut h);
        assert_eq!(
            [one_tab, two_tabs],
            [" NOR  ws [scratch] ", " NOR [scratch] "]
        );
    }
}
