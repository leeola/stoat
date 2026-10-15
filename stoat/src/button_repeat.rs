//! A held pointer button that repeats its press, the way a held key repeats.
//!
//! The platform repeats a held key but never a held button, so one press is
//! all a hold produces. A press of a button in `mouse.repeat_buttons` arms a
//! repeat here. The repeat replays the press through the entry it took, first
//! after `mouse.repeat_delay` and then once per `mouse.repeat_interval`, until
//! a release, a drag, a press of another button, a key press, or a
//! window-socket disconnect ends it.
//!
//! A replay goes through the same entry as the press, so a bound side button
//! re-resolves its binding in the mode of the moment, and an unbound one walks
//! the jumplist.

use crate::{app::Stoat, mouse};
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use std::time::{Duration, Instant};
use stoat_config::RepeatButton;
use stoat_scheduler::Task;
use stoatty_protocol::window_ipc::{MouseButton as IpcMouseButton, MouseKind};

const DEFAULT_DELAY: Duration = Duration::from_millis(500);
const DEFAULT_INTERVAL: Duration = Duration::from_millis(40);
const DEFAULT_BUTTONS: [RepeatButton; 2] = [RepeatButton::Back, RepeatButton::Forward];

/// The press a repeat replays, in the shape of the entry it arrived through.
#[derive(Clone, Copy)]
pub(crate) enum PressSource {
    /// A press the terminal reported in-band.
    Terminal(MouseEvent),
    /// A press stoatty reported over the window socket.
    Window {
        window: u32,
        kind: MouseKind,
        col: u16,
        row: u16,
        mods: u8,
    },
}

/// The armed repeat of a held button.
pub(crate) struct ButtonRepeat {
    source: PressSource,
    /// When the next replay is due, on the scheduler clock.
    due: Instant,
    /// Wakes the run loop at `due`. Dropping it cancels the wake.
    _timer: Task<()>,
}

/// Arm or end the repeat for a pointer gesture from a real entry.
///
/// A press of a listed button arms a repeat, and a press of any other button
/// ends one. A release or a drag ends it too, and a wheel leaves it alone.
/// The replays of [`drain`] do not come through here, so a replay never
/// restarts the delay.
pub(crate) fn note_press(stoat: &mut Stoat, source: PressSource) {
    let Some((button, pressed)) = gesture(source) else {
        return;
    };

    let listed = stoat
        .settings
        .mouse_repeat_buttons
        .as_deref()
        .unwrap_or(&DEFAULT_BUTTONS)
        .contains(&button);
    if !pressed || !listed {
        stoat.button_repeat = None;
        return;
    }

    let delay = millis_or(stoat.settings.mouse_repeat_delay, DEFAULT_DELAY);
    stoat.button_repeat = Some(arm(stoat, source, delay));
}

/// Replay the held press when its repeat is due, then arm the next replay one
/// interval out.
///
/// Returns whether a replay ran. The woken frame repaints what the replay
/// changed, so the replay's own effect goes unread.
pub(crate) fn drain(stoat: &mut Stoat) -> bool {
    let Some(repeat) = &stoat.button_repeat else {
        return false;
    };
    if stoat.executor.now() < repeat.due {
        return false;
    }

    let source = repeat.source;
    match source {
        PressSource::Terminal(event) => {
            mouse::handle_mouse(stoat, event);
        },
        PressSource::Window {
            window,
            kind,
            col,
            row,
            mods,
        } => {
            stoat.handle_aux_mouse(window, kind, col, row, mods);
        },
    }

    if stoat.button_repeat.is_some() {
        let interval = millis_or(stoat.settings.mouse_repeat_interval, DEFAULT_INTERVAL);
        stoat.button_repeat = Some(arm(stoat, source, interval));
    }
    true
}

fn arm(stoat: &Stoat, source: PressSource, wait: Duration) -> ButtonRepeat {
    let timer = stoat.executor.timer(wait);
    ButtonRepeat {
        source,
        due: stoat.executor.now() + wait,
        _timer: stoat.spawn_woken(async move {
            timer.await;
        }),
    }
}

/// The button a gesture names, and whether the gesture presses it. `None` for
/// a gesture that names no button, such as a wheel notch or a bare move.
fn gesture(source: PressSource) -> Option<(RepeatButton, bool)> {
    match source {
        PressSource::Terminal(event) => match event.kind {
            MouseEventKind::Down(button) => Some((terminal_button(button), true)),
            MouseEventKind::Up(button) | MouseEventKind::Drag(button) => {
                Some((terminal_button(button), false))
            },
            _ => None,
        },
        PressSource::Window { kind, .. } => match kind {
            MouseKind::Press(button) => Some((window_button(button), true)),
            MouseKind::Release(button) | MouseKind::Drag(button) => {
                Some((window_button(button), false))
            },
            MouseKind::WheelUp | MouseKind::WheelDown => None,
        },
    }
}

fn terminal_button(button: MouseButton) -> RepeatButton {
    match button {
        MouseButton::Left => RepeatButton::Left,
        MouseButton::Middle => RepeatButton::Middle,
        MouseButton::Right => RepeatButton::Right,
    }
}

fn window_button(button: IpcMouseButton) -> RepeatButton {
    match button {
        IpcMouseButton::Left => RepeatButton::Left,
        IpcMouseButton::Middle => RepeatButton::Middle,
        IpcMouseButton::Right => RepeatButton::Right,
        IpcMouseButton::Back => RepeatButton::Back,
        IpcMouseButton::Forward => RepeatButton::Forward,
    }
}

fn millis_or(setting: Option<u32>, default: Duration) -> Duration {
    setting.map_or(default, |ms| Duration::from_millis(u64::from(ms)))
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_DELAY, DEFAULT_INTERVAL};
    use crate::{
        app::{deliver_window_event, Stoat},
        keymap::Keymap,
        test_fixture::{mouse_event, open_scratch_file},
        test_harness::TestHarness,
    };
    use crossterm::event::{Event, KeyCode, KeyEvent, MouseButton, MouseEventKind};
    use std::time::Duration;
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
}
