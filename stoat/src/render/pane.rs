use crate::{
    action_handlers::search::StatusPrompt,
    app::SPINNER_FRAMES,
    buffer::BufferId,
    buffer_registry::{BufferRegistry, ReadOnly},
    diff_changes::{DiffChangesId, DiffChangesState},
    diff_map::MarkKind,
    display_map::FoldScope,
    editor_state::{EditorId, EditorState},
    minimap::RAIL_MAX_COLUMNS,
    pane::{Divider, DividerOrientation, FocusTarget, Pane, PaneTree, Placement, View},
    render::{
        chrome, commits, diff_changes,
        editor::{editor_cursor_position, render_editor_with_overlay, split_minimap_strip},
        paint::{dim_rgb, style_rgb},
        popout::{
            paint_popout_card, popout_area, popout_card_bg, popout_inset, scaled_char_capacity,
            wrap_popout_lines,
        },
        rail_strip_id,
        review::{self, FrameEdges},
        run_pane::render_run_pane,
        term_pane::render_term_pane,
        text,
        undercurl::UndercurlBatch,
        FrameCtx, MinimapChrome, PaneCtx,
    },
    run::{RunId, RunState},
    term_session::{TermId, TermSession},
    theme::{PanelRole, TypeRole, FULL_CELL_SCALE},
    workspace::{
        diff::{self, DiffBase, HunkPosition, RepoStatus},
        TabConflictMark, Workspace,
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
    bar::Bar,
    minimap::Minimap,
    panel::Panel,
    status_bar::{self, SegmentBox, StatusBar, StatusSegment},
    text_run::{self, TextRun},
    ApcScene,
};
use stoatty_protocol::command::{BorderStyle, Edge, PanelShadow};

/// Buffer lines the minimap strip draws per vertical cell.
///
/// It matches [`crate::minimap::LINES_PER_CELL`], which maps a click on the
/// strip back to a line.
pub(super) const MINIMAP_LINES_PER_CELL: u8 = 6;

/// Widest line, in minimap columns, a strip renders before clipping.
pub(super) const MINIMAP_MAX_COLUMNS: u8 = 120;

/// The corner radius of a strip's viewport band, in logical px, as the Editor
/// and ReadOnly boards round it.
pub(super) const MINIMAP_THUMB_RADIUS: u8 = 4;

/// How far a diff half's fill moves from the status bar's toward the side's
/// status color, for a theme that sets no `diff.sides` fill.
const SIDE_FILL_TINT: f32 = 0.12;

/// How far a diff half's text moves from the bar's toward the status color.
const SIDE_TEXT_TINT: f32 = 0.15;

/// How far a side's name lifts from its status color toward white.
const SIDE_NAME_LIFT: f32 = 0.4;

/// The tab bar's conflict chip's `padding: 0 8px`, in sixteenths.
const CHIP_PAD: u16 = 8;

/// The top of the hairline that parts the tab bar's workspace name from its
/// tabs, in sixteenths down the row.
const TAB_DIVIDER_TOP: i16 = 2;

/// The height of that hairline in sixteenths, which centers it in the row.
///
/// The Editor board draws a 14 px rule, and a sixteenth of the 18 px row is
/// 1.125 px, so 12 sixteenths are the nearest whole count.
const TAB_DIVIDER_HEIGHT: u16 = 12;

/// The logical pixels the boards leave between an island and the window's
/// side, and between two sibling islands.
const ISLAND_SPACING: u8 = 8;

/// The corner radius of a read-only editor's dashed ring, which the ReadOnly
/// board draws as a 2 px curve whatever radius the islands take.
pub(crate) const READ_ONLY_RING_RADIUS: u8 = 2;

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_pane(
    pane: &Pane,
    is_focused: bool,
    ctx: PaneCtx<'_>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    undercurls: &mut UndercurlBatch,
) {
    let theme = frame.theme;
    let text_style = theme.get(crate::theme::scope::UI_TEXT);
    let content_area = pane.area;
    let anchor = card_anchor(pane.area, frame.minimap_band);

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
                if editor.diff_view {
                    emit_pane_base_frame(content_area, FrameEdges::Sides, frame, scene);
                }

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
                if editor.diff_view {
                    emit_pane_base_frame(content_area, FrameEdges::Top, frame, scene);
                }
                if let Some(cell) = editor.ghost_cursor_cell.filter(|_| scene.live()) {
                    review::emit_ghost_cursor(
                        cell,
                        (pane.index, editor.scroll_row as f32),
                        theme,
                        scene,
                    );
                }
                if !editor.diff_view
                    && editor.conflict_view.is_none()
                    && let Some(kind) = buffers.read_only(editor.buffer_id)
                    && let Some(text_area) = editor.text_rect
                {
                    render_read_only_frame(text_area, kind, frame, buf, scene);
                }

                if let (Some(strip), Some(chrome)) = (editor.minimap_rect, frame.minimap_chrome)
                    && let Some((content_id, rail)) = chrome.editor_strip(*editor_id, editor)
                {
                    let (strip, rail_rect) = split_rail(strip, rail.is_some());
                    pane_minimap(pane, chrome, content_id, is_focused, frame)
                        .render(strip, buf, scene);
                    if let (Some(rail_id), Some(rail_rect)) = (rail, rail_rect) {
                        rail_minimap(pane.index, rail_id, chrome).render(rail_rect, buf, scene);
                    }
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
                    scene,
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
        // The window row names the commits screen in place of a pane status,
        // so the LSP popout that sits over a pane status stays off this view.
        View::Commits(id) => {
            if let Some(state) = commit_lists.get_mut(*id) {
                commits::render_commits(pane, state, frame, buf, &mut *scene);
            }
            dim_pane(pane.area, is_focused, frame, buf);
            return;
        },
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
                // The stack's rows draw the frame's sides between the headers,
                // on the live paint and on a held stack's pages alike. The top
                // goes out after the rows, over the first row's hairline, and
                // it alone crosses the first header. An empty stack shows no
                // base column to frame.
                if !state.excerpts.is_empty() {
                    emit_pane_base_frame(content_area, FrameEdges::Top, frame, scene);
                }
                if let (Some(strip), Some(chrome)) = (state.minimap_rect, frame.minimap_chrome)
                    && let Some(content_id) = chrome.diff_changes_content
                {
                    let rail = chrome.diff_changes_rail;
                    let (strip, rail_rect) = split_rail(strip, rail.is_some());
                    pane_minimap(pane, chrome, content_id, is_focused, frame)
                        .render(strip, buf, scene);
                    if let (Some(rail_id), Some(rail_rect)) = (rail, rail_rect) {
                        rail_minimap(pane.index, rail_id, chrome).render(rail_rect, buf, scene);
                    }
                }
            }
            dim_pane(pane.area, is_focused, frame, buf);
            return;
        },
    }

    dim_pane(content_area, is_focused, frame, buf);

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
            && let Some(area) = popout_area(anchor, content_area, rows.len() as u16, 0)
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
        let cell_width = anchor.width.saturating_sub(inset * 2).saturating_sub(2) as usize;
        let scale = theme.type_scale(TypeRole::Ui);
        let width = scaled_char_capacity(cell_width, scale);
        let lines = wrap_popout_lines(msg, width, 4);
        if !lines.is_empty()
            && let Some(area) = popout_area(anchor, content_area, lines.len() as u16, status_rows)
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
/// The halo is a backdrop panel over the content in the editor surface's fill,
/// with the island role's radius and shadow. A backdrop paints its fill only
/// outside its cells, so the pane's tints and its pool composite keep drawing
/// inside it. A pane flush against the single-minimap band, over the band's own
/// rows, takes the band into its island, so the band sits inside an island and
/// not on the window ground.
///
/// The island reaches past its cells as [`island_edges`] sets its sides, so the
/// strips it reaches across take the surface. Its border is the window ground,
/// which the renderer paints over the corner arcs inside the pane's cells.
pub(crate) fn render_island(content_area: Rect, frame: FrameCtx<'_>, scene: &mut ApcScene) {
    let theme = frame.theme;
    let Some(ground) = style_rgb(crate::render::ground_surface(theme)) else {
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
    let surface = style_rgb(
        theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|style| style.bg),
    );
    Panel {
        style: BorderStyle::None_,
        border: ground,
        corner_radius: role.corner_radius,
        fill: surface,
        shadow: role.shadow,
        edges: island_edges(area, frame, role.inset_x),
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: false,
        backdrop: true,
        anchor: None,
    }
    .draw_components(area, scene);
}

/// The sides of the island over `area`, in [`Edge`] order: top, right,
/// bottom, then left.
///
/// A left or right side that meets the window's chrome margin reaches into it,
/// and one that meets the gap toward a sibling pane reaches into that gap, so
/// the island stands [`ISLAND_SPACING`] pixels from the window and from its
/// siblings. A gap divider counts when it spans the island's rows. The pane
/// root runs on over the single minimap band, which the island beside it
/// takes, and no island reaches over the band. Any other left or right side
/// keeps the island role's `inset_x`.
///
/// The top and the bottom stay flush, since each meets a status row or the
/// tab bar, and never a gap.
fn island_edges(area: Rect, frame: FrameCtx<'_>, inset_x: u8) -> [Edge; 4] {
    let root = frame.pane_root;
    let root_right = frame
        .minimap_band
        .map_or(root.right(), |band| band.right().max(root.right()));
    let vertical = |x: u16| {
        frame.dividers.iter().any(|divider| {
            divider.orientation == DividerOrientation::Vertical
                && divider.x == x
                && divider.y <= area.y
                && divider.y + divider.len >= area.bottom()
        })
    };
    let side = |margin: bool, gap: bool, rest: Edge| match (margin, gap) {
        (true, _) => Edge::Margin(ISLAND_SPACING),
        (false, true) => Edge::Gap(ISLAND_SPACING),
        (false, false) => rest,
    };

    [
        Edge::Inset(0),
        side(
            area.right() == root_right,
            vertical(area.right()),
            Edge::Inset(inset_x),
        ),
        Edge::Inset(0),
        side(
            area.x == root.x,
            area.x.checked_sub(1).is_some_and(vertical),
            Edge::Inset(inset_x),
        ),
    ]
}

/// Declare the dashed ring and the label of a read-only editor whose text
/// fills `text_area`.
///
/// The ring is a backdrop panel one cell narrower than the text, so the pane's
/// pool composite and its decorations still draw inside it. The label names
/// `kind` on a chip in the first row, one cell inside the ring's right end. The
/// chip's panel follows the ring, because a later panel's box hides an earlier
/// panel's stroke, and that is how the label cuts the ring's top edge. A ring
/// too narrow for the chip draws no label.
///
/// Only stoatty draws either, at [`BACKDROP_PANEL_PROTOCOL`] or later, under a
/// theme that colors `ui.readonly.frame`, `ui.readonly.label`, and the editor
/// surface.
///
/// [`BACKDROP_PANEL_PROTOCOL`]: stoatty_protocol::BACKDROP_PANEL_PROTOCOL
fn render_read_only_frame(
    text_area: Rect,
    kind: ReadOnly,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    if !scene.live() || !frame.backdrop_panels {
        return;
    }
    let theme = frame.theme;
    let fg = |name: &str| style_rgb(theme.try_get(name).and_then(|style| style.fg));
    let (Some(ring_rgb), Some(label_rgb), Some(base_rgb)) = (
        fg(crate::theme::scope::UI_READONLY_FRAME),
        fg(crate::theme::scope::UI_READONLY_LABEL),
        style_rgb(
            theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|style| style.bg),
        ),
    ) else {
        return;
    };

    let ring = Rect {
        width: text_area.width.saturating_sub(1),
        ..text_area
    };
    Panel {
        style: BorderStyle::Dashed,
        border: ring_rgb,
        corner_radius: READ_ONLY_RING_RADIUS,
        fill: None,
        shadow: PanelShadow::None_,
        edges: Edge::symmetric(0, 0),
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: false,
        backdrop: true,
        anchor: None,
    }
    .draw_components(ring, scene);

    let label = match kind {
        ReadOnly::Compressed => "read only \u{b7} gzip",
        ReadOnly::Permission => "read only \u{b7} no write access",
    };
    let chip_width = label.chars().count() as u16 + 2;
    let Some(x) = ring
        .right()
        .checked_sub(chip_width + 1)
        .filter(|&x| x > ring.x)
    else {
        return;
    };

    // A cell glyph draws above a panel's fill, so the text under the chip
    // blanks first.
    let [r, g, b] = base_rgb;
    let blank = Style::default().bg(Color::Rgb(r, g, b));
    for col in x..x + chip_width {
        text::write_cell(buf, col, ring.y, ' ', blank);
    }
    Panel {
        style: BorderStyle::None_,
        border: label_rgb,
        corner_radius: 0,
        fill: Some(base_rgb),
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
    .draw_components(Rect::new(x, ring.y, chip_width, 1), scene);
    TextRun {
        col: 0,
        row: 0,
        scale: theme.type_scale(TypeRole::Small),
        color: label_rgb,
        bg: None,
        text: label,
        bold: false,
        follow: 0,
        anchor: None,
    }
    .render(Rect::new(x + 1, ring.y, 1, 1), buf, scene);
}

/// The per-pane minimap strip that `pane` declares over content store
/// `content_id`, in the syntax palette.
///
/// A dimmed pane's palette, thumb, and thumb outline blend toward the theme
/// background at the pane's dim, as the rest of the pane does. The thumb and
/// its outline keep their alpha.
fn pane_minimap<'a>(
    pane: &Pane,
    chrome: MinimapChrome<'a>,
    content_id: u32,
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
    let thumb = blend([tr, tg, tb]);
    let [or, og, ob, oa] = chrome.thumb_border;
    let outline = blend([or, og, ob]);
    let palette = match dim > 0.0 {
        true => chrome.dimmed_palette,
        false => chrome.palette,
    };
    Minimap {
        strip_id: pane.index,
        content_id,
        lines_per_cell: MINIMAP_LINES_PER_CELL,
        max_columns: MINIMAP_MAX_COLUMNS,
        bg: [0, 0, 0, 0],
        thumb: [thumb[0], thumb[1], thumb[2], ta],
        thumb_border: Some([outline[0], outline[1], outline[2], oa]),
        rule_class: chrome.rule_class,
        thumb_radius: MINIMAP_THUMB_RADIUS,
        palette,
    }
}

/// The Coverage Rail beside strip `strip_id`, over the rail store `rail_id`.
///
/// The rail draws the whole file on the strip's rows in the class table's
/// palette, on the rail's own ground, with its thumb over the part of the
/// file the strip covers. It has no outline and no rule.
pub(super) fn rail_minimap<'a>(
    strip_id: u32,
    rail_id: u32,
    chrome: MinimapChrome<'a>,
) -> Minimap<'a> {
    Minimap {
        strip_id: rail_strip_id(strip_id),
        content_id: rail_id,
        lines_per_cell: MINIMAP_LINES_PER_CELL,
        max_columns: RAIL_MAX_COLUMNS,
        bg: chrome.rail_ground,
        thumb: chrome.rail_cover,
        thumb_border: None,
        rule_class: 0,
        thumb_radius: 0,
        palette: chrome.palette,
    }
}

/// `strip` split into the strip and the Coverage Rail in its last cell when
/// `rail` holds, else `strip` whole with no rail.
pub(super) fn split_rail(strip: Rect, rail: bool) -> (Rect, Option<Rect>) {
    if !rail || strip.width < 2 {
        return (strip, None);
    }
    let rail = Rect {
        x: strip.x + strip.width - 1,
        width: 1,
        ..strip
    };
    (
        Rect {
            width: strip.width - 1,
            ..strip
        },
        Some(rail),
    )
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
    /// The screen label that leads the base half, such as `structural diff`.
    label: Color,
}

