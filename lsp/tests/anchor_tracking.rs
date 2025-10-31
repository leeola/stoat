//! Tests for anchor-based diagnostic position tracking.
//!
//! Verifies that diagnostics automatically adjust their positions when
//! text is inserted or deleted before, within, or after them.

use stoat_lsp::{BufferDiagnostic, DiagnosticSet, DiagnosticSeverity};
use text::{Bias, Buffer, BufferId, Point, ToPoint};

fn create_buffer(text: &str) -> Buffer {
    Buffer::new(0, BufferId::new(1).unwrap(), text)
}

fn create_diagnostic(
    buffer: &Buffer,
    start: Point,
    end: Point,
    severity: DiagnosticSeverity,
) -> BufferDiagnostic {
    let snapshot = buffer.snapshot();
    BufferDiagnostic {
        range: snapshot.anchor_at(start, Bias::Left)..snapshot.anchor_at(end, Bias::Right),
        severity,
        code: Some("TEST".to_string()),
        source: Some("test".to_string()),
        message: "test diagnostic".to_string(),
        server_id: 0,
    }
}

#[test]
fn diagnostic_tracks_through_insert_before() {
    let mut buffer = create_buffer("let foo = bar;");
    let snapshot = buffer.snapshot();

    // Create diagnostic on "bar" at position 10-13
    let diag = create_diagnostic(
        &buffer,
        Point::new(0, 10),
        Point::new(0, 13),
        DiagnosticSeverity::Error,
    );

    // Insert text before the diagnostic
    buffer.edit([(Point::new(0, 0)..Point::new(0, 0), "// comment\n")]);
    let new_snapshot = buffer.snapshot();

    // Diagnostic should have moved to the next line
    let start = diag.range.start.to_point(&new_snapshot);
    let end = diag.range.end.to_point(&new_snapshot);

    assert_eq!(start, Point::new(1, 10), "Start should move to next line");
    assert_eq!(end, Point::new(1, 13), "End should move to next line");
}

#[test]
fn diagnostic_tracks_through_insert_after() {
    let mut buffer = create_buffer("let foo = bar;");

    // Create diagnostic on "foo" at position 4-7
    let diag = create_diagnostic(
        &buffer,
        Point::new(0, 4),
        Point::new(0, 7),
        DiagnosticSeverity::Warning,
    );

    // Insert text after the diagnostic
    buffer.edit([(Point::new(0, 14)..Point::new(0, 14), "\nlet baz = 42;")]);
    let new_snapshot = buffer.snapshot();

    // Diagnostic position should not change
    let start = diag.range.start.to_point(&new_snapshot);
    let end = diag.range.end.to_point(&new_snapshot);

    assert_eq!(start, Point::new(0, 4), "Start should stay same");
    assert_eq!(end, Point::new(0, 7), "End should stay same");
}

#[test]
fn diagnostic_tracks_through_delete_before() {
    let mut buffer = create_buffer("// comment\nlet foo = bar;");

    // Create diagnostic on "bar" at position (1, 10)-(1, 13)
    let diag = create_diagnostic(
        &buffer,
        Point::new(1, 10),
        Point::new(1, 13),
        DiagnosticSeverity::Error,
    );

    // Delete the comment line before the diagnostic
    buffer.edit([(Point::new(0, 0)..Point::new(1, 0), "")]);
    let new_snapshot = buffer.snapshot();

    // Diagnostic should have moved up to line 0
    let start = diag.range.start.to_point(&new_snapshot);
    let end = diag.range.end.to_point(&new_snapshot);

    assert_eq!(start, Point::new(0, 10), "Start should move to line 0");
    assert_eq!(end, Point::new(0, 13), "End should move to line 0");
}

