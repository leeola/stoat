use crate::{
    action_handlers::search::StatusPrompt,
    app::{MessageKind, SPINNER_FRAMES, STATUS_ROW_PAD, TAB_BAR_PAD},
    blame::{self, LineBlame},
    buffer::BufferId,
    buffer_registry::{BufferRegistry, ReadOnly},
    conflict_session::{ConflictLabels, ConflictPosition},
    diff_changes::{DiffChangesId, DiffChangesState, ExcerptStats},
    diff_map::MarkKind,
    display_map::FoldScope,
    editor_state::{EditorId, EditorState},
    host::git::FileKind,
    jobs::{self, TraySpan},
    minimap::RAIL_MAX_COLUMNS,
    pane::{Divider, DividerOrientation, FocusTarget, Pane, PaneTree, Placement, View},
    render::{
        blame::{BlameLines, BlamePaint},
        chrome, commits,
        conflict_view::ConflictLayout,
        diff_changes::{self, MergeNote},
        editor::{editor_cursor_position, render_editor_with_overlay, split_minimap_strip},
        job_tray,
        paint::{dim_rgb, floor_rgb, luma, style_rgb, FAINT_CONTRAST_FLOOR},
        pixel_rows::RowMetrics,
        popout::{
            paint_popout_card, popout_area, popout_card_bg, popout_inset, scaled_char_capacity,
            wrap_popout_lines,
        },
        rail_strip_id, review,
        run_pane::render_run_pane,
        term_pane::render_term_pane,
        text,
        undercurl::UndercurlBatch,
        FrameCtx, MinimapChrome, PaneCtx, TypeScale,
    },
    run::{Cmd, CmdId, RunId, RunState},
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
    array,
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
pub const MINIMAP_MAX_COLUMNS: u8 = 120;

/// The corner radius of a strip's viewport band, in logical px, as the Editor
/// and ReadOnly boards round it.
pub(super) const MINIMAP_THUMB_RADIUS: u8 = 4;

/// The tab bar's conflict chip's `padding: 0 8px`, two sixteenths to the px.
pub(super) const CHIP_PAD: u16 = 16;

/// The top of the hairline that parts the tab bar's workspace name from its
/// tabs, in sixteenths down the row.
const TAB_DIVIDER_TOP: i16 = 2;

/// The height of that hairline in sixteenths, which centers it in the row.
///
/// The Editor board draws a 14 px rule, and a sixteenth of the 18 px row is
/// 1.125 px, so 12 sixteenths are the nearest whole count.
const TAB_DIVIDER_HEIGHT: u16 = 12;

/// The logical px the active tab's pill stands outside its row at the top and
/// the bottom inside a tab bar band.
///
/// The boards draw the pill 22 px tall, `padding: 1px 6px` on a 20 px line,
/// over the bar's 18 px cell row.
const TAB_PILL_OUTSET: u8 = 2;

/// The logical pixels the boards leave between an island and the window's
/// side, and between two sibling islands.
const ISLAND_SPACING: u8 = 8;

/// The corner radius of a read-only editor's dashed ring, which the ReadOnly
/// board draws as a 2 px curve whatever radius the islands take.
pub const READ_ONLY_RING_RADIUS: u8 = 2;

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
        cmds,
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
                // An unfocused pane selects nothing, as its cursor is not the
                // reader's.
                let selected = is_focused
                    .then(|| editor_cursor_position(editor))
                    .flatten()
                    .and_then(|(line, _)| frame.blame.commit_index(editor.buffer_id, line - 1));
                let blame = frame
                    .chrome
                    .blame
                    .as_ref()
                    .filter(|_| pane.blames(editor))
                    .map(|colors| BlamePaint {
                        colors,
                        lines: frame.blame.rows(editor.buffer_id).map(|rows| BlameLines {
                            rows,
                            base: frame.blame.base(editor.buffer_id).map(|base| base.as_ref()),
                            now_epoch: frame.now_epoch,
                            selected,
                        }),
                    });
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
                    blame,
                    true,
                );
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
            if let Some(run_state) = runs.get_mut(*run_id) {
                let pixels = RowMetrics::of(theme, scene)
                    .filter(|_| scene.inks())
                    .map(|m| (m, island_reach(content_area, frame, m)));
                render_run_pane(
                    run_state,
                    cmds,
                    terms,
                    editors,
                    theme,
                    frame.workspace_root,
                    frame.home,
                    content_area,
                    is_focused,
                    pixels,
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
/// inside it.
///
/// The island reaches past its cells as [`island_edges`] sets its sides, so the
/// strips it reaches across take the surface. Its border is the window ground,
/// which the renderer paints over the corner arcs inside the pane's cells.
pub(crate) fn render_island(content_area: Rect, frame: FrameCtx<'_>, scene: &mut ApcScene) {
    let theme = frame.theme;
    let inset_x = theme.panel(PanelRole::Island).inset_x;
    if let Some(panel) = island_panel(island_edges(content_area, frame, inset_x), theme) {
        panel.draw_components(content_area, scene);
    }
}

/// The halo of an island whose sides reach as `edges` sets them.
///
/// The fill is the editor surface and the border is the window ground, at the
/// island role's radius and shadow. `None` under a theme with no RGB ground,
/// which sets nothing apart from the islands.
fn island_panel(edges: [Edge; 4], theme: &crate::theme::Theme) -> Option<Panel> {
    let ground = style_rgb(crate::render::ground_surface(theme))?;
    let role = theme.panel(PanelRole::Island);
    let surface = style_rgb(
        theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|style| style.bg),
    );
    Some(Panel {
        style: BorderStyle::None_,
        border: ground,
        corner_radius: role.corner_radius,
        fill: surface,
        shadow: role.shadow,
        edges,
        corners: 0,
        fillets: 0,
        fillet_radius: 2,
        hatch: None,
        above_pools: false,
        backdrop: true,
        anchor: None,
    })
}

/// The sides of the island over `area`, in [`Edge`] order: top, right,
/// bottom, then left.
///
/// A left or right side that meets the window's chrome margin reaches into it,
/// and one that meets the gap toward a sibling pane reaches into that gap, so
/// the island stands [`ISLAND_SPACING`] pixels from the window and from its
/// siblings. A gap divider counts when it spans the island's rows. The gap
/// column left of the single minimap band counts too, since the band stands
/// as an island of its own. Any other left or right side keeps the island
/// role's `inset_x`.
///
/// Without chrome bands, the top and the bottom stay flush, since each meets
/// a status row or the tab bar, and never a gap. With them, the bottom reaches
/// into the status band's spare row under the pane. It stops twice
/// [`STATUS_ROW_PAD`] short of the status row, so the band's ground starts
/// that far above the row. A top pane under a shown tab bar reaches up into
/// the bar band's spare row the same way. It stops twice [`TAB_BAR_PAD`]
/// below the bar's cell row, where the 32 px band ends.
fn island_edges(area: Rect, frame: FrameCtx<'_>, inset_x: u8) -> [Edge; 4] {
    let root = frame.pane_root;
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
    let at_root_right = area.right() == root.right();
    let banded = frame.minimap_band.is_some();

    [
        island_top(frame, area.y == root.y),
        side(
            at_root_right && !banded,
            vertical(area.right()) || (at_root_right && banded),
            Edge::Inset(inset_x),
        ),
        island_bottom(frame),
        side(
            area.x == root.x,
            area.x.checked_sub(1).is_some_and(vertical),
            Edge::Inset(inset_x),
        ),
    ]
}

/// The physical px that each side of the Island over `area` stands outside
/// `area` at the cell and the scale of `m`, top, right, bottom, then left, or
/// four zeros for a frame that draws no Island.
///
/// A side inset into the area reaches a negative distance. A margin side
/// crosses the cell beside the area and stops its px short of the far side,
/// and a gap side crosses half of that, rounded down at the top and the left
/// and up at the bottom and the right, as the terminal resolves the edges of
/// the Island that [`island_edges`] sets.
pub(crate) fn island_reach(area: Rect, frame: FrameCtx<'_>, m: RowMetrics) -> [i32; 4] {
    let theme = frame.theme;
    let edges = island_edges(area, frame, theme.panel(PanelRole::Island).inset_x);
    if !frame.islands || island_panel(edges, theme).is_none() {
        return [0; 4];
    }

    let [width, height] = m.cell.map(i32::from);
    array::from_fn(|side| {
        let edge = edges[side];
        let px = i32::try_from(m.px(u16::from(edge.px()))).unwrap_or(i32::MAX);
        let across = match side % 2 {
            0 => height - px,
            _ => width - px,
        };
        match edge {
            Edge::Inset(_) => -px,
            Edge::Outset(_) => px,
            Edge::Margin(_) => across,
            Edge::Gap(_) if side == 0 || side == 3 => across.div_euclid(2),
            Edge::Gap(_) => (across + 1).div_euclid(2),
        }
    })
}

/// Declare the island halo of the single minimap `band`.
///
/// The band stands one gap column right of the pane tree, inside the window's
/// right margin, so its halo reaches into the gap as a sibling pane's does and
/// into the margin as a pane's does. Its top and bottom are those of a pane at
/// the top of the tree, so the band lines up with the panes beside it and
/// leaves the status rows under them on the window ground.
pub(crate) fn render_band_island(band: Rect, frame: FrameCtx<'_>, scene: &mut ApcScene) {
    let edges = [
        island_top(frame, true),
        Edge::Margin(ISLAND_SPACING),
        island_bottom(frame),
        Edge::Gap(ISLAND_SPACING),
    ];
    if let Some(panel) = island_panel(edges, frame.theme) {
        panel.draw_components(band, scene);
    }
}

/// The top side of an island, which reaches up into the band above the pane
/// tree, the tab bar's or the checkout banner's, only from the tree's top row.
fn island_top(frame: FrameCtx<'_>, at_root_top: bool) -> Edge {
    match frame.chrome_bands && (frame.tab_bar_shown || frame.banner_shown) && at_root_top {
        true => Edge::Margin(2 * TAB_BAR_PAD),
        false => Edge::Inset(0),
    }
}

/// The bottom side of an island, which reaches into the spare row of the
/// status band under it whenever chrome bands are on.
fn island_bottom(frame: FrameCtx<'_>) -> Edge {
    match frame.chrome_bands {
        true => Edge::Margin(2 * STATUS_ROW_PAD),
        false => Edge::Inset(0),
    }
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
        ReadOnly::Revision => "read only \u{b7} revision",
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
        italic: false,
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
        mark_class: chrome.mark_class,
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
        mark_class: 0,
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

/// The names of the two sides a diff view compares for the file at `path`, as
/// `(base, target)`.
///
/// A pair names the sides of its own file, and the workspace base names those
/// of every other file.
fn diff_sides_for<'a>(path: Option<&Path>, frame: FrameCtx<'a>) -> (&'a str, &'a str) {
    path.and_then(|path| frame.pair_sides.get(path))
        .map_or(frame.diff_sides, |(from, to)| (from.as_str(), to.as_str()))
}

/// Where a diff view's status row reads the diff's git metrics.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidesLayout {
    /// The row splits at this column, the diff's divider, into a base half and
    /// a target half. The base half reads the metrics, and the target half is
    /// the pane's own row.
    Split(u16),
    /// The row runs whole and reads the metrics after the file name. A unified
    /// diff has no divider to split at.
    Inline,
}

