use crate::{
    app::SPINNER_FRAMES,
    buffer_registry::BufferRegistry,
    editor_state::{EditorId, EditorState},
    pane::{Divider, DividerOrientation, Pane, View},
    render::{
        chrome,
        editor::{editor_cursor_position, render_editor_with_overlay},
        layout::split_pane_status,
        popout::{
            paint_popout_card, popout_area, popout_card_bg, popout_inset, scaled_char_capacity,
            wrap_popout_lines,
        },
        review::{dim_rgb, style_rgb},
        run_pane::render_run_pane,
        term_pane::render_term_pane,
        undercurl::UndercurlBatch,
        FrameCtx, PaneCtx, TEXT_SCALE_COMPACT, TEXT_SCALE_FULL,
    },
    workspace::Workspace,
};
use lsp_types::DiagnosticSeverity;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Text,
    widgets::{Paragraph, StatefulWidget, Widget},
};
use slotmap::SlotMap;
use std::{
    hash::{DefaultHasher, Hash, Hasher},
    path::Path,
};
use stoatty_widgets::{
    minimap::Minimap,
    status_bar::{StatusBar, StatusSegment},
    ApcScene,
};

/// Buffer lines the minimap strip draws per vertical cell.
pub(super) const MINIMAP_LINES_PER_CELL: u8 = 8;

/// Widest line, in minimap columns, a strip renders before clipping.
pub(super) const MINIMAP_MAX_COLUMNS: u8 = 120;

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_pane(
    pane: &Pane,
    is_focused: bool,
    ctx: PaneCtx<'_>,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    undercurls: &mut UndercurlBatch,
    badge_rect: &mut Option<Rect>,
) {
    let theme = frame.theme;
    let text_style = theme.get(crate::theme::scope::UI_TEXT);
    let (content_area, status_area) = pane_areas(pane.area, frame.minimap_band);

    let PaneCtx {
        editors,
        buffers,
        runs,
        terms,
    } = ctx;

    match &pane.view {
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
                    frame.search_query,
                    diagnostic_info,
                    // The editor's rich surfaces -- gutter, minimap strip, review
                    // decorations -- fork on having a scene at all, so a dead one
                    // is withheld and each takes the cell arm it already carries.
                    match scene.live() {
                        true => Some(&mut *scene),
                        false => None,
                    },
                    Some(undercurls),
                    if is_focused { 0.0 } else { frame.inactive_dim },
                    frame.wrap_mode,
                    frame.wrap_column,
                );

                if let (Some(strip), Some(chrome)) = (editor.minimap_rect, frame.minimap_chrome)
                    && let Some(content) = chrome.content.get(&(chrome.workspace, editor.buffer_id))
                {
                    let dim = if is_focused { 0.0 } else { frame.inactive_dim };
                    let dim_bg = (dim > 0.0)
                        .then(|| {
                            style_rgb(
                                theme
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
                        content_id: content.content_id(),
                        lines_per_cell: MINIMAP_LINES_PER_CELL,
                        max_columns: MINIMAP_MAX_COLUMNS,
                        bg: [0, 0, 0, 0],
                        thumb: [tr, tg, tb, ta],
                        thumb_border: [tr, tg, tb],
                        palette: match is_focused {
                            true => chrome.palette,
                            false => chrome.dimmed_palette,
                        },
                    }
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
            if let Some(term) = terms.get(*term_id) {
                render_term_pane(term, theme, content_area, is_focused, buf);
            }
        },
    }

    if !is_focused
        && frame.inactive_dim > 0.0
        && let Some(bg) = style_rgb(
            theme
                .try_get(crate::theme::scope::UI_BACKGROUND)
                .and_then(|s| s.bg),
        )
    {
        dim_pane_content(buf, content_area, bg, frame.inactive_dim);
    }

    render_pane_status(
        &pane.view,
        is_focused,
        status_area,
        frame,
        editors,
        buffers,
        badge_rect,
        buf,
        scene,
    );

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
                .unwrap_or(Color::Reset);
            let content = paint_popout_card(buf, area, bg, border, theme, &mut *scene);

            let cap = scaled_char_capacity(content.width as usize, TEXT_SCALE_COMPACT);
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
                    TEXT_SCALE_COMPACT,
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
        let width = scaled_char_capacity(cell_width, TEXT_SCALE_COMPACT);
        let lines = wrap_popout_lines(msg, width, 4);
        if !lines.is_empty()
            && let Some(area) =
                popout_area(status_area, content_area, lines.len() as u16, status_rows)
        {
            let bg = popout_card_bg(theme);
            let border = theme
                .get(crate::theme::scope::UI_BORDER_INACTIVE)
                .fg
                .unwrap_or(Color::Reset);
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
                    TEXT_SCALE_COMPACT,
                    &mut *scene,
                );
            }
        }
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
/// Shared with the replay cache, which has to know both rectangles to record
/// what a pane painted, and whose key would drift from the paint if it worked
/// the widening out for itself.
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
/// Does not know about editors or buffers; shows only mode + workspace +
/// a short label identifying the overlay. Matches the visual style of
/// [`render_pane_status`] for a focused pane.
pub(crate) fn render_overlay_status(
    area: Rect,
    is_focused: bool,
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

    let left = overlay_status_segments(is_focused, area, frame);
    let mut right: Vec<StatusSeg> = Vec::new();
    if let Some(message) = frame.status_message {
        right.push((
            message.to_string(),
            status_message_style(base_style, frame.theme),
        ));
    }
    render_status_segments(area, base_style, frame, &left, &right, buf, scene, None);
}

