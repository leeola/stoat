//! Drive a parsed keystroke sequence into the live workspace for
//! `stoat gui --inputs`.
//!
//! After a readiness delay (so the workspace, IME handler, and
//! active-editor wiring are live), each keystroke is fed on the gpui
//! foreground: printable bare characters take the macOS IME path through
//! [`InputStateMachine::text_input`] (normal-mode bindings dispatch,
//! insert-mode text reaches the cursor), while named or modified keys go
//! through [`InputStateMachine::feed`]. Either way the resolved actions
//! dispatch via [`Workspace::dispatch_action`], mirroring the keystroke
//! observer. Driving never uses `Window::dispatch_keystroke`, which would
//! re-route printable keys to the focused input handler and double-fire
//! them under Stoat's no-gpui-bindings setup.

use crate::{input_parse::parse_input_sequence, stoat_app::StoatApp};
use gpui::{App, Context, Keystroke, Window, WindowHandle};
use std::time::Duration;

const READINESS_DELAY: Duration = Duration::from_millis(300);
const INTER_KEY_DELAY: Duration = Duration::from_millis(20);

/// Parse `inputs` and, on the gpui foreground, drive the resulting
/// keystroke sequence into `window`'s workspace after a readiness delay,
/// pacing the keys so each settles before the next. A parse failure is
/// logged and drives nothing; driving stops early if the window closes.
pub(crate) fn drive_inputs(cx: &mut App, window: WindowHandle<StoatApp>, inputs: String) {
    let keystrokes = match parse_input_sequence(&inputs) {
        Ok(keystrokes) => keystrokes,
        Err(err) => {
            tracing::error!(error = %err, "failed to parse --inputs sequence");
            return;
        },
    };

    cx.spawn(async move |cx| {
        cx.background_executor().timer(READINESS_DELAY).await;
        for keystroke in keystrokes {
            cx.background_executor().timer(INTER_KEY_DELAY).await;
            let drove = window.update(cx, |app, window, cx| {
                drive_keystroke(app, &keystroke, window, cx);
            });
            if drove.is_err() {
                break;
            }
        }
    })
    .detach();
}

fn drive_keystroke(
    app: &mut StoatApp,
    keystroke: &Keystroke,
    window: &mut Window,
    cx: &mut Context<'_, StoatApp>,
) {
    let workspace = app.workspace().clone();
    workspace.update(cx, |workspace, cx| {
        let state_machine = workspace.input_state_machine().clone();
        let actions = if is_printable_bare_char(keystroke) {
            state_machine.update(cx, |sm, cx| sm.text_input(&keystroke.key, None, window, cx))
        } else {
            state_machine.update(cx, |sm, cx| sm.feed(keystroke, window, cx))
        };
        for action in actions {
            workspace.dispatch_action(action, window, cx);
        }
    });
}

/// A keystroke that the macOS IME would deliver as committed text: a
/// single-character key with no control, alt, or platform modifier.
fn is_printable_bare_char(keystroke: &Keystroke) -> bool {
    let mut chars = keystroke.key.chars();
    let single_char = chars.next().is_some() && chars.next().is_none();
    single_char
        && !keystroke.modifiers.control
        && !keystroke.modifiers.alt
        && !keystroke.modifiers.platform
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::Modifiers;

    fn key(key: &str, modifiers: Modifiers) -> Keystroke {
        Keystroke {
            modifiers,
            key: key.to_string(),
            key_char: None,
        }
    }

    #[test]
    fn printable_bare_chars_route_to_text_input() {
        for ch in ["i", ":", "G", "w"] {
            assert!(
                is_printable_bare_char(&key(ch, Modifiers::default())),
                "{ch} should be a printable bare char",
            );
        }
    }

    #[test]
    fn shift_alone_stays_printable() {
        let shift = Modifiers {
            shift: true,
            ..Default::default()
        };
        assert!(is_printable_bare_char(&key("w", shift)));
    }

    #[test]
    fn named_keys_are_not_printable() {
        for name in ["space", "escape", "enter", "backspace", "up"] {
            assert!(
                !is_printable_bare_char(&key(name, Modifiers::default())),
                "{name} should not be a printable bare char",
            );
        }
    }

    #[test]
    fn control_alt_platform_disqualify() {
        for modifiers in [
            Modifiers {
                control: true,
                ..Default::default()
            },
            Modifiers {
                alt: true,
                ..Default::default()
            },
            Modifiers {
                platform: true,
                ..Default::default()
            },
        ] {
            assert!(!is_printable_bare_char(&key("w", modifiers)));
        }
    }
}
