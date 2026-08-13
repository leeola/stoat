//! Mouse routing: which surface a click lands on, and what it means there.
//!
//! A press arrives as a screen cell and has to become an action on whatever
//! occupies that cell -- a modal, a minimap strip, a pane divider, a terminal
//! grid, a line of text. The hit tests here walk the same layout the paint
//! produced, so a click resolves to the surface the user saw rather than to
//! whatever the model currently holds.

use crate::{
    action_handlers,
    app::{
        modal_split_percent, modal_zoom_steps, ModalKind, ModalSeparator, PanelHit, SeparatorAxis,
        Stoat, UpdateEffect, MIN_PREVIEW_ROWS, MODAL_ZOOM_MAX, MODAL_ZOOM_MIN, PREVIEW_WHEEL_ROWS,
    },
    buffer::BufferId,
    editor_state::{EditorId, ScrollGlide},
    minimap::emit::minimap_view_window,
    pane::{FocusTarget, View},
    render::commit_picker::MIN_LIST_ROWS,
    run::GridSelection,
    term_session::{TermId, TermSelection},
};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::{Position, Rect};
use stoat_config::MinimapMode;
use stoat_text::{Bias, SelectionGoal};
use stoatty_protocol::window_ipc::{MouseButton as IpcMouseButton, MouseKind};

