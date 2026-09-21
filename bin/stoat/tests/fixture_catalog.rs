//! Every fixture in the shared catalog materializes.
//!
//! Gated behind the `fixture` feature so a plain build never compiles it. A
//! catalog entry in `stoat_cli::FIXTURES` with no matching
//! `stoat::fixture::materialize` arm fails this test, keeping the two in sync.
#![cfg(feature = "fixture")]

use crossterm::event::KeyCode;
use stoat::{
    fixture,
    host::LocalFs,
    input_parse::{self, InputStep},
    walkthrough::store,
};

#[test]
fn every_catalog_fixture_materializes() {
    for (name, _) in stoat_cli::FIXTURES {
        let dir = tempfile::tempdir().expect("create tempdir");
        fixture::materialize(name, dir.path())
            .unwrap_or_else(|err| panic!("fixture `{name}` failed to materialize: {err}"));
    }
}

/// A walkthrough fixture is opened to be watched, so its default inputs must
/// reach every stop and every annotation its tour holds.
///
/// Deriving the script from the tour is what keeps that true as a fixture
/// gains stops, so this checks the derivation against the tour each fixture
/// actually commits rather than against a copy of it.
#[test]
fn every_walkthrough_fixture_steps_its_whole_tour() {
    for (name, _) in stoat_cli::FIXTURES {
        let script = fixture::default_inputs(name);
        assert_eq!(
            script.is_some(),
            name.starts_with("walkthrough"),
            "`{name}` drives itself",
        );

        let Some(script) = script else { continue };

        assert!(
            script.starts_with(":walkthrough tour<Enter><Space>W"),
            "`{name}` opens its tour and enters walkthrough mode, got {script:?}",
        );

        let dir = tempfile::tempdir().expect("create tempdir");
        fixture::materialize(name, dir.path())
            .unwrap_or_else(|err| panic!("fixture `{name}` failed to materialize: {err}"));
        let tour = store::load(&LocalFs, dir.path(), "tour")
            .unwrap_or_else(|err| panic!("fixture `{name}` has no tour: {err}"));

        let steps = input_parse::parse_input_sequence(&script)
            .unwrap_or_else(|err| panic!("fixture `{name}` script does not parse: {err}"));
        let annotations: usize = tour.stops.iter().map(|stop| stop.annotations.len()).sum();

        assert_eq!(
            (dwelled(&steps, 'n'), dwelled(&steps, 'a')),
            (tour.stops.len() - 1, annotations),
            "`{name}` steps every stop and annotation",
        );
    }
}

/// How many `wanted` keys the script sends directly after a pause.
///
/// The opening command types `:walkthrough tour`, and its own letters are keys
/// too. Only a stepping key follows a dwell, so that pairing separates the two.
fn dwelled(steps: &[InputStep], wanted: char) -> usize {
    steps
        .windows(2)
        .filter(|pair| {
            matches!(pair[0], InputStep::Wait(_))
                && matches!(pair[1], InputStep::Key(key) if key.code == KeyCode::Char(wanted))
        })
        .count()
}
