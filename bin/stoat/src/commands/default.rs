use clap::{ArgAction, CommandFactory, Parser, Subcommand, ValueHint};
use crossterm::event::{Event, KeyEvent};
use snafu::{whatever, ResultExt, Whatever};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use stoat::{
    host::{EnvHost, FsHost, LocalClipboard, LocalEnv, LocalFs, LocalFsWatcher},
    input_parse,
    lsp::hosts,
    Axis, Settings, Stoat,
};
use stoat_cli::{CommonArgs, FixtureArgs, FixtureSub};
use stoat_scheduler::{Executor, TokioScheduler};
use tokio::sync::mpsc::UnboundedSender;

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

    /// Set the initial workspace root and process working directory, so the
    /// git root and file positionals resolve against it. Defaults to the
    /// current directory when unset.
    #[arg(
        short = 'd',
        long = "working-dir",
        alias = "working-directory",
        value_name = "DIR",
        value_hint = ValueHint::DirPath
    )]
    working_dir: Option<PathBuf>,

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
    /// Resolve merge conflicts in a three-way view
    Conflict,
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
    /// Interrogate a live session over its socket for LSP status, diagnostics,
    /// or hover at a position. Prints the JSON reply.
    Query {
        #[command(subcommand)]
        sub: crate::commands::query::QueryCommand,
    },
    /// Author and check the guided walkthroughs stored in this workspace under
    /// `.stoat/walkthroughs`.
    Walkthrough {
        #[command(subcommand)]
        sub: crate::commands::walkthrough::WalkthroughCommand,
    },
    /// Materialize a deterministic fixture and open the editor inside it. `ls`
    /// lists the catalog.
    Fixture(FixtureArgs),

    /// Print a shell completion script to stdout, e.g. `stoat completions fish >
    /// ~/.config/fish/completions/stoat.fish` (zsh and bash install the same
    /// way).
    Completions {
        /// The shell to generate completions for.
        shell: clap_complete::Shell,
    },
}

pub fn run(args: Args) -> Result<(), Whatever> {
    let Args {
        command,
        common,
        working_dir,
        text_proto_log,
        ..
    } = args;

    match command {
        Some(Command::Dump { sub }) => crate::commands::dump::run(sub),
        Some(Command::Diff(args)) => crate::commands::diff::run(args),
        Some(Command::AgentApi { sub }) => crate::commands::agent_api::run(sub),
        Some(Command::Editor { file }) => crate::commands::editor::run(file),
        Some(Command::Query { sub }) => crate::commands::query::run(sub),
        Some(Command::Walkthrough { sub }) => crate::commands::walkthrough::run(sub),
        Some(Command::Fixture(fixture)) => run_fixture(fixture, text_proto_log, common),
        Some(Command::Completions { shell }) => {
            clap_complete::generate(shell, &mut Args::command(), "stoat", &mut std::io::stdout());
            Ok(())
        },
        Some(Command::Review) => run_tui(text_proto_log, common, working_dir, TuiStart::Review),
        Some(Command::Conflict) => run_tui(text_proto_log, common, working_dir, TuiStart::Conflict),
        None => run_tui(text_proto_log, common, working_dir, TuiStart::Files),
    }
}

/// Run the `fixture` subcommand. `ls` prints the catalog. A bare fixture name
/// materializes and opens it through the same startup path as `--fixture`.
///
/// A name given both as the positional and via `--fixture` is rejected rather
/// than silently picking one.
fn run_fixture(
    args: FixtureArgs,
    text_proto_log: Option<bool>,
    mut common: CommonArgs,
) -> Result<(), Whatever> {
    match (args.sub, args.name) {
        (Some(FixtureSub::Ls), _) => {
            crate::commands::fixture::run_ls();
            Ok(())
        },
        (None, Some(name)) => {
            if common.fixture.is_some() {
                whatever!("`--fixture` conflicts with the fixture subcommand");
            }
            common.fixture = Some(name);
            run_tui(text_proto_log, common, None, TuiStart::Files)
        },
        (None, None) => whatever!("specify a fixture name or `ls`"),
    }
}

