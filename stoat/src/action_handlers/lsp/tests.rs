use crate::{
    test_fixture::{
        diag, enable_document_symbols, enable_document_symbols_and_hover, enable_workspace_symbols,
        flat_symbol, install_two_servers, open_buffer, seed,
    },
    test_harness::TestHarness,
};
use ratatui::style::Style;
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use stoat_action::OpenFile;

/// The layout reads the stored width instead of measuring, so it has to be
/// what measuring would have found. The fixture puts the widest line in the
/// middle and splits lines across spans, since taking the first, the last,
/// or one span per line all give a plausible wrong answer.
#[test]
fn a_popup_stores_the_width_measuring_its_lines_would_find() {
    use crate::{
        editor_state::EditorId,
        render::{hover, hover::HoverPopup},
    };

    let span = |text: &str| (text.to_string(), Style::default());
    let lines = vec![
        vec![span("short")],
        vec![span("four"), span("teen chars")],
        vec![span("middling")],
    ];

    let popup = HoverPopup::new(lines.clone(), 0, EditorId::default());
    assert_eq!(popup.max_line_width, 14);
    assert_eq!(
        popup.max_line_width,
        lines
            .iter()
            .map(|line| hover::line_width(line))
            .max()
            .unwrap_or(0),
    );

    assert_eq!(
        HoverPopup::new(Vec::new(), 0, EditorId::default()).max_line_width,
        0,
        "an empty body measures zero rather than panicking"
    );
}

#[test]
fn lsp_for_feature_routes_to_the_capable_server() {
    use crate::{
        host::{LanguageServerFeature, LspHost},
        lsp::registry::ServerSelector,
    };
    use lsp_types::{CompletionOptions, HoverProviderCapability, ServerCapabilities};

    let mut h = TestHarness::with_size(80, 24);
    let hover_server = std::sync::Arc::new(crate::host::FakeLsp::new());
    hover_server.set_capabilities(ServerCapabilities {
        hover_provider: Some(HoverProviderCapability::Simple(true)),
        ..ServerCapabilities::default()
    });
    let completion_server = std::sync::Arc::new(crate::host::FakeLsp::new());
    completion_server.set_capabilities(ServerCapabilities {
        completion_provider: Some(CompletionOptions::default()),
        ..ServerCapabilities::default()
    });
    h.stoat
        .lsp_registry
        .insert("primary".into(), hover_server.clone());
    h.stoat
        .lsp_registry
        .insert("tailwind".into(), completion_server.clone());
    h.stoat.lsp_registry.set_selectors(
        "rust".into(),
        vec![
            ServerSelector::all("primary".into()),
            ServerSelector::all("tailwind".into()),
        ],
    );

    let root = seed(&mut h, &[("a.rs", "fn a() {}\n")]);
    open_buffer(&mut h, root.join("a.rs"));
    let id = h
        .stoat
        .active_workspace()
        .buffers
        .id_for_path(&root.join("a.rs"))
        .expect("buffer open");

    let hover: std::sync::Arc<dyn LspHost> = hover_server.clone();
    let completion: std::sync::Arc<dyn LspHost> = completion_server.clone();
    assert!(
        std::sync::Arc::ptr_eq(
            &crate::lsp::hosts::lsp_for_feature(&h.stoat, id, LanguageServerFeature::Hover),
            &hover,
        ),
        "hover routes to the hover-capable server"
    );
    assert!(
        std::sync::Arc::ptr_eq(
            &crate::lsp::hosts::lsp_for_feature(&h.stoat, id, LanguageServerFeature::Completion),
            &completion,
        ),
        "completion routes to the completion-capable server"
    );
}

#[test]
fn goto_definition_routes_to_a_secondary_when_the_primary_lacks_it() {
    use crate::lsp::registry::ServerSelector;
    use lsp_types::{OneOf, ServerCapabilities};

    let mut h = TestHarness::with_size(80, 24);
    let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
    primary.set_capabilities(ServerCapabilities::default());
    let secondary = std::sync::Arc::new(crate::host::FakeLsp::new());
    secondary.set_capabilities(ServerCapabilities {
        definition_provider: Some(OneOf::Left(true)),
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

    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    secondary.set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
    h.settle();

    assert_eq!(
        cursor_offset(&mut h),
        8,
        "the capable secondary served goto-definition"
    );
}

/// Install two definition-capable fakes routed primary-then-secondary for
/// `rust`, open a three-line buffer, and return the fakes and its path.
fn two_definition_servers(
    h: &mut TestHarness,
) -> (
    std::sync::Arc<crate::host::FakeLsp>,
    std::sync::Arc<crate::host::FakeLsp>,
    PathBuf,
) {
    use crate::lsp::registry::ServerSelector;
    use lsp_types::{OneOf, ServerCapabilities};

    let caps = ServerCapabilities {
        definition_provider: Some(OneOf::Left(true)),
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

    let root = seed(h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(h, path.clone());
    (primary, secondary, path)
}

#[test]
fn goto_definition_merges_distinct_locations_from_two_servers() {
    let mut h = TestHarness::with_size(80, 24);
    let (primary, secondary, path) = two_definition_servers(&mut h);
    let p = path.to_str().unwrap();
    primary.set_definition(p, 0, 0, p, 1, 0);
    secondary.set_definition(p, 0, 0, p, 2, 0);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
    h.settle();

    let picker = h.stoat.location_picker.as_ref().expect("picker open");
    let offsets: Vec<usize> = picker.entries().iter().map(|e| e.offset).collect();
    assert_eq!(offsets, vec![4, 8], "both servers' targets, primary first");
}

#[test]
fn goto_definition_dedups_a_shared_location() {
    let mut h = TestHarness::with_size(80, 24);
    let (primary, secondary, path) = two_definition_servers(&mut h);
    let p = path.to_str().unwrap();
    primary.set_definition(p, 0, 0, p, 2, 0);
    secondary.set_definition(p, 0, 0, p, 2, 0);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
    h.settle();

    assert!(
        h.stoat.location_picker.is_none(),
        "identical answers dedup to a single direct jump"
    );
    assert_eq!(cursor_offset(&mut h), 8);
}

#[test]
fn goto_definition_survives_a_failing_server() {
    let mut h = TestHarness::with_size(80, 24);
    let (primary, secondary, path) = two_definition_servers(&mut h);
    let p = path.to_str().unwrap();
    primary.set_method_error("textDocument/definition", std::io::ErrorKind::Other);
    secondary.set_definition(p, 0, 0, p, 2, 0);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
    h.settle();

    assert!(h.stoat.location_picker.is_none());
    assert_eq!(
        cursor_offset(&mut h),
        8,
        "the healthy server's answer lands despite the peer erroring"
    );
}

#[test]
fn workspace_symbol_submit_uses_the_host_stashed_at_open() {
    use crate::lsp::registry::ServerSelector;
    use lsp_types::{OneOf, ServerCapabilities, SymbolKind};

    let mut h = TestHarness::with_size(80, 24);
    let capable = std::sync::Arc::new(crate::host::FakeLsp::new());
    capable.set_capabilities(ServerCapabilities {
        workspace_symbol_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    });
    let other = std::sync::Arc::new(crate::host::FakeLsp::new());
    other.set_capabilities(ServerCapabilities::default());
    h.stoat
        .lsp_registry
        .insert("capable".into(), capable.clone());
    h.stoat.lsp_registry.insert("other".into(), other.clone());
    h.stoat.lsp_registry.set_selectors(
        "rust".into(),
        vec![
            ServerSelector::all("capable".into()),
            ServerSelector::all("other".into()),
        ],
    );

    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let main = root.join("main.rs");
    open_buffer(&mut h, main.clone());
    capable.add_workspace_symbol(
        "f",
        "foo",
        SymbolKind::FUNCTION,
        main.to_str().unwrap(),
        0,
        3,
    );

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();

    // The capable server drops workspace symbols mid-query, so re-resolving
    // by capability would find no capable server. The query-change re-issue
    // must still target the server stashed at open, resolved by name.
    capable.set_capabilities(ServerCapabilities::default());
    h.type_keys("f");
    h.settle();

    let finder = h
        .stoat
        .symbol_finder
        .as_ref()
        .expect("finder filled from the stashed server");
    let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, vec!["foo"]);
}

#[test]
fn drive_background_applies_pushed_diagnostics() {
    use crate::host::lsp::LspNotification;
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\n")]);
    let path = root.join("main.rs");
    let uri = super::path_to_uri(&path).expect("file uri");
    h.fake_lsp()
        .push_notification(LspNotification::Diagnostics {
            uri,
            diagnostics: vec![diag(0, 0, "boom")],
            version: None,
        });

    // No input event and no settle(): the background pass alone (the
    // redraw-wake path) must drain the pushed notification and apply it.
    h.stoat.drive_background();

    assert_eq!(h.stoat.diagnostics.get(&path), &[diag(0, 0, "boom")]);
}

fn cursor_offset(h: &mut TestHarness) -> usize {
    let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("editor");
    let snapshot = editor.display_map.snapshot();
    let buffer_snapshot = snapshot.buffer_snapshot();
    let sel = editor.selections.newest_anchor();
    stoat_text::cursor_offset(
        buffer_snapshot.rope(),
        buffer_snapshot.resolve_anchor(&sel.tail()),
        buffer_snapshot.resolve_anchor(&sel.head()),
    )
}

#[test]
fn goto_next_diagnostic_jumps_forward() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
    assert_eq!(cursor_offset(&mut h), 4);
}

/// The ends of the list are taken whatever the cursor is near, so a cursor
/// already past the first still reaches back to it.
#[test]
fn goto_first_diagnostic_reaches_back_past_the_cursor() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoLastDiagnostic);
    assert_eq!(cursor_offset(&mut h), 8, "test setup: on the last");

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoFirstDiagnostic);
    assert_eq!(cursor_offset(&mut h), 4);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(
        cursor_offset(&mut h),
        8,
        "the origin went on the jumplist before the landing",
    );
}

/// A buffer with nothing to go to moves nothing, and leaves the jumplist
/// alone rather than recording a jump that went nowhere.
#[test]
fn goto_first_diagnostic_with_none_pushes_no_jump() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    // A known entry to land on. A push by the no-op takes its place as what
    // the jump back reaches.
    h.type_keys("l");
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("l l");
    assert_eq!(cursor_offset(&mut h), 3, "test setup: away from the entry");

    assert_eq!(
        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoFirstDiagnostic),
        crate::app::UpdateEffect::None,
    );
    assert_eq!(cursor_offset(&mut h), 3, "the cursor stayed put");

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(
        cursor_offset(&mut h),
        1,
        "the jump back reaches the earlier entry, so the no-op pushed none",
    );
}

#[test]
fn goto_next_diagnostic_steps_through_each() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
    assert_eq!(cursor_offset(&mut h), 4);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
    assert_eq!(cursor_offset(&mut h), 8);
}

