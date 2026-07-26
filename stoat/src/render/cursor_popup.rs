//! Base component for popups anchored to the focused editor's cursor.
//!
//! The symbol picker and the code-action picker are the same popup over
//! different lists. Each is a numbered window of rows with an optional
//! position footer, floated next to the cursor and clamped inside the pane.
//! Signature help and completion float the same way over different bodies.
//!
//! Anchoring to a cursor is what makes these different from the modal family
//! in [`crate::render::chrome`]. A modal is centered in its area and can be
//! sized before anything is known about the document, while these have to
//! resolve a buffer offset to a screen cell first and then fit themselves
//! around wherever that lands.

use crate::{
    app::Stoat,
    editor_state::EditorState,
    pane::{FocusTarget, View},
    render::layout::split_pane_status,
};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};

/// The focused editor's content area and the screen cell holding
/// `anchor_offset`, or `None` when no cursor popup should appear.
///
/// Every cursor popup starts here, walking focus to a pane, the pane to an
/// editor, and a buffer offset to a cell. Any step can fail, and each failure
/// means the same thing to the caller, which is that there is nothing to
/// anchor to.
///
/// Returns owned values rather than borrows, so a caller that needs the editor
/// afterwards can reach for it again.
pub(crate) fn focused_editor_popup_ctx(
    stoat: &mut Stoat,
    anchor_offset: usize,
) -> Option<(Rect, (u16, u16))> {
    let ws = stoat.active_workspace_mut();
    let FocusTarget::SplitPane = ws.focus else {
        return None;
    };
    let pane_id = ws.panes.focus();
    let pane = ws.panes.pane(pane_id);
    let View::Editor(editor_id) = pane.view else {
        return None;
    };
    let (content_area, _) = split_pane_status(pane.area);

    let editor = ws.editors.get_mut(editor_id)?;
    let cursor = cursor_screen_position(editor, content_area, anchor_offset)?;
    Some((content_area, cursor))
}

/// The box a popup of `size` content cells takes next to `cursor`, clamped
/// inside `content_area`.
///
/// `size` is the content the caller measured, without the border. The returned
/// rect includes it.
///
/// A popup would rather not cover the line its cursor sits on, so it goes
/// above or below rather than over. `prefer_above` picks which side is tried
/// first, and the other is used when the first does not fit. When neither
/// does, the popup pins to the top of the area and covers what it must.
pub(crate) fn popup_rect(
    content_area: Rect,
    cursor: (u16, u16),
    size: (u16, u16),
    prefer_above: bool,
) -> Rect {
    let width = (size.0 + 2).clamp(3, content_area.width.max(3));
    let height = (size.1 + 2).clamp(3, content_area.height.max(3));

    let x = cursor
        .0
        .min(content_area.x + content_area.width.saturating_sub(width));

    let fits_above = cursor.1 >= content_area.y + height;
    let fits_below = cursor.1 + 1 + height <= content_area.y + content_area.height;
    let above = cursor.1.saturating_sub(height);
    let below = cursor.1 + 1;

    let y = if prefer_above {
        if fits_above {
            above
        } else if fits_below {
            below
        } else {
            content_area.y
        }
    } else if fits_below {
        below
    } else if fits_above {
        above
    } else {
        content_area.y
    };

    Rect {
        x,
        y,
        width,
        height,
    }
}

/// The content cells a numbered popup needs, as `(widest line, line count)`.
///
/// Sized from the strings as they will paint, so a caller that truncated its
/// rows to the pane gets a box that matches what it will put in it.
pub(crate) fn content_size(body: &[String], footer: Option<&String>) -> (u16, u16) {
    let width = body
        .iter()
        .chain(footer)
        .map(|line| line.chars().count())
        .max()
        .unwrap_or(0) as u16;
    let height = body.len() as u16 + u16::from(footer.is_some());
    (width, height)
}

