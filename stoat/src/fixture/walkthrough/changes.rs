//! The `walkthrough-changes` fixture, the `walkthrough` crate committed and then
//! edited in the working tree, with a tour over the edits.
//!
//! A tour over uncommitted changes is how an agent walks a reader through the
//! edits it just made, so its stops ask for the diff view. Two diff stops land
//! on the two edits, and a last plain stop lands on a file the edits leave
//! alone, which shows plain.
//!
//! The tour is committed with the sources, but its ranges read the edited
//! working tree, since that is the side of a change a diff stop names.

use crate::{
    fixture::{walkthrough::tour, FixtureError, FixtureRepo},
    walkthrough::{StopEdit, Walkthrough},
};
use std::path::Path;

const NARRATION_WORKERS: &str = "\
The default pool doubles from four workers to eight. The diff view shows the
old line beside the new one, so the change reads at a glance.
";

const NARRATION_DELETE: &str = "\
`remove` becomes `delete`, so the arm names the method it answers.
";

const NARRATION_MAIN: &str = "\
The edits leave `main` alone, so this stop shows plain under the diff view.
";

/// Build the fixture repository at `dest`.
pub(in crate::fixture) fn materialize(dest: &Path) -> Result<(), FixtureError> {
    let json = super::tour_json(&build());

    let mut repo = FixtureRepo::init(dest)?;
    repo.commit(
        "initial commit",
        &[
            ("Cargo.toml", tour::CARGO),
            ("src/main.rs", tour::MAIN),
            ("src/config.rs", tour::CONFIG),
            ("src/server.rs", tour::SERVER),
            ("src/handler.rs", tour::HANDLER),
            (".stoat/walkthroughs/tour.json", &json),
        ],
    )?;

    repo.unstaged_file("src/config.rs", &edited_config())?;
    repo.unstaged_file("src/handler.rs", &edited_handler())?;
    Ok(())
}

/// The three-stop tour over the edited working tree.
///
/// Every range is derived from the edited text, so each stop's capture matches
/// what the working tree holds.
pub(in crate::fixture) fn build() -> Walkthrough {
    let mut walkthrough = Walkthrough::new(
        "tour".to_string(),
        "What the working tree changes".to_string(),
        None,
    );
    walkthrough.description = "Two edits sit uncommitted. The default worker pool doubles, \
                               and the handler's `remove` becomes `delete`."
        .to_string();

    let (config, handler) = (edited_config(), edited_handler());
    for (title, narration, path, content, needle, diff) in [
        (
            "A bigger pool",
            NARRATION_WORKERS,
            "src/config.rs",
            config.as_str(),
            "workers: 8",
            true,
        ),
        (
            "Delete by name",
            NARRATION_DELETE,
            "src/handler.rs",
            handler.as_str(),
            "\"DELETE\" => delete(request)",
            true,
        ),
        (
            "Untouched",
            NARRATION_MAIN,
            "src/main.rs",
            tour::MAIN,
            "fn main() {",
            false,
        ),
    ] {
        let id = walkthrough
            .add_stop(
                Some(title.to_string()),
                narration.to_string(),
                super::location(path, content, super::line_of(content, needle)),
                None,
                None,
            )
            .expect("appending a stop cannot fail")
            .id
            .clone();
        if diff {
            let edit = StopEdit {
                diff: Some(true),
                ..StopEdit::default()
            };
            walkthrough
                .edit_stop(&id, edit)
                .expect("the stop was just added");
        }
    }

    walkthrough
}

/// `src/config.rs` with the default pool doubled.
fn edited_config() -> String {
    tour::CONFIG.replace("workers: 4", "workers: 8")
}

/// `src/handler.rs` with `remove` renamed to `delete`, which is the same
/// length, so only the renamed lines change.
fn edited_handler() -> String {
    tour::HANDLER.replace("remove", "delete")
}

#[cfg(test)]
mod tests {
    use crate::{
        host::LocalFs,
        walkthrough::{self, store},
    };

    /// The ranges name the edited lines, so they hold against the working tree
    /// the fixture leaves rather than against its commit.
    #[test]
    fn changes_tour_validates_against_the_edited_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        super::materialize(dir.path()).expect("materialize");

        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");
        let findings = walkthrough::validate(&tour, &store::reader(&LocalFs, dir.path()));
        let diff_stops: Vec<bool> = tour.stops.iter().map(|stop| stop.diff).collect();

        assert_eq!(
            (findings, diff_stops),
            (Vec::new(), vec![true, true, false]),
            "every range reads the edited tree, and the two edits are diff stops",
        );
    }
}
