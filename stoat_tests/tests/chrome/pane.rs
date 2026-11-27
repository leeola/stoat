#[test]
fn snapshot_split_right() {
    let mut h = stoat::Stoat::test();
    h.type_action("SplitRight()");
    h.assert_snapshot("split_right");
}

#[test]
fn snapshot_split_down() {
    let mut h = stoat::Stoat::test();
    h.type_action("SplitDown()");
    h.assert_snapshot("split_down");
}

#[test]
fn snapshot_nested_splits() {
    let mut h = stoat::Stoat::test();
    h.type_action("SplitRight()");
    h.type_action("SplitDown()");
    h.assert_snapshot("nested_splits");
}

#[test]
fn snapshot_three_columns() {
    let mut h = stoat::Stoat::test();
    h.type_action("SplitRight()");
    h.type_action("SplitRight()");
    h.assert_snapshot("three_columns");
}

#[test]
fn snapshot_close_returns_to_single() {
    let mut h = stoat::Stoat::test();
    h.type_action("SplitRight()");
    h.type_action("ClosePane()");
    h.assert_snapshot("close_returns_to_single");
}

#[test]
fn snapshot_close_other_panes_from_three() {
    let mut h = stoat::Stoat::test();
    h.type_action("SplitRight()");
    h.type_action("SplitRight()");
    h.type_action("CloseOtherPanes()");
    h.assert_snapshot("close_other_panes_from_three");
}

#[test]
fn snapshot_quit_closes_one_split() {
    let mut h = stoat::Stoat::test();
    h.type_action("SplitRight()");
    h.type_action("Quit()");
    h.assert_snapshot("quit_closes_one_split");
}

#[test]
fn snapshot_split_right_focus_left() {
    let mut h = stoat::Stoat::test();
    h.type_action("SplitRight()");
    h.type_action("FocusLeft()");
    h.assert_snapshot("split_right_focus_left");
}
