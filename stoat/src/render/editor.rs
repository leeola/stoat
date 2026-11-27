use crate::{
    action_handlers::view,
    auto_reload::ReloadFlash,
    buffer::BufferId,
    diagnostic_spans::{
        build_diagnostic_span_cache, diagnostic_at_offset, line_start_diagnostics, severity_rank,
        ResolvedDiag,
    },
    diff_map::{DiffHunkStatus, GutterMark, MarkState},
    display_map::{
        display_width, tab_map, BlockChunks, BlockRowKind, DisplayPoint, DisplaySnapshot,
        PaintVersion, TabPoint, WRAP_MARK,
    },
    editor_state::EditorState,
    minimap::color_to_rgb,
    render::{
        blame::{self, BlameColors, BlamePaint},
        conflict_view::render_conflict_view,
        paint::{dim_rgb, floor_style, style_rgb},
        review::{
            self, render_diff_view, BufferNumbering, DiffColumns, DiffDials, DiffLayout,
            DIFF_TWO_COLUMN_MIN,
        },
        undercurl::{UndercurlBatch, UnderlineKind},
        TypeScale,
    },
    theme::{Theme, TypeRole},
};
use lsp_types::DiagnosticSeverity;
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
use stoatty_protocol::command::{IconKind, PopoverChrome, PopoverSpan};

/// Columns reserved on a pane's right edge for the minimap strip, matching the
/// width the terminal's GPU minimap pass paints there.
pub const MINIMAP_STRIP_COLS: u16 = 7;

/// Widest display row [`WrapMode::None`] produces, in columns.
///
/// Turning wrapping off asks for a display row per buffer line, which makes a
/// multi-megabyte line one row. A frame then walks the whole line to paint the
/// cells the pane shows, and the column of a cell past 65,535 does not fit the
/// coordinate the painter casts it to.
///
/// No pane is this wide, and no horizontal scroll exists, so a row clips at the
/// pane edge well before this. Nothing a user sees changes; the rows the wrap
/// layer builds stay bounded.
pub const MAX_UNWRAPPED_COLUMNS: u32 = 4096;

/// Blank cells between a line's text and the diagnostic message after it.
///
/// The design sets the message 24 px past the text. At the 13 px face the
/// design draws with, three cells cover that distance.
const EOL_MESSAGE_GAP: u32 = 3;

/// Characters a diagnostic popover line holds before it clips.
const POPOVER_COLUMNS: usize = 56;

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
pub fn render_editor(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &Theme,
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
        true,
        None,
        0.0,
        // Not a pane, so it never paints the diff view the scale and the tint
        // apply to.
        DiffDials::shipped(),
        WrapMode::None,
        80,
        0,
        &[],
        false,
        None,
        None,
        false,
    );
}

