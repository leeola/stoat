use super::{
    context, pick, ArgKind, CommandSet, FlatStep, Output, RunSpec, Step, TextDefault, UserCommand,
};
use crate::{
    action_handlers, agent_goto,
    app::{MessageKind, Stoat, UpdateEffect},
    buffer_lifecycle,
    cli_spec::{self, CliSpec},
    command_palette::{ArgRow, PickLoad},
    host::{ShellHost, ShellOutput},
    keymap::ResolvedAction,
    location_picker::{self, LocationEntry, ParsedLocation},
    pane::{FocusTarget, View},
    picker::{self, PREVIEW_BYTE_LIMIT},
    workspace::WorkspaceId,
};
use std::{
    collections::VecDeque,
    io, mem,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::Instant,
};
use stoat_config::{Template, Value};

/// The exit code that `sh` gives a command that it does not find on `PATH`.
const NOT_FOUND_EXIT: i32 = 127;

/// The id of the next [`CommandRun`], unique in the process, so a shell result
/// finds the run that started it.
static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

/// A step of a command run, with each placeholder written.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PlannedStep {
    /// A shell line, which runs in the workspace root.
    Shell(String),
    /// An action, which dispatches as a key binding of it does.
    Act(ResolvedAction),
}

/// How a command run goes.
#[derive(Debug, PartialEq)]
enum Plan {
    /// The steps that run in order, each shell step on a worker.
    Steps(VecDeque<PlannedStep>),
    /// The steps of a Cmd that the palette's Run Block shows.
    Modal(Vec<PlannedStep>),
    /// The steps of a Cmd that joins a Run pane's stack.
    Pane(Vec<PlannedStep>),
}

/// A command run with steps left, or with a shell step out on a worker.
pub(crate) struct CommandRun {
    id: u64,
    name: String,
    output: Output,
    steps: VecDeque<PlannedStep>,
    /// The stdout of every shell step that ran, in order, which a `buffer`
    /// and a `locations` run read.
    stdout: String,
    /// The stdout of the last shell step, which the success message reads.
    last_stdout: String,
    /// Whether the last step was an action. A run that ends on an action
    /// shows no message of its own.
    last_was_action: bool,
    /// The workspace that was active when the run started. An action acts on
    /// the active workspace, so an action step runs only while this one is.
    workspace: WorkspaceId,
    /// The typed line that started the run, the line of a Cmd that the run
    /// ends in.
    line: String,
    /// The Source Tag of the run's command.
    tag: (&'static str, &'static str),
    /// Every step of the run in order, which a Cmd that the run ends in runs
    /// again on its own PTY.
    plan: Vec<PlannedStep>,
    /// When the run started, the start of a Cmd that the run ends in.
    started: Instant,
}

/// What a shell step of a command run sends back to the UI thread.
pub(crate) enum CommandEvent {
    StepDone {
        run: u64,
        result: io::Result<ShellOutput>,
    },
    /// The entries that the stdout of a `locations` run resolved to.
    Locations { entries: Vec<LocationEntry> },
    /// The result of the list command of a `pick` argument, with the
    /// generation of the palette's list that started it.
    PickList {
        generation: u64,
        result: io::Result<ShellOutput>,
    },
    /// The result of the preview `line` of a pick row, with the generation of
    /// the palette's list that started it.
    PickPreview {
        generation: u64,
        line: String,
        result: io::Result<ShellOutput>,
    },
    /// The help of the wrapped tool of the `cli` command `name`.
    CliSpec { name: String, loaded: LoadedCli },
}

/// The help of a wrapped tool, as one load reads it.
#[derive(Debug, PartialEq)]
pub(crate) struct LoadedCli {
    pub(crate) spec: CliSpec,
    /// Where the help came from, as the doc island names it, the line of
    /// [`help_source`].
    pub(crate) source: String,
    /// The first line of the tool's `--version` output, or `None` for a
    /// version run that failed or printed nothing.
    pub(crate) version: Option<String>,
    /// Whether the shell found no tool. The spec of a missing tool is empty,
    /// and its command still runs lines as typed.
    pub(crate) missing: bool,
}

/// Where the load of the help of a `cli` command's tool stands.
///
/// The palette starts a load the first time it parses the command with its
/// argument entry open. The state lasts for the session, and a config reload
/// drops it.
pub(crate) enum CliSpecState {
    /// The load runs on a worker.
    Loading,
    Ready(Arc<LoadedCli>),
}

/// Run `command` with `values`, one value for each of its arguments, for the
/// typed `line`.
///
/// A run with a shell step under the `block` or the `run_pane` output runs as
/// one Cmd whose line is `line`, and the Cmd runs the steps in order, as
/// [`action_handlers::run::run_steps`] runs them. Under the `block` output,
/// the palette that started it turns into the Cmd's Run Block, and with no
/// palette the Cmd joins a Run pane's stack. Under the `run_pane` output, the
/// Cmd joins a Run pane's stack and the focus stays where it is. A
/// `locations` run that starts at a Run pane prompt, as [`at_run_prompt`]
/// reads it, runs as a Cmd on that pane's stack too, whose block shows its
/// matches.
///
/// Any other run goes step by step here, as [`plan`] lays it out. A shell
/// step runs on a worker with no terminal, so its stdout and its stderr stay
/// apart. An action step runs once the step before it exits 0, and the first
/// failure ends the run. A `cli` command runs as a shell step does, with its
/// tail as its one value.
///
/// An empty value of an argument with a default runs with the default, as
/// [`context::default_text`] reads it, and so does the default of a nested
/// command's argument that its caller does not declare. A `file` or `dir`
/// value that starts with `~/` runs with the `HOME` directory in place of the
/// `~`.
pub(crate) fn start(
    stoat: &mut Stoat,
    command: &UserCommand,
    values: Vec<String>,
    line: &str,
) -> UpdateEffect {
    let values = fill_defaults(stoat, command, values);
    let values = expand_home(command, values, stoat.env_host.var("HOME").as_deref());
    let set = stoat.commands.clone();
    let planned = plan(command, &set, &values, &mut |default| {
        context::default_text(stoat, default)
    });
    match planned {
        Plan::Modal(steps) => {
            let width = action_handlers::run_block_width(stoat);
            let source = Some(command.source_tag());
            let (id, effect) =
                action_handlers::run::start_cmd_steps(stoat, line, steps, source, width);
            action_handlers::open_run_block(stoat, id);
            effect.merge(UpdateEffect::Redraw)
        },
        Plan::Pane(steps) => {
            let width = action_handlers::run::run_pane_width(stoat);
            let source = Some(command.source_tag());
            let (id, effect) =
                action_handlers::run::start_cmd_steps(stoat, line, steps, source, width);
            action_handlers::run::append_to_run_pane(stoat, id, false);
            effect.merge(UpdateEffect::Redraw)
        },
        Plan::Steps(steps) if command.output == Output::Locations && at_run_prompt(stoat) => {
            let width = action_handlers::run::run_pane_width(stoat);
            let source = Some(command.source_tag());
            let (id, effect) =
                action_handlers::run::start_cmd_steps(stoat, line, steps.into(), source, width);
            if let Some(cmd) = stoat.active_workspace_mut().cmds.get_mut(id) {
                cmd.locations = Some(Vec::new());
            }
            action_handlers::run::append_to_run_pane(stoat, id, false);
            effect.merge(UpdateEffect::Redraw)
        },
        Plan::Steps(steps) => {
            let run = CommandRun {
                id: NEXT_RUN.fetch_add(1, Ordering::Relaxed),
                name: command.name.clone(),
                output: command.output,
                plan: steps.iter().cloned().collect(),
                steps,
                stdout: String::new(),
                last_stdout: String::new(),
                last_was_action: false,
                workspace: stoat.active_workspace,
                line: line.to_owned(),
                tag: command.source_tag(),
                started: stoat.executor.now(),
            };
            advance(stoat, run)
        },
    }
}

/// Whether a run starts at a Run pane prompt, where no palette holds it and
/// the focused split pane shows a Run pane.
fn at_run_prompt(stoat: &Stoat) -> bool {
    let ws = stoat.active_workspace();
    stoat.palette_held.is_none()
        && ws.focus == FocusTarget::SplitPane
        && matches!(ws.panes.pane(ws.panes.focus()).view, View::Run(_))
}

/// Give the split focus to the pane that [`agent_goto::target_pane`] picks
/// when the focused pane shows a Run pane, so what opens in the focused pane
/// opens beside the stack and the stack stays.
fn focus_beside_a_run_pane(stoat: &mut Stoat) {
    let executor = stoat.executor.clone();
    let ws = stoat.active_workspace_mut();
    if !matches!(ws.panes.pane(ws.panes.focus()).view, View::Run(_)) {
        return;
    }
    let pane = agent_goto::target_pane(ws, executor);
    ws.focus = FocusTarget::SplitPane;
    ws.panes.set_focus(pane);
}

