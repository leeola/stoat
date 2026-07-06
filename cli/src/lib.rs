//! CLI arguments shared by the stoat editor and the stoatty terminal.
//!
//! Both binaries flatten [`CommonArgs`] into their own clap parser so the
//! workspace-open flags parse identically. stoatty reconstructs them with
//! [`CommonArgs::to_argv`] to forward to the stoat child it launches.

use clap::{builder::PossibleValuesParser, Args, Subcommand, ValueHint};
use std::path::PathBuf;

/// The deterministic fixtures both binaries expose, as `(name, one-line
/// description)`, in the order `stoat fixture ls` prints them.
///
/// This is the authoritative name set the `--fixture` value parser validates
/// against, and it must stay in sync with `stoat::fixture::materialize`. A
/// fixture-gated bin test asserts every entry here materializes.
pub const FIXTURES: &[(&str, &str)] = &[
    (
        "basic-diff",
        "two committed files, then one staged and one unstaged modification",
    ),
    (
        "diff-kinds",
        "one working tree holding every git change kind at once",
    ),
    (
        "many-files",
        "twelve files, ten changed across nested directories, for scale",
    ),
    (
        "history",
        "a four-commit linear chain for walking log and history",
    ),
    (
        "conflict",
        "two branches editing the same lines, so cherry-picking conflicts",
    ),
    (
        "rebase",
        "main and feature over disjoint files, so rebasing applies cleanly",
    ),
    (
        "rust-lsp",
        "a clean minimal cargo crate as a rust-analyzer target",
    ),
    (
        "rust-diff",
        "the rust-lsp crate plus a live staged and unstaged rust diff",
    ),
];

/// A clap value parser accepting only the [`FIXTURES`] names, so an unknown
/// fixture fails at parse time with a did-you-mean suggestion.
fn fixture_value_parser() -> PossibleValuesParser {
    PossibleValuesParser::new(FIXTURES.iter().map(|(name, _)| *name))
}

/// Render the fixture catalog as one `name  description` line per entry, the
/// names padded so the descriptions align.
pub fn ls_text() -> String {
    let width = FIXTURES
        .iter()
        .map(|(name, _)| name.len())
        .max()
        .unwrap_or(0);
    let mut out = String::new();
    for (name, description) in FIXTURES {
        out.push_str(&format!("{name:<width$}  {description}\n"));
    }
    out
}

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

    /// Materialize the named deterministic fixture into a fresh temp repo and
    /// open the editor there. Requires a stoat built with the `fixture`
    /// feature. A build without it rejects the flag at startup.
    #[arg(long = "fixture", value_name = "NAME", value_parser = fixture_value_parser())]
    pub fixture: Option<String>,
}

/// The `fixture` subcommand shared by both binaries.
///
/// `ls` lists the catalog, and a bare fixture name materializes and opens it.
/// clap resolves the literal `ls` to [`FixtureSub::Ls`] first. Any other token
/// falls to [`Self::name`], and neither present is an error the binaries report.
#[derive(Args, Debug, PartialEq)]
pub struct FixtureArgs {
    #[command(subcommand)]
    pub sub: Option<FixtureSub>,
    /// The fixture to materialize and open, validated against [`FIXTURES`].
    #[arg(value_name = "NAME", value_parser = fixture_value_parser())]
    pub name: Option<String>,
}

/// Subcommands of `fixture`.
#[derive(Subcommand, Debug, PartialEq)]
pub enum FixtureSub {
    /// List the fixture catalog.
    Ls,
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
        if let Some(fixture) = &self.fixture {
            argv.push("--fixture".to_string());
            argv.push(fixture.clone());
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
            fixture: None,
        });
        round_trip(CommonArgs {
            files: Vec::new(),
            continue_: false,
            resume: true,
            inputs: None,
            timeout: None,
            fixture: None,
        });
        round_trip(CommonArgs {
            files: vec![PathBuf::from("only.rs")],
            continue_: false,
            resume: false,
            inputs: Some("ifoo<Esc>".to_string()),
            timeout: Some(1.5),
            fixture: None,
        });
        round_trip(CommonArgs {
            files: Vec::new(),
            continue_: false,
            resume: false,
            inputs: None,
            timeout: None,
            fixture: Some("basic-diff".to_string()),
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

    #[test]
    fn fixture_flag_validates_against_the_catalog() {
        assert!(Harness::try_parse_from(["prog", "--fixture", "rust-lsp"]).is_ok());
        assert!(Harness::try_parse_from(["prog", "--fixture", "nonesuch"]).is_err());
    }

    #[test]
    fn ls_text_lists_every_fixture() {
        let text = super::ls_text();
        for (name, description) in super::FIXTURES {
            assert!(text.contains(name), "ls_text missing fixture {name}");
            assert!(
                text.contains(description),
                "ls_text missing description for {name}"
            );
        }
        assert_eq!(text.lines().count(), super::FIXTURES.len());
    }
}
