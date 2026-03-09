//! Git unstage hunk implementation and tests.
//!
//! This module implements the [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) action, which
//! unstages individual diff hunks via libgit2's index apply. The
//! action is part of the git staging workflow alongside
//! [`git_unstage_file`](crate::Stoat::git_unstage_file) for unstaging entire files and
//! [`git_unstage_all`](crate::Stoat::git_unstage_all) for unstaging all changes.

use crate::stoat::Stoat;
use git2::DiffOptions;
use gpui::Context;

impl Stoat {
    /// Unstage the current hunk.
    ///
    /// Uses the display diff (working-vs-HEAD) to locate the hunk at cursor, then
    /// spawns an async task that computes an index-vs-HEAD diff via
    /// [`git2::Patch::from_buffers`] to generate the correct patch for
    /// reverse-application.
    ///
    /// # Related Actions
    ///
    /// - [`git_stage_hunk`](crate::Stoat::git_stage_hunk) - Stage this hunk
    /// - [`git_unstage_file`](crate::Stoat::git_unstage_file) - Unstage the entire file
    /// - [`git_unstage_all`](crate::Stoat::git_unstage_all) - Unstage all changes
    pub fn git_unstage_hunk(&mut self, cx: &mut Context<Self>) {
        if self.review_state.source.is_commit() {
            return;
        }

        let file_path = match self.current_file_path.as_ref() {
            Some(p) => p.clone(),
            None => {
                tracing::error!("git_unstage_hunk: No file path set for current buffer");
                return;
            },
        };

        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        let (display_old_start, display_old_end) = {
            let diff = match buffer_item.read(cx).diff() {
                Some(d) => d,
                None => {
                    tracing::error!("git_unstage_hunk: No diff information available");
                    return;
                },
            };
            let hunk_index = match diff.hunk_for_row(cursor_row, buffer_snapshot) {
                Some(idx) => idx,
                None => {
                    tracing::error!("git_unstage_hunk: No hunk at cursor row {cursor_row}");
                    return;
                },
            };
            let hunk = &diff.hunks[hunk_index];
            (hunk.old_start, hunk.old_start + hunk.old_lines)
        };

        let services = self.services.clone();

        cx.spawn(async move |this, cx| {
            let result = async {
                let repo = services
                    .git
                    .discover(&file_path)
                    .await
                    .map_err(|e| format!("Repository not found: {e}"))?;
                let head_content = repo.head_content(&file_path).await.unwrap_or_default();
                let index_content = repo.index_content(&file_path).await.unwrap_or_default();

                let mut diff_options = DiffOptions::new();
                diff_options.context_lines(0);
                diff_options.ignore_whitespace(false);

                let patch = git2::Patch::from_buffers(
                    head_content.as_bytes(),
                    None,
                    index_content.as_bytes(),
                    None,
                    Some(&mut diff_options),
                )
                .map_err(|e| format!("Index-vs-HEAD diff failed: {e}"))?;

                let found_hunk = (0..patch.num_hunks()).find(|&idx| {
                    let Ok((hdr, _)) = patch.hunk(idx) else {
                        return false;
                    };
                    let old_start = hdr.old_start();
                    let old_end = old_start + hdr.old_lines();
                    old_start < display_old_end.max(display_old_start + 1)
                        && old_end.max(old_start + 1) > display_old_start
                });

                if let Some(hunk_idx) = found_hunk {
                    let file_name = file_path
                        .file_name()
                        .and_then(|n| n.to_str())
                        .ok_or_else(|| "Invalid file name".to_string())?;

                    let (hdr, num_lines) = patch
                        .hunk(hunk_idx)
                        .map_err(|e| format!("Failed to read hunk: {e}"))?;

                    let mut patch_str = format!(
                        "diff --git a/{file_name} b/{file_name}\n\
                         --- a/{file_name}\n\
                         +++ b/{file_name}\n\
                         @@ -{},{} +{},{} @@\n",
                        hdr.old_start(),
                        hdr.old_lines(),
                        hdr.new_start(),
                        hdr.new_lines(),
                    );

                    for line_idx in 0..num_lines {
                        let line = patch
                            .line_in_hunk(hunk_idx, line_idx)
                            .map_err(|e| format!("Failed to read line: {e}"))?;
                        let content = String::from_utf8_lossy(line.content());
                        let prefix = match line.origin() {
                            '+' => '+',
                            '-' => '-',
                            _ => ' ',
                        };
                        patch_str.push(prefix);
                        patch_str.push_str(&content);
                        if !content.ends_with('\n') {
                            patch_str.push('\n');
                        }
                    }

                    super::hunk_patch::apply_patch(
                        &patch_str,
                        &*repo,
                        true,
                        crate::git::provider::ApplyLocation::Index,
                    )
                    .await?;
                }

                tracing::info!("Unstaged hunk at row {} in {:?}", cursor_row, file_path);
                Ok::<(), String>(())
            }
            .await;
            let _ = this.update(cx, |stoat, cx| {
                if let Err(e) = result {
                    tracing::error!("git_unstage_hunk: {e}");
                    return;
                }
                stoat.refresh_git_diff(cx);
            });
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use crate::Stoat;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    #[gpui::test]
    fn unstages_hunk_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("test.txt", "line 1\nline 2\nline 3\n")
            .with_staged_change("test.txt", "line 1\nline 2\nline 3\nnew line\n")
            .load_and_diff("test.txt");
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(3, 0)));

        stoat.update(|s, cx| s.git_unstage_hunk(cx));
        stoat.run_until_parked();

        let diffs = stoat.fake_git().applied_diffs();
        assert!(!diffs.is_empty(), "Should have applied an unstage diff");
    }

    #[gpui::test]
    fn noop_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat.update(|s, cx| s.git_unstage_hunk(cx));
        stoat.run_until_parked();
    }

    #[gpui::test]
    fn noop_without_diff(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat.set_file_path(PathBuf::from("/fake/repo/test.txt"));
        stoat.update(|s, cx| s.git_unstage_hunk(cx));
        stoat.run_until_parked();
    }

    #[gpui::test]
    fn noop_when_not_on_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("test.txt", "line 1\n")
            .with_working_change("test.txt", "line 1\n")
            .load_and_diff("test.txt");

        stoat.update(|s, cx| s.git_unstage_hunk(cx));
        stoat.run_until_parked();
    }
}