/// Take the results of the shell steps that finished, and carry each run on.
///
/// Answers whether a result landed. A step that exits 0 hands its stdout to
/// the rest of the run. Any other result ends the run with a message that
/// names the failure, except the step that ends a `locations` run, whose
/// stdout lists the locations whatever its exit code. The result of a pick
/// list's command goes to the palette's list, as [`land_pick`] reads it, and
/// a tool's help goes to [`Stoat::cli_specs`], as [`land_cli`] stores it.
pub(crate) fn pump(stoat: &mut Stoat) -> bool {
    let events = mem::take(&mut *stoat.command_events.lock().expect("command events mutex"));
    let landed = !events.is_empty();
    for event in events {
        let (run, result) = match event {
            CommandEvent::StepDone { run, result } => (run, result),
            CommandEvent::Locations { entries } => {
                match entries.is_empty() {
                    true => stoat.notify(MessageKind::Info, "no locations"),
                    false => {
                        stoat.location_picker =
                            Some(action_handlers::lsp::open_location_picker(stoat, entries));
                    },
                }
                continue;
            },
            CommandEvent::PickList { generation, result } => {
                land_pick(stoat, generation, result);
                continue;
            },
            CommandEvent::PickPreview {
                generation,
                line,
                result,
            } => {
                land_preview(stoat, generation, line, result);
                continue;
            },
            CommandEvent::CliSpec { name, loaded } => {
                land_cli(stoat, name, loaded);
                continue;
            },
        };
        let Some(index) = stoat.command_runs.iter().position(|held| held.id == run) else {
            continue;
        };
        let mut run = stoat.command_runs.remove(index);
        match result {
            Ok(output) if run.output == Output::Locations && run.steps.is_empty() => {
                run.stdout
                    .push_str(&String::from_utf8_lossy(&output.stdout));
                end_locations(stoat, run, output);
            },
            Ok(output) if output.exit_code == 0 => {
                let stdout = String::from_utf8_lossy(&output.stdout);
                run.stdout.push_str(&stdout);
                run.last_stdout = stdout.into_owned();
                advance(stoat, run);
            },
            failed => stoat.set_status(failure(&run.name, &failed)),
        }
    }
    landed
}

/// `values` with each empty value of an argument with a default replaced by
/// that default.
fn fill_defaults(stoat: &mut Stoat, command: &UserCommand, mut values: Vec<String>) -> Vec<String> {
    for (value, arg) in values.iter_mut().zip(&command.args) {
        if let Some(default) = arg.default().filter(|_| value.is_empty()) {
            *value = context::default_text(stoat, default);
        }
    }
    values
}

/// `values` with the leading `~/` of each `file` or `dir` value written as
/// the directory `home`. With no `home`, each value stays as typed.
///
/// A shell step writes each value as a quoted word, and the shell expands no
/// `~` in a quoted word. The tail of a `cli` command stands past the
/// arguments and stays as typed, because the shell reads it unquoted and
/// expands its `~` itself.
fn expand_home(command: &UserCommand, mut values: Vec<String>, home: Option<&str>) -> Vec<String> {
    let Some(home) = home else {
        return values;
    };
    for (value, arg) in values.iter_mut().zip(&command.args) {
        let path = matches!(arg.kind, ArgKind::File | ArgKind::Dir);
        if let Some(rest) = value.strip_prefix("~/").filter(|_| path) {
            *value = Path::new(home).join(rest).to_string_lossy().into_owned();
        }
    }
    values
}

/// The steps of `command` with each placeholder written, and how they run.
///
/// The steps are the command's run as [`UserCommand::flatten`] reads it from
/// `set`. Each step's placeholders read the values of its own command, as
/// [`chain_values`] gives them from `values`, which stand in the order of the
/// arguments of `command`. A nested default reads as `default` writes it. A
/// `cli` command runs the line that [`cli_line`] writes from its tool and its
/// tail.
///
/// Under the `block` and the `run_pane` outputs, a run that holds a shell step
/// is the steps of one Cmd, in order. A run of action steps alone goes step by
/// step under them, as a `notify` run does, because it starts no process for
/// a Cmd to show.
fn plan(
    command: &UserCommand,
    set: &CommandSet,
    values: &[String],
    default: &mut dyn FnMut(&TextDefault) -> String,
) -> Plan {
    if let RunSpec::Cli { tool, .. } = &command.run {
        return shell_plan(cli_line(tool, values), command.output);
    }
    let steps: VecDeque<PlannedStep> = command
        .flatten(set)
        .iter()
        .filter_map(|flat| planned_step(flat, values, default))
        .collect();
    let has_shell = steps
        .iter()
        .any(|step| matches!(step, PlannedStep::Shell(_)));
    match (command.output, has_shell) {
        (Output::Block, true) => Plan::Modal(steps.into()),
        (Output::RunPane, true) => Plan::Pane(steps.into()),
        _ => Plan::Steps(steps),
    }
}

/// The step of `flat` with each placeholder written from the values of the
/// command whose `run` holds it, or `None` for a command step, which adds no
/// step of its own.
///
/// A shell line writes each value as one shell word. A string argument of an
/// action writes each value as it is.
fn planned_step(
    flat: &FlatStep<'_>,
    values: &[String],
    default: &mut dyn FnMut(&TextDefault) -> String,
) -> Option<PlannedStep> {
    let values = chain_values(&flat.chain, values, default);
    let value = |name: &str| {
        flat.chain
            .last()
            .and_then(|command| arg_value(command, &values, name))
    };
    match flat.step {
        Step::Shell(template) => Some(PlannedStep::Shell(template.render_shell(value))),
        Step::Action(action) => {
            let mut action = action.clone();
            for arg in &mut action.args {
                if let Value::String(text) = &mut arg.value {
                    *text = Template::parse(text).render(value);
                }
            }
            Some(PlannedStep::Act(action))
        },
        Step::Command(_) => None,
    }
}

/// The values of the arguments of the last command of `chain`.
///
/// `values` stand in the order of the arguments of the first command, the
/// command that runs. Each command after it runs as a command step of the one
/// before it, so each of its arguments takes the value of its caller's
/// argument of the same name. An argument that its caller does not declare
/// takes its default, as `default` writes it.
fn chain_values(
    chain: &[&UserCommand],
    values: &[String],
    default: &mut dyn FnMut(&TextDefault) -> String,
) -> Vec<String> {
    let mut values = values.to_vec();
    for pair in chain.windows(2) {
        let [caller, nested] = pair else {
            continue;
        };
        values = nested
            .args
            .iter()
            .map(|arg| match arg_value(caller, &values, &arg.name) {
                Some(value) => value,
                None => match arg.default() {
                    Some(text) => default(text),
                    None => String::new(),
                },
            })
            .collect();
    }
    values
}

/// How the shell line `line` runs under `output`.
fn shell_plan(line: String, output: Output) -> Plan {
    match output {
        Output::Block => Plan::Modal(vec![PlannedStep::Shell(line)]),
        Output::RunPane => Plan::Pane(vec![PlannedStep::Shell(line)]),
        Output::Notify | Output::Buffer | Output::Locations => {
            Plan::Steps(VecDeque::from([PlannedStep::Shell(line)]))
        },
    }
}

/// The shell line that runs the wrapped `tool` with the tail, the first of
/// `values`, trimmed.
///
/// The tail joins the line as typed, with no quoting, so its flags, quotes,
/// and globs keep their shell meaning. An empty tail runs the tool alone.
fn cli_line(tool: &str, values: &[String]) -> String {
    match values.first().map_or("", |tail| tail.trim()) {
        "" => tool.to_owned(),
        tail => format!("{tool} {tail}"),
    }
}

/// Run the steps of `run` up to its next shell step, which parks the run
/// until the step's result lands, or to its end.
///
/// An action step runs only while the workspace that was active when the run
/// started still is. Otherwise the run stops with a message that says so.
///
/// A run that ends on a shell step shows a message under the `notify`
/// output. The message is the first line of that step's stdout, or `<name>
/// done` for a step with none. A run of the `buffer` output that ends on a
/// shell step opens the stdout of every shell step in a scratch buffer,
/// beside a focused Run pane, as [`focus_beside_a_run_pane`] moves the focus.
/// The `block` and `run_pane` outputs hand this walk a run of action steps
/// alone, which shows no message of its own.
fn advance(stoat: &mut Stoat, mut run: CommandRun) -> UpdateEffect {
    let mut effect = UpdateEffect::Redraw;
    while let Some(step) = run.steps.pop_front() {
        match step {
            PlannedStep::Act(_) if stoat.active_workspace != run.workspace => {
                stoat.set_status(format!("{}: stopped, the workspace changed", run.name));
                return effect;
            },
            PlannedStep::Act(action) => {
                effect =
                    effect.merge(stoat.run_bound_actions(&Arc::from(vec![action]), None, false));
                run.last_was_action = true;
            },
            PlannedStep::Shell(line) => {
                let id = run.id;
                run.last_was_action = false;
                spawn_shell_job(stoat, line, move |result| CommandEvent::StepDone {
                    run: id,
                    result,
                });
                stoat.command_runs.push(run);
                return effect;
            },
        }
    }

    match run.output {
        _ if run.last_was_action => {},
        Output::Notify => {
            let text = first_line(&run.last_stdout)
                .map_or_else(|| format!("{} done", run.name), str::to_owned);
            stoat.notify(MessageKind::Success, text);
        },
        Output::Buffer => {
            focus_beside_a_run_pane(stoat);
            buffer_lifecycle::show_scratch_with_text(stoat, &run.stdout);
        },
        Output::Block | Output::RunPane | Output::Locations => {},
    }
    effect
}

/// Run `line` through the shell host on a worker, as [`spawn_job`] runs a
/// job.
///
/// The event that `done` makes of the result goes onto
/// [`Stoat::command_events`].
pub(crate) fn spawn_shell_job(
    stoat: &Stoat,
    line: String,
    done: impl FnOnce(io::Result<ShellOutput>) -> CommandEvent + Send + 'static,
) {
    spawn_job(stoat, move |shell, root, env| {
        done(shell.run(&line, b"", Some(root), env))
    });
}

/// Load the help of `tool`, the wrapped tool of the `cli` command `name`, on
/// a worker, from the command `completions` when the command declares one,
/// as [`load_cli`] reads it.
///
/// The help lands as a [`CommandEvent::CliSpec`], which [`pump`] stores.
pub(crate) fn spawn_cli_load(
    stoat: &Stoat,
    name: String,
    tool: String,
    completions: Option<String>,
) {
    spawn_job(stoat, move |shell, root, env| CommandEvent::CliSpec {
        name,
        loaded: load_cli(shell, &tool, completions.as_deref(), root, env),
    });
}

