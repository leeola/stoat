use gpui::TestAppContext;
use stoat::test::headless::HeadlessStoat;

#[gpui::test]
fn open_git_status(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>g");
    app.flush();
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn navigate_files(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("multi-file-diff", cx);

    app.type_input("<Space>g");
    app.flush();
    insta::assert_snapshot!("initial", app.snapshot_active());

    app.type_input("j");
    insta::assert_snapshot!("after-j", app.snapshot_active());

    app.type_input("k");
    insta::assert_snapshot!("after-k", app.snapshot_active());
}

#[gpui::test]
fn cycle_filter(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("staged-and-unstaged", cx);

    app.type_input("<Space>g");
    app.flush();
    insta::assert_snapshot!("all", app.snapshot_active());

    // Enter filter mode and set staged filter
    app.type_input("fs");
    insta::assert_snapshot!("staged", app.snapshot_active());

    // Enter filter mode and set unstaged filter
    app.type_input("fu");
    insta::assert_snapshot!("unstaged", app.snapshot_active());
}

#[gpui::test]
fn dismiss_restores(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>g");
    app.flush();
    insta::assert_snapshot!("git-status-open", app.snapshot_active());

    app.type_input("<Esc>");
    app.flush();
    insta::assert_snapshot!("dismissed", app.snapshot_active());
}
