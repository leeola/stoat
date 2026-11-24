use crate::{
    app::{Stoat, UpdateEffect},
    command_palette::{
        self, ArgPicker, Availability, CommandPalette, PaletteCommand, PaletteOutcome, PickLoad,
        PickState,
    },
    commit_picker,
    file_finder::{Browse, FileScopes, FinderScope},
    host::FsHost,
    keymap_state::StoatKeymapState,
    picker::{PathPicker, PendingScan, PreviewSource},
    user_command::{self, TextDefault},
    walkthrough,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use stoat_action::ValueSource;
use stoat_scheduler::Task;
use tokio::sync::mpsc::UnboundedReceiver;

/// The most commits a revision argument lists after the branches.
///
/// The list is read on the run loop as the picker opens, where
/// [`branch_candidates`] already reads the repo, so the walk stops here rather
/// than reading the whole history.
const REVISION_ROWS: usize = 100;

/// Candidates feeding a palette argument's inline value-picker, resolved from a
/// [`ValueSource`] by [`arg_candidates`].
pub(super) enum ArgCandidates {
    /// Workspace paths that arrive in batches on `rx`, from a streaming walk or
    /// as one batch read from the finder's cached list. `task` runs the
    /// blocking work, and must be held to keep it alive.
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
/// `Files` lists the file finder's cached path list while it is current for
/// the workspace, and otherwise streams workspace paths via the same
/// background walk the file finder uses. `Directories` derives the workspace's
/// directories from that same list or walk. `Buffers` returns the
/// currently-open buffer paths. `Branches` returns the local branch names with
/// HEAD's first, and none outside a repository. `Revisions` adds the newest
/// commits of HEAD's history after them. `Values` returns the argument's
/// accepted spellings. `None` yields no picker.
///
/// The non-path sources still ride [`ArgCandidates::Paths`]: the picker filters
/// and displays a relative path by its text, so a bare name passes through it
/// unchanged.
pub(super) fn arg_candidates(stoat: &Stoat, source: ValueSource) -> Option<ArgCandidates> {
    match source {
        ValueSource::None => None,
        ValueSource::Files => {
            let git_root = stoat.active_workspace().git_root.clone();
            let (rx, task) = match cached_workspace_paths(stoat, &git_root) {
                Some(paths) => super::file_finder::spawn_cached_paths(stoat, paths),
                None => super::file_finder::spawn_workspace_walk(stoat, git_root),
            };
            Some(ArgCandidates::Walk { rx, task })
        },
        ValueSource::Directories => {
            let git_root = stoat.active_workspace().git_root.clone();
            let (rx, task) = match cached_workspace_paths(stoat, &git_root) {
                Some(paths) => super::file_finder::spawn_cached_dirs(stoat, git_root, paths),
                None => super::file_finder::spawn_workspace_dir_walk(stoat, git_root),
            };
            Some(ArgCandidates::Walk { rx, task })
        },
        ValueSource::Buffers => Some(ArgCandidates::Paths(
            stoat.active_workspace().buffers.open_paths(),
        )),
        ValueSource::Themes => {
            let mut seen = std::collections::HashSet::new();
            let names = stoat
                .theme_pool
                .names()
                .filter(|name| seen.insert(name.to_string()))
                .map(PathBuf::from)
                .collect();
            Some(ArgCandidates::Paths(names))
        },
        ValueSource::Walkthroughs => {
            let ws = stoat.active_workspace();
            let slugs = walkthrough::store::list(&*stoat.fs_host, &ws.git_root)
                .unwrap_or_default()
                .into_iter()
                .map(|summary| PathBuf::from(summary.slug))
                .collect();
            Some(ArgCandidates::Paths(slugs))
        },
        ValueSource::Branches => Some(ArgCandidates::Paths(branch_candidates(
            stoat,
            &stoat.active_workspace().git_root,
        ))),
        ValueSource::Revisions => Some(ArgCandidates::Paths(revision_candidates(
            stoat,
            &stoat.active_workspace().git_root,
        ))),
        ValueSource::Values(values) => Some(ArgCandidates::Paths(
            values.iter().map(PathBuf::from).collect(),
        )),
    }
}

/// The file finder's cached path list, when a walk under `git_root` produced
/// it and no file event has retired it since.
///
/// Code search reads the list under the same test. A copy of the `Arc` is all
/// it costs, and the walk it replaces takes seconds on a large tree.
fn cached_workspace_paths(stoat: &Stoat, git_root: &Path) -> Option<Arc<Vec<PathBuf>>> {
    stoat
        .finder_path_cache
        .as_ref()
        .filter(|cache| cache.root == git_root && cache.epoch == stoat.finder_path_epoch)
        .map(|cache| Arc::clone(&cache.paths))
}

/// The local branch names of the repository holding `git_root`, sorted, with
/// the branch HEAD is attached to moved to the front. Empty outside a
/// repository.
fn branch_candidates(stoat: &Stoat, git_root: &Path) -> Vec<PathBuf> {
    let Some(repo) = stoat.git_host.discover(git_root) else {
        return Vec::new();
    };
    let mut names: Vec<String> = repo
        .local_branches()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    names.sort_unstable();
    if let Some(at) = repo
        .head_branch()
        .and_then(|head| names.iter().position(|name| *name == head))
    {
        names[..=at].rotate_right(1);
    }
    names.into_iter().map(PathBuf::from).collect()
}

/// The rows of [`branch_candidates`], then the newest commits of HEAD's
/// first-parent history as `<short sha> <summary>` rows.
///
/// Only the branches when HEAD names no commit, and empty outside a
/// repository.
fn revision_candidates(stoat: &Stoat, git_root: &Path) -> Vec<PathBuf> {
    let mut rows = branch_candidates(stoat, git_root);
    let Some(repo) = stoat.git_host.discover(git_root) else {
        return rows;
    };

    if let Some(head) = repo.resolve_rev("HEAD") {
        rows.extend(
            repo.log_from(&head, REVISION_ROWS)
                .into_iter()
                .map(|commit| PathBuf::from(format!("{} {}", commit.short_sha, commit.summary))),
        );
    }
    rows
}

/// Open the command palette with `seed` already typed.
///
/// A seed that names a command and ends in a space opens the palette already
/// collecting that command's argument, so a bare action lands the reader in
/// inline entry. The seed is applied and the picker synced in the same tick, so
/// the palette shows that mode on the first frame rather than after the next
/// keystroke.
pub(super) fn open_palette_seeded(stoat: &mut Stoat, seed: &str) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let availability = Availability::from_stoat(stoat);
    let commands = stoat.commands.clone();
    let keys = stoat
        .keymap
        .keys_by_action(&StoatKeymapState::from_stoat(stoat));
    {
        let ws = stoat.active_workspace_mut();
        stoat.command_palette = Some(CommandPalette::new(
            ws,
            executor,
            availability,
            keys,
            commands,
        ));
    }

    let active_idx = stoat.active_workspace;
    {
        let ws = &mut stoat.workspaces[active_idx];
        if let Some(palette) = stoat.command_palette.as_ref() {
            palette.input.replace_text(ws, seed);
        }
    }
    sync_palette_picker(stoat);

    UpdateEffect::Redraw
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
/// While the history walks, the palette shows History and no argument list.
/// The sync then drops an installed argument picker and starts no scan. Thus
/// the box keeps the size of the command list for every recalled line.
///
/// The sync also sets the ghost of a user command's empty argument, as
/// [`sync_arg_ghost`] reads it.
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
        // Typing after a recall ends the walk, so the next Up captures the
        // edited text as a fresh needle.
        let current = palette.input.text(ws);
        ws.palette_history.reset_if_edited(&current);
        palette.refilter_from_input(ws);
        if ws.palette_history.is_walking() {
            palette.dispose_arg_picker(ws);
            return;
        }
        palette.arg_source().zip(palette.arg_tail(ws))
    };
    sync_arg_ghost(stoat);
    sync_pick(stoat);
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
        let scopes = (source == ValueSource::Files).then(|| {
            FileScopes::new(
                FinderScope::All,
                super::file_finder::spawn_modified_query(stoat, git_root.clone()),
                super::file_finder::diff_scope_paths(stoat),
                Vec::new(),
                Vec::new(),
                &stoat.settings.finder_scopes,
            )
        });
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
                    scopes,
                ),
                Some(ArgCandidates::Paths(paths)) => {
                    palette.install_arg_picker(ws, executor, source, git_root, None, paths, scopes)
                },
                None => {},
            }
        }
    }

    if source == ValueSource::Directories {
        sync_arg_picker_browse(stoat, &tail);
    }

    let fs_host = stoat.fs_host.clone();
    let pending = {
        let ws = &mut stoat.workspaces[active_idx];
        match stoat.command_palette.as_mut() {
            Some(palette) => {
                palette.sync_arg_picker(&tail, ws, &*fs_host, &stoat.language_registry)
            },
            None => Vec::new(),
        }
    };

    // The fallback's first paths arrive on later frames, which its walk wakes,
    // so no scan of it starts in this one.
    let fallback_root = stoat
        .command_palette
        .as_ref()
        .and_then(|palette| palette.arg_picker.as_ref())
        .and_then(|picker| picker.wants_ignored_fallback(&tail));
    if let Some(root) = fallback_root {
        let walk = super::file_finder::spawn_all_files_walk(stoat, root.clone());
        let executor = stoat.executor.clone();
        let fallback = PathPicker::new(
            &mut stoat.workspaces[active_idx],
            executor,
            root,
            Some(walk),
        );
        if let Some(picker) = stoat
            .command_palette
            .as_mut()
            .and_then(|palette| palette.arg_picker.as_mut())
        {
            picker.ignored = Some(fallback);
        }
    }

    spawn_arg_picker_scans(stoat, pending);
}

