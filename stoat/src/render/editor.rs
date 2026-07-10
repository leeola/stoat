use crate::{
    display_map::{tab_map, BlockRowKind, DisplayPoint, DisplaySnapshot},
    editor_state::{EditorState, SearchMatchCache},
    render::{
        review::{render_review, style_rgb},
        undercurl::UndercurlSpan,
    },
};
use lsp_types::DiagnosticSeverity;
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::StatefulWidget,
};
use std::{cmp::Reverse, collections::BTreeMap, ops::Range, path::Path};
use stoat_text::{cursor_offset, Bias, Point, Rope};
use stoatty_protocol::command::IconKind;
use stoatty_widgets::{
    bar::Bar,
    gutter::{Diagnostic, Gutter, GutterLine},
    icon::Icon,
    popover::Popover,
    ApcScene,
};

/// Line-number glyph size in 256ths of a cell, so numbers read smaller than the
/// body text.
const NUMBER_SCALE: u16 = 160;

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
        false,
        None,
        None,
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
    line_numbers: bool,
    hover_cell: Option<(u16, u16)>,
    goto_word_labels: Option<&BTreeMap<String, usize>>,
    search_query: Option<&str>,
    diagnostic_info: Option<(&Path, &crate::diagnostics::DiagnosticSet)>,
    mut scene: Option<&mut ApcScene>,
    undercurls: Option<&mut Vec<UndercurlSpan>>,
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
    let severity = severity_colors(theme);
    // The pane content area before the gutter inset below, used to resolve a
    // mouse hover cell back to a buffer offset for the diagnostic popover.
    let content_area = inner;
    let gutter_w = if line_numbers {
        draw_line_number_gutter(
            &snapshot,
            editor.scroll_row,
            inner,
            end_row,
            row_severity,
            severity.as_ref(),
            fallback_style,
            theme,
            stoatty,
            scene.as_deref_mut(),
            buf,
        )
    } else if row_severity.is_empty() {
        0
    } else {
        // Rich mode emits a sub-cell severity bar per row instead of the glyph,
        // engaging only inside stoatty with every severity color resolved to RGB.
        let rich = scene
            .as_deref_mut()
            .filter(|_| stoatty)
            .zip(severity.as_ref());
        match rich {
            Some((scene, colors)) => {
                let area = Rect {
                    x: inner.x,
                    y: inner.y,
                    width: 1,
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
                        color: severity_color(*sev, colors),
                    }
                    .render(area, buf, &mut *scene);
                }
            },
            None => paint_diagnostic_gutter(
                row_severity,
                inner.x,
                inner.y,
                inner.height,
                editor.scroll_row,
                end_row,
                theme,
                buf,
            ),
        }
        1
    };

    // Inset the text rect by the gutter, and record the width so click-to-offset
    // subtracts the same shift. Written after the `row_severity` borrow ends.
    let inner = Rect {
        x: inner.x + gutter_w,
        y: inner.y,
        width: inner.width.saturating_sub(gutter_w),
        height: inner.height,
    };
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
            if stoatty { undercurls } else { None },
            severity.as_ref(),
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
                None,
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
                None,
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
        let sel = editor.selections.newest_anchor();
        let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
        let head_off = buffer_snapshot.resolve_anchor(&sel.head());
        let cursor = cursor_offset(rope, tail_off, head_off);
        let cursor_diag = diagnostic_at_offset(set, path, rope, cursor);
        let hover_diag = hover_cell.and_then(|(hx, hy)| {
            let col = hx.checked_sub(content_area.x)?;
            let row = hy.checked_sub(content_area.y)?;
            if col >= content_area.width || row >= content_area.height {
                return None;
            }
            let offset = display_cell_to_offset(&snapshot, editor.scroll_row, gutter_w, col, row)?;
            diagnostic_at_offset(set, path, rope, offset)
        });

        // The mouse hover wins over the cursor when both land in a span. The
        // popover renders only inside stoatty with the severity and background
        // colors resolved to RGB, and its presence suppresses the same
        // diagnostic's redundant EOL message.
        let mut suppress = None;
        if let (Some(index), true) = (hover_diag.or(cursor_diag), stoatty) {
            let bg = style_rgb(fallback_style.bg.or_else(|| {
                theme
                    .try_get(crate::theme::scope::UI_BACKGROUND)
                    .and_then(|style| style.bg)
            }));
            if let (Some(scene), Some(colors), Some(bg)) = (scene, severity.as_ref(), bg) {
                let diag = &set.get(path)[index];
                let sev = diag.severity.unwrap_or(DiagnosticSeverity::ERROR);
                let start = rope.point_to_offset(Point::new(
                    diag.range.start.line,
                    diag.range.start.character,
                ));
                let display = snapshot.buffer_to_display(rope.offset_to_point(start));
                let rel_col = display.column.min(u32::from(content_area.width)) as u16;
                let rel_row = display
                    .row
                    .saturating_sub(editor.scroll_row)
                    .min(u32::from(content_area.height)) as u16;
                let anchor_col = content_area
                    .x
                    .saturating_add(gutter_w)
                    .saturating_add(rel_col);
                let anchor_row = content_area.y.saturating_add(rel_row);
                if render_diagnostic_popover(
                    scene,
                    buf,
                    diag,
                    severity_color(sev, colors),
                    darken(bg),
                    anchor_col,
                    anchor_row,
                    content_area,
                ) {
                    suppress = Some(index);
                }
            }
        }

        paint_cursor_line_diagnostic(
            set,
            path,
            rope,
            &snapshot,
            cursor,
            suppress,
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
    pub(crate) version: u64,
    pub(crate) map: BTreeMap<u32, DiagnosticSeverity>,
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

#[derive(Clone)]
pub(crate) struct SeverityColors {
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

/// The resolved colors the rich sub-cell page gutter needs.
#[derive(Clone)]
pub(crate) struct RichGutterColors {
    pub(crate) colors: SeverityColors,
    pub(crate) number_fg: [u8; 3],
    pub(crate) bg: [u8; 3],
}

/// Resolve the rich page-gutter colors, or `None` outside stoatty or when a
/// gutter color is not RGB.
///
/// Mirrors the live gutter's rich gate so an off-run-loop page render and the
/// live render agree on rich versus fallback for the same theme.
pub(crate) fn resolve_rich_gutter(
    theme: &crate::theme::Theme,
    fallback_style: Style,
    stoatty: bool,
) -> Option<RichGutterColors> {
    use crate::theme::scope as s;
    if !stoatty {
        return None;
    }
    let colors = severity_colors(theme)?;
    let number_fg = style_rgb(theme.get(s::UI_TEXT_MUTED).fg)?;
    let bg = style_rgb(
        fallback_style
            .bg
            .or_else(|| theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg)),
    )?;
    Some(RichGutterColors {
        colors,
        number_fg,
        bg,
    })
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
        let style = theme.get(severity_scope(*sev));
        buf[(x, y + row_offset)]
            .set_char(severity_mark(*sev))
            .set_style(style);
    }
}

