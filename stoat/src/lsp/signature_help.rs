//! The signature of the call being typed, shown while the arguments go in.
//!
//! A server names the parameters and marks which one the cursor sits on, so the
//! popup follows the commas across without the typist looking the call up. It is
//! bound to insert mode and to the trigger characters a server declares, and it
//! yields to the completion popup so the two never cover each other.
//!
//! No key reaches this. A trigger fires from the post-event fan-out, and a pump
//! collects the reply, both of them background work the user never asks for.

use crate::{
    action_handlers, app::Stoat, buffer::BufferId, completion, host::LanguageServerFeature,
    lsp::util, pane::View,
};
use futures::task::noop_waker;
use lsp_types::{
    Documentation, ParameterLabel, SignatureHelp, SignatureHelpParams, SignatureInformation,
    TextDocumentIdentifier, TextDocumentPositionParams,
};
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    task::{Context, Poll},
};
use stoat_text::Rope;

/// Signature-help popup state ready to paint.
///
/// `label` is the active signature's text. `active_param` is the char range
/// within `label` the renderer emphasizes, present when the server reports an
/// active parameter. `doc` is the signature's first documentation line, if any.
/// `anchor_offset` is the cursor byte offset when the request fired.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SignatureHelpPopup {
    pub(crate) label: String,
    pub(crate) active_param: Option<std::ops::Range<usize>>,
    pub(crate) doc: Option<String>,
    pub(crate) anchor_offset: usize,
}

/// Re-fire the signature-help request when a trigger character was just typed,
/// or a retrigger character while the popup is showing. Clears the popup when
/// the editor leaves insert mode or the completion popup takes over, so the two
/// never overlap.
///
/// Version-gated on the focused buffer so a cursor-only tick does not re-request.
pub(crate) fn signature_help_trigger(stoat: &mut Stoat) {
    let in_insert_editor = stoat.focused_mode() == "insert" && {
        let ws = stoat.active_workspace();
        matches!(ws.panes.pane(ws.panes.focus()).view, View::Editor(_))
    };
    if !in_insert_editor || stoat.pending_completion.is_some() {
        stoat.pending_signature_help = None;
        stoat.pending_signature_help_request = None;
        stoat.last_signature_help_key = None;
        return;
    }

    let Some((buffer_id, version, rope, cursor_offset)) = focused_edit_snapshot(stoat) else {
        return;
    };
    if stoat.last_signature_help_key == Some((buffer_id, version)) {
        return;
    }
    stoat.last_signature_help_key = Some((buffer_id, version));

    // The completion trigger ran on this same event and asked the same
    // question, so this reads its answer rather than walking the rope again.
    let context = completion::request::cached_context(stoat, buffer_id, version, cursor_offset)
        .unwrap_or_else(|| completion::request::compute_context(&rope, cursor_offset));
    let Some(ch) = context.text_before_cursor.chars().last() else {
        return;
    };
    let ch = ch.to_string();

    let Some((_, host)) =
        crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::SignatureHelp)
            .into_iter()
            .next()
    else {
        return;
    };
    let caps = host.capabilities();
    let Some(opts) = caps.signature_help_provider.as_ref() else {
        return;
    };
    let is_trigger = opts
        .trigger_characters
        .as_ref()
        .is_some_and(|chars| chars.contains(&ch));
    let is_retrigger = stoat.pending_signature_help.is_some()
        && opts
            .retrigger_characters
            .as_ref()
            .is_some_and(|chars| chars.contains(&ch));

    if is_trigger || is_retrigger {
        request_signature_help(stoat);
    }
}

/// The focused editor's `(buffer_id, version, rope, cursor_offset)`, or `None`
/// when the focused pane is not an editor.
fn focused_edit_snapshot(stoat: &mut Stoat) -> Option<(BufferId, u64, Rope, usize)> {
    let editor = action_handlers::focused_editor_mut(stoat)?;
    let snapshot = editor.display_map.snapshot();
    let buf_snap = snapshot.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    let tail_off = buf_snap.resolve_anchor(&sel.tail());
    let head_off = buf_snap.resolve_anchor(&sel.head());
    let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
    Some((
        editor.buffer_id,
        buf_snap.version(),
        buf_snap.rope().clone(),
        offset,
    ))
}

