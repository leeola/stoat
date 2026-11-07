//! The paint of a [`View::DiffChanges`](crate::pane::View::DiffChanges) pane.
//!
//! The changes view paints its excerpts one under another through the
//! per-file diff's painter, so a hunk reads the same here as in the per-file
//! diff. A header row names each excerpt's file and counts its diff, centered
//! across the pane.
//! The focused excerpt paints the cursor where the changes view's keys act.
//!
//! Under relative numbering every excerpt numbers its rows by the `j` or `k`
//! steps that reach them from the focused excerpt's cursor, so a number is
//! the count to type before the key.

use crate::{
    buffer_registry::BufferRegistry,
    diff_changes::{self, DiffChangesState, ExcerptStats},
    editor_state::{EditorId, EditorState},
    pane::Pane,
    render::{
        chrome::{self, TypeTag},
        editor::{
            fit_diff_view, render_editor_with_overlay, screen_cell, split_minimap_strip,
            DIFF_OVERVIEW_COLS,
        },
        layout::split_pane_status,
        paint::style_rgb,
        pane::diff_base_tint,
        review::{self, FrameEdges},
        text::{text_width, write_str, write_str_clipped},
        undercurl::UndercurlBatch,
        FrameCtx,
    },
    theme::{scope, Theme},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
};
use slotmap::SlotMap;
use std::path::Path;
use stoat_config::LineNumbers;
use stoat_widgets::{
    bar::{Bar, BarBatch},
    ApcScene,
};

/// The sixteenths down a header row at which its two hairlines lie, the row's
/// top and bottom edges.
const HAIRLINE_Y: [i16; 2] = [0, 15];

/// How much of a page of the changes view's pool a paint covers.
///
/// The terminal keeps a page's cells and replaces its decorations as one set.
/// A step that moves only what the decorations show, such as the line that
/// relative numbers count from, sends new decorations over the cells the
/// terminal keeps, so its paint skips the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PagePaint {
    /// The rows' cells and the decorations over them.
    Cells,
    /// The decorations alone, for a page whose cells the terminal keeps. The
    /// paint writes no text into the cells.
    DecorationsOnly,
}

/// What a paint records on an excerpt's editor about where it laid the
/// excerpt out.
///
/// The terminal cursor, cursor popups, and pointer hits read these back from
/// the live paint, so a page paint puts them back as it found them.
struct PaintLayout {
    scroll_row: u32,
    viewport_rows: Option<u32>,
    cursor_screen_cell: Option<(u16, u16)>,
    text_rect: Option<Rect>,
    minimap_rect: Option<Rect>,
}

impl PaintLayout {
    fn of(editor: &EditorState) -> Self {
        Self {
            scroll_row: editor.scroll_row,
            viewport_rows: editor.viewport_rows,
            cursor_screen_cell: editor.cursor_screen_cell,
            text_rect: editor.text_rect,
            minimap_rect: editor.minimap_rect,
        }
    }

    fn restore(self, editor: &mut EditorState) {
        editor.scroll_row = self.scroll_row;
        editor.viewport_rows = self.viewport_rows;
        editor.cursor_screen_cell = self.cursor_screen_cell;
        editor.text_rect = self.text_rect;
        editor.minimap_rect = self.minimap_rect;
    }
}

/// Paint the stack `state` holds into `pane`'s rows above its status row.
///
/// [`render_pane_status`](crate::render::pane::render_pane_status) paints the
/// pane status row through the focused excerpt, into the pane's status lane,
/// so a replay of this paint keeps a fresh row.
///
/// The stack paints from the scroll of `state`, which first moves so the
/// focused excerpt's cursor shows. A screen row that a landing holds takes the
/// cursor's row. Otherwise a cursor that moved since the last paint takes the
/// least scroll that keeps it on screen, and a cursor that stayed leaves the
/// scroll, so a wheel scroll leaves the cursor off screen.
///
/// Each excerpt takes a header row, which names its file and counts its diff,
/// and then its rows. The rows paint from the excerpt's own scroll row, so an
/// excerpt cut at the top shows its lower rows. After the paint each excerpt's
/// viewport holds all of its rows, so the editor's own follow never scrolls an
/// excerpt.
///
/// The focused excerpt records the cell of its cursor in
/// [`EditorState::cursor_screen_cell`] when the terminal draws that cursor.
/// The record is `None` when the paint does not lay the excerpt out, such as
/// an excerpt outside the window.
///
/// With no excerpt the pane says "building changes..." while the repository
/// tally is pending, and "no changes" once it is in.
///
/// With per-pane minimaps on, a pane wide enough for the minimap strip gives
/// its right edge to the strip, with or without excerpts. The strip's rect
/// lands on [`DiffChangesState::minimap_rect`], and no excerpt reserves a strip
/// of its own.
///
/// Where [`stack_composited`] holds, the stack's runs and bars go to a scene
/// the paint drops. The pool's pages carry their own, and the composite covers
/// the live ones.
#[allow(clippy::too_many_arguments)]
pub(crate) fn render_diff_changes(
    pane: &Pane,
    is_focused: bool,
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    text_style: Style,
    undercurls: &mut UndercurlBatch,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let (inner, _) = split_pane_status(pane.area);
    let (inner, strip) = split_minimap_strip(inner, frame.minimap_enabled, DIFF_OVERVIEW_COLS);
    state.minimap_rect = strip;
    if let Some(editor) = state
        .focused_excerpt()
        .and_then(|excerpt| editors.get_mut(excerpt.editor_id))
    {
        editor.cursor_screen_cell = None;
    }
    if inner.width == 0 || inner.height == 0 {
        return;
    }

    if state.excerpts.is_empty() {
        let text = match frame.diff_changes_building {
            true => "building changes...",
            false => "no changes",
        };
        write_str(
            buf,
            inner.x,
            inner.y,
            text,
            frame.theme.get(scope::UI_TEXT_MUTED),
        );
        return;
    }

    state.viewport_rows = inner.height as usize;
    let rich = review::diff_rich_gutter_scale(scene.live(), frame.theme, text_style);
    // Every excerpt sizes its gutters for the widest count among them, so the
    // text column lines up down the stack across files of different sizes.
    let digits = state
        .excerpts
        .iter()
        .filter_map(|excerpt| {
            editors
                .get_mut(excerpt.editor_id)
                .map(|editor| review::diff_gutter_digits(&editor.display_map.snapshot()))
        })
        .max()
        .unwrap_or(2);
    for excerpt in &state.excerpts {
        if let Some(editor) = editors.get_mut(excerpt.editor_id) {
            editor.stack_gutter_digits = Some(digits);
            fit_diff_view(
                editor,
                inner,
                frame.wrap_mode,
                frame.wrap_column,
                digits,
                rich,
            );
        }
    }
    let total = diff_changes::layout_stack(state, editors);
    let cursor = diff_changes::focused_cursor(state, editors);
    if let Some(row) = diff_changes::cursor_stack_row(state, editors) {
        match state.held_screen_row.take() {
            Some(screen_row) => {
                let last = state.viewport_rows.saturating_sub(1);
                state.scroll = row.saturating_sub(usize::from(screen_row).min(last));
            },
            None if cursor != state.followed_cursor => {
                state.ensure_row_visible(row, state.viewport_rows);
            },
            None => {},
        }
        state.followed_cursor = cursor;
    }
    state.scroll = state.scroll.min(total.saturating_sub(state.viewport_rows));

    let first_row = state.scroll;
    let mut covered = stack_composited(pane, frame).then(ApcScene::new);
    let scene = covered.as_mut().unwrap_or(scene);
    paint_stack_rows(
        state,
        editors,
        buffers,
        is_focused,
        text_style,
        undercurls,
        frame,
        buf,
        scene,
        first_row,
        inner,
        PagePaint::Cells,
    );
}