/// How the status row of a pane showing `view` over `content` lays out the
/// diff's metrics, or `None` for a pane that shows no diff.
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
pub fn dim_pane_content(buf: &mut Buffer, area: Rect, bg: [u8; 3], amount: f32) {
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
                status_message_style(base_style, frame.status_kind, frame.theme),
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
            let muted = muted_tint(base_style, theme);
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
    render_status_runs(&status.runs, frame, buf, scene, cache);
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
/// its base half and its target half. The base half takes the cell at the
/// diff's divider, which holds no segment, so the row's fill and hairline run
/// unbroken under the divide.
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
    /// Whether the run draws the bar's hairline along its top.
    hairline: bool,
    /// Runs of left segments that a live scene draws in one box of whole
    /// cells each, such as the mode chip and a tab.
    boxes: Vec<RunBox>,
}

/// A run of a [`StatusRun`]'s left segments that a live scene draws in one
/// box of whole cells, as a [`SegmentBox`] lays it out.
#[derive(Clone, Hash)]
pub struct RunBox {
    /// The run, as indices into [`StatusRun::left`].
    pub segments: Range<usize>,
    /// The panel the box draws as, or `None` for a box that only aligns the
    /// run to whole cells.
    pub shape: Option<BoxShape>,
}

/// The panel a [`RunBox`] draws as, in its first segment's background.
#[derive(Clone, Copy, Hash, PartialEq, Debug)]
pub enum BoxShape {
    /// The mode chip's box, as [`mode_chip_panel`] draws it, with its label
    /// at [`TypeRole::Mode`].
    Chip,
    /// The active tab, a chip in its background.
    Tab,
    /// A File Kind Tag, a chip in its background as [`kind_tag_panel`] draws
    /// it, with its label at [`TypeRole::Small`].
    Tag,
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
/// The boards draw the mode chip 16 px tall and 34 px wide on an 18 px row.
/// The chip's box keeps its whole cells, so the panel stands inside them. It
/// stands 1 px inside the row's top and bottom, 5 px inside the box's start,
/// and 6 px inside its end. Over the five cells of ` NOR ` at 9 px cells, that
/// is 34 by 16 px.
///
/// The box spares 11 px across its width. That count is odd, so the end takes
/// one pixel more than the start.
fn mode_chip_panel(fill: [u8; 3]) -> Panel {
    Panel {
        edges: [
            Edge::Inset(1),
            Edge::Inset(6),
            Edge::Inset(1),
            Edge::Inset(5),
        ],
        ..Panel::chip(fill)
    }
}

/// A File Kind Tag's panel in `fill`, a box smaller than [`Panel::chip`]
/// inside the same cells.
///
/// The boards draw the tag 16 px tall on an 18 px row, with 5 px of pad at
/// each side of its label. The tag's box keeps its whole cells, as the mode
/// chip's does, so the panel stands 1 px inside the row's top and bottom and
/// 2 px inside each end, and its width stands within 2 px of the board's.
fn kind_tag_panel(fill: [u8; 3]) -> Panel {
    Panel {
        edges: [
            Edge::Inset(1),
            Edge::Inset(2),
            Edge::Inset(1),
            Edge::Inset(2),
        ],
        ..Panel::chip(fill)
    }
}

/// The active tab's pill in `fill`, a [`Panel::chip`] over the tab's cells.
///
/// Inside a tab bar `band`, the pill stands [`TAB_PILL_OUTSET`] px outside the
/// row's top and bottom, the boards' 22 px on the 18 px row. The band's ground
/// holds it, because the bar's row draws [`TAB_BAR_PAD`] px down from the
/// band's top. Without a band, the pill keeps the chip's box inside its row.
pub(super) fn tab_pill_panel(fill: [u8; 3], band: bool) -> Panel {
    let chip = Panel::chip(fill);
    match band {
        true => Panel {
            edges: [
                Edge::Outset(TAB_PILL_OUTSET),
                chip.edges[1],
                Edge::Outset(TAB_PILL_OUTSET),
                chip.edges[3],
            ],
            ..chip
        },
        false => chip,
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
    let blame = match pane.view {
        View::Editor(id) => editors.get(id).is_some_and(|editor| pane.blames(editor)),
        _ => false,
    };
    let layout = match pane.view {
        View::Editor(id) => editors
            .get(id)
            .and_then(|editor| editor.conflict_layout.clone()),
        _ => None,
    };
    let mut badge = None;
    let runs = status_segments(
        &pane.view, is_focused, blame, area, sides, layout, frame, editors, buffers, terms,
        changes, &mut badge,
    );
    StatusRow { runs, badge }
}

/// The directory a run's next Cmd starts in, the workspace root `root`, while
/// the focused split pane shows a run, or `None` when focus is off the split
/// panes or the focused pane shows no run.
///
/// Takes the workspace's fields rather than the workspace, so a frame holds a
/// borrow of the root alone while the pane loop borrows the editors mutably.
pub(crate) fn focused_run_cwd<'a>(
    focus: FocusTarget,
    panes: &PaneTree,
    root: &'a Path,
) -> Option<&'a Path> {
    let FocusTarget::SplitPane = focus else {
        return None;
    };
    match panes.pane(panes.focus()).view {
        View::Run(_) => Some(root),
        _ => None,
    }
}

/// The Cmds in the stack of the run that the focused split pane shows, or
/// `None` when focus is off the split panes or the focused pane shows no run.
///
/// Like [`focused_run_cwd`] it takes the workspace's fields rather than the
/// workspace, so a frame holds a borrow of the runs alone while the pane loop
/// borrows the editors mutably.
pub(crate) fn focused_run_cmds(
    focus: FocusTarget,
    panes: &PaneTree,
    runs: &SlotMap<RunId, RunState>,
) -> Option<usize> {
    let FocusTarget::SplitPane = focus else {
        return None;
    };
    match panes.pane(panes.focus()).view {
        View::Run(id) => runs.get(id).map(|run| run.blocks.len()),
        _ => None,
    }
}

