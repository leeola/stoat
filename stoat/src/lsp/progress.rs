//! Per-server [`WorkDoneProgress`] tracking surfaced in the status bar.
//!
//! Notifications drain into [`LspProgressMap::update`] from
//! [`crate::Stoat::update`]; [`LspProgressMap::current`] returns the
//! freshest in-progress entry for [`crate::render::FrameCtx`] to paint.

use crate::host::LspNotification;
use lsp_types::{ProgressToken, WorkDoneProgress};
use std::collections::HashMap;

/// Single in-progress operation reported by an LSP server.
///
/// `sequence` records the insertion / latest-update order so the most
/// recently touched entry surfaces in the status bar without a real
/// clock dependency.
#[derive(Debug, Clone)]
pub(crate) struct LspProgressEntry {
    pub(crate) server: String,
    pub(crate) title: String,
    pub(crate) message: Option<String>,
    pub(crate) percentage: Option<u32>,
    pub(crate) sequence: u64,
}

/// Work-done progress state keyed by `(server, token)`.
///
/// Keying by server as well as [`ProgressToken`] keeps concurrent language
/// servers from colliding when they reuse the same token, and lets the status
/// bar name which server the freshest operation belongs to.
#[derive(Debug, Default)]
pub(crate) struct LspProgressMap {
    entries: HashMap<(String, ProgressToken), LspProgressEntry>,
    next_sequence: u64,
}

impl LspProgressMap {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Filters and dispatches a single notification. Returns `true`
    /// when the call mutated state (a `Progress` notification was
    /// recognised); other variants are no-ops at this layer.
    pub(crate) fn update(&mut self, server: &str, notification: &LspNotification) -> bool {
        let LspNotification::Progress { token, value } = notification else {
            return false;
        };
        let key = (server.to_string(), token.clone());
        match value {
            WorkDoneProgress::Begin(begin) => {
                let seq = self.bump_sequence();
                self.entries.insert(
                    key,
                    LspProgressEntry {
                        server: server.to_string(),
                        title: begin.title.clone(),
                        message: begin.message.clone(),
                        percentage: begin.percentage,
                        sequence: seq,
                    },
                );
                true
            },
            WorkDoneProgress::Report(report) => {
                let seq = self.bump_sequence();
                if let Some(entry) = self.entries.get_mut(&key) {
                    if let Some(message) = &report.message {
                        entry.message = Some(message.clone());
                    }
                    if let Some(percentage) = report.percentage {
                        entry.percentage = Some(percentage);
                    }
                    entry.sequence = seq;
                } else {
                    // A Report can arrive without a prior Begin when the
                    // editor missed the Begin. The spec allows it, so
                    // synthesize an entry to keep the progress visible.
                    self.entries.insert(
                        key,
                        LspProgressEntry {
                            server: server.to_string(),
                            title: String::new(),
                            message: report.message.clone(),
                            percentage: report.percentage,
                            sequence: seq,
                        },
                    );
                }
                true
            },
            WorkDoneProgress::End(_) => {
                self.entries.remove(&key);
                true
            },
        }
    }

    /// Returns the most recently updated in-progress entry, or `None`
    /// when the map is empty.
    pub(crate) fn current(&self) -> Option<&LspProgressEntry> {
        self.entries.values().max_by_key(|e| e.sequence)
    }

    /// Whether `server` has any work-done operation in flight.
    pub(crate) fn server_busy(&self, server: &str) -> bool {
        self.entries.keys().any(|(name, _)| name == server)
    }

