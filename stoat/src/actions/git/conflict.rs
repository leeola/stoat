use crate::{
    fs::Fs,
    git::{
        conflict::{resolve_conflict, ConflictReviewState, ConflictSide, ConflictViewKind},
        provider::GitRepo,
    },
    pane_group::view::PaneGroupView,
    stoat::{KeyContext, Stoat, StoatEvent},
};
use gpui::{AsyncApp, Context, WeakEntity};
use std::path::Path;
use tracing::debug;

impl Stoat {
    /// Enter conflict review mode.
    ///
    /// Scans the repository for conflicted files (status `"!"`), loads the first one,
    /// parses conflict markers, and enters `conflict_review` mode. Resumes previous
    /// review state if it exists and the files still have conflicts.
    pub fn open_conflict_review(&mut self, cx: &mut Context<Self>) {
        debug!("Opening conflict review");

        self.conflict_review_previous_mode = Some(self.mode.clone());

        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let has_existing_state = !self.conflict_state.files.is_empty();
        let existing_file = if has_existing_state {
            self.conflict_state
                .files
                .get(self.conflict_state.file_idx)
                .cloned()
        } else {
            None
        };
        let existing_conflict_idx = self.conflict_state.conflict_idx;

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => {
                    debug!("No git repository found");
                    return Some(());
                },
            };

            // Resume existing state if files still have conflicts
            if let Some(file_path) = existing_file {
                let abs_path = repo.workdir().join(&file_path);
                let content = fs.read_to_string(&abs_path).await.ok();
                let mtime = fs.metadata(&abs_path).await.ok().and_then(|m| m.modified);
                let head = repo.head_content(&abs_path).await.ok();
                let index = repo.index_content(&abs_path).await.ok();

                let resumed = this
                    .update(cx, |s, cx| {
                        let loaded = match s.activate_buffer_by_path(&abs_path, cx) {
                            Some(item) => Some(item),
                            None => {
                                if let Some(ref content) = content {
                                    s.load_file_from_content(
                                        &abs_path,
                                        content,
                                        mtime,
                                        head.as_deref(),
                                        index.as_deref(),
                                        cx,
                                    )
                                    .ok();
                                    Some(s.active_buffer(cx))
                                } else {
                                    None
                                }
                            },
                        };
                        if let Some(buffer_item) = loaded {
                            buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
                            let count = buffer_item.read(cx).conflicts().len();
                            if count > 0 {
                                s.conflict_state.conflict_idx =
                                    existing_conflict_idx.min(count - 1);
                                s.key_context = KeyContext::ConflictReview;
                                s.mode = "conflict_review".to_string();
                                s.jump_to_conflict(cx);
                                cx.emit(StoatEvent::Changed);
                                cx.notify();
                                return true;
                            }
                        }
                        s.conflict_state = ConflictReviewState::default();
                        false
                    })
                    .ok()
                    .unwrap_or(false);

                if resumed {
                    return Some(());
                }
            }

