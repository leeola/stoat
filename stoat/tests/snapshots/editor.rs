use gpui::TestAppContext;
use stoat::test::headless::HeadlessStoat;

#[gpui::test]
fn new_empty(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new(cx);
    insta::assert_snapshot!(app.snapshot_layout(), @"[editor*]");
}

#[gpui::test]
fn new_with_text_snapshot(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("hello world", cx);
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn insert_and_escape(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("hello world", cx);

    app.type_input("i");
    insta::assert_snapshot!("after-i", app.snapshot_active());

    app.type_input("Hi ");
    insta::assert_snapshot!("after-typing", app.snapshot_active());

    app.type_input("<Esc>");
    insta::assert_snapshot!("after-escape", app.snapshot_active());
}

#[gpui::test]
fn visual_selection(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::new_with_text("hello world", cx);
    app.type_input("viw");
    insta::assert_snapshot!(app.snapshot_active());
}

#[gpui::test]
fn center_screen(cx: &mut TestAppContext) {
    let lines: Vec<String> = (0..100).map(|i| format!("Line {i}")).collect();
    let mut app = HeadlessStoat::new_with_text(&lines.join("\n"), cx);

    app.type_input("50j");
    insta::assert_snapshot!("before-zz", app.snapshot_active());

    app.type_input("zz");
    insta::assert_snapshot!("after-zz", app.snapshot_active());
}