impl DiffSidesColors {
    /// The halves' colors in `theme`.
    ///
    /// Each channel a `diff.sides` scope sets wins. An unset one tints the
    /// focused status bar toward the side's status color, and an unset name
    /// lifts that color toward white. The label takes the base name's color
    /// where neither its scope nor a parent sets a foreground.
    ///
    /// No color depends on the pane's focus. The frame around the base column
    /// takes [`diff_base_tint`], which falls back to the base half's fill, and
    /// a gliding pane's pages carry that frame under a version that holds no
    /// focus.
    fn resolve(theme: &crate::theme::Theme) -> Self {
        use crate::theme::scope;
        let bar = theme.get(scope::UI_STATUSBAR_FOCUSED);
        let base = DiffSideColors::resolve(
            theme,
            bar,
            (scope::DIFF_SIDES_BASE, scope::DIFF_SIDES_BASE_NAME),
            scope::DIFF_DELETED,
        );
        Self {
            base,
            target: DiffSideColors::resolve(
                theme,
                bar,
                (scope::DIFF_SIDES_TARGET, scope::DIFF_SIDES_TARGET_NAME),
                scope::DIFF_ADDED,
            ),
            label: theme
                .try_get(scope::DIFF_SIDES_BASE_LABEL)
                .and_then(|style| style.fg)
                .unwrap_or(base.name),
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

/// The color of the frame around a split diff's base column in `theme` as RGB,
/// or `None` for a color outside RGB.
///
/// The frame takes the foreground of `diff.sides.base.frame`, and the base
/// half's fill where the theme leaves that scope unset. The frame draws only
/// as rich bars, so a color with no RGB value has no frame.
pub(crate) fn diff_base_tint(theme: &crate::theme::Theme) -> Option<[u8; 3]> {
    let frame = theme
        .try_get_exact(crate::theme::scope::DIFF_SIDES_BASE_FRAME)
        .and_then(|style| style.fg);
    style_rgb(Some(
        frame.unwrap_or_else(|| DiffSidesColors::resolve(theme).base.bg),
    ))
}

/// Emit the `edges` of the frame around the base column of a diff pane whose
/// content fills `content_area`.
///
/// A pane emits the sides ahead of its own paint and the top after it, as
/// [`review::emit_base_frame`] orders them. The diff body leaves out the pane's
/// minimap strip, so the frame does too. A changes view takes only the top
/// edge here. Its stack rows draw the sides, so one frame spans the whole stack
/// although each excerpt paints its own diff body.
///
/// Emits nothing on a dead scene or for a tint outside RGB.
fn emit_pane_base_frame(
    content_area: Rect,
    edges: FrameEdges,
    frame: FrameCtx<'_>,
    scene: &mut ApcScene,
) {
    let Some(tint) = diff_base_tint(frame.theme).filter(|_| scene.live()) else {
        return;
    };
    let (inner, _) = split_minimap_strip(content_area, frame.minimap_enabled);
    review::emit_base_frame(inner, tint, edges, scene);
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

/// The empty rect along the bottom edge of `pane_area` that the pane's LSP
/// cards stack up from, given where the single-minimap band sits.
///
/// A pane at the bottom of the window puts its cards just above the window's
/// status row. A pane that meets the band's left edge there reaches across the
/// band, as the status row below it does.
fn card_anchor(pane_area: Rect, minimap_band: Option<Rect>) -> Rect {
    let mut anchor = Rect {
        y: pane_area.y + pane_area.height,
        height: 0,
        ..pane_area
    };
    if let Some(band) = minimap_band
        && anchor.y == band.y + band.height
        && anchor.x + anchor.width == band.x
    {
        anchor.width += band.width;
    }
    anchor
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
/// unfocused list is not the screen the frame reports. `subject` adds what
/// the screen has selected to the label.
pub(crate) fn render_overlay_status(
    area: Rect,
    is_focused: bool,
    screen: Option<&str>,
    subject: Option<&ScreenSubject<'_>>,
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
    let (left, left_end) = overlay_status_segments(is_focused, segments, screen, subject, frame);
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
        boxes: mode_chip_box(is_focused),
    };
    render_status_runs(&[run], frame, buf, scene, None);
}

/// Build the overlay status bar's left segments in paint order, with the column
/// where they end.
///
/// Mode shows only when focused, and the workspace name only when focused
/// with the tab bar hidden, since a shown bar leads with the name. A screen
/// label follows unconditionally, left-padded when it leads. The screen
/// segment differs from the pane status's focus-gated one. The end column
/// stops at the right edge of `area`, and the right segments fit between it
/// and that edge.
///
/// A focused bar names an open picker modal in place of the screen, as the
/// pane status does. The label then agrees with the prompt chip beside it, and
/// carries no `subject`.
fn overlay_status_segments(
    is_focused: bool,
    area: Rect,
    screen: Option<&str>,
    subject: Option<&ScreenSubject<'_>>,
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
    let mut chip_end = None;
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
        chip_end = Some(cursor);
        if !frame.tab_bar_shown {
            push_left(
                &mut left,
                &mut cursor,
                end_x,
                format!(" {} ", frame.workspace_name),
                base_style.add_modifier(Modifier::BOLD),
            );
        }
    }
    let label = match frame.modal_label.filter(|_| is_focused) {
        Some(modal_label) => Some((modal_label, base_style, None)),
        None => screen_segment(screen, theme).map(|(screen_label, screen_color)| {
            (screen_label, base_style.fg(screen_color), subject)
        }),
    };
    if let Some((label, style, subject)) = label {
        // A label that opens the row or meets the mode chip takes a pad cell,
        // so it stands clear of the row's edge or of the chip's box.
        let left_pad = if cursor == area.x || chip_end == Some(cursor) {
            " "
        } else {
            ""
        };
        let text = match subject {
            Some(subject) => format!("{left_pad}{label} \u{b7} {} ", subject.name),
            None => format!("{left_pad}{label} "),
        };
        push_left(&mut left, &mut cursor, end_x, text, style);

        if let Some(note) = subject.and_then(|subject| subject.note.as_deref()) {
            let muted = tinted(base_style, theme, crate::theme::scope::UI_TEXT_MUTED);
            push_left(&mut left, &mut cursor, end_x, format!("{note} "), muted);
        }
    }
    (left, cursor)
}

/// What an overlay screen's status label names besides the screen, such as
/// the commit a commits list has selected.
pub(crate) struct ScreenSubject<'a> {
    /// The selected item's name, which the label carries after the screen's
    /// name.
    pub(crate) name: &'a str,
    /// A remark after the label in `ui.text.muted`, such as a merge's count of
    /// parents.
    pub(crate) note: Option<String>,
}

/// Draw the hairline of each vertical divider in `dividers`, lit where it
/// touches the focused pane.
///
/// A horizontal divider draws nothing, because its row holds the status row
/// of the pane above it.
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
            DividerOrientation::Horizontal => continue,
        }
    }
}

/// Paint `status`, the status of `pane`, into its status row.
///
/// The row changes with every cursor move, so the caller opens a lane of its
/// own for it, and a move leaves every pane's content lane off the wire.
pub(crate) fn render_pane_status(
    pane: &Pane,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    status: &StatusRow,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
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
    if render_status_runs(&status.runs, frame, buf, scene, cache) {
        draw_divider_cell(&status.runs, frame.theme, buf, scene);
    }
}

/// Draw the cell between the two halves of a split status row `runs` as the
/// base half's fill up to the diff's hairline and the target half's fill past
/// it, with one pixel of window ground under the hairline.
///
/// The ground pixel is a bar with the hairline's own geometry, drawn last, so
/// it lands on the hairline's pixel whichever way the bar shader rounds the
/// cell's middle. A row with no such cell, a color outside RGB, or a theme
/// whose halves both sit on the ground draws nothing, and the cell keeps the
/// window ground.
fn draw_divider_cell(
    runs: &[StatusRun],
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let [base, target] = runs else {
        return;
    };
    if target.area.x != base.area.right() + 1 {
        return;
    }
    let colors = (|| {
        Some((
            style_rgb(base.style.bg)?,
            style_rgb(target.style.bg)?,
            style_rgb(Some(crate::render::themed_bg(theme)))?,
        ))
    })();
    let Some((base_bg, target_bg, ground)) = colors else {
        return;
    };
    if base_bg == ground && target_bg == ground {
        return;
    }

    // Bars of different colors overlap here, so they render one at a time in
    // draw order rather than through a batch, which groups them by color.
    let cell = Rect::new(base.area.right(), base.area.y, 1, 1);
    let rule = review::DIVIDER_RULE_X;
    for (x, width, color) in [
        (0, rule, base_bg),
        (rule, 16 - rule, target_bg),
        (rule, 1, ground),
    ] {
        Bar {
            x: x as i16,
            y: 0,
            width,
            height: 16,
            color,
            radius: 0,
            corners: 0,
        }
        .render(cell, buf, scene);
    }
}

/// The segments of one pane's status, laid out in its status row.
///
/// The frame builds it once, after the panes paint or replay, and reads the LSP
/// badge's hover rect back from it.
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
/// them at the diff's divider. Under a live scene that cell takes each half's
/// fill on its side of the diff's hairline and keeps one pixel of ground under
/// it, as [`draw_divider_cell`] draws it. A theme whose halves sit on the
/// ground draws no bar there.
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
    /// Runs of left segments that a live scene draws in one box of whole
    /// cells each, such as the mode chip and a tab.
    boxes: Vec<RunBox>,
}

/// A run of a [`StatusRun`]'s left segments that a live scene draws in one
/// box of whole cells, as a [`SegmentBox`] lays it out.
#[derive(Clone, Hash)]
struct RunBox {
    /// The run, as indices into [`StatusRun::left`].
    segments: Range<usize>,
    /// The panel the box draws as, or `None` for a box that only aligns the
    /// run to whole cells.
    shape: Option<BoxShape>,
}

/// The panel a [`RunBox`] draws as, in its first segment's background.
#[derive(Clone, Copy, Hash, PartialEq, Debug)]
enum BoxShape {
    /// The mode chip's box, as [`mode_chip_panel`] draws it, with its label
    /// at [`TypeRole::Mode`].
    Chip,
    /// The active tab, a chip in its background.
    Tab,
}

/// The box a run that leads with the mode chip draws it in, or none for a run
/// without the chip.
fn mode_chip_box(leads: bool) -> Vec<RunBox> {
    match leads {
        true => vec![RunBox {
            segments: 0..1,
            shape: Some(BoxShape::Chip),
        }],
        false => Vec::new(),
    }
}

/// The mode chip's panel in `fill`, a box smaller than [`Panel::chip`] inside
/// the same cells.
///
/// The boards draw the mode chip 14 px tall and about 34 px wide on an 18 px
/// row. The chip's box keeps its whole cells, so the panel stands 2 px inside
/// the row's top and bottom and 5 px inside each end of the box. Over the five
/// cells of ` NOR ` at 9 px cells, that is 35 by 14 px.
fn mode_chip_panel(fill: [u8; 3]) -> Panel {
    Panel {
        edges: Edge::symmetric(5, 2),
        ..Panel::chip(fill)
    }
}

/// The status of `pane`, laid out in `area`, its status row.
///
/// A changes view's row reads its focused excerpt, as an editor pane's row
/// reads its editor. Empty for a commits view, whose row names the screen
/// through [`render_overlay_status`], and for an area with no room.
#[allow(clippy::too_many_arguments)]
pub(crate) fn status_row(
    pane: &Pane,
    area: Rect,
    is_focused: bool,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    terms: &SlotMap<TermId, TermSession>,
    changes: &mut SlotMap<DiffChangesId, DiffChangesState>,
) -> StatusRow {
    if area.width == 0 || area.height == 0 || matches!(pane.view, View::Commits(_)) {
        return StatusRow::default();
    }

    let content = pane.area;
    let sides = sides_layout(&pane.view, content, frame, editors);
    let mut badge = None;
    let runs = status_segments(
        &pane.view, is_focused, area, sides, frame, editors, buffers, terms, changes, &mut badge,
    );
    StatusRow { runs, badge }
}

/// The working directory of the run the focused split pane shows, or `None`
/// when focus is off the split panes or the focused pane shows no run.
///
/// Takes the workspace's fields rather than the workspace, so a frame holds a
/// borrow of the runs alone while the pane loop borrows the editors mutably.
pub(crate) fn focused_run_cwd<'a>(
    focus: FocusTarget,
    panes: &PaneTree,
    runs: &'a SlotMap<RunId, RunState>,
) -> Option<&'a Path> {
    let FocusTarget::SplitPane = focus else {
        return None;
    };
    match panes.pane(panes.focus()).view {
        View::Run(id) => runs.get(id).map(|run| run.cwd.as_path()),
        _ => None,
    }
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
/// The runs and the ground determine the painted row exactly, given the
/// rectangle. A caller that caches the painted bytes hashes this in place of a
/// repaint and a diff, which is why the assembly is exposed apart from
/// [`paint_pane_status_cells`].
#[derive(Hash)]
pub(crate) struct PaneStatusCells {
    /// The row's runs from left to right, as [`status_row`] lays them out for
    /// a pane in the main window.
    runs: Vec<StatusRun>,
    /// The window ground, which shows in the cell between two runs.
    ground: Color,
}

/// Assemble a detached pane's status bar for [`paint_pane_status_cells`].
///
/// Shares [`status_segments`] with [`status_row`], adding the numeric
/// pane badge that only a detached pane needs, since the primary scene's digit
/// popover does not reach its window. The badge paints in the focused bar's
/// style on either bar, so its digit reads against its box. The focused window
/// gets none, since the chord starts from that pane and does not pick it.
///
/// `content` is the pane's content in the row's coordinates. A diff view's row
/// splits at the divider that its diff body takes there, and names the base
/// and the target as a split row in the main window does.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pane_status_cells(
    view: &View,
    is_focused: bool,
    area: Rect,
    content: Rect,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    terms: &SlotMap<TermId, TermSession>,
    changes: &mut SlotMap<DiffChangesId, DiffChangesState>,
    badge: Option<u32>,
) -> PaneStatusCells {
    let sides = sides_layout(view, content, frame, editors);
    let mut runs = status_segments(
        view, is_focused, area, sides, frame, editors, buffers, terms, changes, &mut None,
    );

    if let (Some(digit), Some(first)) = (badge, runs.first_mut()) {
        let badge_style = frame
            .theme
            .get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
            .add_modifier(Modifier::BOLD);
        first.left.insert(0, (format!("[{digit}]"), badge_style));
    }

    PaneStatusCells {
        runs,
        ground: crate::render::themed_bg(frame.theme),
    }
}

