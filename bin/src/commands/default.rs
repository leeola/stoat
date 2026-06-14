use clap::{ArgAction, Parser, Subcommand};
use snafu::{ResultExt, Whatever};
use std::{
    io::{self, IsTerminal, Read, Write},
    path::PathBuf,
};

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
    #[arg(long, conflicts_with_all = ["session", "continue_", "inputs", "timeout"])]
    pub new: bool,

    /// Open the files in the live session with this `WorkspaceUid`. Errors when
    /// no app is running or that session is not live.
    #[arg(long, value_name = "ID", conflicts_with_all = ["continue_", "inputs", "timeout"])]
    pub session: Option<u64>,

    /// Write the text of buffer `ID` in the chosen session (the cwd-matched
    /// one, or `--session`) to stdout and exit, without opening a window. The
    /// read counterpart to piping stdin. Errors when no app is running or the
    /// buffer is unknown.
    #[arg(long, value_name = "ID", conflicts_with_all = ["files", "new", "continue_", "inputs", "timeout"])]
    pub buffer: Option<u64>,

    /// Drive a vim-style keystroke sequence into the window once it is ready,
    /// e.g. `--inputs ":wq<Enter>"`. Lets a headless run exercise interactive
    /// features without a keyboard. Spawns a window for this invocation rather
    /// than routing to a running app.
    #[arg(long)]
    pub inputs: Option<String>,

    /// Auto-close the spawned window after `SECONDS` so a run can capture logs
    /// for an action without a quit keystroke. Fractional values allowed (e.g.
    /// `1.5`). Spawns a window for this invocation rather than routing to a
    /// running app.
    #[arg(long, value_name = "SECONDS")]
    pub timeout: Option<f64>,

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
    /// Inspect and manage live sessions in the running app.
    Session {
        #[command(subcommand)]
        sub: crate::commands::session::SessionCommand,
    },
}

pub fn run() -> Result<(), Whatever> {
    let Args {
        command,
        files,
        continue_,
        new,
        session,
        buffer,
        inputs,
        timeout,
        ..
    } = Args::parse();

    if let Some(id) = buffer {
        let text = crate::commands::client::read_buffer_from_app(id, session)?;
        io::stdout()
            .write_all(text.as_bytes())
            .whatever_context("write buffer text to stdout")?;
        return Ok(());
    }

    let restore = if continue_ {
        stoat_gui::RestoreMode::Continue
    } else {
        stoat_gui::RestoreMode::None
    };

    match command {
        Some(Command::Dump { sub }) => crate::commands::dump::run(sub),
        Some(Command::Diff(args)) => crate::commands::diff::run(args),
        Some(Command::Session { sub }) => crate::commands::session::run(sub),
        None => {
            let stdin = read_piped_stdin();

            // --inputs/--timeout drive a freshly spawned window, so they bypass
            // the route-into-a-running-app path.
            let drive = inputs.is_some() || timeout.is_some();
            if !continue_ && !drive {
                if crate::commands::client::open_in_running_app(&files, new, session)? {
                    return Ok(());
                }
                if let Some(text) = &stdin
                    && crate::commands::client::pipe_to_running_app(text, new, session)?
                {
                    return Ok(());
                }
            }
            crate::commands::gui::run(files, restore, stdin, inputs, timeout)
        },
    }
}

/// Read piped stdin to a string, or `None` when stdin is a terminal or
/// empty. A non-tty stdin means content was piped in (`echo foo | stoat`),
/// which seeds a scratch buffer; the `is_terminal` guard mirrors the diff
/// adapter's check for stdout. A read error is logged and yields `None` so
/// the editor still launches normally.
fn read_piped_stdin() -> Option<String> {
    if io::stdin().is_terminal() {
        return None;
    }
    let mut buf = String::new();
    match io::stdin().read_to_string(&mut buf) {
        Ok(0) => None,
        Ok(_) => Some(buf),
        Err(err) => {
            tracing::warn!(?err, "failed to read piped stdin");
            None
        },
    }
}

#[cfg(test)]
mod tests {
    use super::Args;
    use clap::Parser;

    #[test]
    fn continue_parses() {
        let args =
            Args::try_parse_from(["stoat", "--continue", "foo.txt"]).expect("--continue parses");
        assert!(args.continue_);
    }

    #[test]
    fn resume_flag_removed() {
        assert!(Args::try_parse_from(["stoat", "-r"]).is_err());
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

    #[test]
    fn buffer_parses_an_id() {
        let args = Args::try_parse_from(["stoat", "--buffer", "7"]).expect("--buffer parses");
        assert_eq!(args.buffer, Some(7));
    }

    #[test]
    fn buffer_conflicts_with_files_and_launch_flags() {
        assert!(Args::try_parse_from(["stoat", "--buffer", "7", "foo.txt"]).is_err());
        assert!(Args::try_parse_from(["stoat", "--buffer", "7", "--new"]).is_err());
        assert!(Args::try_parse_from(["stoat", "--buffer", "7", "--timeout", "2"]).is_err());
    }

    #[test]
    fn buffer_allows_session_targeting() {
        let args = Args::try_parse_from(["stoat", "--buffer", "7", "--session", "5"])
            .expect("--buffer with --session parses");
        assert_eq!(args.buffer, Some(7));
        assert_eq!(args.session, Some(5));
    }

    #[test]
    fn inputs_and_timeout_parse() {
        let args =
            Args::try_parse_from(["stoat", "--inputs", "ifoo<Esc>", "--timeout", "1.5", "x"])
                .expect("--inputs/--timeout parse");
        assert_eq!(args.inputs.as_deref(), Some("ifoo<Esc>"));
        assert_eq!(args.timeout, Some(1.5));
    }

    #[test]
    fn drive_flags_conflict_with_selection() {
        assert!(Args::try_parse_from(["stoat", "--timeout", "2", "--new"]).is_err());
        assert!(Args::try_parse_from(["stoat", "--inputs", "i<Esc>", "--session", "5"]).is_err());
    }

    #[test]
    fn session_list_subcommand_parses() {
        assert!(Args::try_parse_from(["stoat", "session", "list"]).is_ok());
    }

    #[test]
    fn session_requires_a_subcommand() {
        assert!(Args::try_parse_from(["stoat", "session"]).is_err());
        assert!(Args::try_parse_from(["stoat", "session", "bogus"]).is_err());
    }

    #[test]
    fn session_buffers_subcommand_parses() {
        assert!(Args::try_parse_from(["stoat", "session", "buffers", "42"]).is_ok());
    }

    #[test]
    fn session_buffers_requires_an_id() {
        assert!(Args::try_parse_from(["stoat", "session", "buffers"]).is_err());
    }

    #[test]
    fn session_close_subcommand_parses() {
        assert!(Args::try_parse_from(["stoat", "session", "close", "42"]).is_ok());
    }

    #[test]
    fn session_close_requires_an_id() {
        assert!(Args::try_parse_from(["stoat", "session", "close"]).is_err());
    }
}
