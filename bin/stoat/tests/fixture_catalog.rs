//! Every fixture in the shared catalog materializes.
//!
//! Gated behind the `fixture` feature so a plain build never compiles it. A
//! catalog entry in `stoat_cli::FIXTURES` with no matching
//! `stoat::fixture::materialize` arm fails this test, keeping the two in sync.
#![cfg(feature = "fixture")]

#[test]
fn every_catalog_fixture_materializes() {
    for (name, _) in stoat_cli::FIXTURES {
        let dir = tempfile::tempdir().expect("create tempdir");
        stoat::fixture::materialize(name, dir.path())
            .unwrap_or_else(|err| panic!("fixture `{name}` failed to materialize: {err}"));
    }
}