/// The single-letter severity mark drawn in the cell-fallback gutter.
fn severity_mark(sev: DiagnosticSeverity) -> char {
    match sev {
        DiagnosticSeverity::ERROR => 'E',
        DiagnosticSeverity::WARNING => 'W',
        DiagnosticSeverity::INFORMATION => 'I',
        DiagnosticSeverity::HINT => 'H',
        _ => 'E',
    }
}

/// One display row's role when folding the gutter: the first row of a buffer
/// line, or a soft-wrap or block row belonging to the line above it.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum RowKind {
    LineStart(u32),
    Continuation,
}

pub(crate) fn row_kind(snapshot: &DisplaySnapshot, display_row: u32) -> RowKind {
    if snapshot.is_wrap_continuation(display_row) {
        return RowKind::Continuation;
    }
    match snapshot.classify_row(display_row) {
        BlockRowKind::BufferRow { buffer_row } => RowKind::LineStart(buffer_row),
        BlockRowKind::Block { .. } => RowKind::Continuation,
    }
}

/// Fold per-display-row classifications into one gutter entry per logical line,
/// as `(line_number, height)`.
///
/// Each `LineStart(buffer_row)` opens an entry numbered `buffer_row + 1`;
/// `Continuation` rows (soft wraps and blocks) extend the current entry's
/// height, so the number sits at the top and a severity bar spans the whole
/// line. Continuations before the first `LineStart` -- a viewport opening
/// mid-line or on a block row -- attach to `lead_number`, the buffer line they
/// belong to.
pub(crate) fn fold_gutter_lines(rows: &[RowKind], lead_number: u32) -> Vec<(u32, u16)> {
    let mut out: Vec<(u32, u16)> = Vec::new();
    for kind in rows {
        match kind {
            RowKind::LineStart(buffer_row) => out.push((buffer_row + 1, 1)),
            RowKind::Continuation => match out.last_mut() {
                Some(last) => last.1 += 1,
                None => out.push((lead_number, 1)),
            },
        }
    }
    out
}

