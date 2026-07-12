use crate::{
    action_handlers::lsp::{HoverPopup, HoverSelection},
    app::Stoat,
    pane::{FocusTarget, View},
    render::layout::split_pane_status,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Clear, StatefulWidget, Widget},
};
use stoatty_widgets::{text_run::TextRun, ApcScene};

/// Hover-body text size under stoatty, in 256ths of a cell (0.85x), matching
/// the hints overlay so popovers and hint rows share one scale.
const HOVER_TEXT_SCALE: u16 = 218;

/// Rows that must remain below the cursor for the popup to open there. With
/// fewer, placement flips above the cursor. Matches Helix's popup bias
/// threshold.
const MIN_HEIGHT: u16 = 6;

/// Absolute popup caps, matching Helix's popup limits, so a large hover never
/// dominates the pane. On a small window the [`hover_popup_layout`] half-pane
/// cap bites first. These bound the popup on a large one.
const MAX_HEIGHT: u16 = 26;
const MAX_WIDTH: u16 = 120;

/// Paint the hover popup, if any, anchored to the focused editor's
/// primary cursor.
///
/// The popup floats above panes, window-bounded rather than pane-bounded, so it
/// can overflow into neighboring panes. Its body stays opaque over them. The
/// landed declaration-order occlusion covers neighbors under stoatty, and the
/// grid path's `Clear` covers plain terminals. Placement is below-biased, and
/// its height shrinks to the chosen side's free space so it never renders past
/// the window. Content that overflows scrolls, and lines wider than the popup
/// interior are truncated.
///
/// No-op when [`Stoat::pending_hover`] is `None`, when the focused
/// pane is not an editor, or when the cursor is off-screen.
pub(crate) fn render_hover(stoat: &mut Stoat, buf: &mut Buffer, mut scene: Option<&mut ApcScene>) {
    let popup = match &stoat.pending_hover {
        Some(p) => p.clone(),
        None => return,
    };

    let Some((popup_area, _)) = hover_popup_layout(stoat) else {
        return;
    };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);

    Clear.render(popup_area, buf);
    let inner = crate::render::chrome::modal_frame(
        buf,
        popup_area,
        None,
        modal_style,
        &stoat.theme,
        scene.as_deref_mut(),
    );

    // Clamp the half-page scroll to the content that overflows the interior, then
    // write the clamped counter back so scrolling up past the bottom takes effect
    // on the first Ctrl-u rather than after replaying the over-scroll.
    let interior = inner.height as usize;
    let half_page = (interior / 2).max(1);
    let scroll = popup
        .lines
        .len()
        .saturating_sub(interior)
        .min(popup.scroll_half_pages * half_page);
    if let Some(open) = stoat.pending_hover.as_mut() {
        open.scroll_half_pages = scroll / half_page;
        open.area = popup_area;
        open.inner = inner;
    }

    let end_x = inner.x + inner.width;
    let body: Vec<Vec<(String, Style)>> = popup
        .lines
        .iter()
        .map(|line| truncate_line(line, inner.width as usize))
        .collect();

    // A span's style is a delta over the modal base, so a plain span keeps the
    // modal look. The rich arm needs an RGB modal fg and background to compose
    // one TextRun per span at the popover text scale. Without them it paints
    // cells.
    let modal_fg = crate::render::review::style_rgb(modal_style.fg);
    let run_bg = crate::render::review::style_rgb(
        stoat
            .theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg),
    );
    match (scene, modal_fg, run_bg) {
        (Some(scene), Some(modal_fg), Some(run_bg)) => {
            let sel_rgb = crate::render::review::style_rgb(
                stoat.theme.get(crate::theme::scope::UI_SELECTION).bg,
            );
            for (row_idx, line) in body.iter().skip(scroll).enumerate() {
                let row = inner.y + row_idx as u16;
                if row >= inner.y + inner.height {
                    break;
                }
                let selection = sel_rgb.and_then(|rgb| {
                    hover_line_selection(&popup, scroll + row_idx).map(|(c0, c1)| (c0, c1, rgb))
                });

                // Patch the cell bg behind the 0.85x glyph boxes so the selection
                // band spans full cell height. Chars map to cells through the
                // scale, floored at the start and ceiled at the end.
                if let Some((c0, c1, rgb)) = selection {
                    let cell0 = c0 * HOVER_TEXT_SCALE as usize / 256;
                    let cell1 = (c1 * HOVER_TEXT_SCALE as usize).div_ceil(256);
                    let x0 = inner.x + (cell0 as u16).min(inner.width);
                    let x1 = inner.x + (cell1 as u16).min(inner.width);
                    let color = Color::Rgb(rgb[0], rgb[1], rgb[2]);
                    for x in x0..x1 {
                        buf[(x, row)].set_bg(color);
                    }
                }

                let mut chars_before = 0usize;
                for (text, style) in line {
                    let color = crate::render::review::style_rgb(style.fg).unwrap_or(modal_fg);
                    let span_chars: Vec<char> = text.chars().collect();
                    let span_end = chars_before + span_chars.len();
                    let (b0, b1) = match selection {
                        Some((c0, c1, _)) => (
                            c0.clamp(chars_before, span_end),
                            c1.clamp(chars_before, span_end),
                        ),
                        None => (chars_before, chars_before),
                    };

                    // Split the span at the selection so its selected piece
                    // composites over the selection bg and the rest over the
                    // modal bg.
                    for (seg_start, seg_end, selected) in [
                        (chars_before, b0, false),
                        (b0, b1, true),
                        (b1, span_end, false),
                    ] {
                        if seg_start >= seg_end {
                            continue;
                        }
                        let seg_text: String = span_chars
                            [(seg_start - chars_before)..(seg_end - chars_before)]
                            .iter()
                            .collect();
                        let col = (seg_start as u16 * HOVER_TEXT_SCALE + 8) / 16;
                        let bg = if selected {
                            selection.map_or(run_bg, |(_, _, rgb)| rgb)
                        } else {
                            run_bg
                        };
                        TextRun {
                            col,
                            row: 0,
                            scale: HOVER_TEXT_SCALE,
                            color,
                            bg: Some(bg),
                            text: &seg_text,
                        }
                        .render(Rect::new(inner.x, row, 1, 1), buf, scene);
                    }
                    chars_before = span_end;
                }
            }
        },
        _ => {
            for (row_idx, line) in body.iter().skip(scroll).enumerate() {
                let row = inner.y + row_idx as u16;
                if row >= inner.y + inner.height {
                    break;
                }
                let mut x = inner.x;
                for (text, style) in line {
                    if x >= end_x {
                        break;
                    }
                    let (next_x, _) = buf.set_stringn(
                        x,
                        row,
                        text,
                        (end_x - x) as usize,
                        modal_style.patch(*style),
                    );
                    x = next_x;
                }
            }
            highlight_grid_selection(buf, &popup, inner, scroll, &stoat.theme);
        },
    }
}

