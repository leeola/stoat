//! Git unstage file implementation and tests.
//!
//! This module implements the [`git_unstage_file`](crate::Stoat::git_unstage_file) action, which
//! unstages individual file changes using `git reset HEAD`. The action is part of the git
//! staging workflow alongside [`git_unstage_all`](crate::Stoat::git_unstage_all) for unstaging
//! all changes and [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) for unstaging hunks.

use crate::stoat::Stoat;
use gpui::Context;

impl Stoat {
    /// Unstage the current file's changes using `git reset HEAD`.
    ///
    /// Spawns an async task that executes `git reset HEAD <file>` to remove the current
    /// file from the staging area while preserving working directory changes. The file
    /// path must be set via [`set_file_path`](crate::Stoat::set_file_path).
    ///
    /// # Related Actions
    ///
    /// - [`git_stage_file`](crate::Stoat::git_stage_file) - Stage this file
    /// - [`git_unstage_all`](crate::Stoat::git_unstage_all) - Unstage all changes
    /// - [`git_unstage_hunk`](crate::Stoat::git_unstage_hunk) - Unstage individual hunks
    pub fn git_unstage_file(&mut self, cx: &mut Context<Self>) {
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
                    .map_err(|e| format!("git unstage failed: {e}"))?;
                repo.unstage_file(&file_path)
                    .await
                    .map_err(|e| format!("git unstage failed: {e}"))?;
                tracing::info!("Unstaged file {:?}", file_path);
                Ok::<(), String>(())
            }
            .await;
            let _ = this.update(cx, |stoat, cx| {
                if let Err(e) = result {
                    tracing::error!("git_unstage_file: {e}");
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
    fn unstages_file_successfully(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);

        let file_path = PathBuf::from("/fake/repo/test.txt");
        stoat.set_file_path(file_path.clone());
        stoat.update(|s, _cx| {
            s.services.fake_git().set_exists(true);
            s.services
                .fake_git()
                .set_workdir(PathBuf::from("/fake/repo"));
        });

        stoat.update(|s, cx| s.git_stage_file(cx));
        stoat.run_until_parked();
        stoat.update(|s, _cx| {
            assert!(s.services.fake_git().staged_files().contains(&file_path));
        });

        stoat.update(|s, cx| s.git_unstage_file(cx));
        stoat.run_until_parked();
        stoat.update(|s, _cx| {
            assert!(!s.services.fake_git().staged_files().contains(&file_path));
        });
    }

    #[gpui::test]
    fn noop_without_file_path(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx);
        stoat.update(|s, cx| s.git_unstage_file(cx));
        stoat.run_until_parked();
    }
}
