use crate::{editor_state::EditorState, render::review::render_review};
use ratatui::{buffer::Buffer, layout::Rect, style::Style};

pub(crate) fn render_editor(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    is_focused: bool,
) {
    editor.viewport_rows = Some(inner.height as u32);

    if editor.review_view.is_some() {
        render_review(editor, inner, fallback_style, theme, buf);
        return;
    }

    let snapshot = editor.display_map.snapshot();
    let visible_rows = inner.height as u32;
    let total_rows = snapshot.line_count();
    let end_row = (editor.scroll_row + visible_rows).min(total_rows);
    if end_row <= editor.scroll_row {
        return;
    }

    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;

    {
        let mut x = inner.x;
        let mut y = inner.y;
        'chunks: for chunk in snapshot.highlighted_chunks(editor.scroll_row..end_row) {
            let style = chunk
                .highlight_style
                .as_ref()
                .map(|hs| hs.to_ratatui_style())
                .unwrap_or(fallback_style);
            for ch in chunk.text.chars() {
                if ch == '\n' {
                    y += 1;
                    x = inner.x;
                    if y >= bottom {
                        break 'chunks;
                    }
                    continue;
                }
                if x >= right {
                    continue;
                }
                buf[(x, y)].set_char(ch).set_style(style);
                x += 1;
            }
        }
    }

    if !is_focused {
        return;
    }

    let buffer_snapshot = snapshot.buffer_snapshot();
    let selection_style = theme.get(crate::theme::scope::UI_SELECTION_EDITOR);
    let cursor_style = theme.get(crate::theme::scope::UI_CURSOR);
    for selection in editor.selections.all_anchors() {
        let start_offset = buffer_snapshot.resolve_anchor(&selection.start);
        let end_offset = buffer_snapshot.resolve_anchor(&selection.end);
        let head_offset = buffer_snapshot.resolve_anchor(&selection.head());
        let rope = buffer_snapshot.rope();

        if start_offset != end_offset {
            let mut offset = start_offset;
            let mut chars = rope.chars_at(offset);
            while offset < end_offset {
                let Some(ch) = chars.next() else {
                    break;
                };
                if ch != '\n' && offset != head_offset {
                    let point = rope.offset_to_point(offset);
                    let display = snapshot.buffer_to_display(point);
                    if display.row >= editor.scroll_row && display.row < end_row {
                        let y = inner.y + (display.row - editor.scroll_row) as u16;
                        let x = inner.x + display.column as u16;
                        if x < right && y < bottom {
                            let cell = &mut buf[(x, y)];
                            cell.set_style(selection_style);
                        }
                    }
                }
                offset += ch.len_utf8();
            }
        }

        let head_point = buffer_snapshot.point_for_anchor(&selection.head());
        let display = snapshot.buffer_to_display(head_point);
        if display.row >= editor.scroll_row && display.row < end_row {
            let y = inner.y + (display.row - editor.scroll_row) as u16;
            let x = inner.x + display.column as u16;
            if x < right && y < bottom {
                let cell = &mut buf[(x, y)];
                let existing_char = cell.symbol().chars().next().unwrap_or(' ');
                let char_to_paint = if existing_char == '\0' {
                    ' '
                } else {
                    existing_char
                };
                cell.set_char(char_to_paint);
                cell.set_style(cursor_style);
            }
        }
    }
}

pub(crate) fn editor_cursor_position(editor: &mut EditorState) -> Option<(u32, u32)> {
    if editor.review_view.is_some() {
        return None;
    }
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    let point = buffer_snapshot.point_for_anchor(&sel.head());
    Some((point.row + 1, point.column + 1))
}
