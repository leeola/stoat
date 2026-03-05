use gpui::TestAppContext;
use stoat::test::headless::HeadlessStoat;

#[gpui::test]
fn open_conflict_review(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("merge-conflict", cx);

    app.type_input("<Space>x");
    app.flush();
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn accept_ours(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("merge-conflict", cx);

    app.type_input("<Space>x");
    app.flush();
    insta::assert_snapshot!("before", app.snapshot_active());

    app.type_input("o");
    insta::assert_snapshot!("after-accept-ours", app.snapshot_active());
}

#[gpui::test]
fn accept_theirs(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("merge-conflict", cx);

    app.type_input("<Space>x");
    app.flush();

    app.type_input("t");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn accept_both(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("merge-conflict", cx);

    app.type_input("<Space>x");
    app.flush();

    app.type_input("b");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn navigate_conflicts(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("merge-conflict", cx);

    app.type_input("<Space>x");
    app.flush();
    insta::assert_snapshot!("initial", app.snapshot_active());

    app.type_input("j");
    insta::assert_snapshot!("after-j", app.snapshot_active());

    app.type_input("k");
    insta::assert_snapshot!("after-k", app.snapshot_active());
}

#[gpui::test]
fn dismiss_conflict_review(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("merge-conflict", cx);

    app.type_input("<Space>x");
    app.flush();
    insta::assert_snapshot!("in-review", app.snapshot_active());

    app.type_input("<Esc>");
    insta::assert_snapshot!("dismissed", app.snapshot_active());
}
