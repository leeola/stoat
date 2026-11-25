use super::jump;
use crate::{
    agent_goto,
    app::{Stoat, UpdateEffect},
    buffer_lifecycle,
    buffer_registry::OpenOrigin,
    command_palette::{self, Availability, PaletteCommand, RunLine},
    commit_picker,
    file_finder::OpenIntent,
    host::{FsHost, OffsetEncoding},
    input_history::InputHistory,
    pane::{Axis, FocusTarget, PaneId, View},
    render::{self, paint::style_rgb},
    run::{
        cargo::CmdReport,
        kind::{self, CargoKind, RunKind},
        pty::{self, CmdEnv, CmdSink},
        vterm::Handoff,
        Cmd, CmdId, PromptInfo, RunId, RunState, VtermGrid,
    },
    term_session::{TermId, ViewLocation},
    theme::scope,
    user_command::{self, run::PlannedStep, CommandSet},
    workspace::{Workspace, WorkspaceId},
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};
use stoat_action::{registry, ActionKind};

pub(super) fn open_run(stoat: &mut Stoat) -> UpdateEffect {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();

    let state = RunState::new(ws, executor.clone());
    let run_id = ws.runs.insert(state);

    buffer_lifecycle::replace_pane_view(ws, &executor, focused, View::Run(run_id));
    stoat.transition_mode("insert".into());
    UpdateEffect::Redraw
}

/// Run the line at the focused Run pane's prompt.
///
/// An Enter at an empty prompt goes to the newest block instead, as
/// [`block_enter`] takes it. A line whose program is `claude` starts an agent
/// in a tab of its own and leaves a Stub, as [`hand_off_agent`] starts it.
///
/// A line whose head names a command runs that command, as
/// [`command_palette::resolve_line`] reads it. An action dispatches and
/// leaves no block, and a config command starts with the typed line, as the
/// palette starts one. A bare `cd` goes to `~`. An argument that does not
/// parse sets the status line and leaves the line at the prompt. Any other
/// line runs as a shell line in a new Cmd.
pub(super) fn run_submit(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let text = {
        let Some(run_state) = ws.runs.get(id) else {
            return UpdateEffect::None;
        };
        run_state.input.text(ws)
    };
    if text.is_empty() {
        let newest = ws.runs.get(id).and_then(|run| run.blocks.last().copied());
        return match newest {
            Some(cmd) => block_enter(stoat, cmd),
            None => UpdateEffect::None,
        };
    }

    // A shell's bare `cd` goes home, and `SetCwd` takes a path.
    let line = match text.trim() {
        "cd" => "cd ~",
        _ => text.as_str(),
    };
    let agent = kind::run_kind(line) == RunKind::Agent;
    let ctx = Availability::from_stoat(stoat);
    let resolved = match agent {
        true => None,
        false => command_palette::resolve_line(line, &ctx, &stoat.commands),
    };
    let command = match resolved.transpose() {
        Ok(command) => command,
        Err(message) => {
            stoat.set_status(message);
            return UpdateEffect::Redraw;
        },
    };

    let entry = history_entry(&text, &ctx, &stoat.commands);
    let ran_at = commit_picker::now_epoch(&stoat.executor);
    let ws = stoat.active_workspace_mut();
    ws.palette_history.push_at(entry, Some(ran_at));
    ws.palette_history.reset();
    let Some(input) = ws.runs.get(id).map(|run_state| run_state.input.clone()) else {
        return UpdateEffect::None;
    };
    input.replace_text(ws, "");

    if agent {
        return hand_off_agent(stoat, &text, true);
    }
    match command {
        Some(RunLine::Action(entry, params)) => match (entry.create)(&params) {
            Ok(action) => super::dispatch(stoat, &*action),
            Err(err) => {
                tracing::warn!(
                    target: "stoat::command",
                    command = entry.def.name(),
                    %err,
                    "run prompt action rejected"
                );
                UpdateEffect::Redraw
            },
        },
        Some(RunLine::User(command, values)) => {
            user_command::run::start(stoat, &command, values, &text)
        },
        None => run_shell_line(stoat, id, focused, &text),
    }
}

/// Run `text` as a shell line in a new Cmd on the stack of the run `id`, which
/// the split pane `pane` shows.
fn run_shell_line(stoat: &mut Stoat, id: RunId, pane: PaneId, text: &str) -> UpdateEffect {
    // The body draws one column inside the stack, which stands one column
    // inside each side of the pane.
    let width = stoat
        .active_workspace()
        .panes
        .pane(pane)
        .area
        .width
        .saturating_sub(3)
        .max(20);
    let cmd = start_cmd(stoat, text, width);
    let Workspace { runs, cmds, .. } = stoat.active_workspace_mut();
    if let Some(run_state) = runs.get_mut(id) {
        run_state.blocks.push(cmd);
        // Submitting snaps the output back to the prompt, like a terminal.
        run_state.scroll_offset = 0;
        run_state.trim_blocks(cmds);
    }
    tracing::info!(target: "stoat::run", command = %text, "run pane command started");
    UpdateEffect::Redraw
}

/// Resolve the prompt of each Run pane of the active workspace whose text
/// changed since its last resolve, so the painter reads the command token
/// and the tag at its right without a lookup of its own.
///
/// One [`Availability`] snapshot and the configured commands serve every
/// prompt, as [`prompt_info`] reads a text.
///
/// Each prompt also learns on every pass whether it shows the line that the
/// active walk of the command history recalled, because a walk ends with no
/// prompt's text changing. A Run pane drops its record of a recalled line
/// once no walk is active.
pub(crate) fn sync_run_prompts(stoat: &mut Stoat) {
    let (texts, walking, changed) = {
        let ws = stoat.active_workspace();
        let texts: Vec<(RunId, String)> = ws
            .runs
            .iter()
            .map(|(id, run)| (id, run.input.text(ws)))
            .collect();
        let changed = texts
            .iter()
            .any(|(id, text)| ws.runs.get(*id).is_some_and(|run| run.prompt.text != *text));
        (texts, ws.palette_history.is_walking(), changed)
    };

    let resolve = changed.then(|| (Availability::from_stoat(stoat), stoat.commands.clone()));
    let runs = &mut stoat.active_workspace_mut().runs;
    for (id, text) in texts {
        let Some(run) = runs.get_mut(id) else {
            continue;
        };
        if !walking {
            run.recalled = None;
        }
        let recalled = run.recalled.as_deref() == Some(text.as_str());
        if let Some((ctx, commands)) = resolve.as_ref()
            && run.prompt.text != text
        {
            run.prompt = prompt_info(text, ctx, commands);
        }
        run.prompt.recalled = recalled;
    }
}

/// What the prompt shows for `text`, its head resolved against `ctx` and
/// `commands` by the palette's rule for a head.
///
/// The command token runs from the text's start to the end of a head that
/// names a command, or to the end of the first word of a Parsed or Agent
/// line, so `cargo` takes the command color beside the `cargo test` chip.
/// Any other line has no token. The chip is the line's Kind Chip, and the
/// source is the Source Tag of the command that the head names.
fn prompt_info(text: String, ctx: &Availability, commands: &CommandSet) -> PromptInfo {
    let trimmed = text.trim_start();
    let lead = text.len() - trimmed.len();
    let first_word = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
    let head = command_palette::line_head(&text);
    let command = Some(head)
        .filter(|head| !head.is_empty())
        .and_then(|head| command_palette::resolve_command(head, ctx, commands));

    let kind = kind::run_kind(&text);
    let token_end = match (&command, kind) {
        (Some(_), _) => lead + head.len(),
        (None, RunKind::Parsed(_) | RunKind::Agent) => lead + first_word,
        (None, _) => 0,
    };
    let source = command.as_ref().and_then(|command| match command {
        PaletteCommand::User(user) => Some(user.source_tag()),
        PaletteCommand::Action(_) => render::command_palette::source_tag(command),
    });
    PromptInfo {
        token_len: text[..token_end].chars().count(),
        chip: kind.chip(),
        source,
        recalled: false,
        text,
    }
}

/// Take an Enter to the Cmd `id`, the newest block of a Run pane's stack or the
/// Cmd that the palette's Run Block shows, and answer a redraw when the Enter
/// acted.
///
/// A finished Cmd whose report located a test failure or a build error goes
/// to the first of them, as [`CmdReport::first_location`] picks it and
/// [`go_to_location`] opens it. rustc counts a column in chars, so the column
/// lands as UTF-32. A finished block of matches goes to its first match,
/// whose column a search tool counts in bytes. A finished Cmd that the
/// palette shows with no located failure opens its output, as
/// [`open_cmd_output`] does. A Stub goes to its tab, as
/// [`super::terminal::show_terminal`] reveals it, which reports that the
/// terminal exited once the tab's session ended. Every other Cmd takes the
/// Enter as nothing.
pub(super) fn block_enter(stoat: &mut Stoat, id: CmdId) -> UpdateEffect {
    let stub = stoat
        .active_workspace()
        .cmds
        .get(id)
        .and_then(|cmd| cmd.handoff.as_ref())
        .map(|handoff| handoff.term);
    if let Some(term) = stub {
        return super::terminal::show_terminal(stoat, term, OpenIntent::Replace);
    }

    let found = stoat.active_workspace().cmds.get(id).and_then(|cmd| {
        if let Some(first) = cmd.finished_locations().and_then(<[_]>::first) {
            return Some((cmd.cwd.clone(), first.clone(), OffsetEncoding::Utf8));
        }
        let location = cmd.finished_report().and_then(CmdReport::first_location)?;
        Some((cmd.cwd.clone(), location.clone(), OffsetEncoding::Utf32))
    });
    if let Some((cwd, location, encoding)) = found {
        go_to_location(
            stoat,
            &cwd,
            &location.path,
            location.line,
            location.column,
            encoding,
        );
        return UpdateEffect::Redraw;
    }

    let shown = stoat.run_block_cmd() == Some(id);
    let finished = stoat
        .active_workspace()
        .cmds
        .get(id)
        .is_some_and(|cmd| cmd.finished);
    if !(shown && finished) {
        return UpdateEffect::None;
    }
    open_cmd_output(stoat, id);
    UpdateEffect::Redraw
}

/// Show the output of the Cmd `id` in a new scratch buffer, in the pane that
/// [`agent_goto::target_pane`] picks, which takes the focus.
fn open_cmd_output(stoat: &mut Stoat, id: CmdId) {
    let Some(text) = stoat.active_workspace().cmds.get(id).map(|cmd| {
        let grid = &cmd.grid;
        grid.text_in(0..usize::from(grid.width()), 0..grid.rendered_line_count())
    }) else {
        return;
    };
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let pane = agent_goto::target_pane(ws, executor);
    ws.focus = FocusTarget::SplitPane;
    ws.panes.set_focus(pane);
    buffer_lifecycle::show_scratch_with_text(stoat, &text);
}

/// Open the file at `path` with the cursor on `line` and `column`, both
/// counted from 1, and answer whether it opened.
///
/// The file opens in the pane that [`agent_goto::target_pane`] picks, since
/// the focused pane shows the Run pane, and that pane takes the focus. Its
/// jumplist records where it was. `column` counts in the units of `encoding`.
///
/// A path that names no file, as [`located_file`] reads it from `base`, sets
/// the status `no file <path>` and opens nothing.
fn go_to_location(
    stoat: &mut Stoat,
    base: &Path,
    path: &str,
    line: u32,
    column: u32,
    encoding: OffsetEncoding,
) -> bool {
    let Some(file) = located_file(&*stoat.fs_host, base, path) else {
        stoat.set_status(format!("no file {path}"));
        return false;
    };

    let target = {
        let executor = stoat.executor.clone();
        let ws = stoat.active_workspace_mut();
        let pane = agent_goto::target_pane(ws, executor);
        ws.focus = FocusTarget::SplitPane;
        ws.panes.set_focus(pane);
        pane
    };
    jump::push_jump(stoat);
    if buffer_lifecycle::open_file_in_pane_now(stoat, target, &file, OpenOrigin::Visited).is_none()
    {
        return false;
    }

    let Some(offset) =
        stoat.offset_for_focused_point(line.saturating_sub(1), column.saturating_sub(1), encoding)
    else {
        return false;
    };
    stoat.collapse_focused_cursor_to(offset);
    true
}

/// The file that a location's `path` names, read from `base`.
///
/// An absolute path names itself. A relative one names the first file it
/// joins to from `base` or from an ancestor of `base`, since cargo names a
/// path from the workspace root and a Cmd can run in a member's directory.
/// `None` when no path it names is a file.
fn located_file(fs: &dyn FsHost, base: &Path, path: &str) -> Option<PathBuf> {
    let is_file =
        |candidate: &Path| matches!(fs.metadata(candidate), Ok(Some(meta)) if !meta.is_dir);
    if Path::new(path).is_absolute() {
        return Some(PathBuf::from(path)).filter(|file| is_file(file));
    }
    base.ancestors()
        .map(|dir| dir.join(path))
        .find(|file| is_file(file))
}

/// Flip a block between its report or its matches and its raw output, the Cmd
/// that the palette's Run Block shows or else the newest block of the focused
/// Run pane.
///
/// Only a block with a [`Cmd::finished_report`] or
/// [`Cmd::finished_locations`] flips, so the key does nothing while the Cmd
/// runs and for a Cmd that keeps neither.
pub(super) fn run_toggle_view(stoat: &mut Stoat) -> UpdateEffect {
    let shown = stoat.run_block_cmd();
    let ws = stoat.active_workspace_mut();
    let newest = || {
        let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
            return None;
        };
        ws.runs.get(run)?.blocks.last().copied()
    };
    let Some(id) = shown.or_else(newest) else {
        return UpdateEffect::None;
    };
    let Some(cmd) = ws
        .cmds
        .get_mut(id)
        .filter(|cmd| cmd.finished_report().is_some() || cmd.finished_locations().is_some())
    else {
        return UpdateEffect::None;
    };
    cmd.raw_view = !cmd.raw_view;
    UpdateEffect::Redraw
}

/// Open the matches of the newest finished block of matches on the focused
/// Run pane's stack as a location list.
///
/// The matches resolve on a worker against the Cmd's cwd, as the matches of
/// a `locations` run from the palette resolve, and the list opens once they
/// land. A location picked from the list opens beside the Run pane, so the
/// stack stays.
pub(super) fn run_location_list(stoat: &mut Stoat) -> UpdateEffect {
    let found = {
        let ws = stoat.active_workspace();
        let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
            return UpdateEffect::None;
        };
        ws.runs.get(run).and_then(|run_state| {
            run_state
                .blocks
                .iter()
                .rev()
                .filter_map(|id| ws.cmds.get(*id))
                .find_map(|cmd| {
                    let locations = cmd.finished_locations()?;
                    Some((locations.to_vec(), cmd.cwd.clone()))
                })
        })
    };
    let Some((locations, cwd)) = found else {
        return UpdateEffect::None;
    };
    user_command::run::spawn_resolve_locations(stoat, locations, cwd);
    UpdateEffect::Redraw
}

/// Run a finished Cmd again in place, the Cmd of the palette's Run Block when
/// the stage shows, else the newest finished Cmd of the focused Run pane's
/// stack.
///
/// A running Cmd of the Run Block stays as it is, and so does a Run pane with
/// no finished Cmd. A Stub whose tab's session lives counts as running,
/// because its program runs on in that tab. The Cmd keeps its place, as
/// [`restart_cmd`] starts it.
pub(super) fn run_again(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace();
    let finished = |id: &CmdId| {
        ws.cmds.get(*id).is_some_and(|cmd| {
            let tab_lives = cmd
                .handoff
                .as_ref()
                .is_some_and(|handoff| ws.terms.contains_key(handoff.term));
            cmd.finished && !tab_lives
        })
    };
    let id = match stoat.run_block_cmd() {
        Some(shown) => Some(shown).filter(finished),
        None => {
            let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
                return UpdateEffect::None;
            };
            ws.runs
                .get(run)
                .and_then(|run_state| run_state.blocks.iter().rev().copied().find(finished))
        },
    };
    let Some(id) = id else {
        return UpdateEffect::None;
    };
    restart_cmd(stoat, id).merge(UpdateEffect::Redraw)
}

/// Start the next run of the Cmd `id` with an empty output, in the directory
/// and in the place where it ran before, and answer the effect of the action
/// steps that ran before its first process.
///
/// A Stub runs in its block again, with the kind that [`steps_kind`] reads
/// from its line and its steps. An agent's Stub starts no process. It starts
/// the agent again in a new tab, as [`open_agent_tab`] opens it, and stays a
/// Stub of that tab.
fn restart_cmd(stoat: &mut Stoat, id: CmdId) -> UpdateEffect {
    let agent_line = stoat
        .active_workspace()
        .cmds
        .get(id)
        .filter(|cmd| cmd.handoff.as_ref().is_some_and(|handoff| handoff.agent))
        .map(|cmd| cmd.run_text().to_owned());
    if let Some(line) = agent_line {
        if let Some(handoff) = open_agent_tab(stoat, &line)
            && let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id)
        {
            cmd.handoff = Some(handoff);
        }
        return UpdateEffect::None;
    }

    let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id) else {
        return UpdateEffect::None;
    };
    cmd.grid = VtermGrid::new(cmd.grid.width());
    cmd.selection = None;
    cmd.handoff = None;
    cmd.kind = steps_kind(&cmd.command, &cmd.steps);
    start_process(stoat, id)
}

/// Hand the Cmd `id`, whose output asked for the alternate screen, to a new
/// terminal tab that runs its text again, and leave the Cmd as a Stub.
///
/// The Cmd's process is killed and the Cmd ends. The tab runs `bash -c` of
/// the Cmd's text in the Cmd's cwd and takes the focus, because the program
/// needs the keys. It goes by the text's program word until the program sets
/// a title, and it closes when the program ends, as [`retire_handoff`] closes
/// it. The Cmd of the palette's Run Block closes the palette and leaves the
/// store, because a Stub is a row of a Run pane. A spawn that fails ends the
/// Cmd with the error line `handoff failed`.
pub(crate) fn hand_off(stoat: &mut Stoat, id: CmdId) {
    let now = stoat.executor.now();
    let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id) else {
        return;
    };
    if let Some(process) = cmd.process.take() {
        process.kill();
    }
    cmd.finished = true;
    cmd.ended = Some(now);
    cmd.kind = RunKind::FullTerminal;
    let (text, cwd) = (cmd.run_text().to_owned(), cmd.cwd.clone());

    // A line of assignments alone names no program, so the tab goes by the
    // whole line.
    let label = kind::program_word(&text).unwrap_or(&text).to_owned();
    let args = ["-c".to_owned(), text];
    let view =
        super::terminal::spawn_terminal_view_with(stoat, "bash", &args, &cwd, Some(label.clone()));
    let View::Terminal(term) = view else {
        if let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id) {
            cmd.error = Some("handoff failed".into());
        }
        return;
    };
    if let Some(session) = stoat.active_workspace_mut().terms.get_mut(term) {
        session.closes_tab_on_exit = true;
    }
    open_tab_on(stoat, view);

    if stoat.run_block_cmd() == Some(id) {
        if let Some(palette) = stoat.command_palette.take() {
            super::palette::dispose_palette(stoat, palette);
        }
        stoat.active_workspace_mut().cmds.remove(id);
        return;
    }
    if let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id) {
        cmd.handoff = Some(Handoff {
            term,
            label,
            agent: false,
        });
    }
}