/// Whether the terminal composites the resident pool of `pane`'s stack over
/// the rect the stack paints into.
///
/// The rect is the pane's content area less its minimap strip, the rect that
/// [`diff_changes_pool_panes`](crate::apc_emit::diff_changes_pool_panes)
/// declares. A pool that the terminal does not hold yet, or holds at another
/// rect, covers no part of the paint.
pub(crate) fn stack_composited(pane: &Pane, frame: FrameCtx<'_>) -> bool {
    let (inner, _) = split_pane_status(pane.area);
    let (inner, _) = split_minimap_strip(inner, frame.minimap_enabled, DIFF_OVERVIEW_COLS);
    frame.composited_stacks.contains(&(pane.index, inner))
}

/// Paint stack rows `first_row..first_row + area.height` of the stack `state`
/// holds into `area` of `buf`.
///
/// Each excerpt the rows reach takes a header row and then its rows. The header
/// names the file and counts its diff, centered on the raised surface. Under a
/// live scene a hairline in the inactive border color runs along the header's
/// top and bottom edges. The rows paint from the excerpt's own scroll row, so
/// an excerpt cut at the top of `area` shows its lower rows. A painted
/// excerpt's editor records where this paint laid it out: its scroll row, its
/// viewport, its text rect, and the cell of its cursor.
///
/// The rows draw the two sides of the frame around the base column, as
/// [`review::emit_base_frame`] lays them over `area`, and no top. The sides
/// cross every header, over its hairlines.
///
/// Under [`PagePaint::DecorationsOnly`] the rows write no cell. The headers'
/// hairlines, the frame's sides, and the excerpts' diff gutters fill `scene`
/// alone.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_stack_rows(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    is_focused: bool,
    text_style: Style,
    undercurls: &mut UndercurlBatch,
    frame: FrameCtx<'_>,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    first_row: usize,
    area: Rect,
    paint: PagePaint,
) {
    let (scroll, rows_on_screen) = (first_row, usize::from(area.height));
    let header_row = |top: usize| {
        (scroll..scroll + rows_on_screen)
            .contains(&top)
            .then(|| Rect {
                y: area.y + (top - scroll) as u16,
                height: 1,
                ..area
            })
    };

    // The frame's sides draw over the header hairlines, and the excerpts'
    // hunk lanes draw over the frame's left side.
    emit_header_hairlines(
        state
            .excerpts
            .iter()
            .filter_map(|excerpt| header_row(excerpt.top)),
        frame.theme,
        scene,
    );
    if let Some(tint) = diff_base_tint(frame.theme) {
        review::emit_base_frame(area, tint, FrameEdges::Sides, scene);
    }

    let origins =
        (is_focused && frame.line_numbers == LineNumbers::Relative && frame.mode != "insert")
            .then(|| diff_changes::step_origins(state, editors))
            .flatten();
    for (index, excerpt) in state.excerpts.iter().enumerate() {
        let rows = diff_changes::excerpt_rows(excerpt, editors);
        let top = excerpt.top;
        if top + 1 + rows <= scroll || top >= scroll + rows_on_screen {
            continue;
        }
        if let Some(row) = header_row(top).filter(|_| paint == PagePaint::Cells) {
            let stats = diff_changes::excerpt_stats(excerpt, buffers);
            let runs = header_runs(&excerpt.rel_path, stats, frame.theme);
            paint_header(buf, row, &runs, frame.theme);
        }

        // The screen row of the excerpt's first row, past its header.
        let first = (top + 1).saturating_sub(scroll);
        let Some(editor) = editors.get_mut(excerpt.editor_id) else {
            continue;
        };
        if rows == 0 || first >= rows_on_screen {
            continue;
        }
        editor.scroll_row = scroll.saturating_sub(top + 1) as u32;
        let height = (rows - editor.scroll_row as usize).min(rows_on_screen - first);
        let excerpt_area = Rect {
            x: area.x,
            y: area.y + first as u16,
            width: area.width,
            height: height as u16,
        };
        let focused = is_focused && index == state.focused;
        let diagnostic_info = buffers
            .path_for(editor.buffer_id)
            .map(|path| (path, frame.diagnostics));
        render_editor_with_overlay(
            editor,
            excerpt_area,
            text_style,
            frame.theme,
            frame.chrome,
            buf,
            focused,
            false,
            frame.line_numbers,
            frame.mode == "insert",
            if focused { frame.hover_cell } else { None },
            if focused {
                frame.goto_word_labels
            } else {
                None
            },
            diagnostic_info,
            match scene.live() {
                true => Some(&mut *scene),
                false => None,
            },
            frame.terminal_cursor_editor == Some(excerpt.editor_id),
            Some(&mut *undercurls),
            0.0,
            frame.diff_dials,
            frame.wrap_mode,
            frame.wrap_column,
            frame.scrolloff,
            frame.reload_flashes,
            paint == PagePaint::DecorationsOnly,
            origins.as_ref().map(|origins| origins[index]),
        );
        editor.viewport_rows = Some(rows as u32);
    }
}

/// Emit a hairline along the top and the bottom edge of each header row in
/// `rows`, in the inactive border color.
fn emit_header_hairlines(rows: impl Iterator<Item = Rect>, theme: &Theme, scene: &mut ApcScene) {
    let Some(color) = style_rgb(theme.get(scope::UI_BORDER_INACTIVE).fg) else {
        return;
    };
    let mut bars = BarBatch::default();
    for row in rows {
        for y in HAIRLINE_Y {
            bars.push(
                row,
                Bar {
                    x: 0,
                    y,
                    width: row.width * 16,
                    height: 1,
                    color,
                },
            );
        }
    }
    bars.flush(scene);
}

/// The runs of the header over the file at `rel_path`, each in the style it
/// paints in.
///
/// The header names the file's type tag, its directory, and its name. Then it
/// counts the lines `stats` adds and removes and the hunks. A count of zero
/// takes no run. Where the tree pass refined a hunk, the count names the git
/// hunks and then the structural ones. With no structural count to tell them
/// from, the count names the hunks alone.
fn header_runs(rel_path: &str, stats: ExcerptStats, theme: &Theme) -> Vec<(String, Style)> {
    let muted = theme.get(scope::UI_TEXT_MUTED);
    let mut runs = Vec::new();
    if let Some(TypeTag {
        tag,
        scope: tag_scope,
        ..
    }) = chrome::type_tag(Path::new(rel_path))
    {
        runs.push((format!("{tag} "), theme.get(tag_scope)));
    }

    let (dir, name) = rel_path.split_at(rel_path.rfind('/').map_or(0, |slash| slash + 1));
    runs.push((dir.to_string(), muted));
    runs.push((
        name.to_string(),
        theme.get(scope::UI_TEXT).add_modifier(Modifier::BOLD),
    ));

    if stats.added > 0 {
        runs.push((format!(" +{}", stats.added), theme.get(scope::DIFF_ADDED)));
    }
    if stats.removed > 0 {
        runs.push((
            format!(" \u{2212}{}", stats.removed),
            theme.get(scope::DIFF_DELETED),
        ));
    }
    if stats.hunks > 0 {
        let count = |n: usize, noun: &str| match n {
            1 => format!("1 {noun}"),
            n => format!("{n} {noun}s"),
        };
        let hunks = match stats.structural {
            0 => count(stats.hunks, "hunk"),
            structural => format!(
                "{} \u{b7} {structural} structural",
                count(stats.hunks, "git hunk")
            ),
        };
        runs.push((format!(" \u{b7} {hunks}"), muted));
    }
    runs
}

