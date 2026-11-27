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

pub const DEFAULT_DELAY: Duration = Duration::from_millis(500);
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(40);
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
pub struct ButtonRepeat {
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
