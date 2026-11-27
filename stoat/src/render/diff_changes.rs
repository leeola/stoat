//! The paint of a [`View::DiffChanges`](crate::pane::View::DiffChanges) pane.
//!
//! The changes view paints its excerpts one under another through the
//! per-file diff's painter, so a hunk reads the same here as in the per-file
//! diff. A header row names each excerpt's file right of the diff's divide and
//! counts its diff left of it.
//! The focused excerpt paints the cursor where the changes view's keys act.
//!
//! Under relative numbering every excerpt numbers its rows by the `j` or `k`
//! steps that reach them from the focused excerpt's cursor, so a number is
//! the count to type before the key.

use crate::{
    buffer_registry::BufferRegistry,
    diff_changes::{self, DiffChangesState, Excerpt, ExcerptStats},
    editor_state::{EditorId, EditorState},
    host::git::{DeletedBy, FileKind},
    pane::Pane,
    render::{
        chrome,
        editor::{fit_diff_view, render_editor_with_overlay, screen_cell, split_minimap_strip},
        paint::style_rgb,
        review,
        text::{text_width, write_str, write_str_clipped},
        undercurl::UndercurlBatch,
        FrameCtx, TypeScale,
    },
    theme::{scope, Theme, TypeRole},
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::StatefulWidget,
};
use slotmap::SlotMap;
use std::path::Path;
use stoat_config::LineNumbers;
use stoat_widgets::{
    bar::{Bar, BarBatch},
    text_run::{self, TextRun, TextRunBatch},
    ApcScene,
};
use stoatty_protocol::command::PanelShadow;

/// The sixteenths down a header row at which its two hairlines lie, the row's
/// top and bottom edges.
const HAIRLINE_Y: [i16; 2] = [0, 15];

/// The sixteenths between a File Header's text and the divide's hairline, or
/// the row's edge, the diff boards' `padding: 0 10px` on each half.
const HEADER_PAD: u16 = 20;

/// The sixteenths between two of a File Header's counts, the diff boards'
/// `gap: 8px`.
const HEADER_COUNT_GAP: u16 = 16;

/// The sixteenths between a File Kind Tag's box and its text on each side, the
/// DiffKindsSpec board's `padding: 0 5px`.
const TAG_INSET: u16 = 10;

/// The height of a File Kind Tag's box, in sixteenths of a row, one in from the
/// row's top, so the header's two hairlines stay clear of it.
const TAG_HEIGHT: u16 = 14;

/// The radius a File Kind Tag's box rounds its corners at, in logical pixels,
/// the board's `border-radius: 4px`.
const TAG_RADIUS: u8 = 4;

/// How much of a page of the changes view's pool a paint covers.
///
/// The terminal keeps a page's cells and replaces its decorations as one set.
/// A step that moves only what the decorations show, such as the line that
/// relative numbers count from, sends new decorations over the cells the
/// terminal keeps, so its paint skips the text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PagePaint {
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
    ghost_cursor_cell: Option<(u16, u16)>,
    text_rect: Option<Rect>,
    minimap_rect: Option<Rect>,
}

impl PaintLayout {
    fn of(editor: &EditorState) -> Self {
        Self {
            scroll_row: editor.scroll_row,
            viewport_rows: editor.viewport_rows,
            cursor_screen_cell: editor.cursor_screen_cell,
            ghost_cursor_cell: editor.ghost_cursor_cell,
            text_rect: editor.text_rect,
            minimap_rect: editor.minimap_rect,
        }
    }