enum TuiStart {
    Review,
    Conflict,
    Files,
}

/// Whether this invocation is the plain "show me these files" form that a
/// terminal pane's parent instance handles better than a nested editor.
///
/// Every other form asks for a session of its own. A subcommand starts
/// somewhere other than a file list, a restore flag names a workspace to
/// reopen, `--inputs` and `--timeout` drive a scripted run, `--fixture` builds
/// a throwaway repo, and `--working-dir` names a root other than the parent's.
fn forwardable(start: &TuiStart, common: &CommonArgs, working_dir: Option<&Path>) -> bool {
    matches!(start, TuiStart::Files)
        && !common.files.is_empty()
        && working_dir.is_none()
        && !common.continue_
        && !common.resume
        && common.inputs.is_none()
        && common.timeout.is_none()
        && common.fixture.is_none()
}

/// Whether this launch names nothing to start on, so the workspace finder asks
/// which project to enter instead of dropping onto the scratch.
///
/// A subcommand, a file list, a restore flag, a scripted `--inputs` run, a
/// `--fixture` repo, and `--working-dir` each answer that question already.
/// `--timeout` does not. It says when to stop rather than where to start, so a
/// timed bare launch still gets the finder.
fn bare_launch(start: &TuiStart, common: &CommonArgs, working_dir: Option<&Path>) -> bool {
    matches!(start, TuiStart::Files)
        && common.files.is_empty()
        && working_dir.is_none()
        && !common.continue_
        && !common.resume
        && common.inputs.is_none()
        && common.fixture.is_none()
}

