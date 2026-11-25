use super::run;
use crate::{
    app::{Stoat, UpdateEffect},
    cli_spec::CliSpec,
    command_palette::{
        self, ArgList, ArgPicker, ArgRow, Availability, CliListKind, CommandPalette, PaletteBlock,
        PaletteCommand, PaletteOutcome, PickLoad, PickState,
    },
    commit_picker,
    file_finder::{Browse, FileScopes, FinderScope},
    host::FsHost,
    keymap_state::StoatKeymapState,
    picker::{PathPicker, PendingScan, PreviewSource},
    render,
    run::CmdId,
    user_command::{self, run::CliSpecState, RunSpec, TextDefault},
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
/// The sync then drops an installed argument picker and the list of a `cli`
/// command, and starts no scan. Thus the box keeps the size of the command
/// list for every recalled line.
///
/// The sync also sets the ghost of a user command's empty argument, as
/// [`sync_arg_ghost`] reads it, starts the load of a wrapped tool's help, as
/// [`sync_cli_spec`] does, and lists that tool's flags or subcommands, as
/// [`sync_cli_list`] does.
///
/// Lives here rather than on [`crate::command_palette::CommandPalette`] because
/// spawning the walk needs [`Stoat`]-level resources (the executor, fs host, and
/// redraw notifier) that a palette method does not see.
pub(crate) fn sync_palette_picker(stoat: &mut Stoat) {
    if stoat
        .command_palette
        .as_ref()
        .is_none_or(|palette| palette.block.is_some())
    {
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
            palette.cli_list = None;
            return;
        }
        palette.arg_source().zip(palette.arg_tail(ws))
    };
    sync_arg_ghost(stoat);
    sync_pick(stoat);
    sync_cli_spec(stoat);
    sync_cli_list(stoat);
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

/// Start the load of the help of the `cli` command whose argument entry the
/// palette's input opens, the first time the input opens it.
///
/// The load runs once for each command while the config stays, as
/// [`user_command::run::load_cli`] reads it, and [`Stoat::cli_specs`] marks
/// it `Loading` until [`user_command::run::pump`] stores its help.
fn sync_cli_spec(stoat: &mut Stoat) {
    let wanted = match stoat
        .command_palette
        .as_ref()
        .and_then(|palette| palette.command.as_ref())
    {
        Some(PaletteCommand::User(command)) => match &command.run {
            RunSpec::Cli { tool, completions } => {
                Some((command.name.clone(), tool.clone(), completions.clone()))
            },
            RunSpec::Steps { .. } => None,
        },
        _ => None,
    };
    let Some((name, tool, completions)) =
        wanted.filter(|(name, ..)| !stoat.cli_specs.contains_key(name))
    else {
        return;
    };
    stoat.cli_specs.insert(name.clone(), CliSpecState::Loading);
    user_command::run::spawn_cli_load(stoat, name, tool, completions);
}

/// Build, narrow, or drop the list of the flags or the subcommands of the
/// `cli` command whose argument entry the palette's input opens.
///
/// The list shows once the command's help lands in [`Stoat::cli_specs`], for
/// the kind that [`command_palette::cli_list_kind`] picks from the word in
/// entry. Its rows build again when the command or the kind changes, and each
/// frame narrows them by the word in entry. Any other input drops the list.
fn sync_cli_list(stoat: &mut Stoat) {
    let active = stoat.active_workspace;
    let wanted = stoat.command_palette.as_ref().and_then(|palette| {
        let PaletteCommand::User(command) = palette.command.as_ref()? else {
            return None;
        };
        let Some(CliSpecState::Ready(loaded)) = stoat.cli_specs.get(&command.name) else {
            return None;
        };
        let text = palette.input.text(&stoat.workspaces[active]);
        let (_, _, tail) =
            command_palette::split_head(&text, &palette.availability, &palette.commands)?;
        let kind = command_palette::cli_list_kind(&loaded.spec, tail)?;
        let word = command_palette::split_last_token(tail).1.to_owned();
        Some((command.name.clone(), kind, loaded.clone(), word))
    });
    let Some(palette) = stoat.command_palette.as_mut() else {
        return;
    };
    let Some((name, kind, loaded, word)) = wanted else {
        palette.cli_list = None;
        return;
    };

    let held = palette
        .cli_list
        .as_ref()
        .is_some_and(|(held, held_kind, _)| *held == name && *held_kind == kind);
    if !held {
        let mut list = ArgList::default();
        list.set_rows(cli_rows(&loaded.spec, kind));
        palette.cli_list = Some((name, kind, list));
    }
    if let Some((_, _, list)) = palette.cli_list.as_mut() {
        list.refilter(&word);
    }
}

/// The rows of the `kind` list of `spec`.
///
/// A flag row shows the flag's label, its value name, and its description,
/// and writes its insert. A subcommand row shows and writes its name.
fn cli_rows(spec: &CliSpec, kind: CliListKind) -> Vec<ArgRow> {
    match kind {
        CliListKind::Flags => spec
            .flags
            .iter()
            .map(|flag| ArgRow {
                label: flag.label(),
                insert: flag.insert(),
                value_name: flag.value.clone(),
                description: flag.description.clone(),
            })
            .collect(),
        CliListKind::Subcommands => spec
            .subcommands
            .iter()
            .map(|subcommand| ArgRow {
                label: subcommand.name.clone(),
                insert: subcommand.name.clone(),
                value_name: None,
                description: subcommand.description.clone(),
            })
            .collect(),
    }
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
///
/// Over a Run Block, Enter acts on its Cmd as [`run::block_enter`] takes it,
/// and an Enter that acts closes the palette, since the stage would stand over
/// what the Enter opened.
pub(super) fn palette_submit(stoat: &mut Stoat) -> Option<UpdateEffect> {
    stoat.command_palette.as_ref()?;
    if let Some(cmd) = stoat.run_block_cmd() {
        let effect = run::block_enter(stoat, cmd);
        if effect == UpdateEffect::Redraw {
            close_palette(stoat);
        }
        return Some(effect);
    }
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
///
/// Over a Run Block, `Esc` closes the palette at once, as [`close_palette`]
/// closes it.
pub(super) fn palette_cancel(stoat: &mut Stoat) -> Option<UpdateEffect> {
    if stoat.command_palette.as_ref()?.block.is_some() {
        return close_palette(stoat).then_some(UpdateEffect::Redraw);
    }
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
    if let Some((_, _, list)) = palette.cli_list.as_mut() {
        list.move_selection(delta);
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
    let picker_row = match (&palette.pick, &palette.cli_list) {
        (Some(pick), _) => Some(pick.list.selected),
        (None, Some((_, _, list))) => Some(list.selected),
        (None, None) => palette
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
    if let Some((_, _, list)) = stoat
        .command_palette
        .as_mut()
        .and_then(|palette| palette.cli_list.as_mut())
    {
        list.page(dir);
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
/// the insert of its selected row in place of the argument's text. The list
/// of a `cli` command writes the insert of its selected row and a space in
/// place of the word in entry.
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
            PaletteCommand::User(user) if matches!(user.run, RunSpec::Cli { .. }) => {
                command_palette::split_last_token(typed_tail)
            },
            PaletteCommand::User(user) => {
                command_palette::current_user_arg(typed_tail, user.args.len())
            },
            PaletteCommand::Action(_) => ("", typed_tail),
        };

        let cli_row = stoat
            .command_palette
            .as_mut()
            .and_then(|palette| palette.cli_list.as_mut())
            .map(|(_, _, list)| {
                list.refilter(typed_tail);
                list.selected_row().map(|row| format!("{} ", row.insert))
            });
        let pick = stoat
            .command_palette
            .as_mut()
            .and_then(|palette| palette.pick.as_mut());
        let tail = if let Some(row) = cli_row {
            let Some(row) = row else {
                return UpdateEffect::None;
            };
            row
        } else if let Some(pick) = pick {
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
                .push_at(line.clone(), Some(ran_at));
            stoat.palette_held = stoat.command_palette.take();
            let effect = user_command::run::start(stoat, &command, values, &line);
            if let Some(palette) = stoat.palette_held.take() {
                dispose_palette(stoat, palette);
            }
            effect
        },
    }
}

/// Close the palette, disposing its [`InputView`] and ending the history walk.
/// Returns `true` if a palette was open, `false` otherwise.
///
/// A Run Block's Cmd outlives the stage only while it runs. A running Cmd
/// moves onto a Run pane's stack, as [`run::append_to_run_pane`] places it,
/// and a finished one leaves the store.
pub(crate) fn close_palette(stoat: &mut Stoat) -> bool {
    let Some(palette) = stoat.command_palette.take() else {
        return false;
    };
    if let Some(block) = &palette.block {
        let running = stoat
            .active_workspace()
            .cmds
            .get(block.cmd)
            .is_some_and(|cmd| !cmd.finished);
        match running {
            true => {
                run::append_to_run_pane(stoat, block.cmd, false);
            },
            false => {
                stoat.active_workspace_mut().cmds.remove(block.cmd);
            },
        }
    }
    dispose_palette(stoat, palette);
    true
}

/// Dispose the editors of `palette`, a palette out of
/// [`Stoat::command_palette`], and end the history walk.
pub(super) fn dispose_palette(stoat: &mut Stoat, palette: CommandPalette) {
    let ws = stoat.active_workspace_mut();
    palette.dispose(ws);
    ws.palette_history.reset();
}

/// The grid width, in cells, of the Cmd that a config command's start shows.
///
/// With a held palette, the Cmd shows in the Run Block, whose plate takes the
/// width that [`render::command_palette::run_block_plate_width`] gives over the
/// plate of the palette's typing stage, and the body's rows stand one cell
/// inside the plate's inner rect. With no held palette, the Cmd joins a Run
/// pane, as [`run::run_pane_width`] sizes it.
pub(crate) fn run_block_width(stoat: &Stoat) -> u16 {
    let Some(palette) = stoat.palette_held.as_ref() else {
        return run::run_pane_width(stoat);
    };
    let area = stoat.size();
    let origin = palette
        .plate
        .unwrap_or_else(|| render::command_palette::run_block_origin(area));
    render::command_palette::run_block_plate_width(origin, area).saturating_sub(4)
}

/// Show the Cmd `id` that a config command's start ran.
///
/// With a held palette, the palette turns into the Cmd's Run Block over the
/// plate of its typing stage. Its argument picker and its pick list go, the
/// history walk ends, and its input stays the modal's input in `normal` mode,
/// so no key types into it. With no held palette, the Cmd joins a Run pane, as
/// [`run::append_to_run_pane`] places it, and the focus stays where it is.
pub(crate) fn open_run_block(stoat: &mut Stoat, id: CmdId) {
    let Some(mut palette) = stoat.palette_held.take() else {
        run::append_to_run_pane(stoat, id, false);
        return;
    };
    let origin = palette
        .plate
        .unwrap_or_else(|| render::command_palette::run_block_origin(stoat.size()));
    let ws = stoat.active_workspace_mut();
    palette.dispose_arg_picker(ws);
    if let Some(pick) = palette.pick.take() {
        pick.preview.dispose(ws);
    }
    ws.palette_history.reset();
    if let Some(editor) = ws.editors.get_mut(palette.input.editor_id) {
        editor.mode = "normal".into();
    }
    palette.block = Some(PaletteBlock {
        cmd: id,
        origin,
        rows: 0,
    });
    stoat.command_palette = Some(palette);
}

#[cfg(test)]
mod tests {
    use super::{list_child_dirs, revision_candidates};
    use crate::{
        action_handlers,
        app::Stoat,
        host::{FakeFs, FakeTerminalSession},
        keymap_state::{self, active_modal, ActiveModal},
        pane::{PaneId, View},
        run::{cargo::ONE_FAILURE, CmdId, PtyNotification},
        test_harness::{editor, TestHarness},
        theme::scope,
        user_command,
    };
    use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };

    /// The user command `hello`, which runs under the default `block` output.
    const HELLO: &str = r#"command hello { run = shell("echo hi"); }"#;

    /// The file that the failure of [`ONE_FAILURE`] names, under the root
    /// `/work`.
    const CHROME: &str = "/work/stoat/src/render/chrome.rs";

    /// A harness with the user config `source` loaded.
    fn loaded(source: &str) -> TestHarness {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(source);
        h
    }

    /// Open the palette and type `line`, with a fresh fake session queued for
    /// the process that a run of it starts, and answer the session.
    fn typed(h: &mut TestHarness, line: &str) -> Arc<FakeTerminalSession> {
        let session = Arc::new(FakeTerminalSession::new());
        h.fake_terminal_host().push_session(session.clone());
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        h.type_text(line);
        session
    }

    /// Run `line` from the palette with Enter, as [`typed`] types it, and
    /// answer the session of its process.
    fn entered(h: &mut TestHarness, line: &str) -> Arc<FakeTerminalSession> {
        let session = typed(h, line);
        h.type_keys("enter");
        session
    }

    /// The Cmd that the palette's Run Block shows.
    fn block_cmd(h: &TestHarness) -> CmdId {
        h.stoat.run_block_cmd().expect("a Run Block")
    }

    /// The plate that the palette's typing stage drew last.
    fn plate(h: &TestHarness) -> Rect {
        h.stoat
            .command_palette
            .as_ref()
            .and_then(|palette| palette.plate)
            .expect("a plate")
    }

    /// Row `y` of `buf` across `plate`, each U+2500 and U+2502 read as a
    /// space, trimmed.
    fn plate_row(buf: &Buffer, plate: Rect, y: u16) -> String {
        let text: String = (plate.x..plate.right())
            .map(|x| buf[(x, y)].symbol())
            .collect();
        text.replace(['\u{2500}', '\u{2502}'], " ")
            .trim()
            .to_owned()
    }

    /// The split panes that show a Run pane, each with its stack.
    fn run_stacks(h: &TestHarness) -> Vec<(PaneId, Vec<CmdId>)> {
        let ws = h.stoat.active_workspace();
        ws.panes
            .split_panes()
            .filter_map(|(pane, shown)| match shown.view {
                View::Run(run) => Some((pane, ws.runs[run].blocks.clone())),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn enter_on_a_block_command_turns_the_palette_into_its_run_block() {
        let mut h = loaded(HELLO);
        entered(&mut h, "hello");

        let cmd = &h.stoat.active_workspace().cmds[block_cmd(&h)];
        let spawn = h
            .fake_terminal_host()
            .spawns()
            .last()
            .map(|spawn| (spawn.program.clone(), spawn.args.clone()));
        assert_eq!(
            (
                (
                    cmd.command.as_str(),
                    cmd.shell.as_deref(),
                    cmd.source.map(|(label, _)| label)
                ),
                active_modal(&h.stoat),
                keymap_state::modal_predicate(&h.stoat),
                h.stoat.focused_mode(),
                spawn,
                h.fake_shell.invocations().len(),
                h.stoat
                    .active_workspace()
                    .palette_history
                    .entries()
                    .last()
                    .map(String::as_str),
            ),
            (
                ("hello", Some("echo hi"), Some("config")),
                Some(ActiveModal::RunBlock),
                Some("run_block"),
                "normal",
                Some((
                    "bash".to_owned(),
                    vec!["-c".to_owned(), "echo hi".to_owned()]
                )),
                0,
                Some("hello"),
            ),
        );
    }

    /// The Run Block's Cmd Line keeps the typed line on the typing stage's
    /// row, and the caret that the typing stage drew goes.
    #[test]
    fn the_run_block_keeps_its_line_and_draws_no_caret() {
        let mut h = loaded(HELLO);
        typed(&mut h, "hello");
        let typing = header(&mut h);
        h.type_keys("enter");
        assert_eq!([typing, header(&mut h)], [(true, true), (true, false)]);
    }

    /// Whether the palette's Cmd Line row reads `hello`, and whether a cell of
    /// it draws a caret.
    ///
    /// A caret draws in the cursor's style in cells, reversed under a theme
    /// with no cursor scope, and on the text color in the plate.
    fn header(h: &mut TestHarness) -> (bool, bool) {
        let buf = h.render_composited();
        let plate = plate(h);
        let y = plate.y + 1;
        let cursor = h.stoat.theme.cursor_style().bg;
        let text = h.stoat.theme.get(scope::UI_TEXT).fg;
        let caret = (plate.x..plate.right()).any(|x| {
            let cell = &buf[(x, y)];
            cell.modifier.contains(Modifier::REVERSED)
                || cursor.is_some_and(|bg| cell.bg == bg)
                || text.is_some_and(|fg| cell.bg == fg)
        });
        (plate_row(&buf, plate, y).contains("hello"), caret)
    }

    /// A body of 40 lines shows its newest 16, under the shell row that opens
    /// the grid. The window leaves room for more rows, so the 16 are the
    /// body's own bound.
    #[test]
    fn the_run_block_body_follows_the_tail() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.reload_user_config(HELLO);
        entered(&mut h, "hello");
        let output: String = (1..=40).map(|n| format!("line {n}\r\n")).collect();
        h.inject_cmd_output(block_cmd(&h), output.as_bytes());

        let buf = h.render_composited();
        let plate = plate(&h);
        let rows: Vec<String> = (plate.y + 3..plate.y + 19)
            .map(|y| plate_row(&buf, plate, y))
            .collect();
        let tail: Vec<String> = (25..=40).map(|n| format!("line {n}")).collect();
        assert_eq!(rows, tail);
    }

    #[test]
    fn the_plate_keeps_its_top_and_its_width_at_enter() {
        let mut h = loaded(HELLO);
        typed(&mut h, "hello");
        let typing = plate_sides(&mut h);
        h.type_keys("enter");
        assert_eq!(plate_sides(&mut h), typing);
    }

    /// The glyph of the plate's top left corner and the columns of its two
    /// sides on the Cmd Line row, under the typing stage's plate.
    fn plate_sides(h: &mut TestHarness) -> (String, u16, u16) {
        let buf = h.render_composited();
        let plate = plate(h);
        let y = plate.y + 1;
        let middle = plate.x + plate.width / 2;
        let side = |x: &u16| buf[(*x, y)].symbol() == "\u{2502}";
        let left = (0..middle).rev().find(side).expect("a left side");
        let right = (middle..buf.area.width).find(side).expect("a right side");
        (buf[(left, plate.y)].symbol().to_owned(), left, right)
    }

    /// The key line names `C-c` and `Esc` while the Cmd runs, and `Enter` and
    /// `Esc` once it finishes, and never the Jobs keys of a later leaf.
    #[test]
    fn the_key_line_names_the_keys_of_a_running_block_and_of_a_finished_one() {
        let mut h = TestHarness::with_size(160, 50);
        h.stoat.reload_user_config(HELLO);
        entered(&mut h, "hello");
        let running = key_line(&mut h);
        h.inject_cmd_done(block_cmd(&h), 0);
        let finished = key_line(&mut h);

        let names = |line: &str, keys: &str| (line.contains(keys), line.contains("Jobs"));
        assert_eq!(
            [
                names(
                    &running,
                    "C-o to Run pane  C-c interrupt  Esc hide (keeps running)"
                ),
                names(
                    &finished,
                    "Enter open output  C-r run again  C-o to Run pane  Esc close"
                ),
            ],
            [(true, false); 2],
        );
    }

    /// The row of the plate that names `Esc`, the key line.
    fn key_line(h: &mut TestHarness) -> String {
        let buf = h.render_composited();
        let plate = plate(h);
        (plate.y..buf.area.height)
            .map(|y| plate_row(&buf, plate, y))
            .find(|row| row.contains("Esc"))
            .expect("a key line")
    }

    /// `Esc` on a running block closes the palette and moves the Cmd onto the
    /// stack of a Run pane that splits the editor pane downward. The Cmd runs
    /// on, and the editor pane keeps the focus.
    #[test]
    fn escape_on_a_running_block_moves_the_cmd_into_a_run_pane() {
        let mut h = loaded(HELLO);
        let editor_pane = h.stoat.active_workspace().panes.focus();
        let session = entered(&mut h, "hello");
        let cmd = block_cmd(&h);
        h.type_keys("escape");

        let stacks = run_stacks(&h);
        let [(run_pane, ref stack)] = stacks[..] else {
            panic!("one Run pane, not {}", stacks.len());
        };
        let ws = h.stoat.active_workspace();
        let (above, below) = (
            ws.panes.pane(editor_pane).area,
            ws.panes.pane(run_pane).area,
        );
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                stack.clone(),
                (below.x, below.y > above.y),
                session.was_killed(),
                ws.panes.focus() == editor_pane,
            ),
            (true, vec![cmd], (above.x, true), false, true),
        );
    }

    #[test]
    fn ctrl_c_interrupts_a_running_block_and_leaves_it_open() {
        let mut h = loaded(HELLO);
        let session = entered(&mut h, "hello");
        h.type_keys("ctrl-c");
        assert_eq!(
            (session.sent_bytes(), active_modal(&h.stoat)),
            (vec![vec![0x03]], Some(ActiveModal::RunBlock)),
        );
    }

    #[test]
    fn ctrl_c_closes_a_finished_block() {
        let mut h = loaded(HELLO);
        entered(&mut h, "hello");
        let cmd = block_cmd(&h);
        h.inject_cmd_done(cmd, 0);
        h.type_keys("ctrl-c");
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                h.stoat.active_workspace().cmds.contains_key(cmd)
            ),
            (true, false),
        );
    }

    #[test]
    fn escape_on_a_finished_block_drops_the_cmd() {
        let mut h = loaded(HELLO);
        entered(&mut h, "hello");
        h.inject_cmd_done(block_cmd(&h), 0);
        h.type_keys("escape");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                ws.cmds.len(),
                ws.runs.len()
            ),
            (true, 0, 0),
        );
    }

    /// The palette's input stays in normal mode under the Run Block, so typed
    /// keys change neither its text nor the buffer behind the palette.
    #[test]
    fn typing_changes_nothing_while_the_block_shows() {
        let mut h = loaded(HELLO);
        editor::seed_focused_buffer(&mut h.stoat, "behind\n");
        let behind = {
            let ws = h.stoat.active_workspace();
            let View::Editor(editor_id) = ws.panes.pane(ws.panes.focus()).view else {
                panic!("the focused pane shows an editor");
            };
            ws.editors[editor_id].buffer_id
        };
        entered(&mut h, "hello");
        h.type_keys("i a b x d d");

        let input = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette is open")
            .input
            .text(h.stoat.active_workspace());
        assert_eq!(
            (input, editor::buffer_text(&h.stoat, behind)),
            ("hello".to_owned(), "behind\n".to_owned()),
        );
    }

    /// Enter on a finished Line Cmd closes the palette and shows its output,
    /// the shell row and the program's rows, in a scratch buffer in the pane
    /// behind.
    #[test]
    fn enter_opens_a_finished_line_cmd_s_output_in_a_buffer() {
        let mut h = loaded(HELLO);
        entered(&mut h, "hello");
        let cmd = block_cmd(&h);
        h.inject_cmd_output(cmd, b"hi\r\n");
        h.inject_cmd_done(cmd, 0);
        h.type_keys("enter");

        let shown = {
            let ws = h.stoat.active_workspace();
            let View::Editor(editor_id) = ws.panes.pane(ws.panes.focus()).view else {
                panic!("the focused pane shows an editor");
            };
            ws.editors[editor_id].buffer_id
        };
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                editor::buffer_text(&h.stoat, shown)
            ),
            (true, "echo hi\nhi".to_owned()),
        );
    }

    /// Enter on a finished `cargo test` Cmd with a located failure closes the
    /// palette and opens the failure's file at its line and column.
    #[test]
    fn enter_goes_to_the_failure_of_a_finished_cargo_test() {
        let mut h = loaded(r#"command t { run = shell("cargo test"); }"#);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.seed_fixture(CHROME, format!("{}\n", "x".repeat(20)).repeat(500));
        entered(&mut h, "t");
        let cmd = block_cmd(&h);
        h.inject_cmd_output(cmd, ONE_FAILURE.as_bytes());
        h.inject_cmd_done(cmd, 101);
        h.type_keys("enter");

        let point = editor::focused_cursor_point(&mut h.stoat);
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                editor::focused_buffer_path(&h.stoat),
                (point.row, point.column),
            ),
            (true, PathBuf::from(CHROME), (411, 8)),
        );
    }

    /// A `block` command that runs with no palette open puts its Cmd on a Run
    /// pane's stack.
    #[test]
    fn a_block_command_with_no_palette_joins_the_run_pane() {
        let mut h = loaded(HELLO);
        let command = h.stoat.commands.get("hello").expect("hello loads").clone();
        user_command::run::start(&mut h.stoat, &command, Vec::new(), "hello");

        let ws = h.stoat.active_workspace();
        let stacked: Vec<&str> = run_stacks(&h)
            .iter()
            .flat_map(|(_, stack)| stack.iter().map(|id| ws.cmds[*id].command.as_str()))
            .collect();
        assert_eq!(
            (h.stoat.command_palette.is_none(), stacked),
            (true, vec!["hello"])
        );
    }

    /// `t` flips the Run Block's finished `cargo test` Cmd between its report
    /// and its raw output.
    #[test]
    fn t_flips_the_run_block_between_its_report_and_its_output() {
        let mut h = loaded(r#"command t { run = shell("cargo test"); }"#);
        entered(&mut h, "t");
        let cmd = block_cmd(&h);
        h.inject_cmd_output(cmd, ONE_FAILURE.as_bytes());
        h.inject_cmd_done(cmd, 101);
        let shows_report =
            |h: &TestHarness| h.stoat.active_workspace().cmds[cmd].report_rows().is_some();

        let report = shows_report(&h);
        h.type_keys("t");
        let raw = shows_report(&h);
        h.type_keys("t");
        assert_eq!([report, raw, shows_report(&h)], [true, false, true]);
    }

    /// Output of the Run Block's Cmd marks the frame dirty, and the running
    /// Cmd keeps the tick armed, as a Cmd of a Run pane on show does.
    #[test]
    fn the_run_block_s_output_repaints_and_its_cmd_ticks() {
        let mut h = loaded(HELLO);
        entered(&mut h, "hello");
        h.snapshot();
        let ticking = h.stoat.cmd_tick.is_some();

        let cmd = block_cmd(&h);
        h.stoat.pty_dirty = false;
        h.stoat.handle_pty_notification(PtyNotification::CmdOutput {
            workspace: h.stoat.active_workspace,
            cmd,
            run: h.stoat.active_workspace().cmds[cmd].run_count,
            data: b"one\r\n".to_vec(),
        });
        assert_eq!((ticking, h.stoat.pty_dirty), (true, true));
    }

    /// A line that Up recalls and Enter runs shows its Run Block, and the
    /// history walk ends.
    #[test]
    fn a_recalled_line_that_runs_ends_the_history_walk() {
        let mut h = loaded(HELLO);
        entered(&mut h, "hello");
        h.inject_cmd_done(block_cmd(&h), 0);
        h.type_keys("escape");

        typed(&mut h, "");
        h.type_keys("up");
        let recalled = h.stoat.active_workspace().palette_history.is_walking();
        h.type_keys("enter");
        assert_eq!(
            (
                recalled,
                h.stoat.run_block_cmd().is_some(),
                h.stoat.active_workspace().palette_history.is_walking(),
            ),
            (true, true, false),
        );
    }

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
