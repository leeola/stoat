use crate::{
    app::Stoat,
    pane::{FocusTarget, View},
    render::layout::split_pane_status,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::Style,
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

/// Paint the hover popup, if any, anchored to the focused editor's
/// primary cursor.
///
/// Placement is below-biased. The popup sits below the cursor when at least
/// [`MIN_HEIGHT`] rows remain there, and flips above otherwise. Its height
/// shrinks to the chosen side's free space so it never renders past the pane,
/// and content that overflows scrolls. Lines wider than the popup interior are
/// truncated.
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
            for (row_idx, line) in body.iter().skip(scroll).enumerate() {
                let row = inner.y + row_idx as u16;
                if row >= inner.y + inner.height {
                    break;
                }
                let mut chars_before = 0u16;
                for (text, style) in line {
                    let col = (chars_before * HOVER_TEXT_SCALE + 8) / 16;
                    let color = crate::render::review::style_rgb(style.fg).unwrap_or(modal_fg);
                    TextRun {
                        col,
                        row: 0,
                        scale: HOVER_TEXT_SCALE,
                        color,
                        bg: run_bg,
                        text,
                    }
                    .render(Rect::new(inner.x, row, 1, 1), buf, scene);
                    chars_before += text.chars().count() as u16;
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
        },
    }
}

/// Compute the hover popup's screen rect and its interior rect.
///
/// Returns [`None`] when no popup is anchorable, which happens with no pending
/// hover, non-editor focus, an off-screen cursor, or a pane too narrow for
/// content.
///
/// The single source of the placement math, shared by [`render_hover`] and the
/// smooth-scroll emit so the live frame and the pooled body agree on geometry.
/// Placement is below-biased. The popup sits below the cursor when at least
/// [`MIN_HEIGHT`] rows remain there, and flips above otherwise, shrinking to the
/// chosen side's free space so it never renders past the pane.
pub(crate) fn hover_popup_layout(stoat: &mut Stoat) -> Option<(Rect, Rect)> {
    let popup = stoat.pending_hover.as_ref()?.clone();

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

    let interior_width = content_area.width.saturating_sub(2);
    if interior_width == 0 {
        return None;
    }
    let body: Vec<Vec<(String, Style)>> = popup
        .lines
        .iter()
        .map(|line| truncate_line(line, interior_width as usize))
        .collect();
    let max_line_width = body.iter().map(|line| line_width(line)).max().unwrap_or(0) as u16;
    let popup_width = (max_line_width + 2).clamp(3, content_area.width.max(3));

    let rel_y = cursor_screen.1.saturating_sub(content_area.y);
    let below = content_area.height > rel_y + MIN_HEIGHT;
    let max_height = if below {
        content_area.height.saturating_sub(rel_y + 1)
    } else {
        rel_y
    };
    let popup_height = (body.len() as u16 + 2).min(max_height.max(3));

    let popup_x = cursor_screen
        .0
        .min(content_area.x + content_area.width.saturating_sub(popup_width));
    let popup_y = if below {
        cursor_screen.1 + 1
    } else {
        cursor_screen
            .1
            .saturating_sub(popup_height)
            .max(content_area.y)
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
