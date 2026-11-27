use codegraph::SymbolKey;
use std::iter;
use stoat::{
    action_handlers,
    app::Stoat,
    buffer::BufferId,
    code_index::nav,
    diff_map::{DiffHunk, DiffHunkStatus, DiffMap, StagedMark},
    host::CommitInfo,
    keymap::{KeymapState, StateValue},
    keymap_state::{active_modal, resolve_focus, user_var, ActiveModal, StoatKeymapState},
    lsp::LspSymbolKind,
    pane::{FocusTarget, View},
    review_walk::{ReturnRef, ReviewWalk, WalkOrigin, WalkSpan},
    run::RunId,
    test_fixture::{field, resolved},
};
use stoat_action::Action;
use stoat_config::Value;

#[test]
fn from_stoat_default_is_editor_file_no_modal() {
    let h = Stoat::test();
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(field(&state, "pane"), Some("editor".to_string()));
    assert_eq!(field(&state, "view"), Some("file".to_string()));
    assert_eq!(state.get("modal"), None);
}

#[test]
fn from_stoat_run_pane_is_run() {
    let mut h = Stoat::test();
    {
        let ws = h.stoat.active_workspace_mut();
        if matches!(ws.focus, FocusTarget::SplitPane) {
            let pane_id = ws.panes.focus();
            ws.panes.pane_mut(pane_id).view = View::Run(RunId::default());
        }
    }
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(field(&state, "pane"), Some("run".to_string()));
    assert_eq!(state.get("view"), None);
}

/// The `pair` predicate as the keymap reads it.
fn pair(h: &stoat::test_harness::TestHarness) -> Option<StateValue> {
    StoatKeymapState::from_stoat(&h.stoat).get("pair").cloned()
}

#[test]
fn two_files_in_two_panes_are_a_pair() {
    let mut h = Stoat::test();
    assert_eq!(
        pair(&h),
        Some(StateValue::Bool(false)),
        "one scratch pane is no pair"
    );

    h.open_side_by_side(("/pair/a.txt", "a\n"), ("/pair/b.txt", "b\n"));
    assert_eq!(pair(&h), Some(StateValue::Bool(true)));
}

/// The pair belongs to the right file, which the pair diff focuses. Closing
/// the view returns that file to the workspace's base, so the flag drops.
///
/// The two files are equal, so the pair holds no hunk. The flag answers for
/// the pair, not for the changes the diff shows.
#[test]
fn pair_diff_reads_whether_the_focused_file_has_a_pair() {
    let mut h = stoat::test_harness::TestHarness::with_size(120, 20);
    h.open_side_by_side(("/pair/a.txt", "same\n"), ("/pair/b.txt", "same\n"));
    action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
    let pair_diff = |h: &stoat::test_harness::TestHarness| {
        StoatKeymapState::from_stoat(&h.stoat)
            .get("pair_diff")
            .cloned()
    };

    let before = pair_diff(&h);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffPair);
    h.settle();
    let paired = pair_diff(&h);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    h.settle();

    assert_eq!(
        [before, paired, pair_diff(&h)],
        [false, true, false].map(|on| Some(StateValue::Bool(on))),
    );
}

#[test]
fn the_same_file_in_both_panes_is_no_pair() {
    let mut h = Stoat::test();
    h.seed_fixture("/pair/a.txt", "a\n");
    h.open_file(std::path::Path::new("/pair/a.txt"));
    h.type_action("SplitRight()");
    assert_eq!(pair(&h), Some(StateValue::Bool(false)));
}

#[test]
fn only_file_panes_count_toward_a_pair() {
    let mut h = Stoat::test();
    h.open_side_by_side(("/pair/a.txt", "a\n"), ("/pair/b.txt", "b\n"));
    h.type_action("SplitDown()");
    assert_eq!(
        pair(&h),
        Some(StateValue::Bool(false)),
        "a third file pane leaves no pair"
    );

    {
        let ws = h.stoat.active_workspace_mut();
        let pane_id = ws.panes.focus();
        ws.panes.pane_mut(pane_id).view = View::Run(RunId::default());
    }
    assert_eq!(
        pair(&h),
        Some(StateValue::Bool(true)),
        "a run pane does not count"
    );
}

