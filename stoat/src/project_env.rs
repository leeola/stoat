//! Per-workspace project environment loaded from direnv.
//!
//! Each workspace runs `direnv export json` against its root once,
//! off-thread, and stores the resulting set/unset diff. The diff is the
//! difference between stoat's own environment and the environment direnv
//! would apply, so it can be replayed onto child spawns. A transient
//! message reports the outcome.
//!
//! [`spawn_load`] is fire-and-forget. It detaches a task that writes its
//! result into [`crate::app::Stoat::pending_env`], and [`install_pending`]
//! drains that slot on the next background pump.

use crate::{app::Stoat, host::ShellOutput, workspace::WorkspaceId};
use std::{collections::HashMap, io, path::Path};

/// The exit code `sh` returns when the command it runs is not found on
/// PATH, which for `direnv export json` means direnv is not installed.
const DIRENV_NOT_INSTALLED_EXIT: i32 = 127;

/// The message stored for a not-installed direnv, matched at install time
/// to keep the auto path quiet.
const DIRENV_NOT_FOUND: &str = "direnv not found on PATH";

/// Progress of a workspace's direnv environment load.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum EnvLoadState {
    /// No load attempted yet.
    #[default]
    Unloaded,
    /// A load is in flight on a background task.
    Loading,
    /// A load finished (successfully or not); the diff reflects the last
    /// successful result, or is empty after an error.
    Loaded,
    /// Automatic loading is disabled for this workspace by settings, so no
    /// load runs until an explicit reload.
    Off,
}

/// A workspace's resolved project environment.
#[derive(Debug, Clone, Default)]
pub(crate) struct WorkspaceEnv {
    pub(crate) state: EnvLoadState,
    /// Set/unset overrides to apply onto child spawns. `Some` sets the
    /// variable, `None` unsets it. Sorted by key.
    pub(crate) diff: Vec<(String, Option<String>)>,
}

/// A finished background load waiting to be installed onto its workspace.
///
/// The load records the workspace id rather than a reference so the
/// install can find the right workspace even if the active one changed
/// while the load ran, and drop the result if that workspace is gone.
pub(crate) struct PendingEnvLoad {
    pub(crate) workspace: WorkspaceId,
    /// Whether the load was user-requested. Manual loads always report
    /// their outcome. Automatic loads stay quiet for the non-actionable
    /// cases.
    pub(crate) manual: bool,
    pub(crate) outcome: Result<Vec<(String, Option<String>)>, String>,
}

/// Parse `direnv export json` stdout into a diff sorted by key.
///
/// The payload is a flat JSON object mapping each changed variable to its
/// new value, or `null` to unset it.
pub(crate) fn parse_direnv_export(bytes: &[u8]) -> Result<Vec<(String, Option<String>)>, String> {
    let map: HashMap<String, Option<String>> =
        serde_json::from_slice(bytes).map_err(|e| format!("parse direnv export: {e}"))?;
    let mut diff: Vec<(String, Option<String>)> = map.into_iter().collect();
    diff.sort();
    Ok(diff)
}

/// Spawn the direnv load for `ws_id` on a background task, marking the
/// workspace [`EnvLoadState::Loading`]. No-op when the workspace is gone.
///
/// The detached task runs `direnv export json` in the workspace root via
/// the shell host and parks its classified outcome in
/// [`Stoat::pending_env`] for [`install_pending`] to drain.
pub(crate) fn spawn_load(stoat: &mut Stoat, ws_id: WorkspaceId, manual: bool) {
    let git_root = {
        let Some(ws) = stoat.workspaces.get_mut(ws_id) else {
            return;
        };
        ws.env.state = EnvLoadState::Loading;
        ws.git_root.clone()
    };

    let shell_host = stoat.shell_host.clone();
    let pending = stoat.pending_env.clone();
    let executor = stoat.executor.clone();

    stoat
        .spawn_woken(async move {
            let result = executor
                .spawn_blocking(move || {
                    shell_host.run("TERM=dumb direnv export json", b"", Some(&git_root), &[])
                })
                .await;
            *pending.lock().expect("pending env mutex") = Some(PendingEnvLoad {
                workspace: ws_id,
                manual,
                outcome: classify(result),
            });
        })
        .detach();
}

