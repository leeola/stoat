//! Mouse routing: which surface a click lands on, and what it means there.
//!
//! A press arrives as a screen cell and has to become an action on whatever
//! occupies that cell -- a modal, a minimap strip, a pane divider, a terminal
//! grid, a line of text. The hit tests here walk the same layout the paint
//! produced, so a click resolves to the surface the user saw rather than to
//! whatever the model currently holds.

use crate::{
    action_handlers::{self, commits::CommitStep, diff_changes::StackPress, rebase::RebaseMove},
    app::{
        modal_split_share, modal_zoom_steps, ModalKind, PanelHit, SeparatorAxis, SplitSeparator,
        Stoat, UpdateEffect, MIN_PREVIEW_ROWS, MODAL_ZOOM_MAX, MODAL_ZOOM_MIN, PREVIEW_WHEEL_ROWS,
        WHEEL_BINDING_COOLDOWN,
    },
    buffer::BufferId,
    commit_preview,
    diff_changes::{self, DiffChangesId, DiffChangesState},
    display_map::DisplaySnapshot,
    editor_state::{EditorId, EditorState, ScrollGlide},
    host::ClipboardKind,
    jobs,
    keymap::WheelDirection,
    keymap_state::{active_modal, ActiveModal, StoatKeymapState},
    merge_view::{ChunkState, Side},
    minimap::{
        density::{self, Density, Tier},
        diff_view::diff_view_window,
        emit::minimap_view_window,
        LINES_PER_CELL,
    },
    pane::{FocusTarget, Placement, View},
    picker::Preview,
    rebase::RebasePause,
    render::{
        commit_picker::MIN_LIST_ROWS,
        conflict_view::{ConflictHover, ConflictTarget},
        pixel_rows::RowMetrics,
        TypeScale,
    },
    run::{CmdId, GridSelection},
    term_session::TermId,
    theme::TypeRole,
    workspace::Workspace,
};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use slotmap::SlotMap;
use std::ops::Range;
use stoat_action::{ActionKind, GitStatusOpen};
use stoat_config::MinimapMode;
use stoat_text::{Bias, SelectionGoal};
use stoat_widgets::minimap;
use stoatty_protocol::window_ipc::{MouseButton as IpcMouseButton, MouseKind};

/// Rows one wheel notch moves a terminal or agent pane's history, the step the
/// run pane's scrollback takes.
const TERM_WHEEL_ROWS: usize = 3;

/// Map an aux window's pointer gesture onto the crossterm event kind the pane
/// mouse handlers dispatch on. A wheel becomes a scroll. A button gesture keeps
/// its button.
///
/// [`None`] for the side buttons, which crossterm does not model.
/// [`Stoat::handle_side_buttons`] takes every gesture of those before this
/// runs, so the case does not arise in practice.
pub(crate) fn mouse_event_kind(kind: MouseKind) -> Option<MouseEventKind> {
    Some(match kind {
        MouseKind::Press(button) => MouseEventKind::Down(mouse_button(button)?),
        MouseKind::Release(button) => MouseEventKind::Up(mouse_button(button)?),
        MouseKind::Drag(button) => MouseEventKind::Drag(mouse_button(button)?),
        MouseKind::WheelUp => MouseEventKind::ScrollUp,
        MouseKind::WheelDown => MouseEventKind::ScrollDown,
    })
}

pub(crate) fn mouse_button(button: IpcMouseButton) -> Option<MouseButton> {
    match button {
        IpcMouseButton::Left => Some(MouseButton::Left),
        IpcMouseButton::Middle => Some(MouseButton::Middle),
        IpcMouseButton::Right => Some(MouseButton::Right),
        IpcMouseButton::Back | IpcMouseButton::Forward => None,
    }
}

/// Where an open boxed modal's list and preview sit, and how far the list runs.
///
/// Hit tests read this rather than each modal's own layout call, so one
/// pointer contract covers every one of them. The rects come from the same
/// layout functions the paint uses, so a clicked row always names the row
/// drawn there.
pub struct ModalSurfaces {
    /// Rows the list occupies. A press here resolves to a row index.
    pub list: Rect,
    /// The preview or detail pane, when the modal shows one. A wheel here
    /// scrolls that pane instead of stepping the list, and the pooled page
    /// covers exactly this rect.
    pub(crate) preview: Option<Rect>,
    /// Rows the list currently holds, which bounds a resolved index.
    len: usize,
    /// The selected row, which fixes the list's scroll window.
    selected: usize,
    /// How the list lays its results out in rows.
    rows: ListRows,
    /// The pool that pages the list, whose `pool_mouse` report names the pixel
    /// under the pointer in a pixel list, or `None` for a list that no such
    /// pool pages.
    pool: Option<u32>,
}

/// How a modal's list lays its results out in rows, which is what a press
/// reads to name the result under the pointer.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ListRows {
    /// One row per result, in the window that `window_start` derives.
    OnePerResult,
    /// The palette's command list, one row per command, which a pixel list
    /// lays out in the window that `command_window` derives.
    Commands,
    /// The file finder's results, whose long labels wrap onto more rows.
    Finder,
    /// The palette argument picker's results, which wrap as the finder's do.
    PaletteArg,
    /// A config command's pick list or `cli` list, one row per result, which
    /// a pixel list lays out one row pitch apart in the window that
    /// `window_start` derives from the rows that fit.
    ArgRows,
    /// The Job List's rows, one per result, which a pixel list lays out one
    /// row pitch apart under the rule of its column header, in the window
    /// that `window_start` derives from the rows on show.
    JobRows,
}

impl ModalSurfaces {
    /// The result that `mouse` lands on, or [`None`] off the list and past its
    /// last result.
    ///
    /// The window comes from the same function the paint uses, so the resolved
    /// result is the one drawn under the pointer. A press on any row of a
    /// wrapped result names that result.
    ///
    /// A pixel list's rows stand off the cell grid, so a press there names the
    /// result under the pointer's pixel row, which its pool's `pool_mouse`
    /// report carries. A press with no such report, as from a terminal that
    /// reports grid cells, names the result under the middle pixel row of the
    /// cell it lands on.
    fn row_at(&self, stoat: &Stoat, mouse: MouseEvent) -> Option<usize> {
        if !self.list.contains(Position::new(mouse.column, mouse.row)) {
            return None;
        }
        let row = mouse.row - self.list.y;
        let home = stoat.home.as_deref();
        let (rows, pixels) = match self.rows {
            ListRows::OnePerResult => return self.one_per_result(row),
            ListRows::Commands => {
                let palette = stoat.command_palette.as_ref()?;
                let Some(metrics) = palette.row_metrics else {
                    return self.one_per_result(row);
                };
                let window = crate::render::command_palette::command_window(
                    &palette.filtered,
                    palette.selected,
                    self.list.height,
                    metrics,
                );
                return window.result_at(pointer_pixel_row(stoat, self.pool, row, metrics));
            },
            ListRows::ArgRows => {
                let palette = stoat.command_palette.as_ref()?;
                let Some(metrics) = palette.row_metrics else {
                    return self.one_per_result(row);
                };
                let fit = metrics.fit(self.list.height);
                let start = crate::render::picker::window_start(self.selected, fit);
                let pixel = pointer_pixel_row(stoat, self.pool, row, metrics);
                let shown = (pixel / metrics.pitch(1)) as usize;
                let index = start + shown;
                return (shown < fit && index < self.len).then_some(index);
            },
            ListRows::JobRows => {
                let list = stoat.job_list.as_ref()?;
                let Some(metrics) = list.row_metrics else {
                    return self.one_per_result(row);
                };
                let fit = list.viewport_rows.unwrap_or(1);
                let start = crate::render::picker::window_start(self.selected, fit);
                let pixel = pointer_pixel_row(stoat, self.pool, row, metrics);
                let shown = crate::render::job_list::pixel_row_at(metrics, pixel)?;
                let index = start + shown;
                return (shown < fit && index < self.len).then_some(index);
            },
            ListRows::Finder => {
                let finder = stoat.file_finder.as_ref()?;
                let rows = crate::render::file_finder::finder_rows(finder, home);
                (rows, finder.row_metrics)
            },
            ListRows::PaletteArg => {
                let palette = stoat.command_palette.as_ref()?;
                let picker = palette.arg_picker.as_ref()?;
                let rows = crate::render::command_palette::arg_rows(picker, home);
                (rows, palette.row_metrics)
            },
        };
        let window =
            crate::render::picker::list_window(&rows, self.list.width, self.list.height, pixels);
        let offset = match pixels {
            Some(metrics) => pointer_pixel_row(stoat, self.pool, row, metrics),
            None => u32::from(row),
        };
        window.result_at(offset)
    }

    /// The result in list row `row` of a list of one row per result, in the
    /// window that `window_start` derives.
    fn one_per_result(&self, row: u16) -> Option<usize> {
        let start = crate::render::picker::window_start(self.selected, self.list.height as usize);
        let index = start + row as usize;
        (index < self.len).then_some(index)
    }
}

/// The list and pane rects of the screen painted over the focused pane, with
/// the state a hit test needs.
///
/// A pane-overlay screen covers an editor rather than replacing it, so every
/// gesture inside its pane has to be claimed here or it reaches the editor
/// underneath. That is what this exists to prevent, and why it reports the
/// whole pane alongside the list.
pub(crate) struct ScreenSurfaces {
    /// Which screen these belong to, which decides what a click and a notch
    /// do with the row they resolve to.
    kind: ScreenKind,
    /// The pane the screen paints over. Every pointer event inside it belongs
    /// to the screen, whether or not it lands on a row.
    pane: Rect,
    /// Rows the list occupies. A press here resolves to a row index.
    list: Rect,
    /// Rows the list currently holds, which bounds a resolved index.
    len: usize,
    /// The selected row, which a click steps the selection to.
    selected: usize,
    /// First list row on screen. The commits list keeps its own scroll rather
    /// than deriving a window from the selection, so the hit test reads this.
    scroll_top: usize,
    /// The detail pane beside the list, when the screen shows one. A wheel
    /// here scrolls that pane instead of stepping the list.
    detail: Option<Rect>,
    /// The minimap strip at the list's right edge, when the screen reserves
    /// one. A left press here scrubs through the minimap routing instead of
    /// selecting a row, and a wheel acts as it does over the rows.
    strip: Option<Rect>,
}

/// A pane-overlay screen the pointer acts on.
///
/// Each one keeps its own selection and steps it through its own handler, so
/// the pointer resolves a row here and hands it to the screen that owns it.
#[derive(Copy, Clone, PartialEq, Eq)]
enum ScreenKind {
    Commits,
    DiffChanges,
    Rebase,
    RebaseConflict,
}

impl ScreenSurfaces {
    /// The list row `mouse` lands on, or [`None`] off the list and past its
    /// last row.
    fn row_at(&self, mouse: MouseEvent) -> Option<usize> {
        if !self.list.contains(Position::new(mouse.column, mouse.row)) {
            return None;
        }
        let index = self.scroll_top + (mouse.row - self.list.y) as usize;
        (index < self.len).then_some(index)
    }

    fn holds(&self, mouse: MouseEvent) -> bool {
        self.pane.contains(Position::new(mouse.column, mouse.row))
    }

    /// Whether `mouse` is a left press on the screen's minimap strip.
    fn presses_strip(&self, mouse: MouseEvent) -> bool {
        matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && self
                .strip
                .is_some_and(|strip| strip.contains(Position::new(mouse.column, mouse.row)))
    }
}

/// The pane-overlay screen in the foreground, or [`None`] when a plain pane or
/// a boxed modal is what the pointer is over.
///
/// The rects come from the same layout the paint reads, so a clicked row is
/// the row drawn under the pointer.
pub(crate) fn screen_surfaces(stoat: &mut Stoat) -> Option<ScreenSurfaces> {
    let ws = stoat.active_workspace();
    match crate::keymap_state::view_predicate(ws)? {
        "commits" => {
            let state = ws.focused_commits()?;
            let pane = ws.panes.pane(ws.panes.focus()).area;
            Some(ScreenSurfaces {
                kind: ScreenKind::Commits,
                pane,
                list: crate::render::commits::commits_list_rect(pane, stoat.commits_split)?,
                len: state.commits.len(),
                selected: state.selected,
                scroll_top: state.scroll_top,
                detail: crate::render::commits::commits_detail_rect(pane, stoat.commits_split),
                strip: None,
            })
        },
        "diff_changes" => {
            let ws = stoat.active_workspace_mut();
            let id = ws.focused_diff_changes_id()?;
            let pane = ws.panes.pane(ws.panes.focus()).area;
            let Workspace {
                diff_changes_views,
                editors,
                ..
            } = ws;
            let state = diff_changes_views.get_mut(id)?;
            let len = diff_changes::layout_stack(state, editors);
            Some(ScreenSurfaces {
                kind: ScreenKind::DiffChanges,
                pane,
                list: pane,
                len,
                selected: diff_changes::cursor_stack_row(state, editors).unwrap_or(0),
                scroll_top: state.scroll,
                // The stack is the whole screen, with no detail pane beside it
                // for a notch to land in.
                detail: None,
                strip: state.minimap_rect,
            })
        },
        "rebase" => {
            let state = ws.rebase.as_ref()?;
            let pane = ws.panes.pane(ws.panes.focus()).area;
            Some(ScreenSurfaces {
                kind: ScreenKind::Rebase,
                pane,
                list: crate::render::rebase::rebase_list_rect(pane)?,
                len: state.todo.len(),
                selected: state.selected,
                scroll_top: state.scroll_top,
                // The plan is the whole screen. There is no second pane for a
                // notch to land in.
                detail: None,
                strip: None,
            })
        },
        "rebase_conflict" => {
            let RebasePause::Conflict {
                files, selected, ..
            } = ws.rebase_active.as_ref()?.pause.as_ref()?
            else {
                return None;
            };
            let pane = ws.panes.pane(ws.panes.focus()).area;
            Some(ScreenSurfaces {
                kind: ScreenKind::RebaseConflict,
                pane,
                list: crate::render::conflict::conflict_list_rect(pane)?,
                len: files.len(),
                selected: *selected,
                // The file list paints from its first entry and stops at the
                // pane's bottom, so there is no window to offset by.
                scroll_top: 0,
                detail: None,
                strip: None,
            })
        },
        _ => None,
    }
}