/// Decimal digit count of `n`, at least 1.
fn decimal_digits(mut n: u32) -> u16 {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

/// The folded gutter lines and digit width for `visible` display rows from
/// `scroll_row`.
///
/// Shared by the live gutter and the pooled-page gutter so both number and fold
/// wrap and block rows identically, keeping the settle handoff pixel-identical.
pub(crate) fn gutter_geometry(
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    visible: u32,
) -> (Vec<(u32, u16)>, u16) {
    let rows: Vec<RowKind> = (scroll_row..scroll_row + visible)
        .map(|display_row| row_kind(snapshot, display_row))
        .collect();
    let lead_number = snapshot
        .display_to_buffer(DisplayPoint::new(scroll_row, 0))
        .map(|point| point.row + 1)
        .unwrap_or(1);
    let folded = fold_gutter_lines(&rows, lead_number);
    let width_digits = decimal_digits(snapshot.buffer_line_count()).max(2);
    (folded, width_digits)
}

/// Build the rich gutter's [`GutterLine`]s from `folded`, coloring each line's
/// diagnostic mark from `colors`.
pub(crate) fn gutter_component_lines(
    folded: &[(u32, u16)],
    row_severity: &BTreeMap<u32, DiagnosticSeverity>,
    colors: &SeverityColors,
) -> Vec<GutterLine> {
    folded
        .iter()
        .map(|&(number, height)| GutterLine {
            number,
            height,
            git: None,
            diagnostic: row_severity.get(&(number - 1)).map(|sev| Diagnostic {
                color: severity_color(*sev, colors),
                mark: severity_mark(*sev),
            }),
        })
        .collect()
}

/// The sub-cell [`Gutter`] widget for `lines`, carrying the geometry the live
/// and pooled-page renders share.
pub(crate) fn rich_gutter(
    lines: &[GutterLine],
    width_digits: u16,
    number_fg: [u8; 3],
    bg: [u8; 3],
) -> Gutter<'_> {
    Gutter {
        lines,
        bar_width: 5,
        pad: 2,
        number_scale: NUMBER_SCALE,
        width_digits,
        number_fg,
        separator: number_fg,
        bg,
    }
}

/// Draw the absolute-line-number gutter and return the cell columns it reserves.
///
/// Inside stoatty with every gutter color resolved to RGB, draws the rich
/// sub-cell gutter (scaled numbers, severity bars, hairline separator). Any
/// other terminal, or a theme whose colors are not RGB, gets right-aligned cell
/// numbers and a one-column severity mark styled from the theme, so the numbers
/// still show.
#[allow(clippy::too_many_arguments)]
fn draw_line_number_gutter(
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    inner: Rect,
    end_row: u32,
    row_severity: &BTreeMap<u32, DiagnosticSeverity>,
    severity: Option<&SeverityColors>,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    stoatty: bool,
    scene: Option<&mut ApcScene>,
    buf: &mut Buffer,
) -> u16 {
    use crate::theme::scope as s;

    let visible = end_row.saturating_sub(scroll_row).min(inner.height as u32);
    let (folded, width_digits) = gutter_geometry(snapshot, scroll_row, visible);

    // Rich mode needs stoatty, a scene, and every gutter color as RGB.
    let rich = scene.filter(|_| stoatty).and_then(|scene| {
        let colors = severity?;
        let number_fg = style_rgb(theme.get(s::UI_TEXT_MUTED).fg)?;
        let bg = style_rgb(
            fallback_style
                .bg
                .or_else(|| theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg)),
        )?;
        Some((scene, colors, number_fg, bg))
    });

    match rich {
        Some((scene, colors, number_fg, bg)) => {
            let lines = gutter_component_lines(&folded, row_severity, colors);
            let gutter = rich_gutter(&lines, width_digits, number_fg, bg);
            gutter.draw_components(inner, buf, scene);
            gutter.cell_width()
        },
        None => draw_fallback_line_numbers(&folded, width_digits, row_severity, inner, theme, buf),
    }
}

