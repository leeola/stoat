//! The `walkthrough-trail` fixture: a build pipeline whose tour steps through
//! every relation the trail between two stops is made of.
//!
//! Stepping calls `install_step_trail`, which resolves both stops to symbols
//! and asks the code graph how they relate. Four searches run in order and the
//! first hit becomes both the trail and the `(trail: N stops)` note: `a` calls
//! `b`, `b` calls `a`, the two share an ancestor, the two share a descendant.
//! No committed tour reaches past the first.
//!
//! The code is free functions calling each other across modules. A
//! `crate::`-qualified free call is captured as a bare name and resolved
//! against the whole workspace by that name, so every function here is named
//! once and no call is ambiguous.
//!
//! Two of the functions exist only for the searches that a single-rooted graph
//! never reaches. `dry_run` is a second root nothing calls, so it shares no
//! ancestor with the rest while sharing the callee `dedupe`. Without it the
//! shared-descendant search never runs, because any two symbols under one root
//! meet at that root first. `version` is joined to nothing at all, which is the
//! only way a step finds no relation and clears the trail.

use crate::{
    fixture::{FixtureError, FixtureRepo},
    walkthrough::Walkthrough,
};
use std::path::Path;

const CARGO: &str = r#"[package]
name = "fixture-walkthrough-trail"
version = "0.1.0"
edition = "2021"

[workspace]
"#;

const README: &str = r#"# The build pipeline

Three stages, run in order from `main`:

1. `cli` reads the arguments and validates them.
2. `planner` expands the request into steps and orders them.
3. `executor` runs the ordered steps.

Every stage ends by deduplicating what it produced, which is why `util`
is the one module all three reach.
"#;

const MAIN: &str = r#"mod cli;
mod executor;
mod planner;
mod util;

fn main() {
    let args = cli::parse();
    let plan = planner::plan(&args);
    executor::run(plan);
}
"#;

const CLI: &str = r#"/// The arguments as the pipeline reads them.
pub struct Args {
    pub targets: Vec<String>,
    pub jobs: usize,
}

/// Read the command line into the arguments the later stages act on.
pub fn parse() -> Args {
    let args = Args {
        targets: vec!["all".to_string()],
        jobs: 4,
    };
    validate(&args);
    args
}

/// Reject an argument set the later stages have no answer for.
fn validate(args: &Args) {
    if args.jobs == 0 {
        panic!("jobs must be at least one");
    }
}
"#;

const PLANNER: &str = r#"use crate::cli::Args;
use crate::util;

/// One unit of work, as the executor receives it.
pub struct Step {
    pub target: String,
    pub stage: u8,
}

/// Turn the arguments into the steps that satisfy them, in run order.
pub fn plan(args: &Args) -> Vec<Step> {
    let steps = expand(args);
    order(steps)
}

/// One step per target per stage, before anything is ordered.
fn expand(args: &Args) -> Vec<Step> {
    let mut steps = Vec::new();
    for target in &args.targets {
        for stage in 0..3 {
            steps.push(Step {
                target: target.clone(),
                stage,
            });
        }
    }
    util::dedupe(steps)
}

/// Sort the steps by stage, so an earlier stage never runs after a later one.
fn order(mut steps: Vec<Step>) -> Vec<Step> {
    steps.sort_by_key(|step| step.stage);
    util::dedupe(steps)
}
"#;

const EXECUTOR: &str = r#"use crate::planner::Step;
use crate::util;

/// Run every step, in the order the planner put them in.
pub fn run(steps: Vec<Step>) {
    for step in util::dedupe(steps) {
        println!("stage {} for {}", step.stage, step.target);
    }
}

/// Report every step a run performs, without performing any of them.
///
/// Reached from the command line rather than from `main`, so it shares no
/// caller with the rest of the pipeline while sharing its last stage.
pub fn dry_run(steps: Vec<Step>) {
    for step in util::dedupe(steps) {
        println!("would run stage {} for {}", step.stage, step.target);
    }
}
"#;

const UTIL: &str = r#"use crate::planner::Step;

/// Drop steps that repeat a target within a stage, keeping the first.
pub fn dedupe(steps: Vec<Step>) -> Vec<Step> {
    let mut seen = Vec::new();
    let mut kept = Vec::new();

    for step in steps {
        let key = (step.target.clone(), step.stage);
        if seen.contains(&key) {
            continue;
        }
        seen.push(key);
        kept.push(step);
    }

    kept
}

/// The pipeline's version string, for the banner.
///
/// Nothing calls this and it calls nothing, so it is joined to no other
/// symbol in the crate.
pub fn version() -> &'static str {
    "0.1.0"
}
"#;

const NARRATION_MAIN: &str = "\
The whole pipeline hangs off these three calls, and every step from here
lands on something one of them reaches.

The trail is laid from the code index, which builds after the workspace
opens. A step taken before it settles lays nothing. Step back and forward
again and the trail appears.
";

const NARRATION_VALIDATE: &str = "\
`main` does not call this. It calls `parse`, and `parse` calls this.

The search that finds a caller chain walks as far as it has to, so the note
counts three stops rather than two.
";

const NARRATION_PARSE: &str = "\
This is the step the reader just came through, taken backwards.

The first search asks whether this stop calls the last one. It does not, so
the second asks the reverse, and that is the one that answers.
";

const NARRATION_PLAN: &str = "\
Neither this nor `parse` calls the other, so both call searches come back
empty and the third one runs.

`main` calls them both, and that shared caller is the trail: out of one, up
to `main`, and down into the other.
";

const NARRATION_EXPAND: &str = "\
Back to the shortest relation there is. `plan` calls this directly, so the
first search answers immediately and the trail is the two stops themselves.

