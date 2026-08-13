use super::TEXT_SCALE_POPUP;
use crate::{app::Stoat, editor_state::EditorId};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, StatefulWidget},
};
use stoat_widgets::{
    text_run::{self, TextRun},
    ApcScene,
};

/// A live text selection over the hover popup body.
///
/// Endpoints are `(content line, char column)` into [`HoverPopup::lines`], so
/// tuple ordering sorts them into a range. `dragging` is true between the mouse
/// down and its release, so a drag past the popup rect keeps extending the head.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HoverSelection {
    pub(crate) anchor: (usize, usize),
    pub(crate) head: (usize, usize),
    pub(crate) dragging: bool,
}

/// Hover popup state ready to paint. Mirrors [`HoverResponse`] but
/// lives on [`Stoat::pending_hover`] (separate from the in-flight
/// task slot) so the renderer can borrow it without polling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoverPopup {
    /// Rendered content, one line per entry, each a list of styled spans.
    pub(crate) lines: Vec<Vec<(String, Style)>>,
    pub(crate) anchor_offset: usize,
    /// The editor that requested this hover. Set from the response's
    /// [`HoverResponse::editor_id`] once focus is confirmed unchanged.
    pub(crate) editor_id: EditorId,
    /// Half-page scroll offset applied by [`render_hover`],
    /// advanced by Ctrl-d/Ctrl-u while the popup is open. Clamped to the content
    /// height at render, so an over-scroll past the bottom does not accumulate.
    pub(crate) scroll_half_pages: usize,
    /// Screen rect the popup last painted over, stamped by
    /// [`render_hover`]. The mouse handler hit-tests it so
    /// a wheel over the popup scrolls it rather than the pane beneath. Empty
    /// ([`Rect::default`]) until the first render.
    pub(crate) area: Rect,
    /// Interior rect (inside the border) the body last painted over, stamped by
    /// [`render_hover`]. The selection hit-test maps a
    /// pointer through this. Empty until the first render.
    pub(crate) inner: Rect,
    /// The live mouse selection over the body, if any.
    pub(crate) selection: Option<HoverSelection>,
    /// Content-version stamp for the hover pool, from the shared generation
    /// counter, set once at construction. A popup's body is immutable once
    /// built, so a new hover gets a new stamp and the per-frame version is O(1)
    /// instead of a walk of every line's text.
    pub(crate) generation: u64,
    /// Characters in the widest of [`Self::lines`], measured at construction on
    /// the same immutability the generation stamp relies on.
    ///
    /// The popup's box is sized from this every frame, twice, and measuring it
    /// there would walk every span of every line for a body that cannot have
    /// changed since the last frame asked.
    pub(crate) max_line_width: usize,
}