/// Paint right-aligned cell line numbers and a one-column severity mark for a
/// terminal without the sub-cell components. Returns the reserved cell columns.
pub(crate) fn draw_fallback_line_numbers(
    folded: &[(u32, u16)],
    width_digits: u16,
    row_severity: &BTreeMap<u32, DiagnosticSeverity>,
    inner: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) -> u16 {
    let mark_w = 1u16;
    let gap = 1u16;
    let width = mark_w + width_digits + gap;
    let number_style = theme.get(crate::theme::scope::UI_TEXT_MUTED);

    let mut top = 0u16;
    for &(number, height) in folded {
        let y = inner.y + top;
        if y >= inner.y + inner.height {
            break;
        }
        if let Some(sev) = row_severity.get(&(number - 1)) {
            buf[(inner.x, y)]
                .set_char(severity_mark(*sev))
                .set_style(theme.get(severity_scope(*sev)));
        }
        let text = format!("{number}");
        let start = inner.x + mark_w + width_digits.saturating_sub(text.len() as u16);
        buf.set_stringn(start, y, &text, text.len(), number_style);
        top += height;
    }
    width
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
    mut undercurls: Option<&mut Vec<UndercurlSpan>>,
    colors: Option<&SeverityColors>,
) {
    let rope_len = rope.len();
    // Paint least-severe first so the worst severity lands last, on top, for
    // both the cell foreground and the collected undercurl spans. rust-analyzer
    // can publish overlapping diagnostics (a WARNING and a HINT over `unused`)
    // in any order, and publish order alone would let the hint's grey win.
    let mut ordered: Vec<_> = set.get(path).iter().collect();
    ordered.sort_by_key(|d| {
        Reverse(severity_rank(
            d.severity.unwrap_or(DiagnosticSeverity::ERROR),
        ))
    });
    for diag in ordered {
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

        // Collect the painted runs only when the stoatty undercurl overlay is
        // live, then re-stamp each as a severity-colored curl span.
        let mut runs: Vec<(u16, u16, u16)> = Vec::new();
        let collect = undercurls.is_some() && colors.is_some();
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
            collect.then_some(&mut runs),
        );
        if let (Some(undercurls), Some(colors)) = (undercurls.as_deref_mut(), colors) {
            let color = severity_color(sev, colors);
            undercurls.extend(runs.into_iter().map(|(x, y, len)| UndercurlSpan {
                x,
                y,
                len,
                color,
                cells: Vec::new(),
            }));
        }
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
    suppress: Option<usize>,
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
    let Some((index, diag)) = set
        .get(path)
        .iter()
        .enumerate()
        .filter(|(_, d)| d.range.start.line <= cursor_line && cursor_line <= d.range.end.line)
        .min_by_key(|(_, d)| severity_rank(d.severity.unwrap_or(DiagnosticSeverity::ERROR)))
    else {
        return;
    };
    // The popover already shows this diagnostic, so skip the redundant EOL text.
    if Some(index) == suppress {
        return;
    }

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

/// Byte offset of the buffer position under the pane-content cell `(col, row)`,
/// or `None` when it maps to no buffer point.
///
/// `col`/`row` are relative to the pane's content area. `gutter_width` is the
/// column inset the gutter shifted the text by, subtracted so a cell over the
/// glyph resolves to that glyph. This is the shared screen-to-offset math both
/// mouse clicks and the diagnostic popover resolve through.
pub(crate) fn display_cell_to_offset(
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    gutter_width: u16,
    col: u16,
    row: u16,
) -> Option<usize> {
    let display_row = scroll_row + row as u32;
    let display_col = (col as u32).saturating_sub(gutter_width as u32);
    let clipped = snapshot.clip_point(DisplayPoint::new(display_row, display_col), Bias::Left);
    let buffer_pt = snapshot.display_to_buffer(clipped)?;
    Some(snapshot.buffer_snapshot().rope().point_to_offset(buffer_pt))
}

/// Index into `set.get(path)` of the highest-severity diagnostic whose byte
/// range contains `offset`, or `None` when none do.
///
/// The worst severity wins a tie, matching the gutter and the EOL message.
pub(crate) fn diagnostic_at_offset(
    set: &crate::diagnostics::DiagnosticSet,
    path: &Path,
    rope: &Rope,
    offset: usize,
) -> Option<usize> {
    let rope_len = rope.len();
    set.get(path)
        .iter()
        .enumerate()
        .filter(|(_, diag)| {
            let start = rope
                .point_to_offset(Point::new(
                    diag.range.start.line,
                    diag.range.start.character,
                ))
                .min(rope_len);
            let end = rope
                .point_to_offset(Point::new(diag.range.end.line, diag.range.end.character))
                .min(rope_len);
            start < end && start <= offset && offset < end
        })
        .min_by_key(|(_, diag)| severity_rank(diag.severity.unwrap_or(DiagnosticSeverity::ERROR)))
        .map(|(index, _)| index)
}

/// Place a `w`x`h` popover for a span whose start sits at cell `(anchor_col,
/// anchor_row)`, clamped inside `pane`.
///
/// The box sits one row below the span, flipping to sit above it when it would
/// cross the pane's bottom edge, and shifts left to stay within the right edge.
fn popover_rect(anchor_col: u16, anchor_row: u16, w: u16, h: u16, pane: Rect) -> Rect {
    let w = w.min(pane.width);
    let h = h.min(pane.height);

    let max_x = (pane.x + pane.width).saturating_sub(w);
    let x = anchor_col.clamp(pane.x, max_x.max(pane.x));

    let below = anchor_row.saturating_add(1);
    let y = if below.saturating_add(h) <= pane.y + pane.height {
        below
    } else {
        anchor_row.saturating_sub(h)
    };
    let max_y = (pane.y + pane.height).saturating_sub(h);
    let y = y.clamp(pane.y, max_y.max(pane.y));

    Rect {
        x,
        y,
        width: w,
        height: h,
    }
}

/// Scale each channel of `rgb` to 82% to darken a fill roughly 18% below the
/// editor background, so a popover reads as a raised surface over the text.
fn darken(rgb: [u8; 3]) -> [u8; 3] {
    rgb.map(|c| (c as u16 * 82 / 100) as u8)
}

/// The `IconKind` for a severity. Hint has no icon of its own and shares Info's.
fn icon_kind(sev: DiagnosticSeverity) -> IconKind {
    match sev {
        DiagnosticSeverity::ERROR => IconKind::Error,
        DiagnosticSeverity::WARNING => IconKind::Warning,
        DiagnosticSeverity::INFORMATION | DiagnosticSeverity::HINT => IconKind::Info,
        _ => IconKind::Error,
    }
}

/// The `&str` prefix of `s` up to `max` characters, respecting UTF-8 boundaries.
fn clip_chars(s: &str, max: usize) -> &str {
    match s.char_indices().nth(max) {
        Some((byte, _)) => &s[..byte],
        None => s,
    }
}

/// Render `diag` as a floating popover anchored at `(anchor_col, anchor_row)`,
/// with a severity icon in its first cell. Returns whether it rendered.
///
/// The content is the first four message lines, each clipped to 40 columns. The
/// box is sized to fit and placed by [`popover_rect`]. A message with no text
/// draws nothing.
#[allow(clippy::too_many_arguments)]
fn render_diagnostic_popover(
    scene: &mut ApcScene,
    buf: &mut Buffer,
    diag: &lsp_types::Diagnostic,
    color: [u8; 3],
    fill: [u8; 3],
    anchor_col: u16,
    anchor_row: u16,
    pane: Rect,
) -> bool {
    let lines: Vec<&str> = diag
        .message
        .lines()
        .take(4)
        .map(|l| clip_chars(l, 40))
        .collect();
    if lines.iter().all(|l| l.is_empty()) {
        return false;
    }
    let longest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0);
    // Prefix each line with the icon cell and a one-cell gap. The box is still
    // sized from the unprefixed longest line, so w and h stay unchanged and the
    // icon cell falls inside the one-cell content inset.
    let content = lines
        .iter()
        .map(|line| format!("  {line}"))
        .collect::<Vec<_>>()
        .join("\n");

    let w = (longest as u16).saturating_add(4);
    let h = (lines.len() as u16).saturating_add(2);
    let rect = popover_rect(anchor_col, anchor_row, w, h, pane);
    if rect.width < 3 || rect.height < 3 {
        return false;
    }

    let sev = diag.severity.unwrap_or(DiagnosticSeverity::ERROR);
    Popover {
        fill,
        border: color,
        content_fg: color,
        scale: 1,
        offset: [3, 6],
        content: &content,
    }
    .render(rect, buf, scene);
    Icon {
        kind: icon_kind(sev),
        color,
        size: 1,
        offset: [3, 6],
    }
    .render(
        Rect {
            x: rect.x + 1,
            y: rect.y + 1,
            width: 1,
            height: 1,
        },
        buf,
        scene,
    );
    true
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
    runs: Option<&mut Vec<(u16, u16, u16)>>,
) {
    let map_simple =
        snapshot.fold_snapshot().fold_count() == 0 && !snapshot.inlay_snapshot().has_inlays();
    let tab_size = snapshot.tab_snapshot().tab_size();
    let max_expansion_column = snapshot.tab_snapshot().max_expansion_column();
    let line_count = snapshot.line_count();

    let collect = runs.is_some();
    let mut painted: Vec<(u16, u16)> = Vec::new();
    let mut paint = |display_row: u32, display_col: u32| {
        if display_row < scroll_row || display_row >= end_row {
            return;
        }
        let y = inner.y + (display_row - scroll_row) as u16;
        let x = inner.x + display_col as u16;
        if x < right && y < bottom {
            buf[(x, y)].set_style(style);
            if collect {
                painted.push((x, y));
            }
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

    if let Some(runs) = runs {
        coalesce_runs(&painted, runs);
    }
}

/// Coalesce painted cells, in paint order, into `(x, y, len)` runs of
/// horizontally adjacent same-row cells, appending them to `out`.
///
/// A diagnostic span paints its cells left to right within each display row, so
/// adjacency breaks exactly at a row change or a gap (a tab or wide-char
/// expansion the span skipped), which is where the underline should break too.
fn coalesce_runs(painted: &[(u16, u16)], out: &mut Vec<(u16, u16, u16)>) {
    let mut cur: Option<(u16, u16, u16)> = None;
    for &(x, y) in painted {
        match cur {
            Some((rx, ry, rlen)) if ry == y && rx + rlen == x => {
                cur = Some((rx, ry, rlen + 1));
            },
            _ => {
                if let Some(run) = cur.take() {
                    out.push(run);
                }
                cur = Some((x, y, 1));
            },
        }
    }
    if let Some(run) = cur {
        out.push(run);
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
    fn fold_gutter_lines_numbers_and_folds_wraps_and_blocks() {
        use super::RowKind::{Continuation, LineStart};
        // Line 1, then line 2 soft-wrapped over two extra rows with a block row
        // folded under it, then line 3.
        let rows = [
            LineStart(0),
            LineStart(1),
            Continuation,
            Continuation,
            LineStart(2),
        ];
        assert_eq!(super::fold_gutter_lines(&rows, 1), [(1, 1), (2, 3), (3, 1)]);
    }

    #[test]
    fn fold_gutter_lines_attaches_leading_continuations_to_lead() {
        use super::RowKind::{Continuation, LineStart};
        // Viewport opens on wrap continuations of buffer line 7 (number 8).
        let rows = [Continuation, Continuation, LineStart(8)];
        assert_eq!(super::fold_gutter_lines(&rows, 8), [(8, 2), (9, 1)]);
    }

    #[test]
    fn decimal_digits_counts_digits() {
        assert_eq!(
            [0, 9, 10, 99, 100, 1000].map(super::decimal_digits),
            [1, 1, 2, 2, 3, 4]
        );
    }

    fn span_diag(start: u32, end: u32, sev: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: start,
                },
                end: Position {
                    line: 0,
                    character: end,
                },
            },
            severity: Some(sev),
            message: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn diagnostic_at_offset_finds_worst_containing_span() {
        use stoat_text::Rope;
        let rope = Rope::from("let x = 1;\n");
        let path = PathBuf::from("/a");
        let mut set = crate::diagnostics::DiagnosticSet::new();
        // A warning over just `x` [4,5) and an error over `x = 1` [4,9).
        set.replace_for_path(
            path.clone(),
            vec![
                span_diag(4, 5, DiagnosticSeverity::WARNING),
                span_diag(4, 9, DiagnosticSeverity::ERROR),
            ],
        );

        // Offset 4 is in both, so the worse severity (the error) wins.
        assert_eq!(super::diagnostic_at_offset(&set, &path, &rope, 4), Some(1));
        // Offset 7 is inside only the error span.
        assert_eq!(super::diagnostic_at_offset(&set, &path, &rope, 7), Some(1));
        // Offset 0 is outside both.
        assert_eq!(super::diagnostic_at_offset(&set, &path, &rope, 0), None);
    }

    #[test]
    fn popover_rect_sits_below_then_flips_and_clamps() {
        use ratatui::layout::Rect;
        let pane = Rect::new(0, 0, 40, 10);
        // Fits below the anchor row.
        assert_eq!(
            super::popover_rect(5, 2, 12, 4, pane),
            Rect::new(5, 3, 12, 4)
        );
        // Would cross the bottom, so it flips above the anchor.
        assert_eq!(
            super::popover_rect(5, 8, 12, 4, pane),
            Rect::new(5, 4, 12, 4)
        );
        // Shifts left to stay within the right edge.
        assert_eq!(
            super::popover_rect(35, 2, 12, 4, pane),
            Rect::new(28, 3, 12, 4)
        );
    }

    #[test]
    fn darken_scales_channels_to_82_percent() {
        assert_eq!(super::darken([40, 44, 52]), [32, 36, 42]);
        assert_eq!(super::darken([0, 100, 200]), [0, 82, 164]);
    }

    #[test]
    fn clip_chars_respects_utf8_boundaries() {
        assert_eq!(super::clip_chars("hello", 3), "hel");
        assert_eq!(super::clip_chars("hi", 5), "hi");
        assert_eq!(super::clip_chars("café", 3), "caf");
    }

    #[test]
    fn icon_kind_maps_hint_to_info() {
        use stoatty_protocol::command::IconKind;
        assert!(matches!(
            super::icon_kind(DiagnosticSeverity::HINT),
            IconKind::Info
        ));
        assert!(matches!(
            super::icon_kind(DiagnosticSeverity::ERROR),
            IconKind::Error
        ));
    }

    #[test]
    fn severity_colors_resolve_under_the_shipped_theme() {
        let h = Stoat::test();
        assert!(
            super::severity_colors(&h.stoat.theme).is_some(),
            "the shipped default theme must resolve every diagnostic severity \
             to RGB so the sub-cell gutter engages under stoatty",
        );
    }

    #[test]
    fn a_diagnostic_span_collects_an_undercurl_under_stoatty() {
        let mut h = Stoat::test();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_stoatty_apc(true, tx);

        let root = PathBuf::from("/undercurl-test");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.stoat
            .diagnostics
            .replace_for_path(path, vec![diag(0, DiagnosticSeverity::WARNING)]);

        let _ = h.stoat.render();

        assert_eq!(
            h.stoat.pending_undercurls.len(),
            1,
            "the warning span paints one underline run",
        );
        assert_eq!(
            h.stoat.pending_undercurls[0].color,
            [0xe5, 0xc0, 0x7b],
            "the run carries the shipped warning severity color",
        );
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

    fn overlap_diag(line: u32, start: u32, end: u32, sev: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: start,
                },
                end: Position {
                    line,
                    character: end,
                },
            },
            severity: Some(sev),
            message: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn snapshot_diagnostic_overlap_warning_beats_hint() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-overlap-warn");
        let path = root.join("a.rs");
        h.fake_fs()
            .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        // Same span, warning then hint in rust-analyzer's publish order. The
        // worse severity must win the span color over the later-published hint,
        // so the underline stays warning yellow rather than turning hint grey.
        h.stoat.diagnostics.replace_for_path(
            path,
            vec![
                overlap_diag(1, 4, 5, DiagnosticSeverity::WARNING),
                overlap_diag(1, 4, 5, DiagnosticSeverity::HINT),
            ],
        );
        h.assert_snapshot("diagnostic_overlap_warning_beats_hint");
    }

    #[test]
    fn snapshot_diagnostic_overlap_error_beats_hint() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-overlap-error");
        let path = root.join("a.rs");
        h.fake_fs()
            .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        // Hint then error in publish order. Error must win the span color.
        h.stoat.diagnostics.replace_for_path(
            path,
            vec![
                overlap_diag(1, 4, 5, DiagnosticSeverity::HINT),
                overlap_diag(1, 4, 5, DiagnosticSeverity::ERROR),
            ],
        );
        h.assert_snapshot("diagnostic_overlap_error_beats_hint");
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
        // Column 3 is the line-number gutter width the cursor sits past.
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((3, 0)));

        for _ in 0..6 {
            dispatch(&mut h.stoat, &MoveRight);
        }
        h.snapshot();
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((9, 0)));

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
        // Column 3 is the line-number gutter width the cursor sits past.
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((3, 0)));

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
