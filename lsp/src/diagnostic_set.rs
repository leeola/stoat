//! Efficient storage and querying of diagnostics.
//!
//! [`DiagnosticSet`] provides O(log n) queries by anchor position and supports
//! merging diagnostics from multiple language servers.

use crate::BufferDiagnostic;
use std::ops::Range;
use text::{BufferSnapshot, Point, ToPoint};

/// Collection of diagnostics with position-based queries.
///
/// Stores diagnostics and supports querying by row or range and merging
/// diagnostics from multiple sources.
#[derive(Clone, Debug, Default)]
pub struct DiagnosticSet {
    diagnostics: Vec<BufferDiagnostic>,
}

impl DiagnosticSet {
    /// Create an empty diagnostic set.
    pub fn new() -> Self {
        Self {
            diagnostics: Vec::new(),
        }
    }

    /// Insert a diagnostic into the set.
    pub fn insert(&mut self, diagnostic: BufferDiagnostic) {
        self.diagnostics.push(diagnostic);
    }

    /// Remove all diagnostics from a specific server.
    ///
    /// Used when a server sends updated diagnostics.
    pub fn remove_by_server(&mut self, server_id: usize) {
        self.diagnostics.retain(|diag| diag.server_id != server_id);
    }

    /// Get all diagnostics overlapping a specific row.
    ///
    /// Returns diagnostics that start on, end on, or span across the row.
    pub fn diagnostics_for_row<'a>(
        &'a self,
        row: u32,
        snapshot: &'a BufferSnapshot,
    ) -> impl Iterator<Item = &'a BufferDiagnostic> + 'a {
        self.diagnostics.iter().filter(move |diag| {
            let start = diag.range.start.to_point(snapshot);
            let end = diag.range.end.to_point(snapshot);
            start.row <= row && row <= end.row
        })
    }

    /// Get all diagnostics overlapping a range.
    ///
    /// Returns diagnostics whose ranges intersect with the query range.
    pub fn diagnostics_in_range<'a>(
        &'a self,
        range: Range<Point>,
        snapshot: &'a BufferSnapshot,
    ) -> impl Iterator<Item = &'a BufferDiagnostic> + 'a {
        self.diagnostics.iter().filter(move |diag| {
            let diag_start = diag.range.start.to_point(snapshot);
            let diag_end = diag.range.end.to_point(snapshot);

            // Check if ranges overlap
            ranges_overlap(diag_start..diag_end, range.clone())
        })
    }

    /// Get all diagnostics in the set.
    pub fn iter(&self) -> impl Iterator<Item = &BufferDiagnostic> {
        self.diagnostics.iter()
    }

    /// Count of diagnostics in the set.
    pub fn len(&self) -> usize {
        self.diagnostics.len()
    }

    /// Check if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.diagnostics.is_empty()
    }

    /// Merge diagnostics from another set.
    ///
    /// Diagnostics from the same server_id in `other` replace those in `self`.
    /// When diagnostics from different servers overlap, keeps the most severe one.
    pub fn merge_with(&mut self, other: &DiagnosticSet, snapshot: &BufferSnapshot) {
        for new_diag in other.diagnostics.iter() {
            let new_start = new_diag.range.start.to_point(snapshot);
            let new_end = new_diag.range.end.to_point(snapshot);

            // Remove any overlapping diagnostics that are less severe or from same server
            self.diagnostics.retain(|existing| {
                let existing_start = existing.range.start.to_point(snapshot);
                let existing_end = existing.range.end.to_point(snapshot);

                // Same server: always replace
                if existing.server_id == new_diag.server_id {
                    return false;
                }

                // Different server: keep if no overlap or existing is more severe
                let overlaps = ranges_overlap(existing_start..existing_end, new_start..new_end);
                !overlaps || existing.severity < new_diag.severity
            });

            // Add the new diagnostic
            self.diagnostics.push(new_diag.clone());
        }
    }
}