A direct call is never passed over for a longer path through a neighbour,
which is why the searches run in this order.
";

const NARRATION_DRY_RUN: &str = "\
Nothing calls this one, so it shares no caller with `expand` and the first
three searches all come back empty.

The fourth asks what they both call. Both end in `dedupe`, and that shared
callee is the only thing joining them.
";

const NARRATION_VERSION: &str = "\
This function calls nothing and nothing calls it, so no search finds a path
from the last stop at all.

The trail clears rather than staying where it was, since a trail left up
claims a connection these two stops do not have.
";

const NARRATION_CARGO: &str = "\
The manifest holds no symbols, so the stop resolves to nothing before any
search is even tried.

That is the ordinary case for a stop over configuration, and it clears the
trail the same way no relation does.
";

const NARRATION_README: &str = "\
The same again from a file the index reads no definitions out of.

A tour is free to end somewhere that is not code, and the trail simply has
nothing to say about it.
";

/// Build the fixture repository at `dest`.
pub(in crate::fixture) fn materialize(dest: &Path) -> Result<(), FixtureError> {
    let json = super::tour_json(&build());

    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[
            ("Cargo.toml", CARGO),
            ("README.md", README),
            ("src/main.rs", MAIN),
            ("src/cli.rs", CLI),
            ("src/planner.rs", PLANNER),
            ("src/executor.rs", EXECUTOR),
            ("src/util.rs", UTIL),
            (".stoat/walkthroughs/tour.json", &json),
        ],
    )?;
    Ok(())
}

/// The nine-stop tour the `walkthrough-trail` fixture commits.
///
/// The order carries the meaning. Each stop is chosen for the relation the step
/// onto it exercises, so walking the tour once runs all four searches and both
/// ways of finding nothing.
fn build() -> Walkthrough {
    let mut tour = Walkthrough::new(
        "tour".to_string(),
        "How the stops are related".to_string(),
        None,
    );

    for (title, narration, path, content, needle) in [
        (
            "Where it starts",
            NARRATION_MAIN,
            "src/main.rs",
            MAIN,
            "fn main() {",
        ),
        (
            "Two hops down",
            NARRATION_VALIDATE,
            "src/cli.rs",
            CLI,
            "fn validate(args: &Args)",
        ),
        (
            "Back up one",
            NARRATION_PARSE,
            "src/cli.rs",
            CLI,
            "pub fn parse() -> Args",
        ),
        (
            "A shared caller",
            NARRATION_PLAN,
            "src/planner.rs",
            PLANNER,
            "pub fn plan(args: &Args)",
        ),
        (
            "A direct call",
            NARRATION_EXPAND,
            "src/planner.rs",
            PLANNER,
            "fn expand(args: &Args)",
        ),
        (
            "A shared callee",
            NARRATION_DRY_RUN,
            "src/executor.rs",
            EXECUTOR,
            "pub fn dry_run(steps",
        ),
        (
            "Joined to nothing",
            NARRATION_VERSION,
            "src/util.rs",
            UTIL,
            "pub fn version()",
        ),
        (
            "Not code at all",
            NARRATION_CARGO,
            "Cargo.toml",
            CARGO,
            "[package]",
        ),
        (
            "Not even a language",
            NARRATION_README,
            "README.md",
            README,
            "# The build pipeline",
        ),
    ] {
        tour.add_stop(
            Some(title.to_string()),
            narration.to_string(),
            super::location(path, content, super::line_of(content, needle)),
            None,
        )
        .expect("appending a stop cannot fail");
    }

    tour
}

#[cfg(test)]
mod tests {
    use crate::{
        host::LocalFs,
        walkthrough::{self, store},
    };
    use std::path::Path;

    /// Each stop's file and the line it must land on, in tour order.
    ///
    /// The snippet is the whole definition line rather than the name alone,
    /// because a stop that lands one line off still resolves to a symbol and
    /// the trail it lays is quietly the wrong one.
    const STOPS: [(&str, &str); 9] = [
        ("src/main.rs", "fn main() {"),
        ("src/cli.rs", "fn validate(args: &Args) {"),
        ("src/cli.rs", "pub fn parse() -> Args {"),
        ("src/planner.rs", "pub fn plan(args: &Args) -> Vec<Step> {"),
        ("src/planner.rs", "fn expand(args: &Args) -> Vec<Step> {"),
        ("src/executor.rs", "pub fn dry_run(steps: Vec<Step>) {"),
        ("src/util.rs", "pub fn version() -> &'static str {"),
        ("Cargo.toml", "[package]"),
        ("README.md", "# The build pipeline"),
    ];

    /// The tour is built from ranges derived out of the source consts, so a
    /// const gaining a line must not leave a range pointing at the wrong code.
    /// `validate` catches that, and this runs it against the materialized
    /// repository rather than against the builder's own idea of the text.
    #[test]
    fn trail_tour_validates_against_the_committed_sources() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();

        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");
        let drift = walkthrough::validate(&tour, &store::workspace_reader(&LocalFs, dir.path()));
        assert_eq!(
            drift,
            Vec::new(),
            "every range in the tour still covers what it captured",
        );
    }

    /// The relation a step exercises is decided entirely by which two
    /// definitions the stops sit inside, so the tour is only staging what it
    /// claims while every stop lands on the line named here.
    #[test]
    fn trail_tour_stops_sit_inside_the_named_definitions() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();
        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");

        assert_eq!(
            tour.stops
                .iter()
                .map(|stop| (stop.focus.path.as_path(), stop.focus.snippet.as_str()))
                .collect::<Vec<_>>(),
            STOPS
                .iter()
                .map(|(path, snippet)| (Path::new(*path), *snippet))
                .collect::<Vec<_>>(),
            "every stop opens the definition its step is named for",
        );
    }
}
