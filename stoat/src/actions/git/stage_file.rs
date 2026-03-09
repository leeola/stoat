//! Git stage file implementation and tests.
//!
//! This module implements the [`git_stage_file`](crate::Stoat::git_stage_file) action, which
//! stages individual file changes for commit using `git add`. The action is part of the git
//! staging workflow alongside [`git_stage_all`](crate::Stoat::git_stage_all) for staging all
//! changes and [`git_stage_hunk`](crate::Stoat::git_stage_hunk) for staging individual hunks.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Stage the current file for commit using `git add`.
    ///
    /// Spawns an async task that executes `git add <file>` to stage the current file's
    /// changes for the next commit. The file path must be set on the current buffer via
    /// [`set_file_path`](crate::Stoat::set_file_path).
    ///
    /// # Related Actions
    ///
    /// - [`git_stage_all`](crate::Stoat::git_stage_all) - Stage all changes in the repository
    /// - [`git_unstage_file`](crate::Stoat::git_unstage_file) - Unstage this file
    /// - [`git_stage_hunk`](crate::Stoat::git_stage_hunk) - Stage individual hunks
    pub fn git_stage_file(&mut self, cx: &mut Context<Self>) {
        let services = self.services.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        let file_path = match self.current_file_path.as_ref() {
            Some(p) => p.clone(),
            None => return,
        };
        cx.spawn(async move |this, cx| {
            let result = async {
                let repo = services
                    .git
                    .discover(&root_path)
                    .await
                    .map_err(|e| format!("git stage failed: {e}"))?;
                repo.stage_file(&file_path)
                    .await
                    .map_err(|e| format!("git stage failed: {e}"))?;
                tracing::info!("Staged file {:?}", file_path);
                Ok::<(), String>(())
            }
            .await;
            let _ = this.update(cx, |stoat, cx| {
                if let Err(e) = result {
                    tracing::error!("git_stage_file: {e}");
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
    use super::*;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    #[gpui::test]
    fn stages_file_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let file_path = PathBuf::from("/fake/repo/test.txt");
        stoat.set_file_path(file_path.clone());
        stoat.update(|s, _cx| {
            s.services.fake_git().set_exists(true);
            s.services
                .fake_git()
                .set_workdir(PathBuf::from("/fake/repo"));
        });

        stoat.update(|s, cx| {
            s.git_stage_file(cx);
        });
        stoat.run_until_parked();

        stoat.update(|s, _cx| {
            let staged = s.services.fake_git().staged_files();
            assert!(staged.contains(&file_path), "File should be staged");
        });
    }

    #[gpui::test]
    fn noop_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.git_stage_file(cx));
        stoat.run_until_parked();
    }
}
