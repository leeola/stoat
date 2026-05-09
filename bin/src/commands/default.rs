use clap::{ArgAction, Parser, Subcommand};
use snafu::{ResultExt, Whatever};
use std::{path::PathBuf, sync::Arc};
use stoat::{
    host::{BashDenialPolicy, ChainedPermissionPolicy, LocalFs, LocalFsWatcher, RuleBasedPolicy},
    Axis, Settings, Stoat,
};
use stoat_agent_claude_code::ClaudeCodeLauncher;
use stoat_scheduler::TokioScheduler;

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

    #[arg(help = "Files to open")]
    pub files: Vec<PathBuf>,

    /// Restore the most-recently-used workspace for this repository instead
    /// of starting in a fresh one. `continue` is a Rust keyword so the field
    /// is named `continue_`; clap exposes it as `--continue` / `-c`.
    #[arg(short = 'c', long = "continue")]
    pub continue_: bool,

    /// Walk ancestors of the current directory and reopen the workspace
    /// whose state is the most recently modified. So a session run from
    /// `~/foo/bar/baz/bang` reopens whichever ancestor (cwd itself, its
    /// parent, ...) most recently saved a workspace; falls back to a
    /// fresh workspace anchored at cwd when no ancestor has any state.
    /// Mutually exclusive with `--continue`.
    #[arg(short = 'r', long = "resume", conflicts_with = "continue_")]
    pub resume: bool,

    /// Enable the Claude Code / LSP text-protocol transcript log. Overrides
    /// the stcfg `text_proto_log` setting when set.
    #[arg(long, env = "STOAT_TEXT_PROTO_LOG")]
    pub text_proto_log: Option<bool>,

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
    /// Render a structural diff of two files to stdout.
    Diff(crate::commands::diff::DiffArgs),
}

pub fn run() -> Result<(), Whatever> {
    let Args {
        command,
        files,
        continue_,
        resume,
        text_proto_log,
        ..
    } = Args::parse();

    match command {
        Some(Command::Dump { sub }) => crate::commands::dump::run(sub),
        Some(Command::Diff(args)) => crate::commands::diff::run(args),
        Some(Command::Review) => {
            run_tui(text_proto_log, files, continue_, resume, TuiStart::Review)
        },
        None => run_tui(text_proto_log, files, continue_, resume, TuiStart::Files),
    }
}

enum TuiStart {
    Review,
    Files,
}

fn run_tui(
    text_proto_log: Option<bool>,
    files: Vec<PathBuf>,
    continue_: bool,
    resume: bool,
    start: TuiStart,
) -> Result<(), Whatever> {
    stoat::ui::install_panic_hook();

    let (event_tx, event_rx) = tokio::sync::mpsc::channel(64);
    // Capacity 1: natural backpressure -- main thread won't render ahead
    // if the UI thread hasn't flushed the previous frame yet
    let (render_tx, render_rx) = tokio::sync::mpsc::channel(1);

    let mouse_capture_policy = stoat::default_mouse_capture_policy();
    let mouse_captured =
        stoat::resolve_mouse_captured(mouse_capture_policy, &stoat::host::LocalEnv);

    let ui_handle = stoat::ui::spawn(event_tx, render_rx, mouse_captured);

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .whatever_context("build tokio runtime")?;
    let scheduler = Arc::new(TokioScheduler::new(rt.handle().clone()));
    let executor = scheduler.executor();

    let cli_settings = Settings {
        text_proto_log,
        claude_default_placement: None,
        theme: None,
        mouse_capture: None,
        mode_badges: std::collections::BTreeMap::new(),
        claude_permissions: std::collections::BTreeMap::new(),
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

    rt.block_on(async {
        let mut stoat = Stoat::new(executor.clone(), cli_settings, initial_git_root);
        if continue_ || resume {
            stoat.load_active_workspace_state();
        }
        let permission_policy = ChainedPermissionPolicy::new(vec![
            Arc::new(BashDenialPolicy::new()),
            Arc::new(RuleBasedPolicy::from_settings(
                &stoat.settings.claude_permissions,
            )),
        ]);
        stoat.set_claude_code_host(Arc::new(
            ClaudeCodeLauncher::new(Arc::new(LocalFs), executor)
                .with_permission_callback(Arc::new(permission_policy)),
        ));
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

        stoat.run(event_rx, render_tx).await
    })
    .whatever_context("stoat event loop")?;

    ui_handle
        .join()
        .expect("ui thread panicked")
        .whatever_context("ui thread")?;

    Ok(())
}
