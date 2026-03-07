use crate::{
    content_view::PaneContent,
    environment::ProjectEnvironment,
    input_simulator::parse_input_sequence,
    keymap::dispatch::dispatch_editor_action,
    pane_group::view::PaneGroupView,
    test::app::{format_member, snapshot_claude, snapshot_editor},
    Stoat,
};
use gpui::{Entity, TestAppContext, VisualTestContext};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};
use stoat_config::{Action, ActionExpr};
use stoat_lsp::response::HoverBlock;
use tempfile::TempDir;

pub struct HeadlessStoat<'a> {
    pub view: Entity<PaneGroupView>,
    cx: &'a mut VisualTestContext,
    _temp_dir: TempDir,
    root: PathBuf,
}

impl<'a> HeadlessStoat<'a> {
    pub fn new(cx: &'a mut TestAppContext) -> Self {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let root = temp_dir
            .path()
            .canonicalize()
            .expect("canonicalize temp dir");

        let keymap = super::test_keymap();
        let config = crate::config::Config::default();

        let root_for_view = root.clone();
        let (view, cx) = cx.add_window_view(|_window, cx| {
            PaneGroupView::new(
                config,
                vec![],
                keymap,
                root_for_view,
                crate::services::Services::fake(),
                cx,
            )
        });

        cx.update(|_window, cx| {
            let pgv = view.read(cx);
            *pgv.app_state.project_env.write() = Some(ProjectEnvironment::from_current());
        });

        cx.update(|window, cx| {
            let handle = view.read(cx).active_editor_focus_handle(cx);
            if let Some(handle) = handle {
                window.focus(&handle, cx);
            }
        });

        Self {
            view,
            cx,
            _temp_dir: temp_dir,
            root,
        }
    }

    pub fn with_fixture(scenario: &str, cx: &'a mut TestAppContext) -> Self {
        let temp_dir = TempDir::new().expect("failed to create temp dir");
        let root = temp_dir
            .path()
            .canonicalize()
            .expect("canonicalize temp dir");

        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let fixture_patches_dir = manifest_dir.join("fixtures/git").join(scenario);
        let changed_files = git_fixture::init_and_apply(&fixture_patches_dir, &root)
            .unwrap_or_else(|e| panic!("failed to apply fixture {scenario}: {e}"));

        let keymap = super::test_keymap();
        let config = crate::config::Config::default();

        let root_for_view = root.clone();
        let initial_paths = changed_files
            .first()
            .cloned()
            .into_iter()
            .collect::<Vec<_>>();
        let (view, cx) = cx.add_window_view(|_window, cx| {
            PaneGroupView::new(
                config,
                initial_paths,
                keymap,
                root_for_view,
                crate::services::Services::fake(),
                cx,
            )
        });

        cx.update(|_window, cx| {
            let pgv = view.read(cx);
            *pgv.app_state.project_env.write() = Some(ProjectEnvironment::from_current());
        });

        view.update(cx, |this, cx| {
            this.app_state
                .setup_lsp_progress_tracking(view.downgrade(), cx);
        });

        cx.update(|window, cx| {
            let handle = view.read(cx).active_editor_focus_handle(cx);
            if let Some(handle) = handle {
                window.focus(&handle, cx);
            }
        });

        Self {
            view,
            cx,
            _temp_dir: temp_dir,
            root,
        }
    }

    pub fn new_with_text(text: &str, cx: &'a mut TestAppContext) -> Self {
        let app = Self::new(cx);
        let text = text.to_string();
        let view = app.view.clone();
        app.cx.update(|_window, cx| {
            if let Some(stoat) = view.read(cx).active_stoat(cx) {
                stoat.update(cx, |s, cx| {
                    let buffer_item = s.active_buffer(cx);
                    let buffer = buffer_item.read(cx).buffer().clone();
                    let len = buffer.read(cx).len();
                    buffer.update(cx, |buf, _| {
                        buf.edit([(0..len, text.as_str())]);
                    });
                });
            }
        });
        app
    }

    pub fn inject_symbols(
        &mut self,
        symbols: Vec<crate::app_state::SymbolEntry>,
        source: crate::app_state::SymbolPickerSource,
    ) {
        let view = self.view.clone();
        self.cx.update(|window, cx| {
            view.update(cx, |pgv, cx| {
                pgv.handle_open_symbol_picker(symbols, source, window, cx);
            });
        });
    }

    // -- Input methods --

    pub fn type_input(&mut self, input: &str) {
        let keystrokes = parse_input_sequence(input);
        for keystroke in keystrokes {
            self.cx.update(|window, cx| {
                window.dispatch_keystroke(keystroke, cx);
            });
        }
        self.cx.run_until_parked();
    }

    pub fn type_action(&mut self, action_name: &str) {
        let view = self.view.clone();
        let action = ActionExpr::Single(Action {
            name: action_name.to_string(),
            args: vec![],
        });
        self.cx.update(|_window, cx| {
            if let Some(stoat) = view.read(cx).active_stoat(cx) {
                dispatch_editor_action(&stoat, &action, cx);
            }
        });
        self.cx.run_until_parked();
    }

    pub fn flush(&mut self) {
        let view = self.view.clone();
        self.cx.update(|window, cx| {
            view.update(cx, |pgv, cx| {
                pgv.process_pending_actions(window, cx);
            });
        });
    }

    // -- Read methods --

    pub fn snapshot_layout(&mut self) -> String {
        let view = self.view.clone();
        self.cx.update(|_window, cx| {
            let pgv = view.read(cx);
            format_member(
                pgv.pane_group.root(),
                &pgv.pane_contents,
                pgv.active_pane,
                cx,
            )
        })
    }