/// Start the active workspace's automatic direnv load if it has not been
/// attempted yet.
///
/// No-op unless [`Stoat::env_auto_load`] is on, so the test harness never
/// fires direnv. Parks the workspace [`EnvLoadState::Off`] instead of
/// loading when `direnv.load` is disabled.
pub(crate) fn ensure_loaded(stoat: &mut Stoat) {
    if !stoat.env_auto_load {
        return;
    }
    let ws_id = stoat.active_workspace;
    let Some(ws) = stoat.workspaces.get(ws_id) else {
        return;
    };
    if ws.env.state != EnvLoadState::Unloaded {
        return;
    }

    if stoat.settings.direnv_load.unwrap_or(true) {
        spawn_load(stoat, ws_id, false);
    } else if let Some(ws) = stoat.workspaces.get_mut(ws_id) {
        ws.env.state = EnvLoadState::Off;
    }
}

/// Force a reload of the active workspace's project environment.
///
/// Backs the `ReloadEnv` action. Unlike [`ensure_loaded`], it ignores
/// both `direnv.load` and the workspace's current state, because the
/// user invoking it is explicit intent. A reload therefore runs even
/// when automatic loading is disabled or a previous load already
/// finished. A load already in flight is left alone and only reported.
pub(crate) fn reload_active_workspace(stoat: &mut Stoat) {
    let ws_id = stoat.active_workspace;
    let in_flight = stoat
        .workspaces
        .get(ws_id)
        .is_some_and(|ws| ws.env.state == EnvLoadState::Loading);

    if in_flight {
        stoat.set_status("direnv: reload already running");
        return;
    }

    stoat.set_status("direnv: reloading...");
    spawn_load(stoat, ws_id, true);
}

/// Install a finished background load onto its workspace and report it.
///
/// Drains [`Stoat::pending_env`], a no-op when nothing finished. On
/// success the diff replaces the workspace's env. On error the env is
/// cleared. Either way the state becomes [`EnvLoadState::Loaded`]. The
/// transient message follows the outcome and whether the load was manual.
///
/// A language-server spawn deferred while the env was loading (see
/// [`crate::action_handlers::lsp::maybe_spawn_language_server`]) is re-fired
/// here, now with the installed diff.
pub(crate) fn install_pending(stoat: &mut Stoat) {
    let pending = stoat.pending_env.lock().expect("pending env mutex").take();
    let Some(PendingEnvLoad {
        workspace,
        manual,
        outcome,
    }) = pending
    else {
        return;
    };

    let unset_on_exit = stoat.settings.direnv_unset_on_exit.unwrap_or(false);

    let message = {
        let Some(ws) = stoat.workspaces.get_mut(workspace) else {
            return;
        };
        match outcome {
            Ok(diff) => {
                let (diff, message) = resolve_ok_diff(diff, &ws.git_root, manual, unset_on_exit);
                ws.env.diff = diff;
                ws.env.state = EnvLoadState::Loaded;
                message
            },
            Err(err) => {
                ws.env.diff = Vec::new();
                ws.env.state = EnvLoadState::Loaded;
                err_message(&err, manual)
            },
        }
    };

    if let Some(message) = message {
        stoat.set_status(message);
    }

    // A language-server spawn deferred while the env loaded now runs with the
    // freshly installed diff.
    if let Some(buffer_id) = stoat.lsp_spawn_deferred.take() {
        crate::action_handlers::lsp::maybe_spawn_language_server(stoat, buffer_id);
    }
}

/// Classify a shell-host run of `direnv export json` into a diff or an
/// error string.
fn classify(result: io::Result<ShellOutput>) -> Result<Vec<(String, Option<String>)>, String> {
    let out = result.map_err(|e| e.to_string())?;
    if out.exit_code == DIRENV_NOT_INSTALLED_EXIT {
        return Err(DIRENV_NOT_FOUND.to_string());
    }
    if out.exit_code != 0 {
        return Err(last_stderr_line(&out.stderr));
    }
    if String::from_utf8_lossy(&out.stdout).trim().is_empty() {
        return Ok(Vec::new());
    }
    parse_direnv_export(&out.stdout)
}