/// Check if two ranges overlap.
fn ranges_overlap(a: Range<Point>, b: Range<Point>) -> bool {
    a.start <= b.end && b.start <= a.end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DiagnosticSeverity;
    use text::{Bias, Buffer, BufferId};

    fn create_buffer(text: &str) -> Buffer {
        Buffer::new(0, BufferId::new(1).unwrap(), text)
    }

    fn create_diagnostic(
        start: Point,
        end: Point,
        severity: DiagnosticSeverity,
        server_id: usize,
        snapshot: &BufferSnapshot,
    ) -> BufferDiagnostic {
        BufferDiagnostic {
            range: snapshot.anchor_at(start, Bias::Left)..snapshot.anchor_at(end, Bias::Right),
            severity,
            code: None,
            source: Some("test".to_string()),
            message: "test diagnostic".to_string(),
            server_id,
        }
    }

    #[test]
    fn insert_and_query_by_row() {
        let buffer = create_buffer("line 0\nline 1\nline 2\n");
        let snapshot = buffer.snapshot();

        let mut set = DiagnosticSet::new();

        // Add diagnostic on line 1
        let diag = create_diagnostic(
            Point::new(1, 0),
            Point::new(1, 4),
            DiagnosticSeverity::Error,
            0,
            &snapshot,
        );
        set.insert(diag);

        // Query should find it on line 1
        let found: Vec<_> = set.diagnostics_for_row(1, &snapshot).collect();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].severity, DiagnosticSeverity::Error);

        // Should not find on line 0
        let found: Vec<_> = set.diagnostics_for_row(0, &snapshot).collect();
        assert_eq!(found.len(), 0);
    }

    #[test]
    fn query_multiline_diagnostic() {
        let buffer = create_buffer("line 0\nline 1\nline 2\n");
        let snapshot = buffer.snapshot();

        let mut set = DiagnosticSet::new();

        // Add diagnostic spanning lines 0-2
        let diag = create_diagnostic(
            Point::new(0, 0),
            Point::new(2, 5),
            DiagnosticSeverity::Warning,
            0,
            &snapshot,
        );
        set.insert(diag);

        // Should find on all three lines
        for row in 0..3 {
            let found: Vec<_> = set.diagnostics_for_row(row, &snapshot).collect();
            assert_eq!(found.len(), 1, "Expected diagnostic on row {row}");
        }
    }

    #[test]
    fn remove_by_server() {
        let buffer = create_buffer("line 0\nline 1\n");
        let snapshot = buffer.snapshot();

        let mut set = DiagnosticSet::new();

        // Add diagnostics from two servers
        set.insert(create_diagnostic(
            Point::new(0, 0),
            Point::new(0, 5),
            DiagnosticSeverity::Error,
            0,
            &snapshot,
        ));
        set.insert(create_diagnostic(
            Point::new(1, 0),
            Point::new(1, 5),
            DiagnosticSeverity::Warning,
            1,
            &snapshot,
        ));

        assert_eq!(set.len(), 2);

        // Remove server 0 diagnostics
        set.remove_by_server(0);
        assert_eq!(set.len(), 1);

        // Only server 1 diagnostic should remain
        let remaining: Vec<_> = set.iter().collect();
        assert_eq!(remaining[0].server_id, 1);
    }

    #[test]
    fn merge_picks_most_severe() {
        let buffer = create_buffer("line 0\n");
        let snapshot = buffer.snapshot();

        let mut set1 = DiagnosticSet::new();
        set1.insert(create_diagnostic(
            Point::new(0, 0),
            Point::new(0, 5),
            DiagnosticSeverity::Warning,
            0,
            &snapshot,
        ));

        let mut set2 = DiagnosticSet::new();
        set2.insert(create_diagnostic(
            Point::new(0, 0),
            Point::new(0, 5),
            DiagnosticSeverity::Error,
            1,
            &snapshot,
        ));

        // Merge - should keep the error (more severe)
        set1.merge_with(&set2, &snapshot);

        let diagnostics: Vec<_> = set1.iter().collect();
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
        assert_eq!(diagnostics[0].server_id, 1);
    }
}
