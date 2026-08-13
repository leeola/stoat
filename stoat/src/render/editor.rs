use super::TEXT_SCALE_COMPACT;
use crate::{
    diff_map::DiffHunkStatus,
    display_map::{display_width, tab_map, BlockRowKind, DisplayPoint, DisplaySnapshot, TabPoint},
    editor_state::{EditorState, SearchMatchCache},
    minimap::color_to_rgb,
    render::{
        conflict_view::render_conflict_view,
        review::{dim_rgb, render_diff_view, render_review, style_rgb},
        undercurl::UndercurlBatch,
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
    collections::{hash_map::DefaultHasher, BTreeMap, HashSet},
    fmt::Write,
    hash::{Hash, Hasher},
    ops::Range,
    path::Path,
    sync::Arc,
};
use stoat_config::{LineNumbers, WrapMode};
use stoat_text::{cursor_offset, Anchor, Bias, Point, Rope};
use stoat_widgets::{
    bar::Bar,
    gutter::{Diagnostic, GitMark, Gutter, GutterLine},
    icon::Icon,
    popover::Popover,
    ApcScene,
};
use stoatty_protocol::command::IconKind;

/// Columns reserved on a pane's right edge for the minimap strip, matching the
/// width the terminal's GPU minimap pass paints there.
pub(super) const MINIMAP_STRIP_COLS: u16 = 8;

/// Narrowest pane, in columns, that still reserves a minimap strip.
///
/// At this width 100 text columns remain beside the 8-column strip. Anything
/// narrower is around half a screen or less, where the strip would crowd the
/// text, so those panes keep their full width instead.
///
/// The gates reading this measure slightly different widths. The per-pane gate
/// compares the width left after the gutter, while the Single-mode band
/// compares the whole window width and so runs a few columns looser. One
/// shared constant is worth that imprecision.
pub(super) const MINIMAP_MIN_PANE_COLS: u16 = 108;

/// Paint an editor with none of a pane's decoration: no gutter, no diagnostics,
/// no minimap, no soft wrap.
///
/// For the editors that are not panes. Modal inputs, the dock, the finder
/// preview, and the rename and reword popups all paint their text this way.
///
/// `chrome` is the frame's resolved theme colors. Passing them costs nothing
/// where a caller already holds a set, and resolving one is around thirty scope
/// lookups that a caller inside a frame has already paid for. `None` resolves a
/// fresh set, for a caller outside any frame.
pub(crate) fn render_editor(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &crate::theme::Theme,
    chrome: Option<&ResolvedChrome>,
    buf: &mut Buffer,
    is_focused: bool,
) {
    let resolved;
    let chrome = match chrome {
        Some(chrome) => chrome,
        None => {
            resolved = ResolvedChrome::resolve(theme);
            &resolved
        },
    };

    render_editor_with_overlay(
        editor,
        inner,
        fallback_style,
        theme,
        chrome,
        buf,
        is_focused,
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
    chrome: &ResolvedChrome,
    buf: &mut Buffer,
    is_focused: bool,
    minimap_enabled: bool,
    line_numbers: LineNumbers,
    insert_mode: bool,
    hover_cell: Option<(u16, u16)>,
    goto_word_labels: Option<&BTreeMap<String, usize>>,
    search_query: Option<&str>,
    diagnostic_info: Option<(&Path, &crate::diagnostics::DiagnosticSet)>,
    mut scene: Option<&mut ApcScene>,
    undercurls: Option<&mut UndercurlBatch>,
    dim: f32,
    wrap: WrapMode,
    wrap_column: u32,
) {
    editor.viewport_rows = Some(inner.height as u32);
    editor.cursor_screen_cell = None;
    editor.minimap_rect = None;

    if editor.review_view.is_some() {
        editor.display_map.set_wrap_width(None);
        render_review(editor, inner, fallback_style, theme, buf, scene);
        return;
    }

    if editor.diff_view {
        editor.display_map.set_wrap_width(None);
        render_diff_view(editor, inner, fallback_style, theme, buf, scene);
        return;
    }

    if editor.conflict_view.is_some() {
        editor.display_map.set_wrap_width(None);
        // A scene means the terminal draws the cursor, matching the delegation
        // the main editor path makes on the same condition.
        let delegates_cursor = scene.is_some();
        render_conflict_view(editor, inner, fallback_style, theme, buf, delegates_cursor);
        return;
    }

    // The gutter measurement and the wrap-width decision ask only buffer-level
    // questions, so they run off a buffer snapshot. A display snapshot taken
    // here would be wasted work, since set_wrap_width below invalidates it and
    // the one the gutter and text paint from re-syncs the wrap from scratch.
    let gutter_is_rich = scene.is_some() && chrome.rich_gutter.is_some();
    let measured_gutter_w = if line_numbers != LineNumbers::Off {
        let buffer = editor.display_map.buffer_snapshot();
        measure_gutter_width(
            gutter_digits(buffer.line_count(), buffer.rope().max_point()),
            gutter_is_rich,
        )
    } else {
        match diagnostic_info {
            Some((path, set)) if !set.get(path).is_empty() => 1,
            _ => 0,
        }
    };

    let after_gutter = inner.width.saturating_sub(measured_gutter_w);
    let minimap_cols = if minimap_enabled && after_gutter >= MINIMAP_MIN_PANE_COLS {
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
        Some((path, set)) => {
            let version = set.version();
            let buffer_version = snapshot.buffer_snapshot().version();
            let stale = match &editor.gutter_severity_cache {
                Some(cache) => cache.version != version || cache.buffer_version != buffer_version,
                None => true,
            };
            if stale {
                // The marks and the underline read the same resolution, so they
                // cannot disagree about where a diagnostic sits.
                build_diagnostic_span_cache(editor, set, path, snapshot.buffer_snapshot());
                let map = editor
                    .diagnostic_span_cache
                    .as_ref()
                    .map(|cache| row_severity_from_spans(&cache.spans))
                    .unwrap_or_default();
                editor.gutter_severity_cache = Some(GutterSeverityCache {
                    version,
                    buffer_version,
                    map: Arc::new(map),
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
    let severity = chrome.severity.clone();
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

    let severity_version = diagnostic_info.map_or(0, |(_, set)| set.version());

    let gutter_w = if line_numbers != LineNumbers::Off {
        draw_line_number_gutter(
            &snapshot,
            editor.scroll_row,
            inner,
            end_row,
            row_severity,
            theme,
            chrome,
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
        let rows = diagnostic_rows(
            &snapshot,
            row_severity,
            editor.scroll_row,
            inner,
            end_row,
            severity_version,
            &mut editor.diagnostic_rows_cache,
        );
        // Rich mode emits a sub-cell severity bar per row instead of the glyph,
        // engaging only with a scene and every severity color resolved to RGB.
        let rich = scene.as_deref_mut().zip(severity.as_ref());
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
                for &(row_offset, sev) in rows {
                    let color = match bar_bg {
                        Some(bg) if dim > 0.0 => dim_rgb(severity_color(sev, colors), bg, dim),
                        _ => severity_color(sev, colors),
                    };
                    Bar {
                        x: 0,
                        y: (row_offset * 16) as i16,
                        width: 6,
                        height: 16,
                        color,
                    }
                    .render(area, buf, &mut *scene);
                }
            },
            None => paint_diagnostic_gutter(rows, inner.x, inner.y, theme, buf),
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

    // Reserve the right-edge minimap strip, recording its screen rect for
    // pointer mapping. Only the text rect shrinks to clear the space. The
    // reserved cells stay blank so the terminal's GPU minimap pass owns them.
    let inner = if minimap_enabled && inner.width >= MINIMAP_MIN_PANE_COLS {
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
        // Cell holding the character a combining mark would attach to. It
        // cannot be derived from `x` once that has advanced, since how far back
        // the base sits depends on how wide it was.
        //
        // Cleared at a row's start, where a mark has nothing before it to join,
        // and after a base too wide for the room left. Only a base that did not
        // fit puts `x` past the edge, so the skip at the top of the loop
        // swallows anything after it and that second case is unreachable, but
        // reading a cell no character went into is not a thing to leave resting
        // on a guard elsewhere.
        let mut base_cell: Option<u16> = None;
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
            let mut rest: &str = &chunk.text;
            while let Some(ch) = rest.chars().next() {
                // `x` only grows within a line and only resets on a newline, so
                // once it reaches the pane edge nothing on the rest of the line
                // can paint. Jump straight to the newline rather than stepping
                // an over-wide line's remainder every frame. A chunk holding no
                // newline ends here, and the line's next chunk repeats the check.
                if x >= right && ch != '\n' {
                    match memchr::memchr(b'\n', rest.as_bytes()) {
                        Some(nl) => rest = &rest[nl..],
                        None => break,
                    }
                    continue;
                }
                rest = &rest[ch.len_utf8()..];
                if ch == '\n' {
                    y += 1;
                    x = inner.x;
                    base_cell = None;
                    if y >= bottom {
                        break 'chunks;
                    }
                    continue;
                }
                let w = display_width(ch);
                // A mark occupies no cell of its own, being drawn on the
                // character before it. A cell carries a whole cluster, so it
                // joins the one its base went into rather than being dropped,
                // which would leave the screen saying something the buffer does
                // not.
                if w == 0 {
                    if let Some(base) = base_cell {
                        let cell = &mut buf[(base, y)];
                        let mut symbol = String::from(cell.symbol());
                        symbol.push(ch);
                        cell.set_symbol(&symbol);
                    }
                    continue;
                }
                if x + w as u16 <= right {
                    buf[(x, y)].set_char(ch).set_style(style);
                    // A double-width glyph occupies two cells. Clear the second
                    // so stale content under it does not show through.
                    if w == 2 {
                        buf[(x + 1, y)].set_char(' ').set_style(style);
                    }
                    base_cell = Some(x);
                } else {
                    base_cell = None;
                }
                x += w as u16;
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

    if let Some((path, set)) = diagnostic_info {
        let rope = buffer_snapshot.rope();
        build_diagnostic_span_cache(editor, set, path, buffer_snapshot);
        // The cache and the scratch are disjoint fields, so the paint borrows one
        // of each rather than the editor twice.
        let EditorState {
            diagnostic_span_cache,
            diagnostic_paint_scratch,
            scroll_row,
            ..
        } = editor;
        if let Some(cache) = diagnostic_span_cache.as_ref() {
            paint_diagnostic_spans(
                cache,
                diagnostic_paint_scratch,
                visible.clone(),
                rope,
                &snapshot,
                theme,
                fallback_style,
                *scroll_row,
                end_row,
                inner,
                right,
                bottom,
                buf,
                undercurls,
                severity.as_ref(),
                dim,
            );
        }
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
    let cursor_style = theme.cursor_style();
    let primary_id = editor.selections.newest_anchor().id;
    let mut primary_cell: Option<(u16, u16)> = None;
    // A scene means the terminal draws the primary cursor itself, so this pass
    // records the cell and leaves it unpainted. Without one, as in an input view
    // or dock, nothing downstream would draw it, so it is painted here.
    let delegates_cursor = scene.is_some();
    let rope = buffer_snapshot.rope();

    // The selections that can reach the viewport. `disjoint` is start-sorted and
    // non-overlapping, so ends ascend with starts and both bounds are monotone
    // over it. Every selection outside this window paints nothing and has no
    // cursor to draw, which a set far longer than the screen is mostly made of.
    //
    // The bounds touch rather than exclude, since a zero-width selection sitting
    // on either edge still has a cursor there. That over-includes by at most one
    // selection per side, and an over-included one costs the same display
    // conversion every selection used to pay.
    let visible_selections = {
        let all = editor.selections.all_anchors();
        let first =
            all.partition_point(|sel| buffer_snapshot.resolve_anchor(&sel.end) < visible.start);
        let last =
            all.partition_point(|sel| buffer_snapshot.resolve_anchor(&sel.start) <= visible.end);
        &all[first..last.max(first)]
    };

    // Every endpoint in one walk. Per-anchor resolution descends the fragment
    // tree from the root, so a few hundred cursors would otherwise cost a
    // thousand descents on every frame painted.
    let endpoints = {
        let anchors: Vec<Anchor> = visible_selections
            .iter()
            .flat_map(|sel| [sel.start, sel.end, sel.tail(), sel.head()])
            .collect();
        buffer_snapshot.resolve_anchors_batch(&anchors)
    };
    // The block-cursor cell of every selection, and its buffer point, each in
    // one walk for the same reason the anchors above are resolved in one.
    let cursors = {
        let pairs: Vec<(usize, usize)> = endpoints
            .chunks_exact(4)
            .map(|ends| (ends[2], ends[3]))
            .collect();
        stoat_text::cursor_offsets(rope, &pairs)
    };
    let cursor_points = rope.offsets_to_points_batch(&cursors);

    // Where the cursor cells this frame draws land, filled by the pass below.
    // The character each one covers is read afterwards, since only these few
    // are wanted and reading them all would cost more than it saves.
    //
    // Deferring them also puts every cursor on top of every selection range
    // rather than only its own. Selections are disjoint, so the two orders
    // differ solely where a fold collapses two of them onto the same cells, and
    // there a visible cursor is the better of the two answers.
    let mut cursor_cells: Vec<(usize, u16, u16)> = Vec::new();

    for ((selection, ends), (&cursor, &cursor_point)) in visible_selections
        .iter()
        .zip(endpoints.chunks_exact(4))
        .zip(cursors.iter().zip(cursor_points.iter()))
    {
        let &[start_offset, end_offset, _, _] = ends else {
            continue;
        };

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

        // Display rows rise with buffer offsets, a fold hiding a range without
        // reordering one, so a cursor outside the visible bytes is on a row
        // outside the drawn ones. Answering that here is what keeps the descent
        // below off the cursors no frame can show.
        if !cursor_reaches_viewport(cursor, &visible, rope.len()) {
            continue;
        }

        let display = snapshot.buffer_to_display(cursor_point);
        if display.row >= editor.scroll_row && display.row < end_row {
            let y = inner.y + (display.row - editor.scroll_row) as u16;
            let x = inner.x + display.column as u16;
            if x < right && y < bottom {
                if delegates_cursor && selection.id == primary_id {
                    primary_cell = Some((x, y));
                } else {
                    cursor_cells.push((cursor, x, y));
                }
            }
        }
    }

    let covered = rope.chars_at_batch(&cursor_cells.iter().map(|&(o, _, _)| o).collect::<Vec<_>>());
    for (&(_, x, y), covered) in cursor_cells.iter().zip(covered) {
        let cell = &mut buf[(x, y)];
        let existing_char = cell.symbol().chars().next().unwrap_or(' ');
        let char_to_paint = if existing_char == '\0' {
            ' '
        } else {
            existing_char
        };
        cell.set_char(char_to_paint);
        cell.set_style(cursor_style);
        // A block cursor covers the character it sits on, so a wide one takes
        // both of its columns rather than reading as half covered. The trailing
        // column carries no character of its own, so only its style changes.
        let width = covered.map_or(1, display_width);
        for extra in 1..width as u16 {
            if x + extra < right {
                buf[(x + extra, y)].set_style(cursor_style);
            }
        }
    }

    editor.cursor_screen_cell = primary_cell;

    if let Some((path, set)) = diagnostic_info {
        build_diagnostic_span_cache(editor, set, path, buffer_snapshot);
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
        // popover needs a scene plus the severity and background colors resolved
        // to RGB, and its presence suppresses the same diagnostic's redundant
        // EOL message.
        let mut suppress = None;
        if let Some(index) = hover_diag.or(cursor_diag) {
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

        if let Some(cache) = editor.diagnostic_span_cache.as_mut() {
            paint_cursor_line_diagnostic(
                cache,
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

/// Cached gutter severity map for one diagnostic set against one buffer.
///
/// `map` is the per-buffer-row worst severity. Rebuilt when either version
/// moves, so the gutter is not rebuilt from the full diagnostic list every
/// frame. The buffer version belongs in the key because the rows come from
/// spans shifted through the edits since each diagnostic was published, so
/// typing moves them without the server saying anything.
pub(crate) struct GutterSeverityCache {
    pub(crate) version: u64,
    buffer_version: u64,
    pub(crate) map: Arc<BTreeMap<u32, DiagnosticSeverity>>,
}

/// Cached gutter geometry for one set of drawn-gutter inputs, in three layers.
///
/// The geometry layer holds the folded gutter lines, digit width, and per-row
/// diff marks. Rebuilding it is the expensive half, costing a block-tree query
/// per visible row and an anchor resolve over every diff hunk in the file.
/// [`Self::geometry_key`] guards it and hashes only what those collections
/// read, namely the viewport window and the buffer, fold, diff, and severity
/// versions.
///
/// The lines layer is the rich component lines, guarded by [`Self::lines_key`]
/// over the geometry key plus the two inputs only the lines read, the
/// relative-numbering cursor line and the resolved colors baked into them.
/// Keeping it separate matters because relative numbering is the default, so
/// every vertical cursor move changes the line inputs while leaving the
/// geometry identical, and the cheap layer rebuilds alone.
///
/// The scene layer is the APC frame those lines encode to, guarded by
/// [`Self::scene_key`] over the lines key plus the rect and colors the encoding
/// reads. It exists because the frame is rebuilt every repaint and then thrown
/// away unchanged by the scene's own flush comparison, so a pane that repaints
/// for a reason the gutter does not share re-encodes four commands a row for
/// nothing.
///
/// `lines_key` is `None` while the lines are unbuilt, which is both a fresh
/// geometry and the whole of fallback mode, where nothing paints from them.
/// `scene_key` is `None` under the same conditions, and on a dead scene, which
/// drops every append and so records an empty frame.
pub(crate) struct GutterGeometryCache {
    geometry_key: u64,
    folded: Vec<(u32, u16)>,
    width_digits: u16,
    marks: BTreeMap<u32, (DiffHunkStatus, bool)>,
    lines_key: Option<u64>,
    lines: Vec<GutterLine>,
    scene_key: Option<u64>,
    scene_bytes: Vec<u8>,
}

/// Build a per-buffer-row map from resolved diagnostic spans, picking the worst
/// severity (lowest LSP code) when several overlap a row.
///
/// The rows come from `spans` rather than from the diagnostics themselves so
/// they name where the text is now. A diagnostic's own rows are where the
/// server last saw it, and the reader has been typing since.
fn row_severity_from_spans(spans: &[ResolvedDiag]) -> BTreeMap<u32, DiagnosticSeverity> {
    let mut out: BTreeMap<u32, DiagnosticSeverity> = BTreeMap::new();
    for span in spans {
        for row in span.start_line..=span.end_line {
            out.entry(row)
                .and_modify(|cur| {
                    if severity_rank(span.severity) < severity_rank(*cur) {
                        *cur = span.severity;
                    }
                })
                .or_insert(span.severity);
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
    pub(crate) spans: Vec<ResolvedDiag>,
    /// Running maximum of [`Self::spans`]' ends, one entry per span.
    ///
    /// The spans are sorted by start, so their ends are not, and a viewport's
    /// lower bound cannot be searched for directly. This is non-decreasing by
    /// construction, and entry `i` at or below an offset proves every span up to
    /// `i` ends at or before it, which is what makes the bound searchable.
    prefix_max_end: Vec<usize>,
    /// Running maximum of [`Self::spans`]' end rows, one entry per span.
    ///
    /// The row counterpart of [`Self::prefix_max_end`], and searchable for the
    /// same reason. The cursor-line query is line-keyed rather than
    /// offset-keyed, and the two do not agree at a line's first byte: a
    /// diagnostic whose range ends at column 0 of a line counts as reaching that
    /// line, while its end offset is the byte before the line's own span. A
    /// bound in rows reaches exactly what a filter in rows accepts.
    prefix_max_end_line: Vec<u32>,
    /// The cursor line the readout last answered for, and the diagnostic it
    /// found there.
    ///
    /// The readout runs on every frame the cursor sits inside a diagnostic, and
    /// the answer only moves when the cursor changes line. The two versions
    /// above are not part of the key, since a change to either replaces this
    /// whole cache and the memo along with it.
    cursor_line_diag: Option<(u32, Option<ResolvedDiag>)>,
}

impl DiagnosticSpanCache {
    /// The index range of [`Self::spans`] that can overlap `visible`.
    ///
    /// Spans outside it are settled, so a caller still filters the ones inside
    /// for a real overlap.
    fn overlapping(&self, visible: Range<usize>) -> Range<usize> {
        let lo = self.prefix_max_end.partition_point(|&e| e <= visible.start);
        let hi = self.spans.partition_point(|s| s.start < visible.end);
        lo..hi.max(lo)
    }

    /// The worst-severity diagnostic whose rows straddle `line`.
    ///
    /// The answer for one line is kept, so a frame whose cursor has not changed
    /// line reads it back rather than searching again.
    ///
    /// Ties go to the earliest span, matching a scan of the whole slice: the
    /// bound below preserves the order the spans are sorted in.
    fn cursor_line_diagnostic(&mut self, line: u32) -> Option<ResolvedDiag> {
        if let Some((cached, found)) = self.cursor_line_diag
            && cached == line
        {
            return found;
        }

        let found = self.spans[self.straddling_line(line)]
            .iter()
            .filter(|s| s.start_line <= line && line <= s.end_line)
            .min_by_key(|s| severity_rank(s.severity))
            .copied();

        self.cursor_line_diag = Some((line, found));
        found
    }

    /// The index range of [`Self::spans`] with rows that straddle `line`.
    ///
    /// Spans outside it end above the line or start below it, so a caller still
    /// filters the ones inside for real containment.
    fn straddling_line(&self, line: u32) -> Range<usize> {
        let lo = self.prefix_max_end_line.partition_point(|&e| e < line);
        let hi = self.spans.partition_point(|s| s.start_line <= line);
        lo..hi.max(lo)
    }
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
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) -> Vec<ResolvedDiag> {
    let diagnostics = set.get(path);
    let published = set.spans(path);

    // Every endpoint in one walk of the fragment tree rather than a root descent
    // apiece, then their lines in one walk of the rope. A diagnostic with no
    // anchor pair contributes none, so the running slot below is what pairs the
    // results back up with the diagnostics that asked for them.
    let anchored: Vec<bool> = (0..diagnostics.len())
        .map(|index| anchors_in(published.get(index), snapshot).is_some())
        .collect();
    let endpoints: Vec<Anchor> = (0..diagnostics.len())
        .filter_map(|index| anchors_in(published.get(index), snapshot))
        .flat_map(|(start, end)| [start, end])
        .collect();
    let offsets = snapshot.resolve_anchors_batch(&endpoints);
    let points = snapshot.rope().offsets_to_points_batch(&offsets);

    let mut slot = 0usize;
    let mut spans: Vec<ResolvedDiag> = diagnostics
        .iter()
        .enumerate()
        .map(|(index, diag)| {
            let (start, end, start_line, end_line) = if anchored[index] {
                let resolved = (
                    offsets[slot],
                    offsets[slot + 1],
                    points[slot].row,
                    points[slot + 1].row,
                );
                slot += 2;
                resolved
            } else {
                (0, 0, 0, 0)
            };
            ResolvedDiag {
                start,
                end,
                severity: diag.severity.unwrap_or(DiagnosticSeverity::ERROR),
                unnecessary: is_unnecessary(diag),
                start_line,
                end_line,
                index,
            }
        })
        .collect();
    spans.sort_by_key(|s| s.start);
    spans
}

/// `span`'s endpoints when they anchor into the buffer `snapshot` is for.
///
/// A span published against a different buffer resolves to a meaningless offset
/// rather than an error, since the fragment tree it names is not the one being
/// asked, so the buffer has to be checked before the anchors are used.
fn anchors_in(
    span: Option<&crate::diagnostics::PublishedSpan>,
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) -> Option<(Anchor, Anchor)> {
    let (start, end) = span?.anchors?;
    (start.buffer_id == Some(snapshot.buffer_id())).then_some((start, end))
}

/// Rebuild `editor.diagnostic_span_cache` when the diagnostic set or buffer
/// version has moved since it was last resolved.
///
/// The paint is not the only caller. Mouse motion warms the same cache to
/// answer what the pointer is over, so a sweep across a diagnostic-heavy
/// buffer resolves the spans once rather than once per motion event.
pub(crate) fn build_diagnostic_span_cache(
    editor: &mut EditorState,
    set: &crate::diagnostics::DiagnosticSet,
    path: &Path,
    snapshot: &crate::multi_buffer::MultiBufferSnapshot,
) {
    let buffer_version = snapshot.version();
    let set_version = set.version();
    let stale = match &editor.diagnostic_span_cache {
        Some(cache) => cache.set_version != set_version || cache.buffer_version != buffer_version,
        None => true,
    };
    if stale {
        let spans = resolve_diagnostic_spans(set, path, snapshot);
        editor.diagnostic_span_cache = Some(DiagnosticSpanCache {
            set_version,
            buffer_version,
            prefix_max_end: prefix_max_ends(&spans),
            prefix_max_end_line: prefix_max_end_lines(&spans),
            cursor_line_diag: None,
            spans,
        });
    }
}

/// The collections [`paint_diagnostic_spans`] refills on every call.
///
/// Lives on the editor rather than the paint so a frame reuses the capacity of
/// the last one. None of them carries meaning across calls, and the paint clears
/// each before use.
#[derive(Default)]
pub(crate) struct DiagnosticPaintScratch {
    /// The viewport's spans, least-severe first.
    ordered: Vec<ResolvedDiag>,
    /// Cells an `Unnecessary` span has already muted, so overlapping spans
    /// never blend a shared cell twice.
    muted_cells: HashSet<(u16, u16)>,
    /// The cell runs one span painted, as `(x, y, len)`, on their way to
    /// becoming undercurl spans. Refilled per span rather than per frame.
    runs: Vec<(u16, u16, u16)>,
}

/// The running maximum of `spans`' ends, for [`DiagnosticSpanCache::prefix_max_end`].
fn prefix_max_ends(spans: &[ResolvedDiag]) -> Vec<usize> {
    let mut max_end = 0;
    spans
        .iter()
        .map(|span| {
            max_end = max_end.max(span.end);
            max_end
        })
        .collect()
}

/// The running maximum of `spans`' end rows, for
/// [`DiagnosticSpanCache::prefix_max_end_line`].
fn prefix_max_end_lines(spans: &[ResolvedDiag]) -> Vec<u32> {
    let mut max_end = 0;
    spans
        .iter()
        .map(|span| {
            max_end = max_end.max(span.end_line);
            max_end
        })
        .collect()
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

/// Resolve the rich page-gutter colors, or `None` when a gutter color is not
/// RGB.
///
/// Mirrors the live gutter's rich gate so an off-run-loop page render and the
/// live render agree on rich versus fallback for the same theme.
pub(crate) fn resolve_rich_gutter(
    theme: &crate::theme::Theme,
    fallback_style: Style,
) -> Option<RichGutterColors> {
    use crate::theme::scope as s;
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

/// The theme colors the editor chrome paints from, resolved once per theme.
///
/// Every field costs a walk over progressively broadening scopes, and the
/// gutter alone asks about thirty of those questions. They are all pure
/// functions of the theme, so a frame that did not change it can read the
/// previous answers.
///
/// Pane-local shading is not baked in. An unfocused pane dims these toward
/// [`Self::gutter_bg`] at paint time, since the dim varies per pane while the
/// colors do not.
pub(crate) struct ResolvedChrome {
    /// `None` when a gutter color is not RGB, which drops the pane to the
    /// fallback glyph gutter rather than mixing the two.
    pub(crate) rich_gutter: Option<RichGutterColors>,
    /// `None` when a diagnostic color is missing or not RGB.
    pub(crate) severity: Option<SeverityColors>,
    pub(crate) diff_marks: DiffMarkColors,
    /// Background the rich gutter fills, and what its foregrounds dim toward.
    pub(crate) gutter_bg: Option<[u8; 3]>,
}

impl ResolvedChrome {
    pub(crate) fn resolve(theme: &crate::theme::Theme) -> Self {
        let fallback_style = theme.get(crate::theme::scope::UI_TEXT);
        Self {
            rich_gutter: resolve_rich_gutter(theme, fallback_style),
            severity: severity_colors(theme),
            diff_marks: DiffMarkColors::resolve(theme),
            gutter_bg: gutter_background(theme, fallback_style),
        }
    }
}

/// The background the gutter fills, preferring the pane's own over the theme's.
fn gutter_background(theme: &crate::theme::Theme, fallback_style: Style) -> Option<[u8; 3]> {
    style_rgb(fallback_style.bg.or_else(|| {
        theme
            .try_get(crate::theme::scope::UI_BACKGROUND)
            .and_then(|st| st.bg)
    }))
}

/// The visible rows carrying a diagnostic, for the gutter with line numbers off.
///
/// Held rather than re-derived because resolving each visible display row to its
/// buffer row is a descent down the layer stack, and the gutter asks for every
/// row on every repaint. The line-numbers-on gutter caches the same question
/// under [`GutterGeometryCache`], but shares no fields with this one, so mixing
/// them would leave whichever path ran last holding a half-filled entry.
pub(crate) struct DiagnosticRowsCache {
    key: u64,
    /// `(offset from the top of the viewport, severity)` for the rows that carry
    /// one. Rows without a diagnostic are absent, both painters skipping them.
    rows: Vec<(u16, DiagnosticSeverity)>,
}

/// The visible rows carrying a diagnostic, rebuilding `cache` only when the
/// viewport's row mapping could have moved.
///
/// Keyed by [`gutter_geometry_key`], which already asks that question for the
/// line-numbers-on gutter. It folds in the width, since that sets the wrap, and
/// the diff version, which this does not read but which only ever costs a spare
/// rebuild of a viewport-sized list.
fn diagnostic_rows<'c>(
    snapshot: &DisplaySnapshot,
    row_severity: &BTreeMap<u32, DiagnosticSeverity>,
    scroll_row: u32,
    inner: Rect,
    end_row: u32,
    severity_version: u64,
    cache: &'c mut Option<DiagnosticRowsCache>,
) -> &'c [(u16, DiagnosticSeverity)] {
    let key = gutter_geometry_key(
        scroll_row,
        inner.width,
        end_row.saturating_sub(scroll_row),
        snapshot.buffer_snapshot().version(),
        snapshot.version(),
        snapshot.diff_map().map_or(0, |dm| dm.version()),
        severity_version,
    );
    if cache.as_ref().is_none_or(|c| c.key != key) {
        let rows = (scroll_row..end_row)
            .map_while(|display_row| {
                let offset = (display_row - scroll_row) as u16;
                (offset < inner.height).then_some((display_row, offset))
            })
            .filter_map(|(display_row, offset)| {
                let row = buffer_row_of(snapshot, display_row)?;
                Some((offset, *row_severity.get(&row)?))
            })
            .collect();
        *cache = Some(DiagnosticRowsCache { key, rows });
    }
    &cache.as_ref().expect("filled just above").rows
}

/// The buffer row shown at `display_row`, or `None` when the row is a block's
/// own and belongs to no buffer line.
///
/// Severity is recorded per buffer row, and soft wrap and blocks both put more
/// display rows on screen than the buffer has, so the two spaces diverge by
/// however many extra rows sit above the viewport.
fn buffer_row_of(snapshot: &DisplaySnapshot, display_row: u32) -> Option<u32> {
    snapshot
        .display_to_buffer(DisplayPoint::new(display_row, 0))
        .map(|point| point.row)
}

fn paint_diagnostic_gutter(
    rows: &[(u16, DiagnosticSeverity)],
    x: u16,
    y: u16,
    theme: &crate::theme::Theme,
    buf: &mut Buffer,
) {
    for &(row_offset, sev) in rows {
        let style = theme.get(severity_scope(sev));
        buf[(x, y + row_offset)]
            .set_char(severity_mark(sev))
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

/// [`gutter_digits`] for a snapshot the caller already holds.
fn gutter_width_digits(snapshot: &DisplaySnapshot) -> u16 {
    gutter_digits(
        snapshot.buffer_line_count(),
        snapshot.buffer_snapshot().rope().max_point(),
    )
}

/// The digit width the gutter reserves for a buffer's line numbers, at least
/// two.
///
/// A trailing newline leaves an empty final line the min-width-1 cursor cannot
/// reach, so it is rendering padding rather than a line and never widens the
/// gutter. `max_point` is what identifies it.
///
/// Both inputs are buffer facts, never wrap or block rows, which is what lets
/// the paint size the gutter and resolve the wrap width before the display
/// snapshot they would otherwise be read from exists.
fn gutter_digits(line_count: u32, max_point: Point) -> u16 {
    let phantom = max_point.row > 0 && max_point.column == 0;
    decimal_digits(line_count - phantom as u32).max(2)
}

/// The cell columns the line-number gutter reserves, measured without painting.
///
/// `rich` selects the sub-cell [`Gutter::cell_width`] layout. The degraded
/// gutter instead reserves a mark column, the digits, and a gap. The result
/// matches what [`draw_line_number_gutter`] paints for the same digit count, so
/// the wrap width can be resolved before the wrapped snapshot exists.
fn measure_gutter_width(width_digits: u16, rich: bool) -> u16 {
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
    // Where the hunks sit now, not where the last diff job left them. It runs
    // in the background per buffer version, so its rows are already behind.
    let live = diff_map.live_hunks(snapshot.buffer_snapshot());
    gutter_diff_marks_from(&live, folded)
}

/// The marks `folded`'s rows take from `live`.
///
/// Split from [`gutter_diff_marks`] for a caller painting several windows of one
/// snapshot. Resolving the hunks costs an anchor batch over every hunk in the
/// file plus a sort, and every window of that snapshot would otherwise pay it
/// again for the same answer.
pub(crate) fn gutter_diff_marks_from(
    live: &crate::diff_map::LiveHunks<'_>,
    folded: &[(u32, u16)],
) -> BTreeMap<u32, (DiffHunkStatus, bool)> {
    folded
        .iter()
        .filter_map(|&(number, _)| {
            let row = number - 1;
            live.gutter_mark_for_line(row).map(|mark| (row, mark))
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
        bound_to_line_layout: false,
    }
}

/// Hash the inputs the cached gutter geometry is built from into a cache key.
///
/// Any change here misses [`GutterGeometryCache`]'s geometry layer and rebuilds
/// the folded lines, digit width, and diff marks. A repaint that changes none
/// of them reuses all three. The relative-numbering line and the resolved
/// colors are deliberately absent, since none of the three collections reads
/// either.
fn gutter_geometry_key(
    scroll_row: u32,
    width: u16,
    visible: u32,
    buffer_version: u64,
    fold_version: usize,
    diff_version: usize,
    severity_version: u64,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    scroll_row.hash(&mut hasher);
    width.hash(&mut hasher);
    visible.hash(&mut hasher);
    buffer_version.hash(&mut hasher);
    fold_version.hash(&mut hasher);
    diff_version.hash(&mut hasher);
    severity_version.hash(&mut hasher);
    hasher.finish()
}

/// Hash the inputs the rich component lines are built from into a cache key.
///
/// `geometry_key` is folded in because the lines are built from the geometry it
/// guards, so a geometry rebuild has to rebuild them too. The two inputs of
/// their own are the relative-numbering line and the colors they bake in, which
/// is what makes a theme change and a cursor move both show up here.
fn gutter_lines_key(
    geometry_key: u64,
    current_line: Option<u32>,
    colors: ([u8; 3], DiffMarkColors, &SeverityColors),
) -> u64 {
    let mut hasher = DefaultHasher::new();
    geometry_key.hash(&mut hasher);
    current_line.hash(&mut hasher);
    colors.hash(&mut hasher);
    hasher.finish()
}

/// Hash everything the rich gutter's APC frame is encoded from into a cache key.
///
/// The frame is positioned against the pane rect in absolute cells, so the rect
/// belongs here whole rather than as the width and height the geometry key
/// already covers. The three colors are the ones the encoding reads directly,
/// as opposed to those the lines key covers by having baked them into the lines.
fn gutter_scene_key(
    lines_key: Option<u64>,
    inner: Rect,
    colors: ([u8; 3], [u8; 3], [u8; 3]),
) -> u64 {
    let mut hasher = DefaultHasher::new();
    lines_key.hash(&mut hasher);
    (inner.x, inner.y, inner.width, inner.height).hash(&mut hasher);
    colors.hash(&mut hasher);
    hasher.finish()
}

/// Draw the absolute-line-number gutter and return the cell columns it reserves.
///
/// With a scene and every gutter color resolved to RGB, draws the rich
/// sub-cell gutter (scaled numbers, severity bars, hairline separator). Without
/// one, or with a theme whose colors are not RGB, gets right-aligned cell
/// numbers and a one-column severity mark styled from the theme, so the numbers
/// still show.
#[allow(clippy::too_many_arguments)]
fn draw_line_number_gutter(
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    inner: Rect,
    end_row: u32,
    row_severity: &BTreeMap<u32, DiagnosticSeverity>,
    theme: &crate::theme::Theme,
    chrome: &ResolvedChrome,
    current_line: Option<u32>,
    severity_version: u64,
    cache: &mut Option<GutterGeometryCache>,
    scene: Option<&mut ApcScene>,
    buf: &mut Buffer,
    dim: f32,
) -> u16 {
    let visible = end_row.saturating_sub(scroll_row).min(inner.height as u32);

    // Dimmed owned gutter colors, borrowed by the Copy `gutter_rgb` tuple and the
    // lines key below. That key hashes them, so a dim change refills the lines.
    // The undimmed colors come from the chrome, which resolved them once for
    // the theme. Only the per-pane shading happens here.
    let diff_colors = match chrome.gutter_bg {
        Some(bg) => chrome.diff_marks.dim(bg, dim),
        None => chrome.diff_marks,
    };
    let dimmed = chrome.rich_gutter.as_ref().map(|rich| rich.dim(dim));

    // Rich mode needs a scene and every gutter color as RGB, which is exactly
    // when the chrome resolved a rich set at all.
    let gutter_rgb = dimmed
        .as_ref()
        .map(|rich| (&rich.colors, rich.number_fg, rich.separator, rich.bg));
    let rich = scene.zip(gutter_rgb);

    let geometry_key = gutter_geometry_key(
        scroll_row,
        inner.width,
        visible,
        snapshot.buffer_snapshot().version(),
        snapshot.version(),
        snapshot.diff_map().map_or(0, |dm| dm.version()),
        severity_version,
    );

    if cache
        .as_ref()
        .is_none_or(|c| c.geometry_key != geometry_key)
    {
        let (folded, width_digits) = gutter_geometry(snapshot, scroll_row, visible);
        let marks = gutter_diff_marks(snapshot, &folded);
        *cache = Some(GutterGeometryCache {
            geometry_key,
            folded,
            width_digits,
            marks,
            lines_key: None,
            lines: Vec::new(),
            scene_key: None,
            scene_bytes: Vec::new(),
        });
    }
    let geometry = cache.as_mut().expect("set above");

    // Only the rich path paints from the lines, so the fallback never builds
    // them and its cache keeps the empty vec the geometry rebuild left.
    if let Some((colors, _, _, bg)) = gutter_rgb {
        let lines_key = gutter_lines_key(geometry_key, current_line, (bg, diff_colors, colors));
        if geometry.lines_key != Some(lines_key) {
            geometry.lines = gutter_component_lines(
                &geometry.folded,
                row_severity,
                &geometry.marks,
                &diff_colors,
                colors,
                current_line,
            );
            geometry.lines_key = Some(lines_key);
        }
    }

    match rich {
        Some((scene, (_colors, number_fg, separator, bg))) => {
            draw_rich_gutter(geometry, inner, (number_fg, separator, bg), buf, scene)
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

/// Emit the rich gutter's APC frame, and return the cell columns it reserves.
///
/// The frame is spliced from [`GutterGeometryCache::scene_bytes`] whenever the
/// cached run encodes the same frame, rather than re-encoded a command at a
/// time. Splicing is only sound because [`Gutter::draw_components`] reads
/// nothing outside what [`gutter_scene_key`] covers, and writes nothing but the
/// scene, so a skipped emit leaves no cell unpainted.
fn draw_rich_gutter(
    geometry: &mut GutterGeometryCache,
    inner: Rect,
    colors: ([u8; 3], [u8; 3], [u8; 3]),
    buf: &mut Buffer,
    scene: &mut ApcScene,
) -> u16 {
    let (number_fg, separator, bg) = colors;
    let scene_key = gutter_scene_key(geometry.lines_key, inner, colors);

    let gutter = rich_gutter(
        &geometry.lines,
        geometry.width_digits,
        number_fg,
        separator,
        bg,
    );
    let cell_width = gutter.cell_width();

    if geometry.scene_key == Some(scene_key) {
        scene.buffer().extend_from_slice(&geometry.scene_bytes);
        return cell_width;
    }

    let start = scene.bytes().len();
    gutter.draw_components(inner, buf, scene);

    // A dead scene routes the appends to a scratch it clears per handout, so the
    // slice below is empty and records a frame that paints nothing.
    if scene.live() {
        geometry.scene_bytes.clear();
        geometry
            .scene_bytes
            .extend_from_slice(&scene.bytes()[start..]);
        geometry.scene_key = Some(scene_key);
    }

    cell_width
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

    // A scope resolves by walking progressively broadening keys, so a gutter
    // repeating a mark down fifty rows would pay that walk fifty times. Each
    // scope resolves on the first row that wants it and is remembered for the
    // rest, which also leaves an unmarked gutter resolving none of them.
    //
    // The slots cannot run out. A row paints one of four severities, one of four
    // diff statuses, and one of two staged states.
    let mut resolved: [Option<(&'static str, Style)>; 10] = [None; 10];
    let mut style_for =
        |scope: &'static str| match resolved.iter().flatten().find(|(known, _)| *known == scope) {
            Some(&(_, style)) => style,
            None => {
                let style = theme.get(scope);
                let free = resolved
                    .iter_mut()
                    .find(|slot| slot.is_none())
                    .expect("a gutter paints at most ten distinct mark scopes");
                *free = Some((scope, style));
                style
            },
        };

    let mut number = String::new();
    let mut top = 0u16;
    for &(line, height) in folded {
        let y = inner.y + top;
        if y >= inner.y + inner.height {
            break;
        }
        if let Some(sev) = row_severity.get(&(line - 1)) {
            buf[(inner.x, y)]
                .set_char(severity_mark(*sev))
                .set_style(style_for(severity_scope(*sev)));
        }

        number.clear();
        write!(number, "{}", gutter_display_number(line, current_line))
            .expect("writing to a String is infallible");
        let start = inner.x + mark_w + width_digits.saturating_sub(number.len() as u16);
        buf.set_stringn(start, y, &number, number.len(), number_style);

        if let Some(&(status, staged)) = diff_marks.get(&(line - 1)) {
            let (mark, scope) = match status {
                DiffHunkStatus::Deleted => ('▔', s::DIFF_DELETED),
                DiffHunkStatus::Added => ('▎', s::DIFF_ADDED),
                DiffHunkStatus::Modified => ('▎', s::DIFF_MODIFIED),
                DiffHunkStatus::Moved => ('▎', s::DIFF_MOVED),
            };
            buf[(change_x, y)]
                .set_char(mark)
                .set_style(style_for(scope));
            let staged_scope = match staged {
                true => s::DIFF_STAGED,
                false => s::DIFF_UNSTAGED,
            };
            buf[(staged_x, y)]
                .set_char('▎')
                .set_style(style_for(staged_scope));
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
    cache: &DiagnosticSpanCache,
    scratch: &mut DiagnosticPaintScratch,
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
    mut undercurls: Option<&mut UndercurlBatch>,
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
    let DiagnosticPaintScratch {
        ordered,
        muted_cells,
        runs,
    } = scratch;
    muted_cells.clear();

    // Only spans overlapping the viewport can paint a cell, and the cache bounds
    // which those are at both ends. Paint them least-severe first so the worst
    // severity lands last, on top, for both the cell foreground and the
    // collected undercurl spans. rust-analyzer can publish a WARNING and a HINT
    // over the same `unused` in any order, and publish order alone would let the
    // hint's grey win.
    ordered.clear();
    ordered.extend(
        cache.spans[cache.overlapping(visible.clone())]
            .iter()
            .filter(|s| s.start < s.end && s.end > visible.start),
    );
    ordered.sort_by_key(|s| Reverse(severity_rank(s.severity)));

    for diag in &*ordered {
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

        // Collect the painted runs only when the undercurl overlay is live,
        // then re-stamp each as a severity-colored curl span.
        runs.clear();
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
            collect.then_some(&mut *runs),
        );
        if let (Some(undercurls), Some(colors)) = (undercurls.as_deref_mut(), colors) {
            let base = severity_color(sev, colors);
            let color = match mute_bg {
                Some(bg) if dim > 0.0 => dim_rgb(base, bg, dim),
                _ => base,
            };
            for &(x, y, len) in &*runs {
                undercurls.push(x, y, len, color);
            }
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
    cache: &mut DiagnosticSpanCache,
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

    // Containment is by line rather than by offset, so a diagnostic reaching the
    // cursor's line from anywhere on it wins the readout.
    let Some(resolved) = cache.cursor_line_diagnostic(cursor_point.row) else {
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
///
/// `runs` collects the painted cells as `(x, y, len)` runs of horizontally
/// adjacent same-row cells, appended as they are painted. Cells arrive left to
/// right within each display row and cover every column a character occupies,
/// so adjacency breaks only where the paint itself does. That is a row change
/// or `skip_offset`, and both are where an undercurl drawn from these runs
/// should break too. A wide character or a tab contributes all of its columns
/// to one run rather than starting a new one after each.
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
    mut runs: Option<&mut Vec<(u16, u16, u16)>>,
) {
    // A fold moves everything after it, so rows from the first one on go the
    // general way whether or not they hold a fold themselves. Rows above it are
    // untouched. Inlays only shift the row they sit on, so that check is per
    // segment rather than against the whole buffer.
    let first_fold_row = snapshot.fold_snapshot().first_fold_row();
    let inlay_snapshot = snapshot.inlay_snapshot();
    let any_inlays = inlay_snapshot.has_inlays();
    let tab_size = snapshot.tab_snapshot().tab_size();
    let max_expansion_column = snapshot.tab_snapshot().max_expansion_column();
    let line_count = snapshot.line_count();

    // `cells` is how far the character moves the column, so a wide glyph
    // washes both of its columns rather than leaving the right one bare, and a
    // tab covers every column up to its stop. A combining mark moves it none
    // and paints none, the cell it shares having been covered by the character
    // it sits on.
    let mut paint = |display_row: u32, display_col: u32, cells: u32| {
        if display_row < scroll_row || display_row >= end_row {
            return;
        }
        let y = inner.y + (display_row - scroll_row) as u16;
        for step in 0..cells {
            let x = inner.x + (display_col + step) as u16;
            if x < right && y < bottom {
                apply(x, y, &mut buf[(x, y)]);
                if let Some(runs) = runs.as_deref_mut() {
                    match runs.last_mut() {
                        Some(last) if last.1 == y && last.0 + last.2 == x => last.2 += 1,
                        _ => runs.push((x, y, 1)),
                    }
                }
            }
        }
    };

    let mut offset = range.start;
    let mut chars = rope.chars_at(offset);

    'segments: while offset < range.end {
        let point = rope.offset_to_point(offset);
        let display = snapshot.buffer_to_display(point);
        let single_display_row = !snapshot.is_wrap_continuation(display.row)
            && (display.row + 1 >= line_count || !snapshot.is_wrap_continuation(display.row + 1));

        // Neither a fold before this row nor an inlay on it, so the row's
        // characters land where the buffer says and the column can be
        // accumulated rather than resolved.
        let accumulable_row = first_fold_row.is_none_or(|fold_row| point.row < fold_row)
            && !(any_inlays && inlay_snapshot.has_inlays_in_row_range(point.row..point.row + 1));
        if accumulable_row && single_display_row {
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
                let start_col = col;
                tab_map::advance_column_for_char(&mut col, ch, tab_size, max_expansion_column);
                if Some(offset) != skip_offset {
                    paint(row, start_col, col - start_col);
                }
                offset += ch.len_utf8();
            }
        } else if accumulable_row {
            // A soft-wrapped row rebases its columns per display row, so the
            // accumulated column is not the display one and each character has
            // to be placed. The accumulation still pays off: it is the
            // tab-expanded column, which is the leg of the resolution that
            // walks the row from its start.
            let tab_point = snapshot.buffer_to_tab_point(point);
            let tab_row = tab_point.row();
            let mut tab_col = tab_point.column();
            let mut char_point = point;
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
                let start_col = tab_col;
                tab_map::advance_column_for_char(&mut tab_col, ch, tab_size, max_expansion_column);
                if Some(offset) != skip_offset {
                    let display = snapshot.tab_to_display(TabPoint::new(tab_row, start_col));
                    debug_assert_eq!(
                        display,
                        snapshot.buffer_to_display(char_point),
                        "the accumulated tab column places the character where its buffer point does"
                    );
                    let mut end_col = display.column;
                    tab_map::advance_column_for_char(
                        &mut end_col,
                        ch,
                        tab_size,
                        max_expansion_column,
                    );
                    paint(display.row, display.column, end_col - display.column);
                }
                offset += ch.len_utf8();
                char_point.column += ch.len_utf8() as u32;
            }
        } else {
            // Carried from the segment head rather than re-derived per
            // character. A point's column counts bytes, so a character moves it
            // by its own encoded length. A newline ends the segment, so the
            // row never has to advance here.
            let mut char_point = point;
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
                    let display = snapshot.buffer_to_display(char_point);
                    // Advanced from the character's own column rather than from
                    // zero, since a tab's width is the distance to its stop.
                    let mut end_col = display.column;
                    tab_map::advance_column_for_char(
                        &mut end_col,
                        ch,
                        tab_size,
                        max_expansion_column,
                    );
                    paint(display.row, display.column, end_col - display.column);
                }
                offset += ch.len_utf8();
                char_point.column += ch.len_utf8() as u32;
            }
        }
    }
}

/// Whether a cursor at `cursor` can land on a drawn row.
///
/// `visible.end` is the offset where the first undrawn row begins, so a cursor
/// sitting exactly there is on that row and off screen. Unless the viewport runs
/// past the last row, where that offset is the rope's end instead and a cursor
/// there is on the last drawn row.
///
/// Answering yes for a cursor that turns out to be off screen costs a display
/// conversion and paints nothing, which is what the caller did for every cursor
/// before. Answering no for one that is on screen would lose it, so the buffer
/// end is the case to get right.
fn cursor_reaches_viewport(cursor: usize, visible: &Range<usize>, rope_len: usize) -> bool {
    if cursor < visible.start {
        return false;
    }
    cursor < visible.end || (cursor == visible.end && visible.end == rope_len)
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

/// The 1-based line and column of `editor`'s newest cursor, or `None` for a
/// review view, which has no single cursor to report.
///
/// Reads the buffer rather than a display snapshot. Where an anchor lands and
/// what row an offset is on are buffer facts, so the fold, wrap, and block
/// mapping a display snapshot would sync are all beside the point. That matters
/// because this runs per pane per frame from the status bar.
pub(crate) fn editor_cursor_position(editor: &EditorState) -> Option<(u32, u32)> {
    if editor.review_view.is_some() {
        return None;
    }
    let buffer = editor.display_map.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    let rope = buffer.rope();
    let cursor = cursor_offset(
        rope,
        buffer.resolve_anchor(&sel.tail()),
        buffer.resolve_anchor(&sel.head()),
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

    /// Each resolved diagnostic as `(index, start, end, start_line)`.
    fn resolved(
        set: &crate::diagnostics::DiagnosticSet,
        path: &std::path::Path,
        snapshot: &crate::multi_buffer::MultiBufferSnapshot,
    ) -> Vec<(usize, usize, usize, u32)> {
        super::resolve_diagnostic_spans(set, path, snapshot)
            .iter()
            .map(|s| (s.index, s.start, s.end, s.start_line))
            .collect()
    }

    /// A diagnostic nothing could anchor has no position of its own, and must
    /// not take the offsets belonging to the ones beside it.
    #[test]
    fn a_diagnostic_without_an_anchored_span_resolves_to_zero() {
        let path = PathBuf::from("/a");
        let snapshot = snapshot_over("alpha\nbravo\n");
        let mut set = crate::diagnostics::DiagnosticSet::new();
        set.replace_from_server(
            path.clone(),
            "lsp".into(),
            vec![
                span_diag(0, 5, DiagnosticSeverity::ERROR),
                span_diag(0, 5, DiagnosticSeverity::WARNING),
            ],
            vec![
                crate::diagnostics::PublishedSpan::unresolved(),
                anchored_span(&snapshot, 6..11),
            ],
        );

        assert_eq!(
            resolved(&set, &path, &snapshot),
            [(0, 0, 0, 0), (1, 6, 11, 1)],
            "the unanchored one sits at zero and the anchored one keeps its span",
        );
    }

    /// An anchor names a position in one buffer's fragment tree. Resolving it
    /// against another answers with an offset rather than an error, so a span
    /// published elsewhere has to read as unanchored instead.
    #[test]
    fn a_span_anchored_in_another_buffer_resolves_to_zero() {
        use crate::{buffer::TextBuffer, multi_buffer::MultiBuffer};
        use std::sync::{Arc, RwLock};

        let path = PathBuf::from("/a");
        let other_id = stoat_text::BufferId::new(7);
        let other = MultiBuffer::singleton(
            other_id,
            Arc::new(RwLock::new(TextBuffer::with_text(other_id, "elsewhere\n"))),
        )
        .snapshot();

        let snapshot = snapshot_over("alpha\nbravo\n");
        let mut set = crate::diagnostics::DiagnosticSet::new();
        set.replace_from_server(
            path.clone(),
            "lsp".into(),
            vec![span_diag(0, 5, DiagnosticSeverity::ERROR)],
            vec![anchored_span(&other, 2..6)],
        );

        assert_eq!(resolved(&set, &path, &snapshot), [(0, 0, 0, 0)]);
    }

    /// The severity map is keyed by buffer row and derived from spans shifted
    /// through the edits since each diagnostic was published, so typing above
    /// one moves its row without the server having said anything.
    #[test]
    fn a_diagnostic_row_follows_an_edit_above_it_without_a_republish() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/severity-shift");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.seed_diagnostics(&path, vec![diag(1, DiagnosticSeverity::WARNING)]);

        let severity_rows = |h: &mut crate::test_harness::TestHarness| -> Vec<u32> {
            let _ = h.stoat.render();
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
            editor
                .gutter_severity_cache
                .as_ref()
                .expect("built by the paint")
                .map
                .keys()
                .copied()
                .collect()
        };

        assert_eq!(
            severity_rows(&mut h),
            vec![1],
            "the row it was published on"
        );

        let buffer_id = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .buffer_id;
        h.stoat
            .active_workspace()
            .buffers
            .get(buffer_id)
            .expect("buffer")
            .write()
            .expect("poisoned")
            .edit(0..0, "inserted\n");

        assert_eq!(
            severity_rows(&mut h),
            vec![2],
            "and the mark rides the inserted line down with its text",
        );
    }

    /// The rows are derived once and reused until something that could move
    /// them does.
    ///
    /// Deriving them resolves every visible display row down the layer stack,
    /// and the gutter asks on every repaint, so reuse is the whole point.
    /// Each input is moved on its own because one missing from the key leaves
    /// the gutter marking where a diagnostic used to be.
    #[test]
    fn the_diagnostic_rows_are_reused_until_an_input_moves() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/severity-cache");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"one\ntwo\nthree\nfour\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        let snapshot = editor.display_map.snapshot();
        let severity: std::collections::BTreeMap<u32, DiagnosticSeverity> =
            std::iter::once((2, DiagnosticSeverity::ERROR)).collect();
        let area = Rect::new(0, 0, 4, 12);

        let mut cache = None;
        let rows = super::diagnostic_rows(&snapshot, &severity, 0, area, 4, 0, &mut cache).to_vec();
        assert_eq!(
            rows,
            vec![(2u16, DiagnosticSeverity::ERROR)],
            "the diagnostic's row carries its severity",
        );

        // A severity this text cannot produce marks the held entry, so reuse is
        // told apart from a rebuild that lands on the same answer.
        cache.as_mut().expect("built").rows[0].1 = DiagnosticSeverity::HINT;
        assert_eq!(
            super::diagnostic_rows(&snapshot, &severity, 0, area, 4, 0, &mut cache)[0].1,
            DiagnosticSeverity::HINT,
            "a repeat frame rebuilt the rows instead of reusing them",
        );

        for (label, scroll, rect, end, version) in [
            ("a scroll", 1u32, area, 4u32, 0u64),
            ("a resize", 0, Rect::new(0, 0, 5, 12), 4, 0),
            ("a shorter viewport", 0, area, 3, 0),
            ("a new diagnostic set", 0, area, 4, 1),
        ] {
            // From the same baseline each time, or one case's key change would
            // stand in for the next's and an input missing from the key would
            // go unnoticed.
            let mut cache = None;
            super::diagnostic_rows(&snapshot, &severity, 0, area, 4, 0, &mut cache);
            cache.as_mut().expect("built").rows[0].1 = DiagnosticSeverity::HINT;

            assert_eq!(
                super::diagnostic_rows(
                    &snapshot, &severity, scroll, rect, end, version, &mut cache
                )
                .first()
                .map(|&(_, sev)| sev),
                Some(DiagnosticSeverity::ERROR),
                "{label} must derive the rows again",
            );
        }
    }

    /// Severity is recorded per buffer row, and a wrapped line puts more display
    /// rows on screen than the buffer has, so the mark has to be painted at the
    /// display row its buffer row occupies.
    #[test]
    fn the_glyph_severity_gutter_marks_the_diagnostics_own_display_row() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/severity-wrap");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"a long first line that wraps\nsecond\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
        editor.display_map.set_wrap_width(Some(8));
        let snapshot = editor.display_map.snapshot();

        let wrapped_row = snapshot.buffer_to_display(Point::new(1, 0)).row;
        assert!(
            wrapped_row > 1,
            "the first line has to wrap for this to bite"
        );

        let severity: std::collections::BTreeMap<u32, DiagnosticSeverity> =
            std::iter::once((1, DiagnosticSeverity::ERROR)).collect();
        let mut buf = Buffer::empty(Rect::new(0, 0, 4, 12));
        let theme = h.stoat.theme.clone();
        let mut cache = None;
        let rows = super::diagnostic_rows(
            &snapshot,
            &severity,
            0,
            Rect::new(0, 0, 4, 12),
            snapshot.line_count().min(12),
            0,
            &mut cache,
        );
        super::paint_diagnostic_gutter(rows, 0, 0, &theme, &mut buf);

        let marked: Vec<u32> = (0..12)
            .filter(|&y| buf[(0, y)].symbol() != " ")
            .map(u32::from)
            .collect();
        assert_eq!(
            marked,
            vec![wrapped_row],
            "the mark paints on the wrapped display row of buffer row 1",
        );
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
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
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
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
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
            &chrome,
            &mut buf,
            true,
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
    fn selection_off_an_inlay_row_paints_the_same_as_without_inlays() {
        fn line1_row(with_inlay: bool) -> Vec<ratatui::buffer::Cell> {
            let mut h = Stoat::test();
            open_search_buffer(&mut h, "let x = 1\nlet y = 2");
            let theme = h.stoat.theme.clone();
            let fallback = theme.get(crate::theme::scope::UI_TEXT);
            let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);

            if with_inlay {
                let editor =
                    action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
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
            }

            // Select line 1, which carries no inlay, so the row-scoped check
            // routes it through the fast paint path whether or not line 0 has an
            // inlay.
            dispatch(&mut h.stoat, &MoveDown);
            dispatch(&mut h.stoat, &ExtendToLineEnd);

            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            let area = Rect::new(0, 0, 40, 4);
            let mut buf = Buffer::empty(area);
            super::render_editor_with_overlay(
                editor,
                area,
                fallback,
                &theme,
                &chrome,
                &mut buf,
                true,
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
            (0..area.width).map(|x| buf[(x, 1)].clone()).collect()
        }

        let sel_bg = {
            let h = Stoat::test();
            h.stoat
                .theme
                .get(crate::theme::scope::UI_SELECTION_EDITOR)
                .bg
        };
        let with_inlay = line1_row(true);
        let without = line1_row(false);

        assert!(
            sel_bg.is_some() && with_inlay.iter().any(|c| Some(c.bg) == sel_bg),
            "the selection wash lands on line 1",
        );
        assert_eq!(
            with_inlay, without,
            "an inlay on line 0 must not change line 1's fast-path selection paint",
        );
    }

    /// Cursors the viewport cannot show contribute nothing to what it paints.
    ///
    /// The pass skips them before converting each to a display point, and a
    /// skip that dropped a cursor it should have drawn, or kept one it should
    /// not have, shows up as a cell differing from the same frame painted with
    /// only the on-screen cursors in the set.
    #[test]
    fn cursors_outside_the_viewport_paint_nothing() {
        // Scrolled well down a long buffer, so cursors can sit far above and far
        // below what is drawn.
        const ROWS: u16 = 6;
        const SCROLL: u32 = 40;

        fn painted(extra_rows: &[u32]) -> Buffer {
            let text: String = (0..120).map(|i| format!("line {i} of text\n")).collect();
            let mut h = Stoat::test();
            open_search_buffer(&mut h, &text);
            let theme = h.stoat.theme.clone();
            let fallback = theme.get(crate::theme::scope::UI_TEXT);
            let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);

            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            editor.scroll_row = SCROLL;

            // A cursor on the third drawn row, plus whatever else is asked for.
            let mut rows = vec![SCROLL + 2];
            rows.extend_from_slice(extra_rows);
            rows.sort_unstable();

            {
                let snapshot = editor.display_map.snapshot();
                let buf_snap = snapshot.buffer_snapshot();
                let rope = buf_snap.rope();
                let spans: Vec<(usize, usize)> = rows
                    .iter()
                    .map(|&row| {
                        let offset = rope.point_to_offset(Point::new(row, 0));
                        (offset, offset + 1)
                    })
                    .collect();
                editor.selections.replace_with_fresh_ids_from_offsets(
                    &spans,
                    Bias::Right,
                    buf_snap,
                );
            }

            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            let area = Rect::new(0, 0, 40, ROWS);
            let mut buf = Buffer::empty(area);
            super::render_editor_with_overlay(
                editor,
                area,
                fallback,
                &theme,
                &chrome,
                &mut buf,
                true,
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
            buf
        }

        let only_visible = painted(&[]);
        let with_offscreen = painted(&[0, 3, SCROLL - 1, SCROLL + u32::from(ROWS), 119]);

        assert_eq!(
            only_visible, with_offscreen,
            "cursors above and below the drawn rows changed what was drawn",
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
            super::measure_gutter_width(super::gutter_width_digits(&snapshot), false),
            editor.gutter_width,
            "the measured fallback width matches the painted gutter",
        );
    }

    /// The paint sizes the gutter from a buffer snapshot rather than a display
    /// one, which is what lets it take a single snapshot per frame. That only
    /// works if the buffer it reads is the current one, so an edit crossing a
    /// digit boundary has to widen the gutter on the very next frame.
    #[test]
    fn the_gutter_widens_on_the_frame_an_edit_adds_a_digit() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/gutter-digits");
        let path = root.join("a.txt");
        let body: String = (0..99).map(|i| format!("line {i}\n")).collect();
        h.fake_fs().insert_file(&path, body.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 6);
        let narrow = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .gutter_width;

        // 99 lines plus the trailing newline's phantom line still fit in two
        // digits. One more line does not.
        let (_, buffer_id) = h.stoat.focused_editor_ids().expect("a focused editor");
        {
            let buffer = h
                .stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer");
            let mut guard = buffer.write().expect("poisoned");
            let end = guard.snapshot.visible_text.len();
            guard.edit(end..end, "line 99\n");
        }

        rendered_gutter(&mut h.stoat, true, false, LineNumbers::Absolute, 6);
        let wide = action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("focused editor")
            .gutter_width;
        assert_eq!(
            wide,
            narrow + 1,
            "the hundredth line widens the gutter by its extra digit",
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

    /// A mark's scope resolves on the first row that paints it and is reused by
    /// the rest, so a second row of the same kind has to come out in that kind's
    /// color rather than whatever the reuse hands back.
    #[test]
    fn fallback_gutter_reuses_a_resolved_mark_style_across_rows() {
        use crate::{diff_map::DiffHunkStatus, theme::scope as s};

        let theme = {
            let src = r##"theme t { ui.text.fg = "#111111"; diff.modified.fg = "#22ff22"; diff.unstaged.fg = "#3333ff"; }"##;
            let (config, _) = stoat_config::parse(src);
            crate::theme::Theme::from_config(&config.expect("theme parses"), "t")
                .expect("theme loads")
        };

        let folded = [(1u32, 1u16), (2, 1)];
        let mut diff_marks = std::collections::BTreeMap::new();
        diff_marks.insert(0u32, (DiffHunkStatus::Modified, false));
        diff_marks.insert(1u32, (DiffHunkStatus::Modified, false));
        let area = Rect::new(0, 0, 12, 2);
        let mut buf = Buffer::empty(area);

        super::draw_fallback_line_numbers(
            &folded,
            1,
            &std::collections::BTreeMap::new(),
            &diff_marks,
            None,
            area,
            &theme,
            &mut buf,
        );

        let modified = theme.get(s::DIFF_MODIFIED).fg.expect("the fixture sets it");
        let unstaged = theme.get(s::DIFF_UNSTAGED).fg.expect("the fixture sets it");
        assert_ne!(
            modified,
            theme.get(s::UI_TEXT).fg.expect("the fixture sets it"),
            "the fixture must give the mark its own color"
        );
        assert_eq!(
            (
                buf[(2u16, 0u16)].fg,
                buf[(2u16, 1u16)].fg,
                buf[(3u16, 0u16)].fg,
                buf[(3u16, 1u16)].fg
            ),
            (modified, modified, unstaged, unstaged),
            "both rows paint their marks in the kind's color",
        );
    }

    fn open_wide_row(h: &mut crate::test_harness::TestHarness, root: &str) {
        let root = PathBuf::from(root);
        let path = root.join("wide.txt");
        h.fake_fs()
            .insert_file(&path, "\u{6c49}\u{5b57}ab\n".as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
    }

    #[test]
    fn wide_glyphs_advance_by_display_width() {
        let mut h = Stoat::test();
        open_wide_row(&mut h, "/cjk-glyphs");

        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        super::render_editor(editor, area, fallback, &theme, None, &mut buf, true);

        let base = (0..area.width)
            .find(|&x| buf[(x, 0)].symbol() == "\u{6c49}")
            .expect("the first wide glyph is painted");
        assert_eq!(
            buf[(base + 1, 0)].symbol(),
            " ",
            "the wide glyph clears its trailing cell",
        );
        assert_eq!(buf[(base + 2, 0)].symbol(), "\u{5b57}");
        assert_eq!(buf[(base + 3, 0)].symbol(), " ");
        assert_eq!(buf[(base + 4, 0)].symbol(), "a");
        assert_eq!(buf[(base + 5, 0)].symbol(), "b");

        let snapshot = editor.display_map.snapshot();
        let a_point = snapshot.buffer_snapshot().rope().offset_to_point(6);
        assert_eq!(
            snapshot.buffer_to_display(a_point).column,
            4,
            "a is painted at the display column the width model reports",
        );
    }

    /// Render `content` as the only file and hand back the painted buffer.
    fn painted_row(h: &mut crate::test_harness::TestHarness, root: &str, content: &str) -> Buffer {
        let root = PathBuf::from(root);
        let path = root.join("marks.txt");
        h.fake_fs().insert_file(&path, content.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        super::render_editor(editor, area, fallback, &theme, None, &mut buf, true);
        buf
    }

    #[test]
    fn a_combining_mark_paints_onto_the_character_it_sits_on() {
        let mut h = Stoat::test();
        let buf = painted_row(&mut h, "/marks-narrow", "ae\u{301}b\n");

        let base = (0..20u16)
            .find(|&x| buf[(x, 0u16)].symbol().starts_with('a'))
            .expect("the row is painted");
        assert_eq!(buf[(base, 0u16)].symbol(), "a");
        assert_eq!(
            buf[(base + 1, 0u16)].symbol(),
            "e\u{301}",
            "the accent rides in the cell holding its letter",
        );
        assert_eq!(buf[(base + 2, 0u16)].symbol(), "b");
    }

    #[test]
    fn a_combining_mark_on_a_wide_character_paints_on_its_first_cell() {
        // The column has already advanced two cells past a wide character, so
        // the mark belongs two back rather than one, where the cleared second
        // half of the glyph sits.
        let mut h = Stoat::test();
        let buf = painted_row(&mut h, "/marks-wide", "a\u{6c49}\u{301}b\n");

        let base = (0..20u16)
            .find(|&x| buf[(x, 0u16)].symbol().starts_with('a'))
            .expect("the row is painted");
        assert_eq!(buf[(base + 1, 0u16)].symbol(), "\u{6c49}\u{301}");
        assert_eq!(
            buf[(base + 2, 0u16)].symbol(),
            " ",
            "the wide glyph's second cell stays cleared",
        );
        assert_eq!(buf[(base + 3, 0u16)].symbol(), "b");
    }

    #[test]
    fn a_row_opening_with_a_combining_mark_drops_it() {
        // Nothing precedes the mark, so there is no cell for it to join. The
        // row above is longer, so a mark landing at the column that row ended
        // on would survive rather than being painted over by this one.
        let mut h = Stoat::test();
        let buf = painted_row(&mut h, "/marks-leading", "abcdef\n\u{301}x\n");

        let second_row: String = (0..20u16).map(|x| buf[(x, 1u16)].symbol()).collect();
        assert!(
            second_row.starts_with('x'),
            "the rest of the row still paints, got {second_row:?}",
        );
        assert!(
            !second_row.contains('\u{301}'),
            "and nothing carries over from the row above, got {second_row:?}",
        );
    }

    /// Render `content` with the whole first line selected, and hand back that
    /// row's cells.
    fn selected_first_row(
        h: &mut crate::test_harness::TestHarness,
        content: &str,
    ) -> (
        Vec<ratatui::buffer::Cell>,
        std::sync::Arc<crate::theme::Theme>,
    ) {
        open_search_buffer(h, content);
        let theme = h.stoat.theme.clone();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);

        dispatch(&mut h.stoat, &ExtendToLineEnd);

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
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
        let row = (0..area.width).map(|x| buf[(x, 0)].clone()).collect();
        (row, theme)
    }

    #[test]
    fn a_selected_wide_glyph_is_washed_across_both_its_cells() {
        let mut h = Stoat::test();
        let (row, theme) = selected_first_row(&mut h, "\u{6c49}\u{5b57}\u{4e00}\nx");
        let sel_bg = theme.get(crate::theme::scope::UI_SELECTION_EDITOR).bg;
        assert!(sel_bg.is_some(), "the theme washes selections");

        // The wash skips the character the cursor sits on, which the line-end
        // extend leaves on the last glyph, so the first two are what it covers.
        let washed: Vec<bool> = (0..4).map(|x| Some(row[x].bg) == sel_bg).collect();
        assert_eq!(
            washed,
            vec![true; 4],
            "both cells of both glyphs, not every other one",
        );
    }

    #[test]
    fn a_selected_tab_is_washed_across_the_cells_it_spans() {
        // A tab is not two cells wide, it is however many reach the next stop,
        // so the count comes from the column advance rather than the character.
        let mut h = Stoat::test();
        let (row, theme) = selected_first_row(&mut h, "\tx\ny");
        let sel_bg = theme.get(crate::theme::scope::UI_SELECTION_EDITOR).bg;

        // The x carries the cursor and so is not washed, leaving the tab's own
        // four cells as what the selection covers.
        let washed: Vec<bool> = (0..4).map(|x| Some(row[x].bg) == sel_bg).collect();
        assert_eq!(washed, vec![true; 4], "every cell the tab spans");
    }

    #[test]
    fn a_selected_wide_glyph_on_an_inlay_row_is_washed_across_both_cells() {
        // An inlay on the row takes the paint off its fast path and onto the
        // one that recomputes each character's column, which has to widen the
        // wash the same way.
        let mut h = Stoat::test();
        open_search_buffer(&mut h, "\u{6c49}\u{5b57}x\ny");
        let theme = h.stoat.theme.clone();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let sel_bg = theme.get(crate::theme::scope::UI_SELECTION_EDITOR).bg;

        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            let inserts = {
                let snapshot = editor.display_map.snapshot();
                let buf_snap = snapshot.buffer_snapshot();
                vec![(
                    buf_snap.anchor_at(6, Bias::Left),
                    ": i32".to_string(),
                    crate::display_map::InlayKind::Hint,
                )]
            };
            editor.display_map.splice_inlays(Vec::new(), inserts);
        }
        dispatch(&mut h.stoat, &ExtendToLineEnd);

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
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

        let washed: Vec<bool> = (0..4).map(|x| Some(buf[(x, 0)].bg) == sel_bg).collect();
        assert_eq!(washed, vec![true; 4], "both cells of both glyphs");
    }

    #[test]
    fn a_cursor_on_a_wide_glyph_covers_both_its_cells() {
        let mut h = Stoat::test();
        open_search_buffer(&mut h, "\u{6c49}x\ny");
        let theme = h.stoat.theme.clone();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let cursor_modifier = theme.cursor_style().add_modifier;

        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let area = Rect::new(0, 0, 40, 4);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
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

        assert!(
            !cursor_modifier.is_empty(),
            "the cursor style is distinguishable",
        );
        assert_eq!(
            (
                buf[(0u16, 0u16)].modifier.contains(cursor_modifier),
                buf[(1u16, 0u16)].modifier.contains(cursor_modifier),
            ),
            (true, true),
            "the block cursor covers the whole glyph it sits on",
        );
    }

    #[test]
    fn the_cursor_lands_on_the_glyph_after_wide_chars() {
        let mut h = Stoat::test();
        open_wide_row(&mut h, "/cjk-cursor");
        dispatch(&mut h.stoat, &MoveRight);
        dispatch(&mut h.stoat, &MoveRight);
        let cursor_col = h.cursor_display_positions()[0].1;

        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let area = Rect::new(0, 0, 20, 2);
        let mut buf = Buffer::empty(area);
        super::render_editor(editor, area, fallback, &theme, None, &mut buf, true);

        let base = (0..area.width)
            .find(|&x| buf[(x, 0)].symbol() == "\u{6c49}")
            .expect("the first wide glyph is painted");
        let a_col = (0..area.width)
            .find(|&x| buf[(x, 0)].symbol() == "a")
            .expect("a is painted");
        assert_eq!(
            (a_col - base) as u32,
            cursor_col,
            "the a glyph is painted at the cursor's display column",
        );
    }

    /// Open a line many times wider than any test pane, carrying wide chars
    /// past the right edge and spanning several rope chunks, followed by a
    /// second line whose row proves the newline reset survives the skip.
    fn open_over_wide_line(h: &mut crate::test_harness::TestHarness, root: &str) {
        let root = PathBuf::from(root);
        let path = root.join("wide-line.txt");
        let line = format!(
            "{}{}{}",
            "x".repeat(10),
            "\u{6c49}".repeat(40),
            "y".repeat(30),
        );
        h.fake_fs()
            .insert_file(&path, format!("{line}\nsecond line\n").as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();
    }

    fn row_text(buf: &Buffer, y: u16, width: u16) -> String {
        (0..width).map(|x| buf[(x, y)].symbol()).collect()
    }

    #[test]
    fn an_over_wide_line_paints_its_visible_prefix_and_the_next_row() {
        let mut h = Stoat::test();
        open_over_wide_line(&mut h, "/over-wide");

        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let area = Rect::new(0, 0, 20, 3);
        let mut buf = Buffer::empty(area);
        super::render_editor(editor, area, fallback, &theme, None, &mut buf, true);

        assert_eq!(
            row_text(&buf, 0, area.width),
            "xxxxxxxxxx\u{6c49} \u{6c49} \u{6c49} \u{6c49} \u{6c49} ",
            "the over-wide line paints only the glyphs that fit",
        );
        assert_eq!(
            row_text(&buf, 1, area.width),
            "second line         ",
            "the newline resets the column after the skipped remainder",
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
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
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
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        {
            let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
            super::render_editor_with_overlay(
                editor,
                area,
                fallback,
                &theme,
                &chrome,
                &mut buf,
                true,
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
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let area = Rect::new(0, 0, 12, rows);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
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
            .geometry_key
    }

    /// The focused editor's cached folded gutter lines.
    ///
    /// Clearing them is a rebuild sentinel. The next paint either reuses the
    /// cache and leaves them empty, or rebuilds the geometry and repopulates
    /// them. Their address is the same sentinel without the mutation, since a
    /// rebuild allocates its replacement while the old vec is still cached.
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

    /// Paint the focused editor's rich sub-cell gutter with relative numbering
    /// and return the number each cached component line carries.
    ///
    /// A scene plus the shipped theme is what engages the rich path, where the
    /// component lines are the artifact the relative numbers land in.
    fn rich_gutter_numbers(stoat: &mut Stoat, rows: u16) -> Vec<u32> {
        let theme = stoat.theme.clone();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let mut scene = super::ApcScene::new();
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let area = Rect::new(0, 0, 12, rows);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
            false,
            LineNumbers::Relative,
            false,
            None,
            None,
            None,
            None,
            Some(&mut scene),
            None,
            0.0,
            WrapMode::None,
            80,
        );
        editor
            .gutter_geometry_cache
            .as_ref()
            .expect("gutter cache set")
            .lines
            .iter()
            .map(|line| line.number)
            .collect()
    }

    /// Relative numbering is the default, so a cursor move changes the painted
    /// numbers on a gutter whose geometry is untouched. Rebuilding the geometry
    /// for it would re-resolve every diff hunk in the file per keypress.
    #[test]
    fn a_cursor_move_renumbers_the_gutter_lines_and_reuses_its_geometry() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/gutter-relnum-cache");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        assert_eq!(
            rich_gutter_numbers(&mut h.stoat, 5),
            [1, 1, 2, 3, 4],
            "the cursor's line keeps its absolute number, the rest their distance",
        );
        let geometry = cached_folded(&mut h.stoat).as_ptr();

        dispatch(&mut h.stoat, &MoveDown);

        assert_eq!(
            rich_gutter_numbers(&mut h.stoat, 5),
            [1, 2, 1, 2, 3],
            "the moved cursor renumbers every line",
        );
        assert_eq!(
            cached_folded(&mut h.stoat).as_ptr(),
            geometry,
            "renumbering reuses the cached geometry instead of resolving the hunks again",
        );
    }

    /// Paint the focused editor into `area` and return the APC frame it emits.
    ///
    /// Read before any flush, so the decoration lane still holds the frame the
    /// paint just built. Handed back as text because the frame is ASCII
    /// throughout, and a mismatch between two of them reads as commands rather
    /// than as a pair of byte arrays.
    fn gutter_scene_frame(stoat: &mut Stoat, area: Rect) -> String {
        let theme = stoat.theme.clone();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let mut scene = super::ApcScene::new();
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
            false,
            LineNumbers::Relative,
            false,
            None,
            None,
            None,
            None,
            Some(&mut scene),
            None,
            0.0,
            WrapMode::None,
            80,
        );
        String::from_utf8_lossy(scene.bytes()).into_owned()
    }

    /// The same paint from an empty cache, which is the frame a full encode
    /// writes with nothing to splice.
    fn cold_gutter_scene_frame(stoat: &mut Stoat, area: Rect) -> String {
        action_handlers::focused_editor_mut(stoat)
            .expect("focused editor")
            .gutter_geometry_cache = None;
        gutter_scene_frame(stoat, area)
    }

    /// The memoized gutter frame is spliced whole, so it is only correct while
    /// it matches what a full encode writes. Each input the splice is keyed on
    /// is moved in turn, since a key that misses one paints the previous frame
    /// at the new state.
    #[test]
    fn a_spliced_gutter_frame_matches_a_freshly_encoded_one() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/gutter-scene-memo");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"one\ntwo\nthree\nfour\nfive");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let area = Rect::new(0, 0, 12, 5);
        gutter_scene_frame(&mut h.stoat, area);
        let spliced = gutter_scene_frame(&mut h.stoat, area);

        assert!(!spliced.is_empty(), "the rich gutter emits an APC frame");
        assert_eq!(
            spliced,
            cold_gutter_scene_frame(&mut h.stoat, area),
            "an unchanged repaint splices the frame a full encode writes",
        );

        dispatch(&mut h.stoat, &MoveDown);
        let moved = gutter_scene_frame(&mut h.stoat, area);

        assert_ne!(moved, spliced, "the moved cursor renumbers the gutter");
        assert_eq!(
            moved,
            cold_gutter_scene_frame(&mut h.stoat, area),
            "a cursor move re-encodes instead of splicing the numbers it cached",
        );

        let shifted = Rect::new(3, 1, 12, 5);
        let at_shifted = gutter_scene_frame(&mut h.stoat, shifted);

        assert_ne!(
            at_shifted, moved,
            "the frame is positioned against the rect"
        );
        assert_eq!(
            at_shifted,
            cold_gutter_scene_frame(&mut h.stoat, shifted),
            "a moved pane re-encodes instead of splicing the frame from its old rect",
        );
    }

    /// Render the focused editor's gutter into cells and return the trimmed
    /// number string each visible row paints.
    ///
    /// Passes no scene, so the gutter takes its cell fallback and the numbers
    /// land as real glyphs the assertions can read.
    fn rendered_gutter(
        stoat: &mut Stoat,
        is_focused: bool,
        insert_mode: bool,
        line_numbers: LineNumbers,
        rows: u16,
    ) -> Vec<String> {
        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let area = Rect::new(0, 0, 12, rows);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            is_focused,
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

    /// Render the focused editor at `width` x `rows` under the given minimap
    /// flag, returning the recorded strip rect and, per row, the
    /// symbols painted in the rightmost [`super::MINIMAP_STRIP_COLS`] columns.
    fn render_minimap(
        stoat: &mut Stoat,
        minimap_enabled: bool,
        width: u16,
        rows: u16,
    ) -> (Option<Rect>, Vec<String>) {
        let theme = crate::theme::Theme::empty();
        let fallback = theme.get(crate::theme::scope::UI_TEXT);
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let area = Rect::new(0, 0, width, rows);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
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

        let (rect, strip) = render_minimap(&mut h.stoat, true, 120, 3);
        assert_eq!(
            rect,
            Some(Rect::new(112, 0, super::MINIMAP_STRIP_COLS, 3)),
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
            render_minimap(&mut h.stoat, false, 120, 3).0,
            None,
            "no strip when the minimap is disabled"
        );
        assert_eq!(
            render_minimap(&mut h.stoat, true, 107, 3).0,
            None,
            "no strip one column below the minimum pane width"
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
        let chrome = crate::render::editor::ResolvedChrome::resolve(&theme);
        let editor = action_handlers::focused_editor_mut(stoat).expect("focused editor");
        let area = Rect::new(0, 0, 12, rows);
        let mut buf = Buffer::empty(area);
        super::render_editor_with_overlay(
            editor,
            area,
            fallback,
            &theme,
            &chrome,
            &mut buf,
            true,
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

    /// A singleton multi-buffer over `content`, for tests that need a snapshot
    /// to resolve diagnostics against.
    fn snapshot_over(content: &str) -> crate::multi_buffer::MultiBufferSnapshot {
        use crate::{buffer::TextBuffer, multi_buffer::MultiBuffer};
        use std::sync::{Arc, RwLock};
        let id = stoat_text::BufferId::new(0);
        let buffer = TextBuffer::with_text(id, content);
        MultiBuffer::singleton(id, Arc::new(RwLock::new(buffer))).snapshot()
    }

    /// A span anchored over `range` in `snapshot`, the way a publish against
    /// that text would have anchored it.
    fn anchored_span(
        snapshot: &crate::multi_buffer::MultiBufferSnapshot,
        range: std::ops::Range<usize>,
    ) -> crate::diagnostics::PublishedSpan {
        crate::diagnostics::PublishedSpan {
            anchors: Some((
                snapshot.anchors_at_batch(&[range.start], Bias::Right)[0],
                snapshot.anchors_at_batch(&[range.end], Bias::Left)[0],
            )),
        }
    }

    #[test]
    fn diagnostic_at_offset_finds_worst_containing_span() {
        let path = PathBuf::from("/a");
        let snapshot = snapshot_over("let x = 1;\n");
        let mut set = crate::diagnostics::DiagnosticSet::new();
        // A warning over just `x` [4,5) and an error over `x = 1` [4,9).
        set.replace_from_server(
            path.clone(),
            "lsp".into(),
            vec![
                span_diag(4, 5, DiagnosticSeverity::WARNING),
                span_diag(4, 9, DiagnosticSeverity::ERROR),
            ],
            vec![
                anchored_span(&snapshot, 4..5),
                anchored_span(&snapshot, 4..9),
            ],
        );

        let spans = super::resolve_diagnostic_spans(&set, &path, &snapshot);
        // Offset 4 is in both, so the worse severity (the error) wins.
        assert_eq!(super::diagnostic_at_offset(&spans, 4), Some(1));
        // Offset 7 is inside only the error span.
        assert_eq!(super::diagnostic_at_offset(&spans, 7), Some(1));
        // Offset 0 is outside both.
        assert_eq!(super::diagnostic_at_offset(&spans, 0), None);
    }

    /// A cache over spans at the given byte ranges, ordered as
    /// [`super::resolve_diagnostic_spans`] leaves them.
    fn span_cache(ranges: &[(usize, usize)]) -> super::DiagnosticSpanCache {
        let spans: Vec<super::ResolvedDiag> = ranges
            .iter()
            .enumerate()
            .map(|(index, &(start, end))| super::ResolvedDiag {
                start,
                end,
                severity: DiagnosticSeverity::WARNING,
                unnecessary: false,
                start_line: 0,
                end_line: 0,
                index,
            })
            .collect();

        super::DiagnosticSpanCache {
            set_version: 0,
            buffer_version: 0,
            prefix_max_end: super::prefix_max_ends(&spans),
            prefix_max_end_line: super::prefix_max_end_lines(&spans),
            cursor_line_diag: None,
            spans,
        }
    }

    /// Spans are ordered by start, so a long one opening above the viewport sits
    /// among short ones that close before it reaches them. Bounding the walk by
    /// the running maximum of the ends is what keeps that span while still
    /// skipping its neighbours.
    #[test]
    fn the_overlap_bound_keeps_a_span_reaching_in_from_above() {
        let cache = span_cache(&[
            (0, 5),
            (10, 400),
            (20, 25),
            (30, 35),
            (200, 205),
            (500, 505),
        ]);

        assert_eq!(
            cache.overlapping(100..300),
            1..5,
            "the walk opens at the long span and closes before the one below",
        );
        assert_eq!(
            cache.overlapping(600..700),
            6..6,
            "a viewport past every span walks nothing",
        );
    }

    /// A zero-width span sits at an offset it neither starts before nor ends
    /// after, which leaves the two bounds crossed. The bound has to come back in
    /// order anyway, since the caller indexes the spans with it.
    #[test]
    fn the_overlap_bound_survives_a_zero_width_span_at_the_viewport_start() {
        let cache = span_cache(&[(5, 5)]);

        assert!(cache.spans[cache.overlapping(5..5)].is_empty());
    }

    /// A cache over spans at the given `(start_line, end_line, severity)`, laid
    /// out one line apart so the byte order matches the row order.
    fn line_span_cache(rows: &[(u32, u32, DiagnosticSeverity)]) -> super::DiagnosticSpanCache {
        let spans: Vec<super::ResolvedDiag> = rows
            .iter()
            .enumerate()
            .map(
                |(index, &(start_line, end_line, severity))| super::ResolvedDiag {
                    start: start_line as usize * 10,
                    end: end_line as usize * 10,
                    severity,
                    unnecessary: false,
                    start_line,
                    end_line,
                    index,
                },
            )
            .collect();

        super::DiagnosticSpanCache {
            set_version: 0,
            buffer_version: 0,
            prefix_max_end: super::prefix_max_ends(&spans),
            prefix_max_end_line: super::prefix_max_end_lines(&spans),
            cursor_line_diag: None,
            spans,
        }
    }

    /// A diagnostic reaching the cursor's line from above still wins the
    /// readout, and a bound in rows is what keeps it. The end offset of a span
    /// ending at column 0 is the byte the line starts at, so a bound in offsets
    /// reads it as settled and drops the span the row filter accepts.
    #[test]
    fn the_line_bound_keeps_a_span_ending_at_the_line_start() {
        let mut cache = line_span_cache(&[
            (0, 3, DiagnosticSeverity::WARNING),
            (5, 5, DiagnosticSeverity::ERROR),
        ]);

        assert_eq!(
            cache.cursor_line_diagnostic(3).map(|d| d.index),
            Some(0),
            "the span reaching line 3 from line 0 wins it",
        );
        assert_eq!(
            cache.cursor_line_diagnostic(4).map(|d| d.index),
            None,
            "and the line below it is inside nothing",
        );
    }

    /// The worst severity wins a line, and ties go to the earliest span, which
    /// is what a scan of the whole slice does. The bound preserves the order the
    /// spans are sorted in, so narrowing it leaves both answers alone.
    #[test]
    fn the_worst_severity_wins_the_cursor_line() {
        let mut cache = line_span_cache(&[
            (2, 2, DiagnosticSeverity::WARNING),
            (2, 2, DiagnosticSeverity::ERROR),
            (2, 2, DiagnosticSeverity::ERROR),
        ]);

        assert_eq!(
            cache.cursor_line_diagnostic(2).map(|d| d.index),
            Some(1),
            "the first of the two errors wins over the warning",
        );
    }

    /// The readout runs on every frame the cursor sits inside a diagnostic, and
    /// only a cursor that changes line changes the answer. Clearing the spans
    /// under the cache is what makes the reuse visible. A fresh search over the
    /// emptied slice finds nothing.
    #[test]
    fn a_cursor_that_stays_on_its_line_answers_from_the_memo() {
        let mut cache = line_span_cache(&[(1, 1, DiagnosticSeverity::ERROR)]);

        assert_eq!(cache.cursor_line_diagnostic(1).map(|d| d.index), Some(0));

        // Emptied together, since the bound indexes the spans through the prefix
        // maximums and a real cache never holds one without the others.
        cache.spans.clear();
        cache.prefix_max_end.clear();
        cache.prefix_max_end_line.clear();

        assert_eq!(
            cache.cursor_line_diagnostic(1).map(|d| d.index),
            Some(0),
            "the same line answers from the memo rather than searching again",
        );
        assert_eq!(
            cache.cursor_line_diagnostic(2).map(|d| d.index),
            None,
            "and a moved cursor searches, over the spans that are there now",
        );
    }

    /// Anchoring at publish is what makes a mark follow its text. The offsets a
    /// server named are in the coordinates of text every later edit moves, so a
    /// resolution reading them back would drift off what it marked.
    #[test]
    fn an_edit_above_a_diagnostic_carries_it_down() {
        use crate::{buffer::TextBuffer, multi_buffer::MultiBuffer};
        use std::sync::{Arc, RwLock};

        let path = PathBuf::from("/a");
        let id = stoat_text::BufferId::new(0);
        let buffer = Arc::new(RwLock::new(TextBuffer::with_text(id, "alpha\nbravo\n")));
        let multi = MultiBuffer::singleton(id, buffer.clone());

        // `bravo` is [6, 11), on the second line.
        let published = multi.snapshot();
        let mut set = crate::diagnostics::DiagnosticSet::new();
        set.replace_from_server(
            path.clone(),
            "lsp".into(),
            vec![span_diag(0, 5, DiagnosticSeverity::ERROR)],
            vec![anchored_span(&published, 6..11)],
        );

        assert_eq!(
            resolved(&set, &path, &published),
            [(0, 6, 11, 1)],
            "over the word it was published on",
        );

        // Nine bytes and a line inserted ahead of it, with no republish behind.
        buffer.write().expect("poisoned").edit(0..0, "inserted\n");

        assert_eq!(
            resolved(&set, &path, &multi.snapshot()),
            [(0, 15, 20, 2)],
            "and still over that word, a line further down",
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

    /// The chrome is resolved once and reused across frames, so it has to
    /// notice a theme replacement. Not every replacement bumps `theme_epoch`,
    /// which is why the cache keys on the theme handle instead.
    #[test]
    fn the_chrome_resolves_once_and_refreshes_on_a_new_theme() {
        let mut h = Stoat::test();
        h.stoat.refresh_chrome();
        let first = h
            .stoat
            .chrome
            .as_ref()
            .expect("resolved above")
            .1
            .diff_marks;

        h.stoat.refresh_chrome();
        assert_eq!(
            h.stoat.chrome.as_ref().expect("still resolved").0.as_ref() as *const _,
            h.stoat.theme.as_ref() as *const _,
            "an unchanged theme reuses what was already resolved",
        );

        // A theme replacement that leaves theme_epoch alone, which is what
        // reloading the config does.
        let epoch = h.stoat.theme_epoch;
        h.stoat.theme = std::sync::Arc::new(crate::theme::Theme::empty());
        h.stoat.refresh_chrome();
        assert_eq!(h.stoat.theme_epoch, epoch, "the epoch did not move");

        let after = h.stoat.chrome.as_ref().expect("re-resolved").1.diff_marks;
        assert_ne!(
            (first.added, first.deleted),
            (after.added, after.deleted),
            "the new theme's diff colors replaced the old ones",
        );
    }

    #[test]
    fn a_diagnostic_span_collects_an_undercurl_under_stoatty() {
        let mut h = Stoat::test();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/undercurl-test");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();
        h.seed_diagnostics(path, vec![diag(0, DiagnosticSeverity::WARNING)]);

        let _ = h.stoat.render();

        assert_eq!(
            h.stoat.pending_undercurls.spans().len(),
            1,
            "the warning span paints one underline run",
        );
        assert_eq!(
            h.stoat.pending_undercurls.spans()[0].color,
            [0xe5, 0xc0, 0x7b],
            "the run carries the shipped warning severity color",
        );
    }

    /// The paint bounds which spans it walks by the viewport. A span opening far
    /// above a scrolled viewport and closing inside it still paints, and the
    /// short ones it was sorted among do not.
    #[test]
    fn a_span_reaching_into_a_scrolled_viewport_still_paints() {
        let mut h = Stoat::test();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/undercurl-scrolled");
        let path = root.join("a.txt");
        let text = vec!["alpha"; 60].join("\n");
        h.fake_fs().insert_file(&path, text.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        // Short hints on the top rows, all closing well above the viewport, plus
        // one warning opening on row 0 and closing inside it.
        let mut diags: Vec<Diagnostic> = (0..20)
            .map(|line| overlap_diag(line, 0, 1, DiagnosticSeverity::HINT))
            .collect();
        diags.push(Diagnostic {
            range: Range {
                start: Position {
                    line: 0,
                    character: 0,
                },
                end: Position {
                    line: 45,
                    character: 3,
                },
            },
            severity: Some(DiagnosticSeverity::WARNING),
            message: String::new(),
            ..Default::default()
        });
        h.seed_diagnostics(path, diags);

        action_handlers::focused_editor_mut(&mut h.stoat)
            .unwrap()
            .scroll_row = 40;
        let _ = h.stoat.render();

        let colors: Vec<[u8; 3]> = h
            .stoat
            .pending_undercurls
            .spans()
            .iter()
            .map(|u| u.color)
            .collect();
        assert!(
            !colors.is_empty() && colors.iter().all(|c| *c == [0xe5, 0xc0, 0x7b]),
            "only the warning reaches the viewport, got {colors:?}",
        );
    }

    /// The paint refills collections that outlive it on the editor, so each one
    /// has to start from empty. A leaked ordered subset repaints spans the
    /// diagnostics have since dropped.
    #[test]
    fn a_repaint_drops_the_spans_the_diagnostics_no_longer_carry() {
        let mut h = Stoat::test();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_apc_tx(tx);

        let root = PathBuf::from("/undercurl-repaint");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\nbravo\ncharlie\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        h.seed_diagnostics(
            path.clone(),
            vec![
                overlap_diag(0, 0, 5, DiagnosticSeverity::WARNING),
                overlap_diag(1, 0, 5, DiagnosticSeverity::WARNING),
            ],
        );
        let _ = h.stoat.render();
        assert_eq!(
            h.stoat.pending_undercurls.spans().len(),
            2,
            "both spans paint"
        );

        h.seed_diagnostics(
            path,
            vec![overlap_diag(2, 0, 5, DiagnosticSeverity::WARNING)],
        );
        let _ = h.stoat.render();
        assert_eq!(
            h.stoat.pending_undercurls.spans().len(),
            1,
            "only the span the new set carries paints",
        );
    }

    /// A leaked mute set skips the cells it blended last frame, so the inactive
    /// region stops being greyed from the second paint on.
    #[test]
    fn an_unchanged_repaint_mutes_the_same_cells() {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-remute");
        let path = root.join("a.rs");
        h.fake_fs().insert_file(&path, b"let x = 1;\nlet y = 2;\n");
        h.stoat.active_workspace_mut().git_root = root;
        dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        h.seed_diagnostics(
            path,
            vec![tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT)],
        );

        let first = h.render_composited();
        let second = h.render_composited();
        assert_eq!(first, second, "nothing moved, so the paint repeats exactly");
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
        h.seed_diagnostics(
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
        h.seed_diagnostics(
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
        h.seed_diagnostics(
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
        h.seed_diagnostics(
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
        h.seed_diagnostics(
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
        h.seed_diagnostics(
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
        h.seed_diagnostics(
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

        h.seed_diagnostics(
            path.clone(),
            vec![tagged_overlap_diag(1, 0, 10, DiagnosticSeverity::HINT)],
        );
        let once = row_fg(&mut h.stoat);

        h.seed_diagnostics(
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
        h.seed_diagnostics(
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

        h.snapshot();
        // Column 4 is the line-number gutter width the cursor sits past.
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((4, 0)));

        for _ in 0..6 {
            dispatch(&mut h.stoat, &MoveRight);
        }
        h.snapshot();
        assert_eq!(h.stoat.primary_cursor_screen_pos(), Some((10, 0)));
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

    /// A soft-wrapped row leaves the accumulating paint path, so the general
    /// one resolves each character from a buffer point it carries forward. That
    /// point's column counts bytes, so an accent moves it two and a drift of one
    /// per character walks the wash off the text it covers.
    #[test]
    fn snapshot_selection_over_a_wrapped_line_of_accents() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 4);
        let path = h.write_file("s.txt", &format!("{}\n", "\u{e9}".repeat(30)));
        h.open_file(&path);
        dispatch(&mut h.stoat, &ExtendToLineEnd);
        h.assert_snapshot("selection_over_wrapped_accents");
    }

    /// Tabs and wide glyphs on a wrapped row are where an accumulated column is
    /// likeliest to part ways with a resolved one. A tab's width depends on
    /// where it starts, and a wide glyph moves the column by two.
    ///
    /// The paint loop's debug assertion turns this into a per-character
    /// comparison against the authoritative mapping. The snapshot pins the
    /// cells that come out of it.
    #[test]
    fn snapshot_selection_over_a_wrapped_line_of_tabs_and_wide_chars() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 5);
        let path = h.write_file(
            "s.txt",
            "\u{4e16}\u{754c}\ta\u{4e16}b\t\u{754c}cd\u{4e16}\te\u{754c}f\n",
        );
        h.open_file(&path);
        dispatch(&mut h.stoat, &ExtendToLineEnd);
        h.assert_snapshot("selection_over_wrapped_tabs_and_wide");
    }

    #[test]
    fn snapshot_selection_over_wide_chars() {
        let mut h = crate::test_harness::TestHarness::with_size(20, 4);
        let path = h.write_file("s.txt", "a世z\n");
        h.open_file(&path);
        dispatch(&mut h.stoat, &ExtendToLineEnd);
        // The text pass advances by display width per glyph, so glyphs after a
        // wide char stay aligned with the selection and cursor columns, which
        // also account for display width. This locks that width-aware column math.
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