/// Map an aux window's pointer gesture onto the crossterm event kind the pane
/// mouse handlers dispatch on. A wheel becomes a scroll. A button gesture keeps
/// its button.
///
/// [`None`] for the side buttons, which crossterm does not model.
/// [`Stoat::handle_jumplist_buttons`] takes every gesture of those before this
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

    let size = stoat.size();

    let (list, selected, filtered_len) = if let Some(finder) = stoat.file_finder.as_ref() {
        let Some(layout) = crate::render::file_finder::file_finder_layout(
            size,
            finder.content_size,
            modal_zoom_steps(&stoat.modal_zoom, ModalKind::FileFinder),
            modal_split_percent(&stoat.modal_split, ModalKind::FileFinder),
        ) else {
            return UpdateEffect::None;
        };
        let core = finder.active_core_ref();
        (
            layout.list,
            core.picklist.selected,
            core.picklist.filtered.len(),
        )
    } else if let Some(palette) = stoat.command_palette.as_ref() {
        let rows = palette.list_rows_hint();
        let zoom = modal_zoom_steps(&stoat.modal_zoom, ModalKind::Palette);
        if palette.command.is_none() {
            let Some(layout) =
                crate::render::command_palette::palette_filter_layout(size, rows, zoom)
            else {
                return UpdateEffect::None;
            };
            (layout.list, palette.selected, palette.filtered.len())
        } else if palette.arg_source().is_some()
            && let Some(picker) = palette.arg_picker.as_ref()
        {
            let Some(list) =
                crate::render::command_palette::palette_arg_list_rect(size, rows, zoom)
            else {
                return UpdateEffect::None;
            };
            let core = picker.active_core_ref();
            (list, core.picklist.selected, core.picklist.filtered.len())
        } else {
            return UpdateEffect::None;
        }
    } else {
        return UpdateEffect::None;
    };

    if !list.contains(Position::new(mouse.column, mouse.row)) {
        return UpdateEffect::None;
    }
    let rows = list.height as usize;
    let start_row = crate::render::picker::window_start(selected, rows);
    let index = start_row + (mouse.row - list.y) as usize;
    if index >= filtered_len {
        return UpdateEffect::None;
    }

    let delta = index as i32 - selected as i32;
    if stoat.file_finder.is_some() {
        action_handlers::file_finder_move_selection(stoat, delta)
    } else {
        action_handlers::palette_move_selection(stoat, delta).unwrap_or(UpdateEffect::Redraw)
    }
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
pub(crate) fn open_modal_box(stoat: &Stoat, kind: ModalKind, zoom: i8) -> Option<Rect> {
    let size = stoat.size();
    let split = modal_split_percent(&stoat.modal_split, kind);

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
        ModalKind::FileFinder => {
            let finder = stoat.file_finder.as_ref()?;
            crate::render::file_finder::file_finder_layout(size, finder.content_size, zoom, split)?
                .modal
        },
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
        // Filter mode and argument mode share one box, so the hint alone
        // sizes both.
        ModalKind::Palette => {
            let palette = stoat.command_palette.as_ref()?;
            crate::render::command_palette::palette_filter_layout(
                size,
                palette.list_rows_hint(),
                zoom,
            )?
            .modal
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
/// renderer reads so a hit-test lands where the user sees the line.
///
/// `None` when no modal with a separator is open, or when the one that is
/// shows no preview -- a modal too small for two panes has no line to grab.
pub(crate) fn open_modal_separator(stoat: &Stoat) -> Option<ModalSeparator> {
    let size = stoat.size();

    // The picker stacks its diff under the table, so its separator runs
    // along the modal's width and a drag redistributes rows.
    if let Some(picker) = stoat.commit_picker.as_ref() {
        let layout = crate::render::commit_picker::commit_picker_layout(
            size,
            picker.graph_lanes,
            modal_zoom_steps(&stoat.modal_zoom, ModalKind::CommitPicker),
            modal_split_percent(&stoat.modal_split, ModalKind::CommitPicker),
        )?;
        layout.preview?;
        let inner = layout.inner;
        return Some(ModalSeparator {
            kind: ModalKind::CommitPicker,
            axis: SeparatorAxis::Rows,
            line: layout.list.y + layout.list.height,
            span: inner.x..inner.x + inner.width,
            body: layout.list.y..inner.y + inner.height,
            min_list: MIN_LIST_ROWS,
            min_preview: MIN_PREVIEW_ROWS,
        });
    }

    // The finder family puts its preview beside the list, so the separator
    // runs down the body and a drag redistributes columns.
    let (kind, inner, list, preview) = if let Some(finder) = stoat.file_finder.as_ref() {
        let kind = ModalKind::FileFinder;
        let layout = crate::render::file_finder::file_finder_layout(
            size,
            finder.content_size,
            modal_zoom_steps(&stoat.modal_zoom, kind),
            modal_split_percent(&stoat.modal_split, kind),
        )?;
        (kind, layout.inner, layout.list, layout.preview)
    } else if stoat.code_search.is_some() {
        // Search hits are unbounded, so code search declares the whole area
        // exactly as its renderer does.
        let kind = ModalKind::CodeSearch;
        let layout = crate::render::file_finder::file_finder_layout(
            size,
            (u16::MAX, u16::MAX),
            modal_zoom_steps(&stoat.modal_zoom, kind),
            modal_split_percent(&stoat.modal_split, kind),
        )?;
        (kind, layout.inner, layout.list, layout.preview)
    } else if let Some(finder) = stoat.symbol_finder.as_ref() {
        let kind = ModalKind::SymbolFinder;
        let (_, inner, list, preview) = crate::render::symbol_finder::symbol_finder_layout(
            size,
            finder.content_rows,
            modal_zoom_steps(&stoat.modal_zoom, kind),
            modal_split_percent(&stoat.modal_split, kind),
        )?;
        (kind, inner, list, preview)
    } else {
        return None;
    };
    preview?;

    Some(ModalSeparator {
        kind,
        axis: SeparatorAxis::Columns,
        line: list.x + list.width,
        span: list.y..list.y + list.height,
        body: inner.x..inner.x + inner.width,
        min_list: crate::render::picker::MIN_PANE_COLUMNS,
        min_preview: crate::render::picker::MIN_PANE_COLUMNS,
    })
}

/// Arm a separator drag when `mouse` presses the open modal's list/preview
/// separator, reporting whether it did.
fn arm_modal_separator(stoat: &mut Stoat, mouse: MouseEvent) -> bool {
    let Some(separator) = open_modal_separator(stoat).filter(|s| s.hit(&mouse)) else {
        return false;
    };

    stoat.modal_separator_drag = Some(separator.kind);
    true
}

/// Move the armed separator to `mouse`, storing the share it lands on.
///
/// A modal that closed or shrank out of its two-pane layout mid-drag leaves
/// the share alone, so a stale arm can never write against a separator that
/// is no longer there.
fn drag_modal_separator(stoat: &mut Stoat, mouse: MouseEvent) -> UpdateEffect {
    let Some(separator) =
        open_modal_separator(stoat).filter(|s| Some(s.kind) == stoat.modal_separator_drag)
    else {
        return UpdateEffect::None;
    };
    let Some(share) = separator.share_at(&mouse) else {
        return UpdateEffect::None;
    };

    stoat.modal_split.insert(separator.kind, share);
    UpdateEffect::Redraw
}

/// Route a mouse press to the open location picker.
///
/// A left press on a row selects it, and a press on the row already selected
/// jumps to it. The two-step keeps a misclick from navigating away, which
/// single-click-to-jump could not, and needs none of the timing machinery a
/// double-click would. Every other press, drag, and release is swallowed so
/// the buffer beneath keeps its cursor and focus.
fn handle_location_picker_mouse(stoat: &mut Stoat, mouse: MouseEvent) -> UpdateEffect {
    let MouseEventKind::Down(MouseButton::Left) = mouse.kind else {
        return UpdateEffect::None;
    };
    let Some(picker) = stoat.location_picker.as_ref() else {
        return UpdateEffect::None;
    };
    let (entries_len, selected) = (picker.entries().len(), picker.selected());

    let Some((_, inner)) =
        crate::render::location_picker::location_picker_layout(stoat.size(), entries_len)
    else {
        return UpdateEffect::None;
    };
    if !inner.contains(Position::new(mouse.column, mouse.row)) {
        return UpdateEffect::None;
    }

    let start = crate::render::picker::window_start(selected, inner.height as usize);
    let index = start + (mouse.row - inner.y) as usize;
    if index >= entries_len {
        return UpdateEffect::None;
    }
    if index == selected {
        return action_handlers::picker::location_picker_select(stoat);
    }

    stoat
        .location_picker
        .as_mut()
        .expect("picker present")
        .set_selected(index);
    UpdateEffect::Redraw
}

pub(crate) fn handle_mouse(stoat: &mut Stoat, mouse: MouseEvent) -> UpdateEffect {
    if matches!(
        mouse.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) {
        return handle_mouse_scroll(stoat, mouse);
    }
    if let MouseEventKind::Moved = mouse.kind {
        return handle_hover(stoat, mouse.column, mouse.row);
    }

    if stoat.location_picker.is_some() {
        return handle_location_picker_mouse(stoat, mouse);
    }

    // Any open modal with a list owns the pointer. A left click selects a row
    // or grabs the modal's separator, and every other press, drag, or release
    // is swallowed so nothing reaches divider arming, focus, or the panes
    // beneath the modal covering them. The wheel is unaffected --
    // handle_mouse_scroll returns above this.
    if stoat.file_finder.is_some()
        || stoat.command_palette.is_some()
        || stoat.commit_picker.is_some()
        || stoat.code_search.is_some()
        || stoat.symbol_finder.is_some()
    {
        return handle_modal_mouse(stoat, mouse);
    }

    // A divider drag owns the pointer once armed. It resizes on drag,
    // releases on up, and swallows the rest so pane handlers never see it.
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

    // A left-button drag over an open hover popup selects its text. Routed
    // ahead of focus_at and the pane handlers so the click never reaches the
    // editor, leaving the buffer selection and cursor untouched.
    if stoat.pending_hover.is_some()
        && let Some(effect) = handle_hover_selection_mouse(stoat, mouse)
    {
        return effect;
    }

    // A press or drag over a pane's minimap strip scrubs that pane, ahead of
    // focus_at and the text-area handlers so the strip owns the gesture.
    if let Some(effect) = handle_minimap_mouse(stoat, mouse) {
        return effect;
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
    let Some((col, row)) = translate_mouse_to_focused(stoat, mouse.column, mouse.row) else {
        return UpdateEffect::None;
    };
    apply_focused_pane_mouse(stoat, mouse.kind, col, row)
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

/// Route a left press or drag over a pane's minimap strip to a viewport
/// scrub. Returns `Some` when the event is consumed, `None` when it should
/// fall through to focus and the text-area handlers.
///
/// Once a press arms [`Stoat::minimap_drag`], every drag re-scrubs the named
/// editor and the release clears the field, so the strip owns the pointer for
/// the whole gesture and the text area never sees it.
fn handle_minimap_mouse(stoat: &mut Stoat, mouse: MouseEvent) -> Option<UpdateEffect> {
    if let Some(editor_id) = stoat.minimap_drag {
        match mouse.kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(strip) = minimap_strip_for(stoat, editor_id) {
                    scrub_minimap_editor(stoat, editor_id, strip, mouse.row);
                }
            },
            MouseEventKind::Up(MouseButton::Left) => stoat.minimap_drag = None,
            _ => {},
        }
        return Some(UpdateEffect::Redraw);
    }

    if let MouseEventKind::Down(MouseButton::Left) = mouse.kind {
        let pos = Position::new(mouse.column, mouse.row);
        // Single mode has one shared band that scrubs the focused editor.
        // Per-pane mode walks the panes to find the strip under the click.
        let hit = if stoat.minimap_mode() == MinimapMode::Single {
            stoat
                .single_minimap_rect
                .filter(|band| band.contains(pos))
                .and_then(|_| stoat.focused_editor_ids().map(|(id, _)| id))
        } else {
            let ws = stoat.active_workspace();
            ws.panes.split_panes().find_map(|(_, pane)| {
                let View::Editor(editor_id) = pane.view else {
                    return None;
                };
                let strip = ws.editors.get(editor_id)?.minimap_rect?;
                strip.contains(pos).then_some(editor_id)
            })
        };
        if let Some(editor_id) = hit
            && let Some(strip) = minimap_strip_for(stoat, editor_id)
        {
            stoat.minimap_drag = Some(editor_id);
            scrub_minimap_editor(stoat, editor_id, strip, mouse.row);
            return Some(UpdateEffect::Redraw);
        }
    }

    None
}

/// Resolve the minimap band `editor_id` scrubs against. In single mode this
/// is the shared window-right band, otherwise the editor's own per-pane strip
/// rect.
fn minimap_strip_for(stoat: &Stoat, editor_id: EditorId) -> Option<Rect> {
    if stoat.minimap_mode() == MinimapMode::Single {
        stoat.single_minimap_rect
    } else {
        stoat
            .active_workspace()
            .editors
            .get(editor_id)?
            .minimap_rect
    }
}

/// Ease `editor_id`'s viewport onto the file line the `strip` row under
/// `screen_row` points at, centered in the viewport.
///
/// Maps the strip-local cell row to a line with the same proportional math
/// the strip renders with, then jumps `scroll_row` and glides the offset up
/// to it like a page motion. `strip` is the caller-resolved band the editor
/// scrubs against (a per-pane rect or the shared single-mode band).
fn scrub_minimap_editor(stoat: &mut Stoat, editor_id: EditorId, strip: Rect, screen_row: u16) {
    let ws = &mut stoat.workspaces[stoat.active_workspace];
    let Some(editor) = ws.editors.get_mut(editor_id) else {
        return;
    };

    let snapshot = editor.display_map.snapshot();
    let viewport = editor.viewport_rows.unwrap_or(strip.height as u32).max(1);
    let strip_local_row = screen_row.saturating_sub(strip.y);
    // The strip is drawn in buffer lines, so the click resolves against the
    // same window the emit ships rather than the editor's display rows.
    let (view_top, view_visible) = minimap_view_window(&snapshot, editor.scroll_offset, viewport);
    let buffer_lines = snapshot.buffer_line_count();
    let target_line = crate::minimap::click_target_line(
        strip.height,
        strip_local_row,
        buffer_lines as f32,
        view_top,
        view_visible as f32,
    );

    // A strip taller than the file resolves cells past its last line, and
    // scrolling happens in display rows, so the target converts back.
    let target_line = target_line.min(buffer_lines.saturating_sub(1));
    let target_display = snapshot
        .buffer_to_display(stoat_text::Point::new(target_line, 0))
        .row;

    let max_scroll = snapshot
        .line_count()
        .saturating_sub(1)
        .saturating_sub(viewport.saturating_sub(1));
    let target_row = target_display.saturating_sub(viewport / 2).min(max_scroll);

    let prev = editor.scroll_row;
    editor.scroll_row = target_row;
    if editor.scroll_offset.floor() as u32 != prev {
        editor.scroll_offset = prev as f32;
    }
    editor.scroll_glide = ScrollGlide::Page;
}

/// Route a left-button press over the open hover popup to its text
/// selection. Returns `Some` when the event is consumed (a press, drag, or
/// release over the popup), `None` when it should fall through to normal
/// mouse handling.
///
/// A press inside starts a drag selection. A press outside clears any
/// selection and falls through, leaving the popup open. A release keeps the
/// selection live and copies it to the clipboard when non-empty.
fn handle_hover_selection_mouse(stoat: &mut Stoat, mouse: MouseEvent) -> Option<UpdateEffect> {
    let popup_area = stoat.pending_hover.as_ref()?.area;
    let inside = popup_area.contains(Position {
        x: mouse.column,
        y: mouse.row,
    });

    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if !inside {
                if let Some(popup) = stoat.pending_hover.as_mut() {
                    popup.selection = None;
                }
                return None;
            }
            let pos = crate::render::hover::hover_hit_test(
                stoat.pending_hover.as_ref()?,
                mouse.column,
                mouse.row,
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

/// Scrolls the pane under the wheel pointer.
///
/// A `View::Editor` split pane gets inertial velocity, so a notch starts
/// or accelerates a momentum glide. A `View::Run` pane (split or dock) does
/// plain stepped scrolling of its output, three rows per notch, clamped to
/// the top. Anything else drops the event.
fn handle_mouse_scroll(stoat: &mut Stoat, mouse: MouseEvent) -> UpdateEffect {
    // The location picker is modal, so it owns the wheel wherever the
    // pointer sits and browses its candidates with it.
    if let Some(picker) = stoat.location_picker.as_mut() {
        match mouse.kind {
            MouseEventKind::ScrollDown => picker.select_next(),
            MouseEventKind::ScrollUp => picker.select_prev(),
            _ => return UpdateEffect::None,
        }
        return UpdateEffect::Redraw;
    }

    // A wheel while a finder or palette modal is open moves its selection
    // rather than scrolling the pane beneath, so the event never falls
    // through. The two modals are mutually exclusive, so two checks suffice.
    if stoat.file_finder.is_some() || stoat.command_palette.is_some() {
        let down = match mouse.kind {
            MouseEventKind::ScrollDown => true,
            MouseEventKind::ScrollUp => false,
            _ => return UpdateEffect::None,
        };
        let size = stoat.size();

        // A wheel over the visible preview pane scrolls the preview content
        // instead of moving the selection, mirroring the editor-pane path.
        let preview = if let Some(finder) = stoat.file_finder.as_ref() {
            crate::render::file_finder::file_finder_layout(
                size,
                finder.content_size,
                modal_zoom_steps(&stoat.modal_zoom, ModalKind::FileFinder),
                modal_split_percent(&stoat.modal_split, ModalKind::FileFinder),
            )
            .and_then(|layout| layout.preview)
            .map(|rect| (rect, finder.active_core_ref().preview.editor))
        } else if let Some(palette) = stoat.command_palette.as_ref() {
            if palette.arg_source().is_some()
                && let Some(picker) = palette.arg_picker.as_ref()
            {
                crate::render::command_palette::palette_arg_body(
                    size,
                    palette.list_rows_hint(),
                    modal_zoom_steps(&stoat.modal_zoom, ModalKind::Palette),
                )
                .and_then(|(_, preview)| preview)
                .map(|rect| (rect, picker.active_core_ref().preview.editor))
            } else {
                None
            }
        } else {
            None
        };

        if let Some((rect, editor_id)) = preview
            && rect.contains(Position::new(mouse.column, mouse.row))
        {
            if let Some(editor) = stoat.active_workspace_mut().editors.get_mut(editor_id) {
                action_handlers::movement::wheel_scroll(editor, down);
            }
            return UpdateEffect::None;
        }

        let delta = if down { 1 } else { -1 };
        return if stoat.file_finder.is_some() {
            action_handlers::file_finder_move_selection(stoat, delta)
        } else {
            action_handlers::palette_move_selection(stoat, delta).unwrap_or(UpdateEffect::Redraw)
        };
    }

    // The commit picker is modal too, so a wheel over it is the picker's.
    // Over the diff it scrolls the diff. Anywhere else it walks the list and
    // the diff follows the selection. Without this arm the event falls
    // through to whatever pane the modal is covering.
    if stoat.commit_picker.is_some() {
        let down = match mouse.kind {
            MouseEventKind::ScrollDown => true,
            MouseEventKind::ScrollUp => false,
            _ => return UpdateEffect::None,
        };

        let lanes = stoat.commit_picker.as_ref().and_then(|p| p.graph_lanes);
        let preview = crate::render::commit_picker::commit_picker_layout(
            stoat.size(),
            lanes,
            modal_zoom_steps(&stoat.modal_zoom, ModalKind::CommitPicker),
            modal_split_percent(&stoat.modal_split, ModalKind::CommitPicker),
        )
        .and_then(|layout| layout.preview);

        if let Some(rect) = preview
            && rect.contains(Position::new(mouse.column, mouse.row))
            && let Some(picker) = stoat.commit_picker.as_mut()
        {
            picker.preview_scroll = match down {
                true => picker.preview_scroll.saturating_add(PREVIEW_WHEEL_ROWS),
                false => picker.preview_scroll.saturating_sub(PREVIEW_WHEEL_ROWS),
            };
            return UpdateEffect::Redraw;
        }

        let delta = if down { 1 } else { -1 };
        return action_handlers::review_walk::commit_picker_step(stoat, delta);
    }

    // A wheel over the open hover popup scrolls the popup, not the pane
    // beneath it. The bump mirrors the Ctrl-d/Ctrl-u path. render_hover
    // clamps it to the content height.
    if let Some(popup) = stoat.pending_hover.as_mut()
        && popup.area.contains(Position::new(mouse.column, mouse.row))
    {
        match mouse.kind {
            MouseEventKind::ScrollDown => popup.scroll_half_pages += 1,
            MouseEventKind::ScrollUp => {
                popup.scroll_half_pages = popup.scroll_half_pages.saturating_sub(1)
            },
            _ => {},
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

    scroll_view_at(
        stoat,
        view,
        area,
        matches!(mouse.kind, MouseEventKind::ScrollDown),
    )
}

/// Advance the wheel-scroll target of `view` (occupying `area`), scrolling
/// down when `down`.
///
/// The pane-resolved half of the wheel path, shared by the primary hit-test
/// and an aux window's wheel events. An editor only moves its glide target,
/// so it reports no redraw -- the frame tick eases and renders, and a
/// trackpad flick of ~100 events must not repaint per event.
pub(crate) fn scroll_view_at(
    stoat: &mut Stoat,
    view: View,
    area: Rect,
    down: bool,
) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    match view {
        View::Editor(id) => {
            let Some(editor) = ws.editors.get_mut(id) else {
                return UpdateEffect::None;
            };
            action_handlers::movement::wheel_scroll(editor, down);
            UpdateEffect::None
        },
        View::Run(id) => {
            let Some(run_state) = ws.runs.get_mut(id) else {
                return UpdateEffect::None;
            };
            run_state.wheel_scroll(down, (area.height as usize).saturating_sub(1));
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
                    Some((id, pane.area))
                } else {
                    None
                }
            },
            FocusTarget::Dock(dock_id) => ws.docks.get(dock_id).and_then(|dock| {
                if let View::Run(id) = dock.view {
                    Some((id, dock.area))
                } else {
                    None
                }
            }),
        }
    };
    let Some((run_id, area)) = target else {
        return false;
    };
    let clipboard_host = stoat.clipboard_host.clone();
    let env_host = stoat.env_host.clone();
    let ws = stoat.active_workspace_mut();
    let Some(run_state) = ws.runs.get_mut(run_id) else {
        return false;
    };
    let pos = run_state.active_block_grid_pos(area, col, row);
    let Some(block) = run_state.active_block_mut() else {
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
            let Some(sel) = block.selection.as_ref() else {
                return false;
            };
            if sel.anchor == sel.head {
                return false;
            }
            let text = block.grid.text_for_selection(sel);
            if text.is_empty() {
                return false;
            }
            crate::host::clipboard_copy(clipboard_host.as_ref(), env_host.as_ref(), &text);
            false
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
fn focused_term_target(stoat: &Stoat) -> Option<(TermId, Rect)> {
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
    crate::render::editor::build_diagnostic_span_cache(editor, diagnostics, &path, buffer);
    let spans = editor
        .diagnostic_span_cache
        .as_ref()
        .map_or(&[][..], |cache| cache.spans.as_slice());
    crate::render::editor::diagnostic_at_offset(spans, offset)
}

/// Track the hovered cell and redraw only when the diagnostic under it
/// changes, so mouse motion within one span does not repaint every event.
///
/// Motion that stays inside one cell reuses the last answer rather than
/// resolving again. That answer only drives the redraw decision, and the
/// paint recomputes the hovered diagnostic from the cell every frame, so a
/// diagnostic set arriving from a server still reaches the screen through
/// its own redraw. The badge hit test is two comparisons and reruns
/// regardless, since the badge rect moves under a pointer that has not.
pub(crate) fn handle_hover(stoat: &mut Stoat, column: u16, row: u16) -> UpdateEffect {
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
    if stoat.hover_diag == resolved && stoat.lsp_badge_hovered == badge_hovered {
        return UpdateEffect::None;
    }
    stoat.hover_diag = resolved;
    stoat.lsp_badge_hovered = badge_hovered;
    UpdateEffect::Redraw
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
            crate::host::clipboard_copy(clipboard_host.as_ref(), env_host.as_ref(), &text);
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
/// marks the drag moved. `Up` copies the selected text to the clipboard and
/// keeps it highlighted when the drag moved, and otherwise clears it so a
/// plain click leaves no selection. Coordinates are pane-relative cells.
fn handle_terminal_pane_mouse(stoat: &mut Stoat, kind: MouseEventKind, col: u16, row: u16) -> bool {
    let Some((term_id, _area)) = focused_term_target(stoat) else {
        return false;
    };
    let (rows, cols) = {
        let ws = stoat.active_workspace();
        let Some(session) = ws.terms.get(term_id) else {
            return false;
        };
        (session.term.rows(), session.term.cols())
    };
    if rows == 0 || cols == 0 {
        return false;
    }

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
                session.selection = Some(TermSelection::new(cell_row, cell_col));
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
            match session.selection.as_mut() {
                // Reporting no change is what drops the frame for a drag
                // that landed back on the cell the head already holds.
                Some(selection) => selection.extend_to(cell_row, cell_col),
                None => true,
            }
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
                crate::host::clipboard_copy(clipboard_host.as_ref(), env_host.as_ref(), &text);
            }
            false
        },
        _ => false,
    }
}

/// Drop any mouse selection on `term_id`, so the next keystroke, click, or
/// drag starts fresh.
pub(crate) fn clear_term_selection(stoat: &mut Stoat, term_id: TermId) {
    if let Some(session) = stoat.active_workspace_mut().terms.get_mut(term_id) {
        session.selection = None;
    }
}

pub(crate) fn editor_screen_to_offset(
    stoat: &mut Stoat,
    editor_id: EditorId,
    area: Rect,
    col: u16,
    row: u16,
) -> Option<usize> {
    if col >= area.width || row >= area.height {
        return None;
    }
    let ws = stoat.active_workspace_mut();
    let editor = ws.editors.get_mut(editor_id)?;
    let scroll_row = editor.scroll_row;
    // The conflict view is three columns. A click in the center text maps to
    // its cell; a click in either side column (or a gutter) drops to col 0 so
    // the cursor lands on the aligned center row's start.
    if editor.conflict_view.is_some() {
        let cols = crate::render::conflict_view::ConflictColumns::compute(area);
        let center_gutter = cols.center_text_x.saturating_sub(area.x);
        let center_end = cols.sep2_x.saturating_sub(area.x);
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
            row,
        );
    }
    // The diff view puts the editable text in the right column, so a click
    // maps against the right column's start rather than the left gutter.
    // Cells left of it (the base column and gutters) clamp to the line start.
    let gutter_width = if editor.diff_view {
        crate::render::review::right_text_x(area).saturating_sub(area.x)
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
pub(crate) fn translate_mouse_to_focused(
    stoat: &Stoat,
    column: u16,
    row: u16,
) -> Option<(u16, u16)> {
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
pub(crate) fn focus_at(stoat: &mut Stoat, column: u16, row: u16) {
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
        stoat.pending_hover_request = None;
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
    use crate::test_fixture::{mouse_event, open_scratch_file};
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_action::OpenFile;
    /// A buffer with `count` single-line diagnostics on consecutive rows,
    /// painted once so the pane areas and the render's span cache exist.
    fn hover_diagnostics_harness(count: u32) -> (crate::test_harness::TestHarness, EditorId) {
        let mut h = Stoat::test();
        let root = PathBuf::from("/diag-hover");
        let path = root.join("a.txt");
        h.fake_fs()
            .insert_file(&path, b"alpha bravo\ncharlie delta\necho foxtrot\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path: path.clone() });
        h.settle();

        set_hover_diagnostics(&mut h, &path, count);
        let _ = h.stoat.render();
        let (editor_id, _) = h.stoat.focused_editor_ids().expect("a focused editor");
        (h, editor_id)
    }

    fn set_hover_diagnostics(h: &mut crate::test_harness::TestHarness, path: &Path, count: u32) {
        let diagnostics = (0..count)
            .map(|line| lsp_types::Diagnostic {
                range: lsp_types::Range {
                    start: lsp_types::Position { line, character: 0 },
                    end: lsp_types::Position { line, character: 5 },
                },
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                ..Default::default()
            })
            .collect();
        h.seed_diagnostics(path, diagnostics);
    }

    fn cached_span_count(h: &crate::test_harness::TestHarness, editor_id: EditorId) -> usize {
        h.stoat
            .active_workspace()
            .editors
            .get(editor_id)
            .expect("editor exists")
            .diagnostic_span_cache
            .as_ref()
            .expect("a painted editor has a span cache")
            .spans
            .len()
    }

    // TEST IMPORTS

    #[test]
    fn mouse_translates_to_focused_pane_coords() {
        let mut h = Stoat::test();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(10, 5, 20, 8);
        let translated = translate_mouse_to_focused(&h.stoat, 15, 9);
        assert_eq!(translated, Some((5, 4)));
    }

    #[test]
    fn mouse_above_focused_pane_saturates_to_zero() {
        let mut h = Stoat::test();
        let pane_id = h.stoat.active_workspace().panes.focus();
        h.stoat.active_workspace_mut().panes.pane_mut(pane_id).area = Rect::new(10, 5, 20, 8);
        let translated = translate_mouse_to_focused(&h.stoat, 3, 2);
        assert_eq!(translated, Some((0, 0)));
    }

    #[test]
    fn hover_popup_overflows_across_a_vertical_split() {
        use crate::{render::hover::HoverPopup, test_harness::TestHarness};
        use ratatui::style::Style;

        let mut h = TestHarness::with_size(80, 24);
        let root = PathBuf::from("/hover");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let left = {
            let ws = h.stoat.active_workspace_mut();
            let left = ws.panes.focus();
            ws.panes.split(crate::pane::Axis::Vertical);
            ws.panes.resize(Rect::new(0, 0, 80, 24));
            left
        };
        let left_content = crate::render::layout::split_pane_status(
            h.stoat.active_workspace().panes.pane(left).area,
        )
        .0;
        focus_at(&mut h.stoat, left_content.x + 1, left_content.y + 1);
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;

        // A hover wider than the left pane.
        h.stoat.pending_hover = Some(HoverPopup::new(
            vec![vec![("x".repeat(60), Style::default())]],
            0,
            editor_id,
        ));

        let (popup, _) = crate::render::hover::hover_popup_layout(&mut h.stoat).expect("layout");
        assert!(
            popup.width > left_content.width,
            "the popup widens past the left pane ({} > {})",
            popup.width,
            left_content.width,
        );
        assert!(
            popup.x + popup.width > left_content.x + left_content.width,
            "the popup crosses the divider into the right pane"
        );
    }

    #[test]
    fn closing_a_mouse_focused_pane_leaves_focus_readers_panic_free() {
        // Mouse-focusing a split pane then closing it used to leave that pane's
        // id dangling in `ws.focus`, so the next focus-reading event dereferenced
        // a freed SlotMap key and panicked. `FocusTarget::SplitPane` is now a unit
        // variant resolved through the live pane-tree focus.
        use crate::test_harness::TestHarness;

        let mut h = TestHarness::with_size(80, 24);
        let root = PathBuf::from("/close");
        let path = root.join("a.txt");
        h.fake_fs().insert_file(&path, b"alpha\n");
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let right = {
            let ws = h.stoat.active_workspace_mut();
            let right = ws.panes.split(crate::pane::Axis::Vertical);
            ws.panes.resize(Rect::new(0, 0, 80, 24));
            right
        };
        let right_area = h.stoat.active_workspace().panes.pane(right).area;
        focus_at(&mut h.stoat, right_area.x + 1, right_area.y + 1);
        assert_eq!(
            h.stoat.active_workspace().panes.focus(),
            right,
            "the mouse click focused the right pane"
        );

        action_handlers::dispatch(&mut h.stoat, &stoat_action::ClosePane);
        assert!(
            !h.stoat
                .active_workspace()
                .panes
                .split_pane_ids()
                .contains(&right),
            "the closed pane is gone"
        );

        // Each of these resolves the focused pane. Before the fix they panicked
        // on the stale id left behind in `ws.focus`.
        assert!(translate_mouse_to_focused(&h.stoat, 1, 1).is_some());
        focus_at(&mut h.stoat, 1, 1);
        let ws = h.stoat.active_workspace();
        assert!(
            ws.panes.contains(ws.panes.focus()),
            "focus resolves to a live pane after the close"
        );
    }

    #[test]
    fn hover_popup_overflows_into_the_pane_below() {
        use crate::{render::hover::HoverPopup, test_harness::TestHarness};
        use ratatui::style::Style;

        let mut h = TestHarness::with_size(40, 24);
        let root = PathBuf::from("/hover");
        let path = root.join("a.txt");
        let content: String = (0..40).map(|_| "x\n").collect();
        h.fake_fs().insert_file(&path, content.as_bytes());
        h.stoat.active_workspace_mut().git_root = root;
        action_handlers::dispatch(&mut h.stoat, &OpenFile { path });
        h.settle();

        let top = {
            let ws = h.stoat.active_workspace_mut();
            let top = ws.panes.focus();
            ws.panes.split(crate::pane::Axis::Horizontal);
            ws.panes.resize(Rect::new(0, 0, 40, 24));
            top
        };
        let top_content = crate::render::layout::split_pane_status(
            h.stoat.active_workspace().panes.pane(top).area,
        )
        .0;
        focus_at(&mut h.stoat, top_content.x + 1, top_content.y + 1);
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;

        // Anchor on the top pane's last visible row (each "x\n" line is 2 bytes).
        let last_row_line = top_content.height as usize - 1;
        h.stoat.pending_hover = Some(HoverPopup::new(
            vec![vec![("hi".to_string(), Style::default())]],
            last_row_line * 2,
            editor_id,
        ));

        let (popup, _) = crate::render::hover::hover_popup_layout(&mut h.stoat).expect("layout");
        let cursor_row = top_content.y + last_row_line as u16;
        assert!(
            popup.y > cursor_row,
            "the popup places below the cursor ({} > {}) instead of flipping above",
            popup.y,
            cursor_row,
        );
        assert!(
            popup.y >= top_content.y + top_content.height,
            "the popup overflows into the pane below"
        );
    }

    #[test]
    fn mouse_routes_to_focused_dock_when_focus_is_dock() {
        use crate::pane::{DockPanel, DockSide, DockVisibility, View};
        let mut h = Stoat::test();
        let dock_id = h.stoat.active_workspace_mut().docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Open { width: 30 },
            default_width: 30,
            area: Rect::new(50, 0, 30, 24),
        });
        h.stoat.active_workspace_mut().focus = FocusTarget::Dock(dock_id);
        let translated = translate_mouse_to_focused(&h.stoat, 60, 7);
        assert_eq!(translated, Some((10, 7)));
    }

    #[test]
    fn mouse_returns_none_when_focused_dock_missing() {
        use crate::pane::DockId;
        let mut h = Stoat::test();
        let dangling = DockId::default();
        h.stoat.active_workspace_mut().focus = FocusTarget::Dock(dangling);
        let translated = translate_mouse_to_focused(&h.stoat, 10, 10);
        assert_eq!(translated, None);
    }

    /// Mouse motion used to resolve every diagnostic in the file per event,
    /// ahead of the changed-dedupe. It now answers from the version-keyed cache
    /// the paint reads, so a sweep resolves the set once and the paint that
    /// follows finds it already built.
    #[test]
    fn hovering_rebuilds_the_shared_diagnostic_span_cache() {
        let (mut h, editor_id) = hover_diagnostics_harness(1);
        assert_eq!(
            cached_span_count(&h, editor_id),
            1,
            "the paint resolved the file's one diagnostic",
        );

        // A second diagnostic moves the set version, leaving the cache stale.
        set_hover_diagnostics(&mut h, &PathBuf::from("/diag-hover/a.txt"), 2);
        handle_hover(&mut h.stoat, 8, 0);

        assert_eq!(
            cached_span_count(&h, editor_id),
            2,
            "the hover refreshed the shared cache rather than resolving its own list",
        );
    }

    /// A pointer resting inside one cell reports motion repeatedly. The
    /// diagnostic under a cell that has not moved is the one already found, so
    /// the repeat answers from it and never reaches the resolve.
    #[test]
    fn a_repeat_hover_on_the_same_cell_resolves_nothing() {
        let (mut h, editor_id) = hover_diagnostics_harness(1);
        handle_hover(&mut h.stoat, 8, 0);
        assert_eq!(cached_span_count(&h, editor_id), 1);

        set_hover_diagnostics(&mut h, &PathBuf::from("/diag-hover/a.txt"), 3);

        assert_eq!(
            handle_hover(&mut h.stoat, 8, 0),
            UpdateEffect::None,
            "an unmoved pointer reports the diagnostic it already resolved",
        );
        assert_eq!(
            cached_span_count(&h, editor_id),
            1,
            "and leaves the cache for the next paint to refresh",
        );
    }

    #[test]
    fn hovering_the_lsp_badge_rect_opens_and_closes() {
        let mut h = Stoat::test();
        h.stoat.lsp_badge_rect = Some(Rect::new(10, 5, 6, 1));

        assert_eq!(
            handle_hover(&mut h.stoat, 12, 5),
            UpdateEffect::Redraw,
            "hovering onto the badge redraws"
        );
        assert!(h.stoat.lsp_badge_hovered, "hovering sets the flag");

        assert_eq!(
            handle_hover(&mut h.stoat, 11, 5),
            UpdateEffect::None,
            "still inside the badge is a no-op"
        );
        assert!(h.stoat.lsp_badge_hovered);

        assert_eq!(
            handle_hover(&mut h.stoat, 0, 0),
            UpdateEffect::Redraw,
            "moving off the badge redraws"
        );
        assert!(!h.stoat.lsp_badge_hovered, "moving off clears the flag");
    }

    #[test]
    fn badge_disappearing_clears_the_hover_flag() {
        let mut h = Stoat::test();
        h.stoat.lsp_badge_rect = Some(Rect::new(10, 5, 6, 1));
        handle_hover(&mut h.stoat, 12, 5);
        assert!(h.stoat.lsp_badge_hovered);

        // The scratch buffer has no language server, so the next paint stamps no
        // badge rect.
        let _ = h.stoat.render();
        assert!(h.stoat.lsp_badge_rect.is_none(), "no badge painted");
        assert!(
            !h.stoat.lsp_badge_hovered,
            "the hover flag clears when the badge vanishes"
        );
    }

    #[test]
    fn diff_view_click_maps_into_the_right_column() {
        let mut h = Stoat::test();
        open_scratch_file(&mut h, "keep\nnew\ntail\n");

        let (editor_id, buffer_id) = {
            let ws = h.stoat.active_workspace();
            let editor_id = match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => id,
                _ => panic!("focused pane is not an editor"),
            };
            (editor_id, ws.editors[editor_id].buffer_id)
        };
        {
            let base = "keep\nold\ntail\n";
            let text = "keep\nnew\ntail\n";
            let dm = crate::diff_map::DiffMap::from_structural_changes(
                stoat_language::structural_diff::diff(base, text),
                Arc::new(base.to_string()),
                text,
            );
            h.stoat
                .active_workspace()
                .buffers
                .get(buffer_id)
                .expect("buffer")
                .write()
                .expect("poisoned")
                .diff_map = Some(dm);
        }
        h.stoat.active_workspace_mut().editors[editor_id].set_diff_view(true);

        let area = Rect::new(0, 0, 120, 10);
        // At width 120 the two-column right text begins at col 68, so col 70 row 0
        // is the right column's third character of the context line "keep".
        assert_eq!(
            editor_screen_to_offset(&mut h.stoat, editor_id, area, 70, 0),
            Some(2),
            "a click in the right column lands on the buffer character"
        );
        assert_eq!(
            editor_screen_to_offset(&mut h.stoat, editor_id, area, 8, 0),
            Some(0),
            "a click left of the right column clamps to the buffer line start"
        );
    }

    #[test]
    fn conflict_view_click_maps_center_exact_and_sides_to_row_start() {
        use stoat_action::Conflict;

        let mut h = Stoat::test();
        let git_root = h.stoat.active_workspace().git_root.clone();
        h.fake_git().add_repo(git_root).conflicted_file(
            "f.txt",
            Some("a\nb\nc\n"),
            Some("a\nB\nc\n"),
            Some("a\nX\nc\n"),
        );
        action_handlers::dispatch(&mut h.stoat, &Conflict);

        let editor_id = {
            let ws = h.stoat.active_workspace();
            match ws.panes.pane(ws.panes.focus()).view {
                View::Editor(id) => id,
                _ => panic!("focused pane is not the conflict editor"),
            }
        };

        // At width 150 the center text starts at col 56 and the theirs column at
        // col 100. Row 1 is the "<<<<<<< ours" marker line.
        let area = Rect::new(0, 0, 150, 20);
        let row_start = editor_screen_to_offset(&mut h.stoat, editor_id, area, 56, 1);
        assert!(row_start.is_some(), "the center row resolves");

        assert_eq!(
            editor_screen_to_offset(&mut h.stoat, editor_id, area, 59, 1),
            row_start.map(|offset| offset + 3),
            "a center-column click maps to the exact clicked cell"
        );
        assert_eq!(
            editor_screen_to_offset(&mut h.stoat, editor_id, area, 3, 1),
            row_start,
            "an ours-column click lands on the aligned center row start"
        );
        assert_eq!(
            editor_screen_to_offset(&mut h.stoat, editor_id, area, 110, 1),
            row_start,
            "a theirs-column click lands on the aligned center row start"
        );
    }

    #[test]
    fn mouse_focus_change_closes_the_hover_popup() {
        use crate::render::hover::HoverPopup;
        use ratatui::style::Style;

        let mut h = Stoat::test();
        let ws = h.stoat.active_workspace_mut();
        let left = ws.panes.focus();
        let right = ws.panes.split(crate::pane::Axis::Vertical);
        ws.panes.resize(Rect::new(0, 0, 101, 24));
        let left_area = h.stoat.active_workspace().panes.pane(left).area;
        let right_area = h.stoat.active_workspace().panes.pane(right).area;

        // Focus the right pane through the real path so ws.focus tracks it, then
        // open a popup there. Its empty area makes any click land outside it.
        focus_at(&mut h.stoat, right_area.x + 1, right_area.y + 1);
        let editor_id = h.stoat.focused_editor_ids().expect("focused editor").0;
        h.stoat.pending_hover = Some(HoverPopup::new(
            vec![vec![("hi".to_string(), Style::default())]],
            0,
            editor_id,
        ));

        // A left-button Down over the other pane moves focus and closes it.
        h.stoat.update(mouse_event(
            MouseEventKind::Down(MouseButton::Left),
            left_area.x + 1,
            left_area.y + 1,
        ));
        assert!(
            h.stoat.pending_hover.is_none(),
            "a mouse focus change closes the hover popup"
        );
    }
}
