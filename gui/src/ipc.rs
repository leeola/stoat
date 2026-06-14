//! IPC verb dispatch for the app socket.
//!
//! The accept loop (in the transport crate) hands each request to a forwarding
//! handler that pushes it onto a channel; [`spawn_dispatch`] drains that
//! channel on the gpui foreground -- where verb handlers may touch windows and
//! entities -- and answers each request from there. Verbs route a client's
//! request into the live session enclosing its working directory, opening a new
//! window when none matches.

use crate::{app_host::AppHost, stoat_app::StoatApp, RestoreMode, Workspace};
use gpui::{
    px, size, App, AppContext, Bounds, Entity, SharedString, Task, TitlebarOptions, WindowBounds,
    WindowOptions,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use stoat::{buffer::BufferId, workspace::WorkspaceUid};
use stoat_agent_claude_code::jsonrpc::{IncomingRequest, RpcError, METHOD_NOT_FOUND};
use tokio::sync::mpsc::UnboundedReceiver;

/// JSON-RPC 2.0 reserved code for malformed parameters.
const INVALID_PARAMS: i64 = -32602;
/// JSON-RPC 2.0 reserved code for an internal server error.
const INTERNAL_ERROR: i64 = -32603;

/// `open_file` request: the client's working directory (for session routing)
/// and the absolute path to open. `session` targets a live session by uid and
/// `new` forces a fresh window; absent both, the path routes to the session
/// enclosing `cwd`.
#[derive(Deserialize)]
struct OpenFileParams {
    cwd: std::path::PathBuf,
    path: std::path::PathBuf,
    #[serde(default)]
    new: bool,
    #[serde(default)]
    session: Option<u64>,
}

/// `open_file` reply: the session the path routed into and the buffer it
/// opened.
#[derive(Serialize)]
struct OpenFileResult {
    session_id: WorkspaceUid,
    buffer_id: BufferId,
}

/// Drain `requests` on the gpui foreground, answering each by running
/// [`dispatch`] on the main thread. Returns the foreground task; hold it for
/// the process lifetime so dispatch keeps running.
pub fn spawn_dispatch(cx: &mut App, mut requests: UnboundedReceiver<IncomingRequest>) -> Task<()> {
    cx.spawn(async move |cx| {
        while let Some(request) = requests.recv().await {
            let result = cx
                .update(|app| dispatch(app, &request.method, request.params.clone()))
                .unwrap_or_else(|_| Err(error(INTERNAL_ERROR, "app shut down")));
            let _ = request.respond(result);
        }
    })
}

fn dispatch(app: &mut App, method: &str, params: Option<Value>) -> Result<Value, RpcError> {
    match method {
        "open_file" => open_file(app, params),
        other => Err(error(
            METHOD_NOT_FOUND,
            format!("method not found: {other}"),
        )),
    }
}

fn open_file(app: &mut App, params: Option<Value>) -> Result<Value, RpcError> {
    let params: OpenFileParams = serde_json::from_value(params.unwrap_or(Value::Null))
        .map_err(|err| error(INVALID_PARAMS, format!("open_file params: {err}")))?;

    let (session_id, buffer_id) = match resolve_target(app, &params)? {
        Some((uid, workspace)) => {
            workspace.update(app, |w, cx| {
                w.open_paths(std::slice::from_ref(&params.path), cx)
            });
            (uid, buffer_id_for(&workspace, &params.path, app))
        },
        None => open_new_session(app, &params.cwd, &params.path)?,
    };

    let buffer_id = buffer_id.ok_or_else(|| error(INTERNAL_ERROR, "opened buffer has no id"))?;
    serde_json::to_value(OpenFileResult {
        session_id,
        buffer_id,
    })
    .map_err(|err| error(INTERNAL_ERROR, format!("encode result: {err}")))
}

/// The existing session a request routes into, or `None` when it should open a
/// fresh window. `session` looks up a live session by uid (erroring if it is
/// gone), `new` always opens fresh, and otherwise the cwd-enclosing session is
/// used when one exists.
fn resolve_target(
    app: &App,
    params: &OpenFileParams,
) -> Result<Option<(WorkspaceUid, Entity<Workspace>)>, RpcError> {
    if let Some(id) = params.session {
        let uid = WorkspaceUid(id);
        let workspace = app
            .global::<AppHost>()
            .session_workspace(uid, app)
            .ok_or_else(|| error(INVALID_PARAMS, format!("session {id} is not live")))?;
        return Ok(Some((uid, workspace)));
    }
    if params.new {
        return Ok(None);
    }
    let host = app.global::<AppHost>();
    Ok(host
        .resolve_cwd(&params.cwd, app)
        .and_then(|uid| host.session_workspace(uid, app).map(|ws| (uid, ws))))
}

/// Open a new window rooted at `cwd` with `path` loaded, returning the new
/// session's uid and the opened buffer id. The new `StoatApp` registers itself
/// with the [`AppHost`], so a later request from the same cwd reuses it.
fn open_new_session(
    app: &mut App,
    cwd: &Path,
    path: &Path,
) -> Result<(WorkspaceUid, Option<BufferId>), RpcError> {
    let bounds = Bounds::centered(None, size(px(1200.0), px(800.0)), app);
    let window = app
        .open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Stoat")),
                    ..Default::default()
                }),
                ..Default::default()
            },
            {
                let cwd = cwd.to_path_buf();
                let path = path.to_path_buf();
                move |window, cx| {
                    cx.new(|cx| StoatApp::new_at(cwd, vec![path], RestoreMode::None, window, cx))
                }
            },
        )
        .map_err(|err| error(INTERNAL_ERROR, format!("open window: {err}")))?;

    window
        .update(app, |stoat_app, _window, cx| {
            let workspace = stoat_app.workspace().clone();
            let uid = workspace.read(cx).uid();
            (uid, buffer_id_for(&workspace, path, cx))
        })
        .map_err(|err| error(INTERNAL_ERROR, format!("read new window: {err}")))
}