/// The Stubs of Jobs in the stack of the run that the focused split pane
/// shows, the ids whose Cmd is in Jobs, or 0 when focus is off the split
/// panes or the focused pane shows no run.
///
/// It takes the workspace's fields for the reason that
/// [`focused_run_cmds`] does.
pub(crate) fn focused_run_stubs(
    focus: FocusTarget,
    panes: &PaneTree,
    runs: &SlotMap<RunId, RunState>,
    cmds: &SlotMap<CmdId, Cmd>,
) -> usize {
    let FocusTarget::SplitPane = focus else {
        return 0;
    };
    let View::Run(id) = panes.pane(panes.focus()).view else {
        return 0;
    };
    runs.get(id).map_or(0, |run| {
        run.blocks
            .iter()
            .filter(|id| cmds.get(**id).is_some_and(|cmd| cmd.in_jobs))
            .count()
    })
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
///
/// `blame` is whether the pane shows the Blame Column, as
/// [`Pane::blames`](crate::pane::Pane::blames) reads it. The row then names it
/// after the file name, and names the cursor line's commit left of the
/// position, as [`status_row`] does in the main window.
///
/// A conflict view's row runs whole, as the cell layout's row does. The window
/// streams the view's rows alone, so no paint places the grid's columns in the
/// row's cells.
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
    blame: bool,
) -> PaneStatusCells {
    let sides = sides_layout(view, content, frame, editors);
    let mut runs = status_segments(
        view, is_focused, blame, area, sides, None, frame, editors, buffers, terms, changes,
        &mut None,
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
pub struct StatusSceneCache {
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
/// draws its label at [`TypeRole::Mode`], a File Kind Tag's box at
/// [`TypeRole::Small`], and every other segment at [`TypeRole::Ui`].
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
                        BoxShape::Tab => vec![tab_pill_panel(first.bg, frame.chrome_bands)],
                        BoxShape::Tag => vec![kind_tag_panel(first.bg)],
                    }),
                scale: match run_box.shape {
                    Some(BoxShape::Chip) => Some(frame.theme.type_scale(TypeRole::Mode)),
                    Some(BoxShape::Tag) => Some(frame.theme.type_scale(TypeRole::Small)),
                    Some(BoxShape::Tab) | None => None,
                },
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
/// The bar ends in the Job Tray, a Tray Chip for each Job of `ws`, as
/// [`job_tray::place_tray`] places them left of `cover`, the cells of the
/// top-right badges, and [`job_tray::draw_tray`] draws them. The tabs and the
/// chips above lay out left of the tray.
///
/// Leaves in `spans` the extent of each painted tab, in sixteenths of a cell
/// from the left edge of `area`, so a pointer press resolves against the
/// segments as painted. Answers the cells of each Tray Chip that it drew, and
/// its pixels where it stands at the board's pixels, with the Cmd of its Job,
/// which a press on a chip and the Glance read.
pub(crate) fn render_tab_bar(
    ws: &Workspace,
    area: Rect,
    frame: FrameCtx<'_>,
    cover: Option<Rect>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    spans: &mut Vec<Range<u16>>,
) -> Vec<TraySpan> {
    spans.clear();
    if area.width == 0 || area.height == 0 {
        return Vec::new();
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

    let views = jobs::views(ws);
    let tray_colors = scene.live().then(|| job_tray::tray_colors(theme)).flatten();
    let tray_scale = tray_colors
        .as_ref()
        .map(|_| theme.type_scale(TypeRole::Caption));
    let tray_pixels = scene
        .pixel_rows()
        .map(|(cell, scale)| RowMetrics { cell, scale });
    let placed = job_tray::place_tray(&views, area, cover, tray_scale, tray_pixels);
    let lead = match placed.first() {
        Some(place) => Rect {
            width: place.cells.start - area.x,
            ..area
        },
        None => area,
    };

    // The chip takes its cells out of the run. It stands one blank cell before
    // the layout chips, which keep the bar's end, or with no layout chips one
    // blank cell ends the bar after it. It drops first where the tabs leave it
    // no room, and the layout chips drop after it.
    let chip = conflict_chip(ws, frame, scene.live()).filter(|chip| {
        let taken = match right.is_empty() {
            true => usize::from(chip.cells),
            false => chars(&right) + usize::from(chip.cells) + 2,
        };
        chars(&left) + taken < usize::from(lead.width)
    });
    let chip_rect = chip.as_ref().map(|chip| {
        let end = lead.right() - chars(&right) as u16 - 1;
        Rect::new(end - chip.cells, lead.y, chip.cells, 1)
    });
    let segments = match chip_rect {
        Some(rect) if !right.is_empty() => Rect {
            width: rect.x - 1 - lead.x,
            ..lead
        },
        Some(rect) => Rect {
            width: rect.x - lead.x,
            ..lead
        },
        None => lead,
    };
    if chars(&left) + chars(&right) > usize::from(lead.width) {
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
        lead,
        inactive,
        rich.then(|| theme.type_scale(TypeRole::Caption)),
        buf,
        scene,
    );
    if let (Some(chip), Some(rect)) = (chip, chip_rect) {
        draw_conflict_chip(&chip, rect, theme, buf, scene);
    }
    job_tray::draw_tray(
        &views,
        &placed,
        tray_colors.as_ref(),
        area,
        frame,
        buf,
        scene,
    );

    let texts: Vec<&str> = runs[0].left.iter().map(|(text, _)| text.as_str()).collect();
    let tabs: Vec<Range<usize>> = runs[0]
        .boxes
        .iter()
        .map(|run_box| run_box.segments.clone())
        .collect();
    spans.extend(status_bar::box_spans(&texts, &tabs, scale, segments.width));

    placed
        .into_iter()
        .map(|place| TraySpan {
            cmd: views[place.index].id,
            cells: place.cells,
            px: place.px.map(|(px, _)| px),
        })
        .collect()
}

/// The tab bar's left segments, the workspace name and then each tab, with
/// what a live scene draws them with.
pub struct TabSegments {
    left: Vec<StatusSeg>,
    /// One box per tab, the active tab's a [`BoxShape::Tab`].
    pub boxes: Vec<RunBox>,
}

/// The tab bar's left segments for `ws` under `workspace_name`, the active
/// tab in the first of `styles` and the rest in the second.
///
/// A tab's segments are its conflict mark and its label. A tagged file's label
/// is its stem, and its extension follows as a suffix in the tag's color.
pub fn tab_segments(
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

/// The chip the tab bar ends in while the conflict view is focused, or holds
/// left of its layout chips while a changes view is focused during a merge.
struct ConflictChip {
    /// The operation and what is left of it, as [`conflict_chip_text`] or
    /// [`merge_badge_text`] words them.
    text: String,
    style: Style,
    /// The chip's text color under a live scene, where it paints as a panel
    /// and a run. `None` paints it in cells.
    rich: Option<[u8; 3]>,
    cells: u16,
}

/// The tab bar's [`ConflictChip`], or `None` with no chip to show.
///
/// The conflict screen shows the chip that [`conflict_chip_text`] words. Any
/// other screen shows the badge that [`merge_badge_text`] words, while a
/// changes view is focused during a merge that leaves a file unmerged.
///
/// Under a live scene the chip's text draws at the Caption scale, the board's
/// 11 px chrome text, so its width follows that text and the board's 8 px of
/// padding on each side. In cells it takes its text and a cell on each side.
fn conflict_chip(ws: &Workspace, frame: FrameCtx<'_>, live: bool) -> Option<ConflictChip> {
    use crate::theme::scope;
    let position = frame
        .conflict_position
        .filter(|_| frame.screen == Some("conflict"));
    let text = match position {
        Some(position) => {
            let session = ws.conflict.as_ref()?;
            let view = ws
                .editors
                .get(session.file.editor_id)?
                .conflict_view
                .as_ref()?;
            conflict_chip_text(&position, &view.labels)
        },
        None => merge_badge_text(ws, frame)?,
    };

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

/// The tab bar's conflict chip text, which names the operation and what is
/// left of it.
///
/// A merge names theirs after `merge`, as in `merge main`, so the chip says
/// what merges in. A rebase reads `rebase`, and conflicts with neither behind
/// them read `conflict`. The chunks left count every file of the session, and
/// the files count those that still hold an open chunk.
fn conflict_chip_text(position: &ConflictPosition, labels: &ConflictLabels) -> String {
    let lead = match position.operation {
        "merge" => format!("merge {}", labels.theirs_short()),
        operation => operation.to_string(),
    };
    let noun = if position.chunks_open == 1 {
        "conflict"
    } else {
        "conflicts"
    };
    format!(
        "{lead} \u{b7} {} {noun} left \u{b7} {} of {} files",
        position.chunks_open, position.files_open, position.file_total
    )
}

/// The tab bar's badge over a focused changes view during a merge, which names
/// the branch the merge brings in and counts what is left to resolve.
///
/// `None` off a changes view, with no merge in progress, and with no file that
/// the tally lists as unmerged. The conflicts count the blocks that the stack's
/// excerpts of the unmerged files hold, so a file whose excerpt waits on its
/// diff adds none until its cut. The files count every unmerged file.
fn merge_badge_text(ws: &Workspace, frame: FrameCtx<'_>) -> Option<String> {
    let state = ws.focused_diff_changes()?;
    let branch = frame.repo?.merge_head.as_deref()?;
    let conflicted = &frame.tallies?.conflicted;
    if conflicted.is_empty() {
        return None;
    }

    let conflicts: u32 = state
        .excerpts
        .iter()
        .filter(|excerpt| {
            conflicted
                .binary_search_by(|path| path.as_path().cmp(Path::new(&excerpt.rel_path)))
                .is_ok()
        })
        .map(|excerpt| excerpt.conflicts)
        .sum();
    let count = |n: usize, noun: &str| match n {
        1 => format!("1 {noun}"),
        n => format!("{n} {noun}s"),
    };
    Some(format!(
        "merge {branch} \u{b7} {} \u{b7} {}",
        count(conflicts as usize, "conflict"),
        count(conflicted.len(), "file"),
    ))
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
        italic: false,
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
            italic: false,
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

/// `style` with the foreground of `ui.text.muted`, lifted to
/// [`FAINT_CONTRAST_FLOOR`] off `style`'s background when it sits closer.
///
/// Some themes fill a status row with their muted color itself, as
/// default_dark fills its focused row, and a muted segment there does not
/// show. The lift stops at the floor, so the segment still reads dimmer than
/// the row's text.
///
/// A row whose own text sits closer to its fill than the floor keeps the muted
/// color, since its theme chose a faint row. There the lift puts the muted
/// segments above the row's own text.
pub fn muted_tint(style: Style, theme: &crate::theme::Theme) -> Style {
    let muted = tinted(style, theme, crate::theme::scope::UI_TEXT_MUTED);
    let (Some(fg), Some(text), Some(bg)) = (
        style_rgb(muted.fg),
        style_rgb(style.fg),
        style_rgb(style.bg),
    ) else {
        return muted;
    };
    if (luma(text) - luma(bg)).abs() < FAINT_CONTRAST_FLOOR {
        return muted;
    }

    let [r, g, b] = floor_rgb(fg, bg);
    muted.fg(Color::Rgb(r, g, b))
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
pub fn name_segments(
    name: &str,
    pad: &str,
    dirty: bool,
    split: bool,
    name_style: Style,
    row_style: Style,
    theme: &crate::theme::Theme,
) -> Vec<StatusSeg> {
    let muted = muted_tint(row_style, theme);
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
/// base style plus the theme's scope for the message's kind.
///
/// An alert takes `ui.message.error`. It falls back to red when the theme
/// leaves that scope undefined, so the message never blends into the bar. An
/// info message takes `ui.message.info`, and a success message takes
/// `ui.message.success`. Each takes the alert style when the theme leaves its
/// scope undefined.
///
/// That fallback is an RGB red rather than a named one because
/// [`render_status_segments`] drops the whole bar to the cell fallback on any
/// non-RGB segment color. A theme missing this scope must degrade only the
/// message's color, never the bar's rendering mode.
fn status_message_style(
    base_style: Style,
    kind: MessageKind,
    theme: &crate::theme::Theme,
) -> Style {
    let scope = match kind {
        MessageKind::Alert => None,
        MessageKind::Info => Some(crate::theme::scope::UI_MESSAGE_INFO),
        MessageKind::Success => Some(crate::theme::scope::UI_MESSAGE_SUCCESS),
    };
    if let Some(style) = scope.and_then(|scope| theme.try_get(scope)) {
        return base_style.patch(style);
    }
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
/// `area` is the whole row. With `sides`, the row shows a diff. It reads the
/// diff's git metrics, as [`diff_metrics`] words them, beside a normal pane
/// row. A split row is two runs. The base half reads the metrics from its left
/// edge and the counts against the divide, and the target half is the pane
/// row. An inline row reads the metrics after the file name. A diff's row
/// leaves out the screen label and the staged label, since the metrics name
/// the diff's kind and its base.
///
/// A focused changes view names the Scope under the cursor after the file
/// name. The Scope takes the room that the rest of the pane row leaves, less
/// the position's, so the position keeps its place.
///
/// With `blame`, the row names the commit of the cursor line left of the
/// position, as [`blame_label`] words it. Before the blame lands, it names
/// none.
///
/// A focused conflict view's row names the conflicted file in place of the
/// scratch buffer that holds the merged result. With `layout`, the grid
/// columns that the view's paint recorded, the row is three runs split at the
/// merged column's edges. The ours third reads the merge's facts, measured at
/// the Ui scale as [`merge_facts`] packs them. The merged third is the pane
/// row, and the theirs third names theirs. With no layout, the row runs whole
/// and reads the facts after the file name.
#[allow(clippy::too_many_arguments)]
fn status_segments(
    view: &View,
    is_focused: bool,
    blame: bool,
    area: Rect,
    sides: Option<SidesLayout>,
    layout: Option<ConflictLayout>,
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

    let split = match sides {
        Some(SidesLayout::Split(divider)) => Some(split_halves(area, divider, frame.badge_cover)),
        _ => None,
    };
    let conflict = frame
        .conflict_position
        .filter(|_| is_focused && frame.screen == Some("conflict"))
        .zip(match view {
            View::Editor(id) => editors
                .get(*id)
                .and_then(|editor| editor.conflict_view.clone()),
            _ => None,
        });
    let thirds = conflict
        .as_ref()
        .zip(layout)
        .map(|((position, merge), layout)| {
            let cells = split_thirds(area, layout.merged, frame.badge_cover);
            (cells, *position, merge.clone())
        });
    // A split row's pane segments fill its target half, since the base half
    // holds the diff's metrics. A conflict row's pane segments fill its merged
    // third, since the outer thirds hold the merge's facts.
    let (pane_x, pane_end) = match (split, &thirds) {
        (Some([_, (_, target)]), _) => (target.x, target.right()),
        (None, Some(([_, (_, merged), _], ..))) => (merged.x, merged.right()),
        (None, None) => (area.x, end_x),
    };

    let mut left: Vec<(String, Style)> = Vec::new();
    let mut cursor = pane_x;
    // The prompt takes the identifying left segments for as long as it is open.
    // The typed text is what the user needs to read mid-type. The workspace,
    // screen and file names are context they already have.
    let prompt = frame.status_prompt.filter(|_| is_focused);
    // An open modal's label names what the keys reach, and the file under the
    // modal is not where they go, so the label stands alone.
    let modal = frame.modal_label.filter(|_| is_focused);
    // A changes view names the Scope under its cursor after the file name.
    // The Scope takes the room that the rest of the row leaves, so it goes in
    // at this index after the rest is in.
    let mut scope_at = None;
    let mut chip_end = None;
    if is_focused {
        let (label, mode_bg) = frame.mode_label;
        let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
        push_left(
            &mut left,
            &mut cursor,
            pane_end,
            format!(" {label} "),
            mode_style,
        );
        chip_end = Some(cursor);
        if let Some(prompt) = prompt {
            push_prompt(&mut left, &mut cursor, pane_end, prompt, bar_style);
        } else {
            if !frame.tab_bar_shown {
                push_left(
                    &mut left,
                    &mut cursor,
                    pane_end,
                    format!(" {} ", frame.workspace_name),
                    bar_style.add_modifier(Modifier::BOLD),
                );
            }
            if let Some(modal_label) = modal {
                push_left(
                    &mut left,
                    &mut cursor,
                    pane_end,
                    format!(" {modal_label} "),
                    bar_style,
                );
            } else if let Some((screen_label, screen_color)) =
                screen_segment(frame.screen, theme).filter(|_| sides.is_none())
            {
                push_left(
                    &mut left,
                    &mut cursor,
                    pane_end,
                    format!(" {screen_label} "),
                    bar_style.fg(screen_color),
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
    let metrics =
        sides.map(|_| diff_metrics(view, status.buffer_id, is_focused, frame, buffers, changes));
    let name = match &conflict {
        Some((_, merge)) => Some(merge.rel_path.as_str()),
        None => status.filename.as_deref(),
    };
    if let Some(name) = name
        && prompt.is_none()
        && modal.is_none()
    {
        // A name that opens the row or meets the mode chip takes a pad cell,
        // so it stands clear of the row's edge or of the chip's box.
        let left_pad = if cursor == pane_x || chip_end == Some(cursor) {
            " "
        } else {
            ""
        };
        let name_style = if is_focused {
            tinted(bar_style, theme, crate::theme::scope::UI_TEXT)
        } else {
            bar_style
        };
        let segments = name_segments(
            name,
            left_pad,
            status.dirty,
            status.buffer_id.is_some(),
            name_style,
            bar_style,
            theme,
        );
        for (text, style) in segments {
            push_left(&mut left, &mut cursor, pane_end, text, style);
        }
        if let (Some((position, merge)), None) = (&conflict, &thirds) {
            push_left(
                &mut left,
                &mut cursor,
                pane_end,
                merge_lead(position, &merge.labels, false),
                muted_tint(bar_style, theme),
            );
        }
        if is_focused && let View::DiffChanges(id) = view {
            scope_at = Some((*id, left.len()));
        }
        if let Some(note) = read_only_note(
            status.read_only,
            status.disk_len,
            status.text_len,
            status.revision.as_deref(),
        ) {
            let muted = muted_tint(bar_style, theme);
            push_left(&mut left, &mut cursor, pane_end, note, muted);
        }
        if blame {
            let muted = muted_tint(bar_style, theme);
            push_left(
                &mut left,
                &mut cursor,
                pane_end,
                " blame ".to_owned(),
                muted,
            );
        }
    }
    if let Some(count) = frame
        .run_cmds
        .filter(|_| is_focused && prompt.is_none() && modal.is_none())
    {
        let noun = match count {
            1 => "Cmd",
            _ => "Cmds",
        };
        let text = match frame.run_stubs {
            0 => format!(" {count} {noun} "),
            stubs => format!(" {count} {noun} \u{b7} {stubs} in Jobs "),
        };
        push_left(
            &mut left,
            &mut cursor,
            pane_end,
            text,
            muted_tint(bar_style, theme),
        );
    }
    // An inline row has no base half, so its metrics follow the file name. The
    // tag's segment comes second among them. Only a per-file diff carries a
    // tag, and its row splices no Scope runs before the metrics.
    let mut inline_tag = None;
    if let Some(metrics) = metrics
        .as_ref()
        .filter(|_| split.is_none() && prompt.is_none() && modal.is_none())
    {
        inline_tag = tag_box(metrics, left.len() + 1);
        let (text, counts) = metric_segments(metrics, bar_style, theme);
        for (text, style) in text.into_iter().chain(counts) {
            push_left(&mut left, &mut cursor, pane_end, text, style);
        }
    }
    // A lone cursor paints no count, since the count only answers how many
    // places an edit lands once there is more than one.
    if let Some((_, count)) = status.selections
        && count > 1
        && prompt.is_none()
    {
        let muted = muted_tint(bar_style, theme);
        push_left(
            &mut left,
            &mut cursor,
            pane_end,
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
            pane_end,
            format!("[for {label}] "),
            bar_style.add_modifier(Modifier::BOLD),
        );
    }

    let position = cursor_pos.map(|(line, col)| format!(" {line}:{col} "));
    if let Some((id, at)) = scope_at
        && let Some(scope) = changes
            .get_mut(id)
            .and_then(|state| state.cursor_scope(editors, buffers))
    {
        let position_width = position
            .as_ref()
            .map_or(0, |text| text.chars().count() as u16);
        let room = pane_end
            .saturating_sub(cursor)
            .saturating_sub(position_width);
        let runs = scope_runs(&scope, room, bar_style, theme);
        let width: u16 = runs
            .iter()
            .map(|(text, _)| text.chars().count() as u16)
            .sum();
        cursor = cursor.saturating_add(width).min(pane_end);
        left.splice(at..at, runs);
    }

    let mut right: Vec<(String, Style)> = Vec::new();
    let mut right_anchor = pane_end;
    if let Some(text) = position {
        let width = text.chars().count() as u16;
        let start = right_anchor.saturating_sub(width);
        if start >= cursor {
            right.push((text, muted_tint(bar_style, theme)));
            right_anchor = start;
        }
    }
    let commit_label = cursor_pos
        .zip(status.buffer_id)
        .filter(|_| blame)
        .and_then(|((line, _), id)| frame.blame.line(id, line - 1))
        .map(|line| blame_label(&line, frame.now_epoch));
    if let Some(text) = commit_label {
        let width = text.chars().count() as u16;
        let start = right_anchor.saturating_sub(width);
        if start >= cursor {
            right.push((text, bar_style));
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
            let muted = muted_tint(bar_style, theme);
            right.push((text, muted));
            right_anchor = start;
        }
    }
    if is_focused {
        let badge_right = right_anchor;
        let muted = muted_tint(bar_style, theme);
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
                right.push((text, bar_style));
                right_anchor = start;
            }
        }
        if let Some(count) = frame.pending_count {
            let text = format!(" {count} ");
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, bar_style.add_modifier(Modifier::BOLD)));
                right_anchor = start;
            }
        }
        if let Some(register) = frame.recording_register {
            let text = format!(" REC {register} ");
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, bar_style.add_modifier(Modifier::BOLD)));
                right_anchor = start;
            }
        }
        // The severities push from hint to error, so they read error first.
        let severities =
            focused_diagnostic_label(view, editors, buffers, frame.diagnostics, bar_style, theme);
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
                (bar_style, theme),
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
                right.push((
                    text,
                    status_message_style(bar_style, frame.status_kind, theme),
                ));
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
                right.push((text, bar_style));
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
                right.push((text, bar_style.add_modifier(Modifier::ITALIC)));
                right_anchor = start;
            }
        }
        #[cfg(feature = "perf")]
        if let Some(seg) = frame.perf {
            let text = crate::render::perf_label(seg);
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, bar_style.add_modifier(Modifier::DIM)));
            }
        }
    }
    let _ = cursor;
    let _ = right_anchor;

    match (split, thirds) {
        (Some([(base, base_segments), (target, target_segments)]), _) => {
            let (base_left, base_right) = metrics
                .as_ref()
                .map(|metrics| metrics_half(metrics, base_segments, bar_style, theme))
                .unwrap_or_default();
            let base_boxes = metrics
                .as_ref()
                .and_then(|metrics| tag_box(metrics, 1))
                .into_iter()
                .collect();
            vec![
                StatusRun {
                    area: base,
                    segments: base_segments,
                    style: bar_style,
                    left: base_left,
                    right: base_right,
                    hairline: true,
                    boxes: base_boxes,
                },
                StatusRun {
                    area: target,
                    segments: target_segments,
                    style: bar_style,
                    left,
                    right,
                    hairline: true,
                    boxes: mode_chip_box(chip_end.is_some()),
                },
            ]
        },
        (None, Some((cells, position, merge))) => {
            let [(ours, ours_segments), (merged, merged_segments), (theirs, theirs_segments)] =
                cells;
            let (facts, open) =
                merge_facts(&position, &merge.labels, ours_segments, bar_style, theme);
            let theirs_label = format!(" {} ", merge.labels.theirs_label());
            vec![
                StatusRun {
                    area: ours,
                    segments: ours_segments,
                    style: bar_style,
                    left: facts,
                    right: open,
                    hairline: true,
                    boxes: Vec::new(),
                },
                StatusRun {
                    area: merged,
                    segments: merged_segments,
                    style: bar_style,
                    left,
                    right,
                    hairline: true,
                    boxes: mode_chip_box(chip_end.is_some()),
                },
                StatusRun {
                    area: theirs,
                    segments: theirs_segments,
                    style: bar_style,
                    left: vec![(theirs_label, muted_tint(bar_style, theme))],
                    right: Vec::new(),
                    hairline: true,
                    boxes: Vec::new(),
                },
            ]
        },
        (None, None) => vec![StatusRun {
            area,
            segments,
            style: bar_style,
            left,
            right,
            hairline: true,
            boxes: mode_chip_box(chip_end.is_some())
                .into_iter()
                .chain(inline_tag)
                .collect(),
        }],
    }
}