/// Start, narrow, or drop the palette's pick list for the argument in entry.
///
/// The list command of a user command's `pick` argument runs on a worker when
/// that argument's entry starts. It runs once while the same argument stays in
/// entry, and each frame narrows the list by the argument's text. Any other
/// input drops the list and disposes its preview pane.
///
/// The preview of the selected row follows, as [`sync_pick_preview`] shows it.
fn sync_pick(stoat: &mut Stoat) {
    let active = stoat.active_workspace;
    let wanted = stoat
        .command_palette
        .as_ref()
        .and_then(|palette| palette.pick_in_entry(&stoat.workspaces[active]));
    let Some((name, index, spec, typed)) = wanted else {
        if let Some(dropped) = stoat
            .command_palette
            .as_mut()
            .and_then(|palette| palette.pick.take())
        {
            dropped.preview.dispose(&mut stoat.workspaces[active]);
        }
        return;
    };

    let key = (name, index);
    let held = stoat
        .command_palette
        .as_ref()
        .and_then(|palette| palette.pick.as_ref())
        .is_some_and(|pick| pick.key == key);
    if !held {
        let generation = crate::picker::next_generation();
        user_command::run::spawn_shell_job(stoat, spec.list.clone(), move |result| {
            user_command::run::CommandEvent::PickList { generation, result }
        });
        let executor = stoat.executor.clone();
        let ws = &mut stoat.workspaces[active];
        if let Some(palette) = stoat.command_palette.as_mut() {
            let started = PickState::new(ws, executor, key, spec, generation);
            if let Some(replaced) = palette.pick.replace(started) {
                replaced.preview.dispose(ws);
            }
        }
    }
    if let Some(pick) = stoat
        .command_palette
        .as_mut()
        .and_then(|palette| palette.pick.as_mut())
    {
        pick.list.refilter(&typed);
    }
    sync_pick_preview(stoat);
}