/// The half-open char range selected on content `line`, bounded to the line's
/// text. `None` when the line lies outside the selection or there is none.
///
/// A middle line of a multi-line selection covers its whole text. The first and
/// last lines start and end at the selection's char columns.
fn hover_line_selection(popup: &HoverPopup, line: usize) -> Option<(usize, usize)> {
    let HoverSelection { anchor, head, .. } = popup.selection?;
    let (start, end) = if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    };
    if line < start.0 || line > end.0 {
        return None;
    }
    let width = popup.lines.get(line).map(|l| line_width(l)).unwrap_or(0);
    let c0 = if line == start.0 { start.1 } else { 0 };
    let c1 = if line == end.0 { end.1 } else { width };
    Some((c0.min(width), c1.min(width)))
}

/// Restyle the selected cells of the grid-rendered body with the selection
/// background, a per-row post-pass over the painted text.
///
/// Grid cells map to characters 1:1, so the selected column range on each
/// visible line is [`hover_line_selection`]. No-op when the theme carries no
/// selection background.
fn highlight_grid_selection(
    buf: &mut Buffer,
    popup: &HoverPopup,
    inner: Rect,
    scroll: usize,
    theme: &crate::theme::Theme,
) {
    let Some(bg) = theme.get(crate::theme::scope::UI_SELECTION).bg else {
        return;
    };
    for row_idx in 0..inner.height {
        let line = scroll + row_idx as usize;
        let Some((c0, c1)) = hover_line_selection(popup, line) else {
            continue;
        };
        let x0 = inner.x + (c0 as u16).min(inner.width);
        let x1 = inner.x + (c1 as u16).min(inner.width);
        let y = inner.y + row_idx;
        for x in x0..x1 {
            buf[(x, y)].set_bg(bg);
        }
    }
}

