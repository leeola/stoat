//! Live integration tests that drive a real headless Stoat over its event and
//! render channels against a materialized fixture.
//!
//! Gated on the `fixture` feature, so a plain `cargo test` never builds them.
//! Run with:
//!
//! ```sh
//! cargo test -p stoat --features fixture --test fixture_live
//! ```
//!
//! The LSP tests require `rust-analyzer` on PATH and fail loudly if it is
//! absent rather than skipping, since this tier is opt-in.

use serde_json::Value;
use std::{path::PathBuf, process::Command, time::Duration};
use stoat::{
    fixture::{
        self,
        harness::{Handle, LiveHarness, Query},
    },
    Settings,
};
use tempfile::TempDir;
use tokio::time::{self, Instant};

/// rust-analyzer indexing plus an initial `cargo check` on a fresh crate takes
/// several seconds, and CI machines are slower, so budget generously.
const LSP_DEADLINE: Duration = Duration::from_secs(90);

const DIFF_TIMEOUT: Duration = Duration::from_secs(10);

const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A walkthrough step opens a file and glides to it, which is slower than a
/// keystroke but nothing like an LSP round trip.
const WALKTHROUGH_TIMEOUT: Duration = Duration::from_secs(10);

#[test]
fn diff_view_shows_fixture_change() {
    let (_dir, _root, mut harness) = fixture_harness("basic-diff");
    harness.run(|mut handle| async move {
        handle
            .send_keys(":o staged.txt<Enter>")
            .await
            .expect("open staged.txt");
        handle
            .send_keys("<Space>d")
            .await
            .expect("toggle the diff view");

        handle
            .await_frame(
                |text| {
                    let shows_new = text.lines().any(|line| line.contains("4 delta changed"));
                    let shows_base = text
                        .lines()
                        .any(|line| line.contains("delta") && !line.contains("changed"));
                    shows_new && shows_base
                },
                DIFF_TIMEOUT,
            )
            .await
            .expect("diff view renders the base line beside the changed buffer line");
    });
}

#[test]
fn hover_over_symbol() {
    require_rust_analyzer();
    let (_dir, root, mut harness) = fixture_harness("rust-lsp");
    let main_path = root.join("src/main.rs");
    harness.run(|handle| async move {
        handle
            .send_keys(":o src/main.rs<Enter>")
            .await
            .expect("open src/main.rs");

        let deadline = Instant::now() + LSP_DEADLINE;
        await_lsp_active(&handle, deadline).await;

        loop {
            let hover = handle
                .query(&Query::Hover {
                    path: main_path.clone(),
                    line: 15,
                    col: 27,
                })
                .await
                .expect("hover query");
            if !hover.is_null() && hover.get("error").is_none() {
                assert!(
                    hover.to_string().contains("greet"),
                    "hover over the greet() call should describe greet, got {hover}",
                );
                return;
            }
            assert!(
                Instant::now() < deadline,
                "hover did not resolve before the deadline (last reply: {hover})",
            );
            time::sleep(POLL_INTERVAL).await;
        }
    });
}

#[test]
fn diagnostics_report_seeded_warning() {
    require_rust_analyzer();
    let (_dir, _root, mut harness) = fixture_harness("rust-lsp");
    harness.run(|handle| async move {
        handle
            .send_keys(":o src/main.rs<Enter>")
            .await
            .expect("open src/main.rs");

        let deadline = Instant::now() + LSP_DEADLINE;
        await_lsp_active(&handle, deadline).await;

        loop {
            let all = handle
                .query(&Query::Diagnostics { path: None })
                .await
                .expect("diagnostics query");
            if has_unused_warning(&all) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "seeded unused-variable diagnostic did not appear before the deadline",
            );
            time::sleep(POLL_INTERVAL).await;
        }
    });
}

/// Materialize `name` into a fresh, canonicalized temp dir and open a harness on
/// it. Canonicalizing matters because rust-analyzer canonicalizes its workspace
/// root, so the buffer path, LSP root, and reported diagnostic URIs only agree
/// when the harness root is canonical too (macOS /tmp and /var are symlinks).
fn fixture_harness(name: &str) -> (TempDir, PathBuf, LiveHarness) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let root = std::fs::canonicalize(dir.path()).expect("canonicalize tempdir");
    fixture::materialize(name, &root).expect("materialize fixture");
    let harness = LiveHarness::open(&root, Settings::default()).expect("open harness");
    (dir, root, harness)
}

