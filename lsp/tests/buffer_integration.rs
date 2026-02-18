//! Tests for BufferItem diagnostic integration.
//!
//! Verifies that diagnostics can be stored in buffers, queried by row,
//! and merged correctly when multiple language servers provide overlapping diagnostics.

use stoat_lsp::{BufferDiagnostic, DiagnosticSet, DiagnosticSeverity};
use text::{Bias, Buffer, BufferId, Point};

fn create_buffer(text: &str) -> Buffer {
    Buffer::new(0, BufferId::new(1).unwrap(), text)
}

fn create_diagnostic(
    buffer: &Buffer,
    start: Point,
    end: Point,
    severity: DiagnosticSeverity,
    server_id: usize,
) -> BufferDiagnostic {
    let snapshot = buffer.snapshot();
    BufferDiagnostic {
        range: snapshot.anchor_at(start, Bias::Left)..snapshot.anchor_at(end, Bias::Right),
        severity,
        code: Some("TEST".to_string()),
        source: Some(format!("server-{server_id}")),
        message: format!("Test diagnostic from server {server_id}"),
        server_id,
    }
}

#[test]
fn diagnostic_set_stores_diagnostics() {
    let buffer = create_buffer("let foo = bar;");
    let snapshot = buffer.snapshot();

    let mut set = DiagnosticSet::new();
    let diag = create_diagnostic(
        &buffer,
        Point::new(0, 10),
        Point::new(0, 13),
        DiagnosticSeverity::Error,
        0,
    );

    set.insert(diag);

    assert_eq!(set.len(), 1);
    let found: Vec<_> = set.diagnostics_for_row(0, &snapshot).collect();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].severity, DiagnosticSeverity::Error);
}

#[test]
fn diagnostic_set_removes_by_server() {
    let buffer = create_buffer("line 0\nline 1\n");
    let snapshot = buffer.snapshot();

    let mut set = DiagnosticSet::new();

    // Add diagnostics from two servers
    set.insert(create_diagnostic(
        &buffer,
        Point::new(0, 0),
        Point::new(0, 4),
        DiagnosticSeverity::Error,
        0,
    ));
    set.insert(create_diagnostic(
        &buffer,
        Point::new(1, 0),
        Point::new(1, 4),
        DiagnosticSeverity::Warning,
        1,
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
fn merge_with_picks_most_severe() {
    let buffer = create_buffer("let foo = bar;");
    let snapshot = buffer.snapshot();

    let mut set1 = DiagnosticSet::new();
    set1.insert(create_diagnostic(
        &buffer,
        Point::new(0, 10),
        Point::new(0, 13),
        DiagnosticSeverity::Warning,
        0,
    ));

    let mut set2 = DiagnosticSet::new();
    set2.insert(create_diagnostic(
        &buffer,
        Point::new(0, 10),
        Point::new(0, 13),
        DiagnosticSeverity::Error,
        1,
    ));

    // Merge - should keep the error (more severe)
    set1.merge_with(&set2, &snapshot);

    let diagnostics: Vec<_> = set1.iter().collect();
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].severity, DiagnosticSeverity::Error);
    assert_eq!(diagnostics[0].server_id, 1);
}

#[test]
fn merge_with_keeps_non_overlapping() {
    let buffer = create_buffer("let foo = bar + baz;");
    let snapshot = buffer.snapshot();

    let mut set1 = DiagnosticSet::new();
    set1.insert(create_diagnostic(
        &buffer,
        Point::new(0, 10),
        Point::new(0, 13),
        DiagnosticSeverity::Error,
        0,
    ));

    let mut set2 = DiagnosticSet::new();
    set2.insert(create_diagnostic(
        &buffer,
        Point::new(0, 16),
        Point::new(0, 19),
        DiagnosticSeverity::Warning,
        1,
    ));

    // Merge - both should be kept (non-overlapping)
    set1.merge_with(&set2, &snapshot);

    assert_eq!(set1.len(), 2);
}

#[test]
fn diagnostics_query_by_row() {
    let buffer = create_buffer("line 0\nline 1\nline 2\n");
    let snapshot = buffer.snapshot();

    let mut set = DiagnosticSet::new();

    // Add diagnostics on different rows
    set.insert(create_diagnostic(
        &buffer,
        Point::new(0, 0),
        Point::new(0, 4),
        DiagnosticSeverity::Error,
        0,
    ));
    set.insert(create_diagnostic(
        &buffer,
        Point::new(1, 0),
        Point::new(1, 4),
        DiagnosticSeverity::Warning,
        0,
    ));
    set.insert(create_diagnostic(
        &buffer,
        Point::new(2, 0),
        Point::new(2, 4),
        DiagnosticSeverity::Hint,
        0,
    ));

    // Query row 1 should only find warning
    let row1_diags: Vec<_> = set.diagnostics_for_row(1, &snapshot).collect();
    assert_eq!(row1_diags.len(), 1);
    assert_eq!(row1_diags[0].severity, DiagnosticSeverity::Warning);

    // Query row 2 should only find hint
    let row2_diags: Vec<_> = set.diagnostics_for_row(2, &snapshot).collect();
    assert_eq!(row2_diags.len(), 1);
    assert_eq!(row2_diags[0].severity, DiagnosticSeverity::Hint);
}

#[test]
fn multiline_diagnostic_appears_on_all_rows() {
    let buffer = create_buffer("fn main() {\n    let x = 1;\n    let y = 2;\n}\n");
    let snapshot = buffer.snapshot();

    let mut set = DiagnosticSet::new();

    // Add diagnostic spanning lines 0-3
    set.insert(create_diagnostic(
        &buffer,
        Point::new(0, 0),
        Point::new(3, 1),
        DiagnosticSeverity::Warning,
        0,
    ));

    // Should appear on all spanned rows
    for row in 0..=3 {
        let diags: Vec<_> = set.diagnostics_for_row(row, &snapshot).collect();
        assert_eq!(diags.len(), 1, "Expected diagnostic on row {row}");
    }

    // Should not appear on row 4
    let row4_diags: Vec<_> = set.diagnostics_for_row(4, &snapshot).collect();
    assert_eq!(row4_diags.len(), 0);
}
