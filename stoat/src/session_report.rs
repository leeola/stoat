//! JSON reports of live session state for the `stoat api` clients, built on
//! the event loop.

use crate::{app::Stoat, workspace::WorkspaceUid};
use serde_json::{json, Value};
use std::{process, time::UNIX_EPOCH};

/// The `session-info` report on the workspace that `uid` names.
///
/// It names the session, where it is rooted, whether it is on screen, when it
/// was last active, and the file in its focused pane. The `uid` field is the
/// hex form that `STOAT_SESSION` carries. An unknown `uid` reports an error
/// object.
pub(crate) fn session_info(stoat: &Stoat, uid: WorkspaceUid) -> Value {
    let Some((id, ws)) = stoat.workspaces.iter().find(|(_, ws)| ws.uid == uid) else {
        return json!({ "error": "unknown session" });
    };

    let focus = ws
        .focused_editor()
        .and_then(|(_, buffer)| ws.buffers.path_for(buffer));
    let last_active_ms = ws
        .last_active
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_millis() as u64);

    json!({
        "uid": uid.to_string(),
        "pid": process::id(),
        "name": ws.display_name(),
        "git_root": ws.git_root,
        "repo_root": ws.repo_root,
        "active": id == stoat.active_workspace,
        "last_active_ms": last_active_ms,
        "tab": ws.active_tab,
        "tabs": ws.tabs.len(),
        "workspaces": stoat.workspaces.len(),
        "focus": focus,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        test_fixture::{open_buffer, seed},
        test_harness::TestHarness,
    };

    #[test]
    fn session_info_names_the_workspace_and_its_focused_file() {
        let mut h = TestHarness::with_size(80, 24);
        let root = seed(&mut h, &[("src/a.rs", "fn a() {}\n")]);
        open_buffer(&mut h, root.join("src/a.rs"));

        let ws = h.stoat.active_workspace();
        let last_active_ms = ws
            .last_active
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        assert_eq!(
            session_info(&h.stoat, ws.uid),
            json!({
                "uid": ws.uid.to_string(),
                "pid": process::id(),
                "name": "lsp-did-open-test",
                "git_root": "/lsp-did-open-test",
                "repo_root": null,
                "active": true,
                "last_active_ms": last_active_ms,
                "tab": 0,
                "tabs": 1,
                "workspaces": 1,
                "focus": "/lsp-did-open-test/src/a.rs",
            }),
        );
    }
}
