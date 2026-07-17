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
    path::{Path, PathBuf},
};

/// A path's diagnostics grouped by the server that published them, plus their
/// merged list cached so reads hand out a borrow without re-merging.
#[derive(Debug, Default, Clone)]
struct PathDiagnostics {
    by_server: BTreeMap<String, Vec<Diagnostic>>,
    merged: Vec<Diagnostic>,
}

impl PathDiagnostics {
    /// The merged diagnostic list for this path.
    ///
    /// A single server's slice is its own merge, so it is read straight from
    /// `by_server`; `merged` is materialized only while more than one server
    /// contributes, sparing a full clone on the common single-server publish.
    fn merged(&self) -> &[Diagnostic] {
        if self.by_server.len() == 1 {
            self.by_server.values().next().expect("one server")
        } else {
            &self.merged
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
    pub fn replace_from_server(
        &mut self,
        path: PathBuf,
        server: String,
        diagnostics: Vec<Diagnostic>,
    ) {
        self.version += 1;
        let entry = self.by_path.entry(path.clone()).or_default();
        if diagnostics.is_empty() {
            entry.by_server.remove(&server);
        } else {
            entry.by_server.insert(server, diagnostics);
        }
        if entry.by_server.is_empty() {
            self.by_path.remove(&path);
        } else if entry.by_server.len() > 1 {
            entry.merged = entry.by_server.values().flatten().cloned().collect();
        } else {
            // A single server's slice is read directly by `merged()`, so skip
            // the clone. Drop any stale multi-server copy so it never lingers.
            entry.merged = Vec::new();
        }
    }

    /// Replaces the whole diagnostic list for `path` from a single unnamed
    /// server, for tests that do not exercise multi-server merging.
    #[cfg(test)]
    pub fn replace_for_path(&mut self, path: PathBuf, diagnostics: Vec<Diagnostic>) {
        self.replace_from_server(path, "lsp".to_string(), diagnostics);
    }

    /// Returns the merged diagnostic list currently stored for `path` across
    /// all servers, or an empty slice when the path is unknown.
    pub fn get(&self, path: &Path) -> &[Diagnostic] {
        self.by_path
            .get(path)
            .map(PathDiagnostics::merged)
            .unwrap_or(&[])
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
    pub fn summarize(&self, path: &Path) -> DiagnosticSummary {
        let mut summary = DiagnosticSummary::default();
        for diag in self.get(path) {
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
}

#[cfg(test)]
mod tests {
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
        );
        set.replace_from_server(
            path.clone(),
            "clippy".into(),
            vec![diag(DiagnosticSeverity::WARNING, "clippy")],
        );

        // Merged in server-name order (a BTreeMap keys the contributions).
        let messages: Vec<&str> = set.get(&path).iter().map(|d| d.message.as_str()).collect();
        assert_eq!(messages, ["clippy", "ra"], "both servers contribute");

        // Clearing one server leaves the other's diagnostics.
        set.replace_from_server(path.clone(), "ra".into(), vec![]);
        let after: Vec<&str> = set.get(&path).iter().map(|d| d.message.as_str()).collect();
        assert_eq!(after, ["clippy"]);

        // Clearing the last server drops the path entirely.
        set.replace_from_server(path.clone(), "clippy".into(), vec![]);
        assert!(set.get(&path).is_empty());
    }
}