    /// Every in-flight entry across servers, freshest (most recently updated)
    /// first, for the detailed status popout to list.
    pub(crate) fn entries_by_freshness(&self) -> Vec<&LspProgressEntry> {
        let mut entries: Vec<&LspProgressEntry> = self.entries.values().collect();
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.sequence));
        entries
    }

    fn bump_sequence(&mut self) -> u64 {
        let s = self.next_sequence;
        self.next_sequence += 1;
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{
        NumberOrString, WorkDoneProgressBegin, WorkDoneProgressEnd, WorkDoneProgressReport,
    };

    fn token(id: u32) -> ProgressToken {
        NumberOrString::Number(id as i32)
    }

    fn begin(title: &str, percentage: Option<u32>) -> WorkDoneProgress {
        WorkDoneProgress::Begin(WorkDoneProgressBegin {
            title: title.to_owned(),
            cancellable: None,
            message: None,
            percentage,
        })
    }

    fn report(message: Option<&str>, percentage: Option<u32>) -> WorkDoneProgress {
        WorkDoneProgress::Report(WorkDoneProgressReport {
            cancellable: None,
            message: message.map(|s| s.to_owned()),
            percentage,
        })
    }

    fn end() -> WorkDoneProgress {
        WorkDoneProgress::End(WorkDoneProgressEnd { message: None })
    }

    #[test]
    fn begin_inserts_entry() {
        let mut map = LspProgressMap::new();
        let n = LspNotification::Progress {
            token: token(1),
            value: begin("indexing", Some(10)),
        };
        assert!(map.update("ra", &n));
        let e = map.current().unwrap();
        assert_eq!(e.server, "ra");
        assert_eq!(e.title, "indexing");
        assert_eq!(e.percentage, Some(10));
    }

    #[test]
    fn report_updates_existing_entry() {
        let mut map = LspProgressMap::new();
        map.update(
            "ra",
            &LspNotification::Progress {
                token: token(1),
                value: begin("indexing", Some(10)),
            },
        );
        map.update(
            "ra",
            &LspNotification::Progress {
                token: token(1),
                value: report(Some("phase 2"), Some(50)),
            },
        );
        let e = map.current().unwrap();
        assert_eq!(e.title, "indexing");
        assert_eq!(e.message.as_deref(), Some("phase 2"));
        assert_eq!(e.percentage, Some(50));
    }

    #[test]
    fn end_removes_entry() {
        let mut map = LspProgressMap::new();
        map.update(
            "ra",
            &LspNotification::Progress {
                token: token(1),
                value: begin("indexing", None),
            },
        );
        map.update(
            "ra",
            &LspNotification::Progress {
                token: token(1),
                value: end(),
            },
        );
        assert!(map.current().is_none());
    }

    #[test]
    fn current_returns_most_recently_touched_among_multiple() {
        let mut map = LspProgressMap::new();
        map.update(
            "ra",
            &LspNotification::Progress {
                token: token(1),
                value: begin("first", None),
            },
        );
        map.update(
            "ra",
            &LspNotification::Progress {
                token: token(2),
                value: begin("second", None),
            },
        );
        map.update(
            "ra",
            &LspNotification::Progress {
                token: token(1),
                value: report(Some("update"), None),
            },
        );
        let e = map.current().unwrap();
        assert_eq!(e.title, "first");
        assert_eq!(e.message.as_deref(), Some("update"));
    }

    #[test]
    fn report_without_begin_synthesizes_entry() {
        let mut map = LspProgressMap::new();
        map.update(
            "ra",
            &LspNotification::Progress {
                token: token(1),
                value: report(Some("late report"), Some(40)),
            },
        );
        let e = map.current().unwrap();
        assert_eq!(e.title, "");
        assert_eq!(e.message.as_deref(), Some("late report"));
        assert_eq!(e.percentage, Some(40));
    }

    #[test]
    fn non_progress_notifications_are_ignored() {
        let mut map = LspProgressMap::new();
        let n = LspNotification::LogMessage {
            typ: lsp_types::MessageType::INFO,
            message: "hello".into(),
        };
        assert!(!map.update("ra", &n));
        assert!(map.current().is_none());
    }

    #[test]
    fn same_token_from_two_servers_does_not_collide() {
        let mut map = LspProgressMap::new();
        map.update(
            "ra",
            &LspNotification::Progress {
                token: token(1),
                value: begin("indexing rust", None),
            },
        );
        map.update(
            "pyright",
            &LspNotification::Progress {
                token: token(1),
                value: begin("indexing python", None),
            },
        );
        map.update(
            "ra",
            &LspNotification::Progress {
                token: token(1),
                value: end(),
            },
        );
        let e = map.current().unwrap();
        assert_eq!(e.server, "pyright");
        assert_eq!(e.title, "indexing python");
    }
}