            // Fresh scan: gather conflicted files
            let entries = match repo.gather_status().await {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::error!("Failed to gather git status: {e}");
                    return Some(());
                },
            };

            let conflicted_files: Vec<std::path::PathBuf> = entries
                .into_iter()
                .filter(|e| e.status == "!")
                .map(|e| e.path)
                .collect();

            if conflicted_files.is_empty() {
                debug!("No conflicted files found");
                return Some(());
            }

            for (idx, file_path) in conflicted_files.iter().enumerate() {
                let abs_path = repo.workdir().join(file_path);
                let content = match fs.read_to_string(&abs_path).await {
                    Ok(c) => c,
                    Err(_) => continue,
                };
                let mtime = fs.metadata(&abs_path).await.ok().and_then(|m| m.modified);
                let head = repo.head_content(&abs_path).await.ok();
                let index = repo.index_content(&abs_path).await.ok();

                let found = this
                    .update(cx, |s, cx| {
                        if s.load_file_from_content(
                            &abs_path,
                            &content,
                            mtime,
                            head.as_deref(),
                            index.as_deref(),
                            cx,
                        )
                        .is_err()
                        {
                            return false;
                        }
                        let buffer_item = s.active_buffer(cx);
                        buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
                        let count = buffer_item.read(cx).conflicts().len();
                        if count > 0 {
                            s.conflict_state = ConflictReviewState {
                                files: conflicted_files.clone(),
                                file_idx: idx,
                                conflict_idx: 0,
                                resolutions: Default::default(),
                            };
                            s.key_context = KeyContext::ConflictReview;
                            s.mode = "conflict_review".to_string();
                            s.jump_to_conflict(cx);
                            cx.emit(StoatEvent::Changed);
                            cx.notify();
                            true
                        } else {
                            false
                        }
                    })
                    .ok()
                    .unwrap_or(false);

                if found {
                    return Some(());
                }
            }

            debug!("No files with conflict markers found");
            Some(())
        })
        .detach();
    }

    /// Exit conflict review mode, applying any pending resolutions.
    pub fn conflict_review_dismiss(&mut self, cx: &mut Context<Self>) {
        if self.mode != "conflict_review" {
            return;
        }
        debug!("Dismissing conflict review");
        self.apply_resolutions(cx);
        self.conflict_review_previous_mode = None;
        self.conflict_view_kind = ConflictViewKind::Inline;
        self.merge_display_row_count = None;
        cx.emit(StoatEvent::Changed);
        cx.notify();
    }

    /// Store resolution metadata for the current conflict (non-destructive).
    pub fn conflict_accept(&mut self, side: ConflictSide, cx: &mut Context<Self>) {
        if self.mode != "conflict_review" {
            return;
        }

        let conflict_count = self.active_buffer(cx).read(cx).conflicts().len();
        if self.conflict_state.conflict_idx >= conflict_count {
            return;
        }

        let file_idx = self.conflict_state.file_idx;
        let conflict_idx = self.conflict_state.conflict_idx;
        self.conflict_state
            .resolutions
            .insert((file_idx, conflict_idx), side);

        let next = conflict_idx + 1;
        if next < conflict_count {
            self.conflict_state.conflict_idx = next;
            self.jump_to_conflict(cx);
            cx.emit(StoatEvent::Changed);
            cx.notify();
        } else {
            self.advance_to_next_conflicted_file_or_dismiss(cx);
        }
    }

    /// Apply all stored resolutions to their respective buffers.
    ///
    /// Groups resolutions by file, computes all replacements from the original text,
    /// then applies them in reverse order (highest offset first) so earlier byte
    /// offsets remain valid through the sequence of edits.
    fn apply_resolutions(&mut self, cx: &mut Context<Self>) {
        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();

        let mut by_file: std::collections::HashMap<usize, Vec<(usize, ConflictSide)>> =
            std::collections::HashMap::new();
        for (&(file_idx, conflict_idx), &side) in &self.conflict_state.resolutions {
            by_file
                .entry(file_idx)
                .or_default()
                .push((conflict_idx, side));
        }

        let files = self.conflict_state.files.clone();

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => return Some(()),
            };

            for (file_idx, mut entries) in by_file {
                let Some(file_path) = files.get(file_idx) else {
                    continue;
                };
                let abs_path = repo.workdir().join(file_path);

                let content = fs.read_to_string(&abs_path).await.ok();
                let mtime = fs.metadata(&abs_path).await.ok().and_then(|m| m.modified);
                let head = repo.head_content(&abs_path).await.ok();
                let index = repo.index_content(&abs_path).await.ok();

                this.update(cx, |s, cx| {
                    let buffer_item = match s.activate_buffer_by_path(&abs_path, cx) {
                        Some(item) => item,
                        None => {
                            if let Some(ref content) = content {
                                if s.load_file_from_content(
                                    &abs_path,
                                    content,
                                    mtime,
                                    head.as_deref(),
                                    index.as_deref(),
                                    cx,
                                )
                                .is_err()
                                {
                                    return;
                                }
                                s.active_buffer(cx)
                            } else {
                                return;
                            }
                        },
                    };

                    buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
                    let text = buffer_item.read(cx).buffer().read(cx).text();
                    let conflicts = buffer_item.read(cx).conflicts().to_vec();

                    entries.sort_by(|a, b| b.0.cmp(&a.0));
                    let edits: Vec<_> = entries
                        .iter()
                        .filter_map(|&(conflict_idx, side)| {
                            conflicts
                                .get(conflict_idx)
                                .map(|c| resolve_conflict(&text, c, side))
                        })
                        .collect();

                    let buffer = buffer_item.read(cx).buffer().clone();
                    for (range, replacement) in edits {
                        buffer.update(cx, |buf, _| {
                            buf.edit([(range, replacement.as_str())]);
                        });
                    }

                    buffer_item.update(cx, |item, cx| {
                        let _ = item.reparse(cx);
                        item.reparse_conflicts(cx);
                    });
                })
                .ok();
            }

            Some(())
        })
        .detach();
    }

    pub fn conflict_accept_ours(&mut self, cx: &mut Context<Self>) {
        self.conflict_accept(ConflictSide::Ours, cx);
    }

    pub fn conflict_accept_theirs(&mut self, cx: &mut Context<Self>) {
        self.conflict_accept(ConflictSide::Theirs, cx);
    }

    pub fn conflict_accept_both(&mut self, cx: &mut Context<Self>) {
        self.conflict_accept(ConflictSide::Both, cx);
    }

    pub fn conflict_next_conflict(&mut self, cx: &mut Context<Self>) {
        if self.mode != "conflict_review" {
            return;
        }

        let count = self.active_buffer(cx).read(cx).conflicts().len();
        if count == 0 {
            return;
        }

        let next = self.conflict_state.conflict_idx + 1;
        if next < count {
            self.conflict_state.conflict_idx = next;
            self.jump_to_conflict(cx);
            cx.emit(StoatEvent::Changed);
            cx.notify();
        } else {
            self.advance_to_next_conflicted_file_or_wrap(cx);
        }
    }

    pub fn conflict_prev_conflict(&mut self, cx: &mut Context<Self>) {
        if self.mode != "conflict_review" {
            return;
        }

        if self.conflict_state.conflict_idx > 0 {
            self.conflict_state.conflict_idx -= 1;
            self.jump_to_conflict(cx);
            cx.emit(StoatEvent::Changed);
            cx.notify();
        } else {
            self.advance_to_prev_conflicted_file_or_wrap(cx);
        }
    }

    fn jump_to_conflict(&mut self, cx: &mut Context<Self>) {
        let conflicts = self.active_buffer(cx).read(cx).conflicts().to_vec();
        if let Some(conflict) = conflicts.get(self.conflict_state.conflict_idx) {
            self.cursor.move_to(text::Point::new(conflict.start_row, 0));
            self.ensure_cursor_visible(cx);
        }
    }

    /// Advance to next conflicted file, or dismiss if none found.
    fn advance_to_next_conflicted_file_or_dismiss(&mut self, cx: &mut Context<Self>) {
        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let files = self.conflict_state.files.clone();
        let start = self.conflict_state.file_idx + 1;

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => {
                    this.update(cx, |s, cx| s.conflict_review_dismiss(cx)).ok();
                    return Some(());
                },
            };

            for (idx, file) in files.iter().enumerate().skip(start) {
                if load_conflict_file(&*repo, &*fs, file, idx, false, &this, cx).await {
                    return Some(());
                }
            }

            this.update(cx, |s, cx| s.conflict_review_dismiss(cx)).ok();
            Some(())
        })
        .detach();
    }

    /// Advance to next conflicted file, or wrap to first.
    fn advance_to_next_conflicted_file_or_wrap(&mut self, cx: &mut Context<Self>) {
        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let files = self.conflict_state.files.clone();
        let start = self.conflict_state.file_idx + 1;

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => return Some(()),
            };

            for (idx, file) in files.iter().enumerate().skip(start) {
                if load_conflict_file(&*repo, &*fs, file, idx, false, &this, cx).await {
                    return Some(());
                }
            }

            for (idx, file) in files.iter().enumerate() {
                if load_conflict_file(&*repo, &*fs, file, idx, false, &this, cx).await {
                    return Some(());
                }
            }

            Some(())
        })
        .detach();
    }

    /// Advance to previous conflicted file, or wrap to last.
    fn advance_to_prev_conflicted_file_or_wrap(&mut self, cx: &mut Context<Self>) {
        let git = self.services.git.clone();
        let fs = self.services.fs.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let files = self.conflict_state.files.clone();
        let current = self.conflict_state.file_idx;

        cx.spawn(async move |this, cx| {
            let repo = match git.discover(&root_path).await {
                Ok(r) => r,
                Err(_) => return Some(()),
            };

            for (idx, file) in files.iter().enumerate().take(current).rev() {
                if load_conflict_file(&*repo, &*fs, file, idx, true, &this, cx).await {
                    return Some(());
                }
            }

            for (idx, file) in files.iter().enumerate().rev() {
                if load_conflict_file(&*repo, &*fs, file, idx, true, &this, cx).await {
                    return Some(());
                }
            }

            Some(())
        })
        .detach();
    }

    pub fn conflict_toggle_view(&mut self, cx: &mut Context<Self>) {
        self.conflict_view_kind = match self.conflict_view_kind {
            ConflictViewKind::Inline => ConflictViewKind::Merge,
            ConflictViewKind::Merge => {
                self.merge_display_row_count = None;
                ConflictViewKind::Inline
            },
        };
        cx.notify();
    }
}

