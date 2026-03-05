use gpui::TestAppContext;
use lsp_types::{Position, SymbolKind};
use stoat::{
    app_state::{SymbolEntry, SymbolPickerSource},
    test::headless::HeadlessStoat,
};
use stoat_lsp::{BufferDiagnostic, DiagnosticSet, DiagnosticSeverity};
use text::{Bias, Point};

fn make_symbol(name: &str, kind: SymbolKind, line: u32, col: u32) -> SymbolEntry {
    SymbolEntry {
        name: name.to_string(),
        kind,
        range_start: Position::new(line, col),
        file_uri: None,
    }
}

fn test_symbols() -> Vec<SymbolEntry> {
    vec![
        make_symbol("main", SymbolKind::FUNCTION, 0, 3),
        make_symbol("Config", SymbolKind::STRUCT, 3, 0),
        make_symbol("process", SymbolKind::FUNCTION, 7, 3),
        make_symbol("MAX_SIZE", SymbolKind::CONSTANT, 10, 0),
        make_symbol("Handler", SymbolKind::INTERFACE, 12, 0),
    ]
}

fn inject_diagnostics(
    app: &mut HeadlessStoat,
    diags: Vec<(Point, Point, DiagnosticSeverity, &str)>,
) {
    app.with_stoat(|stoat, cx| {
        stoat.update(cx, |s, cx| {
            let buffer_item = s.active_buffer(cx);
            let snapshot = buffer_item.read(cx).buffer_snapshot(cx);
            let mut set = DiagnosticSet::new();
            for (start, end, severity, message) in &diags {
                set.insert(BufferDiagnostic {
                    range: snapshot.anchor_at(*start, Bias::Left)
                        ..snapshot.anchor_at(*end, Bias::Right),
                    severity: *severity,
                    code: None,
                    source: None,
                    message: message.to_string(),
                    server_id: 0,
                });
            }
            buffer_item.update(cx, |item, cx| {
                item.update_diagnostics(0, set, 1, cx);
            });
        });
    });
}

// -- Diagnostic Navigation Tests --

#[gpui::test]
fn diagnostic_next(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("aaa\nbbb\nccc\nddd\neee", cx);

    inject_diagnostics(
        &mut app,
        vec![
            (
                Point::new(1, 0),
                Point::new(1, 3),
                DiagnosticSeverity::Error,
                "error on line 1",
            ),
            (
                Point::new(3, 0),
                Point::new(3, 3),
                DiagnosticSeverity::Warning,
                "warn on line 3",
            ),
        ],
    );

    insta::assert_snapshot!("before-navigation", app.snapshot_active());

    // space l w = goto next diagnostic
    app.type_input("<Space>lw");
    insta::assert_snapshot!("after-first-next", app.snapshot_active());

    app.type_input("<Space>lw");
    insta::assert_snapshot!("after-second-next", app.snapshot_active());

    // Wraps around to first
    app.type_input("<Space>lw");
    insta::assert_snapshot!("after-wrap-next", app.snapshot_active());
}

#[gpui::test]
fn diagnostic_prev(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("aaa\nbbb\nccc\nddd\neee", cx);

    inject_diagnostics(
        &mut app,
        vec![
            (
                Point::new(1, 0),
                Point::new(1, 3),
                DiagnosticSeverity::Error,
                "error on line 1",
            ),
            (
                Point::new(3, 0),
                Point::new(3, 3),
                DiagnosticSeverity::Warning,
                "warn on line 3",
            ),
        ],
    );

    // Move to end first
    app.type_input("G");
    insta::assert_snapshot!("at-end", app.snapshot_active());

    // space l W = goto prev diagnostic
    app.type_input("<Space>lW");
    insta::assert_snapshot!("after-first-prev", app.snapshot_active());

    app.type_input("<Space>lW");
    insta::assert_snapshot!("after-second-prev", app.snapshot_active());
}

#[gpui::test]
fn diagnostic_no_diagnostics(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("clean code", cx);

    insta::assert_snapshot!("before", app.snapshot_active());

    // No diagnostics: cursor should stay in place
    app.type_input("<Space>lw");
    insta::assert_snapshot!("after-next-no-diag", app.snapshot_active());
}

// -- LSP Actions Without Server --

