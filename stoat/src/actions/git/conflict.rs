use crate::{
    git::{
        conflict::{resolve_conflict, ConflictReviewState, ConflictSide, ConflictViewKind},
        repository::Repository,
        status::gather_git_status,
    },
    pane_group::view::PaneGroupView,
    stoat::{KeyContext, Stoat, StoatEvent},
};
use gpui::Context;
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

        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match Repository::discover(&root_path) {
            Ok(repo) => repo,
            Err(_) => {
                debug!("No git repository found");
                return;
            },
        };

        // Resume existing state if files still have conflicts
        if !self.conflict_state.files.is_empty() {
            if let Some(file_path) = self.conflict_state.files.get(self.conflict_state.file_idx) {
                let abs_path = repo.workdir().join(file_path);
                let loaded = match self.activate_buffer_by_path(&abs_path, cx) {
                    Some(item) => Some(item),
                    None => match self.load_file(&abs_path, cx) {
                        Ok(()) => Some(self.active_buffer(cx)),
                        Err(e) => {
                            tracing::error!(
                                "Failed to load saved conflict file {:?}: {e}",
                                abs_path
                            );
                            None
                        },
                    },
                };
                if let Some(buffer_item) = loaded {
                    buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
                    let count = buffer_item.read(cx).conflicts().len();

                    if count > 0 {
                        self.conflict_state.conflict_idx =
                            self.conflict_state.conflict_idx.min(count - 1);
                        self.key_context = KeyContext::ConflictReview;
                        self.mode = "conflict_review".to_string();
                        self.jump_to_conflict(cx);
                        cx.emit(StoatEvent::Changed);
                        cx.notify();
                        return;
                    }
                }
                // File no longer has conflicts or failed to load -- fall through to fresh scan
                self.conflict_state = ConflictReviewState::default();
            } else {
                self.conflict_state = ConflictReviewState::default();
            }
        }

        // Fresh scan: gather conflicted files
        let entries = match gather_git_status(repo.inner()) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::error!("Failed to gather git status: {e}");
                return;
            },
        };

        let conflicted_files: Vec<std::path::PathBuf> = entries
            .into_iter()
            .filter(|e| e.status == "!")
            .map(|e| e.path)
            .collect();

        if conflicted_files.is_empty() {
            debug!("No conflicted files found");
            return;
        }

        // Find first file with actual conflict markers
        for (idx, file_path) in conflicted_files.iter().enumerate() {
            let abs_path = repo.workdir().join(file_path);
            if let Err(e) = self.load_file(&abs_path, cx) {
                tracing::warn!("Failed to load {:?}: {e}", abs_path);
                continue;
            }

            let buffer_item = self.active_buffer(cx);
            buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
            let count = buffer_item.read(cx).conflicts().len();

            if count > 0 {
                self.conflict_state = ConflictReviewState {
                    files: conflicted_files,
                    file_idx: idx,
                    conflict_idx: 0,
                    resolutions: Default::default(),
                };
                self.key_context = KeyContext::ConflictReview;
                self.mode = "conflict_review".to_string();
                self.jump_to_conflict(cx);
                cx.emit(StoatEvent::Changed);
                cx.notify();
                return;
            }
        }

        debug!("No files with conflict markers found");
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

        // Navigate to next conflict
        let next = conflict_idx + 1;
        if next < conflict_count {
            self.conflict_state.conflict_idx = next;
            self.jump_to_conflict(cx);
        } else if !self.advance_to_next_conflicted_file(cx) {
            self.conflict_review_dismiss(cx);
            return;
        }

        cx.emit(StoatEvent::Changed);
        cx.notify();
    }

    /// Apply all stored resolutions to their respective buffers.
    ///
    /// Groups resolutions by file, computes all replacements from the original text,
    /// then applies them in reverse order (highest offset first) so earlier byte
    /// offsets remain valid through the sequence of edits.
    fn apply_resolutions(&mut self, cx: &mut Context<Self>) {
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match Repository::discover(&root_path) {
            Ok(r) => r,
            Err(_) => return,
        };

        let mut by_file: std::collections::HashMap<usize, Vec<(usize, ConflictSide)>> =
            std::collections::HashMap::new();
        for (&(file_idx, conflict_idx), &side) in &self.conflict_state.resolutions {
            by_file
                .entry(file_idx)
                .or_default()
                .push((conflict_idx, side));
        }

        for (file_idx, mut entries) in by_file {
            let Some(file_path) = self.conflict_state.files.get(file_idx) else {
                continue;
            };
            let abs_path = repo.workdir().join(file_path);

            let buffer_item = match self.activate_buffer_by_path(&abs_path, cx) {
                Some(item) => item,
                None => {
                    if self.load_file(&abs_path, cx).is_err() {
                        continue;
                    }
                    self.active_buffer(cx)
                },
            };

            buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
            let text = buffer_item.read(cx).buffer().read(cx).text();
            let conflicts = buffer_item.read(cx).conflicts().to_vec();

            // Compute all replacements from the original text
            entries.sort_by(|a, b| b.0.cmp(&a.0));
            let edits: Vec<_> = entries
                .iter()
                .filter_map(|&(conflict_idx, side)| {
                    conflicts
                        .get(conflict_idx)
                        .map(|c| resolve_conflict(&text, c, side))
                })
                .collect();

            // Apply in reverse order (already sorted) one at a time
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
        }
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
        } else if !self.advance_to_next_conflicted_file(cx) {
            self.advance_to_first_conflicted_file(cx);
        }

        self.jump_to_conflict(cx);
        cx.emit(StoatEvent::Changed);
        cx.notify();
    }

    pub fn conflict_prev_conflict(&mut self, cx: &mut Context<Self>) {
        if self.mode != "conflict_review" {
            return;
        }

        if self.conflict_state.conflict_idx > 0 {
            self.conflict_state.conflict_idx -= 1;
        } else if !self.advance_to_prev_conflicted_file(cx) {
            self.advance_to_last_conflicted_file(cx);
        }

        self.jump_to_conflict(cx);
        cx.emit(StoatEvent::Changed);
        cx.notify();
    }

    fn jump_to_conflict(&mut self, cx: &mut Context<Self>) {
        let conflicts = self.active_buffer(cx).read(cx).conflicts().to_vec();
        if let Some(conflict) = conflicts.get(self.conflict_state.conflict_idx) {
            self.cursor.move_to(text::Point::new(conflict.start_row, 0));
            self.ensure_cursor_visible(cx);
        }
    }

    fn advance_to_next_conflicted_file(&mut self, cx: &mut Context<Self>) -> bool {
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match Repository::discover(&root_path) {
            Ok(r) => r,
            Err(_) => return false,
        };

        let start = self.conflict_state.file_idx + 1;
        for idx in start..self.conflict_state.files.len() {
            let abs_path = repo.workdir().join(&self.conflict_state.files[idx]);
            let buffer_item = match self.activate_buffer_by_path(&abs_path, cx) {
                Some(item) => item,
                None => {
                    if self.load_file(&abs_path, cx).is_err() {
                        continue;
                    }
                    self.active_buffer(cx)
                },
            };
            buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
            if !buffer_item.read(cx).conflicts().is_empty() {
                self.conflict_state.file_idx = idx;
                self.conflict_state.conflict_idx = 0;
                self.jump_to_conflict(cx);
                return true;
            }
        }
        false
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

    fn advance_to_prev_conflicted_file(&mut self, cx: &mut Context<Self>) -> bool {
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match Repository::discover(&root_path) {
            Ok(r) => r,
            Err(_) => return false,
        };

        if self.conflict_state.file_idx == 0 {
            return false;
        }

        for idx in (0..self.conflict_state.file_idx).rev() {
            let abs_path = repo.workdir().join(&self.conflict_state.files[idx]);
            let buffer_item = match self.activate_buffer_by_path(&abs_path, cx) {
                Some(item) => item,
                None => {
                    if self.load_file(&abs_path, cx).is_err() {
                        continue;
                    }
                    self.active_buffer(cx)
                },
            };
            buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
            let count = buffer_item.read(cx).conflicts().len();
            if count > 0 {
                self.conflict_state.file_idx = idx;
                self.conflict_state.conflict_idx = count - 1;
                self.jump_to_conflict(cx);
                return true;
            }
        }
        false
    }

    fn advance_to_first_conflicted_file(&mut self, cx: &mut Context<Self>) -> bool {
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match Repository::discover(&root_path) {
            Ok(r) => r,
            Err(_) => return false,
        };

        for idx in 0..self.conflict_state.files.len() {
            let abs_path = repo.workdir().join(&self.conflict_state.files[idx]);
            let buffer_item = match self.activate_buffer_by_path(&abs_path, cx) {
                Some(item) => item,
                None => {
                    if self.load_file(&abs_path, cx).is_err() {
                        continue;
                    }
                    self.active_buffer(cx)
                },
            };
            buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
            if !buffer_item.read(cx).conflicts().is_empty() {
                self.conflict_state.file_idx = idx;
                self.conflict_state.conflict_idx = 0;
                self.jump_to_conflict(cx);
                return true;
            }
        }
        false
    }

    fn advance_to_last_conflicted_file(&mut self, cx: &mut Context<Self>) -> bool {
        let root_path = self.worktree.lock().root().to_path_buf();
        let repo = match Repository::discover(&root_path) {
            Ok(r) => r,
            Err(_) => return false,
        };

        for idx in (0..self.conflict_state.files.len()).rev() {
            let abs_path = repo.workdir().join(&self.conflict_state.files[idx]);
            let buffer_item = match self.activate_buffer_by_path(&abs_path, cx) {
                Some(item) => item,
                None => {
                    if self.load_file(&abs_path, cx).is_err() {
                        continue;
                    }
                    self.active_buffer(cx)
                },
            };
            buffer_item.update(cx, |item, cx| item.reparse_conflicts(cx));
            let count = buffer_item.read(cx).conflicts().len();
            if count > 0 {
                self.conflict_state.file_idx = idx;
                self.conflict_state.conflict_idx = count - 1;
                self.jump_to_conflict(cx);
                return true;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        git::conflict::{ConflictSide, ConflictViewKind},
        stoat::KeyContext,
        test::git_fixture::GitFixture,
        Stoat,
    };
    use gpui::TestAppContext;

    fn conflict_fixture(cx: &mut TestAppContext) -> (GitFixture, crate::test::TestStoat<'_>) {
        let fixture = GitFixture::load("merge-conflict");
        let mut stoat = Stoat::test(cx).use_fixture(&fixture);
        stoat.update(|s, cx| s.open_conflict_review(cx));
        (fixture, stoat)
    }

    #[gpui::test]
    fn open_conflict_review_enters_mode(cx: &mut TestAppContext) {
        let (_fixture, mut stoat) = conflict_fixture(cx);

        assert_eq!(stoat.mode(), "conflict_review");
        let key_ctx = stoat.update(|s, _| s.key_context());
        assert_eq!(key_ctx, KeyContext::ConflictReview);
        assert_eq!(stoat.conflict_files().len(), 2);
        assert_eq!(stoat.conflict_count(), 3);
        assert_eq!(stoat.conflict_position(), (0, 0));
    }

    #[gpui::test]
    fn accept_stores_resolution_metadata(cx: &mut TestAppContext) {
        let (_fixture, mut stoat) = conflict_fixture(cx);

        stoat.update(|s, cx| s.conflict_accept_ours(cx));

        // Buffer still has conflict markers (non-destructive)
        let text = stoat.buffer_text();
        assert!(text.contains("<<<<<<<"), "markers still present: {text}");
        assert_eq!(stoat.conflict_count(), 3);

        // Resolution stored in metadata
        let resolution = stoat.update(|s, _| s.conflict_state.resolutions.get(&(0, 0)).copied());
        assert_eq!(resolution, Some(ConflictSide::Ours));

        // Navigated to next conflict
        assert_eq!(stoat.conflict_position(), (0, 1));
        assert_eq!(stoat.mode(), "conflict_review");
    }

    #[gpui::test]
    fn accept_theirs_stores_resolution(cx: &mut TestAppContext) {
        let (_fixture, mut stoat) = conflict_fixture(cx);

        stoat.update(|s, cx| s.conflict_accept_theirs(cx));

        let resolution = stoat.update(|s, _| s.conflict_state.resolutions.get(&(0, 0)).copied());
        assert_eq!(resolution, Some(ConflictSide::Theirs));
    }

    #[gpui::test]
    fn accept_both_stores_resolution(cx: &mut TestAppContext) {
        let (_fixture, mut stoat) = conflict_fixture(cx);

        stoat.update(|s, cx| s.conflict_accept_both(cx));

        let resolution = stoat.update(|s, _| s.conflict_state.resolutions.get(&(0, 0)).copied());
        assert_eq!(resolution, Some(ConflictSide::Both));
    }

    #[gpui::test]
    fn resolve_all_and_dismiss(cx: &mut TestAppContext) {
        let (_fixture, mut stoat) = conflict_fixture(cx);

        // 3 conflicts per file, 2 files = 6 total
        for _ in 0..6 {
            if stoat.mode() != "conflict_review" {
                break;
            }
            stoat.update(|s, cx| s.conflict_accept_ours(cx));
        }

        let dismissed = stoat.update(|s, _| s.conflict_review_previous_mode.is_none());
        assert!(dismissed);

        // After dismiss, resolutions should have been applied to buffers
        let text = stoat.buffer_text();
        assert!(!text.contains("<<<<<<<"), "all markers resolved: {text}");
    }

    #[gpui::test]
    fn dismiss_applies_resolutions(cx: &mut TestAppContext) {
        let (_fixture, mut stoat) = conflict_fixture(cx);

        stoat.update(|s, cx| s.conflict_accept_ours(cx));

        // Buffer still has markers before dismiss
        assert!(stoat.buffer_text().contains("<<<<<<<"));

        stoat.update(|s, cx| s.conflict_review_dismiss(cx));

        let text = stoat.buffer_text();
        assert!(text.contains("ours-name"), "resolved ours applied: {text}");
        // Only 1 of 3 resolved, so markers remain for the other 2
        assert!(
            text.contains("<<<<<<<"),
            "unresolved markers remain: {text}"
        );
    }

    #[gpui::test]
    fn change_resolution(cx: &mut TestAppContext) {
        let (_fixture, mut stoat) = conflict_fixture(cx);

        // Accept ours first
        stoat.update(|s, cx| s.conflict_accept_ours(cx));
        assert_eq!(stoat.conflict_position(), (0, 1));

        // Navigate back and change to theirs
        stoat.update(|s, cx| s.conflict_prev_conflict(cx));
        assert_eq!(stoat.conflict_position(), (0, 0));

        stoat.update(|s, cx| s.conflict_accept_theirs(cx));
        let resolution = stoat.update(|s, _| s.conflict_state.resolutions.get(&(0, 0)).copied());
        assert_eq!(resolution, Some(ConflictSide::Theirs));

        // Dismiss and verify theirs wins
        stoat.update(|s, cx| s.conflict_review_dismiss(cx));
        let text = stoat.buffer_text();
        assert!(text.contains("theirs-name"), "theirs applied: {text}");
        assert!(!text.contains("ours-name-one"), "ours NOT applied: {text}");
    }

    #[gpui::test]
    fn partial_resolution(cx: &mut TestAppContext) {
        let (_fixture, mut stoat) = conflict_fixture(cx);

        // Resolve only the first of 3 conflicts
        stoat.update(|s, cx| s.conflict_accept_ours(cx));
        let count = stoat.update(|s, _| s.conflict_state.resolutions.len());
        assert_eq!(count, 1);

        stoat.update(|s, cx| s.conflict_review_dismiss(cx));

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
        let (_fixture, mut stoat) = conflict_fixture(cx);
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