#[test]
fn goto_diagnostic_converts_each_servers_position_with_its_encoding() {
    use crate::host::OffsetEncoding;
    let mut h = TestHarness::with_size(80, 24);
    let ra = std::sync::Arc::new(crate::host::FakeLsp::new());
    ra.set_offset_encoding(OffsetEncoding::Utf8);
    let clippy = std::sync::Arc::new(crate::host::FakeLsp::new());
    clippy.set_offset_encoding(OffsetEncoding::Utf16);
    h.stoat.lsp_registry.insert("ra".into(), ra);
    h.stoat.lsp_registry.insert("clippy".into(), clippy);

    // Line 0 "éx" and line 1 "éy": é is two UTF-8 bytes but one UTF-16 unit,
    // so x sits at byte 2 and y at byte 6.
    let root = seed(&mut h, &[("a.rs", "\u{e9}x\n\u{e9}y\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());

    // ra (utf-8) names x at char 2; clippy (utf-16) names y at char 1.
    h.stoat.diagnostics.replace_from_server(
        path.clone(),
        "ra".into(),
        vec![diag(0, 2, "ra")],
        crate::lsp::util::publish_spans(
            &path,
            &[diag(0, 2, "ra")],
            OffsetEncoding::Utf8,
            &h.stoat.active_workspace().buffers,
        ),
    );
    let path2 = path.clone();
    h.stoat.diagnostics.replace_from_server(
        path,
        "clippy".into(),
        vec![diag(1, 1, "clippy")],
        crate::lsp::util::publish_spans(
            &path2,
            &[diag(1, 1, "clippy")],
            OffsetEncoding::Utf16,
            &h.stoat.active_workspace().buffers,
        ),
    );

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
    assert_eq!(cursor_offset(&mut h), 2, "ra's utf-8 column lands on x");
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
    assert_eq!(
        cursor_offset(&mut h),
        6,
        "clippy's utf-16 column lands on y"
    );
}

#[test]
fn goto_next_diagnostic_no_op_after_last() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(0, 0, "only")]);
    crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 11);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
    assert_eq!(cursor_offset(&mut h), 11);
}

#[test]
fn goto_prev_diagnostic_jumps_backward() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(0, 0, "first"), diag(2, 0, "third")]);
    crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 11);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
    assert_eq!(cursor_offset(&mut h), 8);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
    assert_eq!(cursor_offset(&mut h), 0);
}

#[test]
fn goto_prev_diagnostic_no_op_before_first() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(2, 0, "only")]);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
    assert_eq!(cursor_offset(&mut h), 0);
}

#[test]
fn diagnostics_picker_enter_jumps_focused_cursor() {
    use crate::test_harness::keys;
    use crossterm::event::{Event, KeyCode};
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenDiagnosticsPicker);
    assert!(h.stoat.diagnostics_picker.is_some());

    h.stoat.update(Event::Key(keys::key(KeyCode::Down)));
    h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
    assert!(h.stoat.diagnostics_picker.is_none());
    assert_eq!(cursor_offset(&mut h), 8);
}

#[test]
fn diagnostics_picker_esc_closes_without_jumping() {
    use crate::test_harness::keys;
    use crossterm::event::{Event, KeyCode};
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(1, 0, "first")]);
    let before = cursor_offset(&mut h);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenDiagnosticsPicker);
    h.stoat.update(Event::Key(keys::key(KeyCode::Esc)));
    assert!(h.stoat.diagnostics_picker.is_none());
    assert_eq!(cursor_offset(&mut h), before);
}

#[test]
fn a_diagnostics_select_takes_the_row_its_query_names() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenDiagnosticsPicker);

    type_and_enter_in_one_burst(&mut h, "second");
    assert_eq!(
        cursor_offset(&mut h),
        8,
        "the jump lands on the one diagnostic the query matches"
    );
}

#[test]
fn goto_diagnostic_no_op_with_empty_diagnostics() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\n")]);
    open_buffer(&mut h, root.join("a.rs"));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
    assert_eq!(cursor_offset(&mut h), 0);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
    assert_eq!(cursor_offset(&mut h), 0);
}

#[test]
fn space_l_w_jumps_to_next_diagnostic() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);
    h.type_keys("space l w");
    assert_eq!(cursor_offset(&mut h), 4);
    assert_eq!(h.stoat.focused_mode(), "normal");
}

/// A diagnostic several columns wide, since the shared one-column helper
/// leaves a span a bare block cursor reads the same as.
fn wide_diag(line: u32, col: u32, width: u32) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: lsp_types::Range::new(
            lsp_types::Position::new(line, col),
            lsp_types::Position::new(line, col + width),
        ),
        severity: Some(lsp_types::DiagnosticSeverity::ERROR),
        ..Default::default()
    }
}

/// Stepping to a diagnostic selects its whole span rather than landing a
/// bare cursor where it opens.
#[test]
fn goto_next_diagnostic_selects_the_span() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abcdef\nghijkl\nmnopqr\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![wide_diag(1, 1, 3), wide_diag(2, 2, 3)]);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
    assert_eq!(
        h.selection_spans(),
        vec![(8, 11, false)],
        "the first diagnostic's span, forward",
    );
}

/// Stepping back leaves the span reversed, which puts the cursor on its
/// start so a repeat carries on the way it went.
#[test]
fn goto_prev_diagnostic_selects_reversed() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abcdef\nghijkl\nmnopqr\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![wide_diag(1, 1, 3), wide_diag(2, 2, 3)]);
    crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 20);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoPrevDiagnostic);
    assert_eq!(h.selection_spans(), vec![(16, 19, true)]);
}

/// The motion records where it left, so a jump back returns to the reading
/// position rather than to the diagnostic.
#[test]
fn goto_next_diagnostic_pushes_a_jump() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abcdef\nghijkl\nmnopqr\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![wide_diag(1, 1, 3)]);
    crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 2);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
    assert_eq!(cursor_offset(&mut h), 10, "the span's last cell");

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    assert_eq!(cursor_offset(&mut h), 2);
}

/// Alt-. after a diagnostic jump repeats the jump, not the find before it.
#[test]
fn repeat_last_motion_replays_a_diagnostic_jump() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(1, 0, "first"), diag(2, 0, "second")]);

    h.type_keys("f b");
    assert_eq!(cursor_offset(&mut h), 1, "the find lands first");
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoNextDiagnostic);
    assert_eq!(cursor_offset(&mut h), 4);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RepeatLastMotion);
    assert_eq!(
        cursor_offset(&mut h),
        8,
        "the second diagnostic, where replaying the find would hold",
    );
}

#[test]
fn space_l_shift_w_jumps_to_prev_diagnostic() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "abc\ndef\nghi\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path.clone());
    h.seed_diagnostics(path, vec![diag(0, 0, "first"), diag(2, 0, "third")]);
    crate::action_handlers::movement::jump_to_offset(&mut h.stoat, 11);
    h.type_keys("space l shift-w");
    assert_eq!(cursor_offset(&mut h), 8);
    assert_eq!(h.stoat.focused_mode(), "normal");
}

fn enable_goto_definition(h: &TestHarness) {
    use lsp_types::{OneOf, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        definition_provider: Some(OneOf::Left(true)),
        ..Default::default()
    });
}

fn enable_goto_references(h: &TestHarness) {
    use lsp_types::{OneOf, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        references_provider: Some(OneOf::Left(true)),
        ..Default::default()
    });
}

fn focused_buffer_path(h: &TestHarness) -> PathBuf {
    let ws = h.stoat.active_workspace();
    let pane = ws.panes.pane(ws.panes.focus());
    let crate::pane::View::Editor(eid) = pane.view else {
        panic!("focused pane is not an editor");
    };
    let buffer_id = ws.editors.get(eid).expect("editor").buffer_id;
    ws.buffers
        .path_for(buffer_id)
        .expect("focused buffer has path")
        .to_path_buf()
}

/// Type `text` and press Enter as one burst of input, which the run loop
/// handles before the frame that refilters an open picker.
fn type_and_enter_in_one_burst(h: &mut TestHarness, text: &str) {
    use crate::test_harness::keys;
    use crossterm::event::{Event, KeyCode};
    for code in text.chars().map(KeyCode::Char).chain([KeyCode::Enter]) {
        h.stoat.update(Event::Key(keys::key(code)));
    }
}

#[test]
fn goto_definition_jumps_within_same_file() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_definition(&h);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
    h.settle();
    assert!(
        h.stoat.location_picker.is_none(),
        "single target skips picker"
    );
    assert_eq!(cursor_offset(&mut h), 8);
    assert_eq!(focused_buffer_path(&h), path);
}

fn enable_goto_declaration(h: &TestHarness) {
    use lsp_types::{DeclarationCapability, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        declaration_provider: Some(DeclarationCapability::Simple(true)),
        ..Default::default()
    });
}

#[test]
fn goto_declaration_jumps_within_same_file() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_declaration(&h);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_declaration(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDeclaration);
    h.settle();
    assert_eq!(cursor_offset(&mut h), 8);
    assert_eq!(focused_buffer_path(&h), path);
}

#[test]
fn goto_declaration_unsupported_capability_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_declaration(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 0, 2);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDeclaration);
    h.settle();
    assert_eq!(cursor_offset(&mut h), 0);
    assert!(h.stoat.pending_lsp_jump.is_none());
}

#[test]
fn space_l_shift_j_jumps_to_declaration() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_declaration(&h);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_declaration(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
    h.type_keys("space l J");
    h.settle();
    assert_eq!(cursor_offset(&mut h), 8);
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn goto_definition_multiple_targets_opens_picker() {
    use crate::test_harness::keys;
    use crossterm::event::{Event, KeyCode};
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_definition(&h);
    let root = seed(
        &mut h,
        &[
            ("main.rs", "abc\n"),
            ("lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n"),
        ],
    );
    let main_path = root.join("main.rs");
    let lib_path = root.join("lib.rs");
    open_buffer(&mut h, main_path.clone());
    let lib = lib_path.to_str().unwrap();
    h.fake_lsp().set_definitions(
        main_path.to_str().unwrap(),
        0,
        0,
        &[(lib, 0, 3), (lib, 1, 3), (lib, 2, 3)],
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
    h.settle();

    let picker = h.stoat.location_picker.as_ref().expect("picker open");
    assert_eq!(picker.entries().len(), 3);
    assert_eq!(
        focused_buffer_path(&h),
        main_path,
        "picker does not jump yet"
    );

    h.stoat.update(Event::Key(keys::key(KeyCode::Down)));
    h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
    h.settle();

    assert!(h.stoat.location_picker.is_none());
    assert_eq!(focused_buffer_path(&h), lib_path);
    assert_eq!(cursor_offset(&mut h), 15);
}

#[test]
fn a_location_select_takes_the_row_its_query_names() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_definition(&h);
    let root = seed(
        &mut h,
        &[
            ("main.rs", "abc\n"),
            ("lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n"),
        ],
    );
    let (main_path, lib_path) = (root.join("main.rs"), root.join("lib.rs"));
    open_buffer(&mut h, main_path.clone());
    let lib = lib_path.to_str().unwrap();
    h.fake_lsp().set_definitions(
        main_path.to_str().unwrap(),
        0,
        0,
        &[(lib, 0, 3), (lib, 1, 3), (lib, 2, 3)],
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
    h.settle();

    type_and_enter_in_one_burst(&mut h, "three");
    h.settle();
    assert_eq!(
        (focused_buffer_path(&h), cursor_offset(&mut h)),
        (lib_path, 27),
        "the jump lands on the one target the query matches"
    );
}

#[test]
fn goto_definition_opens_target_file() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_definition(&h);
    let root = seed(
        &mut h,
        &[
            ("main.rs", "abc\n"),
            ("lib.rs", "fn one() {}\nfn two() {}\n"),
        ],
    );
    let main_path = root.join("main.rs");
    let lib_path = root.join("lib.rs");
    open_buffer(&mut h, main_path.clone());
    h.fake_lsp().set_definition(
        main_path.to_str().unwrap(),
        0,
        0,
        lib_path.to_str().unwrap(),
        1,
        3,
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
    h.settle();
    assert_eq!(focused_buffer_path(&h), lib_path);
    assert_eq!(cursor_offset(&mut h), 15);
}

#[test]
fn goto_definition_no_result_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_definition(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
    h.settle();
    assert_eq!(cursor_offset(&mut h), 0);
    assert_eq!(focused_buffer_path(&h), path);
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("lsp: no definition found"),
    );
}

#[test]
fn in_flight_code_action_shows_a_status_segment() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    open_buffer(&mut h, root.join("main.rs"));

    // Hold the response open so the request stays in flight through render.
    h.fake_lsp()
        .set_request_delay("textDocument/codeAction", Duration::from_secs(60));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
    h.settle();
    assert!(
        h.stoat.pending_code_action_request.is_some(),
        "the delayed code-action request stays in flight",
    );

    let buf = h.render_composited();
    let shown = (0..buf.area.height).any(|y| {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        row.replace('─', " ").contains("lsp: code actions...")
    });
    assert!(
        shown,
        "the status bar shows the in-flight code-action segment"
    );
}