/// Hand each Cmd that waits on a hand-off to a terminal tab, as [`hand_off`]
/// hands it, in the workspace that holds the Cmd.
///
/// A Cmd waits on a hand-off when its start found that its program needs a
/// full terminal. Its kind is then `FullTerminal`, and it has not finished
/// and holds no process and no handoff. The frame's background pass runs
/// this, because a Cmd takes its place in a stack or in the palette's Run
/// Block after its start, and the hand-off reads that place.
pub(crate) fn pump_handoffs(stoat: &mut Stoat) {
    let waiting: Vec<(WorkspaceId, Vec<CmdId>)> = stoat
        .workspaces
        .iter()
        .map(|(workspace, ws)| {
            let ids = ws
                .cmds
                .iter()
                .filter(|(_, cmd)| {
                    cmd.kind == RunKind::FullTerminal
                        && !cmd.finished
                        && cmd.process.is_none()
                        && cmd.handoff.is_none()
                })
                .map(|(id, _)| id)
                .collect();
            (workspace, ids)
        })
        .collect();
    for (workspace, ids) in waiting {
        stoat.in_workspace(workspace, |stoat| {
            for id in ids {
                hand_off(stoat, id);
            }
        });
    }
}

/// Start the agent that `line` runs in a new tab, and with `stub`, leave a
/// Stub of the line on the stack of the focused Run pane.
///
/// The line starts no Cmd process, because a Cmd runs under `TERM=dumb`,
/// where the agent's terminal UI does not draw, and without the agent's
/// environment. The Run pane is read before the tab opens, because the new
/// tab takes the focus. A spawn that fails leaves no Stub, as
/// [`open_agent_tab`] reports it.
fn hand_off_agent(stoat: &mut Stoat, line: &str, stub: bool) -> UpdateEffect {
    let stack = {
        let ws = stoat.active_workspace();
        match ws.panes.pane(ws.panes.focus()).view {
            View::Run(run) if stub => Some((run, run_pane_width(stoat))),
            _ => None,
        }
    };
    let Some(handoff) = open_agent_tab(stoat, line) else {
        return UpdateEffect::Redraw;
    };
    let Some((run, width)) = stack else {
        return UpdateEffect::Redraw;
    };

    let ws = stoat.active_workspace_mut();
    let mut cmd = Cmd::new(line.to_owned(), ws.git_root.clone(), width);
    cmd.finished = true;
    cmd.handoff = Some(handoff);
    let id = ws.cmds.insert(cmd);
    let Workspace { runs, cmds, .. } = ws;
    if let Some(run_state) = runs.get_mut(run) {
        run_state.blocks.push(id);
        run_state.scroll_offset = 0;
        run_state.trim_blocks(cmds);
    }
    UpdateEffect::Redraw
}

/// Start the agent that `line` runs in a new tab, and answer the handoff
/// that a Stub of the line keeps, or `None` when the agent did not start.
///
/// The session runs `bash -c` of the line with the agent's environment, as
/// [`super::agent::spawn_claude_term`] spawns it, and the tab goes by
/// `claude` until the agent sets a title. A spawn that fails reports `claude
/// did not start` in the status line.
fn open_agent_tab(stoat: &mut Stoat, line: &str) -> Option<Handoff> {
    let Some(term) = super::agent::spawn_claude_term(stoat, Some(line)) else {
        stoat.set_status("claude did not start");
        return None;
    };
    let label = "claude".to_owned();
    if let Some(session) = stoat.active_workspace_mut().terms.get_mut(term) {
        session.label = Some(label.clone());
    }
    open_tab_on(stoat, View::Agent(term));
    Some(Handoff {
        term,
        label,
        agent: true,
    })
}

/// Open a new tab whose one pane shows `view`, and give that pane the focus,
/// because the program it runs needs the keys.
fn open_tab_on(stoat: &mut Stoat, view: View) {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    ws.new_tab(&executor);
    let pane = ws.panes.focus();
    buffer_lifecycle::replace_pane_view(ws, &executor, pane, view);
    ws.focus = FocusTarget::SplitPane;
    super::tab::relayout(stoat);
}

/// Close the tab that a handoff opened for the ended session `term`, and
/// answer whether it closed.
///
/// The tab closes when the session's
/// [`TermSession::closes_tab_on_exit`](crate::term_session::TermSession::closes_tab_on_exit)
/// is set, its pane is the tab's only split pane, and the workspace has
/// another tab. The tab exists for that program, and its last pane turns
/// into a scratch buffer otherwise. The workspace goes back to the tab it
/// left, as [`super::tab::close_tab_at`] moves it.
pub(crate) fn retire_handoff(stoat: &mut Stoat, term: TermId) -> bool {
    let ws = stoat.active_workspace();
    let handed_off = ws
        .terms
        .get(term)
        .is_some_and(|session| session.closes_tab_on_exit);
    let Some(ViewLocation::Pane { tab, pane }) = ws.term_location(term) else {
        return false;
    };
    let tree = match tab == ws.active_tab {
        true => Some(&ws.panes),
        false => ws.tabs.get(tab).and_then(|slot| slot.parked.as_ref()),
    };
    let alone = tree.is_some_and(|tree| tree.split_pane_ids() == [pane]);
    if !(handed_off && alone && ws.tabs.len() > 1) {
        return false;
    }
    super::tab::close_tab_at(stoat, tab);
    true
}

/// Move the Cmd of the palette's Run Block, running or finished, onto a Run
/// pane's stack, and close the palette.
///
/// The Cmd stays in the store and its process runs on. The Run pane, as
/// [`append_to_run_pane`] picks it, takes the focus and its prompt takes
/// typing.
pub(super) fn run_block_to_pane(stoat: &mut Stoat) -> UpdateEffect {
    let Some(cmd) = stoat.run_block_cmd() else {
        return UpdateEffect::None;
    };
    let Some(palette) = stoat.command_palette.take() else {
        return UpdateEffect::None;
    };
    super::palette::dispose_palette(stoat, palette);
    append_to_run_pane(stoat, cmd, true);
    UpdateEffect::Redraw
}

pub(super) fn run_interrupt(stoat: &mut Stoat) -> UpdateEffect {
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let Some(run_state) = ws.runs.get(id) else {
        return UpdateEffect::None;
    };
    let newest_running = run_state
        .blocks
        .iter()
        .rev()
        .filter_map(|id| ws.cmds.get(*id))
        .find(|cmd| !cmd.finished);
    if let Some(process) = newest_running.and_then(|cmd| cmd.process.as_ref()) {
        process.send_interrupt();
    }
    UpdateEffect::Redraw
}

/// Recall the previous command in the focused run pane's prompt.
pub(super) fn run_history_prev(stoat: &mut Stoat) -> UpdateEffect {
    walk_focused_run(stoat, InputHistory::prev)
}

/// Recall the next command toward the newest in the focused run pane's prompt.
pub(super) fn run_history_next(stoat: &mut Stoat) -> UpdateEffect {
    walk_focused_run(stoat, InputHistory::next)
}

/// Walk the workspace's command history with `step` from the focused Run
/// pane's prompt, and write the recalled line into the prompt. No-op when
/// focus is not on a Run pane.
///
/// The history holds palette lines, and a recalled entry comes back as
/// [`prompt_form`] reads it. A step past the newest match ends the walk and
/// writes back the typed text as it was typed, since that text is no entry.
///
/// The prompt records the line it shows, so its status names the recall. The
/// walk goes on while the prompt still shows that line, and any other text
/// starts a fresh walk with that text as its needle. The check reads the
/// record, not the entry of the prompt's text, because a prompt form need not
/// map back to its entry. The entry `run make` shows as `make`, which
/// [`history_entry`] stores as `r make`. A walk runs at a time, so every
/// other Run pane drops its record.
fn walk_focused_run(
    stoat: &mut Stoat,
    step: fn(&mut InputHistory, &str) -> Option<String>,
) -> UpdateEffect {
    let ctx = Availability::from_stoat(stoat);
    let commands = stoat.commands.clone();
    let ws = stoat.active_workspace_mut();
    let focused = ws.panes.focus();
    let View::Run(id) = ws.panes.pane(focused).view else {
        return UpdateEffect::None;
    };
    let Some(input) = ws.runs.get(id).map(|run_state| run_state.input.clone()) else {
        return UpdateEffect::None;
    };

    let current = input.text(ws);
    let walks_on = ws
        .runs
        .get(id)
        .and_then(|run_state| run_state.recalled.as_deref())
        == Some(current.as_str());
    if !walks_on {
        ws.palette_history.reset();
    }
    let Some(recalled) = step(&mut ws.palette_history, &current) else {
        return UpdateEffect::Redraw;
    };
    let shown = match ws.palette_history.is_walking() {
        true => prompt_form(&recalled, &ctx, &commands),
        false => recalled,
    };
    input.replace_text(ws, &shown);
    for (run_id, run_state) in ws.runs.iter_mut() {
        run_state.recalled = (run_id == id).then(|| shown.clone());
    }
    UpdateEffect::Redraw
}

/// The palette line that stores the Run pane line `line` in the workspace's
/// command history.
///
/// A line whose head names a command is stored as typed, because the palette
/// runs it the same. Any other line is stored behind the head of the `run`
/// command, so the palette lists it as that command with the line as its
/// argument.
fn history_entry(line: &str, ctx: &Availability, commands: &CommandSet) -> String {
    let head = command_palette::line_head(line);
    if !head.is_empty() && command_palette::resolve_command(head, ctx, commands).is_some() {
        return line.to_owned();
    }
    registry::lookup("Run").map_or_else(
        || line.to_owned(),
        |run| {
            let head = command_palette::history_head(&PaletteCommand::Action(run));
            format!("{head} {line}")
        },
    )
}

/// The Run pane prompt's form of the command history's entry `entry`.
///
/// An entry that stores a shell line behind the head of the `run` command
/// shows as the bare line, because the bare line runs the same at the prompt.
/// The head stays when the line's own head names a command, since the bare
/// line would run that command. Any other entry shows as stored.
fn prompt_form(entry: &str, ctx: &Availability, commands: &CommandSet) -> String {
    let Some((first, rest)) = entry.split_once(' ') else {
        return entry.to_owned();
    };
    let names_run = matches!(
        command_palette::resolve_command(first, ctx, commands),
        Some(PaletteCommand::Action(found)) if found.def.kind() == ActionKind::Run
    );
    let rest_head = command_palette::line_head(rest);
    let rest_names_command = !rest_head.is_empty()
        && command_palette::resolve_command(rest_head, ctx, commands).is_some();
    match names_run && !rest_names_command {
        true => rest.to_owned(),
        false => entry.to_owned(),
    }
}

/// Run `line` as the `run` command runs it, as a Cmd on a Run pane's stack.
///
/// The Cmd joins the Run pane that [`append_to_run_pane`] picks, and the
/// focus stays where it is, so a user who ran the line from the palette works
/// on in the pane behind. A line of the Agent kind starts its agent tab and
/// leaves no Stub, because no stack holds it.
pub(super) fn run_line(stoat: &mut Stoat, line: &str) -> UpdateEffect {
    if kind::run_kind(line) == RunKind::Agent {
        return hand_off_agent(stoat, line, false);
    }

    let width = run_pane_width(stoat);
    let id = start_cmd(stoat, line, width);
    append_to_run_pane(stoat, id, false);
    tracing::info!(target: "stoat::run", command = %line, ?id, "run started");
    UpdateEffect::Redraw
}

/// Insert a Cmd of `line` in the active workspace's store, with the workspace
/// root as its cwd, and start its process.
fn start_cmd(stoat: &mut Stoat, line: &str, width: u16) -> CmdId {
    // A line with no step dispatches no action, so its start has no effect
    // to pass on.
    let (id, _) = start_cmd_steps(stoat, line, Vec::new(), None, width);
    id
}

/// Insert a Cmd of `line` in the active workspace's store, with the workspace
/// root as its cwd and a grid `width` cells wide, start its run of `steps`,
/// and answer the effect of the action steps that ran before its first
/// process.
///
/// A config command's line names the command, and its planned steps are its
/// run, as [`run_steps`] runs them. A Cmd with no step runs its line. The
/// Run Kind is the one that [`steps_kind`] reads, and `source` is the Source
/// Tag that the Cmd's header shows.
pub(crate) fn start_cmd_steps(
    stoat: &mut Stoat,
    line: &str,
    steps: Vec<PlannedStep>,
    source: Option<(&'static str, &'static str)>,
    width: u16,
) -> (CmdId, UpdateEffect) {
    let ws = stoat.active_workspace_mut();
    let mut cmd = Cmd::new(line.to_owned(), ws.git_root.clone(), width);
    cmd.kind = steps_kind(line, &steps);
    cmd.steps = steps;
    cmd.source = source;
    let id = ws.cmds.insert(cmd);
    (id, start_process(stoat, id))
}

/// The Run Kind of a Cmd of `line` that runs `steps`.
///
/// A plan of one shell step reads as its shell text, and a longer plan as a
/// plain line, because a report reads the output of one program. A Cmd with
/// no step reads as its line.
fn steps_kind(line: &str, steps: &[PlannedStep]) -> RunKind {
    match steps {
        [] => kind::run_kind(line),
        [PlannedStep::Shell(shell)] => kind::run_kind(shell),
        _ => RunKind::Line,
    }
}

/// Insert a finished Cmd of `line` whose run of `steps` went on a worker and
/// printed `output`, in the active workspace's store, and answer its id.
///
/// The Cmd is built as [`start_cmd_steps`] builds one, at the width that
/// [`run_pane_width`] gives, and keeps a list of locations, so a run again on
/// its own PTY reads its matches. Its output reaches its grid as a PTY writes
/// it, a carriage return before each line feed. It ends with `exit_status`,
/// ran from `started` to now, and counts one run.
pub(crate) fn finished_cmd(
    stoat: &mut Stoat,
    line: &str,
    steps: Vec<PlannedStep>,
    source: Option<(&'static str, &'static str)>,
    output: &[u8],
    exit_status: Option<i32>,
    started: Instant,
) -> CmdId {
    let width = run_pane_width(stoat);
    let now = stoat.executor.now();
    let ws = stoat.active_workspace_mut();
    let mut cmd = Cmd::new(line.to_owned(), ws.git_root.clone(), width);
    cmd.kind = steps_kind(line, &steps);
    cmd.reset_report();
    cmd.steps = steps;
    cmd.source = source;
    cmd.locations = Some(Vec::new());

    let mut written = Vec::with_capacity(output.len());
    for &byte in output {
        if byte == b'\n' {
            written.push(b'\r');
        }
        written.push(byte);
    }
    cmd.feed(&written);

    cmd.finished = true;
    cmd.exit_status = exit_status;
    cmd.started = Some(started);
    cmd.ended = Some(now);
    cmd.run_count = 1;
    ws.cmds.insert(cmd)
}

/// The grid width of a Cmd that [`append_to_run_pane`] places, in cells.
///
/// It is the width of the pane that takes the Cmd, or of the focused pane
/// when a new pane splits it, less the stack's inset and the body's column,
/// and at least 20.
pub(crate) fn run_pane_width(stoat: &Stoat) -> u16 {
    let ws = stoat.active_workspace();
    let pane = run_pane_on_show(ws).map_or_else(|| ws.panes.focus(), |(pane, _)| pane);
    ws.panes.pane(pane).area.width.saturating_sub(3).max(20)
}

/// Push the Cmd `cmd` onto a Run pane's stack, and answer that pane's run.
///
/// The focused split pane takes the Cmd when it shows a Run pane, else the
/// first split pane that shows one, else a new Run pane that splits the
/// focused pane downward, as `SplitDown` does. The stack snaps back to its
/// prompt. With `focus`, the pane takes the focus and its prompt takes
/// typing. Without it, the focus stays where it is.
pub(crate) fn append_to_run_pane(stoat: &mut Stoat, cmd: CmdId, focus: bool) -> RunId {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    let (pane, run) = match run_pane_on_show(ws) {
        Some(shown) => shown,
        None => {
            let focused = ws.panes.focus();
            let state = RunState::new(ws, executor);
            let run = ws.runs.insert(state);
            let pane = ws.panes.split(Axis::Horizontal);
            ws.panes.pane_mut(pane).view = View::Run(run);
            ws.panes.set_focus(focused);
            (pane, run)
        },
    };

    let Workspace { runs, cmds, .. } = &mut *ws;
    if let Some(run_state) = runs.get_mut(run) {
        run_state.blocks.push(cmd);
        run_state.scroll_offset = 0;
        run_state.trim_blocks(cmds);
    }
    if focus {
        ws.focus = FocusTarget::SplitPane;
        ws.panes.set_focus(pane);
        stoat.transition_mode("insert".into());
    }
    run
}

/// The split pane that shows a Run pane and its run, the focused pane when it
/// shows one and else the first such pane, or `None` with no Run pane on
/// show.
fn run_pane_on_show(ws: &Workspace) -> Option<(PaneId, RunId)> {
    let focused = ws.panes.focus();
    if let View::Run(run) = ws.panes.pane(focused).view {
        return Some((focused, run));
    }
    ws.panes
        .split_panes()
        .find_map(|(pane, shown)| match shown.view {
            View::Run(run) => Some((pane, run)),
            _ => None,
        })
}

/// Start the next run of the Cmd `id` of the active workspace from its first
/// step, as [`run_steps`] runs it, and answer the effect of the action steps
/// that ran before its first process.
///
/// The run counts one more, starts now, and clears the end, the elapsed
/// seconds, the status, the report, and the matches of the run before it.
fn start_process(stoat: &mut Stoat, id: CmdId) -> UpdateEffect {
    let now = stoat.executor.now();
    let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id) else {
        return UpdateEffect::None;
    };

    cmd.run_count += 1;
    cmd.started = Some(now);
    cmd.ended = None;
    cmd.elapsed_secs = 0;
    cmd.finished = false;
    cmd.exit_status = None;
    cmd.error = None;
    cmd.reset_report();
    if let Some(locations) = cmd.locations.as_mut() {
        locations.clear();
    }
    cmd.raw_view = false;
    cmd.step = 0;
    run_steps(stoat, id, true)
}