/// Issue a `textDocument/signatureHelp` request for the focused editor's primary
/// cursor. The async response is stored on
/// [`Stoat::pending_signature_help_request`] and applied by
/// [`pump_lsp_signature_help`]. No-op when the pane is not an editor, the buffer
/// has no path, or the server does not advertise the capability.
fn request_signature_help(stoat: &mut Stoat) {
    let (anchor_offset, buffer_id, focused_rope, is_review) = {
        let Some(editor) = action_handlers::focused_editor_mut(stoat) else {
            return;
        };
        let snapshot = editor.display_map.snapshot();
        let buf_snap = snapshot.buffer_snapshot();
        let sel = editor.selections.newest_anchor();
        let tail_off = buf_snap.resolve_anchor(&sel.tail());
        let head_off = buf_snap.resolve_anchor(&sel.head());
        let offset = stoat_text::cursor_offset(buf_snap.rope(), tail_off, head_off);
        (
            offset,
            editor.buffer_id,
            buf_snap.rope().clone(),
            editor.review_view.is_some(),
        )
    };

    let Some((_, host)) =
        crate::lsp::hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::SignatureHelp)
            .into_iter()
            .next()
    else {
        return;
    };
    let encoding = host.offset_encoding();

    let (source_path, source_rope, cursor_offset) = if is_review {
        match action_handlers::lsp::review_lsp_source(stoat) {
            Some(resolved) => resolved,
            None => return,
        }
    } else {
        let Some(path) = stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(Path::to_path_buf)
        else {
            return;
        };
        (path, focused_rope, anchor_offset)
    };
    let Some(source_uri) = action_handlers::lsp::path_to_uri(&source_path) else {
        return;
    };

    let position = util::byte_offset_to_lsp_pos(&source_rope, cursor_offset, encoding);
    let params = SignatureHelpParams {
        context: None,
        text_document_position_params: TextDocumentPositionParams {
            text_document: TextDocumentIdentifier { uri: source_uri },
            position,
        },
        work_done_progress_params: Default::default(),
    };

    // The position was measured after edits whose change may still be sitting in
    // its debounce, and a server cannot place a position in text it has not been
    // sent.
    let pending_change = action_handlers::lsp::flush_pending_did_change(stoat, buffer_id);
    let task = stoat.spawn_woken(async move {
        if let Some(pending_change) = pending_change {
            pending_change.await;
        }
        match host.signature_help(params).await {
            Ok(Some(help)) => signature_help_to_popup(help, anchor_offset),
            Ok(None) => None,
            Err(err) => {
                tracing::warn!(target: "stoat::lsp", ?err, "signature_help request failed");
                None
            },
        }
    });
    stoat.pending_signature_help_request = Some(task);
}

/// Reduce an LSP [`SignatureHelp`] to the active signature's paintable state:
/// its label, the char range of the active parameter within that label, and the
/// first documentation line. Returns `None` when there is no active signature.
fn signature_help_to_popup(
    help: SignatureHelp,
    anchor_offset: usize,
) -> Option<SignatureHelpPopup> {
    let active_sig = help.active_signature.unwrap_or(0) as usize;
    let SignatureInformation {
        label,
        documentation,
        parameters,
        active_parameter,
    } = help.signatures.into_iter().nth(active_sig)?;

    let active_param = active_parameter
        .or(help.active_parameter)
        .and_then(|idx| parameters.as_ref()?.get(idx as usize).cloned())
        .and_then(|param| param_label_range(&param.label, &label));

    let doc = documentation.and_then(documentation_first_line);

    Some(SignatureHelpPopup {
        label,
        active_param,
        doc,
        anchor_offset,
    })
}

/// Resolve a parameter's label into a char range within the signature label.
/// Offset labels are taken as-is. A substring label is located in `sig_label`
/// and its byte position converted to a char range for the renderer.
fn param_label_range(label: &ParameterLabel, sig_label: &str) -> Option<std::ops::Range<usize>> {
    match label {
        ParameterLabel::LabelOffsets([start, end]) => Some(*start as usize..*end as usize),
        ParameterLabel::Simple(text) => {
            let byte_start = sig_label.find(text.as_str())?;
            let char_start = sig_label[..byte_start].chars().count();
            Some(char_start..char_start + text.chars().count())
        },
    }
}

