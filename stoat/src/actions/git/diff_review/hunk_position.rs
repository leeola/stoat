//! Tests for diff review hunk position tracking.

#[cfg(test)]
mod tests {
    use crate::{git::status::GitStatusEntry, Stoat};
    use gpui::TestAppContext;
    use std::path::PathBuf;

    #[gpui::test]
    fn counts_hunks_across_multiple_files(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("file1.txt", "line 1\nline 2\nline 3\n")
            .with_committed_file("file2.txt", "foo\nbar\nbaz\n");
        stoat
            .with_working_change("file1.txt", "line 1\nMODIFIED\nline 3\n")
            .with_working_change("file2.txt", "foo\nbar\nADDED\n");
        stoat.update(|s, _| {
            s.services.fake_git().set_status(vec![
                GitStatusEntry::new(PathBuf::from("file1.txt"), "M".into(), false),
                GitStatusEntry::new(PathBuf::from("file2.txt"), "M".into(), false),
            ]);
        });

        stoat.update(|s, cx| s.open_diff_review(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            assert_eq!(s.mode(), "diff_review");

            let position = s.diff_review_hunk_position(cx);
            assert_eq!(
                position,
                Some((1, 2)),
                "First hunk should show position 1/2, not {position:?}"
            );

            s.diff_review_next_hunk(cx);
        });
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            let position_after = s.diff_review_hunk_position(cx);
            assert_eq!(
                position_after,
                Some((2, 2)),
                "Second hunk should show position 2/2, not {position_after:?}"
            );
        });
    }

    #[gpui::test]
    fn counts_update_when_cycling_modes(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat.with_committed_file("file1.txt", "line 1\nline 2\nline 3\nline 4\nline 5\n");
        stoat.with_staged_change("file1.txt", "line 1\nSTAGED\nline 3\nline 4\nline 5\n");
        stoat.with_working_change("file1.txt", "line 1\nSTAGED\nline 3\nline 4\nUNSTAGED\n");
        stoat.update(|s, _| {
            s.services.fake_git().set_status(vec![GitStatusEntry::new(
                PathBuf::from("file1.txt"),
                "M".into(),
                true,
            )]);
        });

        stoat.update(|s, cx| s.open_diff_review(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            assert_eq!(s.mode(), "diff_review");
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::WorkingVsHead
            );

            let position_all = s.diff_review_hunk_position(cx);
            assert!(
                matches!(position_all, Some((1, 2))),
                "WorkingVsHead should show 1/2, got {position_all:?}"
            );

            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::WorkingVsIndex
            );
        });
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            let position_unstaged = s.diff_review_hunk_position(cx);
            assert!(
                matches!(position_unstaged, Some((1, 1))),
                "WorkingVsIndex should show 1/1 (only unstaged hunk), got {position_unstaged:?}"
            );

            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::IndexVsHead
            );
        });
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            let position_staged = s.diff_review_hunk_position(cx);
            assert!(
                matches!(position_staged, Some((1, 1))),
                "IndexVsHead should show 1/1 (only staged hunk), got {position_staged:?}"
            );
        });
    }

    #[gpui::test]
    fn handles_new_untracked_files(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat.with_committed_file("file1.txt", "line 1\nline 2\nline 3\n");
        stoat.with_working_change("file1.txt", "line 1\nMODIFIED\nline 3\n");
        stoat.with_working_change("file2.txt", "new file content\n");
        stoat.update(|s, _| {
            s.services.fake_git().set_status(vec![
                GitStatusEntry::new(PathBuf::from("file1.txt"), "M".into(), false),
                GitStatusEntry::new(PathBuf::from("file2.txt"), "??".into(), false),
            ]);
        });

        stoat.update(|s, cx| s.open_diff_review(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            assert_eq!(s.mode(), "diff_review");

            let position = s.diff_review_hunk_position(cx);
            assert!(
                position.is_some(),
                "Patch counter should work even with new untracked files, got {position:?}"
            );

            let (current, total) = position.unwrap();
            assert_eq!(
                total, 2,
                "Should have 2 total hunks (1 from modified file1, 1 from new file2), got {current}/{total}"
            );
        });
    }

    #[gpui::test]
    fn counts_multiple_hunks_per_file(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat.with_committed_file("file1.txt", "line 1\nline 2\nline 3\nline 4\nline 5\n");
        stoat.with_working_change("file1.txt", "HUNK1\nline 2\nline 3\nHUNK2\nline 5\n");
        stoat.update(|s, _| {
            s.services.fake_git().set_status(vec![GitStatusEntry::new(
                PathBuf::from("file1.txt"),
                "M".into(),
                false,
            )]);
        });

        stoat.update(|s, cx| s.open_diff_review(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            assert_eq!(s.mode(), "diff_review");

            let position1 = s.diff_review_hunk_position(cx);
            assert_eq!(
                position1,
                Some((1, 2)),
                "First hunk should be 1/2, got {position1:?}"
            );

            s.diff_review_next_hunk(cx);
            let position2 = s.diff_review_hunk_position(cx);
            assert_eq!(
                position2,
                Some((2, 2)),
                "Second hunk should be 2/2, got {position2:?}"
            );
        });
    }

    #[gpui::test]
    fn staged_only_shows_same_count_across_modes(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        stoat
            .with_committed_file("file1.txt", "line 1\nline 2\nline 3\nline 4\nline 5\n")
            .with_committed_file("file2.txt", "foo\nbar\nbaz\nqux\n");
        stoat
            .with_staged_change("file1.txt", "HUNK1\nline 2\nline 3\nHUNK2\nline 5\n")
            .with_staged_change("file2.txt", "HUNK3\nbar\nbaz\nHUNK4\n");
        stoat.update(|s, _| {
            s.services.fake_git().set_status(vec![
                GitStatusEntry::new(PathBuf::from("file1.txt"), "M".into(), true),
                GitStatusEntry::new(PathBuf::from("file2.txt"), "M".into(), true),
            ]);
        });

        stoat.update(|s, cx| s.open_diff_review(cx));
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            assert_eq!(s.mode(), "diff_review");
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::WorkingVsHead
            );

            let position_all = s.diff_review_hunk_position(cx);
            let (_current_all, total_all) =
                position_all.expect("Should have position in WorkingVsHead");

            s.diff_review_cycle_comparison_mode(cx);
            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::IndexVsHead
            );
        });
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            let position_all = s.diff_review_hunk_position(cx);
            let (current_all, total_all) = position_all.expect(
                "Should have position in IndexVsHead (same as WorkingVsHead since all staged)",
            );

            s.diff_review_next_hunk(cx);
        });
        stoat.run_until_parked();
        stoat.update(|s, cx| {
            let pos2 = s.diff_review_hunk_position(cx);
            if let Some((current, total)) = pos2 {
                assert!(
                    current <= total,
                    "Current position {current} should not exceed total {total}"
                );
            }
        });
    }
}
