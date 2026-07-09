use crate::{
    app::{Stoat, UpdateEffect},
    command_palette::PaletteOutcome,
};
use std::path::PathBuf;
use stoat_action::ValueSource;
use stoat_scheduler::Task;
use tokio::sync::mpsc::UnboundedReceiver;

/// Candidates feeding a palette argument's inline value-picker, resolved from a
/// [`ValueSource`] by [`arg_candidates`].
pub(super) enum ArgCandidates {
    /// Streaming workspace file walk. Paths arrive in batches on `rx` while
    /// `task` runs the blocking walk. The task must be held to keep it alive.
    Walk {
        rx: UnboundedReceiver<Vec<PathBuf>>,
        task: Task<()>,
    },
    /// Fully-known path set, such as the currently-open buffer paths. Consumed
    /// by the buffer picker that lands with the `OpenBuffer` action.
    #[allow(dead_code)]
    Paths(Vec<PathBuf>),
}

/// Resolve an argument's [`ValueSource`] into the candidates its inline picker
/// lists.
///
/// `Files` streams workspace paths via the same background walk the file finder
/// uses. `Directories` streams the workspace's directories derived from that
/// walk. `Buffers` returns the currently-open buffer paths. `None` yields no
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
        ValueSource::Directories => {
            let git_root = stoat.active_workspace().git_root.clone();
            let (rx, task) = super::file_finder::spawn_workspace_dir_walk(stoat, git_root);
            Some(ArgCandidates::Walk { rx, task })
        },
        ValueSource::Buffers => Some(ArgCandidates::Paths(
            stoat.active_workspace().buffers.open_paths(),
        )),
    }
}

/// Sync the palette's inline file picker once per frame, before the palette is
/// painted.
///
/// Refilters the command list from the input, and when the input parses as a
/// command whose trailing argument sources files (e.g. `:o `), lazily spawns the
/// workspace walk on first entry, then drains it, refilters the path list, and
/// syncs the preview. A no-op when no palette is open or the input is not in
/// file-argument mode.
///
/// Lives here rather than on [`crate::command_palette::CommandPalette`] because
/// spawning the walk needs [`Stoat`]-level resources (the executor, fs host, and
/// redraw notifier) that a palette method does not see.
pub(crate) fn sync_palette_picker(stoat: &mut Stoat) {
    if stoat.command_palette.is_none() {
        return;
    }
    let active_idx = stoat.active_workspace;

    let resolved = {
        let ws = &mut stoat.workspaces[active_idx];
        let palette = stoat.command_palette.as_mut().expect("palette present");
        palette.refilter_from_input(ws);
        palette.arg_source().zip(palette.arg_tail(ws))
    };
    let Some((source, tail)) = resolved else {
        return;
    };

    let needs_picker = stoat
        .command_palette
        .as_ref()
        .is_some_and(|palette| palette.arg_picker.is_none());
    if needs_picker {
        let git_root = stoat.workspaces[active_idx].git_root.clone();
        let candidates = arg_candidates(stoat, source);
        let executor = stoat.executor.clone();
        let ws = &mut stoat.workspaces[active_idx];
        if let Some(palette) = stoat.command_palette.as_mut() {
            match candidates {
                Some(ArgCandidates::Walk { rx, task }) => palette.install_arg_picker(
                    ws,
                    executor,
                    source,
                    git_root,
                    Some((rx, task)),
                    Vec::new(),
                ),
                Some(ArgCandidates::Paths(paths)) => {
                    palette.install_arg_picker(ws, executor, source, git_root, None, paths)
                },
                None => {},
            }
        }
    }

    let fs_host = stoat.fs_host.clone();
    let ws = &mut stoat.workspaces[active_idx];
    if let Some(palette) = stoat.command_palette.as_mut() {
        palette.sync_arg_picker(&tail, ws, &*fs_host, &stoat.language_registry);
    }
}

/// Advance the command palette on a submit keypress. Returns `Some(effect)`
/// when the palette consumed the submission (even if the outcome is a redraw
/// without a dispatch), or `None` if no palette is open so the caller can
/// fall through to other prompt consumers.
pub(super) fn palette_submit(stoat: &mut Stoat) -> Option<UpdateEffect> {
    stoat.command_palette.as_ref()?;
    let outcome = {
        let active_idx = stoat.active_workspace;
        let workspaces = &mut stoat.workspaces;
        let palette = stoat.command_palette.as_mut()?;
        palette.handle_submit(&mut workspaces[active_idx])
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
    let ws = &mut workspaces[active_idx];
    let current = palette.input.text(ws);
    let new_text = format!("{current}\n");
    palette.input.replace_text(ws, &new_text);
    Some(UpdateEffect::Redraw)
}

/// Move the action-list selection. Returns `None` when the palette is closed.
pub(super) fn palette_move_selection(stoat: &mut Stoat, delta: i32) -> Option<UpdateEffect> {
    let palette = stoat.command_palette.as_mut()?;
    if palette.arg_source().is_some()
        && let Some(picker) = palette.arg_picker.as_mut()
    {
        picker.move_selection(delta);
        return Some(UpdateEffect::Redraw);
    }
    if palette.filtered.is_empty() {
        palette.selected = 0;
        return Some(UpdateEffect::Redraw);
    }
    let max = (palette.filtered.len() - 1) as i32;
    let next = (palette.selected as i32 + delta).clamp(0, max);
    palette.selected = next as usize;
    Some(UpdateEffect::Redraw)
}

/// Page the action-list selection by half its rendered list height in `dir`
/// (-1 up, 1 down). Before the first render the viewport is unset and the step
/// falls back to a single row. Delegates to [`palette_move_selection`].
pub(super) fn palette_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    if let Some(palette) = stoat.command_palette.as_mut()
        && palette.arg_source().is_some()
        && let Some(picker) = palette.arg_picker.as_mut()
    {
        picker.core.page(dir);
        return UpdateEffect::Redraw;
    }
    let step = match stoat.command_palette.as_ref() {
        Some(p) => p.viewport_rows.map(|v| v.div_ceil(2).max(1)).unwrap_or(1),
        None => return UpdateEffect::None,
    };
    palette_move_selection(stoat, dir * step as i32).unwrap_or(UpdateEffect::None)
}

/// Flip the palette's [`crate::command_palette::PaletteScope`] and re-filter.
/// No-op when the palette is closed.
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
    true
}