/// Act on a press over a pane-overlay screen, or report that none is open.
///
/// [`None`] leaves the event to the routing below it, which is what keeps a
/// divider drag and the other split panes working while a screen is up.
/// [`Some`] claims the event, so nothing reaches the editor the screen covers.
///
/// A left press on the screen's minimap strip, and every event while a strip
/// scrub is armed, also go to that routing. A scrub owns its gesture wherever
/// the pointer goes.
///
/// On the changes view, a middle press puts the cursor on the pressed cell and
/// runs goto definition. A right press puts the cursor there and runs hover.
/// The view's excerpts are editors, so the buttons act as they do in an editor.
/// A left, middle, or right press on a fold row's Fold Button opens the fold
/// and runs neither. A press on a File Header folds or opens its file and runs
/// neither too.
fn handle_screen_mouse(stoat: &mut Stoat, mouse: MouseEvent) -> Option<UpdateEffect> {
    let surfaces = screen_surfaces(stoat).filter(|s| s.holds(mouse))?;
    if stoat.minimap_drag.is_some() || surfaces.presses_strip(mouse) {
        return None;
    }

    // A left press selects the row without acting on it. The user asked for
    // select rather than activate, so a second press on the same row is a
    // no-op too, unlike the modal two-step.
    let MouseEventKind::Down(button) = mouse.kind else {
        return Some(UpdateEffect::None);
    };
    let Some(index) = surfaces.row_at(mouse) else {
        return Some(UpdateEffect::None);
    };

    Some(match surfaces.kind {
        ScreenKind::DiffChanges => {
            match action_handlers::diff_changes::diff_changes_press(stoat, index, mouse.column) {
                StackPress::Missed => return Some(UpdateEffect::None),
                StackPress::Opened | StackPress::Folded => return Some(UpdateEffect::Redraw),
                StackPress::Placed => {},
            }
            match button {
                MouseButton::Left => {},
                MouseButton::Middle => {
                    action_handlers::lsp::goto_definition(stoat);
                },
                MouseButton::Right => {
                    crate::lsp::hover::hover(stoat);
                },
            }
            UpdateEffect::Redraw
        },
        _ if button != MouseButton::Left => UpdateEffect::None,
        ScreenKind::Commits => {
            let delta = index as isize - surfaces.selected as isize;
            let step = match delta < 0 {
                true => CommitStep::Up(delta.unsigned_abs()),
                false => CommitStep::Down(delta as usize),
            };
            action_handlers::commits::commits_step(stoat, step)
        },
        ScreenKind::Rebase => action_handlers::rebase::rebase_select(stoat, index),
        ScreenKind::RebaseConflict => action_handlers::conflict::conflict_select(stoat, index),
    })
}

/// Rows the open modal's preview pane lays its source out in, or [`None`]
/// without one.
///
/// The keyboard's preview scrolling sizes its step from this, so a half-pane
/// key step and a wheel over the same pane read one geometry. A pane at
/// Preview Scale lays out the scaled rows its last render recorded. The commit
/// picker's diff records no layout, and it lays out the scaled grid of its rect
/// whenever the terminal draws scaled pools.
pub fn modal_preview_rows(stoat: &Stoat) -> Option<usize> {
    let rect = boxed_modal_surfaces(stoat).and_then(|surfaces| surfaces.preview)?;
    let scaled_rows = match active_modal(stoat) {
        Some(ActiveModal::CommitPicker) => {
            crate::render::picker::preview_grid(rect, stoat.scaled_cell(), [0; 4])
                .map(|(rows, _)| rows)
        },
        _ => modal_preview_pane(stoat)
            .and_then(|preview| preview.scaled)
            .map(|scaled| scaled.rows),
    };
    Some(scaled_rows.unwrap_or(rect.height) as usize)
}

/// The open modal's preview pane, or [`None`] when no modal is open or its
/// preview is not an editor.
///
/// Every target list previews through a scratch editor, so a wheel, a keyboard
/// half-page, and a pooled page all want the same one. The commit picker and
/// help keep their own row offset instead and are absent here.
/// Each caller handles those where it handles this.
pub(crate) fn modal_preview_pane(stoat: &Stoat) -> Option<&Preview> {
    match active_modal(stoat)? {
        ActiveModal::FileFinder => stoat
            .file_finder
            .as_ref()
            .map(|f| &f.active_core_ref().preview),
        ActiveModal::CodeSearch => stoat.code_search.as_ref().map(|f| &f.preview),
        ActiveModal::SymbolFinder => stoat.symbol_finder.as_ref().map(|f| &f.preview),
        ActiveModal::Palette => {
            let palette = stoat.command_palette.as_ref()?;
            match palette.pick.as_ref() {
                Some(pick) => Some(&pick.preview),
                None => palette
                    .arg_picker
                    .as_ref()
                    .map(|picker| &picker.active_core_ref().preview),
            }
        },
        ActiveModal::Location => stoat.location_picker.as_ref().map(|p| &p.preview),
        ActiveModal::Diagnostics => stoat.diagnostics_picker.as_ref().map(|p| &p.picker.preview),
        ActiveModal::Jumplist => stoat.jumplist_picker.as_ref().map(|p| &p.picker.preview),
        ActiveModal::WalkthroughPicker => {
            stoat.walkthrough_picker.as_ref().map(|p| &p.picker.preview)
        },
        ActiveModal::PaneFinder => stoat.pane_finder.as_ref().map(|f| &f.picker.preview),
        _ => None,
    }
}

/// The editor backing the open modal's preview pane, as [`modal_preview_pane`]
/// names it.
pub(crate) fn modal_preview_editor(stoat: &Stoat) -> Option<EditorId> {
    modal_preview_pane(stoat).map(|preview| preview.editor)
}

