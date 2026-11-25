//! The Run Kind of a Cmd, read from its line alone when the Cmd is made.
//!
//! The kind decides how a Run Block reads its Cmd. A Parsed Cmd runs a cargo
//! subcommand whose output the block can count, and its header shows a Kind
//! Chip that names the subcommand. An Agent Cmd runs `claude`. Every other
//! line is a plain Line Cmd.

/// The cargo global flags that cargo documents with a separate value word, so
/// the word after one is its value rather than the subcommand.
const CARGO_VALUE_FLAGS: [&str; 5] = ["--color", "--config", "--explain", "-C", "-Z"];

/// A cargo subcommand whose output a Parsed Cmd's block reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CargoKind {
    Test,
    Build,
    Clippy,
    Check,
}

/// What a Cmd's line runs, as [`run_kind`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RunKind {
    /// A shell line that the block shows as it prints.
    Line,
    /// A cargo subcommand that the block can parse.
    Parsed(CargoKind),
    /// A run of `claude`.
    Agent,
}

impl RunKind {
    /// The label of the Kind Chip that a Cmd of this kind shows at its header's
    /// right, or `None` for a kind with no chip.
    ///
    /// Only a Parsed Cmd has a chip.
    pub(crate) fn chip(self) -> Option<&'static str> {
        match self {
            RunKind::Parsed(CargoKind::Test) => Some("cargo test"),
            RunKind::Parsed(CargoKind::Build) => Some("cargo build"),
            RunKind::Parsed(CargoKind::Clippy) => Some("cargo clippy"),
            RunKind::Parsed(CargoKind::Check) => Some("cargo check"),
            RunKind::Line | RunKind::Agent => None,
        }
    }
}

/// The kind of run that `line` asks for.
///
/// The program is the first word of `line` past any leading `NAME=value`
/// words, which set its environment and run the same program. `claude` is an
/// Agent. `cargo` is Parsed when its subcommand is `test`, `build`, `clippy`,
/// or `check`, or the built-in alias `t`, `b`, or `c`, past a `+toolchain`
/// word and the global flags before the subcommand. Every other line is a
/// Line.
pub(crate) fn run_kind(line: &str) -> RunKind {
    match program_word(line) {
        Some("claude") => RunKind::Agent,
        Some("cargo") => {
            let args = line
                .split_whitespace()
                .skip_while(|word| *word != "cargo")
                .skip(1);
            cargo_kind(args).map_or(RunKind::Line, RunKind::Parsed)
        },
        _ => RunKind::Line,
    }
}

/// The program that `line` runs, its first word that is not a `NAME=value`
/// assignment, or `None` for a line of assignments alone.
fn program_word(line: &str) -> Option<&str> {
    line.split_whitespace().find(|word| !is_assignment(word))
}

/// Whether `word` sets an environment variable for the program after it.
///
/// The name before `=` starts with an ASCII letter or `_`, and the rest of it
/// is ASCII letters, digits, or `_`, as a shell reads an assignment.
fn is_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

/// The cargo subcommand that `args`, the words after `cargo`, name, when it is
/// one that a Parsed Cmd reads.
fn cargo_kind<'a>(mut args: impl Iterator<Item = &'a str>) -> Option<CargoKind> {
    while let Some(word) = args.next() {
        if word.starts_with('+') {
            continue;
        }
        if word.starts_with('-') {
            if CARGO_VALUE_FLAGS.contains(&word) {
                args.next();
            }
            continue;
        }
        return match word {
            "test" | "t" => Some(CargoKind::Test),
            "build" | "b" => Some(CargoKind::Build),
            "clippy" => Some(CargoKind::Clippy),
            "check" | "c" => Some(CargoKind::Check),
            _ => None,
        };
    }
    None
}

#[cfg(test)]
mod tests {
    use super::{run_kind, CargoKind, RunKind};

    /// The kind of each of `lines`.
    fn kinds<const N: usize>(lines: [&str; N]) -> [RunKind; N] {
        lines.map(run_kind)
    }

    #[test]
    fn cargo_subcommands_classify_as_parsed() {
        let test = RunKind::Parsed(CargoKind::Test);
        let build = RunKind::Parsed(CargoKind::Build);
        let check = RunKind::Parsed(CargoKind::Check);
        assert_eq!(
            kinds([
                "cargo test",
                "cargo t -p stoat",
                "cargo +nightly test",
                "cargo --color always test",
                "RUST_BACKTRACE=1 cargo test",
                "  cargo   test",
                "cargo --color=never -q build",
                "cargo b",
                "cargo -Z unstable-options check",
                "cargo c",
                "cargo -C sub clippy --fix",
            ]),
            [
                test,
                test,
                test,
                test,
                test,
                test,
                build,
                build,
                check,
                check,
                RunKind::Parsed(CargoKind::Clippy),
            ],
        );
    }

    #[test]
    fn other_lines_classify_as_line() {
        assert_eq!(
            kinds([
                "cargo fmt",
                "cargo",
                "cargo --version",
                "echo cargo test",
                "claudette",
                ""
            ]),
            [RunKind::Line; 6],
        );
    }

    #[test]
    fn a_claude_line_classifies_as_agent() {
        assert_eq!(
            kinds(["claude", "claude -p \"hi\"", "FOO=1 claude"]),
            [RunKind::Agent; 3],
        );
    }

    #[test]
    fn only_a_parsed_kind_has_a_chip() {
        assert_eq!(
            [
                RunKind::Parsed(CargoKind::Test),
                RunKind::Parsed(CargoKind::Build),
                RunKind::Parsed(CargoKind::Clippy),
                RunKind::Parsed(CargoKind::Check),
                RunKind::Line,
                RunKind::Agent,
            ]
            .map(RunKind::chip),
            [
                Some("cargo test"),
                Some("cargo build"),
                Some("cargo clippy"),
                Some("cargo check"),
                None,
                None,
            ],
        );
    }
}