    fn restore(self, editor: &mut EditorState) {
        editor.scroll_row = self.scroll_row;
        editor.viewport_rows = self.viewport_rows;
        editor.cursor_screen_cell = self.cursor_screen_cell;
        editor.ghost_cursor_cell = self.ghost_cursor_cell;
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
/// With the stack's top edge inside an excerpt, the first row repeats that
/// excerpt's header while the stack scrolls under it, as
/// [`diff_changes::pinned_header`] decides. The cursor's follow keeps the
/// cursor clear of that row.
///
/// The focused excerpt records the cell of its cursor in
/// [`EditorState::cursor_screen_cell`] when the terminal draws that cursor.
/// The record is `None` when the paint does not lay the excerpt out, such as
/// an excerpt outside the window, and for a cursor under the pinned row.
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
    let inner = pane.area;
    let (inner, strip) = split_minimap_strip(inner, frame.minimap_enabled);
    state.minimap_rect = strip;
    state.content_rect = Some(inner);
    if let Some(editor) = state
        .focused_excerpt()
        .and_then(|excerpt| editors.get_mut(excerpt.editor_id))
    {
        editor.cursor_screen_cell = None;
        editor.ghost_cursor_cell = None;
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
    diff_changes::apply_stack_hold(state, editors, total);
    let cursor = diff_changes::focused_cursor(state, editors);
    if let Some(row) = diff_changes::cursor_stack_row(state, editors) {
        match state.held_screen_row.take() {
            Some(screen_row) => {
                let last = state.viewport_rows.saturating_sub(1);
                state.scroll = row.saturating_sub(usize::from(screen_row).min(last));
                state.uncover_row(editors, row);
            },
            None if cursor != state.followed_cursor => {
                state.ensure_row_visible(editors, row, state.viewport_rows);
            },
            None => {},
        }
        state.followed_cursor = cursor;
    }
    state.scroll = state.scroll.min(total.saturating_sub(state.viewport_rows));

    let first_row = state.scroll;
    // The stack row under a pinned header paints nothing. If it painted, its
    // runs and bars show over the header's cells on a frame that no pool
    // composites, since the cover draws no fill. A cursor there is off screen,
    // as one past the view's edge is.
    let (rows_from, rows) = pinned_split(state, editors, inner);
    let mut covered = stack_composited(pane, frame).then(ApcScene::new);
    paint_stack_rows(
        state,
        editors,
        buffers,
        is_focused,
        text_style,
        undercurls,
        frame,
        buf,
        covered.as_mut().unwrap_or(&mut *scene),
        rows_from,
        rows,
        PagePaint::Cells,
    );
    paint_pinned_header(
        state, editors, buffers, frame, text_style, inner, buf, scene,
    );

    // The pages carry no cursor, so the ghost beside it goes to the pane's own
    // scene even while the composite covers the live rows. Its panel leaves the
    // cell to the live grid at rest and rides the stack's glide.
    let ghost = state
        .focused_excerpt()
        .and_then(|excerpt| editors.get(excerpt.editor_id))
        .and_then(|editor| editor.ghost_cursor_cell)
        .filter(|_| scene.live());
    if let Some(cell) = ghost {
        review::emit_ghost_cursor(cell, (pane.index, first_row as f32), frame.theme, scene);
    }
}

/// Whether the terminal composites the resident pool of `pane`'s stack over
/// the rect the stack paints into.
///
/// The rect is the pane's content area less its minimap strip, the rect that
/// [`diff_changes_pool_panes`](crate::apc_emit::diff_changes_pool_panes)
/// declares. A pool that the terminal does not hold yet, or holds at another
/// rect, covers no part of the paint.
pub(crate) fn stack_composited(pane: &Pane, frame: FrameCtx<'_>) -> bool {
    let inner = pane.area;
    let (inner, _) = split_minimap_strip(inner, frame.minimap_enabled);
    frame.composited_stacks.contains(&(pane.index, inner))
}

/// Paint stack rows `first_row..first_row + area.height` of the stack `state`
/// holds into `area` of `buf`.
///
/// Each excerpt the rows reach takes a header row and then its rows. The header
/// names the file and counts its diff on the raised surface, as
/// [`paint_header`] lays them out. Under a live scene a hairline in the
/// inactive border color runs along the header's top and bottom edges, and the
/// divide's hairline runs through it. The rows paint from the excerpt's own
/// scroll row, so an excerpt cut at the top of `area` shows its lower rows. A
/// painted excerpt's editor records where this paint laid it out: its scroll
/// row, its viewport, its text rect, and the cell of its cursor.
///
/// The first row of a file with no base reads `no base` on its base half, as
/// [`paint_no_base`] lays it out. An empty file with no base shows one note row
/// under its header in place of rows, as [`review::paint_note_row`] paints it.
///
/// Under [`PagePaint::DecorationsOnly`] the rows write no cell. The headers'
/// hairlines and runs, the `no base` label, and the excerpts' diff gutters
/// fill `scene` alone.
#[allow(clippy::too_many_arguments)]
pub fn paint_stack_rows(
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

    let headers = state
        .excerpts
        .iter()
        .filter_map(|excerpt| header_row(excerpt.top));
    emit_header_hairlines(headers, true, frame.theme, scene);

    let origins =
        (is_focused && frame.line_numbers == LineNumbers::Relative && frame.mode != "insert")
            .then(|| diff_changes::step_origins(state, editors))
            .flatten();
    for (index, excerpt) in state.excerpts.iter().enumerate() {
        let rows = diff_changes::excerpt_rows(excerpt, editors);
        let top = excerpt.top;
        let header = diff_changes::header_rows(excerpt);
        if top + header + rows <= scroll || top >= scroll + rows_on_screen {
            continue;
        }
        // A page replaces its decorations as one set, so a decorations-only
        // page emits the header's runs too.
        if let Some(row) = header_row(top) {
            let (left, hidden) = excerpt_header(excerpt, buffers, frame);
            paint_header(
                buf,
                row,
                &header_path(&excerpt.rel_path, frame.theme),
                &left,
                hidden,
                frame.theme,
                match scene.live() {
                    true => Some(&mut *scene),
                    false => None,
                },
                paint == PagePaint::Cells,
            );
        }
        if let Some(scope) = excerpt.shown_scope()
            && let Some(row) = header_row(top + 1)
        {
            review::paint_scope_row(
                scope,
                row,
                None,
                frame.theme,
                text_style,
                buf,
                match scene.live() {
                    true => Some(&mut *scene),
                    false => None,
                },
                paint == PagePaint::DecorationsOnly,
            );
        }
        if excerpt.empty
            && !excerpt.collapsed
            && let Some(row) = header_row(top + 1)
        {
            review::paint_note_row(
                "no base",
                "empty \u{b7} 0 lines",
                row,
                frame.theme,
                text_style,
                buf,
                match scene.live() {
                    true => Some(&mut *scene),
                    false => None,
                },
                paint == PagePaint::DecorationsOnly,
            );
        }

        // The screen row of the excerpt's first row, past its header rows.
        let first = (top + header).saturating_sub(scroll);
        let Some(editor) = editors.get_mut(excerpt.editor_id) else {
            continue;
        };
        if rows == 0 || first >= rows_on_screen {
            continue;
        }
        editor.scroll_row = scroll.saturating_sub(top + header) as u32;
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
            None,
            false,
        );
        if editor.scroll_row == 0
            && diff_changes::excerpt_no_base(excerpt, buffers)
            && let Some(divider) = review::diff_divider_x(excerpt_area)
        {
            paint_no_base(excerpt_area, divider, frame.theme, buf, scene, paint);
        }
        editor.viewport_rows = Some(rows as u32);
    }
}

/// Write `no base` on the first row of `area`, an excerpt of a file with no
/// base, centered in the base half between the area's left edge and the
/// divide's hairline in column `divider`, in `ui.text.muted`.
///
/// Under a live scene the label is a run at the caption scale, which a page
/// sends with its decorations, so both page paints send it. It goes into the
/// scene after the base half's hatch, so it draws over the hatch. With no live
/// scene a cells paint writes it into the row's cells over their own style.
fn paint_no_base(
    area: Rect,
    divider: u16,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
    paint: PagePaint,
) {
    const LABEL: &str = "no base";
    let muted = theme.get(scope::UI_TEXT_MUTED);
    match style_rgb(muted.fg).filter(|_| scene.live()) {
        Some(color) => {
            let scale = theme.type_scale(TypeRole::Caption);
            let divide = (divider - area.x) * 16 + review::DIVIDER_RULE_X;
            let advance = text_run::advance_sixteenths(LABEL.chars().count(), scale);
            let mut runs = TextRunBatch::default();
            runs.push(
                area,
                TextRun {
                    col: (divide.saturating_sub(advance) / 2) as i16,
                    row: 0,
                    scale,
                    color,
                    bg: None,
                    text: LABEL,
                    bold: false,
                    italic: false,
                    follow: 0,
                    anchor: None,
                },
            );
            runs.flush(scene);
        },
        None if paint == PagePaint::Cells => {
            let half = divider - area.x;
            let x = area.x + half.saturating_sub(LABEL.len() as u16) / 2;
            for (offset, ch) in LABEL.chars().enumerate() {
                let cell = &mut buf[(x + offset as u16, area.y)];
                let style = cell.style().patch(muted);
                cell.set_char(ch).set_style(style);
            }
        },
        None => {},
    }
}

/// The first stack row the stack's rows paint from, and the part of `inner`
/// they paint into, which start under the rows that pin at the top of the
/// view.
fn pinned_split(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    inner: Rect,
) -> (usize, Rect) {
    let pinned = diff_changes::pinned_header(state, editors, state.scroll).rows();
    let height = inner.height.saturating_sub(pinned as u16);
    let rows = Rect {
        y: inner.y + inner.height - height,
        height,
        ..inner
    };
    (state.scroll + pinned, rows)
}

/// Repeat the header of the excerpt under the stack's top edge on the first row
/// of `inner`, and the Scope of the hunk under that row on the second, which
/// hold while the stack scrolls under them.
///
/// The rows first declare a panel whose body draws nothing, so a pool's
/// composite leaves them to the live grid. The panel casts a short shadow on
/// the rows that scroll under it. The header's cells then fill the first row.
/// The Scope paints as a fold row with its Scope alone past the divide, with no
/// Fold Button, no count, and no range. A view that pins nothing, as
/// [`diff_changes::pinned_header`] decides, paints nothing here.
#[allow(clippy::too_many_arguments)]
fn paint_pinned_header(
    state: &DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    buffers: &BufferRegistry,
    frame: FrameCtx<'_>,
    text_style: Style,
    inner: Rect,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) {
    let pinned = diff_changes::pinned_header(state, editors, state.scroll);
    let Some(index) = pinned.file else {
        return;
    };
    let excerpt = &state.excerpts[index];
    // The cover spans the pinned rows alone, so its cast shadow falls from the
    // last pinned row onto the rows that scroll under it.
    let cover = Rect {
        height: pinned.rows() as u16,
        ..inner
    };
    chrome::cover_cells(cover, false, PanelShadow::Cast, scene);

    let row = Rect { height: 1, ..inner };
    let (left, hidden) = excerpt_header(excerpt, buffers, frame);
    paint_header(
        buf,
        row,
        &header_path(&excerpt.rel_path, frame.theme),
        &left,
        hidden,
        frame.theme,
        match scene.live() {
            true => Some(&mut *scene),
            false => None,
        },
        true,
    );

    // A resting pool's composite still paints the top and bottom pixel lines
    // of the cover's rect, so a plain bar there lies under the stack's rows.
    // An occluding bar holds over the composite.
    if let Some(color) = style_rgb(frame.theme.get(scope::UI_BORDER_INACTIVE).fg) {
        for y in HAIRLINE_Y {
            Bar {
                x: 0,
                y,
                width: row.width * 16,
                height: 1,
                color,
                radius: 0,
                corners: 0,
            }
            .render_occluding(row, scene);
        }
        if let Some(divide) = header_divide(row) {
            divide_bar(divide, color).render_occluding(row, scene);
        }
    }

    let Some(pinned) = pinned.scope else {
        return;
    };
    let row = Rect {
        y: inner.y + 1,
        ..row
    };
    review::paint_scope_row(
        &pinned.scope,
        row,
        None,
        frame.theme,
        text_style,
        buf,
        match scene.live() {
            true => Some(&mut *scene),
            false => None,
        },
        false,
    );
}

/// Emit a hairline along the top and the bottom edge of each header row in
/// `rows`, in the inactive border color.
///
/// With `split`, the rows head a split diff, and the divide's hairline runs
/// through each row in the same color, at the place the diff body below draws
/// its divider.
pub(crate) fn emit_header_hairlines(
    rows: impl Iterator<Item = Rect>,
    split: bool,
    theme: &Theme,
    scene: &mut ApcScene,
) {
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
                    radius: 0,
                    corners: 0,
                },
            );
        }
        if let Some(divide) = header_divide(row).filter(|_| split) {
            bars.push(row, divide_bar(divide, color));
        }
    }
    bars.flush(scene);
}