/// The status row's segment for the commit of a blamed pane's cursor line.
///
/// A committed line reads its short sha, the first word of its author, and its
/// date, as [`blame::short_date`] words it against `now_epoch`.
fn blame_label(line: &LineBlame<'_>, now_epoch: i64) -> String {
    match line {
        LineBlame::Uncommitted => " uncommitted ".to_owned(),
        LineBlame::Commit { commit, .. } => format!(
            " {} \u{b7} {} \u{b7} {} ",
            commit.info.short_sha,
            blame::author_word(&commit.info.author_name),
            blame::short_date(commit.info.time, commit.utc_offset_minutes, now_epoch),
        ),
    }
}

/// The git metrics a diff view's status row reads.
///
/// The text reads `lead`, then the File Kind Tag of a per-file diff's file in
/// its own style, then `tail`. A row with no tag reads `lead` alone.
#[derive(Default)]
struct DiffMetrics {
    lead: String,
    /// The tag's text and its style, a foreground on a box's background.
    tag: Option<(String, Style)>,
    tail: String,
    /// The lines the diff adds.
    added: u32,
    /// The lines the diff removes.
    removed: u32,
}

/// The git metrics of the diff that `view` shows.
///
/// A changes view names its base, then during a merge the branch the merge
/// brings in, counts its files as [`stack_counts`] does, and names the hunk at
/// the cursor. A per-file diff names its base, its
/// screen's kind, and the hunk at the cursor. A per-file diff of a file that
/// the tally names moved, untracked, new, or deleted names the kind in a File
/// Kind Tag in place of the screen's, as [`kind_metrics`] reads it.
///
/// Only the focused pane names a hunk, since the hunk position is the focused
/// pane's. The per-file diff counts the lines of its own buffer, and a changes
/// view the lines of its whole stack, as its file headers count them.
fn diff_metrics(
    view: &View,
    buffer_id: Option<BufferId>,
    is_focused: bool,
    frame: FrameCtx<'_>,
    buffers: &BufferRegistry,
    changes: &SlotMap<DiffChangesId, DiffChangesState>,
) -> DiffMetrics {
    let path = buffer_id.and_then(|id| buffers.path_for(id));
    let base = diff_sides_for(path, frame).0;
    let hunk = frame
        .hunk_position
        .as_ref()
        .filter(|_| is_focused)
        .map(|position| {
            let at = position
                .hunk_at
                .map_or_else(|| "-".to_string(), |at| at.to_string());
            format!("hunk {at} / {}", position.hunk_total)
        });

    if let View::DiffChanges(id) = view {
        let mut parts = vec![format!("vs {base}")];
        parts.extend(
            frame
                .repo
                .and_then(|repo| repo.merge_head.as_deref())
                .map(|name| format!("merging {name}")),
        );
        let (added, removed) = changes.get(*id).map_or((0, 0), |state| {
            parts.extend(stack_counts(state, frame));
            state.line_counts(buffers)
        });
        parts.extend(hunk);
        return DiffMetrics {
            lead: parts.join(" \u{b7} "),
            added,
            removed,
            ..DiffMetrics::default()
        };
    }

    // The base text's lines and the buffer's, which a new file staged over a
    // base with text counts. `str::lines` counts as `fold_lines` does, so no
    // rope is built from the base on each frame.
    let (stats, lines) = buffer_id
        .and_then(|id| buffers.get(id))
        .and_then(|buffer| {
            let buffer = buffer.read().ok()?;
            let map = buffer.diff_map.as_ref()?;
            let base_lines = map.base_text().map(|text| text.lines().count() as u32);
            let buffer_lines = crate::diff_changes::rope_lines(buffer.rope());
            Some((
                ExcerptStats::of(map),
                base_lines.map(|base| (base, buffer_lines)),
            ))
        })
        .unwrap_or_default();
    let kind = path
        .zip(frame.repo_root)
        .and_then(|(path, root)| path.strip_prefix(root).ok())
        .and_then(|rel_path| frame.tallies?.change(rel_path))
        .map(|change| &change.kind);

    let (lead, tag, tail) = match kind {
        Some(kind) if *kind != FileKind::Modified => {
            let (lead, tail) = kind_metrics(kind, base, lines, hunk);
            let tag = (
                diff_changes::kind_tag_text(kind),
                diff_changes::kind_tag_style(frame.theme, kind),
            );
            (lead, Some(tag), tail)
        },
        _ => {
            let mut parts = vec![format!("vs {base}")];
            parts.extend(
                screen_segment(Some("diff"), frame.theme).map(|(label, _)| label.to_owned()),
            );
            parts.extend(hunk);
            (parts.join(" \u{b7} "), None, String::new())
        },
    };
    DiffMetrics {
        lead,
        tag,
        tail,
        added: stats.added,
        removed: stats.removed,
    }
}