/// Run `job` on a worker with the shell host, the workspace root, and the
/// environment of a command's shell line, and push the event that it answers
/// onto [`Stoat::command_events`].
///
/// The environment is the project environment with `TERM=dumb`. A job runs
/// its lines with no terminal, because an output that reads stdout reads it
/// apart from stderr, and a terminal merges the two.
fn spawn_job(
    stoat: &Stoat,
    job: impl FnOnce(&dyn ShellHost, &Path, &[(String, Option<String>)]) -> CommandEvent
        + Send
        + 'static,
) {
    let ws = stoat.active_workspace();
    let root = ws.git_root.clone();
    let env: Vec<(String, Option<String>)> = ws
        .env
        .diff
        .iter()
        .cloned()
        .chain([("TERM".to_owned(), Some("dumb".to_owned()))])
        .collect();
    let shell_host = stoat.shell_host.clone();
    let executor = stoat.executor.clone();
    let events = stoat.command_events.clone();

    stoat
        .spawn_woken(async move {
            let event = executor
                .spawn_blocking(move || job(&*shell_host, &root, &env))
                .await;
            events.lock().expect("command events mutex").push(event);
        })
        .detach();
}

/// Read the help of the wrapped `tool` through `shell`, in `root` with `env`.
///
/// The load runs the line that [`help_source`] names and then
/// `<tool> --version`, and blocks until both end. That line is `<tool>
/// --help`, or the command `completions` when the tool declares one, whose
/// fish completion script reads as the help of the tool's first word.
///
/// A help run that does not start, or that exits 127 as `sh` does for a
/// command it does not find, marks the tool as missing and runs no version. A
/// help on stderr with nothing on stdout reads as the help, as an older tool
/// prints it. The version is the first line of the version run's stdout that
/// holds text, when that run exits 0.
pub(crate) fn load_cli(
    shell: &dyn ShellHost,
    tool: &str,
    completions: Option<&str>,
    root: &Path,
    env: &[(String, Option<String>)],
) -> LoadedCli {
    let source = help_source(tool, completions);
    let help = match shell.run(&source, b"", Some(root), env) {
        Ok(output) if output.exit_code != NOT_FOUND_EXIT => output,
        _ => {
            return LoadedCli {
                spec: CliSpec::default(),
                source,
                version: None,
                missing: true,
            };
        },
    };

    let stdout = String::from_utf8_lossy(&help.stdout);
    let text = match stdout.trim().is_empty() {
        true => String::from_utf8_lossy(&help.stderr),
        false => stdout,
    };
    let version = shell
        .run(&format!("{tool} --version"), b"", Some(root), env)
        .ok()
        .filter(|output| output.exit_code == 0)
        .and_then(|output| first_line(&String::from_utf8_lossy(&output.stdout)).map(str::to_owned));
    let spec = match completions {
        Some(_) => {
            let command = tool.split_whitespace().next().unwrap_or(tool);
            cli_spec::parse_fish_completions(&text, command)
        },
        None => cli_spec::parse_help(&text),
    };
    LoadedCli {
        spec,
        source,
        version,
        missing: false,
    }
}

/// The line whose output the load of the wrapped `tool`'s help reads, which
/// the doc island names as where the help comes from.
///
/// It is the command `completions` when the tool declares one, and
/// `<tool> --help` otherwise.
pub(crate) fn help_source(tool: &str, completions: Option<&str>) -> String {
    completions.map_or_else(|| format!("{tool} --help"), str::to_owned)
}

/// Store `loaded`, the help of the `cli` command `name`, for the session.
///
/// The help lands only over the command's `Loading` mark, so a load that was
/// out when the config reloaded drops. The open palette takes a new
/// generation, because the pool pages of its list read the loaded signature.
fn land_cli(stoat: &mut Stoat, name: String, loaded: LoadedCli) {
    let Some(state) = stoat
        .cli_specs
        .get_mut(&name)
        .filter(|state| matches!(state, CliSpecState::Loading))
    else {
        return;
    };
    *state = CliSpecState::Ready(Arc::new(loaded));
    if let Some(palette) = stoat.command_palette.as_mut() {
        palette.generation = picker::next_generation();
    }
}

/// Give the open palette's pick list of `generation` the rows of `result`, or
/// its failure.
///
/// A run that exits 0 gives a row for each row of its stdout, as
/// [`pick::parse_rows`] reads them. The spec's templates write the label and
/// the insert of each row. Any other result fails the list. A result for a
/// list that the palette does not hold drops, because a newer run took its
/// place.
fn land_pick(stoat: &mut Stoat, generation: u64, result: io::Result<ShellOutput>) {
    let Some(pick) = stoat
        .command_palette
        .as_mut()
        .and_then(|palette| palette.pick.as_mut())
        .filter(|pick| pick.generation == generation)
    else {
        return;
    };
    match result {
        Ok(output) if output.exit_code == 0 => {
            let spec = &pick.spec;
            let parsed = pick::parse_rows(&output.stdout);
            let rows = parsed
                .iter()
                .map(|row| {
                    let field = |name: &str| row.field(name).map(str::to_owned);
                    ArgRow {
                        label: spec.show.render(field),
                        insert: spec.value.render(field),
                        value_name: None,
                        description: String::new(),
                    }
                })
                .collect();
            pick.preview_lines = match &spec.preview {
                Some(template) => parsed
                    .iter()
                    .map(|row| template.render_shell(|name| row.field(name).map(str::to_owned)))
                    .collect(),
                None => Vec::new(),
            };
            pick.list.set_rows(rows);
            pick.load = PickLoad::Ready;
        },
        failed => pick.load = PickLoad::Failed(pick_failure(&failed)),
    }
}

/// Give the open palette's pick list of `generation` the preview text of
/// `line`.
///
/// A run that exits 0 gives its stdout, cut at [`PREVIEW_BYTE_LIMIT`] bytes.
/// Any other result gives its failure, as [`pick_failure`] words it. A result
/// for a list that the palette does not hold drops.
fn land_preview(stoat: &mut Stoat, generation: u64, line: String, result: io::Result<ShellOutput>) {
    let Some(pick) = stoat
        .command_palette
        .as_mut()
        .and_then(|palette| palette.pick.as_mut())
        .filter(|pick| pick.generation == generation)
    else {
        return;
    };
    let text: Arc<str> = match &result {
        Ok(output) if output.exit_code == 0 => {
            let shown = &output.stdout[..output.stdout.len().min(PREVIEW_BYTE_LIMIT)];
            Arc::from(String::from_utf8_lossy(shown).as_ref())
        },
        failed => Arc::from(pick_failure(failed)),
    };
    pick.previews.insert(line, Some(text));
}

/// The first shell line that `command` runs with `values`, one for each
/// argument or the tail of a `cli` command, or `None` for a run with no shell
/// step.
///
/// The palette's key line names it. It reads the first shell step of the run
/// as [`UserCommand::flatten`] reads it from `set`, and writes each value as
/// [`plan`] does. A nested command's argument that its caller does not
/// declare writes a literal default's text, and a context default reads as
/// empty, because only the run reads the editor. A `file` or `dir` value
/// keeps its leading `~/`. Only the run expands it to the home directory, so
/// the line on show reads that path as typed.
pub(crate) fn run_line_preview(
    command: &UserCommand,
    set: &CommandSet,
    values: &[String],
) -> Option<String> {
    if let RunSpec::Cli { tool, .. } = &command.run {
        return Some(cli_line(tool, values));
    }
    command
        .flatten(set)
        .iter()
        .filter(|flat| matches!(flat.step, Step::Shell(_)))
        .find_map(
            |flat| match planned_step(flat, values, &mut literal_default)? {
                PlannedStep::Shell(line) => Some(line),
                PlannedStep::Act(_) => None,
            },
        )
}

/// The text of a literal default, or the empty string for a context value,
/// which only a run reads.
fn literal_default(default: &TextDefault) -> String {
    match default {
        TextDefault::Literal(text) => text.clone(),
        TextDefault::Context(_) => String::new(),
    }
}

/// The value in `values` of the argument of `command` named `name`, or `None`
/// for a name that no argument has.
fn arg_value(command: &UserCommand, values: &[String], name: &str) -> Option<String> {
    command
        .args
        .iter()
        .position(|arg| arg.name == name)
        .and_then(|index| values.get(index).cloned())
}

/// The text that a failed pick list shows.
///
/// The text is the first line of the stderr that holds text, or `exit <code>`
/// for a run with none. A run that did not start shows its error.
fn pick_failure(result: &io::Result<ShellOutput>) -> String {
    match result {
        Ok(output) => first_line(&String::from_utf8_lossy(&output.stderr))
            .map_or_else(|| format!("exit {}", output.exit_code), str::to_owned),
        Err(error) => error.to_string(),
    }
}

/// The first line of `text` that holds more than whitespace, trimmed.
fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// End the `locations` run `run`, whose last shell step gave `output`,
/// whatever its exit code.
///
/// The lines of the stdout of every shell step that name a location open as
/// a location list, as [`spawn_resolve_locations`] resolves them. A run with
/// no location becomes a finished Cmd on a Run pane's stack, its output the
/// stdout of every shell step and then the last step's stderr, so its output
/// shows and it can run again on its own PTY. The focus stays where it is.
///
/// A run whose workspace left the screen opens no Run pane there. With no
/// location, its last step's failure with a stderr line shows as a message,
/// and any other run says that it found none.
fn end_locations(stoat: &mut Stoat, run: CommandRun, output: ShellOutput) {
    let parsed: Vec<ParsedLocation> = run
        .stdout
        .lines()
        .filter_map(location_picker::parse_location_line)
        .collect();
    if !parsed.is_empty() {
        let root = stoat.active_workspace().git_root.clone();
        spawn_resolve_locations(stoat, parsed, root);
        return;
    }

    if stoat.active_workspace == run.workspace {
        let mut text = run.stdout.into_bytes();
        text.extend_from_slice(&output.stderr);
        let id = action_handlers::run::finished_cmd(
            stoat,
            &run.line,
            run.plan,
            Some(run.tag),
            &text,
            Some(output.exit_code),
            run.started,
        );
        action_handlers::run::append_to_run_pane(stoat, id, false);
        return;
    }
    let failed =
        output.exit_code != 0 && first_line(&String::from_utf8_lossy(&output.stderr)).is_some();
    match failed {
        true => stoat.set_status(failure(&run.name, &Ok(output))),
        false => stoat.notify(MessageKind::Info, "no locations"),
    }
}