/// The divide's hairline in a header row of the split diff, in sixteenths from
/// the row's left, or `None` for a row too narrow for the divide.
fn header_divide(row: Rect) -> Option<u16> {
    review::diff_divider_x(row).map(|divider| (divider - row.x) * 16 + review::DIVIDER_RULE_X)
}

/// The bar that draws the divide's hairline at `x` sixteenths across a header
/// row, in `color`.
fn divide_bar(x: u16, color: [u8; 3]) -> Bar {
    Bar {
        x: x as i16,
        y: 0,
        width: 1,
        height: 16,
        color,
        radius: 0,
        corners: 0,
    }
}

/// The runs that name the file at `rel_path` in a File Header, each in the
/// style it paints in.
///
/// The runs read the file's directory muted, its name bold, and its extension
/// bold in the type's color.
pub fn header_path(rel_path: &str, theme: &Theme) -> Vec<(String, Style)> {
    path_runs(rel_path, theme.get(scope::UI_TEXT), theme)
}

/// The runs of `rel_path` as [`header_path`] reads them, with the file's name
/// bold in `name`'s colors.
fn path_runs(rel_path: &str, name: Style, theme: &Theme) -> Vec<(String, Style)> {
    let muted = theme.get(scope::UI_TEXT_MUTED);
    let bold = name.add_modifier(Modifier::BOLD);
    let (dir, name) = rel_path.split_at(rel_path.rfind('/').map_or(0, |slash| slash + 1));
    let mut runs = Vec::new();
    if !dir.is_empty() {
        runs.push((dir.to_string(), muted));
    }
    match chrome::tagged_name(Path::new(name)) {
        Some(tagged) => {
            let suffix = theme.get(tagged.tag.scope).add_modifier(Modifier::BOLD);
            runs.push((tagged.stem, bold));
            runs.push((tagged.suffix, suffix));
        },
        None => runs.push((name.to_string(), bold)),
    }
    runs
}

