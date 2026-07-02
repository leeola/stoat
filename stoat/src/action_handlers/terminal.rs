use crate::{
    app::{Stoat, UpdateEffect},
    host::terminal::TerminalSession,
    pane::View,
    run::{spawn_term_reader, spawn_terminal},
    term_screen::TermScreen,
    term_session::TermSession,
};
use futures::FutureExt;
use std::sync::Arc;

/// Dimensions the terminal PTY opens at before the render/resize pass fits it
/// to the focused pane.
const TERM_ROWS: u16 = 24;
const TERM_COLS: u16 = 80;

/// Open a subshell in the focused pane.
///
/// Resolves the program and arguments from the `terminal.shell` /
/// `terminal.args` settings (falling back to `$SHELL`, then `/bin/sh`), spawns
/// it through the terminal host, stores it alongside a fresh screen emulator,
/// and points the focused pane at the new [`View::Terminal`]. A spawn failure
/// leaves the pane unchanged.
pub(super) fn open_terminal_pane(stoat: &mut Stoat) -> UpdateEffect {
    let (program, args) = resolve_shell(
        stoat.settings.terminal_shell.as_deref(),
        stoat.settings.terminal_args.as_deref(),
        std::env::var("SHELL").ok(),
    );

    let host = stoat.terminal_host.clone();
    let executor = stoat.executor.clone();
    let pty_tx = stoat.pty_tx.clone();
    let ws = stoat.active_workspace_mut();
    let cwd = ws.git_root.clone();

    // The local terminal host opens the PTY synchronously, so the spawn future
    // is ready on first poll, matching the claude pane's spawn path.
    let session = match spawn_terminal(&*host, &cwd, &program, &args).now_or_never() {
        Some(Ok(session)) => session,
        Some(Err(err)) => {
            tracing::warn!(target: "stoat::terminal", %err, "failed to spawn terminal session");
            return UpdateEffect::None;
        },
        None => return UpdateEffect::None,
    };

    let session: Arc<dyn TerminalSession> = Arc::from(session);
    let term_id = ws.terms.insert(TermSession {
        term: TermScreen::new(TERM_ROWS, TERM_COLS),
        session: session.clone(),
    });
    let focused = ws.panes.focus();
    ws.panes.pane_mut(focused).view = View::Terminal(term_id);

    spawn_term_reader(&executor, session, term_id, pty_tx);
    UpdateEffect::Redraw
}

/// Resolve the shell program and arguments for a new terminal pane.
///
/// The program is the `terminal.shell` setting when set, otherwise the
/// `$SHELL` environment value, otherwise `/bin/sh`. Arguments come only from
/// the `terminal.args` setting, so a program resolved from the environment or
/// the `/bin/sh` fallback launches with none.
fn resolve_shell(
    settings_shell: Option<&str>,
    settings_args: Option<&[String]>,
    env_shell: Option<String>,
) -> (String, Vec<String>) {
    let program = settings_shell
        .map(str::to_owned)
        .or(env_shell)
        .unwrap_or_else(|| "/bin/sh".to_owned());
    let args = settings_args.map(|a| a.to_vec()).unwrap_or_default();
    (program, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn resolve_shell_precedence() {
        assert_eq!(
            resolve_shell(
                Some("/bin/zsh"),
                Some(&args(&["-l"])),
                Some("/bin/bash".into())
            ),
            ("/bin/zsh".to_string(), args(&["-l"])),
            "settings win over env",
        );
        assert_eq!(
            resolve_shell(None, None, Some("/bin/bash".into())),
            ("/bin/bash".to_string(), vec![]),
            "env shell used when unset, with no default args",
        );
        assert_eq!(
            resolve_shell(None, None, None),
            ("/bin/sh".to_string(), vec![]),
            "final fallback is /bin/sh",
        );
        assert_eq!(
            resolve_shell(None, Some(&args(&["-x"])), Some("/bin/bash".into())),
            ("/bin/bash".to_string(), args(&["-x"])),
            "args come from settings independent of program source",
        );
    }

    #[test]
    fn terminal_action_opens_terminal_pane() {
        let mut h = Stoat::test();
        let fake = Arc::new(crate::host::FakeTerminalSession::new());
        h.stoat.terminal_host = Arc::new(crate::host::FakeTerminalHost::new(fake));

        super::super::dispatch(&mut h.stoat, &stoat_action::Terminal);

        let ws = h.stoat.active_workspace();
        let focused = ws.panes.focus();
        let View::Terminal(term_id) = ws.panes.pane(focused).view else {
            panic!("focused pane should hold a terminal view");
        };
        assert!(
            ws.terms.contains_key(term_id),
            "spawned terminal session is stored",
        );
    }
}