#[test]
fn code_action_no_result_reports_none_available() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    open_buffer(&mut h, root.join("main.rs"));

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
    h.settle();

    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("lsp: no code actions available"),
    );
}

#[test]
fn goto_definition_unsupported_capability_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 0, 2);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);
    h.settle();
    assert_eq!(cursor_offset(&mut h), 0);
    assert!(h.stoat.pending_lsp_jump.is_none());
}

#[test]
fn goto_references_multiple_opens_picker() {
    use crate::test_harness::keys;
    use crossterm::event::{Event, KeyCode};
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_references(&h);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    let p = path.to_str().unwrap();
    h.fake_lsp()
        .set_references(p, 0, 0, &[(p, 0, 0), (p, 1, 0), (p, 2, 0)]);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoReferences);
    h.settle();

    let picker = h.stoat.location_picker.as_ref().expect("picker open");
    assert_eq!(picker.entries().len(), 3);

    h.stoat.update(Event::Key(keys::key(KeyCode::Down)));
    h.stoat.update(Event::Key(keys::key(KeyCode::Enter)));
    h.settle();

    assert!(h.stoat.location_picker.is_none());
    assert_eq!(cursor_offset(&mut h), 4);
}

/// The test counts reads before the pumps run, because the picker's preview
/// reads a closed file of its own.
#[test]
fn references_from_two_servers_read_a_closed_file_once_on_the_pool() {
    use crate::host::FakeFsOp;
    use lsp_types::{OneOf, ServerCapabilities};

    let mut h = TestHarness::with_size(80, 24);
    let (primary, secondary) = install_two_servers(
        &mut h,
        ServerCapabilities {
            references_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        },
    );
    let root = seed(
        &mut h,
        &[
            ("main.rs", "abc\ndef\nghi\n"),
            ("lib.rs", "fn one() {}\nfn two() {}\nfn three() {}\n"),
        ],
    );
    let (main_path, lib_path) = (root.join("main.rs"), root.join("lib.rs"));
    open_buffer(&mut h, main_path.clone());
    let (main, lib) = (main_path.to_str().unwrap(), lib_path.to_str().unwrap());
    primary.set_references(
        main,
        0,
        0,
        &[(lib, 1, 3), (main, 2, 0), (lib, 0, 3), (lib, 2, 3)],
    );
    secondary.set_references(
        main,
        0,
        0,
        &[(lib, 2, 3), (main, 1, 0), (lib, 0, 3), (lib, 1, 0)],
    );

    let lib_reads = |h: &TestHarness| {
        h.fake_fs()
            .ops()
            .iter()
            .filter(|op| matches!(op, FakeFsOp::Read { path } if *path == lib_path))
            .count()
    };
    let (reads, hops) = (lib_reads(&h), h.blocking_calls());
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoReferences);
    h.run_until_parked();
    assert_eq!(
        lib_reads(&h) - reads,
        1,
        "one read of lib.rs for six locations"
    );
    assert_eq!(h.blocking_calls() - hops, 1, "the resolve ran on the pool");

    h.settle();
    let picker = h.stoat.location_picker.as_ref().expect("picker open");
    let targets: Vec<(&Path, usize)> = picker
        .entries()
        .iter()
        .map(|entry| (entry.path.as_path(), entry.offset))
        .collect();
    assert_eq!(
        targets,
        [
            (lib_path.as_path(), 15),
            (main_path.as_path(), 8),
            (lib_path.as_path(), 3),
            (lib_path.as_path(), 27),
            (main_path.as_path(), 4),
            (lib_path.as_path(), 12),
        ],
        "the primary's targets in its order, then the secondary's unseen ones"
    );
}

#[test]
fn goto_references_unsupported_uses_code_graph() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    let p = path.to_str().unwrap();
    h.fake_lsp()
        .set_references(p, 0, 0, &[(p, 0, 0), (p, 1, 0), (p, 2, 0)]);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoReferences);
    h.settle();

    assert!(h.stoat.location_picker.is_none(), "LSP path is gated off");
    assert!(h.stoat.pending_lsp_jump.is_none());
    assert_eq!(
        cursor_offset(&mut h),
        0,
        "code-graph fallback no-ops on empty graph"
    );
}

#[test]
fn space_l_j_jumps_to_definition() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_definition(&h);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
    h.type_keys("space l j");
    h.settle();
    assert_eq!(cursor_offset(&mut h), 8);
    assert_eq!(h.stoat.focused_mode(), "normal");
}

fn enable_goto_type_definition(h: &TestHarness) {
    use lsp_types::{ServerCapabilities, TypeDefinitionProviderCapability};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        type_definition_provider: Some(TypeDefinitionProviderCapability::Simple(true)),
        ..Default::default()
    });
}

#[test]
fn goto_type_definition_jumps_within_same_file() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_type_definition(&h);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_type_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
    h.settle();
    assert_eq!(cursor_offset(&mut h), 8);
    assert_eq!(focused_buffer_path(&h), path);
}

#[test]
fn goto_type_definition_opens_target_file() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_type_definition(&h);
    let root = seed(
        &mut h,
        &[
            ("main.rs", "abc\n"),
            ("types.rs", "struct One;\nstruct Two;\n"),
        ],
    );
    let main_path = root.join("main.rs");
    let types_path = root.join("types.rs");
    open_buffer(&mut h, main_path.clone());
    h.fake_lsp().set_type_definition(
        main_path.to_str().unwrap(),
        0,
        0,
        types_path.to_str().unwrap(),
        1,
        7,
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
    h.settle();
    assert_eq!(focused_buffer_path(&h), types_path);
    assert_eq!(cursor_offset(&mut h), 19);
}

#[test]
fn goto_type_definition_no_result_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_type_definition(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
    h.settle();
    assert_eq!(cursor_offset(&mut h), 0);
    assert_eq!(focused_buffer_path(&h), path);
}

#[test]
fn goto_type_definition_unsupported_capability_is_noop() {
    use lsp_types::{OneOf, ServerCapabilities};
    let mut h = TestHarness::with_size(80, 24);
    h.fake_lsp().set_capabilities(ServerCapabilities {
        definition_provider: Some(OneOf::Left(true)),
        ..Default::default()
    });
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_type_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 0, 2);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoTypeDefinition);
    h.settle();
    assert_eq!(cursor_offset(&mut h), 0);
    assert!(h.stoat.pending_lsp_jump.is_none());
}

#[test]
fn space_l_k_jumps_to_type_definition() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_type_definition(&h);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_type_definition(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
    h.type_keys("space l k");
    h.settle();
    assert_eq!(cursor_offset(&mut h), 8);
    assert_eq!(h.stoat.focused_mode(), "normal");
}

fn enable_goto_implementation(h: &TestHarness) {
    use lsp_types::{ImplementationProviderCapability, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        implementation_provider: Some(ImplementationProviderCapability::Simple(true)),
        ..Default::default()
    });
}

#[test]
fn goto_implementation_jumps_within_same_file() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_implementation(&h);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
    h.settle();
    assert_eq!(cursor_offset(&mut h), 8);
    assert_eq!(focused_buffer_path(&h), path);
}

#[test]
fn goto_implementation_opens_target_file() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_implementation(&h);
    let root = seed(
        &mut h,
        &[
            ("trait.rs", "trait X {}\n"),
            ("impl.rs", "impl X for One {}\nimpl X for Two {}\n"),
        ],
    );
    let trait_path = root.join("trait.rs");
    let impl_path = root.join("impl.rs");
    open_buffer(&mut h, trait_path.clone());
    h.fake_lsp().set_implementation(
        trait_path.to_str().unwrap(),
        0,
        0,
        impl_path.to_str().unwrap(),
        1,
        5,
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
    h.settle();
    assert_eq!(focused_buffer_path(&h), impl_path);
    assert_eq!(cursor_offset(&mut h), 23);
}

#[test]
fn goto_implementation_no_result_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_implementation(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
    h.settle();
    assert_eq!(cursor_offset(&mut h), 0);
    assert_eq!(focused_buffer_path(&h), path);
}

#[test]
fn goto_implementation_unsupported_capability_is_noop() {
    use lsp_types::{OneOf, ServerCapabilities};
    let mut h = TestHarness::with_size(80, 24);
    h.fake_lsp().set_capabilities(ServerCapabilities {
        definition_provider: Some(OneOf::Left(true)),
        ..Default::default()
    });
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 0, 2);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoImplementation);
    h.settle();
    assert_eq!(cursor_offset(&mut h), 0);
    assert!(h.stoat.pending_lsp_jump.is_none());
}

#[test]
fn space_l_t_jumps_to_implementation() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_implementation(&h);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
    h.type_keys("space l t");
    h.settle();
    assert_eq!(cursor_offset(&mut h), 8);
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn g_s_jumps_to_implementation() {
    let mut h = TestHarness::with_size(80, 24);
    enable_goto_implementation(&h);
    let root = seed(&mut h, &[("main.rs", "abc\ndef\nghi\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_implementation(path.to_str().unwrap(), 0, 0, path.to_str().unwrap(), 2, 0);
    h.type_keys("g s");
    h.settle();
    assert_eq!(cursor_offset(&mut h), 8);
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn goto_definition_without_a_server_reports_no_server() {
    let mut h = TestHarness::with_size(80, 24);
    h.allow_host_swap();
    h.stoat
        .set_lsp_host(std::sync::Arc::new(crate::host::NoopLsp));
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    open_buffer(&mut h, root.join("main.rs"));

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::GotoDefinition);

    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("lsp: no language server running"),
    );
}

#[test]
fn unsupported_feature_with_two_servers_reports_does_not_support() {
    use lsp_types::ServerCapabilities;
    let mut h = TestHarness::with_size(80, 24);
    // Two servers run but neither advertises hover, so the sole-host probe is
    // a noop. The report must still name the missing capability rather than
    // claim the server is still starting.
    let _ = install_two_servers(&mut h, ServerCapabilities::default());
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    open_buffer(&mut h, root.join("main.rs"));

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Hover);

    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("lsp: server does not support hover"),
    );
}

