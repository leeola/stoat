use stoat::app::Stoat;

#[test]
fn question_mark_opens_help_in_normal_mode() {
    let mut h = Stoat::test();
    assert!(h.stoat.help.is_none(), "no help open by default");

    h.type_text("?");
    let all = h
        .stoat
        .help
        .as_ref()
        .expect("? opens help directly in normal mode")
        .filtered()
        .len();
    assert!(all > 0, "help lists the active bindings");

    h.type_text("quit");
    let filtered = h
        .stoat
        .help
        .as_ref()
        .expect("help stays open while typing")
        .filtered()
        .len();
    assert!(
        filtered < all,
        "typing into the help search filters the list ({all} -> {filtered})",
    );
}