#[gpui::test]
fn hover_no_lsp(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("fn main() {}", cx);
    app.type_input("<Space>li");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn goto_definition_no_lsp(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("fn main() {}", cx);
    // j = goto definition in lsp mode
    app.type_input("<Space>lj");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn code_action_no_lsp(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("fn main() {}", cx);
    app.type_input("<Space>la");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn rename_no_lsp(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("fn main() {}", cx);
    app.type_input("<Space>lr");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn symbol_picker_no_lsp(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("fn main() {}", cx);
    app.type_input("<Space>ls");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn workspace_symbol_picker_no_lsp(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("fn main() {}", cx);
    app.type_input("<Space>lS");
    insta::assert_snapshot!(app.snapshot_active());
}

// -- Symbol Picker Tests --

#[gpui::test]
fn symbol_picker_open(cx: &mut TestAppContext) {
    let text = "fn main() {\n    println!(\"hello\");\n}\nstruct Config {\n    name: String,\n}\n\nfn process() {\n    todo!()\n}\n\nconst MAX_SIZE: usize = 100;\n\ntrait Handler {\n    fn handle(&self);\n}";
    let mut app = HeadlessStoat::new_with_text(text, cx);
    app.inject_symbols(test_symbols(), SymbolPickerSource::Document);
    insta::assert_snapshot!("picker-open", app.snapshot_active());
}

#[gpui::test]
fn symbol_picker_navigate(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("fn main() {}", cx);
    app.inject_symbols(test_symbols(), SymbolPickerSource::Document);

    // Move down twice
    app.type_input("<Down><Down>");
    insta::assert_snapshot!("picker-after-down", app.snapshot_active());

    // Move up once
    app.type_input("<Up>");
    insta::assert_snapshot!("picker-after-up", app.snapshot_active());
}

#[gpui::test]
fn symbol_picker_filter(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("fn main() {}", cx);
    app.inject_symbols(test_symbols(), SymbolPickerSource::Document);

    app.type_input("ma");
    insta::assert_snapshot!("picker-filtered", app.snapshot_active());
}

#[gpui::test]
fn symbol_picker_select(cx: &mut TestAppContext) {
    let text = "fn main() {\n    println!(\"hello\");\n}\nstruct Config {\n    name: String,\n}";
    let mut app = HeadlessStoat::new_with_text(text, cx);
    app.inject_symbols(test_symbols(), SymbolPickerSource::Document);

    // Select second item (Config at line 3)
    app.type_input("<Down><Enter>");
    insta::assert_snapshot!("picker-after-select", app.snapshot_active());
}

#[gpui::test]
fn symbol_picker_dismiss(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("fn main() {}", cx);
    app.inject_symbols(test_symbols(), SymbolPickerSource::Document);

    app.type_input("<Esc>");
    insta::assert_snapshot!("picker-after-dismiss", app.snapshot_active());
}

#[gpui::test]
fn symbol_picker_empty(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("fn main() {}", cx);
    app.inject_symbols(vec![], SymbolPickerSource::Document);
    insta::assert_snapshot!("picker-empty", app.snapshot_active());
}

// -- Workspace Edit Tests --

#[gpui::test]
fn workspace_edit_current_file(cx: &mut TestAppContext) {
    use lsp_types::{Range, TextEdit, Uri, WorkspaceEdit};
    use std::collections::HashMap;

    let mut app = HeadlessStoat::with_fixture("multi-file-diff", cx);

    let file_uri = {
        let alpha_path = app.root().join("alpha.txt");
        format!("file://{}", alpha_path.display())
            .parse::<Uri>()
            .unwrap()
    };

    app.with_stoat(|stoat, cx| {
        stoat.update(cx, |s, cx| {
            let mut changes = HashMap::new();
            changes.insert(
                file_uri,
                vec![TextEdit {
                    range: Range::new(Position::new(0, 6), Position::new(0, 12)),
                    new_text: "LINE_1".to_string(),
                }],
            );

            let edit = WorkspaceEdit {
                changes: Some(changes),
                document_changes: None,
                change_annotations: None,
            };

            let result = s.apply_workspace_edit(&edit, cx);
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), 1);
        });
    });

    insta::assert_snapshot!("edit-current-file", app.snapshot_active());
}
