use gpui::TestAppContext;
use stoat::test::app::TestApp;

#[gpui::test]
fn command_palette_typing(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);

    app.type_input("<Space>o");
    app.flush();
    insta::assert_snapshot!("open-claude", app.snapshot_layout());
    insta::assert_snapshot!("claude-initial", app.snapshot_active());

    app.type_input("i");
    insta::assert_snapshot!("insert-mode", app.snapshot_active());

    app.type_input("foo");
    insta::assert_snapshot!("typed-foo", app.snapshot_active());

    app.type_input("<Esc>");
    insta::assert_snapshot!("escaped-insert", app.snapshot_active());

    app.type_input(":");
    app.flush();
    insta::assert_snapshot!("command-palette", app.snapshot_active());

    app.type_input("test");
    insta::assert_snapshot!("palette-typing", app.snapshot_active());
}

#[gpui::test]
fn escape_then_pane_switch(cx: &mut TestAppContext) {
    let mut app = TestApp::new_with_text("original", cx);

    app.type_input("<Space>o");
    app.flush();
    insta::assert_snapshot!("layout", app.snapshot_layout());

    app.type_input("i");
    app.type_input("hello");
    insta::assert_snapshot!("claude-typing", app.snapshot_active());

    app.type_input("<Esc>");
    insta::assert_snapshot!("after-first-esc", app.snapshot_active());

    app.type_input("<Esc>");
    insta::assert_snapshot!("after-second-esc", app.snapshot_active());

    app.type_input("<Space>ah");
    app.flush();
    insta::assert_snapshot!("switched-to-editor", app.snapshot_active());

    app.type_input("iworld<Esc>");
    insta::assert_snapshot!("editor-typing", app.snapshot_active());
}

#[gpui::test]
fn overlay_dismiss_restores_context(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);

    app.type_input("<Space>o");
    app.flush();
    insta::assert_snapshot!("claude-open", app.snapshot_active());

    app.type_input(":");
    app.flush();
    insta::assert_snapshot!("palette-open", app.snapshot_active());

    app.type_input("<Esc>");
    app.flush();
    insta::assert_snapshot!("palette-dismissed", app.snapshot_active());

    app.type_input("i");
    insta::assert_snapshot!("restored-insert", app.snapshot_active());
}

#[gpui::test]
fn input_focus_transitions(cx: &mut TestAppContext) {
    let mut app = TestApp::new(cx);

    app.type_input("<Space>o");
    app.flush();
    insta::assert_snapshot!("initial", app.snapshot_active());

    app.type_input("i");
    insta::assert_snapshot!("focus-input-1", app.snapshot_active());

    app.type_input("<Esc>");
    insta::assert_snapshot!("input-normal-1", app.snapshot_active());

    app.type_input("<Esc>");
    insta::assert_snapshot!("unfocus-input-1", app.snapshot_active());

    app.type_input("i");
    insta::assert_snapshot!("focus-input-2", app.snapshot_active());

    app.type_input("hello");
    insta::assert_snapshot!("typed-hello", app.snapshot_active());

    app.type_input("<Esc>");
    insta::assert_snapshot!("input-normal-2", app.snapshot_active());

    app.type_input("<Esc>");
    insta::assert_snapshot!("unfocus-input-2", app.snapshot_active());
}
