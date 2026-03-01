use crate::pane_group::view::PaneGroupView;
use gpui::{Context, Window};

impl PaneGroupView {
    pub(crate) fn handle_dump_env(&mut self, _window: &mut Window, cx: &mut Context<'_, Self>) {
        let env_guard = self.app_state.project_env.read();
        let content = match env_guard.as_ref() {
            Some(env) => {
                let mut vars: Vec<_> = env.vars().iter().collect();
                vars.sort_by_key(|(k, _)| *k);
                let mut buf = String::new();
                for (key, value) in vars {
                    buf.push_str(key);
                    buf.push('=');
                    buf.push_str(value);
                    buf.push('\n');
                }
                buf
            },
            None => "Environment not yet captured.\n".to_string(),
        };
        drop(env_guard);

        let editor_opt = self.active_editor().cloned();
        if let Some(editor) = editor_opt {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.load_scratch_buffer("[env]", &content, cx);
                });
            });
        }
    }
}