/// The last non-empty, trimmed line of stderr, which for direnv carries
/// the actionable hint (e.g. `.envrc is blocked`). Falls back to a
/// generic message when stderr is empty.
fn last_stderr_line(stderr: &[u8]) -> String {
    String::from_utf8_lossy(stderr)
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .unwrap_or("direnv failed")
        .to_string()
}

/// Resolve a successful direnv diff into the env diff to install and the
/// transient message to show.
///
/// A diff that unsets `DIRENV_FILE` is a revert of the inherited environment:
/// direnv found no `.envrc` governing the root and wants to strip the vars
/// stoat inherited. A real load always sets `DIRENV_FILE` to the `.envrc` path
/// while leaving always unsets it, so keying on `DIRENV_FILE` recognizes a
/// leave even when it also sets `PATH` and `XDG_DATA_DIRS` back to their
/// pre-envrc values, which an all-unset test would miss.
///
/// Unless `unset_on_exit` opts in, that revert is skipped, installing an empty
/// diff so child spawns keep inheriting stoat's process env and reporting a
/// keeping-inherited message so the user sees why. Every other diff installs
/// unchanged with the normal [`ok_message`].
fn resolve_ok_diff(
    diff: Vec<(String, Option<String>)>,
    git_root: &Path,
    manual: bool,
    unset_on_exit: bool,
) -> (Vec<(String, Option<String>)>, Option<String>) {
    let is_revert = diff
        .iter()
        .any(|(key, value)| key == "DIRENV_FILE" && value.is_none());

    if is_revert && !unset_on_exit {
        let message = format!(
            "direnv: keeping inherited env (no .envrc at {})",
            git_root.display()
        );
        return (Vec::new(), Some(message));
    }

    let message = ok_message(&diff, git_root, manual);
    (diff, message)
}

/// The transient message for a successful load, or `None` to stay quiet.
///
/// An empty diff is reported only for a manual reload. A non-empty diff
/// reports its variable and unset counts and its source, which is direnv's
/// `DIRENV_FILE` value when present, else the workspace root.
fn ok_message(diff: &[(String, Option<String>)], git_root: &Path, manual: bool) -> Option<String> {
    if diff.is_empty() {
        return manual.then(|| "direnv: no changes".to_string());
    }
    let unset = diff.iter().filter(|(_, value)| value.is_none()).count();
    let source = diff
        .iter()
        .find(|(key, _)| key == "DIRENV_FILE")
        .and_then(|(_, value)| value.clone())
        .unwrap_or_else(|| git_root.display().to_string());
    Some(format!(
        "direnv: {} vars ({} unset) from {}",
        diff.len(),
        unset,
        source
    ))
}

