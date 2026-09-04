//! The `walkthrough-catalog` fixture, one workspace holding four tours.
//!
//! Every other fixture in the family commits a single tour, so three parts of
//! the player exist only for a workspace that holds several and have nowhere to
//! be seen. The `:walkthrough` picker offers what is stored. The badge names
//! the slug of the tour in play. And `open` replaces one run with another.
//!
//! Three tour shapes are unreachable from one six-stop tour as well. A tour of
//! one stop, where a step off either end reports the tour is over rather than
//! moving. A tour with no annotations, where the walk through the stops is the
//! whole walk. And a tour with no stops, which never becomes a run at all, so
//! opening it reports why and leaves the tour already playing where it was.
//!
//! The sources are the `walkthrough` crate's own. Only the tours over them
//! differ, so a reader who has learned that code once reads this fixture
//! without learning any more.

use crate::{
    fixture::{walkthrough::tour, FixtureError, FixtureRepo},
    walkthrough::Walkthrough,
};
use std::path::Path;

/// Narration for the one-stop tour, which is the whole of what it has to say.
const NARRATION_SOLO: &str = "\
One stop, so this is both the start and the end. Stepping either way reports
the tour is over rather than moving.
";

const NARRATION_ENTRY: &str = "\
Three stops and no annotations anywhere, so stepping through the stops is the
whole of the walk.

Nothing here is marked but the focus itself, which is what a tour written
before annotations existed still reads like.
";

const NARRATION_LOAD: &str = "\
The second of the three. A tour this plain has no sub-steps, so `a` reports
there is nothing to step onto.
";

const NARRATION_HANDLE: &str = "\
The last of the three. Stepping on from here reports the tour is over, the
same way the one-stop tour does from its only stop.
";

/// Build the fixture repository at `dest`.
pub(in crate::fixture) fn materialize(dest: &Path) -> Result<(), FixtureError> {
    let tours = [solo(), plain(), empty()].map(|built| super::tour_json(&built));
    let baseline = super::tour_json(&tour::build());

    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[
            ("Cargo.toml", tour::CARGO),
            ("src/main.rs", tour::MAIN),
            ("src/config.rs", tour::CONFIG),
            ("src/server.rs", tour::SERVER),
            ("src/handler.rs", tour::HANDLER),
            (".stoat/walkthroughs/tour.json", &baseline),
            (".stoat/walkthroughs/solo.json", &tours[0]),
            (".stoat/walkthroughs/plain.json", &tours[1]),
            (".stoat/walkthroughs/empty.json", &tours[2]),
        ],
    )?;
    Ok(())
}

/// One untitled stop and nothing else.
///
/// The title falls back to the tour's, the progress reads `1/1`, and both
/// directions of a step land back where they started.
fn solo() -> Walkthrough {
    let mut tour = Walkthrough::new(
        "solo".to_string(),
        "The whole tour, in one stop".to_string(),
        None,
    );
    tour.add_stop(
        None,
        NARRATION_SOLO.to_string(),
        super::location(
            "src/main.rs",
            tour::MAIN,
            super::line_of(tour::MAIN, "fn main() {"),
        ),
        None,
    )
    .expect("appending a stop cannot fail");
    tour
}

/// Three titled stops across three files, with no annotation on any of them.
fn plain() -> Walkthrough {
    let mut plain = Walkthrough::new(
        "plain".to_string(),
        "Three stops, nothing marked".to_string(),
        None,
    );

    for (title, narration, path, content, needle) in [
        (
            "Entry point",
            NARRATION_ENTRY,
            "src/main.rs",
            tour::MAIN,
            "fn main() {",
        ),
        (
            "Loading the config",
            NARRATION_LOAD,
            "src/config.rs",
            tour::CONFIG,
            "pub fn load(path: &Path)",
        ),
        (
            "Handling a request",
            NARRATION_HANDLE,
            "src/handler.rs",
            tour::HANDLER,
            "pub fn handle(request: &Request)",
        ),
    ] {
        plain
            .add_stop(
                Some(title.to_string()),
                narration.to_string(),
                super::location(path, content, super::line_of(content, needle)),
                None,
            )
            .expect("appending a stop cannot fail");
    }

    plain
}

/// A stored tour with no stops.
///
/// It loads and it lists, so the picker offers it. It never becomes a run,
/// which is the whole of what it stages.
fn empty() -> Walkthrough {
    Walkthrough::new(
        "empty".to_string(),
        "Nothing to walk through".to_string(),
        None,
    )
}

#[cfg(test)]
mod tests {
    use crate::{
        host::LocalFs,
        walkthrough::{self, store},
    };

    /// Every slug the workspace holds, in the order `store::list` sorts them,
    /// which is the order the picker offers them in.
    const SLUGS: [&str; 4] = ["empty", "plain", "solo", "tour"];

    /// Four tours means four chances for a range to go stale, and the three
    /// written here point into the same sources the baseline does. Validating
    /// every one of them catches an edit to those sources that only breaks the
    /// tours nothing else checks.
    #[test]
    fn catalog_tours_validate_against_the_committed_sources() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();
        let read = store::workspace_reader(&LocalFs, dir.path());

        let drifted = store::list(&LocalFs, dir.path())
            .expect("the tours are committed")
            .into_iter()
            .filter(|summary| {
                let tour =
                    store::load(&LocalFs, dir.path(), &summary.slug).expect("it just listed");
                !walkthrough::validate(&tour, &read).is_empty()
            })
            .map(|summary| summary.slug)
            .collect::<Vec<_>>();

        assert_eq!(
            drifted,
            Vec::<String>::new(),
            "every range in every stored tour still covers what it captured",
        );
    }

    /// The catalog is four tours, each chosen for a shape no single tour holds.
    /// A tour that gains a stop, an annotation, or a title stops staging the
    /// shape it was written for while still loading and still validating.
    #[test]
    fn catalog_stages_four_tour_shapes() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();

        assert_eq!(
            store::list(&LocalFs, dir.path())
                .expect("the tours are committed")
                .into_iter()
                .map(|summary| summary.slug)
                .collect::<Vec<_>>(),
            SLUGS,
            "the picker lists four tours, sorted by slug",
        );

        let load = |slug: &str| store::load(&LocalFs, dir.path(), slug).expect("it is committed");

        let solo = load("solo");
        assert_eq!(
            (
                solo.stops.len(),
                solo.stops[0].title.as_deref(),
                solo.stops[0].annotations.len(),
            ),
            (1, None, 0),
            "the solo tour is one untitled stop with nothing marked on it",
        );

        let plain = load("plain");
        assert_eq!(
            (
                plain.stops.len(),
                plain
                    .stops
                    .iter()
                    .map(|stop| stop.annotations.len())
                    .sum::<usize>(),
                plain
                    .stops
                    .iter()
                    .filter(|stop| stop.title.is_some())
                    .count(),
            ),
            (3, 0, 3),
            "the plain tour is three titled stops carrying no annotations",
        );

        assert_eq!(
            load("empty").stops.len(),
            0,
            "the empty tour never becomes a run",
        );
    }
}
