//! Git stage all implementation and tests.
//!
//! This module implements the [`git_stage_all`](crate::Stoat::git_stage_all) action, which
//! stages all changes in the repository using `git add -A`. The action is part of the git
//! staging workflow alongside [`git_stage_file`](crate::Stoat::git_stage_file) for staging
//! individual files and [`git_stage_hunk`](crate::Stoat::git_stage_hunk) for staging hunks.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Stage all changes in the repository for commit using `git add -A`.
    ///
    /// Spawns an async task that executes `git add -A` to stage all modified, deleted,
    /// and untracked files. The worktree root is used to determine the repository location.
    ///
    /// # Related Actions
    ///
    /// - [`git_stage_file`](crate::Stoat::git_stage_file) - Stage only the current file
    /// - [`git_unstage_all`](crate::Stoat::git_unstage_all) - Unstage all changes
    /// - [`git_stage_hunk`](crate::Stoat::git_stage_hunk) - Stage individual hunks
    pub fn git_stage_all(&mut self, cx: &mut Context<Self>) {
        let services = self.services.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        cx.spawn(async move |this, cx| {
            let result = async {
                let repo = services
                    .git
                    .discover(&root_path)
                    .await
                    .map_err(|e| format!("git stage all failed: {e}"))?;
                repo.stage_all()
                    .await
                    .map_err(|e| format!("git stage all failed: {e}"))?;
                tracing::info!("Staged all changes in repository at {:?}", root_path);
                Ok::<(), String>(())
            }
            .await;
            let _ = this.update(cx, |stoat, cx| {
                if let Err(e) = result {
                    tracing::error!("git_stage_all: {e}");
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
    use crate::git::status::GitStatusEntry;
    use gpui::TestAppContext;
    use std::path::PathBuf;

    #[gpui::test]
    fn stages_all_changes_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        stoat.update(|s, _cx| {
            s.services.fake_git().set_exists(true);
            s.services
                .fake_git()
                .set_workdir(PathBuf::from("/fake/repo"));
            s.services.fake_git().set_status(vec![
                GitStatusEntry::new(PathBuf::from("file1.txt"), "??".into(), false),
                GitStatusEntry::new(PathBuf::from("file2.txt"), "??".into(), false),
                GitStatusEntry::new(PathBuf::from("file3.txt"), "??".into(), false),
            ]);
        });

        stoat.update(|s, cx| s.git_stage_all(cx));
        stoat.run_until_parked();

        stoat.update(|s, _cx| {
            let staged = s.services.fake_git().staged_files();
            assert!(staged.contains(&PathBuf::from("file1.txt")));
            assert!(staged.contains(&PathBuf::from("file2.txt")));
            assert!(staged.contains(&PathBuf::from("file3.txt")));
        });
    }

    #[gpui::test]
    fn logs_error_outside_git_repo(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.git_stage_all(cx));
        stoat.run_until_parked();
    }
}
