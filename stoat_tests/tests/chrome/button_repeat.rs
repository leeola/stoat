use crossterm::event::{Event, KeyCode, KeyEvent, MouseButton, MouseEventKind};
use std::time::Duration;
use stoat::{
    app::{deliver_window_event, Stoat},
    button_repeat::{DEFAULT_DELAY, DEFAULT_INTERVAL},
    keymap::Keymap,
    test_fixture::{mouse_event, open_scratch_file},
    test_harness::TestHarness,
};
use stoat_config::{RepeatButton, Settings};
use stoatty_protocol::window_ipc::{MouseButton as IpcMouseButton, MouseKind, WindowIpcEvent};

/// A harness over a ten-line buffer whose forward side button moves the
/// cursor down in normal mode.
fn forward_moves_down() -> TestHarness {
    let mut h = Stoat::test();
    open_scratch_file(&mut h, "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n");
    let (config, errors) =
        stoat_config::parse("on key { mode == normal { MouseForward -> MoveDown(); } }");
    assert!(errors.is_empty(), "parse errors: {errors:?}");
    h.stoat.keymap = Keymap::compile(&config.expect("config"));
    h
}

fn forward(h: &mut TestHarness, kind: fn(IpcMouseButton) -> MouseKind) {
    deliver_window_event(
        &mut h.stoat,
        WindowIpcEvent::Mouse {
            window: 0,
            kind: kind(IpcMouseButton::Forward),
            col: 0,
            row: 0,
            mods: 0,
        },
    );
}

/// Advance the clock by `by`, run one background pass, and read the
/// cursor's display row.
fn row_after(h: &mut TestHarness, by: Duration) -> u32 {
    h.advance_clock(by);
    h.stoat.drive_background();
    h.cursor_display_positions()[0].0
}

#[test]
fn a_held_side_button_repeats_after_the_delay_and_then_each_interval() {
    let mut h = forward_moves_down();
    let ms = Duration::from_millis(1);

    forward(&mut h, MouseKind::Press);
    let mut rows = vec![row_after(&mut h, Duration::ZERO)];
    rows.push(row_after(&mut h, DEFAULT_DELAY - ms));
    rows.push(row_after(&mut h, ms));
    rows.push(row_after(&mut h, DEFAULT_INTERVAL));
    forward(&mut h, MouseKind::Release);
    rows.push(row_after(&mut h, DEFAULT_INTERVAL));

    assert_eq!(
        rows,
        [1, 1, 2, 3, 3],
        "row after the press, before the delay, at the delay, one interval on, \
         and one interval after the release"
    );
}

#[test]
fn only_a_listed_button_arms_a_repeat() {
    let left_down = mouse_event(MouseEventKind::Down(MouseButton::Left), 0, 0);
    let left_drag = mouse_event(MouseEventKind::Drag(MouseButton::Left), 1, 0);

    let mut unlisted = Stoat::test();
    unlisted.stoat.update(left_down.clone());

    let mut listed = TestHarness::with_settings(Settings {
        mouse_repeat_buttons: Some(vec![RepeatButton::Left]),
        ..Settings::default()
    });
    listed.stoat.update(left_down);
    let armed = listed.stoat.button_repeat.is_some();
    listed.stoat.update(left_drag);

    assert_eq!(
        [
            unlisted.stoat.button_repeat.is_some(),
            armed,
            listed.stoat.button_repeat.is_some(),
        ],
        [false, true, false],
        "an unlisted press, a listed press, and the drag after it"
    );
}

#[test]
fn a_key_press_cancels_the_repeat() {
    let mut h = forward_moves_down();
    forward(&mut h, MouseKind::Press);
    let armed = h.stoat.button_repeat.is_some();
    h.stoat
        .update(Event::Key(KeyEvent::from(KeyCode::Char('j'))));

    assert_eq!(
        [armed, h.stoat.button_repeat.is_some()],
        [true, false],
        "repeat armed by the press, then after a key"
    );
}