/// Show the preview of the pick list's selected row, or run it.
///
/// A list with rows and a preview template runs the preview line of its
/// selected row on a worker. Each line runs once while the list lives, and a
/// line whose text has landed shows it. The pane stays blank while the run is out,
/// while the list loads or after it failed, and while no row is selected.
///
/// [`Stoat::drive_pumps`] calls this after the pumps too, because a frame's
/// palette sync runs before them. A list that lands then starts its first
/// preview, and a preview that lands shows, with no later frame.
pub(crate) fn sync_pick_preview(stoat: &mut Stoat) {
    let active = stoat.active_workspace;
    let Some(pick) = stoat
        .command_palette
        .as_mut()
        .and_then(|palette| palette.pick.as_mut())
        .filter(|pick| pick.spec.preview.is_some())
    else {
        return;
    };
    let ws = &mut stoat.workspaces[active];
    let selected = match pick.load {
        PickLoad::Ready => pick.selected_preview_line().map(str::to_owned),
        PickLoad::Loading | PickLoad::Failed(_) => None,
    };
    let Some(line) = selected else {
        pick.preview.clear(ws);
        return;
    };

    let text = match pick.previews.get(&line) {
        Some(Some(text)) => text.clone(),
        Some(None) => {
            pick.preview.clear(ws);
            return;
        },
        None => {
            pick.previews.insert(line.clone(), None);
            pick.preview.clear(ws);
            let generation = pick.generation;
            user_command::run::spawn_shell_job(stoat, line.clone(), move |result| {
                user_command::run::CommandEvent::PickPreview {
                    generation,
                    line,
                    result,
                }
            });
            return;
        },
    };
    let source = PreviewSource::Text(text);
    pick.preview
        .sync(ws, &*stoat.fs_host, &stoat.language_registry, source);
}

