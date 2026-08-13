//! Diagnostics the editor asks a server for, rather than waiting to be told.
//!
//! A server that supports it answers `textDocument/diagnostic` on demand, which
//! reports a file the moment it is opened instead of after the server gets round
//! to publishing. The two channels share one store, so a pulled set replaces
//! what that same server published.
//!
//! This one fans out per buffer rather than per focused editor, so the in-flight
//! requests live in a map keyed by buffer and the pump polls the whole map. That
//! is why it holds its own waker instead of a [`crate::lsp::pending::Pending`]
//! slot.
//!
//! No key reaches this. A trigger fires from the post-event fan-out, and a pump
//! collects the replies, both of them background work the user never asks for.

use crate::{
    action_handlers, app::Stoat, buffer::BufferId, host::LanguageServerFeature, lsp::util,
};
use futures::task::noop_waker;
use lsp_types::{
    Diagnostic, DocumentDiagnosticParams, DocumentDiagnosticReport, DocumentDiagnosticReportResult,
    TextDocumentIdentifier, Uri,
};
use std::{
    future::Future,
    path::PathBuf,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

/// Debounce before pulling diagnostics, so a burst of edits collapses into a
/// single request once typing settles.
const PULL_DIAGNOSTICS_DEBOUNCE: Duration = Duration::from_millis(300);

/// The outcome of a `textDocument/diagnostic` pull, ready to apply.
///
/// `Full` replaces the buffer's diagnostics with a fresh set. `Unchanged` is the
/// server's bandwidth optimisation, meaning the previous set still holds, so only
/// the result id is refreshed.
pub(crate) enum PullDiagnosticsOutcome {
    Full {
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
        result_id: Option<String>,
    },
    Unchanged {
        result_id: String,
    },
}

/// Pull diagnostics for every open buffer whose version changed since its last
/// pull, when the server advertises the capability.
///
/// A newly-opened buffer has no key yet, so its first tick pulls. A later edit
/// bumps the version and re-pulls. Each request carries the buffer's previous
/// result id so the server may answer Unchanged. [`pump_lsp_pull_diagnostics`]
/// applies the responses.
pub(crate) fn pull_diagnostics_trigger(stoat: &mut Stoat) {
    let plans: Vec<PullPlan> = stoat
        .lsp_opened
        .iter()
        .copied()
        .filter(|&id| {
            // Skip buffers unchanged since the last pull before the
            // feature_hosts allocation. build_pull_plan repeats this check.
            stoat
                .active_workspace()
                .buffers
                .get(id)
                .map(|buffer| buffer.read().expect("buffer lock").version())
                .is_some_and(|version| stoat.last_pull_diagnostic_key.get(&id) != Some(&version))
        })
        .filter(|&id| {
            stoat
                .feature_hosts(id, LanguageServerFeature::PullDiagnostics)
                .into_iter()
                .next()
                .is_some()
        })
        .filter_map(|id| build_pull_plan(stoat, id))
        .collect();

    for plan in plans {
        stoat.last_pull_diagnostic_key.insert(plan.id, plan.version);

        let params = DocumentDiagnosticParams {
            text_document: TextDocumentIdentifier { uri: plan.uri },
            identifier: None,
            previous_result_id: stoat.pull_diagnostic_result_ids.get(&plan.id).cloned(),
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };

        let host = stoat.lsp_for_feature(plan.id, LanguageServerFeature::PullDiagnostics);
        let executor = stoat.executor.clone();
        let path = plan.path;
        let task = stoat.spawn_woken(async move {
            executor.timer(PULL_DIAGNOSTICS_DEBOUNCE).await;
            match host.document_diagnostic(params).await {
                Ok(Some(report)) => parse_pull_report(report, path),
                Ok(None) => None,
                Err(err) => {
                    tracing::warn!(target: "stoat::lsp", ?err, "document_diagnostic request failed");
                    None
                },
            }
        });
        stoat.pending_pull_diagnostics.insert(plan.id, task);
    }
}

struct PullPlan {
    id: BufferId,
    version: u64,
    uri: Uri,
    path: PathBuf,
}

fn build_pull_plan(stoat: &Stoat, id: BufferId) -> Option<PullPlan> {
    let workspace = stoat.active_workspace();
    let buffer = workspace.buffers.get(id)?;
    let version = buffer.read().expect("buffer lock").version();
    if stoat.last_pull_diagnostic_key.get(&id) == Some(&version) {
        return None;
    }
    let path = workspace.buffers.path_for(id)?.to_path_buf();
    let uri = action_handlers::lsp::path_to_uri(&path)?;
    Some(PullPlan {
        id,
        version,
        uri,
        path,
    })
}

/// Convert a pull report into an applicable outcome, capturing the request-time
/// `path` for the Full case. Streaming `Partial` results carry no primary set and
/// are ignored.
fn parse_pull_report(
    report: DocumentDiagnosticReportResult,
    path: PathBuf,
) -> Option<PullDiagnosticsOutcome> {
    match report {
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(full)) => {
            let report = full.full_document_diagnostic_report;
            Some(PullDiagnosticsOutcome::Full {
                path,
                diagnostics: report.items,
                result_id: report.result_id,
            })
        },
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(unchanged)) => {
            Some(PullDiagnosticsOutcome::Unchanged {
                result_id: unchanged.unchanged_document_diagnostic_report.result_id,
            })
        },
        DocumentDiagnosticReportResult::Partial(_) => None,
    }
}

