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

    /// Reopen the previous session. Walks the current directory and its
    /// ancestors and restores the most-recently-saved workspace among
    /// them, falling back to a fresh workspace anchored at cwd when none
    /// has any state. `continue` is a Rust keyword so the field is named
    /// `continue_`; clap exposes it as `--continue` / `-c`.
    #[arg(short = 'c', long = "continue", global = true)]
    pub continue_: bool,

    /// Open the files in a fresh session/window in the running app instead of
    /// the session enclosing the current directory, spawning the app when none
    /// is running.
    #[arg(long, conflicts_with_all = ["session", "continue_"])]
    pub new: bool,

    /// Open the files in the live session with this `WorkspaceUid`. Errors when
    /// no app is running or that session is not live.
    #[arg(long, value_name = "ID", conflicts_with = "continue_")]
    pub session: Option<u64>,

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
    /// Open the GPUI editor window.
    Gui {
        /// Drive a vim-style keystroke sequence into the window once it is
        /// ready, e.g. `--inputs ":wq<Enter>"`. Lets a headless run exercise
        /// interactive features without a keyboard.
        #[arg(long)]
        inputs: Option<String>,
        /// Auto-close the window after `SECONDS` so a run can capture logs
        /// for an action without a quit keystroke. Fractional values
        /// allowed (e.g. `1.5`).
        #[arg(long, value_name = "SECONDS")]
        timeout: Option<f64>,
    },
}

pub fn run() -> Result<(), Whatever> {
    let Args {
        command,
        files,
        continue_,
        new,
        session,
        ..
    } = Args::parse();

    let restore = if continue_ {
        stoat_gui::RestoreMode::Continue
    } else {
        stoat_gui::RestoreMode::None
    };

    match command {
        Some(Command::Dump { sub }) => crate::commands::dump::run(sub),
        Some(Command::Diff(args)) => crate::commands::diff::run(args),
        Some(Command::Gui { inputs, timeout }) => {
            crate::commands::gui::run(files, restore, inputs, timeout)
        },
        None => {
            if !continue_ && crate::commands::client::open_in_running_app(&files, new, session)? {
                Ok(())
            } else {
                crate::commands::gui::run(files, restore, None, None)
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    #[test]
    fn continue_parses_after_gui_subcommand() {
        let args =
            Args::try_parse_from(["stoat", "gui", "--continue"]).expect("`gui --continue` parses");
        assert!(args.continue_);
    }

    #[test]
    fn continue_parses_before_gui_subcommand() {
        let args = Args::try_parse_from(["stoat", "-c", "gui"]).expect("`-c gui` parses");
        assert!(args.continue_);
    }

    #[test]
    fn resume_flag_removed() {
        assert!(Args::try_parse_from(["stoat", "gui", "-r"]).is_err());
    }

    #[test]
    fn session_parses_a_uid() {
        let args = Args::try_parse_from(["stoat", "--session", "42", "foo.txt"])
            .expect("--session parses");
        assert_eq!(args.session, Some(42));
        assert!(!args.new);
    }

    #[test]
    fn new_conflicts_with_session_and_continue() {
        assert!(Args::try_parse_from(["stoat", "--new", "--session", "5"]).is_err());
        assert!(Args::try_parse_from(["stoat", "--new", "--continue"]).is_err());
    }

    #[test]
    fn session_conflicts_with_continue() {
        assert!(Args::try_parse_from(["stoat", "--session", "5", "--continue"]).is_err());
    }
}