/// The parts of a changes view's metrics that count the files its stack
/// lists, each for a count above zero.
///
/// The parts count every file, then the moved files, the new files, and the
/// deleted files, then during a merge the unmerged files and their conflict
/// blocks, then the files folded to their headers. An untracked file counts as
/// new. The kinds read the frame's tally, so a file listed for an unsaved edit
/// alone counts as a file and under no kind.
///
/// During a merge a file reads as [`diff_changes::merge_note`] classifies it
/// for its File Header. A new file that the merge brought counts under no
/// kind, since the merge's own files are not the reader's changes.
fn stack_counts(state: &DiffChangesState, frame: FrameCtx<'_>) -> Vec<String> {
    let (mut moved, mut new, mut deleted) = (0, 0, 0);
    let (mut conflicted, mut conflicts) = (0, 0);
    for excerpt in &state.excerpts {
        let note = diff_changes::merge_note(excerpt, frame);
        if let Some(MergeNote::Conflict { count, .. }) = note {
            conflicted += 1;
            conflicts += count as usize;
        }
        let from_merge = matches!(note, Some(MergeNote::FromBranch(_)));
        match excerpt.file_kind(frame.tallies) {
            Some(FileKind::Moved { .. }) => moved += 1,
            Some(FileKind::New) if from_merge => {},
            Some(FileKind::Untracked | FileKind::New) => new += 1,
            Some(FileKind::Deleted) => deleted += 1,
            Some(FileKind::Modified) | None => {},
        }
    }
    let collapsed = state
        .excerpts
        .iter()
        .filter(|excerpt| excerpt.collapsed)
        .count();

    let files = state.excerpts.len();
    let noun = if files == 1 { "file" } else { "files" };
    let conflict_noun = if conflicts == 1 {
        "conflict"
    } else {
        "conflicts"
    };
    [
        (files, noun),
        (moved, "moved"),
        (new, "new"),
        (deleted, "deleted"),
        (conflicted, "conflicted"),
        (conflicts, conflict_noun),
        (collapsed, "collapsed"),
    ]
    .into_iter()
    .filter(|&(count, _)| count > 0)
    .map(|(count, word)| format!("{count} {word}"))
    .collect()
}