fn enable_code_action(h: &TestHarness) {
    use lsp_types::{CodeActionProviderCapability, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        ..Default::default()
    });
}

#[allow(clippy::mutable_key_type)]
fn direct_action(
    title: &str,
    file: &str,
    line: u32,
    col: u32,
    text: &str,
) -> lsp_types::CodeActionOrCommand {
    use lsp_types::{
        CodeAction, CodeActionOrCommand, Position, Range, TextEdit, Uri, WorkspaceEdit,
    };
    use std::{collections::HashMap, str::FromStr};
    let uri = Uri::from_str(&format!("file://{file}")).expect("uri");
    let edit = TextEdit {
        range: Range::new(Position::new(line, col), Position::new(line, col)),
        new_text: text.to_string(),
    };
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri, vec![edit]);
    let workspace_edit = WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    };
    CodeActionOrCommand::CodeAction(CodeAction {
        title: title.to_string(),
        kind: None,
        diagnostics: None,
        edit: Some(workspace_edit),
        command: None,
        is_preferred: None,
        disabled: None,
        data: None,
    })
}

fn unresolved_action(title: &str) -> lsp_types::CodeActionOrCommand {
    use lsp_types::{CodeAction, CodeActionOrCommand};
    CodeActionOrCommand::CodeAction(CodeAction {
        title: title.to_string(),
        kind: None,
        diagnostics: None,
        edit: None,
        command: None,
        is_preferred: None,
        disabled: None,
        data: Some(serde_json::Value::Null),
    })
}

fn command_only_action(title: &str) -> lsp_types::CodeActionOrCommand {
    use lsp_types::{CodeActionOrCommand, Command};
    CodeActionOrCommand::Command(Command {
        title: title.to_string(),
        command: "noop".to_string(),
        arguments: None,
    })
}

fn buffer_text(h: &TestHarness, path: &Path) -> String {
    let buffer_id = h
        .stoat
        .active_workspace()
        .buffers
        .id_for_path(path)
        .expect("buffer for path");
    let buffer = h
        .stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer");
    let guard = buffer.read().expect("buffer lock");
    guard.rope().to_string()
}

#[test]
fn code_action_unsupported_capability_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_code_actions(
        path.to_str().unwrap(),
        vec![direct_action("X", path.to_str().unwrap(), 0, 0, "X")],
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
    h.settle();
    assert!(h.stoat.pending_code_action_picker.is_none());
    assert!(h.stoat.pending_code_action_request.is_none());
}

#[test]
fn code_action_no_response_clears_picker() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    open_buffer(&mut h, root.join("main.rs"));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
    h.settle();
    assert!(h.stoat.pending_code_action_picker.is_none());
    assert!(h.stoat.pending_code_action_request.is_none());
}

#[test]
fn code_action_populates_picker_with_titles() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_code_actions(
        path.to_str().unwrap(),
        vec![
            direct_action("Add import", path.to_str().unwrap(), 0, 0, "use a;\n"),
            direct_action("Inline variable", path.to_str().unwrap(), 0, 0, ""),
        ],
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
    h.settle();
    let picker = h
        .stoat
        .pending_code_action_picker
        .as_ref()
        .expect("picker open");
    let titles: Vec<&str> = picker.entries.iter().map(|e| e.title()).collect();
    assert_eq!(titles, vec!["Add import", "Inline variable"]);
}

#[test]
fn code_action_retains_command_only_entries() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_code_actions(
        path.to_str().unwrap(),
        vec![
            command_only_action("Run command"),
            direct_action("Real edit", path.to_str().unwrap(), 0, 0, "X"),
        ],
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
    h.settle();
    let picker = h
        .stoat
        .pending_code_action_picker
        .as_ref()
        .expect("picker open");
    let titles: Vec<&str> = picker.entries.iter().map(|e| e.title()).collect();
    assert_eq!(titles, vec!["Run command", "Real edit"]);
}

#[test]
fn code_action_pick_command_dispatches_execute_command() {
    use lsp_types::{CodeActionOrCommand, Command};
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_code_actions(
        path.to_str().unwrap(),
        vec![CodeActionOrCommand::Command(Command {
            title: "Apply import".to_string(),
            command: "rust-analyzer.applyImport".to_string(),
            arguments: Some(vec![serde_json::json!({"target": "std::io"})]),
        })],
    );
    h.type_keys("space l a");
    h.settle();
    h.type_keys("1");
    h.settle();
    assert!(h.stoat.pending_code_action_picker.is_none());
    let observed = h.fake_lsp().observed_executed_commands();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].command, "rust-analyzer.applyImport");
    assert_eq!(
        observed[0].arguments,
        vec![serde_json::json!({"target": "std::io"})]
    );
}

#[test]
fn code_action_navigates_with_jk_and_picks_with_enter() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    let actions: Vec<lsp_types::CodeActionOrCommand> = (0..12)
        .map(|i| {
            direct_action(
                &format!("Action {i}"),
                path.to_str().unwrap(),
                0,
                0,
                &format!("// {i}\n"),
            )
        })
        .collect();
    h.fake_lsp()
        .set_code_actions(path.to_str().unwrap(), actions);
    h.type_keys("space l a");
    h.settle();
    for _ in 0..11 {
        h.type_keys("j");
    }
    let picker = h.stoat.pending_code_action_picker.as_ref().expect("picker");
    assert_eq!(picker.selected_idx, 11);

    h.type_keys("enter");
    assert!(h.stoat.pending_code_action_picker.is_none());
    assert_eq!(buffer_text(&h, &path), "// 11\nabc\n");
}

/// The guard reads keys ahead of the keymap and closes on anything it does
/// not name, so an arrow it does not name dismisses the picker.
#[test]
fn code_action_navigates_with_the_arrows() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    let actions: Vec<lsp_types::CodeActionOrCommand> = (0..3)
        .map(|i| {
            direct_action(
                &format!("Action {i}"),
                path.to_str().unwrap(),
                0,
                0,
                &format!("// {i}\n"),
            )
        })
        .collect();
    h.fake_lsp()
        .set_code_actions(path.to_str().unwrap(), actions);
    h.type_keys("space l a");
    h.settle();

    let selected = |h: &TestHarness| {
        h.stoat
            .pending_code_action_picker
            .as_ref()
            .expect("the arrow steps the picker rather than closing it")
            .selected_idx
    };

    h.type_keys("down");
    h.type_keys("down");
    let after_down = selected(&h);
    h.type_keys("up");

    assert_eq!(
        (after_down, selected(&h)),
        (2, 1),
        "down walks toward the last entry and up walks back"
    );
}

#[test]
fn code_action_pick_one_applies_edit() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_code_actions(
        path.to_str().unwrap(),
        vec![direct_action(
            "Insert prefix",
            path.to_str().unwrap(),
            0,
            0,
            "// hi\n",
        )],
    );
    h.type_keys("space l a");
    h.settle();
    h.type_keys("1");
    h.settle();
    assert!(h.stoat.pending_code_action_picker.is_none());
    assert_eq!(buffer_text(&h, &path), "// hi\nabc\n");
}

#[test]
fn code_action_resolve_path_applies_resolved_edit() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_code_actions(path.to_str().unwrap(), vec![unresolved_action("Refactor")]);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
    h.settle();
    assert!(h.stoat.pending_code_action_picker.is_some());
    crate::action_handlers::lsp::pick_code_action(&mut h.stoat, 0);
    h.settle();
    assert!(h.stoat.pending_code_action_picker.is_none());
    assert!(!h.stoat.pending_code_action_resolve.is_pending());
}

#[test]
fn code_action_resolve_routes_back_to_the_producing_server() {
    use crate::lsp::registry::ServerSelector;
    use lsp_types::{
        request::CodeActionResolveRequest, CodeActionProviderCapability, ServerCapabilities,
    };

    let mut h = TestHarness::with_size(80, 24);
    let primary = std::sync::Arc::new(crate::host::FakeLsp::new());
    primary.set_capabilities(ServerCapabilities::default());
    let producer = std::sync::Arc::new(crate::host::FakeLsp::new());
    producer.set_capabilities(ServerCapabilities {
        code_action_provider: Some(CodeActionProviderCapability::Simple(true)),
        ..ServerCapabilities::default()
    });
    h.stoat
        .lsp_registry
        .insert("primary".into(), primary.clone());
    h.stoat
        .lsp_registry
        .insert("producer".into(), producer.clone());
    h.stoat.lsp_registry.set_selectors(
        "rust".into(),
        vec![
            ServerSelector::all("primary".into()),
            ServerSelector::all("producer".into()),
        ],
    );

    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    producer.set_code_actions(path.to_str().unwrap(), vec![unresolved_action("Refactor")]);
    producer.set_pending_mode::<CodeActionResolveRequest>(true);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
    h.settle();
    assert!(
        h.stoat.pending_code_action_picker.is_some(),
        "the producing server served the code action"
    );

    crate::action_handlers::lsp::pick_code_action(&mut h.stoat, 0);
    h.settle();

    assert_eq!(
        producer.pending_count("codeAction/resolve"),
        1,
        "resolve routes back to the producing server"
    );
    assert_eq!(
        primary.pending_count("codeAction/resolve"),
        0,
        "resolve does not go to the primary that never saw the action"
    );
}

#[test]
fn code_action_escape_dismisses_picker() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_code_actions(
        path.to_str().unwrap(),
        vec![direct_action("X", path.to_str().unwrap(), 0, 0, "X")],
    );
    h.type_keys("space l a");
    h.settle();
    assert!(h.stoat.pending_code_action_picker.is_some());
    h.type_keys("escape");
    assert!(h.stoat.pending_code_action_picker.is_none());
}

#[test]
fn space_l_a_triggers_code_action() {
    let mut h = TestHarness::with_size(80, 24);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_code_actions(
        path.to_str().unwrap(),
        vec![direct_action("X", path.to_str().unwrap(), 0, 0, "X")],
    );
    h.type_keys("space l a");
    h.settle();
    assert!(h.stoat.pending_code_action_picker.is_some());
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn snapshot_code_action_picker() {
    let mut h = TestHarness::with_size(40, 12);
    enable_code_action(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_code_actions(
        path.to_str().unwrap(),
        vec![
            direct_action("Add import", path.to_str().unwrap(), 0, 0, "X"),
            direct_action("Inline", path.to_str().unwrap(), 0, 0, "X"),
        ],
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::CodeAction);
    h.settle();
    h.assert_snapshot("snapshot_code_action_picker");
}

fn enable_rename(h: &TestHarness) {
    use lsp_types::{OneOf, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        rename_provider: Some(OneOf::Left(true)),
        ..Default::default()
    });
}

#[allow(clippy::mutable_key_type)]
fn rename_workspace_edit(
    file: &str,
    line: u32,
    col: u32,
    len: u32,
    new: &str,
) -> lsp_types::WorkspaceEdit {
    use lsp_types::{Position as LspPosition, Range as LspRange, TextEdit, Uri, WorkspaceEdit};
    use std::{collections::HashMap, str::FromStr};
    let uri = Uri::from_str(&format!("file://{file}")).expect("uri");
    let edit = TextEdit {
        range: LspRange::new(
            LspPosition::new(line, col),
            LspPosition::new(line, col + len),
        ),
        new_text: new.to_string(),
    };
    let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
    changes.insert(uri, vec![edit]);
    WorkspaceEdit {
        changes: Some(changes),
        document_changes: None,
        change_annotations: None,
    }
}

#[test]
fn rename_unsupported_capability_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    open_buffer(&mut h, root.join("main.rs"));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
    h.settle();
    assert!(h.stoat.rename_input.is_none());
    assert!(h.stoat.pending_prepare_rename.is_none());
}

