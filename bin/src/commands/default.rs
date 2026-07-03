use clap::{ArgAction, Parser, Subcommand};
use crossterm::event::{Event, KeyEvent};
use snafu::{ResultExt, Whatever};
use std::{path::PathBuf, sync::Arc, time::Duration};
use stoat::{
    host::{LocalFs, LocalFsWatcher},
    input_parse, Axis, Settings, Stoat,
};
use stoat_cli::CommonArgs;
use stoat_scheduler::{Executor, TokioScheduler};
use tokio::sync::mpsc::Sender;

const VERSION_INFO: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("STOAT_BUILD_INFO"),
    ")",
);

#[derive(Parser)]
#[command(
    name = "stoat",
    about = "A modal text editor",
    version = VERSION_INFO,
    disable_version_flag = true,
)]
pub struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    #[command(flatten)]
    pub common: CommonArgs,

    /// Enable the LSP text-protocol transcript log. Overrides
    /// the stcfg `text_proto_log` setting when set.
    #[arg(long, env = "STOAT_TEXT_PROTO_LOG")]
    pub text_proto_log: Option<bool>,

    /// Route tracing output to stderr instead of the background log file. The
    /// raw-mode TUI is corrupted by stderr unless redirected, e.g. `2>log`.
    #[arg(long = "log-stderr")]
    pub log_stderr: bool,

    #[arg(
        short = 'v',
        long = "version",
        action = ArgAction::Version,
        help = "Print version info",
    )]
    _version: Option<bool>,
}

#[derive(Subcommand)]
enum Command {
    /// Open the first changed file with a diff against HEAD
    Review,
    /// Manage workspace dumps (captured tarballs of the repo + stoat state).
    Dump {
        #[command(subcommand)]
        sub: crate::commands::dump::DumpCommand,
    },
    /// Render a structural diff to stdout. By default, scans the
    /// current repo for changes against HEAD and renders a diff
    /// for each changed path. With `--git`, acts as the
    /// `GIT_EXTERNAL_DIFF` adapter using the seven path arguments
    /// git supplies; positional args are not accepted in the
    /// default mode.
    Diff(crate::commands::diff::DiffArgs),
    /// Thin client the owned Claude subshell's hooks invoke to push status
    /// into the owning session.
    AgentApi {
        #[command(subcommand)]
        sub: crate::commands::agent_api::AgentApiCommand,
    },
    /// Open a file in the owning Stoat instance and block until it is closed,
    /// honoring the `$EDITOR <file>` contract. Set as the owned agent's editor.
    Editor {
        /// File to open in the parent instance.
        file: PathBuf,
    },
}

pub fn run(args: Args) -> Result<(), Whatever> {
    let Args {
        command,
        common,
        text_proto_log,
        ..
    } = args;

    match command {
        Some(Command::Dump { sub }) => crate::commands::dump::run(sub),
        Some(Command::Diff(args)) => crate::commands::diff::run(args),
        Some(Command::AgentApi { sub }) => crate::commands::agent_api::run(sub),
        Some(Command::Editor { file }) => crate::commands::editor::run(file),
        Some(Command::Review) => run_tui(text_proto_log, common, TuiStart::Review),
        None => run_tui(text_proto_log, common, TuiStart::Files),
    }
}

enum TuiStart {
    Review,
    Files,
}

