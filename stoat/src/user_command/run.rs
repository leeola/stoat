use super::{context, pick, ArgKind, Output, Step, UserCommand};
use crate::{
    action_handlers,
    app::{MessageKind, Stoat, UpdateEffect},
    buffer_lifecycle,
    command_palette::{ArgRow, PickLoad},
    host::ShellOutput,
    keymap::ResolvedAction,
    location_picker::{self, LocationEntry, ParsedLocation},
    picker::PREVIEW_BYTE_LIMIT,
};
use std::{
    collections::VecDeque,
    io, mem,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
};
use stoat_config::{Template, Value};

/// The id of the next [`CommandRun`], unique in the process, so a shell result
/// finds the run that started it.
static NEXT_RUN: AtomicU64 = AtomicU64::new(0);

/// A step of a command run, with each placeholder written.
#[derive(Debug, PartialEq)]
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
    /// The shell line that the run modal runs and shows.
    Modal(String),
    /// The shell line that a Run pane's shell runs.
    Pane(String),
}

/// A command run with steps left, or with a shell step out on a worker.
pub(crate) struct CommandRun {
    id: u64,
    name: String,
    output: Output,
    steps: VecDeque<PlannedStep>,
    /// The stdout of the last shell step, which the success message reads.
    last_stdout: String,
    /// Whether the last step was an action. A run that ends on an action
    /// shows no message of its own.
    last_was_action: bool,
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
}

/// Run `command` with `values`, one value for each of its arguments.
///
/// A shell step under the `block` output runs in the run modal. Any other
/// step runs here, a shell step on a worker with no terminal, so its stdout
/// and its stderr stay apart.
///
/// An empty value of an argument with a default runs with the default, as
/// [`context::default_text`] reads it. A `file` or `dir` value that starts
/// with `~/` runs with the `HOME` directory in place of the `~`.
pub(crate) fn start(stoat: &mut Stoat, command: &UserCommand, values: Vec<String>) -> UpdateEffect {
    let values = fill_defaults(stoat, command, values);
    let values = expand_home(command, values, stoat.env_host.var("HOME").as_deref());
    match plan(command, &values) {
        Plan::Modal(line) => action_handlers::run::run_command(stoat, &line),
        Plan::Pane(line) => action_handlers::run::run_line_in_pane(stoat, &line),
        Plan::Steps(steps) => {
            let run = CommandRun {
                id: NEXT_RUN.fetch_add(1, Ordering::Relaxed),
                name: command.name.clone(),
                output: command.output,
                steps,
                last_stdout: String::new(),
                last_was_action: false,
            };
            advance(stoat, run)
        },
    }
}

