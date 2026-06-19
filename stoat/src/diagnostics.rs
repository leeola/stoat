//! Workspace-wide LSP diagnostic store. Receives notifications via
//! [`crate::host::LspNotification::Diagnostics`] and exposes a
//! per-path summary that the status bar consumes.
//!
//! Single-server today: each path has at most one set of
//! diagnostics. When [`crate::lsp`] gains an `LspManager` (TODO
//! line 176) the key widens to `(PathBuf, LanguageServerId)` so
//! parallel servers (rust-analyzer + clippy + custom linters) can
//! contribute layered, independently-toggleable diagnostics.

use lsp_types::{Diagnostic, DiagnosticSeverity};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

/// Maps each known file path to its current diagnostic list. Replaces
/// in place when the server publishes a new set; an empty `Vec` means
/// the server cleared diagnostics for that file.
#[derive(Debug, Default, Clone)]
pub struct DiagnosticSet {
    by_path: HashMap<PathBuf, Vec<Diagnostic>>,
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

    /// Replaces the diagnostic list for `path`. The server publishes
    /// a full snapshot per `textDocument/publishDiagnostics` call,
    /// so prior entries for the same path are dropped.
    pub fn replace_for_path(&mut self, path: PathBuf, diagnostics: Vec<Diagnostic>) {
        self.version += 1;
        if diagnostics.is_empty() {
            self.by_path.remove(&path);
        } else {
            self.by_path.insert(path, diagnostics);
        }
    }

    /// Returns the diagnostic list currently stored for `path`, or an
    /// empty slice when the path is unknown.
    pub fn get(&self, path: &Path) -> &[Diagnostic] {
        self.by_path.get(path).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Monotonic counter bumped on every [`Self::replace_for_path`]. A
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
            .map(|(path, diags)| (path.as_path(), diags.as_slice()))
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
}
