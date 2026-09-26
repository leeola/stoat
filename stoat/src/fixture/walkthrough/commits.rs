//! The `walkthrough-commits` fixture, the `walkthrough` crate grown over three
//! commits with a tour that reads each one.
//!
//! Every other fixture in the family commits its tour over one tree, so the
//! player never checks anything out. This one builds the crate up a commit at
//! a time, and its six stops name those commits two by two. Playing it checks
//! a commit out between stops, stays on one commit between the two stops that
//! share it, shows a file a commit modified rather than added, and follows an
//! annotation into another file at a commit.
//!
//! The tour lands in a fourth commit, so the reader starts on the whole crate.
//! Every commit carries a `main.rs`. rust-analyzer fails to load a package
//! with nothing to build, and its error then takes the status line from the
//! tour at each checkout. The first commit's `main` only loads the config, and
//! the third grows it into the `walkthrough` crate's own.

use crate::{
    fixture::{walkthrough::tour, FixtureError, FixtureRepo},
    walkthrough::Walkthrough,
};
use std::path::Path;

/// `main.rs` as the first commit holds it, before there is a server to run.
const FIRST_MAIN: &str = r#"mod config;

use std::path::PathBuf;

fn main() {
    let path = PathBuf::from("config.toml");
    let config = config::load(&path);
    println!("{} workers on {}", config.workers, config.addr);
}
"#;

const NARRATION_LOAD: &str = "\
The first commit is only the config. `load` reads a file of `key = value`
lines into the defaults, and a missing file leaves the defaults as they are.
";

const NARRATION_APPLY: &str = "\
Each line goes through `apply`, one key at a time. At this commit it knows two
keys, `addr` and `workers`.
";

const NARRATION_VERBOSE: &str = "\
The second commit adds the server, and the server logs. So `Config` gets a
`verbose` field, and `apply` gets the key that sets it.
";

const NARRATION_RUN: &str = "\
The server this commit adds serves one request at a time. `dispatch` is where
each request leaves the loop.
";

const NARRATION_HANDLE: &str = "\
The third commit adds the handler, with one arm for each method.
";

const NARRATION_MAIN: &str = "\
The same commit grows `main` into the program. It loads the config, builds the
server, and runs it, and the server calls the handler.
";

/// Build the fixture repository at `dest`.
pub(in crate::fixture) fn materialize(dest: &Path) -> Result<(), FixtureError> {
    let first_config = config_without_verbose();

    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "feat: load the config",
        &[
            ("Cargo.toml", tour::CARGO),
            ("src/config.rs", &first_config),
            ("src/main.rs", FIRST_MAIN),
        ],
    )?;
    let first = repo.head_sha()?;

    repo.commit(
        "feat: add the server",
        &[
            ("src/config.rs", tour::CONFIG),
            ("src/server.rs", tour::SERVER),
        ],
    )?;
    let second = repo.head_sha()?;

    repo.commit(
        "feat: dispatch to a handler",
        &[
            ("src/handler.rs", tour::HANDLER),
            ("src/main.rs", tour::MAIN),
        ],
    )?;
    let third = repo.head_sha()?;

    let json = super::tour_json(&build(&[first, second, third]));
    repo.commit(
        "docs: add the tour",
        &[(".stoat/walkthroughs/tour.json", &json)],
    )?;
    Ok(())
}