/// The transient message for a failed load, or `None` to stay quiet.
///
/// A not-installed direnv is a debug log rather than a message on the
/// automatic path, so a machine without direnv is not nagged. A manual
/// reload always reports. Every other error is always reported.
fn err_message(err: &str, manual: bool) -> Option<String> {
    if !manual && err == DIRENV_NOT_FOUND {
        tracing::debug!(target: "stoat::direnv", "{err}");
        return None;
    }
    Some(format!("direnv: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{host::FakeShell, test_harness::TestHarness};
    use std::{path::PathBuf, sync::Arc};

    fn setup(h: &mut TestHarness, response: ShellOutput) -> Arc<FakeShell> {
        let fake = Arc::new(FakeShell::new());
        fake.set_response("TERM=dumb direnv export json", response);
        h.stoat.set_shell_host(fake.clone());
        h.stoat.set_env_auto_load(true);
        h.stoat.active_workspace_mut().git_root = PathBuf::from("/proj");
        fake
    }

    fn out(stdout: &[u8], stderr: &[u8], exit_code: i32) -> ShellOutput {
        ShellOutput {
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
            exit_code,
        }
    }

    #[test]
    fn json_diff_lands_with_message() {
        let mut h = TestHarness::with_size(80, 24);
        setup(
            &mut h,
            out(
                br#"{"FOO":"bar","BAZ":null,"DIRENV_FILE":"/proj/.envrc"}"#,
                b"",
                0,
            ),
        );
        ensure_loaded(&mut h.stoat);
        h.settle();
        install_pending(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.env.state, EnvLoadState::Loaded);
        assert_eq!(
            ws.env.diff,
            vec![
                ("BAZ".to_string(), None),
                ("DIRENV_FILE".to_string(), Some("/proj/.envrc".to_string())),
                ("FOO".to_string(), Some("bar".to_string())),
            ]
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("direnv: 3 vars (1 unset) from /proj/.envrc")
        );
    }

    #[test]
    fn revert_diff_keeps_inherited_env_by_default() {
        let mut h = TestHarness::with_size(80, 24);
        setup(&mut h, out(br#"{"FOO":null,"DIRENV_FILE":null}"#, b"", 0));
        ensure_loaded(&mut h.stoat);
        h.settle();
        install_pending(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.env.state, EnvLoadState::Loaded);
        assert!(
            ws.env.diff.is_empty(),
            "a pure-revert diff is skipped so children keep the inherited env"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("direnv: keeping inherited env (no .envrc at /proj)")
        );
    }

    #[test]
    fn revert_diff_applies_when_unset_on_exit_opts_in() {
        let mut h = TestHarness::with_size(80, 24);
        setup(&mut h, out(br#"{"FOO":null,"DIRENV_FILE":null}"#, b"", 0));
        h.stoat.settings.direnv_unset_on_exit = Some(true);
        ensure_loaded(&mut h.stoat);
        h.settle();
        install_pending(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            ws.env.diff,
            vec![("DIRENV_FILE".to_string(), None), ("FOO".to_string(), None),]
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("direnv: 2 vars (2 unset) from /proj")
        );
    }

    #[test]
    fn mixed_leave_diff_keeps_inherited_env() {
        let mut h = TestHarness::with_size(80, 24);
        setup(
            &mut h,
            out(
                br#"{"PATH":"/usr/bin","CC":null,"DIRENV_FILE":null}"#,
                b"",
                0,
            ),
        );
        ensure_loaded(&mut h.stoat);
        h.settle();
        install_pending(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.env.state, EnvLoadState::Loaded);
        assert!(
            ws.env.diff.is_empty(),
            "a leave diff that also sets PATH is still a revert, so children keep the inherited env"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("direnv: keeping inherited env (no .envrc at /proj)")
        );
    }

    #[test]
    fn mixed_leave_diff_applies_when_unset_on_exit_opts_in() {
        let mut h = TestHarness::with_size(80, 24);
        setup(
            &mut h,
            out(
                br#"{"PATH":"/usr/bin","CC":null,"DIRENV_FILE":null}"#,
                b"",
                0,
            ),
        );
        h.stoat.settings.direnv_unset_on_exit = Some(true);
        ensure_loaded(&mut h.stoat);
        h.settle();
        install_pending(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        assert_eq!(
            ws.env.diff,
            vec![
                ("CC".to_string(), None),
                ("DIRENV_FILE".to_string(), None),
                ("PATH".to_string(), Some("/usr/bin".to_string())),
            ]
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("direnv: 3 vars (2 unset) from /proj")
        );
    }

    #[test]
    fn blocked_envrc_stderr_surfaces_as_message() {
        let mut h = TestHarness::with_size(80, 24);
        setup(
            &mut h,
            out(
                b"",
                b"direnv: error /proj/.envrc is blocked. Run `direnv allow`.",
                1,
            ),
        );
        ensure_loaded(&mut h.stoat);
        h.settle();
        install_pending(&mut h.stoat);

        assert_eq!(h.stoat.active_workspace().env.state, EnvLoadState::Loaded);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("direnv: direnv: error /proj/.envrc is blocked. Run `direnv allow`.")
        );
    }

    #[test]
    fn empty_stdout_stays_silent() {
        let mut h = TestHarness::with_size(80, 24);
        setup(&mut h, out(b"", b"", 0));
        ensure_loaded(&mut h.stoat);
        h.settle();
        install_pending(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.env.state, EnvLoadState::Loaded);
        assert!(ws.env.diff.is_empty());
        assert_eq!(h.stoat.pending_message, None);
    }

    #[test]
    fn direnv_load_disabled_parks_off_without_running() {
        let mut h = TestHarness::with_size(80, 24);
        let fake = setup(&mut h, out(b"{}", b"", 0));
        h.stoat.settings.direnv_load = Some(false);
        ensure_loaded(&mut h.stoat);
        h.settle();

        assert_eq!(h.stoat.active_workspace().env.state, EnvLoadState::Off);
        assert!(
            fake.invocations().is_empty(),
            "direnv must not run when disabled"
        );
    }

    #[test]
    fn reload_env_runs_and_swaps_diff_even_when_load_disabled() {
        let mut h = TestHarness::with_size(80, 24);
        let fake = setup(&mut h, out(br#"{"FOO":"new"}"#, b"", 0));
        h.stoat.settings.direnv_load = Some(false);
        h.stoat.active_workspace_mut().env.diff =
            vec![("OLD".to_string(), Some("stale".to_string()))];

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReloadEnv);
        assert_eq!(h.stoat.active_workspace().env.state, EnvLoadState::Loading);
        h.settle();
        install_pending(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.env.state, EnvLoadState::Loaded);
        assert_eq!(
            ws.env.diff,
            vec![("FOO".to_string(), Some("new".to_string()))]
        );
        assert_eq!(fake.invocations().len(), 1, "reload must run direnv once");
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("direnv: 1 vars (0 unset) from /proj")
        );
    }

    #[test]
    fn cd_reloads_env_for_the_new_root() {
        let mut h = TestHarness::with_size(80, 24);
        let fake = setup(
            &mut h,
            out(br#"{"OLD":"1","DIRENV_FILE":"/proj/.envrc"}"#, b"", 0),
        );
        h.stoat.active_workspace_mut().env.diff = vec![("OLD".to_string(), Some("1".to_string()))];
        h.fake_fs().insert_dir("/proj2");

        fake.set_response(
            "TERM=dumb direnv export json",
            out(br#"{"NEW":"2","DIRENV_FILE":"/proj2/.envrc"}"#, b"", 0),
        );
        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::SetCwd {
                path: "/proj2".to_string(),
            },
        );
        h.settle();
        install_pending(&mut h.stoat);

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.git_root, PathBuf::from("/proj2"));
        assert_eq!(
            ws.env.diff,
            vec![
                ("DIRENV_FILE".to_string(), Some("/proj2/.envrc".to_string())),
                ("NEW".to_string(), Some("2".to_string())),
            ]
        );
        assert_eq!(
            fake.invocations().last().and_then(|i| i.cwd.clone()),
            Some(PathBuf::from("/proj2")),
            "direnv reran in the new root"
        );
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("direnv: 2 vars (0 unset) from /proj2/.envrc")
        );
    }

    #[test]
    fn cd_with_reload_disabled_clears_diff_without_running() {
        let mut h = TestHarness::with_size(80, 24);
        let fake = setup(
            &mut h,
            out(br#"{"NEW":"2","DIRENV_FILE":"/proj2/.envrc"}"#, b"", 0),
        );
        h.stoat.settings.direnv_reload_on_cd = Some(false);
        h.stoat.active_workspace_mut().env.diff = vec![("OLD".to_string(), Some("1".to_string()))];
        h.fake_fs().insert_dir("/proj2");

        crate::action_handlers::dispatch(
            &mut h.stoat,
            &stoat_action::SetCwd {
                path: "/proj2".to_string(),
            },
        );
        h.settle();

        let ws = h.stoat.active_workspace();
        assert_eq!(ws.git_root, PathBuf::from("/proj2"));
        assert!(
            ws.env.diff.is_empty(),
            "the old root's diff is cleared even when the reload is disabled"
        );
        assert!(
            fake.invocations().is_empty(),
            "reload_on_cd = false runs no direnv"
        );
    }

    #[test]
    fn reload_env_while_loading_only_messages() {
        let mut h = TestHarness::with_size(80, 24);
        let fake = setup(&mut h, out(br#"{"FOO":"bar"}"#, b"", 0));
        h.stoat.active_workspace_mut().env.state = EnvLoadState::Loading;

        crate::action_handlers::dispatch(&mut h.stoat, &stoat_action::ReloadEnv);

        assert_eq!(h.stoat.active_workspace().env.state, EnvLoadState::Loading);
        assert_eq!(
            h.stoat.pending_message.as_deref(),
            Some("direnv: reload already running")
        );
        assert!(
            fake.invocations().is_empty(),
            "in-flight reload must not re-run direnv"
        );
    }
}
