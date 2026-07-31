//! Workspace-wide LSP diagnostic store. Receives notifications via
//! [`crate::host::LspNotification::Diagnostics`] and exposes a
//! per-path summary that the status bar consumes.
//!
//! Each path's diagnostics are keyed by the reporting server, so several
//! servers on one file (rust-analyzer plus a linter) contribute layered
//! diagnostics that merge on read rather than clobbering each other.

use lsp_types::{Diagnostic, DiagnosticSeverity};
use std::{
    collections::{BTreeMap, HashMap},
    ops::Range,
    path::{Path, PathBuf},
};

/// Where a diagnostic sat in the buffer when its publish landed.
///
/// A server measures a diagnostic against a document version and names it by
/// line and column. Converting that against the current text puts the mark
/// wherever those coordinates now point, which after an edit above it is the
/// wrong place. Resolving once, when the publish arrives and the coordinates
/// still mean what the server meant, gives a span that later edits can be
/// carried through instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedSpan {
    pub range: Range<usize>,
    /// The buffer version [`Self::range`] is in the coordinates of.
    ///
    /// [`u64::MAX`] means the span was never resolved against a buffer, so
    /// there is no history to carry it through and a reader leaves it alone.
    pub base_version: u64,
}

/// One server's published diagnostics for a path, with each one's span.
#[derive(Debug, Default, Clone)]
struct ServerPublish {
    diagnostics: Vec<Diagnostic>,
    /// One per entry of [`Self::diagnostics`], in the same order.
    spans: Vec<PublishedSpan>,
}

/// A path's diagnostics grouped by the server that published them, plus their
/// merged list cached so reads hand out a borrow without re-merging.
#[derive(Debug, Default, Clone)]
struct PathDiagnostics {
    by_server: BTreeMap<String, ServerPublish>,
    merged: Vec<Diagnostic>,
    /// Spans for [`Self::merged`], in the same order, kept beside it so the two
    /// are built by one traversal and cannot fall out of step.
    merged_spans: Vec<PublishedSpan>,
    /// Severity counts over [`Self::merged`], refreshed whenever a server
    /// republishes for this path.
    ///
    /// The status bar asks for this every frame while diagnostics change only
    /// when a server speaks, so counting on read would walk the list for an
    /// answer that almost never moves.
    summary: DiagnosticSummary,
}

impl PathDiagnostics {
    /// The merged diagnostic list for this path.
    ///
    /// A single server's slice is its own merge, so it is read straight from
    /// `by_server`; `merged` is materialized only while more than one server
    /// contributes, sparing a full clone on the common single-server publish.
    fn merged(&self) -> &[Diagnostic] {
        if self.by_server.len() == 1 {
            &self
                .by_server
                .values()
                .next()
                .expect("one server")
                .diagnostics
        } else {
            &self.merged
        }
    }

    /// Spans for [`Self::merged`], in the same order, read the same way.
    fn merged_spans(&self) -> &[PublishedSpan] {
        if self.by_server.len() == 1 {
            &self.by_server.values().next().expect("one server").spans
        } else {
            &self.merged_spans
        }
    }
}

/// Maps each known file path to its per-server diagnostics. Each server
/// publishes a full snapshot per `textDocument/publishDiagnostics`, replacing
/// only its own slice for the path. An empty slice clears that server's
/// contribution.
#[derive(Debug, Default, Clone)]
pub struct DiagnosticSet {
    by_path: HashMap<PathBuf, PathDiagnostics>,
    /// Bumped on every mutation so render-side caches keyed off the set can
    /// detect a change without comparing the diagnostics themselves.
    version: u64,
}

/// Severity-bucketed counts for a single document plus the worst
/// severity present, used by the status bar to paint a compact badge.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiagnosticSummary {
    pub error: usize,
    pub warning: usize,
    pub information: usize,
    pub hint: usize,
    pub worst: Option<DiagnosticSeverity>,
}

impl DiagnosticSummary {
    /// True when no diagnostics are present (all severity counts zero).
    /// The status bar uses this to decide whether to paint a badge.
    pub fn is_empty(&self) -> bool {
        self.error == 0 && self.warning == 0 && self.information == 0 && self.hint == 0
    }
}

impl PublishedSpan {
    /// A span for a diagnostic nothing could resolve, because the path has no
    /// open buffer to measure against. Carried through no edits.
    pub fn unresolved() -> Self {
        Self {
            range: 0..0,
            base_version: u64::MAX,
        }
    }
}