/// The walkthrough fixture exists to give the player a stage, so this drives
/// the whole tour over it: every step lands on the code its stop names, and
/// none of them reports drift.
///
/// Stepping is what makes this worth running live. A stop's range is only
/// right if the file on disk still matches what the fixture captured, and the
/// player says so in the status line rather than by failing.
#[test]
fn walkthrough_fixture_plays_its_whole_tour() {
    let (_dir, _root, mut harness) = fixture_harness("walkthrough");
    harness.run(|mut handle| async move {
        handle
            .send_keys(":walkthrough tour<Enter>")
            .await
            .expect("open the tour");
        let frame = handle
            .await_frame(
                |text| text.contains("1/6: Entry point"),
                WALKTHROUGH_TIMEOUT,
            )
            .await
            .expect("the tour opens on its first stop");
        assert!(
            !frame.contains("drifted"),
            "the first stop still covers what it captured, got frame:\n{frame}",
        );

        handle
            .send_keys("<Space>W")
            .await
            .expect("enter walkthrough mode");

        // Each stop names code in a different place, so the landing text is
        // what proves the step went where the tour said rather than merely
        // advancing a counter.
        for (step, landing) in [
            (2, "pub fn load"),
            (3, "pub fn run"),
            (4, "fn dispatch"),
            (5, "pub fn handle"),
        ] {
            handle.send_keys("n").await.expect("step to the next stop");
            let frame = handle
                .await_frame(
                    |text| text.contains(&format!("{step}/6")) && text.contains(landing),
                    WALKTHROUGH_TIMEOUT,
                )
                .await
                .unwrap_or_else(|_| panic!("stop {step} lands on {landing:?}"));
            assert!(
                !frame.contains("drifted"),
                "stop {step} still covers what it captured, got frame:\n{frame}",
            );
        }

        // The fifth stop carries one annotation per match arm and a last one
        // pointing into another file, so stepping through them ends up outside
        // the stop's own buffer. Each step is awaited, since the next key press
        // is what takes the narration popup down.
        for at in 1..=5 {
            handle.send_keys("a").await.expect("step to the annotation");
            handle
                .await_frame(
                    |text| text.contains(&format!("{at}/5")),
                    WALKTHROUGH_TIMEOUT,
                )
                .await
                .unwrap_or_else(|_| panic!("annotation {at} of 5 is reached"));
        }
        // Read off the status line rather than the buffer. The mode's key-hint
        // overlay covers most of the width, so the annotated line itself is
        // truncated on screen while the status still names both the file the
        // jump opened and which annotation it landed on.
        let frame = handle
            .await_frame(
                |text| text.contains("src/server.rs") && text.contains("a11 5/5"),
                WALKTHROUGH_TIMEOUT,
            )
            .await
            .expect("the last annotation opens server.rs, the file it names");
        assert!(
            !frame.contains("drifted"),
            "the cross-file annotation still covers what it captured, got frame:\n{frame}",
        );

        handle.send_keys("d").await.expect("end the tour");
    });
}

