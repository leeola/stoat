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
    host::git::FileKind,
    pane::Pane,
    render::{
        chrome,
        editor::{fit_diff_view, render_editor_with_overlay, screen_cell, split_minimap_strip},
        paint::style_rgb,
        review,
        text::{text_width, write_str, write_str_clipped},
        undercurl::UndercurlBatch,
        FrameCtx,
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
            paint_header(
                buf,
                row,
                &header_path(&excerpt.rel_path, frame.theme),
                &excerpt_header_left(excerpt, buffers, frame),
                frame.theme,
                match scene.live() {
                    true => Some(&mut *scene),
                    false => None,
                },
                paint == PagePaint::Cells,
            );
        }
        if let Some(scope) = &excerpt.scope_row
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
    paint_header(
        buf,
        row,
        &header_path(&excerpt.rel_path, frame.theme),
        &excerpt_header_left(excerpt, buffers, frame),
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
pub(crate) fn header_path(rel_path: &str, theme: &Theme) -> Vec<(String, Style)> {
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
pub(crate) fn header_counts(stats: ExcerptStats, theme: &Theme) -> Vec<(String, Style)> {
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

/// The runs a File Header reads left of the divide, as [`header_left`] builds
/// them for a file.
///
/// `from` names the path a moved file left, and reads at the row's left edge.
/// `tag` names the file's kind in its File Kind Tag, and `counts` counts the
/// diff or notes the file in their place. The tag and the counts read against
/// the divide.
struct HeaderLeft {
    from: Vec<(String, Style)>,
    tag: (String, Style),
    counts: Vec<(String, Style)>,
}

/// The runs that a File Header of a file of `kind`, whose diff `stats` counts,
/// reads left of the divide.
///
/// A moved file reads `from` and the path it left, muted with the name bold.
/// The counts are [`header_counts`] of `stats`. In their place a move with no
/// hunk reads `identical`, and an `empty` file reads `empty file`.
fn header_left(kind: &FileKind, stats: ExcerptStats, empty: bool, theme: &Theme) -> HeaderLeft {
    let muted = theme.get(scope::UI_TEXT_MUTED);
    let from = match kind {
        FileKind::Moved { from, .. } => {
            let mut runs = vec![("from ".to_string(), muted)];
            runs.extend(path_runs(&from.to_string_lossy(), muted, theme));
            runs
        },
        _ => Vec::new(),
    };
    let counts = match kind {
        _ if empty => vec![("empty file".to_string(), muted)],
        FileKind::Moved { .. } if stats.hunks == 0 => vec![("identical".to_string(), muted)],
        _ => header_counts(stats, theme),
    };
    HeaderLeft {
        from,
        tag: (kind_tag_text(kind), theme.kind_tag_style(kind)),
        counts,
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

/// The left half of `excerpt`'s File Header, its file's kind as the frame's
/// tally names it.
///
/// A file with no tally entry reads as modified.
fn excerpt_header_left(
    excerpt: &Excerpt,
    buffers: &BufferRegistry,
    frame: FrameCtx<'_>,
) -> HeaderLeft {
    let modified = FileKind::Modified;
    let kind = excerpt.file_kind(frame.tallies).unwrap_or(&modified);
    header_left(
        kind,
        diff_changes::excerpt_stats(excerpt, buffers),
        excerpt.empty,
        frame.theme,
    )
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
/// With `scene` and colors in RGB, the text draws as runs: the path and the old
/// path at the Ui scale, the counts at the Caption scale, and the tag at the
/// Small scale over a rounded box in its background. Otherwise it fills cells.
/// With `cells` false the row's cells stay as they are, for a page whose cells
/// the terminal keeps.
fn paint_header(
    buf: &mut Buffer,
    row: Rect,
    path: &[(String, Style)],
    left: &HeaderLeft,
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

    let drawn = scene.is_some_and(|scene| emit_header_runs(row, path, left, theme, buf, scene));
    if !drawn && cells {
        write_header_cells(row, path, left, buf);
    }
}

/// Emit a File Header's `path` and `left` over `row` as runs and a tag box
/// into `scene`, laid out as [`paint_header`] describes.
///
/// Emits nothing and answers false where a run's color lies outside RGB. A tag
/// whose style has no RGB background draws its text alone.
fn emit_header_runs(
    row: Rect,
    path: &[(String, Style)],
    left: &HeaderLeft,
    theme: &Theme,
    buf: &mut Buffer,
    scene: &mut ApcScene,
) -> bool {
    let rgb = |runs: &[(String, Style)]| -> Option<Vec<[u8; 3]>> {
        runs.iter().map(|(_, style)| style_rgb(style.fg)).collect()
    };
    let (Some(path_colors), Some(from_colors), Some(count_colors), Some(tag_color)) = (
        rgb(path),
        rgb(&left.from),
        rgb(&left.counts),
        style_rgb(left.tag.1.fg),
    ) else {
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

    let mut runs = TextRunBatch::default();
    let mut col = divide.map_or(HEADER_PAD, |divide| divide + HEADER_PAD);
    for ((text, style), &color) in path.iter().zip(&path_colors) {
        let text = fitting_prefix(text, width.saturating_sub(col), ui);
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
            let limit = width.saturating_sub(HEADER_PAD);
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

/// Write a File Header's `path` and `left` into the cells of `row`, laid out
/// as [`paint_header`] describes, with one cell for each gap.
///
/// The path starts two cells past the divider's column. The left half reads
/// the old path, the tag's text in its foreground, and the counts, and ends
/// one cell before the divider's column, the cells nearest the runs' pads. A
/// left half wider than its room loses the cells left of the row's edge.
fn write_header_cells(row: Rect, path: &[(String, Style)], left: &HeaderLeft, buf: &mut Buffer) {
    let divider = review::diff_divider_x(row);
    let mut x = divider.map_or(row.x + 1, |divider| divider + 2);
    for (text, style) in path {
        write_str_clipped(buf, x, row.y, text, *style, row.right());
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
        let limit = row.right() - 1;
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
    use super::{
        header_counts, header_left, header_path, paint_header, paint_stack_page, paint_stack_rows,
        PagePaint,
    };
    use crate::{
        action_handlers, apc_emit,
        app::Stoat,
        buffer_registry::BufferRegistry,
        diff_changes::{self, DiffChangesState, ExcerptStats},
        editor_state::{EditorId, EditorState},
        host::git::FileKind,
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
        render::{
            commits::PreviewDials,
            content_lane, paint,
            review::{self, DiffColumns, DiffDials, DiffLayout},
            surface_blank,
            undercurl::UndercurlBatch,
            FrameCtx, SINGLE_MINIMAP_STRIP_ID,
        },
        test_fixture::drain_apc,
        test_harness::TestHarness,
        theme::{scope, TypeRole},
        workspace::{diff::WorktreeBase, Workspace},
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
    use stoat_widgets::{text_run, ApcScene};
    use stoatty_protocol::command::{decode_stream, Command};
    use tokio::sync::mpsc::UnboundedReceiver;

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
        // The changes view keeps its hints box up over the lower right, across
        // the rows the checks read.
        h.stoat.screen_hints_hidden = true;
        h.stage_review_scenario("/repo", files);
        h.open_file(&Path::new("/repo").join(files[0].0));
        h.cover_with_diff_changes(false);
        h.settle();
        h
    }

    /// Each excerpt paints under a header row that counts its diff left of the
    /// divider and names its file right of it, with its rows numbered in the
    /// per-file diff's gutters, and only the focused excerpt paints a cursor.
    ///
    /// The numbers are absolute, so each pair names its own base and buffer
    /// line. The cell layout ends the counts one cell before the divider and
    /// starts the name two cells past it.
    #[test]
    fn a_changes_view_paints_its_excerpts_under_their_file_headers() {
        let mut h = two_files(16);
        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Absolute);
        let buf = h.render_composited();
        let inner = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let cols = excerpt_columns(&h, inner, 2);

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
        // a.txt's three rows sit under its header on row 0, and b.txt's first
        // row under its header on row 4.
        h.stoat.stoatty = false;
        let cells = h.render_composited();
        let divider = review::diff_divider_x(inner).expect("a split stack");
        let headers = [inner.y, inner.y + 4].map(|y| {
            [
                header_text(&cells, inner.x, divider - inner.x, y),
                header_text(&cells, divider + 1, inner.right() - divider - 1, y),
            ]
        });

        let pair = |n: &str| (n.to_string(), n.to_string());
        let counts = "modified +1 \u{2212}1 \u{b7} 1 hunk";
        let left = (
            counts.to_string(),
            Some(divider - 1 - counts.chars().count() as u16),
        );
        let right = |name: &str| (name.to_string(), Some(divider + 2));
        assert_eq!(
            (headers, numbers, cursors),
            (
                [[left.clone(), right("a.txt")], [left, right("b.txt")]],
                vec![pair("1"), pair("2"), pair("3"), pair("1")],
                vec![Some((cols.right_text_x, inner.y + 1)), None],
            ),
        );
    }

    /// The deepest soften leaves an excerpt's changed base word readable, as
    /// it does in the per-file diff view.
    ///
    /// `main` became `other`, so the `m` is a changed char and the `f` is an
    /// unchanged one on the same row.
    #[test]
    fn the_deepest_soften_keeps_an_excerpt_s_changed_base_word_readable() {
        let mut h = changes_view(
            120,
            16,
            &[(
                "a.rs",
                "fn keep() {}\nfn main() {}\n",
                "fn keep() {}\nfn other() {}\n",
            )],
        );
        h.stoat.diff_syntax = false;
        h.stoat.diff_soften = review::DIFF_SOFTEN_MAX;
        let buf = h.render_composited();
        let inner = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let cols = excerpt_columns(&h, inner, 2);

        let bg = paint::style_rgb(h.stoat.theme.get(scope::UI_BACKGROUND).bg)
            .expect("the test theme has an rgb background");
        let row = (inner.y..inner.y + inner.height)
            .find(|&y| cells(&buf, cols.left_text_x, 7, y) == "fn main")
            .expect("the changed line's base row paints in the left column");
        let distance = |x: u16| {
            let fg = paint::style_rgb(buf[(x, row)].style().fg).expect("an rgb foreground");
            (paint::luma(fg) - paint::luma(bg)).abs()
        };
        let (unchanged, changed) = (distance(cols.left_text_x), distance(cols.left_text_x + 3));

        assert!(
            changed >= paint::FAINT_CONTRAST_FLOOR,
            "the changed `m` stands {changed} from the background, under the floor",
        );
        assert!(
            changed > unchanged,
            "the changed `m` stands further from the background than the unchanged `f`: \
             {changed} against {unchanged}",
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
            ws.panes.pane(ws.panes.focus()).area
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

    /// A file name wider than the half right of the divider starts two cells
    /// past the divider and clips at the row's right edge.
    #[test]
    fn a_path_wider_than_its_half_clips_at_the_row_s_right_edge() {
        let name = format!("{}.txt", "n".repeat(70));
        let mut h = changes_view(120, 16, &[(&name, "a\n", "A\n")]);
        h.stoat.stoatty = false;
        let buf = h.render_composited();
        let inner = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let start = review::diff_divider_x(inner).expect("a split stack") + 2;

        assert_eq!(
            cells(&buf, start, inner.right() - start, inner.y),
            name[..usize::from(inner.right() - start)]
        );
    }

    /// A header names the directory, the bold name, and the bold extension in
    /// the type's color right of the divider. Left of it, against the
    /// divider, it reads the file's kind in its tag and counts the lines in
    /// their diff colors and the hunks in muted text. With no structural hunk
    /// the count names the hunks alone, and a count of zero takes no run.
    ///
    /// The cells of a 120-wide row end the left half at column 58, one before
    /// the divider's column 59, and start the path at column 61.
    #[test]
    fn a_header_counts_its_lines_and_hunks_left_of_the_divider() {
        let h = TestHarness::with_size(40, 4);
        let theme = &h.stoat.theme;
        let stats = |added, removed, hunks, structural| ExcerptStats {
            added,
            removed,
            hunks,
            structural,
        };
        let painted = |path: &str, stats: ExcerptStats| {
            let row = Rect::new(0, 0, 120, 1);
            let mut buf = Buffer::empty(row);
            let path = header_path(path, theme);
            let left = header_left(&FileKind::Modified, stats, false, theme);
            paint_header(&mut buf, row, &path, &left, theme, None, true);
            [header_text(&buf, 0, 59, 0), header_text(&buf, 60, 60, 0)]
        };
        let run = |text: &str, style: Style| (text.to_string(), style);
        let header = |counts: &str, path: &str| {
            let left = format!("modified {counts}").trim_end().to_string();
            let start = Some(58 - left.chars().count() as u16);
            [(left, start), (path.to_string(), Some(61))]
        };

        assert_eq!(
            (
                header_path("stoat/src/render/chrome.rs", theme),
                header_counts(stats(12, 15, 2, 2), theme),
                [
                    painted("src/theme.rs", stats(6, 0, 1, 0)),
                    painted("notes.txt", stats(0, 4, 3, 0)),
                    painted("a.rs", stats(1, 1, 1, 1)),
                    painted("new.txt", ExcerptStats::default()),
                ],
            ),
            (
                vec![
                    run("stoat/src/render/", theme.get(scope::UI_TEXT_MUTED)),
                    run(
                        "chrome",
                        theme.get(scope::UI_TEXT).add_modifier(Modifier::BOLD)
                    ),
                    run(
                        ".rs",
                        theme.get(scope::UI_TAG_RUST).add_modifier(Modifier::BOLD)
                    ),
                ],
                vec![
                    run("+12", theme.get(scope::DIFF_ADDED)),
                    run("\u{2212}15", theme.get(scope::DIFF_DELETED)),
                    run(
                        "\u{b7} 2 git hunks \u{b7} 2 structural",
                        theme.get(scope::UI_TEXT_MUTED)
                    ),
                ],
                [
                    header("+6 \u{b7} 1 hunk", "src/theme.rs"),
                    header("\u{2212}4 \u{b7} 3 hunks", "notes.txt"),
                    header("+1 \u{2212}1 \u{b7} 1 git hunk \u{b7} 1 structural", "a.rs"),
                    header("", "new.txt"),
                ],
            ),
        );
    }

    /// A header too narrow for the divide starts its path one cell in from the
    /// left and follows it with the tag and the counts, a cell apart. A tag or
    /// a count that does not fit before the row's last cell takes no place,
    /// so none shows cut.
    #[test]
    fn a_unified_header_follows_its_path_with_its_tag_and_counts() {
        let h = TestHarness::with_size(40, 4);
        let theme = &h.stoat.theme;
        let stats = ExcerptStats {
            added: 12,
            removed: 3,
            hunks: 2,
            structural: 0,
        };
        let painted = |width: u16| {
            let row = Rect::new(0, 0, width, 1);
            let mut buf = Buffer::empty(row);
            let path = header_path("src/lib.rs", theme);
            let left = header_left(&FileKind::Modified, stats, false, theme);
            paint_header(&mut buf, row, &path, &left, theme, None, true);
            (cells(&buf, 0, width, 0), buf[(1, 0)].symbol().to_string())
        };

        assert_eq!(
            [painted(40), painted(24), painted(16)],
            [
                "src/lib.rs modified +12 \u{2212}3 \u{b7} 2 hunks",
                "src/lib.rs modified",
                "src/lib.rs",
            ]
            .map(|text| (text.to_string(), "s".to_string())),
        );
    }

    /// The left half of each header reads its file's kind in a File Kind Tag,
    /// in the tag's foreground, before its counts, and ends one cell before
    /// the divider. A moved file reads `from` and the path it left before its
    /// tag, and a move that also edits names how alike the two files are.
    #[test]
    fn a_header_tags_its_file_s_kind() {
        let h = TestHarness::with_size(40, 4);
        let theme = &h.stoat.theme;
        let stats = ExcerptStats {
            added: 1,
            removed: 1,
            hunks: 1,
            structural: 0,
        };
        let painted = |kind: FileKind| {
            let row = Rect::new(0, 0, 120, 1);
            let mut buf = Buffer::empty(row);
            let left = header_left(&kind, stats, false, theme);
            let path = header_path("a.rs", theme);
            paint_header(&mut buf, row, &path, &left, theme, None, true);
            let (text, start) = header_text(&buf, 0, 59, 0);
            let tag_x =
                start.expect("a left half") as usize + text.find(&left.tag.0).expect("the tag");
            let tag_fg = buf[(tag_x as u16, 0)].fg;
            ((text, start), Some(tag_fg) == left.tag.1.fg)
        };
        let moved = FileKind::Moved {
            from: PathBuf::from("src/old.rs"),
            similarity: 94,
        };

        let left = |text: &str| {
            let start = 58 - text.chars().count() as u16;
            ((text.to_string(), Some(start)), true)
        };
        assert_eq!(
            [
                FileKind::Modified,
                moved,
                FileKind::Untracked,
                FileKind::New,
                FileKind::Deleted
            ]
            .map(painted),
            [
                left("modified +1 \u{2212}1 \u{b7} 1 hunk"),
                left("from src/old.rs moved \u{b7} 94% +1 \u{2212}1 \u{b7} 1 hunk"),
                left("untracked +1 \u{2212}1 \u{b7} 1 hunk"),
                left("new +1 \u{2212}1 \u{b7} 1 hunk"),
                left("deleted +1 \u{2212}1 \u{b7} 1 hunk"),
            ],
        );
    }

    /// A stack's header reads the kind that the repository's tally names for
    /// its file. A tally that changes the kind and nothing else, as a new
    /// similarity for a move does, repaints the header.
    #[test]
    fn a_stack_header_reads_the_kind_its_repo_tally_names() {
        let mut h = TestHarness::with_size(120, 16);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
        h.stoat.screen_hints_hidden = true;
        h.stage_rename_scenario("/repo", "old.txt", "moved.txt", "one\n", "ONE\n");
        h.fake_git().add_repo("/repo").similarity("moved.txt", 94);
        h.stoat
            .active_workspace_mut()
            .set_worktree_base(WorktreeBase::Head);
        h.open_file(Path::new("/repo/moved.txt"));
        h.cover_with_diff_changes(false);
        h.settle();
        h.stoat.stoatty = false;
        let left_half = |h: &mut TestHarness| {
            let buf = h.render_composited();
            let inner = {
                let ws = h.stoat.active_workspace();
                ws.panes.pane(ws.panes.focus()).area
            };
            let divider = review::diff_divider_x(inner).expect("a split stack");
            header_text(&buf, inner.x, divider - inner.x, inner.y).0
        };
        // The first frame stores no pane cache key, so the frame after it is
        // the one a replay matches.
        left_half(&mut h);
        let before = left_half(&mut h);

        h.fake_git().add_repo("/repo").similarity("moved.txt", 80);
        h.stoat.active_workspace_mut().stale_hunk_tally();
        h.settle_diff_jobs();
        let left = |similarity: u8| {
            format!("from old.txt moved \u{b7} {similarity}% +1 \u{2212}1 \u{b7} 1 hunk")
        };
        assert_eq!((before, left_half(&mut h)), (left(94), left(80)));
    }

    /// A move with no edit reads `identical` in place of its counts, after
    /// `from` and the path it left, muted with the name bold. An empty file
    /// reads `empty file` there.
    #[test]
    fn a_move_with_no_edit_reads_identical() {
        let h = TestHarness::with_size(40, 4);
        let theme = &h.stoat.theme;
        let muted = theme.get(scope::UI_TEXT_MUTED);
        let moved = FileKind::Moved {
            from: PathBuf::from("src/old.rs"),
            similarity: 100,
        };
        let left = header_left(&moved, ExcerptStats::default(), false, theme);
        let empty = header_left(&FileKind::Untracked, ExcerptStats::default(), true, theme);

        let run = |text: &str, style: Style| (text.to_string(), style);
        assert_eq!(
            (left.from, left.tag.0, left.counts, empty.counts),
            (
                vec![
                    run("from ", muted),
                    run("src/", muted),
                    run("old", muted.add_modifier(Modifier::BOLD)),
                    run(
                        ".rs",
                        theme.get(scope::UI_TAG_RUST).add_modifier(Modifier::BOLD)
                    ),
                ],
                "moved".to_string(),
                vec![run("identical", muted)],
                vec![run("empty file", muted)],
            ),
        );
    }

    /// Under a live scene the tag is a rounded bar 14 sixteenths tall in the
    /// tag's background, which ends a gap before the first count's run, under
    /// the tag's run at the Small scale ten sixteenths inside it.
    #[test]
    fn a_live_header_draws_its_tag_as_a_rounded_bar() {
        let mut h = two_files(16);
        h.stoat.render();
        let width = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area.width
        };
        let (bars, runs, small, tag) =
            paint_directly(&mut h, |state, editors, buffers, frame, scene| {
                let mut buf = Buffer::empty(Rect::new(0, 0, width, 6));
                let area = buf.area;
                paint_stack_rows(
                    state,
                    editors,
                    buffers,
                    true,
                    frame.theme.get(scope::UI_TEXT),
                    &mut UndercurlBatch::default(),
                    frame,
                    &mut buf,
                    scene,
                    0,
                    area,
                    PagePaint::Cells,
                );
                let commands = decode_stream(scene.bytes());
                let bars: Vec<(i16, i16, u16, u16, [u8; 3], u8)> = commands
                    .iter()
                    .filter_map(|command| match command {
                        Command::Bar(bar) if bar.y == 1 => {
                            Some((bar.x, bar.y, bar.width, bar.height, bar.color, bar.radius))
                        },
                        _ => None,
                    })
                    .collect();
                let mut runs: Vec<(i16, String, u16)> = commands
                    .iter()
                    .filter_map(|command| match command {
                        Command::TextRun(run)
                            if run.row == 0 && run.col < (width / 2 * 16) as i16 =>
                        {
                            Some((run.col, run.text.clone(), run.scale))
                        },
                        _ => None,
                    })
                    .collect();
                runs.sort();
                (
                    bars,
                    runs,
                    frame.theme.type_scale(TypeRole::Small),
                    frame.theme.get(scope::DIFF_KIND_MODIFIED),
                )
            });

        let first_count = runs
            .iter()
            .find(|run| run.1 != "modified")
            .map_or(0, |run| run.0 as u16);
        let tag_width = text_run::advance_sixteenths(8, small) + 20;
        let start = first_count - 16 - tag_width;
        assert_eq!(
            (bars, runs.first().cloned()),
            (
                vec![(
                    start as i16,
                    1,
                    tag_width,
                    14,
                    paint::style_rgb(tag.bg).expect("an RGB tag box"),
                    4
                )],
                Some((start as i16 + 10, "modified".to_string(), small)),
            ),
        );
    }

    /// Under a live scene a header too narrow for the divide lays its tag's
    /// box a gap past the path's end and its counts a gap past the box, and
    /// leaves out the path a moved file left.
    ///
    /// Every type scale advances a full cell under test, so the path takes 16
    /// sixteenths a character from its pad of 20, and the box is the tag's
    /// advance plus an inset of 10 each side.
    #[test]
    fn a_live_unified_header_follows_its_path_with_its_tag() {
        let h = TestHarness::with_size(40, 4);
        let theme = &h.stoat.theme;
        let row = Rect::new(0, 0, 60, 1);
        let mut buf = Buffer::empty(row);
        let mut scene = ApcScene::new();
        scene.set_live(true);
        let moved = FileKind::Moved {
            from: PathBuf::from("old.rs"),
            similarity: 100,
        };
        let stats = ExcerptStats {
            added: 1,
            removed: 0,
            hunks: 1,
            structural: 0,
        };
        let left = header_left(&moved, stats, false, theme);
        let path = header_path("a.rs", theme);
        paint_header(&mut buf, row, &path, &left, theme, Some(&mut scene), true);

        let commands = decode_stream(scene.bytes());
        let runs: Vec<(i16, String)> = commands
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some((run.col, run.text.clone())),
                _ => None,
            })
            .collect();
        let boxes: Vec<(i16, u16)> = frame_bars(commands)
            .into_iter()
            .map(|bar| (bar.0, bar.2))
            .collect();
        let run = |col: i16, text: &str| (col, text.to_string());
        assert_eq!(
            (runs, boxes),
            (
                vec![
                    run(20, "a"),
                    run(36, ".rs"),
                    run(110, "moved"),
                    run(216, "+1"),
                    run(264, "\u{b7} 1 hunk"),
                ],
                vec![(100, 100)],
            ),
        );
    }

    /// Under a live scene the path a moved file left starts a pad in from the
    /// row's left edge and loses the characters that pass a gap before the
    /// tag's box.
    ///
    /// The row is 100 wide, so the divide sits 792 sixteenths in, the counts
    /// end at 772, and the box of `moved` ends a gap before `+1` at 596.
    #[test]
    fn a_live_header_clips_the_old_path_a_gap_before_its_tag() {
        let h = TestHarness::with_size(40, 4);
        let theme = &h.stoat.theme;
        let row = Rect::new(0, 0, 100, 1);
        let mut buf = Buffer::empty(row);
        let mut scene = ApcScene::new();
        scene.set_live(true);
        let moved = FileKind::Moved {
            from: PathBuf::from("a/very/long/directory/path/to/the/old/file.rs"),
            similarity: 100,
        };
        let stats = ExcerptStats {
            added: 1,
            removed: 0,
            hunks: 1,
            structural: 0,
        };
        let left = header_left(&moved, stats, false, theme);
        let path = header_path("a.rs", theme);
        paint_header(&mut buf, row, &path, &left, theme, Some(&mut scene), true);

        let commands = decode_stream(scene.bytes());
        let mut runs: Vec<(i16, String)> = commands
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run) => Some((run.col, run.text.clone())),
                _ => None,
            })
            .collect();
        runs.sort();
        let boxes: Vec<(i16, u16)> = frame_bars(commands)
            .into_iter()
            .map(|bar| (bar.0, bar.2))
            .collect();
        let run = |col: i16, text: &str| (col, text.to_string());
        assert_eq!(
            (runs, boxes),
            (
                vec![
                    run(20, "from "),
                    run(100, "a/very/long/directory/"),
                    run(490, "moved"),
                    run(596, "+1"),
                    run(644, "\u{b7} 1 hunk"),
                    run(812, "a"),
                    run(828, ".rs"),
                ],
                vec![(480, 100)],
            ),
        );
    }

    /// A focused changes view at 120 by `height` over one untracked file of
    /// `lines` numbered lines, with no minimap, settled.
    fn untracked_stack(height: u16, lines: u32) -> TestHarness {
        let mut h = TestHarness::with_size(120, height);
        h.stoat.settings.editor_minimap = Some(MinimapMode::Off);
        h.stoat.screen_hints_hidden = true;
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.fake_git()
            .add_repo(PathBuf::from("/repo"))
            .with_fs(h.fake_fs())
            .untracked("new.txt");
        let text: String = (1..=lines).map(|n| format!("{n}\n")).collect();
        h.fake_fs()
            .insert_file(Path::new("/repo/new.txt"), text.as_bytes());
        h.open_file(Path::new("/repo/new.txt"));
        h.cover_with_diff_changes(false);
        h.settle();
        h
    }

    /// The pane-relative column and row of each `no base` in `buf`'s cells.
    fn no_base_cells(h: &TestHarness, buf: &Buffer) -> Vec<(u16, u16)> {
        let inner = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        (inner.y..inner.bottom())
            .flat_map(|y| (inner.x..inner.right() - 6).map(move |x| (x, y)))
            .filter(|&(x, y)| {
                (x..x + 7).map(|x| buf[(x, y)].symbol()).collect::<String>() == "no base"
            })
            .map(|(x, y)| (x - inner.x, y - inner.y))
            .collect()
    }

    /// In cells a file with no base reads `no base` on its excerpt's first
    /// row, centered in the base half left of the divider. A stack scrolled
    /// past that row reads the label nowhere.
    ///
    /// The view is shorter than the stack's thirteen rows, so the scroll holds.
    #[test]
    fn a_file_with_no_base_reads_no_base_on_its_first_row() {
        let mut h = untracked_stack(10, 30);
        h.stoat.stoatty = false;
        let top = h.render_composited();
        stack(&mut h).0.scroll = 2;
        let scrolled = h.render_composited();

        let half = {
            let ws = h.stoat.active_workspace();
            let inner = ws.panes.pane(ws.panes.focus()).area;
            review::diff_divider_x(inner).expect("a split stack") - inner.x
        };
        assert_eq!(
            (no_base_cells(&h, &top), no_base_cells(&h, &scrolled)),
            (vec![((half - 7) / 2, 1)], Vec::new()),
        );
    }

    /// Under a live scene the label is one caption run in the muted color,
    /// centered between the excerpt's left edge and the divide's hairline,
    /// with no box of its own over the base half's hatch.
    #[test]
    fn a_live_stack_draws_no_base_as_a_centered_run() {
        let mut h = untracked_stack(16, 30);
        let width = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area.width
        };
        let (runs, scale, muted) =
            paint_directly(&mut h, |state, editors, buffers, frame, scene| {
                let mut buf = Buffer::empty(Rect::new(0, 0, width, 14));
                let area = buf.area;
                paint_stack_rows(
                    state,
                    editors,
                    buffers,
                    true,
                    frame.theme.get(scope::UI_TEXT),
                    &mut UndercurlBatch::default(),
                    frame,
                    &mut buf,
                    scene,
                    0,
                    area,
                    PagePaint::Cells,
                );
                let runs: Vec<_> = decode_stream(scene.bytes())
                    .into_iter()
                    .filter_map(|command| match command {
                        Command::TextRun(run) if run.text == "no base" => {
                            Some((run.col, run.row, run.scale, run.color, run.bg))
                        },
                        _ => None,
                    })
                    .collect();
                (
                    runs,
                    frame.theme.type_scale(TypeRole::Caption),
                    paint::style_rgb(frame.theme.get(scope::UI_TEXT_MUTED).fg),
                )
            });

        let divide = (width - 1) / 2 * 16 + review::DIVIDER_RULE_X;
        let advance = text_run::advance_sixteenths(7, scale);
        assert_eq!(
            runs,
            vec![(
                ((divide - advance) / 2) as i16,
                16,
                scale,
                muted.expect("an RGB muted color"),
                None
            )],
        );
    }

    /// In cells an empty file with no base reads `no base` and `empty U+00B7 0
    /// lines` on the row under its header, each centered in its half of the
    /// row.
    #[test]
    fn an_empty_file_reads_no_base_and_empty_on_its_note_row() {
        let mut h = untracked_stack(16, 0);
        h.stoat.stoatty = false;
        let buf = h.render_composited();
        let inner = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let divider = review::diff_divider_x(inner).expect("a split stack");

        let right = "empty \u{b7} 0 lines";
        let (left_width, right_width) = (divider - inner.x, inner.right() - divider - 1);
        assert_eq!(
            [
                header_text(&buf, inner.x, left_width, inner.y + 1),
                header_text(&buf, divider + 1, right_width, inner.y + 1),
            ],
            [
                ("no base".to_string(), Some(inner.x + (left_width - 7) / 2)),
                (
                    right.to_string(),
                    Some(divider + 1 + (right_width - 15) / 2)
                ),
            ],
        );
    }

    /// Under a live scene the note row's two texts are caption runs in the
    /// muted color, each centered in its half, and the divide's hairline runs
    /// between them as a bar.
    #[test]
    fn a_live_note_row_draws_two_centered_runs_and_the_divide() {
        let mut h = untracked_stack(16, 0);
        let width = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area.width
        };
        let (runs, bars, scale) =
            paint_directly(&mut h, |state, editors, buffers, frame, scene| {
                let mut buf = Buffer::empty(Rect::new(0, 0, width, 14));
                let area = buf.area;
                paint_stack_rows(
                    state,
                    editors,
                    buffers,
                    true,
                    frame.theme.get(scope::UI_TEXT),
                    &mut UndercurlBatch::default(),
                    frame,
                    &mut buf,
                    scene,
                    0,
                    area,
                    PagePaint::Cells,
                );
                let commands = decode_stream(scene.bytes());
                let runs: Vec<(String, i16)> = commands
                    .iter()
                    .filter_map(|command| match command {
                        Command::TextRun(run) if run.row == 16 => Some((run.text.clone(), run.col)),
                        _ => None,
                    })
                    .collect();
                let bars: Vec<_> = frame_bars(commands)
                    .into_iter()
                    .filter(|bar| bar.1 == 16 && bar.2 == 1)
                    .collect();
                (runs, bars, frame.theme.type_scale(TypeRole::Caption))
            });

        let divide = (width - 1) / 2 * 16 + review::DIVIDER_RULE_X;
        let right = "empty \u{b7} 0 lines";
        let advance = |text: &str| text_run::advance_sixteenths(text.chars().count(), scale);
        let right_col = divide + (width * 16 - divide - advance(right)) / 2;
        assert_eq!(
            (runs, bars),
            (
                vec![
                    (
                        "no base".to_string(),
                        ((divide - advance("no base")) / 2) as i16
                    ),
                    (right.to_string(), right_col as i16),
                ],
                vec![(divide as i16, 16, 1, 16, HAIRLINE)],
            ),
        );
    }

    /// Under a live scene each header lies between two hairlines in the
    /// inactive border color, along its top and bottom edges, the width of the
    /// stack. The divide's hairline runs through it in the same color. The
    /// diff's mid divider below takes that color too, so the check reads the
    /// bars inside the header rows alone.
    #[test]
    fn a_header_lies_between_two_hairlines() {
        let mut h = two_files(16);
        h.stoat.render();
        let index = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).index
        };
        let in_header = |y: i16, height: u16| {
            [0, 64]
                .into_iter()
                .any(|top| y >= top && y + height as i16 <= top + 16)
        };
        let hairlines: Vec<_> = frame_bars(decode_stream(
            h.stoat.apc_scene.lane_bytes(content_lane(index)),
        ))
        .into_iter()
        .filter(|bar| bar.4 == HAIRLINE && in_header(bar.1, bar.3))
        .collect();

        assert_eq!(hairlines, header_hairlines());
    }

    /// Under a live scene a header draws its path as runs at the Ui scale from
    /// a pad past the divide's hairline, and its counts as runs at the Caption
    /// scale, the last ending a pad before the hairline. The stack is 120
    /// wide, so the hairline sits 952 sixteenths in, and the pad is 20.
    #[test]
    fn a_live_header_draws_its_path_and_counts_as_runs() {
        let mut h = two_files(16);
        h.stoat.render();
        let theme = &h.stoat.theme;
        let (ui, caption) = (
            theme.type_scale(TypeRole::Ui),
            theme.type_scale(TypeRole::Caption),
        );
        let commands = paint_directly(&mut h, |state, editors, buffers, frame, scene| {
            let mut page = Buffer::empty(Rect::new(0, 0, 120, 4));
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

        // a.txt's header is row 0, and its rows' gutter runs start on row 1.
        let runs: Vec<(i16, u16, String)> = commands
            .into_iter()
            .filter_map(|command| match command {
                Command::TextRun(run) if run.row == 0 => Some((run.col, run.scale, run.text)),
                _ => None,
            })
            .collect();
        let hairline: i16 = 59 * 16 + 8;
        let path = runs
            .iter()
            .filter(|run| run.0 > hairline)
            .min_by_key(|run| run.0)
            .map(|run| (run.0, run.1));
        let last_count = runs
            .iter()
            .filter(|run| run.0 < hairline)
            .max_by_key(|run| run.0)
            .map(|(col, scale, text)| {
                let advance = text_run::advance_sixteenths(text.chars().count(), *scale);
                (*col + advance as i16, *scale)
            });

        assert_eq!(
            (path, last_count),
            (Some((hairline + 20, ui)), Some((hairline - 20, caption))),
            "the header's runs: {runs:?}",
        );
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

    /// Two files of twelve changed lines each under a changes view `height`
    /// rows tall, numbered absolute and painted once.
    ///
    /// The files end with no newline, so no empty row follows their last
    /// lines. a.txt takes stack rows 0 to 12 and b.txt rows 13 to 25, and the
    /// cursor stands on a.txt's first row.
    fn two_long_files(height: u16) -> TestHarness {
        let base = (1..=12)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let text = (1..=12)
            .map(|n| format!("{n}!"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut h = changes_view(
            120,
            height,
            &[("a.txt", &base, &text), ("b.txt", &base, &text)],
        );
        h.stoat.settings.editor_line_numbers = Some(LineNumbers::Absolute);
        h.render_composited();
        h
    }

    /// A stack scrolled into a file's rows pins that file's header on the
    /// first row, over the stack row under it, and the rows below show the
    /// stack. A header row at the top, or a file's last row, shows in place,
    /// so two headers never stack.
    #[test]
    fn a_scrolled_stack_pins_the_header_of_the_file_under_its_top_edge() {
        let mut h = two_long_files(12);
        let cols = excerpt_columns(&h, Rect::new(0, 0, 120, 11), 2);
        let frames = [0, 3, 12, 13, 15].map(|scroll| {
            stack(&mut h).0.scroll = scroll;
            let buf = h.render_composited();
            let (state, editors) = stack(&mut h);
            (
                diff_changes::pinned_header(state, editors, scroll).file,
                buf,
            )
        });
        // The halves of the header row either side of the divider's column 59,
        // which a pinned header's divide fills.
        let header = |at: usize| {
            let buf = &frames[at].1;
            [header_text(buf, 0, 59, 0).0, header_text(buf, 60, 60, 0).0]
        };
        let number =
            |at: usize, y: u16| gutter_number(&frames[at].1, cols.right_x, cols.right_text_x, y);
        // The composite reads the live layout, where the tag's inset and the
        // gap after it span two cells.
        let named = |file: &str| {
            [
                "modified  +12 \u{2212}12 \u{b7} 1 hunk".to_string(),
                file.to_string(),
            ]
        };

        assert_eq!(
            (
                frames.each_ref().map(|(file, _)| *file),
                [header(0), header(1), header(3), header(4)],
                (number(2, 0), number(1, 1)),
            ),
            (
                [None, Some(0), None, None, Some(1)],
                [
                    named("a.txt"),
                    named("a.txt"),
                    named("b.txt"),
                    named("b.txt")
                ],
                ("12".to_string(), "4".to_string()),
            ),
        );
    }

    /// A pinned header covers its row with a panel whose body draws nothing, so
    /// a pool's composite leaves the row to the live grid, and lays its two
    /// hairlines and the divide over that panel. The panel casts its shadow on
    /// the rows under it. A header in place declares no panel on its row.
    #[test]
    fn a_pinned_header_covers_its_row_with_a_frameless_panel() {
        let mut h = two_long_files(12);
        let index = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).index
        };
        let mut first_row_lane = |scroll: usize| -> Vec<String> {
            stack(&mut h).0.scroll = scroll;
            h.stoat.render();
            decode_stream(h.stoat.apc_scene.lane_bytes(content_lane(index)))
                .into_iter()
                .flat_map(|command| match command {
                    Command::Panel(panel) if panel.top > 0 => Vec::new(),
                    Command::Panel(panel) => vec![format!(
                        "panel {},{} {}x{} {:?} {:?} {:?}",
                        panel.left,
                        panel.top,
                        panel.width,
                        panel.height,
                        panel.style,
                        panel.fill,
                        panel.shadow
                    )],
                    command => frame_bars(vec![command])
                        .into_iter()
                        .filter(|bar| bar.4 == HAIRLINE && bar.1 < 16)
                        .map(|bar| match bar.3 {
                            1 => format!("hairline {}", bar.1),
                            _ => format!("divide {}", bar.0),
                        })
                        .collect(),
                })
                .collect()
        };

        let rules = ["hairline 0", "hairline 15", "divide 952"].map(String::from);
        assert_eq!(
            (first_row_lane(3), first_row_lane(0)),
            (
                [
                    vec!["panel 0,0 120x1 None_ None Cast".to_string()],
                    rules.to_vec()
                ]
                .concat(),
                rules.to_vec(),
            ),
        );
    }

    /// A view under eight rows pins nothing, so its first row shows the stack
    /// row under the scroll.
    #[test]
    fn a_viewport_under_eight_rows_pins_nothing() {
        let mut h = two_long_files(8);
        stack(&mut h).0.scroll = 3;
        let buf = h.render_composited();
        let cols = excerpt_columns(&h, Rect::new(0, 0, 120, 7), 2);

        assert_eq!(
            (
                {
                    let (state, editors) = stack(&mut h);
                    diff_changes::pinned_header(state, editors, 3).file
                },
                gutter_number(&buf, cols.right_x, cols.right_text_x, 0),
            ),
            (None, "3".to_string()),
        );
    }

    /// A cursor under the pinned header is off screen, so the paint records no
    /// cell for it. One row lower it records the cell under the pinned row.
    #[test]
    fn a_cursor_under_the_pinned_header_records_no_cell() {
        let mut h = two_long_files(12);
        {
            let (state, editors) = stack(&mut h);
            diff_changes::place_cursor(&mut editors[state.excerpts[0].editor_id], Point::new(4, 0));
        }
        h.render_composited();
        let mut cell_at = |scroll: usize| {
            stack(&mut h).0.scroll = scroll;
            h.render_composited();
            let (state, editors) = stack(&mut h);
            editors[state.excerpts[0].editor_id]
                .cursor_screen_cell
                .map(|(_, y)| y)
        };

        assert_eq!((cell_at(5), cell_at(4)), (None, Some(1)));
    }

    /// A held screen row under the pinned header moves to the first row under
    /// it, so the cursor shows there.
    #[test]
    fn a_held_screen_row_keeps_the_cursor_clear_of_the_pinned_header() {
        let mut h = two_long_files(12);
        {
            let (state, editors) = stack(&mut h);
            diff_changes::place_cursor(&mut editors[state.excerpts[0].editor_id], Point::new(5, 0));
            state.held_screen_row = Some(0);
        }
        h.render_composited();
        let (state, editors) = stack(&mut h);
        let cell = editors[state.excerpts[0].editor_id].cursor_screen_cell;

        assert_eq!((state.scroll, cell.map(|(_, y)| y)), (5, Some(1)));
    }

    /// The Greeter fixture under a changes view 13 rows tall, parsed, so its
    /// fold rows carry their Scopes, and painted.
    ///
    /// The stack reads the header on row 0, Fold Row A (`impl Greeter U+203A
    /// fn greet`) on row 1, buffer rows 5 to 25 on rows 2 to 22, Fold Row B
    /// (`impl Greeter U+203A fn part`) on row 23, buffer rows 32 to 52 on rows
    /// 24 to 44, and the trailing fold on row 45, in a view of twelve rows.
    fn greeter_stack() -> TestHarness {
        let (base, text) = crate::test_harness::greeter_rust_file();
        parsed_stack(&[("a.rs", &base, &text)])
    }

    /// `files` under a changes view 13 rows tall, parsed, so their fold rows
    /// carry their Scopes, and painted.
    fn parsed_stack(files: &[(&str, &str, &str)]) -> TestHarness {
        let mut h = changes_view(120, 13, files);
        // The paint asks for the parse, and the refold after it lands reads
        // the Scopes.
        h.render_composited();
        for _ in 0..2 {
            h.stoat.drive_background();
            h.settle();
        }
        h.render_composited();
        h
    }

    /// An excerpt whose hunk opens under its header pins its bare Scope row's
    /// Scope. The pinned Scope gives way on the excerpt's last row, since no
    /// row of its section shows under that row.
    ///
    /// The function's whole body changes, so the excerpt takes the header, the
    /// bare Scope row, and its 23 rows with the empty row after the final
    /// newline, and folds nothing.
    #[test]
    fn a_bare_scope_row_pins_until_the_excerpt_ends() {
        let file = |edit: bool| {
            let body: String = (1..=20)
                .map(|n| match edit {
                    true => format!("    let x{n} = {n} + 1;\n"),
                    false => format!("    let x{n} = {n};\n"),
                })
                .collect();
            format!("fn greet() {{\n{body}}}\n")
        };
        let mut h = parsed_stack(&[("a.rs", &file(false), &file(true))]);
        let (state, editors) = stack(&mut h);
        let rows = state.rows;
        let pinned = [2, rows - 3, rows - 2].map(|scroll| {
            let pinned = diff_changes::pinned_header(state, editors, scroll);
            (pinned.file, pinned.scope.map(|scope| scope.row))
        });

        assert_eq!(
            (rows, pinned),
            (
                25,
                [(Some(0), Some(1)), (Some(0), Some(1)), (Some(0), None)]
            ),
        );
    }

    /// A fold row over a hunk outside every item names no Scope, so it pins
    /// nothing, and the stack rows under it show in place.
    ///
    /// The function's whole body changes, and so does a `use` line with 12
    /// comment lines on each side. The bare Scope row names the function, and
    /// the fold row above the `use` line names no item.
    #[test]
    fn a_top_level_fold_row_pins_no_scope() {
        let file = |edit: bool| {
            let body: String = (1..=20)
                .map(|n| match edit {
                    true => format!("    let x{n} = {n} + 1;\n"),
                    false => format!("    let x{n} = {n};\n"),
                })
                .collect();
            let comments =
                |from: u32| -> String { (from..from + 12).map(|n| format!("// {n}\n")).collect() };
            let path = match edit {
                true => "a::C",
                false => "a::B",
            };
            format!(
                "fn greet() {{\n{body}}}\n{}use {path};\n{}",
                comments(1),
                comments(13)
            )
        };
        let mut h = parsed_stack(&[("a.rs", &file(false), &file(true))]);
        let (state, editors) = stack(&mut h);
        let mut pinned: Vec<Option<usize>> = (0..state.rows)
            .map(|scroll| {
                diff_changes::pinned_header(state, editors, scroll)
                    .scope
                    .map(|scope| scope.row)
            })
            .collect();
        pinned.dedup();

        assert_eq!(
            pinned,
            [None, Some(1), None],
            "only the bare Scope row pins"
        );
    }

    /// Under the pinned header the second row pins the nearest Scope row above
    /// the stack row it covers. A Scope row on that row, or one right under
    /// it, shows in place, so the pinned Scope gives way to it.
    #[test]
    fn a_scrolled_hunk_pins_its_scope_under_the_header() {
        let mut h = greeter_stack();
        let (state, editors) = stack(&mut h);
        let pinned = [0, 1, 20, 21, 22, 23].map(|scroll| {
            let pinned = diff_changes::pinned_header(state, editors, scroll);
            (pinned.file, pinned.scope.map(|scope| scope.row))
        });

        assert_eq!(
            pinned,
            [
                (None, None),
                (Some(0), Some(1)),
                (Some(0), Some(1)),
                (Some(0), None),
                (Some(0), None),
                (Some(0), Some(23)),
            ],
        );
    }

    /// A pinned Scope draws a fold row's hatched surface under the pinned
    /// header, with the divide's hairline through it and the Scope 20
    /// sixteenths past the divide. It draws no Fold Button, count, or range.
    /// The cover spans both pinned rows.
    #[test]
    fn a_pinned_scope_draws_its_scope_past_the_divide() {
        let mut h = greeter_stack();
        let index = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).index
        };
        stack(&mut h).0.scroll = 20;
        h.stoat.render();
        let commands = decode_stream(h.stoat.apc_scene.lane_bytes(content_lane(index)));
        let covers: Vec<(u16, u16, u16, u16)> = commands
            .iter()
            .filter_map(|command| match command {
                Command::Panel(panel) => Some((panel.left, panel.top, panel.width, panel.height)),
                _ => None,
            })
            .collect();
        let bars: Vec<_> = frame_bars(commands.clone())
            .into_iter()
            .filter(|bar| (16..32).contains(&bar.1))
            .collect();
        let mut runs: Vec<(i16, String)> = commands
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run) if (16..32).contains(&run.row) => {
                    Some((run.col, run.text.clone()))
                },
                _ => None,
            })
            .collect();
        runs.sort();
        let first = runs.first().map(|run| run.0);
        let text: String = runs.into_iter().map(|(_, text)| text).collect();
        let row_cells = cells(&h.render_composited(), 0, 120, 1);
        let fold = paint::style_rgb(h.stoat.theme.get(scope::DIFF_FOLD).bg).expect("an RGB fold");

        let divide = 59 * 16 + 8;
        // The composite draws the hatched surface as rules across the row's
        // blank cells, so the Scope's text is all the row reads past them.
        assert_eq!(
            (
                covers,
                bars,
                first,
                text,
                row_cells.trim_matches(['└', '┘', '─'])
            ),
            (
                vec![(0, 0, 120, 2)],
                vec![(0, 16, 1920, 16, fold), (divide, 16, 1, 16, HAIRLINE)],
                Some(divide + 20),
                "impl Greeter \u{203a} fn greet".to_string(),
                "impl Greeter \u{203a} fn greet",
            ),
        );
    }

    /// A cursor under the pinned Scope is off screen, and an uncover scrolls
    /// the stack up until its row shows under the pinned rows.
    #[test]
    fn a_cursor_under_the_pinned_scope_moves_the_stack() {
        let mut h = greeter_stack();
        {
            let (state, editors) = stack(&mut h);
            diff_changes::place_cursor(
                &mut editors[state.excerpts[0].editor_id],
                Point::new(24, 0),
            );
        }
        h.render_composited();
        stack(&mut h).0.scroll = 20;
        h.render_composited();
        let (state, editors) = stack(&mut h);
        let cell = editors[state.excerpts[0].editor_id].cursor_screen_cell;
        state.uncover_row(editors, 21);

        assert_eq!((cell, state.scroll), (None, 19));
    }

    /// The pair names of a frame with no pair open.
    static NO_PAIRS: LazyLock<HashMap<PathBuf, (String, String)>> = LazyLock::new(HashMap::new);

    /// A frame context that paints the stack as the live frame of `stoat`
    /// paints it, under no prompt, popup, or search.
    fn live_frame(stoat: &Stoat) -> FrameCtx<'_> {
        FrameCtx {
            workspace_name: "",
            tab_bar_shown: false,
            banner_shown: false,
            chrome_bands: false,
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
            tallies: None,
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
            pane_root: Rect::default(),
            dividers: &[],
            badge_cover: None,
            hover_cell: None,
            home: None,
            run_cwd: None,
            blame: &stoat.active_workspace().blame,
            now_epoch: 0,
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
    /// its second row, and b.txt's header falls inside the run and draws its
    /// path there.
    #[test]
    fn a_run_of_stack_rows_paints_what_the_live_paint_shows() {
        let mut h = two_files(16);
        let live = h.stoat.render();
        let inner = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let (run, commands) = paint_directly(&mut h, |state, editors, buffers, frame, scene| {
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
            (run, decode_stream(scene.bytes()))
        });

        // b.txt's header falls on the run's third row, and under a live scene
        // its path draws as runs right of the divide's hairline.
        let path: String = commands
            .iter()
            .filter_map(|command| match command {
                Command::TextRun(run) if run.row == 32 && run.col > 59 * 16 + 8 => {
                    Some(run.text.as_str())
                },
                _ => None,
            })
            .collect();
        let rows = |buf: &Buffer, x: u16, y: u16| -> Vec<String> {
            (y..y + 4).map(|y| cells(buf, x, inner.width, y)).collect()
        };
        assert_eq!(
            (rows(&run, 0, 0), path),
            (rows(&live, inner.x, inner.y + 2), "b.txt".to_string()),
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
        let firsts: Vec<usize> = stack(&mut h)
            .0
            .excerpts
            .iter()
            .map(|excerpt| excerpt.top + diff_changes::header_rows(excerpt))
            .collect();
        let (mut targets, mut reached) = (Vec::new(), Vec::new());
        for (first, rows) in firsts.into_iter().zip(&numbers) {
            for (offset, number) in rows.iter().enumerate() {
                let row = first + offset;
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
            ws.panes.pane(ws.panes.focus()).area
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
    /// out as the live paint left it, so the terminal cursor, the ghost beside
    /// it, and pointer hits stay on the live rows. The page starts at a.txt's
    /// second row, where the paint lays a.txt out from another scroll row and
    /// cuts its cursor's row.
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
                        editor.ghost_cursor_cell,
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
            // The page's third row is b.txt's header, whose path draws as runs
            // right of the divide's hairline.
            decode_stream(scene.bytes())
                .into_iter()
                .filter_map(|command| match command {
                    Command::TextRun(run) if run.row == 32 && run.col > 59 * 16 + 8 => {
                        Some(run.text)
                    },
                    _ => None,
                })
                .collect::<String>()
        });

        assert_eq!(
            (
                live[0].2.is_some(),
                live[0].3.is_some(),
                header.as_str(),
                layouts(&mut h)
            ),
            (true, true, "b.txt", live.clone()),
            "the live cursor and ghost cells, the page's header path, and every layout field",
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

    /// A split changes view draws no frame around the base column. The header
    /// hairlines lead the content, and no bar marks the base half's edges.
    #[test]
    fn a_split_changes_view_draws_no_base_frame() {
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

        assert_eq!(
            (bars[..6].to_vec(), base_edge_bars(&bars)),
            (header_hairlines(), Vec::new()),
        );
    }

    /// A stack page draws no frame around the base column. The header
    /// hairlines lead its bars, and no bar marks the base half's edges.
    #[test]
    fn a_stack_page_draws_no_base_frame() {
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
            (bars[..6].to_vec(), base_edge_bars(&bars)),
            (header_hairlines(), Vec::new()),
        );
    }

    /// The hairlines along the top and bottom edges of the headers on rows 0
    /// and 4 of a stack 120 columns wide, and the divide through each header
    /// at the hairline of the stack's divider, column 59.
    fn header_hairlines() -> Vec<(i16, i16, u16, u16, [u8; 3])> {
        [0, 64]
            .into_iter()
            .flat_map(|top| {
                [
                    (0, top, 1920, 1, HAIRLINE),
                    (0, top + 15, 1920, 1, HAIRLINE),
                    (59 * 16 + 8, top, 1, 16, HAIRLINE),
                ]
            })
            .collect()
    }

    /// The bars of `bars` that mark the edges of a 120-wide stack's base half.
    ///
    /// A side is 6 sixteenths wide, at the half's left edge or a sixteenth
    /// short of the divider's hairline, which sits 952 sixteenths in. A top is
    /// 951 sixteenths wide from the left edge. The gutter's status bars, also
    /// 6 sixteenths wide, start at 5 and 14, past the half's pad.
    fn base_edge_bars(
        bars: &[(i16, i16, u16, u16, [u8; 3])],
    ) -> Vec<(i16, i16, u16, u16, [u8; 3])> {
        bars.iter()
            .copied()
            .filter(|bar| matches!((bar.0, bar.2), (0 | 945, 6) | (0, 951)))
            .collect()
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
        // The changes view keeps its hints box up over the lower right, across
        // the strip's lower rows.
        h.stoat.screen_hints_hidden = true;
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

    /// The workspace's changes view strip as `strip_id` declares it, in the
    /// class table's palette blended toward the background by `dim`.
    fn diff_changes_strip(h: &TestHarness, strip_id: u32, dim: f32) -> Declared {
        let bg = paint::style_rgb(h.stoat.theme.get(scope::UI_BACKGROUND).bg)
            .expect("the test theme has an rgb background");
        let palette: Vec<_> = h
            .stoat
            .minimap_class_table
            .palette()
            .iter()
            .map(|&color| paint::dim_rgb(color, bg, dim))
            .collect();
        let content_id = h.stoat.diff_changes_strips[&h.stoat.active_workspace].content_id();
        (strip_id, content_id, palette)
    }

    fn focused_pane_index(h: &TestHarness) -> u32 {
        let ws = h.stoat.active_workspace();
        ws.panes.pane(ws.panes.focus()).index
    }

    /// The width of the single band the last paint reserved, `None` with no
    /// band.
    fn band_width(h: &TestHarness) -> Option<u16> {
        h.stoat.single_minimap_rect.map(|band| band.width)
    }

    /// A per-pane changes view gives the seven cells at its right edge to its
    /// minimap strip, leaves them blank, and declares the strip in the syntax
    /// palette.
    #[test]
    fn a_per_pane_changes_view_declares_a_seven_cell_strip() {
        let (mut h, mut rx) = strip_harness(MinimapMode::PerPane);
        let (buf, reserved, declared) = paint_strips(&mut h, &mut rx);
        let strip = Rect::new(113, 0, 7, 23);
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
    /// strip at the inactive dim.
    #[test]
    fn an_unfocused_per_pane_changes_view_dims_its_strip() {
        let (mut h, mut rx) = strip_harness(MinimapMode::PerPane);
        let index = focused_pane_index(&h);
        focus_hidden_dock(&mut h);

        let (_, _, declared) = paint_strips(&mut h, &mut rx);
        let dim = h.stoat.painted_inactive_dim();
        assert_eq!(declared, [diff_changes_strip(&h, index, dim)]);
    }

    /// The single band shows a focused changes view's strip at its seven
    /// cells, and the pane reserves no strip of its own.
    #[test]
    fn the_single_band_follows_a_focused_changes_view() {
        let (mut h, mut rx) = strip_harness(MinimapMode::Single);
        let (_, reserved, declared) = paint_strips(&mut h, &mut rx);
        assert_eq!(
            (reserved, band_width(&h), declared),
            (
                None,
                Some(7),
                vec![diff_changes_strip(&h, SINGLE_MINIMAP_STRIP_ID, 0.0)]
            ),
        );
    }

    /// The single band keeps its seven cells over a changes view whether the
    /// view is the widened pane or not, so a widen never reflows the panes.
    #[test]
    fn the_single_band_keeps_its_width_over_a_changes_view() {
        let (mut h, mut rx) = strip_harness(MinimapMode::Single);
        let (_, _, declared) = paint_strips(&mut h, &mut rx);
        let widened = band_width(&h);
        h.stoat.active_workspace_mut().panes.unwiden();
        paint_strips(&mut h, &mut rx);
        assert_eq!(
            (declared, [widened, band_width(&h)]),
            (
                vec![diff_changes_strip(&h, SINGLE_MINIMAP_STRIP_ID, 0.0)],
                [Some(7), Some(7)]
            ),
        );
    }

    /// A dial step sends no band over a changes view and leaves the band at
    /// its seven cells. The strip takes the syntax palette, which no dial
    /// moves.
    #[test]
    fn a_dial_step_leaves_the_single_band_over_a_changes_view() {
        let (mut h, mut rx) = strip_harness(MinimapMode::Single);
        let (_, _, before) = paint_strips(&mut h, &mut rx);
        let before_width = band_width(&h);
        h.stoat.diff_tint = 2;
        let (_, _, stepped) = paint_strips(&mut h, &mut rx);
        assert_eq!(
            (before, [before_width, band_width(&h)], stepped),
            (
                vec![diff_changes_strip(&h, SINGLE_MINIMAP_STRIP_ID, 0.0)],
                [Some(7), Some(7)],
                vec![]
            ),
        );
    }
}
