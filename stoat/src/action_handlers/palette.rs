use crate::{
    app::{Stoat, UpdateEffect},
    command_palette::PaletteOutcome,
    file_finder::Browse,
    host::FsHost,
    picker::PathPicker,
};
use std::path::{Path, PathBuf};
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
    /// Fully-known path set, such as the currently-open buffer paths. Feeds the
    /// buffer picker (the `:b ` argument source).
    Paths(Vec<PathBuf>),
}

/// Resolve an argument's [`ValueSource`] into the candidates its inline picker
/// lists.
///
/// `Files` streams workspace paths via the same background walk the file finder
/// uses. `Directories` streams the workspace's directories derived from that
/// walk. `Buffers` returns the currently-open buffer paths. `None` yields no
/// picker.
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

    // A picker installed for a different source than the input now parses to
    // (the command head was edited, e.g. `:o ` to `cd `) is stale. Tear it down
    // so the correct-source picker installs below.
    {
        let ws = &mut stoat.workspaces[active_idx];
        if let Some(palette) = stoat.command_palette.as_mut()
            && palette
                .arg_picker
                .as_ref()
                .is_some_and(|picker| picker.source() != source)
        {
            palette.dispose_arg_picker(ws);
        }
    }

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

    if source == ValueSource::Directories {
        sync_arg_picker_browse(stoat, &tail);
    }

    let fs_host = stoat.fs_host.clone();
    let ws = &mut stoat.workspaces[active_idx];
    if let Some(palette) = stoat.command_palette.as_mut() {
        palette.sync_arg_picker(&tail, ws, &*fs_host, &stoat.language_registry);
    }
}

/// The immediate child directories of `root`, sorted non-hidden before hidden
/// and alphabetically within each group.
///
/// This is a one-level listing, never a recursive descent, so empty directories
/// appear too. A missing or unreadable root yields an empty list. The returned
/// order is the helper's contract. The picker re-sorts rows alphabetically for
/// display.
fn list_child_dirs(fs_host: &dyn FsHost, root: &Path) -> Vec<PathBuf> {
    let mut entries = fs_host.list_dir(root).unwrap_or_default();
    entries.retain(|entry| entry.is_dir);
    entries.sort_by(|a, b| {
        let a_hidden = a.name.starts_with('.');
        let b_hidden = b.name.starts_with('.');
        a_hidden.cmp(&b_hidden).then_with(|| a.name.cmp(&b.name))
    });
    entries
        .into_iter()
        .map(|entry| root.join(entry.name.as_str()))
        .collect()
}

