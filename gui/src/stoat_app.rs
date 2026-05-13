use crate::workspace::Workspace;
use gpui::{
    div, AppContext, Context, Entity, FocusHandle, IntoElement, ParentElement, Render,
    SharedString, Styled, Window,
};
use std::path::PathBuf;

pub(crate) struct StoatApp {
    workspace: Entity<Workspace>,
    #[allow(dead_code)]
    focus_handle: FocusHandle,
}

impl StoatApp {
    pub(crate) fn new(cx: &mut Context<'_, Self>) -> Self {
        let git_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let name = git_root
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| SharedString::from(s.to_string()))
            .unwrap_or_else(|| SharedString::from("stoat"));
        let workspace = cx.new(|cx| Workspace::new(name, git_root.clone(), cx));
        Self {
            workspace,
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Render for StoatApp {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<'_, Self>) -> impl IntoElement {
        div().size_full().child(self.workspace.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[test]
    fn new_constructs_workspace_anchored_to_cwd() {
        let mut cx = TestAppContext::single();
        let (app, _vcx) = cx.add_window_view(|_window, cx| StoatApp::new(cx));
        let cwd = std::env::current_dir().expect("current_dir");
        let expected_name: SharedString = cwd
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| SharedString::from(s.to_string()))
            .unwrap_or_else(|| SharedString::from("stoat"));

        app.read_with(&cx, |app, cx| {
            let ws = app.workspace.read(cx);
            assert_eq!(ws.git_root(), &cwd);
            assert_eq!(ws.name(), &expected_name);
        });
    }
}