/// Paint `editor` into `inner`, through its diff or conflict view when it shows
/// one.
///
/// `delegates_cursor` names whether the terminal draws the primary cursor over
/// this editor. With it and a live `scene`, the paint leaves the primary cell
/// to the terminal. Otherwise the paint draws the block itself.
///
/// The editor washes its buffer's entries of `reload_flashes` over the text
/// they changed, focused or not, under its selections and its cursor.
///
/// With `decorations_only` a diff view paints its rich gutter into `scene`
/// alone, for a page whose cells the terminal keeps. The other views ignore
/// the flag.
///
/// A changes view excerpt passes its entry of
/// [`step_origins`](crate::diff_changes::step_origins) as `step_origin`.
/// Under relative numbering its diff view then numbers each row by the `j` or
/// `k` steps from the focused cursor across the whole stack, focused or not.
/// With no step origin, a focused diff view counts buffer lines from its
/// cursor line, and an unfocused one paints absolute numbers.
///
/// With `blame`, a plain editor shows the Blame Column left of its gutter,
/// when the pane has the room that [`blame::blame_cells`] asks for. The column
/// moves the text right, and `gutter_width` counts it.
///
/// With `cursor_line`, a focused plain editor fills every row of its cursor
/// line in `ui.cursorline`, from the pane's left edge to the text's right edge.
/// A cell with a background of its own keeps it, such as a selection wash, a
/// search match, or an inlay hint.
#[allow(clippy::too_many_arguments)]
pub fn render_editor_with_overlay(
    editor: &mut EditorState,
    inner: Rect,
    fallback_style: Style,
    theme: &Theme,
    chrome: &ResolvedChrome,
    buf: &mut Buffer,
    is_focused: bool,
    minimap_enabled: bool,
    line_numbers: LineNumbers,
    insert_mode: bool,
    hover_cell: Option<(u16, u16)>,
    goto_word_labels: Option<&BTreeMap<String, (usize, usize)>>,
    diagnostic_info: Option<(&Path, &crate::diagnostics::DiagnosticSet)>,
    mut scene: Option<&mut ApcScene>,
    delegates_cursor: bool,
    undercurls: Option<&mut UndercurlBatch>,
    dim: f32,
    dials: DiffDials,
    wrap: WrapMode,
    wrap_column: u32,
    scrolloff: u32,
    reload_flashes: &[ReloadFlash],
    decorations_only: bool,
    step_origin: Option<i64>,
    blame: Option<BlamePaint<'_>>,
    cursor_line: bool,
) {
    editor.viewport_rows = Some(inner.height as u32);
    editor.cursor_screen_cell = None;
    editor.ghost_cursor_cell = None;
    editor.minimap_rect = None;
    editor.text_rect = None;
    editor.conflict_layout = None;
    editor.eol_cursor = None;

    if editor.diff_view {
        let inner = reserve_minimap_strip(editor, inner, minimap_enabled);
        let digits = review::diff_gutter_digits_for(editor);
        let rich = review::diff_rich_gutter_scale(scene.is_some(), theme, fallback_style);
        fit_diff_view(editor, inner, wrap, wrap_column, digits, rich);
        view::apply_held_screen_row(editor);
        view::clamp_scroll_to_display(editor, scrolloff);
        // A changes view excerpt counts the steps across its stack. Any other
        // diff view counts from the cursor line on the same condition the plain
        // gutter below counts from it.
        let numbering = match (
            line_numbers == LineNumbers::Relative && !insert_mode,
            step_origin,
        ) {
            (false, _) => BufferNumbering::Absolute,
            (true, Some(origin)) => BufferNumbering::FromSteps { origin },
            (true, None) if is_focused => editor_cursor_position(editor)
                .map_or(BufferNumbering::Absolute, |(line, _)| {
                    BufferNumbering::FromLine(line)
                }),
            (true, None) => BufferNumbering::Absolute,
        };
        render_diff_view(
            editor,
            inner,
            fallback_style,
            theme,
            buf,
            is_focused,
            goto_word_labels,
            scene,
            delegates_cursor,
            dials,
            numbering,
            reload_flashes,
            decorations_only,
        );
        return;
    }

    if editor.conflict_view.is_some() {
        let inner = reserve_minimap_strip(editor, inner, minimap_enabled);
        editor.display_map.set_wrap_width(None);
        // The terminal draws the cursor on the same condition the main editor
        // path delegates on.
        render_conflict_view(
            editor,
            inner,
            fallback_style,
            theme,
            buf,
            scene,
            delegates_cursor,
        );
        return;
    }

    // The gutter measurement and the wrap-width decision ask only buffer-level
    // questions, so they run off a buffer snapshot. A display snapshot taken
    // here would be wasted work, since set_wrap_width below invalidates it and
    // the one the gutter and text paint from re-syncs the wrap from scratch.
    let rich_number_scale = chrome
        .rich_gutter
        .as_ref()
        .filter(|_| scene.is_some())
        .map(|rich| rich.number_scale);
    let measured_gutter_w = if line_numbers != LineNumbers::Off {
        let buffer = editor.display_map.buffer_snapshot();
        measure_gutter_width(gutter_digits(buffer.line_count()), rich_number_scale)
    } else {
        match diagnostic_info {
            Some((path, set)) if !set.get(path).is_empty() => 1,
            _ => 0,
        }
    };

    let blame_w = blame
        .as_ref()
        .map_or(0, |_| blame::blame_cells(inner.width, measured_gutter_w));

    let after_gutter = inner
        .width
        .saturating_sub(measured_gutter_w)
        .saturating_sub(blame_w);
    let minimap_cols = if minimap_enabled && after_gutter >= MINIMAP_MIN_PANE_COLS {
        MINIMAP_STRIP_COLS
    } else {
        0
    };
    let text_width = after_gutter.saturating_sub(minimap_cols);
    // A per-editor ToggleWrap override wins over the frame's configured mode.
    editor.display_map.set_wrap_width(wrap_width_for(
        editor.wrap_override.unwrap_or(wrap),
        u32::from(text_width),
        wrap_column,
    ));
    view::apply_held_screen_row(editor);

    let snapshot = editor.display_map.snapshot();
    let visible_rows = inner.height as u32;
    let total_rows = snapshot.line_count();
    let end_row = (editor.scroll_row + visible_rows).min(total_rows);
    if end_row <= editor.scroll_row {
        return;
    }

    let empty_severity = RowSeverity::default();
    let row_severity: &RowSeverity = match diagnostic_info {
        Some((path, set)) => {
            let version = set.version_for(path);
            let buffer_snapshot = snapshot.buffer_snapshot();
            let buffer_version = buffer_snapshot.version();
            let stale = match &editor.gutter_severity_cache {
                Some(cache) => cache.version != version || cache.buffer_version != buffer_version,
                None => true,
            };
            if stale {
                // The marks and the underline read the same resolution, so they
                // cannot disagree about where a diagnostic sits.
                build_diagnostic_span_cache(editor, set, path, buffer_snapshot);
                let spans: Arc<[ResolvedDiag]> = editor
                    .diagnostic_span_cache
                    .as_ref()
                    .map(|cache| cache.resolve_all(buffer_snapshot).into())
                    .unwrap_or_default();
                editor.gutter_severity_cache = Some(GutterSeverityCache {
                    version,
                    buffer_version,
                    map: Arc::new(row_severity_from_spans(&spans)),
                    spans,
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
    // The 1-based line the paint fills, which the gutter's runs composite over.
    let fill_line = (cursor_line && is_focused && chrome.cursor_line.is_some())
        .then(|| editor_cursor_position(editor).map(|(line, _)| line))
        .flatten();

    let severity_version = diagnostic_info.map_or(0, |(path, set)| set.version_for(path));

    let gutter_area = Rect {
        x: inner.x + blame_w,
        width: inner.width - blame_w,
        ..inner
    };
    if let Some(paint) = blame.as_ref().filter(|_| blame_w > 0) {
        let colors = match chrome.gutter_bg {
            Some(bg) => paint.colors.dim(bg, dim),
            None => paint.colors.clone(),
        };
        let (folded, _) =
            gutter_geometry(&snapshot, editor.scroll_row, end_row - editor.scroll_row);
        blame::paint_blame_column(
            paint.lines.as_ref(),
            &folded,
            Rect {
                width: blame_w,
                ..inner
            },
            &colors,
            buf,
            scene.as_deref_mut(),
        );
    }

    let gutter_w = if line_numbers != LineNumbers::Off {
        draw_line_number_gutter(
            &snapshot,
            editor.scroll_row,
            gutter_area,
            end_row,
            row_severity,
            theme,
            chrome,
            current_line,
            fill_line,
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
            gutter_area,
            end_row,
            severity_version,
            &mut editor.diagnostic_rows_cache,
        );
        let column = Rect {
            width: 1,
            ..gutter_area
        };
        paint_severity_column(
            rows,
            column,
            severity.as_ref(),
            gutter_background(theme, fallback_style),
            dim,
            theme,
            buf,
            scene.as_deref_mut(),
        );
        1
    };

    // The wrap width stamped above subtracted `measured_gutter_w` from the pane;
    // the painted gutter must reserve exactly that so the text rect and the wrap
    // width agree.
    debug_assert_eq!(
        gutter_w, measured_gutter_w,
        "painted gutter width matches the measured width the wrap used",
    );

    // Inset the text rect by the column and the gutter, and record the width so
    // click-to-offset subtracts the same shift. Written after the
    // `row_severity` borrow ends.
    let text_inset = blame_w + gutter_w;
    let inner = Rect {
        x: inner.x + text_inset,
        y: inner.y,
        width: inner.width.saturating_sub(text_inset),
        height: inner.height,
    };
    editor.gutter_width = text_inset;

    let inner = reserve_minimap_strip(editor, inner, minimap_enabled);
    editor.text_rect = Some(inner);

    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;

    paint_chunk_rows(
        snapshot.highlighted_chunks_cached(
            editor.scroll_row..end_row,
            &mut editor.highlight_endpoint_cache,
        ),
        inner.x,
        right,
        inner.y,
        bottom,
        fallback_style,
        fallback_style.patch(theme.get(crate::theme::scope::UI_VIRTUAL_INLAY)),
        fallback_style.patch(wrap_mark_style(theme)),
        buf,
    );

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
                cache.resolve_overlapping(&visible, buffer_snapshot),
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

    paint_reload_flash(
        reload_flashes,
        editor.buffer_id,
        &snapshot,
        inner,
        editor.scroll_row,
        end_row,
        &visible,
        theme,
        fallback_style,
        buf,
    );

    if !is_focused {
        return;
    }

    // With a scene and `delegates_cursor` the terminal draws the primary cursor,
    // so the pass records its cell and leaves it unpainted. Otherwise nothing
    // downstream draws it, as in an input view, a dock, or a pane under an open
    // prompt, so the pass paints it.
    paint_selections(
        editor,
        &snapshot,
        inner,
        end_row,
        &visible,
        theme,
        buf,
        scene.is_some() && delegates_cursor,
    );

    if let (Some(line), Some(fill)) = (fill_line, chrome.cursor_line) {
        let rows = line_display_rows(&snapshot, line);
        let shown = rows.start.max(editor.scroll_row)..rows.end.min(end_row);
        let top = |row: u32| content_area.y + (row - editor.scroll_row) as u16;
        let body = fallback_style
            .bg
            .or_else(|| {
                theme
                    .try_get(crate::theme::scope::UI_BACKGROUND)
                    .and_then(|style| style.bg)
            })
            .unwrap_or(Color::Reset);
        fill_cursor_line(
            buf,
            content_area.x..right,
            top(shown.start)..top(shown.end.max(shown.start)),
            body,
            fill,
        );
    }

    if let Some((path, set)) = diagnostic_info {
        let rope = buffer_snapshot.rope();
        build_diagnostic_span_cache(editor, set, path, buffer_snapshot);
        let sel = editor.selections.newest_anchor();
        let tail_off = buffer_snapshot.resolve_anchor(&sel.tail());
        let head_off = buffer_snapshot.resolve_anchor(&sel.head());
        let cursor = cursor_offset(rope, tail_off, head_off);
        let (cursor_diag, hover_diag) = match editor.diagnostic_span_cache.as_ref() {
            Some(cache) => {
                let hover = hover_cell.and_then(|(hx, hy)| {
                    let col = hx.checked_sub(content_area.x)?;
                    let row = hy.checked_sub(content_area.y)?;
                    if col >= content_area.width || row >= content_area.height {
                        return None;
                    }
                    let offset =
                        display_cell_to_offset(&snapshot, editor.scroll_row, text_inset, col, row)?;
                    diagnostic_at_offset(cache, offset, buffer_snapshot)
                });
                (diagnostic_at_offset(cache, cursor, buffer_snapshot), hover)
            },
            None => (None, None),
        };

        // The mouse hover wins over the cursor when both land in a span. The
        // popover needs a scene plus the severity and background colors resolved
        // to RGB, and its presence suppresses the same diagnostic's redundant
        // EOL message. A blamed pane's Blame Card claims the row under the
        // cursor's line, so there the cursor's diagnostic opens no popover.
        let mut suppress = None;
        if let Some(found) = hover_diag.or(cursor_diag.filter(|_| blame.is_none())) {
            let bg = style_rgb(fallback_style.bg.or_else(|| {
                theme
                    .try_get(crate::theme::scope::UI_BACKGROUND)
                    .and_then(|style| style.bg)
            }));
            if let (Some(scene), Some(colors), Some(bg)) = (scene, severity.as_ref(), bg) {
                let diag = &set.get(path)[found.index];
                let sev = diag.severity.unwrap_or(DiagnosticSeverity::ERROR);
                // The span the query resolved, rather than the offset re-derived
                // from this diagnostic's raw character column under its server's
                // encoding.
                let display = snapshot.buffer_to_display(rope.offset_to_point(found.start));
                let rel_col = display.column.min(u32::from(content_area.width)) as u16;
                let rel_row = display
                    .row
                    .saturating_sub(editor.scroll_row)
                    .min(u32::from(content_area.height)) as u16;
                let anchor_col = content_area
                    .x
                    .saturating_add(text_inset)
                    .saturating_add(rel_col);
                let anchor_row = content_area.y.saturating_add(rel_row);
                if render_diagnostic_popover(
                    scene,
                    buf,
                    diag,
                    &popover_colors(theme, severity_color(sev, colors), bg),
                    anchor_col,
                    anchor_row,
                    content_area,
                    editor.cursor_screen_cell,
                ) {
                    suppress = Some(found.index);
                }
            }
        }

        if let (Some(cache), Some(gutter_cache)) = (
            editor.diagnostic_span_cache.as_mut(),
            editor.gutter_severity_cache.as_ref(),
        ) {
            let line = rope.offset_to_point(cursor).row;
            let eol = EolCursor {
                line,
                readout: cache.cursor_line_diagnostic(line, buffer_snapshot),
                suppress,
            };
            editor.eol_cursor = Some(eol);
            paint_line_diagnostics(
                &gutter_cache.spans,
                &cache.messages,
                eol,
                theme,
                &snapshot,
                editor.scroll_row,
                end_row,
                inner,
                right,
                buf,
            );
        }
    }

    if let Some(labels) = goto_word_labels {
        paint_goto_word_labels(
            labels,
            &snapshot,
            inner,
            editor.scroll_row,
            end_row,
            fallback_style,
            buf,
        );
    }
}

/// Fills each cell of `rows` across `columns` that shows the editor surface
/// `body` with `fill`.
///
/// A cell that carries a background of its own keeps it, so a selection wash,
/// a search match, an inlay hint, and the Blame Column stay as they paint.
fn fill_cursor_line(
    buf: &mut Buffer,
    columns: Range<u16>,
    rows: Range<u16>,
    body: Color,
    fill: Color,
) {
    for y in rows {
        for x in columns.clone() {
            let cell = &mut buf[(x, y)];
            if cell.bg == body {
                cell.set_bg(fill);
            }
        }
    }
}

/// Settle the display settings a diff-view editor takes from the width of
/// `inner`, so the rows a caller measures before the paint are the rows it
/// paints.
///
/// Both columns wrap at the narrower one, so a mirrored segment always fits
/// the side that shows it. A width that fits two columns pairs each modified
/// line with the base line it replaced, on one row.
///
/// `digits` and `rich`, the rich gutter's number scale or `None` for a cell
/// gutter, size the gutters, as the paint that follows sizes them.
pub(crate) fn fit_diff_view(
    editor: &mut EditorState,
    inner: Rect,
    wrap: WrapMode,
    wrap_column: u32,
    digits: u16,
    rich: Option<u16>,
) {
    let cols = DiffColumns::compute(inner, DiffLayout::diff_view(digits, rich));
    let text_width = cols.left_content_w.min(cols.right_content_w) as u32;
    editor.display_map.set_wrap_width(wrap_width_for(
        editor.wrap_override.unwrap_or(wrap),
        text_width,
        wrap_column,
    ));
    editor
        .display_map
        .set_pair_modified_hunks(inner.width >= DIFF_TWO_COLUMN_MIN);
}

/// Reserve the right-edge minimap strip out of `inner`, and return the rect
/// the text keeps.
///
/// The strip's screen rect lands on [`EditorState::minimap_rect`] for pointer
/// mapping.
fn reserve_minimap_strip(editor: &mut EditorState, inner: Rect, minimap_enabled: bool) -> Rect {
    let (inner, strip) = split_minimap_strip(inner, minimap_enabled);
    editor.minimap_rect = strip;
    inner
}

/// Split `inner` into the rect the pane's content keeps and the right-edge
/// minimap strip, [`MINIMAP_STRIP_COLS`] wide.
///
/// The reserved cells stay blank, so the terminal's GPU minimap pass owns
/// them. A pane narrower than [`MINIMAP_MIN_PANE_COLS`], or one with the
/// minimap off, keeps all of `inner` and reserves no strip.
pub(crate) fn split_minimap_strip(inner: Rect, minimap_enabled: bool) -> (Rect, Option<Rect>) {
    if !minimap_enabled || inner.width < MINIMAP_MIN_PANE_COLS {
        return (inner, None);
    }
    let strip = Rect {
        x: inner.x + inner.width - MINIMAP_STRIP_COLS,
        y: inner.y,
        width: MINIMAP_STRIP_COLS,
        height: inner.height,
    };
    let content = Rect {
        width: inner.width - MINIMAP_STRIP_COLS,
        ..inner
    };
    (content, Some(strip))
}

/// Paint every selection of `editor` that reaches the display rows
/// `scroll_row..end_row` into `inner`.
///
/// Each selection's range washes in `ui.selection.editor`, and each cursor draws
/// as a block over the whole character it covers. A foreground too close to the
/// wash to read lifts to the floor per [`floor_style`]. `visible` is the byte
/// range of those rows, as [`visible_byte_range`] answers it.
///
/// With `delegates_cursor` the terminal draws the primary cursor, so its cell
/// goes into [`EditorState::cursor_screen_cell`] and the pass paints no block
/// there. Without it, or with the primary cursor off the rows,
/// `cursor_screen_cell` ends `None`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_selections(
    editor: &mut EditorState,
    snapshot: &DisplaySnapshot,
    inner: Rect,
    end_row: u32,
    visible: &Range<usize>,
    theme: &Theme,
    buf: &mut Buffer,
    delegates_cursor: bool,
) {
    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;
    let buffer_snapshot = snapshot.buffer_snapshot();

    let selection_style = theme.get(crate::theme::scope::UI_SELECTION_EDITOR);
    let wash = style_rgb(selection_style.bg);
    let cursor_style = theme.cursor_style();
    let primary_id = editor.selections.newest_anchor().id;
    let mut primary_cell: Option<(u16, u16)> = None;
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
            .as_chunks::<4>()
            .0
            .iter()
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
        .zip(endpoints.as_chunks::<4>().0.iter())
        .zip(cursors.iter().zip(cursor_points.iter()))
    {
        let &[start_offset, end_offset, _, _] = ends;

        let lo = start_offset.max(visible.start);
        let hi = end_offset.min(visible.end);
        if lo < hi {
            paint_offset_range(
                rope,
                snapshot,
                lo..hi,
                Some(cursor),
                &mut |_, _, cell| {
                    cell.set_style(selection_style);
                    if let Some(bg) = wash {
                        cell.set_style(floor_style(cell.style(), bg));
                    }
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
        if !cursor_reaches_viewport(cursor, visible, rope.len()) {
            continue;
        }

        let display = snapshot.buffer_to_display(cursor_point);
        if let Some((x, y)) = screen_cell(display, editor.scroll_row, end_row, inner) {
            if delegates_cursor && selection.id == primary_id {
                primary_cell = Some((x, y));
            } else {
                cursor_cells.push((cursor, x, y));
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
}

/// Wash the text each of `buffer`'s reload flashes changed, on the display rows
/// `scroll_row..end_row` of `inner`.
///
/// Each cell's background blends toward the theme's `diff.modified` color by
/// the flash's [`ReloadFlash::strength`], so the wash fades as the flash ages
/// and leaves the cell as it was once the fade ends. A cell with no background
/// of its own blends against `fallback_style`'s. A theme whose modified color is
/// not RGB, or a cell with no RGB background to blend against, takes no wash.
/// `visible` is the byte range of the rows, as [`visible_byte_range`] answers
/// it.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_reload_flash(
    flashes: &[ReloadFlash],
    buffer: BufferId,
    snapshot: &DisplaySnapshot,
    inner: Rect,
    scroll_row: u32,
    end_row: u32,
    visible: &Range<usize>,
    theme: &Theme,
    fallback_style: Style,
    buf: &mut Buffer,
) {
    let Some(flash_rgb) = style_rgb(theme.get(crate::theme::scope::DIFF_MODIFIED).fg) else {
        return;
    };
    let editor_bg = style_rgb(fallback_style.bg);
    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;
    let buffer_snapshot = snapshot.buffer_snapshot();
    let rope = buffer_snapshot.rope();

    for flash in flashes.iter().filter(|flash| flash.buffer == buffer) {
        let keep = 1.0 - flash.strength();
        let ends = {
            let anchors: Vec<Anchor> = flash
                .spans
                .iter()
                .flat_map(|&(start, end)| [start, end])
                .collect();
            buffer_snapshot.resolve_anchors_batch(&anchors)
        };
        for &[start, end] in ends.as_chunks::<2>().0 {
            let lo = start.max(visible.start);
            let hi = end.min(visible.end);
            if lo >= hi {
                continue;
            }
            paint_offset_range(
                rope,
                snapshot,
                lo..hi,
                None,
                &mut |_, _, cell| {
                    if let Some(bg) = style_rgb(cell.style().bg).or(editor_bg) {
                        let [r, g, b] = dim_rgb(flash_rgb, bg, keep);
                        cell.set_bg(Color::Rgb(r, g, b));
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
        }
    }
}

/// The screen cell of display point `display` in an editor painted from display
/// row `scroll_row` into `text`, or `None` off the rows `scroll_row..end_row` or
/// off `text`.
///
/// [`paint_selections`] places its cursors by this, and a changes view that
/// replays its last paint places the recorded cursor by it, so the two land on
/// one cell.
pub(crate) fn screen_cell(
    display: DisplayPoint,
    scroll_row: u32,
    end_row: u32,
    text: Rect,
) -> Option<(u16, u16)> {
    if display.row < scroll_row || display.row >= end_row {
        return None;
    }
    let y = text.y + (display.row - scroll_row) as u16;
    let x = text.x + display.column as u16;
    (x < text.x + text.width && y < text.y + text.height).then_some((x, y))
}

/// Draw each goto-word label over the start of the word it names, for the
/// words on the display rows `scroll_row..end_row` of `inner`.
///
/// A label paints in `fallback_style`, reversed and bold, and stops at the right
/// edge of `inner`.
pub(crate) fn paint_goto_word_labels(
    labels: &BTreeMap<String, (usize, usize)>,
    snapshot: &DisplaySnapshot,
    inner: Rect,
    scroll_row: u32,
    end_row: u32,
    fallback_style: Style,
    buf: &mut Buffer,
) {
    let right = inner.x + inner.width;
    let bottom = inner.y + inner.height;
    let buffer_snapshot = snapshot.buffer_snapshot();

    let label_style = fallback_style.add_modifier(Modifier::REVERSED | Modifier::BOLD);
    // Drawn at the word's start, which is where the user reads the label
    // against the word it names.
    for (label, &(start, _)) in labels {
        let rope = buffer_snapshot.rope();
        if start > rope.len() {
            continue;
        }
        let point = rope.offset_to_point(start);
        let display = snapshot.buffer_to_display(point);
        if display.row < scroll_row || display.row >= end_row {
            continue;
        }
        let y = inner.y + (display.row - scroll_row) as u16;
        for (i, ch) in label.chars().enumerate() {
            let x = inner.x + display.column as u16 + i as u16;
            if x >= right || y >= bottom {
                break;
            }
            buf[(x, y)].set_char(ch).set_style(label_style);
        }
    }
}

/// The wrap width `mode` gives a text area `text_width` columns wide.
///
/// [`WrapMode::EditorWidth`] leaves the last column of the area free. A row
/// that the wrap fills puts its newline cell one column past its text, and the
/// end-of-line caret shows on that cell. [`WrapMode::Bounded`] does the same,
/// and caps the width at `wrap_column`. [`WrapMode::None`] still answers a
/// width, [`MAX_UNWRAPPED_COLUMNS`], which keeps a huge line's rows bounded.
fn wrap_width_for(mode: WrapMode, text_width: u32, wrap_column: u32) -> Option<u32> {
    let width = text_width.saturating_sub(1).max(1);
    match mode {
        WrapMode::None => Some(MAX_UNWRAPPED_COLUMNS),
        WrapMode::EditorWidth => Some(width),
        WrapMode::Bounded => Some(width.min(wrap_column)),
    }
}

/// Paint `chunks` into `buf` as rows of cells, from `left`, `top` up to but
/// not including `right` and `bottom`.
///
/// The live editor and a pooled smooth-scroll page both paint their text
/// through this, so a glide and the grid it settles onto agree cell for cell.
/// They differ only in what they hand over, which is why the chunk iterator,
/// the inlay and wrap styles, and the geometry all arrive as arguments.
///
/// `left` is both the first column and the column a newline returns to, so a
/// caller reserving a gutter passes the column just past it.
// Nine arguments, because the geometry is four independent columns and rows
// rather than a Rect. `left` is the newline reset column, which a region's own
// left edge is not once a gutter is reserved.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_chunk_rows(
    chunks: BlockChunks<'_>,
    left: u16,
    right: u16,
    top: u16,
    bottom: u16,
    fallback_style: Style,
    inlay_style: Style,
    wrap_style: Style,
    buf: &mut Buffer,
) {
    let mut x = left;
    let mut y = top;
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
    'chunks: for chunk in chunks {
        let style = if chunk.is_wrap_mark {
            wrap_style
        } else if chunk.is_inlay {
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
            // once it reaches the right edge nothing on the rest of the line
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
                x = left;
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

/// The style a continuation row's Wrap Mark paints with.
///
/// A theme that gives `ui.virtual.wrap` no foreground paints the mark in
/// `ui.text.disabled`. Every theme draws the mark, because the mark is how a
/// reader tells a continuation row from a new line.
pub fn wrap_mark_style(theme: &Theme) -> Style {
    let style = theme.get(crate::theme::scope::UI_VIRTUAL_WRAP);
    if style.fg.is_some() {
        return style;
    }
    theme.get(crate::theme::scope::UI_TEXT_DISABLED)
}

/// Paints the Wrap Mark of a continuation row whose text starts `indent` cells
/// past `text_x`, for a painter that lays out its own rows rather than reading
/// the chunk stream.
///
/// The mark takes the indent's second-to-last cell, as the chunk stream places
/// it. An indent under two cells takes no mark, and a mark cell at or past
/// `right` is not painted.
pub(crate) fn paint_wrap_mark(
    buf: &mut Buffer,
    text_x: u16,
    y: u16,
    indent: u16,
    right: u16,
    style: Style,
) {
    if indent < 2 {
        return;
    }
    let x = text_x + indent - 2;
    if x >= right {
        return;
    }
    buf[(x, y)].set_symbol(WRAP_MARK).set_style(style);
}

/// Cached gutter severity map for one diagnostic set against one buffer.
///
/// `map` is the worst severity per buffer row, over the whole buffer, because
/// the page gutter and the minimap read rows outside the viewport. The paint
/// rebuilds it when the version or the buffer moves, not every frame.
///
/// The buffer version belongs in the key because the rows come from spans
/// shifted through the edits since each diagnostic was published, so typing
/// moves them without the server saying anything.
pub struct GutterSeverityCache {
    pub(crate) version: u64,
    buffer_version: u64,
    pub map: Arc<RowSeverity>,
    /// Every diagnostic of the buffer, resolved against the cache's buffer
    /// version, in start order.
    ///
    /// A pooled page renders off the run loop, where the anchored
    /// [`DiagnosticSpanCache`](crate::diagnostic_spans::DiagnosticSpanCache) does
    /// not reach, so the page paints its diagnostic spans from this list.
    pub(crate) spans: Arc<[ResolvedDiag]>,
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
pub struct GutterGeometryCache {
    pub geometry_key: u64,
    pub folded: Vec<(u32, u16)>,
    width_digits: u16,
    marks: BTreeMap<u32, GutterMark>,
    lines_key: Option<u64>,
    pub lines: Vec<GutterLine>,
    scene_key: Option<u64>,
    scene_bytes: Vec<u8>,
}

/// The severity each buffer row carries, held as the runs it comes in rather
/// than a row at a time.
///
/// A diagnostic covers a contiguous run, and one over a `#[cfg]`-excluded block
/// or a test module covers thousands of rows. The map this replaced held an
/// entry for each, and it is rebuilt on every buffer version because the
/// anchors move under typing, so a keystroke paid for every covered row.
#[derive(Default, Clone, PartialEq, Eq, Debug)]
pub struct RowSeverity {
    /// Sorted by start row and disjoint, so a lookup is one partition point.
    runs: Vec<(Range<u32>, DiagnosticSeverity)>,
}

impl RowSeverity {
    pub(crate) fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// The severity at `row`, or `None` where no diagnostic reaches it.
    pub(crate) fn at(&self, row: u32) -> Option<DiagnosticSeverity> {
        let index = self.runs.partition_point(|(rows, _)| rows.end <= row);
        let (rows, severity) = self.runs.get(index)?;
        rows.contains(&row).then_some(*severity)
    }

    /// Build from single rows, for a caller naming the rows directly rather
    /// than the spans they came from.
    #[cfg(any(test, feature = "test-support"))]
    pub fn for_rows(rows: impl IntoIterator<Item = (u32, DiagnosticSeverity)>) -> Self {
        let mut runs: Vec<(Range<u32>, DiagnosticSeverity)> = rows
            .into_iter()
            .map(|(row, severity)| (row..row + 1, severity))
            .collect();
        runs.sort_unstable_by_key(|(rows, _)| rows.start);
        Self { runs }
    }

    /// Every row in `rows` a diagnostic reaches, in order.
    ///
    /// The minimap marks each one, so this spells a run back out into the rows
    /// it stands for. Clipped to the query, so a diagnostic covering a whole
    /// file costs only the rows asked about.
    pub fn rows_in(&self, rows: Range<u32>) -> Vec<u32> {
        let first = self.runs.partition_point(|(run, _)| run.end <= rows.start);
        self.runs[first..]
            .iter()
            .take_while(|(run, _)| run.start < rows.end)
            .flat_map(|(run, _)| run.start.max(rows.start)..run.end.min(rows.end))
            .collect()
    }
}

/// Build the row severities from resolved diagnostic spans, the worst severity
/// (lowest LSP code) winning where several overlap a row.
///
/// The rows come from `spans` rather than from the diagnostics themselves so
/// they name where the text is now. A diagnostic's own rows are where the
/// server last saw it, and the reader has been typing since.
///
/// Swept over the spans' endpoints, so the cost follows how many diagnostics
/// there are rather than how many rows they cover.
fn row_severity_from_spans(spans: &[ResolvedDiag]) -> RowSeverity {
    // A span opens at its first row and closes past its last, so a row's
    // severities are the ones opened and not yet closed when the sweep reaches
    // it.
    let mut events: Vec<(u32, i32, DiagnosticSeverity)> = Vec::with_capacity(spans.len() * 2);
    for span in spans {
        events.push((span.start_line, 1, span.severity));
        events.push((span.end_line + 1, -1, span.severity));
    }
    events.sort_unstable_by_key(|&(row, _, _)| row);

    // Counted per rank rather than kept as a set, there being four of them.
    let mut open = [0i32; 4];
    let mut runs: Vec<(Range<u32>, DiagnosticSeverity)> = Vec::new();
    let mut index = 0;
    while index < events.len() {
        let row = events[index].0;
        while let Some(&(_, delta, severity)) = events.get(index).filter(|e| e.0 == row) {
            open[severity_rank(severity) as usize] += delta;
            index += 1;
        }

        // Nothing is open past the last event, every span having closed, so a
        // run always has the next boundary to end at.
        let Some(&(next_row, _, _)) = events.get(index) else {
            break;
        };
        let Some(rank) = open.iter().position(|&count| count > 0) else {
            continue;
        };
        let severity = SEVERITY_BY_RANK[rank];
        match runs.last_mut() {
            Some((rows, held)) if rows.end == row && *held == severity => rows.end = next_row,
            _ => runs.push((row..next_row, severity)),
        }
    }
    RowSeverity { runs }
}

/// The severities [`severity_rank`] numbers, which is how a sweep counting by
/// rank names the one it settled on.
const SEVERITY_BY_RANK: [DiagnosticSeverity; 4] = [
    DiagnosticSeverity::ERROR,
    DiagnosticSeverity::WARNING,
    DiagnosticSeverity::INFORMATION,
    DiagnosticSeverity::HINT,
];

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

/// Blend a syntax foreground 9:11 toward the pane background, keeping the hue but
/// reading muted. Used to dim Unnecessary-tagged (inactive-code) regions without
/// discarding their per-token syntax colors.
///
/// The 45 percent that the foreground keeps matches the opacity the design
/// gives unused code, and the dotted line under it blends as far.
pub fn mute_rgb(fg: [u8; 3], bg: [u8; 3]) -> [u8; 3] {
    let mix = |f: u8, b: u8| ((f as u16 * 9 + b as u16 * 11) / 20) as u8;
    [mix(fg[0], bg[0]), mix(fg[1], bg[1]), mix(fg[2], bg[2])]
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
pub struct SeverityColors {
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
pub fn severity_colors(theme: &Theme) -> Option<SeverityColors> {
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

fn severity_underline(sev: DiagnosticSeverity) -> UnderlineKind {
    match sev {
        DiagnosticSeverity::HINT => UnderlineKind::Dotted,
        _ => UnderlineKind::Curly,
    }
}

/// The diff colors the gutter's hunk and state lanes use.
///
/// The four status colors resolve the way the minimap edge lane resolves them.
/// Each is `theme.get(diff.*)` under [`crate::theme::Theme::get`]'s progressive
/// scope-broadening fallback, so a theme omitting `diff.modified` or
/// `diff.moved` still yields a color that agrees with the minimap lane. The four
/// state colors resolve through [`mark_state_scope`], as the diff view's state
/// bar does.
#[derive(Clone, Copy, Hash)]
pub struct DiffMarkColors {
    pub added: [u8; 3],
    modified: [u8; 3],
    moved: [u8; 3],
    pub deleted: [u8; 3],
    staged: [u8; 3],
    unstaged: [u8; 3],
    approved: [u8; 3],
    unapproved: [u8; 3],
}

impl DiffMarkColors {
    pub fn resolve(theme: &Theme) -> Self {
        use crate::theme::scope as s;
        let get = |scope| color_to_rgb(theme.get(scope).fg.unwrap_or(Color::White));
        let state = |state| get(mark_state_scope(theme, state));
        Self {
            added: get(s::DIFF_ADDED),
            modified: get(s::DIFF_MODIFIED),
            moved: get(s::DIFF_MOVED),
            deleted: get(s::DIFF_DELETED),
            staged: state(MarkState::Staged),
            unstaged: state(MarkState::Unstaged),
            approved: state(MarkState::Approved),
            unapproved: state(MarkState::Unapproved),
        }
    }

    fn for_state(&self, state: MarkState) -> [u8; 3] {
        match state {
            MarkState::Staged => self.staged,
            MarkState::Unstaged => self.unstaged,
            MarkState::Approved => self.approved,
            MarkState::Unapproved => self.unapproved,
        }
    }

    pub fn for_status(&self, status: DiffHunkStatus) -> [u8; 3] {
        match status {
            DiffHunkStatus::Added => self.added,
            DiffHunkStatus::Modified => self.modified,
            DiffHunkStatus::Moved => self.moved,
            DiffHunkStatus::Deleted => self.deleted,
        }
    }

    /// Blend every diff-mark color toward `bg` by `amount` (`0.0` is identity),
    /// dimming the gutter's diff marks with an unfocused pane.
    pub fn dim(&self, bg: [u8; 3], amount: f32) -> DiffMarkColors {
        DiffMarkColors {
            added: dim_rgb(self.added, bg, amount),
            modified: dim_rgb(self.modified, bg, amount),
            moved: dim_rgb(self.moved, bg, amount),
            deleted: dim_rgb(self.deleted, bg, amount),
            staged: dim_rgb(self.staged, bg, amount),
            unstaged: dim_rgb(self.unstaged, bg, amount),
            approved: dim_rgb(self.approved, bg, amount),
            unapproved: dim_rgb(self.unapproved, bg, amount),
        }
    }
}

/// The theme scope a row's state bar paints in for `state`.
///
/// A theme that styles neither `diff.approved` nor a scope above it paints an
/// approved row in `diff.staged`. An unapproved row takes `diff.unstaged` by
/// the same rule. A theme that sets only the staged scopes thus keeps two
/// distinct state colors under a review, and the diff view keeps its rich
/// gutter.
pub fn mark_state_scope(theme: &Theme, state: MarkState) -> &'static str {
    use crate::theme::scope as s;
    let (scope, fallback) = match state {
        MarkState::Staged => return s::DIFF_STAGED,
        MarkState::Unstaged => return s::DIFF_UNSTAGED,
        MarkState::Approved => (s::DIFF_APPROVED, s::DIFF_STAGED),
        MarkState::Unapproved => (s::DIFF_UNAPPROVED, s::DIFF_UNSTAGED),
    };
    match theme.try_get(scope) {
        Some(_) => scope,
        None => fallback,
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
    /// The scale the line numbers draw at, the theme's [`TypeRole::Small`]
    /// size. The gutter's width follows it, so the layout reads it too.
    pub(crate) number_scale: u16,
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
            number_scale: self.number_scale,
        }
    }
}

/// Resolve the rich page-gutter colors, or `None` when a gutter color is not
/// RGB.
///
/// Mirrors the live gutter's rich gate so an off-run-loop page render and the
/// live render agree on rich versus fallback for the same theme.
pub(crate) fn resolve_rich_gutter(
    theme: &Theme,
    fallback_style: Style,
) -> Option<RichGutterColors> {
    use crate::theme::scope as s;
    let colors = severity_colors(theme)?;
    let diff = DiffMarkColors::resolve(theme);
    let number_fg = style_rgb(theme.get(s::UI_TEXT_MUTED).fg)?;
    // The rule is a divider, so it takes the theme's divider color. A theme
    // with no divider keeps the inactive border.
    let separator = theme
        .try_get(s::UI_DIVIDER)
        .and_then(|style| style.fg)
        .or(theme.get(s::UI_BORDER_INACTIVE).fg);
    let separator = style_rgb(separator).unwrap_or(number_fg);
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
        number_scale: theme.type_scale(TypeRole::Small),
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
pub struct ResolvedChrome {
    /// `None` when a gutter color is not RGB, which drops the pane to the
    /// fallback glyph gutter rather than mixing the two.
    pub(crate) rich_gutter: Option<RichGutterColors>,
    /// `None` when a diagnostic color is missing or not RGB.
    pub(crate) severity: Option<SeverityColors>,
    pub diff_marks: DiffMarkColors,
    /// Background the rich gutter fills, and what its foregrounds dim toward.
    pub gutter_bg: Option<[u8; 3]>,
    /// The Blame Column's colors. `None` when the theme holds no RGB surface,
    /// and then a blamed pane shows no column.
    pub(crate) blame: Option<BlameColors>,
    /// The fill behind the focused plain editor's cursor line, or `None` when
    /// the theme gives `ui.cursorline` no background and the line takes none.
    pub(crate) cursor_line: Option<Color>,
}

impl ResolvedChrome {
    pub fn resolve(theme: &Theme) -> Self {
        let fallback_style = theme.get(crate::theme::scope::UI_TEXT);
        Self {
            rich_gutter: resolve_rich_gutter(theme, fallback_style),
            severity: severity_colors(theme),
            diff_marks: DiffMarkColors::resolve(theme),
            gutter_bg: gutter_background(theme, fallback_style),
            blame: BlameColors::resolve(theme),
            cursor_line: theme
                .try_get(crate::theme::scope::UI_CURSORLINE)
                .and_then(|style| style.bg),
        }
    }
}

/// The background the gutter fills, preferring the pane's own over the theme's.
pub(crate) fn gutter_background(theme: &Theme, fallback_style: Style) -> Option<[u8; 3]> {
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
pub struct DiagnosticRowsCache {
    key: u64,
    /// `(offset from the top of the viewport, severity)` for the rows that carry
    /// one. Rows without a diagnostic are absent, both painters skipping them.
    pub rows: Vec<(u16, DiagnosticSeverity)>,
}

/// The visible rows carrying a diagnostic, rebuilding `cache` only when the
/// viewport's row mapping could have moved.
///
/// Keyed by [`gutter_geometry_key`], which already asks that question for the
/// line-numbers-on gutter. It folds in the width, since that sets the wrap, and
/// the diff version, which this does not read but which only ever costs a spare
/// rebuild of a viewport-sized list.
pub fn diagnostic_rows<'c>(
    snapshot: &DisplaySnapshot,
    row_severity: &RowSeverity,
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
        snapshot.paint_version(),
        snapshot.diff_map().map_or(0, |dm| dm.version()),
        severity_version,
    );
    if cache.as_ref().is_none_or(|c| c.key != key) {
        let rows = severity_rows(snapshot, row_severity, scroll_row..end_row, inner.height);
        *cache = Some(DiagnosticRowsCache { key, rows });
    }
    &cache.as_ref().expect("filled just above").rows
}

/// The display rows of `rows` that carry a diagnostic, each as its offset from
/// the first of `rows` and its severity, for the gutter with line numbers off.
///
/// Stops after `height` rows, the height of the column the marks go in.
pub(crate) fn severity_rows(
    snapshot: &DisplaySnapshot,
    row_severity: &RowSeverity,
    rows: Range<u32>,
    height: u16,
) -> Vec<(u16, DiagnosticSeverity)> {
    let top = rows.start;
    rows.map_while(|display_row| {
        let offset = (display_row - top) as u16;
        (offset < height).then_some((display_row, offset))
    })
    .filter_map(|(display_row, offset)| {
        let row = buffer_row_of(snapshot, display_row)?;
        Some((offset, row_severity.at(row)?))
    })
    .collect()
}

/// Paint the one-column severity gutter of an editor with line numbers off,
/// a mark on each of `rows` in `column`.
///
/// With a scene and RGB severity colors, each row takes a sub-cell bar in its
/// severity color, faded toward `ground` by `dim`. Otherwise each row takes
/// its severity letter in the cell, styled from `theme`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_severity_column(
    rows: &[(u16, DiagnosticSeverity)],
    column: Rect,
    colors: Option<&SeverityColors>,
    ground: Option<[u8; 3]>,
    dim: f32,
    theme: &Theme,
    buf: &mut Buffer,
    scene: Option<&mut ApcScene>,
) {
    let Some((scene, colors)) = scene.zip(colors) else {
        paint_diagnostic_gutter(rows, column.x, column.y, theme, buf);
        return;
    };

    for &(row_offset, sev) in rows {
        let color = match ground {
            Some(bg) if dim > 0.0 => dim_rgb(severity_color(sev, colors), bg, dim),
            _ => severity_color(sev, colors),
        };
        Bar {
            x: 0,
            y: (row_offset * 16) as i16,
            width: 6,
            height: 16,
            color,
            radius: 0,
            corners: 0,
        }
        .render(column, buf, &mut *scene);
    }
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

pub fn paint_diagnostic_gutter(
    rows: &[(u16, DiagnosticSeverity)],
    x: u16,
    y: u16,
    theme: &Theme,
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

/// The icon the rich gutter draws for a severity, or `None` for a hint, which
/// the gutter marks with a dot instead.
fn gutter_icon(sev: DiagnosticSeverity) -> Option<IconKind> {
    match sev {
        DiagnosticSeverity::HINT => None,
        _ => Some(icon_kind(sev)),
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
/// A trailing newline leaves a final empty line, and the cursor reaches it, so
/// it numbers like any other. A bare `"\n"` scratch shows two numbered rows.
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

    (folded, gutter_width_digits(snapshot))
}

/// [`gutter_digits`] for a snapshot the caller already holds.
pub fn gutter_width_digits(snapshot: &DisplaySnapshot) -> u16 {
    gutter_digits(snapshot.buffer_line_count())
}

/// The digit width the gutter reserves for a buffer's line numbers, at least
/// two.
///
/// `line_count` is a buffer fact, never wrap or block rows, which is what lets
/// the paint size the gutter and resolve the wrap width before the display
/// snapshot it is otherwise read from exists.
pub(crate) fn gutter_digits(line_count: u32) -> u16 {
    decimal_digits(line_count).max(2)
}

/// The cell columns the line-number gutter reserves, measured without painting.
///
/// `rich` is the number scale of a sub-cell [`Gutter::cell_width`] layout, or
/// `None` for the degraded gutter, which reserves a mark column, the digits,
/// one git glyph, and a gap. The result matches what
/// [`draw_line_number_gutter`] paints for the same digit count, so the paint
/// resolves the wrap width before the wrapped snapshot exists.
pub fn measure_gutter_width(width_digits: u16, rich: Option<u16>) -> u16 {
    match rich {
        Some(number_scale) => {
            rich_gutter(&[], width_digits, ([0; 3], [0; 3], [0; 3]), number_scale).cell_width()
        },
        None => width_digits + 3,
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

/// Map each folded row a diff hunk marks to its [`GutterMark`], for the
/// gutter's git bar. Rows outside any hunk are absent from the result.
pub(crate) fn gutter_diff_marks(
    snapshot: &DisplaySnapshot,
    folded: &[(u32, u16)],
) -> BTreeMap<u32, GutterMark> {
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
) -> BTreeMap<u32, GutterMark> {
    folded
        .iter()
        .filter_map(|&(number, _)| {
            let row = number - 1;
            live.gutter_mark_for_line(row).map(|mark| (row, mark))
        })
        .collect()
}

/// Build the rich gutter's [`GutterLine`]s from `folded`, coloring each line's
/// diagnostic mark from `colors`.
///
/// A marked line's hunk lane takes its change kind's color from `diff_colors`,
/// and its state lane the color of the hunk's staged state.
///
/// `current_line` selects relative numbering per [`gutter_display_number`]. The
/// diagnostic mark stays keyed to the absolute buffer line.
///
/// `cursor_fill` names the 1-based cursor line that the editor fills, with the
/// fill's color. That line's runs composite over the fill, so their boxes match
/// the row beneath them.
pub(crate) fn gutter_component_lines(
    folded: &[(u32, u16)],
    row_severity: &RowSeverity,
    diff_marks: &BTreeMap<u32, GutterMark>,
    diff_colors: &DiffMarkColors,
    colors: &SeverityColors,
    current_line: Option<u32>,
    cursor_fill: Option<(u32, [u8; 3])>,
) -> Vec<GutterLine> {
    folded
        .iter()
        .map(|&(number, height)| GutterLine {
            number: gutter_display_number(number, current_line),
            height,
            git: diff_marks.get(&(number - 1)).map(|mark| GitMark {
                slots: [
                    Some(diff_colors.for_status(mark.status)),
                    Some(diff_colors.for_state(mark.state)),
                ],
                seam: mark.status == DiffHunkStatus::Deleted,
            }),
            diagnostic: row_severity.at(number - 1).map(|sev| Diagnostic {
                color: severity_color(sev, colors),
                kind: gutter_icon(sev),
            }),
            bg: cursor_fill
                .filter(|&(line, _)| line == number)
                .map(|(_, fill)| fill),
        })
        .collect()
}

/// The width of every rich gutter lane, in sixteenths.
///
/// A cell is half as wide as it is tall, so 6 sixteenths draw the boards' 3 px
/// lane in a 9 px cell.
const GUTTER_LANE_WIDTH: u16 = 6;

/// The gap between the two rich gutter lanes, in sixteenths.
///
/// Each lane's edges round to whole pixels. In a 9 px cell the first lane ends
/// at 3.4 px and the second starts at 3.9 px, which round to 3 and 4, so the
/// lanes take pixel columns 0 to 2 and 4 to 6 with one pixel between them.
const GUTTER_LANE_GAP: u16 = 1;

/// The sub-cell [`Gutter`] widget for `lines`, carrying the geometry the live
/// and pooled-page renders share.
///
/// The hunk and state lanes sit left of the number, each [`GUTTER_LANE_WIDTH`]
/// wide and [`GUTTER_LANE_GAP`] apart. The diff view lays its sides out from
/// the same widget, with its lanes [`review::DIFF_GUTTER_LANE_GAP`] apart.
///
/// The `colors` are the number, separator, and background colors. The numbers
/// draw at `number_scale`, which also sets the gutter's width, so a measure and
/// the paint it sizes pass the same scale.
pub(crate) fn rich_gutter(
    lines: &[GutterLine],
    width_digits: u16,
    (number_fg, separator, bg): ([u8; 3], [u8; 3], [u8; 3]),
    number_scale: u16,
) -> Gutter<'_> {
    Gutter {
        lines,
        left_bars: 2,
        left_bar_width: GUTTER_LANE_WIDTH,
        pad: GUTTER_LANE_GAP,
        lead: 0,
        number_scale,
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
///
/// `paint_version` carries every display transform rather than the folds
/// alone, because the rows the geometry counts are what the whole mapping
/// produces. A background rewrap makes the distinction matter: it settles
/// moving no version but its own, so a key without it serves the row mapping
/// from before the file was wrapped.
fn gutter_geometry_key(
    scroll_row: u32,
    width: u16,
    visible: u32,
    buffer_version: u64,
    paint_version: PaintVersion,
    diff_version: usize,
    severity_version: u64,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    scroll_row.hash(&mut hasher);
    width.hash(&mut hasher);
    visible.hash(&mut hasher);
    buffer_version.hash(&mut hasher);
    paint_version.hash(&mut hasher);
    diff_version.hash(&mut hasher);
    severity_version.hash(&mut hasher);
    hasher.finish()
}

/// Hash the inputs the rich component lines are built from into a cache key.
///
/// `geometry_key` is folded in because the lines are built from the geometry it
/// guards, so a geometry rebuild has to rebuild them too. The inputs of their
/// own are the relative-numbering line, the filled cursor line, and the colors
/// they bake in, which is what makes a theme change and a cursor move both show
/// up here.
fn gutter_lines_key(
    geometry_key: u64,
    current_line: Option<u32>,
    cursor_fill: Option<(u32, [u8; 3])>,
    colors: (DiffMarkColors, &SeverityColors),
) -> u64 {
    let mut hasher = DefaultHasher::new();
    geometry_key.hash(&mut hasher);
    current_line.hash(&mut hasher);
    cursor_fill.hash(&mut hasher);
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
    number_scale: u16,
) -> u64 {
    let mut hasher = DefaultHasher::new();
    lines_key.hash(&mut hasher);
    (inner.x, inner.y, inner.width, inner.height).hash(&mut hasher);
    colors.hash(&mut hasher);
    number_scale.hash(&mut hasher);
    hasher.finish()
}

/// Draw the absolute-line-number gutter and return the cell columns it reserves.
///
/// With a scene and every gutter color resolved to RGB, draws the rich
/// sub-cell gutter (scaled numbers, severity bars, hairline separator). Without
/// one, or with a theme whose colors are not RGB, gets right-aligned cell
/// numbers and a one-column severity mark styled from the theme, so the numbers
/// still show.
///
/// `fill_line` is the 1-based cursor line whose rows the editor fills, whose
/// rich runs then composite over the fill.
#[allow(clippy::too_many_arguments)]
fn draw_line_number_gutter(
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    inner: Rect,
    end_row: u32,
    row_severity: &RowSeverity,
    theme: &Theme,
    chrome: &ResolvedChrome,
    current_line: Option<u32>,
    fill_line: Option<u32>,
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
    let gutter_rgb = dimmed.as_ref().map(|rich| {
        (
            &rich.colors,
            rich.number_fg,
            rich.separator,
            rich.bg,
            rich.number_scale,
        )
    });
    let rich = scene.zip(gutter_rgb);

    let geometry_key = gutter_geometry_key(
        scroll_row,
        inner.width,
        visible,
        snapshot.buffer_snapshot().version(),
        snapshot.paint_version(),
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
    if let Some((colors, .., bg, _)) = gutter_rgb {
        // The pane's fade dims the filled cells toward the gutter background
        // as `dimmed` dims the gutter's colors, so the runs' boxes fade alike.
        let cursor_fill = fill_line
            .zip(style_rgb(chrome.cursor_line))
            .map(|(line, fill)| (line, dim_rgb(fill, bg, dim)));
        let lines_key = gutter_lines_key(
            geometry_key,
            current_line,
            cursor_fill,
            (diff_colors, colors),
        );
        if geometry.lines_key != Some(lines_key) {
            geometry.lines = gutter_component_lines(
                &geometry.folded,
                row_severity,
                &geometry.marks,
                &diff_colors,
                colors,
                current_line,
                cursor_fill,
            );
            geometry.lines_key = Some(lines_key);
        }
    }

    match rich {
        Some((scene, (_colors, number_fg, separator, bg, number_scale))) => draw_rich_gutter(
            geometry,
            inner,
            (number_fg, separator, bg),
            number_scale,
            buf,
            scene,
        ),
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
    number_scale: u16,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) -> u16 {
    let scene_key = gutter_scene_key(geometry.lines_key, inner, colors, number_scale);

    let gutter = rich_gutter(&geometry.lines, geometry.width_digits, colors, number_scale);
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
/// number, and one git glyph cell right of it, for a terminal without the
/// sub-cell components. Returns the reserved cell columns.
///
/// The glyph takes the row's change kind scope. A marked row paints `▎`, and a
/// deletion paints `▔`, since the row it marks is the one below the removed
/// text.
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_fallback_line_numbers(
    folded: &[(u32, u16)],
    width_digits: u16,
    row_severity: &RowSeverity,
    diff_marks: &BTreeMap<u32, GutterMark>,
    current_line: Option<u32>,
    inner: Rect,
    theme: &Theme,
    buf: &mut Buffer,
) -> u16 {
    use crate::theme::scope as s;
    let mark_w = 1u16;
    let gap = 1u16;
    let glyph_x = inner.x + mark_w + width_digits;
    let width = mark_w + width_digits + 1 + gap;
    let number_style = theme.get(s::UI_TEXT_MUTED);

    // A pane as narrow as two columns is smaller than the gutter it reserves,
    // and a cell past its right edge belongs to a neighbour or lies off the
    // screen.
    let right = inner.x + inner.width;

    // A scope resolves by walking progressively broadening keys, so a gutter
    // repeating a mark down fifty rows would pay that walk fifty times. Each
    // scope resolves on the first row that wants it and is remembered for the
    // rest, which also leaves an unmarked gutter resolving none of them.
    //
    // Eight slots hold every scope a row paints. A row paints one of four
    // severities and one of four diff statuses.
    let mut resolved: [Option<(&'static str, Style)>; 8] = [None; 8];
    let mut style_for =
        |scope: &'static str| match resolved.iter().flatten().find(|(known, _)| *known == scope) {
            Some(&(_, style)) => style,
            None => {
                let style = theme.get(scope);
                let free = resolved
                    .iter_mut()
                    .find(|slot| slot.is_none())
                    .expect("a gutter paints at most eight distinct mark scopes");
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
        if let Some(sev) = row_severity.at(line - 1)
            && inner.x < right
        {
            buf[(inner.x, y)]
                .set_char(severity_mark(sev))
                .set_style(style_for(severity_scope(sev)));
        }

        number.clear();
        write!(number, "{}", gutter_display_number(line, current_line))
            .expect("writing to a String is infallible");
        let start = inner.x + mark_w + width_digits.saturating_sub(number.len() as u16);
        if start < right {
            let max_width = usize::from(right - start).min(number.len());
            buf.set_stringn(start, y, &number, max_width, number_style);
        }

        if let Some(&GutterMark { status, .. }) = diff_marks.get(&(line - 1))
            && glyph_x < right
        {
            let scope = match status {
                DiffHunkStatus::Deleted => s::DIFF_DELETED,
                DiffHunkStatus::Added => s::DIFF_ADDED,
                DiffHunkStatus::Modified => s::DIFF_MODIFIED,
                DiffHunkStatus::Moved => s::DIFF_MOVED,
            };
            let glyph = match status {
                DiffHunkStatus::Deleted => '▔',
                _ => '▎',
            };
            buf[(glyph_x, y)]
                .set_char(glyph)
                .set_style(style_for(scope));
        }
        top += height;
    }
    width
}

/// Underline every visible diagnostic's text span in its severity color.
///
/// `spans` are resolved diagnostics in start order. A span that ends before
/// `visible` or is empty paints nothing, and the rest clip to `visible`. Each
/// paints through [`paint_offset_range`], which merges the style so the
/// underlined span keeps its syntax background.
///
/// The live grid passes the spans that the cache bounds keep for the viewport.
/// A pooled page passes the spans of its own rows from the list that the gutter
/// refresh resolved, since a page renders off the run loop.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_diagnostic_spans(
    spans: Vec<ResolvedDiag>,
    scratch: &mut DiagnosticPaintScratch,
    visible: Range<usize>,
    rope: &Rope,
    snapshot: &DisplaySnapshot,
    theme: &Theme,
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
    // An Unnecessary-tagged span below error severity blends the cell's syntax
    // fg toward this background rather than overwriting it. It resolves once,
    // and the dedup set keeps overlapping muted spans from double-blending a
    // shared cell.
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

    // Only the spans that overlap the viewport paint a cell, and the caller bounds
    // which those are at both ends. Paint them least-severe first so the worst
    // severity lands last, on top, for both the cell foreground and the
    // collected undercurl spans. rust-analyzer can publish a WARNING and a HINT
    // over the same `unused` in any order, and publish order alone would let the
    // hint's grey win.
    ordered.clear();
    ordered.extend(
        spans
            .into_iter()
            .filter(|s| s.start < s.end && s.end > visible.start),
    );
    ordered.sort_by_key(|s| Reverse(severity_rank(s.severity)));

    // Collect the painted runs only when the undercurl overlay is live. The
    // overlay re-stamps each run with the line its span draws.
    let collect = undercurls.is_some() && colors.is_some();

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
                DiagnosticSeverity::HINT
                    | DiagnosticSeverity::INFORMATION
                    | DiagnosticSeverity::WARNING
            )
        {
            // Unused or inactive code mutes each cell's syntax fg toward the
            // background rather than recoloring it, a tagged warning such as an
            // unused variable included. The overlay draws a dotted line under
            // it in the hint color, muted as far as the text. The dedup set
            // blends a cell shared by overlapping Unnecessary spans exactly once.
            let grey = theme.get(severity_scope(sev));
            runs.clear();
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
                collect.then_some(&mut *runs),
            );
            if let (Some(undercurls), Some(colors)) = (undercurls.as_deref_mut(), colors) {
                let hint = match mute_bg {
                    Some(bg) => mute_rgb(colors.hint, bg),
                    None => colors.hint,
                };
                push_undercurl_runs(undercurls, runs, hint, mute_bg, dim, UnderlineKind::Dotted);
            }
            continue;
        }

        let style = theme
            .get(severity_scope(sev))
            .add_modifier(Modifier::UNDERLINED);

        runs.clear();
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
            push_undercurl_runs(
                undercurls,
                runs,
                severity_color(sev, colors),
                mute_bg,
                dim,
                severity_underline(sev),
            );
        }
    }
}

/// Queue each of `runs` as an undercurl span that draws `style` in `color`,
/// blended toward `bg` by `dim` for an unfocused pane.
fn push_undercurl_runs(
    undercurls: &mut UndercurlBatch,
    runs: &[(u16, u16, u16)],
    color: [u8; 3],
    bg: Option<[u8; 3]>,
    dim: f32,
    style: UnderlineKind,
) {
    let color = match bg {
        Some(bg) if dim > 0.0 => dim_rgb(color, bg, dim),
        _ => color,
    };
    for &(x, y, len) in runs {
        undercurls.push(x, y, len, color, style);
    }
}

/// The cursor's inputs to the end-of-line diagnostic messages.
///
/// The cursor's line reads out the worst diagnostic that reaches it from
/// anywhere. Every other line reads the worst diagnostic that starts on it, and
/// the diagnostic that the popover shows prints on no line. One value carries
/// these, so [`paint_line_diagnostics`] reads no cache.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EolCursor {
    /// The buffer row the cursor sits on.
    pub(crate) line: u32,
    /// The diagnostic that the cursor's line reads out.
    pub(crate) readout: Option<ResolvedDiag>,
    /// The index of the diagnostic that the popover shows.
    pub(crate) suppress: Option<usize>,
}

/// Paint each visible line's diagnostic as a message after the line's text.
///
/// A line shows the first message line of the worst diagnostic that starts on
/// it, italic in the severity color, [`EOL_MESSAGE_GAP`] cells past its text.
/// A span over many lines prints once, on the line it starts. The cursor's line
/// reads `cursor.readout` instead, so a span the cursor sits inside still reads
/// out there.
///
/// `spans` are the buffer's resolved diagnostics in start order, and `messages`
/// holds the first message line of each by its index. A soft-wrapped line
/// prints after its last row, and a message that reaches the pane's right edge
/// ends in an ellipsis.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_line_diagnostics(
    spans: &[ResolvedDiag],
    messages: &[Box<str>],
    cursor: EolCursor,
    theme: &Theme,
    snapshot: &DisplaySnapshot,
    scroll_row: u32,
    end_row: u32,
    inner: Rect,
    right: u16,
    buf: &mut Buffer,
) {
    let line_ends = line_end_rows(snapshot, scroll_row, end_row);
    let lines: Vec<u32> = line_ends.iter().map(|&(_, line)| line).collect();
    let starts = line_start_diagnostics(spans, &lines);

    for (&(display_row, line), start) in line_ends.iter().zip(starts) {
        // Containment is by line rather than by offset, so a diagnostic
        // reaching the cursor's line from anywhere on it wins the readout.
        let found = match line == cursor.line {
            true => cursor.readout,
            false => start,
        };
        let Some(found) = found.filter(|found| Some(found.index) != cursor.suppress) else {
            continue;
        };

        let message = messages.get(found.index).map_or("", AsRef::as_ref);
        let style = theme
            .get(severity_scope(found.severity))
            .add_modifier(Modifier::ITALIC);
        let x = u32::from(inner.x) + snapshot.line_len(display_row) + EOL_MESSAGE_GAP;
        let y = inner.y + (display_row - scroll_row) as u16;
        paint_clipped_message(buf, message, x, y, right, style);
    }
}

/// The display rows in `scroll_row..end_row` that end a buffer line, each with
/// the line it ends.
///
/// A soft-wrapped line ends on its last row, and a block row ends no line.
fn line_end_rows(snapshot: &DisplaySnapshot, scroll_row: u32, end_row: u32) -> Vec<(u32, u32)> {
    let last_row = snapshot.line_count().saturating_sub(1);
    (scroll_row..end_row)
        .filter_map(|row| match snapshot.classify_row(row) {
            BlockRowKind::BufferRow { buffer_row } => Some((row, buffer_row)),
            BlockRowKind::Block { .. } => None,
        })
        .filter(|&(row, _)| row >= last_row || !snapshot.is_wrap_continuation(row + 1))
        .collect()
}

/// Write `message` from the cell at `(x, y)` up to `right`, with an ellipsis in
/// the last cell when the message does not fit.
fn paint_clipped_message(
    buf: &mut Buffer,
    message: &str,
    x: u32,
    y: u16,
    right: u16,
    style: Style,
) {
    let room = u32::from(right).saturating_sub(x) as usize;
    if room == 0 {
        return;
    }

    let fits = message.chars().count() <= room;
    let shown = if fits { room } else { room - 1 };
    let mut col = x;
    for ch in message.chars().take(shown) {
        buf[(col as u16, y)].set_char(ch).set_style(style);
        col += 1;
    }
    if !fits {
        buf[(col as u16, y)].set_char('\u{2026}').set_style(style);
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

/// The colors a diagnostic popover draws in.
struct PopoverColors {
    fill: [u8; 3],
    border: [u8; 3],
    /// The message lines.
    text: [u8; 3],
    /// The severity word at the right of the header.
    muted: [u8; 3],
    /// The header's name for the diagnostic, and the icon beside it.
    severity: [u8; 3],
}

/// Render `diag` as a floating popover anchored at `(anchor_col, anchor_row)`,
/// with a severity icon in its first cell. Returns whether it rendered.
///
/// A header row names the diagnostic by [`popover_title`] in the severity
/// color at bold weight, with the severity word at its right in the muted
/// color. The first four message lines follow in the text color. Every line
/// clips at [`POPOVER_COLUMNS`] characters.
///
/// The box is sized to fit and placed by [`popover_rect_avoiding`] so it never
/// covers `cursor_cell`. A message with no text, or a popover with nowhere to
/// go that clears the cursor, draws nothing.
#[allow(clippy::too_many_arguments)]
fn render_diagnostic_popover(
    scene: &mut ApcScene,
    buf: &mut Buffer,
    diag: &lsp_types::Diagnostic,
    colors: &PopoverColors,
    anchor_col: u16,
    anchor_row: u16,
    pane: Rect,
    cursor_cell: Option<(u16, u16)>,
) -> bool {
    let lines: Vec<&str> = diag
        .message
        .lines()
        .take(4)
        .map(|l| clip_chars(l, POPOVER_COLUMNS))
        .collect();
    if lines.iter().all(|l| l.is_empty()) {
        return false;
    }

    let sev = diag.severity.unwrap_or(DiagnosticSeverity::ERROR);
    let word = severity_word(sev);
    // Two blank cells at least keep the name off the severity word.
    let title = popover_title(diag);
    let title = clip_chars(&title, POPOVER_COLUMNS.saturating_sub(word.len() + 2));
    let title_len = title.chars().count();
    let longest = lines
        .iter()
        .map(|l| l.chars().count())
        .chain([title_len + 2 + word.len()])
        .max()
        .unwrap_or(0);

    // Prefix each line with the icon cell and a one-cell gap. The box is still
    // sized from the unprefixed longest line, so w and h stay unchanged and the
    // icon cell falls inside the one-cell content inset.
    let gap = longest - title_len - word.len();
    let mut content = format!("  {title}{:gap$}{word}", "");
    for line in &lines {
        content.push_str("\n  ");
        content.push_str(line);
    }
    let spans = [
        PopoverSpan {
            line: 0,
            start: 2,
            len: title_len as u16,
            fg: colors.severity,
            bold: true,
        },
        PopoverSpan {
            line: 0,
            start: (2 + longest - word.len()) as u16,
            len: word.len() as u16,
            fg: colors.muted,
            bold: false,
        },
    ];

    let w = (longest as u16).saturating_add(4);
    let h = (lines.len() as u16).saturating_add(3);
    let Some(rect) = popover_rect_avoiding(anchor_col, anchor_row, w, h, pane, cursor_cell) else {
        return false;
    };
    if rect.width < 3 || rect.height < 3 {
        return false;
    }

    Popover {
        fill: colors.fill,
        border: colors.border,
        content_fg: colors.text,
        scale: 1,
        offset: [3, 6],
        bold: false,
        chrome: PopoverChrome::CARD,
        center: false,
        content: &content,
        spans: &spans,
    }
    .render(rect, buf, scene);
    Icon {
        kind: icon_kind(sev),
        color: colors.severity,
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

/// The popover colors `theme` gives a diagnostic of `severity` color over an
/// editor background of `editor_bg`.
///
/// A theme without `ui.popover` fills the box a shade darker than the editor
/// and borders it in the severity color. Text that the theme leaves without an
/// RGB color takes the severity color.
fn popover_colors(theme: &Theme, severity: [u8; 3], editor_bg: [u8; 3]) -> PopoverColors {
    let popover = theme.try_get(crate::theme::scope::UI_POPOVER);
    let text = style_rgb(theme.get(crate::theme::scope::UI_TEXT_DIM).fg).unwrap_or(severity);
    PopoverColors {
        fill: popover
            .and_then(|style| style_rgb(style.bg))
            .unwrap_or_else(|| darken(editor_bg)),
        border: popover
            .and_then(|style| style_rgb(style.fg))
            .unwrap_or(severity),
        text,
        muted: style_rgb(theme.get(crate::theme::scope::UI_TEXT_MUTED).fg).unwrap_or(text),
        severity,
    }
}

/// The name a diagnostic's popover header gives it, from its source and code.
///
/// The two join as `source::code`, unless the code already leads with the
/// source that way, as a clippy lint's code does. A diagnostic with one of the
/// two shows that one, and one with neither shows no name.
fn popover_title(diag: &lsp_types::Diagnostic) -> String {
    let code = diag.code.as_ref().map(|code| match code {
        lsp_types::NumberOrString::Number(number) => number.to_string(),
        lsp_types::NumberOrString::String(text) => text.clone(),
    });
    match (diag.source.as_deref(), code) {
        (Some(source), Some(code))
            if code
                .strip_prefix(source)
                .is_some_and(|rest| rest.starts_with("::")) =>
        {
            code
        },
        (Some(source), Some(code)) => format!("{source}::{code}"),
        (Some(source), None) => source.to_owned(),
        (None, code) => code.unwrap_or_default(),
    }
}

/// The word a popover header names `sev` by.
fn severity_word(sev: DiagnosticSeverity) -> &'static str {
    match sev {
        DiagnosticSeverity::WARNING => "warning",
        DiagnosticSeverity::INFORMATION => "info",
        DiagnosticSeverity::HINT => "hint",
        _ => "error",
    }
}

/// Paint `style` over every character cell in the buffer byte range `range`,
/// skipping newlines and `skip_offset` when it is set.
///
/// `skip_offset` is the cursor offset during selection painting, which the
/// caller renders separately. Diagnostic painting passes `None`.
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
            // Widened, since a caller is free to hand in a column past what the
            // coordinate holds. Narrowing first would wrap it onto a cell the
            // pane does show.
            let x = u32::from(inner.x) + display_col + step;
            if x < u32::from(right) && y < bottom {
                let x = x as u16;
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
/// always valid to slice. A row inside a block resolves to the first buffer row
/// below it, since a block holds no buffer bytes.
pub(crate) fn visible_byte_range(
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
            .row_start_at_or_after(row)
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
pub fn editor_cursor_position(editor: &EditorState) -> Option<(u32, u32)> {
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

/// The display rows that 1-based buffer line `line` shows on, from the row of
/// its first character to the row of its last.
///
/// A folded line is the one row its fold shows on, and the block rows below a
/// line are not its own.
pub(crate) fn line_display_rows(snapshot: &DisplaySnapshot, line: u32) -> Range<u32> {
    let row = line.saturating_sub(1);
    let len = snapshot.buffer_snapshot().rope().line_len(row);
    let first = snapshot.buffer_to_display(Point::new(row, 0)).row;
    let last = snapshot.buffer_to_display(Point::new(row, len)).row;
    first..last + 1
}

#[cfg(test)]
mod tests {
    use super::RowSeverity;
    use crate::{
        diff_map::{DiffHunkStatus, GutterMark},
        test_fixture::mark,
    };
    use lsp_types::{Diagnostic, DiagnosticSeverity, NumberOrString};
    use ratatui::{buffer::Buffer, layout::Rect};

    /// Overlapping spans of mixed severity, so the sweep has to keep the worst
    /// one inside the overlap and hand each row back to the one still open once
    /// the other closes. The gaps between them answer nothing.
    #[test]
    fn row_severity_keeps_the_worst_of_overlapping_spans() {
        let span = |start_line, end_line, severity| super::ResolvedDiag {
            start: 0,
            end: 0,
            severity,
            unnecessary: false,
            start_line,
            end_line,
            index: 0,
        };
        let severity = super::row_severity_from_spans(&[
            span(2, 8, DiagnosticSeverity::WARNING),
            span(5, 6, DiagnosticSeverity::ERROR),
            span(12, 12, DiagnosticSeverity::HINT),
        ]);

        let at: Vec<Option<DiagnosticSeverity>> = (0..14).map(|row| severity.at(row)).collect();
        let w = Some(DiagnosticSeverity::WARNING);
        let e = Some(DiagnosticSeverity::ERROR);
        let h = Some(DiagnosticSeverity::HINT);
        assert_eq!(
            at,
            vec![None, None, w, w, w, e, e, w, w, None, None, None, h, None],
            "the error wins rows 5 and 6, the warning holds either side of it",
        );

        assert_eq!(
            severity.rows_in(4..8),
            vec![4, 5, 6, 7],
            "the run spells back out to the rows asked about, clipped to them",
        );
        assert_eq!(severity.rows_in(9..12), Vec::<u32>::new(), "a gap has none");
        assert!(super::row_severity_from_spans(&[]).is_empty());
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

    /// The fallback gutter paints one glyph right of the number on every
    /// marked row. A change takes `▎`, and a deletion `▔`.
    #[test]
    fn fallback_gutter_paints_the_git_glyph_cell() {
        let theme = crate::theme::Theme::empty();
        let folded: Vec<(u32, u16)> = (1..=5).map(|number| (number, 1)).collect();
        let diff_marks: std::collections::BTreeMap<u32, GutterMark> = (0u32..)
            .zip([
                mark(DiffHunkStatus::Modified),
                mark(DiffHunkStatus::Modified),
                mark(DiffHunkStatus::Modified),
                mark(DiffHunkStatus::Deleted),
                mark(DiffHunkStatus::Deleted),
            ])
            .collect();
        let area = Rect::new(0, 0, 12, 5);
        let mut buf = Buffer::empty(area);

        let width = super::draw_fallback_line_numbers(
            &folded,
            1,
            &RowSeverity::default(),
            &diff_marks,
            None,
            area,
            &theme,
            &mut buf,
        );

        let row = |y: u16| (0..width).map(|x| buf[(x, y)].symbol()).collect::<String>();
        assert_eq!(width, 4, "mark cell, one digit, the git glyph, and a gap");
        assert_eq!(
            (0..5).map(row).collect::<Vec<_>>(),
            [" 1▎ ", " 2▎ ", " 3▎ ", " 4▔ ", " 5▔ "],
            "the change glyph on each changed row and the deletion glyph on each deletion",
        );
    }

    /// A pane 3 columns wide at the screen's right edge holds the mark and the
    /// number, and its glyph cell falls past the edge.
    #[test]
    fn fallback_gutter_clips_to_a_pane_narrower_than_itself() {
        use crate::diff_map::DiffHunkStatus;
        let theme = crate::theme::Theme::empty();
        let folded = [(1u32, 1u16), (2, 1)];
        let mut diff_marks = std::collections::BTreeMap::new();
        diff_marks.insert(0u32, mark(DiffHunkStatus::Modified));
        let mut buf = Buffer::empty(Rect::new(0, 0, 10, 2));

        let width = super::draw_fallback_line_numbers(
            &folded,
            2,
            &RowSeverity::for_rows([(0, DiagnosticSeverity::ERROR)]),
            &diff_marks,
            None,
            Rect::new(7, 0, 3, 2),
            &theme,
            &mut buf,
        );

        let row = |y: u16| (0..10u16).map(|x| buf[(x, y)].symbol()).collect::<String>();
        assert_eq!(
            (width, row(0), row(1)),
            (5, "       E 1".to_string(), "         2".to_string()),
            "the reserved width stays whole while every write stays inside the pane",
        );
    }

    /// A mark's scope resolves on the first row that paints it and is reused by
    /// the rest, so a second row of the same kind has to come out in that kind's
    /// color rather than whatever the reuse hands back.
    #[test]
    fn fallback_gutter_reuses_a_resolved_mark_style_across_rows() {
        use crate::{diff_map::DiffHunkStatus, theme::scope as s};

        let theme = {
            let src = r##"theme t { ui.text.fg = "#111111"; diff.modified.fg = "#22ff22"; diff.added.fg = "#3333ff"; }"##;
            let (config, _) = stoat_config::parse(src);
            crate::theme::Theme::from_config(&config.expect("theme parses"), "t")
                .expect("theme loads")
        };

        let folded = [(1u32, 1u16), (2, 1), (3, 1)];
        let diff_marks: std::collections::BTreeMap<u32, GutterMark> = (0u32..)
            .zip(
                [
                    DiffHunkStatus::Modified,
                    DiffHunkStatus::Added,
                    DiffHunkStatus::Modified,
                ]
                .map(mark),
            )
            .collect();
        let area = Rect::new(0, 0, 12, 3);
        let mut buf = Buffer::empty(area);

        super::draw_fallback_line_numbers(
            &folded,
            1,
            &RowSeverity::default(),
            &diff_marks,
            None,
            area,
            &theme,
            &mut buf,
        );

        let modified = theme.get(s::DIFF_MODIFIED).fg.expect("the fixture sets it");
        let added = theme.get(s::DIFF_ADDED).fg.expect("the fixture sets it");
        assert_ne!(
            modified,
            theme.get(s::UI_TEXT).fg.expect("the fixture sets it"),
            "the fixture must give the mark its own color"
        );
        assert_eq!(
            [0u16, 1, 2].map(|y| buf[(2u16, y)].fg),
            [modified, added, modified],
            "each row paints its mark in its kind's color",
        );
    }

    /// Mark colors with a different value per field, so a bar's color names
    /// the field it took.
    fn distinct_mark_colors() -> super::DiffMarkColors {
        super::DiffMarkColors {
            added: [10, 20, 30],
            modified: [40, 50, 60],
            moved: [70, 80, 90],
            deleted: [100, 110, 120],
            staged: [130, 140, 150],
            unstaged: [160, 170, 180],
            approved: [190, 200, 210],
            unapproved: [220, 230, 240],
        }
    }

    /// Severity colors for a gutter test with no diagnostic in it.
    fn no_severity() -> super::SeverityColors {
        super::SeverityColors {
            error: [0, 0, 0],
            warning: [0, 0, 0],
            info: [0, 0, 0],
            hint: [0, 0, 0],
        }
    }

    /// The rich gutter's hunk lane takes the kind's full color on every marked
    /// row, and its state lane the hunk's state color. A deletion's line sets
    /// `seam`, which draws its hunk bar short.
    #[test]
    fn rich_gutter_hunk_lane_marks_every_row_of_a_hunk() {
        let folded: Vec<(u32, u16)> = (1..=4).map(|number| (number, 1)).collect();
        let diff_marks: std::collections::BTreeMap<u32, GutterMark> = (0u32..)
            .zip([
                mark(DiffHunkStatus::Modified),
                mark(DiffHunkStatus::Modified),
                mark(DiffHunkStatus::Modified),
                mark(DiffHunkStatus::Deleted),
            ])
            .collect();

        let lines = super::gutter_component_lines(
            &folded,
            &RowSeverity::default(),
            &diff_marks,
            &distinct_mark_colors(),
            &no_severity(),
            None,
            None,
        );

        let modified = Some(([Some([40, 50, 60]), Some([160, 170, 180])], false));
        assert_eq!(
            lines
                .iter()
                .map(|line| line.git.map(|git| (git.slots, git.seam)))
                .collect::<Vec<_>>(),
            [
                modified,
                modified,
                modified,
                Some(([Some([100, 110, 120]), Some([160, 170, 180])], true)),
            ],
            "three modified rows at the full modified color, then the deletion with `seam` set",
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

    /// A popover names a diagnostic `source::code`, the code alone when it
    /// already leads with its source and `::`, or whichever of the two it has.
    #[test]
    fn a_popover_title_joins_source_and_code() {
        let titled = |source: Option<&str>, code: Option<NumberOrString>| {
            super::popover_title(&Diagnostic {
                source: source.map(String::from),
                code,
                ..Default::default()
            })
        };
        let text = |code: &str| Some(NumberOrString::String(code.into()));
        assert_eq!(
            [
                titled(Some("rustc"), text("E0308")),
                titled(Some("clippy"), text("clippy::clone_on_copy")),
                titled(Some("clip"), text("clippy::x")),
                titled(Some("pyright"), Some(NumberOrString::Number(2304))),
                titled(Some("rust-analyzer"), None),
                titled(None, text("E0425")),
                titled(None, None),
            ],
            [
                "rustc::E0308",
                "clippy::clone_on_copy",
                "clip::clippy::x",
                "pyright::2304",
                "rust-analyzer",
                "E0425",
                "",
            ],
        );
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
}
