//! CLI arguments shared by the stoat editor and the stoatty terminal.
//!
//! Both binaries flatten [`CommonArgs`] into their own clap parser so the
//! workspace-open flags parse identically. stoatty reconstructs them with
//! [`CommonArgs::to_argv`] to forward to the stoat child it launches.

use clap::{Args, ValueHint};
use std::path::PathBuf;

/// The workspace-open arguments both stoat and stoatty accept.
///
/// These are the files to open and the `--continue`/`--resume` session-restore
/// selectors.
#[derive(Args, Debug, PartialEq, Eq)]
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
    use super::CommonArgs;
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
        });
        round_trip(CommonArgs {
            files: Vec::new(),
            continue_: false,
            resume: true,
        });
        round_trip(CommonArgs {
            files: vec![PathBuf::from("only.rs")],
            continue_: false,
            resume: false,
        });
    }
}