/// Build the overlay status bar's left segments in paint order.
///
/// Mode and workspace show only when focused, then a screen label that shows
/// unconditionally, left-padded when it leads. The screen segment differs from
/// the pane status's focus-gated one.
fn overlay_status_segments(is_focused: bool, area: Rect, frame: FrameCtx<'_>) -> Vec<StatusSeg> {
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
    if let Some((screen_label, screen_color)) = screen_segment(frame.screen, theme) {
        let left_pad = if cursor == area.x { " " } else { "" };
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!("{left_pad}{screen_label} "),
            base_style.fg(screen_color),
        );
    }
    let _ = cursor;
    left
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

#[allow(clippy::too_many_arguments)]
fn render_pane_status(
    view: &View,
    is_focused: bool,
    area: Rect,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    badge_rect: &mut Option<Rect>,
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

    let (left, right) =
        status_segments(view, is_focused, area, frame, editors, buffers, badge_rect);

    let cache = match view {
        View::Editor(id) => editors.get_mut(*id).map(|e| &mut e.status_scene_cache),
        _ => None,
    };
    render_status_segments(area, base_style, frame, &left, &right, buf, scene, cache);
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
/// Shares [`status_segments`] with [`render_pane_status`], adding the numeric
/// selection badge that only a detached pane needs, since it cannot host the
/// primary scene's digit popover.
#[allow(clippy::too_many_arguments)]
pub(crate) fn pane_status_cells(
    view: &View,
    is_focused: bool,
    area: Rect,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    badge: Option<u32>,
) -> PaneStatusCells {
    let base_style = if is_focused {
        frame.theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        frame.theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };

    let (mut left, right) =
        status_segments(view, is_focused, area, frame, editors, buffers, &mut None);

    if let Some(digit) = badge {
        let badge_style = frame
            .theme
            .get(crate::theme::scope::UI_SELECTION_EDITOR)
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

/// Render the built status segments as rich APC components inside stoatty, or
/// into cells otherwise.
///
/// Rich mode needs a live scene and every segment color as RGB. A dead scene or
/// any color outside RGB drops the whole bar to the cell fallback, so a foreign
/// terminal and a theme without RGB status colors both keep the cell rendering.
///
/// With a `cache`, a repaint whose segments, rect, and colors all hold splices
/// the recorded frame. Splicing is only sound because
/// [`StatusBar::draw_components`] reads nothing outside those, and writes
/// nothing but the scene, so a skipped encode leaves no cell unpainted.
#[allow(clippy::too_many_arguments)]
fn render_status_segments(
    area: Rect,
    base_style: Style,
    frame: FrameCtx<'_>,
    left: &[StatusSeg],
    right: &[StatusSeg],
    buf: &mut Buffer,
    scene: &mut ApcScene,
    cache: Option<&mut StatusSceneCache>,
) {
    let colors = (|| {
        scene.live().then_some(())?;
        let separator = style_rgb(frame.theme.get(crate::theme::scope::UI_BORDER_INACTIVE).fg)?;
        let base_bg = style_rgb(base_style.bg)?;
        Some((separator, base_bg))
    })();

    let Some((separator, base_bg)) = colors else {
        paint_status_fallback(buf, area, left, right);
        return;
    };

    let key = status_scene_key(area, base_style, left, right, (separator, base_bg));

    if let Some(cache) = &cache
        && cache.key == Some(key)
    {
        scene.buffer().extend_from_slice(&cache.bytes);
        return;
    }

    let rich =
        resolve_rich_segments(left, base_style).zip(resolve_rich_segments(right, base_style));

    let Some((left_rich, right_rich)) = rich else {
        paint_status_fallback(buf, area, left, right);
        return;
    };

    let start = scene.bytes().len();
    StatusBar {
        left: &left_rich,
        right: &right_rich,
        scale: TEXT_SCALE_COMPACT,
        separator,
        bg: base_bg,
    }
    .draw_components(area, buf, scene);

    if let Some(cache) = cache {
        cache.bytes.clear();
        cache.bytes.extend_from_slice(&scene.bytes()[start..]);
        cache.key = Some(key);
    }
}

/// Hash everything the status bar's APC frame is encoded from into a cache key.
///
/// The segments go in as their pre-resolution text and style, which is what
/// they are compared as on the windowed path too. Resolving them to RGB is
/// itself part of the work the cache exists to skip, and the resolution is a
/// function of the pair hashed here.
fn status_scene_key(
    area: Rect,
    base_style: Style,
    left: &[StatusSeg],
    right: &[StatusSeg],
    colors: ([u8; 3], [u8; 3]),
) -> u64 {
    let mut hasher = DefaultHasher::new();
    (area.x, area.y, area.width, area.height).hash(&mut hasher);
    base_style.hash(&mut hasher);
    left.hash(&mut hasher);
    right.hash(&mut hasher);
    colors.hash(&mut hasher);
    hasher.finish()
}

/// Paint the tab bar across `area`, one segment per tab.
///
/// Each segment reads ` <n>:<title> ` at its 1-based number, so a tab is
/// addressable by what the bar shows. Painting through the status-bar segment
/// path means the bar picks up the same scaled-run rendering under stoatty and
/// the same cell fallback elsewhere.
pub(crate) fn render_tab_bar(
    ws: &Workspace,
    area: Rect,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    if area.width == 0 || area.height == 0 {
        return;
    }

    let active = frame.theme.get(crate::theme::scope::UI_TABBAR_ACTIVE);
    let inactive = frame.theme.get(crate::theme::scope::UI_TABBAR_INACTIVE);

    for x in area.x..area.x + area.width {
        buf[(x, area.y)].set_char(' ').set_style(inactive);
    }

    let left: Vec<StatusSeg> = (0..ws.tabs.len())
        .map(|idx| {
            let style = if idx == ws.active_tab {
                active
            } else {
                inactive
            };
            (format!(" {}:{} ", idx + 1, ws.tab_title(idx)), style)
        })
        .collect();

    render_status_segments(area, inactive, frame, &left, &[], buf, scene, None);
}

/// One built status-bar segment pairing painted text with its cell style.
type StatusSeg = (String, Style);

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

/// Build the left- and right-anchored status segments as `(text, style)` pairs
/// in paint order.
///
/// Both the cell fallback and the rich status path consume these, so the two
/// renderings stay in lockstep. The left cursor and right anchor track the same
/// cell arithmetic [`paint_segment`] applies, so a segment enters the list only
/// when it would be painted and the `lsp_message` truncation matches today's.
#[allow(clippy::too_many_arguments)]
fn status_segments(
    view: &View,
    is_focused: bool,
    area: Rect,
    frame: FrameCtx<'_>,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    badge_rect: &mut Option<Rect>,
) -> (Vec<StatusSeg>, Vec<StatusSeg>) {
    let theme = frame.theme;
    let base_style = if is_focused {
        theme.get(crate::theme::scope::UI_STATUSBAR_FOCUSED)
    } else {
        theme.get(crate::theme::scope::UI_STATUSBAR_UNFOCUSED)
    };
    let end_x = area.x + area.width;

    let mut left: Vec<(String, Style)> = Vec::new();
    let mut cursor = area.x;
    if is_focused {
        let (label, mode_bg) = frame.mode_label;
        let mode_style = theme.get(crate::theme::scope::UI_MODE_LABEL).bg(mode_bg);
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!(" {label} "),
            mode_style,
        );
        push_left(
            &mut left,
            &mut cursor,
            end_x,
            format!(" {} ", frame.workspace_name),
            base_style.add_modifier(Modifier::BOLD),
        );
        if let Some((screen_label, screen_color)) = screen_segment(frame.screen, theme) {
            push_left(
                &mut left,
                &mut cursor,
                end_x,
                format!(" {screen_label} "),
                base_style.fg(screen_color),
            );
        }
    }

    let status = pane_status_info(view, frame.workspace_root, frame.home, editors, buffers);
    let cursor_pos = status.cursor_pos;
    if let Some(name) = &status.filename {
        let left_pad = if cursor == area.x { " " } else { "" };
        let text = if status.dirty {
            format!("{left_pad}{name} [+] ")
        } else {
            format!("{left_pad}{name} ")
        };
        push_left(&mut left, &mut cursor, end_x, text, base_style);
    }

    let mut right: Vec<(String, Style)> = Vec::new();
    let mut right_anchor = end_x;
    if let Some((line, col)) = cursor_pos {
        let text = format!(" {line}:{col} ");
        let width = text.chars().count() as u16;
        let start = right_anchor.saturating_sub(width);
        if start >= cursor {
            right.push((text, base_style));
            right_anchor = start;
        }
    }
    if is_focused {
        let badge_right = right_anchor;
        for server in frame.lsp_servers {
            if !server.busy {
                continue;
            }
            let text = format!(
                " {} {} ",
                SPINNER_FRAMES[frame.spinner_phase as usize], server.short
            );
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, base_style));
                right_anchor = start;
            }
        }
        if right_anchor < badge_right {
            // The bar paints the badge glyphs at TEXT_SCALE_COMPACT, right-anchored
            // at badge_right, so they cover fewer cells than their char count.
            // Record the hover rect over that scaled extent (dropping the leftmost
            // pad space, which carries no glyph) so hover matches the drawn glyphs.
            let chars = badge_right - right_anchor;
            let width = ((chars - 1) * TEXT_SCALE_COMPACT).div_ceil(TEXT_SCALE_FULL);
            *badge_rect = Some(Rect::new(badge_right - width, area.y, width, area.height));
        }
        if frame.diff_warm_busy {
            let text = format!(" {} diff ", SPINNER_FRAMES[frame.spinner_phase as usize]);
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, base_style));
                right_anchor = start;
            }
        }
        if let Some(count) = frame.pending_count {
            let text = format!(" {count} ");
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, base_style.add_modifier(Modifier::BOLD)));
                right_anchor = start;
            }
        }
        if let Some((text, worst)) =
            focused_diagnostic_label(view, editors, buffers, frame.diagnostics)
        {
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                let badge_style = base_style
                    .add_modifier(Modifier::BOLD)
                    .patch(theme.get(diagnostic_severity_scope(worst)));
                right.push((text, badge_style));
                right_anchor = start;
            }
        }
        if let Some(text) = focused_staged_label(status.staged_counts) {
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, base_style));
                right_anchor = start;
            }
        }
        if let Some(message) = frame.status_message {
            let available = right_anchor.saturating_sub(cursor) as usize;
            if available > 0 {
                let text: String = message.chars().take(available).collect();
                let width = text.chars().count() as u16;
                let start = right_anchor.saturating_sub(width);
                right.push((text, status_message_style(base_style, theme)));
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
                right.push((text, base_style));
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
                right.push((text, base_style.add_modifier(Modifier::ITALIC)));
                right_anchor = start;
            }
        }
        #[cfg(feature = "perf")]
        if let Some(seg) = frame.perf {
            let text = crate::render::perf_label(seg);
            let width = text.chars().count() as u16;
            let start = right_anchor.saturating_sub(width);
            if start >= cursor {
                right.push((text, base_style.add_modifier(Modifier::DIM)));
            }
        }
    }
    let _ = cursor;
    let _ = right_anchor;
    (left, right)
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