/// Map a screen pointer over the hover body to a `(content line, char column)`
/// position, clamped to the popup interior and the target line's length.
///
/// Replays [`render_hover`]'s scroll clamp to resolve the line. Under stoatty it
/// inverts the 0.85x popover scale (256ths of a cell over [`HOVER_TEXT_SCALE`])
/// to resolve the column. The grid path maps a cell to a column 1:1.
pub(crate) fn hover_hit_test(
    popup: &HoverPopup,
    stoatty: bool,
    col: u16,
    row: u16,
) -> (usize, usize) {
    let inner = popup.inner;
    let clamped_col = col.clamp(inner.x, inner.x + inner.width.saturating_sub(1));
    let clamped_row = row.clamp(inner.y, inner.y + inner.height.saturating_sub(1));

    let interior = inner.height as usize;
    let half_page = (interior / 2).max(1);
    let scroll = popup
        .lines
        .len()
        .saturating_sub(interior)
        .min(popup.scroll_half_pages * half_page);
    let line = (scroll + (clamped_row - inner.y) as usize).min(popup.lines.len().saturating_sub(1));

    let cell = (clamped_col - inner.x) as usize;
    let char_col = if stoatty {
        (cell * 256 + 128) / HOVER_TEXT_SCALE as usize
    } else {
        cell
    };
    let max_col = popup.lines.get(line).map(|l| line_width(l)).unwrap_or(0);
    (line, char_col.min(max_col))
}

/// The text of the popup's live selection, joined across full logical lines.
///
/// Endpoints clamp to visible columns, but a render-truncated middle line copies
/// as its whole logical text, since cell-granular mouse reporting cannot address
/// the truncated tail. Empty with no selection or a collapsed one.
pub(crate) fn hover_selected_text(popup: &HoverPopup) -> String {
    let Some(HoverSelection { anchor, head, .. }) = popup.selection else {
        return String::new();
    };
    let (start, end) = if anchor <= head {
        (anchor, head)
    } else {
        (head, anchor)
    };
    let logical = |line: usize| -> String {
        popup
            .lines
            .get(line)
            .map(|spans| spans.iter().map(|(t, _)| t.as_str()).collect())
            .unwrap_or_default()
    };

    if start.0 == end.0 {
        return logical(start.0)
            .chars()
            .skip(start.1)
            .take(end.1.saturating_sub(start.1))
            .collect();
    }

    let mut out: String = logical(start.0).chars().skip(start.1).collect();
    for line in (start.0 + 1)..end.0 {
        out.push('\n');
        out.push_str(&logical(line));
    }
    out.push('\n');
    out.extend(logical(end.0).chars().take(end.1));
    out
}

/// Compute the hover popup's screen rect and its interior rect.
///
/// Returns [`None`] when no popup is anchorable, which happens with no pending
/// hover, non-editor focus, an off-screen cursor, or a terminal too narrow for
/// content.
///
/// The single source of the placement math, shared by [`render_hover`] and the
/// smooth-scroll emit so the live frame and the pooled body agree on geometry.
///
/// The popup floats above panes. Only the cursor anchor is pane-relative, so a
/// wide or tall hover overflows pane boundaries freely while its width and
/// height stay bounded by the whole terminal frame. Placement is below-biased.
/// The popup sits below the cursor when at least [`MIN_HEIGHT`] rows remain in
/// the frame, and flips above otherwise, shrinking to the chosen side's free
/// space so it never renders past the window.
pub(crate) fn hover_popup_layout(stoat: &mut Stoat) -> Option<(Rect, Rect)> {
    let popup = stoat.pending_hover.as_ref()?.clone();

    // The popup floats above panes, so placement is bounded by the whole
    // terminal frame. Only the cursor anchor stays pane-relative.
    let frame = stoat.size();

    let ws = stoat.active_workspace_mut();
    let FocusTarget::SplitPane(pane_id) = ws.focus else {
        return None;
    };
    let pane = ws.panes.pane(pane_id);
    let View::Editor(editor_id) = pane.view else {
        return None;
    };
    let (content_area, _) = split_pane_status(pane.area);

    let editor = ws.editors.get_mut(editor_id)?;
    let cursor_screen = cursor_screen_position(editor, content_area, popup.anchor_offset)?;

    let interior_width = frame.width.saturating_sub(2);
    if interior_width == 0 {
        return None;
    }
    let body: Vec<Vec<(String, Style)>> = popup
        .lines
        .iter()
        .map(|line| truncate_line(line, interior_width as usize))
        .collect();
    let max_line_width = body.iter().map(|line| line_width(line)).max().unwrap_or(0) as u16;
    let popup_width = (max_line_width + 2).clamp(3, frame.width.clamp(3, MAX_WIDTH));

    let rel_y = cursor_screen.1.saturating_sub(frame.y);
    let below = frame.height > rel_y + MIN_HEIGHT;
    let max_height = if below {
        frame.height.saturating_sub(rel_y + 1)
    } else {
        rel_y
    };
    // Cap at the room beside the cursor and the absolute MAX_HEIGHT, then at
    // half the frame, which is the bound that actually shrinks a large hover on
    // a small window. Both bounds hold a 3-row minimum box.
    let height_cap = max_height
        .clamp(3, MAX_HEIGHT)
        .min((frame.height / 2).max(3));
    let popup_height = (body.len() as u16 + 2).min(height_cap);

    let popup_x = cursor_screen
        .0
        .min(frame.x + frame.width.saturating_sub(popup_width));
    let popup_y = if below {
        cursor_screen.1 + 1
    } else {
        cursor_screen.1.saturating_sub(popup_height).max(frame.y)
    };

    let popup_area = Rect {
        x: popup_x,
        y: popup_y,
        width: popup_width,
        height: popup_height,
    };
    let inner = Block::default().borders(Borders::ALL).inner(popup_area);
    Some((popup_area, inner))
}

