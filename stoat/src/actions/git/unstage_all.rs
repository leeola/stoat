//! Git unstage all implementation and tests.
//!
//! This module implements the [`git_unstage_all`](crate::Stoat::git_unstage_all) action, which
//! unstages all changes in the repository using `git reset HEAD`. The action is part of the git
//! staging workflow alongside [`git_unstage_file`](crate::Stoat::git_unstage_file) for
//! unstaging individual files and [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) for
//! unstaging hunks.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Unstage all changes in the repository using `git reset HEAD`.
    ///
    /// Spawns an async task that executes `git reset HEAD` to remove all files from the
    /// staging area while preserving working directory changes. The worktree root is used
    /// to determine the repository location.
    ///
    /// # Related Actions
    ///
    /// - [`git_unstage_file`](crate::Stoat::git_unstage_file) - Unstage only the current file
    /// - [`git_stage_all`](crate::Stoat::git_stage_all) - Stage all changes
    /// - [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) - Unstage individual hunks
    pub fn git_unstage_all(&mut self, cx: &mut Context<Self>) {
        let services = self.services.clone();
        let root_path = self.worktree.lock().root().to_path_buf();
        cx.spawn(async move |this, cx| {
            let result = async {
                let repo = services
                    .git
                    .discover(&root_path)
                    .await
                    .map_err(|e| format!("git unstage all failed: {e}"))?;
                repo.unstage_all()
                    .await
                    .map_err(|e| format!("git unstage all failed: {e}"))?;
                tracing::info!("Unstaged all changes in repository at {:?}", root_path);
                Ok::<(), String>(())
            }
            .await;
            let _ = this.update(cx, |stoat, cx| {
                if let Err(e) = result {
                    tracing::error!("git_unstage_all: {e}");
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
    fn unstages_all_changes_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let file1 = PathBuf::from("file1.txt");
        let file2 = PathBuf::from("file2.txt");
        let file3 = PathBuf::from("file3.txt");

        stoat.update(|s, _cx| {
            s.services.fake_git().set_exists(true);
            s.services
                .fake_git()
                .set_workdir(PathBuf::from("/fake/repo"));
        });

        stoat.update(|s, cx| {
            s.current_file_path = Some(file1.clone());
            s.git_stage_file(cx);
        });
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            s.current_file_path = Some(file2.clone());
            s.git_stage_file(cx);
        });
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            s.current_file_path = Some(file3.clone());
            s.git_stage_file(cx);
        });
        stoat.run_until_parked();

        stoat.update(|s, _cx| {
            assert_eq!(s.services.fake_git().staged_files().len(), 3);
        });

        stoat.update(|s, cx| s.git_unstage_all(cx));
        stoat.run_until_parked();

        stoat.update(|s, _cx| {
            assert!(s.services.fake_git().staged_files().is_empty());
        });
    }

    #[gpui::test]
    fn logs_error_outside_git_repo(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.git_unstage_all(cx));
        stoat.run_until_parked();
    }
}