/// The runs that count a File Header's diff from `stats`, each in the style it
/// paints in.
///
/// The runs count the lines the diff adds and removes, and then its hunks. A
/// count of zero takes no run. Where the tree pass refined a hunk, the count
/// names the git hunks and then the structural ones. With no structural count
/// to tell them from, the count names the hunks alone.
pub fn header_counts(stats: ExcerptStats, theme: &Theme) -> Vec<(String, Style)> {
    let mut runs = line_counts(stats, theme);
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
        runs.push((format!("\u{b7} {hunks}"), theme.get(scope::UI_TEXT_MUTED)));
    }
    runs
}

/// The added and the removed lines of `stats`, each in its diff color, the
/// runs of [`header_counts`] before its hunk count.
fn line_counts(stats: ExcerptStats, theme: &Theme) -> Vec<(String, Style)> {
    let mut runs = Vec::new();
    if stats.added > 0 {
        runs.push((format!("+{}", stats.added), theme.get(scope::DIFF_ADDED)));
    }
    if stats.removed > 0 {
        runs.push((
            format!("\u{2212}{}", stats.removed),
            theme.get(scope::DIFF_DELETED),
        ));
    }
    runs
}

/// What a merge in progress adds to a File Header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum MergeNote<'a> {
    /// The index leaves the file unmerged. `count` is the conflict blocks its
    /// text holds, and `deleted_by` the side that deleted it, if one did.
    Conflict {
        count: u32,
        deleted_by: Option<DeletedBy>,
    },
    /// The merge changed the file clean, from the named branch it brings in.
    FromBranch(&'a str),
}

/// The runs a File Header reads left of the divide, as [`header_left`] builds
/// them for a file.
///
/// `from` names the path a moved file left, and reads at the row's left edge.
/// `tag` names the file's kind in its File Kind Tag, and `counts` counts the
/// diff or notes the file in their place. The tag and the counts read against
/// the divide.
pub struct HeaderLeft {
    pub from: Vec<(String, Style)>,
    pub tag: (String, Style),
    pub counts: Vec<(String, Style)>,
}