/// Run the Cmd `id` of the active workspace from its current step, up to the
/// shell step whose process it waits on or to its end, and answer the effect
/// of the action steps that ran.
///
/// A Cmd with no step runs its line. A shell step runs as a `bash -c` of its
/// text, after a row of the text in `ui.text.muted` when the text is not the
/// Cmd's line, so each step's output reads under its text. An action step
/// dispatches as a key binding of it does, and the run goes on to the next
/// step. Past the last step, the Cmd ends with the status of the step before
/// it.
///
/// An action acts on the workspace on screen, so an action step that comes
/// due while the Cmd's workspace is not `on_screen` ends the Cmd with the line
/// `stopped, the workspace changed`.
pub(crate) fn run_steps(stoat: &mut Stoat, id: CmdId, on_screen: bool) -> UpdateEffect {
    let muted = style_rgb(stoat.theme.get(scope::UI_TEXT_MUTED).fg);
    let mut effect = UpdateEffect::None;
    loop {
        let now = stoat.executor.now();
        let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id) else {
            return effect;
        };
        if cmd.steps.is_empty() {
            start_run_text(stoat, id);
            return effect;
        }
        let Some(step) = cmd.steps.get(cmd.step).cloned() else {
            cmd.finished = true;
            cmd.ended = Some(now);
            return effect;
        };

        match step {
            PlannedStep::Act(_) if !on_screen => {
                cmd.error = Some("stopped, the workspace changed".to_owned());
                cmd.exit_status = None;
                cmd.finished = true;
                cmd.ended = Some(now);
                return effect;
            },
            PlannedStep::Act(action) => {
                let ran = stoat.run_bound_actions(&Arc::from(vec![action]), None, false);
                effect = effect.merge(ran);
                let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id) else {
                    return effect;
                };
                cmd.step += 1;
            },
            PlannedStep::Shell(text) => {
                if text != cmd.command {
                    let row = match muted {
                        Some([r, g, b]) => format!("\x1b[38;2;{r};{g};{b}m{text}\x1b[0m\r\n"),
                        None => format!("{text}\r\n"),
                    };
                    cmd.grid.feed(row.as_bytes());
                }
                cmd.shell = Some(text);
                start_run_text(stoat, id);
                return effect;
            },
        }
    }
}

/// Start the run text of the Cmd `id` of the active workspace as the process
/// of its current run, or leave the Cmd to a hand-off when its program needs
/// a full terminal.
///
/// A program that needs a full terminal, as [`needs_full_terminal`] reads the
/// setting `run.full_terminal`, starts no run under `TERM=dumb`, where it
/// draws no screen. The Cmd takes the kind `FullTerminal` instead, and
/// [`pump_handoffs`] hands it to a terminal tab once it holds its place.
fn start_run_text(stoat: &mut Stoat, id: CmdId) {
    let full_terminal = stoat
        .active_workspace()
        .cmds
        .get(id)
        .is_some_and(|cmd| needs_full_terminal(stoat, cmd.run_text()));
    if !full_terminal {
        spawn_run_text(stoat, id);
        return;
    }
    if let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id) {
        cmd.kind = RunKind::FullTerminal;
    }
}

/// Whether `text` runs a program that needs a full terminal, one that the
/// setting `run.full_terminal` names, or with no setting one of
/// [`kind::FULL_TERMINAL_PROGRAMS`].
fn needs_full_terminal(stoat: &Stoat, text: &str) -> bool {
    match stoat.settings.run_full_terminal.as_deref() {
        Some(programs) => kind::needs_full_terminal(text, programs),
        None => kind::needs_full_terminal(text, &kind::FULL_TERMINAL_PROGRAMS),
    }
}