fn buffer_id_for(workspace: &Entity<Workspace>, path: &Path, cx: &App) -> Option<BufferId> {
    let registry = workspace.read(cx).buffer_registry().clone();
    registry.read(cx).id_for_path(path)
}

fn error(code: i64, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        data: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::{ExecutorGlobal, FsHostGlobal, FsWatchHostGlobal};
    use gpui::{BorrowAppContext, TestAppContext, VisualContext};
    use std::{path::PathBuf, sync::Arc};
    use stoat::host::{FakeFs, FsHost, FsWatchHost};
    use stoat_host::NoopFsWatcher;
    use stoat_scheduler::{Executor, TestScheduler};

    fn install_globals(cx: &mut TestAppContext) {
        cx.update(|cx| {
            cx.set_global(ExecutorGlobal(Executor::new(
                Arc::new(TestScheduler::new()),
            )));
            cx.set_global(FsWatchHostGlobal(
                Arc::new(NoopFsWatcher::new()) as Arc<dyn FsWatchHost>
            ));
            cx.set_global(FsHostGlobal(Arc::new(FakeFs::new()) as Arc<dyn FsHost>));
            cx.set_global(AppHost::default());
        });
    }

    fn open_file_params(cwd: &str, path: &str) -> Option<Value> {
        Some(serde_json::json!({ "cwd": cwd, "path": path }))
    }

    /// Open a window rooted at `root` and register it as a live session,
    /// returning the workspace and its uid.
    fn register_session(cx: &mut TestAppContext, root: &str) -> (Entity<Workspace>, WorkspaceUid) {
        let (workspace, vcx) =
            cx.add_window_view(|_window, cx| Workspace::new(root.to_string(), root.into(), cx));
        let handle = vcx.window_handle();
        let registered = workspace.clone();
        cx.update(|app| {
            app.update_global::<AppHost, _>(|host, cx| host.add_session(registered, handle, cx));
        });
        let uid = cx.update(|app| workspace.read(app).uid());
        (workspace, uid)
    }

    #[test]
    fn open_file_routes_into_the_cwd_matched_session() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx);

        let (workspace, vcx) = cx
            .add_window_view(|_window, cx| Workspace::new("repo".to_string(), "/repo".into(), cx));
        let handle = vcx.window_handle();
        let uid = workspace.read_with(&cx, |w, _| w.uid());
        cx.update(|app| {
            let workspace = workspace.clone();
            app.update_global::<AppHost, _>(|host, cx| host.add_session(workspace, handle, cx));
        });

        let result = cx
            .update(|app| dispatch(app, "open_file", open_file_params("/repo", "/repo/x.txt")))
            .expect("open_file succeeds");

        assert_eq!(result["session_id"], serde_json::json!(uid.0));
        assert!(result["buffer_id"].is_u64(), "result carries a buffer id");

        let opened = workspace.read_with(&cx, |w, cx| {
            w.buffer_registry()
                .read(cx)
                .id_for_path(Path::new("/repo/x.txt"))
                .is_some()
        });
        assert!(opened, "the path opened in the matched workspace");
    }

    #[test]
    fn open_file_opens_a_new_window_when_unmatched() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx);

        let before = cx.update(|app| app.windows().len());
        let result = cx
            .update(|app| dispatch(app, "open_file", open_file_params("/fresh", "/fresh/y.txt")))
            .expect("open_file opens a new session");
        let after = cx.update(|app| app.windows().len());

        assert_eq!(after, before + 1, "an unmatched cwd opens a new window");
        assert!(result["session_id"].is_u64());
        assert!(result["buffer_id"].is_u64());

        let resolved = cx.update(|app| {
            app.global::<AppHost>()
                .resolve_cwd(&PathBuf::from("/fresh"), app)
        });
        assert!(resolved.is_some(), "the new session registers at its cwd");
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx);

        let err = cx
            .update(|app| dispatch(app, "no_such_verb", None))
            .expect_err("unknown method rejected");
        assert_eq!(err.code, METHOD_NOT_FOUND);
    }

    #[test]
    fn open_file_new_forces_a_fresh_window() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx);
        let (_workspace, cwd_uid) = register_session(&mut cx, "/repo");

        let before = cx.update(|app| app.windows().len());
        let params = serde_json::json!({ "cwd": "/repo", "path": "/repo/x.txt", "new": true });
        let result = cx
            .update(|app| dispatch(app, "open_file", Some(params)))
            .expect("open_file --new succeeds");
        let after = cx.update(|app| app.windows().len());

        assert_eq!(
            after,
            before + 1,
            "--new opens a fresh window despite a matching session",
        );
        assert_ne!(
            result["session_id"],
            serde_json::json!(cwd_uid.0),
            "--new routes to a new session, not the cwd-matched one",
        );
    }

    #[test]
    fn open_file_session_routes_to_the_targeted_session() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx);
        let (workspace, uid) = register_session(&mut cx, "/repo");

        // The cwd does not enclose the session; --session targets it by uid.
        let params =
            serde_json::json!({ "cwd": "/other", "path": "/other/y.txt", "session": uid.0 });
        let result = cx
            .update(|app| dispatch(app, "open_file", Some(params)))
            .expect("open_file --session succeeds");

        assert_eq!(
            result["session_id"],
            serde_json::json!(uid.0),
            "routed to the targeted session",
        );
        let opened = workspace.read_with(&cx, |w, cx| {
            w.buffer_registry()
                .read(cx)
                .id_for_path(Path::new("/other/y.txt"))
                .is_some()
        });
        assert!(opened, "the path opened in the targeted session");
    }

    #[test]
    fn open_file_session_errors_when_not_live() {
        let mut cx = TestAppContext::single();
        install_globals(&mut cx);

        let params = serde_json::json!({ "cwd": "/repo", "path": "/repo/x.txt", "session": 404 });
        let err = cx
            .update(|app| dispatch(app, "open_file", Some(params)))
            .expect_err("a dead session is rejected");
        assert_eq!(err.code, INVALID_PARAMS);
    }
}