    pub fn snapshot_active(&mut self) -> String {
        let view = self.view.clone();
        self.cx.update(|window, cx| {
            let pgv = view.read(cx);
            let pane_id = pgv.active_pane;
            let content = pgv.pane_contents.get(&pane_id);

            match content {
                Some(PaneContent::Editor(editor)) => {
                    let stoat = editor.read(cx).stoat.clone();
                    snapshot_editor(&stoat, pane_id, &pgv.app_state, cx)
                },
                Some(PaneContent::Claude(claude_view)) => {
                    snapshot_claude(claude_view, pane_id, &pgv.app_state, window, cx)
                },
                Some(PaneContent::Static(_)) => {
                    format!("[static] pane={pane_id}")
                },
                None => format!("[empty] pane={pane_id}"),
            }
        })
    }

    pub fn flash_message(&mut self) -> Option<String> {
        let view = self.view.clone();
        self.cx
            .update(|_window, cx| view.read(cx).app_state.flash_message.clone())
    }

    pub async fn await_flash_message(&mut self, timeout: Duration) {
        let start = std::time::Instant::now();
        loop {
            self.cx.run_until_parked();
            if self.flash_message().is_some() {
                return;
            }
            if start.elapsed() >= timeout {
                return;
            }
            self.cx
                .background_executor
                .timer(Duration::from_millis(100))
                .await;
        }
    }

    pub async fn await_lsp_ready(&mut self, timeout: Duration) {
        use crate::app_state::LspStatus;

        let lsp_state = {
            let view = self.view.clone();
            self.cx
                .update(|_window, cx| view.read(cx).app_state.lsp_state.clone())
        };
        let start = std::time::Instant::now();
        loop {
            self.cx.run_until_parked();
            if *lsp_state.status.read() == LspStatus::Ready {
                return;
            }
            if start.elapsed() >= timeout {
                panic!(
                    "await_lsp_ready timed out after {timeout:?}, status: {:?}",
                    *lsp_state.status.read()
                );
            }
            self.cx
                .background_executor
                .timer(Duration::from_millis(200))
                .await;
        }
    }

    // -- Hover --

    pub fn hover_visible(&mut self) -> bool {
        let view = self.view.clone();
        self.cx.update(|_window, cx| {
            view.read(cx)
                .active_stoat(cx)
                .map(|s| s.read(cx).hover_state.visible)
                .unwrap_or(false)
        })
    }

    pub fn hover_blocks(&mut self) -> Vec<HoverBlock> {
        let view = self.view.clone();
        self.cx.update(|_window, cx| {
            view.read(cx)
                .active_stoat(cx)
                .map(|s| s.read(cx).hover_state.blocks.clone())
                .unwrap_or_default()
        })
    }

    pub async fn await_hover(&mut self, timeout: Duration) {
        let start = std::time::Instant::now();
        loop {
            self.cx.run_until_parked();
            if self.hover_visible() {
                return;
            }
            if start.elapsed() >= timeout {
                panic!("await_hover timed out after {timeout:?}");
            }
            self.cx
                .background_executor
                .timer(Duration::from_millis(100))
                .await;
        }
    }

    pub async fn sleep(&mut self, duration: Duration) {
        self.cx.background_executor.timer(duration).await;
    }

    // -- File I/O --

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write_file(&self, relative_path: &str, content: &str) {
        let path = self.root.join(relative_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .unwrap_or_else(|e| panic!("failed to create dirs for {}: {e}", path.display()));
        }
        std::fs::write(&path, content)
            .unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
    }

    pub fn load_file(&mut self, relative_path: &str) {
        let path = self.root.join(relative_path);
        let view = self.view.clone();
        self.cx.update(|_window, cx| {
            if let Some(stoat) = view.read(cx).active_stoat(cx) {
                stoat.update(cx, |s, cx| {
                    s.load_file(&path, cx)
                        .unwrap_or_else(|e| panic!("failed to load {}: {e}", path.display()));
                });
            }
        });
    }

    // -- Git --

    pub fn git(&self, args: &[&str]) -> String {
        git_fixture::run_git(&self.root, args)
            .unwrap_or_else(|e| panic!("git {} failed: {e}", args.join(" ")))
    }

    // -- Direct access --

    pub fn with_stoat(&mut self, f: impl FnOnce(&Entity<Stoat>, &mut gpui::App)) {
        let view = self.view.clone();
        self.cx.update(|_window, cx| {
            if let Some(stoat) = view.read(cx).active_stoat(cx) {
                f(&stoat, cx);
            }
        });
    }

    pub fn blame_line_to_entry(&mut self) -> Option<Vec<usize>> {
        let view = self.view.clone();
        self.cx.update(|_window, cx| {
            view.read(cx).active_stoat(cx).and_then(|s| {
                s.read(cx)
                    .blame_state
                    .data
                    .as_ref()
                    .map(|d| d.line_to_entry.clone())
            })
        })
    }

    pub fn view(&self) -> &Entity<PaneGroupView> {
        &self.view
    }
}

#[cfg(test)]
mod tests {
    use gpui::TestAppContext;

    #[gpui::test]
    fn smoke_new(cx: &mut TestAppContext) {
        let mut app = super::HeadlessStoat::new(cx);
        let layout = app.snapshot_layout();
        assert!(layout.contains("editor"), "layout: {layout}");
    }

    #[gpui::test]
    fn smoke_with_fixture(cx: &mut TestAppContext) {
        let mut app = super::HeadlessStoat::with_fixture("basic-diff", cx);
        let snap = app.snapshot_active();
        assert!(snap.contains("[editor]"), "snap: {snap}");
    }
}