/// Run the list command of the palette's pick list again, as `Ctrl-r` asks.
///
/// The rows on show stay until the new ones land, and the Cmd Line reads
/// `loading` meanwhile. The preview pane stays blank until then, and the
/// preview texts clear, so each preview line runs again. With no pick list on
/// show, nothing changes.
pub(super) fn palette_refresh(stoat: &mut Stoat) -> UpdateEffect {
    let Some(pick) = stoat
        .command_palette
        .as_mut()
        .and_then(|palette| palette.pick.as_mut())
    else {
        return UpdateEffect::None;
    };
    let generation = crate::picker::next_generation();
    pick.generation = generation;
    pick.load = PickLoad::Loading;
    pick.previews.clear();
    let list = pick.spec.list.clone();
    user_command::run::spawn_shell_job(stoat, list, move |result| {
        user_command::run::CommandEvent::PickList { generation, result }
    });
    UpdateEffect::Redraw
}

/// Set the palette's argument ghost to the default of the user command's
/// argument in entry while that argument holds no text.
///
/// A default resolves once for each command and argument while the palette
/// is open, so an idle frame reads no repository.
fn sync_arg_ghost(stoat: &mut Stoat) {
    let wanted = {
        let ws = &stoat.workspaces[stoat.active_workspace];
        stoat
            .command_palette
            .as_ref()
            .and_then(|palette| palette.empty_default(ws))
    };
    let ghost = wanted.map(|(name, index, default)| shown_default(stoat, name, index, &default));
    if let Some(palette) = stoat.command_palette.as_mut() {
        palette.arg_ghost = ghost;
    }
}

/// The text of `default`, the default of argument `index` of the command
/// `name`, from the palette's cache, or resolved and cached on a first read.
fn shown_default(stoat: &mut Stoat, name: String, index: usize, default: &TextDefault) -> String {
    let held = stoat.command_palette.as_ref().and_then(|palette| {
        palette
            .resolved_defaults
            .iter()
            .find(|(command, at, _)| *command == name && *at == index)
            .map(|(_, _, text)| text.clone())
    });
    if let Some(text) = held {
        return text;
    }

    let text = user_command::context::default_text(stoat, default);
    if let Some(palette) = stoat.command_palette.as_mut() {
        palette.resolved_defaults.push((name, index, text.clone()));
    }
    text
}

/// Bring the command list up to date with what is typed, before an action
/// reads the selection rather than paints it.
///
/// The list refilters once a frame. A macro replay or a burst of keys reaches
/// the action with no frame between the keys, so the selection still names a
/// row for an older query.
fn settle_command_list(stoat: &mut Stoat) {
    let active_idx = stoat.active_workspace;
    let ws = &stoat.workspaces[active_idx];
    let Some(palette) = stoat.command_palette.as_mut() else {
        return;
    };
    palette.refilter_from_input(ws);
}

/// Bring the arg picker's rows up to date with what is typed, before an action
/// reads the selection rather than paints it.
///
/// A scan running elsewhere answers the tail that started it. Completing or
/// submitting between keystrokes would otherwise act on the row the previous
/// tail had selected, which is the wrong value.
fn settle_arg_picker_scan(stoat: &mut Stoat) {
    let active_idx = stoat.active_workspace;
    let ws = &stoat.workspaces[active_idx];
    let Some(palette) = stoat.command_palette.as_mut() else {
        return;
    };
    let Some(tail) = palette.arg_tail(ws) else {
        return;
    };
    palette.settle_arg_picker(&tail);
}

