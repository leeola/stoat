use clap::{ArgAction, Parser, Subcommand};
use snafu::Whatever;
use std::path::PathBuf;

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
    /// Render a structural diff to stdout. By default, scans the
    /// current repo for changes against HEAD and renders a diff
    /// for each changed path. With `--git`, acts as the
    /// `GIT_EXTERNAL_DIFF` adapter using the seven path arguments
    /// git supplies; positional args are not accepted in the
    /// default mode.
    Diff(crate::commands::diff::DiffArgs),
}

pub fn run() -> Result<(), Whatever> {
    let Args { command, .. } = Args::parse();

    match command {
        Some(Command::Dump { sub }) => crate::commands::dump::run(sub),
        Some(Command::Diff(args)) => crate::commands::diff::run(args),
        Some(Command::Review) | None => print_gui_hint(),
    }
}

/// Until the GPUI entry lands, the default and `Review` invocations
/// have no working backend; tell the user where the new entry will
/// live and exit.
fn print_gui_hint() -> Result<(), Whatever> {
    eprintln!("use `stoat gui`");
    Ok(())
}