impl HoverPopup {
    /// Build a popup around `lines`, measuring them and claiming a generation
    /// stamp before anything paints.
    ///
    /// The measurement is why this exists rather than a struct literal.
    /// [`Self::max_line_width`] has to agree with [`Self::lines`], and a caller
    /// free to set both can silently disagree, which mis-sizes the box without
    /// failing anything. The fields a render stamps later start empty.
    pub(crate) fn new(
        lines: Vec<Vec<(String, Style)>>,
        anchor_offset: usize,
        editor_id: EditorId,
    ) -> Self {
        Self {
            max_line_width: lines.iter().map(|line| line_width(line)).max().unwrap_or(0),
            lines,
            anchor_offset,
            editor_id,
            scroll_half_pages: 0,
            area: Rect::default(),
            inner: Rect::default(),
            selection: None,
            generation: crate::picker::next_generation(),
        }
    }
}

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
pub(crate) fn render_hover(stoat: &mut Stoat, buf: &mut Buffer, scene: &mut ApcScene) {
    let Some((popup_area, _)) = hover_popup_layout(stoat) else {
        // An unplaceable popup paints nothing, so clear its stored rects. A
        // default rect hit-tests to no point, so the mouse and wheel handlers
        // fall through to the pane beneath instead of the stale area swallowing
        // the event.
        if let Some(open) = stoat.pending_hover.as_mut() {
            open.area = Rect::default();
            open.inner = Rect::default();
        }
        return;
    };

    let modal_style = stoat.theme.get(crate::theme::scope::UI_MODAL_HINTS);

    crate::render::clear_themed(popup_area, buf, &stoat.theme);
    let inner = crate::render::chrome::modal_frame(
        buf,
        popup_area,
        None,
        modal_style,
        &stoat.theme,
        &mut *scene,
    );

    // Clamp the half-page scroll to the content that overflows the interior, then
    // write the clamped counter back so scrolling up past the bottom takes effect
    // on the first Ctrl-u rather than after replaying the over-scroll.
    let interior = inner.height as usize;
    let half_page = (interior / 2).max(1);
    let scroll = {
        let popup = stoat.pending_hover.as_ref().expect("layout placed a popup");
        popup
            .lines
            .len()
            .saturating_sub(interior)
            .min(popup.scroll_half_pages * half_page)
    };
    if let Some(open) = stoat.pending_hover.as_mut() {
        open.scroll_half_pages = scroll / half_page;
        open.area = popup_area;
        open.inner = inner;
    }

    // A span's style is a delta over the modal base, so a plain span keeps the
    // modal look. The rich arm needs an RGB modal fg and background to compose
    // one TextRun per span at the popover text scale. Without them it paints
    // cells.
    let modal_fg = crate::render::paint::style_rgb(modal_style.fg);
    let run_bg = crate::render::paint::style_rgb(
        stoat
            .theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg),
    );
    let sel_rgb =
        crate::render::paint::style_rgb(stoat.theme.get(crate::theme::scope::UI_SELECTION).bg);

    let end_x = inner.x + inner.width;
    let popup = stoat.pending_hover.as_ref().expect("layout placed a popup");
    // Clipped where it is painted rather than gathered first, so the visible
    // text stays borrowed from the popup for a repaint that only reads it.
    let visible = || {
        popup
            .lines
            .iter()
            .skip(scroll)
            .take(inner.height as usize)
            .map(|line| clip_line(line, inner.width as usize))
    };

    match (scene.live(), modal_fg, run_bg) {
        (true, Some(modal_fg), Some(run_bg)) => {
            for (row_idx, line) in visible().enumerate() {
                let row = inner.y + row_idx as u16;
                let selection = sel_rgb.and_then(|rgb| {
                    hover_line_selection(popup, scroll + row_idx).map(|(c0, c1)| (c0, c1, rgb))
                });

                // Patch the cell bg behind the 0.85x glyph boxes so the selection
                // band spans full cell height. Chars map to cells through the
                // scale, floored at the start and ceiled at the end.
                if let Some((c0, c1, rgb)) = selection {
                    let cell0 = c0 * TEXT_SCALE_POPUP as usize / 256;
                    let cell1 = (c1 * TEXT_SCALE_POPUP as usize).div_ceil(256);
                    let x0 = inner.x + (cell0 as u16).min(inner.width);
                    let x1 = inner.x + (cell1 as u16).min(inner.width);
                    let color = Color::Rgb(rgb[0], rgb[1], rgb[2]);
                    for x in x0..x1 {
                        buf[(x, row)].set_bg(color);
                    }
                }

                let mut chars_before = 0usize;
                for (text, style) in line {
                    let color = crate::render::paint::style_rgb(style.fg).unwrap_or(modal_fg);
                    let span_end = chars_before + text.chars().count();
                    let (b0, b1) = match selection {
                        Some((c0, c1, _)) => (
                            c0.clamp(chars_before, span_end),
                            c1.clamp(chars_before, span_end),
                        ),
                        None => (chars_before, chars_before),
                    };

                    // Split the span at the selection so its selected piece
                    // composites over the selection bg and the rest over the
                    // modal bg. The split points are char offsets into the span,
                    // resolved to byte offsets so each segment stays a slice.
                    let byte0 = char_to_byte(text, b0 - chars_before);
                    let byte1 = char_to_byte(text, b1 - chars_before);
                    for (seg_start, seg_text, selected) in [
                        (chars_before, &text[..byte0], false),
                        (b0, &text[byte0..byte1], true),
                        (b1, &text[byte1..], false),
                    ] {
                        if seg_text.is_empty() {
                            continue;
                        }
                        let col = text_run::advance_sixteenths(seg_start, TEXT_SCALE_POPUP) as i16;
                        let bg = if selected {
                            selection.map_or(run_bg, |(_, _, rgb)| rgb)
                        } else {
                            run_bg
                        };
                        TextRun {
                            col,
                            row: 0,
                            scale: TEXT_SCALE_POPUP,
                            color,
                            bg: Some(bg),
                            text: seg_text,
                        }
                        .render(Rect::new(inner.x, row, 1, 1), buf, scene);
                    }
                    chars_before = span_end;
                }
            }
        },
        _ => {
            for (row_idx, line) in visible().enumerate() {
                let row = inner.y + row_idx as u16;
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
                        modal_style.patch(style),
                    );
                    x = next_x;
                }
            }
            highlight_grid_selection(buf, popup, inner, scroll, &stoat.theme);
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
/// Replays [`render_hover`]'s scroll clamp to resolve the line, and inverts the
/// 0.85x popover scale (256ths of a cell over [`TEXT_SCALE_POPUP`]) to resolve
/// the column.
pub(crate) fn hover_hit_test(popup: &HoverPopup, col: u16, row: u16) -> (usize, usize) {
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
    let char_col = (cell * 256 + 128) / TEXT_SCALE_POPUP as usize;
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
    let anchor_offset = stoat.pending_hover.as_ref()?.anchor_offset;

    // The popup floats above panes, so placement is bounded by the whole
    // terminal frame. Only the cursor anchor stays pane-relative.
    let frame = stoat.size();

    // The cursor cell is pane-relative, but the box is sized against the whole
    // frame, so the content area this resolves alongside it goes unused.
    let (_, cursor_screen) =
        crate::render::cursor_popup::focused_editor_popup_ctx(stoat, anchor_offset)?;

    let interior_width = frame.width.saturating_sub(2);
    if interior_width == 0 {
        return None;
    }

    // A truncated line's width is its content width capped at the interior, and
    // truncation is one-to-one per line, so the geometry never rebuilds a body.
    // Every line meets the same cap, so capping the widest gives what capping
    // each and taking the widest would.
    let popup = stoat.pending_hover.as_ref()?;
    let max_line_width = popup.max_line_width.min(interior_width as usize) as u16;
    let body_len = popup.lines.len();

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
    let popup_height = (body_len as u16 + 2).min(height_cap);

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
    let buf = crate::smooth_scroll::page_buffer(area, theme);

    let modal_style = theme.get(crate::theme::scope::UI_MODAL_HINTS);
    let modal_fg = crate::render::paint::style_rgb(modal_style.fg).unwrap_or([255, 255, 255]);
    let run_bg = crate::render::paint::style_rgb(
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
            let col = text_run::advance_sixteenths(chars_before.into(), TEXT_SCALE_POPUP) as i16;
            let color = crate::render::paint::style_rgb(style.fg).unwrap_or(modal_fg);
            TextRun {
                col,
                row: 0,
                scale: TEXT_SCALE_POPUP,
                color,
                bg: Some(run_bg),
                text,
            }
            .render(Rect::new(0, row_idx, 1, 1), &mut scratch, &mut scene);
            chars_before += text.chars().count() as u16;
        }
    }

    let apc = scene.buffer().clone();
    let mut bytes = crate::render::serialize_buffer(&buf);
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
    // The diff view paints the buffer text in its right column, so a cursor-
    // anchored popup lines up against right_text_x rather than the pane's left
    // edge. The clamp below still skips a popup that would fall past the pane.
    let base_x = if editor.diff_view {
        crate::render::review::right_text_x(content_area)
    } else {
        content_area.x
    };
    let x = base_x + display.column as u16;
    if x >= content_area.x + content_area.width || y >= content_area.y + content_area.height {
        return None;
    }
    Some((x, y))
}