/// Run each of `pending` on a worker and report it back to the picker that
/// asked.
///
/// The candidate list is a whole walk, which is too much to rank inside the
/// update path without input and paint waiting on it. The rows on display stay
/// as they are until the result lands.
///
/// A list and its fallback of ignored files rank on the same keystroke, so
/// each scan names the picker whose sink takes its result.
fn spawn_arg_picker_scans(stoat: &mut Stoat, pending: Vec<PendingScan>) {
    let redraw = stoat.redraw_notify.clone();
    let executor = stoat.executor.clone();
    let Some(picker) = stoat
        .command_palette
        .as_mut()
        .and_then(|palette| palette.arg_picker.as_mut())
    else {
        return;
    };
    for PendingScan {
        target,
        generation,
        scan,
    } in pending
    {
        picker
            .scan_target_mut(target)
            .spawn_scan(&executor, redraw.clone(), generation, scan);
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
                        ignored: None,
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
    settle_command_list(stoat);
    settle_arg_picker_scan(stoat);
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
///
/// Under the `action` token the palette steps back to the root list instead.
/// The input clears, so the root lists every command as it does at open, and
/// a second `Esc` closes the palette.
///
/// While the history walks, `Esc` ends the walk and writes the typed text
/// back, and the palette stays open on the list for that text.
pub(super) fn palette_cancel(stoat: &mut Stoat) -> Option<UpdateEffect> {
    let active_idx = stoat.active_workspace;
    let palette = stoat.command_palette.as_mut()?;
    let ws = &mut stoat.workspaces[active_idx];
    if let Some(typed) = ws.palette_history.leave() {
        palette.input.replace_text(ws, &typed);
        sync_palette_picker(stoat);
        return Some(UpdateEffect::Redraw);
    }
    if command_palette::action_query(&palette.input.text(ws)).is_none() {
        return close_palette(stoat).then_some(UpdateEffect::Redraw);
    }

    palette.input.replace_text(ws, "");
    sync_palette_picker(stoat);
    Some(UpdateEffect::Redraw)
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
///
/// A move to another entry starts the doc island at the new entry's first
/// line. While the history walks, the palette lists History, so a negative
/// `delta` recalls the next older match and a positive one the next newer.
pub(crate) fn palette_move_selection(stoat: &mut Stoat, delta: i32) -> Option<UpdateEffect> {
    stoat.command_palette.as_ref()?;
    if stoat.active_workspace().palette_history.is_walking() {
        return match delta < 0 {
            true => palette_history_prev(stoat),
            false => palette_history_next(stoat),
        };
    }

    let palette = stoat.command_palette.as_mut()?;
    if let Some(pick) = palette.pick.as_mut() {
        pick.list.move_selection(delta);
        return Some(UpdateEffect::Redraw);
    }
    if palette.arg_source().is_some()
        && let Some(picker) = palette.arg_picker.as_mut()
    {
        picker.move_selection(delta);
        return Some(UpdateEffect::Redraw);
    }
    let before = palette.selected;
    crate::picker::nav_move(palette.filtered.len(), &mut palette.selected, delta);
    if palette.selected != before {
        palette.doc_scroll = 0;
    }
    Some(UpdateEffect::Redraw)
}

/// Move the palette's selection up one row, or start the history walk from
/// the list's first row.
///
/// On the first row of the command list or of the argument list, Up starts the
/// walk as [`palette_history_prev`] starts it. A free-typed argument lists
/// nothing, so Up over it starts the walk too. The typed text is the needle,
/// and the newest match goes into the input. With no match the palette stays
/// as it is. While the history walks, Up recalls the next older match. On a
/// lower row Up moves the selection up.
pub(super) fn palette_up(stoat: &mut Stoat) -> UpdateEffect {
    let Some(palette) = stoat.command_palette.as_ref() else {
        return UpdateEffect::None;
    };
    let picker_row = match &palette.pick {
        Some(pick) => Some(pick.list.selected),
        None => palette
            .arg_picker
            .as_ref()
            .filter(|_| palette.arg_source().is_some())
            .map(|picker| picker.active_core_ref().picklist.selected),
    };
    let first_row = match (palette.command.as_ref(), picker_row) {
        (_, Some(row)) => row == 0,
        (Some(_), None) => true,
        (None, None) => palette.selected == 0,
    };

    let walking = stoat.active_workspace().palette_history.is_walking();
    let effect = match walking || first_row {
        true => palette_history_prev(stoat),
        false => palette_move_selection(stoat, -1),
    };
    effect.unwrap_or(UpdateEffect::None)
}

/// Scroll the palette's doc island by [`command_palette::DOC_SCROLL_STEP`]
/// lines in `dir` (-1 up, 1 down), or answer `None` when the palette shows no
/// doc island.
///
/// The command list and the `action` list show the selected entry's doc. An
/// argument stage shows none, so `C-d` and `C-u` keep to its preview there.
/// The paint holds a scroll past the doc's end at its last line. History
/// shows no doc, so the keys change nothing while the history walks.
pub(super) fn palette_doc_scroll(stoat: &mut Stoat, dir: i32) -> Option<UpdateEffect> {
    stoat.command_palette.as_ref()?;
    if stoat.active_workspace().palette_history.is_walking() {
        return Some(UpdateEffect::None);
    }
    let palette = stoat
        .command_palette
        .as_mut()
        .filter(|palette| palette.command.is_none())?;
    palette.doc_scroll = match dir > 0 {
        true => palette
            .doc_scroll
            .saturating_add(command_palette::DOC_SCROLL_STEP),
        false => palette
            .doc_scroll
            .saturating_sub(command_palette::DOC_SCROLL_STEP),
    };
    Some(UpdateEffect::Redraw)
}

/// Recall the previous palette-history command into the input, fish-style.
/// Returns `None` when the palette is closed.
///
/// The already-typed text is the needle, and a line matches when it holds each
/// of its words. A saturated or empty walk leaves the input unchanged. The
/// every-frame refilter re-parses the recalled line into command or argument
/// mode, so no extra wiring is needed here. An edit of the recalled line ends
/// the walk first, so the edited text is the needle of a new one.
pub(crate) fn palette_history_prev(stoat: &mut Stoat) -> Option<UpdateEffect> {
    let active_idx = stoat.active_workspace;
    let palette = stoat.command_palette.as_mut()?;
    let ws = &mut stoat.workspaces[active_idx];
    let current = palette.input.text(ws);
    ws.palette_history.reset_if_edited(&current);
    if let Some(recalled) = ws.palette_history.prev(&current) {
        palette.input.replace_text(ws, &recalled);
    }
    Some(UpdateEffect::Redraw)
}

/// Recall the next palette-history command toward the newest, restoring the
/// originally-typed text past the newest match. Returns `None` when the palette
/// is closed.
pub(crate) fn palette_history_next(stoat: &mut Stoat) -> Option<UpdateEffect> {
    let active_idx = stoat.active_workspace;
    let palette = stoat.command_palette.as_mut()?;
    let ws = &mut stoat.workspaces[active_idx];
    let current = palette.input.text(ws);
    ws.palette_history.reset_if_edited(&current);
    if let Some(recalled) = ws.palette_history.next(&current) {
        palette.input.replace_text(ws, &recalled);
    }
    Some(UpdateEffect::Redraw)
}

/// Page the action-list selection by half its rendered list height in `dir`
/// (-1 up, 1 down). Before the first render the viewport is unset and the step
/// falls back to a single row. Delegates to [`palette_move_selection`]. History
/// pages nothing, so the page keys change nothing while the history walks.
pub(super) fn palette_page(stoat: &mut Stoat, dir: i32) -> UpdateEffect {
    if stoat.active_workspace().palette_history.is_walking() {
        return UpdateEffect::None;
    }
    if let Some(pick) = stoat
        .command_palette
        .as_mut()
        .and_then(|palette| palette.pick.as_mut())
    {
        pick.list.page(dir);
        return UpdateEffect::Redraw;
    }
    if let Some(palette) = stoat.command_palette.as_mut()
        && palette.arg_source().is_some()
        && let Some(picker) = palette.arg_picker.as_mut()
    {
        picker.page(dir);
        return UpdateEffect::Redraw;
    }
    let step = match stoat.command_palette.as_ref() {
        Some(palette) => crate::picker::nav_page_step(palette.viewport_rows),
        None => return UpdateEffect::None,
    };
    palette_move_selection(stoat, dir * step).unwrap_or(UpdateEffect::None)
}

/// Flip the palette's [`crate::command_palette::PaletteScope`] and re-filter.
/// No-op when the palette is closed, and while the history walks, because
/// History holds no scope.
///
/// In the list of a file argument the key steps the file scope instead, as
/// [`arg_scope_toggle`] steps it.
pub(super) fn palette_scope_toggle(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().palette_history.is_walking() {
        return UpdateEffect::None;
    }
    let file_argument = stoat.command_palette.as_ref().is_some_and(|palette| {
        palette.arg_source() == Some(ValueSource::Files)
            && palette
                .arg_picker
                .as_ref()
                .is_some_and(|picker| picker.scopes.is_some())
    });
    if file_argument {
        return arg_scope_toggle(stoat);
    }
    let active_idx = stoat.active_workspace;
    let workspaces = &mut stoat.workspaces;
    let Some(palette) = stoat.command_palette.as_mut() else {
        return UpdateEffect::None;
    };
    palette.toggle_scope(&workspaces[active_idx]);
    UpdateEffect::Redraw
}

/// Step the file scope of the palette's file argument, as the File Finder's
/// Shift-Tab steps its scope.
///
/// A landing on [`FinderScope::Modified`] asks git again. An entry to
/// [`FinderScope::AllWorkspaces`] walks every known workspace root and shows
/// each row under its root. A leave walks the workspace again, and reads the
/// cached path list while that list is current.
fn arg_scope_toggle(stoat: &mut Stoat) -> UpdateEffect {
    let Some((left, landed, git_root)) = arg_picker_mut(stoat).and_then(|picker| {
        let scopes = picker.scopes.as_mut()?;
        let left = scopes.scope.clone();
        scopes.toggle(&mut picker.core);
        Some((left, scopes.scope.clone(), picker.core.git_root.clone()))
    }) else {
        return UpdateEffect::None;
    };

    if landed == FinderScope::Modified {
        let (rx, task) = super::file_finder::spawn_modified_query(stoat, git_root);
        if let Some(scopes) = arg_picker_mut(stoat).and_then(|picker| picker.scopes.as_mut()) {
            scopes.set_modified_source(rx, task);
        }
    }

    let walks_every_root = landed == FinderScope::AllWorkspaces;
    if walks_every_root != (left == FinderScope::AllWorkspaces) {
        let (walk, roots) = match walks_every_root {
            true => {
                let roots = super::file_finder::collect_workspace_roots(stoat);
                let walk = super::file_finder::spawn_multi_workspace_walk(stoat, roots.clone());
                (Some(walk), Some(roots))
            },
            false => match arg_candidates(stoat, ValueSource::Files) {
                Some(ArgCandidates::Walk { rx, task }) => (Some((rx, task)), None),
                _ => (None, None),
            },
        };
        if let (Some((rx, task)), Some(picker)) = (walk, arg_picker_mut(stoat)) {
            picker.core.reset_walk(rx, task);
            picker.core.picklist.display_roots = roots;
        }
    }
    UpdateEffect::Redraw
}

/// The palette's argument picker, while one is installed.
fn arg_picker_mut(stoat: &mut Stoat) -> Option<&mut ArgPicker> {
    stoat.command_palette.as_mut()?.arg_picker.as_mut()
}

/// Complete the highlighted candidate into the palette input, replacing the
/// typed tail with exactly what the selected row shows.
///
/// In command-list mode this completes the highlighted command's name as the
/// palette spells it, which is what the row shows and what matching sees. A
/// command taking parameters completes with a trailing space so the argument
/// picker opens. A parameterless one completes bare, which the exact-match arm
/// of `handle_submit` dispatches on the next Enter.
///
/// In argument mode it completes the selected row of any picker-sourced
/// argument. Pressing it again without editing the tail in between steps to the
/// next candidate the typed text matched, wrapping at the end, so a run of
/// presses walks the whole match list. A command with more than one parameter
/// completes the token being typed and keeps the tokens before it as typed. A
/// user command completes its argument in entry and keeps the text before it,
/// as [`command_palette::current_user_arg`] splits them. A pick list writes
/// the insert of its selected row in place of the argument's text.
///
/// A browse-mode tail keeps the typed directory prefix and appends the
/// highlighted child, and no completion appends a trailing `/`, so the browse
/// stays rooted where it was with the completed name highlighted and a following
/// Enter opens that entry. Descending is a further `/` the user types, which
/// re-roots the browse. A browse tail does not cycle, since a completed
/// directory name is the prefix the next press builds on.
///
/// No-op when no palette is open, or when the list it would complete from is
/// empty. The picker is refiltered synchronously against the completed tail and
/// its selection reset to the top row, so an Enter arriving before the next
/// render still reads the completed entry rather than its first child. History
/// lists no candidate, so the key leaves the recalled line alone while the
/// history walks.
pub(super) fn palette_complete(stoat: &mut Stoat) -> UpdateEffect {
    if stoat.active_workspace().palette_history.is_walking() {
        return UpdateEffect::None;
    }
    let active_idx = stoat.active_workspace;
    settle_command_list(stoat);

    // The argument arm below reads the selected row, and its cycle walks the
    // filtered ones, so the list has to answer what is typed rather than
    // whatever a scan still out was started for. A no-op in command-list mode,
    // which has no argument tail.
    settle_arg_picker_scan(stoat);

    let new_text = {
        let Some(palette) = stoat.command_palette.as_ref() else {
            return UpdateEffect::None;
        };
        if palette.command.is_none() {
            let Some(command) = palette.filtered.get(palette.selected) else {
                return UpdateEffect::None;
            };
            let token = command_palette::typed_head(command);
            let completed = if command.arg_count() == 0 {
                token
            } else {
                format!("{token} ")
            };

            let ws = &mut stoat.workspaces[active_idx];
            palette.input.replace_text(ws, &completed);
            sync_palette_picker(stoat);
            return UpdateEffect::Redraw;
        }
        let ws = &stoat.workspaces[active_idx];
        let text = palette.input.text(ws);
        let Some((command, head, typed_tail)) =
            command_palette::split_head(&text, &palette.availability, &palette.commands)
        else {
            return UpdateEffect::None;
        };
        let (kept, typed_tail) = match command {
            PaletteCommand::Action(entry) if entry.def.params().len() > 1 => {
                command_palette::split_last_token(typed_tail)
            },
            PaletteCommand::User(user) => {
                command_palette::current_user_arg(typed_tail, user.args.len())
            },
            PaletteCommand::Action(_) => ("", typed_tail),
        };

        let pick = stoat
            .command_palette
            .as_mut()
            .and_then(|palette| palette.pick.as_mut());
        let tail = if let Some(pick) = pick {
            pick.list.refilter(typed_tail.trim());
            let Some(row) = pick.list.selected_row() else {
                return UpdateEffect::None;
            };
            row.insert.clone()
        } else {
            let git_root = &ws.git_root;
            let Some(picker) = stoat
                .command_palette
                .as_mut()
                .and_then(|palette| palette.arg_picker.as_mut())
            else {
                return UpdateEffect::None;
            };

            match picker.browse.as_ref() {
                Some(browse) => {
                    let Some(name) = picker.browse_selected_path().and_then(|p| p.file_name())
                    else {
                        return UpdateEffect::None;
                    };
                    format!("{}{}", browse.typed_dir, name.to_string_lossy())
                },
                None => {
                    let Some(cycled) = picker.advance_tab_cycle(typed_tail, git_root) else {
                        return UpdateEffect::None;
                    };
                    cycled
                },
            }
        };

        format!("{head} {kept}{tail}")
    };

    {
        let ws = &mut stoat.workspaces[active_idx];
        if let Some(palette) = stoat.command_palette.as_ref() {
            palette.input.replace_text(ws, &new_text);
        }
    }

    // Refilter against the completed tail now so the browse re-sync and selection
    // are settled before the next Enter reads them, not one render frame later.
    sync_palette_picker(stoat);

    if let Some(palette) = stoat.command_palette.as_mut()
        && let Some(picker) = palette.arg_picker.as_mut()
    {
        picker.active_core().picklist.selected = 0;
    }
    UpdateEffect::Redraw
}

fn apply_outcome(stoat: &mut Stoat, outcome: PaletteOutcome) -> UpdateEffect {
    match outcome {
        PaletteOutcome::None => UpdateEffect::Redraw,
        PaletteOutcome::Dispatch(entry, params, line) => {
            tracing::info!(
                target: "stoat::command",
                line = %line,
                action = entry.def.name(),
                "command"
            );
            let ran_at = commit_picker::now_epoch(&stoat.executor);
            stoat
                .active_workspace_mut()
                .palette_history
                .push_at(line, Some(ran_at));
            close_palette(stoat);
            match (entry.create)(&params) {
                Ok(action) => super::dispatch(stoat, &*action),
                Err(e) => {
                    tracing::warn!(
                        target: "stoat::command",
                        "palette dispatch `{}`: {e}",
                        entry.def.name()
                    );
                    UpdateEffect::Redraw
                },
            }
        },
        PaletteOutcome::Run(command, values, line) => {
            tracing::info!(
                target: "stoat::command",
                line = %line,
                command = %command.name,
                "user command"
            );
            let ran_at = commit_picker::now_epoch(&stoat.executor);
            stoat
                .active_workspace_mut()
                .palette_history
                .push_at(line, Some(ran_at));
            close_palette(stoat);
            user_command::run::start(stoat, &command, values)
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
        workspaces[active_idx].palette_history.reset();
    }
    true
}

#[cfg(test)]
mod tests {
    use super::{list_child_dirs, revision_candidates};
    use crate::{app::Stoat, host::FakeFs};
    use std::path::{Path, PathBuf};

    #[test]
    fn revision_candidates_list_branches_then_head_history() {
        let h = Stoat::test();
        h.fake_git()
            .add_repo("/repo")
            .commit("c1", &[])
            .commit_with_parent_message("c2", "c1", "chore: tweak a", &[])
            .branch("main", "c2")
            .branch("feature", "c1")
            .set_head_branch("main");
        h.fake_git().add_repo("/bare").branch("main", "c9");

        assert_eq!(
            revision_candidates(&h.stoat, Path::new("/repo")),
            ["main", "feature", "c2 chore: tweak a", "c1 commit c1"].map(PathBuf::from),
        );
        assert_eq!(
            revision_candidates(&h.stoat, Path::new("/bare")),
            [PathBuf::from("main")],
            "a HEAD that names no commit leaves the branches alone",
        );
    }

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