/// Fill `row` with the raised surface and write `runs` across it, centered.
///
/// Runs wider than the row start at its left column and clip at its right
/// edge, so the start of the file's path stays in view.
fn paint_header(buf: &mut Buffer, row: Rect, runs: &[(String, Style)], theme: &Theme) {
    let fill = theme.get(scope::UI_SURFACE_RAISED);
    let end_x = row.x + row.width;
    for x in row.x..end_x {
        buf[(x, row.y)].set_char(' ').set_style(fill);
    }

    let width: usize = runs.iter().map(|(text, _)| text_width(text)).sum();
    let mut x = row.x + (row.width - width.min(usize::from(row.width)) as u16) / 2;
    for (text, style) in runs {
        write_str_clipped(buf, x, row.y, text, *style, end_x);
        x = x.saturating_add(text_width(text) as u16);
    }
}

/// Paint the stack rows from `first_row` over the whole of `page`, as a page of
/// the changes view's pool holds them.
///
/// The rows paint as [`paint_stack_rows`] paints them, with selections, search
/// matches, and diagnostics. Each excerpt's editor keeps the layout the live
/// paint recorded, so a page painted after the live frame leaves the terminal
/// cursor and pointer hits on the live rows.
///
/// `paint` names whether the page's cells paint too, as [`PagePaint`]
/// describes.
///
/// The page scrolls with the stack, so it takes the sides of the frame that
/// the rows draw and no top. The pane's own frame keeps the top edge above the
/// pool. An empty stack holds no pool, so every page has excerpts to frame.
#[allow(clippy::too_many_arguments)]
pub(crate) fn paint_stack_page(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    is_focused: bool,
    text_style: Style,
    undercurls: &mut UndercurlBatch,
    frame: FrameCtx<'_>,
    page: &mut Buffer,
    scene: &mut ApcScene,
    first_row: usize,
    paint: PagePaint,
) {
    let live: Vec<(EditorId, PaintLayout)> = state
        .excerpts
        .iter()
        .filter_map(|excerpt| {
            let editor = editors.get(excerpt.editor_id)?;
            Some((excerpt.editor_id, PaintLayout::of(editor)))
        })
        .collect();
    let area = page.area;
    paint_stack_rows(
        state, editors, buffers, is_focused, text_style, undercurls, frame, page, scene, first_row,
        area, paint,
    );
    for (editor_id, layout) in live {
        if let Some(editor) = editors.get_mut(editor_id) {
            layout.restore(editor);
        }
    }
}

/// Record the stack's cursor on a frame that replays the pane's last paint, as
/// [`render_diff_changes`] records it when it paints.
///
/// The paint records which cursor the stack followed. It also lays the focused
/// excerpt out at the stack's scroll and records the cell where the terminal
/// draws its cursor. A replay puts back the cells of an earlier paint at the
/// same scroll. A paint under no key between the two leaves the excerpt laid
/// out at the scroll that paint used, which differs when it scrolled. So the
/// replay lays the excerpt out again from the stack's scroll and the excerpt's
/// place in it, and records the cell there.
///
/// When the focused excerpt is outside the window, or the terminal does not
/// draw its cursor, a replay records no cell. The paint does the same.
pub(crate) fn replay_cursor(
    pane: &Pane,
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    frame: FrameCtx<'_>,
    scene_live: bool,
) {
    if diff_changes::cursor_stack_row(state, editors).is_some() {
        state.followed_cursor = diff_changes::focused_cursor(state, editors);
    }
    let Some(excerpt) = state.focused_excerpt() else {
        return;
    };
    if let Some(editor) = editors.get_mut(excerpt.editor_id) {
        editor.cursor_screen_cell = None;
    }
    if !scene_live || frame.terminal_cursor_editor != Some(excerpt.editor_id) {
        return;
    }

    let (top, scroll, viewport) = (excerpt.top, state.scroll, state.viewport_rows);
    let rows = diff_changes::excerpt_rows(excerpt, editors);
    let first = (top + 1).saturating_sub(scroll);
    if rows == 0 || top + 1 + rows <= scroll || top >= scroll + viewport || first >= viewport {
        return;
    }
    let Some(editor) = editors.get_mut(excerpt.editor_id) else {
        return;
    };
    let Some(text) = editor.text_rect else {
        return;
    };
    let (inner, _) = split_pane_status(pane.area);
    let (inner, _) = split_minimap_strip(inner, frame.minimap_enabled, DIFF_OVERVIEW_COLS);
    let scroll_row = scroll.saturating_sub(top + 1);
    let text = Rect {
        y: inner.y + first as u16,
        height: (rows - scroll_row).min(viewport - first) as u16,
        ..text
    };
    editor.scroll_row = scroll_row as u32;
    editor.text_rect = Some(text);

    let point = diff_changes::cursor_point(editor);
    let snapshot = editor.display_map.snapshot();
    let end_row = (editor.scroll_row + u32::from(text.height)).min(snapshot.line_count());
    editor.cursor_screen_cell = screen_cell(
        snapshot.buffer_to_display(point),
        editor.scroll_row,
        end_row,
        text,
    );
}

