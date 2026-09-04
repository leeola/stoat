//! The `walkthrough-drift` fixture: the `walkthrough` crate committed with its
//! tour, then edited underneath it in the working tree.
//!
//! Every other fixture in the family commits sources its tour still matches, so
//! nothing stages the two places drift is reported. The player replaces the
//! status line with `<id> drifted from its capture` as the reader steps onto a
//! range that moved, and `stoat walkthrough check` classifies the same ranges
//! as stale or error.
//!
//! The edits are chosen so the tour reports all three outcomes at once. One
//! file shifts by a line, so its ranges still fit and read something else. One
//! is renamed in place at equal length, so only the range over the name moves.
//! One is cut short, so the ranges past its end no longer fit at all. One is
//! left alone, so two stops stay clean and the report is a mix rather than a
//! wall.
//!
//! Because the edits are unstaged, the diff gutter marks them too, and the
//! reader sees the drifted code and the change that caused it together.

use crate::fixture::{walkthrough::tour, FixtureError};
use std::path::Path;

/// `src/config.rs` cut down to the struct, its default, and a `load` that reads
/// nothing.
///
/// Twenty-two lines, against a tour whose `load` block runs to line 31 and
/// whose three annotations sit at lines 21, 25, and 28. Every one of those is
/// past what the stub holds or past the end of the line it lands on, which is
/// what makes them errors rather than merely stale.
///
/// `main` only ever calls `config::load`, so the crate still builds.
const CONFIG_STUB: &str = r#"use std::path::Path;

/// Everything the server reads before it binds.
pub struct Config {
    pub addr: String,
    pub workers: usize,
    pub verbose: bool,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            addr: "127.0.0.1:8080".to_string(),
            workers: 4,
            verbose: false,
        }
    }
}

pub fn load(_path: &Path) -> Config {
    Config::default()
}
"#;

/// Build the fixture repository at `dest`.
pub(in crate::fixture) fn materialize(dest: &Path) -> Result<(), FixtureError> {
    let mut repo = tour::commit(dest)?;

    // One line at the top, so every range below it reads the line before the
    // one it captured.
    repo.unstaged_file(
        "src/main.rs",
        &format!("//! A fixture server.\n{}", tour::MAIN),
    )?;

    // `remove` and `delete` are the same length, so no range moves. Only the
    // annotation whose bytes name the function reads anything different.
    repo.unstaged_file("src/handler.rs", &tour::HANDLER.replace("remove", "delete"))?;

    repo.unstaged_file("src/config.rs", CONFIG_STUB)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::{
        host::LocalFs,
        walkthrough::{self, store, FindingKind},
    };

    /// Every finding the edited tree produces, as `(stop, annotation, kind)` in
    /// the order `walkthrough::validate` reports them.
    ///
    /// The mix is the point. A stop whose focus no longer fits sits beside one
    /// whose focus reads the wrong line, both sit beside two that are clean,
    /// and one stop is clean while a single annotation under it is not.
    const EXPECTED: [(&str, Option<&str>, FindingKind); 9] = [
        ("s1", None, FindingKind::Error),
        ("s1", Some("a1"), FindingKind::Stale),
        ("s1", Some("a2"), FindingKind::Stale),
        ("s2", None, FindingKind::Error),
        ("s2", Some("a3"), FindingKind::Error),
        ("s2", Some("a4"), FindingKind::Error),
        ("s2", Some("a5"), FindingKind::Error),
        ("s5", Some("a9"), FindingKind::Stale),
        ("s6", None, FindingKind::Stale),
    ];

    /// The fixture exists to produce drift, so what it produces is the whole of
    /// what it stages. An edit that shifts a range into fitting again, or that
    /// breaks one that was meant to hold, changes what the reader is shown
    /// without changing anything a compile catches.
    #[test]
    fn drift_tour_reports_stale_and_error_ranges() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();

        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");
        let findings = walkthrough::validate(&tour, &store::workspace_reader(&LocalFs, dir.path()));

        assert_eq!(
            findings
                .iter()
                .map(|finding| (
                    finding.stop.as_str(),
                    finding.annotation.as_deref(),
                    finding.kind
                ))
                .collect::<Vec<_>>(),
            EXPECTED.to_vec(),
            "the edits drift exactly the ranges they were chosen to drift",
        );
    }

    /// The DELETE arm is renamed at equal length, so the stop over the whole
    /// function still covers what it captured while the annotation inside it
    /// does not. A stop-level check alone misses that pairing.
    #[test]
    fn drift_leaves_the_handler_stop_clean_under_a_drifted_annotation() {
        let dir = tempfile::tempdir().unwrap();
        super::materialize(dir.path()).unwrap();

        let tour = store::load(&LocalFs, dir.path(), "tour").expect("the tour is committed");
        let findings = walkthrough::validate(&tour, &store::workspace_reader(&LocalFs, dir.path()));

        assert_eq!(
            findings
                .iter()
                .filter(|finding| finding.stop == "s5")
                .map(|finding| (finding.annotation.as_deref(), finding.kind))
                .collect::<Vec<_>>(),
            vec![(Some("a9"), FindingKind::Stale)],
            "only the range over the renamed arm moves, and the stop holds",
        );
    }
}