/// Paint assembled status runs into `buf` as plain cells, for a detached
/// pane's aux window where no rich APC scene is available.
pub(crate) fn paint_pane_status_cells(cells: &PaneStatusCells, area: Rect, buf: &mut Buffer) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    for x in area.left()..area.right() {
        buf[(x, area.y)].set_char(' ').set_bg(cells.ground);
    }
    for run in &cells.runs {
        for x in run.area.left()..run.area.right() {
            buf[(x, run.area.y)].set_char(' ').set_style(run.style);
        }
    }
    paint_runs_fallback(buf, &cells.runs);
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
/// A run's [`RunBox`]es reach the bar as [`SegmentBox`]es, each with its
/// shape's panels in its first segment's background. The mode chip's box
/// draws its label at [`TypeRole::Mode`], and every other segment at
/// [`TypeRole::Ui`].
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
        let boxes: Vec<SegmentBox> = run
            .boxes
            .iter()
            .map(|run_box| SegmentBox {
                segments: run_box.segments.clone(),
                panels: run_box
                    .shape
                    .zip(left.get(run_box.segments.start))
                    .map_or_else(Vec::new, |(shape, first)| match shape {
                        BoxShape::Chip => vec![mode_chip_panel(first.bg)],
                        BoxShape::Tab => vec![Panel::chip(first.bg)],
                    }),
                scale: (run_box.shape == Some(BoxShape::Chip))
                    .then(|| frame.theme.type_scale(TypeRole::Mode)),
            })
            .collect();
        StatusBar {
            left,
            boxes: &boxes,
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
/// shows. It takes the color of `ui.tabbar.name` where the theme sets one. A
/// tab reads as its [`Workspace::tab_label`], and a tagged file's
/// extension follows its stem as a `.ext` suffix in the type's color, `.md.gz`
/// for a gzip file. Painting through the status-bar segment path means the bar
/// picks up the same scaled-run rendering under stoatty and the same cell
/// fallback elsewhere.
///
/// Under a live scene a hairline in `ui.border.inactive` follows the name,
/// each tab takes whole cells with its text centered, and the active tab
/// draws as a chip in its background, one logical pixel inside the row. In
/// cells the active tab is its background. Every tab takes a box, so no tab
/// moves when another becomes the active one.
///
/// While the split pane in focus shows a diff view or a changes view, the bar
/// ends in two chips, `files` and `changes`, which name the two diff layouts.
/// The focused layout's chip takes `ui.tabbar.toggle.active` and the other
/// `ui.tabbar.toggle.inactive`. The chips show only where they fit beside the
/// tabs, and a press on them does nothing.
///
/// Under a live scene the chips read at the Caption scale, and a chip whose
/// background differs from the bar's draws as a [`Panel::chip`], one logical
/// pixel inside the row. A chip on the bar's background reads as plain text.
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

    let TabSegments { left, boxes } =
        tab_segments(ws, theme, frame.workspace_name, (active, inactive));

    // The chips pack from the bar's edge in slice order, so they list from
    // right to left after the pad that ends the bar.
    let mut right: Vec<StatusSeg> = Vec::new();
    if let Some(focused) = focused_diff_layout(ws) {
        let on = inactive.patch(theme.get(crate::theme::scope::UI_TABBAR_TOGGLE_ACTIVE));
        let off = inactive.patch(theme.get(crate::theme::scope::UI_TABBAR_TOGGLE_INACTIVE));
        let chip = |name: &str| (format!(" {name} "), if name == focused { on } else { off });
        right = vec![(" ".to_string(), inactive), chip("changes"), chip("files")];
    }
    let chars = |segments: &[StatusSeg]| -> usize {
        segments.iter().map(|(text, _)| text.chars().count()).sum()
    };

    // The chip takes its cells out of the run, and one blank cell ends the bar
    // after it. It drops where the tabs leave it no room, as the diff chips do.
    let chip = conflict_chip(ws, frame, scene.live())
        .filter(|chip| chars(&left) + usize::from(chip.cells) < usize::from(area.width));
    let segments = match &chip {
        Some(chip) => Rect {
            width: area.width - chip.cells - 1,
            ..area
        },
        None => area,
    };
    if chars(&left) + chars(&right) > usize::from(segments.width) {
        right.clear();
    }

    let runs = [StatusRun {
        area,
        segments,
        style: inactive,
        left,
        right: Vec::new(),
        hairline: true,
        boxes,
    }];
    let rich = render_status_runs(&runs, frame, buf, scene, None);
    let scale = if rich {
        theme.type_scale(TypeRole::Ui)
    } else {
        FULL_CELL_SCALE
    };
    if rich {
        draw_tab_divider(&runs[0], scale, theme, buf, scene);
    }
    draw_layout_chips(
        &right,
        segments,
        inactive,
        rich.then(|| theme.type_scale(TypeRole::Caption)),
        buf,
        scene,
    );
    if let Some(chip) = chip {
        let rect = Rect::new(segments.right(), area.y, chip.cells, 1);
        draw_conflict_chip(&chip, rect, theme, buf, scene);
    }

    let texts: Vec<&str> = runs[0].left.iter().map(|(text, _)| text.as_str()).collect();
    let tabs: Vec<Range<usize>> = runs[0]
        .boxes
        .iter()
        .map(|run_box| run_box.segments.clone())
        .collect();
    spans.extend(status_bar::box_spans(&texts, &tabs, scale, segments.width));
}

/// The tab bar's left segments, the workspace name and then each tab, with
/// what a live scene draws them with.
struct TabSegments {
    left: Vec<StatusSeg>,
    /// One box per tab, the active tab's a [`BoxShape::Tab`].
    boxes: Vec<RunBox>,
}

/// The tab bar's left segments for `ws` under `workspace_name`, the active
/// tab in the first of `styles` and the rest in the second.
///
/// A tab's segments are its conflict mark and its label. A tagged file's label
/// is its stem, and its extension follows as a suffix in the tag's color.
fn tab_segments(
    ws: &Workspace,
    theme: &crate::theme::Theme,
    workspace_name: &str,
    (active, inactive): (Style, Style),
) -> TabSegments {
    let name = theme
        .try_get(crate::theme::scope::UI_TABBAR_NAME)
        .and_then(|style| style.fg)
        .map_or(inactive, |fg| inactive.fg(fg))
        .add_modifier(Modifier::BOLD);
    let mut left: Vec<StatusSeg> = vec![
        (format!(" {workspace_name} "), name),
        (" ".to_string(), inactive),
    ];
    let mut boxes = Vec::new();
    for idx in 0..ws.tabs.len() {
        let is_active = idx == ws.active_tab;
        let style = if is_active { active } else { inactive };
        let first = left.len();
        let (label, suffix) = ws.tab_label(idx);
        if let Some(mark) = ws.tab_conflict_mark(idx) {
            left.push(tab_mark(mark, style, theme));
        }
        match suffix {
            Some((suffix, tag_scope)) => {
                left.push((format!(" {label}"), style));
                left.push((format!("{suffix} "), tinted(style, theme, tag_scope)));
            },
            None => left.push((format!(" {label} "), style)),
        }
        boxes.push(RunBox {
            segments: first..left.len(),
            shape: is_active.then_some(BoxShape::Tab),
        });
    }

    TabSegments { left, boxes }
}

/// Draw the hairline that parts the workspace name from the tabs in the tab
/// bar's `run`, at the end of the name, its first segment, at `scale`.
///
/// The hairline takes `ui.border.inactive`, and a theme without an RGB color
/// there draws none. A name that fills the bar leaves no room for it.
fn draw_tab_divider(
    run: &StatusRun,
    scale: u16,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let Some(color) = style_rgb(theme.get(crate::theme::scope::UI_BORDER_INACTIVE).fg) else {
        return;
    };
    let Some((name, _)) = run.left.first() else {
        return;
    };
    let x = text_run::advance_sixteenths(name.chars().count(), scale);
    if u32::from(x) >= u32::from(run.segments.width) * 16 {
        return;
    }

    Bar {
        x: x as i16,
        y: TAB_DIVIDER_TOP,
        width: 1,
        height: TAB_DIVIDER_HEIGHT,
        color,
        radius: 0,
        corners: 0,
    }
    .render(run.area, buf, scene);
}

/// The segment ahead of a tab's label that marks its file as unmerged or as
/// resolved.
///
/// An unmerged file's `!` takes `vcs.conflict.mark`, or the conflict header's
/// color in a theme without it.
fn tab_mark(mark: TabConflictMark, style: Style, theme: &crate::theme::Theme) -> StatusSeg {
    use crate::theme::scope;
    match mark {
        TabConflictMark::Conflicted => {
            let fg = theme
                .try_get(scope::VCS_CONFLICT_MARK)
                .and_then(|mark| mark.fg)
                .or(theme.get(scope::VCS_CONFLICT_HEADER).fg);
            (" !".to_string(), fg.map_or(style, |fg| style.fg(fg)))
        },
        TabConflictMark::Resolved => (
            " \u{2713}".to_string(),
            tinted(style, theme, scope::DIFF_ADDED),
        ),
    }
}

/// The chip the tab bar ends in while the conflict view is focused.
struct ConflictChip {
    /// The files the session has not applied, and the chunk at the cursor.
    text: String,
    style: Style,
    /// The chip's text color under a live scene, where it paints as a panel
    /// and a run. `None` paints it in cells.
    rich: Option<[u8; 3]>,
    cells: u16,
}

/// The tab bar's [`ConflictChip`], or `None` off the conflict screen.
///
/// Under a live scene the chip's text draws at the Caption scale, the board's
/// 11 px chrome text, so its width follows that text and the board's 8/16
/// padding on each side. In cells it takes its text and a cell on each side.
fn conflict_chip(ws: &Workspace, frame: FrameCtx<'_>, live: bool) -> Option<ConflictChip> {
    use crate::theme::scope;
    let position = frame
        .conflict_position
        .filter(|_| frame.screen == Some("conflict"))?;
    let session = ws.conflict.as_ref()?;

    let unresolved = session.files.len().saturating_sub(session.applied.len());
    let at = position
        .chunk_at
        .map_or_else(|| "-".to_string(), |at| at.to_string());
    let text = format!(
        "{unresolved} unresolved \u{b7} {at} / {} chunks",
        position.chunk_total
    );

    let theme = frame.theme;
    let style = theme
        .try_get(scope::VCS_CONFLICT_CHIP)
        .unwrap_or_else(|| theme.get(scope::VCS_CONFLICT_HEADER));
    let rich = style_rgb(style.fg).filter(|_| live);
    let chars = text.chars().count();
    let cells = match rich {
        Some(_) => {
            let advance = text_run::advance_sixteenths(chars, theme.type_scale(TypeRole::Caption));
            (2 * CHIP_PAD + advance).div_ceil(16)
        },
        None => chars as u16 + 2,
    };

    Some(ConflictChip {
        text,
        style,
        rich,
        cells,
    })
}

/// Paint `chip` over `rect`, a one-row rounded panel with its text in a run
/// under a live scene, or its cells otherwise.
///
/// The cells under the panel take the chip's fill too, so no glyph the bar
/// left there shows through it.
fn draw_conflict_chip(
    chip: &ConflictChip,
    rect: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    for x in rect.x..rect.right() {
        text::write_cell(buf, x, rect.y, ' ', chip.style);
    }
    let Some(color) = chip.rich else {
        text::write_str(buf, rect.x + 1, rect.y, &chip.text, chip.style);
        return;
    };

    Panel {
        style: BorderStyle::None_,
        border: color,
        corner_radius: 4,
        fill: style_rgb(chip.style.bg),
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
    .draw_components(rect, scene);
    TextRun {
        col: CHIP_PAD as i16,
        row: 0,
        scale: theme.type_scale(TypeRole::Caption),
        color,
        bg: None,
        text: &chip.text,
        bold: false,
        follow: 0,
        anchor: None,
    }
    .render(rect, buf, scene);
}

/// Paint the tab bar's layout `chips` over `row`, packed leftward from its
/// right edge in slice order, one cell per character.
///
/// With a `scale`, each chip's text draws as a run at `scale`, centered in the
/// chip's cells. A chip whose background differs from `base`'s also draws as a
/// [`Panel::chip`] in that background. Each chip first blanks its cells in
/// `base`, so no glyph left there draws over the chip.
///
/// Without a scale, or where a color is not RGB, the chips paint into cells as
/// [`paint_status_fallback`] lays them out.
fn draw_layout_chips(
    chips: &[StatusSeg],
    row: Rect,
    base: Style,
    scale: Option<u16>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let rich = scale
        .and_then(|scale| resolve_rich_segments(chips, base).map(|segments| (scale, segments)));
    let Some((scale, segments)) = rich else {
        paint_status_fallback(buf, row, &[], chips);
        return;
    };

    let base_bg = style_rgb(base.bg);
    let mut right = row.right();
    for segment in &segments {
        let cells = segment.text.chars().count() as u16;
        let Some(left) = right.checked_sub(cells).filter(|&left| left >= row.x) else {
            break;
        };
        right = left;

        let rect = Rect::new(left, row.y, cells, 1);
        for x in rect.x..rect.right() {
            text::write_cell(buf, x, rect.y, ' ', base);
        }
        if Some(segment.bg) != base_bg {
            Panel::chip(segment.bg).draw_components(rect, scene);
        }

        let advance = text_run::advance_sixteenths(usize::from(cells), scale);
        TextRun {
            col: ((u32::from(cells) * 16).saturating_sub(u32::from(advance)) / 2) as i16,
            row: 0,
            scale,
            color: segment.fg,
            bg: None,
            text: segment.text,
            bold: segment.bold,
            follow: 0,
            anchor: None,
        }
        .render(rect, buf, scene);
    }
}

/// The diff layout that the split pane in focus shows, as the tab bar's chip
/// names it, or `None` for any other view or a focus outside the split panes.
///
/// A diff-view editor is the `files` layout, which steps file by file, and a
/// changes view is the `changes` layout.
fn focused_diff_layout(ws: &Workspace) -> Option<&'static str> {
    if ws.focus != FocusTarget::SplitPane {
        return None;
    }
    match &ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) if ws.editors.get(*id).is_some_and(|editor| editor.diff_view) => {
            Some("files")
        },
        View::DiffChanges(_) => Some("changes"),
        _ => None,
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

