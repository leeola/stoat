use crate::{
    app::{Stoat, UpdateEffect},
    command_palette::{PaletteOutcome, PalettePhase},
};
use std::path::PathBuf;
use stoat_action::ValueSource;
use stoat_scheduler::Task;
use tokio::sync::mpsc::UnboundedReceiver;

/// Candidates feeding a palette argument's inline value-picker, resolved from a
/// [`ValueSource`] by [`arg_candidates`].
#[allow(dead_code)]
pub(super) enum ArgCandidates {
    /// Streaming workspace file walk. Paths arrive in batches on `rx` while
    /// `task` runs the blocking walk. The task must be held to keep it alive.
    Walk {
        rx: UnboundedReceiver<Vec<PathBuf>>,
        task: Task<()>,
    },
    /// Fully-known path set, such as the currently-open buffer paths.
    Paths(Vec<PathBuf>),
}

/// Resolve an argument's [`ValueSource`] into the candidates its inline picker
/// lists.
///
/// `Files` streams workspace paths via the same background walk the file finder
/// uses. `Buffers` returns the currently-open buffer paths. `None` yields no
/// picker.
#[allow(dead_code)]
pub(super) fn arg_candidates(stoat: &Stoat, source: ValueSource) -> Option<ArgCandidates> {
    match source {
        ValueSource::None => None,
        ValueSource::Files => {
            let git_root = stoat.active_workspace().git_root.clone();
            let (rx, task) = super::file_finder::spawn_workspace_walk(stoat, git_root);
            Some(ArgCandidates::Walk { rx, task })
        },
        ValueSource::Buffers => Some(ArgCandidates::Paths(
            stoat.active_workspace().buffers.open_paths(),
        )),
    }
}

/// Advance the command palette on a submit keypress. Returns `Some(effect)`
/// when the palette consumed the submission (even if the outcome is a redraw
/// with no phase change), or `None` if no palette is open so the caller can
/// fall through to other prompt consumers.
pub(super) fn palette_submit(stoat: &mut Stoat) -> Option<UpdateEffect> {
    stoat.command_palette.as_ref()?;
    let outcome = {
        let active_idx = stoat.active_workspace;
        let executor = stoat.executor.clone();
        let workspaces = &mut stoat.workspaces;
        let palette = stoat.command_palette.as_mut()?;
        palette.handle_submit(&mut workspaces[active_idx], executor)
    };
    Some(apply_outcome(stoat, outcome))
}

/// Cancel the currently open palette, closing it and restoring the previous
/// mode.
pub(super) fn palette_cancel(stoat: &mut Stoat) -> Option<UpdateEffect> {
    close_palette(stoat).then_some(UpdateEffect::Redraw)
}

/// Insert a literal newline in the palette's active [`InputView`].
pub(super) fn palette_insert_newline(stoat: &mut Stoat) -> Option<UpdateEffect> {
    stoat.command_palette.as_ref()?;
    let active_idx = stoat.active_workspace;
    let workspaces = &mut stoat.workspaces;
    let palette = stoat.command_palette.as_mut()?;
    let input = match &mut palette.phase {
        PalettePhase::Filter { input, .. } => input,
        PalettePhase::CollectArgs { input, .. } => input,
    };
    let ws = &mut workspaces[active_idx];
    let current = input.text(ws);
    let new_text = format!("{current}\n");
    input.replace_text(ws, &new_text);
    Some(UpdateEffect::Redraw)
}

/// Move the filter selection. Returns `None` when the palette is not open or
/// not in the filter phase.
pub(super) fn palette_move_selection(stoat: &mut Stoat, delta: i32) -> Option<UpdateEffect> {
    let palette = stoat.command_palette.as_mut()?;
    let PalettePhase::Filter {
        filtered, selected, ..
    } = &mut palette.phase
    else {
        return None;
    };
    if filtered.is_empty() {
        *selected = 0;
        return Some(UpdateEffect::Redraw);
    }
    let max = (filtered.len() - 1) as i32;
    let next = (*selected as i32 + delta).clamp(0, max);
    *selected = next as usize;
    Some(UpdateEffect::Redraw)
}

/// Page the palette filter selection by half its rendered list height in `dir`
/// (-1 up, 1 down). Before the first render the viewport is unset and the step
/// falls back to a single row. No-op when the palette is not in the filter
/// phase, deferring to [`palette_move_selection`].
pub(super) fn palette_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    let step = match stoat.command_palette.as_ref() {
        Some(p) => p.viewport_rows.map(|v| v.div_ceil(2).max(1)).unwrap_or(1),
        None => return UpdateEffect::None,
    };
    palette_move_selection(stoat, dir * step as i32).unwrap_or(UpdateEffect::None)
}

/// Flip the palette's [`crate::command_palette::PaletteScope`]. No-op when the
/// palette is closed or is past the filter phase (args collection).
pub(super) fn palette_scope_toggle(stoat: &mut Stoat) -> UpdateEffect {
    let active_idx = stoat.active_workspace;
    let workspaces = &mut stoat.workspaces;
    let Some(palette) = stoat.command_palette.as_mut() else {
        return UpdateEffect::None;
    };
    palette.toggle_scope(&workspaces[active_idx]);
    UpdateEffect::Redraw
}

fn apply_outcome(stoat: &mut Stoat, outcome: PaletteOutcome) -> UpdateEffect {
    match outcome {
        PaletteOutcome::None => UpdateEffect::Redraw,
        PaletteOutcome::Close => {
            close_palette(stoat);
            UpdateEffect::Redraw
        },
        PaletteOutcome::Dispatch(entry, params) => {
            close_palette(stoat);
            match (entry.create)(&params) {
                Ok(action) => super::dispatch(stoat, &*action),
                Err(e) => {
                    tracing::warn!("palette dispatch `{}`: {e}", entry.def.name());
                    UpdateEffect::Redraw
                },
            }
        },
    }
}

/// Close the palette, disposing its [`InputView`] and restoring the saved
/// pre-palette mode. Returns `true` if a palette was open, `false` otherwise.
fn close_palette(stoat: &mut Stoat) -> bool {
    let Some(palette) = stoat.command_palette.take() else {
        return false;
    };
    {
        let active_idx = stoat.active_workspace;
        let workspaces = &mut stoat.workspaces;
        palette.dispose(&mut workspaces[active_idx]);
    }
    stoat.mode = palette.previous_mode.clone();
    true
}