impl DiagnosticSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replaces `server`'s diagnostics for `path`, leaving other servers'
    /// contributions intact.
    ///
    /// A server publishes a full snapshot per `textDocument/publishDiagnostics`,
    /// so its prior slice for the path is dropped. An empty slice clears its
    /// contribution. When the last server clears its slice, the path is
    /// dropped.
    ///
    /// `spans` is one entry per diagnostic, resolved by the caller against the
    /// text the publish was measured for. The caller has the rope and the
    /// server's offset encoding. The store has neither, and by the time anything
    /// reads these the text has usually moved on.
    pub fn replace_from_server(
        &mut self,
        path: PathBuf,
        server: String,
        diagnostics: Vec<Diagnostic>,
        spans: Vec<PublishedSpan>,
    ) {
        debug_assert_eq!(diagnostics.len(), spans.len(), "one span per diagnostic");
        self.version += 1;
        let entry = self.by_path.entry(path.clone()).or_default();
        if diagnostics.is_empty() {
            entry.by_server.remove(&server);
        } else {
            entry
                .by_server
                .insert(server, ServerPublish { diagnostics, spans });
        }
        if entry.by_server.is_empty() {
            self.by_path.remove(&path);
            return;
        }

        if entry.by_server.len() > 1 {
            entry.merged = entry
                .by_server
                .values()
                .flat_map(|publish| publish.diagnostics.iter().cloned())
                .collect();
            entry.merged_spans = entry
                .by_server
                .values()
                .flat_map(|publish| publish.spans.iter().cloned())
                .collect();
        } else {
            // A single server's slice is read directly by `merged()`, so skip
            // the clone. Drop any stale multi-server copy so it never lingers.
            entry.merged = Vec::new();
            entry.merged_spans = Vec::new();
        }
        entry.summary = summarize_diagnostics(entry.merged());
    }

    /// Replaces the whole diagnostic list for `path` from a single unnamed
    /// server, for tests whose subject is the store rather than where anything
    /// sits. Every span is unresolved, so nothing carries them anywhere.
    #[cfg(test)]
    pub fn replace_for_path(&mut self, path: PathBuf, diagnostics: Vec<Diagnostic>) {
        let spans = vec![PublishedSpan::unresolved(); diagnostics.len()];
        self.replace_from_server(path, "lsp".to_string(), diagnostics, spans);
    }

    /// [`Self::replace_for_path`] with each span resolved against `rope` in
    /// UTF-16, for tests that assert where a diagnostic lands rather than what
    /// the store holds.
    #[cfg(test)]
    pub fn replace_for_path_in(
        &mut self,
        path: PathBuf,
        diagnostics: Vec<Diagnostic>,
        rope: &stoat_text::Rope,
    ) {
        let spans = diagnostics
            .iter()
            .map(|diag| PublishedSpan {
                range: crate::lsp::util::lsp_range_to_byte_range(
                    rope,
                    diag.range,
                    crate::host::OffsetEncoding::Utf16,
                ),
                base_version: u64::MAX,
            })
            .collect();
        self.replace_from_server(path, "lsp".to_string(), diagnostics, spans);
    }

    /// Each diagnostic for `path` with the span it was published at, in the same
    /// order [`Self::get`] yields. Empty when the path is unknown.
    ///
    /// The span is where the diagnostic sat when its publish landed. A caller
    /// painting it against text that has moved since carries it through the
    /// edits, which is what [`PublishedSpan::base_version`] names the start of.
    pub fn spans(&self, path: &Path) -> &[PublishedSpan] {
        self.by_path
            .get(path)
            .map(PathDiagnostics::merged_spans)
            .unwrap_or(&[])
    }

    /// Returns the merged diagnostic list currently stored for `path` across
    /// all servers, or an empty slice when the path is unknown.
    pub fn get(&self, path: &Path) -> &[Diagnostic] {
        self.by_path
            .get(path)
            .map(PathDiagnostics::merged)
            .unwrap_or(&[])
    }

    /// Iterate `path`'s diagnostics paired with the server that published each,
    /// in the same order [`Self::get`] yields.
    ///
    /// A consumer converting an LSP position to a byte offset needs the encoding
    /// the publishing server negotiated, which `get()` discards. Empty when the
    /// path is unknown.
    pub fn attributed(&self, path: &Path) -> impl Iterator<Item = (&str, &Diagnostic)> {
        self.by_path.get(path).into_iter().flat_map(|entry| {
            entry.by_server.iter().flat_map(|(server, publish)| {
                publish
                    .diagnostics
                    .iter()
                    .map(move |diag| (server.as_str(), diag))
            })
        })
    }

    /// Iterate every `(path, server, diagnostic)` triple in the set, attributing
    /// each diagnostic to its publishing server. The workspace-scope diagnostics
    /// picker uses this to convert each position with the right encoding.
    pub fn iter_attributed(&self) -> impl Iterator<Item = (&Path, &str, &Diagnostic)> {
        self.by_path.iter().flat_map(|(path, entry)| {
            entry.by_server.iter().flat_map(move |(server, publish)| {
                publish
                    .diagnostics
                    .iter()
                    .map(move |diag| (path.as_path(), server.as_str(), diag))
            })
        })
    }

    /// Monotonic counter bumped on every [`Self::replace_from_server`]. A
    /// render-side cache keyed off this can skip recomputing while the
    /// diagnostics are unchanged.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Iterate every `(path, diagnostics)` pair currently in the set.
    /// Used by the workspace-scope diagnostics picker.
    pub fn iter(&self) -> impl Iterator<Item = (&Path, &[Diagnostic])> {
        self.by_path
            .iter()
            .map(|(path, entry)| (path.as_path(), entry.merged()))
    }

    /// Returns severity counts plus the worst severity for `path`.
    ///
    /// Read from what the last publish for `path` computed rather than counted
    /// here, so a status bar can ask per frame.
    pub fn summarize(&self, path: &Path) -> DiagnosticSummary {
        self.by_path
            .get(path)
            .map(|entry| entry.summary)
            .unwrap_or_default()
    }
}