#[test]
fn the_first_pane_in_layout_order_leads_the_pair() {
    let mut h = Stoat::test();
    h.open_side_by_side(("/pair/a.txt", "a\n"), ("/pair/b.txt", "b\n"));
    let paths = |h: &stoat::test_harness::TestHarness| {
        let ws = h.stoat.active_workspace();
        ws.pair_panes().map(|pair| {
            pair.map(|(_, buffer_id)| {
                ws.buffers
                    .path_for(buffer_id)
                    .expect("a pair buffer has a path")
                    .to_path_buf()
            })
        })
    };
    let expected = Some(["/pair/a.txt", "/pair/b.txt"].map(std::path::PathBuf::from));
    assert_eq!(paths(&h), expected);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::FocusLeft);
    assert_eq!(paths(&h), expected, "focus does not reorder the pair");
}

/// The `git` predicate as the keymap reads it.
fn git(h: &stoat::test_harness::TestHarness) -> Option<StateValue> {
    StoatKeymapState::from_stoat(&h.stoat).get("git").cloned()
}

#[test]
fn git_reads_whether_the_root_is_in_a_repository() {
    let mut h = Stoat::test();
    let scratch = git(&h);

    h.stage_review_scenario("/repo", &[("a.rs", "a\n", "b\n")]);
    h.type_keys("escape");
    let staged = git(&h);

    h.stoat.active_workspace_mut().git_root = "/elsewhere".into();
    h.type_keys("escape");
    assert_eq!(
        [scratch, staged, git(&h)],
        [false, true, false].map(|on| Some(StateValue::Bool(on))),
        "a scratch root, the staged repository, then a root outside it",
    );
}

/// The `blame` predicate as the keymap reads it.
fn blame(h: &stoat::test_harness::TestHarness) -> Option<StateValue> {
    StoatKeymapState::from_stoat(&h.stoat).get("blame").cloned()
}

/// `blame` reads the focused pane's flag while the pane shows a plain
/// editor, and reads false once that editor turns into a diff view.
#[test]
fn blame_reads_the_focused_pane_over_a_plain_editor() {
    let mut h = Stoat::test();
    let off = blame(&h);
    {
        let ws = h.stoat.active_workspace_mut();
        let focus = ws.panes.focus();
        ws.panes.pane_mut(focus).blame = true;
    }
    let on = blame(&h);

    action_handlers::focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    assert_eq!(
        [off, on, blame(&h)],
        [false, true, false].map(|on| Some(StateValue::Bool(on))),
        "off, on over the plain editor, then off over its diff view",
    );
}

/// Store `map` as the focused buffer's diff, anchored as a diff job leaves
/// it.
fn install_diff_map(h: &stoat::test_harness::TestHarness, mut map: DiffMap) {
    let ws = h.stoat.active_workspace();
    let (_, buffer_id) = resolve_focus(ws).expect("an editor has the focus");
    let buffer = ws.buffers.get(buffer_id).expect("the focused buffer");
    let mut guard = buffer.write().expect("buffer poisoned");
    map.anchor_hunks(&guard.snapshot);
    guard.diff_map = Some(map);
}

#[test]
fn changes_reads_whether_the_focused_diff_holds_a_hunk() {
    let h = Stoat::test();
    let changes = |h: &stoat::test_harness::TestHarness| {
        StoatKeymapState::from_stoat(&h.stoat)
            .get("changes")
            .cloned()
    };
    let scratch = changes(&h);

    let hunk = DiffHunk {
        status: DiffHunkStatus::Added,
        unstaged_lines: iter::once(0..1).collect(),
        marked_rows: Vec::new(),
        relocated_rows: Vec::new(),
        buffer_start_line: 0,
        buffer_line_range: 0..1,
        base_byte_range: 0..0,
        anchor_range: None,
        token_detail: None,
    };
    install_diff_map(&h, DiffMap::from_hunks([hunk], None));
    let unstaged = changes(&h);

    let mut staged = DiffMap::default();
    staged.set_staged_marks(vec![StagedMark {
        status: DiffHunkStatus::Added,
        base_lines: 0..1,
    }]);
    install_diff_map(&h, staged);
    assert_eq!(
        [scratch, unstaged, changes(&h)],
        [false, true, true].map(|on| Some(StateValue::Bool(on))),
        "a scratch buffer, then an unstaged hunk, then a staged hunk alone",
    );
}

