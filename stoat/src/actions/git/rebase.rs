mod abort;
mod confirm;
mod continue_rebase;
mod dismiss;
mod edit_message;
mod move_commit;
mod next;
mod open;
mod prev;
mod set_operation;
mod skip;

#[cfg(test)]
mod tests {
    use crate::{
        git::{
            rebase::{RebaseCommit, RebaseOperation, RebasePhase},
            repository::CommitFileChange,
            status::GitBranchInfo,
        },
        stoat::KeyContext,
        test::headless::HeadlessStoat,
    };
    use gpui::TestAppContext;
    use std::{collections::HashMap, path::PathBuf};

    fn setup_fake_git(app: &mut HeadlessStoat) {
        let view = app.view().clone();
        app.cx.update(|_window, cx| {
            let pgv = view.read(cx);
            let root = pgv.app_state.worktree.lock().root().to_path_buf();
            let fake_git = pgv.app_state.services.fake_git();
            fake_git.set_exists(true);
            fake_git.set_workdir(root);
            fake_git.set_branch_info(Some(GitBranchInfo {
                branch_name: "feature".to_string(),
                ahead: 3,
                behind: 0,
            }));
            fake_git.add_commit(
                "aaa1111111111",
                vec![CommitFileChange {
                    path: PathBuf::from("src/a.rs"),
                    status: "M".to_string(),
                }],
                HashMap::new(),
            );
            fake_git.add_commit(
                "bbb2222222222",
                vec![CommitFileChange {
                    path: PathBuf::from("src/b.rs"),
                    status: "M".to_string(),
                }],
                HashMap::new(),
            );
            fake_git.add_commit(
                "ccc3333333333",
                vec![CommitFileChange {
                    path: PathBuf::from("src/c.rs"),
                    status: "A".to_string(),
                }],
                HashMap::new(),
            );
        });
    }

    fn make_commits() -> Vec<RebaseCommit> {
        vec![
            RebaseCommit {
                oid: "aaa1111".into(),
                short_hash: "aaa1111".into(),
                author: "Alice".into(),
                date: "1 day ago".into(),
                message: "First commit".into(),
                operation: RebaseOperation::Pick,
            },
            RebaseCommit {
                oid: "bbb2222".into(),
                short_hash: "bbb2222".into(),
                author: "Bob".into(),
                date: "2 days ago".into(),
                message: "Second commit".into(),
                operation: RebaseOperation::Pick,
            },
            RebaseCommit {
                oid: "ccc3333".into(),
                short_hash: "ccc3333".into(),
                author: "Carol".into(),
                date: "3 days ago".into(),
                message: "Third commit".into(),
                operation: RebaseOperation::Pick,
            },
        ]
    }

    fn enter_rebase_planning(app: &mut HeadlessStoat) {
        let view = app.view().clone();
        app.cx.update(|_window, cx| {
            view.update(cx, |pgv, cx| {
                let mode = pgv
                    .active_stoat(cx)
                    .map(|s| s.read(cx).mode().to_string())
                    .unwrap_or_default();
                let key_ctx = pgv
                    .active_stoat(cx)
                    .map(|s| s.read(cx).key_context())
                    .unwrap_or(KeyContext::TextEditor);

                pgv.app_state.open_rebase(mode, key_ctx);
                pgv.app_state.rebase.commits = make_commits();
                pgv.app_state.rebase.base_ref = "origin/main".to_string();
                pgv.app_state.rebase.phase = RebasePhase::Planning;

                if let Some(editor) = pgv.active_editor().cloned() {
                    editor.update(cx, |editor, cx| {
                        editor.stoat.update(cx, |stoat, _cx| {
                            stoat.set_key_context(KeyContext::Rebase);
                            stoat.set_mode("rebase_plan");
                        });
                    });
                }
            });
        });
    }