/// The status row's segments for `name`, the name of what its pane shows.
///
/// A file's name, with `split`, reads its directory in `ui.text.muted` over
/// `row_style`, its stem in `name_style`, and the extension that its
/// [`chrome::TypeTag`] names in the tag's color, as the tab bar reads an
/// extension. Any other name, such as a terminal's title, is one segment in
/// `name_style`. The modified mark that `dirty` adds reads muted.
///
/// The texts join to `pad`, `name`, the mark, and one closing space, whether
/// the name parts or not. So a split changes the row's colors and never its
/// text or its width.
fn name_segments(
    name: &str,
    pad: &str,
    dirty: bool,
    split: bool,
    name_style: Style,
    row_style: Style,
    theme: &crate::theme::Theme,
) -> Vec<StatusSeg> {
    let muted = tinted(row_style, theme, crate::theme::scope::UI_TEXT_MUTED);
    let mut segments: Vec<StatusSeg> = Vec::new();
    if split {
        let (dir, file) = name.split_at(name.rfind('/').map_or(0, |slash| slash + 1));
        let lead = match dir {
            "" => pad,
            dir => {
                segments.push((format!("{pad}{dir}"), muted));
                ""
            },
        };
        match chrome::tagged_name(Path::new(file)) {
            Some(tagged) => {
                let suffix = tinted(row_style, theme, tagged.tag.scope);
                segments.push((format!("{lead}{}", tagged.stem), name_style));
                segments.push((tagged.suffix, suffix));
            },
            None => segments.push((format!("{lead}{file}"), name_style)),
        }
    } else {
        segments.push((format!("{pad}{name}"), name_style));
    }
    if dirty {
        segments.push((" [+]".to_owned(), muted));
    }

    if let Some((text, _)) = segments.last_mut() {
        text.push(' ');
    }
    segments
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
///
/// A changes view's lead names the Scope under the cursor before the hunk. The
/// Scope takes the room that the rest of the lead and the side names in its
/// run leave. After the hunk, the lead counts the stack, as [`stack_summary`]
/// describes.
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
    changes: &mut SlotMap<DiffChangesId, DiffChangesState>,
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

    let colors = sides.map(|_| DiffSidesColors::resolve(theme));
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
    // An open modal's label names what the keys reach, and the file under the
    // modal is not where they go, so the label stands alone.
    let modal = frame.modal_label.filter(|_| is_focused);
    // The conflict screen's lead names the operation and the chunk at the
    // cursor in place of the file name. The pane's buffer is the scratch
    // merged result, which the grid's header already names after its file.
    let conflict = frame
        .conflict_position
        .filter(|_| is_focused && frame.screen == Some("conflict"));
    // A changes view names the Scope under its cursor after the screen label.
    // The Scope takes the room that the rest of the lead leaves, so it goes in
    // at this index after the rest is in.
    let mut scope_at = None;
    let mut chip_end = None;
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
        chip_end = Some(cursor);
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
            if let Some(modal_label) = modal {
                push_left(
                    &mut left,
                    &mut cursor,
                    lead_end,
                    format!(" {modal_label} "),
                    lead_style,
                );
            } else if let Some(position) = conflict {
                let at = position
                    .chunk_at
                    .map_or_else(|| "-".to_string(), |at| at.to_string());
                push_left(
                    &mut left,
                    &mut cursor,
                    lead_end,
                    format!(
                        " {} \u{b7} chunk {at} / {} ",
                        position.operation, position.chunk_total
                    ),
                    tinted(lead_style, theme, crate::theme::scope::UI_TEXT),
                );
            } else if let Some((screen_label, screen_color)) = screen_segment(frame.screen, theme) {
                let screen_color = split.map_or(screen_color, |(_, colors)| colors.label);
                push_left(
                    &mut left,
                    &mut cursor,
                    lead_end,
                    format!(" {screen_label} "),
                    lead_style.fg(screen_color),
                );
                if let View::DiffChanges(id) = view {
                    scope_at = Some((*id, left.len()));
                }
            }
            if let Some(position) = frame.hunk_position.as_ref().filter(|_| sides.is_some()) {
                let at = position
                    .hunk_at
                    .map_or_else(|| "-".to_string(), |k| k.to_string());
                let stack = match view {
                    View::DiffChanges(id) => changes
                        .get(*id)
                        .map_or_else(Vec::new, |state| stack_summary(state, buffers)),
                    _ => Vec::new(),
                };
                let lead: Vec<String> =
                    std::iter::once(format!("hunk {at} / {}", position.hunk_total))
                        .chain(stack)
                        .collect();
                push_left(
                    &mut left,
                    &mut cursor,
                    lead_end,
                    format!(" {} ", lead.join(" \u{b7} ")),
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
        && modal.is_none()
        && sides.is_none()
        && conflict.is_none()
    {
        // A name that opens the row or meets the mode chip takes a pad cell,
        // so it stands clear of the row's edge or of the chip's box.
        let left_pad = if cursor == area.x || chip_end == Some(cursor) {
            " "
        } else {
            ""
        };
        let name_style = if is_focused {
            tinted(lead_style, theme, crate::theme::scope::UI_TEXT)
        } else {
            lead_style
        };
        let segments = name_segments(
            name,
            left_pad,
            status.dirty,
            status.buffer_id.is_some(),
            name_style,
            lead_style,
            theme,
        );
        for (text, style) in segments {
            push_left(&mut left, &mut cursor, lead_end, text, style);
        }
        if let Some(note) = read_only_note(status.read_only, status.disk_len, status.text_len) {
            let muted = tinted(lead_style, theme, crate::theme::scope::UI_TEXT_MUTED);
            push_left(&mut left, &mut cursor, lead_end, note, muted);
        }
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
        let scope = scope_at.and_then(|(id, at)| {
            let scope = changes.get_mut(id)?.cursor_scope(editors, buffers)?;
            Some((at, scope))
        });
        if let Some((at, scope)) = scope {
            let pair_width = |side: DiffSideColors, mark: char, name: &str| -> u16 {
                side.pair(mark, name, lead_style)
                    .iter()
                    .map(|(text, _)| text.chars().count() as u16)
                    .sum()
            };
            // A split row's target pair packs in the target half, so only the
            // base pair shares the lead's run.
            let names = match split {
                Some(_) => pair_width(colors.base, '\u{2212}', base_name),
                None => {
                    pair_width(colors.base, '\u{2212}', base_name)
                        + pair_width(colors.target, '+', target_name)
                },
            };
            let room = lead_end.saturating_sub(cursor).saturating_sub(names);
            let runs = scope_runs(&scope, room, lead_style, theme);
            let width: u16 = runs
                .iter()
                .map(|(text, _)| text.chars().count() as u16)
                .sum();
            cursor = cursor.saturating_add(width).min(lead_end);
            left.splice(at..at, runs);
        }
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
            let style = split.map_or_else(
                || tinted(trail_style, theme, crate::theme::scope::UI_TEXT_MUTED),
                |(_, colors)| trail_style.fg(colors.target.name),
            );
            right.push((text, style));
            right_anchor = start;
        }
    }
    if let Some(cwd) = frame.run_cwd.filter(|_| is_focused) {
        let text = format!(
            " {} ",
            crate::paths::display_relative_with_home(cwd, Path::new(""), frame.home)
        );
        let width = text.chars().count() as u16;
        let start = right_anchor.saturating_sub(width);
        if start >= cursor {
            let muted = tinted(trail_style, theme, crate::theme::scope::UI_TEXT_MUTED);
            right.push((text, muted));
            right_anchor = start;
        }
    }
    if is_focused {
        let badge_right = right_anchor;
        let muted = tinted(trail_style, theme, crate::theme::scope::UI_TEXT_MUTED);
        // The block reads left to right as the servers and then the sources the
        // file's diagnostics name, and right segments push leftward, so the
        // sources push first. The first piece that fits ends the block with a
        // space, and each piece left of it ends with the dot that joins it to
        // the next.
        let sources = focused_diagnostic_sources(view, editors, buffers, frame.diagnostics);
        let sources = (!sources.is_empty()).then(|| sources.join(" \u{b7} "));
        let spinner = SPINNER_FRAMES[frame.spinner_phase as usize];
        let servers = frame
            .lsp_servers
            .iter()
            .rev()
            .map(|server| (server.busy.then_some(spinner), server.name.as_str()));
        let mut joined = false;
        for (spinner, name) in sources
            .as_deref()
            .map(|sources| (None, sources))
            .into_iter()
            .chain(servers)
        {
            let end = if joined { " \u{b7}" } else { " " };
            let text = match spinner {
                Some(spinner) => format!(" {spinner} {name}{end}"),
                None => format!(" {name}{end}"),
            };
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, muted));
                right_anchor = start;
                joined = true;
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
        // A run's commands run in the directory its row names, which need not
        // be the repo, so the repo segment stays off a run pane's row.
        if sides.is_none()
            && frame.run_cwd.is_none()
            && let Some(segments) = focused_staged_label(
                status.mark_kind,
                pair_lead.as_deref().or(frame.diff_base),
                frame.repo,
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
                boxes: mode_chip_box(chip_end.is_some()),
            },
            StatusRun {
                area: target,
                segments: target_segments,
                style: trail_style,
                left: target_left,
                right,
                hairline: false,
                boxes: Vec::new(),
            },
        ],
        None => vec![StatusRun {
            area,
            segments,
            style: bar_style,
            left,
            right,
            hairline: true,
            boxes: mode_chip_box(chip_end.is_some()),
        }],
    }
}

/// The parts a changes view's status row puts after the hunk position: how
/// many files `state` lists, then the lines their hunks add and remove.
///
/// A line count of zero takes no place, as in the file headers. The row shows
/// no hunk position for an empty stack, so the parts never count zero files.
fn stack_summary(state: &DiffChangesState, buffers: &BufferRegistry) -> Vec<String> {
    let files = state.excerpts.len();
    let noun = if files == 1 { "file" } else { "files" };
    let (added, removed) = state.line_counts(buffers);
    let lines: Vec<String> = [('+', added), ('\u{2212}', removed)]
        .into_iter()
        .filter(|&(_, count)| count > 0)
        .map(|(mark, count)| format!("{mark}{count}"))
        .collect();
    std::iter::once(format!("{files} {noun}"))
        .chain((!lines.is_empty()).then(|| lines.join(" ")))
        .collect()
}