#[test]
fn trail_reads_the_phase_of_the_trail() {
    let mut h = Stoat::test();
    let phase = |h: &stoat::test_harness::TestHarness| {
        StoatKeymapState::from_stoat(&h.stoat).get("trail").cloned()
    };
    let scratch = phase(&h);

    open_foo_bar(&mut h);
    nav::mark_trail_start(&mut h.stoat);
    let armed = phase(&h);

    nav::install_trail(&mut h.stoat, &[SymbolKey([1u8; 16]), SymbolKey([2u8; 16])]);
    let path = phase(&h);

    nav::trail_clear(&mut h.stoat);
    assert_eq!(
        [scratch, armed, path, phase(&h)],
        [None, Some("armed"), Some("path"), None]
            .map(|phase| phase.map(|phase| StateValue::String(phase.into()))),
        "no trail, a marked start, an installed path, then a cleared trail",
    );
}

#[test]
fn panes_detached_and_window_ipc_read_the_layout_and_the_window_link() {
    let mut h = Stoat::test();
    let layout = |h: &stoat::test_harness::TestHarness| {
        let state = StoatKeymapState::from_stoat(&h.stoat);
        ["panes", "detached", "window_ipc"].map(|field| state.get(field).cloned())
    };
    let scratch = layout(&h);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::SplitRight);
    let split = layout(&h);

    h.stoat.window_ipc_connected = true;
    action_handlers::dispatch(&mut h.stoat, &stoat_action::DetachPane);
    let number = |n| Some(StateValue::Number(n));
    let flag = |on| Some(StateValue::Bool(on));
    assert_eq!(
        [scratch, split, layout(&h)],
        [
            [number(1.0), flag(false), flag(false)],
            [number(2.0), flag(false), flag(false)],
            [number(1.0), flag(true), flag(true)],
        ],
        "one pane, a split, then the focused pane detached over a live window link",
    );
}

#[test]
fn tabs_and_prev_tab_read_the_workspace_tabs() {
    let mut h = Stoat::test();
    let tabs = |h: &stoat::test_harness::TestHarness| {
        let state = StoatKeymapState::from_stoat(&h.stoat);
        ["tabs", "prev_tab"].map(|field| state.get(field).cloned())
    };
    let scratch = tabs(&h);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::NewTab);
    assert_eq!(
        [scratch, tabs(&h)],
        [
            [Some(StateValue::Number(1.0)), Some(StateValue::Bool(false))],
            [Some(StateValue::Number(2.0)), Some(StateValue::Bool(true))],
        ],
        "one tab, then a second tab with the first to toggle back to",
    );
}

#[test]
fn workspaces_counts_the_open_workspaces() {
    let mut h = Stoat::test();
    let count = |h: &stoat::test_harness::TestHarness| {
        StoatKeymapState::from_stoat(&h.stoat)
            .get("workspaces")
            .cloned()
    };
    let one = count(&h);

    h.type_action("NewWorkspace()");
    assert_eq!(
        [one, count(&h)],
        [1.0, 2.0].map(|n| Some(StateValue::Number(n))),
        "one workspace, then a second",
    );
}

#[test]
fn jump_back_and_jump_forward_read_the_focused_jumplist() {
    let mut h = Stoat::test();
    h.seed_focused_buffer("alpha\nbeta\n");
    let jumps = |h: &stoat::test_harness::TestHarness| {
        let state = StoatKeymapState::from_stoat(&h.stoat);
        ["jump_back", "jump_forward"].map(|field| state.get(field).cloned())
    };
    let scratch = jumps(&h);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::SaveSelection);
    h.type_keys("j");
    let saved = jumps(&h);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::JumpBackward);
    let flag = |on| Some(StateValue::Bool(on));
    assert_eq!(
        [scratch, saved, jumps(&h)],
        [
            [flag(false), flag(false)],
            [flag(true), flag(false)],
            [flag(false), flag(true)],
        ],
        "an empty list, a saved jump to go back to, then the oldest entry \
             with the jump just left ahead",
    );
}

#[test]
fn prev_buffer_reads_whether_the_pane_showed_another_open_buffer() {
    let mut h = Stoat::test();
    let root = stoat::test_fixture::seed(&mut h, &[("a.rs", "a\n"), ("b.rs", "b\n")]);
    let prev = |h: &stoat::test_harness::TestHarness| {
        StoatKeymapState::from_stoat(&h.stoat)
            .get("prev_buffer")
            .cloned()
    };
    let scratch = prev(&h);

    h.open_file(&root.join("a.rs"));
    h.open_file(&root.join("b.rs"));
    let two = prev(&h);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::CloseBuffer);
    assert_eq!(
        [scratch, two, prev(&h)],
        [false, true, false].map(|on| Some(StateValue::Bool(on))),
        "a fresh pane, a second file shown after the first, then the second closed",
    );
}