fn run_tui(
    text_proto_log: Option<bool>,
    common: CommonArgs,
    start: TuiStart,
) -> Result<(), Whatever> {
    let CommonArgs {
        files,
        continue_,
        resume,
        inputs,
        timeout,
    } = common;

    // Parse `--inputs` before taking over the terminal, so a malformed
    // sequence fails the invocation with a plain error instead of after a
    // UI takeover that must then be unwound.
    let inputs = inputs
        .as_deref()
        .map(input_parse::parse_input_sequence)
        .transpose()
        .with_whatever_context(|e| format!("parse --inputs sequence: {e}"))?;

    stoat::ui::install_panic_hook();

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
    // Latest-frame-wins: the main loop ships frames without ever parking on a
    // slow flush, so input acceptance is never stalled behind rendering.
    // Redundant frames coalesce; only the most recently sent frame is drawn.
    let (render_tx, render_rx) = tokio::sync::watch::channel(None);
    // Ordered, lossless side channel for stoatty APC byte batches. Separate
    // from the render watch because `fill` page content must not coalesce or
    // drop; written to stdout by the UI thread right after each grid frame.
    let (apc_tx, apc_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    let mouse_capture_policy = stoat::default_mouse_capture_policy();
    let mouse_captured =
        stoat::resolve_mouse_captured(mouse_capture_policy, &stoat::host::LocalEnv);

    // Cloned only when a sequence will be driven. A lingering extra sender
    // would hold the event channel open past a natural shutdown.
    let driver_tx = inputs.is_some().then(|| event_tx.clone());

    let ui_handle = stoat::ui::spawn(event_tx, render_rx, apc_rx, mouse_captured);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .whatever_context("build tokio runtime")?;
    let scheduler = Arc::new(TokioScheduler::new(rt.handle().clone()));
    let executor = scheduler.executor();

    let cli_settings = Settings {
        text_proto_log,
        theme: None,
        mouse_capture: None,
        scrolloff: None,
        terminal_shell: None,
        terminal_args: None,
        review_follow: None,
        mode_badges: std::collections::BTreeMap::new(),
    };

    let cwd = std::env::current_dir().unwrap_or_default();
    let initial_git_root = if resume {
        stoat::workspace::find_resume_anchor(&cwd, &LocalFs)
            .ok()
            .flatten()
            .unwrap_or_else(|| cwd.clone())
    } else {
        cwd
    };

    // Stoat smooth-scrolls the focused editor via stoatty's APC when it detects
    // it is running inside stoatty; the env var is set on the shell stoatty
    // spawns. Absent it, the APC emit stays a no-op.
    let stoatty = std::env::var_os("STOATTY").is_some();

    rt.block_on(async {
        let mut stoat = Stoat::new(executor.clone(), cli_settings, initial_git_root);
        stoat.set_stoatty_apc(stoatty, apc_tx);
        stoat.set_version_info(VERSION_INFO);
        stoat.set_lsp_auto_spawn(true);
        if continue_ || resume {
            stoat.load_active_workspace_state();
        }
        match LocalFsWatcher::new() {
            Ok(watcher) => stoat.set_fs_watch_host(Arc::new(watcher)),
            Err(err) => tracing::warn!(
                target: "stoat::bin",
                %err,
                "LocalFsWatcher init failed; review modification tracker disabled this session",
            ),
        }

        match start {
            TuiStart::Review => stoat.open_review(),
            TuiStart::Files => {
                for (i, path) in files.iter().enumerate() {
                    if i > 0 {
                        stoat.active_workspace_mut().panes.split(Axis::Vertical);
                    }
                    stoat.open_file(path);
                }
            },
        }

        if let Some(raw) = stoat.env_host().var("STOAT_DUMP_LOAD") {
            let dump_path = PathBuf::from(&raw);
            if dump_path.exists() {
                match stoat::dump::hydrate(&mut stoat, &dump_path, &LocalFs) {
                    Ok(()) => {
                        tracing::info!(path = %raw, "hydrated workspace from dump");
                    },
                    Err(e) => {
                        tracing::error!(error = %e, path = %raw, "failed to hydrate dump");
                    },
                }
            } else {
                tracing::warn!(path = %raw, "STOAT_DUMP_LOAD points at missing file");
            }
        }

        if let (Some(keys), Some(tx)) = (inputs, driver_tx) {
            executor
                .spawn(drive_inputs(tx, keys, executor.clone()))
                .detach();
        }

        if let Some(seconds) = timeout {
            let shutdown = stoat.shutdown_handle();
            let timer_exec = executor.clone();
            executor
                .spawn(async move {
                    timer_exec.timer(Duration::from_secs_f64(seconds)).await;
                    shutdown.notify_one();
                })
                .detach();
        }

        stoat.run(event_rx, render_tx).await
    })
    .whatever_context("stoat event loop")?;

    ui_handle
        .join()
        .expect("ui thread panicked")
        .whatever_context("ui thread")?;

    Ok(())
}

/// Delay before the first driven key, so the workspace, UI thread, and
/// render wiring are live before input arrives.
const READINESS_DELAY: Duration = Duration::from_millis(300);

/// Gap between driven keys, so each keystroke's effect settles before the
/// next is sent.
const INTER_KEY_DELAY: Duration = Duration::from_millis(20);

/// Feed `keys` into the event channel as `Event::Key`s, paced like real
/// typing.
///
/// A readiness delay comes first so the workspace and render wiring are
/// live, then one key lands every [`INTER_KEY_DELAY`]. This is the
/// `--inputs` self-driver, run on the shared executor so a scripted session
/// exercises the same input path a human keyboard drives. Stops early if the
/// receiver has gone away.
async fn drive_inputs(tx: Sender<Event>, keys: Vec<KeyEvent>, executor: Executor) {
    executor.timer(READINESS_DELAY).await;
    for key in keys {
        executor.timer(INTER_KEY_DELAY).await;
        if tx.send(Event::Key(key)).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_scheduler::TestScheduler;

    #[test]
    fn drive_inputs_paces_parsed_keys_onto_the_channel() {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Event>(64);

        let keys = input_parse::parse_input_sequence("if<Esc>").expect("parse");
        let expected: Vec<Event> = keys.iter().cloned().map(Event::Key).collect();

        executor
            .spawn(drive_inputs(tx, keys, executor.clone()))
            .detach();

        scheduler.run_until_parked();
        assert!(
            rx.try_recv().is_err(),
            "no key arrives before the readiness delay"
        );

        scheduler.advance_clock(READINESS_DELAY + INTER_KEY_DELAY * 4);
        scheduler.run_until_parked();

        let mut got = Vec::new();
        while let Ok(event) = rx.try_recv() {
            got.push(event);
        }
        assert_eq!(got, expected);
    }
}