/// Spawn a `bash -c` of the run text of the Cmd `id` of the active workspace
/// in its cwd, as the process of its current run.
///
/// The process runs with the variables of the Cmd's Run Kind. A spawn that
/// fails ends the run at once with the error line `spawn failed: <error>`.
fn spawn_run_text(stoat: &mut Stoat, id: CmdId) {
    let now = stoat.executor.now();
    let executor = stoat.executor.clone();
    let host = stoat.terminal_host.clone();
    let tx = stoat.pty_tx.clone();
    let workspace = stoat.active_workspace;

    let ws = stoat.active_workspace_mut();
    let diff = ws.env.diff.clone();
    let Some(cmd) = ws.cmds.get_mut(id) else {
        return;
    };

    let sink = CmdSink {
        tx,
        workspace,
        cmd: id,
        run: cmd.run_count,
    };
    let width = cmd.grid.width();
    // TERM=dumb turns cargo's color off, and the block of a Parsed Cmd shows
    // cargo's colors and lets cargo erase its own status lines. Cargo draws
    // its progress bar to a dumb terminal only when asked, and only with a
    // width. Each redraw fills the grid's width and ends in a carriage return,
    // so the bar keeps to one row. A test run's progress counts its tests, so
    // it asks for no bar.
    let color = ("CARGO_TERM_COLOR".to_owned(), "always".to_owned());
    let vars = match cmd.kind {
        RunKind::Parsed(CargoKind::Test) => vec![color],
        RunKind::Parsed(CargoKind::Build | CargoKind::Check | CargoKind::Clippy) => vec![
            color,
            ("CARGO_TERM_PROGRESS_WHEN".to_owned(), "always".to_owned()),
            ("CARGO_TERM_PROGRESS_WIDTH".to_owned(), width.to_string()),
        ],
        RunKind::Line | RunKind::Agent | RunKind::FullTerminal => Vec::new(),
    };
    let env = CmdEnv {
        diff: &diff,
        vars: &vars,
    };
    match pty::spawn_cmd(
        &*host,
        &executor,
        cmd.run_text(),
        &cmd.cwd,
        width,
        sink,
        &env,
    ) {
        Ok(process) => cmd.process = Some(process),
        Err(error) => {
            cmd.error = Some(format!("spawn failed: {error}"));
            cmd.finished = true;
            cmd.ended = Some(now);
        },
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        app::{Stoat, UpdateEffect},
        commit_picker,
        host::FakeTerminalSession,
        input_history::InputHistory,
        keymap_state,
        location_picker::MAX_LOCATIONS,
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, PaneId, View},
        render::{paint::style_rgb, run_pane::RECALL_NOTE},
        run::{
            cargo::{ERROR_AND_WARNING, ONE_FAILURE, THREE_BINARIES},
            kind::RunKind,
            vterm::BodyRow,
            Cmd, CmdId, GridSelection, PtyNotification, RunId,
        },
        term_session::TermId,
        test_harness::{editor, CmdHandle, TestHarness},
        theme::scope,
        workspace::Workspace,
    };
    use futures::FutureExt;
    use ratatui::{
        buffer::Buffer,
        layout::Rect,
        style::{Color, Modifier},
    };
    use std::{io, path::PathBuf, sync::Arc, time::Duration};
    use stoatty_protocol::command::{self, Command};

    /// The file that the failure and the error of the samples name, under the
    /// root `/work`.
    const CHROME: &str = "/work/stoat/src/render/chrome.rs";

    /// The user command `hello`, which runs `echo hi` under the default
    /// `block` output.
    const HELLO: &str = r#"command hello { run = shell("echo hi"); }"#;

    /// The request for the alternate screen that a full-screen program writes
    /// first.
    const FULL_SCREEN: &[u8] = b"\x1b[?1049h";

    /// A line that runs the agent with a quoted prompt.
    const CLAUDE_LINE: &str = r#"claude -p "hi""#;

    /// The user command `tui`, which runs a full-screen program under the
    /// default `block` output.
    const TUI: &str = r#"command tui { run = shell("mytui"); }"#;

    /// The program, the arguments, the cwd, and the last variable of a spawn of
    /// the fake terminal host.
    type Spawn = (String, Vec<String>, PathBuf, Option<(String, String)>);

    /// A progress bar that the run pane draws, as its left edge, its width,
    /// and its color.
    type ProgressBar = (i16, u16, [u8; 3]);

    /// A bar beside a block's body, as its top edge, its height, and its color.
    type BodyBar = (i16, u16, [u8; 3]);

    /// A text run of a frame's scene, as its text and its color.
    type SceneRun = (String, [u8; 3]);

    /// A box of a frame's scene, as its color and its right edge in sixteenths.
    type SceneBox = ([u8; 3], i32);

    #[test]
    fn a_run_drops_the_editor_it_replaces() {
        let mut h = Stoat::test();
        let editor = {
            let ws = h.stoat.active_workspace();
            let View::Editor(editor) = ws.panes.pane(ws.panes.focus()).view else {
                panic!("the focused pane shows an editor");
            };
            editor
        };

        super::super::dispatch(&mut h.stoat, &stoat_action::OpenRun);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                matches!(ws.panes.pane(ws.panes.focus()).view, View::Run(_)),
                ws.editors.contains_key(editor)
            ),
            (true, false),
            "the run takes the pane, and the editor it replaced goes"
        );
    }

    /// Closing a Run pane drops its run and every Cmd of its stack.
    #[test]
    fn closing_a_run_pane_empties_the_cmd_store() {
        let mut h = Stoat::test();
        let run_id = h.open_run();
        for line in ["ls", "date"] {
            let input = h.stoat.active_workspace().runs[run_id].input.clone();
            input.replace_text(h.stoat.active_workspace_mut(), line);
            super::super::dispatch(&mut h.stoat, &stoat_action::RunSubmit);
        }
        let submitted = h.stoat.active_workspace().cmds.len();

        let executor = h.stoat.executor.clone();
        super::super::pane::dispose_view(
            h.stoat.active_workspace_mut(),
            &executor,
            View::Run(run_id),
            super::super::pane::EditorDisposal::Remove,
        );

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (submitted, ws.cmds.len(), ws.runs.len()),
            (2, 0, 0),
            "the two submitted Cmds leave the store with their run"
        );
    }

    /// Each spawn of the fake terminal host of `h`, in order.
    fn spawns(h: &TestHarness) -> Vec<Spawn> {
        h.fake_terminal_host()
            .spawns()
            .into_iter()
            .map(|spawn| {
                (
                    spawn.program,
                    spawn.args,
                    spawn.cwd,
                    spawn.env.last().cloned(),
                )
            })
            .collect()
    }

    /// A spawn of `bash -c line` in `cwd` with `TERM=dumb` last.
    fn bash_in(line: &str, cwd: &str) -> Spawn {
        (
            "bash".to_owned(),
            vec!["-c".to_owned(), line.to_owned()],
            PathBuf::from(cwd),
            Some(("TERM".to_owned(), "dumb".to_owned())),
        )
    }

    /// A new Run pane starts no process, and each submitted line starts a
    /// `bash -c` of its own in the workspace root and writes into no session.
    #[test]
    fn each_submitted_line_starts_its_own_bash() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.open_run();
        let opened = h.fake_terminal_host().spawns().len();
        let handles = ["ls", "date"].map(|line| h.submit_run(line));

        let writes = [&handles[0].session, &handles[1].session, h.fake_terminal()]
            .map(|session| session.sent_bytes().len());
        assert_eq!(
            (opened, spawns(&h), writes),
            (
                0,
                vec![bash_in("ls", "/work"), bash_in("date", "/work")],
                [0, 0, 0]
            ),
        );
    }

    /// Output and the end of each process land on the Cmd that owns it alone.
    #[test]
    fn output_reaches_the_cmd_that_owns_the_process() {
        let mut h = Stoat::test();
        h.open_run();
        let [first, second] = ["one", "two"].map(|line| h.submit_run(line));
        first.session.push_output(b"hello");
        h.settle();
        let text = |h: &TestHarness, handle: &CmdHandle| {
            h.stoat.active_workspace().cmds[handle.cmd]
                .grid
                .text_in(0..5, 0..1)
        };
        let read = [text(&h, &first), text(&h, &second)];

        first.session.finish(3);
        second.session.finish(0);
        h.settle();
        let ws = h.stoat.active_workspace();
        let ends = [&first, &second].map(|handle| {
            let cmd = &ws.cmds[handle.cmd];
            (cmd.exit_status, cmd.ended.is_some(), cmd.process.is_none())
        });
        assert_eq!(
            (read, ends),
            (
                ["hello".to_owned(), String::new()],
                [(Some(3), true, true), (Some(0), true, true)]
            ),
        );
    }

    #[test]
    fn a_cmd_records_when_it_ran() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("sleep 1");
        h.advance_clock(Duration::from_millis(1500));
        handle.session.finish(0);
        h.settle();

        let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
        let ran = cmd
            .ended
            .zip(cmd.started)
            .map(|(ended, started)| ended - started);
        assert_eq!((ran, cmd.run_count), (Some(Duration::from_millis(1500)), 1));
    }

    #[test]
    fn output_of_an_earlier_run_is_dropped() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("ls");
        h.stoat.handle_pty_notification(PtyNotification::CmdOutput {
            workspace: h.stoat.active_workspace,
            cmd: handle.cmd,
            run: 0,
            data: b"stale".to_vec(),
        });

        let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
        assert_eq!(
            (cmd.run_count, cmd.grid.text_in(0..5, 0..1)),
            (1, String::new())
        );
    }

    /// A `cd` line starts no process, since shell state does not carry from
    /// one Cmd to the next. It moves the workspace root that the next line
    /// starts in.
    #[test]
    fn a_cd_line_moves_the_workspace_root_and_starts_no_process() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.fake_fs().insert_file("/work/sub/a.txt", b"");
        let run_id = h.open_run();
        let cd = h.submit_run("cd sub");
        let ws = h.stoat.active_workspace();
        let after_cd = (
            ws.git_root.clone(),
            h.fake_terminal_host().spawns().len(),
            ws.runs[run_id].blocks.len(),
            cd.cmd == CmdId::default(),
        );

        h.submit_run("ls");
        assert_eq!(
            (after_cd, spawns(&h)),
            (
                (PathBuf::from("/work/sub"), 0, 0, true),
                vec![bash_in("ls", "/work/sub")]
            ),
        );
    }

    /// Ctrl-C goes to the newest Cmd that still runs.
    #[test]
    fn ctrl_c_reaches_the_newest_running_cmd() {
        let mut h = Stoat::test();
        h.open_run();
        let [first, second] = ["sleep 9", "sleep 8"].map(|line| h.submit_run(line));
        super::super::dispatch(&mut h.stoat, &stoat_action::RunInterrupt);
        let after_one = [first.session.sent_bytes(), second.session.sent_bytes()];

        second.session.finish(0);
        h.settle();
        super::super::dispatch(&mut h.stoat, &stoat_action::RunInterrupt);
        let after_two = [first.session.sent_bytes(), second.session.sent_bytes()];
        assert_eq!(
            (after_one, after_two),
            (
                [Vec::new(), vec![vec![0x03]]],
                [vec![vec![0x03]], vec![vec![0x03]]]
            ),
        );
    }

    #[test]
    fn closing_a_run_pane_kills_its_running_cmds() {
        let mut h = Stoat::test();
        let run_id = h.open_run();
        let handles = ["sleep 9", "sleep 8"].map(|line| h.submit_run(line));

        let executor = h.stoat.executor.clone();
        super::super::pane::dispose_view(
            h.stoat.active_workspace_mut(),
            &executor,
            View::Run(run_id),
            super::super::pane::EditorDisposal::Remove,
        );
        assert_eq!(
            handles.map(|handle| handle.session.was_killed()),
            [true, true]
        );
    }

    /// A full-screen request kills its own Cmd's process and runs the Cmd's
    /// text again in a new terminal tab, which takes the focus and goes by the
    /// program's name. The other Cmd runs on.
    #[test]
    fn a_full_screen_request_hands_the_cmd_to_a_terminal_tab() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.open_run();
        let [first, second] = ["mytui -d 5", "sleep 9"].map(|line| h.submit_run(line));
        h.inject_cmd_output(first.cmd, FULL_SCREEN);

        let ws = h.stoat.active_workspace();
        let stub = &ws.cmds[first.cmd];
        let term = stub.handoff.as_ref().map(|handoff| handoff.term);
        assert_eq!(
            (
                first.session.was_killed(),
                spawns(&h).pop(),
                (ws.tabs.len(), ws.active_tab, ws.tab_title(1)),
                Some(ws.panes.pane(ws.panes.focus()).view.clone()),
                (stub.finished, stub.kind),
                (ws.cmds[second.cmd].finished, second.session.was_killed()),
            ),
            (
                true,
                Some((
                    "bash".to_owned(),
                    vec!["-c".to_owned(), "mytui -d 5".to_owned()],
                    PathBuf::from("/work"),
                    Some(("TERM".to_owned(), "xterm-256color".to_owned())),
                )),
                (2, 1, "mytui".to_owned()),
                term.map(View::Terminal),
                (true, RunKind::FullTerminal),
                (false, false),
            ),
        );
    }

    /// Back on the Run pane's tab, a Stub's header names its tab in the run
    /// color, its row names the tab, and its hint names the key that goes
    /// there. One bar in the run color runs beside the two rows.
    #[test]
    fn the_stub_names_its_tab() {
        let mut h = Stoat::test();
        let (cmd, _) = handed_off(&mut h);
        super::super::dispatch(&mut h.stoat, &stoat_action::ToggleTab);

        let (prompt, bars) = body_bars(&mut h);
        let run = style_rgb(h.stoat.theme.get(scope::UI_STATUSLINE_RUN).fg).expect("an RGB color");
        assert_eq!(
            (h.cmd_status(cmd), report_and_hint(&h, cmd), bars),
            (
                "\u{25cf} Terminal tab".to_owned(),
                (
                    Some(vec![
                        "needs a full terminal \u{b7} handed off to its own tab mytui".to_owned()
                    ]),
                    Some("Enter go to tab".to_owned()),
                ),
                vec![(prompt - 32, 32, run)],
            ),
        );
    }

    #[test]
    fn enter_on_a_stub_goes_to_its_tab() {
        let mut h = Stoat::test();
        let (_, term) = handed_off(&mut h);
        super::super::dispatch(&mut h.stoat, &stoat_action::ToggleTab);
        h.type_keys("enter");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.active_tab, ws.panes.pane(ws.panes.focus()).view.clone()),
            (1, View::Terminal(term)),
        );
    }

    /// `Ctrl-r` leaves a Stub whose tab lives alone, because its program runs
    /// on in that tab.
    #[test]
    fn ctrl_r_leaves_a_stub_with_a_live_tab_alone() {
        let mut h = Stoat::test();
        let (cmd, term) = handed_off(&mut h);
        super::super::dispatch(&mut h.stoat, &stoat_action::ToggleTab);
        let before = spawns(&h).len();
        h.type_keys("ctrl-r");

        let stub = &h.stoat.active_workspace().cmds[cmd];
        assert_eq!(
            (
                spawns(&h).len(),
                stub.handoff.as_ref().map(|handoff| handoff.term)
            ),
            (before, Some(term)),
        );
    }

    /// The end of the program of a tab that its terminal fills alone closes
    /// the tab and goes back to the Run pane's tab. The Stub reads that the
    /// tab closed, and its hint names the key that runs it again.
    #[test]
    fn a_handoff_tab_closes_when_its_program_ends() {
        let mut h = Stoat::test();
        let (cmd, term) = handed_off(&mut h);
        end_term(&mut h, term);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                (ws.tabs.len(), ws.active_tab, ws.terms.len()),
                h.cmd_status(cmd),
                report_and_hint(&h, cmd).1,
            ),
            (
                (1, 0, 0),
                "tab closed".to_owned(),
                Some("C-r run again".to_owned())
            ),
        );
    }

    /// The end of the program of a handoff tab that the workspace left closes
    /// that tab, and the Run pane's tab stays on show.
    #[test]
    fn a_parked_handoff_tab_closes_when_its_program_ends() {
        let mut h = Stoat::test();
        let (cmd, term) = handed_off(&mut h);
        super::super::dispatch(&mut h.stoat, &stoat_action::ToggleTab);
        end_term(&mut h, term);

        let ws = h.stoat.active_workspace();
        let run_shown = matches!(ws.panes.pane(ws.panes.focus()).view, View::Run(_));
        assert_eq!(
            (
                (ws.tabs.len(), ws.active_tab, ws.terms.len(), run_shown),
                h.cmd_status(cmd),
            ),
            ((1, 0, 0, true), "tab closed".to_owned()),
        );
    }

    /// A handoff tab that is the workspace's last tab stays when its program
    /// ends, and its pane turns into a scratch buffer, as a terminal's last
    /// pane does.
    ///
    /// The Stub sits in a Run pane of a dock, which outlives the tab that the
    /// Cmd ran in.
    #[test]
    fn a_handoff_tab_that_is_the_last_tab_stays() {
        let mut h = Stoat::test();
        let run = h.open_run();
        let cmd = h.submit_run("mytui -d 5").cmd;
        {
            let ws = h.stoat.active_workspace_mut();
            ws.docks.insert(dock(View::Run(run)));
            let focus = ws.panes.focus();
            ws.panes.pane_mut(focus).view = View::Label("left".into());
        }
        h.inject_cmd_output(cmd, FULL_SCREEN);
        let term = h.stoat.active_workspace().cmds[cmd]
            .handoff
            .as_ref()
            .expect("a handoff")
            .term;
        super::super::dispatch(&mut h.stoat, &stoat_action::ToggleTab);
        super::super::dispatch(&mut h.stoat, &stoat_action::CloseTab);
        end_term(&mut h, term);

        let ws = h.stoat.active_workspace();
        let scratch = matches!(ws.panes.pane(ws.panes.focus()).view, View::Editor(_));
        assert_eq!(
            ((ws.tabs.len(), ws.terms.len(), scratch), h.cmd_status(cmd)),
            ((1, 0, true), "tab closed".to_owned()),
        );
    }

    /// A handoff takes the focus from a dock, because the program needs the
    /// keys.
    #[test]
    fn a_handoff_takes_the_focus_from_a_dock() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = h.submit_run("mytui -d 5").cmd;
        {
            let ws = h.stoat.active_workspace_mut();
            let label = ws.docks.insert(dock(View::Label("dock".into())));
            ws.focus = FocusTarget::Dock(label);
        }
        h.inject_cmd_output(cmd, FULL_SCREEN);

        let ws = h.stoat.active_workspace();
        assert_eq!((ws.focus, ws.active_tab), (FocusTarget::SplitPane, 1));
    }

    /// A handoff lays its tab out before the next frame and asks for a
    /// redraw, and so does the end of its tab's program, since each moves the
    /// workspace to another tab.
    #[test]
    fn a_handoff_and_the_end_of_its_tab_ask_for_a_redraw() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = h.submit_run("mytui -d 5").cmd;
        let workspace = h.stoat.active_workspace;
        let handed = h.stoat.handle_pty_notification(PtyNotification::CmdOutput {
            workspace,
            cmd,
            run: 1,
            data: FULL_SCREEN.to_vec(),
        });
        let (term, area) = {
            let ws = h.stoat.active_workspace();
            let handoff = ws.cmds[cmd].handoff.as_ref().expect("a handoff");
            (handoff.term, ws.panes.pane(ws.panes.focus()).area)
        };
        let ended = h
            .stoat
            .handle_pty_notification(PtyNotification::TermExited {
                workspace,
                term_id: term,
            });

        assert_eq!(
            (handed, ended, area.is_empty()),
            (UpdateEffect::Redraw, UpdateEffect::Redraw, false),
        );
    }

    /// A handoff whose spawn fails ends the Cmd with its error line, and no
    /// tab opens.
    #[test]
    fn a_failed_handoff_ends_the_cmd_with_its_error() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = h.submit_run("mytui -d 5").cmd;
        h.fake_terminal_host()
            .fail_next_spawn(io::ErrorKind::NotFound);
        h.inject_cmd_output(cmd, FULL_SCREEN);

        let ws = h.stoat.active_workspace();
        let failed = &ws.cmds[cmd];
        assert_eq!(
            (
                failed.finished,
                failed.error.as_deref(),
                failed.handoff.is_none(),
                ws.tabs.len(),
            ),
            (true, Some("handoff failed"), true, 1),
        );
    }

    /// A handoff tab with a second pane stays when its program ends, and
    /// keeps that pane.
    #[test]
    fn a_handoff_tab_with_a_second_pane_stays() {
        let mut h = Stoat::test();
        let (_, term) = handed_off(&mut h);
        super::super::dispatch(&mut h.stoat, &stoat_action::SplitNewRight);
        end_term(&mut h, term);

        let ws = h.stoat.active_workspace();
        let editors: Vec<bool> = ws
            .panes
            .split_panes()
            .map(|(_, pane)| matches!(pane.view, View::Editor(_)))
            .collect();
        assert_eq!((ws.tabs.len(), ws.active_tab, editors), (2, 1, vec![true]));
    }

    /// `Ctrl-r` on a Stub whose tab closed runs its line in its block again,
    /// as a Line Cmd with no handoff.
    #[test]
    fn ctrl_r_on_a_closed_stub_runs_the_line_again() {
        let mut h = Stoat::test();
        let (cmd, term) = handed_off(&mut h);
        end_term(&mut h, term);
        h.fake_terminal_host()
            .push_session(Arc::new(FakeTerminalSession::new()));
        h.type_keys("ctrl-r");

        let rerun = &h.stoat.active_workspace().cmds[cmd];
        let last = spawns(&h).pop().map(|(_, args, _, var)| (args, var));
        assert_eq!(
            (
                last,
                rerun.run_count,
                rerun.kind,
                rerun.handoff.is_none(),
                rerun.finished,
            ),
            (
                Some((
                    vec!["-c".to_owned(), "mytui -d 5".to_owned()],
                    Some(("TERM".to_owned(), "dumb".to_owned())),
                )),
                2,
                RunKind::Line,
                true,
                false,
            ),
        );
    }

    /// A line whose program is in the full-terminal list hands off at submit,
    /// so its one run is the one in the new terminal tab, and no run goes under
    /// `TERM=dumb`. Back on the Run pane's tab, its Stub names the tab.
    #[test]
    fn a_listed_program_hands_off_at_submit_with_no_first_run() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        let run = h.open_run();
        let cmd = h.submit_run("htop -d 5").cmd;

        let ws = h.stoat.active_workspace();
        let stub = &ws.cmds[cmd];
        let term = stub.handoff.as_ref().map(|handoff| handoff.term);
        let tab = (
            (ws.tabs.len(), ws.active_tab, ws.tab_title(1)),
            ws.panes.pane(ws.panes.focus()).view.clone(),
        );
        let stack = (ws.runs[run].blocks.clone(), stub.finished, stub.kind);
        let spawned = spawns(&h);
        super::super::dispatch(&mut h.stoat, &stoat_action::ToggleTab);

        assert_eq!(
            (
                spawned,
                tab,
                stack,
                h.cmd_status(cmd),
                report_and_hint(&h, cmd)
            ),
            (
                vec![(
                    "bash".to_owned(),
                    vec!["-c".to_owned(), "htop -d 5".to_owned()],
                    PathBuf::from("/work"),
                    Some(("TERM".to_owned(), "xterm-256color".to_owned())),
                )],
                (
                    (2, 1, "htop".to_owned()),
                    View::Terminal(term.expect("a handoff"))
                ),
                (vec![cmd], true, RunKind::FullTerminal),
                "\u{25cf} Terminal tab".to_owned(),
                (
                    Some(vec![
                        "needs a full terminal \u{b7} handed off to its own tab htop".to_owned()
                    ]),
                    Some("Enter go to tab".to_owned()),
                ),
            ),
        );
    }

    /// A line whose program is outside the list runs as a Cmd under
    /// `TERM=dumb`.
    #[test]
    fn a_program_outside_the_list_runs_as_a_cmd() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.open_run();
        h.submit_run("ls");

        assert_eq!(spawns(&h), vec![bash_in("ls", "/work")]);
    }

    /// A list of the setting takes the place of the default list, so its
    /// program hands off and a program of the default list runs as a Cmd.
    #[test]
    fn the_setting_replaces_the_default_list() {
        let mut h = Stoat::test();
        h.stoat.settings.run_full_terminal = Some(vec!["mytui".to_owned()]);
        h.open_run();
        let listed = h.submit_run("mytui").cmd;
        super::super::dispatch(&mut h.stoat, &stoat_action::ToggleTab);
        let unlisted = h.submit_run("htop").cmd;

        let ws = h.stoat.active_workspace();
        let read = |cmd: CmdId| (ws.cmds[cmd].kind, ws.cmds[cmd].handoff.is_some());
        assert_eq!(
            [read(listed), read(unlisted)],
            [(RunKind::FullTerminal, true), (RunKind::Line, false)],
        );
    }

    /// An empty list of the setting turns the rule off, so a program of the
    /// default list runs as a Cmd.
    #[test]
    fn an_empty_list_turns_the_rule_off() {
        let mut h = Stoat::test();
        h.stoat.settings.run_full_terminal = Some(Vec::new());
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.open_run();
        h.submit_run("htop");

        assert_eq!(spawns(&h), vec![bash_in("htop", "/work")]);
    }

    /// The `run` command with a listed program leaves its Stub on a new Run
    /// pane under the editor pane, and the terminal tab is active.
    #[test]
    fn the_run_command_hands_a_listed_program_off() {
        let mut h = Stoat::test();
        dispatch_run(&mut h, "htop");
        h.stoat.drive_background();

        let ws = h.stoat.active_workspace();
        let tab = (
            ws.tabs.len(),
            ws.active_tab,
            matches!(ws.panes.pane(ws.panes.focus()).view, View::Terminal(_)),
        );
        super::super::dispatch(&mut h.stoat, &stoat_action::ToggleTab);
        let ws = h.stoat.active_workspace();
        let lower = ws
            .panes
            .split_panes()
            .max_by_key(|(_, pane)| pane.area.y)
            .map(|(_, pane)| match pane.view {
                View::Run(run) => ws.runs[run]
                    .blocks
                    .iter()
                    .map(|cmd| ws.cmds[*cmd].handoff.is_some())
                    .collect(),
                _ => Vec::new(),
            });
        assert_eq!((tab, lower), ((2, 1, true), Some(vec![true])));
    }

    /// A config command whose run is a listed program hands it off from the
    /// palette, which closes, and leaves no Cmd, since a Stub is a row of a
    /// Run pane.
    #[test]
    fn a_config_command_hands_its_listed_program_off() {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(r#"command top = cli("htop");"#);
        h.type_text(":top");
        h.type_keys("enter enter");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                ws.cmds.len(),
                (ws.tabs.len(), ws.active_tab)
            ),
            (true, 0, (2, 1)),
        );
    }

    /// A Cmd that has not finished and holds no process waits on no hand-off
    /// unless its start found a program that needs a full terminal.
    #[test]
    fn only_a_cmd_marked_at_its_start_hands_off() {
        let mut h = Stoat::test();
        let run = h.open_run();
        let cmd = {
            let ws = h.stoat.active_workspace_mut();
            let id = ws
                .cmds
                .insert(Cmd::new("htop".to_owned(), PathBuf::from("/work"), 80));
            ws.runs[run].blocks.push(id);
            id
        };
        h.stoat.drive_background();

        let ws = h.stoat.active_workspace();
        assert_eq!((ws.tabs.len(), ws.cmds[cmd].handoff.is_none()), (1, true));
    }

    /// `Ctrl-r` on the Stub of a listed program whose tab closed hands the
    /// line off again, with no run under `TERM=dumb`.
    #[test]
    fn ctrl_r_on_a_closed_stub_hands_off_again() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = h.submit_run("htop").cmd;
        let term = h.stoat.active_workspace().cmds[cmd]
            .handoff
            .as_ref()
            .expect("a handoff")
            .term;
        end_term(&mut h, term);
        h.type_keys("ctrl-r");

        let terms: Vec<Option<(String, String)>> =
            spawns(&h).into_iter().map(|(_, _, _, var)| var).collect();
        let xterm = Some(("TERM".to_owned(), "xterm-256color".to_owned()));
        assert_eq!(terms, [xterm.clone(), xterm]);
    }

    /// A file opened at the Run prompt takes the pane, and the Run pane ends as
    /// a pane close ends it. The running Cmd's process is killed, and the run
    /// and its Cmds leave the workspace.
    #[test]
    fn opening_a_file_at_the_run_prompt_ends_the_run_pane() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.fake_fs().insert_file("/work/a.rs", b"fn a() {}\n");
        let run = h.open_run();
        let finished = h.submit_run("true").cmd;
        h.inject_cmd_done(finished, 0);
        let running = h.submit_run("sleep 9");
        submit_ending(&mut h, run, "o a.rs");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                editor::focused_buffer_path(&h.stoat),
                (ws.runs.len(), ws.cmds.len()),
                running.session.was_killed(),
            ),
            (PathBuf::from("/work/a.rs"), (0, 0), true),
        );
    }

    /// A Run pane that a second pane shows stays when a file takes one of the
    /// two panes, and the other pane shows its stack.
    #[test]
    fn a_run_that_a_second_pane_shows_stays() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.fake_fs().insert_file("/work/a.rs", b"fn a() {}\n");
        let run = h.open_run();
        let cmd = h.submit_run("true").cmd;
        super::super::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        submit_ending(&mut h, run, "o a.rs");

        let ws = h.stoat.active_workspace();
        let shown: Vec<RunId> = ws
            .panes
            .split_panes()
            .filter_map(|(_, pane)| match pane.view {
                View::Run(id) => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(
            (shown, ws.runs[run].blocks.clone(), ws.cmds.len()),
            (vec![run], vec![cmd], 1),
        );
    }

    /// A pane whose terminal covers a Run pane closes with the run, since no
    /// other pane shows it.
    #[test]
    fn closing_a_pane_whose_terminal_covers_a_run_pane_ends_that_run() {
        let mut h = Stoat::test();
        super::super::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        h.open_run();
        h.submit_run("true");
        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);
        super::super::dispatch(&mut h.stoat, &stoat_action::ClosePane);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.panes.split_panes().count(), ws.runs.len(), ws.cmds.len()),
            (1, 0, 0),
        );
    }

    /// A pane whose terminal covers a Run pane that a second pane shows closes
    /// without the run, which the second pane keeps.
    #[test]
    fn closing_a_pane_whose_terminal_covers_a_shown_run_keeps_the_run() {
        let mut h = Stoat::test();
        let run = h.open_run();
        let cmd = h.submit_run("true").cmd;
        super::super::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);
        super::super::dispatch(&mut h.stoat, &stoat_action::ClosePane);

        let ws = h.stoat.active_workspace();
        let shown: Vec<RunId> = ws
            .panes
            .split_panes()
            .filter_map(|(_, pane)| match pane.view {
                View::Run(id) => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!((shown, ws.runs[run].blocks.clone()), (vec![run], vec![cmd]));
    }

    /// A tab whose terminal covers a Run pane that another tab shows closes
    /// without the run.
    #[test]
    fn closing_a_tab_keeps_a_covered_run_that_another_tab_shows() {
        let mut h = Stoat::test();
        let run = h.open_run();
        let cmd = h.submit_run("true").cmd;
        super::super::dispatch(&mut h.stoat, &stoat_action::NewTab);
        let ws = h.stoat.active_workspace_mut();
        let focused = ws.panes.focus();
        ws.panes.pane_mut(focused).view = View::Run(run);
        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);
        super::super::dispatch(&mut h.stoat, &stoat_action::CloseTab);

        let ws = h.stoat.active_workspace();
        assert_eq!((ws.tabs.len(), ws.runs[run].blocks.clone()), (1, vec![cmd]));
    }

    /// A tab whose terminal covers a Run pane closes with the run.
    #[test]
    fn closing_a_tab_ends_the_run_that_its_terminal_covers() {
        let mut h = Stoat::test();
        super::super::dispatch(&mut h.stoat, &stoat_action::NewTab);
        h.open_run();
        h.submit_run("true");
        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);
        super::super::dispatch(&mut h.stoat, &stoat_action::CloseTab);

        let ws = h.stoat.active_workspace();
        assert_eq!((ws.tabs.len(), ws.runs.len(), ws.cmds.len()), (1, 0, 0));
    }

    /// Submit `text` at the prompt of the run `run` of `h`, as Enter does, for
    /// a line whose run can end the run, which the harness's submit reads
    /// after it.
    fn submit_ending(h: &mut TestHarness, run: RunId, text: &str) {
        let ws = h.stoat.active_workspace_mut();
        let input = ws.runs[run].input.clone();
        input.replace_text(ws, text);
        super::super::dispatch(&mut h.stoat, &stoat_action::RunSubmit);
    }

    /// The exit of the killed process, which arrives after the handoff,
    /// leaves the Stub as it is.
    #[test]
    fn a_late_exit_leaves_a_stub_as_it_is() {
        let mut h = Stoat::test();
        let (cmd, term) = handed_off(&mut h);
        let ended = h.stoat.active_workspace().cmds[cmd].ended;
        h.advance_clock(Duration::from_secs(3));
        h.inject_cmd_done(cmd, 0);

        let stub = &h.stoat.active_workspace().cmds[cmd];
        assert_eq!(
            (
                stub.exit_status,
                stub.ended,
                stub.handoff.as_ref().map(|handoff| handoff.term),
                h.cmd_status(cmd),
            ),
            (None, ended, Some(term), "\u{25cf} Terminal tab".to_owned()),
        );
    }

    /// A Cmd of the palette's Run Block that asks for the alternate screen
    /// hands off too. The palette closes with its input, and the Cmd leaves
    /// the store.
    #[test]
    fn a_palette_block_that_goes_full_screen_closes_the_palette_and_leaves_no_cmd() {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(TUI);
        let (cmd, session) = enter_command(&mut h, "tui");
        let input = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette is open")
            .input
            .editor_id;
        h.inject_cmd_output(cmd, FULL_SCREEN);

        let ws = h.stoat.active_workspace();
        let shown = matches!(ws.panes.pane(ws.panes.focus()).view, View::Terminal(_));
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                ws.editors.contains_key(input),
                ws.cmds.len(),
                session.was_killed(),
                (ws.active_tab, shown),
            ),
            (true, false, 0, true, (1, true)),
        );
    }

    /// The tab that a palette Cmd handed off to closes when its program ends,
    /// though no Stub names it, and the workspace goes back to the tab it
    /// left.
    #[test]
    fn a_palette_handoff_tab_closes_when_its_program_ends() {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(TUI);
        let (cmd, _) = enter_command(&mut h, "tui");
        h.inject_cmd_output(cmd, FULL_SCREEN);
        let term = {
            let ws = h.stoat.active_workspace();
            let View::Terminal(term) = ws.panes.pane(ws.panes.focus()).view else {
                panic!("the handoff tab shows a terminal");
            };
            term
        };
        end_term(&mut h, term);

        let ws = h.stoat.active_workspace();
        assert_eq!((ws.tabs.len(), ws.active_tab, ws.terms.len()), (1, 0, 0));
    }

    /// A `claude` line at the Run pane prompt starts no Cmd process. The line
    /// runs through `bash -c` with the agent's environment in a new tab, which
    /// shows the agent and takes the focus, and the Run pane's stack holds a
    /// Stub of the kind `Agent`.
    #[test]
    fn a_claude_line_starts_in_an_agent_tab_with_its_arguments() {
        let mut h = Stoat::test();
        let (run, cmd) = agent_stub(&mut h);

        let spawns = h
            .fake_terminal_host()
            .spawns()
            .into_iter()
            .map(|spawn| {
                let keys: Vec<String> = spawn.env.into_iter().map(|(key, _)| key).collect();
                (spawn.program, spawn.args, keys)
            })
            .collect::<Vec<_>>();
        let ws = h.stoat.active_workspace();
        let stub = &ws.cmds[cmd];
        assert_eq!(
            (
                spawns,
                (ws.tabs.len(), ws.active_tab),
                Some(ws.panes.pane(ws.panes.focus()).view.clone()),
                ws.runs[run].blocks.clone(),
                stub.kind,
            ),
            (
                vec![(
                    "bash".to_owned(),
                    vec!["-c".to_owned(), CLAUDE_LINE.to_owned()],
                    vec!["EDITOR".to_owned(), "VISUAL".to_owned()],
                )],
                (2, 1),
                stub.handoff
                    .as_ref()
                    .map(|handoff| View::Agent(handoff.term)),
                vec![cmd],
                RunKind::Agent,
            ),
        );
    }

    /// An agent's Stub names its tab as an agent's, and the tab goes by
    /// `claude` until the agent sets a title.
    #[test]
    fn an_agent_stub_names_its_tab() {
        let mut h = Stoat::test();
        let (_, cmd) = agent_stub(&mut h);

        let ws = h.stoat.active_workspace();
        let rows = ws.cmds[cmd]
            .report_rows()
            .map(|rows| rows.iter().map(BodyRow::text).collect::<Vec<_>>());
        assert_eq!(
            (h.cmd_status(cmd), rows, ws.tab_title(1)),
            (
                "\u{25cf} Agent tab".to_owned(),
                Some(vec![
                    "runs as an agent \u{b7} handed off to its own tab claude".to_owned()
                ]),
                "claude".to_owned(),
            ),
        );
    }

    /// The end of an agent's session leaves its tab on the agent's last
    /// frame, so the Stub still names the tab.
    #[test]
    fn an_agent_stub_outlives_its_session() {
        let mut h = Stoat::test();
        let (_, cmd) = agent_stub(&mut h);
        let term = h.stoat.active_workspace().cmds[cmd]
            .handoff
            .as_ref()
            .expect("a handoff")
            .term;
        end_term(&mut h, term);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                (ws.tabs.len(), ws.active_tab),
                ws.panes.pane(ws.panes.focus()).view.clone(),
                h.cmd_status(cmd),
            ),
            ((2, 1), View::Agent(term), "\u{25cf} Agent tab".to_owned()),
        );
    }

    /// `Ctrl-r` on an agent's Stub whose tab closed starts the agent again in
    /// a new tab, from the line and with no `TERM=dumb`. The Stub keeps its
    /// place and names the new tab.
    #[test]
    fn ctrl_r_on_a_closed_agent_stub_starts_the_agent_again() {
        let mut h = Stoat::test();
        let (run, cmd) = agent_stub(&mut h);
        super::super::dispatch(&mut h.stoat, &stoat_action::CloseTab);
        let closed = h.cmd_status(cmd);
        h.type_keys("ctrl-r");

        let spawns = h.fake_terminal_host().spawns();
        let dumb = ("TERM".to_owned(), "dumb".to_owned());
        let line = (
            "bash".to_owned(),
            vec!["-c".to_owned(), CLAUDE_LINE.to_owned()],
        );
        let ws = h.stoat.active_workspace();
        let stub = &ws.cmds[cmd];
        assert_eq!(
            (
                closed,
                spawns
                    .iter()
                    .map(|spawn| (spawn.program.clone(), spawn.args.clone()))
                    .collect::<Vec<_>>(),
                spawns.iter().any(|spawn| spawn.env.contains(&dumb)),
                (ws.tabs.len(), ws.active_tab),
                Some(ws.panes.pane(ws.panes.focus()).view.clone()),
                ws.runs[run].blocks.clone(),
                h.cmd_status(cmd),
            ),
            (
                "tab closed".to_owned(),
                vec![line.clone(), line],
                false,
                (2, 1),
                stub.handoff
                    .as_ref()
                    .map(|handoff| View::Agent(handoff.term)),
                vec![cmd],
                "\u{25cf} Agent tab".to_owned(),
            ),
        );
    }

    /// An agent's Stub joins its stack as a submitted line's Cmd does. The
    /// stack snaps back to its prompt, and its trim drops the full finished
    /// block that the Stub moves out of the newest place.
    ///
    /// The running Cmd's rows hold the stack over its cap, so the trim that
    /// the full block's own output ran had nothing it could drop.
    #[test]
    fn a_claude_line_joins_its_stack_as_a_submitted_line_does() {
        let mut h = Stoat::test();
        let run = h.open_run();
        let running = h.submit_run("sleep 9").cmd;
        h.inject_cmd_output(running, "x\r\n".repeat(600).as_bytes());
        let full = h.submit_run("seq 10000").cmd;
        h.inject_cmd_output(full, "x\r\n".repeat(10_000).as_bytes());
        h.inject_cmd_done(full, 0);
        h.stoat.active_workspace_mut().runs[run].scroll_offset = 3;
        let stub = h.submit_run(CLAUDE_LINE).cmd;

        let run_state = &h.stoat.active_workspace().runs[run];
        assert_eq!(
            (run_state.blocks.clone(), run_state.scroll_offset),
            (vec![running, stub], 0),
        );
    }

    /// An agent handoff without a Stub opens the agent's tab and leaves the
    /// Run pane's stack as it was.
    #[test]
    fn an_agent_handoff_without_a_stub_opens_its_tab_alone() {
        let mut h = Stoat::test();
        let run = h.open_run();
        super::hand_off_agent(&mut h.stoat, CLAUDE_LINE, false);

        let ws = h.stoat.active_workspace();
        let shown = matches!(ws.panes.pane(ws.panes.focus()).view, View::Agent(_));
        assert_eq!(
            (
                ws.cmds.len(),
                ws.runs[run].blocks.len(),
                (ws.tabs.len(), shown)
            ),
            (0, 0, (2, true)),
        );
    }

    /// A `claude` line whose agent fails to start reports it, and leaves no
    /// Stub and no tab.
    #[test]
    fn a_claude_line_that_fails_to_start_reports_it() {
        let mut h = Stoat::test();
        let run = h.open_run();
        h.fake_terminal_host()
            .fail_next_spawn(io::ErrorKind::NotFound);
        h.submit_run(CLAUDE_LINE);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.pending_message.as_deref(),
                ws.runs[run].blocks.len(),
                ws.tabs.len(),
            ),
            (Some("claude did not start"), 0, 1),
        );
    }

    /// A line whose head names a command runs that command at the prompt and
    /// leaves no block.
    #[test]
    fn a_resolved_command_runs_at_the_run_prompt() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        let run = h.open_run();
        h.submit_run("pwd");

        assert_eq!(
            (
                h.stoat.pending_message.as_deref(),
                spawns(&h).len(),
                h.stoat.active_workspace().runs[run].blocks.len(),
            ),
            (Some("Current working directory is /work"), 0, 0),
        );
    }

    /// A command that takes no argument runs as a shell line when text follows
    /// its name, so the shell's own program runs.
    #[test]
    fn a_zero_argument_command_with_arguments_runs_as_a_shell_line() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.open_run();
        h.submit_run("pwd -P");

        assert_eq!(spawns(&h), vec![bash_in("pwd -P", "/work")]);
    }

    /// An argument that does not parse reports in the status line, runs
    /// nothing, and leaves the line at the prompt and out of the history.
    #[test]
    fn a_bad_argument_reports_and_keeps_the_prompt() {
        let mut h = Stoat::test();
        let run = h.open_run();
        h.submit_run("tab x");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.pending_message.as_deref(),
                ws.runs[run].input.text(ws),
                ws.palette_history.entries().len(),
                (spawns(&h).len(), ws.tabs.len()),
            ),
            (
                Some("tab: cannot parse `x` as number"),
                "tab x".to_owned(),
                0,
                (0, 1)
            ),
        );
    }

    /// A bare `cd` goes to the home directory, as a shell's does.
    #[test]
    fn a_bare_cd_goes_home() {
        let mut h = Stoat::test();
        h.fake_env().set("HOME", "/home/u");
        h.fake_fs().insert_file("/home/u/a.txt", b"");
        h.open_run();
        h.submit_run("cd");

        assert_eq!(
            h.stoat.active_workspace().git_root,
            PathBuf::from("/home/u")
        );
    }

    /// A config command at the prompt joins the Run pane's stack with its
    /// Source Tag. Its typed line is the Cmd's line, and its shell line is the
    /// run's first row, in the muted color.
    #[test]
    fn a_config_command_joins_the_stack_with_its_tag() {
        let mut h = with_hello();
        let run = h.open_run();
        let cmd = h.submit_run("hello").cmd;

        let ws = h.stoat.active_workspace();
        let block = &ws.cmds[cmd];
        let row: Vec<(char, Option<Color>)> = block
            .grid
            .row(0)
            .iter()
            .map(|cell| (cell.ch, cell.fg))
            .collect();
        let muted = h.stoat.theme.get(scope::UI_TEXT_MUTED).fg;
        assert_eq!(
            (
                ws.runs[run].blocks.clone(),
                (block.command.as_str(), block.shell.as_deref()),
                block.source.map(|(label, _)| label),
                h.cmd_status(cmd),
                row,
            ),
            (
                vec![cmd],
                ("hello", Some("echo hi")),
                Some("config"),
                "config \u{25cf} 0:00".to_owned(),
                "echo hi".chars().map(|ch| (ch, muted)).collect(),
            ),
        );
    }

    /// A `cli` command at the prompt takes the rest of the line as its tool's
    /// tail.
    #[test]
    fn a_cli_command_takes_the_rest_of_the_line() {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(r#"command gh = cli("gh");"#);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.open_run();
        h.submit_run("gh pr list");

        assert_eq!(spawns(&h), vec![bash_in("gh pr list", "/work")]);
    }

    /// A config command whose argument with no default gets no value reports
    /// it, and the line stays at the prompt.
    #[test]
    fn a_config_command_with_a_missing_argument_reports_it() {
        let mut h = Stoat::test();
        h.stoat
            .reload_user_config(r#"command say { arg what = text; run = shell("echo {what}"); }"#);
        let run = h.open_run();
        h.submit_run("say");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.pending_message.as_deref(),
                ws.runs[run].input.text(ws),
                spawns(&h).len(),
            ),
            (Some("say: what is missing"), "say".to_owned(), 0),
        );
    }

    /// A bare `claude` line stays an Agent line, though `claude` names a
    /// built-in command, so the Run pane keeps its place and gains a Stub.
    #[test]
    fn a_bare_claude_line_stays_an_agent_line() {
        let mut h = Stoat::test();
        let run = h.open_run();
        h.submit_run("claude");

        let ws = h.stoat.active_workspace();
        let first = ws.tabs[0].parked.as_ref().expect("the first tab is parked");
        let shown = matches!(ws.panes.pane(ws.panes.focus()).view, View::Agent(_));
        let stack: Vec<(RunKind, bool)> = ws.runs[run]
            .blocks
            .iter()
            .map(|id| (ws.cmds[*id].kind, ws.cmds[*id].handoff.is_some()))
            .collect();
        assert_eq!(
            (
                (ws.tabs.len(), ws.active_tab, shown),
                first.pane(first.focus()).view.clone(),
                stack,
            ),
            ((2, 1, true), View::Run(run), vec![(RunKind::Agent, true)]),
        );
    }

    /// A shell line from a Run pane joins the palette's history behind the
    /// head of the `run` command, with the time it ran.
    #[test]
    fn a_shell_line_joins_the_palette_history_as_a_run_command() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("ls -la");

        let history = &h.stoat.active_workspace().palette_history;
        assert_eq!(
            (history.entries().to_vec(), history.times().to_vec(),),
            (
                vec!["r ls -la".to_owned()],
                vec![Some(commit_picker::now_epoch(&h.stoat.executor))],
            ),
        );
    }

    /// The palette's line for a shell line recalls bare at the Run prompt,
    /// and so does a line behind the `run` command's name, which the walk goes
    /// on past. A `run` line whose own head names a command keeps the `run`
    /// head, since the bare line would run that command.
    #[test]
    fn a_palette_run_line_recalls_bare_at_the_run_prompt() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().palette_history = InputHistory::from_entries(
            ["r ls -la", "r pwd", "run make"]
                .map(str::to_owned)
                .to_vec(),
        );
        h.open_run();
        let walk = [(); 3].map(|()| {
            h.type_keys("up");
            focused_prompt_text(&h)
        });

        assert_eq!(walk, ["make", "r pwd", "ls -la"]);
    }

    /// A submit ends the walk, so the next Up starts a fresh walk from the
    /// newest line, even over the recalled line typed again.
    #[test]
    fn a_submit_ends_the_walk() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("make");
        h.submit_run("ls -la");
        h.type_keys("up enter");
        h.type_text("ls -la");
        h.type_keys("up");

        assert_eq!(focused_prompt_text(&h), "ls -la");
    }

    /// A walk that steps past the newest line ends and writes back the typed
    /// text as typed, even text that reads as a stored `run` line, and the
    /// prompt names no recall.
    #[test]
    fn a_walk_that_ends_restores_the_typed_text_and_names_no_recall() {
        let mut h = Stoat::test();
        let run = h.open_run();
        h.submit_run("ls -la");
        h.type_text("r ls");
        h.type_keys("up");
        let recalled = focused_prompt_text(&h);
        h.type_keys("down");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                recalled,
                focused_prompt_text(&h),
                ws.runs[run].prompt.recalled
            ),
            ("ls -la".to_owned(), "r ls".to_owned(), false),
        );
    }

    /// A line whose head names a command is stored and recalled as typed.
    #[test]
    fn a_resolved_command_is_stored_and_recalled_as_typed() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.fake_fs().insert_file("/work/sub/a.txt", b"");
        h.open_run();
        h.submit_run("cd sub");
        h.type_keys("up");

        assert_eq!(
            (
                h.stoat
                    .active_workspace()
                    .palette_history
                    .entries()
                    .to_vec(),
                focused_prompt_text(&h),
            ),
            (vec!["cd sub".to_owned()], "cd sub".to_owned()),
        );
    }

    /// Two Run panes walk one history. A walk from the second pane recalls
    /// the first pane's line, and the first pane, whose walk ended, names no
    /// recall.
    #[test]
    fn two_run_panes_walk_one_history() {
        let mut h = Stoat::test();
        let first = h.open_run();
        h.submit_run("ls -la");
        h.type_keys("up");
        super::super::dispatch(&mut h.stoat, &stoat_action::SplitNewRight);
        let second = h.open_run();
        h.type_keys("up");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                focused_prompt_text(&h),
                [first, second].map(|run| ws.runs[run].prompt.recalled),
            ),
            ("ls -la".to_owned(), [false, true]),
        );
    }

    /// A recalled line left as it was walks on, so a second Up reaches the
    /// older line.
    #[test]
    fn a_walk_goes_on_through_the_prompt_form() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("ls -la");
        h.submit_run("make test");
        h.type_keys("up");
        let newest = focused_prompt_text(&h);
        h.type_keys("up");

        assert_eq!(
            (newest, focused_prompt_text(&h)),
            ("make test".to_owned(), "ls -la".to_owned()),
        );
    }

    /// A recalled line names the walk at the right end of the prompt row in
    /// the muted color, and an edit of the line drops the name.
    #[test]
    fn the_prompt_names_a_recalled_line() {
        let mut h = Stoat::test();
        let run = h.open_run();
        h.submit_run("ls -la");
        let read = |h: &TestHarness| {
            let (runs, _) = row_scene(h, focused_prompt_row(h));
            (h.stoat.active_workspace().runs[run].prompt.recalled, runs)
        };
        h.type_keys("up");
        let recalled = read(&h);
        h.type_text("x");

        let muted = rgb(h.stoat.theme.get(scope::UI_TEXT_MUTED).fg);
        assert_eq!(
            (recalled, read(&h)),
            (
                (true, vec![(RECALL_NOTE.to_owned(), muted)]),
                (false, vec![])
            ),
        );
    }

    /// A palette that opens over a Run pane's walk ends that walk on its first
    /// frame and lists its commands.
    #[test]
    fn a_palette_that_opens_over_a_run_pane_walk_shows_its_command_list() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("ls -la");
        h.type_keys("up");
        let walking = h.stoat.active_workspace().palette_history.is_walking();
        h.type_keys("escape :");

        let palette = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette is open");
        assert_eq!(
            (
                walking,
                palette.filtered.is_empty(),
                h.stoat.active_workspace().palette_history.is_walking(),
            ),
            (true, false, false),
        );
    }

    /// A Run pane's line saves with the workspace and restores with its time.
    #[test]
    fn a_run_pane_line_survives_a_restart() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("ls -la");
        let exec = h.stoat.executor.clone();
        let path = PathBuf::from("/state/workspace.ron");
        let fs = h.fake_fs().clone();
        h.stoat
            .active_workspace()
            .save_state(&path, &*fs)
            .expect("the state saves");

        let mut fresh = Workspace::new(PathBuf::from("/work"), &exec, crate::test_notify());
        fresh
            .restore_state(&path, &*fs, &exec)
            .expect("the state restores");
        let history = &h.stoat.active_workspace().palette_history;
        assert_eq!(
            (
                fresh.palette_history.entries(),
                fresh.palette_history.times()
            ),
            (history.entries(), history.times()),
        );
    }

    /// The text at the focused Run pane's prompt.
    fn focused_prompt_text(h: &TestHarness) -> String {
        let ws = h.stoat.active_workspace();
        let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the focused pane shows a Run pane");
        };
        ws.runs[run].input.text(ws)
    }

    /// Submit [`CLAUDE_LINE`] at a new Run pane's prompt, and answer the run
    /// and the Stub that the line leaves.
    fn agent_stub(h: &mut TestHarness) -> (RunId, CmdId) {
        let run = h.open_run();
        let cmd = h.submit_run(CLAUDE_LINE).cmd;
        (run, cmd)
    }

    /// Run `mytui -d 5` in a new Run pane and have its output ask for the
    /// alternate screen, and answer the Cmd and the session of its tab.
    fn handed_off(h: &mut TestHarness) -> (CmdId, TermId) {
        h.open_run();
        let cmd = h.submit_run("mytui -d 5").cmd;
        h.inject_cmd_output(cmd, FULL_SCREEN);
        let term = h.stoat.active_workspace().cmds[cmd]
            .handoff
            .as_ref()
            .expect("a handoff")
            .term;
        (cmd, term)
    }

    /// An open dock on the right that shows `view`.
    fn dock(view: View) -> DockPanel {
        DockPanel {
            view,
            side: DockSide::Right,
            visibility: DockVisibility::Open { width: 10 },
            default_width: 10,
            area: Default::default(),
        }
    }

    /// End the session `term` of the active workspace, as its reader does at
    /// the PTY's end.
    fn end_term(h: &mut TestHarness, term: TermId) {
        h.stoat
            .handle_pty_notification(PtyNotification::TermExited {
                workspace: h.stoat.active_workspace,
                term_id: term,
            });
    }

    /// The bars 4 sixteenths wide at the left edge of the focused Run pane's
    /// stack, after the top edge of the pane's prompt row.
    fn body_bars(h: &mut TestHarness) -> (i16, Vec<BodyBar>) {
        h.render_composited();
        let area = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let x = (crate::run::stack_rect(area).x * 16) as i16;
        let bars = command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Bar(bar) if bar.width == 4 && bar.x == x => {
                    Some((bar.y, bar.height, bar.color))
                },
                _ => None,
            })
            .collect();
        (((area.bottom() - 1) * 16) as i16, bars)
    }

    /// The `run` command over one editor pane starts its line as a Cmd on a
    /// new Run pane that splits the editor pane downward, its grid as wide as
    /// the pane less the stack's inset. The editor pane keeps the focus, and
    /// no modal opens.
    #[test]
    fn the_run_command_appends_its_cmd_to_the_run_pane() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        let editor = h.stoat.active_workspace().panes.focus();
        dispatch_run(&mut h, "ls");

        let ws = h.stoat.active_workspace();
        let (lower, shown) = ws
            .panes
            .split_panes()
            .max_by_key(|(_, pane)| pane.area.y)
            .expect("a split pane");
        let View::Run(run) = shown.view else {
            panic!("the lower pane shows a Run pane");
        };
        assert_eq!(
            (
                spawns(&h),
                run_stacks(&h),
                ws.cmds[ws.runs[run].blocks[0]].grid.width() + 3,
                (lower != editor, ws.panes.focus()),
                keymap_state::active_modal(&h.stoat),
            ),
            (
                vec![bash_in("ls", "/work")],
                vec![vec!["ls".to_owned()]],
                shown.area.width,
                (true, editor),
                None,
            ),
        );
    }

    /// The `run` command puts its Cmd on the stack of the Run pane on show,
    /// no pane opens, and the focus stays on the editor pane.
    #[test]
    fn the_run_command_uses_the_run_pane_that_shows() {
        let mut h = Stoat::test();
        super::super::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        h.open_run();
        h.submit_run("make");
        super::super::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
        let editor = h.stoat.active_workspace().panes.focus();
        dispatch_run(&mut h, "ls");

        assert_eq!(
            (run_stacks(&h), h.stoat.active_workspace().panes.focus()),
            (vec![vec!["make".to_owned(), "ls".to_owned()]], editor),
        );
    }

    /// `:run ls` and Enter closes the palette and runs the line on a Run
    /// pane's stack.
    #[test]
    fn the_run_command_from_the_palette_closes_the_palette() {
        let mut h = Stoat::test();
        h.type_text(":run ls");
        h.type_keys("enter");

        assert_eq!(
            (h.stoat.command_palette.is_none(), run_stacks(&h)),
            (true, vec![vec!["ls".to_owned()]]),
        );
    }

    /// `run ls` at a Run pane prompt puts the Cmd on that pane's stack.
    #[test]
    fn run_at_the_run_prompt_appends_to_that_pane() {
        let mut h = Stoat::test();
        h.open_run();
        let ran = h.submit_run("run ls").cmd;

        assert_eq!(
            (
                run_stacks(&h),
                h.stoat.active_workspace().cmds[ran].command.as_str()
            ),
            (vec![vec!["ls".to_owned()]], "ls"),
        );
    }

    /// The `run` command with a `claude` line starts the agent in a new tab
    /// that takes the focus. No Run pane opens and no Stub stays.
    #[test]
    fn the_run_command_starts_an_agent_tab_for_a_claude_line() {
        let mut h = Stoat::test();
        dispatch_run(&mut h, CLAUDE_LINE);

        let ws = h.stoat.active_workspace();
        let shown = matches!(ws.panes.pane(ws.panes.focus()).view, View::Agent(_));
        assert_eq!(
            (ws.runs.len(), ws.cmds.len(), (ws.tabs.len(), shown)),
            (0, 0, (2, true)),
        );
    }

    /// `run` with a `claude` line at a Run pane prompt starts the agent's tab
    /// and leaves no Stub on the pane's stack, as the `run` command leaves
    /// none on any stack.
    #[test]
    fn run_at_the_run_prompt_leaves_no_stub_for_a_claude_line() {
        let mut h = Stoat::test();
        let run = h.open_run();
        h.submit_run(&format!("run {CLAUDE_LINE}"));

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (ws.runs[run].blocks.len(), ws.cmds.len(), ws.tabs.len()),
            (0, 0, 2),
        );
    }

    /// Dispatch the `run` command with the line `line`.
    fn dispatch_run(h: &mut TestHarness, line: &str) {
        super::super::dispatch(
            &mut h.stoat,
            &stoat_action::Run {
                command: line.to_owned(),
            },
        );
    }

    /// The stack of each split pane of `h` that shows a Run pane, as the
    /// lines of its Cmds.
    fn run_stacks(h: &TestHarness) -> Vec<Vec<String>> {
        let ws = h.stoat.active_workspace();
        ws.panes
            .split_panes()
            .filter_map(|(_, shown)| match shown.view {
                View::Run(run) => Some(
                    ws.runs[run]
                        .blocks
                        .iter()
                        .map(|cmd| ws.cmds[*cmd].command.clone())
                        .collect(),
                ),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_finished_block_reads_its_mark_and_runtime() {
        let mut h = Stoat::test();
        h.open_run();
        let ok = h.submit_run("true");
        h.advance_clock(Duration::from_millis(18));
        h.inject_cmd_done(ok.cmd, 0);
        let first = h.cmd_status(ok.cmd);

        let failed = h.submit_run("false");
        h.inject_cmd_done(failed.cmd, 5);
        assert_eq!(
            [first, h.cmd_status(failed.cmd)],
            ["\u{2713} 18ms", "\u{2717} exit 5 0ms"],
        );
    }

    #[test]
    fn a_running_cmd_ticks_once_a_second() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("sleep 9");
        let mut reads = vec![h.cmd_status(handle.cmd)];
        for _ in 0..3 {
            h.advance_clock(Duration::from_secs(1));
            h.snapshot();
            reads.push(h.cmd_status(handle.cmd));
        }

        assert_eq!(
            reads,
            [
                "\u{25cf} 0:00",
                "\u{25cf} 0:01",
                "\u{25cf} 0:02",
                "\u{25cf} 0:03"
            ],
        );
    }

    /// With no output and no input, the tick wakes the run loop at each second
    /// of a running Cmd, so each second's count reaches the screen.
    #[test]
    fn a_running_cmd_wakes_the_run_loop_each_second() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("sleep 9");
        let notify = h.stoat.redraw_notify.clone();

        let wakes: Vec<bool> = (0..3)
            .map(|_| {
                let _ = notify.notified().now_or_never();
                h.advance_clock(Duration::from_secs(1));
                let woke = notify.notified().now_or_never().is_some();
                h.snapshot();
                woke
            })
            .collect();
        assert_eq!(wakes, [true; 3]);
    }

    /// The tick is armed while a Run pane on show holds a running Cmd, and
    /// dropped before the first Cmd and after the last one ends.
    #[test]
    fn no_tick_is_armed_while_no_cmd_runs() {
        let mut h = Stoat::test();
        h.open_run();
        h.snapshot();
        let idle = h.stoat.cmd_tick.is_some();

        let handle = h.submit_run("sleep 9");
        h.snapshot();
        let running = h.stoat.cmd_tick.is_some();

        h.inject_cmd_done(handle.cmd, 0);
        h.snapshot();
        assert_eq!(
            [idle, running, h.stoat.cmd_tick.is_some()],
            [false, true, false]
        );
    }

    /// A running Cmd in a parked tab arms no tick, and the first frame after
    /// its tab returns reads the seconds that passed while it was parked.
    #[test]
    fn a_cmd_in_a_parked_tab_arms_no_tick() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("sleep 9");
        super::super::dispatch(&mut h.stoat, &stoat_action::NewTab);
        h.snapshot();
        let parked = h.stoat.cmd_tick.is_some();

        h.advance_clock(Duration::from_secs(5));
        super::super::dispatch(&mut h.stoat, &stoat_action::PrevTab);
        h.snapshot();
        assert_eq!(
            (parked, h.cmd_status(handle.cmd)),
            (false, "\u{25cf} 0:05".to_owned())
        );
    }

    /// A submitted `cargo test` line reads its Kind Chip before its elapsed
    /// time, since a Cmd reads its kind from its line when it is made.
    #[test]
    fn a_submitted_cargo_line_leads_its_status_with_its_chip() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("cargo test");

        assert_eq!(h.cmd_status(handle.cmd), "cargo test \u{25cf} 0:00");
    }

    /// A running `cargo test` Cmd counts its result lines against its total,
    /// and a progress bar on its first body row fills the same share of the
    /// body's width.
    #[test]
    fn a_running_cargo_test_counts_its_results() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("cargo test");
        h.inject_cmd_output(
            handle.cmd,
            b"running 4 tests\r\ntest a ... ok\r\ntest b ... ok\r\n",
        );
        let status = h.cmd_status(handle.cmd);

        let (x, width, bars) = progress_bars(&mut h, 4);
        let color = |name| style_rgb(h.stoat.theme.get(name).fg).expect("an RGB color");
        assert_eq!(
            (status, bars),
            (
                "cargo test \u{25cf} 2 / 4 \u{b7} 0:00".to_owned(),
                vec![
                    (x, width, color(scope::UI_DIVIDER)),
                    (x, width / 2, color(scope::UI_STATUSLINE_RUN)),
                ],
            ),
        );
    }

    /// A running build reads the counts of cargo's progress bar, and a
    /// progress bar on its first body row fills the same share of the body's
    /// width.
    #[test]
    fn a_running_build_reads_its_progress() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("cargo build");
        h.inject_cmd_output(
            handle.cmd,
            b"\x1b[1m\x1b[96m    Building\x1b[0m [=====>      ] 212/240: stoat(build)\r",
        );
        let status = h.cmd_status(handle.cmd);

        let (x, width, bars) = progress_bars(&mut h, 2);
        let color = |name| style_rgb(h.stoat.theme.get(name).fg).expect("an RGB color");
        let fill = u16::try_from(u32::from(width) * 212 / 240).expect("a fill inside the track");
        assert_eq!(
            (status, bars),
            (
                "cargo build \u{25cf} 212 / 240 \u{b7} 0:00".to_owned(),
                vec![
                    (x, width, color(scope::UI_DIVIDER)),
                    (x, fill, color(scope::UI_STATUSLINE_RUN)),
                ],
            ),
        );
    }

    /// The bars two pixels high that the focused Run pane draws on the row
    /// `rows_up` rows above its bottom edge, each as its left edge, its width,
    /// and its color, after the left edge and the width of a full track.
    fn progress_bars(h: &mut TestHarness, rows_up: u16) -> (i16, u16, Vec<ProgressBar>) {
        h.render_composited();
        let area = {
            let ws = h.stoat.active_workspace();
            ws.panes.pane(ws.panes.focus()).area
        };
        let inner = crate::run::stack_rect(area);
        let y = ((area.bottom() - rows_up) * 16) as i16;
        let bars = command::decode_stream(&h.stoat.apc_scene.all_bytes())
            .into_iter()
            .filter_map(|cmd| match cmd {
                Command::Bar(bar) if bar.height == 2 && bar.y == y => {
                    Some((bar.x, bar.width, bar.color))
                },
                _ => None,
            })
            .collect();
        (((inner.x + 1) * 16) as i16, (inner.width - 1) * 16, bars)
    }

    /// A finished `cargo test` Cmd whose report counts a failed test names the
    /// count as its mark, and a run with none reads the plain mark.
    #[test]
    fn a_finished_cargo_test_reads_its_failed_count() {
        let mut h = Stoat::test();
        h.open_run();
        let reads = [(ONE_FAILURE, 101), (THREE_BINARIES, 0)].map(|(output, code)| {
            let handle = h.submit_run("cargo test");
            h.inject_cmd_output(handle.cmd, output.as_bytes());
            h.inject_cmd_done(handle.cmd, code);
            h.cmd_status(handle.cmd)
        });

        assert_eq!(
            reads,
            [
                "cargo test \u{2717} 1 failed 0ms",
                "cargo test \u{2713} 0ms"
            ],
        );
    }

    /// A Parsed Cmd asks cargo for color after `TERM`, which turns it off.
    #[test]
    fn a_parsed_cmd_asks_cargo_for_color() {
        let mut h = Stoat::test();
        h.open_run();
        h.submit_run("cargo test");

        let spawns = h.fake_terminal_host().spawns();
        let env = &spawns.last().expect("a spawn").env;
        assert_eq!(
            env[env.len().saturating_sub(2)..],
            [
                ("TERM".to_owned(), "dumb".to_owned()),
                ("CARGO_TERM_COLOR".to_owned(), "always".to_owned()),
            ],
        );
    }

    /// Run `line` at the focused Run pane's prompt to its end, with `output`
    /// and the exit `code`, and answer its Cmd.
    fn finished_run(h: &mut TestHarness, line: &str, output: &str, code: i32) -> CmdId {
        let handle = h.submit_run(line);
        h.inject_cmd_output(handle.cmd, output.as_bytes());
        h.inject_cmd_done(handle.cmd, code);
        handle.cmd
    }

    /// The texts of the rows that the Cmd `cmd` shows in place of its output,
    /// and the text of the hint row of the focused Run pane.
    fn report_and_hint(h: &TestHarness, cmd: CmdId) -> (Option<Vec<String>>, Option<String>) {
        let ws = h.stoat.active_workspace();
        let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the focused pane shows a Run pane");
        };
        let rows = ws.cmds[cmd]
            .report_rows()
            .map(|rows| rows.iter().map(BodyRow::text).collect());
        (
            rows,
            ws.runs[run].hint(&ws.cmds, &ws.terms).map(|row| row.text()),
        )
    }

    /// A finished `cargo test` Cmd shows its report in place of its output,
    /// and the newest block's hint names the key that goes to its failure.
    #[test]
    fn a_finished_cargo_test_shows_its_report() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(&mut h, "cargo test", ONE_FAILURE, 101);

        let lines = h.stoat.active_workspace().cmds[cmd]
            .grid
            .rendered_line_count();
        assert_eq!(
            (h.cmd_status(cmd), report_and_hint(&h, cmd)),
            (
                "cargo test \u{2717} 1 failed 0ms".to_owned(),
                (
                    Some(vec![
                        "1 passed  1 failed  3 tests \u{b7} 1 binary".to_owned(),
                        "\u{2717} render::chrome::tests::zoomed_negative_steps   chrome.rs:412"
                            .to_owned(),
                        "  assertion `left == right` failed  left: 18  right: 16".to_owned(),
                        format!("\u{22ef} {lines} lines of output \u{b7} t terminal view"),
                    ]),
                    Some("C-r run again \u{b7} Enter go to failure".to_owned()),
                ),
            ),
        );
    }

    #[test]
    fn a_passing_run_shows_no_failed_count_and_no_hint() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(&mut h, "cargo test", THREE_BINARIES, 0);

        let lines = h.stoat.active_workspace().cmds[cmd]
            .grid
            .rendered_line_count();
        assert_eq!(
            report_and_hint(&h, cmd),
            (
                Some(vec![
                    "3 passed  3 tests \u{b7} 3 binaries".to_owned(),
                    format!("\u{22ef} {lines} lines of output \u{b7} t terminal view"),
                ]),
                None,
            ),
        );
    }

    /// A `cargo test` Cmd shows its raw output while it runs, and after a run
    /// that started no test binary, such as one that failed to build.
    #[test]
    fn a_running_or_unbuilt_test_run_shows_its_raw_output() {
        let mut h = Stoat::test();
        h.open_run();
        let running = h.submit_run("cargo test");
        h.inject_cmd_output(running.cmd, ONE_FAILURE.as_bytes());
        let unbuilt = finished_run(
            &mut h,
            "cargo test",
            "error: could not compile `stoat`\n",
            101,
        );

        let shows_report = |cmd| h.stoat.active_workspace().cmds[cmd].report_rows().is_some();
        assert_eq!(
            [shows_report(running.cmd), shows_report(unbuilt)],
            [false; 2]
        );
    }

    /// `t` in normal mode turns the newest block's report into its raw output,
    /// which the block lays out as its grid rows, and `t` again turns it back.
    #[test]
    fn t_shows_the_raw_output_and_t_again_the_report() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(&mut h, "cargo test", ONE_FAILURE, 101);
        let read = |h: &TestHarness| {
            let block = &h.stoat.active_workspace().cmds[cmd];
            (block.report_rows().is_some(), block.rendered_line_span())
        };
        let grid_rows = h.stoat.active_workspace().cmds[cmd]
            .grid
            .rendered_line_count();

        let report = read(&h);
        h.type_keys("escape t");
        let raw = read(&h);
        h.type_keys("t");
        assert_eq!(
            [report, raw, read(&h)],
            [(true, 5), (false, 1 + grid_rows), (true, 5)]
        );
    }

    /// `t` flips only a block that has a report to show, so a press while the
    /// run goes on leaves the report to show once it ends.
    #[test]
    fn t_before_the_run_ends_flips_nothing() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("cargo test");
        h.inject_cmd_output(handle.cmd, ONE_FAILURE.as_bytes());
        h.type_keys("escape t");
        h.inject_cmd_done(handle.cmd, 101);

        let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
        assert_eq!((cmd.raw_view, cmd.report_rows().is_some()), (false, true));
    }

    /// Run `line` to its end with `output` and the exit `code` in a Run pane
    /// beside an editor pane, with the root `/work` over a 500-line
    /// [`CHROME`], and press Enter at the empty prompt.
    ///
    /// Answers whether the editor pane took the focus, the file it shows, and
    /// its cursor's row and column, from 0.
    fn enter_after(line: &str, output: &str, code: i32) -> (bool, PathBuf, (u32, u32)) {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.seed_fixture(CHROME, format!("{}\n", "x".repeat(20)).repeat(500));
        let editor_pane = h.stoat.active_workspace().panes.focus();
        h.type_action("SplitRight()");
        h.open_run();
        finished_run(&mut h, line, output, code);

        h.type_keys("enter");
        let point = editor::focused_cursor_point(&mut h.stoat);
        (
            h.stoat.active_workspace().panes.focus() == editor_pane,
            editor::focused_buffer_path(&h.stoat),
            (point.row, point.column),
        )
    }

    /// An Enter at an empty prompt opens the first failure's file in the
    /// editor pane beside the Run pane, which takes the focus, with the cursor
    /// on the failure's line and column. Cargo names the file from the root.
    #[test]
    fn enter_at_an_empty_prompt_goes_to_the_first_failure() {
        assert_eq!(
            enter_after("cargo test", ONE_FAILURE, 101),
            (true, PathBuf::from(CHROME), (411, 8)),
        );
    }

    /// The Enter after a failed build goes to its first error, past the
    /// warning that came before it.
    #[test]
    fn enter_goes_to_the_first_error() {
        assert_eq!(
            enter_after("cargo build", ERROR_AND_WARNING, 101),
            (true, PathBuf::from(CHROME), (411, 19)),
        );
    }

    /// The user command `rg`, which wraps `rg --vimgrep` and opens its
    /// matches.
    const RG: &str = r#"command rg = cli("rg --vimgrep", output: locations);"#;

    /// The second file that [`ZOOMED`] names, under the root `/work`.
    const THEME: &str = "/work/stoat/src/theme.rs";

    /// Three matches of `zoomed` in two files, as `rg --vimgrep` prints them.
    const ZOOMED: [&str; 3] = [
        "stoat/src/render/chrome.rs:412:4:fn zoomed(area: u16) -> u16 {",
        "stoat/src/render/chrome.rs:431:8:fn zoomed_clamps() {",
        "stoat/src/theme.rs:75:12:pub fn zoomed() {",
    ];

    /// A harness rooted at `/work` with [`RG`] loaded, the two files of
    /// [`ZOOMED`] in its file system, and a Run pane beside an editor pane,
    /// and answer the editor pane.
    ///
    /// Each line of the files starts with a two-byte char, so a byte column
    /// and a char column name different places.
    fn rg_harness() -> (TestHarness, PaneId) {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work");
        h.stoat.reload_user_config(RG);
        let text = format!("\u{e9}{}\n", "x".repeat(19)).repeat(500);
        h.seed_fixture(CHROME, &text);
        h.seed_fixture(THEME, &text);
        let editor_pane = h.stoat.active_workspace().panes.focus();
        h.type_action("SplitRight()");
        h.open_run();
        (h, editor_pane)
    }

    /// Run `rg zoomed` at the Run prompt of `h` to its end, with the output
    /// `lines` and the exit `code`, and answer its Cmd.
    fn rg_run(h: &mut TestHarness, lines: &[&str], code: i32) -> CmdId {
        let cmd = h.submit_run("rg zoomed").cmd;
        let output: String = lines.iter().map(|line| format!("{line}\r\n")).collect();
        h.inject_cmd_output(cmd, output.as_bytes());
        h.inject_cmd_done(cmd, code);
        cmd
    }

    /// A `locations` command at the Run prompt joins the stack as a Cmd on its
    /// own PTY, and the shell host runs nothing.
    #[test]
    fn a_locations_command_at_the_run_prompt_joins_the_stack() {
        let (mut h, _) = rg_harness();
        let cmd = h.submit_run("rg zoomed").cmd;

        let block = &h.stoat.active_workspace().cmds[cmd];
        assert_eq!(
            (
                spawns(&h)
                    .pop()
                    .map(|(program, args, _, _)| (program, args)),
                h.fake_shell.invocations().len(),
                (block.command.as_str(), block.source.map(|(label, _)| label)),
            ),
            (
                Some((
                    "bash".to_owned(),
                    vec!["-c".to_owned(), "rg --vimgrep zoomed".to_owned()]
                )),
                0,
                ("rg zoomed", Some("cli")),
            ),
        );
    }

    /// A finished block of matches shows each path once, with a row under it
    /// for each of its matches, counts its matches in its status, and names
    /// the keys that open them. The stack spans the header, the five rows,
    /// and the hint row.
    #[test]
    fn a_finished_locations_block_groups_its_matches_by_file() {
        let (mut h, _) = rg_harness();
        let cmd = rg_run(&mut h, &ZOOMED, 0);

        let ws = h.stoat.active_workspace();
        let View::Run(run) = ws.panes.pane(ws.panes.focus()).view else {
            panic!("the focused pane shows a Run pane");
        };
        assert_eq!(
            (
                ws.runs[run].output_line_total(&ws.cmds),
                h.cmd_status(cmd),
                report_and_hint(&h, cmd)
            ),
            (
                7,
                "cli \u{2713} 3 matches \u{b7} 0ms".to_owned(),
                (
                    Some(
                        [
                            "stoat/src/render/chrome.rs",
                            "  412:4  fn zoomed(area: u16) -> u16 {",
                            "  431:8  fn zoomed_clamps() {",
                            "stoat/src/theme.rs",
                            "  75:12  pub fn zoomed() {",
                        ]
                        .map(str::to_owned)
                        .to_vec()
                    ),
                    Some("Enter open location \u{b7} C-q to location list".to_owned()),
                ),
            ),
        );
    }

    /// A block of matches shows its raw output while it runs, and its matches
    /// once it ends.
    #[test]
    fn a_running_block_of_matches_shows_its_output() {
        let (mut h, _) = rg_harness();
        let cmd = h.submit_run("rg zoomed").cmd;
        h.inject_cmd_output(cmd, format!("{}\r\n", ZOOMED[0]).as_bytes());
        let running = report_and_hint(&h, cmd);
        h.inject_cmd_done(cmd, 0);

        assert_eq!(
            (running, report_and_hint(&h, cmd).0.map(|rows| rows.len())),
            ((None, None), Some(2)),
        );
    }

    /// A block keeps at most as many locations as a location list opens, so a
    /// search that matches every line of a large tree keeps the stack light.
    #[test]
    fn a_block_keeps_at_most_the_location_list_bound() {
        let mut cmd = Cmd::new("rg x".to_owned(), PathBuf::from("/work"), 80);
        cmd.locations = Some(Vec::new());
        let output: String = (1..=MAX_LOCATIONS + 1)
            .map(|line| format!("a.rs:{line}:1:x\r\n"))
            .collect();
        cmd.feed(output.as_bytes());

        assert_eq!(cmd.locations.map(|list| list.len()), Some(MAX_LOCATIONS));
    }

    #[test]
    fn one_match_reads_as_one_match() {
        let (mut h, _) = rg_harness();
        let cmd = rg_run(&mut h, &ZOOMED[..1], 0);

        assert_eq!(h.cmd_status(cmd), "cli \u{2713} 1 match \u{b7} 0ms");
    }

    /// A block of a `locations` command whose output names no location shows
    /// its raw output, and its status reads as any block's does.
    #[test]
    fn a_locations_block_with_no_location_keeps_its_raw_output() {
        let (mut h, _) = rg_harness();
        let cmd = rg_run(&mut h, &[], 1);

        assert_eq!(
            (h.cmd_status(cmd), report_and_hint(&h, cmd)),
            ("cli \u{2717} exit 1 0ms".to_owned(), (None, None)),
        );
    }

    /// A block lists its first 100 matches, and a last row counts the rest.
    #[test]
    fn a_long_match_list_ends_in_a_count_row() {
        let (mut h, _) = rg_harness();
        let lines: Vec<String> = (1..=130).map(|line| format!("a.rs:{line}:1:x")).collect();
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let cmd = rg_run(&mut h, &lines, 0);

        let rows = report_and_hint(&h, cmd).0.expect("the rows of the matches");
        assert_eq!(
            (
                rows.len(),
                rows.iter().filter(|row| row.starts_with("  ")).count(),
                rows.last().map(String::as_str),
            ),
            (102, 100, Some("\u{22ef} 30 more in the location list")),
        );
    }

    /// An Enter at an empty prompt opens the first match of the newest block
    /// in the editor pane beside the Run pane, which takes the focus, with the
    /// column read as a byte column.
    #[test]
    fn enter_opens_the_first_match_beside_the_run_pane() {
        let (mut h, editor_pane) = rg_harness();
        rg_run(&mut h, &ZOOMED, 0);
        h.type_keys("enter");

        let point = editor::focused_cursor_point(&mut h.stoat);
        assert_eq!(
            (
                h.stoat.active_workspace().panes.focus() == editor_pane,
                editor::focused_buffer_path(&h.stoat),
                (point.row, point.column),
            ),
            (true, PathBuf::from(CHROME), (411, 3)),
        );
    }

    /// `C-q` opens the matches of the newest block as a location list, each
    /// in output order at its place in its file. The block before it keeps
    /// its own match out of the list.
    #[test]
    fn ctrl_q_opens_the_matches_as_a_location_list() {
        let (mut h, _) = rg_harness();
        rg_run(&mut h, &ZOOMED[2..], 0);
        rg_run(&mut h, &ZOOMED, 0);
        h.type_keys("ctrl-q");
        h.settle();

        let entries: Option<Vec<(PathBuf, u32, u32)>> =
            h.stoat.location_picker.as_ref().map(|picker| {
                picker
                    .entries()
                    .iter()
                    .map(|entry| (entry.path.clone(), entry.line, entry.column))
                    .collect()
            });
        assert_eq!(
            entries,
            Some(vec![
                (PathBuf::from(CHROME), 412, 4),
                (PathBuf::from(CHROME), 431, 8),
                (PathBuf::from(THEME), 75, 12),
            ]),
        );
    }

    /// A location picked from the list over a Run pane opens in the editor
    /// pane beside it, and the Run pane keeps its stack.
    #[test]
    fn a_location_picked_over_a_run_pane_opens_beside_it() {
        let (mut h, editor_pane) = rg_harness();
        let run_pane = h.stoat.active_workspace().panes.focus();
        let cmd = rg_run(&mut h, &ZOOMED, 0);
        h.type_keys("ctrl-q");
        h.settle();
        h.type_keys("enter");

        let ws = h.stoat.active_workspace();
        let stack = match ws.panes.pane(run_pane).view {
            View::Run(run) => ws.runs[run].blocks.clone(),
            _ => Vec::new(),
        };
        assert_eq!(
            (
                ws.panes.focus() == editor_pane,
                editor::focused_buffer_path(&h.stoat),
                stack,
            ),
            (true, PathBuf::from(CHROME), vec![cmd]),
        );
    }

    /// `t` shows the raw output of a block of matches, the row of the step's
    /// text and then the lines as the tool printed them.
    #[test]
    fn t_shows_the_raw_lines_of_a_locations_block() {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(RG);
        h.open_run();
        let cmd = rg_run(&mut h, &ZOOMED, 0);
        h.type_keys("escape t");

        let block = &h.stoat.active_workspace().cmds[cmd];
        let rows: Vec<String> = (0..block.grid.rendered_line_count())
            .map(|row| {
                block
                    .grid
                    .text_in(0..usize::from(block.grid.width()), row..row + 1)
            })
            .collect();
        let mut printed = vec!["rg --vimgrep zoomed".to_owned()];
        printed.extend(ZOOMED.map(str::to_owned));
        assert_eq!(
            (block.raw_view, block.report_rows().is_none(), rows),
            (true, true, printed),
        );
    }

    /// `C-r` runs a block of matches again, with no match until the second
    /// run's output names one.
    #[test]
    fn ctrl_r_runs_a_locations_block_again() {
        let (mut h, _) = rg_harness();
        let cmd = rg_run(&mut h, &ZOOMED, 0);
        h.type_keys("ctrl-r");

        let block = &h.stoat.active_workspace().cmds[cmd];
        assert_eq!(
            (
                block.run_count,
                block.finished,
                block.locations.as_ref().map(Vec::len)
            ),
            (2, false, Some(0)),
        );
    }

    /// A build asks cargo for its progress bar as wide as the block's grid,
    /// after the color, and a test run asks for no bar.
    #[test]
    fn a_build_cmd_asks_cargo_for_its_progress_bar() {
        let mut h = Stoat::test();
        h.open_run();
        let build = h.submit_run("cargo build");
        h.submit_run("cargo test");

        let width = h.stoat.active_workspace().cmds[build.cmd]
            .grid
            .width()
            .to_string();
        let spawns = h.fake_terminal_host().spawns();
        let (build_env, test_env) = (&spawns[0].env, &spawns[1].env);
        let var = |name: &str, value: &str| (name.to_owned(), value.to_owned());
        assert_eq!(
            (
                build_env[build_env.len().saturating_sub(4)..].to_vec(),
                test_env
                    .iter()
                    .any(|(name, _)| name.starts_with("CARGO_TERM_PROGRESS")),
            ),
            (
                vec![
                    var("TERM", "dumb"),
                    var("CARGO_TERM_COLOR", "always"),
                    var("CARGO_TERM_PROGRESS_WHEN", "always"),
                    var("CARGO_TERM_PROGRESS_WIDTH", &width),
                ],
                false,
            ),
        );
    }

    /// A finished build with an error names its count as its mark, and its
    /// report counts both kinds and lists the errors before the warnings.
    #[test]
    fn a_finished_build_counts_its_errors_and_warnings() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(&mut h, "cargo build", ERROR_AND_WARNING, 101);

        let lines = h.stoat.active_workspace().cmds[cmd]
            .grid
            .rendered_line_count();
        assert_eq!(
            (h.cmd_status(cmd), report_and_hint(&h, cmd)),
            (
                "cargo build \u{2717} 1 error 0ms".to_owned(),
                (
                    Some(vec![
                        "1 error  1 warning".to_owned(),
                        "\u{2717} mismatched types   chrome.rs:412".to_owned(),
                        "\u{25b2} unused variable: `width`   chrome.rs:210".to_owned(),
                        format!("\u{22ef} {lines} lines of output \u{b7} t terminal view"),
                    ]),
                    Some("C-r run again \u{b7} Enter go to failure".to_owned()),
                ),
            ),
        );
    }

    /// Each row of a build's report draws its errors in the error color and
    /// its warnings in the warning color, and its locations muted.
    #[test]
    fn a_build_report_colors_errors_and_warnings_apart() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(&mut h, "cargo build", ERROR_AND_WARNING, 101);

        let scopes: Vec<Vec<&str>> = h.stoat.active_workspace().cmds[cmd]
            .report_rows()
            .expect("a report")
            .iter()
            .map(|row| row.spans.iter().map(|(_, tone)| tone.scope()).collect())
            .collect();
        assert_eq!(
            scopes,
            [
                vec![scope::UI_ERROR, scope::UI_DIAGNOSTIC_WARNING],
                vec![scope::UI_ERROR, scope::UI_TEXT_MUTED],
                vec![scope::UI_DIAGNOSTIC_WARNING, scope::UI_TEXT_MUTED],
                vec![scope::UI_TEXT_MUTED],
            ],
        );
    }

    /// A build with warnings alone shows its report, with its mark of a
    /// passing run, and offers no Enter, which never goes to a warning.
    #[test]
    fn a_build_with_warnings_alone_shows_its_report() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(
            &mut h,
            "cargo build",
            concat!(
                "warning: unused variable: `width`\n",
                "  --> stoat/src/render/chrome.rs:210:9\n",
            ),
            0,
        );

        let lines = h.stoat.active_workspace().cmds[cmd]
            .grid
            .rendered_line_count();
        assert_eq!(
            (h.cmd_status(cmd), report_and_hint(&h, cmd)),
            (
                "cargo build \u{2713} 0ms".to_owned(),
                (
                    Some(vec![
                        "1 warning".to_owned(),
                        "\u{25b2} unused variable: `width`   chrome.rs:210".to_owned(),
                        format!("\u{22ef} {lines} lines of output \u{b7} t terminal view"),
                    ]),
                    None,
                ),
            ),
        );
    }

    /// A build that ends with no error and no warning keeps cargo's own lines.
    #[test]
    fn a_clean_build_keeps_its_raw_output() {
        let mut h = Stoat::test();
        h.open_run();
        let cmd = finished_run(
            &mut h,
            "cargo build",
            concat!(
                "   Compiling stoat v0.4.0\n",
                "    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.44s\n",
            ),
            0,
        );

        assert_eq!(
            (h.cmd_status(cmd), report_and_hint(&h, cmd)),
            ("cargo build \u{2713} 0ms".to_owned(), (None, None)),
        );
    }

    /// Cargo names a failure's file from the workspace root, so a Cmd that
    /// ran in a member's directory finds the file under an ancestor of it.
    #[test]
    fn enter_reads_a_failure_path_from_an_ancestor_of_the_cwd() {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/work/stoat");
        let file = PathBuf::from("/work/stoat/src/render/chrome.rs");
        h.seed_fixture(&file, "x\n".repeat(500));
        h.type_action("SplitRight()");
        h.open_run();
        finished_run(&mut h, "cargo test", ONE_FAILURE, 101);

        h.type_keys("enter");
        assert_eq!(editor::focused_buffer_path(&h.stoat), file);
    }

    #[test]
    fn enter_on_a_failure_in_no_file_names_the_path() {
        let mut h = Stoat::test();
        h.open_run();
        finished_run(&mut h, "cargo test", ONE_FAILURE, 101);

        h.type_keys("enter");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("no file stoat/src/render/chrome.rs"),
        );
    }

    /// A harness with the user command [`HELLO`] loaded.
    fn with_hello() -> TestHarness {
        let mut h = Stoat::test();
        h.stoat.reload_user_config(HELLO);
        h
    }

    /// Run the user command `name` from the palette with Enter, with a fresh
    /// fake session queued for its process, and answer the Run Block's Cmd and
    /// the session.
    fn enter_command(h: &mut TestHarness, name: &str) -> (CmdId, Arc<FakeTerminalSession>) {
        let session = Arc::new(FakeTerminalSession::new());
        h.fake_terminal_host().push_session(session.clone());
        super::super::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        h.type_text(name);
        h.type_keys("enter");
        let cmd = h.stoat.run_block_cmd().expect("a Run Block");
        (cmd, session)
    }

    /// The program and the arguments of each spawn of the fake terminal host.
    fn programs(h: &TestHarness) -> Vec<(String, Vec<String>)> {
        spawns(h)
            .into_iter()
            .map(|(program, args, ..)| (program, args))
            .collect()
    }

    /// `Ctrl-r` at a Run pane's prompt runs its finished Cmd again in the same
    /// block with an empty grid, and the block's status names the second run
    /// once it ends.
    #[test]
    fn ctrl_r_runs_a_finished_cmd_again_in_its_block() {
        let mut h = Stoat::test();
        let run = h.open_run();
        let handle = h.submit_run("make");
        h.inject_cmd_output(handle.cmd, b"one\r\n");
        h.inject_cmd_done(handle.cmd, 1);
        h.stoat.active_workspace_mut().cmds[handle.cmd].selection = Some(GridSelection {
            anchor: (0, 0),
            head: (2, 0),
        });
        h.fake_terminal_host()
            .push_session(Arc::new(FakeTerminalSession::new()));
        h.type_keys("ctrl-r");

        let restarted = {
            let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
            (
                cmd.finished,
                cmd.run_count,
                cmd.grid.rendered_line_count(),
                cmd.selection.is_none(),
            )
        };
        h.advance_clock(Duration::from_secs(151));
        h.inject_cmd_done(handle.cmd, 0);
        let make = ("bash".to_owned(), vec!["-c".to_owned(), "make".to_owned()]);
        assert_eq!(
            (
                programs(&h),
                h.stoat.active_workspace().runs[run].blocks.clone(),
                restarted,
                h.cmd_status(handle.cmd),
            ),
            (
                vec![make.clone(), make],
                vec![handle.cmd],
                (false, 2, 0, true),
                "\u{2713} 2:31 \u{b7} run 2".to_owned(),
            ),
        );
    }

    /// `Ctrl-r` leaves a running Cmd alone, at a Run pane's prompt and on the
    /// palette's Run Block.
    #[test]
    fn ctrl_r_leaves_a_running_cmd_alone() {
        let mut pane = Stoat::test();
        pane.open_run();
        let handle = pane.submit_run("sleep 9");
        pane.type_keys("ctrl-r");

        let mut block = with_hello();
        let (cmd, _) = enter_command(&mut block, "hello");
        block.type_keys("ctrl-r");

        let runs = |h: &TestHarness, cmd: CmdId| {
            (
                spawns(h).len(),
                h.stoat.active_workspace().cmds[cmd].run_count,
            )
        };
        assert_eq!([runs(&pane, handle.cmd), runs(&block, cmd)], [(1, 1); 2]);
    }

    /// `Ctrl-r` in a Run pane's normal mode runs the newest finished Cmd of the
    /// stack again, and the older one keeps its run.
    #[test]
    fn ctrl_r_in_normal_mode_runs_the_newest_finished_cmd() {
        let mut h = Stoat::test();
        h.open_run();
        let [older, newer] = ["make", "ls"].map(|line| {
            let handle = h.submit_run(line);
            h.inject_cmd_done(handle.cmd, 0);
            handle.cmd
        });
        h.type_keys("escape ctrl-r");

        let ws = h.stoat.active_workspace();
        assert_eq!([ws.cmds[older].run_count, ws.cmds[newer].run_count], [1, 2]);
    }

    /// Output that the first run's reader sends after the restart never lands
    /// in the second run's grid.
    #[test]
    fn output_of_the_first_run_does_not_reach_the_second() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("make");
        h.inject_cmd_done(handle.cmd, 1);
        h.type_keys("ctrl-r");
        h.stoat.handle_pty_notification(PtyNotification::CmdOutput {
            workspace: h.stoat.active_workspace,
            cmd: handle.cmd,
            run: 1,
            data: b"stale\r\n".to_vec(),
        });

        let cmd = &h.stoat.active_workspace().cmds[handle.cmd];
        assert_eq!((cmd.run_count, cmd.grid.rendered_line_count()), (2, 0));
    }

    /// `Ctrl-r` on the palette's finished Run Block runs its Cmd again, and the
    /// palette stays open on it.
    #[test]
    fn ctrl_r_on_the_palette_block_runs_it_again_in_place() {
        let mut h = with_hello();
        let (cmd, _) = enter_command(&mut h, "hello");
        h.inject_cmd_done(cmd, 0);
        h.type_keys("ctrl-r");

        let shown = &h.stoat.active_workspace().cmds[cmd];
        assert_eq!(
            (h.stoat.run_block_cmd(), shown.finished, shown.run_count),
            (Some(cmd), false, 2),
        );
    }

    /// `Ctrl-r` in the palette's typing stage over a Run pane with a finished
    /// Cmd stays the palette's, so no Cmd runs again.
    #[test]
    fn ctrl_r_in_the_typing_stage_over_a_run_pane_runs_no_cmd() {
        let mut h = Stoat::test();
        h.open_run();
        let handle = h.submit_run("make");
        h.inject_cmd_done(handle.cmd, 0);
        super::super::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        h.type_keys("ctrl-r");
        assert_eq!(
            (
                spawns(&h).len(),
                h.stoat.active_workspace().cmds[handle.cmd].run_count
            ),
            (1, 1),
        );
    }

    /// `Ctrl-o` moves the running Run Block's Cmd onto the stack of a new Run
    /// pane under the editor pane, which takes the focus in insert mode. The
    /// Cmd runs on, its later output reaches it, and the palette's input goes.
    #[test]
    fn ctrl_o_moves_a_running_block_into_a_new_run_pane() {
        let mut h = with_hello();
        let (cmd, session) = enter_command(&mut h, "hello");
        let input = h
            .stoat
            .command_palette
            .as_ref()
            .expect("the palette is open")
            .input
            .editor_id;
        h.type_keys("ctrl-o");
        h.inject_cmd_output(cmd, b"later\r\n");

        let ws = h.stoat.active_workspace();
        let lower = ws
            .panes
            .split_panes()
            .max_by_key(|(_, pane)| pane.area.y)
            .map(|(id, _)| id)
            .expect("a split pane");
        let View::Run(run) = ws.panes.pane(lower).view else {
            panic!("the lower pane shows a Run pane");
        };
        let last_row = {
            let grid = &ws.cmds[cmd].grid;
            let rows = grid.rendered_line_count();
            grid.text_in(0..usize::from(grid.width()), rows - 1..rows)
        };
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                ws.editors.contains_key(input),
                ws.panes.split_panes().count(),
                ws.runs[run].blocks.clone(),
                ws.panes.focus() == lower,
                h.stoat.focused_mode(),
                session.was_killed(),
                last_row,
            ),
            (
                true,
                false,
                2,
                vec![cmd],
                true,
                "insert",
                false,
                "later".to_owned()
            ),
        );
    }

    /// `Ctrl-o` puts the Run Block's Cmd on the stack of the Run pane on show,
    /// and no pane opens.
    #[test]
    fn ctrl_o_appends_to_the_run_pane_that_shows() {
        let mut h = with_hello();
        super::super::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let run = h.open_run();
        h.submit_run("ls");
        super::super::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
        let (cmd, _) = enter_command(&mut h, "hello");
        h.type_keys("ctrl-o");

        let ws = h.stoat.active_workspace();
        let stack = &ws.runs[run].blocks;
        assert_eq!(
            (
                ws.panes.split_panes().count(),
                stack.len(),
                stack.last().copied()
            ),
            (2, 2, Some(cmd)),
        );
    }

    #[test]
    fn ctrl_o_moves_a_finished_block_too() {
        let mut h = with_hello();
        let (cmd, _) = enter_command(&mut h, "hello");
        h.inject_cmd_done(cmd, 0);
        h.type_keys("ctrl-o");

        let ws = h.stoat.active_workspace();
        let stacked: Vec<CmdId> = ws
            .runs
            .values()
            .flat_map(|run| run.blocks.clone())
            .collect();
        assert_eq!(
            (
                h.stoat.command_palette.is_none(),
                stacked,
                ws.cmds[cmd].finished
            ),
            (true, vec![cmd], true),
        );
    }

    /// A first word that names a command takes the command color at the Run
    /// pane prompt, and the rest of the line the text color.
    #[test]
    fn the_prompt_colors_a_resolved_first_word() {
        let mut h = Stoat::test();
        h.open_run();
        h.type_text("cd sub");

        let (command, text) = (look(&h, scope::UI_CMD_COMMAND), look(&h, scope::UI_TEXT));
        assert_eq!(
            prompt_cells(&mut h, "cd sub"),
            [vec![command; 2], vec![text; 4]].concat(),
        );
    }

    /// A cargo line's first word takes the command color, and its Kind Chip
    /// stands at the right end of the prompt row in the `ui.run.kind` colors.
    #[test]
    fn the_prompt_colors_a_cargo_line_and_shows_its_chip() {
        let mut h = Stoat::test();
        h.open_run();
        h.type_text("cargo test -p stoat");

        let cells = prompt_cells(&mut h, "cargo test");
        let row = focused_prompt_row(&h);
        let (runs, boxes) = row_scene(&h, row);
        let kind = h.stoat.theme.get(scope::UI_RUN_KIND);
        let (fg, bg) = (rgb(kind.fg), rgb(kind.bg));
        let (command, text) = (look(&h, scope::UI_CMD_COMMAND), look(&h, scope::UI_TEXT));
        assert_eq!(
            (
                cells,
                runs,
                boxes
                    .into_iter()
                    .filter(|(color, _)| *color == bg)
                    .collect::<Vec<_>>(),
            ),
            (
                [vec![command; 5], vec![text; 5]].concat(),
                vec![("cargo test".to_owned(), fg)],
                vec![(bg, i32::from(row.right()) * 16)],
            ),
        );
    }

    /// A first word that names a command shows that command's Source Tag at
    /// the right end of the prompt row.
    #[test]
    fn the_prompt_shows_the_source_tag_of_a_resolved_command() {
        let mut h = Stoat::test();
        h.open_run();
        h.type_text("cd sub");

        let (runs, _) = row_scene(&h, focused_prompt_row(&h));
        let builtin = h.stoat.theme.get(scope::UI_CMD_SOURCE_BUILTIN);
        assert_eq!(runs, vec![("built-in".to_owned(), rgb(builtin.fg))]);
    }

    /// A first word that names a user command shows that command's Source Tag
    /// at the right end of the prompt row.
    #[test]
    fn the_prompt_shows_the_source_tag_of_a_user_command() {
        let mut h = with_hello();
        h.open_run();
        h.type_text("hello");

        let (runs, _) = row_scene(&h, focused_prompt_row(&h));
        let config = h.stoat.theme.get(scope::UI_CMD_SOURCE_CONFIG);
        assert_eq!(runs, vec![("config".to_owned(), rgb(config.fg))]);
    }

    /// A line whose first word names no command draws in the text color, with
    /// no tag at its right.
    #[test]
    fn a_plain_shell_word_stays_plain() {
        let mut h = Stoat::test();
        h.open_run();
        h.type_text("ls -la");

        let cells = prompt_cells(&mut h, "ls -la");
        let (runs, _) = row_scene(&h, focused_prompt_row(&h));
        assert_eq!((cells, runs), (vec![look(&h, scope::UI_TEXT); 6], vec![]));
    }

    /// A command name that holds a space names nothing at the prompt, so
    /// `git log` runs git and draws in the text color.
    #[test]
    fn a_command_name_with_a_space_does_not_resolve_at_the_prompt() {
        let mut h = Stoat::test();
        h.open_run();
        h.type_text("git log");

        let cells = prompt_cells(&mut h, "git log");
        let (runs, _) = row_scene(&h, focused_prompt_row(&h));
        assert_eq!((cells, runs), (vec![look(&h, scope::UI_TEXT); 7], vec![]));
    }

    /// The `action` token and the internal action's name after it take the
    /// command color together.
    #[test]
    fn an_internal_action_resolves_under_the_action_token() {
        let mut h = Stoat::test();
        h.open_run();
        h.type_text("action focus-left");

        assert_eq!(
            prompt_cells(&mut h, "action focus-left"),
            vec![look(&h, scope::UI_CMD_COMMAND); 17],
        );
    }

    /// The prompt draws its caret in the theme's cursor style after the typed
    /// text, and a Run pane without the focus draws none.
    ///
    /// The fade of unfocused panes is off, so the cells of the pane without
    /// the focus keep the colors that the prompt paints them in.
    #[test]
    fn the_prompt_keeps_its_caret() {
        let mut h = Stoat::test();
        h.stoat.pane_fade_override = Some(false);
        h.open_run();
        h.type_text("ls");
        let pane = h.stoat.active_workspace().panes.focus();
        let focused = caret_offsets(&mut h, pane, "ls");
        super::super::dispatch(&mut h.stoat, &stoat_action::SplitNewRight);

        assert_eq!(
            (focused, caret_offsets(&mut h, pane, "ls")),
            (vec![2], vec![])
        );
    }

    /// The foreground and the modifiers of the theme scope `name`.
    fn look(h: &TestHarness, name: &str) -> (Color, Modifier) {
        let style = h.stoat.theme.get(name);
        (style.fg.expect("a foreground"), style.add_modifier)
    }

    /// The RGB value of `color`.
    fn rgb(color: Option<Color>) -> [u8; 3] {
        style_rgb(color).expect("an RGB color")
    }

    /// The prompt row of the Run pane `pane`, as the last frame laid it out.
    fn prompt_row(h: &TestHarness, pane: PaneId) -> Rect {
        let area = h.stoat.active_workspace().panes.pane(pane).area;
        let inner = crate::run::stack_rect(area);
        Rect::new(inner.x, area.bottom() - 1, inner.width, 1)
    }

    /// The prompt row of the focused Run pane, as the last frame laid it out.
    fn focused_prompt_row(h: &TestHarness) -> Rect {
        prompt_row(h, h.stoat.active_workspace().panes.focus())
    }

    /// The column of the first cell of `text` on `row` of `buf`.
    fn text_col(buf: &Buffer, row: Rect, text: &str) -> u16 {
        let chars: Vec<String> = text.chars().map(String::from).collect();
        let symbols: Vec<String> = (row.x..row.right())
            .map(|x| buf[(x, row.y)].symbol().to_owned())
            .collect();
        let at = symbols
            .windows(chars.len())
            .position(|cells| cells == chars.as_slice())
            .expect("the row shows the text");
        row.x + at as u16
    }

    /// The foreground and the modifiers of the cells of `text` on the focused
    /// Run pane's prompt row, as a frame paints them.
    fn prompt_cells(h: &mut TestHarness, text: &str) -> Vec<(Color, Modifier)> {
        let buf = h.stoat.render();
        let row = focused_prompt_row(h);
        let start = text_col(&buf, row, text);
        (start..start + text.chars().count() as u16)
            .map(|x| (buf[(x, row.y)].fg, buf[(x, row.y)].modifier))
            .collect()
    }

    /// The text runs and the boxes that the last frame's scene draws on `row`.
    fn row_scene(h: &TestHarness, row: Rect) -> (Vec<SceneRun>, Vec<SceneBox>) {
        let top = i32::from(row.y) * 16;
        let on_row = |y: i16| (top..top + 16).contains(&i32::from(y));
        let (mut runs, mut boxes) = (Vec::new(), Vec::new());
        for cmd in command::decode_stream(&h.stoat.apc_scene.all_bytes()) {
            match cmd {
                Command::TextRun(run) if on_row(run.row) => runs.push((run.text, run.color)),
                Command::TextRuns(group) => runs.extend(
                    group
                        .runs
                        .into_iter()
                        .filter(|(_, run_row, _)| on_row(*run_row))
                        .map(|(.., text)| (text, group.color)),
                ),
                Command::Bar(bar) if on_row(bar.y) => {
                    boxes.push((bar.color, i32::from(bar.x) + i32::from(bar.width)))
                },
                _ => {},
            }
        }
        (runs, boxes)
    }

    /// The offsets from the first cell of `text`, on the prompt row of the Run
    /// pane `pane`, of the cells that carry the theme's cursor style.
    fn caret_offsets(h: &mut TestHarness, pane: PaneId, text: &str) -> Vec<i32> {
        let buf = h.stoat.render();
        let row = prompt_row(h, pane);
        let start = i32::from(text_col(&buf, row, text));
        let cursor = h.stoat.theme.cursor_style();
        (row.x..row.right())
            .filter(|&x| {
                let cell = &buf[(x, row.y)];
                match cursor.bg {
                    Some(bg) => cell.bg == bg,
                    None => cell.modifier.contains(Modifier::REVERSED),
                }
            })
            .map(|x| i32::from(x) - start)
            .collect()
    }
}