#[test]
fn last_picker_reads_whether_a_picker_opened() {
    let mut h = Stoat::test();
    let opened = |h: &stoat::test_harness::TestHarness| {
        StoatKeymapState::from_stoat(&h.stoat)
            .get("last_picker")
            .cloned()
    };
    let fresh = opened(&h);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
    h.type_keys("escape");
    assert_eq!(
        [fresh, opened(&h)],
        [false, true].map(|on| Some(StateValue::Bool(on))),
        "a fresh session, then one whose finder opened and closed",
    );
}

#[test]
fn any_diags_reads_whether_any_path_holds_a_diagnostic() {
    let mut h = Stoat::test();
    let flags = |h: &stoat::test_harness::TestHarness| {
        let state = StoatKeymapState::from_stoat(&h.stoat);
        ["any_diags", "diags"].map(|field| state.get(field).cloned())
    };
    let clean = flags(&h);

    h.seed_diagnostics("/lsp/a.rs", vec![lsp_types::Diagnostic::default()]);
    let flag = |on| Some(StateValue::Bool(on));
    assert_eq!(
        [clean, flags(&h)],
        [[flag(false), flag(false)], [flag(true), flag(false)]],
        "a clean session, then a diagnostic on a file no pane shows",
    );
}

fn open_foo_bar(h: &mut stoat::test_harness::TestHarness) -> BufferId {
    open_rust(h, b"Foo bar")
}

/// Open `/lsp/a.rs` holding `text` in the focused pane, and answer its
/// buffer.
fn open_rust(h: &mut stoat::test_harness::TestHarness, text: &[u8]) -> BufferId {
    let root = std::path::PathBuf::from("/lsp");
    let path = root.join("a.rs");
    h.fake_fs().insert_file(&path, text);
    h.stoat.active_workspace_mut().git_root = root;
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFile { path });
    h.settle();
    let ws = h.stoat.active_workspace();
    match ws.panes.pane(ws.panes.focus()).view {
        View::Editor(id) => ws.editors[id].buffer_id,
        _ => panic!("focused pane is not an editor"),
    }
}

/// Seed a symbol-kind index over "Foo bar": `Foo` [0,3) a trait, `bar` [4,7)
/// a function, leaving the space at offset 3 uncovered.
fn seed_foo_bar_kinds(h: &mut stoat::test_harness::TestHarness, id: BufferId) {
    let ws = h.stoat.active_workspace_mut();
    let snapshot = ws
        .buffers
        .get(id)
        .expect("buffer")
        .read()
        .expect("buffer poisoned")
        .snapshot
        .clone();
    let start = |o| snapshot.anchors_at_batch(&[o], stoat_text::Bias::Right)[0];
    let end = |o| snapshot.anchors_at_batch(&[o], stoat_text::Bias::Left)[0];
    let kinds = std::sync::Arc::from(vec![
        (start(0usize)..end(3usize), LspSymbolKind::Trait),
        (start(4usize)..end(7usize), LspSymbolKind::Function),
    ]);
    ws.buffers.store_lsp_symbol_kinds(id, kinds);
}

#[test]
fn from_stoat_token_absent_without_an_index() {
    let mut h = Stoat::test();
    open_foo_bar(&mut h);
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(state.get("token_known"), Some(&StateValue::Bool(false)));
    assert_eq!(state.get("token"), None);
}

#[test]
fn from_stoat_token_known_but_absent_on_plain_text() {
    let mut h = Stoat::test();
    let id = open_foo_bar(&mut h);
    seed_foo_bar_kinds(&mut h, id);
    action_handlers::movement::jump_to_offset(&mut h.stoat, 3);
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(state.get("token_known"), Some(&StateValue::Bool(true)));
    assert_eq!(
        state.get("token"),
        None,
        "the space between tokens is untyped"
    );
}

/// `comment` holds while the cursor row opens with a line comment token,
/// and a row of code below it reads false.
#[test]
fn comment_predicate_follows_the_cursor_row() {
    let mut h = Stoat::test();
    open_rust(&mut h, b"// a\nfn f() {}\n");

    let on_comment = StoatKeymapState::from_stoat(&h.stoat)
        .get("comment")
        .cloned();
    h.type_keys("j");
    let on_code = StoatKeymapState::from_stoat(&h.stoat)
        .get("comment")
        .cloned();
    assert_eq!(
        [on_comment, on_code],
        [Some(StateValue::Bool(true)), Some(StateValue::Bool(false))],
    );
}

