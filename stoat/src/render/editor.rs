use super::TEXT_SCALE_COMPACT;
use crate::{
    diff_map::DiffHunkStatus,
    display_map::{tab_map, BlockRowKind, DisplayPoint, DisplaySnapshot},
    editor_state::{EditorState, SearchMatchCache},
    host::OffsetEncoding,
    minimap::color_to_rgb,
    render::{
        conflict_view::render_conflict_view,
        review::{dim_rgb, render_diff_view, render_review, style_rgb},
        undercurl::UndercurlSpan,
    },
};
use lsp_types::{DiagnosticSeverity, DiagnosticTag};
use ratatui::{
    buffer::{Buffer, Cell},
    layout::{Position, Rect},
    style::{Color, Modifier, Style},
    widgets::StatefulWidget,
};
use std::{
    cmp::Reverse,
    collections::{hash_map::DefaultHasher, BTreeMap, HashMap, HashSet},
    hash::{Hash, Hasher},
    ops::Range,
    path::Path,
    sync::Arc,
};
use stoat_config::{LineNumbers, WrapMode};
use stoat_text::{cursor_offset, Bias, Rope};
use stoatty_protocol::command::IconKind;
use stoatty_widgets::{
    bar::Bar,
    gutter::{Diagnostic, GitMark, Gutter, GutterLine},
    icon::Icon,
    popover::Popover,
    ApcScene,
};

/// Columns reserved on a pane's right edge for the minimap strip under stoatty,
/// matching the width stoatty's GPU minimap pass paints there.
pub(super) const MINIMAP_STRIP_COLS: u16 = 8;

/// Narrowest pane, in columns, that still reserves a minimap strip. Below this
/// the strip would crowd the remaining text, so the pane keeps its full width.
pub(super) const MINIMAP_MIN_PANE_COLS: u16 = 60;

