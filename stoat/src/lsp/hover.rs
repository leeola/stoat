//! What a server says about the symbol under the cursor.
//!
//! Hover is the one LSP feature here that a key reaches directly, but what it
//! does with the answer is the same as its keyless siblings. The request goes
//! out on a task, a pump collects it, and the result becomes a popup the
//! renderer owns.
//!
//! Every server that routes hover for the buffer is asked, and their answers
//! are merged under per-server headers. A lone responder renders with no header
//! at all, so the common case reads as one server's answer.

use crate::{
    action_handlers,
    app::{Stoat, UpdateEffect},
    editor_state::EditorId,
    host::LanguageServerFeature,
    lsp::{hosts, session, sync, util},
    markdown,
    render::hover::HoverPopup,
};
use lsp_types::{
    HoverContents, HoverParams, MarkedString, MarkupKind, TextDocumentIdentifier,
    TextDocumentPositionParams,
};
use ratatui::style::Style;
use std::{
    future::Future,
    path::Path,
    pin::Pin,
    task::{Context, Poll},
};

/// Hover response carried from the spawned task to [`pump_lsp_hover`].
///
/// `text` is the flattened markdown, awaiting parsing on the main-loop side
/// where the theme lives. `plain` marks PlainText content that must render
/// verbatim rather than as markdown. `anchor_offset` is the cursor byte offset
/// captured when the request fired so the popup anchors at the symbol even if
/// the cursor moves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HoverResponse {
    pub(crate) text: String,
    pub(crate) plain: bool,
    pub(crate) anchor_offset: usize,
    /// The editor focused when the request fired. A response is dropped if
    /// focus has since moved, so a popup never anchors against a pane that did
    /// not request it.
    pub(crate) editor_id: EditorId,
}

/// The outcome of a spawned hover request, carried to [`pump_lsp_hover`].
///
/// Distinguishing an empty answer from a failed request lets the status bar
/// report honest state. A server still indexing says so and a broken request
/// says it failed, rather than collapsing both to a flat "no hover info".
pub(crate) enum HoverOutcome {
    /// The server returned hover content to render.
    Content(HoverResponse),
    /// The server answered with no hover for the cursor position.
    Empty,
    /// The request errored.
    Failed,
}

