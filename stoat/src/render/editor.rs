use crate::{editor_state::EditorState, render::review::render_review};
use lsp_types::DiagnosticSeverity;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use std::{collections::BTreeMap, path::Path};
use stoat_text::{cursor_offset, Bias};

pub(crate) fn render_editor(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    is_focused: bool,
) {
    render_editor_with_overlay(
        editor,
        inner,
        fallback_style,
        theme,
        buf,
        is_focused,
        None,
        None,
        None,
    );
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn render_editor_with_overlay(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
    is_focused: bool,
    goto_word_labels: Option<&BTreeMap<String, usize>>,
    search_query: Option<&str>,
    diagnostic_info: Option<(&Path, &crate::diagnostics::DiagnosticSet)>,
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

    let row_severity = diagnostic_info
        .map(|(path, set)| compute_row_severity(set, path))
        .unwrap_or_default();
    let gutter_w: u16 = if row_severity.is_empty() { 0 } else { 1 };
    let inner = Rect {
        x: inner.x + gutter_w,
        y: inner.y,
        width: inner.width.saturating_sub(gutter_w),
        height: inner.height,
    };
    if gutter_w > 0 {
        paint_diagnostic_gutter(
            &row_severity,
            inner.x - gutter_w,
            inner.y,
            inner.height,
            editor.scroll_row,
            end_row,
            theme,
            buf,
        );
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

    let buffer_snapshot = snapshot.buffer_snapshot();

    if let Some(query) = search_query.filter(|q| !q.is_empty()) {
        if let Ok(regex) = stoat_text::compile_search_regex(query) {
            let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);
            let rope = buffer_snapshot.rope();
            let text = rope.to_string();
            for m in regex.find_iter(&text) {
                let match_start = m.start();
                let match_end = m.end();
                if match_end == match_start {
                    continue;
                }
                let mut offset = match_start;
                let mut chars = rope.chars_at(offset);
                while offset < match_end {
                    let Some(ch) = chars.next() else {
                        break;
                    };
                    if ch != '\n' {
                        let point = rope.offset_to_point(offset);
                        let display = snapshot.buffer_to_display(point, Bias::Left);
                        if display.row >= editor.scroll_row && display.row < end_row {
                            let y = inner.y + (display.row - editor.scroll_row) as u16;
                            let x = inner.x + display.column as u16;
                            if x < right && y < bottom {
                                buf[(x, y)].set_style(match_style);
                            }
                        }
                    }
                    offset += ch.len_utf8();
                }
            }
        }
    }

    if !is_focused {
        return;
    }

    let selection_style = theme.get(crate::theme::scope::UI_SELECTION_EDITOR);
    let cursor_style = theme.get(crate::theme::scope::UI_CURSOR);
    for selection in editor.selections.all_anchors() {
        let start_offset = buffer_snapshot.resolve_anchor(&selection.start);
        let end_offset = buffer_snapshot.resolve_anchor(&selection.end);
        let rope = buffer_snapshot.rope();
        let head_offset = buffer_snapshot.resolve_anchor(&selection.head());
        let tail_offset = buffer_snapshot.resolve_anchor(&selection.tail());
        let cursor = cursor_offset(rope, tail_offset, head_offset);

        if start_offset != end_offset {
            let mut offset = start_offset;
            let mut chars = rope.chars_at(offset);
            while offset < end_offset {
                let Some(ch) = chars.next() else {
                    break;
                };
                if ch != '\n' && offset != cursor {
                    let point = rope.offset_to_point(offset);
                    let display = snapshot.buffer_to_display(point, Bias::Left);
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

        let cursor_point = rope.offset_to_point(cursor);
        let display = snapshot.buffer_to_display(cursor_point, Bias::Left);
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

    if let Some(labels) = goto_word_labels {
        let label_style = fallback_style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        for (label, &offset) in labels {
            let rope = buffer_snapshot.rope();
            if offset > rope.len() {
                continue;
            }
            let point = rope.offset_to_point(offset);
            let display = snapshot.buffer_to_display(point, Bias::Left);
            if display.row < editor.scroll_row || display.row >= end_row {
                continue;
            }
            let y = inner.y + (display.row - editor.scroll_row) as u16;
            for (i, ch) in label.chars().enumerate() {
                let x = inner.x + display.column as u16 + i as u16;
                if x >= right || y >= bottom {
                    break;
                }
                buf[(x, y)].set_char(ch).set_style(label_style);
            }
        }
    }
}

/// Build a per-buffer-row map from `path`'s diagnostics, picking the
/// worst severity (lowest LSP code) when multiple diagnostics overlap
/// the same row.
fn compute_row_severity(
    set: &crate::diagnostics::DiagnosticSet,
    path: &Path,
) -> BTreeMap<u32, DiagnosticSeverity> {
    let mut out: BTreeMap<u32, DiagnosticSeverity> = BTreeMap::new();
    for diag in set.get(path) {
        let sev = diag.severity.unwrap_or(DiagnosticSeverity::ERROR);
        let start_line = diag.range.start.line;
        let end_line = diag.range.end.line;
        for row in start_line..=end_line {
            out.entry(row)
                .and_modify(|cur| {
                    if severity_rank(sev) < severity_rank(*cur) {
                        *cur = sev;
                    }
                })
                .or_insert(sev);
        }
    }
    out
}

fn severity_rank(sev: DiagnosticSeverity) -> u8 {
    match sev {
        DiagnosticSeverity::ERROR => 0,
        DiagnosticSeverity::WARNING => 1,
        DiagnosticSeverity::INFORMATION => 2,
        DiagnosticSeverity::HINT => 3,
        _ => 0,
    }
}

#[allow(clippy::too_many_arguments)]
fn paint_diagnostic_gutter(
    row_severity: &BTreeMap<u32, DiagnosticSeverity>,
    x: u16,
    y: u16,
    height: u16,
    scroll_row: u32,
    end_row: u32,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    for display_row in scroll_row..end_row {
        let row_offset = display_row.saturating_sub(scroll_row) as u16;
        if row_offset >= height {
            break;
        }
        let Some(sev) = row_severity.get(&display_row) else {
            continue;
        };
        let (glyph, scope) = match *sev {
            DiagnosticSeverity::ERROR => ('E', crate::theme::scope::UI_DIAGNOSTIC_ERROR),
            DiagnosticSeverity::WARNING => ('W', crate::theme::scope::UI_DIAGNOSTIC_WARNING),
            DiagnosticSeverity::INFORMATION => ('I', crate::theme::scope::UI_DIAGNOSTIC_INFO),
            DiagnosticSeverity::HINT => ('H', crate::theme::scope::UI_DIAGNOSTIC_HINT),
            _ => ('E', crate::theme::scope::UI_DIAGNOSTIC_ERROR),
        };
        let style = theme.get(scope);
        buf[(x, y + row_offset)].set_char(glyph).set_style(style);
    }
}

pub(crate) fn editor_cursor_position(editor: &mut EditorState) -> Option<(u32, u32)> {
    if editor.review_view.is_some() {
        return None;
    }
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    let rope = buffer_snapshot.rope();
    let head = buffer_snapshot.resolve_anchor(&sel.head());
    let tail = buffer_snapshot.resolve_anchor(&sel.tail());
    let point = rope.offset_to_point(cursor_offset(rope, tail, head));
    Some((point.row + 1, point.column + 1))
}

#[cfg(test)]
mod tests {
    use crate::{action_handlers::dispatch, Stoat};
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    use std::path::PathBuf;
    use stoat_action::OpenFile;

    fn diag(line: u32, severity: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position { line, character: 0 },
                end: Position { line, character: 1 },
            },
            severity: Some(severity),
            message: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn snapshot_diagnostic_gutter_renders_severity_glyphs() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-test");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"alpha\nbravo\ncharlie\ndelta\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.stoat.diagnostics.replace_for_path(
            path,
            vec![
                diag(0, DiagnosticSeverity::ERROR),
                diag(1, DiagnosticSeverity::WARNING),
                diag(2, DiagnosticSeverity::INFORMATION),
                diag(3, DiagnosticSeverity::HINT),
            ],
        );
        h.assert_snapshot("diagnostic_gutter_each_severity");
    }

    #[test]
    fn snapshot_diagnostic_gutter_worst_severity_per_row() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-worst");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.stoat.diagnostics.replace_for_path(
            path,
            vec![
                diag(0, DiagnosticSeverity::WARNING),
                diag(0, DiagnosticSeverity::ERROR),
            ],
        );
        h.assert_snapshot("diagnostic_gutter_worst_severity_wins");
    }
}