/// The runs that a File Header of a file of `kind`, whose diff `stats` counts,
/// reads left of the divide.
///
/// A moved file reads `from` and the path it left, muted with the name bold.
/// The counts are [`header_counts`] of `stats`. In their place a move with no
/// hunk reads `identical`, and an `empty` file reads `empty file`.
///
/// During a merge, `merge` names what the merge adds. A conflicted file's tag
/// reads `conflict` and the file's conflict blocks, and reads `conflict` alone
/// for a file with none or one that a side deleted. A modified file that the
/// merge changed clean reads `merged`. A side that deleted the file, or the
/// branch the merge brought a change from, takes the place of the hunk count.
pub fn header_left(
    kind: &FileKind,
    stats: ExcerptStats,
    empty: bool,
    merge: Option<MergeNote<'_>>,
    theme: &Theme,
) -> HeaderLeft {
    let muted = theme.get(scope::UI_TEXT_MUTED);
    let from = match kind {
        FileKind::Moved { from, .. } => {
            let mut runs = vec![("from ".to_string(), muted)];
            runs.extend(path_runs(&from.to_string_lossy(), muted, theme));
            runs
        },
        _ => Vec::new(),
    };
    let note = match merge {
        Some(MergeNote::Conflict {
            deleted_by: Some(side),
            ..
        }) => Some((delete_note(side).to_string(), muted)),
        Some(MergeNote::FromBranch(branch)) => Some((format!("from {branch}"), muted)),
        _ => None,
    };
    let mut counts = match kind {
        _ if empty => vec![("empty file".to_string(), muted)],
        FileKind::Moved { .. } if stats.hunks == 0 => vec![("identical".to_string(), muted)],
        _ if note.is_some() => line_counts(stats, theme),
        _ => header_counts(stats, theme),
    };
    counts.extend(note);
    let tag = match merge {
        Some(MergeNote::Conflict {
            count,
            deleted_by: None,
        }) if count > 0 => (
            format!("conflict \u{b7} {count}"),
            theme.conflict_tag_style(),
        ),
        Some(MergeNote::Conflict { .. }) => ("conflict".to_string(), theme.conflict_tag_style()),
        Some(MergeNote::FromBranch(_)) if *kind == FileKind::Modified => {
            ("merged".to_string(), theme.merged_tag_style())
        },
        _ => (kind_tag_text(kind), kind_tag_style(theme, kind)),
    };
    HeaderLeft { from, tag, counts }
}

/// The note a File Header reads for a conflicted file that `side` deleted,
/// what ours did to the file and then what theirs did.
fn delete_note(side: DeletedBy) -> &'static str {
    match side {
        DeletedBy::Theirs => "modify / delete",
        DeletedBy::Ours => "delete / modify",
    }
}

/// The text of the File Kind Tag that names `kind`.
///
/// A move that also edits names how alike the two files are, as `moved U+00B7
/// 94%` does.
pub(crate) fn kind_tag_text(kind: &FileKind) -> String {
    match kind {
        FileKind::Modified => "modified".to_string(),
        FileKind::Moved { similarity, .. } if *similarity < 100 => {
            format!("moved \u{b7} {similarity}%")
        },
        FileKind::Moved { .. } => "moved".to_string(),
        FileKind::Untracked => "untracked".to_string(),
        FileKind::New => "new".to_string(),
        FileKind::Deleted => "deleted".to_string(),
    }
}

/// The style of the File Kind Tag that names `kind` in a File Header, its
/// text (foreground) on its box (background).
///
/// A theme that leaves the kind's `diff.kind` scope unset gives the tag no
/// box. The tag's text then takes the foreground of the kind's diff mark,
/// which is `diff.modified`, `diff.moved`, `diff.added` for an untracked or
/// a new file, or `diff.deleted`.
pub fn kind_tag_style(theme: &Theme, kind: &FileKind) -> Style {
    let (tag, mark) = match kind {
        FileKind::Modified => (scope::DIFF_KIND_MODIFIED, scope::DIFF_MODIFIED),
        FileKind::Moved { .. } => (scope::DIFF_KIND_MOVED, scope::DIFF_MOVED),
        FileKind::Untracked => (scope::DIFF_KIND_UNTRACKED, scope::DIFF_ADDED),
        FileKind::New => (scope::DIFF_KIND_NEW, scope::DIFF_ADDED),
        FileKind::Deleted => (scope::DIFF_KIND_DELETED, scope::DIFF_DELETED),
    };
    theme.tag_style(tag, mark)
}

/// The left half of `excerpt`'s File Header and the lines its file hides, with
/// the file's kind as the frame's tally names it.
///
/// A file with no tally entry reads as modified. The hidden lines are `None`
/// for a file that shows its rows.
fn excerpt_header(
    excerpt: &Excerpt,
    buffers: &BufferRegistry,
    frame: FrameCtx<'_>,
) -> (HeaderLeft, Option<u32>) {
    let modified = FileKind::Modified;
    let kind = excerpt.file_kind(frame.tallies).unwrap_or(&modified);
    let left = header_left(
        kind,
        diff_changes::excerpt_stats(excerpt, buffers),
        excerpt.empty,
        merge_note(excerpt, frame),
        frame.theme,
    );
    let hidden = excerpt
        .collapsed
        .then(|| diff_changes::hidden_lines(excerpt, kind, buffers));
    (left, hidden)
}

