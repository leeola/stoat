use clap::{Parser, ValueHint};
use std::path::PathBuf;

/// Per-invocation launch overrides for the stoatty terminal, parsed from argv.
///
/// Flags here apply to a single run and take precedence over the persistent
/// `[shell]` config; with no flags, stoatty falls back to the configured shell
/// or the system default.
#[derive(Parser)]
#[command(
    name = "stoatty",
    about = "A GPU-accelerated terminal emulator",
    version
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
}