    #[gpui::test]
    fn open_detects_in_progress(cx: &mut TestAppContext) {
        let mut app = HeadlessStoat::new(cx);
        setup_fake_git(&mut app);

        let view = app.view().clone();
        app.cx.update(|_window, cx| {
            let pgv = view.read(cx);
            let root = pgv.app_state.worktree.lock().root().to_path_buf();
            let git_dir = root.join(".git");
            let fake_fs = pgv.app_state.services.fake_fs();
            fake_fs.insert_file(
                git_dir.join("rebase-merge/head-name"),
                "refs/heads/feature\n",
            );
            fake_fs.insert_file(git_dir.join("rebase-merge/onto"), "abc123\n");
            fake_fs.insert_file(git_dir.join("rebase-merge/msgnum"), "2\n");
            fake_fs.insert_file(git_dir.join("rebase-merge/end"), "5\n");
        });

        app.type_action("OpenRebase");
        app.flush();

        let view = app.view().clone();
        app.cx.update(|_window, cx| {
            let pgv = view.read(cx);
            assert!(
                matches!(
                    pgv.app_state.rebase.phase,
                    RebasePhase::PausedEdit { .. } | RebasePhase::PausedReword { .. }
                ),
                "Expected paused phase, got {:?}",
                pgv.app_state.rebase.phase
            );
        });
    }

    #[gpui::test]
    fn open_no_upstream_flashes_error(cx: &mut TestAppContext) {
        let mut app = HeadlessStoat::new(cx);

        let view = app.view().clone();
        app.cx.update(|_window, cx| {
            let pgv = view.read(cx);
            let root = pgv.app_state.worktree.lock().root().to_path_buf();
            let fake_git = pgv.app_state.services.fake_git();
            fake_git.set_exists(true);
            fake_git.set_workdir(root);
        });

        app.type_action("OpenRebase");
        app.flush();

        let msg = app.flash_message();
        assert!(
            msg.as_deref().is_some_and(|m| m.contains("No upstream")),
            "Expected 'No upstream' flash, got: {msg:?}"
        );
    }

    #[gpui::test]
    fn dismiss_restores_context(cx: &mut TestAppContext) {
        let mut app = HeadlessStoat::new(cx);
        setup_fake_git(&mut app);
        enter_rebase_planning(&mut app);

        app.type_action("RebaseDismiss");
        app.flush();

        let view = app.view().clone();
        app.cx.update(|_window, cx| {
            let pgv = view.read(cx);
            assert!(pgv.app_state.rebase.commits.is_empty());
            if let Some(stoat) = pgv.active_stoat(cx) {
                let ctx = stoat.read(cx).key_context();
                assert_ne!(
                    ctx,
                    KeyContext::Rebase,
                    "Should have restored previous context"
                );
            }
        });
    }

    #[gpui::test]
    fn set_operation_changes_commit(cx: &mut TestAppContext) {
        let mut app = HeadlessStoat::new(cx);
        setup_fake_git(&mut app);
        enter_rebase_planning(&mut app);

        app.type_action("RebaseSetSquash");
        app.flush();

        let view = app.view().clone();
        app.cx.update(|_window, cx| {
            let pgv = view.read(cx);
            assert_eq!(pgv.app_state.rebase.commits.len(), 3);
            assert_eq!(
                pgv.app_state.rebase.commits[pgv.app_state.rebase.selected].operation,
                RebaseOperation::Squash
            );
        });
    }

    #[gpui::test]
    fn move_commit_swaps(cx: &mut TestAppContext) {
        let mut app = HeadlessStoat::new(cx);
        setup_fake_git(&mut app);
        enter_rebase_planning(&mut app);

        // Set selected to 1 directly (avoids RebaseNext which triggers async preview load)
        let view = app.view().clone();
        app.cx.update(|_window, cx| {
            view.update(cx, |pgv, _cx| {
                pgv.app_state.rebase.selected = 1;
            });
        });

        app.type_action("RebaseMoveUp");
        app.flush();

        let view = app.view().clone();
        app.cx.update(|_window, cx| {
            let pgv = view.read(cx);
            assert_eq!(pgv.app_state.rebase.commits.len(), 3);
            assert_eq!(pgv.app_state.rebase.commits[0].oid, "bbb2222");
            assert_eq!(pgv.app_state.rebase.commits[1].oid, "aaa1111");
            assert_eq!(pgv.app_state.rebase.selected, 0);
        });
    }

    #[gpui::test]
    fn confirm_validates_squash_first(cx: &mut TestAppContext) {
        let mut app = HeadlessStoat::new(cx);
        setup_fake_git(&mut app);
        enter_rebase_planning(&mut app);

        app.type_action("RebaseSetSquash");
        app.flush();

        app.type_action("RebaseConfirm");
        app.flush();

        let msg = app.flash_message();
        assert!(
            msg.as_deref().is_some_and(|m| m.contains("squash")),
            "Expected validation error about squash, got: {msg:?}"
        );
    }
}