/// What the merge in progress adds to `excerpt`'s File Header, or `None` with
/// no merge in progress and for a file the merge leaves alone.
///
/// A file the tally lists as unmerged is a conflict. A modified or a new file
/// that the index changes with no working change reads as the merge's, since a
/// merge writes its clean results to the index. A change that the reader
/// staged before the merge reads the same.
pub(crate) fn merge_note<'a>(excerpt: &Excerpt, frame: FrameCtx<'a>) -> Option<MergeNote<'a>> {
    let branch = frame.repo?.merge_head.as_deref()?;
    let tallies = frame.tallies?;
    let path = Path::new(&excerpt.rel_path);
    if tallies
        .conflicted
        .binary_search_by(|conflicted| conflicted.as_path().cmp(path))
        .is_ok()
    {
        let deleted_by = tallies
            .deleted_in_conflict
            .binary_search_by(|(deleted, _)| deleted.as_path().cmp(path))
            .ok()
            .map(|index| tallies.deleted_in_conflict[index].1);
        return Some(MergeNote::Conflict {
            count: excerpt.conflicts,
            deleted_by,
        });
    }
    let change = tallies.change(path)?;
    let merged = matches!(change.kind, FileKind::Modified | FileKind::New) && !change.unstaged;
    merged.then_some(MergeNote::FromBranch(branch))
}

/// The text a folded File Header counts its `lines` with.
fn hidden_text(lines: u32) -> String {
    match lines {
        1 => "1 line hidden".to_string(),
        lines => format!("{lines} lines hidden"),
    }
}

/// Fill `row` with the raised surface and lay a File Header across it, the
/// file's `path` right of the diff's divide and `left` left of it.
///
/// The path starts a pad past the divide, over the editable side, and clips at
/// the row's right edge. The counts of `left` end a pad before the divide, and
/// its File Kind Tag ends a gap before them, so both read against the divide.
/// The path a moved file left starts a pad in from the row's left edge and
/// clips a gap before the tag.
///
/// A row too narrow for the divide, as a unified diff lays it out, starts the
/// path a pad in from its left and follows it with the tag and the counts, a
/// gap apart. It leaves the old path out, and a tag or a count that does not
/// fit before the row's right pad takes no place.
///
/// The header of a file folded to it counts the `hidden` lines at the row's
/// right edge, a pad in. The path stops a gap before the count, and so do the
/// tag and the counts of a row with no divide. A row too narrow to start the
/// count at or past the path's start leaves the count out.
///
/// With `scene` and colors in RGB, the text draws as runs: the path and the old
/// path at the Ui scale, the counts and the hidden count at the Caption scale,
/// and the tag at the Small scale over a rounded box in its background.
/// Otherwise it fills cells. With `cells` false the row's cells stay as they
/// are, for a page whose cells the terminal keeps.
#[allow(clippy::too_many_arguments)]
pub fn paint_header(
    buf: &mut Buffer,
    row: Rect,
    path: &[(String, Style)],
    left: &HeaderLeft,
    hidden: Option<u32>,
    theme: &Theme,
    scene: Option<&mut ApcScene>,
    cells: bool,
) {
    if cells {
        let fill = theme.get(scope::UI_SURFACE_RAISED);
        for x in row.left()..row.right() {
            buf[(x, row.y)].set_char(' ').set_style(fill);
        }
    }

    let hidden = hidden.map(|lines| (hidden_text(lines), theme.get(scope::UI_TEXT_DISABLED)));
    let hidden = hidden.as_slice();
    let drawn =
        scene.is_some_and(|scene| emit_header_runs(row, path, left, hidden, theme, buf, scene));
    if !drawn && cells {
        write_header_cells(row, path, left, hidden, buf);
    }
}

