use crossterm::event::{KeyCode, KeyModifiers};
use stoat::{action_handlers, keymap::CompiledKey, test_harness::TestHarness};

/// A dial binding names a view, so the box lists the five chords beside
/// the other context keys of a diff surface, and none of them on a plain
/// pane, where no dial answers.
#[test]
fn the_hints_box_lists_the_diff_chords_on_a_diff_surface() {
    const DIALS: [(char, &str); 5] = [
        ('6', "underline changes"),
        ('7', "bold changes"),
        ('8', "syntax colors"),
        ('9', "tint down"),
        ('0', "tint up"),
    ];
    let mut h = TestHarness::default();
    h.stoat.key_hints_visible = true;
    let mut dial_rows = |diff_view: bool| {
        action_handlers::focused_editor_mut(&mut h.stoat)
            .expect("editor")
            .set_diff_view(diff_view);
        h.snapshot();
        h.stoat
            .hints_cache
            .as_ref()
            .expect("the hints box laid out its rows")
            .rows
            .iter()
            .filter(|row| DIALS.iter().any(|(_, action)| row.action == *action))
            .map(|row| (row.keys.clone(), row.action.clone()))
            .collect::<Vec<_>>()
    };
    let chord_label = |digit| {
        CompiledKey {
            code: KeyCode::Char(digit),
            modifiers: KeyModifiers::SUPER,
            any_digit: false,
            wheel: None,
            side_button: None,
        }
        .display_label()
    };

    assert_eq!(
        (dial_rows(true), dial_rows(false)),
        (
            DIALS
                .iter()
                .map(|&(digit, action)| (chord_label(digit), action.to_string()))
                .collect::<Vec<_>>(),
            Vec::new(),
        ),
        "the diff view lists each dial under its chord, and a plain pane lists none"
    );
}