fn run_tui(
    text_proto_log: Option<bool>,
    common: CommonArgs,
    working_dir: Option<PathBuf>,
    start: TuiStart,
) -> Result<(), Whatever> {
    // Run before anything takes over the terminal, so a forwarded open leaves
    // the shell exactly as it found it. Outside a stoat terminal pane this
    // costs one absent environment read.
    if forwardable(&start, &common, working_dir.as_deref())
        && crate::commands::term_open::try_forward(&common.files)
    {
        return Ok(());
    }

    // Read before `common` is spent, since the answer depends on flags the
    // destructure moves out.
    let bare = bare_launch(&start, &common, working_dir.as_deref());

    let CommonArgs {
        files,
        continue_,
        resume,
        inputs,
        timeout,
        fixture,
    } = common;

    // Parse `--inputs` before taking over the terminal, so a malformed
    // sequence fails the invocation with a plain error instead of after a
    // UI takeover that must then be unwound.
    let inputs = inputs
        .as_deref()
        .or_else(|| fixture.as_deref().and_then(stoat_cli::default_inputs))
        .map(input_parse::parse_input_sequence)
        .transpose()
        .with_whatever_context(|e| format!("parse --inputs sequence: {e}"))?;

    stoat::ui::install_panic_hook();

    // Unbounded because the UI thread forwards onto this inside the same loop
    // that flushes frames and polls stdin. A bound parks that thread once the
    // main thread stalls with the queue full, and nothing reads fd 0 while it is
    // parked, so backpressure lands in the kernel's tty buffer where an
    // overflow tears escape sequences into garbage input. Input events are
    // small and human-rate, and `drain_pending` applies a whole backlog before
    // one render, so a queue grown during a stall collapses on the next wake.
    let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
    // Latest-frame-wins: the main loop ships frames without ever parking on a
    // slow flush, so input acceptance is never stalled behind rendering.
    // Redundant frames coalesce; only the most recently sent frame is drawn.
    let (render_tx, render_rx) = tokio::sync::watch::channel(None);
    // Ordered, lossless side channel for stoatty APC byte batches. Separate
    // from the render watch because `fill` page content must not coalesce or
    // drop; written to stdout by the UI thread right after each grid frame.
    let (apc_tx, apc_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
    // The UI thread's one report of whether a stoatty answered the startup ident
    // handshake, which only it can run: the reply arrives on raw stdin, which
    // that thread owns.
    let (stoatty_tx, stoatty_rx) = tokio::sync::mpsc::unbounded_channel::<Option<u32>>();
    // The tty's cell size, for the same reason: only that thread can ask it.
    let (cell_pixels_tx, cell_pixels_rx) =
        tokio::sync::mpsc::unbounded_channel::<Option<(u16, u16)>>();

    let mouse_capture_policy = stoat::default_mouse_capture_policy();
    let mouse_captured = stoat::resolve_mouse_captured(mouse_capture_policy, &LocalEnv);

    // Cloned only when a sequence will be driven. A lingering extra sender
    // would hold the event channel open past a natural shutdown.
    let driver_tx = inputs.is_some().then(|| event_tx.clone());

    let ui_handle = stoat::ui::spawn(
        event_tx,
        render_rx,
        apc_rx,
        stoatty_tx,
        cell_pixels_tx,
        mouse_captured,
    );

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .whatever_context("build tokio runtime")?;
    let scheduler = Arc::new(TokioScheduler::new(rt.handle().clone()));
    let executor = scheduler.executor();

    let cli_settings = Settings {
        text_proto_log,
        format_on_save: None,
        search_smart_case: None,
        config_auto_reload: None,
        theme: None,
        mouse_capture: None,
        scrolloff: None,
        editor_line_numbers: None,
        editor_minimap: None,
        editor_auto_pairs: None,
        editor_emoji_expansion: None,
        editor_wrap: None,
        editor_wrap_column: None,
        ui_tab_bar: None,
        ui_inactive_dim: None,
        ui_pin_hides_hints: None,
        highlight_retention: None,
        terminal_shell: None,
        terminal_args: None,
        direnv_load: None,
        direnv_reload_on_cd: None,
        direnv_unset_on_exit: None,
        review_precompute: None,
        mode_badges: std::collections::BTreeMap::new(),
        lsp_servers: std::collections::BTreeMap::new(),
        lsp_server_lists: std::collections::BTreeMap::new(),
        lsp_commands: std::collections::BTreeMap::new(),
        lsp_only: std::collections::BTreeMap::new(),
        lsp_except: std::collections::BTreeMap::new(),
        lsp_globals: None,
        finder_scopes: std::collections::BTreeMap::new(),
        finder_default_scope: None,
    };

    // Materialize a requested fixture and switch into it before resolving the
    // cwd below, so the git root, LSP root, and file positionals all land
    // inside the fixture. The temp dir is kept (leaked) for the session.
    if let Some(name) = fixture {
        #[cfg(feature = "fixture")]
        {
            let dir = tempfile::Builder::new()
                .prefix("stoat-fixture-")
                .tempdir()
                .whatever_context("create fixture temp dir")?;
            stoat::fixture::materialize(&name, dir.path())
                .with_whatever_context(|_| format!("materialize fixture `{name}`"))?;
            // Canonicalize because macOS /tmp is a symlink to /private/tmp, and
            // an uncanonicalized cwd breaks path-comparing consumers.
            let root = dir
                .keep()
                .canonicalize()
                .whatever_context("canonicalize fixture dir")?;
            tracing::info!(
                target: "stoat::bin",
                fixture = %name,
                path = %root.display(),
                "materialized fixture into temp dir",
            );
            std::env::set_current_dir(&root).whatever_context("set cwd to fixture dir")?;
        }
        #[cfg(not(feature = "fixture"))]
        {
            whatever!(
                "requested fixture `{name}`, but stoat was built without the \
                 fixture feature (rebuild with --features fixture)"
            );
        }
    }

    // Applied after the fixture block so an explicit --working-dir wins over the
    // degenerate --fixture plus --working-dir combination, and before the cwd is
    // resolved so the git root and file positionals all land inside it.
    if let Some(dir) = working_dir {
        std::env::set_current_dir(&dir)
            .with_whatever_context(|_| format!("set working directory to {}", dir.display()))?;
    }

    let cwd = std::env::current_dir().unwrap_or_default();
    let initial_git_root = if resume {
        stoat::workspace::find_resume_anchor(&cwd, &LocalFs)
            .ok()
            .flatten()
            .unwrap_or_else(|| cwd.clone())
    } else {
        cwd
    };

    rt.block_on(async {
        let user_config = stoat::user_config_path().and_then(|path| read_config(&path));
        let user_themes = stoat::user_themes_dir()
            .map(|dir| vscode_theme::discover(&dir))
            .unwrap_or_default();
        let env_theme = LocalEnv.var("STOAT_THEME").filter(|s| !s.is_empty());
        let mut stoat = Stoat::new_with_user_config(
            executor.clone(),
            cli_settings,
            initial_git_root,
            user_config,
            user_themes,
            env_theme,
        );
        stoat.set_apc_tx(apc_tx.clone());
        stoat.set_stoatty_rx(stoatty_rx);
        stoat.set_cell_pixels_rx(cell_pixels_rx);
        stoat.set_window_ipc(window_socket_path());
        stoat.set_version_info(VERSION_INFO);
        stoat.set_lsp_auto_spawn(true);
        stoat.set_env_auto_load(true);
        stoat.set_diff_warm_auto(true);
        // OSC 52 rides the same ordered channel the UI thread drains to stdout,
        // so a large yank over SSH never blocks the event loop on the pipe and
        // never lands mid-frame.
        stoat.set_clipboard_host(Arc::new(LocalClipboard::new(Some(apc_tx))));
        if continue_ || resume {
            stoat.load_active_workspace_state();
        }

        // Bind the active session's agent hook socket so an owned agent's hooks
        // and runtime queries reach this process. A workspace opened later
        // binds its own on the spawn that needs it. Deferred until after state
        // restore, which adopts the persisted session uid. Production-only like
        // set_lsp_auto_spawn, since binding needs the reactor only a real run
        // has.
        match stoat::log::state_dir() {
            Ok(dir) => {
                stoat.set_agent_socket_dir(dir);
                stoat.set_serve_agent_sockets(true);
            },
            Err(err) => tracing::warn!(
                target: "stoat::bin",
                %err,
                "state directory unresolved; agent hooks and runtime queries disabled this session",
            ),
        }
        let active_uid = stoat.active_workspace().uid();
        if let Err(err) = stoat.serve_term_session(active_uid) {
            tracing::warn!(
                target: "stoat::bin",
                %err,
                "session hook socket bind failed; agent hooks and runtime queries disabled this session",
            );
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
            TuiStart::Review => stoat.open_working_tree_diff(),
            TuiStart::Conflict => stoat.open_conflict_view(),
            TuiStart::Files => {
                for (i, path) in files.iter().enumerate() {
                    if i > 0 {
                        stoat.active_workspace_mut().panes.split(Axis::Vertical);
                    }
                    stoat.open_file(path);
                }
            },
        }

        let hydrated = match stoat.env_host().var("STOAT_DUMP_LOAD") {
            Some(raw) => {
                let dump_path = PathBuf::from(&raw);
                match dump_path.exists() {
                    true => match stoat::dump::hydrate(&mut stoat, &dump_path, &LocalFs) {
                        Ok(()) => {
                            tracing::info!(path = %raw, "hydrated workspace from dump");
                            true
                        },
                        Err(e) => {
                            tracing::error!(error = %e, path = %raw, "failed to hydrate dump");
                            false
                        },
                    },
                    false => {
                        tracing::warn!(path = %raw, "STOAT_DUMP_LOAD points at missing file");
                        false
                    },
                }
            },
            None => false,
        };

        // After the hydrate, never before it. The finder's input view lives in
        // the active workspace, which a hydrate replaces wholesale. A hydrated
        // dump is also a start state the caller chose, so it skips the finder.
        if bare && !hydrated {
            stoat.open_workspace_picker();
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

        let outcome = stoat.run(event_rx, render_tx).await;
        hosts::shutdown_lsp(&stoat).await;
        outcome
    })
    .whatever_context("stoat event loop")?;

    ui_handle
        .join()
        .expect("ui thread panicked")
        .whatever_context("ui thread")?;

    Ok(())
}

/// The user config file's contents, or `None` when it is absent or holds
/// bytes that are not UTF-8.
///
/// A missing or unreadable config is not an error. The editor starts on its
/// built-in defaults, which is what a first run does anyway.
fn read_config(path: &Path) -> Option<String> {
    let mut bytes = Vec::new();
    LocalFs.read(path, &mut bytes).ok()?;
    String::from_utf8(bytes).ok()
}

/// The window-event socket stoatty passes down through `STOATTY_WINDOW_SOCKET`,
/// or `None` when the editor was not launched by one.
// The value is a filesystem path, and EnvHost::var yields only UTF-8. Routing
// this through the host silently drops a path that carries other bytes.
#[allow(clippy::disallowed_methods)]
fn window_socket_path() -> Option<PathBuf> {
    std::env::var_os("STOATTY_WINDOW_SOCKET").map(PathBuf::from)
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
async fn drive_inputs(tx: UnboundedSender<Event>, keys: Vec<KeyEvent>, executor: Executor) {
    executor.timer(READINESS_DELAY).await;
    for key in keys {
        executor.timer(INTER_KEY_DELAY).await;
        if tx.send(Event::Key(key)).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stoat_scheduler::TestScheduler;

    fn bare_files() -> CommonArgs {
        CommonArgs {
            files: vec![PathBuf::from("a.rs")],
            continue_: false,
            resume: false,
            inputs: None,
            timeout: None,
            fixture: None,
        }
    }

    #[test]
    fn only_a_bare_file_open_forwards_to_the_parent() {
        assert!(forwardable(&TuiStart::Files, &bare_files(), None));

        assert!(
            !forwardable(
                &TuiStart::Files,
                &CommonArgs {
                    files: Vec::new(),
                    ..bare_files()
                },
                None
            ),
            "with no files there is nothing to show",
        );
        assert!(
            !forwardable(&TuiStart::Review, &bare_files(), None),
            "a subcommand starts somewhere other than a file list",
        );
        assert!(
            !forwardable(&TuiStart::Conflict, &bare_files(), None),
            "a subcommand starts somewhere other than a file list",
        );
        assert!(
            !forwardable(
                &TuiStart::Files,
                &bare_files(),
                Some(Path::new("/elsewhere"))
            ),
            "--working-dir names a root other than the parent's",
        );
        assert!(
            !forwardable(
                &TuiStart::Files,
                &CommonArgs {
                    continue_: true,
                    ..bare_files()
                },
                None
            ),
            "--continue reopens a workspace of its own",
        );
        assert!(
            !forwardable(
                &TuiStart::Files,
                &CommonArgs {
                    resume: true,
                    ..bare_files()
                },
                None
            ),
            "--resume reopens a workspace of its own",
        );
        assert!(
            !forwardable(
                &TuiStart::Files,
                &CommonArgs {
                    inputs: Some("ifoo<Esc>".into()),
                    ..bare_files()
                },
                None,
            ),
            "--inputs drives a session of its own",
        );
        assert!(
            !forwardable(
                &TuiStart::Files,
                &CommonArgs {
                    timeout: Some(1.0),
                    ..bare_files()
                },
                None
            ),
            "--timeout ends a session of its own",
        );
        assert!(
            !forwardable(
                &TuiStart::Files,
                &CommonArgs {
                    fixture: Some("basic".into()),
                    ..bare_files()
                },
                None,
            ),
            "--fixture builds a repo of its own",
        );
    }

    /// Anything that names where to start answers the finder's question
    /// already, so only a launch carrying none of them is bare.
    #[test]
    fn only_a_launch_naming_nothing_opens_the_finder() {
        let empty = || CommonArgs {
            files: Vec::new(),
            ..bare_files()
        };
        assert!(bare_launch(&TuiStart::Files, &empty(), None));

        assert!(
            !bare_launch(&TuiStart::Files, &bare_files(), None),
            "a file list is what to show",
        );
        assert!(
            !bare_launch(&TuiStart::Review, &empty(), None),
            "a subcommand starts somewhere other than a file list",
        );
        assert!(
            !bare_launch(&TuiStart::Files, &empty(), Some(Path::new("/elsewhere"))),
            "--working-dir names the root to start in",
        );
        assert!(
            !bare_launch(
                &TuiStart::Files,
                &CommonArgs {
                    continue_: true,
                    ..empty()
                },
                None
            ),
            "--continue names the workspace to reopen",
        );
        assert!(
            !bare_launch(
                &TuiStart::Files,
                &CommonArgs {
                    resume: true,
                    ..empty()
                },
                None
            ),
            "--resume names the workspace to reopen",
        );
        assert!(
            !bare_launch(
                &TuiStart::Files,
                &CommonArgs {
                    inputs: Some("ifoo<Esc>".into()),
                    ..empty()
                },
                None
            ),
            "a scripted run expects the scratch as its start state",
        );
        assert!(
            !bare_launch(
                &TuiStart::Files,
                &CommonArgs {
                    fixture: Some("basic".into()),
                    ..empty()
                },
                None
            ),
            "--fixture builds the repo to start in",
        );
    }

    /// `--timeout` says when to stop, not where to start, so it leaves a bare
    /// launch bare.
    #[test]
    fn a_timeout_still_leaves_a_launch_bare() {
        assert!(bare_launch(
            &TuiStart::Files,
            &CommonArgs {
                files: Vec::new(),
                timeout: Some(1.0),
                ..bare_files()
            },
            None
        ));
    }

    #[test]
    fn drive_inputs_paces_parsed_keys_onto_the_channel() {
        let scheduler = Arc::new(TestScheduler::new());
        let executor = scheduler.executor();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

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

    #[test]
    fn fixture_subcommand_parses_ls_and_a_name() {
        let ls = Args::try_parse_from(["stoat", "fixture", "ls"]).expect("parse ls");
        let Some(Command::Fixture(args)) = ls.command else {
            panic!("expected fixture subcommand");
        };
        assert_eq!(args.sub, Some(FixtureSub::Ls));
        assert_eq!(args.name, None);

        let named = Args::try_parse_from(["stoat", "fixture", "rust-lsp"]).expect("parse name");
        let Some(Command::Fixture(args)) = named.command else {
            panic!("expected fixture subcommand");
        };
        assert_eq!(args.sub, None);
        assert_eq!(args.name.as_deref(), Some("rust-lsp"));
    }

    #[test]
    fn conflict_subcommand_parses() {
        let args = Args::try_parse_from(["stoat", "conflict"]).expect("parse conflict");
        assert!(matches!(args.command, Some(Command::Conflict)));
    }

    #[test]
    fn working_dir_flag_parses_all_spellings() {
        for flag in ["-d", "--working-dir", "--working-directory"] {
            let args = Args::try_parse_from(["stoat", flag, "/tmp"])
                .unwrap_or_else(|e| panic!("parse {flag}: {e}"));
            assert_eq!(
                args.working_dir,
                Some(PathBuf::from("/tmp")),
                "{flag} sets the working directory",
            );
        }
        assert_eq!(
            Args::try_parse_from(["stoat"])
                .expect("bare argv")
                .working_dir,
            None,
        );
    }

    #[test]
    fn fixture_subcommand_composes_with_top_level_flags() {
        let args = Args::try_parse_from(["stoat", "--timeout", "2", "fixture", "rust-lsp"])
            .expect("parse");
        assert_eq!(args.common.timeout, Some(2.0));
        let Some(Command::Fixture(fixture)) = args.command else {
            panic!("expected fixture subcommand");
        };
        assert_eq!(fixture.name.as_deref(), Some("rust-lsp"));
    }

    #[test]
    fn completions_carry_the_fixture_names() {
        let mut out = Vec::new();
        clap_complete::generate(
            clap_complete::Shell::Fish,
            &mut Args::command(),
            "stoat",
            &mut out,
        );
        let script = String::from_utf8(out).expect("fish script is utf8");
        assert!(
            script.contains("rust-lsp"),
            "generated completions must offer the fixture names"
        );
    }
}