/// Builds the status-bar diagnostic label for the focused pane's
/// editor along with its worst severity, or `None` when the pane is
/// not an editor, has no path, or has no diagnostics. Format:
/// ` Ee Ww Ii Hh ` showing each present severity's count; the worst
/// severity drives the badge's foreground color at the call site.
fn focused_diagnostic_label(
    view: &View,
    editors: &SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    diagnostics: &crate::diagnostics::DiagnosticSet,
) -> Option<(String, DiagnosticSeverity)> {
    let View::Editor(editor_id) = view else {
        return None;
    };
    let editor = editors.get(*editor_id)?;
    let path = buffers.path_for(editor.buffer_id)?;
    let summary = diagnostics.summarize(path);
    let worst = summary.worst?;
    let mut parts = Vec::new();
    if summary.error > 0 {
        parts.push(format!("E{}", summary.error));
    }
    if summary.warning > 0 {
        parts.push(format!("W{}", summary.warning));
    }
    if summary.information > 0 {
        parts.push(format!("I{}", summary.information));
    }
    if summary.hint > 0 {
        parts.push(format!("H{}", summary.hint));
    }
    Some((format!(" {} ", parts.join(" ")), worst))
}

/// Statusline label counting the focused buffer's staged and unstaged diff
/// hunks, or `None` when the buffer has no diff map or no hunks.
fn focused_staged_label(counts: Option<(usize, usize)>) -> Option<String> {
    let (staged, unstaged) = counts?;
    if staged == 0 && unstaged == 0 {
        return None;
    }
    Some(format!(" {staged} staged / {unstaged} unstaged "))
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
        "diff" => ("diff", Color::Cyan, scope::UI_STATUSLINE_REVIEW),
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
) -> PaneStatusInfo {
    let owned = |name: &str| PaneStatusInfo {
        filename: Some(name.to_string()),
        ..Default::default()
    };
    match view {
        View::Editor(editor_id) => {
            let Some(editor) = editors.get_mut(*editor_id) else {
                return PaneStatusInfo::default();
            };
            let buffer_id = editor.buffer_id;
            let filename =
                status_filename(editor, buffers.path_for(buffer_id), workspace_root, home)
                    .to_string();
            // Both fields come off the same buffer at the same moment, and the
            // bar wants them together, so one acquisition answers for both
            // rather than the staged label taking a lock of its own.
            let shared = buffers.get(buffer_id);
            let (dirty, staged_counts) = shared
                .as_ref()
                .and_then(|b| b.read().ok())
                .map(|g| (g.dirty, g.diff_map.as_ref().map(|dm| dm.staged_counts())))
                .unwrap_or_default();
            PaneStatusInfo {
                filename: Some(filename),
                dirty,
                cursor_pos: editor_cursor_position(editor),
                staged_counts,
            }
        },
        View::Run(_) => owned("[run]"),
        View::Agent(_) => owned("[agent]"),
        View::Terminal(_) => owned("[term]"),
        View::Label(label) => owned(label),
    }
}

