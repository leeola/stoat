// FIXME: DiffReviewEnterLineSelect and line_select mode keybindings are not
// yet in keymap.stcfg. These tests verify the formatter infrastructure and
// will show line selection output once bindings are added.
//
// The `S` keystroke in diff_review context maps to GitToggleStageLine (instant
// stage/unstage), not DiffReviewEnterLineSelect (line selection mode).

use gpui::TestAppContext;
use stoat::test::headless::HeadlessStoat;

#[gpui::test]
fn stage_line_in_diff_review(cx: &mut TestAppContext) {
    let mut app = HeadlessStoat::with_fixture("basic-diff", cx);

    app.type_input("<Space>r");
    app.flush();
    insta::assert_snapshot!("before-stage", app.snapshot_active());

    app.type_input("S");
    insta::assert_snapshot!("after-stage", app.snapshot_active());
}
