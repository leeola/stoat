use gpui::TestAppContext;
use stoat::test::headless::HeadlessStoat;

#[gpui::test]
fn open_blame(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("blame-test", cx);

    app.type_input("<Space>gb");
    app.flush();
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn toggle_author(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("blame-test", cx);

    app.type_input("<Space>gb");
    app.flush();
    app.type_input("a");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn toggle_date(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("blame-test", cx);

    app.type_input("<Space>gb");
    app.flush();
    app.type_input("d");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn toggle_both(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("blame-test", cx);

    app.type_input("<Space>gb");
    app.flush();
    app.type_input("a");
    app.type_input("d");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn blame_detail_popup(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("blame-test", cx);

    app.type_input("<Space>gb");
    app.flush();
    app.type_input("jj");
    app.type_input("i");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn dismiss_blame(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("blame-test", cx);

    app.type_input("<Space>gb");
    app.flush();
    app.type_input("q");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn git_mode_status(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>gs");
    app.flush();
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn blame_per_line_hashes(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("blame-test", cx);

    app.type_input("<Space>gb");
    app.flush();

    let line_to_entry = app.blame_line_to_entry().expect("blame data should exist");

    // 8 lines in the file
    assert_eq!(line_to_entry.len(), 8, "should have 8 lines");

    // Lines 0, 3, 4 are from first commit (Alice initial) -> entry 0
    // Lines 1, 2 are from second commit (Bob modify) -> entry 1
    // Lines 5, 6, 7 are from third commit (Alice extend) -> entry 2
    assert_eq!(
        line_to_entry[0], line_to_entry[3],
        "line 0 and 3 same author"
    );
    assert_eq!(
        line_to_entry[0], line_to_entry[4],
        "line 0 and 4 same author"
    );
    assert_eq!(
        line_to_entry[1], line_to_entry[2],
        "line 1 and 2 same author"
    );
    assert_eq!(
        line_to_entry[5], line_to_entry[6],
        "line 5 and 6 same author"
    );
    assert_eq!(
        line_to_entry[5], line_to_entry[7],
        "line 5 and 7 same author"
    );

    // Different authors
    assert_ne!(line_to_entry[0], line_to_entry[1], "Alice vs Bob");
    assert_ne!(line_to_entry[1], line_to_entry[5], "Bob vs Alice (extend)");
}
