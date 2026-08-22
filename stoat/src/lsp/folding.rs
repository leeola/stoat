//! Server-declared folding ranges, as the creases the editor folds on.
//!
//! Tree-sitter already gives the buffer foldable regions from its syntax tree.
//! A language server knows regions the grammar misses, such as an import block
//! or a region marker, so a server that advertises the capability replaces the
//! buffer's creases with what it reports.
//!
//! No key reaches this. A trigger fires from the post-event fan-out, and a pump
//! collects the reply, both of them background work the user never asks for.

use crate::{action_handlers, app::Stoat, buffer::BufferId, lsp, lsp::hosts};
use lsp_types::{FoldingRange, FoldingRangeParams, TextDocumentIdentifier};
use std::{path::Path, time::Duration};
use stoat_text::{Anchor, Bias, Point, Rope};

/// Debounce before requesting folding ranges, so a burst of edits collapses into
/// a single request once typing settles.
const FOLDING_RANGE_DEBOUNCE: Duration = Duration::from_millis(500);

/// A completed folding-range request's payload. It carries the buffer and each
/// foldable region as a `(byte range, collapsed text)` pair in request-time
/// coordinates.
pub(crate) type FoldingRangesOutcome = (BufferId, Vec<(std::ops::Range<usize>, Option<String>)>);

/// Request folding ranges for the focused editor when the server advertises the
/// capability and the `(buffer, version)` key changed.
///
/// A newly-focused buffer and each edit re-request behind a 500ms debounce.
/// [`pump_lsp_folding_ranges`] feeds the response into the display map's
/// `set_lsp_folding_ranges` hook, which replaces the buffer's foldable creases.
pub(crate) fn folding_ranges_trigger(stoat: &mut Stoat) {
    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return;
    };
    let Some(version) = lsp::focused_buffer_version(stoat) else {
        return;
    };
    if stoat.last_folding_range_key == Some((buffer_id, version)) {
        return;
    }

    let host = hosts::lsp_for(stoat, buffer_id);
    if host.capabilities().folding_range_provider.is_none() {
        return;
    }

    let Some((buffer_id, version, rope, params)) = build_folding_range_request(stoat) else {
        return;
    };

    let key = (buffer_id, version);
    stoat.last_folding_range_key = Some(key);

    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        executor.timer(FOLDING_RANGE_DEBOUNCE).await;
        match host.folding_range(params).await {
            Ok(Some(ranges)) => Some((buffer_id, convert_folding_ranges(ranges, &rope))),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "folding_range request failed");
                None
            },
        }
    });
    stoat.pending_folding_ranges.arm(task);
}

fn build_folding_range_request(
    stoat: &mut Stoat,
) -> Option<(BufferId, u64, Rope, FoldingRangeParams)> {
    let (buffer_id, version, rope) = {
        let editor = action_handlers::focused_editor_mut(stoat)?;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        (
            editor.buffer_id,
            buf_snap.version(),
            buf_snap.rope().clone(),
        )
    };

    let path = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)?;
    let uri = action_handlers::lsp::path_to_uri(&path)?;
    let params = FoldingRangeParams {
        text_document: TextDocumentIdentifier { uri },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    Some((buffer_id, version, rope, params))
}

/// Convert LSP folding ranges into `(byte range, collapsed text)` foldable spans
/// using the request-time rope.
///
/// Each span runs from the end of the start line to the end of the end line, so a
/// fold keeps the header line visible and collapses the body. Degenerate spans
/// (start at or after end) are dropped.
fn convert_folding_ranges(
    ranges: Vec<FoldingRange>,
    rope: &Rope,
) -> Vec<(std::ops::Range<usize>, Option<String>)> {
    let line_end_offset = |line: u32| rope.point_to_offset(Point::new(line, rope.line_len(line)));
    ranges
        .into_iter()
        .filter_map(|fr| {
            let start = line_end_offset(fr.start_line);
            let end = line_end_offset(fr.end_line);
            (start < end).then_some((start..end, fr.collapsed_text))
        })
        .collect()
}

/// Poll any in-flight folding-range request and install the results as foldable
/// creases on the focused editor. Returns true when state changed.
pub(crate) fn pump_lsp_folding_ranges(stoat: &mut Stoat) -> bool {
    let Some(outcome) = stoat.pending_folding_ranges.poll() else {
        return false;
    };
    if let Some((buffer_id, items)) = outcome {
        apply_folding_ranges(stoat, buffer_id, items);
    }
    true
}

fn apply_folding_ranges(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    items: Vec<(std::ops::Range<usize>, Option<String>)>,
) {
    let Some(editor) = action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    if editor.buffer_id != buffer_id {
        return;
    }

    let anchored: Vec<(std::ops::Range<Anchor>, Option<String>)> = {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        items
            .into_iter()
            .map(|(range, text)| {
                (
                    buf_snap.anchor_at(range.start, Bias::Right)
                        ..buf_snap.anchor_at(range.end, Bias::Left),
                    text,
                )
            })
            .collect()
    };
    editor
        .display_map
        .set_lsp_folding_ranges(buffer_id, anchored);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixture::{open_buffer, seed},
        test_harness::TestHarness,
    };

    fn enable_folding_range(h: &TestHarness) {
        use lsp_types::{FoldingRangeProviderCapability, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            folding_range_provider: Some(FoldingRangeProviderCapability::Simple(true)),
            ..Default::default()
        });
    }

    fn crease_point_ranges(h: &mut TestHarness) -> Vec<std::ops::Range<Point>> {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let resolve = |a: &Anchor| buf_snap.rope().offset_to_point(buf_snap.resolve_anchor(a));
        snapshot
            .crease_snapshot()
            .crease_items_with_offsets(&resolve)
            .into_iter()
            .map(|(_, range)| range)
            .collect()
    }

    #[test]
    fn folding_ranges_land_as_creases() {
        let mut h = TestHarness::with_size(40, 10);
        enable_folding_range(&h);
        let root = seed(&mut h, &[("main.rs", "fn a() {\n    x;\n}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_folding_ranges(
            path.to_str().unwrap(),
            vec![FoldingRange {
                start_line: 0,
                start_character: None,
                end_line: 2,
                end_character: None,
                kind: None,
                collapsed_text: None,
            }],
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(550));
        assert_eq!(
            crease_point_ranges(&mut h),
            vec![Point::new(0, 8)..Point::new(2, 1)]
        );
    }
}
