//! The other occurrences of the symbol under the cursor, lit up as the cursor
//! rests on it.
//!
//! A server resolves the symbol properly, so it finds the occurrences a text
//! search misses and skips the ones that only look alike. The read and write
//! occurrences paint as separate layers, which is what makes an assignment
//! stand out from a mention.
//!
//! No key reaches this. A trigger fires from the post-event fan-out, and a pump
//! collects the reply, both of them background work the user never asks for.

use crate::{
    action_handlers,
    app::Stoat,
    buffer::BufferId,
    display_map::{syntax_theme, HighlightKey, HighlightLayer, HighlightStyle},
    host::{LanguageServerFeature, OffsetEncoding},
    lsp::util,
    theme::scope,
};
use lsp_types::{
    DocumentHighlight, DocumentHighlightKind, DocumentHighlightParams, TextDocumentIdentifier,
    TextDocumentPositionParams,
};
use std::{path::Path, time::Duration};
use stoat_text::{Anchor, Bias, Rope};

/// Debounce before requesting document highlights, so the symbol under the
/// cursor lights up only once cursor motion settles.
const DOCUMENT_HIGHLIGHT_DEBOUNCE: Duration = Duration::from_millis(200);

/// A completed document-highlight request's payload. It carries the buffer the
/// request targeted and each occurrence as a byte-offset range paired with
/// whether the server marked it a write.
pub(crate) type DocumentHighlightResponse = (BufferId, Vec<(std::ops::Range<usize>, bool)>);

/// The focused editor's `(buffer_id, version, cursor_offset)` document-highlight
/// dedupe key, or `None` when no editor is focused.
///
/// Mirrors [`build_document_highlight_request`]'s editor read without cloning
/// the rope, so the trigger can bail before host resolution.
fn document_highlight_key(stoat: &mut Stoat) -> Option<(BufferId, u64, usize)> {
    let editor = action_handlers::focused_editor_mut(stoat)?;
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    let tail_off = buf_snap.resolve_anchor(&sel.tail());
    let head_off = buf_snap.resolve_anchor(&sel.head());
    let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
    Some((editor.buffer_id, buf_snap.version(), offset))
}

/// Highlight the occurrences of the symbol under the focused editor's cursor when
/// the server supports it and the cursor rests in normal mode.
///
/// Leaving normal mode, or a change to the `(buffer, version, cursor offset)`
/// key, clears the current highlights immediately and re-arms a debounced
/// request. Occurrences therefore vanish while navigating and reappear once the
/// cursor settles. [`pump_lsp_document_highlight`] applies the response.
pub(crate) fn document_highlight_trigger(stoat: &mut Stoat) {
    if stoat.focused_mode() != "normal" {
        if stoat.last_document_highlight_key.is_some() {
            clear_document_highlights(stoat);
            stoat.last_document_highlight_key = None;
            stoat.pending_document_highlight_request.clear();
        }
        return;
    }

    let Some((_, buffer_id)) = stoat.focused_editor_ids() else {
        return;
    };
    let Some(key) = document_highlight_key(stoat) else {
        return;
    };
    if stoat.last_document_highlight_key == Some(key) {
        return;
    }

    let Some((_, host)) = crate::lsp::hosts::feature_hosts(
        stoat,
        buffer_id,
        LanguageServerFeature::DocumentHighlight,
    )
    .into_iter()
    .next() else {
        return;
    };
    let encoding = host.offset_encoding();
    let Some((buffer_id, version, offset, rope, params)) =
        build_document_highlight_request(stoat, encoding)
    else {
        return;
    };

    stoat.last_document_highlight_key = Some((buffer_id, version, offset));
    clear_document_highlights(stoat);

    let executor = stoat.executor.clone();
    let task = stoat.spawn_woken(async move {
        executor.timer(DOCUMENT_HIGHLIGHT_DEBOUNCE).await;
        match host.document_highlight(params).await {
            Ok(Some(highlights)) => Some((
                buffer_id,
                convert_document_highlights(highlights, &rope, encoding),
            )),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "document_highlight request failed");
                None
            },
        }
    });
    stoat.pending_document_highlight_request.arm(task);
}

fn build_document_highlight_request(
    stoat: &mut Stoat,
    encoding: OffsetEncoding,
) -> Option<(BufferId, u64, usize, Rope, DocumentHighlightParams)> {
    let (buffer_id, version, offset, rope) = {
        let editor = action_handlers::focused_editor_mut(stoat)?;
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        (
            editor.buffer_id,
            buf_snap.version(),
            offset,
            buf_snap.rope().clone(),
        )
    };

    let path = stoat
        .active_workspace()
        .buffers
        .path_for(buffer_id)
        .map(Path::to_path_buf)?;
    let uri = action_handlers::lsp::path_to_uri(&path)?;
    let position = util::byte_offset_to_lsp_pos(&rope, offset, encoding);
    let params = DocumentHighlightParams {
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri },
            position,
        },
        work_done_progress_params: Default::default(),
        partial_result_params: Default::default(),
    };
    Some((buffer_id, version, offset, rope, params))
}

/// Convert LSP document highlights into `(byte range, is_write)` pairs using the
/// request-time rope. WRITE occurrences carry the write flag; READ, TEXT, and
/// unspecified occurrences carry the read flag.
fn convert_document_highlights(
    highlights: Vec<DocumentHighlight>,
    rope: &Rope,
    encoding: OffsetEncoding,
) -> Vec<(std::ops::Range<usize>, bool)> {
    highlights
        .into_iter()
        .map(|hl| {
            let start = util::lsp_pos_to_byte_offset(rope, hl.range.start, encoding);
            let end = util::lsp_pos_to_byte_offset(rope, hl.range.end, encoding);
            let is_write = hl.kind == Some(DocumentHighlightKind::WRITE);
            (start..end, is_write)
        })
        .collect()
}

