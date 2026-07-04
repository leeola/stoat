use crate::{
    display_map::{tab_map, DisplayPoint, DisplaySnapshot},
    editor_state::{EditorState, SearchMatchCache},
    render::review::{render_review, style_rgb},
};
use lsp_types::DiagnosticSeverity;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::StatefulWidget,
};
use std::{collections::BTreeMap, ops::Range, path::Path};
use stoat_text::{cursor_offset, Point, Rope};
use stoatty_widgets::{bar::Bar, ApcScene};

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
        false,
        None,
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
    stoatty: bool,
    goto_word_labels: Option<&BTreeMap<String, usize>>,
    search_query: Option<&str>,
    diagnostic_info: Option<(&Path, &crate::diagnostics::DiagnosticSet)>,
    scene: Option<&mut ApcScene>,
) {
    editor.viewport_rows = Some(inner.height as u32);
    editor.cursor_screen_cell = None;

    if editor.review_view.is_some() {
        let scene = if stoatty { scene } else { None };
        render_review(editor, inner, fallback_style, theme, buf, scene);
        return;
    }

    let snapshot = editor.display_map.snapshot();
    let visible_rows = inner.height as u32;
    let total_rows = snapshot.line_count();
    let end_row = (editor.scroll_row + visible_rows).min(total_rows);
    if end_row <= editor.scroll_row {
        return;
    }

    let empty_severity = BTreeMap::new();
    let row_severity: &BTreeMap<u32, DiagnosticSeverity> = match diagnostic_info {
        Some((path, set)) => {
            let version = set.version();
            let stale = match &editor.gutter_severity_cache {
                Some(cache) => cache.version != version,
                None => true,
            };
            if stale {
                editor.gutter_severity_cache = Some(GutterSeverityCache {
                    version,
                    map: compute_row_severity(set, path),
                });
            }
            &editor
                .gutter_severity_cache
                .as_ref()
                .expect("set above")
                .map
        },
        None => &empty_severity,
    };
    let gutter_w: u16 = if row_severity.is_empty() { 0 } else { 1 };
    let inner = Rect {
        x: inner.x + gutter_w,
        y: inner.y,
        width: inner.width.saturating_sub(gutter_w),
        height: inner.height,
    };
    if gutter_w > 0 {
        let gutter_x = inner.x - gutter_w;
        // Rich mode emits a sub-cell severity bar per row instead of the glyph,
        // engaging only inside stoatty with every severity color resolved to RGB.
        let rich = scene.filter(|_| stoatty).zip(severity_colors(theme));
        match rich {
            Some((scene, colors)) => {
                let area = Rect {
                    x: gutter_x,
                    y: inner.y,
                    width: gutter_w,
                    height: inner.height,
                };
                for display_row in editor.scroll_row..end_row {
                    let row_offset = (display_row - editor.scroll_row) as u16;
                    if row_offset >= inner.height {
                        break;
                    }
                    let Some(sev) = row_severity.get(&display_row) else {
                        continue;
                    };
                    Bar {
                        x: 0,
                        y: row_offset * 16,
                        width: 6,
                        height: 16,
                        color: severity_color(*sev, &colors),
                    }
                    .render(area, buf, &mut *scene);
                }
            },
            None => paint_diagnostic_gutter(
                row_severity,
                gutter_x,
                inner.y,
                inner.height,
                editor.scroll_row,
                end_row,
                theme,
                buf,
            ),
        }
    }
    // Record the inset so click-to-offset can subtract the same shift the text
    // rect took above. Written after the `row_severity` borrow of `editor` ends.
    editor.gutter_width = gutter_w;

    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;

    {
        let mut x = inner.x;
        let mut y = inner.y;
        'chunks: for chunk in snapshot.highlighted_chunks_cached(
            editor.scroll_row..end_row,
            &mut editor.highlight_endpoint_cache,
        ) {
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

    if let Some((path, set)) = diagnostic_info {
        paint_diagnostic_spans(
            set,
            path,
            buffer_snapshot.rope(),
            &snapshot,
            theme,
            editor.scroll_row,
            end_row,
            inner,
            right,
            bottom,
            buf,
        );
    }

    if let Some(query) = search_query.filter(|q| !q.is_empty()) {
        let version = buffer_snapshot.version();
        let rope = buffer_snapshot.rope();
        let visible = visible_byte_range(&snapshot, rope, editor.scroll_row, end_row);
        let stale = match &editor.search_match_cache {
            Some(cache) => {
                cache.version != version || cache.query != query || cache.visible != visible
            },
            None => true,
        };
        if stale {
            let mut window = editor
                .search_match_cache
                .take()
                .map(|c| c.window)
                .unwrap_or_default();
            window.clear();
            for chunk in rope.chunks_in_range(visible.clone()) {
                window.push_str(chunk);
            }
            let matches = match crate::action_handlers::search::compile_search_regex(query) {
                Ok(regex) => regex
                    .find_iter(&window)
                    .filter(|m| m.end() > m.start())
                    .map(|m| (m.start() + visible.start, m.end() + visible.start))
                    .collect(),
                Err(_) => Vec::new(),
            };
            editor.search_match_cache = Some(SearchMatchCache {
                version,
                query: query.to_string(),
                visible: visible.clone(),
                matches,
                window,
            });
        }

        let match_style = theme.get(crate::theme::scope::UI_SEARCH_MATCH);
        let cache = editor.search_match_cache.as_ref().expect("set above");
        for &(match_start, match_end) in &cache.matches {
            paint_offset_range(
                rope,
                &snapshot,
                match_start..match_end,
                None,
                match_style,
                editor.scroll_row,
                end_row,
                inner,
                right,
                bottom,
                buf,
            );
        }
    }

    if !is_focused {
        return;
    }

    let selection_style = theme.get(crate::theme::scope::UI_SELECTION_EDITOR);
    let cursor_style = theme.get(crate::theme::scope::UI_CURSOR);
    let primary_id = editor.selections.newest_anchor().id;
    let mut primary_cell: Option<(u16, u16)> = None;
    let rope = buffer_snapshot.rope();
    let visible = visible_byte_range(&snapshot, rope, editor.scroll_row, end_row);
    for selection in editor.selections.all_anchors() {
        let start_offset = buffer_snapshot.resolve_anchor(&selection.start);
        let end_offset = buffer_snapshot.resolve_anchor(&selection.end);
        let head_offset = buffer_snapshot.resolve_anchor(&selection.head());
        let cursor = cursor_offset(
            rope,
            buffer_snapshot.resolve_anchor(&selection.tail()),
            head_offset,
        );

        let lo = start_offset.max(visible.start);
        let hi = end_offset.min(visible.end);
        if lo < hi {
            paint_offset_range(
                rope,
                &snapshot,
                lo..hi,
                Some(cursor),
                selection_style,
                editor.scroll_row,
                end_row,
                inner,
                right,
                bottom,
                buf,
            );
        }

        let cursor_point = rope.offset_to_point(cursor);
        let display = snapshot.buffer_to_display(cursor_point);
        if display.row >= editor.scroll_row && display.row < end_row {
            let y = inner.y + (display.row - editor.scroll_row) as u16;
            let x = inner.x + display.column as u16;
            if x < right && y < bottom {
                if stoatty && selection.id == primary_id {
                    primary_cell = Some((x, y));
                } else {
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

    editor.cursor_screen_cell = primary_cell;

    if let Some((path, set)) = diagnostic_info {
        let cursor = buffer_snapshot.resolve_anchor(&editor.selections.newest_anchor().head());
        paint_cursor_line_diagnostic(
            set,
            path,
            rope,
            &snapshot,
            cursor,
            theme,
            editor.scroll_row,
            end_row,
            inner,
            right,
            buf,
        );
    }

    if let Some(labels) = goto_word_labels {
        let label_style = fallback_style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
        for (label, &offset) in labels {
            let rope = buffer_snapshot.rope();
            if offset > rope.len() {
                continue;
            }
            let point = rope.offset_to_point(offset);
            let display = snapshot.buffer_to_display(point);
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

/// Cached gutter severity map for one diagnostic-set version.
///
/// `map` is the per-buffer-row worst severity. Recomputed only when the
/// diagnostic set's version changes, so the gutter is not rebuilt from the
/// full diagnostic list every frame.
pub(crate) struct GutterSeverityCache {
    version: u64,
    map: BTreeMap<u32, DiagnosticSeverity>,
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

fn severity_scope(sev: DiagnosticSeverity) -> &'static str {
    use crate::theme::scope as s;
    match sev {
        DiagnosticSeverity::ERROR => s::UI_DIAGNOSTIC_ERROR,
        DiagnosticSeverity::WARNING => s::UI_DIAGNOSTIC_WARNING,
        DiagnosticSeverity::INFORMATION => s::UI_DIAGNOSTIC_INFO,
        DiagnosticSeverity::HINT => s::UI_DIAGNOSTIC_HINT,
        _ => s::UI_DIAGNOSTIC_ERROR,
    }
}

struct SeverityColors {
    error: [u8; 3],
    warning: [u8; 3],
    info: [u8; 3],
    hint: [u8; 3],
}

/// Extract every diagnostic-severity color as RGB, or `None` if any is missing
/// or not an RGB color. A `None` here disables the sub-cell gutter for the whole
/// frame, so it falls back to the ASCII glyphs rather than mixing the two.
fn severity_colors(theme: &crate::theme::Theme) -> Option<SeverityColors> {
    use crate::theme::scope as s;
    Some(SeverityColors {
        error: style_rgb(theme.get(s::UI_DIAGNOSTIC_ERROR).fg)?,
        warning: style_rgb(theme.get(s::UI_DIAGNOSTIC_WARNING).fg)?,
        info: style_rgb(theme.get(s::UI_DIAGNOSTIC_INFO).fg)?,
        hint: style_rgb(theme.get(s::UI_DIAGNOSTIC_HINT).fg)?,
    })
}

fn severity_color(sev: DiagnosticSeverity, colors: &SeverityColors) -> [u8; 3] {
    match sev {
        DiagnosticSeverity::ERROR => colors.error,
        DiagnosticSeverity::WARNING => colors.warning,
        DiagnosticSeverity::INFORMATION => colors.info,
        DiagnosticSeverity::HINT => colors.hint,
        _ => colors.error,
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
        let glyph = match *sev {
            DiagnosticSeverity::ERROR => 'E',
            DiagnosticSeverity::WARNING => 'W',
            DiagnosticSeverity::INFORMATION => 'I',
            DiagnosticSeverity::HINT => 'H',
            _ => 'E',
        };
        let style = theme.get(severity_scope(*sev));
        buf[(x, y + row_offset)].set_char(glyph).set_style(style);
    }
}

/// Underline every visible diagnostic's text span in its severity color.
///
/// Each diagnostic range is resolved from LSP line/character positions to buffer
/// byte offsets and painted through [`paint_offset_range`], which merges the
/// style so the underlined span keeps its syntax background. Empty ranges paint
/// nothing.
#[allow(clippy::too_many_arguments)]
fn paint_diagnostic_spans(
    set: &crate::diagnostics::DiagnosticSet,
    path: &Path,
    rope: &Rope,
    snapshot: &DisplaySnapshot,
    theme: &crate::theme::Theme,
    scroll_row: u32,
    end_row: u32,
    inner: Rect,
    right: u16,
    bottom: u16,
    buf: &mut Buffer,
) {
    let rope_len = rope.len();
    for diag in set.get(path) {
        let sev = diag.severity.unwrap_or(DiagnosticSeverity::ERROR);
        let start = rope
            .point_to_offset(Point::new(
                diag.range.start.line,
                diag.range.start.character,
            ))
            .min(rope_len);
        let end = rope
            .point_to_offset(Point::new(diag.range.end.line, diag.range.end.character))
            .min(rope_len);
        if start >= end {
            continue;
        }
        let style = theme
            .get(severity_scope(sev))
            .add_modifier(Modifier::UNDERLINED);
        paint_offset_range(
            rope,
            snapshot,
            start..end,
            None,
            style,
            scroll_row,
            end_row,
            inner,
            right,
            bottom,
            buf,
        );
    }
}

/// Paint the highest-severity diagnostic covering the primary cursor's line as
/// an end-of-line message, dimmed in the severity color.
///
/// The message is the first line of the winning diagnostic, started two columns
/// past the row's content and clipped to the pane's right edge. A no-op when the
/// cursor row is scrolled off, no diagnostic covers it, or the message is empty.
#[allow(clippy::too_many_arguments)]
fn paint_cursor_line_diagnostic(
    set: &crate::diagnostics::DiagnosticSet,
    path: &Path,
    rope: &Rope,
    snapshot: &DisplaySnapshot,
    cursor: usize,
    theme: &crate::theme::Theme,
    scroll_row: u32,
    end_row: u32,
    inner: Rect,
    right: u16,
    buf: &mut Buffer,
) {
    let cursor_point = rope.offset_to_point(cursor);
    let display = snapshot.buffer_to_display(cursor_point);
    if display.row < scroll_row || display.row >= end_row {
        return;
    }

    let cursor_line = cursor_point.row;
    let Some(diag) = set
        .get(path)
        .iter()
        .filter(|d| d.range.start.line <= cursor_line && cursor_line <= d.range.end.line)
        .min_by_key(|d| severity_rank(d.severity.unwrap_or(DiagnosticSeverity::ERROR)))
    else {
        return;
    };

    let message = diag.message.lines().next().unwrap_or("");
    if message.is_empty() {
        return;
    }

    let sev = diag.severity.unwrap_or(DiagnosticSeverity::ERROR);
    let style = theme.get(severity_scope(sev)).add_modifier(Modifier::DIM);
    let y = inner.y + (display.row - scroll_row) as u16;
    let base_x = inner.x as u32 + snapshot.line_len(display.row) + 2;
    for (i, ch) in message.chars().enumerate() {
        let x = base_x + i as u32;
        if x >= right as u32 {
            break;
        }
        buf[(x as u16, y)].set_char(ch).set_style(style);
    }
}

/// Paint `style` over every character cell in the buffer byte range `range`,
/// skipping newlines and `skip_offset` when it is set.
///
/// `skip_offset` is the cursor offset during selection painting, which the
/// caller renders separately. Search-match painting passes `None`.
///
/// The display anchor is resolved once per buffer-row segment via
/// [`DisplaySnapshot::buffer_to_display`]. On a row with no folds, inlays, or
/// soft wrap the display column is the tab-expanded buffer column, so the
/// segment advances one cell at a time through
/// [`tab_map::advance_column_for_char`] instead of re-resolving each character.
/// Re-resolving walks the whole row prefix, so the per-character path is
/// quadratic in the row length. It is kept only for rows carrying folds,
/// inlays, or soft wrap, where the display column is not a simple accumulation.
#[allow(clippy::too_many_arguments)]
fn paint_offset_range(
    rope: &Rope,
    snapshot: &DisplaySnapshot,
    range: Range<usize>,
    skip_offset: Option<usize>,
    style: Style,
    scroll_row: u32,
    end_row: u32,
    inner: Rect,
    right: u16,
    bottom: u16,
    buf: &mut Buffer,
) {
    let map_simple =
        snapshot.fold_snapshot().fold_count() == 0 && !snapshot.inlay_snapshot().has_inlays();
    let tab_size = snapshot.tab_snapshot().tab_size();
    let max_expansion_column = snapshot.tab_snapshot().max_expansion_column();
    let line_count = snapshot.line_count();

    let mut paint = |display_row: u32, display_col: u32| {
        if display_row < scroll_row || display_row >= end_row {
            return;
        }
        let y = inner.y + (display_row - scroll_row) as u16;
        let x = inner.x + display_col as u16;
        if x < right && y < bottom {
            buf[(x, y)].set_style(style);
        }
    };

    let mut offset = range.start;
    let mut chars = rope.chars_at(offset);

    'segments: while offset < range.end {
        let display = snapshot.buffer_to_display(rope.offset_to_point(offset));
        let single_display_row = !snapshot.is_wrap_continuation(display.row)
            && (display.row + 1 >= line_count || !snapshot.is_wrap_continuation(display.row + 1));

        if map_simple && single_display_row {
            let row = display.row;
            let mut col = display.column;
            loop {
                if offset >= range.end {
                    break 'segments;
                }
                let Some(ch) = chars.next() else {
                    break 'segments;
                };
                if ch == '\n' {
                    offset += 1;
                    continue 'segments;
                }
                if Some(offset) != skip_offset {
                    paint(row, col);
                }
                tab_map::advance_column_for_char(&mut col, ch, tab_size, max_expansion_column);
                offset += ch.len_utf8();
            }
        } else {
            loop {
                if offset >= range.end {
                    break 'segments;
                }
                let Some(ch) = chars.next() else {
                    break 'segments;
                };
                if ch == '\n' {
                    offset += 1;
                    continue 'segments;
                }
                if Some(offset) != skip_offset {
                    let display = snapshot.buffer_to_display(rope.offset_to_point(offset));
                    paint(display.row, display.column);
                }
                offset += ch.len_utf8();
            }
        }
    }
}

/// Byte range of `rope` spanned by display rows `scroll_row..end_row`.
///
/// Rows beyond the buffer resolve to the rope length, so the returned range is
/// always valid to slice.
fn visible_byte_range(
    snapshot: &DisplaySnapshot,
    rope: &Rope,
    scroll_row: u32,
    end_row: u32,
) -> Range<usize> {
    let rope_len = rope.len();
    let row_offset = |row: u32| {
        snapshot
            .display_to_buffer(DisplayPoint::new(row, 0))
            .map(|point| rope.point_to_offset(point))
            .unwrap_or(rope_len)
            .min(rope_len)
    };
    row_offset(scroll_row)..row_offset(end_row)
}

pub(crate) fn editor_cursor_position(editor: &mut EditorState) -> Option<(u32, u32)> {
    if editor.review_view.is_some() {
        return None;
    }
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    let rope = buffer_snapshot.rope();
    let cursor = cursor_offset(
        rope,
        buffer_snapshot.resolve_anchor(&sel.tail()),
        buffer_snapshot.resolve_anchor(&sel.head()),
    );
    let point = rope.offset_to_point(cursor);
    Some((point.row + 1, point.column + 1))
}

#[cfg(test)]
mod tests {
    use crate::{
        action_handlers::{self, dispatch},
        Stoat,
    };
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    use std::path::PathBuf;
    use stoat_action::{ExtendToLineEnd, MoveRight, OpenFile, OpenFileFinder};
    use stoat_text::{Bias, Point, SelectionGoal};

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

    #[test]
    fn snapshot_diagnostic_inline_underline_span() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-inline");
        let path = root.join("a.rs");
        h.fake_fs()
            .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        // The diagnostic sits on line 1 while the cursor stays on line 0, so
        // only the span is underlined and no end-of-line message appears.
        h.stoat.diagnostics.replace_for_path(
            path,
            vec![Diagnostic {
                range: Range {
                    start: Position {
                        line: 1,
                        character: 4,
                    },
                    end: Position {
                        line: 1,
                        character: 5,
                    },
                },
                severity: Some(DiagnosticSeverity::WARNING),
                message: "unused variable".into(),
                ..Default::default()
            }],
        );
        h.assert_snapshot("diagnostic_inline_underline_span");
    }

    #[test]
    fn snapshot_diagnostic_cursor_line_eol_message() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-eol");
        let path = root.join("a.rs");
        h.fake_fs().insert_file(&path, b"let x = 1;\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        // The cursor opens on line 0. The diagnostic underlines its span, and
        // its message trails the line content, dimmed in the severity color.
        h.stoat.diagnostics.replace_for_path(
            path,
            vec![Diagnostic {
                range: Range {
                    start: Position {
                        line: 0,
                        character: 4,
                    },
                    end: Position {
                        line: 0,
                        character: 5,
                    },
                },
                severity: Some(DiagnosticSeverity::ERROR),
                message: "mismatched types".into(),
                ..Default::default()
            }],
        );
        h.assert_snapshot("diagnostic_cursor_line_eol_message");
    }

    fn add_cursor_at(stoat: &mut Stoat, offset: usize) {
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buffer_snapshot = snapshot.buffer_snapshot();
        let anchor = buffer_snapshot.anchor_at(offset, Bias::Left);
        editor
            .selections
            .insert_cursor(anchor, SelectionGoal::None, buffer_snapshot);
    }

    #[test]
    fn snapshot_stoatty_delegates_only_primary_cursor() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/cursor-stoatty");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha bravo charlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        add_cursor_at(&mut h.stoat, 6);
        h.stoat.stoatty = true;

        h.assert_snapshot("stoatty_primary_cursor_delegated");
    }

    #[test]
    fn primary_cursor_screen_pos_matches_painted_cell() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/cursor-pos");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha bravo\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        h.stoat.stoatty = true;
        h.snapshot();
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((0, 0)));

        for _ in 0..6 {
            dispatch(&mut h.stoat, &MoveRight);
        }
        h.snapshot();
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((6, 0)));

        h.stoat.stoatty = false;
        h.snapshot();
        assert_eq!(h.stoat.primary_cursor_screen_pos(), None);
    }

    #[test]
    fn primary_cursor_screen_pos_none_when_finder_open() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/cursor-finder");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        h.stoat.stoatty = true;
        h.snapshot();
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((0, 0)));

        dispatch(&mut h.stoat, &OpenFileFinder);
        h.settle();
        h.snapshot();
        assert_eq!(h.stoat.primary_cursor_screen_pos(), None);
    }

    #[test]
    fn snapshot_selection_over_tab_line() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 4);
        let path = h.write_file("s.txt", "ab\tcd\n");
        h.open_file(&path);
        dispatch(&mut h.stoat, &ExtendToLineEnd);
        h.assert_snapshot("selection_over_tab_line");
    }

    #[test]
    fn snapshot_selection_over_wide_chars() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 4);
        let path = h.write_file("s.txt", "a世z\n");
        h.open_file(&path);
        dispatch(&mut h.stoat, &ExtendToLineEnd);
        // The text pass advances one terminal cell per glyph, so glyphs after a
        // wide char diverge from the selection/cursor columns, which do account
        // for display width. This locks that width-aware column math.
        h.assert_snapshot("selection_over_wide_chars");
    }

    #[test]
    fn snapshot_selection_spanning_fold() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 4);
        let path = h.write_file("s.txt", "abcdefgh\nij\n");
        h.open_file(&path);
        h.settle();
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            editor
                .display_map
                .fold(vec![Point::new(0, 2)..Point::new(0, 6)]);
        }
        dispatch(&mut h.stoat, &ExtendToLineEnd);
        h.assert_snapshot("selection_spanning_fold");
    }
}
