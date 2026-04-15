//! Fixture-driven regression test for stream-JSON wire parsing.
//!
//! Walks every `*.jsonl` file under `tests/fixtures/`, deserializes each
//! line as an [`SdkMessage`], and asserts:
//!
//! - Every line parses successfully (schema-loose deserialization must never fail outright;
//!   failures are a regression).
//! - Zero [`MessageContent::Unknown`] or [`UserContentBlock`] fallthroughs occur. `Unknown` is the
//!   "please teach me about this wire shape" sentinel, so any hit means the wrapper has a missing
//!   variant.
//!
//! # Populating fixtures
//!
//! The fixtures directory starts empty. When discovery surfaces a wire
//! shape the wrapper does not yet model:
//!
//! 1. Grab the offending line from `~/.local/share/stoat/logs/rx-<pid>.jsonl`.
//! 2. Copy it (or a handful of related lines) into a new file under `tests/fixtures/` with a
//!    descriptive name like `thinking_block.jsonl` or `server_tool_use.jsonl`.
//! 3. Run this test. It will fail with the `Unknown` count until [`MessageContent`] /
//!    [`UserContentBlock`] are extended to cover the new shape.
//! 4. Extend the wire model, re-run, watch the fixture go green.
//!
//! This keeps the fixture set grounded in real wire data without
//! requiring a separate capture script.

use std::{
    fs::{self, File},
    io::{BufRead, BufReader},
    path::PathBuf,
};
use stoat_agent_claude_code::{MessageContent, SdkMessage, UserContent, UserContentBlock};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
}

#[derive(Debug, Default)]
struct UnknownCounts {
    message_content: usize,
    stream_event: usize,
}

impl UnknownCounts {
    fn total(&self) -> usize {
        self.message_content + self.stream_event
    }
}

fn inspect_sdk_message(msg: &SdkMessage, counts: &mut UnknownCounts) {
    match msg {
        SdkMessage::Assistant { message, .. } => {
            for block in &message.content {
                if matches!(block, MessageContent::Unknown(_)) {
                    counts.message_content += 1;
                }
            }
        },
        SdkMessage::User { message, .. } => {
            if let UserContent::Blocks(blocks) = &message.content {
                for block in blocks {
                    match block {
                        UserContentBlock::Text { .. } | UserContentBlock::ToolResult { .. } => {},
                    }
                }
            }
        },
        SdkMessage::System { .. }
        | SdkMessage::Result { .. }
        | SdkMessage::StreamEvent { .. }
        | SdkMessage::ControlRequest { .. }
        | SdkMessage::ControlResponse { .. } => {},
    }
}

#[test]
fn every_fixture_line_parses_with_no_unknowns() {
    let dir = fixtures_dir();
    if !dir.exists() {
        return;
    }

    let entries: Vec<_> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("read fixtures dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("jsonl"))
        })
        .collect();

    if entries.is_empty() {
        return;
    }

    let mut total_lines = 0usize;
    let mut parse_failures: Vec<(PathBuf, usize, String)> = Vec::new();
    let mut unknown_failures: Vec<(PathBuf, usize, UnknownCounts)> = Vec::new();

    for entry in entries {
        let path = entry.path();
        let file = File::open(&path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
        for (idx, line) in BufReader::new(file).lines().enumerate() {
            let line = line.unwrap_or_else(|e| panic!("read {}:{idx}: {e}", path.display()));
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            total_lines += 1;

            let msg = match serde_json::from_str::<SdkMessage>(trimmed) {
                Ok(m) => m,
                Err(e) => {
                    parse_failures.push((path.clone(), idx + 1, e.to_string()));
                    continue;
                },
            };

            let mut counts = UnknownCounts::default();
            inspect_sdk_message(&msg, &mut counts);
            if counts.total() > 0 {
                unknown_failures.push((path.clone(), idx + 1, counts));
            }
        }
    }

    if !parse_failures.is_empty() {
        let report: String = parse_failures
            .iter()
            .map(|(p, line, err)| format!("  {}:{line}: {err}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!("fixture parse failures ({total_lines} lines scanned):\n{report}");
    }

    if !unknown_failures.is_empty() {
        let report: String = unknown_failures
            .iter()
            .map(|(p, line, c)| format!("  {}:{line}: {c:?}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        panic!(
            "fixture lines produced Unknown fallthroughs ({total_lines} lines scanned):\n\
             extend MessageContent / UserContentBlock to cover these shapes.\n{report}"
        );
    }
}
