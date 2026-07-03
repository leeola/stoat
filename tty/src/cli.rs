use clap::{Parser, ValueHint};
use std::path::PathBuf;
use stoat_cli::CommonArgs;

/// The version string `stoatty --version` prints, and the value exported to
/// child programs via `STOATTY_VERSION` so an inner stoat can report it.
///
/// Shape is `<semver> (<hash>[-dirty] <date>)`. `STOATTY_BUILD_INFO` is the
/// git hash and date emitted by build.rs.
pub(crate) const VERSION_INFO: &str = concat!(
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("STOATTY_BUILD_INFO"),
    ")",
);

/// Per-invocation launch overrides for the stoatty terminal, parsed from argv.
///
/// Flags here apply to a single run and take precedence over the persistent
/// `[shell]` config; with no flags, stoatty falls back to the configured shell
/// or the system default.
#[derive(Parser)]
#[command(
    name = "stoatty",
    about = "A GPU-accelerated terminal emulator",
    version = VERSION_INFO
)]
pub struct Cli {
    /// Run PROGRAM (with any following arguments) instead of the shell, e.g.
    /// `stoatty -e nvim --clean`. Overrides the `[shell]` config for this run.
    #[arg(
        short = 'e',
        long = "command",
        value_name = "PROGRAM",
        allow_hyphen_values = true,
        num_args = 1..,
    )]
    command: Vec<String>,

    /// Set the spawned command's working directory for this run. Defaults to
    /// stoatty's own working directory when unset.
    #[arg(long = "working-directory", value_name = "DIR", value_hint = ValueHint::DirPath)]
    pub working_directory: Option<PathBuf>,

    /// Files to open and the session-restore flags, forwarded to the stoat
    /// editor when it is the launched child. Ignored under `-e`/`--command` and
    /// `--terminal`, which run their own program.
    #[command(flatten)]
    pub common: CommonArgs,

    /// Run the login shell instead of the stoat editor, so stoatty opens as a
    /// plain terminal. Cannot combine with `-e`/`--command`, which already
    /// names its own program.
    #[arg(long = "terminal", conflicts_with = "command")]
    pub terminal: bool,
}

impl Cli {
    /// The launch program and its arguments from `-e`/`--command`, or `None`
    /// when the flag was not given. The first token is the program, the rest
    /// its arguments.
    pub fn command(&self) -> Option<(String, Vec<String>)> {
        let (program, args) = self.command.split_first()?;
        Some((program.clone(), args.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::Parser;

    #[test]
    fn command_splits_program_from_args() {
        let cli = Cli::parse_from(["stoatty", "-e", "nvim", "--clean"]);
        assert_eq!(
            cli.command(),
            Some(("nvim".to_string(), vec!["--clean".to_string()]))
        );
    }

    #[test]
    fn bare_argv_has_no_command() {
        let cli = Cli::parse_from(["stoatty"]);
        assert_eq!(cli.command(), None);
    }

    #[test]
    fn command_without_args() {
        let cli = Cli::parse_from(["stoatty", "--command", "ls"]);
        assert_eq!(cli.command(), Some(("ls".to_string(), Vec::new())));
    }

    #[test]
    fn working_directory_parses_path() {
        use std::path::PathBuf;

        let cli = Cli::parse_from(["stoatty", "--working-directory", "/tmp"]);
        assert_eq!(cli.working_directory, Some(PathBuf::from("/tmp")));
        assert_eq!(Cli::parse_from(["stoatty"]).working_directory, None);
    }

    #[test]
    fn bare_positionals_collect_as_files() {
        use std::path::PathBuf;

        let cli = Cli::parse_from(["stoatty", "a.rs", "b.rs"]);
        assert_eq!(
            cli.common.files,
            vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")]
        );
        assert_eq!(cli.command(), None);
    }

    #[test]
    fn dash_e_consumes_trailing_args_leaving_files_empty() {
        use std::path::PathBuf;

        let cli = Cli::parse_from(["stoatty", "-e", "nvim", "a.rs"]);
        assert_eq!(
            cli.command(),
            Some(("nvim".to_string(), vec!["a.rs".to_string()]))
        );
        assert_eq!(cli.common.files, Vec::<PathBuf>::new());
    }

    #[test]
    fn terminal_flag_parses() {
        assert!(Cli::parse_from(["stoatty", "--terminal"]).terminal);
        assert!(!Cli::parse_from(["stoatty"]).terminal);
    }

    #[test]
    fn terminal_conflicts_with_command() {
        assert!(Cli::try_parse_from(["stoatty", "--terminal", "-e", "sh"]).is_err());
    }
}