/// Emit a File Header's `path`, `left`, and `hidden` count over `row` as runs
/// and a tag box into `scene`, laid out as [`paint_header`] describes.
///
/// Emits nothing and answers false where a run's color lies outside RGB. A tag
/// whose style has no RGB background draws its text alone.
fn emit_header_runs(
    row: Rect,
    path: &[(String, Style)],
    left: &HeaderLeft,
    hidden: &[(String, Style)],
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) -> bool {
    let rgb = |runs: &[(String, Style)]| -> Option<Vec<[u8; 3]>> {
        runs.iter().map(|(_, style)| style_rgb(style.fg)).collect()
    };
    let (
        Some(path_colors),
        Some(from_colors),
        Some(count_colors),
        Some(tag_color),
        Some(hidden_colors),
    ) = (
        rgb(path),
        rgb(&left.from),
        rgb(&left.counts),
        style_rgb(left.tag.1.fg),
        rgb(hidden),
    )
    else {
        return false;
    };

    let (ui, caption, small) = (
        theme.type_scale(TypeRole::Ui),
        theme.type_scale(TypeRole::Caption),
        theme.type_scale(TypeRole::Small),
    );
    let advance = |text: &str, scale| text_run::advance_sixteenths(text.chars().count(), scale);
    let width = row.width * 16;
    let divide = header_divide(row);
    let path_start = divide.map_or(HEADER_PAD, |divide| divide + HEADER_PAD);

    // The text that reads from the left stops at `end`, a gap before the
    // hidden count where one shows, else at the row's right edge.
    let mut runs = TextRunBatch::default();
    let mut end = width;
    for ((text, style), &color) in hidden.iter().zip(&hidden_colors) {
        let start = width.saturating_sub(HEADER_PAD + advance(text, caption));
        if start >= path_start {
            runs.push(row, header_run(start, text, caption, color, *style));
            end = start.saturating_sub(HEADER_COUNT_GAP);
        }
    }

    let mut col = path_start;
    for ((text, style), &color) in path.iter().zip(&path_colors) {
        let text = fitting_prefix(text, end.saturating_sub(col), ui);
        if text.is_empty() {
            break;
        }
        runs.push(row, header_run(col, text, ui, color, *style));
        col += advance(text, ui);
    }

    // The tag's box goes into the scene before the runs, so its text draws
    // over it. It goes alone, because a bar batch drops the corner radius.
    let (tag_text, tag_style) = &left.tag;
    let tag_width = advance(tag_text, small) + 2 * TAG_INSET;
    let mut tag = |start: u16, runs: &mut TextRunBatch| {
        if let Some(bg) = style_rgb(tag_style.bg) {
            let tag_box = Bar {
                x: start as i16,
                y: 1,
                width: tag_width,
                height: TAG_HEIGHT,
                color: bg,
                radius: TAG_RADIUS,
                corners: 0,
            };
            StatefulWidget::render(tag_box, row, buf, scene);
        }
        let text = header_run(start + TAG_INSET, tag_text, small, tag_color, *tag_style);
        runs.push(row, text);
    };

    match divide {
        Some(divide) => {
            // The counts pack leftward from their end, and the tag ends a gap
            // before the first count placed.
            let counts_end = divide.saturating_sub(HEADER_PAD);
            let mut end = counts_end;
            let mut placed = Vec::new();
            for ((text, style), &color) in left.counts.iter().zip(&count_colors).rev() {
                let Some(start) = end.checked_sub(advance(text, caption)) else {
                    break;
                };
                placed.push(header_run(start, text, caption, color, *style));
                end = start.saturating_sub(HEADER_COUNT_GAP);
            }
            let tag_end = match placed.last() {
                Some(first) => (first.col as u16).saturating_sub(HEADER_COUNT_GAP),
                None => counts_end,
            };
            let tag_start = tag_end.saturating_sub(tag_width);
            tag(tag_start, &mut runs);
            for count in placed.into_iter().rev() {
                runs.push(row, count);
            }

            let from_end = tag_start.saturating_sub(HEADER_COUNT_GAP);
            let mut col = HEADER_PAD;
            for ((text, style), &color) in left.from.iter().zip(&from_colors) {
                let text = fitting_prefix(text, from_end.saturating_sub(col), ui);
                if text.is_empty() {
                    break;
                }
                runs.push(row, header_run(col, text, ui, color, *style));
                col += advance(text, ui);
            }
        },
        None => {
            let limit = end.min(width.saturating_sub(HEADER_PAD));
            let mut col = col + HEADER_COUNT_GAP;
            if col + tag_width <= limit {
                tag(col, &mut runs);
                col += tag_width + HEADER_COUNT_GAP;
                for ((text, style), &color) in left.counts.iter().zip(&count_colors) {
                    let width = advance(text, caption);
                    if col + width > limit {
                        break;
                    }
                    runs.push(row, header_run(col, text, caption, color, *style));
                    col += width + HEADER_COUNT_GAP;
                }
            }
        },
    }
    runs.flush(scene);
    true
}

/// A File Header's run of `text` at `col` sixteenths across its row, bold
/// where `style` is.
///
/// The run takes no background, so the header's hairlines stay unbroken under
/// its text.
fn header_run(col: u16, text: &str, scale: u16, color: [u8; 3], style: Style) -> TextRun<'_> {
    TextRun {
        col: col as i16,
        row: 0,
        scale,
        color,
        bg: None,
        text,
        bold: style.add_modifier.contains(Modifier::BOLD),
        italic: false,
        follow: 0,
        anchor: None,
    }
}

/// The longest start of `text` whose run at `scale` advances at most `room`
/// sixteenths.
fn fitting_prefix(text: &str, room: u16, scale: u16) -> &str {
    let fit = (u32::from(room) * 16 + 7) / u32::from(scale.max(1));
    text.char_indices()
        .nth(fit as usize)
        .map_or(text, |(at, _)| &text[..at])
}