/// The runs that name `scope` in a changes view's status row, fitted to
/// `room` cells with a pad cell at each side.
///
/// The parts are the ones a fold row's chip shows, each in `style` with its
/// role's foreground from `theme`. No runs show where not even the innermost
/// item fits.
fn scope_runs(
    scope: &FoldScope,
    room: u16,
    style: Style,
    theme: &crate::theme::Theme,
) -> Vec<StatusSeg> {
    let parts = crate::diff_changes::scope_parts(
        scope,
        room.saturating_sub(2).saturating_mul(16),
        FULL_CELL_SCALE,
    );
    let last = parts.len().saturating_sub(1);
    parts
        .into_iter()
        .enumerate()
        .map(|(index, part)| {
            let lead = if index == 0 { " " } else { "" };
            let trail = if index == last { " " } else { "" };
            (
                format!("{lead}{}{trail}", part.text),
                tinted(style, theme, part.role.theme_scope()),
            )
        })
        .collect()
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

/// The muted segment the status row names a read-only buffer's kind in, or
/// `None` for a buffer that writes back.
///
/// A compressed buffer reads the file's size on disk and its text's size,
/// `disk_len` and `text_len` in bytes. A buffer a state file restored without
/// the disk size reads the kind alone. A buffer the process lacks write
/// access to reads `read only`.
fn read_only_note(
    kind: Option<ReadOnly>,
    disk_len: Option<u64>,
    text_len: usize,
) -> Option<String> {
    Some(match kind? {
        ReadOnly::Compressed => match disk_len {
            Some(disk) => format!(
                " gzip \u{b7} {} \u{2192} {} ",
                byte_size(disk),
                byte_size(text_len as u64)
            ),
            None => " gzip ".to_owned(),
        },
        ReadOnly::Permission => " read only ".to_owned(),
    })
}

/// `bytes` in the largest binary unit that keeps a whole number before the
/// point.
///
/// Bytes and KiB read as whole numbers, the KiB rounded. MiB and GiB keep one
/// decimal place.
fn byte_size(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;

    let scaled = |unit: u64| bytes as f64 / unit as f64;
    match bytes {
        0..KIB => format!("{bytes} B"),
        KIB..MIB => format!("{} KiB", scaled(KIB).round()),
        MIB..GIB => format!("{:.1} MiB", scaled(MIB)),
        _ => format!("{:.1} GiB", scaled(GIB)),
    }
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
/// A segment draws bold where its style over `base` adds the bold modifier, as
/// the cell fallback paints it over the run's fill.
///
/// Returns `None` if any resolved color is not RGB, since a theme without RGB
/// status colors drives the cell fallback rather than the rich bar.
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
                bold: base.patch(*style).add_modifier.contains(Modifier::BOLD),
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

/// Statusline label leading with the repo segment and appending the focused
/// buffer's hunk counts, or `None` when neither has anything.
///
/// The repo segment reads the repository as [`repo_segments`] lays it out:
/// its head, a rebase chip, and its counts, or a clean mark. It shows whenever
/// a tally has landed, a clean repository included, since `clean` is itself
/// news. The file's counts are the one on screen, and a tally the file
/// contradicts still shows beside them, since that is worth seeing.
///
/// With no repo status the label falls back to the file's counts alone. That
/// covers a buffer outside a repo and the window before the first diff lands,
/// where leading with a clean mark would misreport the repo.
///
/// A `base_lead` naming a base off the working tree replaces the repo segment
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
/// The file's counts follow as two segments, the pencil count of unstaged
/// hunks in `ui.statusbar.unstaged` and the plus count of staged hunks in
/// `ui.statusbar.staged`, so each count reads in its own color. A range
/// review's counts keep their words inside the text, `approved / unapproved`,
/// since the glyphs name git states.
fn focused_staged_label(
    kind: MarkKind,
    base_lead: Option<&str>,
    repo: Option<&RepoStatus>,
    file: Option<(usize, usize)>,
    position: Option<&HunkPosition>,
    (base_style, theme): (Style, &crate::theme::Theme),
) -> Option<Vec<StatusSeg>> {
    let place = position.map(|position| (position.file_at, position.file_total));
    let StagedBody { repo, text, counts } = staged_label_body(kind, base_lead, repo, file, place)?;

    let mut segments = match repo {
        Some(repo) => repo_segments(repo, base_style, theme),
        None => Vec::new(),
    };
    if let Some(text) = text {
        let lead = if segments.is_empty() { " " } else { " \u{b7} " };
        segments.push((format!("{lead}{text}"), base_style));
    }
    if let Some((staged, unstaged)) = counts {
        if !segments.is_empty() {
            segments.push((" \u{b7}".to_string(), base_style));
        }
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

/// The repo segment of the staged label, as the VcsStatus line reads it, each
/// piece led by a space.
///
/// The head follows the branch glyph, the glyph in `ui.text.dim` and the name
/// in bold `ui.text`. The branch's divergence from its upstream follows in
/// `ui.text.muted`, an up arrow for the commits it holds past the upstream and
/// a down arrow for the ones it lacks, each only when not zero. A `REB` chip
/// follows while a rebase is paused, in the
/// mode chip's style over `ui.statusline.rebase`'s color. While a merge is in
/// progress a `MERGE` chip follows in the mode chip's style over
/// `vcs.merge.banner`'s color, and then the merge head's name in `ui.text.dim`.
/// A `U+2502` divider in the fg of `ui.surface.overlay` parts all of that
/// from the counts. A repository with no head, divergence, or chip shows no
/// divider. Each count the repository holds follows in its own color, the
/// staged hunks in `diff.staged`, the unstaged hunks in `ui.text.muted`, and
/// the untracked files in `ui.text.dim`. A repository that holds none reads
/// `clean` in `ui.statusbar.clean` in their place.
fn repo_segments(
    repo: &RepoStatus,
    base_style: Style,
    theme: &crate::theme::Theme,
) -> Vec<StatusSeg> {
    use crate::theme::scope as s;
    let mut segments = Vec::new();
    if let Some(head) = &repo.head {
        let name = tinted(base_style, theme, s::UI_TEXT).add_modifier(Modifier::BOLD);
        segments.push((
            " \u{2387} ".to_string(),
            tinted(base_style, theme, s::UI_TEXT_DIM),
        ));
        segments.push((head.clone(), name));
    }
    let divergence: String = [(repo.ahead, '\u{2191}'), (repo.behind, '\u{2193}')]
        .into_iter()
        .filter(|&(count, _)| count > 0)
        .map(|(count, arrow)| format!(" {arrow}{count}"))
        .collect();
    if !divergence.is_empty() {
        segments.push((divergence, tinted(base_style, theme, s::UI_TEXT_MUTED)));
    }
    if repo.rebasing {
        let color = theme.get(s::UI_STATUSLINE_REBASE).fg.unwrap_or(Color::Red);
        segments.push((" ".to_string(), base_style));
        segments.push((" REB ".to_string(), theme.get(s::UI_MODE_LABEL).bg(color)));
    }
    if let Some(merge_head) = &repo.merge_head {
        // A theme without the merge banner's color takes the conflict screen's.
        let color = theme
            .try_get(s::VCS_MERGE_BANNER)
            .and_then(|banner| banner.fg)
            .or(theme.get(s::UI_STATUSLINE_CONFLICT).fg)
            .unwrap_or(Color::LightRed);
        segments.push((" ".to_string(), base_style));
        segments.push((" MERGE ".to_string(), theme.get(s::UI_MODE_LABEL).bg(color)));
        segments.push((
            format!(" {merge_head}"),
            tinted(base_style, theme, s::UI_TEXT_DIM),
        ));
    }
    if !segments.is_empty() {
        segments.push((
            " \u{2502}".to_string(),
            tinted(base_style, theme, s::UI_SURFACE_OVERLAY),
        ));
    }

    let counts = [
        (repo.staged, '\u{271a}', "staged", s::DIFF_STAGED),
        (repo.unstaged, '\u{270e}', "unstaged", s::UI_TEXT_MUTED),
        (repo.untracked, '?', "untracked", s::UI_TEXT_DIM),
    ];
    let held: Vec<StatusSeg> = counts
        .into_iter()
        .filter(|&(count, ..)| count > 0)
        .map(|(count, glyph, word, scope)| {
            (
                format!(" {glyph} {count} {word}"),
                tinted(base_style, theme, scope),
            )
        })
        .collect();
    match held.is_empty() {
        true => segments.push((
            " \u{2713} clean".to_string(),
            tinted(base_style, theme, s::UI_STATUSBAR_CLEAN),
        )),
        false => segments.extend(held),
    }
    segments
}

/// What [`staged_label_body`] answers for [`focused_staged_label`].
struct StagedBody<'a> {
    /// The repository's state, which leads the label as its own segments.
    /// `None` under a base off the working tree and before a tally lands.
    repo: Option<&'a RepoStatus>,
    /// The unpadded text after the repo segment, or `None` when only the
    /// counts show.
    text: Option<String>,
    /// The file's git hunk counts as `(staged, unstaged)`, which follow the
    /// text as glyph segments. `None` for a review, whose counts stay words in
    /// the text, and for a file with no hunks.
    counts: Option<(usize, usize)>,
}

/// The parts of [`focused_staged_label`] past its styling, with the file's git
/// counts that follow them.
///
/// `place` is the focused file's `(at, total)` among the changed files. With
/// it, the text reads `file {at}/{total}` after the repo segment or the base.
/// A file with no counts keeps its place alone.
fn staged_label_body<'a>(
    kind: MarkKind,
    base_lead: Option<&str>,
    repo: Option<&'a RepoStatus>,
    file: Option<(usize, usize)>,
    place: Option<(usize, usize)>,
) -> Option<StagedBody<'a>> {
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

    if let Some(lead) = base_lead {
        let text = match &file_part {
            Some(part) => format!("{lead} \u{b7} {part}"),
            None => lead.to_string(),
        };
        return Some(StagedBody {
            repo: None,
            text: Some(text),
            counts,
        });
    }

    let Some(repo) = repo else {
        let pair = file?;
        return Some(match kind {
            MarkKind::Staged => StagedBody {
                repo: None,
                text: None,
                counts: Some(pair),
            },
            MarkKind::Approved => StagedBody {
                repo: None,
                text: Some(words(pair)),
                counts: None,
            },
        });
    };
    Some(StagedBody {
        repo: Some(repo),
        text: file_part,
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
        View::Run(_) => owned("run"),
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
    let (dirty, staged_counts, mark_kind, text_len) = shared
        .as_ref()
        .and_then(|b| b.read().ok())
        .map(|g| {
            let map = g.diff_map.as_ref();
            (
                g.dirty,
                map.map(|dm| dm.staged_counts()),
                map.map(|dm| dm.mark_kind()).unwrap_or_default(),
                g.rope().len(),
            )
        })
        .unwrap_or_default();
    PaneStatusInfo {
        filename: Some(filename),
        buffer_id: Some(buffer_id),
        read_only: buffers.read_only(buffer_id),
        disk_len: buffers.disk_len(buffer_id),
        text_len,
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
    /// Why the buffer must not write back to its file, which the bar names in a
    /// muted segment after the name and the modified tag.
    read_only: Option<ReadOnly>,
    /// The file's byte length on disk, which a compressed buffer's segment
    /// names.
    disk_len: Option<u64>,
    /// The buffer's text length in bytes, which a compressed buffer's segment
    /// names beside the file's.
    text_len: usize,
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
            None => "scratch".to_string(),
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
        byte_size, diff_base_lead, focused_staged_label, name_segments, paint_status_fallback,
        read_only_note, repo_segments, resolve_rich_segments, split_halves, split_side_segments,
        status_filename, tab_mark, DiffSideColors, DiffSidesColors,
    };
    use crate::{
        action_handlers::{dispatch, focused_editor_mut},
        agent_ipc::AgentControl,
        badge::{Anchor, Badge, BadgeSource, BadgeState},
        buffer::{BufferId, TextBuffer},
        buffer_lifecycle,
        buffer_registry::ReadOnly,
        diff_map::{DiffMap, MarkKind},
        editor_state::EditorState,
        host::LspNotification,
        lsp::drain,
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
        rebase::RebaseState,
        term_session::TermId,
        workspace::{
            diff::{DiffBase, HunkPosition, RepoStatus, WorktreeBase},
            TabConflictMark,
        },
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

    /// The way into a diff screen from an open file, so one test covers both
    /// screens.
    type EnterScreen = fn(&mut crate::test_harness::TestHarness);

    /// A split diff and a changes view declare the strip a plain editor
    /// declares, seven cells in the syntax palette, as a per-pane strip and as
    /// the single band alike.
    ///
    /// The strip fills no ground of its own, and its columns end at the text
    /// strip's limit. The viewport is the thumb color at the plain strip's
    /// alpha, outlined in the same color at the outline's alpha.
    #[test]
    fn a_diff_screen_declares_a_seven_cell_syntax_strip() {
        let screens: [(&str, EnterScreen); 2] = [
            ("split diff", |h| {
                crate::action_handlers::review::enter_diff_view(&mut h.stoat);
            }),
            ("changes view", |h| {
                h.cover_with_diff_changes(false);
            }),
        ];
        for mode in [
            stoat_config::MinimapMode::PerPane,
            stoat_config::MinimapMode::Single,
        ] {
            for (screen, enter) in screens {
                let mut h = crate::test_harness::TestHarness::with_size(120, 12);
                let (tx, mut rx) = mpsc::unbounded_channel();
                h.stoat.set_apc_tx(tx);
                h.stoat.stoatty_protocol = 13;
                h.stoat.settings.editor_minimap = Some(mode);
                h.stage_review_scenario("/repo", &[("a.txt", "a\nb\n", "A\nb\n")]);
                h.open_file(Path::new("/repo/a.txt"));
                enter(&mut h);
                h.settle();
                for _ in 0..2 {
                    h.stoat.render();
                    crate::apc_emit::emit_apc_scene(&mut h.stoat);
                }
                let strips: Vec<_> = crate::test_fixture::drain_apc(&mut rx)
                    .into_iter()
                    .filter_map(|command| match command {
                        stoatty_protocol::command::Command::Minimap(strip) => Some((
                            strip.width,
                            strip.max_columns,
                            strip.bg,
                            strip.thumb,
                            strip.thumb_border,
                            strip.palette,
                        )),
                        _ => None,
                    })
                    .collect();

                let theme = &h.stoat.theme;
                let thumb = super::style_rgb(theme.get(crate::theme::scope::UI_BORDER_FOCUSED).fg)
                    .expect("an rgb thumb");
                let declared = (
                    crate::render::editor::MINIMAP_STRIP_COLS,
                    super::MINIMAP_MAX_COLUMNS,
                    [0, 0, 0, 0],
                    [
                        thumb[0],
                        thumb[1],
                        thumb[2],
                        crate::render::MINIMAP_THUMB_ALPHA,
                    ],
                    Some([
                        thumb[0],
                        thumb[1],
                        thumb[2],
                        crate::render::MINIMAP_THUMB_OUTLINE_ALPHA,
                    ]),
                    h.stoat.minimap_class_table.palette().to_vec(),
                );
                assert_eq!(strips.last(), Some(&declared), "{screen} in {mode:?}");
            }
        }
    }

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
            "scratch",
            "a pathless buffer names itself",
        );
    }

    /// A file's name reads its directory muted, its stem in the name's style,
    /// and a tagged extension in the tag's color, and any other name stays one
    /// segment. However the name parts, its texts join to the pad, the name,
    /// the modified mark, and one closing space.
    #[test]
    fn a_status_name_parts_into_directory_stem_and_suffix() {
        let h = Stoat::test();
        let theme = &*h.stoat.theme;
        let row = theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED);
        let [name, muted, rust] = [
            crate::theme::scope::UI_TEXT,
            crate::theme::scope::UI_TEXT_MUTED,
            crate::theme::scope::UI_TAG_RUST,
        ]
        .map(|scope| row.fg(theme.get(scope).fg.expect("an ink")));
        let parts = |text: &str, pad: &str, dirty: bool, split: bool| {
            name_segments(text, pad, dirty, split, name, row, theme)
        };
        let seg = |text: &str, style| (text.to_owned(), style);

        assert_eq!(
            [
                parts("src/render/chrome.rs", " ", true, true),
                parts("src/render/chrome.rs", "", false, true),
                parts("Makefile", " ", false, true),
                parts("zsh: ~/src", " ", false, false),
            ],
            [
                vec![
                    seg(" src/render/", muted),
                    seg("chrome", name),
                    seg(".rs", rust),
                    seg(" [+] ", muted),
                ],
                vec![
                    seg("src/render/", muted),
                    seg("chrome", name),
                    seg(".rs ", rust),
                ],
                vec![seg(" Makefile ", name)],
                vec![seg(" zsh: ~/src ", name)],
            ],
        );
    }

    /// The cell fallback inks each split pane's row with the directory muted,
    /// the name in the text color on the focused row and dim on the other,
    /// the extension in the tag's color, and the position muted.
    #[test]
    fn each_status_row_inks_the_directory_name_extension_and_position() {
        let mut h = crate::test_harness::TestHarness::with_size(120, 12);
        h.stoat.stoatty = false;
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        let path = PathBuf::from("/repo/src/a.rs");
        h.fake_fs().insert_file(&path, b"fn a() {}\n");
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        h.type_action("SplitRight()");

        let buf = h.render_composited();
        let inks: Vec<_> = pane_status_rects(&h)
            .into_iter()
            .map(|row| {
                let text = status_row(&buf, row);
                let at = |part: &str| row.x + text.find(part).expect("the part shows") as u16;
                [at("src/"), at("a.rs"), at(".rs"), at("1:1")].map(|x| buf[(x, row.y)].fg)
            })
            .collect();

        let theme = &*h.stoat.theme;
        let [text, dim, muted, rust] = [
            crate::theme::scope::UI_TEXT,
            crate::theme::scope::UI_TEXT_DIM,
            crate::theme::scope::UI_TEXT_MUTED,
            crate::theme::scope::UI_TAG_RUST,
        ]
        .map(|scope| theme.get(scope).fg.expect("an ink"));
        assert_eq!(
            inks,
            [[muted, dim, rust, muted], [muted, text, rust, muted]],
            "the unfocused left row, then the focused right row",
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

    /// The symbols of `buf`'s bottom row, the status row. A split diff's
    /// divider cell reads U+2502 there, the glyph the composite draws for the
    /// bars that fill the cell.
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
            bar.contains(" rust-analyzer \u{b7} rustc \u{b7} clippy "),
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

    /// The text the staged label's segments read, styles left out, for a repo
    /// on `main` whose tally holds `repo`'s staged and unstaged hunks.
    fn staged_label(
        kind: MarkKind,
        base_lead: Option<&str>,
        repo: Option<(usize, usize)>,
        file: Option<(usize, usize)>,
        position: Option<&HunkPosition>,
    ) -> Option<String> {
        let repo = repo.map(|(staged, unstaged)| RepoStatus {
            head: Some("main".to_string()),
            staged,
            unstaged,
            untracked: 0,
            ahead: 0,
            behind: 0,
            rebasing: false,
            merge_head: None,
        });
        repo_label(kind, base_lead, repo.as_ref(), file, position)
    }

    /// The text the staged label's segments read for `repo`, styles left out.
    fn repo_label(
        kind: MarkKind,
        base_lead: Option<&str>,
        repo: Option<&RepoStatus>,
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
            Some(" \u{2387} main \u{2502} \u{271a} 2 staged \u{270e} 3 unstaged · \u{270e}4 \u{271a}1 "),
            "both pairs read side by side"
        );
        assert_eq!(
            staged_label(MarkKind::Staged, None, Some((2, 3)), None, None).as_deref(),
            Some(" \u{2387} main \u{2502} \u{271a} 2 staged \u{270e} 3 unstaged "),
            "a file with no diff map drops its half"
        );
        assert_eq!(
            staged_label(MarkKind::Staged, None, Some((2, 3)), Some((0, 0)), None).as_deref(),
            Some(" \u{2387} main \u{2502} \u{271a} 2 staged \u{270e} 3 unstaged "),
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
            staged_label(MarkKind::Staged, None, Some((0, 0)), Some((0, 0)), None).as_deref(),
            Some(" \u{2387} main \u{2502} \u{2713} clean "),
            "a clean repo says so beside an unchanged file"
        );
        assert_eq!(
            staged_label(MarkKind::Staged, None, Some((0, 0)), Some((1, 0)), None).as_deref(),
            Some(" \u{2387} main \u{2502} \u{2713} clean · \u{270e}0 \u{271a}1 "),
            "a stale clean tally still shows against a file that has hunks"
        );
    }

    /// The repo segment names a detached HEAD by its short sha, counts the
    /// untracked files, and carries a `REB` chip while a rebase is paused. A
    /// repo with no head name leads with its counts.
    #[test]
    fn the_repo_segment_names_the_head_untracked_files_and_a_rebase() {
        let status = |head: Option<&str>, untracked: usize, rebasing: bool| RepoStatus {
            head: head.map(str::to_string),
            staged: 0,
            unstaged: 1,
            untracked,
            ahead: 0,
            behind: 0,
            rebasing,
            merge_head: None,
        };
        let label = |repo: RepoStatus| repo_label(MarkKind::Staged, None, Some(&repo), None, None);
        assert_eq!(
            [
                label(status(Some("abc1234"), 2, false)),
                label(status(Some("main"), 0, true)),
                label(status(None, 0, false)),
            ],
            [
                Some(" \u{2387} abc1234 \u{2502} \u{270e} 1 unstaged ? 2 untracked ".to_string()),
                Some(" \u{2387} main  REB  \u{2502} \u{270e} 1 unstaged ".to_string()),
                Some(" \u{270e} 1 unstaged ".to_string()),
            ],
        );
    }

    /// The divergence from the upstream shows each side only when it is not
    /// zero, right after the head.
    #[test]
    fn the_divergence_shows_each_side_only_when_not_zero() {
        let label = |ahead: usize, behind: usize| {
            let repo = RepoStatus {
                head: Some("main".to_string()),
                staged: 0,
                unstaged: 0,
                untracked: 0,
                ahead,
                behind,
                rebasing: false,
                merge_head: None,
            };
            repo_label(MarkKind::Staged, None, Some(&repo), None, None)
        };
        assert_eq!(
            [label(2, 0), label(0, 3), label(0, 0)],
            [
                Some(" \u{2387} main \u{2191}2 \u{2502} \u{2713} clean ".to_string()),
                Some(" \u{2387} main \u{2193}3 \u{2502} \u{2713} clean ".to_string()),
                Some(" \u{2387} main \u{2502} \u{2713} clean ".to_string()),
            ],
        );
    }

    /// A rich segment draws bold where its style over the run's base adds the
    /// bold modifier, as the cell fallback paints it. A base that adds it
    /// makes every segment bold but one whose style takes it away.
    #[test]
    fn a_rich_segment_takes_its_weight_from_its_style_over_the_base() {
        use ratatui::style::{Color, Modifier, Style};

        let segments = [
            Style::default(),
            Style::default().add_modifier(Modifier::BOLD),
            Style::default().remove_modifier(Modifier::BOLD),
        ]
        .map(|style| ("x".to_string(), style));
        let base = Style::default()
            .fg(Color::Rgb(1, 2, 3))
            .bg(Color::Rgb(4, 5, 6));
        let weights = |base: Style| {
            resolve_rich_segments(&segments, base)
                .expect("RGB colors")
                .iter()
                .map(|segment| segment.bold)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            [weights(base), weights(base.add_modifier(Modifier::BOLD))],
            [[false, true, false], [true, true, false]].map(Vec::from),
        );
    }

    /// Each piece of the repo segment reads in its own color. The glyph is dim,
    /// the head bold, the divergence muted, the rebase chip on the rebase
    /// color, the divider in the overlay's line color, and each count in its
    /// scope.
    #[test]
    fn the_repo_segment_colors_each_piece() {
        use ratatui::style::{Color, Modifier, Style};

        let h = Stoat::test();
        let theme = &*h.stoat.theme;
        let repo = RepoStatus {
            head: Some("main".to_string()),
            staged: 1,
            unstaged: 2,
            untracked: 3,
            ahead: 2,
            behind: 1,
            rebasing: true,
            merge_head: None,
        };
        let fg = |scope: &str| theme.get(scope).fg;
        let segments: Vec<(String, Option<Color>, Option<Color>, bool)> =
            repo_segments(&repo, Style::default(), theme)
                .into_iter()
                .map(|(text, style)| {
                    let bold = style.add_modifier.contains(Modifier::BOLD);
                    (text, style.fg, style.bg, bold)
                })
                .collect();
        let rebase = fg(crate::theme::scope::UI_STATUSLINE_REBASE);
        let chip = fg(crate::theme::scope::UI_MODE_LABEL);
        assert_eq!(
            segments,
            [
                (
                    " \u{2387} ".to_string(),
                    fg(crate::theme::scope::UI_TEXT_DIM),
                    None,
                    false
                ),
                (
                    "main".to_string(),
                    fg(crate::theme::scope::UI_TEXT),
                    None,
                    true
                ),
                (
                    " \u{2191}2 \u{2193}1".to_string(),
                    fg(crate::theme::scope::UI_TEXT_MUTED),
                    None,
                    false
                ),
                (" ".to_string(), None, None, false),
                (" REB ".to_string(), chip, rebase, true),
                (
                    " \u{2502}".to_string(),
                    fg(crate::theme::scope::UI_SURFACE_OVERLAY),
                    None,
                    false
                ),
                (
                    " \u{271a} 1 staged".to_string(),
                    fg(crate::theme::scope::DIFF_STAGED),
                    None,
                    false
                ),
                (
                    " \u{270e} 2 unstaged".to_string(),
                    fg(crate::theme::scope::UI_TEXT_MUTED),
                    None,
                    false
                ),
                (
                    " ? 3 untracked".to_string(),
                    fg(crate::theme::scope::UI_TEXT_DIM),
                    None,
                    false
                ),
            ],
        );
    }

    /// The merge chip is the mode chip's bold label over the merge banner's
    /// color, or over the conflict screen's color in a theme without it. The
    /// merge head's name follows in the dim color.
    #[test]
    fn the_merge_chip_takes_the_merge_banner_color() {
        use ratatui::style::{Color, Modifier, Style};

        let (config, errors) = stoat_config::parse(crate::app::DEFAULT_KEYMAP);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let config = config.expect("the shipped config parses");
        let repo = RepoStatus {
            head: None,
            staged: 0,
            unstaged: 1,
            untracked: 0,
            ahead: 0,
            behind: 0,
            rebasing: false,
            merge_head: Some("feature".to_string()),
        };
        let merge = |name: &str, [r, g, b]: [u8; 3]| {
            let theme = crate::theme::Theme::from_config(&config, name).expect("the theme loads");
            let found: Vec<_> = repo_segments(&repo, Style::default(), &theme)
                .into_iter()
                .take(3)
                .map(|(text, style)| {
                    let bold = style.add_modifier.contains(Modifier::BOLD);
                    (text, style.fg, style.bg, bold)
                })
                .collect();
            let chip = theme.get(crate::theme::scope::UI_MODE_LABEL).fg;
            let dim = theme.get(crate::theme::scope::UI_TEXT_DIM).fg;
            let want = vec![
                (" ".to_string(), None, None, false),
                (" MERGE ".to_string(), chip, Some(Color::Rgb(r, g, b)), true),
                (" feature".to_string(), dim, None, false),
            ];
            (found, want)
        };

        for (found, want) in [
            merge("ermine_night", [0xf5, 0x9a, 0xa0]),
            merge("default_dark", [0xea, 0x85, 0x8b]),
        ] {
            assert_eq!(found, want);
        }
    }

    #[test]
    fn a_clean_repo_reads_in_the_clean_color() {
        use ratatui::style::Style;

        let h = Stoat::test();
        let theme = &*h.stoat.theme;
        let clean = RepoStatus {
            head: None,
            staged: 0,
            unstaged: 0,
            untracked: 0,
            ahead: 0,
            behind: 0,
            rebasing: false,
            merge_head: None,
        };
        let segments: Vec<_> = repo_segments(&clean, Style::default(), theme)
            .into_iter()
            .map(|(text, style)| (text, style.fg))
            .collect();
        assert_eq!(
            segments,
            [(
                " \u{2713} clean".to_string(),
                theme.get(crate::theme::scope::UI_STATUSBAR_CLEAN).fg
            )],
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
        let mut h = status_halves_harness();
        h.type_keys("j j j");
        h.assert_snapshot("diff_view_status_halves");
    }

    /// Under a live scene the divider's cell of a split diff view's status row
    /// takes the base half's fill left of the diff's hairline and the target
    /// half's fill right of it, then one sixteenth of window ground over the
    /// hairline's place. The divider is column 59 of the 120-wide window, and
    /// the status row is row 7.
    ///
    /// Ermine's halves sit on the window ground, so its cell draws no bar. A
    /// theme over Ermine that restates the two tints draws all three.
    #[test]
    fn the_status_divider_cell_splits_between_tinted_side_fills() {
        use stoatty_protocol::command::{decode_stream, Command};

        let bars = |h: &mut crate::test_harness::TestHarness| -> Vec<_> {
            h.stoat.render();
            decode_stream(h.stoat.apc_scene.lane_bytes(crate::render::STATUS_LANE))
                .into_iter()
                .flat_map(|command| match command {
                    Command::Bar(bar) => vec![(bar.x, bar.y, bar.width, bar.height, bar.color)],
                    Command::Bars(group) => group
                        .bars
                        .into_iter()
                        .map(|(x, y, width, height)| (x, y, width, height, group.color))
                        .collect(),
                    _ => Vec::new(),
                })
                .filter(|bar| (944..960).contains(&bar.0))
                .collect()
        };

        let mut h = status_halves_harness();
        let shipped = bars(&mut h);

        let src = format!(
            "{}\ntheme tinted inherits ermine_night {{ diff.sides.base.bg = \"#2a1d20\"; \
             diff.sides.target.bg = \"#1d2a1f\"; }}",
            crate::app::DEFAULT_KEYMAP
        );
        let (config, _) = stoat_config::parse(&src);
        let tinted =
            crate::theme::Theme::from_config(&config.expect("the config parses"), "tinted")
                .expect("the theme loads");
        h.stoat.theme = Arc::new(tinted);
        h.stoat.theme_epoch += 1;

        assert_eq!(
            (shipped, bars(&mut h)),
            (
                vec![],
                vec![
                    (944, 112, 8, 16, [0x2a, 0x1d, 0x20]),
                    (952, 112, 8, 16, [0x1d, 0x2a, 0x1f]),
                    (952, 112, 1, 16, [0x0c, 0x0e, 0x11]),
                ],
            ),
            "no bar on the ground, and under the tints the base fill to the hairline, the \
             target fill past it, and the ground on the hairline's sixteenth, in draw order",
        );
    }

    /// A split diff view's status row reads its screen label in the theme's
    /// label color, #e6d3d5 in Ermine, apart from the base name beside the
    /// divider.
    #[test]
    fn the_split_status_row_reads_its_screen_label_in_the_label_color() {
        use ratatui::style::Color;

        let mut h = status_halves_harness();
        let buf = h.render_composited();
        let row = bar_row(&buf);
        let label = row
            .find("structural diff")
            .map(|at| buf[(row[..at].chars().count() as u16, buf.area.height - 1)].fg);

        assert_eq!(label, Some(Color::Rgb(0xe6, 0xd3, 0xd5)));
    }

    /// A 120 by 8 split diff view of one changed `a.rs`, with no minimap and
    /// its hints box hidden.
    fn status_halves_harness() -> crate::test_harness::TestHarness {
        let mut h = crate::test_harness::TestHarness::with_size(120, 8);
        h.stoat.minimap_override = Some(false);
        // The diff view keeps its hints box up, and 8 rows leave it over the
        // whole pane above the status row.
        h.stoat.screen_hints_hidden = true;
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .set_diff_view(true);
        h
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
            Some(" \u{2387} main \u{2502} \u{271a} 2 staged \u{270e} 3 unstaged · file 2/4 · \u{270e}4 \u{271a}1 · hunk 2/7 "),
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
            Some(" \u{2387} main \u{2502} \u{271a} 2 staged \u{270e} 3 unstaged · file 2/4 · hunk 2/7 "),
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
            Some(" \u{2387} main \u{2502} \u{271a} 2 staged \u{270e} 3 unstaged · file 2/4 · \u{270e}4 \u{271a}1 · hunk -/7 "),
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
                    " \u{2387} main \u{2502} \u{271a} 2 staged \u{270e} 3 unstaged · file 2/4 · 1 approved / 4 unapproved · hunk 1/5 "
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
            label: Color::White,
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

    /// A theme that sets the base half's text and fill but no frame frames the
    /// base column in the fill. The half's foreground is its text color, so
    /// the frame never takes it.
    #[test]
    fn an_unset_frame_takes_the_base_fill_and_not_its_text_color() {
        use super::diff_base_tint;

        let src = r##"theme t { diff.sides.base = { fg: "#010203", bg: "#040506" }; }"##;
        let (config, _) = stoat_config::parse(src);
        let theme =
            crate::theme::Theme::from_config(&config.expect("theme parses"), "t").expect("loads");
        assert_eq!(diff_base_tint(&theme), Some([4, 5, 6]), "the base fill");
    }

    /// Ermine sets each half's colors through its `diff.sides` scopes, and a
    /// theme without them tints the status bar toward each side's status
    /// color.
    ///
    /// Ermine's halves sit on the window ground, and its base frame keeps the
    /// removal tint through `diff.sides.base.frame`. default_dark frames the
    /// base column in the base half's fill.
    #[test]
    fn the_side_colors_take_the_theme_s_scopes_and_derive_the_rest() {
        use super::diff_base_tint;
        use ratatui::style::Color;

        let (config, _) = stoat_config::parse(crate::app::DEFAULT_KEYMAP);
        let config = config.expect("the shipped config parses");
        let themes = ["ermine_night", "default_dark"]
            .map(|name| crate::theme::Theme::from_config(&config, name).expect("theme loads"));
        let resolved = themes.each_ref().map(DiffSidesColors::resolve);
        let frames = themes.each_ref().map(diff_base_tint);
        assert_eq!(
            frames,
            [Some([0x2a, 0x1d, 0x20]), Some([0x72, 0x6d, 0x81])],
            "Ermine's frame scope, and default_dark's base fill",
        );

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
                    base: side([0x0c0e11, 0xa3898c, 0xf2b3b7, 0xf2777f]),
                    target: side([0x0c0e11, 0x8ea78b, 0xbfe6bb, 0x8fd18a]),
                    label: rgb(0xe6d3d5),
                },
                DiffSidesColors {
                    base: side([0x726d81, 0xf6e5e6, 0xeca7ac, 0xe06c75]),
                    target: side([0x697782, 0xebf2e7, 0xc1dbaf, 0x98c379]),
                    label: rgb(0xeca7ac),
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
                .contains("\u{271a} 1 staged \u{270e} 1 unstaged · \u{270e}1 \u{271a}1"),
            "statusline reports the repo's files and the focused file's hunks:\n{rendered}"
        );
    }

    /// The repo segment names the branch and reads `clean` for a tree with no
    /// change, and it carries a `REB` chip while a rebase is paused. A merge in
    /// progress carries a `MERGE` chip and the merge head's name, or its short
    /// sha when no branch names it. A detached HEAD reads as its short sha, an
    /// untracked file counts apart from the unstaged hunks, and the branch's
    /// divergence from its upstream follows its name.
    #[test]
    fn statusline_names_the_branch_and_a_clean_tree() {
        let row = |branch: Option<&str>,
                   rebasing: bool,
                   untracked: bool,
                   upstream: Option<(usize, usize)>,
                   merge: Option<Option<&str>>| {
            let mut h = crate::test_harness::TestHarness::with_size(80, 12);
            h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
            h.fake_fs().insert_file(Path::new("/repo/f.txt"), b"a\n");
            {
                let mut repo = h.fake_git().add_repo("/repo");
                repo.commit("a1000000", &[("f.txt", "a\n")])
                    .head_file("f.txt", "a\n")
                    .index_file("f.txt", "a\n")
                    .set_rebase_in_progress(rebasing);
                if let Some(branch) = branch {
                    repo.set_head_branch(branch);
                }
                if untracked {
                    repo.untracked("new.txt");
                }
                if let Some((ahead, behind)) = upstream {
                    repo.upstream("origin/main", ahead, behind);
                }
                if let Some(head_name) = merge {
                    repo.merge_in_progress("f00dfeed1234", head_name, "Merge");
                }
            }
            h.stoat.set_diff_warm_auto(true);
            h.open_file(Path::new("/repo/f.txt"));
            h.settle_diff_jobs();
            let row = bar_row(&h.render_composited()).replace('\u{2500}', " ");
            row.split_whitespace().collect::<Vec<_>>().join(" ")
        };
        assert_eq!(
            [
                row(Some("main"), false, false, None, None),
                row(Some("main"), true, false, None, None),
                row(None, false, true, None, None),
                row(Some("main"), false, false, Some((2, 1)), None),
                row(Some("main"), false, false, None, Some(Some("feature"))),
                row(Some("main"), false, false, None, Some(None)),
            ],
            [
                "NOR repo f.txt \u{2387} main \u{2502} \u{2713} clean 1:1",
                "NOR repo f.txt \u{2387} main REB \u{2502} \u{2713} clean 1:1",
                "NOR repo f.txt \u{2387} a100000 \u{2502} ? 1 untracked 1:1",
                "NOR repo f.txt \u{2387} main \u{2191}2 \u{2193}1 \u{2502} \u{2713} clean 1:1",
                "NOR repo f.txt \u{2387} main MERGE feature \u{2502} \u{2713} clean 1:1",
                "NOR repo f.txt \u{2387} main MERGE f00dfee \u{2502} \u{2713} clean 1:1",
            ],
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
    /// The row is 55 cells wide, so the room left of the summary holds
    /// `saved` and not `follow changes off`.
    #[test]
    fn the_staged_summary_gives_way_to_a_message_it_leaves_no_room_for() {
        for stoatty in [true, false] {
            let mut h = crate::test_harness::TestHarness::with_size(55, 12);
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
                    "NOR repo f.txt saved \u{270e} 1 unstaged · \u{270e}1 \u{271a}1 1:1",
                    "NOR repo f.txt follow changes off 1:1",
                ],
                "stoatty {stoatty}",
            );
        }
    }

    /// A changes view's status row is the diff view's row with the screen
    /// named `changes` and the stack's counts after the hunk, read through the
    /// focused excerpt. A walk puts the cursor on the first chunk, so the hunk
    /// reads `1 / 1`.
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
        h.type_keys("] g");

        let row = bar_row(&h.render_composited()).replace('─', " ");
        let point = crate::test_harness::editor::focused_cursor_point(&mut h.stoat);
        assert_eq!(
            row.split_whitespace().collect::<Vec<_>>().join(" "),
            format!(
                "NOR changes hunk 1 / 1 \u{b7} 1 file \u{b7} +1 \u{2212}1 \u{2212} index \
                 + working tree {}:{}",
                point.row + 1,
                point.column + 1,
            ),
        );
    }

    /// A changes view's status row counts the files its stack lists and sums
    /// the lines their hunks add and remove. A line count of zero takes no
    /// place.
    #[test]
    fn the_changes_view_bar_counts_the_stack_s_files_and_lines() {
        let bar = |files: &[(&str, &str, &str)]| {
            let mut h = crate::test_harness::TestHarness::with_size(140, 12);
            h.stage_review_scenario("/repo", files);
            h.open_file(&Path::new("/repo").join(files[0].0));
            h.cover_with_diff_changes(false);
            h.settle();
            let row = bar_row(&h.render_composited()).replace('─', " ");
            row.split_whitespace().collect::<Vec<_>>().join(" ")
        };

        assert_eq!(
            [
                bar(&[
                    ("a.txt", "a\nx\n", "A\nx\n"),
                    ("b.txt", "b\nd\ne\n", "b\ne\n")
                ]),
                bar(&[("c.txt", "c\n", "c\nC\nD\n")]),
            ],
            [
                "NOR changes hunk 1 / 2 \u{b7} 2 files \u{b7} +1 \u{2212}2 \u{2212} HEAD \
                 + working tree 1:1",
                "NOR changes hunk - / 1 \u{b7} 1 file \u{b7} +2 \u{2212} HEAD + working \
                 tree 1:1",
            ],
        );
    }

    /// A changes view's status row names the Scope of the cursor's row after
    /// the screen label, each part in its role's color. A row too narrow for
    /// the whole chain drops items from the chain's middle, and the base
    /// side's name keeps its place.
    #[test]
    fn the_changes_view_bar_names_the_scope_under_the_cursor() {
        let base =
            "mod outer {\n    impl Theme {\n        fn panel() -> u32 {\n            1\n        \
                    }\n    }\n}\n";
        let open = |width: u16| {
            let mut h = crate::test_harness::TestHarness::with_size(width, 12);
            h.stage_review_scenario("/repo", &[("a.rs", base, &base.replacen('1', "2", 1))]);
            h.open_file(Path::new("/repo/a.rs"));
            h.cover_with_diff_changes(false);
            h.settle();
            // The paint asks for the parse, and the next paint reads it.
            h.render_composited();
            for _ in 0..2 {
                h.stoat.drive_background();
                h.settle();
            }
            h.type_keys("] g");
            let buf = h.render_composited();
            (h, buf)
        };
        let text = |buf: &Buffer| {
            let row = bar_row(buf).replace('─', " ");
            row.split_whitespace().collect::<Vec<_>>().join(" ")
        };

        let (wide, buf) = open(200);
        let y = buf.area.height - 1;
        let symbols: Vec<&str> = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        let fg_at = |word: &str| {
            let x = symbols
                .windows(word.chars().count())
                .position(|cells| cells.concat() == word)?;
            Some(buf[(x as u16, y)].fg)
        };
        let theme = |scope: &str| wide.stoat.theme.get(scope).fg;

        let tail = "hunk 1 / 1 \u{b7} 1 file \u{b7} +1 \u{2212}1 \u{2212} HEAD + working \
                    tree 4:1";
        assert_eq!(
            (
                text(&buf),
                text(&open(148).1),
                [fg_at("mod"), fg_at("outer"), fg_at("\u{203a}")],
            ),
            (
                format!("NOR changes mod outer \u{203a} impl Theme \u{203a} fn panel {tail}"),
                format!("NOR changes \u{2026} \u{203a} fn panel {tail}"),
                [
                    theme(crate::theme::scope::DIFF_SCOPE_KEYWORD),
                    theme(crate::theme::scope::DIFF_SCOPE_NAME),
                    theme(crate::theme::scope::DIFF_SCOPE_SEPARATOR),
                ],
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
    /// over the buffer returns to a shell, so the status row of each pane that
    /// shows the buffer names what waits. The split shows the buffer in both
    /// panes, so both rows name it.
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
            "each row over the file names its waiter"
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
                left_of_the_box, " NOR  (unnamed) scratch [+] walkthrough  1:1 ",
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

    /// Sizes read whole in bytes and rounded KiB, and to one decimal in MiB and
    /// GiB.
    #[test]
    fn byte_sizes_read_in_the_largest_whole_unit() {
        assert_eq!(
            [512, 41_600, 190_464, 1_572_864, 3_221_225_472].map(byte_size),
            ["512 B", "41 KiB", "186 KiB", "1.5 MiB", "3.0 GiB"],
        );
    }

    /// A compressed buffer names both sizes, or the kind alone without the size
    /// on disk, and a buffer the process lacks write access to reads `read only`.
    /// A buffer that writes back has no segment.
    #[test]
    fn the_read_only_note_names_the_kind_and_the_sizes() {
        assert_eq!(
            [
                read_only_note(None, Some(41_600), 190_464),
                read_only_note(Some(ReadOnly::Compressed), Some(41_600), 190_464),
                read_only_note(Some(ReadOnly::Compressed), None, 190_464),
                read_only_note(Some(ReadOnly::Permission), None, 5),
            ],
            [
                None,
                Some(" gzip \u{b7} 41 KiB \u{2192} 186 KiB ".to_owned()),
                Some(" gzip ".to_owned()),
                Some(" read only ".to_owned()),
            ],
        );
    }

    /// A compressed buffer's status row names the file's size on disk and its
    /// text's size after the name.
    #[test]
    fn the_status_row_names_a_gzip_buffer_s_sizes() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 12);
        let root = PathBuf::from("/ro");
        let path = root.join("a.txt.gz");
        let gz = crate::test_harness::gzip(b"hello\n");
        h.fake_fs().insert_file(&path, gz.clone());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        // A gzip file reads on the blocking pool, and the read installs here.
        buffer_lifecycle::install_pending_opens(&mut h.stoat);
        h.settle();

        let buf = h.render_composited();

        let row = buf.area.height - 1;
        let bar = (0..buf.area.width)
            .map(|x| buf[(x, row)].symbol())
            .collect::<String>()
            .replace('─', " ");
        let lead = format!(" NOR  ro a.txt.gz  gzip \u{b7} {} B \u{2192} 6 B", gz.len());
        let kind_x = lead.find("gzip").expect("the lead names the kind") as u16;
        let muted = h
            .stoat
            .theme
            .get(crate::theme::scope::UI_TEXT_MUTED)
            .fg
            .expect("the theme mutes text");
        assert_eq!(
            (bar, buf[(kind_x, row)].fg),
            (format!("{lead:<56}1:1 "), muted),
            "the sizes after the name, in the muted color",
        );
    }

    /// A buffer the process lacks write access to reads `name [+]  read only`,
    /// its kind in a muted segment after the modified tag.
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
            format!("{:<56}1:1 ", " NOR  ro file.txt [+]  read only")
        );
    }

    /// The conflict screen's row leads with the operation and the chunk at the
    /// cursor, in the text color, in place of the scratch buffer's name.
    #[test]
    fn the_status_row_names_the_conflict_chunk_at_the_cursor() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 12);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .conflicted_file(
                "f.txt",
                Some("a\nbase\n"),
                Some("a\nours\n"),
                Some("a\ntheirs\n"),
            );
        dispatch(&mut h.stoat, &Conflict);
        h.settle();

        let buf = h.render_composited();
        let lead = " NOR  repo  conflict \u{b7} chunk 1 / 1";
        let at = lead.find("conflict").expect("the lead names the operation") as u16;
        let text = h
            .stoat
            .theme
            .get(crate::theme::scope::UI_TEXT)
            .fg
            .expect("the theme colors text");
        assert_eq!(
            (
                status_row(&buf, buf.area),
                buf[(at, buf.area.height - 1)].fg
            ),
            (format!("{lead:<56}2:1 "), text),
        );
    }

    /// The conflict pane's status row leads with the chunk while the conflict
    /// view holds focus. Once focus moves to the pane beside it, that pane's
    /// row leads with the mode chip and its file, and the conflict pane's row
    /// names its scratch buffer, since only the focused conflict pane leads
    /// with the chunk. The open widens its pane, so the layout unwidens to show
    /// the other one.
    #[test]
    fn the_conflict_lead_leaves_the_row_with_the_focus() {
        let mut h = crate::test_harness::TestHarness::with_size(120, 12);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo("/repo")
            .with_fs(h.fake_fs())
            .conflicted_file(
                "f.txt",
                Some("a\nbase\n"),
                Some("a\nours\n"),
                Some("a\ntheirs\n"),
            );
        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: PathBuf::from("/repo/f.txt"),
            },
        );
        h.settle();
        dispatch(&mut h.stoat, &stoat_action::SplitRight);
        dispatch(&mut h.stoat, &Conflict);
        h.settle();
        h.stoat.active_workspace_mut().panes.unwiden();

        let rows = |h: &mut crate::test_harness::TestHarness| {
            let buf = h.render_composited();
            pane_status_rows(h, &buf)
        };
        let on_conflict = rows(&mut h);
        {
            let panes = &mut h.stoat.active_workspace_mut().panes;
            let focused = panes.focus();
            let beside = panes
                .split_pane_ids()
                .into_iter()
                .find(|&id| id != focused)
                .expect("the pane beside the conflict view");
            panes.set_focus(beside);
        }
        let on_file = rows(&mut h);

        let [left, right] = [on_conflict[0].0, on_conflict[1].0];
        assert_eq!(
            [on_conflict, on_file],
            [
                vec![
                    pane_row(left, " f.txt", "1:1"),
                    pane_row(right, " NOR  repo  conflict \u{b7} chunk 1 / 1", "2:1"),
                ],
                vec![
                    pane_row(left, " NOR  repo f.txt", "1:1"),
                    pane_row(right, " scratch", "2:1"),
                ],
            ],
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

    /// The split panes' status rows in `buf`, in [`pane_status_rects`] order,
    /// each as its width and its text as [`status_row`] reads it.
    fn pane_status_rows(h: &crate::test_harness::TestHarness, buf: &Buffer) -> Vec<(u16, String)> {
        pane_status_rects(h)
            .into_iter()
            .map(|row| (row.width, status_row(buf, row)))
            .collect()
    }

    /// The split panes' status rows, left to right and then top to bottom.
    fn pane_status_rects(h: &crate::test_harness::TestHarness) -> Vec<Rect> {
        let mut areas: Vec<Rect> = h
            .stoat
            .active_workspace()
            .panes
            .split_panes()
            .map(|(_, pane)| pane.area)
            .collect();
        areas.sort_by_key(|area| (area.x, area.y));
        areas
            .into_iter()
            .map(|area| {
                crate::render::pane_status_row(area, h.stoat.single_minimap_rect, h.stoat.size())
                    .expect("the pane has a status row")
            })
            .collect()
    }

    /// A status row `width` cells wide that leads with `lead` and ends in the
    /// position `at` and one pad cell.
    fn pane_row(width: u16, lead: &str, at: &str) -> (u16, String) {
        let fill = usize::from(width) - at.len() - 1;
        (width, format!("{lead:<fill$}{at} "))
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
                format!("{:<56}2:1 ", " NOR  (unnamed) scratch [+]  2 sel "),
                format!("{:<56}2:1 ", " NOR  (unnamed) scratch [+]"),
            ],
        );
    }

    /// An open picker shows the prompt chip and its own name where a screen
    /// label sits, with no file name after it. A closed picker gives the row
    /// back to the mode and the file. The search input has no name, so its row
    /// keeps the mode chip.
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
                format!("{:<56}1:1 ", " PMT  (unnamed)  find file"),
                format!("{:<56}1:1 ", " NOR  (unnamed) scratch [+]"),
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

    /// The overlay bar names the workspace until a second tab shows the tab
    /// bar, which then carries the name. The screen label then meets the mode
    /// chip across a gap cell.
    #[test]
    fn the_overlay_bar_leaves_the_workspace_name_to_the_tab_bar() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 12);
        let ws = h.stoat.active_workspace_mut();
        ws.name = "ws".to_string();
        ws.rebase = Some(RebaseState::new(
            PathBuf::from("/overlay"),
            "onto".into(),
            vec![],
        ));
        let row = |h: &mut crate::test_harness::TestHarness| {
            let buf = h.render_composited();
            status_row(&buf, buf.area)
        };

        let one_tab = row(&mut h);
        dispatch(&mut h.stoat, &stoat_action::NewTab);
        assert_eq!(
            [one_tab, row(&mut h)],
            [
                format!("{:<60}", " NOR  ws rebase"),
                format!("{:<60}", " NOR  rebase"),
            ],
        );
    }

    /// A focused commits bar names the selected commit after its screen, and a
    /// merge's count of parents after that. An open picker takes the label
    /// whole, with no commit after it.
    #[test]
    fn the_commits_bar_names_its_commit_and_an_open_picker_alone() {
        let mut h = crate::test_harness::TestHarness::with_size(60, 12);
        h.fake_git()
            .add_repo("/repo")
            .commit_with_message("a1000000", "feat: root", &[])
            .commit_with_parent_message("b2000000", "a1000000", "feat: side", &[])
            .commit_with_parent_message("c3000000", "a1000000", "feat: main", &[])
            .merge_commit("d4000000", "b2000000", &[]);
        h.open_commits("/repo");
        let row = |h: &mut crate::test_harness::TestHarness| {
            let buf = h.render_composited();
            status_row(&buf, buf.area)
        };

        let merge = row(&mut h);
        h.type_keys("j");
        let plain = row(&mut h);
        h.type_keys(":");
        assert_eq!(
            [merge, plain, row(&mut h)],
            [
                format!(
                    "{:<60}",
                    " NOR  repo commits \u{b7} d400000 merge \u{b7} 2 parents"
                ),
                format!("{:<60}", " NOR  repo commits \u{b7} c300000"),
                format!("{:<60}", " PMT  repo palette"),
            ],
        );
    }

    /// Each pane's status row counts that pane's cursors, and the mode chip
    /// leads the focused pane's row alone, so the chip follows the focus while
    /// each count stays with its pane.
    #[test]
    fn each_pane_s_status_row_counts_its_own_cursors() {
        let mut h = crate::test_harness::TestHarness::with_size(120, 12);
        h.seed_focused_buffer("foo\nbar\n");
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &AddSelectionAbove);
        let counted = h.stoat.active_workspace().panes.focus();
        h.type_action("SplitRight()");
        dispatch(&mut h.stoat, &KeepPrimarySelection);
        let rows = |h: &mut crate::test_harness::TestHarness| {
            let buf = h.render_composited();
            pane_status_rows(h, &buf)
        };

        let away = rows(&mut h);
        h.stoat.active_workspace_mut().panes.set_focus(counted);
        let back = rows(&mut h);
        let [left, right] = [away[0].0, away[1].0];
        assert_eq!(
            [away, back],
            [
                vec![
                    pane_row(left, " scratch [+]  2 sel ", "1:1"),
                    pane_row(right, " NOR  (unnamed) scratch [+] ", "1:1"),
                ],
                vec![
                    pane_row(left, " NOR  (unnamed) scratch [+]  2 sel ", "1:1"),
                    pane_row(right, " scratch [+] ", "1:1"),
                ],
            ],
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

    /// The tab bar ends in the `files` and `changes` chips while a diff view or
    /// a changes view has the focus. It shows neither over a plain editor, over
    /// a diff whose split pane gave the focus to a dock, or where the chips do
    /// not fit beside the tabs.
    #[test]
    fn the_tab_bar_names_the_diff_layouts_over_a_diff() {
        let mut h = toggles_harness();
        h.stoat.stoatty = false;
        let bar = |h: &mut crate::test_harness::TestHarness| {
            h.snapshot();
            let text = h.rendered_text();
            let row = text.lines().next().unwrap_or_default();
            row.split_whitespace().collect::<Vec<_>>().join(" ")
        };

        let plain = bar(&mut h);
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        let diff = bar(&mut h);

        let ws = h.stoat.active_workspace_mut();
        let dock = ws.docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Hidden,
            default_width: 30,
            area: Rect::default(),
        });
        ws.focus = FocusTarget::Dock(dock);
        let docked = bar(&mut h);
        h.stoat.active_workspace_mut().focus = FocusTarget::SplitPane;

        h.resize(30, 8);
        let narrow = bar(&mut h);
        h.resize(120, 8);
        h.cover_with_diff_changes(false);
        h.settle();
        let changes = bar(&mut h);

        assert_eq!(
            [plain, diff, docked, narrow, changes],
            [
                "repo a.rs",
                "repo \u{b1} a.rs files changes",
                "repo \u{b1} a.rs",
                "repo \u{b1} a.rs",
                "repo \u{b1} changes \u{b7} 1 file files changes",
            ],
        );
    }

    /// A diff view marks the `files` chip active, and a changes view marks the
    /// `changes` chip.
    #[test]
    fn diff_view_tab_toggles() {
        let mut h = toggles_harness();
        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        h.assert_snapshot("diff_view_tab_toggles");

        h.cover_with_diff_changes(false);
        h.settle();
        h.assert_snapshot("changes_view_tab_toggles");
    }

    /// Under a live scene the focused layout's chip draws as a chip panel in its
    /// fill, and no bar in that fill backs either chip. The pad ends the bar in
    /// column 119, the `changes` chip takes the nine columns before it, and the
    /// `files` chip the seven before those.
    #[test]
    fn the_live_tab_bar_boxes_the_focused_layout_s_chip() {
        use super::style_rgb;
        use stoatty_protocol::command::{Command, Edge};

        let mut h = toggles_harness();
        let fill = style_rgb(
            h.stoat
                .theme
                .get(crate::theme::scope::UI_TABBAR_TOGGLE_ACTIVE)
                .bg,
        )
        .expect("the theme fills the active chip in RGB");
        let chips = |h: &mut crate::test_harness::TestHarness| {
            h.render_composited();
            let scene = stoatty_protocol::command::decode_stream(&h.stoat.apc_scene.all_bytes());
            let panels: Vec<_> = scene
                .iter()
                .filter_map(|command| match command {
                    Command::Panel(panel) if panel.top == 0 && panel.fill == Some(fill) => {
                        Some((panel.left, panel.width, panel.edges))
                    },
                    _ => None,
                })
                .collect();
            let bars = scene
                .iter()
                .filter(|command| {
                    matches!(command, Command::Bar(bar) if bar.y < 16 && bar.color == fill)
                })
                .count();
            (panels, bars)
        };

        focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(true);
        let diff = chips(&mut h);
        h.cover_with_diff_changes(false);
        h.settle();
        let changes = chips(&mut h);

        let inset = Edge::symmetric(0, 1);
        assert_eq!(
            (diff, changes),
            ((vec![(103, 7, inset)], 0), (vec![(110, 9, inset)], 0)),
            "the files chip's panel over the diff and the changes chip's over the changes view, \
             and no bar in the chip's fill",
        );
    }

    /// One changed `a.rs` open in an editor, 120 by 8, with the tab bar on and
    /// no minimap.
    fn toggles_harness() -> crate::test_harness::TestHarness {
        let mut h = crate::test_harness::TestHarness::with_size(120, 8);
        h.stoat.settings.ui_tab_bar = Some(stoat_config::TabBarMode::Always);
        h.stoat.minimap_override = Some(false);
        // The diff view keeps its hints box up, and 8 rows leave it over the
        // tab bar.
        h.stoat.screen_hints_hidden = true;
        h.stage_review_scenario("/repo", &[("a.rs", "a\nb\nc\nold\n", "a\nb\nc\nnew\n")]);
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/a.rs"));
        h.settle_diff_jobs();
        h
    }

    /// The bar leads with the workspace name in its own color and a one-cell
    /// gap. A tagged file's tab reads its stem and then its extension as a
    /// suffix in the tag's color, and a gzip file's suffix holds both
    /// extensions. Each tab's click span covers its label and its suffix.
    #[test]
    fn the_tab_bar_leads_with_the_workspace_and_tags_each_file() {
        use super::style_rgb;
        use ratatui::style::Color;

        let mut h = tagged_tabs_harness();
        h.stoat.stoatty = false;
        h.snapshot();

        let text = h.rendered_text();
        let bar = text.lines().next().expect("the tab bar row");
        let lead = " ws   chrome.rs  panel.rs  CHANGELOG.md.gz ";
        let fg_at = |needle: &str| {
            let col = lead[..lead.find(needle).expect("the bar names it")]
                .chars()
                .count();
            h.rendered_buffer()[(col as u16, 0)].fg
        };
        let theme = &h.stoat.theme;
        let md = style_rgb(theme.get(crate::theme::scope::UI_TAG_MARKDOWN).fg)
            .expect("the theme colors it in RGB");
        assert_eq!(
            (bar.trim_end(), Some(fg_at("ws")), fg_at(".md.gz")),
            (
                lead.trim_end(),
                theme.get(crate::theme::scope::UI_TABBAR_NAME).fg,
                Color::Rgb(md[0], md[1], md[2]),
            ),
            "the bar text, the workspace name in its color, then the active gzip tab's suffix \
             in the markdown color",
        );
        assert_eq!(
            h.stoat.tab_bar_spans,
            [(5, 16), (16, 26), (26, 43)].map(|(start, end): (u16, u16)| start * 16..end * 16),
            "each tab's span in sixteenths, from its label's lead space to its suffix",
        );
    }

    /// Under a live scene a hairline parts the workspace name from the tabs, and
    /// each suffix draws in its tag's color at the label's Ui type scale.
    ///
    /// The active tab draws as one chip in its background, rounded at 4 and one
    /// logical pixel inside the row's top and bottom, over the tab's cells.
    ///
    /// The cells match the plain bar, since every role scales to a whole cell
    /// under `cfg(test)`. The name ends at column 4, where the hairline stands
    /// 12 sixteenths tall from sixteenth 2, and the bar draws no other mark.
    #[test]
    fn the_live_tab_bar_divides_the_name_boxes_the_active_tab_and_colors_each_suffix() {
        use super::{style_rgb, BorderStyle, PanelShadow, TypeRole};
        use ratatui::style::Color;
        use stoatty_protocol::command::{Command, Edge, PanelCommand};

        let mut h = tagged_tabs_harness();
        h.render_composited();

        let theme = &h.stoat.theme;
        let rgb = |color: Option<Color>| style_rgb(color).expect("the theme colors it in RGB");
        let active = rgb(theme.get(crate::theme::scope::UI_TABBAR_ACTIVE).bg);
        let rust = rgb(theme.get(crate::theme::scope::UI_TAG_RUST).fg);
        let md = rgb(theme.get(crate::theme::scope::UI_TAG_MARKDOWN).fg);
        let divider = rgb(theme.get(crate::theme::scope::UI_BORDER_INACTIVE).fg);
        let tab = PanelCommand {
            top: 0,
            left: 26,
            width: 17,
            height: 1,
            style: BorderStyle::None_,
            border: active,
            corner_radius: 4,
            fill: Some(active),
            shadow: PanelShadow::None_,
            edges: Edge::symmetric(0, 1),
            corners: 0,
            fillets: 0,
            fillet_radius: 2,
            hatch: None,
            above_pools: false,
            backdrop: false,
            anchor: None,
        };

        let scene = stoatty_protocol::command::decode_stream(&h.stoat.apc_scene.all_bytes());
        let panels: Vec<PanelCommand> = scene
            .iter()
            .filter_map(|command| match command {
                Command::Panel(panel) if panel.top == 0 => Some(*panel),
                _ => None,
            })
            .collect();
        let marks: Vec<_> = scene
            .iter()
            .filter_map(|command| match command {
                Command::Bar(bar) if bar.y < 16 && (2..16).contains(&bar.height) => {
                    Some((bar.x, bar.y, bar.width, bar.height, bar.color))
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            (panels, marks),
            (vec![tab], vec![(64, 2, 1, 12, divider)]),
            "the active tab's chip, and the name's hairline alone",
        );

        let ui = theme.type_scale(TypeRole::Ui);
        let suffixes: Vec<(&str, u16, [u8; 3])> = scene
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run)
                    if run.row < 16 && [".rs ", ".md.gz "].contains(&run.text.as_str()) =>
                {
                    Some((run.text.as_str(), run.scale, run.color))
                },
                _ => None,
            })
            .collect();
        assert_eq!(
            suffixes,
            [(".rs ", ui, rust), (".rs ", ui, rust), (".md.gz ", ui, md)],
            "each suffix's run carries the label's scale and its tag's color",
        );
    }

    /// The tab bar gives every tab one box, the active tab's a panel.
    #[test]
    fn the_tab_segments_box_each_tab() {
        use super::{tab_segments, BoxShape};

        let h = tagged_tabs_harness();
        let theme = &h.stoat.theme;
        let styles = (
            theme.get(crate::theme::scope::UI_TABBAR_ACTIVE),
            theme.get(crate::theme::scope::UI_TABBAR_INACTIVE),
        );
        let segments = tab_segments(h.stoat.active_workspace(), theme, "ws", styles);
        let boxes: Vec<_> = segments
            .boxes
            .iter()
            .map(|run_box| (run_box.segments.clone(), run_box.shape))
            .collect();

        assert_eq!(
            boxes,
            vec![(2..4, None), (4..6, None), (6..8, Some(BoxShape::Tab))],
        );
    }

    /// Tabs over `chrome.rs`, `panel.rs`, and a gzip `CHANGELOG.md.gz`, the last
    /// one active, in a 60 by 8 harness whose workspace is `ws`.
    fn tagged_tabs_harness() -> crate::test_harness::TestHarness {
        let mut h = crate::test_harness::TestHarness::with_size(60, 8);
        h.stoat.active_workspace_mut().name = "ws".to_string();
        let chrome = h.write_file("chrome.rs", "fn a() {}\n");
        h.open_file(&chrome);
        dispatch(&mut h.stoat, &stoat_action::NewTab);
        let panel = h.write_file("panel.rs", "fn b() {}\n");
        h.open_file(&panel);
        dispatch(&mut h.stoat, &stoat_action::NewTab);
        let changelog = PathBuf::from("/test/CHANGELOG.md.gz");
        h.fake_fs()
            .insert_file(&changelog, crate::test_harness::gzip(b"# 1.0\n"));
        h.open_file(&changelog);
        h.settle();
        // A gzip file reads on the blocking pool, and the read installs here.
        buffer_lifecycle::install_pending_opens(&mut h.stoat);
        h
    }

    /// A conflicted file's tab leads with `!` in the conflict mark's color once
    /// the tally lists the file. The conflict view's tab names the file it
    /// resolves and keeps the mark. While the view is focused the bar ends in a
    /// chip that counts the files left and names the chunk at the cursor.
    #[test]
    fn the_tab_bar_marks_a_conflicted_file_and_ends_in_the_chunk_chip() {
        let mut h = conflict_tabs_harness(&["f.txt"]);
        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: PathBuf::from("/repo/f.txt"),
            },
        );
        h.settle_diff_jobs();
        let file_bar = tab_bar_row(&h.render_composited());

        dispatch(&mut h.stoat, &Conflict);
        h.settle();
        let buf = h.render_composited();
        let scene = stoatty_protocol::command::decode_stream(&h.stoat.apc_scene.all_bytes());
        let panels: Vec<_> = scene
            .iter()
            .filter_map(|command| match command {
                stoatty_protocol::command::Command::Panel(panel) if panel.top == 0 => {
                    Some((panel.left, panel.width, panel.fill))
                },
                _ => None,
            })
            .collect();
        let chip_text = scene.iter().find_map(|command| match command {
            stoatty_protocol::command::Command::TextRun(run) if run.text.contains("unresolved") => {
                Some((run.col, run.text.clone(), run.color))
            },
            _ => None,
        });

        let text = "1 unresolved \u{b7} 1 / 1 chunks";
        assert_eq!(
            (
                file_bar.chars().take(30).collect::<String>(),
                tab_bar_row(&buf).chars().take(30).collect::<String>(),
                buf[(6, 0)].fg,
                panels,
                chip_text,
            ),
            (
                format!("{:<30}", " ws   ! f.txt"),
                format!("{:<30}", " ws   ! f.txt"),
                ratatui::style::Color::Rgb(0xf5, 0x9a, 0xa0),
                vec![
                    (5, 9, Some([0x24, 0x35, 0x52])),
                    (71, 28, Some([0x33, 0x20, 0x1f])),
                ],
                Some((71 * 16 + 8, text.to_string(), [0xf2, 0x77, 0x7f])),
            ),
            "the active tab's chip in the accent wash, then the chunk chip one cell before \
             the bar's edge, its text 8/16 in",
        );
    }

    /// A tab whose file the conflict view applied reads U+2713 in the added
    /// color, and the view's own tab marks the next file it shows.
    #[test]
    fn the_tab_bar_marks_an_applied_file_resolved() {
        let mut h = conflict_tabs_harness(&["a.txt", "b.txt"]);
        dispatch(
            &mut h.stoat,
            &OpenFile {
                path: PathBuf::from("/repo/a.txt"),
            },
        );
        dispatch(&mut h.stoat, &stoat_action::NewTab);
        h.settle_diff_jobs();
        dispatch(&mut h.stoat, &Conflict);
        h.settle();
        dispatch(&mut h.stoat, &stoat_action::ConflictPickOurs);
        dispatch(&mut h.stoat, &stoat_action::ConflictApply);
        h.settle();

        let buf = h.render_composited();
        let chip = stoatty_protocol::command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .find_map(|command| match command {
                stoatty_protocol::command::Command::TextRun(run)
                    if run.text.contains("unresolved") =>
                {
                    Some(run.text)
                },
                _ => None,
            });
        let added = h
            .stoat
            .theme
            .get(crate::theme::scope::DIFF_ADDED)
            .fg
            .expect("the theme colors added text");
        assert_eq!(
            (
                tab_bar_row(&buf).chars().take(30).collect::<String>(),
                buf[(6, 0)].fg,
                chip,
            ),
            (
                format!("{:<30}", " ws   \u{2713} a.txt  ! b.txt"),
                added,
                Some("1 unresolved \u{b7} 1 / 1 chunks".to_string()),
            ),
            "the chip counts the one file the view has not applied",
        );
    }

    /// An unmerged file's `!` takes `vcs.conflict.mark`, or the conflict
    /// header's color in a theme without that scope. A resolved file's mark
    /// takes the added color.
    #[test]
    fn a_tab_mark_takes_the_conflict_mark_or_the_header_color() {
        let (config, errors) = stoat_config::parse(crate::app::DEFAULT_KEYMAP);
        assert!(errors.is_empty(), "parse errors: {errors:?}");
        let config = config.expect("the shipped config parses");
        let marks = |name: &str| {
            let theme = crate::theme::Theme::from_config(&config, name).expect("the theme loads");
            [TabConflictMark::Conflicted, TabConflictMark::Resolved]
                .map(|mark| tab_mark(mark, ratatui::style::Style::default(), &theme))
        };
        let segment = |text: &str, [r, g, b]: [u8; 3]| {
            (
                text.to_string(),
                ratatui::style::Style::default().fg(ratatui::style::Color::Rgb(r, g, b)),
            )
        };

        assert_eq!(
            [marks("ermine_night"), marks("default_dark")],
            [
                [
                    segment(" !", [0xf5, 0x9a, 0xa0]),
                    segment(" \u{2713}", [0x8f, 0xd1, 0x8a])
                ],
                [
                    segment(" !", [0xe0, 0x6c, 0x75]),
                    segment(" \u{2713}", [0x98, 0xc3, 0x79])
                ],
            ],
        );
    }

    /// A 100 by 12 harness whose tab bar always shows, in workspace `ws` over
    /// a fake repository at `/repo` where each of `files` is conflicted.
    fn conflict_tabs_harness(files: &[&str]) -> crate::test_harness::TestHarness {
        let mut h = crate::test_harness::TestHarness::with_size(100, 12);
        h.stoat.settings.ui_tab_bar = Some(stoat_config::TabBarMode::Always);
        h.stoat.set_diff_warm_auto(true);
        let ws = h.stoat.active_workspace_mut();
        ws.name = "ws".to_string();
        ws.git_root = PathBuf::from("/repo");
        let mut repo = h.fake_git().add_repo("/repo").with_fs(h.fake_fs());
        for file in files {
            repo.conflicted_file(
                file,
                Some("a\nbase\n"),
                Some("a\nours\n"),
                Some("a\ntheirs\n"),
            );
        }
        h
    }

    /// The tab bar's row of `buf`, with the composite's blank bar cells read as
    /// spaces.
    fn tab_bar_row(buf: &Buffer) -> String {
        (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>()
            .replace('─', " ")
    }

    /// The status row names the workspace until a second tab shows the tab
    /// bar, which then carries the name. The file name then meets the mode
    /// chip across a gap cell.
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
        assert_eq!([one_tab, two_tabs], [" NOR  ws scratch ", " NOR  scratch "]);
    }

    /// A focused run pane's row leads with the run chip in either mode of its
    /// input and names the directory its commands run in at its right end,
    /// where an editor's row shows the cursor. The repo segment stays off the
    /// row, as the StartRun board draws the directory alone.
    #[test]
    fn a_focused_run_pane_s_row_reads_run_and_its_directory() {
        let mut h = crate::test_harness::TestHarness::with_size(80, 12);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_fs().insert_file(Path::new("/repo/f.txt"), b"a\n");
        h.fake_git()
            .add_repo("/repo")
            .commit("a1000000", &[("f.txt", "a\n")])
            .head_file("f.txt", "a\n")
            .index_file("f.txt", "a\n")
            .set_head_branch("main");
        h.stoat.set_diff_warm_auto(true);
        h.open_file(Path::new("/repo/f.txt"));
        h.settle_diff_jobs();
        let run = h.open_run();
        h.stoat.home = Some(PathBuf::from("/home/u"));
        h.stoat.active_workspace_mut().runs[run].cwd = PathBuf::from("/home/u/code/stoat");
        let row = |h: &mut crate::test_harness::TestHarness| {
            let row = bar_row(&h.render_composited()).replace('\u{2500}', " ");
            row.split_whitespace().collect::<Vec<_>>().join(" ")
        };

        let insert = row(&mut h);
        h.type_keys("escape");
        let normal = row(&mut h);

        assert_eq!(
            [insert, normal],
            ["RUN repo run ~/code/stoat"; 2].map(String::from),
        );
    }
}