/// What one pane's status bar reads off its view.
#[derive(Default)]
struct PaneStatusInfo {
    filename: Option<String>,
    dirty: bool,
    cursor_pos: Option<(u32, u32)>,
    /// `(staged, unstaged)` hunk counts, `None` when the buffer has no diff.
    /// Read alongside `dirty` so the bar takes one lock, not two.
    staged_counts: Option<(usize, usize)>,
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

#[cfg(test)]
mod tests {
    use super::status_filename;
    use crate::{
        action_handlers::dispatch,
        buffer::{BufferId, TextBuffer},
        editor_state::EditorState,
        Stoat,
    };
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    use std::{
        path::{Path, PathBuf},
        sync::{Arc, RwLock},
    };
    use stoat_action::OpenFile;

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

    fn bar_row(buf: &ratatui::buffer::Buffer) -> String {
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

    #[test]
    fn lsp_badge_hidden_when_idle() {
        let mut h = Stoat::test();
        h.install_lsp_server("rust", "rust-analyzer");
        open_rust(&mut h);

        let buf = h.render_composited();
        let bar = bar_row(&buf);
        assert!(
            !bar.contains("RA"),
            "an idle server paints no badge:\n{bar}"
        );
        assert!(h.stoat.lsp_badge_rect.is_none(), "and stamps no hover rect");
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
            bar.contains('⠋'),
            "busy badge shows the spinner glyph:\n{bar}"
        );
        assert!(
            bar.contains("RA"),
            "busy badge keeps the short name:\n{bar}"
        );
    }

