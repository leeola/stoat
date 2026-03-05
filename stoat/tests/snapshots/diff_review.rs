use gpui::TestAppContext;
use stoat::test::headless::HeadlessStoat;

#[gpui::test]
fn open_basic_diff(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>r");
    app.flush();
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn navigate_hunks(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>r");
    app.flush();
    insta::assert_snapshot!("at-hunk-1", app.snapshot_active());

    app.type_input("j");
    insta::assert_snapshot!("at-hunk-2", app.snapshot_active());

    app.type_input("k");
    insta::assert_snapshot!("back-to-hunk-1", app.snapshot_active());
}

#[gpui::test]
fn approve_hunk(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>r");
    app.flush();

    app.type_input("a");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn approve_all_dismisses(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>r");
    app.flush();

    app.type_input("a");
    insta::assert_snapshot!("after-first-approve", app.snapshot_active());

    app.type_input("a");
    insta::assert_snapshot!("after-all-approved", app.snapshot_active());
}

#[gpui::test]
fn cycle_comparison_mode(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>r");
    app.flush();
    insta::assert_snapshot!("all-changes", app.snapshot_active());

    app.type_input("c");
    insta::assert_snapshot!("unstaged", app.snapshot_active());

    app.type_input("c");
    insta::assert_snapshot!("staged", app.snapshot_active());

    app.type_input("c");
    insta::assert_snapshot!("last-commit", app.snapshot_active());
}

#[gpui::test]
fn dismiss_restores_mode(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>r");
    app.flush();
    insta::assert_snapshot!("in-review", app.snapshot_active());

    app.type_input("<Esc>");
    insta::assert_snapshot!("dismissed", app.snapshot_active());
}

#[gpui::test]
fn follow_toggle(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>r");
    app.flush();
    insta::assert_snapshot!("follow-off", app.snapshot_active());

    app.type_input("f");
    insta::assert_snapshot!("follow-on", app.snapshot_active());

    app.type_input("f");
    insta::assert_snapshot!("follow-off-again", app.snapshot_active());
}

#[gpui::test]
fn cross_file_navigation(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("multi-file-diff", cx);

    app.type_input("<Space>r");
    app.flush();
    insta::assert_snapshot!("first-file", app.snapshot_active());

    app.type_input("j");
    insta::assert_snapshot!("second-file", app.snapshot_active());
}

#[gpui::test]
fn staged_comparison(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("staged-and-unstaged", cx);

    app.type_input("<Space>r");
    app.flush();

    // Cycle to Staged (All -> Unstaged -> Staged)
    app.type_input("cc");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn unstaged_comparison(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("staged-and-unstaged", cx);

    app.type_input("<Space>r");
    app.flush();

    // Cycle to Unstaged
    app.type_input("c");
    insta::assert_snapshot!(app.snapshot_active());
}