/// Render hover body page `page` as a self-contained VT plus APC byte stream for
/// the HOVER smooth-scroll pool.
///
/// The `region_height` body lines starting at `page * region_height` paint as
/// sub-cell text runs at the 0.85x hover scale over the region's opaque cell
/// background. Every cell carries a background (the default resolves to the
/// theme background), so when the pool composites over the region during a
/// glide it occludes the live body drawn there rather than double-rendering it.
/// Coordinates are region-local because the pool composites the page at the
/// region origin.
pub(crate) fn render_hover_page(
    popup: &HoverPopup,
    page: u64,
    theme: &crate::theme::Theme,
    region_width: u16,
    region_height: u16,
) -> Vec<u8> {
    let area = Rect::new(0, 0, region_width, region_height);
    let buf = Buffer::empty(area);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HINTS);
    let modal_fg = crate::render::review::style_rgb(modal_style.fg).unwrap_or([255, 255, 255]);
    let run_bg = crate::render::review::style_rgb(
        theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg),
    )
    .unwrap_or([0, 0, 0]);

    let start_row = page.saturating_mul(region_height as u64) as usize;

    let mut scene = ApcScene::new();
    let mut scratch = Buffer::empty(area);
    for row_idx in 0..region_height {
        let Some(line) = popup.lines.get(start_row + row_idx as usize) else {
            break;
        };
        let line = truncate_line(line, region_width as usize);
        let mut chars_before = 0u16;
        for (text, style) in &line {
            let col = (chars_before * HOVER_TEXT_SCALE + 8) / 16;
            let color = crate::render::review::style_rgb(style.fg).unwrap_or(modal_fg);
            TextRun {
                col,
                row: 0,
                scale: HOVER_TEXT_SCALE,
                color,
                bg: Some(run_bg),
                text,
            }
            .render(Rect::new(0, row_idx, 1, 1), &mut scratch, &mut scene);
            chars_before += text.chars().count() as u16;
        }
    }

    let apc = scene.buffer().clone();
    let mut bytes = crate::smooth_scroll::serialize_buffer(&buf);
    bytes.extend_from_slice(&apc);
    bytes
}

pub(crate) fn cursor_screen_position(
    editor: &mut crate::editor_state::EditorState,
    content_area: Rect,
    anchor_offset: usize,
) -> Option<(u16, u16)> {
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();
    if anchor_offset > rope.len() {
        return None;
    }
    let point = rope.offset_to_point(anchor_offset);
    let display = snapshot.buffer_to_display(point);
    if display.row < editor.scroll_row {
        return None;
    }
    let visible_rows = content_area.height as u32;
    if display.row >= editor.scroll_row + visible_rows {
        return None;
    }
    let y = content_area.y + (display.row - editor.scroll_row) as u16;
    let x = content_area.x + display.column as u16;
    if x >= content_area.x + content_area.width || y >= content_area.y + content_area.height {
        return None;
    }
    Some((x, y))
}

pub(crate) fn truncate_to_width(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        return line.to_string();
    }
    line.chars().take(width).collect()
}

/// Total character width of a styled line, summed across its spans.
fn line_width(line: &[(String, Style)]) -> usize {
    line.iter().map(|(text, _)| text.chars().count()).sum()
}

/// Truncate a styled line to `width` characters, clipping the span that crosses
/// the limit and dropping the rest.
fn truncate_line(line: &[(String, Style)], width: usize) -> Vec<(String, Style)> {
    let mut out = Vec::new();
    let mut used = 0;
    for (text, style) in line {
        if used >= width {
            break;
        }
        let remaining = width - used;
        let chars = text.chars().count();
        if chars <= remaining {
            out.push((text.clone(), *style));
            used += chars;
        } else {
            out.push((text.chars().take(remaining).collect(), *style));
            break;
        }
    }
    out
}