/// Resolve the locations `parsed` against `root` on a worker, and open the
/// location picker over them once they land, as [`pump`] lands them.
///
/// A file that does not read drops its locations, and with none left the
/// message says that the run found no locations.
pub(crate) fn spawn_resolve_locations(stoat: &Stoat, parsed: Vec<ParsedLocation>, root: PathBuf) {
    let open = action_handlers::lsp::open_ropes(stoat);
    let fs = stoat.fs_host.clone();
    let executor = stoat.executor.clone();
    let events = stoat.command_events.clone();
    stoat
        .spawn_woken(async move {
            let entries = executor
                .spawn_blocking(move || {
                    location_picker::resolve_locations(parsed, &root, &open, &*fs)
                })
                .await;
            events
                .lock()
                .expect("command events mutex")
                .push(CommandEvent::Locations { entries });
        })
        .detach();
}

/// The message for the run of the command `name` that `result` ended.
///
/// A step that ran names the first line of its stderr and its exit code. A
/// step that did not start names the error.
fn failure(name: &str, result: &io::Result<ShellOutput>) -> String {
    match result {
        Ok(output) => match first_line(&String::from_utf8_lossy(&output.stderr)) {
            Some(line) => format!("{name}: {line} (exit {})", output.exit_code),
            None => format!("{name}: exit {}", output.exit_code),
        },
        Err(error) => format!("{name}: {error}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{load_cli, pick_failure, plan, pump, CommandEvent, LoadedCli, Plan, PlannedStep};
    use crate::{
        action_handlers,
        app::{MessageKind, Stoat, UpdateEffect},
        cli_spec::{self, CliSpec, JJ_FISH, RG_HELP},
        host::{FakeTerminalSession, ShellOutput},
        location_picker::LocationEntry,
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, PaneId, View},
        run::{
            kind::{CargoKind, RunKind},
            CmdId, PtyNotification, RunId,
        },
        test_harness::TestHarness,
        theme::scope,
        user_command::{ArgKind, CommandArg, CommandSet, Output, RunSpec, Step, UserCommand},
    };
    use ratatui::style::Color;
    use std::{
        path::{Path, PathBuf},
        sync::Arc,
    };
    use stoat_config::Template;
    use stoat_host::{FakeShell, FakeShellInvocation};

    /// The user command `greet`, which takes one argument and reports its
    /// output in a status message.
    const GREET: &str = r#"command greet { doc = "say hello"; arg who = text; run = shell("echo hello {who}"); output = notify; }"#;

    /// A harness rooted at `/repo` with the user config `source` loaded.
    fn loaded(source: &str) -> TestHarness {
        let mut h = Stoat::test();
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/repo");
        h.stoat.reload_user_config(source);
        h
    }

    /// Open the palette, type `line`, press Enter, and settle the run.
    ///
    /// The palette opens through its action, so a mode with no `:` binding
    /// opens it too.
    fn submit(h: &mut TestHarness, line: &str) {
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        h.type_text(line);
        h.type_keys("enter");
        h.settle();
    }

    /// The status message and its kind.
    fn message(h: &TestHarness) -> (Option<&str>, MessageKind) {
        (
            h.stoat.pending_message.as_deref(),
            h.stoat.pending_message_kind,
        )
    }

    /// The lines that the fake shell ran, in order.
    fn shell_lines(h: &TestHarness) -> Vec<String> {
        h.fake_shell
            .invocations()
            .into_iter()
            .map(|run| run.cmd)
            .collect()
    }

    /// The program and the arguments of the last spawn of the fake terminal
    /// host.
    fn last_spawn(h: &TestHarness) -> Option<(String, Vec<String>)> {
        h.fake_terminal_host()
            .spawns()
            .last()
            .map(|spawn| (spawn.program.clone(), spawn.args.clone()))
    }

    /// The program and the arguments of a `bash -c` of `line`.
    fn bash(line: &str) -> (String, Vec<String>) {
        ("bash".to_owned(), vec!["-c".to_owned(), line.to_owned()])
    }

    fn output(stdout: &[u8], stderr: &[u8], exit_code: i32) -> ShellOutput {
        ShellOutput {
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            exit_code,
        }
    }

    #[test]
    fn the_default_output_plans_the_run_block() {
        let command = UserCommand {
            name: "pr".to_owned(),
            doc: String::new(),
            line: 1,
            args: vec![CommandArg {
                name: "pr".to_owned(),
                kind: ArgKind::Text { default: None },
            }],
            run: RunSpec::Steps {
                steps: vec![Step::Shell(Template::parse("gh pr checkout {pr}"))],
                script: false,
            },
            output: Output::Block,
        };
        assert_eq!(
            plan(
                &command,
                &CommandSet::default(),
                &["412".to_owned()],
                &mut |_| String::new()
            ),
            Plan::Modal(vec![PlannedStep::Shell("gh pr checkout 412".to_owned())]),
        );
    }

    #[test]
    fn a_notify_command_runs_its_line_and_shows_the_first_output_line() {
        let mut h = loaded(GREET);
        h.fake_shell.set_response(
            "echo hello 'big world'",
            output(b"hello big world\nsecond line\n", b"", 0),
        );
        submit(&mut h, "greet big world");

        assert_eq!(
            (
                h.fake_shell.invocations(),
                message(&h),
                h.stoat.command_palette.is_none(),
                h.stoat.active_workspace().palette_history.entries(),
            ),
            (
                vec![FakeShellInvocation {
                    cmd: "echo hello 'big world'".to_owned(),
                    stdin: Vec::new(),
                    cwd: Some(PathBuf::from("/repo")),
                    env: vec![("TERM".to_owned(), Some("dumb".to_owned()))],
                }],
                (Some("hello big world"), MessageKind::Success),
                true,
                &["greet big world".to_owned()][..],
            ),
        );
    }

    /// A failure names the first line of stderr that holds text and the exit
    /// code. A step with no such line names the exit code alone.
    #[test]
    fn a_failed_notify_command_shows_its_first_stderr_line_and_exit_code() {
        let mut h = loaded(GREET);
        h.fake_shell
            .set_response("echo hello x", output(b"", b"\nboom\nmore\n", 2));
        h.fake_shell
            .set_response("echo hello y", output(b"out\n", b"", 3));
        submit(&mut h, "greet x");
        let boom = {
            let (text, kind) = message(&h);
            (text.map(str::to_owned), kind)
        };
        submit(&mut h, "greet y");

        assert_eq!(
            (boom, message(&h)),
            (
                (Some("greet: boom (exit 2)".to_owned()), MessageKind::Alert),
                (Some("greet: exit 3"), MessageKind::Alert),
            ),
        );
    }

    #[test]
    fn a_notify_command_with_no_output_reports_its_name_and_done() {
        let mut h = loaded(GREET);
        h.fake_shell
            .set_response("echo hello x", output(b"\n  \n", b"", 0));
        submit(&mut h, "greet x");
        assert_eq!(message(&h), (Some("greet done"), MessageKind::Success));
    }

    /// A `notify` run that ends on an action shows the action's message and no
    /// message of its own.
    #[test]
    fn a_notify_run_that_ends_on_an_action_shows_no_message_of_its_own() {
        let mut h =
            loaded(r#"command say { arg what = text; run = Notify("{what}"); output = notify; }"#);
        submit(&mut h, "say hi");
        assert_eq!(message(&h), (Some("hi"), MessageKind::Info));
    }

    #[test]
    fn an_action_step_dispatches_with_its_placeholders_filled() {
        let mut h = loaded(r#"command say { arg what = text; run = Notify("{what} done"); }"#);
        submit(&mut h, "say hi there");
        assert_eq!(
            (message(&h), h.fake_shell.invocations()),
            ((Some("hi there done"), MessageKind::Info), Vec::new()),
        );
    }

    /// A `file` or `dir` value that starts with `~/` runs with the home
    /// directory in place of the `~`. A `text` value stays as typed.
    #[test]
    fn a_home_relative_file_value_takes_the_home_directory() {
        let mut h = loaded(concat!(
            "command catnote { arg path = file; arg place = dir; arg note = text; ",
            "run = shell(\"cat {path} {place} {note}\"); output = notify; }",
        ));
        h.fake_env().set("HOME", "/home/tester");
        submit(&mut h, "catnote ~/a.txt ~/docs ~/x");
        assert_eq!(
            shell_lines(&h),
            ["cat /home/tester/a.txt /home/tester/docs '~/x'"]
        );
    }

    /// The user command `title`, whose message defaults to the summary of the
    /// newest commit.
    const TITLE: &str = r#"command title { arg message = text(default: git.last_subject); run = shell("echo {message}"); output = notify; }"#;

    /// The default fills an argument left empty, from the bare name and from
    /// a name with an empty tail. The history records no empty value.
    #[test]
    fn a_literal_default_fills_an_empty_argument() {
        let mut h = loaded(
            r#"command say { arg what = text(default: "hi"); run = shell("echo {what}"); output = notify; }"#,
        );
        submit(&mut h, "say");
        submit(&mut h, "say ");
        submit(&mut h, "say yo");
        assert_eq!(
            (
                shell_lines(&h),
                h.stoat.active_workspace().palette_history.entries(),
            ),
            (
                vec![
                    "echo hi".to_owned(),
                    "echo hi".to_owned(),
                    "echo yo".to_owned()
                ],
                &["say".to_owned(), "say yo".to_owned()][..],
            ),
        );
    }

    #[test]
    fn git_last_subject_reads_the_newest_commit_summary() {
        let mut h = loaded(TITLE);
        h.seed_linear_history(
            "/repo",
            &[
                ("a1b2c3d4", "add the lexer", &[("a.rs", "1\n")]),
                ("b2c3d4e5", "fix the parser", &[("a.rs", "2\n")]),
            ],
        );
        submit(&mut h, "title");
        assert_eq!(shell_lines(&h), ["echo 'fix the parser'"]);
    }

    /// The selection, the file, and the line come from the editor of the
    /// focused pane. The line is the cursor's, which a forward selection draws
    /// on its last char. So the whole second line selected reads line 2,
    /// although its head stands at the start of line 3.
    #[test]
    fn selection_file_and_line_read_the_pane_under_the_palette() {
        let mut h = loaded(concat!(
            "command ctx { arg s = text(default: selection); arg f = text(default: file); ",
            "arg l = text(default: line); run = shell(\"echo {s} {f} {l}\"); output = notify; }\n",
            "command atline { arg l = text(default: line); run = shell(\"echo {l}\"); output = notify; }",
        ));
        h.fake_fs()
            .insert_file("/repo/src/a.rs", "one\ntwo\nthree\n");
        h.open_file(Path::new("/repo/src/a.rs"));
        h.type_keys("j m i w");
        submit(&mut h, "ctx");
        h.type_keys("x");
        submit(&mut h, "atline");
        assert_eq!(shell_lines(&h), ["echo two src/a.rs 2", "echo 2"]);
    }

    /// With no repository, `git.last_subject` reads as the empty string.
    #[test]
    fn a_context_value_with_nothing_to_read_is_empty() {
        let mut h = loaded(TITLE);
        submit(&mut h, "title");
        assert_eq!(shell_lines(&h), ["echo ''"]);
    }

    #[test]
    fn a_command_with_no_argument_runs_from_its_bare_name() {
        let mut h = loaded(r#"command hi { run = Notify("hello"); }"#);
        submit(&mut h, "hi");
        assert_eq!(message(&h), (Some("hello"), MessageKind::Info));
    }

    /// The user command `catlog`, which opens its output in a scratch buffer.
    const DUMP: &str = r#"command catlog { run = shell("cat log"); output = buffer; }"#;

    /// The user command `check`, which runs its line in a Run pane.
    const CHECK: &str = r#"command check { run = shell("make test"); output = run_pane; }"#;

    #[test]
    fn a_buffer_command_opens_its_output_in_a_clean_scratch_buffer() {
        let mut h = loaded(DUMP);
        h.fake_shell
            .set_response("cat log", output(b"a\nb\n", b"", 0));
        submit(&mut h, "catlog");

        let (buffer, cursor) = h.stoat.focused_cursor_pos().expect("an editor has focus");
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.buffer_text(buffer),
                ws.buffers.path_for(buffer).map(Path::to_path_buf),
                ws.buffers
                    .dirty_buffers()
                    .iter()
                    .map(|dirty| dirty.id)
                    .collect::<Vec<_>>(),
                cursor,
            ),
            ("a\nb\n".to_owned(), None, Vec::new(), 0),
        );
    }

    #[test]
    fn a_failed_buffer_command_opens_no_buffer() {
        let mut h = loaded(DUMP);
        h.fake_shell
            .set_response("cat log", output(b"", b"no log\n", 1));
        let before = h.stoat.focused_cursor_pos().map(|(buffer, _)| buffer);
        submit(&mut h, "catlog");

        let after = h.stoat.focused_cursor_pos().map(|(buffer, _)| buffer);
        assert_eq!(
            (after, message(&h)),
            (
                before,
                (Some("catlog: no log (exit 1)"), MessageKind::Alert)
            ),
        );
    }

    /// The command's Cmd joins the stack of a Run pane that splits the editor
    /// pane downward, with its line, its shell text, and its Source Tag, and
    /// the editor pane keeps the focus.
    #[test]
    fn a_run_pane_command_opens_a_run_pane_and_keeps_the_focus() {
        let mut h = loaded(CHECK);
        let editor_pane = h.stoat.active_workspace().panes.focus();
        submit(&mut h, "check");

        let ws = h.stoat.active_workspace();
        let shown: Vec<(PaneId, RunId)> = ws
            .panes
            .split_panes()
            .filter_map(|(pane, shown)| match shown.view {
                View::Run(run) => Some((pane, run)),
                _ => None,
            })
            .collect();
        let [(run_pane, run)] = shown[..] else {
            panic!("one Run pane, not {}", shown.len());
        };
        let (above, below) = (
            ws.panes.pane(editor_pane).area,
            ws.panes.pane(run_pane).area,
        );
        let stack: Vec<_> = ws.runs[run]
            .blocks
            .iter()
            .map(|id| {
                let cmd = &ws.cmds[*id];
                (
                    cmd.command.as_str(),
                    cmd.shell.as_deref(),
                    cmd.source.map(|(label, _)| label),
                )
            })
            .collect();
        assert_eq!(
            (
                ws.panes.focus() == editor_pane,
                (below.x, below.y > above.y),
                stack,
                last_spawn(&h),
                h.fake_shell.invocations(),
            ),
            (
                true,
                (above.x, true),
                vec![("check", Some("make test"), Some("config"))],
                Some(bash("make test")),
                Vec::new()
            ),
        );
    }

    /// A Run pane on show takes the Cmd onto its stack, no second run opens,
    /// and the focus stays on the editor pane.
    #[test]
    fn a_run_pane_command_uses_the_run_pane_on_show() {
        let mut h = loaded(CHECK);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenRun);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
        let editor_pane = h.stoat.active_workspace().panes.focus();
        submit(&mut h, "check");

        let ws = h.stoat.active_workspace();
        let stacks: Vec<Vec<&str>> = ws
            .runs
            .values()
            .map(|run| {
                run.blocks
                    .iter()
                    .map(|id| ws.cmds[*id].command.as_str())
                    .collect()
            })
            .collect();
        assert_eq!(
            (stacks, ws.panes.focus() == editor_pane, last_spawn(&h)),
            (vec![vec!["check"]], true, Some(bash("make test"))),
        );
    }

    /// A Run pane command leaves the focus in the dock that held it.
    #[test]
    fn a_run_pane_command_leaves_the_focus_in_a_dock() {
        let mut h = loaded(CHECK);
        let ws = h.stoat.active_workspace_mut();
        let dock = ws.docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Open { width: 10 },
            default_width: 10,
            area: Default::default(),
        });
        ws.focus = FocusTarget::Dock(dock);
        submit(&mut h, "check");

        let ws = h.stoat.active_workspace();
        let stacked: usize = ws.runs.values().map(|run| run.blocks.len()).sum();
        assert_eq!((ws.focus, stacked), (FocusTarget::Dock(dock), 1));
    }

    /// The user command `ship` under the output `output`, which tests, pushes,
    /// and opens a pull request titled after the newest commit, and then says
    /// so.
    fn ship(output: &str) -> String {
        format!(
            "command ship {{ arg message = text(default: git.last_subject); \
             run = [shell(\"cargo test\"), shell(\"git push -u origin HEAD\"), \
             shell(\"gh pr create --fill --title {{message}}\"), Notify(\"shipped\")]; \
             output = {output}; }}"
        )
    }

    /// The step texts of [`ship`] for the newest commit `fix the parser`.
    const SHIP_STEPS: [&str; 3] = [
        "cargo test",
        "git push -u origin HEAD",
        "gh pr create --fill --title 'fix the parser'",
    ];

    /// A harness with [`ship`] under `output` loaded over a repository whose
    /// newest commit reads `fix the parser`.
    fn ship_harness(output: &str) -> TestHarness {
        let h = loaded(&ship(output));
        h.seed_linear_history(
            "/repo",
            &[("a1b2c3d4", "fix the parser", &[("a.rs", "1\n")])],
        );
        h
    }

    /// The user command `mid` under the output `output`, whose action step
    /// stands between two shell steps.
    fn mid(output: &str) -> String {
        format!(
            "command mid {{ run = [shell(\"true\"), Notify(\"half\"), shell(\"echo end\")]; \
             output = {output}; }}"
        )
    }

    /// Queue a session of the fake terminal host of `h` for each of the next
    /// `count` spawns, in order.
    fn queue_sessions(h: &TestHarness, count: usize) -> Vec<Arc<FakeTerminalSession>> {
        (0..count)
            .map(|_| {
                let session = Arc::new(FakeTerminalSession::new());
                h.fake_terminal_host().push_session(session.clone());
                session
            })
            .collect()
    }

    /// The program and the arguments of each spawn of the fake terminal host,
    /// in order.
    fn spawns(h: &TestHarness) -> Vec<(String, Vec<String>)> {
        h.fake_terminal_host()
            .spawns()
            .into_iter()
            .map(|spawn| (spawn.program, spawn.args))
            .collect()
    }

    /// The one Cmd of the store of the active workspace.
    fn only_cmd(h: &TestHarness) -> CmdId {
        let ids: Vec<CmdId> = h.stoat.active_workspace().cmds.keys().collect();
        let [id] = ids[..] else {
            panic!("one Cmd, not {}", ids.len());
        };
        id
    }

    /// A script under `run_pane` is one Cmd on a Run pane's stack. Each shell
    /// step spawns once the step before it exits 0, under a row of its text in
    /// the muted color, and the last action shows its message once the last
    /// shell step ends.
    #[test]
    fn a_script_runs_its_steps_as_one_cmd() {
        let mut h = ship_harness("run_pane");
        let sessions = queue_sessions(&h, 3);
        submit(&mut h, "ship");
        let mut spawned = vec![spawns(&h).len()];
        for session in &sessions {
            session.finish(0);
            h.settle();
            spawned.push(spawns(&h).len());
        }

        let cmd = only_cmd(&h);
        let ws = h.stoat.active_workspace();
        let block = &ws.cmds[cmd];
        let stacked: Vec<CmdId> = ws
            .runs
            .values()
            .flat_map(|run| run.blocks.clone())
            .collect();
        let rows: Vec<Vec<(char, Option<Color>)>> = (0..SHIP_STEPS.len())
            .map(|row| {
                block
                    .grid
                    .row(row)
                    .iter()
                    .map(|cell| (cell.ch, cell.fg))
                    .collect()
            })
            .collect();
        let muted = h.stoat.theme.get(scope::UI_TEXT_MUTED).fg;
        assert_eq!(
            (
                (
                    stacked,
                    block.command.as_str(),
                    block.source.map(|(label, _)| label)
                ),
                (spawned, spawns(&h)),
                message(&h),
                (block.finished, block.exit_status, block.run_count),
                rows,
                h.fake_shell.invocations(),
            ),
            (
                (vec![cmd], "ship", Some("script")),
                (vec![1, 2, 3, 3], SHIP_STEPS.map(bash).to_vec()),
                (Some("shipped"), MessageKind::Info),
                (true, Some(0), 1),
                SHIP_STEPS
                    .map(|text| text.chars().map(|ch| (ch, muted)).collect())
                    .to_vec(),
                Vec::new(),
            ),
        );
    }

    /// A step that exits 1 ends the Cmd with that status, and no later step
    /// runs, so the closing action shows no message.
    #[test]
    fn a_failed_step_ends_the_cmd_and_no_later_step_runs() {
        let mut h = ship_harness("run_pane");
        let sessions = queue_sessions(&h, 2);
        h.stoat.pending_message = None;
        submit(&mut h, "ship");
        sessions[0].finish(0);
        h.settle();
        sessions[1].finish(1);
        h.settle();

        let cmd = only_cmd(&h);
        assert_eq!(
            (
                spawns(&h).len(),
                h.cmd_status(cmd),
                h.stoat.pending_message.as_deref()
            ),
            (2, "script \u{2717} exit 1 0ms".to_owned(), None),
        );
    }

    /// An action step between two shell steps dispatches once the first one
    /// exits 0, and the second one spawns after it. The palette's Run Block
    /// holds the Cmd.
    #[test]
    fn an_action_step_dispatches_between_two_shell_steps() {
        let mut h = loaded(&mid("block"));
        let sessions = queue_sessions(&h, 2);
        submit(&mut h, "mid");
        let shown = h.stoat.run_block_cmd();
        sessions[0].finish(0);
        h.settle();

        assert_eq!(
            (shown, h.stoat.pending_message.as_deref(), last_spawn(&h)),
            (Some(only_cmd(&h)), Some("half"), Some(bash("echo end"))),
        );
    }

    /// A leading action step dispatches as the run starts, and the shell step
    /// after it spawns.
    #[test]
    fn a_leading_action_step_runs_before_the_first_process() {
        let mut h = loaded(
            r#"command lead { run = [Notify("first"), shell("make")]; output = run_pane; }"#,
        );
        submit(&mut h, "lead");

        let cmd = only_cmd(&h);
        assert_eq!(
            (
                h.stoat.pending_message.as_deref(),
                spawns(&h),
                h.stoat.active_workspace().cmds[cmd].step,
            ),
            (Some("first"), vec![bash("make")], 1),
        );
    }

    /// `Ctrl-r` on a finished script's block starts a second run from the
    /// first step.
    #[test]
    fn ctrl_r_runs_every_step_again() {
        let mut h = ship_harness("run_pane");
        let sessions = queue_sessions(&h, 4);
        submit(&mut h, "ship");
        for session in &sessions[..3] {
            session.finish(0);
            h.settle();
        }
        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusDown);
        h.type_keys("ctrl-r");

        let block = &h.stoat.active_workspace().cmds[only_cmd(&h)];
        assert_eq!(
            (
                spawns(&h).len(),
                last_spawn(&h),
                (block.run_count, block.step, block.finished),
            ),
            (4, Some(bash("cargo test")), (2, 0, false)),
        );
    }

    /// The first step exits 0 after another workspace became active, so the
    /// action step after it does not run, and the Cmd ends with the line that
    /// says why.
    #[test]
    fn an_action_step_stops_when_its_workspace_is_not_on_screen() {
        let mut h = loaded(&mid("run_pane"));
        let sessions = queue_sessions(&h, 1);
        h.stoat.pending_message = None;
        submit(&mut h, "mid");
        let (home, cmd) = (h.stoat.active_workspace, only_cmd(&h));
        let other = h.create_workspace();
        h.set_active_workspace(other);
        sessions[0].finish(0);
        h.settle();

        let block = &h.stoat.workspaces[home].cmds[cmd];
        assert_eq!(
            (
                (block.error.as_deref(), block.exit_status, block.finished),
                h.stoat.pending_message.as_deref(),
                spawns(&h).len(),
            ),
            (
                (Some("stopped, the workspace changed"), None, true),
                None,
                1
            ),
        );
    }

    /// A step whose output asks for the alternate screen hands off and ends
    /// the plan, so the step after it does not run when its process exits 0.
    #[test]
    fn a_step_that_hands_off_ends_the_plan() {
        let mut h = loaded(
            r#"command tui { run = [shell("mytui"), shell("echo after")]; output = run_pane; }"#,
        );
        submit(&mut h, "tui");
        let cmd = only_cmd(&h);
        h.inject_cmd_output(cmd, b"\x1b[?1049h");
        h.inject_cmd_done(cmd, 0);

        let block = &h.stoat.active_workspace().cmds[cmd];
        assert_eq!(
            (spawns(&h), block.finished, block.step),
            (vec![bash("mytui"), bash("mytui")], true, 0),
        );
    }

    /// A script keeps the kind of a plain line when it runs again, though its
    /// last step reads as a test run.
    #[test]
    fn a_script_keeps_its_line_kind_when_it_runs_again() {
        let mut h = loaded(
            r#"command chk { run = [shell("cargo fmt"), shell("cargo test")]; output = run_pane; }"#,
        );
        submit(&mut h, "chk");
        let cmd = only_cmd(&h);
        h.inject_cmd_done(cmd, 0);
        h.inject_cmd_done(cmd, 0);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusDown);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::RunAgain);

        let block = &h.stoat.active_workspace().cmds[cmd];
        assert_eq!((block.run_count, block.kind), (2, RunKind::Line));
    }

    /// A plan of one `cargo test` step keeps the report kind of its text, and
    /// a plan of several steps reads as a plain line.
    #[test]
    fn a_plan_of_one_cargo_step_keeps_its_report_kind() {
        let kind = |source: &str, line: &str| {
            let mut h = loaded(source);
            submit(&mut h, line);
            h.stoat.active_workspace().cmds[only_cmd(&h)].kind
        };
        assert_eq!(
            [
                kind(r#"command t { run = shell("cargo test"); }"#, "t"),
                kind(&ship("run_pane"), "ship"),
            ],
            [RunKind::Parsed(CargoKind::Test), RunKind::Line],
        );
    }

    /// A script of action steps alone dispatches them and starts no Cmd.
    #[test]
    fn a_run_of_action_steps_alone_starts_no_cmd() {
        let mut h = loaded(r#"command hi { run = [Notify("hi")]; output = run_pane; }"#);
        submit(&mut h, "hi");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                h.stoat.pending_message.as_deref(),
                ws.cmds.len(),
                ws.runs.len()
            ),
            (Some("hi"), 0, 0),
        );
    }

    /// An action step whose effect is Quit quits wherever the run reaches it.
    /// That holds as the run starts, after a shell step exits 0, and as a run
    /// again starts.
    #[test]
    fn an_action_step_that_quits_quits() {
        let mut h = loaded(concat!(
            "command bye { run = [ForceQuitAll(), shell(\"true\"), ForceQuitAll()]; ",
            "output = run_pane; }",
        ));
        let command = h.stoat.commands.get("bye").cloned().expect("bye loads");
        let started = super::start(&mut h.stoat, &command, Vec::new(), "bye");
        let cmd = only_cmd(&h);
        let exited = h.stoat.handle_pty_notification(PtyNotification::CmdExited {
            workspace: h.stoat.active_workspace,
            cmd,
            run: 1,
            exit_status: Some(0),
        });
        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusDown);
        let again = action_handlers::dispatch(&mut h.stoat, &stoat_action::RunAgain);

        assert_eq!([started, exited, again], [UpdateEffect::Quit; 3]);
    }

    /// The steps after the failed one do not run, and the run names the
    /// failure.
    #[test]
    fn a_failed_step_stops_the_steps_after_it() {
        let mut h = ship_harness("notify");
        h.fake_shell
            .set_response("git push -u origin HEAD", output(b"", b"rejected\n", 1));
        submit(&mut h, "ship");
        assert_eq!(
            (
                shell_lines(&h),
                message(&h),
                h.stoat.active_workspace().runs.len()
            ),
            (
                vec![
                    "cargo test".to_owned(),
                    "git push -u origin HEAD".to_owned()
                ],
                (Some("ship: rejected (exit 1)"), MessageKind::Alert),
                0,
            ),
        );
    }

    #[test]
    fn a_script_runs_every_step_in_order_and_its_last_action_has_the_last_word() {
        let mut h = ship_harness("notify");
        submit(&mut h, "ship");
        assert_eq!(
            (shell_lines(&h), message(&h)),
            (
                vec![
                    "cargo test".to_owned(),
                    "git push -u origin HEAD".to_owned(),
                    "gh pr create --fill --title 'fix the parser'".to_owned(),
                ],
                (Some("shipped"), MessageKind::Info),
            ),
        );
    }

    /// The command `co`, which checks out a pull request, and `rv`, whose run
    /// checks one out through `co` and then shows a message.
    const CO_RV: &str = concat!(
        "command co { arg pr = text; run = shell(\"gh pr checkout {pr}\"); output = notify; }\n",
        "command rv { arg pr = same_as(co); run = [co, Notify(\"after\")]; output = notify; }",
    );

    #[test]
    fn a_command_step_runs_with_the_caller_s_values() {
        let mut h = loaded(CO_RV);
        submit(&mut h, "rv 412");
        assert_eq!(
            (shell_lines(&h), message(&h)),
            (
                vec!["gh pr checkout 412".to_owned()],
                (Some("after"), MessageKind::Info)
            ),
        );
    }

    /// An argument of a step's command that the caller does not declare takes
    /// that command's default.
    #[test]
    fn a_command_step_takes_its_own_default_for_an_argument_the_caller_lacks() {
        let mut h = loaded(concat!(
            "command hail { arg who = text(default: \"world\"); ",
            "run = shell(\"echo hello {who}\"); }\n",
            "command wave { run = [hail, Notify(\"waved\")]; output = notify; }",
        ));
        submit(&mut h, "wave");
        assert_eq!(
            (shell_lines(&h), message(&h)),
            (
                vec!["echo hello world".to_owned()],
                (Some("waved"), MessageKind::Info)
            ),
        );
    }

    #[test]
    fn buffer_collects_the_stdout_of_every_shell_step() {
        let mut h = loaded(
            r#"command twologs { run = [shell("cat a.log"), shell("cat b.log")]; output = buffer; }"#,
        );
        h.fake_shell
            .set_response("cat a.log", output(b"a\n", b"", 0));
        h.fake_shell
            .set_response("cat b.log", output(b"b\n", b"", 0));
        submit(&mut h, "twologs");

        let (buffer, _) = h.stoat.focused_cursor_pos().expect("an editor has focus");
        assert_eq!(h.buffer_text(buffer), "a\nb\n");
    }

    /// A `buffer` command at the Run pane prompt opens its output in the
    /// editor pane beside the Run pane, which takes the focus, and the Run
    /// pane keeps its stack.
    #[test]
    fn a_buffer_command_at_the_run_prompt_opens_beside_the_run_pane() {
        let mut h = loaded(r#"command alog { run = shell("cat a.log"); output = buffer; }"#);
        h.fake_shell
            .set_response("cat a.log", output(b"a\n", b"", 0));
        let editor_pane = h.stoat.active_workspace().panes.focus();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        let run = h.open_run();
        let cmd = h.submit_run("true").cmd;
        h.submit_run("alog");
        h.settle();

        let (buffer, _) = h.stoat.focused_cursor_pos().expect("an editor has focus");
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.panes.focus() == editor_pane,
                h.buffer_text(buffer),
                ws.runs[run].blocks.clone(),
            ),
            (true, "a\n".to_owned(), vec![cmd]),
        );
    }

    /// A `buffer` command opens its output in a focused pane that shows no Run
    /// pane, a terminal's included, and the focus stays on that pane.
    #[test]
    fn a_buffer_command_over_a_terminal_opens_in_its_pane() {
        let mut h = loaded(r#"command alog { run = shell("cat a.log"); output = buffer; }"#);
        h.fake_shell
            .set_response("cat a.log", output(b"a\n", b"", 0));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Terminal);
        let terminal_pane = h.stoat.active_workspace().panes.focus();
        submit(&mut h, "alog");

        let (buffer, _) = h.stoat.focused_cursor_pos().expect("an editor has focus");
        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.panes.focus() == terminal_pane,
                ws.panes.split_panes().count(),
                h.buffer_text(buffer),
            ),
            (true, 1, "a\n".to_owned()),
        );
    }

    /// The shell step finishes after another workspace became active, so the
    /// action after it does not run, and the run says why.
    #[test]
    fn an_action_step_does_not_run_after_the_workspace_changed() {
        let mut h = loaded(
            r#"command nap { run = [shell("sleep 1"), Notify("after")]; output = notify; }"#,
        );
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        h.type_text("nap");
        h.hold_blocking();
        h.type_keys("enter");
        h.settle();
        let other = h.create_workspace();
        h.set_active_workspace(other);
        h.run_held_blocking();
        h.settle();
        assert_eq!(
            (shell_lines(&h), message(&h)),
            (
                vec!["sleep 1".to_owned()],
                (
                    Some("nap: stopped, the workspace changed"),
                    MessageKind::Alert
                ),
            ),
        );
    }

    /// The user command `find`, which opens the locations that `rg` prints.
    const FIND: &str =
        r#"command find { arg what = text; run = shell("rg -n {what}"); output = locations; }"#;

    #[test]
    fn a_locations_command_opens_the_location_picker() {
        let mut h = loaded(FIND);
        h.fake_fs()
            .insert_file("/repo/src/a.rs", "one\ntwo three\n");
        h.fake_shell.set_response(
            "rg -n three",
            output(b"src/a.rs:2:5:two three\nnoise\n", b"", 0),
        );
        submit(&mut h, "find three");

        let entries = h
            .stoat
            .location_picker
            .as_ref()
            .map(|picker| picker.entries().to_vec());
        assert_eq!(
            entries,
            Some(vec![LocationEntry {
                path: PathBuf::from("/repo/src/a.rs"),
                offset: 8,
                line: 2,
                column: 5,
                text: "two three".to_owned(),
                block: None,
            }]),
        );
    }

    /// A run from the palette that names no location becomes a finished Cmd on
    /// a new Run pane under the editor pane, with no message and no picker.
    /// The editor pane keeps the focus.
    #[test]
    fn a_locations_command_with_no_location_goes_to_a_run_pane() {
        let mut h = loaded(FIND);
        h.fake_shell.set_response("rg -n x", output(b"", b"", 1));
        let editor_pane = h.stoat.active_workspace().panes.focus();
        h.stoat.pending_message = None;
        submit(&mut h, "find x");

        let panes = run_panes(&h);
        let [(run_pane, run)] = panes[..] else {
            panic!("one Run pane, not {}", panes.len());
        };
        let ws = h.stoat.active_workspace();
        let stack: Vec<_> = ws.runs[run]
            .blocks
            .iter()
            .map(|id| {
                let cmd = &ws.cmds[*id];
                (
                    cmd.command.as_str(),
                    cmd.source.map(|(label, _)| label),
                    (cmd.finished, cmd.exit_status),
                    cmd.grid.rendered_line_count(),
                )
            })
            .collect();
        assert_eq!(
            (
                (
                    h.stoat.pending_message.as_deref(),
                    h.stoat.location_picker.is_none()
                ),
                (
                    ws.panes.focus() == editor_pane,
                    ws.panes.pane(run_pane).area.y > ws.panes.pane(editor_pane).area.y,
                ),
                stack,
            ),
            (
                (None, true),
                (true, true),
                vec![("find x", Some("config"), (true, Some(1)), 0)]
            ),
        );
    }

    /// A failed run from the palette that names no location shows its stdout
    /// and then its stderr in its block on a Run pane, each line from the
    /// left edge, and its status names its exit code.
    #[test]
    fn a_failed_locations_command_shows_its_stderr_in_a_run_pane() {
        let mut h = loaded(FIND);
        h.fake_shell.set_response(
            "rg -n three",
            output(b"no match here\n", b"rg: bad regex\n", 2),
        );
        submit(&mut h, "find three");

        let cmd = only_cmd(&h);
        let grid = &h.stoat.active_workspace().cmds[cmd].grid;
        let rows: Vec<String> = (0..grid.rendered_line_count())
            .map(|row| grid.text_in(0..usize::from(grid.width()), row..row + 1))
            .collect();
        assert_eq!(
            (rows, h.cmd_status(cmd)),
            (
                vec!["no match here".to_owned(), "rg: bad regex".to_owned()],
                "config \u{2717} exit 2 0ms".to_owned()
            ),
        );
    }

    /// `C-r` on the block of a worker run starts the command again on its own
    /// PTY, as a `bash -c` of its shell line, and the second run's block shows
    /// its matches.
    #[test]
    fn a_cmd_from_a_worker_run_runs_again_on_its_own_pty() {
        let mut h = loaded(FIND);
        h.fake_shell.set_response("rg -n x", output(b"", b"", 1));
        submit(&mut h, "find x");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusDown);
        h.type_keys("ctrl-r");
        let spawned = last_spawn(&h);
        let cmd = only_cmd(&h);
        h.inject_cmd_output(cmd, b"src/a.rs:2:5:two three\r\n");
        h.inject_cmd_done(cmd, 0);

        assert_eq!(
            (spawned, h.cmd_status(cmd)),
            (
                Some(bash("rg -n x")),
                "config \u{2713} 1 match \u{b7} 0ms \u{b7} run 2".to_owned()
            ),
        );
    }

    /// From the palette a `locations` run goes on a worker and opens the
    /// location picker, also over a focused Run pane.
    #[test]
    fn a_locations_command_from_the_palette_over_a_run_pane_opens_the_picker() {
        let mut h = loaded(FIND);
        h.fake_fs()
            .insert_file("/repo/src/a.rs", "one\ntwo three\n");
        h.fake_shell
            .set_response("rg -n three", output(b"src/a.rs:2:5:two three\n", b"", 0));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenRun);
        submit(&mut h, "find three");

        assert_eq!(
            (
                shell_lines(&h),
                h.stoat
                    .location_picker
                    .as_ref()
                    .map(|picker| picker.entries().len()),
                h.stoat.active_workspace().cmds.len(),
            ),
            (vec!["rg -n three".to_owned()], Some(1), 0),
        );
    }

    /// A run that starts with a dock in focus is not at a Run pane prompt, so
    /// it goes on a worker, though the split focus shows a Run pane.
    #[test]
    fn a_locations_run_with_a_dock_in_focus_goes_on_a_worker() {
        let mut h = loaded(FIND);
        h.fake_shell.set_response("rg -n x", output(b"", b"", 1));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenRun);
        let ws = h.stoat.active_workspace_mut();
        let dock = ws.docks.insert(DockPanel {
            view: View::Label("dock".into()),
            side: DockSide::Right,
            visibility: DockVisibility::Open { width: 10 },
            default_width: 10,
            area: Default::default(),
        });
        ws.focus = FocusTarget::Dock(dock);
        let command = h.stoat.commands.get("find").cloned().expect("find loads");
        super::start(&mut h.stoat, &command, vec!["x".to_owned()], "find x");
        h.settle();

        assert_eq!(shell_lines(&h), vec!["rg -n x".to_owned()]);
    }

    /// A run that names no location after its workspace left the screen opens
    /// no Run pane, and it says that it found none.
    #[test]
    fn a_run_whose_workspace_left_says_it_found_no_location() {
        let mut h = loaded(FIND);
        h.fake_shell.set_response("rg -n x", output(b"", b"", 1));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        h.type_text("find x");
        h.hold_blocking();
        h.type_keys("enter");
        h.settle();
        let home = h.stoat.active_workspace;
        let other = h.create_workspace();
        h.set_active_workspace(other);
        h.run_held_blocking();
        h.settle();

        assert_eq!(
            (
                message(&h),
                h.stoat.workspaces[home].cmds.len(),
                h.stoat.active_workspace().cmds.len(),
            ),
            ((Some("no locations"), MessageKind::Info), 0, 0),
        );
    }

    /// The split panes of `h` that show a Run pane, each with its run.
    fn run_panes(h: &TestHarness) -> Vec<(PaneId, RunId)> {
        h.stoat
            .active_workspace()
            .panes
            .split_panes()
            .filter_map(|(pane, shown)| match shown.view {
                View::Run(run) => Some((pane, run)),
                _ => None,
            })
            .collect()
    }

    /// A failed pick list names the first stderr line that holds text, or the
    /// exit code of a run with none. A run that did not start names its error.
    #[test]
    fn a_pick_failure_names_its_stderr_or_its_exit_code() {
        assert_eq!(
            [
                pick_failure(&Ok(output(b"", b"\n  gh: offline \nmore\n", 1))),
                pick_failure(&Ok(output(b"rows\n", b" \n", 3))),
                pick_failure(&Err(std::io::Error::other("no shell"))),
            ],
            ["gh: offline", "exit 3", "no shell"],
        );
    }

    /// A pick result of another run drops, because the palette's list belongs
    /// to the run of its own generation.
    #[test]
    fn a_pick_result_of_another_generation_drops() {
        let mut h = loaded(
            r#"command peek { arg f = pick("ls"); run = shell("cat {f}"); output = notify; }"#,
        );
        h.fake_shell.set_response("ls", output(b"x\n", b"", 0));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
        h.type_text("peek ");
        let _ = h.snapshot();
        h.settle();
        let labels = |h: &TestHarness| {
            let pick = h
                .stoat
                .command_palette
                .as_ref()
                .and_then(|palette| palette.pick.as_ref())
                .expect("a pick list");
            let labels: Vec<String> = pick.list.rows.iter().map(|row| row.label.clone()).collect();
            (pick.generation, labels)
        };
        let (generation, landed) = labels(&h);

        h.stoat
            .command_events
            .lock()
            .expect("command events mutex")
            .push(CommandEvent::PickList {
                generation: generation + 1,
                result: Ok(output(b"a\nb\n", b"", 0)),
            });
        pump(&mut h.stoat);
        assert_eq!(
            (landed, labels(&h).1),
            (vec!["x".to_owned()], vec!["x".to_owned()])
        );
    }

    /// A run whose locations name no file that reads says so, as a run with
    /// no location does.
    #[test]
    fn a_locations_command_whose_files_do_not_read_says_so() {
        let mut h = loaded(FIND);
        h.fake_shell
            .set_response("rg -n three", output(b"src/gone.rs:1:1:three\n", b"", 0));
        submit(&mut h, "find three");
        assert_eq!(
            (message(&h), h.stoat.location_picker.is_none()),
            ((Some("no locations"), MessageKind::Info), true),
        );
    }

    /// The `cli` command `rg`, which opens the locations that `rg` prints.
    const RG: &str = r#"command rg = cli("rg", output: locations);"#;

    #[test]
    fn a_wrap_command_runs_the_tool_with_the_typed_tail() {
        let mut h = loaded(RG);
        h.fake_fs()
            .insert_file("/repo/src/a.rs", "one\ntwo three\n");
        h.fake_shell.set_response(
            "rg -n three src",
            output(b"src/a.rs:2:5:two three\n", b"", 0),
        );
        submit(&mut h, "rg -n three src");

        let entries = h
            .stoat
            .location_picker
            .as_ref()
            .map(|picker| picker.entries().to_vec());
        assert_eq!(
            (shell_lines(&h), entries),
            (
                vec![
                    "rg --help".to_owned(),
                    "rg --version".to_owned(),
                    "rg -n three src".to_owned()
                ],
                Some(vec![LocationEntry {
                    path: PathBuf::from("/repo/src/a.rs"),
                    offset: 8,
                    line: 2,
                    column: 5,
                    text: "two three".to_owned(),
                    block: None,
                }]),
            ),
        );
    }

    /// Enter on the bare name opens the entry of the tail, and Enter on an
    /// empty tail runs the tool alone.
    #[test]
    fn a_wrap_command_with_no_tail_runs_the_tool_alone() {
        let mut h = loaded(r#"command st = cli("git status", output: notify);"#);
        submit(&mut h, "st");
        let entry = h
            .stoat
            .command_palette
            .as_ref()
            .map(|palette| palette.input.text(h.stoat.active_workspace()));
        h.type_keys("enter");
        h.settle();
        assert_eq!(
            (entry.as_deref(), shell_lines(&h)),
            (
                Some("st "),
                vec![
                    "git status --help".to_owned(),
                    "git status --version".to_owned(),
                    "git status".to_owned()
                ]
            ),
        );
    }

    /// The tail goes to the shell as typed, so a quote and a glob keep their
    /// shell meaning.
    #[test]
    fn the_tail_keeps_its_quotes_and_globs() {
        let mut h = loaded(RG);
        submit(&mut h, r#"rg "a b" *.rs"#);
        assert_eq!(
            shell_lines(&h),
            ["rg --help", "rg --version", r#"rg "a b" *.rs"#]
        );
    }

    /// The help of `tool`, which declares no completions, loaded through
    /// `shell` from `/repo`.
    fn loaded_help(shell: &FakeShell, tool: &str) -> LoadedCli {
        load_cli(shell, tool, None, Path::new("/repo"), &[])
    }

    /// The lines that `shell` ran, in order.
    fn ran(shell: &FakeShell) -> Vec<String> {
        shell.invocations().into_iter().map(|run| run.cmd).collect()
    }

    /// The version is the first line of the version run's stdout.
    #[test]
    fn load_cli_reads_the_help_and_the_version() {
        let shell = FakeShell::new();
        shell.set_response("rg --help", output(RG_HELP.as_bytes(), b"", 0));
        shell.set_response(
            "rg --version",
            output(b"ripgrep 14.1.0\n\nfeatures:+pcre2\n", b"", 0),
        );
        assert_eq!(
            loaded_help(&shell, "rg"),
            LoadedCli {
                spec: cli_spec::parse_help(RG_HELP),
                source: "rg --help".to_owned(),
                version: Some("ripgrep 14.1.0".to_owned()),
                missing: false,
            },
        );
    }

    /// A help run that exits 127 marks the tool as missing, and no version
    /// run follows it.
    #[test]
    fn a_tool_that_is_not_installed_loads_as_missing() {
        let shell = FakeShell::new();
        shell.set_response("nope --help", output(b"", b"sh: nope: not found\n", 127));
        assert_eq!(
            (loaded_help(&shell, "nope"), ran(&shell)),
            (
                LoadedCli {
                    spec: CliSpec::default(),
                    source: "nope --help".to_owned(),
                    version: None,
                    missing: true,
                },
                vec!["nope --help".to_owned()],
            ),
        );
    }

    /// A help on stderr with an empty stdout reads as the help, whatever the
    /// exit code. A version run that prints nothing gives no version.
    #[test]
    fn help_on_stderr_is_read() {
        let shell = FakeShell::new();
        shell.set_response("old --help", output(b"\n", RG_HELP.as_bytes(), 2));
        assert_eq!(
            loaded_help(&shell, "old"),
            LoadedCli {
                spec: cli_spec::parse_help(RG_HELP),
                source: "old --help".to_owned(),
                version: None,
                missing: false,
            },
        );
    }

    /// A tool that declares `completions` runs that command and no help run,
    /// and the command's fish script gives the spec.
    #[test]
    fn completions_take_the_place_of_the_help_run() {
        let shell = FakeShell::new();
        shell.set_response(
            "jj util completion fish",
            output(JJ_FISH.as_bytes(), b"", 0),
        );
        shell.set_response("jj --version", output(b"jj 0.30.0\n", b"", 0));
        let loaded = load_cli(
            &shell,
            "jj",
            Some("jj util completion fish"),
            Path::new("/repo"),
            &[],
        );
        assert_eq!(
            (loaded, ran(&shell)),
            (
                LoadedCli {
                    spec: cli_spec::parse_fish_completions(JJ_FISH, "jj"),
                    source: "jj util completion fish".to_owned(),
                    version: Some("jj 0.30.0".to_owned()),
                    missing: false,
                },
                vec![
                    "jj util completion fish".to_owned(),
                    "jj --version".to_owned()
                ],
            ),
        );
    }
}
