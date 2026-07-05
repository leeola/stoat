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

const REVIEW_TIMEOUT: Duration = Duration::from_secs(10);

const POLL_INTERVAL: Duration = Duration::from_millis(250);

#[test]
fn review_shows_fixture_diff() {
    let (_dir, _root, mut harness) = fixture_harness("basic-diff");
    harness.run(|mut handle| async move {
        handle.send_keys("R").await.expect("open review");
        handle
            .await_frame(|text| text.contains("delta changed"), REVIEW_TIMEOUT)
            .await
            .expect("review renders the fixture's changed line");
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