/// Move `offset` from the coordinates `patch` starts in into the ones it ends
/// in.
///
/// An offset inside a range an edit replaced has no position of its own in the
/// new text, so it lands at the start of what replaced it. Edits are disjoint
/// and ascending, so one pass accumulating the length each one added or removed
/// answers the rest.
pub fn shift_offset(offset: usize, patch: &stoat_text::patch::Patch<usize>) -> usize {
    let mut shifted = offset as i64;
    for edit in patch.edits() {
        if edit.old.start >= offset {
            break;
        }
        if edit.old.end > offset {
            return edit.new.start;
        }
        shifted += edit.new.len() as i64 - edit.old.len() as i64;
    }
    shifted.max(0) as usize
}

/// Bucket `diagnostics` by severity and name the worst one present.
///
/// A diagnostic with no severity is counted as an error, since a server that
/// omits it is reporting something it could not classify rather than nothing.
fn summarize_diagnostics(diagnostics: &[Diagnostic]) -> DiagnosticSummary {
    let mut summary = DiagnosticSummary::default();
    for diag in diagnostics {
        match diag.severity {
            Some(DiagnosticSeverity::ERROR) => summary.error += 1,
            Some(DiagnosticSeverity::WARNING) => summary.warning += 1,
            Some(DiagnosticSeverity::INFORMATION) => summary.information += 1,
            Some(DiagnosticSeverity::HINT) => summary.hint += 1,
            _ => summary.error += 1,
        }
    }

    summary.worst = if summary.error > 0 {
        Some(DiagnosticSeverity::ERROR)
    } else if summary.warning > 0 {
        Some(DiagnosticSeverity::WARNING)
    } else if summary.information > 0 {
        Some(DiagnosticSeverity::INFORMATION)
    } else if summary.hint > 0 {
        Some(DiagnosticSeverity::HINT)
    } else {
        None
    };
    summary
}

#[cfg(test)]
mod tests {
    /// One empty span per diagnostic, for tests whose subject is the store
    /// rather than where anything sits.
    fn no_spans(count: usize) -> Vec<PublishedSpan> {
        vec![
            PublishedSpan {
                range: 0..0,
                base_version: 0,
            };
            count
        ]
    }

    use super::*;
    use lsp_types::{Position, Range};