/// Issue a `textDocument/hover` request for the symbol under the
/// focused editor's primary cursor. The async response is stored on
/// [`Stoat::pending_hover_request`] and applied by [`pump_lsp_hover`]
/// on the next render tick.
///
/// No-op when the focused pane is not an editor or the buffer has no
/// path. When the server does not advertise
/// [`LanguageServerFeature::Hover`], reports the language-server state
/// to the status bar instead. Replacing the prior pending task
/// drops it, cancelling its spawned future -- only one in-flight hover
/// is tracked at a time.
pub(crate) fn hover(stoat: &mut Stoat) -> UpdateEffect {
    let Some((editor_id, _)) = stoat.focused_editor_ids() else {
        return UpdateEffect::None;
    };

    let (anchor_offset, buffer_id, focused_rope, is_review) = {
        let Some(editor) = action_handlers::focused_editor_mut(stoat) else {
            return UpdateEffect::None;
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

    let hosts = hosts::feature_hosts(stoat, buffer_id, LanguageServerFeature::Hover);
    if hosts.is_empty() {
        return session::report_lsp_unavailable(stoat, "hover");
    }

    // A review cursor requests against the real working-tree file, but the
    // popup still anchors at the placeholder cursor cell, so `anchor_offset`
    // stays the review-editor offset while the request uses the real file.
    let (source_path, source_rope, cursor_offset) = if is_review {
        match action_handlers::lsp::review_lsp_source(stoat) {
            Some(resolved) => resolved,
            None => return UpdateEffect::None,
        }
    } else {
        let Some(path) = stoat
            .active_workspace()
            .buffers
            .path_for(buffer_id)
            .map(Path::to_path_buf)
        else {
            return UpdateEffect::None;
        };
        (path, focused_rope, anchor_offset)
    };
    let Some(source_uri) = action_handlers::lsp::path_to_uri(&source_path) else {
        return UpdateEffect::None;
    };

    // The position was measured after edits whose change may still be sitting in
    // its debounce, and a server cannot place a position in text it has not been
    // sent.
    let pending_change = sync::flush_pending_did_change(stoat, buffer_id);
    let task = stoat.spawn_woken(async move {
        if let Some(pending_change) = pending_change {
            pending_change.await;
        }
        let requests = hosts.iter().map(|(name, host)| {
            let name = name.clone();
            let encoding = host.offset_encoding();
            let position = util::byte_offset_to_lsp_pos(&source_rope, cursor_offset, encoding);
            let params = HoverParams {
                text_document_position_params: TextDocumentPositionParams {
                    text_document: TextDocumentIdentifier {
                        uri: source_uri.clone(),
                    },
                    position,
                },
                work_done_progress_params: Default::default(),
            };
            async move { (name, host.hover(params).await) }
        });
        let responses = futures::future::join_all(requests).await;

        let mut sections = Vec::new();
        let mut any_empty = false;
        for (name, result) in responses {
            match result {
                Ok(Some(hover)) => {
                    let (text, plain) = flatten_hover_contents(hover.contents);
                    sections.push((name, text, plain));
                },
                Ok(None) => any_empty = true,
                Err(err) => tracing::warn!(target: "stoat::lsp", ?err, "hover request failed"),
            }
        }

        if sections.is_empty() {
            if any_empty {
                HoverOutcome::Empty
            } else {
                HoverOutcome::Failed
            }
        } else {
            let (text, plain) = merge_hovers(sections);
            HoverOutcome::Content(HoverResponse {
                text,
                plain,
                anchor_offset,
                editor_id,
            })
        }
    });
    stoat.pending_hover_request = Some(task);
    UpdateEffect::None
}

/// Flatten an LSP [`HoverContents`] payload into a markdown string and a flag
/// marking whether it is PlainText.
///
/// A [`MarkedString::LanguageString`] becomes a fenced code block so the
/// language is highlighted, except a `markdown` language passes through as-is.
/// PlainText markup is returned verbatim with the flag set, so the caller
/// renders it without interpreting markdown syntax.
pub(crate) fn flatten_hover_contents(contents: HoverContents) -> (String, bool) {
    fn marked_to_markdown(m: MarkedString) -> String {
        match m {
            MarkedString::String(s) => s,
            MarkedString::LanguageString(ls) if ls.language == "markdown" => ls.value,
            MarkedString::LanguageString(ls) => {
                format!("```{}\n{}\n```", ls.language, ls.value)
            },
        }
    }

    match contents {
        HoverContents::Scalar(m) => (marked_to_markdown(m), false),
        HoverContents::Array(items) => (
            items
                .into_iter()
                .map(marked_to_markdown)
                .collect::<Vec<_>>()
                .join("\n"),
            false,
        ),
        HoverContents::Markup(markup) => (markup.value, markup.kind == MarkupKind::PlainText),
    }
}

/// Combine every responding server's hover markdown into one popup body.
///
/// A lone responder renders exactly as a single-server hover always has, its
/// own text with no header. Two or more responders each get a `**{server}**`
/// section header and are joined by a `---` rule so a reader can tell which
/// server said what.
///
/// The merged body is plain only when every section is plain text. One markdown
/// section makes the whole popup markdown.
///
/// `sections` is `(server_name, text, plain)` in server routing order, which the
/// output preserves.
pub(crate) fn merge_hovers(sections: Vec<(String, String, bool)>) -> (String, bool) {
    if sections.len() == 1 {
        let (_, text, plain) = sections.into_iter().next().expect("one section");
        return (text, plain);
    }

    let plain = sections.iter().all(|(_, _, plain)| *plain);
    let body = sections
        .iter()
        .map(|(server, text, _)| format!("**{server}**\n\n{text}"))
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");
    (body, plain)
}

/// Poll any in-flight hover request ([`Stoat::pending_hover_request`])
/// and apply the [`HoverOutcome`].
///
/// `Content` writes the popup to [`Stoat::pending_hover`]. `Empty` and `Failed`
/// clear it and set an honest status message. `Pending` puts the task back.
/// Returns true when state changed so the caller can request a redraw.
pub(crate) fn pump_lsp_hover(stoat: &mut Stoat) -> bool {
    let Some(mut task) = stoat.pending_hover_request.take() else {
        return false;
    };
    let waker = futures::task::noop_waker();
    let mut cx = Context::from_waker(&waker);
    match Pin::new(&mut task).poll(&mut cx) {
        Poll::Ready(HoverOutcome::Content(response)) => {
            // Drop a response whose editor lost focus while the request was in
            // flight, so the popup never anchors against a pane that did not
            // request it.
            if stoat.focused_editor_ids().map(|(id, _)| id) != Some(response.editor_id) {
                stoat.pending_hover = None;
                return true;
            }
            let lines = if response.plain {
                response
                    .text
                    .lines()
                    .map(|line| vec![(line.to_string(), Style::default())])
                    .collect()
            } else {
                markdown::render_markdown(&response.text, &stoat.theme, &stoat.language_registry)
            };
            stoat.pending_hover = Some(HoverPopup::new(
                lines,
                response.anchor_offset,
                response.editor_id,
            ));
            true
        },
        Poll::Ready(HoverOutcome::Empty) => {
            // A busy server is still worth naming, so an empty result during
            // work-done progress reports which operation is running. The
            // progress segment already shows the percentage, so none is added.
            let status = match stoat.lsp_progress.current() {
                Some(entry) => {
                    let body = if !entry.title.is_empty() {
                        entry.title.as_str()
                    } else {
                        entry.message.as_deref().unwrap_or("working")
                    };
                    format!("lsp: no hover info yet ({} {})", entry.server, body)
                },
                None => "lsp: no hover info".to_string(),
            };
            session::set_lsp_status(stoat, status);
            stoat.pending_hover = None;
            true
        },
        Poll::Ready(HoverOutcome::Failed) => {
            session::set_lsp_status(stoat, "lsp: hover request failed".to_string());
            stoat.pending_hover = None;
            true
        },
        Poll::Pending => {
            stoat.pending_hover_request = Some(task);
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
    use crossterm::event::{Event, KeyModifiers, MouseEvent, MouseEventKind};
    use futures::FutureExt;
    use std::{path::PathBuf, time::Duration};

    #[test]
    fn hover_routes_to_a_secondary_when_the_primary_lacks_it() {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{HoverProviderCapability, ServerCapabilities};

        let mut h = TestHarness::with_size(80, 24);
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(ServerCapabilities::default());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        });
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );

        let root = seed(&mut h, &[("a.rs", "abc\ndef\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        secondary.set_hover(path.to_str().unwrap(), 0, 0, "from secondary");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let popup = h
            .stoat
            .pending_hover
            .as_ref()
            .expect("the hover-capable secondary answered");
        assert_eq!(
            popup.lines,
            vec![vec![("from secondary".to_string(), Style::default())]]
        );
        assert_ne!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: server does not support hover"),
            "a capable secondary means the noop sole host never gates hover out"
        );
    }

    #[test]
    fn hover_position_encodes_with_the_routed_host() {
        use crate::{host::OffsetEncoding, lsp::registry::ServerSelector};
        use lsp_types::{HoverProviderCapability, ServerCapabilities};

        let mut h = TestHarness::with_size(80, 24);
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(ServerCapabilities::default());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        });
        secondary.set_offset_encoding(OffsetEncoding::Utf8);
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );

        // The cursor sits after a 2-byte char, so the LSP column is 2 under UTF-8
        // and 1 under UTF-16. Placing the hover at column 2 means the popup only
        // appears when the position encodes with the routed host's UTF-8, not the
        // noop sole host's default UTF-16.
        let root = seed(&mut h, &[("a.rs", "\u{e9}x\n")]);
        let path = root.join("a.rs");
        open_buffer(&mut h, path.clone());
        action_handlers::movement::jump_to_offset(&mut h.stoat, 2);
        secondary.set_hover(path.to_str().unwrap(), 0, 2, "utf8 routed");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let popup = h
            .stoat
            .pending_hover
            .as_ref()
            .expect("hover position encoded with the routed host's UTF-8");
        assert_eq!(
            popup.lines,
            vec![vec![("utf8 routed".to_string(), Style::default())]]
        );
    }

    /// Install two hover-capable fakes routed primary-then-secondary for `rust`,
    /// open a buffer, and return the fakes and its path.
    fn two_hover_servers(
        h: &mut TestHarness,
    ) -> (
        std::sync::Arc<crate::host::FakeLsp>,
        std::sync::Arc<crate::host::FakeLsp>,
        PathBuf,
    ) {
        use crate::lsp::registry::ServerSelector;
        use lsp_types::{HoverProviderCapability, ServerCapabilities};

        let caps = ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..ServerCapabilities::default()
        };
        let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
        primary.set_capabilities(caps.clone());
        let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
        secondary.set_capabilities(caps);
        h.stoat
            .lsp_registry
            .insert("primary".into(), primary.clone());
        h.stoat
            .lsp_registry
            .insert("secondary".into(), secondary.clone());
        h.stoat.lsp_registry.set_selectors(
            "rust".into(),
            vec![
                ServerSelector::all("primary".into()),
                ServerSelector::all("secondary".into()),
            ],
        );

        let root = seed(h, &[("a.rs", "abc\ndef\n")]);
        let path = root.join("a.rs");
        open_buffer(h, path.clone());
        (primary, secondary, path)
    }

    fn hover_body(h: &TestHarness) -> String {
        h.stoat
            .pending_hover
            .as_ref()
            .expect("hover popup")
            .lines
            .iter()
            .map(|line| line.iter().map(|(t, _)| t.as_str()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn hover_merges_sections_from_two_servers() {
        let mut h = TestHarness::with_size(80, 24);
        let (primary, secondary, path) = two_hover_servers(&mut h);
        let p = path.to_str().unwrap();
        primary.set_hover(p, 0, 0, "alpha docs");
        secondary.set_hover(p, 0, 0, "beta docs");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let body = hover_body(&h);
        for needle in ["primary", "secondary", "alpha docs", "beta docs"] {
            assert!(
                body.contains(needle),
                "merged hover missing {needle:?}: {body:?}"
            );
        }
    }

    #[test]
    fn hover_omits_the_header_when_only_one_server_answers() {
        let mut h = TestHarness::with_size(80, 24);
        let (_primary, secondary, path) = two_hover_servers(&mut h);
        secondary.set_hover(path.to_str().unwrap(), 0, 0, "from secondary");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let popup = h
            .stoat
            .pending_hover
            .as_ref()
            .expect("the content section is shown");
        assert_eq!(
            popup.lines,
            vec![vec![("from secondary".to_string(), Style::default())]],
            "a lone responder renders unheaded"
        );
    }

    #[test]
    fn merge_hovers_single_section_passes_through_unheaded() {
        assert_eq!(
            merge_hovers(vec![("ra".into(), "hello".into(), false)]),
            ("hello".to_string(), false)
        );
    }

    #[test]
    fn merge_hovers_joins_sections_in_routing_order_with_headers() {
        assert_eq!(
            merge_hovers(vec![
                ("ra".into(), "A".into(), false),
                ("ty".into(), "B".into(), false),
            ]),
            ("**ra**\n\nA\n\n---\n\n**ty**\n\nB".to_string(), false)
        );
    }

    #[test]
    fn merge_hovers_is_plain_only_when_every_section_is_plain() {
        assert!(merge_hovers(vec![("a".into(), "x".into(), true)]).1);
        assert!(
            !merge_hovers(vec![
                ("a".into(), "x".into(), true),
                ("b".into(), "y".into(), false),
            ])
            .1
        );
    }

    #[test]
    fn hover_no_result_reports_no_hover_info() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: no hover info"),
        );
    }

    #[test]
    fn hover_no_result_during_progress_names_the_operation() {
        use crate::host::LspNotification;
        use lsp_types::{NumberOrString, WorkDoneProgress, WorkDoneProgressBegin};

        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        h.stoat.lsp_progress.update(
            "primary",
            &LspNotification::Progress {
                token: NumberOrString::Number(1),
                value: WorkDoneProgress::Begin(WorkDoneProgressBegin {
                    title: "indexing".into(),
                    cancellable: None,
                    message: None,
                    percentage: None,
                }),
            },
        );

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: no hover info yet (primary indexing)"),
        );
    }

    #[test]
    fn hover_request_failure_reports_it() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        h.fake_lsp()
            .fail_next_request("textDocument/hover", std::io::ErrorKind::Other);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: hover request failed"),
        );
    }

    #[test]
    fn in_flight_hover_shows_a_status_segment() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        // Hold the response open so the request stays in flight through render.
        h.fake_lsp()
            .set_request_delay("textDocument/hover", Duration::from_secs(60));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        assert!(
            h.stoat.pending_hover_request.is_some(),
            "the delayed hover request stays in flight",
        );

        let buf = h.render_composited();
        let shown = (0..buf.area.height).any(|y| {
            let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
            row.replace('─', " ").contains("lsp: hover...")
        });
        assert!(shown, "the status bar shows the in-flight hover segment");
    }

    fn enable_hover(h: &TestHarness) {
        use lsp_types::{HoverProviderCapability, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            ..Default::default()
        });
    }

    #[test]
    fn hover_popup_appears_on_response() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        let popup = h.stoat.pending_hover.as_ref().expect("popup");
        assert_eq!(
            popup.lines,
            vec![vec![("fn foo() -> u32".to_string(), Style::default())]]
        );
        assert_eq!(popup.anchor_offset, 0);
    }

    #[test]
    fn a_hover_flushes_the_edit_its_position_was_measured_after() {
        use lsp_types::{
            HoverProviderCapability, ServerCapabilities, TextDocumentSyncCapability,
            TextDocumentSyncKind,
        };
        let mut h = TestHarness::with_size(80, 24);
        h.fake_lsp().set_capabilities(ServerCapabilities {
            hover_provider: Some(HoverProviderCapability::Simple(true)),
            text_document_sync: Some(TextDocumentSyncCapability::Kind(
                TextDocumentSyncKind::INCREMENTAL,
            )),
            ..Default::default()
        });
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());

        h.type_keys("i");
        h.type_text("x");
        // No clock advance, so the change debounce has not run out on its own.
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let sent: Vec<String> = h
            .fake_lsp()
            .observed_changes()
            .iter()
            .flat_map(|c| c.content_changes.iter().map(|e| e.text.clone()))
            .collect();
        assert_eq!(
            sent,
            vec!["x".to_string()],
            "hover named a position in text the server was never sent",
        );
    }

    #[test]
    fn hover_response_dropped_when_focus_moved_to_another_editor() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");

        // Hover from the focused pane, then split so focus moves to the new
        // pane's editor before the response settles.
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
        h.settle();

        assert!(
            h.stoat.pending_hover.is_none(),
            "a response for an editor that lost focus is dropped"
        );
    }

    #[test]
    fn hover_response_signals_redraw_notify() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");

        // open_buffer's parse/reindex also wakes redraw_notify. Consume that
        // permit (against an Arc clone, so the observer never borrows `h`
        // across settle) before triggering hover, leaving the hover
        // response's wake as the only one to observe. Notify holds at most
        // one permit, so a single drain clears it.
        let redraw = h.stoat.redraw_notify.clone();
        let _ = redraw.notified().now_or_never();

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        let notified = redraw.notified();
        tokio::pin!(notified);
        assert!(
            notified.enable(),
            "hover response should wake redraw_notify so the popup paints \
             without waiting for the next keystroke",
        );
    }

    #[test]
    fn hover_no_response_clears_request() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        assert!(h.stoat.pending_hover.is_none());
        assert!(h.stoat.pending_hover_request.is_none());
    }

    #[test]
    fn hover_unsupported_capability_is_noop() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "ignored");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        assert!(h.stoat.pending_hover.is_none());
        assert!(h.stoat.pending_hover_request.is_none());
    }

    #[test]
    fn hover_without_capability_reports_it_in_the_status() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        open_buffer(&mut h, root.join("main.rs"));

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);

        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("lsp: server does not support hover"),
        );
    }

    /// Populate a hover popup over `main.rs`, leaving the editor in normal mode.
    fn open_hover(h: &mut TestHarness) {
        enable_hover(h);
        let root = seed(h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "details");
        h.type_keys("space l i");
        h.settle();
        assert!(h.stoat.pending_hover.is_some(), "popup should be open");
    }

    #[test]
    fn hover_dismissed_by_escape() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);

        h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
        assert!(h.stoat.pending_hover.is_none());
        assert!(h.stoat.pending_hover_request.is_none());
    }

    #[test]
    fn hover_dismissed_by_ctrl_c_without_quitting() {
        use crate::{test_harness::keys, UpdateEffect};
        use crossterm::event::Event;
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);

        let effect = h.stoat.update(Event::Key(keys::ctrl('c')));
        assert!(
            matches!(effect, UpdateEffect::Redraw),
            "Ctrl-c closes the hover rather than quitting the app"
        );
        assert!(h.stoat.pending_hover.is_none());
    }

    #[test]
    fn hover_dismissed_entering_insert_mode() {
        use crate::test_harness::keys;
        use crossterm::event::{Event, KeyCode};
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);

        // `i` is SetMode(insert)-only, so it skips the post-dispatch clear; the
        // auto-close intercept closes the popup and the key still enters insert.
        h.stoat.update(Event::Key(keys::key(KeyCode::Char('i'))));
        assert!(h.stoat.pending_hover.is_none(), "the popup closes");
        assert_eq!(
            h.stoat.focused_mode(),
            "insert",
            "and the key still dispatches"
        );
    }

    fn hover_scroll(h: &TestHarness) -> usize {
        h.stoat
            .pending_hover
            .as_ref()
            .expect("popup")
            .scroll_half_pages
    }

    fn scroll_event(kind: MouseEventKind, column: u16, row: u16) -> Event {
        Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        })
    }

    #[test]
    fn hover_scrolls_by_half_pages() {
        use crate::test_harness::keys;
        use crossterm::event::Event;
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);

        h.stoat.update(Event::Key(keys::ctrl('d')));
        h.stoat.update(Event::Key(keys::ctrl('d')));
        assert_eq!(hover_scroll(&h), 2);
        h.stoat.update(Event::Key(keys::ctrl('u')));
        assert_eq!(hover_scroll(&h), 1);
        assert!(
            h.stoat.pending_hover.is_some(),
            "scrolling consumes the key without closing the popup"
        );
    }

    #[test]
    fn hover_scroll_up_saturates_at_the_top() {
        use crate::test_harness::keys;
        use crossterm::event::Event;
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);

        h.stoat.update(Event::Key(keys::ctrl('u')));
        assert_eq!(hover_scroll(&h), 0);
        assert!(h.stoat.pending_hover.is_some());
    }

    #[test]
    fn wheel_over_the_popup_scrolls_it() {
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);
        // Render once so render_hover stamps the popup's screen rect.
        h.stoat.render();

        let area = h.stoat.pending_hover.as_ref().expect("popup").area;
        h.stoat.update(scroll_event(
            MouseEventKind::ScrollDown,
            area.x + area.width / 2,
            area.y + area.height / 2,
        ));

        assert_eq!(hover_scroll(&h), 1, "the wheel scrolls the popup");
        assert!(h.stoat.pending_hover.is_some(), "and leaves it open");
    }

    #[test]
    fn wheel_outside_the_popup_leaves_it_unscrolled() {
        let mut h = TestHarness::with_size(80, 24);
        open_hover(&mut h);
        h.stoat.render();

        let area = h.stoat.pending_hover.as_ref().expect("popup").area;
        // Just past the popup's bottom edge, still over the editor pane.
        h.stoat.update(scroll_event(
            MouseEventKind::ScrollDown,
            area.x,
            area.y + area.height,
        ));

        assert_eq!(
            hover_scroll(&h),
            0,
            "a wheel off the popup does not scroll it"
        );
        assert!(h.stoat.pending_hover.is_some(), "and leaves it open");
    }

    #[test]
    fn snapshot_hover_scrolled_down() {
        use crate::test_harness::keys;
        use crossterm::event::Event;
        let mut h = TestHarness::with_size(40, 12);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let body = (0..20)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.fake_lsp().set_hover(path.to_str().unwrap(), 0, 0, &body);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        h.stoat.update(Event::Key(keys::ctrl('d')));
        h.stoat.update(Event::Key(keys::ctrl('d')));
        h.assert_snapshot("snapshot_hover_scrolled");
    }

    #[test]
    fn snapshot_hover_below_when_tall() {
        let mut h = TestHarness::with_size(40, 20);
        enable_hover(&h);
        let source = (0..12)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let root = seed(&mut h, &[("main.rs", &source)]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());

        // Cursor on buffer line 5, leaving room below for the tall body.
        if let Some(editor) = action_handlers::focused_editor_mut(&mut h.stoat) {
            action_handlers::movement::set_cursor_row(editor, 5);
        }

        let body = (0..10)
            .map(|i| format!("hover {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        h.fake_lsp().set_hover(path.to_str().unwrap(), 5, 0, &body);
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        h.assert_snapshot("snapshot_hover_below_when_tall");
    }

    #[test]
    fn hover_cleared_on_motion() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "details");
        h.type_keys("space l i");
        h.settle();
        assert!(h.stoat.pending_hover.is_some());
        h.type_keys("j");
        assert!(h.stoat.pending_hover.is_none());
    }

    #[test]
    fn space_l_i_triggers_hover() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "documentation");
        h.type_keys("space l i");
        h.settle();
        let popup = h.stoat.pending_hover.as_ref().expect("popup");
        assert_eq!(
            popup.lines,
            vec![vec![("documentation".to_string(), Style::default())]]
        );
        assert_eq!(h.stoat.focused_mode(), "normal");
    }

    #[test]
    fn hover_renders_highlighted_code_and_prose() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "abc\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_hover(
            path.to_str().unwrap(),
            0,
            0,
            "```rust\nfn foo()\n```\nDocs here",
        );
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        let popup = h.stoat.pending_hover.as_ref().expect("popup");

        let texts: Vec<String> = popup
            .lines
            .iter()
            .map(|line| line.iter().map(|(text, _)| text.as_str()).collect())
            .collect();
        assert_eq!(texts, vec!["fn foo()", "", "Docs here"]);
        assert!(
            popup.lines[0].len() > 1,
            "the rust code line is syntax-highlighted into multiple spans"
        );
    }

    #[test]
    fn snapshot_hover_popup_above_cursor() {
        let mut h = TestHarness::with_size(40, 12);
        enable_hover(&h);
        let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp()
            .set_hover(path.to_str().unwrap(), 0, 0, "fn foo() -> u32");
        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();
        h.assert_snapshot("snapshot_hover_popup");
    }

    /// Move the review editor's text cursor to `buffer_row`. Panics without an
    /// open review session.
    fn place_review_cursor(h: &mut TestHarness, buffer_row: u32) {
        let review_editor_id = h.with_review(|s| s.view_editor).expect("review editor");
        let ws = h.stoat.active_workspace_mut();
        let editor = ws.editors.get_mut(review_editor_id).expect("editor");
        action_handlers::movement::set_cursor_row(editor, buffer_row);
    }

    #[test]
    fn hover_from_a_non_working_tree_review_issues_nothing() {
        let mut h = TestHarness::with_size(80, 24);
        enable_hover(&h);
        // An in-memory (non-working-tree) review: the new side is not disk
        // state, so LSP stays off and no request is issued.
        h.open_review_from_texts(&[("a.rs", "a\nb\nc\nd\n", "a\nb\nX\nd\n")]);

        place_review_cursor(&mut h, 2);
        h.fake_lsp().set_hover("a.rs", 2, 0, "unreachable");

        action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);
        h.settle();

        assert!(
            h.stoat.pending_hover.is_none(),
            "no popup for a non-working-tree review",
        );
        assert!(
            h.stoat.pending_hover_request.is_none(),
            "no request was issued",
        );
    }
}
