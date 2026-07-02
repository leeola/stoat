//! CLI arguments shared by the stoat editor and the stoatty terminal.
//!
//! Both binaries flatten [`CommonArgs`] into their own clap parser so the
//! workspace-open flags parse identically. stoatty reconstructs them with
//! [`CommonArgs::to_argv`] to forward to the stoat child it launches.

use clap::{Args, ValueHint};
use std::path::PathBuf;

/// The workspace-open arguments both stoat and stoatty accept.
///
/// These are the files to open, the `--continue`/`--resume` session-restore
/// selectors, and the `--inputs`/`--timeout` scripted-run controls.
#[derive(Args, Debug, PartialEq)]
pub struct CommonArgs {
    /// Files to open.
    #[arg(help = "Files to open", value_hint = ValueHint::FilePath)]
    pub files: Vec<PathBuf>,

    /// Restore the most-recently-used workspace for this repository instead of
    /// starting in a fresh one. `continue` is a Rust keyword, so the field is
    /// named `continue_` and exposed as `--continue` / `-c`.
    #[arg(short = 'c', long = "continue")]
    pub continue_: bool,

    /// Reopen the workspace whose state is the most recently modified among the
    /// current directory and its ancestors, falling back to a fresh workspace
    /// at the current directory. Mutually exclusive with `--continue`.
    #[arg(short = 'r', long = "resume", conflicts_with = "continue_")]
    pub resume: bool,

    /// Keystroke sequence fed once the editor is live, in the Helix/vim-style
    /// grammar (e.g. `ifoo<Esc>`). Drives the editor for a scripted run.
    #[arg(long = "inputs", value_name = "KEYS")]
    pub inputs: Option<String>,

    /// Auto-close the editor after this many seconds, so a scripted `--inputs`
    /// run exits on its own. Rejects non-finite or negative values.
    #[arg(long = "timeout", value_name = "SECONDS", value_parser = parse_timeout)]
    pub timeout: Option<f64>,
}

/// Parse and validate a `--timeout` value, rejecting non-finite or negative
/// seconds so a bad duration fails at parse time.
fn parse_timeout(value: &str) -> Result<f64, String> {
    let seconds: f64 = value
        .parse()
        .map_err(|_| format!("`{value}` is not a number"))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!("must be finite and non-negative, got {seconds}"));
    }
    Ok(seconds)
}

impl CommonArgs {
    /// Reconstruct the canonical argv these arguments parse from, for
    /// forwarding to a child process.
    ///
    /// The flags come first, then the file paths as positionals. Parsing the
    /// result back yields an equal [`CommonArgs`].
    pub fn to_argv(&self) -> Vec<String> {
        let mut argv = Vec::new();
        if self.continue_ {
            argv.push("--continue".to_string());
        }
        if self.resume {
            argv.push("--resume".to_string());
        }
        if let Some(inputs) = &self.inputs {
            argv.push("--inputs".to_string());
            argv.push(inputs.clone());
        }
        if let Some(timeout) = self.timeout {
            argv.push("--timeout".to_string());
            argv.push(timeout.to_string());
        }
        argv.extend(
            self.files
                .iter()
                .map(|file| file.to_string_lossy().into_owned()),
        );
        argv
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_timeout, CommonArgs};
    use clap::Parser;
    use std::path::PathBuf;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        common: CommonArgs,
    }

    fn round_trip(common: CommonArgs) {
        let mut argv = vec!["prog".to_string()];
        argv.extend(common.to_argv());
        assert_eq!(Harness::parse_from(argv).common, common);
    }

    #[test]
    fn to_argv_round_trips_files_and_flags() {
        round_trip(CommonArgs {
            files: vec![PathBuf::from("a.rs"), PathBuf::from("b.rs")],
            continue_: true,
            resume: false,
            inputs: None,
            timeout: None,
        });
        round_trip(CommonArgs {
            files: Vec::new(),
            continue_: false,
            resume: true,
            inputs: None,
            timeout: None,
        });
        round_trip(CommonArgs {
            files: vec![PathBuf::from("only.rs")],
            continue_: false,
            resume: false,
            inputs: Some("ifoo<Esc>".to_string()),
            timeout: Some(1.5),
        });
    }

    #[test]
    fn parse_timeout_rejects_non_finite_and_negative() {
        assert_eq!(parse_timeout("2.5"), Ok(2.5));
        assert_eq!(parse_timeout("0"), Ok(0.0));
        assert!(parse_timeout("-1").is_err());
        assert!(parse_timeout("nan").is_err());
        assert!(parse_timeout("inf").is_err());
        assert!(parse_timeout("abc").is_err());
    }
}