#[test]
fn from_stoat_token_is_the_kind_under_the_cursor() {
    let mut h = Stoat::test();
    let id = open_foo_bar(&mut h);
    seed_foo_bar_kinds(&mut h, id);
    action_handlers::movement::jump_to_offset(&mut h.stoat, 4);
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(state.get("token_known"), Some(&StateValue::Bool(true)));
    assert_eq!(field(&state, "token"), Some("function".to_string()));
}

#[test]
fn from_stoat_lsp_lang_and_diags_for_a_rust_buffer() {
    use lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range};
    let mut h = Stoat::test();
    open_foo_bar(&mut h);
    h.seed_diagnostics(
        std::path::PathBuf::from("/lsp/a.rs"),
        vec![Diagnostic {
            range: Range::new(Position::new(0, 0), Position::new(0, 1)),
            severity: Some(DiagnosticSeverity::ERROR),
            code: None,
            code_description: None,
            source: None,
            message: String::new(),
            related_information: None,
            tags: None,
            data: None,
        }],
    );

    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(field(&state, "lang"), Some("rust".to_string()));
    assert_eq!(
        state.get("lsp"),
        Some(&StateValue::Bool(true)),
        "the fake client serves rust",
    );
    assert_eq!(state.get("diags"), Some(&StateValue::Bool(true)));
}

#[test]
fn from_stoat_modified_true_after_an_edit() {
    let mut h = Stoat::test();
    open_foo_bar(&mut h);
    h.type_keys("i x escape");
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(state.get("modified"), Some(&StateValue::Bool(true)));
}

#[test]
fn from_stoat_has_selection_true_with_an_active_selection() {
    let mut h = Stoat::test();
    open_foo_bar(&mut h);
    h.type_keys("v l");
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(state.get("has_selection"), Some(&StateValue::Bool(true)));
}

#[test]
fn from_stoat_lang_absent_for_a_scratch_buffer() {
    let h = Stoat::test();
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(state.get("lang"), None, "a scratch buffer has no grammar");
    assert_eq!(state.get("lsp"), Some(&StateValue::Bool(false)));
}

#[test]
fn diff_files_toggles_the_diff_view() {
    let mut h = Stoat::test();
    assert_eq!(
        h.stoat.current_view(),
        Some("file"),
        "a plain editor starts outside the diff view"
    );
    action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    assert_eq!(
        h.stoat.current_view(),
        Some("diff"),
        "DiffFiles turns the diff view on"
    );
    action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    assert_eq!(
        h.stoat.current_view(),
        Some("file"),
        "DiffFiles again turns it back off"
    );
}

#[test]
fn escape_in_the_diff_view_stays_in_the_view() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::DiffFiles { rev: None });
    assert_eq!(h.stoat.current_view(), Some("diff"));

    h.type_keys("Escape");

    assert_eq!(
        h.stoat.current_view(),
        Some("diff"),
        "Escape does not leave the diff view"
    );
}

#[test]
fn from_stoat_commits_is_commits_view() {
    let mut h = Stoat::test();
    h.seed_linear_history(
        "/repo",
        &[
            ("c1", "init", &[("a.rs", "fn a() {}\n")]),
            ("c2", "more", &[("a.rs", "fn a() {}\nfn b() {}\n")]),
        ],
    );
    h.open_commits("/repo");
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(field(&state, "view"), Some("commits".to_string()));
    assert_eq!(
        field(&state, "mode"),
        Some("normal".to_string()),
        "the commits screen is a view, so the editor stays in normal mode"
    );
}

#[test]
fn from_stoat_modal_maps_topmost() {
    let mut h = Stoat::test();
    assert_eq!(StoatKeymapState::from_stoat(&h.stoat).get("modal"), None);

    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(field(&state, "modal"), Some("palette".to_string()));
}

/// The `scope` field names the open finder's scope and is absent without a
/// finder, so a `scope == x` binding holds only inside the finder.
#[test]
fn from_stoat_scope_names_the_open_finders_scope() {
    let scope_after = |actions: &[&dyn Action]| {
        let mut h = Stoat::test();
        for action in actions {
            action_handlers::dispatch(&mut h.stoat, *action);
        }
        StoatKeymapState::from_stoat(&h.stoat).get("scope").cloned()
    };
    let named = |s: &str| Some(StateValue::String(s.into()));
    assert_eq!(
        (
            scope_after(&[]),
            scope_after(&[&stoat_action::OpenBufferPicker]),
            scope_after(&[&stoat_action::OpenFileFinder]),
        ),
        (None, named("buffers"), named("all")),
    );
}