/// The lead and the tail that a per-file diff's metrics read around the File
/// Kind Tag of a file of `kind`, against the base named `base`.
///
/// `lines` holds the lines of the diff's base text and of its buffer, `None`
/// while the diff has no base text. A new file over a base that holds text
/// counts them as staged, and a new file over an empty base reads `no base`.
/// An untracked file names no base.
fn kind_metrics(
    kind: &FileKind,
    base: &str,
    lines: Option<(u32, u32)>,
    hunk: Option<String>,
) -> (String, String) {
    let lead = format!("vs {base} \u{b7}");
    let tail = |first: Option<String>| -> String {
        first
            .into_iter()
            .chain(hunk.clone())
            .collect::<Vec<_>>()
            .join(" \u{b7} ")
    };
    match kind {
        FileKind::Moved { from, .. } => (lead, tail(Some(format!("from {}", from.display())))),
        FileKind::Untracked => ("no base \u{b7}".to_string(), "not in the index".to_string()),
        FileKind::New => match lines {
            Some((0, _)) => (lead, "no base".to_string()),
            Some((base_lines, buffer_lines)) => (
                lead,
                tail(Some(format!("staged {base_lines} / {buffer_lines}"))),
            ),
            None => (lead, String::new()),
        },
        FileKind::Deleted | FileKind::Modified => (lead, tail(None)),
    }
}

/// The segments of a diff view's metrics over `style`, as the text's segments
/// and then the counts' segments.
///
/// The text reads the muted lead, the File Kind Tag in its own style, and the
/// muted tail. The tag and the tail take no segment where the metrics hold
/// none. The counts read the lines the diff adds and removes in their status
/// colors, and a count of zero takes no segment. The last count ends in a pad
/// cell, as the text does, so the counts stand clear of whatever follows
/// them.
fn metric_segments(
    metrics: &DiffMetrics,
    style: Style,
    theme: &crate::theme::Theme,
) -> (Vec<StatusSeg>, Vec<StatusSeg>) {
    use crate::theme::scope;
    let muted = muted_tint(style, theme);
    let mut text = vec![(format!(" {} ", metrics.lead), muted)];
    if let Some((tag, tag_style)) = &metrics.tag {
        text.push((format!(" {tag} "), style.patch(*tag_style)));
    }
    if !metrics.tail.is_empty() {
        text.push((format!(" {} ", metrics.tail), muted));
    }

    let mut counts: Vec<StatusSeg> = [
        ('+', metrics.added, scope::DIFF_ADDED),
        ('\u{2212}', metrics.removed, scope::DIFF_DELETED),
    ]
    .into_iter()
    .filter(|&(_, count, _)| count > 0)
    .map(|(mark, count, scope)| (format!(" {mark}{count}"), tinted(style, theme, scope)))
    .collect();
    if let Some((last, _)) = counts.last_mut() {
        last.push(' ');
    }
    (text, counts)
}

/// The box of the File Kind Tag in `metrics`, whose segment stands at `at` in
/// its run's left segments, or none for metrics with no tag.
///
/// A tag style with no background takes a box with no panel, which keeps the
/// tag on whole cells.
fn tag_box(metrics: &DiffMetrics, at: usize) -> Option<RunBox> {
    let (_, style) = metrics.tag.as_ref()?;
    Some(RunBox {
        segments: at..at + 1,
        shape: style.bg.is_some().then_some(BoxShape::Tag),
    })
}

/// The base half's segments of a split diff row, whose segment rect is
/// `segments`: the metrics text from the half's left edge, and the counts
/// packed against the divide in screen order.
///
/// The counts read as a pair, so the first count that does not clear the text
/// ends the packing.
fn metrics_half(
    metrics: &DiffMetrics,
    segments: Rect,
    style: Style,
    theme: &crate::theme::Theme,
) -> (Vec<StatusSeg>, Vec<StatusSeg>) {
    let (text, counts) = metric_segments(metrics, style, theme);
    let mut left = Vec::new();
    let mut cursor = segments.x;
    for (text, text_style) in text {
        push_left(&mut left, &mut cursor, segments.right(), text, text_style);
    }

    let mut right = Vec::new();
    let mut anchor = segments.right();
    for (text, style) in counts.into_iter().rev() {
        let start = anchor.saturating_sub(text.chars().count() as u16);
        if start < cursor {
            break;
        }
        right.push((text, style));
        anchor = start;
    }
    (left, right)
}