/// Enter, re-root, or leave the `:cd` argument picker's directory-browse mode
/// for the current `tail`.
///
/// Mirrors [`super::file_finder::sync_file_finder_browse`] for the palette's
/// [`ValueSource::Directories`] argument. A `/` or `~/` tail lists the typed
/// directory's immediate child directories synchronously via
/// [`FsHost::list_dir`], re-rooting whenever the directory part changes. A tail
/// that stops being path-shaped drops browse so the workspace-derived directory
/// list resumes.
fn sync_arg_picker_browse(stoat: &mut Stoat, tail: &str) {
    let home = stoat.env_host.var("HOME");

    let resolved = crate::file_finder::split_path_query(tail, home.as_deref()).or_else(|| {
        // A relative tail with a `/` (e.g. `src/`) browses git_root/<dir part>,
        // mirroring split_path_query's split but rooted in the workspace. A bare
        // tail with no `/` falls through to leave_browse and keeps the recursive
        // workspace-directory list.
        let last_slash = tail.rfind('/')?;
        let typed_dir = tail[..=last_slash].to_string();
        let partial = tail[last_slash + 1..].to_string();
        let root = stoat
            .active_workspace()
            .git_root
            .join(typed_dir.trim_end_matches('/'));
        Some((typed_dir, root, partial))
    });

    let Some((typed_dir, root, partial)) = resolved else {
        let active_idx = stoat.active_workspace;
        if let Some(picker) = stoat
            .command_palette
            .as_mut()
            .and_then(|palette| palette.arg_picker.as_mut())
        {
            picker.leave_browse(&mut stoat.workspaces[active_idx]);
        }
        return;
    };

    let root_changed = stoat
        .command_palette
        .as_ref()
        .and_then(|palette| palette.arg_picker.as_ref())
        .and_then(|picker| picker.browse.as_ref())
        .map(|browse| &browse.root)
        != Some(&root);

    if root_changed {
        let fs_host = stoat.fs_host.clone();
        let executor = stoat.executor.clone();
        let active_idx = stoat.active_workspace;
        let ws = &mut stoat.workspaces[active_idx];
        if let Some(picker) = stoat
            .command_palette
            .as_mut()
            .and_then(|palette| palette.arg_picker.as_mut())
        {
            let children = list_child_dirs(&*fs_host, &root);
            match &mut picker.browse {
                Some(browse) => {
                    browse.root = root.clone();
                    browse.picker.git_root = root.clone();
                    browse.picker.stop_walk();
                    browse.picker.all_paths = children;
                    browse.picker.invalidate();
                },
                None => {
                    let mut child_picker = PathPicker::new(ws, executor, root.clone(), None);
                    child_picker.all_paths = children;
                    picker.browse = Some(Browse {
                        typed_dir: String::new(),
                        root: root.clone(),
                        partial: String::new(),
                        picker: child_picker,
                    });
                },
            }
        }
    }

    if let Some(browse) = stoat
        .command_palette
        .as_mut()
        .and_then(|palette| palette.arg_picker.as_mut())
        .and_then(|picker| picker.browse.as_mut())
    {
        browse.typed_dir = typed_dir;
        browse.partial = partial;
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
        picker.page(dir);
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

/// Complete the highlighted directory into the `:cd` input with a trailing `/`,
/// so the next frame's browse re-root descends into it.
///
/// No-op unless a palette is open on a [`ValueSource::Directories`] argument
/// with a picker and a selected row. In browse mode the completed tail keeps the
/// typed directory prefix and appends the highlighted child. From the workspace
/// list it is the selected directory relative to the workspace root.
pub(super) fn palette_complete_path(stoat: &mut Stoat) -> UpdateEffect {
    let active_idx = stoat.active_workspace;

    let new_text = {
        let Some(palette) = stoat.command_palette.as_ref() else {
            return UpdateEffect::None;
        };
        if palette.arg_source() != Some(ValueSource::Directories) {
            return UpdateEffect::None;
        }
        let Some(picker) = palette.arg_picker.as_ref() else {
            return UpdateEffect::None;
        };
        let ws = &stoat.workspaces[active_idx];

        let tail = match picker.browse.as_ref() {
            Some(browse) => {
                let Some(name) = picker.browse_selected_path().and_then(|p| p.file_name()) else {
                    return UpdateEffect::None;
                };
                format!("{}{}/", browse.typed_dir, name.to_string_lossy())
            },
            None => {
                let Some(selected) = picker.selected_path() else {
                    return UpdateEffect::None;
                };
                format!(
                    "{}/",
                    crate::paths::display_relative(selected, &ws.git_root)
                )
            },
        };

        let text = palette.input.text(ws);
        let Some((head, _)) = text.split_once(' ') else {
            return UpdateEffect::None;
        };
        format!("{head} {tail}")
    };

    let ws = &mut stoat.workspaces[active_idx];
    if let Some(palette) = stoat.command_palette.as_ref() {
        palette.input.replace_text(ws, &new_text);
    }
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

#[cfg(test)]
mod tests {
    use super::list_child_dirs;
    use crate::host::FakeFs;
    use std::path::PathBuf;

    #[test]
    fn list_child_dirs_lists_one_level_sorted_hidden_last() {
        let fs = FakeFs::new();
        let root = PathBuf::from("/root");
        fs.insert_dir(root.join("visible"));
        fs.insert_dir(root.join("zeta"));
        fs.insert_dir(root.join(".hidden"));
        // A file is not a directory, and a nested dir is not an immediate child.
        fs.insert_file(root.join("a-file.txt"), b"y");
        fs.insert_file(root.join("visible/nested/f.rs"), b"x");

        assert_eq!(
            list_child_dirs(&fs, &root),
            [
                root.join("visible"),
                root.join("zeta"),
                root.join(".hidden"),
            ],
        );
    }
}
