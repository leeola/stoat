use super::{Output, Step, UserCommand};
use crate::{
    action_handlers,
    app::{MessageKind, Stoat, UpdateEffect},
    host::ShellOutput,
    keymap::ResolvedAction,
};
use std::{
    collections::VecDeque,
    io, mem,
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
}

/// Run `command` with `values`, one value for each of its arguments.
///
/// A shell step under the `block` output runs in the run modal. Any other
/// step runs here, a shell step on a worker with no terminal, so its stdout
/// and its stderr stay apart.
pub(crate) fn start(stoat: &mut Stoat, command: &UserCommand, values: Vec<String>) -> UpdateEffect {
    match plan(command, &values) {
        Plan::Modal(line) => action_handlers::run::run_command(stoat, &line),
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
/// names the failure.
pub(crate) fn pump(stoat: &mut Stoat) -> bool {
    let events = mem::take(&mut *stoat.command_events.lock().expect("command events mutex"));
    let landed = !events.is_empty();
    for CommandEvent::StepDone { run, result } in events {
        let Some(index) = stoat.command_runs.iter().position(|held| held.id == run) else {
            continue;
        };
        let mut run = stoat.command_runs.remove(index);
        match result {
            Ok(output) if output.exit_code == 0 => {
                run.last_stdout = String::from_utf8_lossy(&output.stdout).into_owned();
                advance(stoat, run);
            },
            failed => stoat.set_status(failure(&run.name, &failed)),
        }
    }
    landed
}

/// The steps of `command` with each placeholder written from `values`, which
/// stand in the order of the command's arguments.
///
/// A shell line writes each value as one shell word. A string argument of an
/// action writes each value as it is.
fn plan(command: &UserCommand, values: &[String]) -> Plan {
    let value = |name: &str| {
        command
            .args
            .iter()
            .position(|arg| arg.name == name)
            .and_then(|index| values.get(index).cloned())
    };
    match &command.step {
        Step::Shell(template) => {
            let line = template.render_shell(value);
            match command.output {
                Output::Block => Plan::Modal(line),
                Output::Notify => Plan::Steps(VecDeque::from([PlannedStep::Shell(line)])),
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
/// a step with none.
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

    if run.output == Output::Notify && !run.last_was_action {
        let text = first_line(&run.last_stdout)
            .map_or_else(|| format!("{} done", run.name), str::to_owned);
        stoat.notify(MessageKind::Success, text);
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
fn spawn_shell_job(
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

/// The first line of `text` that holds more than whitespace, trimmed.
fn first_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).find(|line| !line.is_empty())
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
    use super::{plan, Plan};
    use crate::{
        app::{MessageKind, Stoat},
        host::ShellOutput,
        test_harness::TestHarness,
        user_command::{CommandArg, Output, Step, UserCommand},
    };
    use std::path::PathBuf;
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

    /// Type `line` into the palette, press Enter, and settle the run.
    fn submit(h: &mut TestHarness, line: &str) {
        h.type_text(&format!(":{line}"));
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
            args: vec![CommandArg {
                name: "pr".to_owned(),
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

    #[test]
    fn a_command_with_no_argument_runs_from_its_bare_name() {
        let mut h = loaded(r#"command hi { run = Notify("hello"); }"#);
        submit(&mut h, "hi");
        assert_eq!(message(&h), (Some("hello"), MessageKind::Info));
    }
}