/// Load a conflict file async and set conflict state if it has conflicts.
///
/// When `last_conflict` is true, positions at the last conflict in the file
/// (for backward navigation). Returns true if the file had conflicts.
async fn load_conflict_file(
    repo: &dyn GitRepo,
    fs: &dyn Fs,
    file_path: &Path,
    file_idx: usize,
    last_conflict: bool,
    this: &WeakEntity<Stoat>,
    cx: &mut AsyncApp,
) -> bool {
    let abs_path = repo.workdir().join(file_path);
    let content = match fs.read_to_string(&abs_path).await {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mtime = fs.metadata(&abs_path).await.ok().and_then(|m| m.modified);
    let head = repo.head_content(&abs_path).await.ok();
    let index = repo.index_content(&abs_path).await.ok();

    this.update(cx, |s, cx| {
        let buffer_item = match s.activate_buffer_by_path(&abs_path, cx) {
            Some(item) => item,
            None => {
                if s.load_file_from_content(
                    &abs_path,
                    &content,
                    mtime,
                    head.as_deref(),
                    index.as_deref(),
                    cx,
                )
                .is_err()
                {
                    return false;
                }
                s.active_buffer(cx)
            },
        };
        buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
        let count = buffer_item.read(cx).conflicts().len();
        if count > 0 {
            s.conflict_state.file_idx = file_idx;
            s.conflict_state.conflict_idx = if last_conflict { count - 1 } else { 0 };
            s.jump_to_conflict(cx);
            cx.emit(StoatEvent::Changed);
            cx.notify();
            true
        } else {
            false
        }
    })
    .ok()
    .unwrap_or(false)
}

impl PaneGroupView {
    pub(crate) fn handle_open_conflict_review(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<'_, Self>,
    ) {
        if let Some(editor) = self.active_editor() {
            editor.update(cx, |editor, cx| {
                editor.stoat.update(cx, |stoat, cx| {
                    stoat.open_conflict_review(cx);
                });
            });
            cx.notify();
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        git::{
            conflict::{ConflictSide, ConflictViewKind},
            status::GitStatusEntry,
        },
        stoat::KeyContext,
        Stoat,
    };
    use gpui::TestAppContext;
    use std::path::PathBuf;

    const CONFIG_CONFLICT: &str = "\
[settings]
<<<<<<< HEAD
ours-name-one
ours-name-two
ours-name-three
=======
theirs-name
>>>>>>> theirs
[database]
<<<<<<< HEAD
ours-host
=======
theirs-host-one
theirs-host-two
>>>>>>> theirs
[logging]
<<<<<<< HEAD
ours-level-one
ours-level-two
=======
theirs-level-one
theirs-level-two
theirs-level-three
theirs-level-four
>>>>>>> theirs
done
";

    const FILE_CONFLICT: &str = "\
header
<<<<<<< HEAD
ours alpha one
ours alpha two
=======
theirs alpha one
theirs alpha two
theirs alpha three
theirs alpha four
theirs alpha five
>>>>>>> theirs
middle section
<<<<<<< HEAD
ours beta one
ours beta two
ours beta three
ours beta four
=======
theirs beta one
>>>>>>> theirs
footer section
<<<<<<< HEAD
ours gamma one
ours gamma two
ours gamma three
=======
theirs gamma one
theirs gamma two
theirs gamma three
>>>>>>> theirs
end
";

    fn conflict_fixture(cx: &mut TestAppContext) -> crate::test::TestStoat<'_> {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat.with_working_change("config.txt", CONFIG_CONFLICT);
        stoat.with_working_change("file.txt", FILE_CONFLICT);
        stoat.update(|s, _| {
            s.services.fake_git().set_status(vec![
                GitStatusEntry {
                    path: PathBuf::from("config.txt"),
                    status: "!".to_string(),
                    staged: false,
                },
                GitStatusEntry {
                    path: PathBuf::from("file.txt"),
                    status: "!".to_string(),
                    staged: false,
                },
            ]);
        });
        stoat.update(|s, cx| s.open_conflict_review(cx));
        stoat.run_until_parked();
        stoat
    }

    #[gpui::test]
    fn open_conflict_review_enters_mode(cx: &mut TestAppContext) {
        let mut stoat = conflict_fixture(cx);

        assert_eq!(stoat.mode(), "conflict_review");
        let key_ctx = stoat.update(|s, _| s.key_context());
        assert_eq!(key_ctx, KeyContext::ConflictReview);
        assert_eq!(stoat.conflict_files().len(), 2);
        assert_eq!(stoat.conflict_count(), 3);
        assert_eq!(stoat.conflict_position(), (0, 0));
    }

    #[gpui::test]
    fn accept_stores_resolution_metadata(cx: &mut TestAppContext) {
        let mut stoat = conflict_fixture(cx);

        stoat.update(|s, cx| s.conflict_accept_ours(cx));

        let text = stoat.buffer_text();
        assert!(text.contains("<<<<<<<"), "markers still present: {text}");
        assert_eq!(stoat.conflict_count(), 3);

        let resolution = stoat.update(|s, _| s.conflict_state.resolutions.get(&(0, 0)).copied());
        assert_eq!(resolution, Some(ConflictSide::Ours));

        assert_eq!(stoat.conflict_position(), (0, 1));
        assert_eq!(stoat.mode(), "conflict_review");
    }

    #[gpui::test]
    fn accept_theirs_stores_resolution(cx: &mut TestAppContext) {
        let mut stoat = conflict_fixture(cx);

        stoat.update(|s, cx| s.conflict_accept_theirs(cx));

        let resolution = stoat.update(|s, _| s.conflict_state.resolutions.get(&(0, 0)).copied());
        assert_eq!(resolution, Some(ConflictSide::Theirs));
    }

    #[gpui::test]
    fn accept_both_stores_resolution(cx: &mut TestAppContext) {
        let mut stoat = conflict_fixture(cx);

        stoat.update(|s, cx| s.conflict_accept_both(cx));

        let resolution = stoat.update(|s, _| s.conflict_state.resolutions.get(&(0, 0)).copied());
        assert_eq!(resolution, Some(ConflictSide::Both));
    }

    #[gpui::test]
    fn resolve_all_and_dismiss(cx: &mut TestAppContext) {
        let mut stoat = conflict_fixture(cx);

        // 3 conflicts per file, 2 files = 6 total
        for _ in 0..6 {
            if stoat.mode() != "conflict_review" {
                break;
            }
            stoat.update(|s, cx| s.conflict_accept_ours(cx));
            stoat.run_until_parked();
        }

        let dismissed = stoat.update(|s, _| s.conflict_review_previous_mode.is_none());
        assert!(dismissed);

        stoat.run_until_parked();
        let text = stoat.buffer_text();
        assert!(!text.contains("<<<<<<<"), "all markers resolved: {text}");
    }

    #[gpui::test]
    fn dismiss_applies_resolutions(cx: &mut TestAppContext) {
        let mut stoat = conflict_fixture(cx);

        stoat.update(|s, cx| s.conflict_accept_ours(cx));

        assert!(stoat.buffer_text().contains("<<<<<<<"));

        stoat.update(|s, cx| s.conflict_review_dismiss(cx));
        stoat.run_until_parked();

        let text = stoat.buffer_text();
        assert!(text.contains("ours-name"), "resolved ours applied: {text}");
        assert!(
            text.contains("<<<<<<<"),
            "unresolved markers remain: {text}"
        );
    }

    #[gpui::test]
    fn change_resolution(cx: &mut TestAppContext) {
        let mut stoat = conflict_fixture(cx);

        stoat.update(|s, cx| s.conflict_accept_ours(cx));
        assert_eq!(stoat.conflict_position(), (0, 1));

        stoat.update(|s, cx| s.conflict_prev_conflict(cx));
        assert_eq!(stoat.conflict_position(), (0, 0));

        stoat.update(|s, cx| s.conflict_accept_theirs(cx));
        let resolution = stoat.update(|s, _| s.conflict_state.resolutions.get(&(0, 0)).copied());
        assert_eq!(resolution, Some(ConflictSide::Theirs));

        stoat.update(|s, cx| s.conflict_review_dismiss(cx));
        stoat.run_until_parked();
        let text = stoat.buffer_text();
        assert!(text.contains("theirs-name"), "theirs applied: {text}");
        assert!(!text.contains("ours-name-one"), "ours NOT applied: {text}");
    }

    #[gpui::test]
    fn partial_resolution(cx: &mut TestAppContext) {
        let mut stoat = conflict_fixture(cx);

        stoat.update(|s, cx| s.conflict_accept_ours(cx));
        let count = stoat.update(|s, _| s.conflict_state.resolutions.len());
        assert_eq!(count, 1);

        stoat.update(|s, cx| s.conflict_review_dismiss(cx));
        stoat.run_until_parked();

        let text = stoat.buffer_text();
        assert!(
            text.contains("ours-name"),
            "first conflict resolved: {text}"
        );
        assert!(
            text.contains("<<<<<<<"),
            "remaining conflicts have markers: {text}"
        );
    }

    #[gpui::test]
    fn toggle_conflict_view(cx: &mut TestAppContext) {
        let mut stoat = conflict_fixture(cx);
        assert_eq!(
            stoat.update(|s, _| s.conflict_view_kind),
            ConflictViewKind::Merge
        );
        stoat.update(|s, cx| s.conflict_toggle_view(cx));
        assert_eq!(
            stoat.update(|s, _| s.conflict_view_kind),
            ConflictViewKind::Inline
        );
        stoat.update(|s, cx| s.conflict_toggle_view(cx));
        assert_eq!(
            stoat.update(|s, _| s.conflict_view_kind),
            ConflictViewKind::Merge
        );
    }
}
