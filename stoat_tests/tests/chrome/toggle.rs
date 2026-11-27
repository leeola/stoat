use stoat::{
    action_handlers::{dispatch, focused_editor_mut},
    test_fixture::action,
    toggle::{binding_toggle, Toggle, ToggleStates},
    Stoat,
};
use stoat_action::{Conflict, ToggleMinimap, TogglePaneWiden, ToggleTabBar, ToggleWrap};

/// Every toggle that is on in `stoat`, in [`Toggle::ALL`] order.
fn on(stoat: &Stoat) -> Vec<Toggle> {
    let states = ToggleStates::read(stoat);
    Toggle::ALL
        .into_iter()
        .filter(|toggle| states.is_on(*toggle))
        .collect()
}

#[test]
fn toggle_states_read_the_session_flags() {
    let mut h = Stoat::test();
    assert_eq!(
        on(&h.stoat),
        [
            Toggle::ChangeWalkWrap,
            Toggle::DiffSyntax,
            Toggle::SyntaxHighlight,
            Toggle::Wrap,
            Toggle::Minimap,
            Toggle::PaneFade,
            Toggle::RelativeNumbers,
        ],
        "the flags a lone scratch pane starts with",
    );

    h.stoat.follow_changes = true;
    h.stoat.live_reload = true;
    h.stoat.diff_underline = true;
    h.stoat.diff_bold = true;
    h.stoat.diff_syntax = false;
    h.stoat.inlay_hints_enabled = true;
    assert_eq!(
        on(&h.stoat),
        [
            Toggle::FollowChanges,
            Toggle::LiveReload,
            Toggle::FollowReload,
            Toggle::ChangeWalkWrap,
            Toggle::DiffUnderline,
            Toggle::DiffBold,
            Toggle::SyntaxHighlight,
            Toggle::InlayHints,
            Toggle::Wrap,
            Toggle::Minimap,
            Toggle::PaneFade,
            Toggle::RelativeNumbers,
        ],
    );
}

/// The pair toggle reads on only while both of the modes it flips are on.
#[test]
fn follow_reload_reads_on_only_while_both_modes_are() {
    let mut h = Stoat::test();
    let reads = [(true, false), (false, true), (true, true)].map(|(follow, reload)| {
        h.stoat.follow_changes = follow;
        h.stoat.live_reload = reload;
        Toggle::FollowReload.is_on(&h.stoat)
    });
    assert_eq!(reads, [false, false, true]);
}

#[test]
fn toggle_states_read_the_view_flags() {
    let mut h = Stoat::test();
    dispatch(&mut h.stoat, &TogglePaneWiden);
    focused_editor_mut(&mut h.stoat)
        .expect("editor")
        .set_diff_view(true);
    dispatch(&mut h.stoat, &ToggleWrap);
    dispatch(&mut h.stoat, &ToggleMinimap);
    dispatch(&mut h.stoat, &ToggleTabBar);
    {
        let ws = h.stoat.active_workspace_mut();
        let focus = ws.panes.focus();
        ws.panes.pane_mut(focus).blame = true;
    }

    assert_eq!(
        on(&h.stoat),
        [
            Toggle::ChangeWalkWrap,
            Toggle::DiffSyntax,
            Toggle::SyntaxHighlight,
            Toggle::DiffView,
            Toggle::PaneWiden,
            Toggle::TabBar,
            Toggle::PaneFade,
            Toggle::RelativeNumbers,
            Toggle::Blame,
        ],
        "each view toggle reads the state its action flipped",
    );
}

/// The `space d c` row reads as the changes view toggle, which is on while
/// a changes view has the focus.
#[test]
fn the_changes_view_toggle_reads_the_focused_pane() {
    let mut h = Stoat::test();
    let reads = |stoat: &Stoat| ToggleStates::read(stoat).is_on(Toggle::ChangesView);

    let before = reads(&h.stoat);
    h.cover_with_diff_changes(false);
    assert_eq!(
        (
            binding_toggle(&[action("DiffChanges"), action("SetMode")]),
            before,
            reads(&h.stoat)
        ),
        (Some(Toggle::ChangesView), false, true),
    );
}

/// The `space d x` row reads as the conflict view toggle, which is on while
/// the workspace holds a conflict session.
#[test]
fn the_conflict_view_toggle_reads_the_session() {
    let mut h = Stoat::test();
    let git_root = h.stoat.active_workspace().git_root.clone();
    h.fake_git().add_repo(git_root).conflicted_file(
        "f.txt",
        Some("base\n"),
        Some("ours\n"),
        Some("theirs\n"),
    );
    let reads = |stoat: &Stoat| ToggleStates::read(stoat).is_on(Toggle::ConflictView);

    let before = reads(&h.stoat);
    dispatch(&mut h.stoat, &Conflict);
    h.settle();
    assert_eq!(
        (
            binding_toggle(&[action("Conflict"), action("SetMode")]),
            before,
            reads(&h.stoat)
        ),
        (Some(Toggle::ConflictView), false, true),
    );
}