/// Paint a numbered popup's rows into `inner`, with `footer` on the row under
/// them.
///
/// `selected` is the highlighted row's position within the painted window, so
/// a caller whose selection has scrolled out of view passes `None`. Rows past
/// the bottom of `inner` and characters past its right edge are dropped, which
/// is what lets a caller hand over a full window it measured before the box
/// was clamped.
pub(crate) fn paint_numbered_rows(
    body: &[String],
    footer: Option<&String>,
    selected: Option<usize>,
    inner: Rect,
    styles: (Style, Style),
    buf: &mut Buffer,
) {
    let (base_style, selected_style) = styles;

    for (row_idx, line) in body.iter().enumerate() {
        let row = inner.y + row_idx as u16;
        if row >= inner.y + inner.height {
            break;
        }
        let style = if selected == Some(row_idx) {
            selected_style
        } else {
            base_style
        };
        paint_row(line, inner, row, style, buf);
    }

    let Some(footer) = footer else {
        return;
    };
    let row = inner.y + body.len() as u16;
    if row < inner.y + inner.height {
        paint_row(footer, inner, row, base_style, buf);
    }
}

fn paint_row(line: &str, inner: Rect, row: u16, style: Style, buf: &mut Buffer) {
    for (col_idx, ch) in line.chars().enumerate() {
        let col = inner.x + col_idx as u16;
        if col >= inner.x + inner.width {
            break;
        }
        buf[(col, row)].set_char(ch).set_style(style);
    }
}

/// The screen cell holding `anchor_offset`, or `None` when the popup should
/// not appear at all.
///
/// An editor showing a review diff has no stable mapping from a buffer offset
/// to a screen row, so a popup anchored to one would land somewhere arbitrary.
/// Returning `None` there is what keeps it from being painted.
fn cursor_screen_position(
    editor: &mut EditorState,
    content_area: Rect,
    anchor_offset: usize,
) -> Option<(u16, u16)> {
    if editor.review_view.is_some() {
        return None;
    }
    crate::render::hover::cursor_screen_position(editor, content_area, anchor_offset)
}

#[cfg(test)]
mod tests {
    use super::{content_size, popup_rect};
    use ratatui::layout::Rect;

    /// Room on both sides of the cursor, so only the preference decides.
    #[test]
    fn a_popup_takes_the_side_it_prefers_when_both_fit() {
        let area = Rect::new(0, 0, 80, 40);
        let above = popup_rect(area, (10, 20), (10, 4), true);
        let below = popup_rect(area, (10, 20), (10, 4), false);
        assert_eq!(above.y + above.height, 20, "above ends on the cursor row");
        assert_eq!(below.y, 21, "below starts under it");
        assert_eq!(above.height, 6, "the border adds a row at each end");
    }

    #[test]
    fn a_popup_takes_the_other_side_when_its_first_choice_does_not_fit() {
        let area = Rect::new(0, 0, 80, 40);
        assert_eq!(
            popup_rect(area, (10, 2), (10, 4), true).y,
            3,
            "no room above, so it drops below"
        );
        assert_eq!(
            popup_rect(area, (10, 37), (10, 4), false).y + 6,
            37,
            "no room below, so it rises above"
        );
    }

    #[test]
    fn a_popup_with_room_on_neither_side_pins_to_the_top() {
        // Six rows of popup against an eight-row area leaves no room on
        // either side of a cursor in the middle of it.
        let area = Rect::new(0, 3, 80, 8);
        let rect = popup_rect(area, (10, 7), (10, 4), true);
        assert_eq!(rect.y, area.y, "it pins to the area rather than vanishing");
    }

    #[test]
    fn a_popup_stays_inside_the_area_it_floats_over() {
        let area = Rect::new(0, 0, 20, 40);
        let rect = popup_rect(area, (18, 20), (30, 4), true);
        assert_eq!(rect.width, 20, "content wider than the area clamps to it");
        assert_eq!(
            rect.x + rect.width,
            20,
            "and the box slides left to stay inside"
        );
    }

    #[test]
    fn content_size_measures_the_widest_line_and_counts_the_footer() {
        let body = vec!["1. short".to_string(), "2. a longer entry".to_string()];
        assert_eq!(content_size(&body, None), (17, 2));
        assert_eq!(
            content_size(&body, Some(&"1-2 / 9".to_string())),
            (17, 3),
            "the footer takes a row without widening a wider body"
        );
        assert_eq!(content_size(&[], None), (0, 0));
    }
}