#[test]
fn diagnostic_tracks_through_replace_within() {
    let mut buffer = create_buffer("let foo = bar;");

    // Create diagnostic on "bar" at position 10-13
    let diag = create_diagnostic(
        &buffer,
        Point::new(0, 10),
        Point::new(0, 13),
        DiagnosticSeverity::Error,
    );

    // Replace "bar" with "baz" (same length)
    buffer.edit([(Point::new(0, 10)..Point::new(0, 13), "baz")]);
    let new_snapshot = buffer.snapshot();

    // Diagnostic range should cover the new text
    let start = diag.range.start.to_point(&new_snapshot);
    let end = diag.range.end.to_point(&new_snapshot);

    assert_eq!(start, Point::new(0, 10));
    assert_eq!(end, Point::new(0, 13));
}

#[test]
fn diagnostic_tracks_through_insert_at_start() {
    let mut buffer = create_buffer("let foo = bar;");

    // Create diagnostic on "bar" at position 10-13
    let diag = create_diagnostic(
        &buffer,
        Point::new(0, 10),
        Point::new(0, 13),
        DiagnosticSeverity::Error,
    );

    // Insert at the start of the diagnostic (Bias::Left should keep position)
    buffer.edit([(Point::new(0, 10)..Point::new(0, 10), "x")]);
    let new_snapshot = buffer.snapshot();

    let start = diag.range.start.to_point(&new_snapshot);
    let end = diag.range.end.to_point(&new_snapshot);

    // Start anchor with Bias::Left stays before inserted text
    assert_eq!(start, Point::new(0, 10));
    // End should move
    assert_eq!(end, Point::new(0, 14));
}

#[test]
fn diagnostic_set_multiple_diagnostics_track_independently() {
    let mut buffer = create_buffer("line 0\nline 1\nline 2\n");
    let mut set = DiagnosticSet::new();

    // Add diagnostics on each line
    set.insert(create_diagnostic(
        &buffer,
        Point::new(0, 0),
        Point::new(0, 4),
        DiagnosticSeverity::Error,
    ));
    set.insert(create_diagnostic(
        &buffer,
        Point::new(1, 0),
        Point::new(1, 4),
        DiagnosticSeverity::Warning,
    ));
    set.insert(create_diagnostic(
        &buffer,
        Point::new(2, 0),
        Point::new(2, 4),
        DiagnosticSeverity::Hint,
    ));

    // Insert text before second line
    buffer.edit([(Point::new(0, 6)..Point::new(0, 6), "\nextra line")]);
    let new_snapshot = buffer.snapshot();

    // First diagnostic should be unchanged
    let diags_row_0: Vec<_> = set.diagnostics_for_row(0, &new_snapshot).collect();
    assert_eq!(diags_row_0.len(), 1);

    // Second diagnostic should have moved down
    let diags_row_2: Vec<_> = set.diagnostics_for_row(2, &new_snapshot).collect();
    assert_eq!(diags_row_2.len(), 1);
    assert_eq!(diags_row_2[0].severity, DiagnosticSeverity::Warning);

    // Third diagnostic should have moved down by 2
    let diags_row_3: Vec<_> = set.diagnostics_for_row(3, &new_snapshot).collect();
    assert_eq!(diags_row_3.len(), 1);
    assert_eq!(diags_row_3[0].severity, DiagnosticSeverity::Hint);
}

#[test]
fn multiline_diagnostic_tracks_through_insert() {
    let mut buffer = create_buffer("fn main() {\n    let x = 1;\n}");

    // Create diagnostic spanning entire function
    let diag = create_diagnostic(
        &buffer,
        Point::new(0, 0),
        Point::new(2, 1),
        DiagnosticSeverity::Warning,
    );

    // Insert a line in the middle
    buffer.edit([(Point::new(1, 14)..Point::new(1, 14), "\n    let y = 2;")]);
    let new_snapshot = buffer.snapshot();

    // Diagnostic should still span from start to beyond the new end
    let start = diag.range.start.to_point(&new_snapshot);
    let end = diag.range.end.to_point(&new_snapshot);

    assert_eq!(start, Point::new(0, 0));
    // End should have moved down because of inserted line
    assert_eq!(end.row, 3);
}