#[cfg(test)]
mod tests {
    use super::{header_runs, paint_stack_page, paint_stack_rows, PagePaint};
    use crate::{
        action_handlers, apc_emit,
        app::Stoat,
        buffer_registry::BufferRegistry,
        diff_changes::{self, DiffChangesState, ExcerptStats},
        editor_state::{EditorId, EditorState},
        minimap,
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
        render::{
            commits::PreviewDials,
            content_lane,
            layout::split_pane_status,
            paint,
            review::{self, DiffColumns, DiffDials, DiffLayout},
            surface_blank, themed_bg,
            undercurl::UndercurlBatch,
            FrameCtx, SINGLE_MINIMAP_STRIP_ID,
        },
        test_fixture::drain_apc,
        test_harness::TestHarness,
        theme::scope,
        workspace::Workspace,
    };
    use crossterm::event::Event;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier, Style},
    };
    use slotmap::SlotMap;
    use std::{
        collections::HashMap,
        mem,
        path::{Path, PathBuf},
        sync::LazyLock,
    };
    use stoat_config::{LineNumbers, MinimapMode, WrapMode};
    use stoat_text::Point;
    use stoat_widgets::ApcScene;
    use stoatty_protocol::command::{decode_stream, Command};
    use tokio::sync::mpsc::UnboundedReceiver;

    /// The base tint of the shipped theme, which the frame around the base
    /// column takes.
    const TINT: [u8; 3] = [0x2a, 0x1d, 0x20];

    /// The inactive border color of the shipped theme, which the header
    /// hairlines take.
    const HAIRLINE: [u8; 3] = [0x2e, 0x32, 0x3b];

    /// The strip id, content id, and palette of a minimap strip that a frame
    /// declares.
    type Declared = (u32, u32, Vec<[u8; 3]>);

    /// The first row the changes view paints, as text, from one render that
    /// runs no pool job.
    fn first_row(h: &mut TestHarness) -> String {
        let buf = h.render_composited();
        (0..buf.area.width)
            .map(|x| buf[(x, 0)].symbol())
            .collect::<String>()
            .trim_end()
            .to_string()
    }

    /// The focused changes view's state, with the editors its excerpts are.
    fn stack(h: &mut TestHarness) -> (&mut DiffChangesState, &mut SlotMap<EditorId, EditorState>) {
        let ws = h.stoat.active_workspace_mut();
        let id = ws
            .focused_diff_changes_id()
            .expect("the changes view is open");
        let Workspace {
            diff_changes_views,
            editors,
            ..
        } = ws;
        (&mut diff_changes_views[id], editors)
    }

    /// The text in columns `x` to `x + width` of row `y`, trimmed.
    fn cells(buf: &Buffer, x: u16, width: u16, y: u16) -> String {
        (x..x + width)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// The text in columns `x` to `x + width` of header row `y`, trimmed, and
    /// the column it starts on.
    ///
    /// The composite draws the header's hairlines as rules over the row's
    /// blank cells, so the reader takes a rule cell as blank.
    fn header_text(buf: &Buffer, x: u16, width: u16, y: u16) -> (String, Option<u16>) {
        let symbol = |x: u16| match buf[(x, y)].symbol() {
            "─" => " ",
            symbol => symbol,
        };
        let start = (x..x + width).find(|&x| symbol(x) != " ");
        let text: String = (x..x + width).map(symbol).collect();
        (text.trim().to_string(), start)
    }

    /// The columns the live frame lays an excerpt out in over `inner` when the
    /// stack's gutters take `digits`, rich as the harness's theme resolves it.
    fn excerpt_columns(h: &TestHarness, inner: Rect, digits: u16) -> DiffColumns {
        let theme = &h.stoat.theme;
        let rich = review::diff_rich_gutter_scale(true, theme, theme.get(scope::UI_TEXT));
        DiffColumns::compute(inner, DiffLayout::diff_view(digits, rich))
    }

    /// The line number a diff side's gutter shows on row `y`, the digits in
    /// the cells from the side's first cell to its text.
    ///
    /// A rich gutter's number composites at the cell its run starts in, which
    /// is not the cell gutter's number field, so the reader takes the digits
    /// wherever the gutter put them.
    fn gutter_number(buf: &Buffer, side_x: u16, text_x: u16, y: u16) -> String {
        (side_x..text_x)
            .map(|x| buf[(x, y)].symbol())
            .collect::<String>()
            .chars()
            .filter(char::is_ascii_digit)
            .collect()
    }

    /// Two changed files under a focused changes view with no minimap, at a
    /// width that pairs each modified line with its base line, settled.
    fn two_files(height: u16) -> TestHarness {
        changes_view(
            120,
            height,
            &[("a.txt", "a\nx\n", "A\nx\n"), ("b.txt", "b\n", "B\n")],
        )
    }

    /// `files` changed under a focused changes view with no minimap, opened
    /// over the first file, settled.
    fn changes_view(width: u16, height: u16, files: &[(&str, &str, &str)]) -> TestHarness {
        let mut h = TestHarness::with_size(width, height);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
        h.stage_review_scenario("/repo", files);
        h.open_file(&Path::new("/repo").join(files[0].0));
        h.cover_with_diff_changes(false);
        h.settle();
        h
    }

    /// Each excerpt paints under a header row that names its file and counts
    /// its diff in the middle of the row, with its rows numbered in the
    /// per-file diff's gutters, and only the focused excerpt paints a cursor.
    ///
    /// The numbers are absolute, so each pair names its own base and buffer
    /// line.
    #[test]
    fn a_changes_view_paints_its_excerpts_under_their_file_headers() {
        let mut h = two_files(16);
        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Absolute);
        let buf = h.render_composited();
        let inner = {
            let ws = h.stoat.active_workspace();
            split_pane_status(ws.panes.pane(ws.panes.focus()).area).0
        };
        let cols = excerpt_columns(&h, inner, 2);

        // a.txt's three rows sit under its header on row 0, and b.txt's first
        // row under its header on row 4. Each header pairs the rule in column
        // 0 with its text and the column the text starts on. The composite
        // keeps the first rule that a blank cell takes, and the header's
        // hairline goes out ahead of the frame's left side, which the terminal
        // draws over it.
        let headers = [inner.y, inner.y + 4].map(|y| {
            let (text, start) = header_text(&buf, inner.x + 1, inner.width - 1, y);
            (buf[(inner.x, y)].symbol().to_string(), text, start)
        });
        let numbers: Vec<(String, String)> = [1, 2, 3, 5]
            .into_iter()
            .map(|row| {
                (
                    gutter_number(&buf, cols.left_x, cols.left_text_x, inner.y + row),
                    gutter_number(&buf, cols.right_x, cols.right_text_x, inner.y + row),
                )
            })
            .collect();
        let (state, editors) = stack(&mut h);
        let cursors: Vec<Option<(u16, u16)>> = state
            .excerpts
            .iter()
            .map(|excerpt| editors[excerpt.editor_id].cursor_screen_cell)
            .collect();
        let pair = |n: &str| (n.to_string(), n.to_string());
        let header = |name: &str| format!("{name} +1 \u{2212}1 \u{b7} 1 hunk");
        let centered = Some(inner.x + (inner.width - 20) / 2);
        assert_eq!(
            (headers, numbers, cursors),
            (
                [
                    ("─".to_string(), header("a.txt"), centered),
                    ("─".to_string(), header("b.txt"), centered)
                ],
                vec![pair("1"), pair("2"), pair("3"), pair("1")],
                vec![Some((cols.right_text_x, inner.y + 1)), None],
            ),
        );
    }

    /// The excerpts share the widest gutter any of them needs, so a five-line
    /// file's text starts at the column a 1200-line file's text starts at, the
    /// column a four-digit gutter leaves, and both wrap at that gutter's width.
    #[test]
    fn excerpts_of_different_sizes_share_one_text_column() {
        let long_base: String = (1..=1200).map(|n| format!("{n}\n")).collect();
        let long_text = long_base.replacen("1\n", "ONE\n", 1);
        let mut h = changes_view(
            120,
            24,
            &[
                ("a.txt", "a\nb\nc\nd\ne\n", "A\nb\nc\nd\ne\n"),
                ("b.txt", &long_base, &long_text),
            ],
        );
        h.stoat.settings.editor_wrap = Some(WrapMode::EditorWidth);
        h.render_composited();
        let inner = {
            let ws = h.stoat.active_workspace();
            split_pane_status(ws.panes.pane(ws.panes.focus()).area).0
        };
        // The wrap stops one cell short of the narrower text column.
        let cols = excerpt_columns(&h, inner, 4);
        let wrap = (cols.left_content_w.min(cols.right_content_w) - 1) as u32;

        let (state, editors) = stack(&mut h);
        let layouts: Vec<(Option<u16>, Option<u32>)> = state
            .excerpts
            .iter()
            .map(|excerpt| {
                let editor = &editors[excerpt.editor_id];
                (
                    editor.text_rect.map(|text| text.x),
                    editor.display_map.wrap_width(),
                )
            })
            .collect();
        assert_eq!(
            layouts,
            [(Some(cols.right_text_x), Some(wrap)); 2],
            "both excerpts paint and wrap for the four-digit gutter"
        );
    }

    /// A file name wider than the pane starts at the pane's left column and
    /// clips at its right edge.
    #[test]
    fn a_header_wider_than_the_pane_clips_from_its_left_column() {
        let name = "a-file-name-wider-than-the-pane.txt";
        let mut h = changes_view(30, 16, &[(name, "a\n", "A\n")]);
        let buf = h.render_composited();
        let inner = {
            let ws = h.stoat.active_workspace();
            split_pane_status(ws.panes.pane(ws.panes.focus()).area).0
        };

        assert_eq!(
            cells(&buf, inner.x, inner.width, inner.y),
            name[..usize::from(inner.width)]
        );
    }

    /// A header names the type tag, the directory, and the bold name, then the
    /// line counts in their diff colors and the hunks in muted text. With no
    /// structural hunk the count names the hunks alone, and a count of zero
    /// takes no run.
    #[test]
    fn a_header_counts_its_lines_and_hunks_after_the_path() {
        let h = TestHarness::with_size(40, 4);
        let theme = &h.stoat.theme;
        let stats = |added, removed, hunks, structural| ExcerptStats {
            added,
            removed,
            hunks,
            structural,
        };
        let text = |path: &str, stats: ExcerptStats| -> String {
            header_runs(path, stats, theme)
                .into_iter()
                .map(|(text, _)| text)
                .collect()
        };
        let run = |text: &str, style: Style| (text.to_string(), style);

        assert_eq!(
            (
                header_runs("stoat/src/render/chrome.rs", stats(12, 15, 2, 2), theme),
                [
                    text("src/theme.rs", stats(6, 0, 1, 0)),
                    text("notes.txt", stats(0, 4, 3, 0)),
                    text("a.rs", stats(1, 1, 1, 1)),
                    text("new.txt", ExcerptStats::default()),
                ],
            ),
            (
                vec![
                    run("rs ", theme.get(scope::UI_TAG_RUST)),
                    run("stoat/src/render/", theme.get(scope::UI_TEXT_MUTED)),
                    run(
                        "chrome.rs",
                        theme.get(scope::UI_TEXT).add_modifier(Modifier::BOLD)
                    ),
                    run(" +12", theme.get(scope::DIFF_ADDED)),
                    run(" \u{2212}15", theme.get(scope::DIFF_DELETED)),
                    run(
                        " \u{b7} 2 git hunks \u{b7} 2 structural",
                        theme.get(scope::UI_TEXT_MUTED)
                    ),
                ],
                [
                    "rs src/theme.rs +6 \u{b7} 1 hunk",
                    "notes.txt \u{2212}4 \u{b7} 3 hunks",
                    "rs a.rs +1 \u{2212}1 \u{b7} 1 git hunk \u{b7} 1 structural",
                    "new.txt",
                ]
                .map(String::from),
            ),
        );
    }

    /// Under a live scene each header lies between two hairlines in the
    /// inactive border color, along its top and bottom edges, the width of the
    /// stack.
    #[test]
    fn a_header_lies_between_two_hairlines() {
        let mut h = two_files(16);
        h.stoat.render();
        let index = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).index
        };
        let hairlines: Vec<_> = frame_bars(decode_stream(
            h.stoat.apc_scene.lane_bytes(content_lane(index)),
        ))
        .into_iter()
        .filter(|bar| bar.4 == HAIRLINE)
        .collect();

        assert_eq!(hairlines, header_hairlines());
    }

    /// With no excerpt the changes view says that its list builds, or that the
    /// list holds no change.
    #[test]
    fn a_changes_view_with_no_excerpt_says_why() {
        let mut building = TestHarness::with_size(100, 16);
        building.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n")]);
        building.cover_with_diff_changes(false);

        let mut clean = TestHarness::with_size(100, 16);
        clean.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        clean.fake_git().add_repo(PathBuf::from("/repo"));
        clean.cover_with_diff_changes(false);
        clean.settle();

        assert_eq!(
            (first_row(&mut building), first_row(&mut clean)),
            ("building changes...".to_string(), "no changes".to_string()),
        );
    }

    /// A cursor on the last excerpt's last row makes the paint scroll the stack
    /// so that row is the bottom of the view.
    #[test]
    fn the_last_excerpts_last_row_scrolls_the_stack_to_its_end() {
        let mut h = two_files(4);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffChangesLast);
        h.render_composited();

        let (state, editors) = stack(&mut h);
        let total = diff_changes::layout_stack(state, editors);
        let cursor = diff_changes::cursor_stack_row(state, editors);
        assert_eq!(
            (state.focused, cursor, state.scroll + state.viewport_rows),
            (1, Some(total - 1), total),
        );
    }

    /// An excerpt whose header scrolled above the view paints from its own
    /// scroll row, so the top row shows the excerpt's row under the scroll.
    /// The numbers are absolute, so that row's number names its line.
    #[test]
    fn an_excerpt_cut_at_the_top_paints_from_its_scroll_row() {
        let mut h = TestHarness::with_size(120, 6);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Absolute);
        let base: String = (1..=8).map(|n| format!("{n}\n")).collect();
        let text: String = (1..=8).map(|n| format!("{n}!\n")).collect();
        h.stage_review_scenario("/repo", &[("a.txt", &base, &text)]);
        h.cover_with_diff_changes(false);
        h.settle();
        {
            let (state, editors) = stack(&mut h);
            let editor = &mut editors[state.excerpts[0].editor_id];
            diff_changes::place_cursor(editor, Point::new(4, 0));
            state.scroll = 3;
        }

        let buf = h.render_composited();
        let inner = Rect::new(0, 0, 120, 5);
        let cols = excerpt_columns(&h, inner, 2);
        let (state, editors) = stack(&mut h);
        let scroll_row = editors[state.excerpts[0].editor_id].scroll_row;
        assert_eq!(
            (
                state.scroll,
                scroll_row,
                gutter_number(&buf, cols.right_x, cols.right_text_x, inner.y)
            ),
            (3, 2, "3".to_string()),
        );
    }

    /// The pair names of a frame with no pair open.
    static NO_PAIRS: LazyLock<HashMap<PathBuf, (String, String)>> = LazyLock::new(HashMap::new);

    /// A frame context that paints the stack as the live frame of `stoat`
    /// paints it, under no prompt, popup, or search.
    fn live_frame(stoat: &Stoat) -> FrameCtx<'_> {
        FrameCtx {
            workspace_name: "",
            tab_bar_shown: false,
            workspace_root: Path::new(""),
            mode: "normal",
            screen: None,
            theme: &stoat.theme,
            commits_split: None,
            preview_dials: PreviewDials::from_stoat(stoat),
            diff_changes_building: false,
            chrome: &stoat
                .chrome
                .as_ref()
                .expect("a render resolved the chrome")
                .1,
            pending_count: None,
            recording_register: None,
            held_buffers: &[],
            reload_flashes: &stoat.active_workspace().reload_flashes,
            lsp_status_open: false,
            lsp_progress_entries: &[],
            spinner_phase: 0,
            lsp_servers: &[],
            diff_warm_busy: false,
            repo: None,
            hunk_position: None,
            conflict_position: None,
            diff_base: None,
            diff_sides: ("", ""),
            pair_sides: &NO_PAIRS,
            lsp_pending: None,
            lsp_message: None,
            status_message: None,
            goto_word_labels: None,
            mode_label: ("", Color::Reset),
            modal_label: None,
            diagnostics: &stoat.diagnostics,
            status_prompt: None,
            terminal_cursor_editor: stoat.focused_editor_ids().map(|(id, _)| id),
            composited_stacks: &[],
            line_numbers: LineNumbers::Relative,
            wrap_mode: WrapMode::EditorWidth,
            wrap_column: 80,
            scrolloff: stoat.settings.scrolloff.unwrap_or(3),
            inactive_dim: 0.0,
            modal_dim: 0.0,
            diff_dials: DiffDials::from_stoat(stoat),
            images_capable: false,
            batch_decorations: false,
            minimap_enabled: false,
            minimap_chrome: None,
            minimap_band: None,
            islands: false,
            backdrop_panels: false,
            badge_cover: None,
            hover_cell: None,
            home: None,
            #[cfg(feature = "perf")]
            perf: None,
        }
    }

    /// Run `draw` over the focused changes view's stack beside a frame context
    /// that paints as the live frame does, with a live scene.
    ///
    /// The context borrows the whole `Stoat`, so the editors leave the
    /// workspace for the paint and return after it.
    fn paint_directly<R>(
        h: &mut TestHarness,
        draw: impl FnOnce(
            &DiffChangesState,
            &mut SlotMap<EditorId, EditorState>,
            &BufferRegistry,
            FrameCtx<'_>,
            &mut ApcScene,
        ) -> R,
    ) -> R {
        let mut editors = mem::take(&mut h.stoat.active_workspace_mut().editors);
        let mut scene = ApcScene::new();
        scene.set_live(true);
        let out = {
            let ws = h.stoat.active_workspace();
            let id = ws
                .focused_diff_changes_id()
                .expect("the changes view is open");
            draw(
                &ws.diff_changes_views[id],
                &mut editors,
                &ws.buffers,
                live_frame(&h.stoat),
                &mut scene,
            )
        };
        h.stoat.active_workspace_mut().editors = editors;
        out
    }

    /// A run of stack rows painted on its own shows what the live paint shows
    /// on those rows. The run starts inside a.txt, so that excerpt paints from
    /// its second row, and b.txt's header falls inside the run.
    #[test]
    fn a_run_of_stack_rows_paints_what_the_live_paint_shows() {
        let mut h = two_files(16);
        let live = h.stoat.render();
        let inner = {
            let ws = h.stoat.active_workspace();
            split_pane_status(ws.panes.pane(ws.panes.focus()).area).0
        };
        let run = paint_directly(&mut h, |state, editors, buffers, frame, scene| {
            let mut run = Buffer::empty(Rect::new(0, 0, inner.width, 4));
            let area = run.area;
            let text_style = frame.theme.get(scope::UI_TEXT);
            let mut undercurls = UndercurlBatch::default();
            paint_stack_rows(
                state,
                editors,
                buffers,
                true,
                text_style,
                &mut undercurls,
                frame,
                &mut run,
                scene,
                2,
                area,
                PagePaint::Cells,
            );
            run
        });

        let rows = |buf: &Buffer, x: u16, y: u16| -> Vec<String> {
            (y..y + 4).map(|y| cells(buf, x, inner.width, y)).collect()
        };
        let expected = rows(&live, inner.x, inner.y + 2);
        assert_eq!(
            (rows(&run, 0, 0), expected[2].as_str()),
            (expected.clone(), "b.txt +1 \u{2212}1 \u{b7} 1 hunk"),
        );
    }

    /// Every excerpt numbers its live column by the steps from the focused
    /// cursor, so the excerpt below counts on past the focused excerpt's last
    /// row. The frame before the keys leaves a capture that the frames after
    /// them do not replay.
    #[test]
    fn every_excerpt_counts_the_steps_from_the_focused_cursor() {
        let mut h = numbered_files();
        focus_at(&mut h, 0, Point::new(0, 0));
        h.render_composited();
        h.type_keys("j j");

        let buf = h.render_composited();
        assert_eq!(
            excerpt_numbers(&mut h, &buf),
            [["2", "1", "3", "1", "2"], ["3", "4", "5", "6", "7"]]
        );
    }

    /// An excerpt above the focused one counts the steps up to the cursor.
    #[test]
    fn an_excerpt_above_the_cursor_counts_up_to_it() {
        let mut h = numbered_files();
        focus_at(&mut h, 1, Point::new(1, 0));
        let buf = h.render_composited();
        assert_eq!(
            excerpt_numbers(&mut h, &buf),
            [["6", "5", "4", "3", "2"], ["1", "2", "1", "2", "3"]]
        );
    }

    /// Each number is the count that `j` or `k` takes to reach its row from
    /// the cursor, across fold rows, removed base rows, and excerpt headers.
    /// A fold row takes a step and shows no number.
    ///
    /// a.txt changes its first line and folds the lines past its fourth into
    /// a fold row. b.txt changes line 15 and folds lines 1 to 11 into a fold
    /// row, where the cursor sits, and lines 19 and 20 into another. c.txt
    /// drops line 3, which leaves a removed base row and no fold.
    #[test]
    fn a_number_is_the_count_that_reaches_its_row() {
        let twenty: String = (1..=20).map(|n| format!("{n}\n")).collect();
        let six: String = (1..=6).map(|n| format!("{n}\n")).collect();
        let mut h = changes_view(
            120,
            30,
            &[
                ("a.txt", &twenty, &twenty.replacen("1\n", "ONE\n", 1)),
                ("b.txt", &twenty, &twenty.replace("\n15\n", "\nFIFTEEN\n")),
                ("c.txt", &six, &six.replace("\n3\n", "\n")),
            ],
        );
        // The stack row of the cursor, as of the last paint's layout.
        let home = |h: &mut TestHarness| {
            focus_at(h, 1, Point::new(0, 0));
            let (state, editors) = stack(h);
            diff_changes::cursor_stack_row(state, editors).expect("b.txt shows its rows")
        };
        home(&mut h);
        let buf = h.render_composited();
        let numbers = excerpt_numbers(&mut h, &buf);
        assert_eq!(
            numbers,
            vec![
                vec!["5", "4", "3", "2", ""],
                vec!["", "1", "2", "3", "4", "5", "6", "7", ""],
                vec!["9", "10", "", "11", "12", "13", "14"],
            ]
        );

        let cursor = home(&mut h);
        let tops: Vec<usize> = stack(&mut h)
            .0
            .excerpts
            .iter()
            .map(|excerpt| excerpt.top)
            .collect();
        let (mut targets, mut reached) = (Vec::new(), Vec::new());
        for (top, rows) in tops.into_iter().zip(&numbers) {
            for (offset, number) in rows.iter().enumerate() {
                let row = top + 1 + offset;
                if row == cursor || number.is_empty() {
                    continue;
                }
                let digits: Vec<String> = number.chars().map(String::from).collect();
                let key = if row < cursor { "k" } else { "j" };
                home(&mut h);
                h.type_keys(&format!("{} {key}", digits.join(" ")));
                let (state, editors) = stack(&mut h);
                targets.push(Some(row));
                reached.push(diff_changes::cursor_stack_row(state, editors));
            }
        }
        assert_eq!(reached, targets, "the stack row each count's keys reach");
    }

    /// While the window is in the background every excerpt numbers its rows
    /// by their own lines, and the steps from the cursor come back with the
    /// focus.
    #[test]
    fn an_unfocused_window_numbers_each_excerpt_by_its_own_lines() {
        let mut h = numbered_files();
        focus_at(&mut h, 0, Point::new(2, 0));
        h.stoat.update(Event::FocusLost);
        let buf = h.render_composited();
        let background = excerpt_numbers(&mut h, &buf);
        h.stoat.update(Event::FocusGained);
        let buf = h.render_composited();
        assert_eq!(
            [background, excerpt_numbers(&mut h, &buf)],
            [
                [["1", "2", "3", "4", "5"], ["1", "2", "3", "4", "5"]],
                [["2", "1", "3", "1", "2"], ["3", "4", "5", "6", "7"]],
            ],
            "the numbers in the background, then with the focus back",
        );
    }

    /// A changes view that a dock took focus from numbers each excerpt by its
    /// own lines, as an unfocused per-file diff does.
    #[test]
    fn an_unfocused_changes_view_numbers_each_excerpt_by_its_own_lines() {
        let mut h = numbered_files();
        focus_at(&mut h, 0, Point::new(2, 0));
        focus_hidden_dock(&mut h);
        let buf = h.render_composited();
        h.stoat.active_workspace_mut().focus = FocusTarget::SplitPane;
        assert_eq!(
            excerpt_numbers(&mut h, &buf),
            [["1", "2", "3", "4", "5"], ["1", "2", "3", "4", "5"]]
        );
    }

    /// Give the focus to a hidden dock, so the changes view's pane keeps the
    /// window's focus and loses its own.
    fn focus_hidden_dock(h: &mut TestHarness) {
        let ws = h.stoat.active_workspace_mut();
        let dock = ws.docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Hidden,
            default_width: 30,
            area: Default::default(),
        });
        ws.focus = FocusTarget::Dock(dock);
    }

    /// Two four-line files that each change their second line, under a
    /// focused changes view. The context rows cover each whole file, so no
    /// fold row joins the numbers, and each excerpt shows five rows.
    fn numbered_files() -> TestHarness {
        let (base, text) = ("1\n2\n3\n4\n", "1\nTWO\n3\n4\n");
        changes_view(120, 24, &[("a.txt", base, text), ("b.txt", base, text)])
    }

    /// Focus excerpt `index` of the changes view, with its cursor at `point`.
    fn focus_at(h: &mut TestHarness, index: usize, point: Point) {
        let (state, editors) = stack(h);
        state.focused = index;
        diff_changes::place_cursor(&mut editors[state.excerpts[index].editor_id], point);
    }

    /// The buffer-column number on each row of each excerpt in `buf`, in stack
    /// order, for a stack that starts at the window's top.
    fn excerpt_numbers(h: &mut TestHarness, buf: &Buffer) -> Vec<Vec<String>> {
        let inner = {
            let ws = h.stoat.active_workspace();
            split_pane_status(ws.panes.pane(ws.panes.focus()).area).0
        };
        let cols = excerpt_columns(h, inner, 2);
        let (state, editors) = stack(h);
        state
            .excerpts
            .iter()
            .map(|excerpt| {
                let rows = diff_changes::excerpt_rows(excerpt, editors);
                (1..=rows)
                    .map(|row| {
                        let y = inner.y + (excerpt.top + row) as u16;
                        gutter_number(buf, cols.right_x, cols.right_text_x, y)
                    })
                    .collect()
            })
            .collect()
    }

    /// A page painted after the live frame leaves each excerpt's editor laid
    /// out as the live paint left it, so the terminal cursor and pointer hits
    /// stay on the live rows. The page starts at a.txt's second row, where the
    /// paint lays a.txt out from another scroll row and cuts its cursor's row.
    #[test]
    fn a_stack_page_leaves_the_live_layout() {
        let mut h = two_files(16);
        h.stoat.render();
        let layouts = |h: &mut TestHarness| {
            let (state, editors) = stack(h);
            state
                .excerpts
                .iter()
                .map(|excerpt| {
                    let editor = &editors[excerpt.editor_id];
                    (
                        editor.scroll_row,
                        editor.viewport_rows,
                        editor.cursor_screen_cell,
                        editor.text_rect,
                        editor.minimap_rect,
                    )
                })
                .collect::<Vec<_>>()
        };
        let live = layouts(&mut h);

        let header = paint_directly(&mut h, |state, editors, buffers, frame, scene| {
            let mut page = Buffer::empty(Rect::new(0, 0, 120, 4));
            let text_style = frame.theme.get(scope::UI_TEXT);
            let mut undercurls = UndercurlBatch::default();
            paint_stack_page(
                state,
                editors,
                buffers,
                true,
                text_style,
                &mut undercurls,
                frame,
                &mut page,
                scene,
                2,
                PagePaint::Cells,
            );
            cells(&page, 0, 120, 2)
        });

        assert_eq!(
            (live[0].2.is_some(), header.as_str(), layouts(&mut h)),
            (true, "b.txt +1 \u{2212}1 \u{b7} 1 hunk", live.clone()),
            "the live cursor cell, the page's header row, and every layout field",
        );
    }

    /// A decorations-only page paint lays the scene that a full paint of the
    /// page lays, and writes no cell. The numbers and bars of both columns
    /// therefore survive a refill that sends the decorations alone. The files
    /// give the page paired, mirrored, and block rows.
    #[test]
    fn a_decorations_only_page_lays_the_full_scene_and_no_cell() {
        let mut h = changes_view(
            120,
            24,
            &[
                ("a.txt", "a\nx\n", "A\nx\n"),
                ("b.txt", "b\nd\ne\n", "b\ne\n"),
            ],
        );
        h.stoat.render();
        let mut paint = |paint: PagePaint| {
            paint_directly(&mut h, |state, editors, buffers, frame, scene| {
                let mut page = Buffer::empty(Rect::new(0, 0, 120, 12));
                let text_style = frame.theme.get(scope::UI_TEXT);
                paint_stack_page(
                    state,
                    editors,
                    buffers,
                    true,
                    text_style,
                    &mut UndercurlBatch::default(),
                    frame,
                    &mut page,
                    scene,
                    0,
                    paint,
                );
                (page, scene.bytes().to_vec())
            })
        };
        let (full_page, full_scene) = paint(PagePaint::Cells);
        let (page, scene) = paint(PagePaint::DecorationsOnly);

        assert_eq!(
            (
                full_scene.is_empty(),
                scene == full_scene,
                page == Buffer::empty(page.area),
                page == full_page,
            ),
            (false, true, true, false),
            "an empty full scene, equal scenes, a blank page, and a page like the full one",
        );
    }

    /// A split changes view frames its content once around the base column, in
    /// the base tint. The top edge leads the content, the header hairlines
    /// follow, and the sides cross over them. No excerpt adds a bar of that
    /// tint.
    #[test]
    fn a_split_changes_view_frames_its_content_once() {
        let mut h = changes_view(
            120,
            24,
            &[
                ("a.txt", "a\nx\n", "A\nx\n"),
                ("b.txt", "b\nd\ne\n", "b\ne\n"),
            ],
        );
        h.stoat.render();
        let index = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).index
        };
        let bars = frame_bars(decode_stream(
            h.stoat.apc_scene.lane_bytes(content_lane(index)),
        ));

        // The stack spans the 120 columns and 23 rows above the status row, its
        // headers lie on rows 0 and 4, and the divider's hairline sits 952
        // sixteenths in.
        assert_eq!(
            (
                bars[..7].to_vec(),
                bars.iter().filter(|bar| bar.4 == TINT).count()
            ),
            (
                [
                    vec![(0, -3, 952, 3, TINT)],
                    header_hairlines(),
                    vec![(0, 0, 3, 368, TINT), (949, 0, 3, 368, TINT)],
                ]
                .concat(),
                3,
            ),
        );
    }

    /// A stack page draws the two sides of the frame around the base column,
    /// the height of the page, after the header hairlines, so the sides cross
    /// over them. The page draws no top.
    #[test]
    fn a_stack_page_draws_the_frame_s_sides_over_the_header_hairlines() {
        let mut h = changes_view(
            120,
            24,
            &[
                ("a.txt", "a\nx\n", "A\nx\n"),
                ("b.txt", "b\nd\ne\n", "b\ne\n"),
            ],
        );
        h.stoat.render();
        let commands = paint_directly(&mut h, |state, editors, buffers, frame, scene| {
            let mut page = Buffer::empty(Rect::new(0, 0, 120, 12));
            let text_style = frame.theme.get(scope::UI_TEXT);
            paint_stack_page(
                state,
                editors,
                buffers,
                true,
                text_style,
                &mut UndercurlBatch::default(),
                frame,
                &mut page,
                scene,
                0,
                PagePaint::Cells,
            );
            decode_stream(scene.bytes())
        });
        let bars = frame_bars(commands);

        assert_eq!(
            (
                bars[..6].to_vec(),
                bars.iter().filter(|bar| bar.4 == TINT).count()
            ),
            (
                [
                    header_hairlines(),
                    vec![(0, 0, 3, 192, TINT), (949, 0, 3, 192, TINT)],
                ]
                .concat(),
                2,
            ),
        );
    }

    /// The hairlines along the top and bottom edges of the headers on rows 0
    /// and 4 of a stack 120 columns wide.
    fn header_hairlines() -> Vec<(i16, i16, u16, u16, [u8; 3])> {
        [0, 15, 64, 79].map(|y| (0, y, 1920, 1, HAIRLINE)).to_vec()
    }

    /// Every bar `commands` draws, in draw order, as `(x, y, width, height,
    /// color)` in sixteenths, whether the scene sends it alone or in a batch.
    fn frame_bars(commands: Vec<Command>) -> Vec<(i16, i16, u16, u16, [u8; 3])> {
        commands
            .into_iter()
            .flat_map(|command| match command {
                Command::Bar(bar) => vec![(bar.x, bar.y, bar.width, bar.height, bar.color)],
                Command::Bars(group) => group
                    .bars
                    .into_iter()
                    .map(|(x, y, width, height)| (x, y, width, height, group.color))
                    .collect(),
                _ => Vec::new(),
            })
            .collect()
    }

    /// Two staged files under a focused changes view at 120 by 24, with the
    /// minimap in `mode`, on a stoatty of protocol 13.
    fn strip_harness(mode: MinimapMode) -> (TestHarness, UnboundedReceiver<Vec<u8>>) {
        let mut h = TestHarness::with_size(120, 24);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        h.stoat.set_apc_tx(tx);
        h.stoat.stoatty_protocol = 13;
        h.stoat.settings.editor_minimap = Some(mode);
        h.stage_review_scenario("/repo", &[("a.txt", "a\n", "A\n"), ("b.txt", "b\n", "B\n")]);
        h.open_file(Path::new("/repo/a.txt"));
        h.cover_with_diff_changes(false);
        h.settle();
        (h, rx)
    }

    /// Paint one frame, and return its cells, the strip rect the focused
    /// changes view reserved, and every minimap strip the frame declared.
    fn paint_strips(
        h: &mut TestHarness,
        rx: &mut UnboundedReceiver<Vec<u8>>,
    ) -> (Buffer, Option<Rect>, Vec<Declared>) {
        let buf = h.stoat.render();
        apc_emit::emit_apc_scene(&mut h.stoat);
        let declared = drain_apc(rx)
            .into_iter()
            .filter_map(|command| match command {
                Command::Minimap(strip) => Some((strip.strip_id, strip.content_id, strip.palette)),
                _ => None,
            })
            .collect();
        let reserved = h
            .stoat
            .active_workspace()
            .focused_diff_changes()
            .and_then(|state| state.minimap_rect);
        (buf, reserved, declared)
    }

    /// The workspace's changes view strip as `strip_id` declares it, the
    /// scroll overview of the class table's palette, blended toward the
    /// background by `dim`.
    fn diff_changes_strip(h: &TestHarness, strip_id: u32, dim: f32) -> Declared {
        let bg = paint::style_rgb(h.stoat.theme.get(scope::UI_BACKGROUND).bg)
            .expect("the test theme has an rgb background");
        let table = &h.stoat.minimap_class_table;
        let palette: Vec<_> = table
            .palette()
            .iter()
            .map(|&color| paint::dim_rgb(color, bg, dim))
            .collect();
        let ground = paint::dim_rgb(minimap::color_to_rgb(themed_bg(&h.stoat.theme)), bg, dim);
        let content_id = h.stoat.diff_changes_strips[&h.stoat.active_workspace].content_id();
        (
            strip_id,
            content_id,
            review::diff_overview_palette(table, &palette, ground),
        )
    }

    fn focused_pane_index(h: &TestHarness) -> u32 {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).index
    }

    /// A per-pane changes view gives the cell at its right edge to the scroll
    /// overview, leaves that cell blank, and declares the overview.
    #[test]
    fn a_per_pane_changes_view_declares_its_strip_as_an_overview() {
        let (mut h, mut rx) = strip_harness(MinimapMode::PerPane);
        let (buf, reserved, declared) = paint_strips(&mut h, &mut rx);
        let strip = Rect::new(119, 0, 1, 23);
        let blank = surface_blank(&h.stoat.theme);
        let painted: Vec<_> = strip
            .positions()
            .filter(|&cell| buf[cell] != blank)
            .collect();
        assert_eq!(
            (reserved, painted, declared),
            (
                Some(strip),
                vec![],
                vec![diff_changes_strip(&h, focused_pane_index(&h), 0.0)]
            ),
        );
    }

    /// A per-pane changes view that a dock took focus from declares its
    /// overview at the inactive dim.
    #[test]
    fn an_unfocused_per_pane_changes_view_dims_its_strip() {
        let (mut h, mut rx) = strip_harness(MinimapMode::PerPane);
        let index = focused_pane_index(&h);
        focus_hidden_dock(&mut h);

        let (_, _, declared) = paint_strips(&mut h, &mut rx);
        let dim = h.stoat.painted_inactive_dim();
        assert_eq!(declared, [diff_changes_strip(&h, index, dim)]);
    }

    /// The single band narrows to one cell over a focused changes view and
    /// shows its overview, and the pane reserves no strip of its own.
    #[test]
    fn the_single_band_follows_a_focused_changes_view() {
        let (mut h, mut rx) = strip_harness(MinimapMode::Single);
        let (_, reserved, declared) = paint_strips(&mut h, &mut rx);
        assert_eq!(
            (
                reserved,
                h.stoat.single_minimap_rect.map(|band| band.width),
                declared
            ),
            (
                None,
                Some(1),
                vec![diff_changes_strip(&h, SINGLE_MINIMAP_STRIP_ID, 0.0)]
            ),
        );
    }

    /// The single band keeps its text width over a changes view that is not
    /// the widened pane, and shows the overview across that width.
    #[test]
    fn the_single_band_keeps_its_width_over_an_unwidened_changes_view() {
        let (mut h, mut rx) = strip_harness(MinimapMode::Single);
        h.stoat.active_workspace_mut().panes.unwiden();
        let (_, _, declared) = paint_strips(&mut h, &mut rx);
        assert_eq!(
            (h.stoat.single_minimap_rect.map(|band| band.width), declared),
            (
                Some(7),
                vec![diff_changes_strip(&h, SINGLE_MINIMAP_STRIP_ID, 0.0)]
            ),
        );
    }

    /// A dial step sends no band over a changes view. The overview keeps only
    /// the row marks, which no dial moves.
    #[test]
    fn a_dial_step_leaves_the_single_band_over_a_changes_view() {
        let (mut h, mut rx) = strip_harness(MinimapMode::Single);
        let (_, _, before) = paint_strips(&mut h, &mut rx);
        h.stoat.diff_tint = 2;
        let (_, _, stepped) = paint_strips(&mut h, &mut rx);
        assert_eq!(
            (before, stepped),
            (
                vec![diff_changes_strip(&h, SINGLE_MINIMAP_STRIP_ID, 0.0)],
                vec![]
            ),
        );
    }
}