/// Take the results of the shell steps that finished, and carry each run on.
///
/// Answers whether a result landed. A step that exits 0 hands its stdout to
/// the rest of the run. Any other result ends the run with a message that
/// names the failure. The result of a pick list's command goes to the
/// palette's list, as [`land_pick`] reads it.
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
        };
        let Some(index) = stoat.command_runs.iter().position(|held| held.id == run) else {
            continue;
        };
        let mut run = stoat.command_runs.remove(index);
        match result {
            Ok(output) if run.output == Output::Locations => end_locations(stoat, &run, output),
            Ok(output) if output.exit_code == 0 => {
                run.last_stdout = String::from_utf8_lossy(&output.stdout).into_owned();
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
/// `~` in a quoted word.
fn expand_home(command: &UserCommand, values: Vec<String>, home: Option<&str>) -> Vec<String> {
    let Some(home) = home else {
        return values;
    };
    command
        .args
        .iter()
        .zip(values)
        .map(|(arg, value)| {
            let path = matches!(arg.kind, ArgKind::File | ArgKind::Dir);
            match path && value.starts_with("~/") {
                true => Path::new(home)
                    .join(&value["~/".len()..])
                    .to_string_lossy()
                    .into_owned(),
                false => value,
            }
        })
        .collect()
}

/// The steps of `command` with each placeholder written from `values`, which
/// stand in the order of the command's arguments.
///
/// A shell line writes each value as one shell word. A string argument of an
/// action writes each value as it is.
fn plan(command: &UserCommand, values: &[String]) -> Plan {
    let value = |name: &str| arg_value(command, values, name);
    match &command.step {
        Step::Shell(template) => {
            let line = template.render_shell(value);
            match command.output {
                Output::Block => Plan::Modal(line),
                Output::RunPane => Plan::Pane(line),
                Output::Notify | Output::Buffer | Output::Locations => {
                    Plan::Steps(VecDeque::from([PlannedStep::Shell(line)]))
                },
            }
        },
        Step::Action(action) => {
            let mut action = action.clone();
            for arg in &mut action.args {
                if let Value::String(text) = &mut arg.value {
                    *text = Template::parse(text).render(value);
                }
            }
            Plan::Steps(VecDeque::from([PlannedStep::Act(action)]))
        },
    }
}

/// Run the steps of `run` up to its next shell step, which parks the run
/// until the step's result lands, or to its end.
///
/// A run of the `notify` output that ends on a shell step shows a message.
/// The message is the first line of that step's stdout, or `<name> done` for
/// a step with none. A run of the `buffer` output that ends on a shell step
/// opens that step's stdout in a scratch buffer.
fn advance(stoat: &mut Stoat, mut run: CommandRun) -> UpdateEffect {
    let mut effect = UpdateEffect::Redraw;
    while let Some(step) = run.steps.pop_front() {
        match step {
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
            buffer_lifecycle::show_scratch_with_text(stoat, &run.last_stdout);
        },
        Output::Block | Output::RunPane | Output::Locations => {},
    }
    effect
}

/// Run `line` through the shell host on a worker.
///
/// The line runs in the workspace root with the project environment and
/// `TERM=dumb`. The event that `done` makes of the result goes onto
/// [`Stoat::command_events`]. The step runs with no terminal, because an
/// output that reads stdout reads it apart from stderr, and a terminal merges
/// the two.
pub(crate) fn spawn_shell_job(
    stoat: &Stoat,
    line: String,
    done: impl FnOnce(io::Result<ShellOutput>) -> CommandEvent + Send + 'static,
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
            let result = executor
                .spawn_blocking(move || shell_host.run(&line, b"", Some(&root), &env))
                .await;
            events
                .lock()
                .expect("command events mutex")
                .push(done(result));
        })
        .detach();
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

/// The shell line that the shell step of `command` runs with `values`, one
/// for each argument, or `None` for an action step.
///
/// The palette's key line names it. It writes each value as [`plan`] does.
/// A `file` or `dir` value keeps its leading `~/`. Only the run expands it to
/// the home directory, so the line on show reads that path as typed.
pub(crate) fn run_line_preview(command: &UserCommand, values: &[String]) -> Option<String> {
    let Step::Shell(template) = &command.step else {
        return None;
    };
    Some(template.render_shell(|name| arg_value(command, values, name)))
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

/// End the `locations` run `run`, whose shell step gave `output`, whatever
/// its exit code.
///
/// The lines of the stdout that name a location resolve on a worker, and the
/// picker opens over them when they land. With no location, a run that failed
/// with a stderr line shows its failure, and any other run says so.
fn end_locations(stoat: &mut Stoat, run: &CommandRun, output: ShellOutput) {
    let parsed: Vec<ParsedLocation> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(location_picker::parse_location_line)
        .collect();
    if parsed.is_empty() {
        let failed =
            output.exit_code != 0 && first_line(&String::from_utf8_lossy(&output.stderr)).is_some();
        match failed {
            true => stoat.set_status(failure(&run.name, &Ok(output))),
            false => stoat.notify(MessageKind::Info, "no locations"),
        }
        return;
    }

    let root = stoat.active_workspace().git_root.clone();
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
    use super::{pick_failure, plan, pump, CommandEvent, Plan};
    use crate::{
        action_handlers,
        app::{MessageKind, Stoat},
        host::ShellOutput,
        location_picker::LocationEntry,
        pane::{DockPanel, DockSide, DockVisibility, FocusTarget, View},
        test_harness::TestHarness,
        user_command::{ArgKind, CommandArg, Output, Step, UserCommand},
    };
    use std::path::{Path, PathBuf};
    use stoat_config::Template;
    use stoat_host::FakeShellInvocation;

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

    fn output(stdout: &[u8], stderr: &[u8], exit_code: i32) -> ShellOutput {
        ShellOutput {
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            exit_code,
        }
    }

    #[test]
    fn the_default_output_plans_the_run_modal() {
        let command = UserCommand {
            name: "pr".to_owned(),
            doc: String::new(),
            line: 1,
            args: vec![CommandArg {
                name: "pr".to_owned(),
                kind: ArgKind::Text { default: None },
            }],
            step: Step::Shell(Template::parse("gh pr checkout {pr}")),
            output: Output::Block,
        };
        assert_eq!(
            plan(&command, &["412".to_owned()]),
            Plan::Modal("gh pr checkout 412".to_owned()),
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

    #[test]
    fn a_run_pane_command_opens_a_run_pane_and_sends_its_line() {
        let mut h = loaded(CHECK);
        submit(&mut h, "check");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                matches!(ws.panes.pane(ws.panes.focus()).view, View::Run(_)),
                h.fake_terminal.sent_strings().last().cloned(),
                h.fake_shell.invocations(),
            ),
            (true, Some("make test\n".to_owned()), Vec::new()),
        );
    }

    /// A Run pane on show takes the line and the focus, and no second run
    /// opens.
    #[test]
    fn a_run_pane_command_uses_the_run_pane_on_show() {
        let mut h = loaded(CHECK);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenRun);
        let run_pane = h.stoat.active_workspace().panes.focus();
        action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
        submit(&mut h, "check");

        let ws = h.stoat.active_workspace();
        assert_eq!(
            (
                ws.runs.len(),
                ws.panes.focus() == run_pane,
                h.fake_terminal.sent_strings().last().cloned(),
            ),
            (1, true, Some("make test\n".to_owned())),
        );
    }

    /// A Run pane takes the focus from a dock that held it.
    #[test]
    fn a_run_pane_command_takes_the_focus_from_a_dock() {
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
        assert_eq!(
            (
                ws.focus,
                matches!(ws.panes.pane(ws.panes.focus()).view, View::Run(_)),
            ),
            (FocusTarget::SplitPane, true),
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

    #[test]
    fn a_locations_command_with_no_location_says_so() {
        let mut h = loaded(FIND);
        h.fake_shell
            .set_response("rg -n three", output(b"", b"", 1));
        submit(&mut h, "find three");
        assert_eq!(
            (message(&h), h.stoat.location_picker.is_none()),
            ((Some("no locations"), MessageKind::Info), true),
        );
    }

    #[test]
    fn a_failed_locations_command_reports_its_stderr() {
        let mut h = loaded(FIND);
        h.fake_shell
            .set_response("rg -n three", output(b"", b"rg: bad regex\n", 2));
        submit(&mut h, "find three");
        assert_eq!(
            message(&h),
            (Some("find: rg: bad regex (exit 2)"), MessageKind::Alert),
        );
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
}