/// Byte offset of the `char_idx`-th character in `s`, or `s.len()` when
/// `char_idx` is at or past the end.
fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(byte, _)| byte)
        .unwrap_or(s.len())
}

/// Total character width of a styled line, summed across its spans.
pub(crate) fn line_width(line: &[(String, Style)]) -> usize {
    line.iter().map(|(text, _)| text.chars().count()).sum()
}

/// The spans of `line` that fit in `width` characters, borrowed from it.
///
/// The span crossing the limit is sliced at a character boundary and the ones
/// past it are dropped. Nothing is copied, which is what lets a hover repaint
/// without rebuilding the text it is showing.
fn clip_line(line: &[(String, Style)], width: usize) -> impl Iterator<Item = (&str, Style)> {
    let mut used = 0;
    line.iter().map_while(move |(text, style)| {
        if used >= width {
            return None;
        }

        let remaining = width - used;
        let chars = text.chars().count();
        if chars <= remaining {
            used += chars;
            return Some((text.as_str(), *style));
        }

        // Byte offset of the character that would overshoot, which is where the
        // span is cut. Taking `remaining` characters instead would collect them
        // into a string of their own.
        let end = text
            .char_indices()
            .nth(remaining)
            .map_or(text.len(), |(idx, _)| idx);
        used = width;
        Some((&text[..end], *style))
    })
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

#[cfg(test)]
mod tests {
    use super::clip_line;
    use ratatui::style::{Color, Style};

    fn line(spans: &[&str]) -> Vec<(String, Style)> {
        spans
            .iter()
            .enumerate()
            .map(|(i, text)| {
                let style = Style::default().fg(if i % 2 == 0 { Color::Red } else { Color::Blue });
                ((*text).to_owned(), style)
            })
            .collect()
    }

    fn clipped(spans: &[&str], width: usize) -> Vec<String> {
        clip_line(&line(spans), width)
            .map(|(text, _)| text.to_owned())
            .collect()
    }

    #[test]
    fn a_line_inside_the_width_keeps_every_span() {
        assert_eq!(
            clipped(&["abc", "de"], 10),
            ["abc", "de"],
            "nothing is cut when the line fits"
        );
    }

    #[test]
    fn the_span_crossing_the_width_is_cut_and_the_rest_dropped() {
        assert_eq!(
            clipped(&["abc", "defgh", "ij"], 6),
            ["abc", "def"],
            "the crossing span keeps the characters that fit, the next one goes"
        );
    }

    #[test]
    fn a_crossing_span_is_cut_on_a_character_not_a_byte() {
        assert_eq!(
            clipped(&["\u{e9}\u{e9}\u{e9}\u{e9}"], 2),
            ["\u{e9}\u{e9}"],
            "two accents are four bytes, so cutting at byte two would split one"
        );
    }

    #[test]
    fn a_span_ending_exactly_on_the_width_keeps_the_whole_span() {
        assert_eq!(
            clipped(&["abc", "def"], 3),
            ["abc"],
            "the span that fills the width is kept whole and stops the line"
        );
    }

    #[test]
    fn no_width_yields_nothing() {
        assert!(
            clipped(&["abc"], 0).is_empty(),
            "a zero width shows no text"
        );
    }

    #[test]
    fn styles_ride_along_with_the_spans_they_came_from() {
        let spans = line(&["ab", "cdef"]);
        let styles: Vec<Option<Color>> = clip_line(&spans, 4).map(|(_, s)| s.fg).collect();
        assert_eq!(
            styles,
            [Some(Color::Red), Some(Color::Blue)],
            "a cut span keeps its own style"
        );
    }
}