/// Each server name mapped to its negotiated offset encoding, so a diagnostic's
/// LSP position converts to a byte column through the server that published it.
pub(crate) type DiagnosticEncodings = HashMap<String, OffsetEncoding>;

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
        LineNumbers::Off,
        false,
        None,
        None,
        None,
        None,
        None,
        None,
        0.0,
        WrapMode::None,
        80,
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
    minimap_enabled: bool,
    line_numbers: LineNumbers,
    insert_mode: bool,
    hover_cell: Option<(u16, u16)>,
    goto_word_labels: Option<&BTreeMap<String, usize>>,
    search_query: Option<&str>,
    diagnostic_info: Option<(
        &Path,
        &crate::diagnostics::DiagnosticSet,
        &DiagnosticEncodings,
    )>,
    mut scene: Option<&mut ApcScene>,
    undercurls: Option<&mut Vec<UndercurlSpan>>,
    dim: f32,
    wrap: WrapMode,
    wrap_column: u32,
) {
    editor.viewport_rows = Some(inner.height as u32);
    editor.cursor_screen_cell = None;
    editor.minimap_rect = None;

    if editor.review_view.is_some() {
        editor.display_map.set_wrap_width(None);
        let scene = if stoatty { scene } else { None };
        render_review(editor, inner, fallback_style, theme, buf, scene);
        return;
    }

    if editor.diff_view {
        editor.display_map.set_wrap_width(None);
        let scene = if stoatty { scene } else { None };
        render_diff_view(editor, inner, fallback_style, theme, buf, scene);
        return;
    }

    if editor.conflict_view.is_some() {
        editor.display_map.set_wrap_width(None);
        render_conflict_view(editor, inner, fallback_style, theme, buf, stoatty);
        return;
    }

    // A first snapshot drives the gutter measurement and the wrap-width
    // decision. Its buffer-level facts (line count, cursor line, diagnostics)
    // are wrap-independent, so the width is resolved and stamped here before the
    // wrapped snapshot the gutter and text paint from is taken below.
    let snapshot = editor.display_map.snapshot();

    let rich_gutter_colors = resolve_rich_gutter(theme, fallback_style, stoatty);
    let gutter_is_rich = scene.is_some() && rich_gutter_colors.is_some();
    let measured_gutter_w = if line_numbers != LineNumbers::Off {
        measure_gutter_width(&snapshot, gutter_is_rich)
    } else {
        match diagnostic_info {
            Some((path, set, _)) if !set.get(path).is_empty() => 1,
            _ => 0,
        }
    };

    let after_gutter = inner.width.saturating_sub(measured_gutter_w);
    let minimap_cols = if stoatty && minimap_enabled && after_gutter >= MINIMAP_MIN_PANE_COLS {
        MINIMAP_STRIP_COLS
    } else {
        0
    };
    let text_width = after_gutter.saturating_sub(minimap_cols);
    // A per-editor ToggleWrap override wins over the frame's configured mode.
    let wrap_width = match editor.wrap_override.unwrap_or(wrap) {
        WrapMode::None => None,
        WrapMode::EditorWidth => Some(u32::from(text_width).max(1)),
        WrapMode::Bounded => Some(u32::from(text_width).max(1).min(wrap_column)),
    };
    editor.display_map.set_wrap_width(wrap_width);

    let snapshot = editor.display_map.snapshot();
    let visible_rows = inner.height as u32;
    let total_rows = snapshot.line_count();
    let end_row = (editor.scroll_row + visible_rows).min(total_rows);
    if end_row <= editor.scroll_row {
        return;
    }

    let empty_severity = BTreeMap::new();
    let row_severity: &BTreeMap<u32, DiagnosticSeverity> = match diagnostic_info {
        Some((path, set, _)) => {
            let version = set.version();
            let stale = match &editor.gutter_severity_cache {
                Some(cache) => cache.version != version,
                None => true,
            };
            if stale {
                editor.gutter_severity_cache = Some(GutterSeverityCache {
                    version,
                    map: Arc::new(compute_row_severity(set, path)),
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

    // Relative numbering measures each line against the cursor's buffer line,
    // and only for the focused pane outside insert mode. Every other case
    // paints absolute. Resolved here so the digits track the cursor.
    let current_line =
        (line_numbers == LineNumbers::Relative && is_focused && !insert_mode).then(|| {
            let buffer_snapshot = snapshot.buffer_snapshot();
            let rope = buffer_snapshot.rope();
            let sel = editor.selections.newest_anchor();
            let cursor = cursor_offset(
                rope,
                buffer_snapshot.resolve_anchor(&sel.tail()),
                buffer_snapshot.resolve_anchor(&sel.head()),
            );
            rope.offset_to_point(cursor).row + 1
        });

    let severity_version = diagnostic_info.map_or(0, |(_, set, _)| set.version());

    let gutter_w = if line_numbers != LineNumbers::Off {
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
            current_line,
            severity_version,
            &mut editor.gutter_geometry_cache,
            scene.as_deref_mut(),
            buf,
            dim,
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
                let bar_bg = style_rgb(fallback_style.bg.or_else(|| {
                    theme
                        .try_get(crate::theme::scope::UI_BACKGROUND)
                        .and_then(|st| st.bg)
                }));
                for display_row in editor.scroll_row..end_row {
                    let row_offset = (display_row - editor.scroll_row) as u16;
                    if row_offset >= inner.height {
                        break;
                    }
                    let Some(sev) = row_severity.get(&display_row) else {
                        continue;
                    };
                    let color = match bar_bg {
                        Some(bg) if dim > 0.0 => dim_rgb(severity_color(*sev, colors), bg, dim),
                        _ => severity_color(*sev, colors),
                    };
                    Bar {
                        x: 0,
                        y: row_offset * 16,
                        width: 6,
                        height: 16,
                        color,
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

    // The wrap width stamped above subtracted `measured_gutter_w` from the pane;
    // the painted gutter must reserve exactly that so the text rect and the wrap
    // width agree.
    debug_assert_eq!(
        gutter_w, measured_gutter_w,
        "painted gutter width matches the measured width the wrap used",
    );

    // Inset the text rect by the gutter, and record the width so click-to-offset
    // subtracts the same shift. Written after the `row_severity` borrow ends.
    let inner = Rect {
        x: inner.x + gutter_w,
        y: inner.y,
        width: inner.width.saturating_sub(gutter_w),
        height: inner.height,
    };
    editor.gutter_width = gutter_w;

    // Reserve the right-edge minimap strip under stoatty, recording its screen
    // rect for pointer mapping. Only the text rect shrinks to clear the space.
    // The reserved cells stay blank so stoatty's GPU minimap pass owns them.
    let inner = if stoatty && minimap_enabled && inner.width >= MINIMAP_MIN_PANE_COLS {
        editor.minimap_rect = Some(Rect {
            x: inner.x + inner.width - MINIMAP_STRIP_COLS,
            y: inner.y,
            width: MINIMAP_STRIP_COLS,
            height: inner.height,
        });
        Rect {
            width: inner.width - MINIMAP_STRIP_COLS,
            ..inner
        }
    } else {
        inner
    };

    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;

    {
        let mut x = inner.x;
        let mut y = inner.y;
        let inlay_style = fallback_style.patch(theme.get(crate::theme::scope::UI_VIRTUAL_INLAY));
        'chunks: for chunk in snapshot.highlighted_chunks_cached(
            editor.scroll_row..end_row,
            &mut editor.highlight_endpoint_cache,
        ) {
            let style = if chunk.is_inlay {
                inlay_style
            } else {
                chunk
                    .highlight_style
                    .as_ref()
                    .map(|hs| hs.to_ratatui_style())
                    .unwrap_or(fallback_style)
            };
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
    let visible = visible_byte_range(
        &snapshot,
        buffer_snapshot.rope(),
        editor.scroll_row,
        end_row,
    );

    if let Some((path, set, encodings)) = diagnostic_info {
        let rope = buffer_snapshot.rope();
        build_diagnostic_span_cache(
            editor,
            set,
            path,
            rope,
            encodings,
            buffer_snapshot.version(),
        );
        let spans: &[ResolvedDiag] = editor
            .diagnostic_span_cache
            .as_ref()
            .map_or(&[], |c| c.spans.as_slice());
        paint_diagnostic_spans(
            spans,
            visible.clone(),
            rope,
            &snapshot,
            theme,
            fallback_style,
            editor.scroll_row,
            end_row,
            inner,
            right,
            bottom,
            buf,
            if stoatty { undercurls } else { None },
            severity.as_ref(),
            dim,
        );
    }

    if let Some(query) = search_query.filter(|q| !q.is_empty()) {
        let version = buffer_snapshot.version();
        let rope = buffer_snapshot.rope();
        let stale = match &editor.search_match_cache {
            Some(cache) => {
                cache.version != version || cache.query != query || cache.visible != visible
            },
            None => true,
        };
        if stale {
            // Reuse the compiled regex while the query text holds, so only a new
            // query pays a fresh compile. A cached None from a failed compile is
            // reused too, so an invalid query does not recompile every frame.
            let (mut window, regex) = match editor.search_match_cache.take() {
                Some(cache) if cache.query == query => (cache.window, cache.regex),
                Some(cache) => (
                    cache.window,
                    crate::action_handlers::search::compile_search_regex(query).ok(),
                ),
                None => (
                    String::new(),
                    crate::action_handlers::search::compile_search_regex(query).ok(),
                ),
            };
            window.clear();
            for chunk in rope.chunks_in_range(visible.clone()) {
                window.push_str(chunk);
            }
            let matches = match &regex {
                Some(regex) => regex
                    .find_iter(&window)
                    .filter(|m| m.end() > m.start())
                    .map(|m| (m.start() + visible.start, m.end() + visible.start))
                    .collect(),
                None => Vec::new(),
            };
            editor.search_match_cache = Some(SearchMatchCache {
                version,
                query: query.to_string(),
                visible: visible.clone(),
                matches,
                window,
                regex,
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
                &mut |_, _, cell| {
                    cell.set_style(match_style);
                },
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
                &mut |_, _, cell| {
                    cell.set_style(selection_style);
                },
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

    if let Some((path, set, encodings)) = diagnostic_info {
        build_diagnostic_span_cache(
            editor,
            set,
            path,
            rope,
            encodings,
            buffer_snapshot.version(),
        );
        let spans: &[ResolvedDiag] = editor
            .diagnostic_span_cache
            .as_ref()
            .map_or(&[], |c| c.spans.as_slice());
        let sel = editor.selections.newest_anchor();
        let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
        let head_off = buffer_snapshot.resolve_anchor(&sel.head());
        let cursor = cursor_offset(rope, tail_off, head_off);
        let cursor_diag = diagnostic_at_offset(spans, cursor);
        let hover_diag = hover_cell.and_then(|(hx, hy)| {
            let col = hx.checked_sub(content_area.x)?;
            let row = hy.checked_sub(content_area.y)?;
            if col >= content_area.width || row >= content_area.height {
                return None;
            }
            let offset = display_cell_to_offset(&snapshot, editor.scroll_row, gutter_w, col, row)?;
            diagnostic_at_offset(spans, offset)
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
                // Reuse the span resolved with this diagnostic's server encoding
                // rather than re-deriving the offset from its raw character column.
                let start = spans
                    .iter()
                    .find(|s| s.index == index)
                    .map_or(0, |s| s.start);
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
                    primary_cell,
                ) {
                    suppress = Some(index);
                }
            }
        }

        paint_cursor_line_diagnostic(
            spans,
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
    pub(crate) map: Arc<BTreeMap<u32, DiagnosticSeverity>>,
}

/// Cached gutter geometry for one set of drawn-gutter inputs.
///
/// Holds the folded gutter lines, digit width, per-row diff marks, and the rich
/// component lines, rebuilt only when [`Self::key`] changes. The key hashes
/// every input that changes the drawn gutter -- the viewport window, the buffer,
/// fold, diff, and diagnostic-severity versions, the relative-numbering line,
/// and the resolved colors baked into the lines -- so a repaint that changes
/// none of them reuses the collections instead of rebuilding them each frame.
pub(crate) struct GutterGeometryCache {
    key: u64,
    folded: Vec<(u32, u16)>,
    width_digits: u16,
    marks: BTreeMap<u32, (DiffHunkStatus, bool)>,
    lines: Vec<GutterLine>,
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

/// A diagnostic resolved to byte offsets once per (set, buffer) version, so the
/// per-frame render paths binary-search a cached slice instead of re-resolving
/// and re-scanning the whole list every frame.
///
/// `index` is the position in `set.get(path)`, so a consumer can recover the
/// original diagnostic (its message, tags) after locating a span. `start_line`/
/// `end_line` are the diagnostic's LSP rows, kept so the cursor-line query stays
/// line-based rather than reinterpreting byte ranges at line boundaries.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResolvedDiag {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) severity: DiagnosticSeverity,
    pub(crate) unnecessary: bool,
    pub(crate) start_line: u32,
    pub(crate) end_line: u32,
    pub(crate) index: usize,
}

/// Per-editor cache of [`ResolvedDiag`]s, rebuilt when the diagnostic set or the
/// buffer version changes. Transient render state, not persisted.
pub(crate) struct DiagnosticSpanCache {
    set_version: u64,
    buffer_version: u64,
    spans: Vec<ResolvedDiag>,
}

/// Resolve every diagnostic for `path` to byte offsets, sorted by start.
///
/// Each range is converted through the offset encoding its publishing server
/// negotiated (a server absent from `encodings` falls back to UTF-16), so a
/// utf-16 server's diagnostic on a multibyte line lands on the right byte. The
/// index into `set.get(path)` is retained so callers can recover the source
/// diagnostic.
pub(crate) fn resolve_diagnostic_spans(
    set: &crate::diagnostics::DiagnosticSet,
    path: &Path,
    rope: &Rope,
    encodings: &DiagnosticEncodings,
) -> Vec<ResolvedDiag> {
    let mut spans: Vec<ResolvedDiag> = set
        .attributed(path)
        .enumerate()
        .map(|(index, (server, diag))| {
            let encoding = encodings
                .get(server)
                .copied()
                .unwrap_or(OffsetEncoding::Utf16);
            let start = crate::lsp::util::lsp_pos_to_byte_offset(rope, diag.range.start, encoding);
            let end = crate::lsp::util::lsp_pos_to_byte_offset(rope, diag.range.end, encoding);
            ResolvedDiag {
                start,
                end,
                severity: diag.severity.unwrap_or(DiagnosticSeverity::ERROR),
                unnecessary: is_unnecessary(diag),
                start_line: diag.range.start.line,
                end_line: diag.range.end.line,
                index,
            }
        })
        .collect();
    spans.sort_by_key(|s| s.start);
    spans
}

/// Rebuild `editor.diagnostic_span_cache` when the diagnostic set or buffer
/// version has moved since it was last resolved.
fn build_diagnostic_span_cache(
    editor: &mut EditorState,
    set: &crate::diagnostics::DiagnosticSet,
    path: &Path,
    rope: &Rope,
    encodings: &DiagnosticEncodings,
    buffer_version: u64,
) {
    let set_version = set.version();
    let stale = match &editor.diagnostic_span_cache {
        Some(cache) => cache.set_version != set_version || cache.buffer_version != buffer_version,
        None => true,
    };
    if stale {
        editor.diagnostic_span_cache = Some(DiagnosticSpanCache {
            set_version,
            buffer_version,
            spans: resolve_diagnostic_spans(set, path, rope, encodings),
        });
    }
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

/// Blend a syntax foreground 3:2 toward the pane background, keeping the hue but
/// reading muted. Used to dim Unnecessary-tagged (inactive-code) regions without
/// discarding their per-token syntax colors.
fn mute_rgb(fg: [u8; 3], bg: [u8; 3]) -> [u8; 3] {
    let mix = |f: u8, b: u8| ((f as u16 * 3 + b as u16 * 2) / 5) as u8;
    [mix(fg[0], bg[0]), mix(fg[1], bg[1]), mix(fg[2], bg[2])]
}

/// Whether a diagnostic carries the `Unnecessary` tag, marking dead or
/// inactive code (e.g. a `#[cfg]`-excluded region) that renders muted rather
/// than underlined.
fn is_unnecessary(diag: &lsp_types::Diagnostic) -> bool {
    diag.tags
        .as_ref()
        .is_some_and(|tags| tags.contains(&DiagnosticTag::UNNECESSARY))
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

#[derive(Clone, Hash)]
pub(crate) struct SeverityColors {
    error: [u8; 3],
    warning: [u8; 3],
    info: [u8; 3],
    hint: [u8; 3],
}

impl SeverityColors {
    /// Blend every severity color toward `bg` by `amount` (`0.0` is identity),
    /// dimming the gutter's diagnostic marks with an unfocused pane.
    fn dim(&self, bg: [u8; 3], amount: f32) -> SeverityColors {
        SeverityColors {
            error: dim_rgb(self.error, bg, amount),
            warning: dim_rgb(self.warning, bg, amount),
            info: dim_rgb(self.info, bg, amount),
            hint: dim_rgb(self.hint, bg, amount),
        }
    }
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

/// The four diff-status colors the gutter mark uses, resolved the way the
/// minimap edge lane resolves them. Each is `theme.get(diff.*)` under
/// [`crate::theme::Theme::get`]'s progressive scope-broadening fallback, so a
/// theme omitting `diff.modified` or `diff.moved` still yields a color that
/// agrees with the minimap lane.
#[derive(Clone, Copy, Hash)]
pub(crate) struct DiffMarkColors {
    added: [u8; 3],
    modified: [u8; 3],
    moved: [u8; 3],
    deleted: [u8; 3],
    staged: [u8; 3],
    unstaged: [u8; 3],
}

impl DiffMarkColors {
    fn resolve(theme: &crate::theme::Theme) -> Self {
        use crate::theme::scope as s;
        let get = |scope| color_to_rgb(theme.get(scope).fg.unwrap_or(Color::White));
        Self {
            added: get(s::DIFF_ADDED),
            modified: get(s::DIFF_MODIFIED),
            moved: get(s::DIFF_MOVED),
            deleted: get(s::DIFF_DELETED),
            staged: get(s::DIFF_STAGED),
            unstaged: get(s::DIFF_UNSTAGED),
        }
    }

    fn for_status(&self, status: DiffHunkStatus) -> [u8; 3] {
        match status {
            DiffHunkStatus::Added => self.added,
            DiffHunkStatus::Modified => self.modified,
            DiffHunkStatus::Moved => self.moved,
            DiffHunkStatus::Deleted => self.deleted,
        }
    }

    /// Blend every diff-mark color toward `bg` by `amount` (`0.0` is identity),
    /// dimming the gutter's diff marks with an unfocused pane.
    fn dim(&self, bg: [u8; 3], amount: f32) -> DiffMarkColors {
        DiffMarkColors {
            added: dim_rgb(self.added, bg, amount),
            modified: dim_rgb(self.modified, bg, amount),
            moved: dim_rgb(self.moved, bg, amount),
            deleted: dim_rgb(self.deleted, bg, amount),
            staged: dim_rgb(self.staged, bg, amount),
            unstaged: dim_rgb(self.unstaged, bg, amount),
        }
    }
}

/// The resolved colors the rich sub-cell page gutter needs.
#[derive(Clone)]
pub(crate) struct RichGutterColors {
    pub(crate) colors: SeverityColors,
    pub(crate) diff: DiffMarkColors,
    pub(crate) number_fg: [u8; 3],
    pub(crate) separator: [u8; 3],
    pub(crate) bg: [u8; 3],
}

impl RichGutterColors {
    /// Blend every foreground color toward the gutter background by `amount`
    /// (`0.0` is identity), dimming a pooled page's gutter for an unfocused pane.
    pub(crate) fn dim(&self, amount: f32) -> RichGutterColors {
        RichGutterColors {
            colors: self.colors.dim(self.bg, amount),
            diff: self.diff.dim(self.bg, amount),
            number_fg: dim_rgb(self.number_fg, self.bg, amount),
            separator: dim_rgb(self.separator, self.bg, amount),
            bg: self.bg,
        }
    }
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
    let diff = DiffMarkColors::resolve(theme);
    let number_fg = style_rgb(theme.get(s::UI_TEXT_MUTED).fg)?;
    let separator = style_rgb(theme.get(s::UI_BORDER_INACTIVE).fg).unwrap_or(number_fg);
    let bg = style_rgb(
        fallback_style
            .bg
            .or_else(|| theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg)),
    )?;
    Some(RichGutterColors {
        colors,
        diff,
        number_fg,
        separator,
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
///
/// A trailing newline leaves a final empty line the min-width-1 cursor can never
/// reach, so it is rendering padding rather than a line. Its gutter number is
/// dropped and it is excluded from the width, so a bare `"\n"` scratch shows one
/// numbered row and a trailing newline never widens the gutter.
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
    let mut folded = fold_gutter_lines(&rows, lead_number);

    // The rope ends with a newline exactly when its max point sits at column 0
    // of a row past the first, making that last row the cursor-unreachable
    // phantom. Never fires for the empty command-input rope (row 0).
    let max = snapshot.buffer_snapshot().rope().max_point();
    let phantom = (max.row > 0 && max.column == 0).then_some(max.row + 1);
    folded.retain(|&(number, _)| Some(number) != phantom);

    (folded, gutter_width_digits(snapshot))
}

/// The digit width the gutter reserves for `snapshot`'s line numbers, at least
/// two.
///
/// A trailing newline leaves an empty final line the min-width-1 cursor cannot
/// reach, so it is rendering padding rather than a line and never widens the
/// gutter. Counts only buffer lines, never wrap or block rows, so it can be
/// measured before the wrapped snapshot is taken.
fn gutter_width_digits(snapshot: &DisplaySnapshot) -> u16 {
    let max = snapshot.buffer_snapshot().rope().max_point();
    let phantom = max.row > 0 && max.column == 0;
    decimal_digits(snapshot.buffer_line_count() - phantom as u32).max(2)
}

/// The cell columns the line-number gutter reserves, measured without painting.
///
/// `rich` selects the sub-cell [`Gutter::cell_width`] layout. The degraded
/// gutter instead reserves a mark column, the digits, and a gap. The result
/// matches what [`draw_line_number_gutter`] paints for the same snapshot, so the
/// wrap width can be resolved before the wrapped snapshot exists.
fn measure_gutter_width(snapshot: &DisplaySnapshot, rich: bool) -> u16 {
    let width_digits = gutter_width_digits(snapshot);
    if rich {
        rich_gutter(&[], width_digits, [0; 3], [0; 3], [0; 3]).cell_width()
    } else {
        width_digits + 4
    }
}

/// The number the gutter paints for an absolute 1-based line.
///
/// With `current_line` set (relative numbering active), every line but the
/// cursor's shows its distance from the cursor line. The cursor line, and the
/// `None` case of absolute numbering, show the absolute number. Severity keying
/// stays on the absolute number, so only the painted digits change.
pub(crate) fn gutter_display_number(absolute: u32, current_line: Option<u32>) -> u32 {
    match current_line {
        Some(cur) if absolute != cur => absolute.abs_diff(cur),
        _ => absolute,
    }
}

/// Build the rich gutter's [`GutterLine`]s from `folded`, coloring each line's
/// diagnostic mark from `colors`.
///
/// `current_line` selects relative numbering per [`gutter_display_number`]. The
/// diagnostic mark stays keyed to the absolute buffer line.
/// Map each folded row a diff hunk marks to its `(status, staged)` pair, for the
/// gutter's git bar. Rows outside any hunk are absent from the result.
pub(crate) fn gutter_diff_marks(
    snapshot: &DisplaySnapshot,
    folded: &[(u32, u16)],
) -> BTreeMap<u32, (DiffHunkStatus, bool)> {
    let Some(diff_map) = snapshot.diff_map() else {
        return BTreeMap::new();
    };
    folded
        .iter()
        .filter_map(|&(number, _)| {
            let row = number - 1;
            diff_map.gutter_mark_for_line(row).map(|mark| (row, mark))
        })
        .collect()
}

pub(crate) fn gutter_component_lines(
    folded: &[(u32, u16)],
    row_severity: &BTreeMap<u32, DiagnosticSeverity>,
    diff_marks: &BTreeMap<u32, (DiffHunkStatus, bool)>,
    diff_colors: &DiffMarkColors,
    colors: &SeverityColors,
    current_line: Option<u32>,
) -> Vec<GutterLine> {
    folded
        .iter()
        .map(|&(number, height)| GutterLine {
            number: gutter_display_number(number, current_line),
            height,
            git: diff_marks
                .get(&(number - 1))
                .map(|&(status, staged)| GitMark {
                    color: diff_colors.for_status(status),
                    staged_color: if staged {
                        diff_colors.staged
                    } else {
                        diff_colors.unstaged
                    },
                    seam: status == DiffHunkStatus::Deleted,
                }),
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
    separator: [u8; 3],
    bg: [u8; 3],
) -> Gutter<'_> {
    Gutter {
        lines,
        bar_width: 5,
        pad: 2,
        number_scale: TEXT_SCALE_COMPACT,
        width_digits,
        number_fg,
        separator,
        bg,
    }
}

/// Hash the inputs that change the drawn line-number gutter into a cache key.
///
/// Any change here misses [`GutterGeometryCache`] and rebuilds the geometry;
/// otherwise a repaint reuses it. `colors` is `Some` only in rich mode, where
/// the component lines bake the diff and severity colors in, so a theme change
/// shows up as a different key.
#[allow(clippy::too_many_arguments)]
fn gutter_geometry_key(
    scroll_row: u32,
    width: u16,
    visible: u32,
    buffer_version: u64,
    fold_version: usize,
    diff_version: usize,
    severity_version: u64,
    current_line: Option<u32>,
    colors: Option<([u8; 3], DiffMarkColors, &SeverityColors)>,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    scroll_row.hash(&mut hasher);
    width.hash(&mut hasher);
    visible.hash(&mut hasher);
    buffer_version.hash(&mut hasher);
    fold_version.hash(&mut hasher);
    diff_version.hash(&mut hasher);
    severity_version.hash(&mut hasher);
    current_line.hash(&mut hasher);
    colors.hash(&mut hasher);
    hasher.finish()
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
    current_line: Option<u32>,
    severity_version: u64,
    cache: &mut Option<GutterGeometryCache>,
    scene: Option<&mut ApcScene>,
    buf: &mut Buffer,
    dim: f32,
) -> u16 {
    use crate::theme::scope as s;

    let visible = end_row.saturating_sub(scroll_row).min(inner.height as u32);

    // Background the rich gutter fills, and the target its foregrounds dim toward
    // so an unfocused pane's gutter fades with its text (`dim == 0.0` is identity).
    let rich_bg = style_rgb(
        fallback_style
            .bg
            .or_else(|| theme.try_get(s::UI_BACKGROUND).and_then(|st| st.bg)),
    );

    // Dimmed owned gutter colors, borrowed by the Copy `gutter_rgb` tuple and the
    // geometry key below. The key hashes them, so a dim change refills the cache.
    let diff_colors = {
        let base = DiffMarkColors::resolve(theme);
        match rich_bg {
            Some(bg) if stoatty => base.dim(bg, dim),
            _ => base,
        }
    };
    let dimmed_severity = match (stoatty, severity, rich_bg) {
        (true, Some(colors), Some(bg)) => Some(colors.dim(bg, dim)),
        _ => None,
    };

    // Rich mode needs stoatty, a scene, and every gutter color as RGB. The
    // colors resolve here, ahead of the scene, so the same values feed both the
    // cache key and the component-line rebuild.
    let gutter_rgb = stoatty
        .then(|| {
            let colors = dimmed_severity.as_ref()?;
            let number_fg = style_rgb(theme.get(s::UI_TEXT_MUTED).fg)?;
            let separator = style_rgb(theme.get(s::UI_BORDER_INACTIVE).fg).unwrap_or(number_fg);
            let bg = rich_bg?;
            Some((
                colors,
                dim_rgb(number_fg, bg, dim),
                dim_rgb(separator, bg, dim),
                bg,
            ))
        })
        .flatten();
    let rich = scene.filter(|_| stoatty).zip(gutter_rgb);

    let key = gutter_geometry_key(
        scroll_row,
        inner.width,
        visible,
        snapshot.buffer_snapshot().version(),
        snapshot.version(),
        snapshot.diff_map().map_or(0, |dm| dm.version()),
        severity_version,
        current_line,
        gutter_rgb.map(|(colors, _, _, bg)| (bg, diff_colors, colors)),
    );

    let stale = cache.as_ref().is_none_or(|c| c.key != key);
    if stale {
        let (folded, width_digits) = gutter_geometry(snapshot, scroll_row, visible);
        let marks = gutter_diff_marks(snapshot, &folded);
        let lines = match gutter_rgb {
            Some((colors, _, _, _)) => gutter_component_lines(
                &folded,
                row_severity,
                &marks,
                &diff_colors,
                colors,
                current_line,
            ),
            None => Vec::new(),
        };
        *cache = Some(GutterGeometryCache {
            key,
            folded,
            width_digits,
            marks,
            lines,
        });
    }
    let geometry = cache.as_ref().expect("set above");

    match rich {
        Some((scene, (_colors, number_fg, separator, bg))) => {
            let gutter = rich_gutter(
                &geometry.lines,
                geometry.width_digits,
                number_fg,
                separator,
                bg,
            );
            gutter.draw_components(inner, buf, scene);
            gutter.cell_width()
        },
        None => draw_fallback_line_numbers(
            &geometry.folded,
            geometry.width_digits,
            row_severity,
            &geometry.marks,
            current_line,
            inner,
            theme,
            buf,
        ),
    }
}

/// Paint right-aligned cell line numbers, a one-column severity mark left of the
/// number, and two diff glyph cells (change kind then staged state) right of it,
/// for a terminal without the sub-cell components. Returns the reserved cell
/// columns.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_fallback_line_numbers(
    folded: &[(u32, u16)],
    width_digits: u16,
    row_severity: &BTreeMap<u32, DiagnosticSeverity>,
    diff_marks: &BTreeMap<u32, (DiffHunkStatus, bool)>,
    current_line: Option<u32>,
    inner: Rect,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) -> u16 {
    use crate::theme::scope as s;
    let mark_w = 1u16;
    let gap = 1u16;
    let change_x = inner.x + mark_w + width_digits;
    let staged_x = change_x + 1;
    let width = mark_w + width_digits + 2 + gap;
    let number_style = theme.get(s::UI_TEXT_MUTED);

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
        let text = format!("{}", gutter_display_number(number, current_line));
        let start = inner.x + mark_w + width_digits.saturating_sub(text.len() as u16);
        buf.set_stringn(start, y, &text, text.len(), number_style);
        if let Some(&(status, staged)) = diff_marks.get(&(number - 1)) {
            let (mark, scope) = match status {
                DiffHunkStatus::Deleted => ('▔', s::DIFF_DELETED),
                DiffHunkStatus::Added => ('▎', s::DIFF_ADDED),
                DiffHunkStatus::Modified => ('▎', s::DIFF_MODIFIED),
                DiffHunkStatus::Moved => ('▎', s::DIFF_MOVED),
            };
            buf[(change_x, y)]
                .set_char(mark)
                .set_style(theme.get(scope));
            let staged_scope = if staged {
                s::DIFF_STAGED
            } else {
                s::DIFF_UNSTAGED
            };
            buf[(staged_x, y)]
                .set_char('▎')
                .set_style(theme.get(staged_scope));
        }
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
    spans: &[ResolvedDiag],
    visible: Range<usize>,
    rope: &Rope,
    snapshot: &DisplaySnapshot,
    theme: &crate::theme::Theme,
    fallback_style: Style,
    scroll_row: u32,
    end_row: u32,
    inner: Rect,
    right: u16,
    bottom: u16,
    buf: &mut Buffer,
    mut undercurls: Option<&mut Vec<UndercurlSpan>>,
    colors: Option<&SeverityColors>,
    dim: f32,
) {
    // An Unnecessary-tagged hint/info span blends the cell's syntax fg toward
    // this background rather than overwriting it. It resolves once, and the
    // dedup set keeps overlapping muted spans from double-blending a shared cell.
    let mute_bg = style_rgb(fallback_style.bg.or_else(|| {
        theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|s| s.bg)
    }));
    let mut muted_cells: HashSet<(u16, u16)> = HashSet::new();

    // Only spans overlapping the viewport can paint a cell, so the start-sorted
    // cache bounds the upper end with a partition_point. Paint the visible
    // subset least-severe first so the worst severity lands last, on top, for
    // both the cell foreground and the collected undercurl spans -- rust-analyzer
    // can publish a WARNING and a HINT over the same `unused` in any order, and
    // publish order alone would let the hint's grey win.
    let hi = spans.partition_point(|s| s.start < visible.end);
    let mut ordered: Vec<&ResolvedDiag> = spans[..hi]
        .iter()
        .filter(|s| s.start < s.end && s.end > visible.start)
        .collect();
    ordered.sort_by_key(|s| Reverse(severity_rank(s.severity)));

    for diag in ordered {
        let sev = diag.severity;
        // Clip to the visible bytes so offscreen columns are never walked. The
        // clamped range paints exactly the on-screen cells `paint_offset_range`
        // would have kept anyway.
        let start = diag.start.max(visible.start);
        let end = diag.end.min(visible.end);
        if start >= end {
            continue;
        }
        if diag.unnecessary
            && matches!(
                sev,
                DiagnosticSeverity::HINT | DiagnosticSeverity::INFORMATION
            )
        {
            // An inactive-code region mutes each cell's syntax fg toward the
            // background, with no underline and no undercurl. The dedup set
            // blends a cell shared by overlapping Unnecessary spans exactly once.
            let grey = theme.get(severity_scope(sev));
            paint_offset_range(
                rope,
                snapshot,
                start..end,
                None,
                &mut |x, y, cell| {
                    if !muted_cells.insert((x, y)) {
                        return;
                    }
                    match (mute_bg, cell.fg) {
                        (Some(bg), Color::Rgb(r, g, b)) => {
                            let [mr, mg, mb] = mute_rgb([r, g, b], bg);
                            cell.set_fg(Color::Rgb(mr, mg, mb));
                        },
                        _ => {
                            cell.set_style(grey);
                        },
                    }
                },
                scroll_row,
                end_row,
                inner,
                right,
                bottom,
                buf,
                None,
            );
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
            &mut |_, _, cell| {
                cell.set_style(style);
            },
            scroll_row,
            end_row,
            inner,
            right,
            bottom,
            buf,
            collect.then_some(&mut runs),
        );
        if let (Some(undercurls), Some(colors)) = (undercurls.as_deref_mut(), colors) {
            let base = severity_color(sev, colors);
            let color = match mute_bg {
                Some(bg) if dim > 0.0 => dim_rgb(base, bg, dim),
                _ => base,
            };
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
    spans: &[ResolvedDiag],
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

    // Line-based containment, matching the pre-cache scan: the winning span is
    // the worst-severity one whose LSP rows straddle the cursor line.
    let cursor_line = cursor_point.row;
    let Some(resolved) = spans
        .iter()
        .filter(|s| s.start_line <= cursor_line && cursor_line <= s.end_line)
        .min_by_key(|s| severity_rank(s.severity))
    else {
        return;
    };
    let index = resolved.index;
    // The popover already shows this diagnostic, so skip the redundant EOL text.
    if Some(index) == suppress {
        return;
    }

    let message = set.get(path)[index].message.lines().next().unwrap_or("");
    if message.is_empty() {
        return;
    }

    let sev = resolved.severity;
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
/// `spans` is [`resolve_diagnostic_spans`] output, sorted by start. A
/// `partition_point` bounds the scan to spans starting at or before `offset`.
/// The worst severity wins a tie, matching the gutter and the EOL message.
pub(crate) fn diagnostic_at_offset(spans: &[ResolvedDiag], offset: usize) -> Option<usize> {
    let hi = spans.partition_point(|s| s.start <= offset);
    spans[..hi]
        .iter()
        .filter(|s| s.start < s.end && offset < s.end)
        .min_by_key(|s| severity_rank(s.severity))
        .map(|s| s.index)
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

/// Place a `w` by `h` popover near `(anchor_col, anchor_row)` within `pane`
/// without covering the `cursor` cell.
///
/// Tries [`popover_rect`]'s below-the-anchor placement first, then above the
/// anchor, then a horizontal dodge to the left and right of the cursor column
/// at the below placement's row. Returns [`None`] when every candidate still
/// covers the cursor, since keeping the cursor visible outranks showing the
/// popover. With no `cursor`, this is [`popover_rect`].
fn popover_rect_avoiding(
    anchor_col: u16,
    anchor_row: u16,
    w: u16,
    h: u16,
    pane: Rect,
    cursor: Option<(u16, u16)>,
) -> Option<Rect> {
    let below = popover_rect(anchor_col, anchor_row, w, h, pane);
    let Some((cursor_col, cursor_row)) = cursor else {
        return Some(below);
    };
    let cursor = Position::new(cursor_col, cursor_row);
    if !below.contains(cursor) {
        return Some(below);
    }

    let w = w.min(pane.width);
    let h = h.min(pane.height);

    let above = {
        let max_y = (pane.y + pane.height).saturating_sub(h);
        Rect {
            x: below.x,
            y: anchor_row
                .saturating_sub(h)
                .clamp(pane.y, max_y.max(pane.y)),
            width: w,
            height: h,
        }
    };
    if !above.contains(cursor) {
        return Some(above);
    }

    let max_x = (pane.x + pane.width).saturating_sub(w);
    let left = Rect {
        x: cursor_col
            .saturating_sub(w)
            .clamp(pane.x, max_x.max(pane.x)),
        y: below.y,
        width: w,
        height: h,
    };
    if !left.contains(cursor) {
        return Some(left);
    }

    if cursor_col.saturating_add(1).saturating_add(w) <= pane.x + pane.width {
        let right = Rect {
            x: cursor_col + 1,
            y: below.y,
            width: w,
            height: h,
        };
        if !right.contains(cursor) {
            return Some(right);
        }
    }

    None
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
/// box is sized to fit and placed by [`popover_rect_avoiding`] so it never
/// covers `cursor_cell`. A message with no text, or a popover with nowhere to
/// go that clears the cursor, draws nothing.
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
    cursor_cell: Option<(u16, u16)>,
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
    let Some(rect) = popover_rect_avoiding(anchor_col, anchor_row, w, h, pane, cursor_cell) else {
        return false;
    };
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
        bold: false,
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
    apply: &mut dyn FnMut(u16, u16, &mut Cell),
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
            apply(x, y, &mut buf[(x, y)]);
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
    let line_count = snapshot.line_count();
    let row_offset = |row: u32| {
        if row >= line_count {
            return rope_len;
        }
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
    use lsp_types::{Diagnostic, DiagnosticSeverity, DiagnosticTag, Position, Range};
    use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
    use std::path::PathBuf;
    use stoat_action::{ExtendToLineEnd, MoveDown, MoveRight, OpenFile, OpenFileFinder};
    use stoat_config::{LineNumbers, WrapMode};
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

    fn open_search_buffer(h: &mut crate::test_harness::TestHarness, contents: &str) {
        let root = PathBuf::from("/search");
        let path = root.join("s.txt");
        h.fake_fs().insert_file(&path, contents.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
    }

    /// Render the focused editor with `query` active and return the cached match
    /// byte-ranges.
    fn render_search(stoat: &mut Stoat, area: Rect, query: &str) -> Vec<(usize, usize)> {
        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &mut buf,
            true,
            false,
            false,
            LineNumbers::Off,
            false,
            None,
            None,
            Some(query),
            None,
            None,
            None,
            0.0,
            WrapMode::None,
            80,
        );
        editor
            .search_match_cache
            .as_ref()
            .expect("a search render populates the cache")
            .matches
            .clone()
    }

    #[test]
    fn search_reuses_the_cached_regex_without_recompiling() {
        let mut h = Stoat::test();
        open_search_buffer(&mut h, "foo bar");
        let area = Rect::new(0, 0, 20, 4);

        assert_eq!(
            render_search(&mut h.stoat, area, "foo"),
            vec![(0, 3)],
            "the query matches foo"
        );

        // Swap the cached regex for one matching "bar" while keeping the query,
        // then force the stale path the way an edit would, with a bumped version.
        // A recompile from the query would match "foo". Reusing the swapped
        // object instead matches "bar".
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            let cache = editor.search_match_cache.as_mut().expect("cache set");
            cache.regex =
                Some(action_handlers::search::compile_search_regex("bar").expect("valid"));
            cache.version = cache.version.wrapping_sub(1);
        }

        assert_eq!(
            render_search(&mut h.stoat, area, "foo"),
            vec![(4, 7)],
            "the reused regex still matches bar, so the query was not recompiled"
        );
    }

    #[test]
    fn search_recompiles_on_a_new_query() {
        let mut h = Stoat::test();
        open_search_buffer(&mut h, "foo bar");
        let area = Rect::new(0, 0, 20, 4);

        assert_eq!(render_search(&mut h.stoat, area, "foo"), vec![(0, 3)]);
        assert_eq!(
            render_search(&mut h.stoat, area, "bar"),
            vec![(4, 7)],
            "a new query recompiles and matches the new pattern"
        );
    }

    #[test]
    fn inlay_hints_paint_in_the_virtual_style() {
        let mut h = Stoat::test();
        open_search_buffer(&mut h, "let x = 1");
        let theme = h.stoat.theme.clone();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let inlay_bg = theme
            .get(crate::theme::scope::UI_VIRTUAL_INLAY)
            .bg
            .expect("the default theme sets an inlay bg");

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let inserts = {
            let snapshot = editor.display_map.snapshot();
            let buf_snap = snapshot.buffer_snapshot();
            vec![(
                buf_snap.anchor_at(5, Bias::Left),
                ": i32".to_string(),
                crate::display_map::InlayKind::Hint,
            )]
        };
        editor.display_map.splice_inlays(Vec::new(), inserts);

        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &mut buf,
            true,
            false,
            false,
            LineNumbers::Off,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            0.0,
            WrapMode::None,
            80,
        );

        let italic: String = (0..area.width)
            .filter(|&x| buf[(x, 0)].modifier.contains(Modifier::ITALIC))
            .map(|x| buf[(x, 0)].symbol())
            .collect();
        assert_eq!(
            italic, ": i32",
            "the inlay hint renders italic while code stays upright"
        );

        let hint_x = (0..area.width)
            .find(|&x| buf[(x, 0)].modifier.contains(Modifier::ITALIC))
            .expect("a hint cell exists");
        assert_eq!(
            buf[(hint_x, 0)].bg,
            inlay_bg,
            "the hint carries the inlay background wash"
        );
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

    #[test]
    fn measure_gutter_width_matches_the_painted_fallback_gutter() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/measure-gutter");
        let path = root.join("a.txt");
        let body: String = (0..120).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 6);

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        assert_eq!(
            super::gutter_width_digits(&snapshot),
            3,
            "120 lines need three digits",
        );
        assert_eq!(
            super::measure_gutter_width(&snapshot, false),
            editor.gutter_width,
            "the measured fallback width matches the painted gutter",
        );
    }

    #[test]
    fn fallback_gutter_paints_change_and_staged_glyph_cells() {
        use crate::diff_map::DiffHunkStatus;
        let theme = crate::theme::Theme::empty();
        let folded = [(1u32, 1u16), (2, 1)];
        let mut diff_marks = std::collections::BTreeMap::new();
        diff_marks.insert(0u32, (DiffHunkStatus::Modified, false));
        let area = Rect::new(0, 0, 12, 2);
        let mut buf = Buffer::empty(area);

        let width = super::draw_fallback_line_numbers(
            &folded,
            1,
            &std::collections::BTreeMap::new(),
            &diff_marks,
            None,
            area,
            &theme,
            &mut buf,
        );

        assert_eq!(width, 5, "mark cell, one digit, two glyph cells, and a gap");
        assert_eq!(
            buf[(2u16, 0u16)].symbol(),
            "▎",
            "the change-kind glyph sits right of the number",
        );
        assert_eq!(
            buf[(3u16, 0u16)].symbol(),
            "▎",
            "the staged-state glyph sits right of the change glyph",
        );
        assert_eq!(
            buf[(2u16, 1u16)].symbol(),
            " ",
            "a row with no diff mark leaves the change cell blank",
        );
        assert_eq!(
            buf[(3u16, 1u16)].symbol(),
            " ",
            "a row with no diff mark leaves the staged cell blank",
        );
    }

    /// Open a single 200-column line with no trailing newline, so any wrapping
    /// splits it across display rows.
    fn open_long_line(h: &mut crate::test_harness::TestHarness) {
        let root = PathBuf::from("/wrap");
        let path = root.join("long.txt");
        h.fake_fs().insert_file(&path, "a".repeat(200).as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
    }

    /// Render the focused editor with `wrap` and return its stamped wrap width
    /// alongside the resulting display and buffer line counts.
    fn wrap_after_render(
        stoat: &mut Stoat,
        area: Rect,
        wrap: WrapMode,
        wrap_column: u32,
    ) -> (Option<u32>, u32, u32) {
        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &mut buf,
            true,
            false,
            false,
            LineNumbers::Off,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            0.0,
            wrap,
            wrap_column,
        );
        let snapshot = editor.display_map.snapshot();
        (
            editor.display_map.wrap_width(),
            snapshot.line_count(),
            snapshot.buffer_line_count(),
        )
    }

    #[test]
    fn editor_width_wrap_splits_a_long_line() {
        let mut h = Stoat::test();
        open_long_line(&mut h);
        let area = Rect::new(0, 0, 40, 10);
        let (width, display_rows, buffer_rows) =
            wrap_after_render(&mut h.stoat, area, WrapMode::EditorWidth, 80);
        assert_eq!(width, Some(40), "editor_width wraps at the pane text width");
        assert_eq!(buffer_rows, 1, "the buffer is one long line");
        assert_eq!(display_rows, 5, "200 columns wrap into five 40-column rows");
    }

    #[test]
    fn wrap_override_forces_wrap_off_then_restores() {
        let mut h = Stoat::test();
        open_long_line(&mut h);
        let area = Rect::new(0, 0, 40, 10);

        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .wrap_override = Some(WrapMode::None);
        let (off_width, off_rows, buffer_rows) =
            wrap_after_render(&mut h.stoat, area, WrapMode::EditorWidth, 80);
        assert_eq!(
            off_width, None,
            "a wrap-off override truncates even under the editor_width frame",
        );
        assert_eq!(
            off_rows, buffer_rows,
            "the long line stays on its single row"
        );

        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .wrap_override = None;
        let (on_width, on_rows, _) =
            wrap_after_render(&mut h.stoat, area, WrapMode::EditorWidth, 80);
        assert_eq!(
            on_width,
            Some(40),
            "clearing the override follows the frame again"
        );
        assert!(
            on_rows > buffer_rows,
            "the line wraps once the override is cleared"
        );
    }

    #[test]
    fn wrap_none_leaves_a_long_line_on_one_row() {
        let mut h = Stoat::test();
        open_long_line(&mut h);
        let area = Rect::new(0, 0, 40, 10);
        let (width, display_rows, buffer_rows) =
            wrap_after_render(&mut h.stoat, area, WrapMode::None, 80);
        assert_eq!(width, None, "none disables wrapping");
        assert_eq!(
            display_rows, buffer_rows,
            "the long line keeps its single row and truncates",
        );
    }

    #[test]
    fn bounded_wrap_caps_at_the_wrap_column() {
        let mut h = Stoat::test();
        open_long_line(&mut h);
        let area = Rect::new(0, 0, 40, 10);
        let (width, display_rows, _) = wrap_after_render(&mut h.stoat, area, WrapMode::Bounded, 20);
        assert_eq!(
            width,
            Some(20),
            "bounded caps at the wrap column below the pane width",
        );
        assert_eq!(display_rows, 10, "200 columns wrap into ten 20-column rows");
    }

    #[test]
    fn an_unrendered_editor_has_no_wrap_width() {
        let mut h = Stoat::test();
        open_long_line(&mut h);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert_eq!(
            editor.display_map.wrap_width(),
            None,
            "an editor never rendered has no pane width to wrap at",
        );
    }

    #[test]
    fn wrapped_continuation_row_paints_the_parent_indent() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/wrap-indent");
        let path = root.join("a.txt");
        let body = format!("    {}", "word ".repeat(20));
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            super::render_editor_with_overlay(
                editor,
                area,
                fallback,
                &theme,
                &mut buf,
                true,
                false,
                false,
                LineNumbers::Off,
                false,
                None,
                None,
                None,
                None,
                None,
                None,
                0.0,
                WrapMode::EditorWidth,
                80,
            );
        }

        let row_text = |y: u16| -> String {
            (0..area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect()
        };
        let continuation = row_text(1);
        assert!(
            continuation.starts_with("    ") && !continuation.trim_start().is_empty(),
            "the continuation row is indented under the parent's whitespace: {continuation:?}",
        );
    }

    /// Paint the focused editor's line-number gutter and return its
    /// geometry-cache key.
    fn paint_gutter_key(stoat: &mut Stoat, rows: u16) -> u64 {
        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let area = Rect::new(0, 0, 12, rows);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &mut buf,
            true,
            false,
            false,
            LineNumbers::Absolute,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            0.0,
            WrapMode::None,
            80,
        );
        editor
            .gutter_geometry_cache
            .as_ref()
            .expect("gutter cache set")
            .key
    }

    /// The focused editor's cached folded gutter lines.
    ///
    /// Clearing them is a rebuild sentinel. The next paint either reuses the
    /// cache and leaves them empty, or rebuilds the geometry and repopulates
    /// them.
    fn cached_folded(stoat: &mut Stoat) -> &mut Vec<(u32, u16)> {
        &mut action_handlers::focused_editor_mut(stoat)
            .unwrap()
            .gutter_geometry_cache
            .as_mut()
            .unwrap()
            .folded
    }

    #[test]
    fn gutter_geometry_cache_reuses_until_an_input_changes() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/gutter-cache");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let key = paint_gutter_key(&mut h.stoat, 5);
        cached_folded(&mut h.stoat).clear();

        assert_eq!(
            paint_gutter_key(&mut h.stoat, 5),
            key,
            "an identical paint keeps the cache key"
        );
        assert!(
            cached_folded(&mut h.stoat).is_empty(),
            "an identical paint reuses the cached geometry instead of rebuilding it"
        );

        action_handlers::focused_editor_mut(&mut h.stoat)
            .unwrap()
            .scroll_row = 1;

        assert_ne!(
            paint_gutter_key(&mut h.stoat, 5),
            key,
            "a scroll changes the cache key"
        );
        assert!(
            !cached_folded(&mut h.stoat).is_empty(),
            "an invalidated cache rebuilds the geometry"
        );
    }

    /// Render the focused editor's gutter in fallback (non-stoatty) mode and
    /// return the trimmed number string each visible row paints.
    fn rendered_gutter(
        stoat: &mut Stoat,
        is_focused: bool,
        insert_mode: bool,
        line_numbers: LineNumbers,
        rows: u16,
    ) -> Vec<String> {
        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let area = Rect::new(0, 0, 12, rows);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &mut buf,
            is_focused,
            false,
            false,
            line_numbers,
            insert_mode,
            None,
            None,
            None,
            None,
            None,
            None,
            0.0,
            WrapMode::None,
            80,
        );
        let gutter_w = editor.gutter_width;
        (0..rows)
            .map(|y| {
                (0..gutter_w)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
                    .trim()
                    .to_string()
            })
            .collect()
    }

    /// Render the focused editor at `width` x `rows` under the given stoatty and
    /// minimap flags, returning the recorded strip rect and, per row, the
    /// symbols painted in the rightmost [`super::MINIMAP_STRIP_COLS`] columns.
    fn render_minimap(
        stoat: &mut Stoat,
        stoatty: bool,
        minimap_enabled: bool,
        width: u16,
        rows: u16,
    ) -> (Option<Rect>, Vec<String>) {
        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let area = Rect::new(0, 0, width, rows);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &mut buf,
            true,
            stoatty,
            minimap_enabled,
            LineNumbers::Off,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            0.0,
            WrapMode::None,
            80,
        );
        let rect = editor.minimap_rect;
        let strip = (0..rows)
            .map(|y| {
                ((width - super::MINIMAP_STRIP_COLS)..width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        (rect, strip)
    }

    #[test]
    fn minimap_strip_reserves_right_edge_under_stoatty() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/minimap");
        let path = root.join("a.txt");
        let line = "x".repeat(100);
        let body = format!("{line}\n{line}\n{line}");
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let (rect, strip) = render_minimap(&mut h.stoat, true, true, 80, 3);
        assert_eq!(
            rect,
            Some(Rect::new(72, 0, super::MINIMAP_STRIP_COLS, 3)),
            "strip pins to the right edge at full width"
        );
        assert!(
            strip.iter().all(|row| row.chars().all(|c| c == ' ')),
            "text never paints into the reserved strip: {strip:?}"
        );
    }

    #[test]
    fn minimap_strip_absent_when_disabled_or_narrow() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/minimap-off");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"one\ntwo\nthree");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        assert_eq!(
            render_minimap(&mut h.stoat, false, true, 80, 3).0,
            None,
            "no strip outside stoatty"
        );
        assert_eq!(
            render_minimap(&mut h.stoat, true, false, 80, 3).0,
            None,
            "no strip when the minimap is disabled"
        );
        assert_eq!(
            render_minimap(&mut h.stoat, true, true, 50, 3).0,
            None,
            "no strip below the minimum pane width"
        );
    }

    #[test]
    fn relative_line_numbers_center_on_the_cursor_line() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/relnum");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &MoveDown);

        // When focused and in normal mode, the cursor's line keeps its absolute
        // number (3) and every other line shows its distance from it.
        assert_eq!(
            rendered_gutter(&mut h.stoat, true, false, LineNumbers::Relative, 5),
            ["2", "1", "3", "1", "2"],
        );
    }

    /// Render the focused editor's gutter in fallback mode and return each
    /// visible row's leftmost mark glyph paired with whether it is dimmed.
    /// The fallback gutter's per-row `(change glyph, staged glyph color)`, read
    /// from the two diff cells right of the number. Uses the active theme so the
    /// staged and unstaged scopes resolve to distinct colors.
    fn gutter_mark_cells(stoat: &mut Stoat, rows: u16) -> Vec<(String, ratatui::style::Color)> {
        let theme = stoat.theme.clone();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let area = Rect::new(0, 0, 12, rows);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &mut buf,
            true,
            false,
            false,
            LineNumbers::Absolute,
            false,
            None,
            None,
            None,
            None,
            None,
            None,
            0.0,
            WrapMode::None,
            80,
        );
        let change_x = editor.gutter_width - 3;
        let staged_x = change_x + 1;
        (0..rows)
            .map(|y| {
                (
                    buf[(change_x, y)].symbol().to_string(),
                    buf[(staged_x, y)].fg,
                )
            })
            .collect()
    }

    #[test]
    fn gutter_marks_modified_lines_with_a_distinct_staged_glyph() {
        let mut h = Stoat::test();
        h.stage_index_scenario(
            "/repo",
            &[("f.txt", "a\nb\nc\nd\n", "a\nB\nc\nd\n", "a\nB\nc\nD\n")],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(std::path::Path::new("/repo/f.txt"));
        h.settle_diff_jobs();

        let cells = gutter_mark_cells(&mut h.stoat, 6);
        // Line 2 (b -> B) is staged in the index; line 4 (d -> D) is unstaged.
        assert_eq!(
            cells[1].0, "▎",
            "a modified line shows a change glyph: {cells:?}"
        );
        assert_eq!(
            cells[3].0, "▎",
            "a modified line shows a change glyph: {cells:?}"
        );
        assert_ne!(
            cells[1].1, cells[3].1,
            "the staged glyph color distinguishes staged from unstaged: {cells:?}",
        );
    }

    #[test]
    fn gutter_marks_a_deletion_seam() {
        let mut h = Stoat::test();
        h.stage_index_scenario(
            "/repo",
            &[("f.txt", "a\nb\nc\nd\n", "a\nb\nc\nd\n", "a\nb\nd\n")],
        );
        h.stoat.set_diff_warm_auto(true);
        h.open_file(std::path::Path::new("/repo/f.txt"));
        h.settle_diff_jobs();

        let cells = gutter_mark_cells(&mut h.stoat, 6);
        assert!(
            cells.iter().any(|(mark, _)| mark == "▔"),
            "the row below the deleted line carries the seam mark: {cells:?}",
        );
    }

    #[test]
    fn rich_gutter_change_bar_by_status_staged_bar_by_state() {
        use crate::diff_map::DiffHunkStatus;
        let folded = [(1u32, 1u16), (2, 1), (3, 1)];
        let diff_colors = super::DiffMarkColors {
            added: [10, 20, 30],
            modified: [40, 50, 60],
            moved: [70, 80, 90],
            deleted: [100, 110, 120],
            staged: [1, 2, 3],
            unstaged: [4, 5, 6],
        };
        let severity = super::SeverityColors {
            error: [0, 0, 0],
            warning: [0, 0, 0],
            info: [0, 0, 0],
            hint: [0, 0, 0],
        };
        let mut diff_marks = std::collections::BTreeMap::new();
        diff_marks.insert(0, (DiffHunkStatus::Modified, false));
        diff_marks.insert(1, (DiffHunkStatus::Modified, true));
        diff_marks.insert(2, (DiffHunkStatus::Deleted, false));

        let lines = super::gutter_component_lines(
            &folded,
            &std::collections::BTreeMap::new(),
            &diff_marks,
            &diff_colors,
            &severity,
            None,
        );

        let git = |i: usize| lines[i].git.expect("a marked row has a git mark");
        assert_eq!(
            git(0).color,
            git(1).color,
            "the change bar keeps the status color whether staged or not",
        );
        assert_eq!(git(0).color, [40, 50, 60], "modified takes diff.modified");
        assert_eq!(
            git(0).staged_color,
            [4, 5, 6],
            "an unstaged row's staged bar takes diff.unstaged",
        );
        assert_eq!(
            git(1).staged_color,
            [1, 2, 3],
            "a staged row's staged bar takes diff.staged",
        );
        assert!(git(2).seam, "a deletion is a seam mark");
    }

    #[test]
    fn relative_line_numbers_fall_back_to_absolute() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/relnum-abs");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        dispatch(&mut h.stoat, &MoveDown);
        dispatch(&mut h.stoat, &MoveDown);

        let absolute = ["1", "2", "3", "4", "5"];
        assert_eq!(
            rendered_gutter(&mut h.stoat, true, true, LineNumbers::Relative, 5),
            absolute,
            "insert mode paints absolute"
        );
        assert_eq!(
            rendered_gutter(&mut h.stoat, false, false, LineNumbers::Relative, 5),
            absolute,
            "an unfocused pane paints absolute"
        );
        assert_eq!(
            rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 5),
            absolute,
            "the Absolute setting paints absolute"
        );
    }

    #[test]
    fn scratch_gutter_numbers_only_the_real_line() {
        let mut h = Stoat::test();
        // A bare scratch is a seeded "\n": one real line plus the phantom line
        // the trailing newline creates. The phantom row stays blank.
        assert_eq!(
            rendered_gutter(&mut h.stoat, true, false, LineNumbers::Relative, 2),
            ["1", ""],
        );
    }

    #[test]
    fn absolute_gutter_skips_the_phantom_final_line() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/phantom-abs");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"one\ntwo\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
        assert_eq!(
            rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 3),
            ["1", "2", ""],
            "the two real lines are numbered and the phantom row is blank",
        );
    }

    #[test]
    fn trailing_newline_does_not_widen_the_gutter() {
        let width_of = |contents: &[u8]| {
            let mut h = Stoat::test();
            let root = PathBuf::from("/gutter-width");
            let path = root.join("a.txt");
            h.fake_fs().insert_file(&path, contents);
            h.stoat.active_workspace_mut().git_root = root;
            dispatch(&mut h.stoat, &OpenFile { path });
            h.settle();
            rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 5);
            action_handlers::focused_editor_mut(&mut h.stoat)
                .expect("focused editor")
                .gutter_width
        };
        // 99 real lines: a trailing newline pushes the rope line count to 100,
        // but the phantom line is excluded, so the width stays 2-digit rather
        // than widening to 3 digits.
        let with_newline = "x\n".repeat(99);
        let without_newline = format!("{}x", "x\n".repeat(98));
        assert_eq!(
            width_of(with_newline.as_bytes()),
            width_of(without_newline.as_bytes()),
            "the trailing newline does not widen the gutter"
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

        let spans =
            super::resolve_diagnostic_spans(&set, &path, &rope, &super::DiagnosticEncodings::new());
        // Offset 4 is in both, so the worse severity (the error) wins.
        assert_eq!(super::diagnostic_at_offset(&spans, 4), Some(1));
        // Offset 7 is inside only the error span.
        assert_eq!(super::diagnostic_at_offset(&spans, 7), Some(1));
        // Offset 0 is outside both.
        assert_eq!(super::diagnostic_at_offset(&spans, 0), None);
    }

    #[test]
    fn resolve_diagnostic_spans_uses_each_servers_encoding() {
        use crate::host::OffsetEncoding;
        use stoat_text::Rope;

        // "éxy": é is two UTF-8 bytes but one UTF-16 unit, so char 2 is byte 2
        // (x) under UTF-8 and byte 3 (y) under UTF-16.
        let rope = Rope::from("\u{e9}xy\n");
        let path = PathBuf::from("/a");
        let mut set = crate::diagnostics::DiagnosticSet::new();
        set.replace_from_server(
            path.clone(),
            "utf8".into(),
            vec![span_diag(2, 3, DiagnosticSeverity::ERROR)],
        );
        set.replace_from_server(
            path.clone(),
            "utf16".into(),
            vec![span_diag(2, 3, DiagnosticSeverity::ERROR)],
        );
        let mut encodings = super::DiagnosticEncodings::new();
        encodings.insert("utf8".into(), OffsetEncoding::Utf8);
        encodings.insert("utf16".into(), OffsetEncoding::Utf16);

        let spans = super::resolve_diagnostic_spans(&set, &path, &rope, &encodings);
        let starts: Vec<usize> = spans.iter().map(|s| s.start).collect();
        assert_eq!(
            starts,
            vec![2, 3],
            "utf-8 char 2 lands on x (byte 2), utf-16 char 2 on y (byte 3)"
        );
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
    fn popover_rect_avoiding_dodges_the_cursor() {
        use ratatui::layout::Rect;
        let pane = Rect::new(0, 0, 40, 10);

        // With no cursor it reproduces popover_rect's below/flip/clamp result.
        for &(col, row) in &[(5, 2), (5, 8), (35, 2)] {
            assert_eq!(
                super::popover_rect_avoiding(col, row, 12, 4, pane, None),
                Some(super::popover_rect(col, row, 12, 4, pane)),
            );
        }

        // Cursor inside the below rect flips the popover above the anchor.
        assert_eq!(
            super::popover_rect_avoiding(5, 2, 12, 4, pane, Some((8, 4))),
            Some(Rect::new(5, 0, 12, 4)),
        );

        // Cursor covered by both below and above, near the left edge, dodges right.
        assert_eq!(
            super::popover_rect_avoiding(5, 2, 12, 4, pane, Some((8, 3))),
            Some(Rect::new(9, 3, 12, 4)),
        );

        // Same, near the right edge, dodges left.
        assert_eq!(
            super::popover_rect_avoiding(35, 2, 12, 4, pane, Some((35, 3))),
            Some(Rect::new(23, 3, 12, 4)),
        );

        // A full-width popover cannot dodge, so a covered cursor drops it.
        let narrow = Rect::new(0, 0, 12, 10);
        assert_eq!(
            super::popover_rect_avoiding(0, 2, 12, 4, narrow, Some((5, 3))),
            None,
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

    fn tagged_overlap_diag(line: u32, start: u32, end: u32, sev: DiagnosticSeverity) -> Diagnostic {
        Diagnostic {
            tags: Some(vec![DiagnosticTag::UNNECESSARY]),
            ..overlap_diag(line, start, end, sev)
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
    fn snapshot_diagnostic_unnecessary_mutes_syntax() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-unnecessary");
        let path = root.join("a.rs");
        h.fake_fs()
            .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        // An Unnecessary-tagged hint marks inactive code. Its span blends each
        // token's syntax fg toward the background, keeping the per-token hues
        // rather than flattening the line to one hint color or underlining it.
        h.stoat.diagnostics.replace_for_path(
            path,
            vec![tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT)],
        );
        h.assert_snapshot("diagnostic_unnecessary_mutes_syntax");
    }

    #[test]
    fn snapshot_diagnostic_warning_over_unnecessary_still_underlines() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-warn-over-unnecessary");
        let path = root.join("a.rs");
        h.fake_fs()
            .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        // A warning overlapping an inactive-code region sorts last and paints on
        // top, so its underline lands over the muted span rather than being
        // erased by the mute.
        h.stoat.diagnostics.replace_for_path(
            path,
            vec![
                tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT),
                overlap_diag(1, 4, 5, DiagnosticSeverity::WARNING),
            ],
        );
        h.assert_snapshot("diagnostic_warning_over_unnecessary_still_underlines");
    }

    #[test]
    fn unnecessary_span_blends_a_shared_cell_once() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-unnecessary-dedup");
        let path = root.join("a.rs");
        h.fake_fs()
            .insert_file(&path, b"let x = 1;\nlet y = 2;\nlet z = 3;\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        let row_fg = |stoat: &mut Stoat| {
            let buf = stoat.render();
            (0..buf.area.width)
                .map(|x| buf[(x, 1)].fg)
                .collect::<Vec<_>>()
        };

        h.stoat.diagnostics.replace_for_path(
            path.clone(),
            vec![tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT)],
        );
        let once = row_fg(&mut h.stoat);

        h.stoat.diagnostics.replace_for_path(
            path,
            vec![
                tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT),
                tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT),
            ],
        );
        let twice = row_fg(&mut h.stoat);

        assert_eq!(
            once, twice,
            "overlapping muted spans must blend a shared cell exactly once"
        );
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
        h.stoat.settings.editor_minimap = Some(stoat_config::MinimapMode::PerPane);
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
        // Column 4 is the line-number gutter width the cursor sits past.
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((4, 0)));

        for _ in 0..6 {
            dispatch(&mut h.stoat, &MoveRight);
        }
        h.snapshot();
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((10, 0)));

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
        // Column 4 is the line-number gutter width the cursor sits past.
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((4, 0)));

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