/// Poll any in-flight document-highlight request and paint the results as read
/// and write text highlights on the focused editor. Returns true when state
/// changed.
pub(crate) fn pump_lsp_document_highlight(stoat: &mut Stoat) -> bool {
    let Some(response) = stoat.pending_document_highlight_request.poll() else {
        return false;
    };
    if let Some((buffer_id, items)) = response {
        apply_document_highlights(stoat, buffer_id, items);
    }
    true
}

fn apply_document_highlights(
    stoat: &mut Stoat,
    buffer_id: BufferId,
    items: Vec<(std::ops::Range<usize>, bool)>,
) {
    let read_style = document_highlight_style(stoat, scope::UI_HIGHLIGHT_READ);
    let write_style = document_highlight_style(stoat, scope::UI_HIGHLIGHT_WRITE);

    let Some(editor) = action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    if editor.buffer_id != buffer_id {
        return;
    }

    let (read, write) = {
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let mut read: Vec<std::ops::Range<Anchor>> = Vec::new();
        let mut write: Vec<std::ops::Range<Anchor>> = Vec::new();
        for (range, is_write) in items {
            let anchors = buf_snap.anchor_at(range.start, Bias::Right)
                ..buf_snap.anchor_at(range.end, Bias::Left);
            if is_write {
                write.push(anchors);
            } else {
                read.push(anchors);
            }
        }
        (read, write)
    };

    let read_key = HighlightKey::layer(HighlightLayer::DocumentHighlightRead);
    if read.is_empty() {
        editor.display_map.clear_highlights(read_key);
    } else {
        editor
            .display_map
            .highlight_text(read_key, read, read_style);
    }

    let write_key = HighlightKey::layer(HighlightLayer::DocumentHighlightWrite);
    if write.is_empty() {
        editor.display_map.clear_highlights(write_key);
    } else {
        editor
            .display_map
            .highlight_text(write_key, write, write_style);
    }
}

/// Remove the read and write document-highlight ranges from the focused editor.
fn clear_document_highlights(stoat: &mut Stoat) {
    let Some(editor) = action_handlers::focused_editor_mut(stoat) else {
        return;
    };
    editor
        .display_map
        .clear_highlights(HighlightKey::layer(HighlightLayer::DocumentHighlightRead));
    editor
        .display_map
        .clear_highlights(HighlightKey::layer(HighlightLayer::DocumentHighlightWrite));
}

fn document_highlight_style(stoat: &Stoat, scope_key: &str) -> HighlightStyle {
    syntax_theme::style_to_highlight_style(&stoat.theme.get(scope_key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixture::{open_buffer, seed},
        test_harness::TestHarness,
    };

    fn enable_document_highlight(h: &TestHarness) {
        use lsp_types::{OneOf, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            document_highlight_provider: Some(OneOf::Left(true)),
            ..Default::default()
        });
    }

    fn doc_highlight_count(h: &mut TestHarness, layer: HighlightLayer) -> usize {
        let editor = action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        let snapshot = editor.display_map.snapshot();
        snapshot
            .text_highlights()
            .get(&HighlightKey::layer(layer))
            .map(|hl| hl.1.len())
            .unwrap_or(0)
    }

    #[test]
    fn snapshot_document_highlight_read_write() {
        let mut h = TestHarness::with_size(24, 4);
        enable_document_highlight(&h);
        let root = seed(&mut h, &[("main.rs", "foo bar foo\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_highlights(
            path.to_str().unwrap(),
            0,
            0,
            &[
                (0, 0, 3, DocumentHighlightKind::WRITE),
                (0, 8, 11, DocumentHighlightKind::READ),
            ],
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(250));
        h.assert_snapshot("document_highlight_read_write");
    }

    #[test]
    fn document_highlight_re_requests_on_cursor_move() {
        let mut h = TestHarness::with_size(24, 4);
        enable_document_highlight(&h);
        let root = seed(&mut h, &[("main.rs", "foo bar foo\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp().set_highlights(
            p,
            0,
            0,
            &[
                (0, 0, 3, DocumentHighlightKind::READ),
                (0, 8, 11, DocumentHighlightKind::READ),
            ],
        );
        h.fake_lsp()
            .set_highlights(p, 0, 1, &[(0, 0, 3, DocumentHighlightKind::READ)]);

        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(250));
        assert_eq!(
            doc_highlight_count(&mut h, HighlightLayer::DocumentHighlightRead),
            2
        );

        h.type_keys("l");
        h.advance_clock(Duration::from_millis(250));
        assert_eq!(
            doc_highlight_count(&mut h, HighlightLayer::DocumentHighlightRead),
            1
        );
    }

    #[test]
    fn document_highlight_cleared_in_insert_mode() {
        let mut h = TestHarness::with_size(24, 4);
        enable_document_highlight(&h);
        let root = seed(&mut h, &[("main.rs", "foo bar foo\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_highlights(
            path.to_str().unwrap(),
            0,
            0,
            &[
                (0, 0, 3, DocumentHighlightKind::READ),
                (0, 8, 11, DocumentHighlightKind::READ),
            ],
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(250));
        assert_eq!(
            doc_highlight_count(&mut h, HighlightLayer::DocumentHighlightRead),
            2
        );

        h.type_keys("i");
        assert_eq!(
            doc_highlight_count(&mut h, HighlightLayer::DocumentHighlightRead),
            0
        );
    }
}