/// The six-stop tour the fixture commits, over `commits` oldest first.
///
/// Each pair of stops reads one commit. Every range is derived from the text
/// committed there, so a stop captures its own commit's bytes rather than the
/// final tree's.
pub(in crate::fixture) fn build(commits: &[String; 3]) -> Walkthrough {
    let first_config = config_without_verbose();
    let mut tour = Walkthrough::new("tour".to_string(), "How the server grew".to_string(), None);

    let s1 = tour
        .add_stop(
            Some("Loading the config".to_string()),
            NARRATION_LOAD.to_string(),
            super::location(
                "src/config.rs",
                &first_config,
                super::block_of(&first_config, "pub fn load(path: &Path)", "    config"),
            ),
            Some(commits[0].clone()),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    super::annotate(
        &mut tour,
        &s1,
        None,
        &first_config,
        super::span_of(&first_config, "unwrap_or_default()"),
        "a missing file is not an error",
        "",
    );

    tour.add_stop(
        Some("One key at a time".to_string()),
        NARRATION_APPLY.to_string(),
        super::location(
            "src/config.rs",
            &first_config,
            super::line_of(&first_config, "fn apply("),
        ),
        Some(commits[0].clone()),
        None,
    )
    .expect("appending a stop cannot fail");

    // A file the commit modified rather than added, so the diff view shows a
    // change inside the file rather than the whole of it as new.
    tour.add_stop(
        Some("A setting for the server".to_string()),
        NARRATION_VERBOSE.to_string(),
        super::location(
            "src/config.rs",
            tour::CONFIG,
            super::line_of(tour::CONFIG, "\"verbose\" => config.verbose"),
        ),
        Some(commits[1].clone()),
        None,
    )
    .expect("appending a stop cannot fail");

    let s4 = tour
        .add_stop(
            Some("The accept loop".to_string()),
            NARRATION_RUN.to_string(),
            super::location(
                "src/server.rs",
                tour::SERVER,
                super::line_of(tour::SERVER, "pub fn run(&mut self)"),
            ),
            Some(commits[1].clone()),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    super::annotate(
        &mut tour,
        &s4,
        None,
        tour::SERVER,
        super::span_of(tour::SERVER, "self.dispatch(request)"),
        "every request goes through here",
        "",
    );

    let s5 = tour
        .add_stop(
            Some("Handling a request".to_string()),
            NARRATION_HANDLE.to_string(),
            super::location(
                "src/handler.rs",
                tour::HANDLER,
                super::line_of(tour::HANDLER, "pub fn handle(request: &Request)"),
            ),
            Some(commits[2].clone()),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    for (needle, label) in [
        ("\"GET\" => read(request)", "reads the path back"),
        ("\"POST\" => write(request)", "echoes the body"),
        ("\"DELETE\" => remove(request)", "answers 204, with no body"),
        ("_ => Response::not_found()", "anything else is a 404"),
    ] {
        super::annotate(
            &mut tour,
            &s5,
            None,
            tour::HANDLER,
            super::span_of(tour::HANDLER, needle),
            label,
            "",
        );
    }

    let s6 = tour
        .add_stop(
            Some("Wiring it together".to_string()),
            NARRATION_MAIN.to_string(),
            super::location(
                "src/main.rs",
                tour::MAIN,
                super::line_of(tour::MAIN, "fn main() {"),
            ),
            Some(commits[2].clone()),
            None,
        )
        .expect("appending a stop cannot fail")
        .id
        .clone();
    super::annotate(
        &mut tour,
        &s6,
        Some("src/server.rs"),
        tour::SERVER,
        super::span_of(tour::SERVER, "handler::handle(&request)"),
        "where the server calls the new handler",
        "",
    );

    tour
}

/// `config.rs` as the first commit holds it, before the server needed a
/// `verbose` setting.
///
/// Derived from [`tour::CONFIG`] by dropping the setting's three lines, so the
/// two versions share every other byte and a diff between them shows exactly
/// the setting arriving.
fn config_without_verbose() -> String {
    tour::CONFIG
        .replace("    pub verbose: bool,\n", "")
        .replace("            verbose: false,\n", "")
        .replace(
            "        \"verbose\" => config.verbose = value == \"true\",\n",
            "",
        )
}

#[cfg(test)]
mod tests {
    use crate::{
        fixture::walkthrough::tour,
        host::LocalFs,
        walkthrough::{self, store},
    };
    use git2::{Repository, Sort};
    use std::path::Path;

    /// Each stop reads the commit it names, so the tour validates against
    /// those trees. Read through the working tree alone, the stops over the
    /// first commit's config.rs point at lines the `verbose` setting moved
    /// down, which is what makes the commits matter.
    #[test]
    fn commits_tour_validates_against_the_trees_it_names() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();
        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");

        let at_commits = walkthrough::validate(&tour, &store::reader(&LocalFs, dir.path()));
        let working = store::workspace_reader(&LocalFs, dir.path());
        let at_head: Vec<(String, Option<String>)> =
            walkthrough::validate(&tour, &|_: Option<&str>, path: &Path| working(path))
                .into_iter()
                .map(|finding| (finding.stop, finding.annotation))
                .collect();

        assert_eq!(
            at_commits,
            Vec::new(),
            "every range reads what it captured at its own commit",
        );
        assert_eq!(
            at_head,
            [
                ("s1".to_owned(), None),
                ("s1".to_owned(), Some("a1".to_owned())),
                ("s2".to_owned(), None),
            ],
            "only the first commit's stops read another config.rs at HEAD",
        );
    }

    #[test]
    fn commits_tour_names_three_distinct_commits_in_order() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();
        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");

        let repo = Repository::open(dir.path()).expect("the fixture is a repository");
        let mut walk = repo.revwalk().expect("revwalk");
        walk.push_head().expect("HEAD has a commit");
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::REVERSE)
            .expect("sort oldest first");
        let history: Vec<String> = walk.map(|oid| oid.expect("walkable").to_string()).collect();

        let named: Vec<Option<&str>> = tour
            .stops
            .iter()
            .map(|stop| stop.commit.as_deref())
            .collect();
        let expected: Vec<Option<&str>> = [0, 0, 1, 1, 2, 2]
            .into_iter()
            .map(|at| Some(history[at].as_str()))
            .collect();
        assert_eq!(
            history.len(),
            4,
            "three commits of code and one of the tour"
        );
        assert_eq!(
            named, expected,
            "the stops read the code commits two by two"
        );
    }

    /// The reader starts on the finished crate with its tour, before any stop
    /// checks an older commit out.
    #[test]
    fn head_holds_the_tour_and_the_whole_crate() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();

        let working = store::workspace_reader(&LocalFs, dir.path());
        let read = |name: &str| {
            working(Path::new(name)).unwrap_or_else(|| panic!("{name} is in the working tree"))
        };
        assert_eq!(
            [
                read("Cargo.toml"),
                read("src/main.rs"),
                read("src/config.rs"),
                read("src/server.rs"),
                read("src/handler.rs"),
            ],
            [
                tour::CARGO,
                tour::MAIN,
                tour::CONFIG,
                tour::SERVER,
                tour::HANDLER,
            ],
            "the working tree is the walkthrough crate",
        );
        assert_eq!(
            store::load(&LocalFs, dir.path(), "tour")
                .expect("the tour is committed")
                .stops
                .len(),
            6,
        );
    }
}