/// The ours third's segments of a conflict row, whose segment rect is
/// `segments`.
///
/// The merge's facts read from the third's left edge, and the count of the
/// session's open chunks packs against the divide. The count goes in only
/// where it clears the facts, as a split diff row's counts clear its metrics,
/// and only while a chunk stays open.
///
/// The thirds come only with the MergeGrid, which only a live scene paints.
/// So the facts and the count measure at the Ui scale that the scene draws
/// the row at. A measure in whole cells drops the count from a column with
/// room for both at that scale.
fn merge_facts(
    position: &ConflictPosition,
    labels: &ConflictLabels,
    segments: Rect,
    style: Style,
    theme: &crate::theme::Theme,
) -> (Vec<StatusSeg>, Vec<StatusSeg>) {
    let facts = merge_lead(position, labels, true);
    let scale = theme.type_scale(TypeRole::Ui);
    let advance = |text: &str| u32::from(text_run::advance_sixteenths(text.chars().count(), scale));
    let room = u32::from(segments.width) * 16;
    let open = (position.chunks_open > 0)
        .then(|| format!(" {} left ", position.chunks_open))
        .filter(|open| advance(&facts) + advance(open) <= room)
        .map(|open| {
            let tone = tinted(style, theme, crate::theme::scope::VCS_CONFLICT_HEADER);
            (open, tone)
        });
    (
        vec![(facts, muted_tint(style, theme))],
        open.into_iter().collect(),
    )
}

/// The merge's facts as a conflict row reads them, what merges into what and
/// the chunk at the cursor.
///
/// With `place`, the facts also give the shown file's place in the session.
fn merge_lead(position: &ConflictPosition, labels: &ConflictLabels, place: bool) -> String {
    let at = position
        .chunk_at
        .map_or_else(|| "-".to_string(), |at| at.to_string());
    let mut parts = vec![format!(
        "{} \u{2192} {}",
        labels.theirs_short(),
        labels.ours_short()
    )];
    if place {
        parts.push(format!(
            "file {} / {}",
            position.file_at, position.file_total
        ));
    }
    parts.push(format!("chunk {at} / {}", position.chunk_total));
    format!(" {} ", parts.join(" \u{b7} "))
}

/// The runs that name `scope` in a changes view's status row, fitted to
/// `room` cells with a pad cell at each side.
///
/// The parts are the ones a fold row's Scope shows, each in `style` with its
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
/// The base half's whole rect takes the divider's own cell, so the row's
/// hairline runs unbroken across it, and its segments stop before that cell.
/// A divider outside the row leaves one half empty.
fn split_halves(row: Rect, divider: u16, cover: Option<Rect>) -> [(Rect, Rect); 2] {
    let divider = divider.clamp(row.left(), row.right());
    let target_x = divider.saturating_add(1).min(row.right());
    let base = Rect {
        width: target_x - row.x,
        ..row
    };
    let base_segments = Rect {
        width: divider - row.x,
        ..row
    };
    let target = Rect {
        x: target_x,
        width: row.right() - target_x,
        ..row
    };
    [
        (base, status_segments_area(base_segments, cover)),
        (target, status_segments_area(target, cover)),
    ]
}