    fn diag(severity: DiagnosticSeverity, message: &str) -> Diagnostic {
        Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            severity: Some(severity),
            code: None,
            code_description: None,
            source: None,
            message: message.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    #[test]
    fn replace_for_path_stores_latest() {
        let mut set = DiagnosticSet::new();
        let path = PathBuf::from("/ws/a.rs");
        set.replace_for_path(path.clone(), vec![diag(DiagnosticSeverity::ERROR, "first")]);
        set.replace_for_path(
            path.clone(),
            vec![diag(DiagnosticSeverity::WARNING, "second")],
        );
        assert_eq!(set.get(&path).len(), 1);
        assert_eq!(set.get(&path)[0].message, "second");
    }

    #[test]
    fn version_bumps_on_every_replace() {
        let mut set = DiagnosticSet::new();
        let path = PathBuf::from("/ws/a.rs");
        assert_eq!(set.version(), 0);
        set.replace_for_path(path.clone(), vec![diag(DiagnosticSeverity::ERROR, "x")]);
        assert_eq!(set.version(), 1);
        set.replace_for_path(path.clone(), vec![]);
        assert_eq!(set.version(), 2, "clearing a path is still a change");
    }

    /// The summary is stored at publish rather than counted at read, so it has
    /// to be refreshed on every path that a publish touches, and go away with
    /// the path when the last server clears it.
    #[test]
    fn the_stored_summary_follows_each_publish() {
        let mut set = DiagnosticSet::new();
        let path = PathBuf::from("/ws/a.rs");

        set.replace_for_path(
            path.clone(),
            vec![
                diag(DiagnosticSeverity::ERROR, "x"),
                diag(DiagnosticSeverity::WARNING, "y"),
            ],
        );
        assert_eq!(
            set.summarize(&path),
            DiagnosticSummary {
                error: 1,
                warning: 1,
                worst: Some(DiagnosticSeverity::ERROR),
                ..DiagnosticSummary::default()
            },
        );

        set.replace_for_path(path.clone(), vec![diag(DiagnosticSeverity::HINT, "z")]);
        assert_eq!(
            set.summarize(&path),
            DiagnosticSummary {
                hint: 1,
                worst: Some(DiagnosticSeverity::HINT),
                ..DiagnosticSummary::default()
            },
            "republishing replaces the counts rather than adding to them",
        );

        set.replace_for_path(path.clone(), vec![]);
        assert_eq!(
            set.summarize(&path),
            DiagnosticSummary::default(),
            "clearing the last server leaves nothing to summarize",
        );
    }

    #[test]
    fn replace_with_empty_clears() {
        let mut set = DiagnosticSet::new();
        let path = PathBuf::from("/ws/a.rs");
        set.replace_for_path(path.clone(), vec![diag(DiagnosticSeverity::ERROR, "x")]);
        set.replace_for_path(path.clone(), vec![]);
        assert_eq!(set.get(&path).len(), 0);
    }

    #[test]
    fn summarize_counts_each_severity() {
        let mut set = DiagnosticSet::new();
        let path = PathBuf::from("/ws/a.rs");
        set.replace_for_path(
            path.clone(),
            vec![
                diag(DiagnosticSeverity::ERROR, "e1"),
                diag(DiagnosticSeverity::ERROR, "e2"),
                diag(DiagnosticSeverity::WARNING, "w1"),
                diag(DiagnosticSeverity::INFORMATION, "i1"),
                diag(DiagnosticSeverity::HINT, "h1"),
            ],
        );
        let s = set.summarize(&path);
        assert_eq!(s.error, 2);
        assert_eq!(s.warning, 1);
        assert_eq!(s.information, 1);
        assert_eq!(s.hint, 1);
        assert_eq!(s.worst, Some(DiagnosticSeverity::ERROR));
    }

    #[test]
    fn summarize_worst_is_warning_when_no_errors() {
        let mut set = DiagnosticSet::new();
        let path = PathBuf::from("/ws/a.rs");
        set.replace_for_path(
            path.clone(),
            vec![
                diag(DiagnosticSeverity::WARNING, "w"),
                diag(DiagnosticSeverity::HINT, "h"),
            ],
        );
        let s = set.summarize(&path);
        assert_eq!(s.worst, Some(DiagnosticSeverity::WARNING));
    }

    #[test]
    fn summarize_unknown_path_is_empty() {
        let set = DiagnosticSet::new();
        let s = set.summarize(Path::new("/missing"));
        assert!(s.is_empty());
        assert_eq!(s.worst, None);
    }

    #[test]
    fn diagnostics_from_several_servers_merge() {
        let mut set = DiagnosticSet::new();
        let path = PathBuf::from("/ws/a.rs");
        set.replace_from_server(
            path.clone(),
            "ra".into(),
            vec![diag(DiagnosticSeverity::ERROR, "ra")],
            no_spans(1),
        );
        set.replace_from_server(
            path.clone(),
            "clippy".into(),
            vec![diag(DiagnosticSeverity::WARNING, "clippy")],
            no_spans(1),
        );

        // Merged in server-name order (a BTreeMap keys the contributions).
        let messages: Vec<&str> = set.get(&path).iter().map(|d| d.message.as_str()).collect();
        assert_eq!(messages, ["clippy", "ra"], "both servers contribute");

        // Clearing one server leaves the other's diagnostics.
        set.replace_from_server(path.clone(), "ra".into(), vec![], no_spans(0));
        let after: Vec<&str> = set.get(&path).iter().map(|d| d.message.as_str()).collect();
        assert_eq!(after, ["clippy"]);

        // Clearing the last server drops the path entirely.
        set.replace_from_server(path.clone(), "clippy".into(), vec![], no_spans(0));
        assert!(set.get(&path).is_empty());
    }
}