/// The palette's `scope` reads `pick` while a pick list shows, and is
/// absent over the command list. The refresh key holds only over a pick
/// list.
#[test]
fn the_scope_predicate_reads_pick_while_a_pick_list_shows() {
    let mut h = Stoat::test();
    h.stoat.reload_user_config(
        "command pr { arg pr = pick(\"gh pr list\"); run = shell(\"gh pr checkout {pr}\"); }",
    );
    let scope = |stoat: &Stoat| StoatKeymapState::from_stoat(stoat).get("scope").cloned();
    h.type_text(":pr");
    let listed = scope(&h.stoat);
    h.type_text(" ");
    assert_eq!(
        (listed, scope(&h.stoat)),
        (None, Some(StateValue::String("pick".into()))),
    );
}

/// `checkout` holds while a review walk is open, and not otherwise. A
/// `SetVar` of the same name is refused, so no user variable shadows it.
#[test]
fn checkout_holds_only_while_a_review_walk_is_open() {
    let mut h = Stoat::test();
    let checkout = |stoat: &Stoat| match StoatKeymapState::from_stoat(stoat).get("checkout") {
        Some(StateValue::Bool(open)) => Some(*open),
        _ => None,
    };
    let closed = checkout(&h.stoat);
    h.stoat.active_workspace_mut().review_walk = Some(ReviewWalk {
        workdir: "/repo".into(),
        commits: vec![CommitInfo {
            sha: "c1".into(),
            short_sha: "c1".into(),
            summary: String::new(),
            author_name: String::new(),
            author_email: String::new(),
            time: 0,
            parents: Vec::new(),
        }],
        cursor: 0,
        return_ref: ReturnRef::Branch("main".into()),
        span: WalkSpan::Commits,
        origin: WalkOrigin::Commits,
        stash: None,
    });
    let shadow = user_var(&resolved(
        "SetVar",
        vec![Value::Ident("checkout".into()), Value::Bool(true)],
    ));
    assert_eq!(
        (closed, checkout(&h.stoat), shadow),
        (
            Some(false),
            Some(true),
            Err("SetVar name `checkout` shadows a built-in field".to_string()),
        ),
    );
}

/// A `block` command's Enter turns the palette into its Run Block, whose
/// keys bind under `run_block` in the input's normal mode.
#[test]
fn from_stoat_modal_names_the_run_block() {
    let mut h = Stoat::test();
    h.stoat
        .reload_user_config(r#"command hello { run = shell("echo hi"); }"#);
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenCommandPalette);
    h.type_text("hello");
    h.type_keys("enter");

    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(
        (field(&state, "modal"), field(&state, "mode")),
        (Some("run_block".to_string()), Some("normal".to_string())),
    );
}

#[test]
fn from_stoat_modal_covers_text_inputs() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenSearchInput);
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(field(&state, "modal"), Some("isearch".to_string()));
}

/// The canonical order is only observable when two modals are somehow open at
/// once, which the keymap's `!modal` guards prevent. This pins what the app
/// does anyway if some path ever opens a second one.
#[test]
fn a_quit_prompt_outranks_a_finder_opened_under_it() {
    let mut h = Stoat::test();
    action_handlers::dispatch(&mut h.stoat, &stoat_action::OpenFileFinder);
    assert_eq!(
        active_modal(&h.stoat),
        Some(ActiveModal::FileFinder),
        "the finder alone is the active modal"
    );

    h.stoat.quit_all_confirm = Some(stoat::quit_all_confirm::QuitAllConfirm::new(
        &[],
        std::path::Path::new("/"),
    ));

    assert_eq!(
        active_modal(&h.stoat),
        Some(ActiveModal::QuitConfirm),
        "the prompt keys route to wins over the finder behind it"
    );
}

#[test]
fn user_var_reads_through_get_without_shadowing_builtins() {
    let mut h = Stoat::test();
    h.stoat
        .user_vars
        .insert("sidebar".into(), StateValue::String("on".into()));
    let state = StoatKeymapState::from_stoat(&h.stoat);
    assert_eq!(field(&state, "sidebar"), Some("on".to_string()));
    assert_eq!(field(&state, "mode"), Some("normal".to_string()));
}