/// First non-empty documentation line, plain text (markdown passes through).
fn documentation_first_line(doc: Documentation) -> Option<String> {
    let text = match doc {
        Documentation::String(s) => s,
        Documentation::MarkupContent(markup) => markup.value,
    };
    text.lines()
        .find(|line| !line.is_empty())
        .map(str::to_string)
}

/// Poll any in-flight signature-help request
/// ([`Stoat::pending_signature_help_request`]) and apply the result to
/// [`Stoat::pending_signature_help`]. Returns true when state changed.
pub(crate) fn pump_lsp_signature_help(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_signature_help_request.take() else {
        return false;
    };
    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(popup) => {
            stoat.pending_signature_help = popup;
            true
        },
        Poll::Pending => {
            stoat.pending_signature_help_request = Some(task);
            false
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixture::{open_buffer, seed},
        test_harness::TestHarness,
    };

    fn enable_signature_help(h: &TestHarness) {
        use lsp_types::{ServerCapabilities, SignatureHelpOptions};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            signature_help_provider: Some(SignatureHelpOptions {
                trigger_characters: Some(vec!["(".into(), ",".into()]),
                retrigger_characters: Some(vec![",".into()]),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    fn sig_help(active_param: u32) -> SignatureHelp {
        use lsp_types::ParameterInformation;
        SignatureHelp {
            signatures: vec![SignatureInformation {
                label: "fn add(x: i32, y: i32) -> i32".to_string(),
                documentation: None,
                parameters: Some(vec![
                    ParameterInformation {
                        label: ParameterLabel::Simple("x: i32".into()),
                        documentation: None,
                    },
                    ParameterInformation {
                        label: ParameterLabel::Simple("y: i32".into()),
                        documentation: None,
                    },
                ]),
                active_parameter: Some(active_param),
            }],
            active_signature: Some(0),
            active_parameter: Some(active_param),
        }
    }

    #[test]
    fn signature_help_opens_on_trigger_char() {
        let mut h = TestHarness::with_size(80, 24);
        enable_signature_help(&h);
        let root = seed(&mut h, &[("main.rs", "")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        // After typing `(` the cursor sits at line 0, column 1.
        h.fake_lsp()
            .set_signature_help(path.to_str().unwrap(), 0, 1, sig_help(1));
        h.type_keys("i");
        h.type_text("(");
        h.settle();

        let popup = h.stoat.pending_signature_help.as_ref().expect("popup");
        assert_eq!(popup.label, "fn add(x: i32, y: i32) -> i32");
        assert_eq!(popup.active_param, Some(15..21));
    }

    #[test]
    fn signature_help_retrigger_updates_active_parameter() {
        let mut h = TestHarness::with_size(80, 24);
        enable_signature_help(&h);
        let root = seed(&mut h, &[("main.rs", "")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp().set_signature_help(p, 0, 1, sig_help(0));
        h.fake_lsp().set_signature_help(p, 0, 3, sig_help(1));

        h.type_keys("i");
        h.type_text("(");
        h.settle();
        assert_eq!(
            h.stoat
                .pending_signature_help
                .as_ref()
                .expect("popup")
                .active_param,
            Some(7..13),
        );

        h.type_text("x,");
        h.settle();
        assert_eq!(
            h.stoat
                .pending_signature_help
                .as_ref()
                .expect("popup")
                .active_param,
            Some(15..21),
        );
    }

    #[test]
    fn signature_help_cleared_on_leaving_insert() {
        let mut h = TestHarness::with_size(80, 24);
        enable_signature_help(&h);
        let root = seed(&mut h, &[("main.rs", "")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_signature_help(path.to_str().unwrap(), 0, 1, sig_help(1));

        h.type_keys("i");
        h.type_text("(");
        h.settle();
        assert!(h.stoat.pending_signature_help.is_some());

        h.type_keys("escape");
        h.settle();
        assert!(h.stoat.pending_signature_help.is_none());
    }

    #[test]
    fn snapshot_signature_help_active_parameter_bold() {
        let mut h = TestHarness::with_size(60, 12);
        let root = seed(&mut h, &[("main.rs", "add()\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.stoat.pending_signature_help = Some(SignatureHelpPopup {
            label: "fn add(x: i32, y: i32) -> i32".to_string(),
            active_param: Some(15..21),
            doc: Some("adds two integers".to_string()),
            anchor_offset: 0,
        });
        h.assert_snapshot("signature_help_active_param_bold");
    }
}
