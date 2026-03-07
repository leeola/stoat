use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    pub fn open_git_blame(&mut self, cx: &mut Context<Self>) {
        if self.blame_state.active {
            self.blame_dismiss(cx);
            return;
        }

        let file_path = match &self.current_file_path {
            Some(p) => p.clone(),
            None => {
                tracing::debug!("No file open for blame");
                return;
            },
        };

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match self.services.git.discover(&root_path) {
            Ok(r) => r,
            Err(_) => {
                tracing::debug!("No git repository found");
                return;
            },
        };

        let data = match repo.blame_file(&file_path) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!("Blame failed: {e}");
                return;
            },
        };

        self.blame_state.active = true;
        self.blame_state.data = Some(data);

        self.key_context = crate::stoat::KeyContext::BlameReview;
        self.mode = "blame_review".to_string();

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    pub fn blame_dismiss(&mut self, cx: &mut Context<Self>) {
        self.blame_state.active = false;
        self.blame_state.data = None;

        self.key_context = crate::stoat::KeyContext::TextEditor;
        self.mode = "normal".to_string();

        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    pub fn blame_toggle_author(&mut self, cx: &mut Context<Self>) {
        self.blame_state.show_author = !self.blame_state.show_author;
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }

    pub fn blame_toggle_date(&mut self, cx: &mut Context<Self>) {
        self.blame_state.show_date = !self.blame_state.show_date;
        cx.emit(crate::stoat::StoatEvent::Changed);
        cx.notify();
    }
}

use crate::{app_state::BlameCommitDiff, pane_group::view::PaneGroupView, stoat::KeyContext};

impl PaneGroupView {
    pub(crate) fn handle_open_git_blame(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_git_blame(cx);
                });
            });
            cx.notify();
        }
    }

    pub(crate) fn handle_blame_open_commit_diff(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        let Some(editor) = self.active_editor().cloned() else {
            return;
        };

        let (full_oid, short_hash, author_name, date_display, summary) = {
            let editor_ref = editor.read(cx);
            let stoat = editor_ref.stoat.read(cx);
            let data = match &stoat.blame_state.data {
                Some(d) => d,
                None => return,
            };
            let cursor_row = stoat.cursor_position().row as usize;
            let entry_idx = match data.line_to_entry.get(cursor_row) {
                Some(&idx) => idx,
                None => return,
            };
            let entry = &data.entries[entry_idx];
            (
                entry.full_oid.clone(),
                entry.short_hash.clone(),
                entry.author_name.clone(),
                entry.date_display.clone(),
                entry.summary.clone(),
            )
        };

        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let oid = full_oid.clone();

        self.app_state.blame_commit_diff = Some(BlameCommitDiff {
            commit_oid: full_oid,
            short_hash,
            author_name,
            date_display,
            summary,
            files: Vec::new(),
            selected: 0,
            preview: None,
            preview_task: None,
        });

        editor.update(cx, |editor, cx| {
            editor.stoat.update(cx, |stoat, cx| {
                stoat.key_context = KeyContext::BlameCommitDiff;
                stoat.mode = "blame_commit_diff".to_string();
                cx.emit(crate::stoat::StoatEvent::Changed);
                cx.notify();
            });
        });

        let oid_for_preview = oid.clone();
        let git = self.app_state.services.git.clone();
        let git2 = self.app_state.services.git.clone();
        self.app_state
            .blame_commit_diff
            .as_mut()
            .unwrap()
            .preview_task = Some(cx.spawn(async move |this, cx| {
            if let Some(files) =
                crate::git::commit_diff::load_commit_files(git, root_path.clone(), oid).await
            {
                let first_file_path = files.first().map(|f| f.path.clone());
                let _ = this.update(cx, |pane_group, cx| {
                    if let Some(ref mut bcd) = pane_group.app_state.blame_commit_diff {
                        bcd.files = files;
                    }
                    cx.notify();
                });

                if let Some(file_path) = first_file_path {
                    if let Some(preview) = crate::git::commit_diff::load_commit_file_diff(
                        git2,
                        root_path,
                        oid_for_preview,
                        file_path,
                    )
                    .await
                    {
                        let _ = this.update(cx, |pane_group, cx| {
                            if let Some(ref mut bcd) = pane_group.app_state.blame_commit_diff {
                                bcd.preview = Some(preview);
                            }
                            cx.notify();
                        });
                    }
                }
            }
        }));

        cx.notify();
    }

    pub(crate) fn handle_blame_commit_diff_next(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        let Some(ref mut bcd) = self.app_state.blame_commit_diff else {
            return;
        };
        if bcd.files.is_empty() {
            return;
        }
        bcd.selected = (bcd.selected + 1).min(bcd.files.len() - 1);
        self.load_blame_commit_diff_preview(cx);
        cx.notify();
    }

    pub(crate) fn handle_blame_commit_diff_prev(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        let Some(ref mut bcd) = self.app_state.blame_commit_diff else {
            return;
        };
        bcd.selected = bcd.selected.saturating_sub(1);
        self.load_blame_commit_diff_preview(cx);
        cx.notify();
    }

    pub(crate) fn handle_blame_commit_diff_dismiss(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        self.app_state.blame_commit_diff = None;

        if let Some(editor) = self.active_editor().cloned() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.key_context = KeyContext::BlameReview;
                    stoat.mode = "blame_review".to_string();
                    cx.emit(crate::stoat::StoatEvent::Changed);
                    cx.notify();
                });
            });
        }
        cx.notify();
    }

    pub(crate) fn load_blame_commit_diff_preview(&mut self, cx: &mut gpui::Context<'_, Self>) {
        let Some(ref mut bcd) = self.app_state.blame_commit_diff else {
            return;
        };
        bcd.preview_task = None;
        bcd.preview = None;

        let file_path = match bcd.files.get(bcd.selected) {
            Some(f) => f.path.clone(),
            None => return,
        };

        let root_path = self.app_state.worktree.lock().root().to_path_buf();
        let oid = bcd.commit_oid.clone();
        let git = self.app_state.services.git.clone();

        bcd.preview_task = Some(cx.spawn(async move |this, cx| {
            if let Some(preview) =
                crate::git::commit_diff::load_commit_file_diff(git, root_path, oid, file_path).await
            {
                let _ = this.update(cx, |pane_group, cx| {
                    if let Some(ref mut bcd) = pane_group.app_state.blame_commit_diff {
                        bcd.preview = Some(preview);
                    }
                    cx.notify();
                });
            }
        }));
    }
}