/// The ours, merged, and theirs thirds of status row `row` split at the edges
/// of the MergeGrid's `merged` column, each as its whole rect and the part of
/// it that holds segments.
///
/// The grid draws the rules between its columns as hairlines on the merged
/// column's edges, with no cell of their own, so the thirds meet with no cell
/// between them. An edge outside the row leaves a third empty.
fn split_thirds(row: Rect, merged: Range<u16>, cover: Option<Rect>) -> [(Rect, Rect); 3] {
    let start = merged.start.clamp(row.left(), row.right());
    let end = merged.end.clamp(start, row.right());
    let third = |x: u16, end: u16| {
        let rect = Rect {
            x,
            width: end - x,
            ..row
        };
        (rect, status_segments_area(rect, cover))
    };
    [
        third(row.x, start),
        third(start, end),
        third(end, row.right()),
    ]
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
/// access to reads `read only`. A revision buffer reads `at` and then
/// `revision`, the short sha of its commit.
fn read_only_note(
    kind: Option<ReadOnly>,
    disk_len: Option<u64>,
    text_len: usize,
    revision: Option<&str>,
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
        ReadOnly::Revision => format!(" at {} ", revision?),
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
pub fn focused_staged_label(
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
pub fn repo_segments(
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
        segments.push((divergence, muted_tint(base_style, theme)));
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
        (
            repo.staged,
            '\u{271a}',
            "staged",
            tinted(base_style, theme, s::DIFF_STAGED),
        ),
        (
            repo.unstaged,
            '\u{270e}',
            "unstaged",
            muted_tint(base_style, theme),
        ),
        (
            repo.untracked,
            '?',
            "untracked",
            tinted(base_style, theme, s::UI_TEXT_DIM),
        ),
    ];
    let held: Vec<StatusSeg> = counts
        .into_iter()
        .filter(|&(count, ..)| count > 0)
        .map(|(count, glyph, word, style)| (format!(" {glyph} {count} {word}"), style))
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
                .and_then(TermSession::display_title)
                .unwrap_or("[agent]"),
        ),
        View::Terminal(id) => owned(
            terms
                .get(*id)
                .and_then(TermSession::display_title)
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
    let revision = buffers.revision(buffer_id);
    let path = buffers
        .path_for(buffer_id)
        .or(revision.map(|source| source.path.as_path()));
    let filename = status_filename(editor, path, workspace_root, home).to_string();
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
        revision: revision.map(|source| diff::short_sha(&source.sha)),
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
    /// The short sha of the commit a revision buffer holds, which its segment
    /// names.
    revision: Option<String>,
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
        byte_size, conflict_chip_text, diff_base_lead, focused_staged_label, merge_facts,
        metrics_half, muted_tint, paint_status_fallback, read_only_note, repo_segments,
        resolve_rich_segments, split_halves, split_thirds, status_filename, status_message_style,
        tab_mark, DiffMetrics,
    };
    use crate::{
        app::MessageKind,
        buffer::{BufferId, TextBuffer},
        buffer_registry::ReadOnly,
        conflict_session::{ConflictLabels, ConflictPosition},
        diff_map::MarkKind,
        editor_state::EditorState,
        host::git::MergeState,
        workspace::{
            diff::{DiffBase, HunkPosition, RepoStatus},
            TabConflictMark,
        },
    };
    use ratatui::{buffer::Buffer, layout::Rect};
    use std::{
        collections::HashMap,
        path::{Path, PathBuf},
        sync::{Arc, RwLock},
    };

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

    /// A muted segment lifts off a status row filled with the muted color, as
    /// default_dark's focused row is, and stays dimmer than the row's text.
    /// default_dark's faint unfocused row and both Ermine rows keep the theme's
    /// muted color.
    #[test]
    fn a_muted_segment_lifts_off_a_row_filled_with_the_muted_color() {
        use ratatui::style::Color;

        let (config, _) = stoat_config::parse(crate::app::DEFAULT_KEYMAP);
        let config = config.expect("the shipped config parses");
        let inks = |name: &str| {
            let theme = crate::theme::Theme::from_config(&config, name).expect("the theme loads");
            [
                crate::theme::scope::UI_STATUSBAR_FOCUSED,
                crate::theme::scope::UI_STATUSBAR_UNFOCUSED,
            ]
            .map(|row| muted_tint(theme.get(row), &theme).fg)
        };
        let rgb = |[r, g, b]: [u8; 3]| Some(Color::Rgb(r, g, b));

        assert_eq!(
            [inks("default_dark"), inks("ermine_night")],
            [
                [rgb([0xb4, 0xb9, 0xc4]), rgb([0x63, 0x6d, 0x83])],
                [rgb([0x83, 0x8a, 0x98]), rgb([0x83, 0x8a, 0x98])],
            ],
        );
    }

    /// A status message takes the scope of its kind. A theme that leaves
    /// `ui.message.info` unset draws an info message as an alert.
    #[test]
    fn a_status_message_takes_the_color_of_its_kind() {
        use ratatui::style::{Color, Style};

        let foregrounds = |source: &str| {
            let (config, _) = stoat_config::parse(source);
            let config = config.expect("the theme parses");
            let theme = crate::theme::Theme::from_config(&config, "t").expect("the theme loads");
            [MessageKind::Alert, MessageKind::Info, MessageKind::Success]
                .map(|kind| status_message_style(Style::default(), kind, &theme).fg)
        };
        let red = Some(Color::Rgb(0xff, 0, 0));

        assert_eq!(
            [
                foregrounds(concat!(
                    r##"theme t { ui.message.error.fg = "#ff0000"; ui.message.info.fg = "#00ff00"; "##,
                    r##"ui.message.success.fg = "#0000ff"; }"##,
                )),
                foregrounds(r##"theme t { ui.message.error.fg = "#ff0000"; }"##),
            ],
            [
                [
                    red,
                    Some(Color::Rgb(0, 0xff, 0)),
                    Some(Color::Rgb(0, 0, 0xff))
                ],
                [red, red, red],
            ],
        );
    }

    #[test]
    fn lsp_short_name_abbreviates_server_names() {
        use super::lsp_short_name;
        assert_eq!(lsp_short_name("rust-analyzer"), "RA");
        assert_eq!(lsp_short_name("pyright"), "PY");
        assert_eq!(lsp_short_name("typescript-language-server"), "TLS");
        assert_eq!(lsp_short_name("gopls"), "GO");
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

    /// Each kind of base a diff takes reads from the base half's left edge as
    /// `vs` and the base's name, and the counts pack against the divider.
    #[test]
    fn the_base_half_names_each_kind_of_base() {
        use ratatui::style::Style;

        let theme = crate::theme::Theme::empty();
        let row = Rect::new(0, 0, 120, 1);
        let painted = |base: &str| {
            let [(_, segments), _] = split_halves(row, 59, None);
            let metrics = DiffMetrics {
                lead: format!("vs {base}"),
                added: 6,
                removed: 4,
                ..DiffMetrics::default()
            };
            let (left, right) = metrics_half(&metrics, segments, Style::default(), &theme);
            let mut buf = Buffer::empty(row);
            paint_status_fallback(&mut buf, segments, &left, &right);
            (0..row.width)
                .map(|x| buf[(x, 0)].symbol())
                .collect::<String>()
        };

        let bases = [
            "index",
            "HEAD",
            "main abc1234",
            "abc1234",
            "parent abc1234",
            "empty",
            "original",
            "a.txt",
            "x/conf.toml",
        ];
        assert_eq!(
            bases.map(painted),
            bases.map(|base| format!("{:<52} +6 \u{2212}4 {:61}", format!(" vs {base} "), "")),
        );
    }

    /// A count of zero takes no segment. The counts pack from the divider and
    /// show only clear of the text, so the first count that does not fit ends
    /// the pair.
    #[test]
    fn the_counts_show_only_above_zero_and_clear_of_the_text() {
        use ratatui::style::Style;

        let theme = crate::theme::Theme::empty();
        let painted = |width: u16, (added, removed): (u32, u32)| {
            let segments = Rect::new(0, 0, width, 1);
            let metrics = DiffMetrics {
                lead: "vs HEAD".to_string(),
                added,
                removed,
                ..DiffMetrics::default()
            };
            let (left, right) = metrics_half(&metrics, segments, Style::default(), &theme);
            let mut buf = Buffer::empty(segments);
            paint_status_fallback(&mut buf, segments, &left, &right);
            (0..width).map(|x| buf[(x, 0)].symbol()).collect::<String>()
        };

        assert_eq!(
            [
                painted(20, (6, 0)),
                painted(20, (0, 4)),
                painted(14, (6, 4)),
                painted(11, (6, 4)),
            ],
            [
                format!("{:<16} +6 ", " vs HEAD "),
                format!("{:<16} \u{2212}4 ", " vs HEAD "),
                format!("{:<10} \u{2212}4 ", " vs HEAD "),
                format!("{:<11}", " vs HEAD "),
            ],
        );
    }

    /// The base half takes the divider's cell, so the row's hairline runs
    /// unbroken, and its segments stop before that cell. A badge box over the
    /// row's end takes the target half's segment room.
    #[test]
    fn the_base_half_takes_the_divider_cell_and_its_segments_stop_before_it() {
        let row = Rect::new(2, 9, 120, 1);
        let badge = Rect::new(100, 9, 22, 1);
        assert_eq!(
            [
                split_halves(row, 61, None),
                split_halves(row, 61, Some(badge))
            ],
            [
                [
                    (Rect::new(2, 9, 60, 1), Rect::new(2, 9, 59, 1)),
                    (Rect::new(62, 9, 60, 1), Rect::new(62, 9, 60, 1)),
                ],
                [
                    (Rect::new(2, 9, 60, 1), Rect::new(2, 9, 59, 1)),
                    (Rect::new(62, 9, 60, 1), Rect::new(62, 9, 38, 1)),
                ],
            ],
        );
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
    /// A revision buffer names its commit. A buffer that writes back has no
    /// segment.
    #[test]
    fn the_read_only_note_names_the_kind_and_the_sizes() {
        assert_eq!(
            [
                read_only_note(None, Some(41_600), 190_464, None),
                read_only_note(Some(ReadOnly::Compressed), Some(41_600), 190_464, None),
                read_only_note(Some(ReadOnly::Compressed), None, 190_464, None),
                read_only_note(Some(ReadOnly::Permission), None, 5, None),
                read_only_note(Some(ReadOnly::Revision), None, 5, Some("a1b2c3d")),
            ],
            [
                None,
                Some(" gzip \u{b7} 41 KiB \u{2192} 186 KiB ".to_owned()),
                Some(" gzip ".to_owned()),
                Some(" read only ".to_owned()),
                Some(" at a1b2c3d ".to_owned()),
            ],
        );
    }

    /// The thirds meet at the merged column's edges with no cell between them.
    /// An edge past the row leaves the third beyond it empty, and a badge box
    /// over the row ends the segments of the third it covers.
    #[test]
    fn the_thirds_split_at_the_merged_column_s_edges() {
        let row = Rect::new(10, 5, 100, 1);
        let thirds = |start: u16, end: u16, cover: Option<Rect>| {
            split_thirds(row, start..end, cover)
                .map(|(rect, segments)| (rect.x, rect.width, segments.width))
        };
        let badge = Some(Rect::new(100, 5, 10, 1));

        assert_eq!(
            [
                thirds(40, 70, None),
                thirds(40, 130, None),
                thirds(0, 30, None),
                thirds(40, 70, badge),
            ],
            [
                [(10, 30, 30), (40, 30, 30), (70, 40, 40)],
                [(10, 30, 30), (40, 70, 70), (110, 0, 0)],
                [(10, 0, 0), (10, 20, 20), (30, 80, 80)],
                [(10, 30, 30), (40, 30, 30), (70, 40, 30)],
            ],
        );
    }

    /// The ours third counts the session's open chunks only while one stays
    /// open, and only where the count clears the facts.
    #[test]
    fn the_merge_facts_count_open_chunks_that_clear_the_facts() {
        use ratatui::style::Style;

        let theme = crate::theme::Theme::empty();
        let labels = ConflictLabels::new(Some("main".to_string()), None);
        let texts = |segments: Vec<(String, Style)>| {
            segments
                .into_iter()
                .map(|(text, _)| text)
                .collect::<Vec<_>>()
        };
        let packed = |chunks_open: usize, width: u16| {
            let position = ConflictPosition {
                operation: "conflict",
                chunk_at: Some(1),
                chunk_total: 2,
                file_at: 1,
                file_total: 1,
                file_open: chunks_open,
                chunks_open,
                files_open: usize::from(chunks_open > 0),
            };
            let segments = Rect::new(0, 0, width, 1);
            let (facts, open) = merge_facts(&position, &labels, segments, Style::default(), &theme);
            (texts(facts), texts(open))
        };

        let facts = vec![" theirs \u{2192} main \u{b7} file 1 / 1 \u{b7} chunk 1 / 2 ".to_string()];
        assert_eq!(
            [packed(2, 50), packed(2, 49), packed(0, 50)],
            [
                (facts.clone(), vec![" 2 left ".to_string()]),
                (facts.clone(), Vec::new()),
                (facts, Vec::new()),
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

    /// The conflict chip names the operation, with theirs after a merge. It
    /// counts the open chunks of every file, in the singular for one, and the
    /// files that still hold an open chunk.
    #[test]
    fn the_conflict_chip_names_the_operation_and_what_is_left() {
        let merge = MergeState {
            head_sha: "a1c4e9f00".to_string(),
            head_name: Some("main".to_string()),
            message: "Merge branch 'main'".to_string(),
            conflicted: 3,
        };
        let labels = ConflictLabels::new(Some("ermine-tokens".to_string()), Some(&merge));
        let chip = |operation: &'static str, chunks_open: usize, files_open: usize| {
            let position = ConflictPosition {
                operation,
                chunk_at: Some(1),
                chunk_total: 2,
                file_at: 1,
                file_total: 3,
                file_open: chunks_open.min(1),
                chunks_open,
                files_open,
            };
            conflict_chip_text(&position, &labels)
        };

        assert_eq!(
            [
                chip("merge", 2, 2),
                chip("rebase", 1, 1),
                chip("conflict", 0, 0)
            ],
            [
                "merge main \u{b7} 2 conflicts left \u{b7} 2 of 3 files",
                "rebase \u{b7} 1 conflict left \u{b7} 1 of 3 files",
                "conflict \u{b7} 0 conflicts left \u{b7} 0 of 3 files",
            ],
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
}
