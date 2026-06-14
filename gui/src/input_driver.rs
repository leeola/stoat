//! Drive a parsed keystroke sequence into the live workspace for
//! `stoat --inputs`.
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

use crate::stoat_app::StoatApp;
use gpui::{App, Context, Keystroke, Window, WindowHandle};
use std::time::Duration;

const READINESS_DELAY: Duration = Duration::from_millis(300);
const INTER_KEY_DELAY: Duration = Duration::from_millis(20);

/// On the gpui foreground, drive `keystrokes` into `window`'s workspace
/// after a readiness delay, pacing the keys so each settles before the
/// next. Parsing happens at the binary entry (see `parse_input_sequence`
/// re-exported from the crate root) so a bad `--inputs` argument exits
/// non-zero before the window opens. Driving stops early if the window
/// closes.
pub(crate) fn drive_inputs(
    cx: &mut App,
    window: WindowHandle<StoatApp>,
    keystrokes: Vec<Keystroke>,
) {
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

    #[test]
    fn colon_escape_colon_drives_palette_open_each_time() {
        use crate::{
            command_palette::CommandPaletteDelegate,
            globals::{ExecutorGlobal, FsWatchHostGlobal},
            input_parse::parse_input_sequence,
            picker::Picker,
            workspace::Workspace,
        };
        use gpui::{Entity, TestAppContext};
        use std::{path::PathBuf, sync::Arc};
        use stoat::host::FsWatchHost;
        use stoat_host::NoopFsWatcher;
        use stoat_scheduler::{Executor, TestScheduler};

        let mut cx = TestAppContext::single();
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
        });
        let (ws, vcx): (Entity<Workspace>, _) = cx
            .add_window_view(|_window, cx| Workspace::new("main", PathBuf::from("/tmp/repo"), cx));

        let keystrokes = parse_input_sequence(":<Esc>:").expect("parse");
        for keystroke in &keystrokes {
            ws.update_in(vcx, |workspace, window, cx| {
                let sm = workspace.input_state_machine().clone();
                let actions = if is_printable_bare_char(keystroke) {
                    sm.update(cx, |sm, cx| sm.text_input(&keystroke.key, None, window, cx))
                } else {
                    sm.update(cx, |sm, cx| sm.feed(keystroke, window, cx))
                };
                for action in actions {
                    workspace.dispatch_action(action, window, cx);
                }
            });
            vcx.run_until_parked();
        }

        let picker = ws
            .read_with(vcx, |w, cx| {
                w.modal_layer()
                    .read(cx)
                    .active_modal::<Picker<CommandPaletteDelegate>>()
            })
            .expect("the second ':' must reopen the command palette after the Escape dismiss");
        let query_text = picker.read_with(vcx, |p, cx| {
            p.query_editor()
                .read(cx)
                .multi_buffer()
                .read(cx)
                .snapshot()
                .text()
                .to_string()
        });
        assert_eq!(
            query_text, "",
            "neither ':' open should leak its commit into the picker query editor",
        );
    }
}