/// Write a File Header's `path`, `left`, and `hidden` count into the cells of
/// `row`, laid out as [`paint_header`] describes, with one cell for each gap.
///
/// The path starts two cells past the divider's column. The left half reads
/// the old path, the tag's text in its foreground, and the counts, and ends
/// one cell before the divider's column, the cells nearest the runs' pads. A
/// left half wider than its room loses the cells left of the row's edge. The
/// hidden count ends one cell before the row's right edge.
fn write_header_cells(
    row: Rect,
    path: &[(String, Style)],
    left: &HeaderLeft,
    hidden: &[(String, Style)],
    buf: &mut Buffer,
) {
    let divider = review::diff_divider_x(row);
    let path_start = divider.map_or(row.x + 1, |divider| divider + 2);
    let mut end = row.right();
    for (text, style) in hidden {
        let start = (row.right() - 1).checked_sub(text_width(text) as u16);
        if let Some(start) = start.filter(|&start| start >= path_start) {
            write_str(buf, start, row.y, text, *style);
            end = start - 1;
        }
    }

    let mut x = path_start;
    for (text, style) in path {
        write_str_clipped(buf, x, row.y, text, *style, end);
        x = x.saturating_add(text_width(text) as u16);
    }

    let tag = (
        left.tag.0.as_str(),
        Style {
            fg: left.tag.1.fg,
            ..Style::default()
        },
    );
    let Some(divider) = divider else {
        let limit = (row.right() - 1).min(end);
        let mut x = x + 1;
        let tag_width = text_width(tag.0) as u16;
        if x + tag_width > limit {
            return;
        }
        write_str(buf, x, row.y, tag.0, tag.1);
        x += tag_width + 1;
        for (text, style) in &left.counts {
            let width = text_width(text) as u16;
            if x + width > limit {
                break;
            }
            write_str(buf, x, row.y, text, *style);
            x += width + 1;
        }
        return;
    };

    // Each piece and the cells of space before it.
    let pieces: Vec<(&str, Style, u16)> = left
        .from
        .iter()
        .map(|(text, style)| (text.as_str(), *style, 0))
        .chain([(tag.0, tag.1, u16::from(!left.from.is_empty()))])
        .chain(
            left.counts
                .iter()
                .map(|(text, style)| (text.as_str(), *style, 1)),
        )
        .collect();
    let total: u16 = pieces
        .iter()
        .map(|&(text, _, gap)| gap + text_width(text) as u16)
        .sum();
    let mut x = i32::from(divider - 1) - i32::from(total);
    for (text, style, gap) in pieces {
        x += i32::from(gap);
        let skip = (i32::from(row.x) - x).max(0) as usize;
        let shown: String = text.chars().skip(skip).collect();
        if !shown.is_empty() {
            write_str(buf, (x + skip as i32) as u16, row.y, &shown, style);
        }
        x += text_width(text) as i32;
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
#[allow(clippy::too_many_arguments)]
pub fn paint_stack_page(
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
/// place in it, under the pinned rows as the paint lays it, and records the
/// cell there.
///
/// When the focused excerpt is outside the window, the cursor is under the
/// pinned row, or the terminal does not draw its cursor, a replay records no
/// cell. The paint does the same.
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

    let inner = pane.area;
    let (inner, _) = split_minimap_strip(inner, frame.minimap_enabled);
    let (scroll, area) = pinned_split(state, editors, inner);
    let (top, viewport) = (excerpt.top, usize::from(area.height));
    let rows = diff_changes::excerpt_rows(excerpt, editors);
    let header = diff_changes::header_rows(excerpt);
    let first = (top + header).saturating_sub(scroll);
    if rows == 0 || top + header + rows <= scroll || top >= scroll + viewport || first >= viewport {
        return;
    }
    let Some(editor) = editors.get_mut(excerpt.editor_id) else {
        return;
    };
    let Some(text) = editor.text_rect else {
        return;
    };
    let scroll_row = scroll.saturating_sub(top + header);
    let text = Rect {
        y: area.y + first as u16,
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
    use super::kind_tag_style;
    use crate::{
        app::DEFAULT_KEYMAP,
        host::git::FileKind,
        theme::{scope, Theme},
    };
    use ratatui::style::{Color, Style};
    use std::path::PathBuf;

    /// The ermine File Kind Tags take the DiffKindsSpec board's colors. A
    /// theme that sets no `diff.kind` scope, as default_dark does, tags each
    /// kind with its text alone, in the foreground of the kind's diff mark.
    #[test]
    fn a_kind_tag_without_its_scope_takes_its_diff_mark_s_text_color() {
        let [ermine, dark] = ["ermine_night", "default_dark"].map(|name| {
            let (config, errors) = stoat_config::parse(DEFAULT_KEYMAP);
            assert!(errors.is_empty(), "parse errors: {errors:?}");
            let config = config.expect("expected successful parse");
            Theme::from_config(&config, name).expect("theme load failed")
        });
        let kinds = [
            FileKind::Modified,
            FileKind::Moved {
                from: PathBuf::from("a"),
                similarity: 100,
            },
            FileKind::Untracked,
            FileKind::New,
            FileKind::Deleted,
        ];
        let rgb = |hex: u32| {
            let [_, r, g, b] = hex.to_be_bytes();
            Color::Rgb(r, g, b)
        };
        let tag = |fg: u32, bg: u32| Style::new().fg(rgb(fg)).bg(rgb(bg));
        let mark = |scope: &str| Style {
            fg: dark.get(scope).fg,
            ..Style::default()
        };

        assert_eq!(
            (
                kinds.each_ref().map(|kind| kind_tag_style(&ermine, kind)),
                kinds.each_ref().map(|kind| kind_tag_style(&dark, kind)),
            ),
            (
                [
                    tag(0x7fb0ff, 0x1f2a3d),
                    tag(0x6cc7d6, 0x1e3237),
                    tag(0xe8bd6a, 0x2e2a1e),
                    tag(0x8fd18a, 0x1d2a1f),
                    tag(0xf2777f, 0x33201f),
                ],
                [
                    mark(scope::DIFF_MODIFIED),
                    mark(scope::DIFF_MOVED),
                    mark(scope::DIFF_ADDED),
                    mark(scope::DIFF_ADDED),
                    mark(scope::DIFF_DELETED),
                ],
            ),
        );
    }
}