    #[test]
    fn diff_warm_shows_spinner_segment_while_busy() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 12);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "b\n")]);
        h.stoat.set_diff_warm_auto(true);
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
        assert!(!bar.contains("RA"), "no server is up yet:\n{bar}");

        let fake = h.install_lsp_server("rust", "rust-analyzer");
        mark_busy(&mut h, &fake);
        let bar = bar_row(&h.render_composited());
        assert!(
            bar.contains("RA"),
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
            !bar.contains("RA"),
            "an idle server paints no badge:\n{bar}"
        );

        mark_busy(&mut h, &fake);
        let bar = bar_row(&h.render_composited());
        assert!(
            bar.contains("RA"),
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
            bar.contains("RA") && bar.contains("SS"),
            "each running server gets its own badge:\n{bar}"
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
            rendered.matches("RA").count(),
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
        // Under cfg(test) TEXT_SCALE_COMPACT is a full cell, so the busy badge
        // (spinner + " RA " = 6 chars) covers (6 - 1) = 5 cells; the leftmost pad
        // space carries no glyph and is dropped.
        assert_eq!(
            rect.width, 5,
            "the rect is the scaled extent of the drawn glyphs, not their full char span"
        );
        assert!(
            span.contains("RA"),
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

    #[test]
    fn statusline_shows_staged_and_unstaged_counts() {
        let mut h = crate::test_harness::TestHarness::with_size(100, 12);
        h.stage_index_scenario(
            "/repo",
            &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
        );
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
            rendered.replace('─', " ").contains("1 staged / 1 unstaged"),
            "statusline reports the hunk counts:\n{rendered}"
        );
    }

    #[test]
    fn dim_pane_content_blends_rgb_and_skips_indexed() {
        use super::dim_pane_content;
        use crate::render::review::dim_rgb;
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
        use crate::render::review::dim_rgb;
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
}