/// Drift is only ever reported against what is on screen, so the status line
/// the reader sees is the thing worth pinning here. This walks the drifted
/// fixture's tour and checks each stop reports what the edits did to it: two
/// that no longer read what they captured, one the edits never touched, and one
/// clean stop with a drifted annotation under it.
///
/// The `walkthrough check` findings are pinned in the fixture's own tests. This
/// is the other half, since a range listed there still has to say so when the
/// reader steps onto it.
#[test]
fn walkthrough_drift_fixture_reports_drift_per_stop() {
    let (_dir, _root, mut harness) = fixture_harness("walkthrough-drift");
    harness.run(|mut handle| async move {
        handle
            .send_keys(":walkthrough tour<Enter>")
            .await
            .expect("open the tour");

        // The unstaged edits give the status line a repo segment, which cuts
        // every message here short. Each predicate matches the part that
        // survives rather than the whole of what is set.
        handle
            .await_frame(|text| text.contains("stop s1 drifted"), WALKTHROUGH_TIMEOUT)
            .await
            .expect("the first stop reports the line main.rs gained");

        handle
            .send_keys("<Space>W")
            .await
            .expect("enter walkthrough mode");

        handle
            .send_keys("n")
            .await
            .expect("step to the second stop");
        handle
            .await_frame(|text| text.contains("stop s2 drifted"), WALKTHROUGH_TIMEOUT)
            .await
            .expect("the second stop reports the block config.rs no longer holds");

        handle.send_keys("n").await.expect("step to the third stop");
        let frame = handle
            .await_frame(
                |text| text.contains("src/server.rs") && text.contains("3/6"),
                WALKTHROUGH_TIMEOUT,
            )
            .await
            .expect("the third stop lands on the file no edit touched");
        assert!(
            !frame.contains("drifted"),
            "server.rs is untouched, so its stop reports its title, got frame:\n{frame}",
        );

        for _ in 0..2 {
            handle
                .send_keys("n")
                .await
                .expect("step toward the handler");
        }
        handle
            .await_frame(
                |text| text.contains("src/handler.rs") && text.contains("5/6"),
                WALKTHROUGH_TIMEOUT,
            )
            .await
            .expect("the rename leaves the stop over the whole function clean");

        // The rename is the same length as the name it replaced, so only the
        // annotation naming it moves. The two arms before it reporting normally
        // is what makes the third one mean anything.
        for (at, id) in [(1, "a7"), (2, "a8")] {
            handle.send_keys("a").await.expect("step to the annotation");
            let frame = handle
                .await_frame(
                    |text| text.contains(&format!("{id} {at}/5")),
                    WALKTHROUGH_TIMEOUT,
                )
                .await
                .unwrap_or_else(|_| panic!("annotation {at} of 5 reports its label"));
            assert!(
                !frame.contains("drifted"),
                "the arms above the renamed one still cover what they captured, \
                 got frame:\n{frame}",
            );
        }

        handle
            .send_keys("a")
            .await
            .expect("step to the renamed arm");
        handle
            .await_frame(|text| text.contains("a9 drifted"), WALKTHROUGH_TIMEOUT)
            .await
            .expect("the annotation over the renamed arm reports drift");

        handle.send_keys("d").await.expect("end the tour");
    });
}

/// The conflict view's three columns come from the on-disk index stages, which
/// only a real repository mid-merge produces. This drives the whole chain --
/// stage read, merge alignment, and paint -- against one, so a regression
/// anywhere along it shows up as an empty flanking column.
#[test]
fn conflict_view_shows_side_columns_in_a_real_merge() {
    let (_dir, _root, mut harness) = fixture_harness("conflict");
    harness.run(|mut handle| async move {
        handle
            .send_keys(":conflict<Enter>")
            .await
            .expect("open the conflict view");
        let frame = handle
            .await_frame(|text| text.contains("<<<<<<< ours"), Duration::from_secs(5))
            .await
            .expect("the conflict view renders its marker band");

        // Read each column off the rules. A whole-frame check would pass on the
        // center alone, which is exactly the failure this pins.
        let band = frame
            .lines()
            .find(|line| line.contains("<<<<<<< ours"))
            .expect("the marker band row");
        let mut columns = band.split('│');
        let ours = columns.next().unwrap_or_default().trim();
        let theirs = columns.nth(1).unwrap_or_default().trim();

        assert_eq!(
            (ours, theirs),
            ("2 ours edit", "2 theirs edit"),
            "both flanking columns carry their stage content beside the marker \
             band, got frame:\n{frame}",
        );
    });
}

fn require_rust_analyzer() {
    let available = Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .is_ok();
    assert!(
        available,
        "rust-analyzer must be on PATH for the fixture_live LSP tests. This \
         opt-in tier fails loudly rather than skipping when the tool is missing.",
    );
}

async fn await_lsp_active(handle: &Handle, deadline: Instant) {
    loop {
        let status = handle
            .query(&Query::LspStatus)
            .await
            .expect("lsp-status query");
        if status.get("active").and_then(Value::as_bool) == Some(true) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "rust-analyzer did not become active before the deadline",
        );
        time::sleep(POLL_INTERVAL).await;
    }
}

fn has_unused_warning(diagnostics: &Value) -> bool {
    let Some(entries) = diagnostics.as_array() else {
        return false;
    };
    entries.iter().any(|entry| {
        let is_main = entry
            .get("path")
            .and_then(Value::as_str)
            .is_some_and(|path| path.ends_with("main.rs"));
        let has_unused = entry
            .get("diagnostics")
            .and_then(Value::as_array)
            .is_some_and(|diags| {
                diags.iter().any(|d| {
                    d.get("message")
                        .and_then(Value::as_str)
                        .is_some_and(|message| message.contains("unused variable"))
                })
            });
        is_main && has_unused
    })
}