/// Poll in-flight pull-diagnostic requests and apply any that completed. Returns
/// true when a request resolved.
pub(crate) fn pump_lsp_pull_diagnostics(stoat: &mut Stoat) -> bool {
    if stoat.pending_pull_diagnostics.is_empty() {
        return false;
    }

    let waker = noop_waker();
    let mut cx = Context::from_waker(&waker);
    let ready: Vec<(BufferId, Option<PullDiagnosticsOutcome>)> = {
        let mut ready = Vec::new();
        stoat
            .pending_pull_diagnostics
            .retain(|&id, task| match Pin::new(task).poll(&mut cx) {
                Poll::Ready(outcome) => {
                    ready.push((id, outcome));
                    false
                },
                Poll::Pending => true,
            });
        ready
    };

    if ready.is_empty() {
        return false;
    }
    for (id, outcome) in ready {
        apply_pull_diagnostics(stoat, id, outcome);
    }
    true
}

fn apply_pull_diagnostics(
    stoat: &mut Stoat,
    id: BufferId,
    outcome: Option<PullDiagnosticsOutcome>,
) {
    match outcome {
        Some(PullDiagnosticsOutcome::Full {
            path,
            diagnostics,
            result_id,
        }) => {
            let server = stoat
                .feature_hosts(id, LanguageServerFeature::PullDiagnostics)
                .into_iter()
                .next()
                .map(|(name, _)| name)
                .unwrap_or_else(|| String::from("lsp"));
            let encoding = stoat.lsp_for(id).offset_encoding();
            let spans = util::publish_spans(
                &path,
                &diagnostics,
                encoding,
                &stoat.active_workspace().buffers,
            );
            stoat
                .diagnostics
                .replace_from_server(path, server, diagnostics, spans);
            match result_id {
                Some(rid) => {
                    stoat.pull_diagnostic_result_ids.insert(id, rid);
                },
                None => {
                    stoat.pull_diagnostic_result_ids.remove(&id);
                },
            }
        },
        Some(PullDiagnosticsOutcome::Unchanged { result_id }) => {
            stoat.pull_diagnostic_result_ids.insert(id, result_id);
        },
        None => {},
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixture::{diag, open_buffer, seed},
        test_harness::TestHarness,
    };

    fn enable_pull_diagnostics(h: &TestHarness) {
        use lsp_types::{DiagnosticOptions, DiagnosticServerCapabilities, ServerCapabilities};
        h.fake_lsp().set_capabilities(ServerCapabilities {
            diagnostic_provider: Some(DiagnosticServerCapabilities::Options(
                DiagnosticOptions::default(),
            )),
            ..Default::default()
        });
    }

    fn full_report(
        diagnostics: Vec<Diagnostic>,
        result_id: &str,
    ) -> DocumentDiagnosticReportResult {
        use lsp_types::{FullDocumentDiagnosticReport, RelatedFullDocumentDiagnosticReport};
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Full(
            RelatedFullDocumentDiagnosticReport {
                related_documents: None,
                full_document_diagnostic_report: FullDocumentDiagnosticReport {
                    result_id: Some(result_id.to_string()),
                    items: diagnostics,
                },
            },
        ))
    }

    fn unchanged_report(result_id: &str) -> DocumentDiagnosticReportResult {
        use lsp_types::{
            RelatedUnchangedDocumentDiagnosticReport, UnchangedDocumentDiagnosticReport,
        };
        DocumentDiagnosticReportResult::Report(DocumentDiagnosticReport::Unchanged(
            RelatedUnchangedDocumentDiagnosticReport {
                related_documents: None,
                unchanged_document_diagnostic_report: UnchangedDocumentDiagnosticReport {
                    result_id: result_id.to_string(),
                },
            },
        ))
    }

    #[test]
    fn pull_diagnostics_on_open_renders() {
        let mut h = TestHarness::with_size(80, 24);
        enable_pull_diagnostics(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_diagnostic(
            path.to_str().unwrap(),
            full_report(vec![diag(0, 4, "unused")], "rev-1"),
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(350));
        assert_eq!(h.stoat.diagnostics.get(&path).len(), 1);
        assert_eq!(h.stoat.diagnostics.get(&path)[0].message, "unused");
    }

    #[test]
    fn pull_diagnostics_unchanged_keeps_set() {
        let mut h = TestHarness::with_size(80, 24);
        enable_pull_diagnostics(&h);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        let p = path.to_str().unwrap();
        h.fake_lsp()
            .set_document_diagnostic(p, full_report(vec![diag(0, 4, "unused")], "rev-1"));
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(350));
        assert_eq!(h.stoat.diagnostics.get(&path).len(), 1);

        h.fake_lsp()
            .set_document_diagnostic(p, unchanged_report("rev-1"));
        h.edit_focused(0..0, "// c\n");
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(350));
        assert_eq!(h.stoat.diagnostics.get(&path).len(), 1);
        assert_eq!(h.stoat.diagnostics.get(&path)[0].message, "unused");
    }

    #[test]
    fn pull_diagnostics_push_only_server_never_pulls() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
        let path = root.join("main.rs");
        open_buffer(&mut h, path.clone());
        h.fake_lsp().set_document_diagnostic(
            path.to_str().unwrap(),
            full_report(vec![diag(0, 4, "unused")], "rev-1"),
        );
        h.type_keys("escape");
        h.advance_clock(Duration::from_millis(350));
        assert!(h.stoat.diagnostics.get(&path).is_empty());
    }
}