/// The open boxed modal's surfaces, or [`None`] when none is open.
///
/// A boxed modal is one drawn as a bordered box over the editor. The
/// bottom-line prompts are deliberately absent. The editor stays fully visible
/// under those, so a click there still belongs to the editor.
///
/// `quit_confirm` reports no list. It owns the pointer all the same, since
/// the gate reads "some modal is open" from this returning `Some`.
pub fn boxed_modal_surfaces(stoat: &Stoat) -> Option<ModalSurfaces> {
    let size = stoat.size();
    let zoom = |kind| modal_zoom_steps(&stoat.modal_zoom, kind);
    let split = |kind| modal_split_share(&stoat.modal_split, kind);

    match active_modal(stoat)? {
        ActiveModal::FileFinder => {
            let finder = stoat.file_finder.as_ref()?;
            let layout = open_finder_layout(stoat, zoom(ModalKind::FileFinder))?;
            let core = finder.active_core_ref();
            // A query that matches no file shows no preview in the preview's
            // cells, so there is nothing to pool or scroll there.
            Some(ModalSurfaces {
                list: layout.list,
                preview: layout.preview.filter(|_| !finder.shows_no_match()),
                len: core.picklist.filtered.len(),
                selected: core.picklist.selected,
                rows: ListRows::Finder,
                pool: Some(crate::smooth_scroll::non_pane_pool::FINDER),
            })
        },
        ActiveModal::CodeSearch => {
            let finder = stoat.code_search.as_ref()?;
            // Search hits are unbounded, so code search asks for the whole area
            // through the finder's layout, exactly as its renderer does.
            let layout = crate::render::file_finder::file_finder_layout(
                size,
                (u16::MAX, u16::MAX),
                zoom(ModalKind::CodeSearch),
                split(ModalKind::CodeSearch),
            )?;
            Some(ModalSurfaces {
                list: layout.list,
                preview: layout.preview,
                len: finder.matches.len(),
                selected: finder.selected,
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        ActiveModal::SymbolFinder => {
            let finder = stoat.symbol_finder.as_ref()?;
            let (_, _, list, preview) = crate::render::symbol_finder::symbol_finder_layout(
                size,
                finder.filtered.len() as u16,
                zoom(ModalKind::SymbolFinder),
                split(ModalKind::SymbolFinder),
            )?;
            Some(ModalSurfaces {
                list,
                preview,
                len: finder.filtered.len(),
                selected: finder.selected,
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        ActiveModal::CommitPicker => {
            let picker = stoat.commit_picker.as_ref()?;
            let layout = crate::render::commit_picker::commit_picker_layout(
                size,
                picker.graph_lanes,
                zoom(ModalKind::CommitPicker),
                split(ModalKind::CommitPicker),
            )?;
            Some(ModalSurfaces {
                list: layout.list,
                preview: layout.preview,
                len: picker.filtered.len(),
                selected: picker.selected,
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        ActiveModal::Palette => {
            let palette = stoat.command_palette.as_ref()?;
            // History's rows take the height of the wrap before each age, so
            // its press and its wheel read the row map of the paint instead.
            if stoat.active_workspace().palette_history.is_walking() {
                return Some(ModalSurfaces {
                    list: Rect::default(),
                    preview: None,
                    len: 0,
                    selected: 0,
                    rows: ListRows::OnePerResult,
                    pool: None,
                });
            }
            let rows = palette.list_rows_hint();
            let zoom = zoom(ModalKind::Palette);
            if let Some(pick) = palette.pick.as_ref() {
                let layout = crate::render::command_palette::palette_filter_layout(
                    size,
                    rows,
                    zoom,
                    palette.header_rows,
                    palette.doc_rows(),
                )?;
                let (list, preview) =
                    crate::render::command_palette::pick_body(&layout, pick.spec.preview.is_some());
                return Some(ModalSurfaces {
                    list,
                    preview,
                    len: pick.list.filtered.len(),
                    selected: pick.list.selected,
                    rows: ListRows::ArgRows,
                    pool: Some(crate::smooth_scroll::non_pane_pool::PALETTE),
                });
            }
            if let Some((_, _, list)) = palette.cli_list.as_ref() {
                let layout = crate::render::command_palette::palette_filter_layout(
                    size,
                    rows,
                    zoom,
                    palette.header_rows,
                    palette.doc_rows(),
                )?;
                return Some(ModalSurfaces {
                    list: layout.list,
                    preview: None,
                    len: list.filtered.len(),
                    selected: list.selected,
                    rows: ListRows::ArgRows,
                    pool: Some(crate::smooth_scroll::non_pane_pool::PALETTE),
                });
            }
            if palette.command.is_none() {
                let layout = crate::render::command_palette::palette_filter_layout(
                    size,
                    rows,
                    zoom,
                    palette.header_rows,
                    palette.doc_rows(),
                )?;
                return Some(ModalSurfaces {
                    list: layout.list,
                    preview: None,
                    len: palette.filtered.len(),
                    selected: palette.selected,
                    rows: ListRows::Commands,
                    pool: Some(crate::smooth_scroll::non_pane_pool::PALETTE),
                });
            }
            let picker = palette
                .arg_picker
                .as_ref()
                .filter(|_| palette.arg_source().is_some())?;
            let layout = open_palette_arg_layout(stoat, zoom)?;
            let core = picker.active_core_ref();
            Some(ModalSurfaces {
                list: layout.list,
                preview: layout.preview,
                len: core.picklist.filtered.len(),
                selected: core.picklist.selected,
                rows: ListRows::PaletteArg,
                pool: Some(crate::smooth_scroll::non_pane_pool::PALETTE),
            })
        },
        ActiveModal::Help => {
            let help = stoat.help.as_ref()?;
            let layout = crate::render::help::help_layout(
                size,
                crate::render::help::help_content_rows(help),
                zoom(ModalKind::Help),
            )?;
            Some(ModalSurfaces {
                list: layout.list,
                preview: Some(layout.detail),
                len: help.filtered().len(),
                selected: help.selected(),
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        ActiveModal::Jumplist => {
            let picker = stoat.jumplist_picker.as_ref()?;
            let layout = crate::render::picker::target_picker_layout(
                size,
                zoom(ModalKind::JumplistPicker),
                split(ModalKind::JumplistPicker),
            )?;
            Some(ModalSurfaces {
                list: layout.list,
                preview: layout.preview,
                len: picker.filtered().len(),
                selected: picker.selected(),
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        ActiveModal::WalkthroughPicker => {
            let picker = stoat.walkthrough_picker.as_ref()?;
            let layout = crate::render::picker::target_picker_layout(
                size,
                zoom(ModalKind::WalkthroughPicker),
                split(ModalKind::WalkthroughPicker),
            )?;
            Some(ModalSurfaces {
                list: layout.list,
                preview: layout.preview,
                len: picker.filtered().len(),
                selected: picker.selected(),
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        ActiveModal::PaneFinder => {
            let finder = stoat.pane_finder.as_ref()?;
            let layout = crate::render::picker::target_picker_layout(
                size,
                zoom(ModalKind::PaneFinder),
                split(ModalKind::PaneFinder),
            )?;
            Some(ModalSurfaces {
                list: layout.list,
                preview: layout.preview,
                len: finder.filtered().len(),
                selected: finder.selected(),
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        ActiveModal::Diagnostics => {
            let picker = stoat.diagnostics_picker.as_ref()?;
            let layout = crate::render::picker::target_picker_layout(
                size,
                zoom(ModalKind::DiagnosticsPicker),
                split(ModalKind::DiagnosticsPicker),
            )?;
            Some(ModalSurfaces {
                list: layout.list,
                preview: layout.preview,
                len: picker.filtered().len(),
                selected: picker.selected(),
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        ActiveModal::Location => {
            let picker = stoat.location_picker.as_ref()?;
            let layout = crate::render::picker::target_picker_layout(
                size,
                zoom(ModalKind::LocationPicker),
                split(ModalKind::LocationPicker),
            )?;
            Some(ModalSurfaces {
                list: layout.list,
                preview: layout.preview,
                len: picker.filtered().len(),
                selected: picker.selected(),
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        ActiveModal::WorkspacePicker => {
            let picker = stoat.workspace_picker.as_ref()?;
            let (_, inner) = crate::render::workspace_picker::workspace_picker_layout(
                size,
                picker.entries().len(),
            )?;
            Some(ModalSurfaces {
                list: crate::render::workspace_picker::workspace_picker_list(inner),
                preview: None,
                len: picker.filtered().len(),
                selected: picker.selected(),
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        ActiveModal::JobList => {
            let list = stoat.job_list.as_ref()?;
            let layout = crate::render::job_list::job_list_layout(
                size,
                list.entries().len(),
                list.row_metrics,
            )?;
            Some(ModalSurfaces {
                list: layout.rows,
                preview: None,
                len: list.filtered().len(),
                selected: list.selected(),
                rows: ListRows::JobRows,
                pool: layout
                    .pixels
                    .map(|_| crate::smooth_scroll::non_pane_pool::JOB_LIST),
            })
        },
        // The panel's rows interleave its section heads, so no list of one
        // row per line maps a press to its file, and `press_git_status_row`
        // maps it instead. It owns the pointer all the same, so a press never
        // reaches the panes it covers.
        ActiveModal::GitStatus => Some(ModalSurfaces {
            list: Rect::new(0, 0, 0, 0),
            preview: None,
            len: 0,
            selected: 0,
            rows: ListRows::OnePerResult,
            pool: None,
        }),
        // The patch is the one surface the preview scrolls. The box has no list,
        // so a press or a wheel off the patch does nothing.
        ActiveModal::CommitPreview => Some(ModalSurfaces {
            list: Rect::new(0, 0, 0, 0),
            preview: Some(
                crate::render::commit_preview::commit_preview_layout(size, stoat.tab_bar_rows())?
                    .body,
            ),
            len: 0,
            selected: 0,
            rows: ListRows::OnePerResult,
            pool: None,
        }),
        // The Glance holds no list. It owns the pointer, so a press never
        // reaches the panes it covers, and `press_over_glance` reads a press on
        // a Tray Chip.
        ActiveModal::JobGlance => Some(ModalSurfaces {
            list: Rect::new(0, 0, 0, 0),
            preview: None,
            len: 0,
            selected: 0,
            rows: ListRows::OnePerResult,
            pool: None,
        }),
        // Nothing to point at. It owns the pointer all the same, so a press
        // never reaches the panes it covers, and keys alone answer it.
        ActiveModal::QuitConfirm | ActiveModal::CheckoutConfirm | ActiveModal::RunBlock => {
            Some(ModalSurfaces {
                list: Rect::new(0, 0, 0, 0),
                preview: None,
                len: 0,
                selected: 0,
                rows: ListRows::OnePerResult,
                pool: None,
            })
        },
        // The bottom-line prompts leave the editor visible, so the pointer
        // stays the editor's.
        ActiveModal::Rename
        | ActiveModal::Search
        | ActiveModal::SplitSelection
        | ActiveModal::FilterSelections
        | ActiveModal::ShellInput => None,
    }
}

/// Route a pointer event to the open finder, palette, or commit picker.
///
/// Two gestures act. A left press on the modal's list/preview separator arms
/// a drag, and each following `Drag(Left)` moves that separator until the
/// release. A left press elsewhere selects the clicked list row, where the
/// modal has one. Everything else returns [`UpdateEffect::None`], swallowed
/// so the buffer beneath keeps its cursor and focus rather than reacting to
/// a click on a modal covering it. A press outside the list rect, or on an
/// empty row past the last filtered item, is a swallowed no-op too.
fn handle_modal_mouse(stoat: &mut Stoat, mouse: MouseEvent) -> UpdateEffect {
    match mouse.kind {
        MouseEventKind::Drag(MouseButton::Left) if stoat.modal_separator_drag.is_some() => {
            return drag_modal_separator(stoat, mouse);
        },
        MouseEventKind::Up(MouseButton::Left) => {
            stoat.modal_separator_drag = None;
            return UpdateEffect::None;
        },
        MouseEventKind::Down(MouseButton::Left) => {},
        _ => return UpdateEffect::None,
    }

    if arm_modal_separator(stoat, mouse) {
        return UpdateEffect::Redraw;
    }
    if active_modal(stoat) == Some(ActiveModal::GitStatus) {
        return press_git_status_row(stoat, mouse);
    }
    if active_modal(stoat) == Some(ActiveModal::JobGlance) {
        return press_over_glance(stoat, mouse);
    }
    if let Some(effect) = press_history_row(stoat, mouse) {
        return effect;
    }

    let Some(surfaces) = boxed_modal_surfaces(stoat) else {
        return UpdateEffect::None;
    };
    let Some(index) = surfaces.row_at(stoat, mouse) else {
        return UpdateEffect::None;
    };

    // A press on the row already selected submits it. The two-step keeps a
    // misclick from acting, and needs none of the timing machinery a
    // double-click does.
    if index == surfaces.selected {
        return action_handlers::prompt::submit_prompt_input(stoat);
    }

    action_handlers::picker::picker_step(stoat, index as i32 - surfaces.selected as i32)
}

/// Select the git status file row under a left press, or open it when the
/// press lands on the row already selected, as a picker's row takes a second
/// press.
///
/// The panel's rows interleave its section heads, so the press maps through
/// the panel's own layout rather than the one-row-per-line list that
/// [`ModalSurfaces::row_at`] reads. A press off every file row does nothing.
fn press_git_status_row(stoat: &mut Stoat, mouse: MouseEvent) -> UpdateEffect {
    let size = stoat.size();
    let top_rows = stoat.tab_bar_rows() + stoat.banner_rows();
    let status_rows = stoat.status_rows();
    let Some(panel) = stoat.git_status.as_mut() else {
        return UpdateEffect::None;
    };
    let Some(index) = crate::render::git_status::git_status_row_at(
        panel,
        size,
        top_rows,
        status_rows,
        mouse.column,
        mouse.row,
    ) else {
        return UpdateEffect::None;
    };

    if index == panel.selected {
        return action_handlers::dispatch(stoat, &GitStatusOpen);
    }
    panel.selected = index;
    UpdateEffect::Redraw
}

/// Move the Glance to the Tray Chip under a left press on the tab bar, or
/// close it for a left press anywhere else.
fn press_over_glance(stoat: &mut Stoat, mouse: MouseEvent) -> UpdateEffect {
    let bar = stoat.size();
    let on_bar = (bar.y..bar.y + stoat.tab_bar_rows()).contains(&mouse.row);
    match chip_at(stoat, mouse.column).filter(|_| on_bar) {
        Some(cmd) => jobs::show_glance(stoat, cmd),
        None => stoat.job_glance = None,
    }
    UpdateEffect::Redraw
}

/// Recall the palette's History line under a left press.
///
/// A press on the line that the walk recalls runs it, as a list row takes a
/// second press. A press off every row does nothing. Answers `None` when no
/// palette is open and while its history does not walk.
///
/// The press reads the row map that the paint keeps in
/// [`CommandPalette::history_rows`](crate::command_palette::CommandPalette::history_rows).
/// A row's height comes from the wrap before its age. The age follows the
/// clock, which moves between the paint and the press.
///
/// Pixel rows stand off the cell grid, so a press on pixel History reads the
/// spans that the paint keeps in
/// [`CommandPalette::history_spans`](crate::command_palette::CommandPalette::history_spans)
/// at the pointer's pixel row, as [`pointer_pixel_row`] finds it.
fn press_history_row(stoat: &mut Stoat, mouse: MouseEvent) -> Option<UpdateEffect> {
    let palette = stoat.command_palette.as_ref()?;
    let history = &stoat.active_workspace().palette_history;
    if !history.is_walking() {
        return None;
    }
    let at = Position::new(mouse.column, mouse.row);
    let pressed = match (palette.history_body, palette.row_metrics) {
        (Some(body), Some(metrics)) => {
            let pool = Some(crate::smooth_scroll::non_pane_pool::PALETTE);
            body.contains(at)
                .then(|| pointer_pixel_row(stoat, pool, mouse.row - body.y, metrics))
                .and_then(|pixel| {
                    palette
                        .history_spans
                        .iter()
                        .find(|(span, _)| span.contains(&pixel))
                })
                .map(|&(_, index)| index)
        },
        _ => palette
            .history_rows
            .iter()
            .find(|(rect, _)| rect.contains(at))
            .map(|&(_, index)| index),
    };
    let Some(index) = pressed else {
        return Some(UpdateEffect::None);
    };

    if history.cursor() == Some(index) {
        return Some(action_handlers::prompt::submit_prompt_input(stoat));
    }
    Some(recall_history_line(stoat, index))
}

/// The physical px row of a pixel list laid out by `metrics` that a press in
/// list row `row` names.
///
/// That is the pixel row that the list's pool `pool` reports with the press,
/// or the middle pixel row of the cell row where no report names it.
fn pointer_pixel_row(stoat: &Stoat, pool: Option<u32>, row: u16, metrics: RowMetrics) -> u32 {
    let reported = stoat
        .pool_pointer
        .filter(|&(named, ..)| Some(named) == pool)
        .map(|(_, _, pixel_row)| u32::from(pixel_row));
    reported.unwrap_or_else(|| {
        let height = u32::from(metrics.cell[1]);
        u32::from(row) * height + height / 2
    })
}

/// Step the palette's History by `steps` wheel notches in list order.
///
/// A notch down goes to the next older line, and a notch up goes to the next
/// newer line. Answers `None` when no palette is open and while its history
/// does not walk. The step holds to the newest and the oldest match, so no
/// notch leaves History, and a notch that moves nothing answers
/// [`UpdateEffect::None`].
fn wheel_history_rows(stoat: &mut Stoat, steps: i32) -> Option<UpdateEffect> {
    stoat.command_palette.as_ref()?;
    let history = &stoat.active_workspace().palette_history;
    if !history.is_walking() {
        return None;
    }
    let matches = history.matches();
    let place = history
        .cursor()
        .and_then(|cursor| matches.iter().position(|&index| index == cursor))?;
    let target = place
        .saturating_add_signed(steps as isize)
        .min(matches.len() - 1);
    if target == place {
        return Some(UpdateEffect::None);
    }
    Some(recall_history_line(stoat, matches[target]))
}

/// Move the palette's history walk to the line at `index` and write the line
/// into the palette's input.
fn recall_history_line(stoat: &mut Stoat, index: usize) -> UpdateEffect {
    let active_idx = stoat.active_workspace;
    let Some(palette) = stoat.command_palette.as_ref() else {
        return UpdateEffect::None;
    };
    let ws = &mut stoat.workspaces[active_idx];
    let Some(line) = ws.palette_history.recall(index) else {
        return UpdateEffect::None;
    };
    palette.input.replace_text(ws, &line);
    UpdateEffect::Redraw
}

/// The open File Finder's or Buffer Finder's layout at `zoom`, from the same
/// inputs its renderer reads, or `None` when no finder is open or the terminal
/// is too small to host it.
pub(crate) fn open_finder_layout(
    stoat: &Stoat,
    zoom: i8,
) -> Option<crate::render::file_finder::FinderLayout> {
    let finder = stoat.file_finder.as_ref()?;
    crate::render::file_finder::finder_layout(
        stoat.size(),
        finder,
        stoat.home.as_deref(),
        zoom,
        stoat.modal_split.get(&ModalKind::FileFinder).copied(),
        stoat.preview_scale(),
    )
}

/// The open palette's argument picker layout at `zoom`, from the same inputs
/// its renderer reads, or `None` when no argument picker is open or the
/// terminal is too small to host it.
pub(crate) fn open_palette_arg_layout(
    stoat: &Stoat,
    zoom: i8,
) -> Option<crate::render::command_palette::PaletteArgLayout> {
    let palette = stoat
        .command_palette
        .as_ref()
        .filter(|palette| palette.arg_source().is_some())?;
    crate::render::command_palette::palette_arg_layout(
        stoat.size(),
        palette,
        stoat.home.as_deref(),
        zoom,
        stoat.preview_scale(),
    )
}

/// The bordered box `kind`'s modal would occupy at `zoom`, sized from the
/// same inputs its renderer reads.
///
/// Each kind resolves its own content measurement, so the box is the one the
/// user is looking at rather than a nominal size for the family.
///
/// `None` when that modal is not open, or when the terminal is too small to
/// host it.
///
/// See also:
/// - [`modal_zoom_range`], which walks this across the zoom ledger to find the levels that actually
///   move the box.
pub fn open_modal_box(stoat: &Stoat, kind: ModalKind, zoom: i8) -> Option<Rect> {
    let size = stoat.size();
    let split = modal_split_share(&stoat.modal_split, kind);

    let modal = match kind {
        ModalKind::Help => {
            let help = stoat.help.as_ref()?;
            crate::render::help::help_layout(
                size,
                crate::render::help::help_content_rows(help),
                zoom,
            )?
            .modal
        },
        ModalKind::FileFinder => open_finder_layout(stoat, zoom)?.modal,
        ModalKind::SymbolFinder => {
            let finder = stoat.symbol_finder.as_ref()?;
            crate::render::symbol_finder::symbol_finder_layout(
                size,
                finder.content_rows,
                zoom,
                split,
            )?
            .0
        },
        // Search hits are unbounded, so code search declares the whole area
        // exactly as its renderer does.
        ModalKind::CodeSearch => {
            stoat.code_search.as_ref()?;
            crate::render::file_finder::file_finder_layout(size, (u16::MAX, u16::MAX), zoom, split)?
                .modal
        },
        // The argument picker fits its plate to its labels, as the finder
        // does, and the command list and the free-typed argument keep the
        // list modal's box.
        ModalKind::Palette => {
            let palette = stoat.command_palette.as_ref()?;
            match palette.arg_picker.is_some() && palette.arg_source().is_some() {
                true => open_palette_arg_layout(stoat, zoom)?.modal,
                false => {
                    crate::render::command_palette::palette_filter_layout(
                        size,
                        palette.list_rows_hint(),
                        zoom,
                        palette.header_rows,
                        palette.doc_rows(),
                    )?
                    .modal
                },
            }
        },
        ModalKind::CommitPicker => {
            let picker = stoat.commit_picker.as_ref()?;
            crate::render::commit_picker::commit_picker_layout(
                size,
                picker.graph_lanes,
                zoom,
                split,
            )?
            .modal
        },
        ModalKind::LocationPicker => {
            stoat.location_picker.as_ref()?;
            crate::render::picker::target_picker_layout(size, zoom, split)?.modal
        },
        ModalKind::DiagnosticsPicker => {
            stoat.diagnostics_picker.as_ref()?;
            crate::render::picker::target_picker_layout(size, zoom, split)?.modal
        },
        ModalKind::JumplistPicker => {
            stoat.jumplist_picker.as_ref()?;
            crate::render::picker::target_picker_layout(size, zoom, split)?.modal
        },
        ModalKind::WalkthroughPicker => {
            stoat.walkthrough_picker.as_ref()?;
            crate::render::picker::target_picker_layout(size, zoom, split)?.modal
        },
        ModalKind::PaneFinder => {
            stoat.pane_finder.as_ref()?;
            crate::render::picker::target_picker_layout(size, zoom, split)?.modal
        },
    };

    Some(modal)
}

/// The zoom levels that actually move `kind`'s box, as an inclusive
/// `(lo, hi)` pair.
///
/// A modal saturates against the screen well before the ledger's own
/// [`MODAL_ZOOM_MIN`]`..=`[`MODAL_ZOOM_MAX`] runs out, because
/// [`modal_box`](crate::render::chrome::modal_box) clamps the box to between
/// its minimum and the area less a thin margin. Steps past that point change
/// nothing on screen, and counting them would leave the user unwinding
/// invisible levels before the modal moved again.
///
/// `None` when there is no box to measure, or when the box is the same at
/// both ends of the ledger and there is nothing to narrow to.
pub(crate) fn modal_zoom_range(stoat: &Stoat, kind: ModalKind) -> Option<(i8, i8)> {
    let smallest = open_modal_box(stoat, kind, MODAL_ZOOM_MIN)?;
    let largest = open_modal_box(stoat, kind, MODAL_ZOOM_MAX)?;

    // Both dimensions grow monotonically with the level, so the levels
    // sharing an end's box are a contiguous run and one scan from each end
    // finds where it stops.
    let lo = (MODAL_ZOOM_MIN..=MODAL_ZOOM_MAX)
        .rev()
        .find(|&level| open_modal_box(stoat, kind, level) == Some(smallest))?;
    let hi = (MODAL_ZOOM_MIN..=MODAL_ZOOM_MAX)
        .find(|&level| open_modal_box(stoat, kind, level) == Some(largest))?;

    (lo <= hi).then_some((lo, hi))
}

/// The open modal's list/preview separator, sized from the same inputs the
/// renderer reads so a hit-test lands where the user sees the line, paired
/// with the modal it belongs to.
///
/// The kind rides beside the descriptor rather than inside it, so a drag keys
/// [`Stoat::modal_split`] while the descriptor stays pure geometry.
///
/// `None` when no modal with a separator is open, or when the one that is
/// shows no preview -- a modal too small for two panes has no line to grab.
pub fn open_modal_separator(stoat: &Stoat) -> Option<(ModalKind, SplitSeparator)> {
    let size = stoat.size();

    // The picker stacks its diff under the table, so its separator runs
    // along the modal's width and a drag redistributes rows.
    if let Some(picker) = stoat.commit_picker.as_ref() {
        let layout = crate::render::commit_picker::commit_picker_layout(
            size,
            picker.graph_lanes,
            modal_zoom_steps(&stoat.modal_zoom, ModalKind::CommitPicker),
            modal_split_share(&stoat.modal_split, ModalKind::CommitPicker),
        )?;
        let preview = layout.preview?;
        let inner = layout.inner;
        return Some((
            ModalKind::CommitPicker,
            SplitSeparator {
                axis: SeparatorAxis::Rows,
                line: layout.list.y + layout.list.height,
                span: inner.x..inner.x + inner.width,
                body: layout.list.y..preview.y + preview.height,
                min_list: MIN_LIST_ROWS,
                min_preview: MIN_PREVIEW_ROWS,
            },
        ));
    }

    // The finder family puts its preview beside the list, so the separator
    // runs down the body and a drag redistributes columns.
    let (kind, inner, list, preview) = if stoat.file_finder.is_some() {
        let kind = ModalKind::FileFinder;
        let layout = open_finder_layout(stoat, modal_zoom_steps(&stoat.modal_zoom, kind))?;
        (kind, layout.inner, layout.list, layout.preview)
    } else if stoat.code_search.is_some() {
        // Search hits are unbounded, so code search declares the whole area
        // exactly as its renderer does.
        let kind = ModalKind::CodeSearch;
        let layout = crate::render::file_finder::file_finder_layout(
            size,
            (u16::MAX, u16::MAX),
            modal_zoom_steps(&stoat.modal_zoom, kind),
            modal_split_share(&stoat.modal_split, kind),
        )?;
        (kind, layout.inner, layout.list, layout.preview)
    } else {
        let finder = stoat.symbol_finder.as_ref()?;
        let kind = ModalKind::SymbolFinder;
        let (_, inner, list, preview) = crate::render::symbol_finder::symbol_finder_layout(
            size,
            finder.content_rows,
            modal_zoom_steps(&stoat.modal_zoom, kind),
            modal_split_share(&stoat.modal_split, kind),
        )?;
        (kind, inner, list, preview)
    };
    preview?;

    Some((
        kind,
        SplitSeparator {
            axis: SeparatorAxis::Columns,
            line: list.x + list.width,
            span: list.y..list.y + list.height,
            body: inner.x..inner.x + inner.width,
            min_list: crate::render::picker::MIN_PANE_COLUMNS,
            min_preview: crate::render::picker::MIN_PANE_COLUMNS,
        },
    ))
}

/// The commits screen's list/detail separator, or `None` when that screen is
/// not the one in the foreground.
///
/// The overlay renders into the focused pane, so the separator is sized from
/// that pane's area. [`crate::keymap_state::view_predicate`] ranks the rebase
/// and conflict screens above commits, which is what keeps this from claiming
/// the pointer under one of those.
fn open_commits_separator(stoat: &Stoat) -> Option<SplitSeparator> {
    let ws = stoat.active_workspace();
    if crate::keymap_state::view_predicate(ws) != Some("commits") {
        return None;
    }
    let pane = ws.panes.pane(ws.panes.focus());
    crate::render::commits::commits_separator(pane.area, stoat.commits_split)
}

/// Arm a separator drag when `mouse` presses the open modal's list/preview
/// separator, reporting whether it did.
fn arm_modal_separator(stoat: &mut Stoat, mouse: MouseEvent) -> bool {
    let Some((kind, _)) = open_modal_separator(stoat).filter(|(_, s)| s.hit(&mouse)) else {
        return false;
    };

    stoat.modal_separator_drag = Some(kind);
    true
}

/// Move the armed separator to `mouse`, storing the share it lands on.
///
/// A modal that closed or shrank out of its two-pane layout mid-drag leaves
/// the share alone, so a stale arm can never write against a separator that
/// is no longer there.
fn drag_modal_separator(stoat: &mut Stoat, mouse: MouseEvent) -> UpdateEffect {
    let Some((kind, separator)) =
        open_modal_separator(stoat).filter(|(kind, _)| Some(*kind) == stoat.modal_separator_drag)
    else {
        return UpdateEffect::None;
    };
    let Some(share) = separator.share_at(&mouse) else {
        return UpdateEffect::None;
    };

    stoat.modal_split.insert(kind, share);
    UpdateEffect::Redraw
}

pub(crate) fn handle_mouse(stoat: &mut Stoat, mouse: MouseEvent) -> UpdateEffect {
    if matches!(
        mouse.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) {
        // A notch is one line of travel, the unit every wheel consumer here
        // already spends.
        return handle_mouse_scroll(
            stoat,
            mouse,
            match mouse.kind {
                MouseEventKind::ScrollDown => 1.0,
                _ => -1.0,
            },
        );
    }
    if let MouseEventKind::Moved = mouse.kind {
        return handle_hover(stoat, mouse.column, mouse.row, mouse.modifiers);
    }

    // Any open boxed modal owns the pointer. A left click selects a row or
    // grabs the modal's separator, and every other press, drag, or release is
    // swallowed so nothing reaches divider arming, focus, or the panes beneath
    // the modal covering them. The wheel is unaffected. handle_mouse_scroll
    // returns above this.
    if boxed_modal_surfaces(stoat).is_some() {
        return handle_modal_mouse(stoat, mouse);
    }

    // The commits separator drag owns the pointer once armed, the same way a
    // divider drag does below.
    if stoat.commits_separator_drag {
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(share) =
                    open_commits_separator(stoat).and_then(|sep| sep.share_at(&mouse))
                {
                    stoat.commits_split = Some(share);
                }
            },
            MouseEventKind::Up(MouseButton::Left) => stoat.commits_separator_drag = false,
            _ => {},
        }
        return UpdateEffect::Redraw;
    }

    // A divider drag owns the pointer once armed. It resizes on drag,
    // releases on up, and swallows the rest so pane handlers never see it.
    // Ahead of the screen gate below, because a drag that wanders over a
    // screen's pane still belongs to the divider it started on.
    if stoat.divider_drag.is_some() {
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((node, gap)) = stoat.divider_drag {
                    stoat.active_workspace_mut().panes.set_divider(
                        node,
                        gap,
                        mouse.column,
                        mouse.row,
                    );
                }
            },
            MouseEventKind::Up(MouseButton::Left) => stoat.divider_drag = None,
            _ => {},
        }
        return UpdateEffect::Redraw;
    }

    // The separator is the commits screen's own chrome, so it arms ahead of
    // the screen gate below. That gate claims the whole pane, and a press
    // swallowed there never reaches this.
    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind
        && open_commits_separator(stoat).is_some_and(|sep| sep.hit(&mouse))
    {
        stoat.commits_separator_drag = true;
        return UpdateEffect::Redraw;
    }

    // A left-button drag over an open hover popup selects its text. Routed
    // ahead of focus_at and the pane handlers so the click never reaches the
    // editor, leaving the buffer selection and cursor untouched.
    // A press outside closes the popup and falls through, so the frame still
    // owes a repaint even when nothing below the fall-through consumes the
    // press. The exits through closed_effect carry that debt.
    let mut hover_closed = false;
    if stoat.pending_hover.is_some() {
        if let Some(effect) = handle_hover_selection_mouse(stoat, mouse) {
            return effect;
        }
        hover_closed = stoat.pending_hover.is_none();
    }
    let closed_effect = |effect| match effect {
        UpdateEffect::None if hover_closed => UpdateEffect::Redraw,
        other => other,
    };

    // A pane-overlay screen owns the pointer inside the pane it paints over.
    // The editor underneath is still open, so an unclaimed press moves a
    // cursor hidden behind the screen. Outside that pane this reports None and
    // the routing below carries on, which keeps the other split panes working
    // while a screen is up. The popup's handling above runs first, because a
    // right press on the changes view opens a popup over the stack. A press
    // outside that popup closes it before this gate reads the press.
    if let Some(effect) = handle_screen_mouse(stoat, mouse) {
        return closed_effect(effect);
    }

    // A press or drag over a pane's minimap strip scrubs that pane, ahead of
    // focus_at and the text-area handlers so the strip owns the gesture.
    if let Some(effect) = handle_minimap_mouse(stoat, mouse) {
        return effect;
    }

    // A press on the tab bar's rows is the bar's. Ahead of focus_at and the pane
    // handlers, which read a row above every pane as the focused pane's first
    // row.
    if let Some(effect) = handle_tab_bar_mouse(stoat, mouse) {
        return closed_effect(effect);
    }
    if let Some(effect) = handle_banner_mouse(stoat, mouse) {
        return closed_effect(effect);
    }

    // Every button focuses the pane under the pointer, not just the left.
    // The translation below is relative to the focused pane, so a middle or
    // right click elsewhere would otherwise land in the wrong buffer.
    if let MouseEventKind::Down(button) = mouse.kind {
        if button == MouseButton::Left
            && let Some(hit) = stoat
                .active_workspace()
                .panes
                .divider_at(mouse.column, mouse.row)
        {
            stoat.divider_drag = Some(hit);
            return UpdateEffect::Redraw;
        }
        focus_at(stoat, mouse.column, mouse.row);
    }

    let menu_closed = match handle_conflict_press(stoat, mouse) {
        ConflictPress::Taken(effect) => return closed_effect(effect),
        ConflictPress::Passed { menu_closed } => menu_closed,
    };
    let owed = |effect| match effect {
        UpdateEffect::None if menu_closed => UpdateEffect::Redraw,
        other => other,
    };

    let Some((col, row)) = translate_mouse_to_focused(stoat, mouse.column, mouse.row) else {
        return closed_effect(owed(UpdateEffect::None));
    };
    closed_effect(owed(apply_focused_pane_mouse(stoat, mouse.kind, col, row)))
}

/// What [`handle_conflict_press`] made of a press.
enum ConflictPress {
    /// The press took a pick, reset a chunk, or opened or closed the chip
    /// menu, and owes this effect.
    Taken(UpdateEffect),
    /// The press takes its own path. With `menu_closed` it closed an open chip
    /// menu, whose buttons show until the frame repaints.
    Passed { menu_closed: bool },
}

/// Take what a left press on the conflict view lands on, as the `o`, `t`,
/// `b`, `O`, `T`, and `x` keys take it.
///
/// A press on an open chunk's chip shows its pick buttons in the chip's
/// place, and a second press on the chip closes them. A press on a button
/// takes its pick, and a press on a chip that took ours or theirs resets its
/// chunk. A press on the ours or theirs side of a chunk that still takes picks
/// takes that whole side, and with Alt held it takes the one line under the
/// pointer. A pick or a reset is one undo step, as a key's is.
///
/// Any other press closes an open menu and takes the editor's press path,
/// which places the cursor, so the merged column stays a text area.
fn handle_conflict_press(stoat: &mut Stoat, mouse: MouseEvent) -> ConflictPress {
    if !matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        return ConflictPress::Passed { menu_closed: false };
    }
    let menu = stoat
        .active_workspace()
        .conflict
        .as_ref()
        .and_then(|session| session.chip_menu);
    let target = action_handlers::focused_editor_mut(stoat).and_then(|editor| {
        crate::render::conflict_view::conflict_target(editor, mouse.column, mouse.row)
    });

    let opened = match target {
        Some(ConflictTarget::Chip {
            chunk,
            state: ChunkState::Unresolved | ChunkState::Picked,
        }) => (menu != Some(chunk)).then_some(chunk),
        _ => None,
    };
    if menu.is_some() || opened.is_some() {
        action_handlers::conflict_view::set_chip_menu(stoat, opened);
    }

    let passed = ConflictPress::Passed {
        menu_closed: menu.is_some(),
    };
    match target {
        Some(ConflictTarget::Button(kind)) => match menu {
            Some(chunk) => take(stoat, |stoat| {
                action_handlers::conflict_view::pick_chunk_side(stoat, chunk, kind)
            }),
            None => passed,
        },
        Some(ConflictTarget::Chip {
            state: ChunkState::Unresolved | ChunkState::Picked,
            ..
        }) => ConflictPress::Taken(UpdateEffect::Redraw),
        Some(ConflictTarget::Chip {
            chunk,
            state: ChunkState::Ours | ChunkState::Theirs,
        }) => take(stoat, |stoat| {
            action_handlers::conflict_view::reset_chunk(stoat, chunk)
        }),
        Some(ConflictTarget::Side {
            chunk,
            side,
            merge_row,
        }) => match (mouse.modifiers.contains(KeyModifiers::ALT), merge_row) {
            (true, Some(merge_row)) => take(stoat, |stoat| {
                action_handlers::conflict_view::pick_chunk_row(stoat, chunk, merge_row, side)
            }),
            (true, None) => passed,
            (false, _) => {
                let kind = match side {
                    Side::Ours => ActionKind::ConflictPickOurs,
                    Side::Theirs => ActionKind::ConflictPickTheirs,
                };
                take(stoat, |stoat| {
                    action_handlers::conflict_view::pick_chunk_side(stoat, chunk, kind)
                })
            },
        },
        Some(ConflictTarget::Chip { .. }) | None => passed,
    }
}

/// Run `edit` as one undo step and answer the redraw it owes, as a press on
/// the conflict view takes a pick.
fn take(stoat: &mut Stoat, edit: impl FnOnce(&mut Stoat)) -> ConflictPress {
    ConflictPress::Taken(action_handlers::with_action_group(stoat, |stoat| {
        edit(stoat);
        UpdateEffect::Redraw
    }))
}

/// Route a pane-relative pointer event to whichever focused pane kind owns
/// it, returning [`UpdateEffect::Redraw`] when one consumes it.
///
/// The shared tail of both the primary mouse path and an aux window's
/// pointer events. The caller resolves and focuses the target pane first --
/// the primary via its grid hit-test, an aux window via its pane binding --
/// so `col`/`row` are relative to the focused pane's content area.
pub(crate) fn apply_focused_pane_mouse(
    stoat: &mut Stoat,
    kind: MouseEventKind,
    col: u16,
    row: u16,
) -> UpdateEffect {
    if handle_run_pane_mouse(stoat, kind, col, row) {
        return UpdateEffect::Redraw;
    }
    if handle_editor_pane_mouse(stoat, kind, col, row) {
        return UpdateEffect::Redraw;
    }
    if handle_terminal_pane_mouse(stoat, kind, col, row) {
        return UpdateEffect::Redraw;
    }
    tracing::trace!(
        target: "stoat::app",
        kind = ?kind,
        col,
        row,
        "mouse event routed to focused element"
    );
    UpdateEffect::None
}

/// The view a minimap scrub drives, from the press on its strip until the
/// release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MinimapDrag {
    /// An editor, whose viewport eases to the line under the pointer.
    Editor(EditorId),
    /// A changes view, whose cursor lands on the row under the pointer.
    DiffChanges(DiffChangesId),
    /// An editor's Coverage Rail, whose press eases the viewport to the line
    /// the rail shows under the pointer, at the rail's fold.
    Rail(EditorId),
    /// A changes view's Coverage Rail, whose press lands the cursor on the
    /// stack row the rail shows under the pointer, at the rail's fold.
    DiffChangesRail(DiffChangesId),
}

/// Route a left press or drag over a pane's minimap strip to a scrub. Returns
/// `Some` when the event is consumed, and `None` when it falls through to
/// focus and the text-area handlers.
///
/// Once a press arms [`Stoat::minimap_drag`], every drag re-scrubs the named
/// editor or changes view and the release clears the field, so the strip owns
/// the pointer for the whole gesture and the text area never sees it.
fn handle_minimap_mouse(stoat: &mut Stoat, mouse: MouseEvent) -> Option<UpdateEffect> {
    if let Some(drag) = stoat.minimap_drag {
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(strip) = minimap_strip_for(stoat, drag) {
                    scrub_minimap(stoat, drag, strip, mouse.row);
                }
            },
            MouseEventKind::Up(MouseButton::Left) => stoat.minimap_drag = None,
            _ => {},
        }
        return Some(UpdateEffect::Redraw);
    }

    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
        let pos = Position::new(mouse.column, mouse.row);
        // Single mode has one shared band that scrubs the changes view or the
        // editor of the pane it tracks. Per-pane mode walks the panes to find
        // the strip under the click.
        let hit = if stoat.minimap_mode() == MinimapMode::Single {
            stoat
                .single_minimap_rect
                .filter(|band| band.contains(pos))
                .and_then(|_| {
                    let ws = stoat.active_workspace();
                    match (ws.band_diff_changes_id(), ws.band_editor_id()) {
                        (Some(id), _) => Some(MinimapDrag::DiffChanges(id)),
                        (None, Some(editor_id)) => Some(MinimapDrag::Editor(editor_id)),
                        (None, None) => None,
                    }
                })
        } else {
            let ws = stoat.active_workspace();
            ws.panes.split_panes().find_map(|(_, pane)| {
                let (drag, strip) = match pane.view {
                    View::Editor(editor_id) => (
                        MinimapDrag::Editor(editor_id),
                        ws.editors.get(editor_id)?.minimap_rect?,
                    ),
                    View::DiffChanges(id) => (
                        MinimapDrag::DiffChanges(id),
                        ws.diff_changes_views.get(id)?.minimap_rect?,
                    ),
                    _ => return None,
                };
                strip.contains(pos).then_some(drag)
            })
        };
        if let Some(drag) = hit
            && let Some(strip) = minimap_strip_for(stoat, drag)
        {
            let drag = rail_drag(stoat, drag, strip, mouse.column);
            stoat.minimap_drag = Some(drag);
            scrub_minimap(stoat, drag, strip, mouse.row);
            return Some(UpdateEffect::Redraw);
        }
    }

    None
}

/// Resolve the minimap band `drag` scrubs against. In single mode this is the
/// shared window-right band, otherwise the editor's or the changes view's own
/// per-pane strip rect.
fn minimap_strip_for(stoat: &Stoat, drag: MinimapDrag) -> Option<Rect> {
    if stoat.minimap_mode() == MinimapMode::Single {
        return stoat.single_minimap_rect;
    }
    let ws = stoat.active_workspace();
    match drag {
        MinimapDrag::Editor(editor_id) | MinimapDrag::Rail(editor_id) => {
            ws.editors.get(editor_id)?.minimap_rect
        },
        MinimapDrag::DiffChanges(id) | MinimapDrag::DiffChangesRail(id) => {
            ws.diff_changes_views.get(id)?.minimap_rect
        },
    }
}

/// Scrub the view `drag` names to the line that the `strip` cell under
/// `screen_row` points at.
fn scrub_minimap(stoat: &mut Stoat, drag: MinimapDrag, strip: Rect, screen_row: u16) {
    match drag {
        MinimapDrag::Editor(editor_id) => scrub_minimap_editor(stoat, editor_id, strip, screen_row),
        MinimapDrag::Rail(editor_id) => scrub_minimap_rail(stoat, editor_id, strip, screen_row),
        MinimapDrag::DiffChanges(id) => scrub_minimap_diff_changes(stoat, id, strip, screen_row),
        MinimapDrag::DiffChangesRail(id) => {
            scrub_minimap_diff_changes_rail(stoat, id, strip, screen_row)
        },
    }
}

/// Ease `editor_id`'s viewport onto the file line the `strip` row under
/// `screen_row` points at, centered in the viewport.
///
/// Maps the strip-local cell row to a line with the same proportional math
/// the strip renders with, then jumps `scroll_row` and glides the offset up
/// to it like a page motion. `strip` is the caller-resolved band the editor
/// scrubs against (a per-pane rect or the shared single-mode band).
///
/// A strip that folds a band of lines into each row maps the cell to a row,
/// with the view in rows as the thumb ships it, and lands on the middle line
/// of that row's band.
///
/// A diff view's or a conflict view's strip holds one row per display row, so
/// its press resolves in display rows and lands on the display row itself.
fn scrub_minimap_editor(stoat: &mut Stoat, editor_id: EditorId, strip: Rect, screen_row: u16) {
    let thumb_min_rows = stoat.minimap_thumb_min_rows();
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    let Some(editor) = ws.editors.get_mut(editor_id) else {
        return;
    };

    let snapshot = editor.display_map.snapshot();
    let viewport = editor.viewport_rows.unwrap_or(strip.height as u32).max(1);
    let strip_local_row = screen_row.saturating_sub(strip.y);
    // A plain editor's strip is drawn in buffer lines, so the click resolves
    // against the same window the emit ships rather than the editor's display
    // rows. The emit ships the window of the declared rest, the row plus the
    // wheel fraction, and the window of the eased offset only while a glide is
    // in flight.
    let rest = editor.scroll_row as f32 + editor.scroll_frac;
    let shown_offset = match editor.scroll_glide {
        ScrollGlide::None => rest,
        ScrollGlide::Page | ScrollGlide::Wheel => editor.scroll_offset,
    };
    let (view_top, view_visible, total) = match editor.display_row_strip() {
        true => {
            let (top, visible) = diff_view_window(&snapshot, shown_offset, viewport);
            (top, visible, snapshot.line_count())
        },
        false => {
            let (top, visible) = minimap_view_window(&snapshot, shown_offset, viewport);
            (top, visible, snapshot.buffer_line_count())
        },
    };
    let target = match editor.minimap_density {
        None => minimap::click_target_line(
            strip.height,
            LINES_PER_CELL,
            strip_local_row,
            total as f32,
            view_top,
            view_visible as f32,
        ),
        Some(fold) => {
            let lines_per_row = fold.lines_per_row;
            let (top_256, visible_256) = density::thumb_rows_256(
                (view_top * 256.0) as u64,
                u32::from(view_visible),
                lines_per_row,
                thumb_min_rows,
            );
            let row = minimap::click_target_line(
                strip.height,
                LINES_PER_CELL,
                strip_local_row,
                density::rows(total, lines_per_row) as f32,
                top_256 as f32 / 256.0,
                visible_256 as f32 / 256.0,
            );
            density::band(row, lines_per_row, total).start + lines_per_row / 2
        },
    };

    match editor.display_row_strip() {
        true => scrub_to_display_row(editor, &snapshot, target, viewport),
        false => scrub_to_line(editor, &snapshot, target, viewport),
    }
}

/// `drag` as a press on the Coverage Rail when `column` is the last of
/// `strip`, the cell the rail takes while the strip's rows do not fit.
fn rail_drag(stoat: &Stoat, drag: MinimapDrag, strip: Rect, column: u16) -> MinimapDrag {
    let density = match drag {
        MinimapDrag::Editor(editor_id) => stoat
            .active_workspace()
            .editors
            .get(editor_id)
            .and_then(|editor| editor.minimap_density),
        MinimapDrag::DiffChanges(_) => stoat
            .diff_changes_strips
            .get(&stoat.active_workspace)
            .map(|strip| strip.density)
            .filter(|density| density.tier != Tier::Line),
        MinimapDrag::Rail(_) | MinimapDrag::DiffChangesRail(_) => None,
    };
    let railed = density.is_some_and(|density| !density.fits);
    if !railed || strip.width < 2 || column != strip.x + strip.width - 1 {
        return drag;
    }
    match drag {
        MinimapDrag::Editor(editor_id) => MinimapDrag::Rail(editor_id),
        MinimapDrag::DiffChanges(id) => MinimapDrag::DiffChangesRail(id),
        MinimapDrag::Rail(_) | MinimapDrag::DiffChangesRail(_) => drag,
    }
}

/// Ease `editor_id`'s viewport onto the line its Coverage Rail shows under
/// `screen_row`, centered in the viewport.
///
/// A diff view's or a conflict view's rail counts display rows, as its strip
/// does, so its press lands on the display row itself.
fn scrub_minimap_rail(stoat: &mut Stoat, editor_id: EditorId, strip: Rect, screen_row: u16) {
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    let Some(editor) = ws.editors.get_mut(editor_id) else {
        return;
    };
    let Some(fold) = editor.minimap_density else {
        return;
    };

    let snapshot = editor.display_map.snapshot();
    let viewport = editor.viewport_rows.unwrap_or(strip.height as u32).max(1);
    let target = rail_target(strip, screen_row, fold.whole_lines_per_row);
    match editor.display_row_strip() {
        true => scrub_to_display_row(editor, &snapshot, target, viewport),
        false => scrub_to_line(editor, &snapshot, target, viewport),
    }
}

/// The row a Coverage Rail in `strip` shows under `screen_row`, at
/// `rows_per_row` of its strip's rows to a rail row.
///
/// The rail holds the whole file at its own fold, so it fits its cell and
/// never scrolls. The cell's middle rail row names a band, and the target is
/// that band's middle row.
fn rail_target(strip: Rect, screen_row: u16, rows_per_row: u32) -> u32 {
    let cell = screen_row
        .saturating_sub(strip.y)
        .min(strip.height.saturating_sub(1));
    let lines_per_cell = u32::from(LINES_PER_CELL);
    let rail_row = u32::from(cell) * lines_per_cell + lines_per_cell / 2;
    rail_row * rows_per_row + rows_per_row / 2
}

/// Jump `editor`'s view so buffer line `line` of `snapshot` sits centered in a
/// viewport of `viewport` rows, and glide there from the declared rest.
fn scrub_to_line(editor: &mut EditorState, snapshot: &DisplaySnapshot, line: u32, viewport: u32) {
    // A strip taller than the file resolves cells past its last line, and
    // scrolling happens in display rows, so the target converts back.
    let target_line = line.min(snapshot.buffer_line_count().saturating_sub(1));
    let target_display = snapshot
        .buffer_to_display(stoat_text::Point::new(target_line, 0))
        .row;
    scrub_to_display_row(editor, snapshot, target_display, viewport);
}

/// Jump `editor`'s view so display row `row` of `snapshot` sits centered in a
/// viewport of `viewport` rows, and glide there from the declared rest.
///
/// The view stops at the last page, so a row past the last one lands there.
fn scrub_to_display_row(
    editor: &mut EditorState,
    snapshot: &DisplaySnapshot,
    row: u32,
    viewport: u32,
) {
    let rest = editor.scroll_row as f32 + editor.scroll_frac;
    let max_scroll = snapshot
        .line_count()
        .saturating_sub(1)
        .saturating_sub(viewport.saturating_sub(1));
    let target_row = row.saturating_sub(viewport / 2).min(max_scroll);

    editor.scroll_row = target_row;
    editor.scroll_frac = 0.0;
    // The glide starts from the declared rest before the scrub, as a page
    // motion's does, because a follow or a jump leaves the offset behind it.
    if editor.scroll_offset != rest {
        editor.scroll_offset = rest;
    }
    editor.scroll_glide = ScrollGlide::Page;
}

/// Put changes view `id`'s cursor on the stack row that the `strip` cell under
/// `screen_row` points at, and scroll that row to the middle of the view.
///
/// The strip holds one line per stack row, or one per band of stack rows past
/// the strip's fit, so the row resolves with the math the strip renders with,
/// and a band lands on its middle stack row. The scroll stops at the last page,
/// as an editor's scrub does, and a stack with no rows leaves the changes view
/// as it is.
fn scrub_minimap_diff_changes(stoat: &mut Stoat, id: DiffChangesId, strip: Rect, screen_row: u16) {
    let thumb_min_rows = stoat.minimap_thumb_min_rows();
    let density = stoat
        .diff_changes_strips
        .get(&stoat.active_workspace)
        .map_or(Density::LINE, |strip| strip.density);
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = &mut stoat.workspaces[stoat.active_workspace];
    let Some(state) = diff_changes_views.get_mut(id) else {
        return;
    };
    let total = diff_changes::layout_stack(state, editors);
    if total == 0 {
        return;
    }

    let cell = screen_row.saturating_sub(strip.y);
    let (top, visible) = (state.scroll as f32, state.viewport_rows.max(1) as u32);
    let target = match density.tier {
        Tier::Line => minimap::click_target_line(
            strip.height,
            LINES_PER_CELL,
            cell,
            total as f32,
            top,
            visible as f32,
        ) as usize,
        Tier::Fit | Tier::Shape => {
            let per_row = density.lines_per_row;
            let (top_256, visible_256) =
                density::thumb_rows_256((top * 256.0) as u64, visible, per_row, thumb_min_rows);
            let band = minimap::click_target_line(
                strip.height,
                LINES_PER_CELL,
                cell,
                density::rows(total as u32, per_row) as f32,
                top_256 as f32 / 256.0,
                visible_256 as f32 / 256.0,
            );
            (density::band(band, per_row, total as u32).start + per_row / 2) as usize
        },
    };
    land_on_stack_row(state, editors, target.min(total - 1), total);
}

/// Put changes view `id`'s cursor on the stack row its Coverage Rail shows
/// under `screen_row`, and scroll that row to the middle of the view.
///
/// The rail holds the whole stack at its own fold, as an editor's rail holds
/// its file.
fn scrub_minimap_diff_changes_rail(
    stoat: &mut Stoat,
    id: DiffChangesId,
    strip: Rect,
    screen_row: u16,
) {
    let density = stoat
        .diff_changes_strips
        .get(&stoat.active_workspace)
        .map_or(Density::LINE, |strip| strip.density);
    let Workspace {
        diff_changes_views,
        editors,
        ..
    } = &mut stoat.workspaces[stoat.active_workspace];
    let Some(state) = diff_changes_views.get_mut(id) else {
        return;
    };
    let total = diff_changes::layout_stack(state, editors);
    if total == 0 {
        return;
    }

    let target = rail_target(strip, screen_row, density.whole_lines_per_row) as usize;
    land_on_stack_row(state, editors, target.min(total - 1), total);
}

/// Put `state`'s cursor on stack row `row` of its `total` rows, and scroll
/// that row to the middle of the view, which stops at the last page.
fn land_on_stack_row(
    state: &mut DiffChangesState,
    editors: &mut SlotMap<EditorId, EditorState>,
    row: usize,
    total: usize,
) {
    diff_changes::select_row(state, editors, row);
    state.scroll = row
        .saturating_sub(state.viewport_rows / 2)
        .min(total.saturating_sub(state.viewport_rows));
}

/// Claims a button press on the tab bar's rows, switching to the tab under a
/// left press, or opening the Glance of the Job whose Tray Chip it lands on.
///
/// Returns `None` for an event that is not a press on the visible bar. A band
/// takes two rows, and each tab draws over part of both, so a press on either
/// row finds the tab. Every press on the rows is claimed, on a tab or not,
/// because the rows are above every pane and an unclaimed press there reaches
/// the focused pane as a press on its first row.
fn handle_tab_bar_mouse(stoat: &mut Stoat, mouse: MouseEvent) -> Option<UpdateEffect> {
    let MouseEventKind::Down(button) = mouse.kind else {
        return None;
    };
    let bar = stoat.size();
    if !(bar.y..bar.y + stoat.tab_bar_rows()).contains(&mouse.row) {
        return None;
    }

    if button == MouseButton::Left
        && let Some(cmd) = chip_at(stoat, mouse.column)
    {
        jobs::show_glance(stoat, cmd);
        return Some(UpdateEffect::Redraw);
    }
    let hit = tab_at(&stoat.tab_bar_spans, mouse.column.saturating_sub(bar.x));
    match (button, hit) {
        (MouseButton::Left, Some(idx)) => Some(action_handlers::tab::goto_tab(stoat, idx + 1)),
        _ => Some(UpdateEffect::None),
    }
}

/// The tab whose painted extent holds the center of the bar-relative cell
/// `column`.
fn tab_at(spans: &[Range<u16>], column: u16) -> Option<usize> {
    let center = u32::from(column) * 16 + 8;
    spans
        .iter()
        .position(|span| (u32::from(span.start)..u32::from(span.end)).contains(&center))
}

/// The Cmd of the Job whose Tray Chip a press in the window column `column`
/// names.
///
/// A chip that the bar drew at the board's pixels holds the pixel column of
/// the press. That is the pixel that the tray's pool reports from the left
/// edge of its region, or the middle pixel of `column` with no report. A press
/// in the 3 px between two chips names none. A chip in cells holds `column`.
fn chip_at(stoat: &Stoat, column: u16) -> Option<CmdId> {
    let spans = &stoat.tray_spans;
    let pixel = spans
        .first()
        .filter(|first| first.px.is_some())
        .zip(stoat.pixel_rows())
        .map(|(first, m)| {
            let width = u32::from(m.cell[0]);
            let reported = stoat
                .pool_pointer
                .filter(|&(pool, ..)| pool == crate::smooth_scroll::non_pane_pool::JOB_TRAY);
            match reported {
                Some((_, x, _)) => u32::from(first.cells.start) * width + u32::from(x),
                None => u32::from(column) * width + width / 2,
            }
        });
    spans
        .iter()
        .find(|span| match (&span.px, pixel) {
            (Some(px), Some(pixel)) => px.contains(&pixel),
            _ => span.cells.contains(&column),
        })
        .map(|span| span.cmd)
}

/// Claims a button press on the checkout banner's rows, which does nothing.
///
/// Returns `None` for an event that is not a press on the shown banner. The
/// claim is there because the rows are above every pane, as the tab bar's are,
/// and an unclaimed press there reaches the focused pane as a press on its
/// first row.
fn handle_banner_mouse(stoat: &Stoat, mouse: MouseEvent) -> Option<UpdateEffect> {
    let MouseEventKind::Down(_) = mouse.kind else {
        return None;
    };
    let top = stoat.size().y + stoat.tab_bar_rows();
    (top..top + stoat.banner_rows())
        .contains(&mouse.row)
        .then_some(UpdateEffect::None)
}

/// Route a left-button press over the open hover popup to its text
/// selection. Returns `Some` when the event is consumed (a press, drag, or
/// release over the popup), `None` when it should fall through to normal
/// mouse handling.
///
/// A left press inside starts a drag selection, and a release keeps that
/// selection live, copying it to the clipboard when non-empty.
///
/// A press outside closes the popup and its in-flight request, then falls
/// through so the press still lands. This mirrors the key path, where any key
/// the popup does not scroll with closes it and then dispatches. The press is
/// not swallowed, so it goes on to place the cursor, arm a divider, or change
/// focus exactly as it does with no popup open.
fn handle_hover_selection_mouse(stoat: &mut Stoat, mouse: MouseEvent) -> Option<UpdateEffect> {
    let popup_area = stoat.pending_hover.as_ref()?.area;
    let inside = popup_area.contains(Position {
        x: mouse.column,
        y: mouse.row,
    });

    match mouse.kind {
        MouseEventKind::Down(button) => {
            if !inside {
                stoat.pending_hover = None;
                stoat.pending_hover_request.clear();
                return None;
            }
            if button != MouseButton::Left {
                return None;
            }
            let pos = crate::render::hover::hover_hit_test(
                stoat.pending_hover.as_ref()?,
                mouse.column,
                mouse.row,
                stoat.theme.type_scale(TypeRole::Ui),
            );
            if let Some(popup) = stoat.pending_hover.as_mut() {
                popup.selection = Some(crate::render::hover::HoverSelection {
                    anchor: pos,
                    head: pos,
                    dragging: true,
                });
            }
            Some(UpdateEffect::Redraw)
        },
        MouseEventKind::Drag(MouseButton::Left) => {
            if !hover_selection_dragging(stoat) {
                return None;
            }
            let pos = crate::render::hover::hover_hit_test(
                stoat.pending_hover.as_ref()?,
                mouse.column,
                mouse.row,
                stoat.theme.type_scale(TypeRole::Ui),
            );
            if let Some(sel) = stoat
                .pending_hover
                .as_mut()
                .and_then(|p| p.selection.as_mut())
            {
                sel.head = pos;
            }
            Some(UpdateEffect::Redraw)
        },
        MouseEventKind::Up(MouseButton::Left) => {
            if !hover_selection_dragging(stoat) {
                return None;
            }
            if let Some(sel) = stoat
                .pending_hover
                .as_mut()
                .and_then(|p| p.selection.as_mut())
            {
                sel.dragging = false;
            }
            let text = crate::render::hover::hover_selected_text(stoat.pending_hover.as_ref()?);
            if text.is_empty() {
                if let Some(popup) = stoat.pending_hover.as_mut() {
                    popup.selection = None;
                }
            } else {
                crate::host::clipboard_copy(
                    stoat.clipboard_host().as_ref(),
                    stoat.env_host().as_ref(),
                    ClipboardKind::System,
                    &text,
                );
            }
            Some(UpdateEffect::Redraw)
        },
        _ => None,
    }
}

fn hover_selection_dragging(stoat: &Stoat) -> bool {
    stoat
        .pending_hover
        .as_ref()
        .and_then(|p| p.selection)
        .is_some_and(|s| s.dragging)
}

/// Run the keymap binding a modified wheel notch names, if it names one.
///
/// `None` hands the notch back to the ordinary scroll routing, which covers an
/// unmodified wheel, a modifier nothing binds, and a notch arriving inside
/// [`WHEEL_BINDING_COOLDOWN`] of the last one that dispatched.
///
/// The pointer position is not consulted. The actions a wheel binds move the
/// focused editor's selection, so acting on the pane under the pointer moves a
/// selection the user is not looking at.
///
/// A focused modal needs no special casing here. The mode predicate is then the
/// modal input's, so an editor binding misses and the notch falls through to
/// the modal arms below.
fn wheel_binding(stoat: &mut Stoat, mouse: MouseEvent) -> Option<UpdateEffect> {
    if mouse.modifiers.is_empty() {
        return None;
    }
    let dir = match mouse.kind {
        MouseEventKind::ScrollUp => WheelDirection::Up,
        MouseEventKind::ScrollDown => WheelDirection::Down,
        _ => return None,
    };

    let actions = {
        let state = StoatKeymapState::from_stoat(stoat);
        stoat.keymap.lookup_wheel(&state, dir, mouse.modifiers)?
    };

    let now = stoat.executor.now();
    if stoat
        .wheel_binding_last
        .is_some_and(|last| now.duration_since(last) < WHEEL_BINDING_COOLDOWN)
    {
        return None;
    }
    stoat.wheel_binding_last = Some(now);

    // A click is not a dismissal here. The click-outside path takes a popup
    // down on its own, before ever reaching a binding.
    Some(stoat.run_bound_actions(&actions, None, false))
}

/// Scroll the open modal's preview pane one wheel step.
///
/// Each modal holds its preview differently. Most back it with a real editor
/// and scroll through the same path an editor pane does. The commit picker,
/// help, and the Commit Preview keep their own row offset instead, since none
/// of their previews is a buffer.
fn scroll_modal_preview(stoat: &mut Stoat, down: bool) -> UpdateEffect {
    let editor = match active_modal(stoat) {
        Some(ActiveModal::CommitPicker) => {
            let Some(picker) = stoat.commit_picker.as_mut() else {
                return UpdateEffect::None;
            };
            picker.preview_scroll = match down {
                true => picker.preview_scroll.saturating_add(PREVIEW_WHEEL_ROWS),
                false => picker.preview_scroll.saturating_sub(PREVIEW_WHEEL_ROWS),
            };
            return UpdateEffect::Redraw;
        },
        Some(ActiveModal::CommitPreview) => {
            let rows = PREVIEW_WHEEL_ROWS as i32;
            return commit_preview::scroll_by(
                stoat,
                match down {
                    true => rows,
                    false => -rows,
                },
            );
        },
        Some(ActiveModal::Help) => {
            return match down {
                true => action_handlers::help::help_scroll_detail_down(stoat),
                false => action_handlers::help::help_scroll_detail_up(stoat),
            };
        },
        _ => modal_preview_editor(stoat),
    };

    let Some(editor_id) = editor else {
        return UpdateEffect::None;
    };
    if let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) {
        action_handlers::view::wheel_scroll(editor, down);
    }
    UpdateEffect::None
}

/// Whole notches `lines` of travel makes, holding the remainder on `stoat` for
/// the reports that follow.
///
/// A notch is the unit every wheel consumer but an editor pane spends, and a
/// trackpad reports far less than one at a time. Accruing rather than rounding
/// is what makes a slow drag over a list eventually step it, instead of
/// throwing every report away.
fn take_whole_notches(stoat: &mut Stoat, lines: f32) -> i32 {
    stoat.wheel_line_remainder += lines;
    let whole = stoat.wheel_line_remainder.trunc();
    stoat.wheel_line_remainder -= whole;
    whole as i32
}

/// Route `lines` of wheel travel, positive scrolling the content down, to
/// whatever sits under the pointer.
///
/// An editor pane takes the travel as it is and can rest between rows. Every
/// other surface steps by whole notches, so travel worth less than one accrues
/// on the session rather than falling through to the pane beneath it, which
/// would scroll the wrong thing under the pointer.
pub fn handle_mouse_scroll(stoat: &mut Stoat, mouse: MouseEvent, lines: f32) -> UpdateEffect {
    if let Some(effect) = wheel_binding(stoat, mouse) {
        return effect;
    }

    // An open boxed modal owns the wheel wherever the pointer sits, so the
    // event never reaches the pane it covers. Over the preview it scrolls that
    // pane, and anywhere else it walks the list.
    if let Some(surfaces) = boxed_modal_surfaces(stoat) {
        let steps = take_whole_notches(stoat, lines);
        if steps == 0 {
            return UpdateEffect::None;
        }
        let down = steps > 0;
        let over_preview = surfaces
            .preview
            .is_some_and(|rect| rect.contains(Position::new(mouse.column, mouse.row)));

        if over_preview {
            let mut effect = UpdateEffect::None;
            for _ in 0..steps.unsigned_abs() {
                effect = scroll_modal_preview(stoat, down);
            }
            return effect;
        }
        if let Some(effect) = wheel_history_rows(stoat, steps) {
            return effect;
        }
        return action_handlers::picker::picker_step(stoat, steps);
    }

    // A pane-overlay screen owns the wheel inside its pane, the same way a
    // boxed modal owns it everywhere. Over the list the notches step the
    // selection, and over the changes view they scroll its stack. Anywhere
    // else in the pane they are spent and dropped, so the wheel never reaches
    // the editor the screen covers.
    if let Some(surfaces) = screen_surfaces(stoat).filter(|s| s.holds(mouse)) {
        let steps = take_whole_notches(stoat, lines);
        if steps == 0 {
            return UpdateEffect::None;
        }
        let at = Position::new(mouse.column, mouse.row);
        let by = steps.unsigned_abs() as usize;

        if surfaces.list.contains(at) {
            return match surfaces.kind {
                ScreenKind::Commits => {
                    let step = match steps > 0 {
                        true => CommitStep::Down(by),
                        false => CommitStep::Up(by),
                    };
                    action_handlers::commits::commits_step(stoat, step)
                },
                ScreenKind::DiffChanges => {
                    action_handlers::diff_changes::diff_changes_scroll(stoat, steps)
                },
                // The rebase step moves one entry, so a multi-notch report
                // spends one call per notch rather than carrying a count.
                ScreenKind::Rebase => {
                    let step = match steps > 0 {
                        true => RebaseMove::Next,
                        false => RebaseMove::Prev,
                    };
                    let mut effect = UpdateEffect::None;
                    for _ in 0..by {
                        effect = action_handlers::rebase::rebase_move(stoat, step, 1);
                    }
                    effect
                },
                // One file per notch, for the reason the rebase arm above
                // spends one call each.
                ScreenKind::RebaseConflict => {
                    let mut effect = UpdateEffect::None;
                    for _ in 0..by {
                        effect = action_handlers::conflict::conflict_step(stoat, steps > 0, 1);
                    }
                    effect
                },
            };
        }
        if surfaces.detail.is_some_and(|rect| rect.contains(at)) {
            let rows = (by * PREVIEW_WHEEL_ROWS) as i32;
            return action_handlers::commits::commits_detail_scroll(
                stoat,
                match steps > 0 {
                    true => rows,
                    false => -rows,
                },
            );
        }
        return UpdateEffect::None;
    }

    // A wheel over the open hover popup scrolls the popup, not the pane
    // beneath it. The bump mirrors the Ctrl-d/Ctrl-u path. render_hover
    // clamps it to the content height.
    let over_popup = stoat
        .pending_hover
        .as_ref()
        .is_some_and(|popup| popup.area.contains(Position::new(mouse.column, mouse.row)));
    if over_popup {
        let steps = take_whole_notches(stoat, lines);
        if steps == 0 {
            return UpdateEffect::None;
        }
        if let Some(popup) = stoat.pending_hover.as_mut() {
            let by = steps.unsigned_abs() as usize;
            popup.scroll_half_pages = match steps > 0 {
                true => popup.scroll_half_pages + by,
                false => popup.scroll_half_pages.saturating_sub(by),
            };
        }
        return UpdateEffect::Redraw;
    }

    let Some(target) = target_at(stoat, mouse.column, mouse.row) else {
        return UpdateEffect::None;
    };
    let ws = stoat.active_workspace();

    // Snapshot the view and pane area under the cursor so the scroll below
    // can take a fresh mutable borrow of the run or editor state.
    let (view, area) = match target {
        PanelHit::Pane(pid) => {
            let pane = ws.panes.pane(pid);
            (pane.view.clone(), pane.area)
        },
        PanelHit::Dock(dock_id) => match ws.docks.get(dock_id) {
            Some(dock) => (dock.view.clone(), dock.area),
            None => return UpdateEffect::None,
        },
    };

    scroll_view_at(stoat, view, area, lines)
}

/// Advance the wheel-scroll target of `view` (occupying `area`) by `lines` of
/// travel, positive scrolling the content down.
///
/// The pane-resolved half of the wheel path, shared by the primary hit-test
/// and an aux window's wheel events. An editor takes the travel whole and only
/// moves its glide target, so it reports no redraw -- the frame tick eases and
/// renders, and a trackpad flick of ~100 events must not repaint per event. A
/// run pane scrolls by whole notches, so its share accrues on the session.
///
/// A terminal or agent pane also steps by whole notches. It walks its history,
/// or on the alternate screen it sends the child arrow keys, the convention a
/// pager or an editor scrolls by.
pub(crate) fn scroll_view_at(
    stoat: &mut Stoat,
    view: View,
    area: Rect,
    lines: f32,
) -> UpdateEffect {
    match view {
        View::Editor(id) => {
            let Some(editor) = stoat.active_workspace_mut().editors.get_mut(id) else {
                return UpdateEffect::None;
            };
            action_handlers::view::wheel_scroll_by(editor, lines);
            UpdateEffect::None
        },
        View::Run(id) => {
            let steps = take_whole_notches(stoat, lines);
            if steps == 0 {
                return UpdateEffect::None;
            }
            let Workspace { runs, cmds, .. } = stoat.active_workspace_mut();
            let Some(run_state) = runs.get_mut(id) else {
                return UpdateEffect::None;
            };
            for _ in 0..steps.unsigned_abs() {
                run_state.wheel_scroll(steps > 0, (area.height as usize).saturating_sub(1), cmds);
            }
            UpdateEffect::Redraw
        },
        View::Agent(id) | View::Terminal(id) => {
            let steps = take_whole_notches(stoat, lines);
            if steps == 0 {
                return UpdateEffect::None;
            }
            let Some(alternate) = stoat
                .active_workspace()
                .terms
                .get(id)
                .map(|session| session.term.alternate_scroll())
            else {
                return UpdateEffect::None;
            };

            // The child repaints through its own output, so the arrow keys
            // owe no frame here.
            if alternate {
                let arrow: &[u8] = match steps > 0 {
                    true => b"\x1b[B",
                    false => b"\x1b[A",
                };
                stoat.write_to_term(
                    id,
                    &arrow.repeat(TERM_WHEEL_ROWS * steps.unsigned_abs() as usize),
                );
                return UpdateEffect::None;
            }

            // A notch up arrives as negative travel and walks back into
            // history, which is a positive delta.
            let moved = stoat
                .active_workspace_mut()
                .terms
                .get_mut(id)
                .is_some_and(|session| session.term.scroll_lines(-steps * TERM_WHEEL_ROWS as i32));
            if !moved {
                return UpdateEffect::None;
            }
            // The selection names viewport cells, which show other rows once
            // the view moves.
            clear_term_selection(stoat, id);
            UpdateEffect::Redraw
        },
        _ => UpdateEffect::None,
    }
}

fn handle_run_pane_mouse(stoat: &mut Stoat, kind: MouseEventKind, col: u16, row: u16) -> bool {
    let target = {
        let ws = stoat.active_workspace();
        match ws.focus {
            FocusTarget::SplitPane => {
                let pane = ws.panes.pane(ws.panes.focus());
                if let View::Run(id) = pane.view {
                    Some((id, pane.area, Some(pane.index)))
                } else {
                    None
                }
            },
            FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).and_then(|dock| {
                if let View::Run(id) = dock.view {
                    Some((id, dock.area, None))
                } else {
                    None
                }
            }),
        }
    };
    let Some((run_id, area, pool)) = target else {
        return false;
    };
    // A pane at the board's pixels declares its own pool over its paint, so a
    // report there carries the pointer's pixel from the corner of its box. A
    // gesture with no report keeps the middle of its cell.
    let pixel = stoat
        .pool_pointer
        .filter(|&(at, ..)| Some(at) == pool)
        .map(|(_, x, y)| (u32::from(x), u32::from(y)));
    let clipboard_host = stoat.clipboard_host.clone();
    let env_host = stoat.env_host.clone();
    let Workspace { runs, cmds, .. } = stoat.active_workspace_mut();
    let Some(run_state) = runs.get(run_id) else {
        return false;
    };
    let pos = match run_state.pixel_box.zip(pixel) {
        Some((b, (x, y))) => run_state.active_block_grid_px(cmds, &b, x, y),
        None => run_state.active_block_grid_pos(cmds, area, col, row),
    };
    let Some(block) = run_state.active_block_mut(cmds) else {
        return false;
    };
    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(pos) = pos else {
                return false;
            };
            block.selection = Some(GridSelection {
                anchor: pos,
                head: pos,
            });
            true
        },
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some(pos) = pos else {
                return false;
            };
            let Some(sel) = block.selection.as_mut() else {
                return false;
            };
            if sel.head == pos {
                return false;
            }
            sel.head = pos;
            true
        },
        MouseEventKind::Up(MouseButton::Left) => {
            let Some(sel) = block.selection.as_mut() else {
                return false;
            };
            // A pixel drag reports only on entering another grid cell, so the
            // release, which reports its pixel, can stand on a char past the
            // last drag's.
            if let Some(pos) = pos {
                sel.head = pos;
            }
            if sel.is_empty() {
                block.selection = None;
                return true;
            }
            let text = block.grid.text_for_selection(sel);
            if text.is_empty() {
                return false;
            }
            crate::host::clipboard_copy(
                clipboard_host.as_ref(),
                env_host.as_ref(),
                ClipboardKind::System,
                &text,
            );
            block.selection = None;
            stoat.set_status(copy_status(&text));
            true
        },
        _ => false,
    }
}

/// Handles left-button Down/Drag/Up events on a focused editor
/// pane. `Down(Left)` lands a 1-wide block cursor at the clicked
/// offset and arms `editor_drag`. `Drag(Left)` extends the head
/// of the dragged editor's primary selection and marks the drag
/// moved. `Up(Left)` writes the primary-selection text to the
/// clipboard (and conditionally OSC 52 emits) only when the drag
/// moved, then clears `editor_drag`. Clicks outside the pane's
/// rendered text area saturate to the nearest valid offset via
/// `clip_point` (Bias::Left). Returns `true` when the event
/// mutated state.
/// The focused pane's editor id and area, or `None` when the focus is not
/// on an editor view.
fn focused_editor_target(stoat: &Stoat) -> Option<(EditorId, Rect)> {
    let ws = stoat.active_workspace();
    match ws.focus {
        FocusTarget::SplitPane => {
            let pane = ws.panes.pane(ws.panes.focus());
            if let View::Editor(id) = pane.view {
                Some((id, pane.area))
            } else {
                None
            }
        },
        FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).and_then(|dock| {
            if let View::Editor(id) = dock.view {
                Some((id, dock.area))
            } else {
                None
            }
        }),
    }
}

/// The focused terminal or agent pane's [`TermId`] and pane area, or `None`
/// when the focused element is not a terminal-backed pane.
pub(crate) fn focused_term_target(stoat: &Stoat) -> Option<(TermId, Rect)> {
    let ws = stoat.active_workspace();
    match ws.focus {
        FocusTarget::SplitPane => {
            let pane = ws.panes.pane(ws.panes.focus());
            match pane.view {
                View::Agent(id) | View::Terminal(id) => Some((id, pane.area)),
                _ => None,
            }
        },
        FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).and_then(|dock| match dock.view {
            View::Agent(id) | View::Terminal(id) => Some((id, dock.area)),
            _ => None,
        }),
    }
}

/// Index of the diagnostic under the terminal cell `(column, row)` in the
/// focused editor pane, or `None` when the pointer is off a diagnostic or
/// off the focused editor.
///
/// Answers from the same version-keyed span cache the paint reads, so a
/// sweep across a diagnostic-heavy buffer resolves the set once instead of
/// once per motion event, and leaves the cache warm for the paint that
/// follows.
fn resolve_hover_diagnostic(stoat: &mut Stoat, column: u16, row: u16) -> Option<usize> {
    let (col, row) = translate_mouse_to_focused(stoat, column, row)?;
    let (editor_id, area) = focused_editor_target(stoat)?;
    let offset = editor_screen_to_offset(stoat, editor_id, area, col, row)?;

    let path = {
        let ws = stoat.active_workspace();
        let editor = ws.editors.get(editor_id)?;
        ws.buffers.path_for(editor.buffer_id)?.to_owned()
    };

    let Stoat {
        workspaces,
        active_workspace,
        diagnostics,
        ..
    } = stoat;
    let editor = workspaces[*active_workspace].editors.get_mut(editor_id)?;
    let snapshot = editor.display_map.snapshot();
    let buffer = snapshot.buffer_snapshot();
    crate::diagnostic_spans::build_diagnostic_span_cache(editor, diagnostics, &path, buffer);
    let cache = editor.diagnostic_span_cache.as_ref()?;
    crate::diagnostic_spans::diagnostic_at_offset(cache, offset, buffer).map(|found| found.index)
}

/// Track the hovered cell and redraw only when the diagnostic or the conflict
/// side under it changes, so mouse motion within one span does not repaint
/// every event.
///
/// Motion that stays inside one cell reuses the last diagnostic rather than
/// resolving again. That answer only drives the redraw decision, and the
/// paint recomputes the hovered diagnostic from the cell every frame, so a
/// diagnostic set arriving from a server still reaches the screen through
/// its own redraw. The badge hit test is two comparisons and reruns
/// regardless, since the badge rect moves under a pointer that has not. The
/// conflict side resolves on every event too, since `modifiers` decides
/// whether it names one line, and Alt changes inside one cell.
pub fn handle_hover(
    stoat: &mut Stoat,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
) -> UpdateEffect {
    let moved = stoat.hover_cell != Some((column, row));
    stoat.hover_cell = Some((column, row));
    let resolved = if moved {
        resolve_hover_diagnostic(stoat, column, row)
    } else {
        stoat.hover_diag
    };
    let badge_hovered = stoat.lsp_badge_rect.is_some_and(|rect| {
        column >= rect.x
            && column < rect.x + rect.width
            && row >= rect.y
            && row < rect.y + rect.height
    });
    let conflict_hover = conflict_hover_at(stoat, column, row, modifiers);
    if stoat.hover_diag == resolved
        && stoat.lsp_badge_hovered == badge_hovered
        && stoat.conflict_hover == conflict_hover
    {
        return UpdateEffect::None;
    }
    stoat.hover_diag = resolved;
    stoat.lsp_badge_hovered = badge_hovered;
    stoat.conflict_hover = conflict_hover;
    UpdateEffect::Redraw
}

/// The open conflict chunk's side under cell `column`, `row`, with `modifiers`
/// held, or `None` off every open side.
///
/// With Alt held the hover names the one line under the pointer, as an Alt
/// press takes it. A row of the chunk's band past that side's lines holds no
/// line to take, so it is no hover under Alt.
fn conflict_hover_at(
    stoat: &mut Stoat,
    column: u16,
    row: u16,
    modifiers: KeyModifiers,
) -> Option<ConflictHover> {
    let editor = action_handlers::focused_editor_mut(stoat)?;
    let Some(ConflictTarget::Side {
        chunk,
        side,
        merge_row,
    }) = crate::render::conflict_view::conflict_target(editor, column, row)
    else {
        return None;
    };
    let merge_row = match modifiers.contains(KeyModifiers::ALT) {
        true => Some(merge_row?),
        false => None,
    };
    Some(ConflictHover {
        chunk,
        side,
        merge_row,
    })
}

/// Collapse the focused editor's selection to a block cursor on the cell at
/// `col`/`row`, returning the buffer it landed in.
///
/// [`None`] when the cell maps to no text position, such as a click in the
/// diagnostic gutter, leaving the cursor where it was.
fn place_cursor_at_click(
    stoat: &mut Stoat,
    editor_id: EditorId,
    area: Rect,
    col: u16,
    row: u16,
) -> Option<BufferId> {
    let offset = editor_screen_to_offset(stoat, editor_id, area, col, row)?;

    let ws = stoat.active_workspace_mut();
    let editor = ws.editors.get_mut(editor_id).expect("editor exists");
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    editor.selections.set_block_cursor(offset, buf_snap);
    Some(editor.buffer_id)
}

fn handle_editor_pane_mouse(stoat: &mut Stoat, kind: MouseEventKind, col: u16, row: u16) -> bool {
    let Some((editor_id, area)) = focused_editor_target(stoat) else {
        return false;
    };

    let clipboard_host = stoat.clipboard_host.clone();
    let env_host = stoat.env_host.clone();

    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let Some(buffer_id) = place_cursor_at_click(stoat, editor_id, area, col, row) else {
                return false;
            };
            stoat.editor_drag = Some((editor_id, buffer_id, false));
            true
        },
        // Middle and right click drive the cursor-based LSP handlers, so
        // they place the cursor first and the request reads the clicked
        // symbol. Neither arms editor_drag, since a following Drag belongs
        // to whatever left-button selection was in progress.
        MouseEventKind::Down(MouseButton::Middle) => {
            if place_cursor_at_click(stoat, editor_id, area, col, row).is_none() {
                return false;
            }
            action_handlers::lsp::goto_definition(stoat);
            true
        },
        MouseEventKind::Down(MouseButton::Right) => {
            if place_cursor_at_click(stoat, editor_id, area, col, row).is_none() {
                return false;
            }
            crate::lsp::hover::hover(stoat);
            true
        },
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some((drag_editor, drag_buffer, _)) = stoat.editor_drag else {
                return false;
            };
            if drag_editor != editor_id {
                return false;
            }
            let Some(offset) = editor_screen_to_offset(stoat, editor_id, area, col, row) else {
                return false;
            };
            stoat.editor_drag = Some((drag_editor, drag_buffer, true));
            let ws = stoat.active_workspace_mut();
            let editor = ws.editors.get_mut(editor_id).expect("editor exists");
            let snapshot = editor.display_map.snapshot();
            let buf_snap = snapshot.buffer_snapshot();

            // The head already covers this offset, so the transform below
            // would rebuild the selection as it stands and the frame after
            // it would paint what is on screen.
            let head = editor.selections.newest_anchor().head();
            if buf_snap.resolve_anchor(&head) == offset {
                return false;
            }

            let head_anchor = buf_snap.anchor_at(offset, Bias::Right);
            editor.selections.transform(buf_snap, |sel| {
                let tail_anchor = sel.tail();
                let tail_offset = buf_snap.resolve_anchor(&tail_anchor);
                let mut new = sel.clone();
                new.goal = SelectionGoal::None;
                if offset < tail_offset {
                    new.start = head_anchor;
                    new.end = tail_anchor;
                    new.reversed = true;
                } else {
                    new.start = tail_anchor;
                    new.end = head_anchor;
                    new.reversed = false;
                }
                new
            });
            true
        },
        MouseEventKind::Up(MouseButton::Left) => {
            let Some((_, _, moved)) = stoat.editor_drag else {
                return false;
            };
            stoat.editor_drag = None;
            if !moved {
                return false;
            }
            let text = {
                let ws = stoat.active_workspace_mut();
                let editor = ws.editors.get_mut(editor_id).expect("editor exists");
                let snapshot = editor.display_map.snapshot();
                let buf_snap = snapshot.buffer_snapshot();
                let sel = editor.selections.newest_anchor();
                let start = buf_snap.resolve_anchor(&sel.start);
                let end = buf_snap.resolve_anchor(&sel.end);
                if start == end {
                    String::new()
                } else {
                    buf_snap.rope().slice(start..end).to_string()
                }
            };
            if text.is_empty() {
                return false;
            }
            crate::host::clipboard_copy(
                clipboard_host.as_ref(),
                env_host.as_ref(),
                ClipboardKind::System,
                &text,
            );
            false
        },
        _ => false,
    }
}

/// Route a left press, drag, or release over the focused terminal pane to a
/// grid selection, returning `true` when the event is consumed.
///
/// `Down` anchors a selection at the clicked cell and arms
/// [`Stoat::terminal_drag`]. `Drag` extends the head, clamped to the grid, and
/// marks the drag moved. `Up` after a moved drag copies the selected text to
/// the clipboard, clears the selection, and shows a status line that names the
/// copied line count. `Up` after a plain click clears the selection and copies
/// nothing. Coordinates are pane-relative cells.
///
/// A split pane's terminal that stoatty draws at its own font size holds other
/// cells than the pane's content area. Its gestures arrive by `pool_mouse`,
/// which names the terminal cell under the pointer through
/// [`Stoat::pool_pointer`]. Any other report in such a pane is not consumed.
fn handle_terminal_pane_mouse(stoat: &mut Stoat, kind: MouseEventKind, col: u16, row: u16) -> bool {
    let Some((term_id, _)) = focused_term_target(stoat) else {
        return false;
    };
    let (rows, cols, pooled, pool) = {
        let ws = stoat.active_workspace();
        let Some(session) = ws.terms.get(term_id) else {
            return false;
        };
        let pane = ws.panes.pane(ws.panes.focus());
        let pooled = matches!(ws.focus, FocusTarget::SplitPane)
            && pane.placement == Placement::Split
            && ws.term_pool_grids.contains_key(&term_id);
        (session.term.rows(), session.term.cols(), pooled, pane.index)
    };
    if rows == 0 || cols == 0 {
        return false;
    }

    let (col, row) = match pooled {
        true => match stoat.pool_pointer.filter(|(named, ..)| *named == pool) {
            Some((_, cell_col, cell_row)) => (cell_col, cell_row),
            None => return false,
        },
        false => (col, row),
    };

    match kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let (cell_row, cell_col) = (row as usize, col as usize);
            if cell_row >= rows || cell_col >= cols {
                clear_term_selection(stoat, term_id);
                return false;
            }
            {
                let ws = stoat.active_workspace_mut();
                let session = ws.terms.get_mut(term_id).expect("focused term exists");
                session.term.start_selection(cell_row, cell_col);
            }
            stoat.terminal_drag = Some((term_id, false));
            true
        },
        MouseEventKind::Drag(MouseButton::Left) => {
            let Some((drag_term, _)) = stoat.terminal_drag else {
                return false;
            };
            if drag_term != term_id {
                return false;
            }
            let cell_row = (row as usize).min(rows - 1);
            let cell_col = (col as usize).min(cols - 1);
            stoat.terminal_drag = Some((term_id, true));
            let ws = stoat.active_workspace_mut();
            let session = ws.terms.get_mut(term_id).expect("focused term exists");
            // Reporting no change is what drops the frame for a drag that
            // landed back on the cell the head already holds.
            session.term.extend_selection(cell_row, cell_col)
        },
        MouseEventKind::Up(MouseButton::Left) => {
            let Some((drag_term, moved)) = stoat.terminal_drag.take() else {
                return false;
            };
            if drag_term != term_id {
                return false;
            }
            if !moved {
                clear_term_selection(stoat, term_id);
                return false;
            }
            let text = stoat
                .active_workspace()
                .terms
                .get(term_id)
                .and_then(|session| session.selection_text());
            if let Some(text) = text {
                let clipboard_host = stoat.clipboard_host.clone();
                let env_host = stoat.env_host.clone();
                crate::host::clipboard_copy(
                    clipboard_host.as_ref(),
                    env_host.as_ref(),
                    ClipboardKind::System,
                    &text,
                );
                stoat.set_status(copy_status(&text));
            }
            clear_term_selection(stoat, term_id);
            true
        },
        _ => false,
    }
}

/// Drop any mouse selection on `term_id`, so the next keystroke, click, or
/// drag starts fresh.
pub(crate) fn clear_term_selection(stoat: &mut Stoat, term_id: TermId) {
    if let Some(session) = stoat.active_workspace_mut().terms.get_mut(term_id) {
        session.term.clear_selection();
    }
}

/// The status line a mouse copy shows, naming how many lines it copied.
fn copy_status(text: &str) -> String {
    let lines = text.lines().count().max(1);
    let plural = if lines == 1 { "" } else { "s" };
    format!("copied {lines} line{plural} to clipboard")
}

/// The buffer offset under the cell `col`, `row` of the editor pane at `area`,
/// or `None` for a cell outside the painted text.
///
/// A cell past the pane's last row, such as one on the pane's status row, maps
/// to nothing rather than to the line below the last visible one.
pub fn editor_screen_to_offset(
    stoat: &mut Stoat,
    editor_id: EditorId,
    area: Rect,
    col: u16,
    row: u16,
) -> Option<usize> {
    let content = area;
    if col >= content.width || row >= content.height {
        return None;
    }
    let ws = stoat.active_workspace_mut();
    let editor = ws.editors.get_mut(editor_id)?;
    // At an unaligned rest the composite draws document row r in the cell that
    // `r - offset` rounds to, so a click reads through the row holding the
    // majority of its cell.
    let scroll_row = editor.scroll_row + u32::from(editor.scroll_frac >= 0.5);
    // The conflict view is three columns. A click in the center text maps to
    // its cell; a click in either side column (or a gutter) drops to col 0 so
    // the cursor lands on the aligned center row's start. The paint records
    // the center text, and under the MergeGrid the rows start below a header
    // row that maps to no line.
    if editor.conflict_view.is_some() {
        let text = editor.text_rect?;
        let header = text.y.saturating_sub(content.y);
        if row < header {
            return None;
        }
        let center_gutter = text.x.saturating_sub(area.x);
        let center_end = text.right().saturating_sub(area.x);
        let click_col = if (center_gutter..center_end).contains(&col) {
            col
        } else {
            0
        };
        let snapshot = editor.display_map.snapshot();
        return crate::render::editor::display_cell_to_offset(
            &snapshot,
            scroll_row,
            center_gutter,
            click_col,
            row - header,
        );
    }
    // The diff view puts the editable text in the right column, so a click
    // maps against the right column's start rather than the left gutter.
    // Cells left of it (the base column and gutters) clamp to the line start.
    // The paint records that column, because the gutter widths follow the
    // file's line count.
    let gutter_width = if editor.diff_view {
        editor
            .text_rect
            .map_or(0, |text| text.x.saturating_sub(area.x))
    } else {
        editor.gutter_width
    };
    let snapshot = editor.display_map.snapshot();
    crate::render::editor::display_cell_to_offset(&snapshot, scroll_row, gutter_width, col, row)
}

/// Returns the focused element's area-relative cell for the given
/// terminal-relative `(column, row)`. Coordinates above or left of
/// the focused element saturate to `0`. Returns `None` when the
/// focus points at a dock that no longer exists in the workspace's
/// dock map.
pub fn translate_mouse_to_focused(stoat: &Stoat, column: u16, row: u16) -> Option<(u16, u16)> {
    let ws = stoat.active_workspace();
    let area = match ws.focus {
        FocusTarget::SplitPane => ws.panes.pane(ws.panes.focus()).area,
        FocusTarget::Dock(dock_id) => ws.docks.get(dock_id)?.area,
    };
    Some((column.saturating_sub(area.x), row.saturating_sub(area.y)))
}

/// Hit-tests a terminal-global `(column, row)` against the active
/// workspace's focusable panels.
///
/// Split panes are tested before docks. Returns `None` for a point in a
/// divider gap or over no panel. A hidden dock has a zero-width `area`, so
/// it never matches. Unlike [`FocusTarget`], the pane hit carries the id of
/// the pane actually under the cursor, which is not necessarily the focused
/// one.
fn target_at(stoat: &Stoat, column: u16, row: u16) -> Option<PanelHit> {
    let ws = stoat.active_workspace();
    let pos = Position::new(column, row);
    for (id, pane) in ws.panes.split_panes() {
        if pane.area.contains(pos) {
            return Some(PanelHit::Pane(id));
        }
    }
    ws.docks
        .iter()
        .find(|(_, dock)| dock.area.contains(pos))
        .map(|(id, _)| PanelHit::Dock(id))
}

/// Moves focus to the panel under a terminal-global `(column, row)`. A
/// point over no panel is a no-op.
///
/// A split-pane target updates both the pane tree's focus and the
/// workspace focus so the two stay in sync, mirroring the keyboard focus
/// path. A dock target leaves the pane tree's focus at the last split
/// pane.
pub fn focus_at(stoat: &mut Stoat, column: u16, row: u16) {
    let Some(hit) = target_at(stoat, column, row) else {
        return;
    };
    // A mouse focus change closes any open hover. Its popup was anchored
    // against the previously focused editor and must not re-anchor here.
    // Focus stays put when the hit pane is already the focused one, so the
    // unit `SplitPane` is compared against the live pane-tree focus.
    let ws = stoat.active_workspace();
    let changed = match hit {
        PanelHit::Pane(id) => !matches!(ws.focus, FocusTarget::SplitPane) || ws.panes.focus() != id,
        PanelHit::Dock(id) => ws.focus != FocusTarget::Dock(id),
    };
    if changed {
        stoat.pending_hover = None;
        stoat.pending_hover_request.clear();
    }
    let ws = stoat.active_workspace_mut();
    match hit {
        PanelHit::Pane(id) => {
            ws.panes.set_focus(id);
            ws.focus = FocusTarget::SplitPane;
        },
        PanelHit::Dock(id) => ws.focus = FocusTarget::Dock(id),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_resolves_by_its_cell_center() {
        let spans = [0..50, 50..120];
        assert_eq!(
            [2, 3, 7].map(|column| tab_at(&spans, column)),
            [Some(0), Some(1), None],
        );
    }
}