#[test]
fn rename_no_response_does_not_open_modal() {
    let mut h = TestHarness::with_size(80, 24);
    enable_rename(&h);
    let root = seed(&mut h, &[("main.rs", "abc\n")]);
    open_buffer(&mut h, root.join("main.rs"));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
    h.settle();
    assert!(h.stoat.rename_input.is_none());
}

#[test]
fn rename_range_response_seeds_placeholder_from_rope() {
    use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
    let mut h = TestHarness::with_size(80, 24);
    enable_rename(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_prepare_rename(
        path.to_str().unwrap(),
        0,
        0,
        PrepareRenameResponse::Range(LspRange::new(
            LspPosition::new(0, 3),
            LspPosition::new(0, 6),
        )),
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
    h.settle();
    let modal = h.stoat.rename_input.as_ref().expect("modal open");
    assert_eq!(modal.input.text(h.stoat.active_workspace()), "foo");
    assert_eq!(h.stoat.focused_mode(), "insert");
}

/// A range whose endpoints arrive out of order seeds an empty
/// placeholder rather than taking the editor down.
///
/// A server that has not seen the latest edits answers over a document
/// state the buffer no longer holds, and a line past the buffer converts
/// to the rope's end. Either way the start offset lands above the end
/// offset, and the rope slice then subtracts the chunk start from the
/// smaller end offset and underflows.
///
/// The fixture spans several rope chunks because the arithmetic only
/// underflows once the two offsets fall in different ones.
#[test]
fn rename_range_response_with_an_inverted_range_seeds_nothing() {
    use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
    let mut h = TestHarness::with_size(80, 24);
    enable_rename(&h);
    let root = seed(&mut h, &[("main.rs", &"fn foo() {}\n".repeat(20))]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_prepare_rename(
        path.to_str().unwrap(),
        0,
        0,
        PrepareRenameResponse::Range(LspRange::new(
            LspPosition::new(19, 0),
            LspPosition::new(0, 6),
        )),
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
    h.settle();
    let modal = h.stoat.rename_input.as_ref().expect("modal open");
    assert_eq!(modal.input.text(h.stoat.active_workspace()), "");
}

#[test]
fn rename_with_placeholder_form() {
    use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
    let mut h = TestHarness::with_size(80, 24);
    enable_rename(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_prepare_rename(
        path.to_str().unwrap(),
        0,
        0,
        PrepareRenameResponse::RangeWithPlaceholder {
            range: LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 6)),
            placeholder: "Renamed".to_string(),
        },
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
    h.settle();
    let modal = h.stoat.rename_input.as_ref().expect("modal open");
    assert_eq!(modal.input.text(h.stoat.active_workspace()), "Renamed");
}

#[test]
fn rename_submit_applies_workspace_edit() {
    use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
    let mut h = TestHarness::with_size(80, 24);
    enable_rename(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_prepare_rename(
        path.to_str().unwrap(),
        0,
        0,
        PrepareRenameResponse::Range(LspRange::new(
            LspPosition::new(0, 3),
            LspPosition::new(0, 6),
        )),
    );
    h.fake_lsp().set_rename(
        path.to_str().unwrap(),
        0,
        0,
        rename_workspace_edit(path.to_str().unwrap(), 0, 3, 3, "bar"),
    );
    h.type_keys("space l r");
    h.settle();
    assert!(h.stoat.rename_input.is_some());
    crate::action_handlers::lsp::rename_input_submit(&mut h.stoat);
    h.settle();
    assert!(h.stoat.rename_input.is_none());
    assert_eq!(buffer_text(&h, &path), "fn bar() {}\n");
}

/// Move the focused buffer on without going through a keypress, which would
/// also pump the reply and close the window the guard exists for.
fn edit_focused_buffer_directly(h: &mut TestHarness, text: &str) {
    let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
    h.stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("poisoned")
        .edit(0..0, text);
}

#[test]
fn a_rename_reply_is_dropped_when_the_buffer_moved_under_it() {
    use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
    let mut h = TestHarness::with_size(80, 24);
    enable_rename(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_prepare_rename(
        path.to_str().unwrap(),
        0,
        0,
        PrepareRenameResponse::Range(LspRange::new(
            LspPosition::new(0, 3),
            LspPosition::new(0, 6),
        )),
    );
    h.fake_lsp().set_rename(
        path.to_str().unwrap(),
        0,
        0,
        rename_workspace_edit(path.to_str().unwrap(), 0, 3, 3, "bar"),
    );
    h.type_keys("space l r");
    h.settle();

    crate::action_handlers::lsp::rename_input_submit(&mut h.stoat);
    edit_focused_buffer_directly(&mut h, "x");
    h.settle();

    assert_eq!(buffer_text(&h, &path), "xfn foo() {}\n");
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("lsp: rename skipped, buffer changed"),
    );
}

#[test]
fn rename_cancel_discards_modal() {
    use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
    let mut h = TestHarness::with_size(80, 24);
    enable_rename(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_prepare_rename(
        path.to_str().unwrap(),
        0,
        0,
        PrepareRenameResponse::Range(LspRange::new(
            LspPosition::new(0, 3),
            LspPosition::new(0, 6),
        )),
    );
    h.type_keys("space l r");
    h.settle();
    assert!(h.stoat.rename_input.is_some());
    let cancelled = crate::action_handlers::lsp::rename_input_cancel(&mut h.stoat);
    assert!(cancelled);
    assert!(h.stoat.rename_input.is_none());
    assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn space_l_r_triggers_rename() {
    use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
    let mut h = TestHarness::with_size(80, 24);
    enable_rename(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_prepare_rename(
        path.to_str().unwrap(),
        0,
        0,
        PrepareRenameResponse::Range(LspRange::new(
            LspPosition::new(0, 3),
            LspPosition::new(0, 6),
        )),
    );
    h.type_keys("space l r");
    h.settle();
    let modal = h.stoat.rename_input.as_ref().expect("modal open");
    assert_eq!(modal.input.text(h.stoat.active_workspace()), "foo");
    assert_eq!(h.stoat.focused_mode(), "insert");
}

#[test]
fn cursor_keys_move_inside_the_rename_input() {
    let mut h = open_rename_input();
    let document_cursor = h.stoat.focused_cursor_pos();

    h.type_keys("left");
    h.type_text("X");
    h.type_keys("home");
    h.type_text("Y");
    h.type_keys("end");
    h.type_text("Z");
    h.type_keys("up down");

    assert_eq!(rename_text(&h), "Yfoo_baXrZ");
    assert_eq!(
        h.stoat.focused_cursor_pos(),
        document_cursor,
        "the document cursor stays"
    );
}

/// `foo_bar` is one word because `_` is a word character, which is why a bare
/// `Ctrl-w` empties the prefill.
#[test]
fn rename_input_takes_the_readline_kill_keys() {
    for (keys, expected) in [
        ("ctrl-w", ""),
        ("alt-backspace", ""),
        ("ctrl-u", ""),
        ("left left left ctrl-w", "bar"),
        ("left left left ctrl-u", "bar"),
        ("left left left ctrl-k", "foo_"),
        ("home alt-d", ""),
        ("home alt-delete", ""),
    ] {
        let mut h = open_rename_input();
        h.type_keys(keys);
        assert_eq!(rename_text(&h), expected, "{keys}");
    }
}

/// The rename input over `foo_bar` in `fn foo_bar() {}`, prefilled with the
/// name, its cursor at the end, in insert mode.
fn open_rename_input() -> TestHarness {
    use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
    let mut h = TestHarness::with_size(80, 24);
    enable_rename(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo_bar() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_prepare_rename(
        path.to_str().unwrap(),
        0,
        0,
        PrepareRenameResponse::Range(LspRange::new(
            LspPosition::new(0, 3),
            LspPosition::new(0, 10),
        )),
    );
    h.type_keys("space l r");
    h.settle();
    h
}

fn rename_text(h: &TestHarness) -> String {
    h.stoat
        .rename_input
        .as_ref()
        .expect("modal open")
        .input
        .text(h.stoat.active_workspace())
}

#[test]
fn snapshot_rename_input_modal() {
    use lsp_types::{Position as LspPosition, PrepareRenameResponse, Range as LspRange};
    let mut h = TestHarness::with_size(40, 12);
    enable_rename(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_prepare_rename(
        path.to_str().unwrap(),
        0,
        0,
        PrepareRenameResponse::Range(LspRange::new(
            LspPosition::new(0, 3),
            LspPosition::new(0, 6),
        )),
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::RenameSymbol);
    h.settle();
    h.assert_snapshot("snapshot_rename_input");
}

use lsp_types::{DocumentSymbol, DocumentSymbolResponse};

#[test]
fn symbol_picker_unsupported_capability_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        DocumentSymbolResponse::Flat(vec![flat_symbol("foo", path.to_str().unwrap(), 0, 3)]),
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();
    assert!(h.stoat.symbol_finder.is_none());
    assert!(h.stoat.pending_symbol_picker_request.is_none());
}

#[test]
fn symbol_picker_no_response_keeps_modal_open() {
    let mut h = TestHarness::with_size(80, 24);
    enable_document_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    open_buffer(&mut h, root.join("main.rs"));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();
    let finder = h.stoat.symbol_finder.as_ref().expect("modal stays open");
    assert!(finder.entries.is_empty(), "no symbols yields an empty list");
}

#[test]
fn symbol_picker_populates_with_flat_symbols() {
    let mut h = TestHarness::with_size(80, 24);
    enable_document_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\nfn bar() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        DocumentSymbolResponse::Flat(vec![
            flat_symbol("foo", path.to_str().unwrap(), 0, 3),
            flat_symbol("bar", path.to_str().unwrap(), 1, 3),
        ]),
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();
    let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
    let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, vec!["foo", "bar"]);
}

#[test]
fn symbol_picker_flattens_nested_symbols() {
    use lsp_types::{Position as LspPosition, Range as LspRange, SymbolKind};
    let mut h = TestHarness::with_size(80, 24);
    enable_document_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn outer() {\n  fn inner() {}\n}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    let range = LspRange::new(LspPosition::new(0, 0), LspPosition::new(0, 1));
    let inner = {
        #[allow(deprecated)]
        DocumentSymbol {
            name: "inner".to_string(),
            detail: None,
            kind: SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            range,
            selection_range: range,
            children: None,
        }
    };
    let outer = {
        #[allow(deprecated)]
        DocumentSymbol {
            name: "outer".to_string(),
            detail: None,
            kind: SymbolKind::FUNCTION,
            tags: None,
            deprecated: None,
            range,
            selection_range: range,
            children: Some(vec![inner]),
        }
    };
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        DocumentSymbolResponse::Nested(vec![outer]),
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();
    let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
    let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, vec!["outer", "outer.inner"]);
}

#[test]
fn symbol_picker_pick_jumps_to_offset() {
    let mut h = TestHarness::with_size(80, 24);
    enable_document_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\nfn bar() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        DocumentSymbolResponse::Flat(vec![
            flat_symbol("foo", path.to_str().unwrap(), 0, 3),
            flat_symbol("bar", path.to_str().unwrap(), 1, 3),
        ]),
    );
    h.type_keys("space l s");
    h.settle();
    h.type_keys("down");
    h.type_keys("enter");
    assert!(h.stoat.symbol_finder.is_none());
    assert_eq!(cursor_offset(&mut h), 15);
}

#[test]
fn symbol_picker_keeps_all_entries() {
    let mut h = TestHarness::with_size(80, 24);
    enable_document_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "x\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    let many: Vec<lsp_types::SymbolInformation> = (0..15)
        .map(|i| flat_symbol(&format!("sym{i}"), path.to_str().unwrap(), 0, 0))
        .collect();
    h.fake_lsp()
        .set_document_symbols(path.to_str().unwrap(), DocumentSymbolResponse::Flat(many));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();
    let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
    assert_eq!(finder.entries.len(), 15);
    assert_eq!(finder.selected, 0);
}

#[test]
fn symbol_picker_navigates_with_arrows_and_picks_with_enter() {
    let mut h = TestHarness::with_size(80, 24);
    enable_document_symbols(&h);
    let mut text = String::new();
    for _ in 0..15 {
        text.push_str("fn x() {}\n");
    }
    let root = seed(&mut h, &[("main.rs", text.as_str())]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    let many: Vec<lsp_types::SymbolInformation> = (0..15)
        .map(|i| flat_symbol(&format!("sym{i}"), path.to_str().unwrap(), i as u32, 3))
        .collect();
    h.fake_lsp()
        .set_document_symbols(path.to_str().unwrap(), DocumentSymbolResponse::Flat(many));

    h.type_keys("space l s");
    h.settle();
    for _ in 0..11 {
        h.type_keys("down");
    }
    let finder = h.stoat.symbol_finder.as_ref().expect("finder");
    assert_eq!(finder.selected, 11);

    h.type_keys("enter");
    assert!(h.stoat.symbol_finder.is_none());
    assert_eq!(cursor_offset(&mut h), 11 * 10 + 3);
}

#[test]
fn symbol_picker_escape_dismisses() {
    let mut h = TestHarness::with_size(80, 24);
    enable_document_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        DocumentSymbolResponse::Flat(vec![flat_symbol("foo", path.to_str().unwrap(), 0, 3)]),
    );
    h.type_keys("space l s");
    h.settle();
    assert!(h.stoat.symbol_finder.is_some());
    h.type_keys("escape");
    assert!(h.stoat.symbol_finder.is_none());
}

#[test]
fn space_l_s_triggers_symbol_picker() {
    let mut h = TestHarness::with_size(80, 24);
    enable_document_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        DocumentSymbolResponse::Flat(vec![flat_symbol("foo", path.to_str().unwrap(), 0, 3)]),
    );
    h.type_keys("space l s");
    h.settle();
    assert!(h.stoat.symbol_finder.is_some());
    assert_eq!(h.stoat.focused_mode(), "insert");
}

#[test]
fn snapshot_symbol_picker() {
    let mut h = TestHarness::with_size(60, 16);
    enable_document_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        DocumentSymbolResponse::Flat(vec![
            flat_symbol("foo", path.to_str().unwrap(), 0, 3),
            flat_symbol("bar", path.to_str().unwrap(), 1, 3),
        ]),
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();
    h.assert_snapshot("snapshot_symbol_picker");
}

#[test]
fn symbol_finder_renders_hover_doc_above_source() {
    let mut h = TestHarness::with_size(120, 30);
    enable_document_symbols_and_hover(&h);
    let root = seed(&mut h, &[("main.rs", "fn target() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        DocumentSymbolResponse::Flat(vec![flat_symbol("target", path.to_str().unwrap(), 0, 3)]),
    );
    h.fake_lsp()
        .set_hover(path.to_str().unwrap(), 0, 3, "TARGETDOC");
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();

    assert_eq!(
        h.stoat
            .symbol_finder
            .as_ref()
            .unwrap()
            .doc_markdown
            .as_deref(),
        Some("TARGETDOC"),
    );
    let buf = h.stoat.render();
    let shown = (0..buf.area.height).any(|y| {
        let row: String = (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect();
        row.contains("TARGETDOC")
    });
    assert!(shown, "the hover doc renders in the preview pane");
}

#[test]
fn symbol_finder_hover_none_leaves_doc_empty() {
    let mut h = TestHarness::with_size(120, 30);
    enable_document_symbols_and_hover(&h);
    let root = seed(&mut h, &[("main.rs", "fn target() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        DocumentSymbolResponse::Flat(vec![flat_symbol("target", path.to_str().unwrap(), 0, 3)]),
    );
    // No set_hover, so the server answers None. The doc stays empty and no
    // error surfaces.
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();

    assert!(
        h.stoat
            .symbol_finder
            .as_ref()
            .unwrap()
            .doc_markdown
            .is_none(),
        "an empty hover response leaves the doc area empty",
    );
    assert!(
        h.stoat.pending_message.is_none(),
        "a missing doc surfaces no error",
    );
}

#[test]
fn symbol_finder_hover_doc_follows_selection() {
    let mut h = TestHarness::with_size(120, 30);
    enable_document_symbols_and_hover(&h);
    let root = seed(&mut h, &[("main.rs", "fn aaa() {}\nfn bbb() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_document_symbols(
        path.to_str().unwrap(),
        DocumentSymbolResponse::Flat(vec![
            flat_symbol("aaa", path.to_str().unwrap(), 0, 3),
            flat_symbol("bbb", path.to_str().unwrap(), 1, 3),
        ]),
    );
    h.fake_lsp()
        .set_hover(path.to_str().unwrap(), 0, 3, "AAA DOC");
    h.fake_lsp()
        .set_hover(path.to_str().unwrap(), 1, 3, "BBB DOC");
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();
    assert_eq!(
        h.stoat
            .symbol_finder
            .as_ref()
            .unwrap()
            .doc_markdown
            .as_deref(),
        Some("AAA DOC"),
    );

    h.type_keys("down");
    h.settle();
    assert_eq!(
        h.stoat
            .symbol_finder
            .as_ref()
            .unwrap()
            .doc_markdown
            .as_deref(),
        Some("BBB DOC"),
        "the doc follows the selection, discarding the previous entry's doc",
    );
}

/// Install two identically-capable fakes routed primary-then-secondary for
/// `rust`. The caller seeds and opens its own buffer.
fn document_symbol_caps() -> lsp_types::ServerCapabilities {
    use lsp_types::{OneOf, ServerCapabilities};
    ServerCapabilities {
        document_symbol_provider: Some(OneOf::Left(true)),
        ..ServerCapabilities::default()
    }
}

#[test]
fn document_symbols_merge_from_two_servers() {
    let mut h = TestHarness::with_size(80, 24);
    let (primary, secondary) = install_two_servers(&mut h, document_symbol_caps());
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\nfn bar() {}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    let p = path.to_str().unwrap();
    primary.set_document_symbols(
        p,
        DocumentSymbolResponse::Flat(vec![flat_symbol("foo", p, 0, 3)]),
    );
    secondary.set_document_symbols(
        p,
        DocumentSymbolResponse::Flat(vec![flat_symbol("bar", p, 1, 3)]),
    );

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();

    let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
    let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["foo", "bar"],
        "both servers' symbols, primary first"
    );
}

#[test]
fn document_symbols_convert_each_with_its_servers_encoding() {
    use crate::{host::OffsetEncoding, symbol_finder::SymbolTarget};
    let mut h = TestHarness::with_size(80, 24);
    let (primary, secondary) = install_two_servers(&mut h, document_symbol_caps());
    primary.set_offset_encoding(OffsetEncoding::Utf8);
    secondary.set_offset_encoding(OffsetEncoding::Utf16);
    let root = seed(&mut h, &[("main.rs", "\u{e9}x\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    let p = path.to_str().unwrap();

    // `x` sits at byte offset 2. `é` is two UTF-8 bytes but one UTF-16 unit,
    // so each server names `x`'s column in its own encoding.
    primary.set_document_symbols(
        p,
        DocumentSymbolResponse::Flat(vec![flat_symbol("utf8", p, 0, 2)]),
    );
    secondary.set_document_symbols(
        p,
        DocumentSymbolResponse::Flat(vec![flat_symbol("utf16", p, 0, 1)]),
    );

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSymbolPicker);
    h.settle();

    let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
    let resolved: Vec<(&str, usize)> = finder
        .entries
        .iter()
        .map(|e| match &e.target {
            SymbolTarget::Offset(offset) => (e.title.as_str(), *offset),
            other => unreachable!("document symbols carry offset targets, got {other:?}"),
        })
        .collect();
    assert_eq!(
        resolved,
        vec![("utf8", 2), ("utf16", 2)],
        "each server's column resolves with its own encoding"
    );
}

#[test]
fn workspace_symbols_merge_from_two_servers() {
    use lsp_types::{OneOf, ServerCapabilities, SymbolKind};
    let mut h = TestHarness::with_size(80, 24);
    let (primary, secondary) = install_two_servers(
        &mut h,
        ServerCapabilities {
            workspace_symbol_provider: Some(OneOf::Left(true)),
            ..ServerCapabilities::default()
        },
    );
    let root = seed(
        &mut h,
        &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
    );
    let main = root.join("main.rs");
    let lib = root.join("lib.rs");
    open_buffer(&mut h, main.clone());
    primary.add_workspace_symbol(
        "f",
        "foo",
        SymbolKind::FUNCTION,
        main.to_str().unwrap(),
        0,
        3,
    );
    secondary.add_workspace_symbol(
        "f",
        "bar",
        SymbolKind::FUNCTION,
        lib.to_str().unwrap(),
        0,
        3,
    );

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();
    h.type_keys("f");
    h.settle();

    let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
    let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(
        titles,
        vec!["foo", "bar"],
        "both servers' symbols, primary first"
    );
}

#[test]
fn workspace_symbol_unsupported_capability_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    open_buffer(&mut h, root.join("main.rs"));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();
    assert!(h.stoat.symbol_finder.is_none());
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn workspace_symbol_opens_finder_modal() {
    let mut h = TestHarness::with_size(80, 24);
    enable_workspace_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    open_buffer(&mut h, root.join("main.rs"));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();
    assert!(h.stoat.symbol_finder.is_some());
    assert_eq!(h.stoat.focused_mode(), "insert");
}

#[test]
fn workspace_symbol_query_populates_finder() {
    use lsp_types::SymbolKind;
    let mut h = TestHarness::with_size(80, 24);
    enable_workspace_symbols(&h);
    let root = seed(
        &mut h,
        &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
    );
    let main = root.join("main.rs");
    let lib = root.join("lib.rs");
    open_buffer(&mut h, main.clone());
    h.fake_lsp().add_workspace_symbol(
        "f",
        "foo",
        SymbolKind::FUNCTION,
        main.to_str().unwrap(),
        0,
        3,
    );
    h.fake_lsp().add_workspace_symbol(
        "f",
        "bar",
        SymbolKind::FUNCTION,
        lib.to_str().unwrap(),
        0,
        3,
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();
    h.type_keys("f");
    h.settle();
    let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
    let titles: Vec<&str> = finder.entries.iter().map(|e| e.title.as_str()).collect();
    assert_eq!(titles, vec!["foo", "bar"]);
}

#[test]
fn workspace_symbol_query_handles_nested_response() {
    use crate::symbol_finder::SymbolTarget;
    use lsp_types::{
        Location, OneOf, Position as LspPosition, Range as LspRange, SymbolKind, Uri,
        WorkspaceLocation, WorkspaceSymbol, WorkspaceSymbolResponse,
    };
    use std::str::FromStr;
    let mut h = TestHarness::with_size(80, 24);
    enable_workspace_symbols(&h);
    let root = seed(
        &mut h,
        &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
    );
    let main = root.join("main.rs");
    let lib = root.join("lib.rs");
    open_buffer(&mut h, main.clone());
    let main_uri = Uri::from_str(&format!("file://{}", main.to_str().unwrap())).unwrap();
    let lib_uri = Uri::from_str(&format!("file://{}", lib.to_str().unwrap())).unwrap();
    let nested = WorkspaceSymbolResponse::Nested(vec![
        WorkspaceSymbol {
            name: "foo".to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Left(Location::new(
                main_uri,
                LspRange::new(LspPosition::new(0, 3), LspPosition::new(0, 6)),
            )),
            data: None,
        },
        WorkspaceSymbol {
            name: "bar".to_string(),
            kind: SymbolKind::FUNCTION,
            tags: None,
            container_name: None,
            location: OneOf::Right(WorkspaceLocation { uri: lib_uri }),
            data: None,
        },
    ]);
    h.fake_lsp().set_workspace_symbol_response("f", nested);
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();
    h.type_keys("f");
    h.settle();
    let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
    let entries: Vec<(&str, &Path, LspPosition)> = finder
        .entries
        .iter()
        .map(|e| match &e.target {
            SymbolTarget::Workspace { path, position, .. } => {
                (e.title.as_str(), path.as_path(), *position)
            },
            other => unreachable!("workspace symbols carry workspace targets, got {other:?}"),
        })
        .collect();
    assert_eq!(
        entries,
        vec![
            ("foo", main.as_path(), LspPosition::new(0, 3)),
            ("bar", lib.as_path(), LspPosition::new(0, 0)),
        ]
    );
}

#[test]
fn workspace_symbol_coalesces_mid_flight_query_edits() {
    use std::time::Duration;
    let mut h = TestHarness::with_size(80, 24);
    enable_workspace_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    open_buffer(&mut h, root.join("main.rs"));

    // Hold the initial empty-query request in flight so later edits coalesce
    // onto it rather than firing their own requests.
    h.fake_lsp()
        .set_request_delay("workspace/symbol", Duration::from_secs(60));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();
    h.type_keys("a");
    h.settle();
    h.type_keys("b");
    h.settle();

    let finder = h.stoat.symbol_finder.as_ref().expect("finder open");
    assert!(
        finder.query_dirty,
        "edits while a request is in flight mark the query dirty",
    );
    assert_eq!(finder.last_query, "ab", "the latest text is remembered");
    assert!(
        h.stoat.pending_workspace_symbol_request.is_some(),
        "only the one in-flight request exists; no second fired mid-flight",
    );
}

#[test]
fn workspace_symbol_pick_opens_target_file() {
    use lsp_types::SymbolKind;
    let mut h = TestHarness::with_size(80, 24);
    enable_workspace_symbols(&h);
    let root = seed(
        &mut h,
        &[("main.rs", "fn foo() {}\n"), ("lib.rs", "fn bar() {}\n")],
    );
    let main = root.join("main.rs");
    let lib = root.join("lib.rs");
    open_buffer(&mut h, main.clone());
    h.fake_lsp().add_workspace_symbol(
        "bar",
        "bar",
        SymbolKind::FUNCTION,
        lib.to_str().unwrap(),
        0,
        3,
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();
    h.type_keys("b a r");
    h.settle();
    h.type_keys("enter");
    let ws = h.stoat.active_workspace();
    let pane = ws.panes.pane(ws.panes.focus());
    let crate::pane::View::Editor(editor_id) = pane.view else {
        panic!("not an editor");
    };
    let buffer_id = ws.editors.get(editor_id).expect("editor").buffer_id;
    let path = ws
        .buffers
        .path_for(buffer_id)
        .expect("buffer path")
        .to_path_buf();
    assert_eq!(path, lib);
    assert_eq!(cursor_offset(&mut h), 3);
}

#[test]
fn workspace_symbol_navigates_with_arrows_and_picks_with_enter() {
    use lsp_types::SymbolKind;
    let mut h = TestHarness::with_size(80, 24);
    enable_workspace_symbols(&h);
    let mut files: Vec<(&str, &str)> = (0..12)
        .map(|i| {
            let path = Box::leak(format!("f{i}.rs").into_boxed_str()) as &str;
            (path, "fn target() {}\n")
        })
        .collect();
    files.push(("anchor.rs", "fn anchor() {}\n"));
    let root = seed(&mut h, &files);
    let anchor_path = root.join("anchor.rs");
    open_buffer(&mut h, anchor_path);
    for i in 0..12 {
        let p = root.join(format!("f{i}.rs"));
        h.fake_lsp().add_workspace_symbol(
            "t",
            "target",
            SymbolKind::FUNCTION,
            p.to_str().unwrap(),
            0,
            3,
        );
    }
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();
    h.type_keys("t");
    h.settle();

    for _ in 0..11 {
        h.type_keys("down");
    }
    let finder = h.stoat.symbol_finder.as_ref().expect("finder");
    assert_eq!(finder.selected, 11);

    h.type_keys("enter");
    let ws = h.stoat.active_workspace();
    let pane = ws.panes.pane(ws.panes.focus());
    let crate::pane::View::Editor(eid) = pane.view else {
        panic!("not an editor");
    };
    let buffer_id = ws.editors.get(eid).expect("editor").buffer_id;
    let path = ws.buffers.path_for(buffer_id).expect("path").to_path_buf();
    assert_eq!(path, root.join("f11.rs"));
    assert_eq!(cursor_offset(&mut h), 3);
}

#[test]
fn workspace_symbol_cancel_clears_modal() {
    let mut h = TestHarness::with_size(80, 24);
    enable_workspace_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    open_buffer(&mut h, root.join("main.rs"));
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();
    assert!(h.stoat.symbol_finder.is_some());
    h.type_keys("escape");
    assert!(h.stoat.symbol_finder.is_none());
    assert_eq!(h.stoat.focused_mode(), "normal");
}

#[test]
fn space_l_shift_s_triggers_workspace_symbol() {
    let mut h = TestHarness::with_size(80, 24);
    enable_workspace_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    open_buffer(&mut h, root.join("main.rs"));
    h.type_keys("space l shift-s");
    h.settle();
    assert!(h.stoat.symbol_finder.is_some());
    assert_eq!(h.stoat.focused_mode(), "insert");
}

#[test]
fn snapshot_workspace_symbol_finder() {
    use lsp_types::SymbolKind;
    let mut h = TestHarness::with_size(60, 16);
    enable_workspace_symbols(&h);
    let root = seed(&mut h, &[("main.rs", "fn foo() {}\n")]);
    let main = root.join("main.rs");
    open_buffer(&mut h, main.clone());
    h.fake_lsp().add_workspace_symbol(
        "f",
        "foo",
        SymbolKind::FUNCTION,
        main.to_str().unwrap(),
        0,
        3,
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenWorkspaceSymbolPicker);
    h.settle();
    h.type_keys("f");
    h.settle();
    h.assert_snapshot("snapshot_workspace_symbol_finder");
}

fn enable_format(h: &TestHarness) {
    use lsp_types::{OneOf, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        document_formatting_provider: Some(OneOf::Left(true)),
        document_range_formatting_provider: Some(OneOf::Left(true)),
        ..Default::default()
    });
}

fn format_text_edit(
    line: u32,
    col: u32,
    end_line: u32,
    end_col: u32,
    new: &str,
) -> lsp_types::TextEdit {
    use lsp_types::{Position as LspPosition, Range as LspRange, TextEdit};
    TextEdit {
        range: LspRange::new(
            LspPosition::new(line, col),
            LspPosition::new(end_line, end_col),
        ),
        new_text: new.to_string(),
    }
}

#[test]
fn format_unsupported_capability_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_range_formatting(
        path.to_str().unwrap(),
        vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
    h.settle();
    assert_eq!(buffer_text(&h, &path), "fn  foo (){}\n");
}

#[test]
fn format_no_response_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    enable_format(&h);
    let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
    h.settle();
    assert_eq!(buffer_text(&h, &path), "fn  foo (){}\n");
}

#[test]
fn format_applies_returned_edits() {
    let mut h = TestHarness::with_size(80, 24);
    enable_format(&h);
    let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_range_formatting(
        path.to_str().unwrap(),
        vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
    h.settle();
    assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
}

#[test]
fn format_equals_keystroke_triggers() {
    let mut h = TestHarness::with_size(80, 24);
    enable_format(&h);
    let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_range_formatting(
        path.to_str().unwrap(),
        vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
    );
    h.type_keys("=");
    h.settle();
    assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
}

#[test]
fn format_document_applies_returned_edits() {
    let mut h = TestHarness::with_size(80, 24);
    enable_format(&h);
    let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_formatting(
        path.to_str().unwrap(),
        vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Format);
    h.settle();
    assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
}

#[test]
fn a_format_reply_is_dropped_when_the_buffer_moved_under_it() {
    let mut h = TestHarness::with_size(80, 24);
    enable_format(&h);
    let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_formatting(
        path.to_str().unwrap(),
        vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
    );

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Format);

    // The buffer moves while the request is in flight, so every position
    // the server sent back names text one character along from what it
    // measured. Edited directly because a keypress would also pump the
    // reply, closing the window before the edit lands.
    let buffer_id = h.stoat.focused_editor_ids().expect("editor").1;
    h.stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("poisoned")
        .edit(0..0, "x");
    h.settle();

    assert_eq!(
        buffer_text(&h, &path),
        "xfn  foo (){}\n",
        "the reply was applied to text the server never saw"
    );
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("lsp: format skipped, buffer changed"),
    );
}

#[test]
fn formatting_asks_for_the_indentation_the_buffer_uses() {
    // Two leading levels each, so the style detector has something to vote
    // on. One file indents with tabs and the other with two spaces.
    for (name, text, tab_size, insert_spaces) in [
        ("tabs.rs", "fn a() {\n\tlet b = 1;\n\t\tc();\n}\n", 4, false),
        (
            "spaces.rs",
            "fn a() {\n  let b = 1;\n    c();\n}\n",
            2,
            true,
        ),
    ] {
        let mut h = TestHarness::with_size(80, 24);
        enable_format(&h);
        let root = seed(&mut h, &[(name, text)]);
        let path = root.join(name);
        open_buffer(&mut h, path.clone());

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Format);
        h.settle();

        let observed = h.fake_lsp().observed_formatting();
        assert_eq!(observed.len(), 1, "{name}");
        assert_eq!(observed[0].options.tab_size, tab_size, "{name} tab size");
        assert_eq!(
            observed[0].options.insert_spaces, insert_spaces,
            "{name} insert spaces",
        );
    }
}

#[test]
fn range_formatting_asks_for_the_indentation_too() {
    let mut h = TestHarness::with_size(80, 24);
    enable_format(&h);
    let root = seed(&mut h, &[("a.rs", "fn a() {\n  let b = 1;\n    c();\n}\n")]);
    let path = root.join("a.rs");
    open_buffer(&mut h, path);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::FormatSelections);
    h.settle();

    let observed = h.fake_lsp().observed_range_formatting();
    assert_eq!(observed.len(), 1);
    assert_eq!(observed[0].options.tab_size, 2);
    assert!(observed[0].options.insert_spaces);
}

#[test]
fn format_document_unsupported_capability_is_noop() {
    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_formatting(
        path.to_str().unwrap(),
        vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
    );
    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::Format);
    h.settle();
    assert_eq!(buffer_text(&h, &path), "fn  foo (){}\n");
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("lsp: server does not support format"),
    );
}

#[test]
fn space_l_f_formats_document() {
    let mut h = TestHarness::with_size(80, 24);
    enable_format(&h);
    let root = seed(&mut h, &[("main.rs", "fn  foo (){}\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp().set_formatting(
        path.to_str().unwrap(),
        vec![format_text_edit(0, 0, 1, 0, "fn foo() {}\n")],
    );
    h.type_keys("space l f");
    h.settle();
    assert_eq!(buffer_text(&h, &path), "fn foo() {}\n");
}

fn enable_inlay_hints(h: &TestHarness) {
    use lsp_types::{OneOf, ServerCapabilities};
    h.fake_lsp().set_capabilities(ServerCapabilities {
        inlay_hint_provider: Some(OneOf::Left(true)),
        ..Default::default()
    });
}

fn type_hint(line: u32, col: u32, label: &str) -> lsp_types::InlayHint {
    use lsp_types::{InlayHint, InlayHintKind, InlayHintLabel, Position};
    InlayHint {
        position: Position::new(line, col),
        label: InlayHintLabel::String(label.to_string()),
        kind: Some(InlayHintKind::TYPE),
        text_edits: None,
        tooltip: None,
        padding_left: None,
        padding_right: None,
        data: None,
    }
}

fn hint_ids_len(h: &mut TestHarness) -> usize {
    crate::action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .hint_inlay_ids
        .len()
}

fn focused_editor_id(h: &TestHarness) -> crate::editor_state::EditorId {
    let ws = h.stoat.active_workspace();
    match ws.panes.pane(ws.panes.focus()).view {
        crate::pane::View::Editor(id) => id,
        _ => panic!("focused pane is not an editor"),
    }
}

fn editor_hint_ids_len(h: &TestHarness, id: crate::editor_state::EditorId) -> usize {
    h.stoat
        .active_workspace()
        .editors
        .get(id)
        .expect("editor")
        .hint_inlay_ids
        .len()
}

#[test]
fn snapshot_inlay_hints_render_when_enabled() {
    let mut h = TestHarness::with_size(40, 8);
    enable_inlay_hints(&h);
    let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
    h.capture("prime");
    h.type_keys("space l h");
    h.advance_clock(Duration::from_millis(150));
    h.assert_snapshot("inlay_hints_enabled");
}

#[test]
fn inlay_hints_toggle_off_clears_inlays() {
    let mut h = TestHarness::with_size(40, 8);
    enable_inlay_hints(&h);
    let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
    h.capture("prime");
    h.type_keys("space l h");
    h.advance_clock(Duration::from_millis(150));
    assert_eq!(hint_ids_len(&mut h), 1);

    h.type_keys("space l h");
    assert_eq!(hint_ids_len(&mut h), 0);
}

#[test]
fn inlay_hints_toggle_off_clears_unfocused_editors() {
    let mut h = TestHarness::with_size(40, 8);
    enable_inlay_hints(&h);
    let root = seed(&mut h, &[("a.rs", "let x = 1\n"), ("b.rs", "let y = 2\n")]);
    let a = root.join("a.rs");
    open_buffer(&mut h, a.clone());
    h.fake_lsp()
        .set_range_inlay_hints(a.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
    h.type_keys("space l h");
    h.advance_clock(Duration::from_millis(150));
    let a_editor = focused_editor_id(&h);
    assert_eq!(editor_hint_ids_len(&h, a_editor), 1);

    crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
    open_buffer(&mut h, root.join("b.rs"));
    assert_ne!(
        focused_editor_id(&h),
        a_editor,
        "opening b.rs in the split moves focus off a.rs's editor"
    );

    h.type_keys("space l h");
    assert_eq!(
        editor_hint_ids_len(&h, a_editor),
        0,
        "toggle-off clears hints from the unfocused a.rs editor"
    );
}

#[test]
fn inlay_hints_toggle_on_requests_without_the_debounce() {
    let mut h = TestHarness::with_size(40, 8);
    enable_inlay_hints(&h);
    let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
    h.capture("prime");
    h.type_keys("space l h");
    h.settle();
    assert_eq!(
        hint_ids_len(&mut h),
        1,
        "toggle-on applies hints without advancing the debounce clock"
    );
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("inlay hints on"),
        "toggle-on acknowledges in the status bar"
    );
}

#[test]
fn inlay_hints_toggle_off_acknowledges() {
    let mut h = TestHarness::with_size(40, 8);
    enable_inlay_hints(&h);
    let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.fake_lsp()
        .set_range_inlay_hints(path.to_str().unwrap(), vec![type_hint(0, 5, ": u32")]);
    h.capture("prime");
    h.type_keys("space l h");
    h.settle();
    h.type_keys("space l h");
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("inlay hints off"),
        "toggle-off acknowledges in the status bar"
    );
}

#[test]
fn inlay_hints_toggle_on_without_a_capable_server_reports_why() {
    let mut h = TestHarness::with_size(40, 8);
    let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    h.type_keys("space l h");
    h.settle();
    assert_eq!(
        hint_ids_len(&mut h),
        0,
        "no capable server means no hints are applied"
    );
    assert_eq!(
        h.stoat.pending_message.as_deref(),
        Some("lsp: server does not support inlay hints"),
        "toggle-on with no inlay capability reports why"
    );
}

#[test]
fn inlay_hints_refresh_after_edit() {
    let mut h = TestHarness::with_size(40, 8);
    enable_inlay_hints(&h);
    let root = seed(&mut h, &[("main.rs", "let x = 1\n")]);
    let path = root.join("main.rs");
    open_buffer(&mut h, path.clone());
    let p = path.to_str().unwrap();
    h.fake_lsp()
        .set_range_inlay_hints(p, vec![type_hint(0, 5, ": u32")]);
    h.capture("prime");
    h.type_keys("space l h");
    h.advance_clock(Duration::from_millis(150));
    assert_eq!(hint_ids_len(&mut h), 1);

    h.fake_lsp()
        .set_range_inlay_hints(p, vec![type_hint(0, 5, ": u32"), type_hint(0, 8, ": b")]);
    h.type_keys("i");
    h.type_text("z");
    h.type_keys("escape");
    h.advance_clock(Duration::from_millis(150));
    assert_eq!(hint_ids_len(&mut h), 2);
}

#[test]
fn an_inlay_request_starts_below_a_deleted_block_on_the_top_row() {
    use crate::{diff_map::DiffMap, host::OffsetEncoding};
    use std::sync::Arc;
    use stoat_language::structural_diff;

    let mut h = TestHarness::with_size(80, 24);
    let root = seed(&mut h, &[("a.rs", "a\nb\nc\n")]);
    open_buffer(&mut h, root.join("a.rs"));

    let buffer_id = crate::action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("focused editor")
        .buffer_id;
    let (base, text) = ("a\nd1\nd2\nd3\nb\nc\n", "a\nb\nc\n");
    h.stoat
        .active_workspace()
        .buffers
        .get(buffer_id)
        .expect("buffer")
        .write()
        .expect("poisoned")
        .diff_map = Some(DiffMap::from_structural_changes(
        structural_diff::diff(base, text),
        Arc::new(base.to_string()),
        text,
    ));

    let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    editor.set_diff_view(true);
    editor.scroll_row = 1;
    editor.viewport_rows = Some(5);

    let range = super::build_inlay_hint_request(&mut h.stoat, OffsetEncoding::Utf8)
        .expect("the focused editor builds a request")
        .params
        .range;
    assert_eq!(
        (range.start.line, range.end.line),
        (1, 3),
        "the range starts on the first buffer row below the deleted lines",
    );
}

fn tree_sitter_token_count(h: &mut TestHarness) -> usize {
    let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    let snapshot = editor.display_map.snapshot();
    snapshot
        .semantic_token_highlights()
        .values()
        .map(|channel| channel.len())
        .sum()
}

#[test]
fn switching_back_keeps_tree_sitter_highlights_on_first_frame() {
    let mut h = TestHarness::with_size(24, 4);
    let root = seed(&mut h, &[("a.rs", "fn a() {}\n"), ("b.rs", "fn b() {}\n")]);

    // A render cycle drives the parse so A's tokens land in the registry and
    // on its editor.
    open_buffer(&mut h, root.join("a.rs"));
    h.snapshot();
    assert!(tree_sitter_token_count(&mut h) > 0, "file A parses on open");

    open_buffer(&mut h, root.join("b.rs"));
    h.snapshot();

    // Switch back to A with no render or parse cycle in between. The parse
    // pipeline skips a version-current buffer, so the fresh editor is styled
    // only if it was seeded from the registry's retained tokens.
    crate::action_handlers::dispatch(
        &mut h.stoat,
        &OpenFile {
            path: root.join("a.rs"),
        },
    );
    assert!(
        tree_sitter_token_count(&mut h) > 0,
        "re-shown buffer is styled on the first frame after switch-back"
    );
}

#[test]
fn a_same_file_jump_glides_while_a_cross_file_jump_snaps() {
    use crate::editor_state::ScrollGlide;

    let mut h = TestHarness::with_size(40, 12);
    let long: String = (0..200).map(|i| format!("line {i:03}\n")).collect();
    let root = seed(&mut h, &[("a.rs", long.as_str()), ("b.rs", long.as_str())]);
    crate::action_handlers::dispatch(
        &mut h.stoat,
        &OpenFile {
            path: root.join("a.rs"),
        },
    );
    h.settle();
    {
        let editor =
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        editor.viewport_rows = Some(10);
        editor.scroll_glide = ScrollGlide::None;
    }

    super::apply_jump(&mut h.stoat, &root.join("a.rs"), long.len());
    {
        let editor =
            crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
        assert!(
            editor.scroll_row > 20,
            "the view followed the cursor down the file"
        );
        assert_eq!(
            editor.scroll_glide,
            ScrollGlide::Page,
            "a same-file jump glides from where the view was"
        );
        editor.scroll_glide = ScrollGlide::None;
    }

    super::apply_jump(&mut h.stoat, &root.join("b.rs"), long.len());
    h.settle();
    let editor = crate::action_handlers::focused_editor_mut(&mut h.stoat).expect("focused editor");
    assert!(
        editor.scroll_row > 20,
        "the cursor is still pulled into view across files"
    );
    assert_eq!(
        editor.scroll_glide,
        ScrollGlide::None,
        "a fresh editor has no origin to glide from, so it snaps"
    );
}
