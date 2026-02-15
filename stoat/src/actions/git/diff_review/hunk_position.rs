//! Tests for diff review hunk position tracking.

#[cfg(test)]
mod tests {
    use crate::Stoat;
    use gpui::TestAppContext;

    #[gpui::test]
    fn counts_hunks_across_multiple_files(cx: &mut TestAppContext) {
        use std::process::Command;

        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap();

        // Create initial committed state with 2 files
        let file1 = repo_path.join("file1.txt");
        let file2 = repo_path.join("file2.txt");

        std::fs::write(&file1, "line 1\nline 2\nline 3\n").unwrap();
        std::fs::write(&file2, "foo\nbar\nbaz\n").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial"])
            .current_dir(repo_path)
            .output()
            .unwrap();

        // Modify both files - each file gets exactly 1 hunk
        std::fs::write(&file1, "line 1\nMODIFIED\nline 3\n").unwrap(); // 1 hunk at line 1
        std::fs::write(&file2, "foo\nbar\nADDED\n").unwrap(); // 1 hunk at line 2

        stoat.update(|s, cx| {
            // Open diff review
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");

            // At first file, first hunk
            // Should show "Patch 1/2" (1st hunk out of 2 total hunks across both files)
            let position = s.diff_review_hunk_position(cx);
            assert_eq!(
                position,
                Some((1, 2)),
                "First hunk should show position 1/2, not {position:?}"
            );

            // Navigate to next hunk (which is in file2)
            s.diff_review_next_hunk(cx);

            // At second file, first hunk
            // Should show "Patch 2/2" (2nd hunk out of 2 total)
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
        use std::process::Command;

        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap();

        // Create initial committed state with 5 lines to allow non-adjacent changes
        let file1 = repo_path.join("file1.txt");
        std::fs::write(&file1, "line 1\nline 2\nline 3\nline 4\nline 5\n").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial"])
            .current_dir(repo_path)
            .output()
            .unwrap();

        // Modify and stage line 2 only (1 staged hunk at line 2)
        std::fs::write(&file1, "line 1\nSTAGED\nline 3\nline 4\nline 5\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_path)
            .output()
            .unwrap();

        // Further modify working tree - change line 5 (1 unstaged hunk at line 5)
        std::fs::write(&file1, "line 1\nSTAGED\nline 3\nline 4\nUNSTAGED\n").unwrap();

        stoat.update(|s, cx| {
            // Open in WorkingVsHead mode - should see both hunks (line 2 and line 5 modified)
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::WorkingVsHead
            );

            // In WorkingVsHead: 2 hunks total (line 2: "line 2" -> "STAGED", line 5: "line 5" ->
            // "UNSTAGED") These are non-adjacent so they form 2 separate hunks.
            let position_all = s.diff_review_hunk_position(cx);
            assert!(
                matches!(position_all, Some((1, 2))),
                "WorkingVsHead should show 1/2, got {position_all:?}"
            );

            // Cycle to WorkingVsIndex (unstaged only) - should see 1 hunk
            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::WorkingVsIndex
            );

            let position_unstaged = s.diff_review_hunk_position(cx);
            assert!(
                matches!(position_unstaged, Some((1, 1))),
                "WorkingVsIndex should show 1/1 (only unstaged hunk), got {position_unstaged:?}"
            );

            // Cycle to IndexVsHead (staged only) - should see 1 hunk
            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::IndexVsHead
            );

            let position_staged = s.diff_review_hunk_position(cx);
            assert!(
                matches!(position_staged, Some((1, 1))),
                "IndexVsHead should show 1/1 (only staged hunk), got {position_staged:?}"
            );
        });
    }

    #[gpui::test]
    fn handles_new_untracked_files(cx: &mut TestAppContext) {
        use std::process::Command;

        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap();

        // Create initial committed state with one file
        let file1 = repo_path.join("file1.txt");
        std::fs::write(&file1, "line 1\nline 2\nline 3\n").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial"])
            .current_dir(repo_path)
            .output()
            .unwrap();

        // Modify file1 (1 hunk)
        std::fs::write(&file1, "line 1\nMODIFIED\nline 3\n").unwrap();

        // Create a NEW file that's never been committed (untracked)
        let file2 = repo_path.join("file2.txt");
        std::fs::write(&file2, "new file content\n").unwrap();

        stoat.update(|s, cx| {
            // Open diff review - should find both files
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");

            // Should have position info even though file2 doesn't exist in HEAD
            let position = s.diff_review_hunk_position(cx);
            assert!(
                position.is_some(),
                "Patch counter should work even with new untracked files, got {position:?}"
            );

            // Should have 2 total hunks: 1 from file1, 1 from file2 (entire file is a hunk)
            let (current, total) = position.unwrap();
            assert_eq!(
                total, 2,
                "Should have 2 total hunks (1 from modified file1, 1 from new file2), got {current}/{total}"
            );
        });
    }

    #[gpui::test]
    fn counts_multiple_hunks_per_file(cx: &mut TestAppContext) {
        use std::process::Command;

        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap();

        // Create file with more lines
        let file1 = repo_path.join("file1.txt");
        std::fs::write(&file1, "line 1\nline 2\nline 3\nline 4\nline 5\n").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial"])
            .current_dir(repo_path)
            .output()
            .unwrap();

        // Create 2 separate hunks (non-contiguous changes)
        std::fs::write(&file1, "HUNK1\nline 2\nline 3\nHUNK2\nline 5\n").unwrap();

        stoat.update(|s, cx| {
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");

            // First hunk
            let position1 = s.diff_review_hunk_position(cx);
            assert_eq!(
                position1,
                Some((1, 2)),
                "First hunk should be 1/2, got {position1:?}"
            );

            // Navigate to second hunk
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
        use std::process::Command;

        let mut stoat = Stoat::test(cx).init_git();
        let repo_path = stoat.repo_path().unwrap();

        // Create initial committed state with 2 files
        let file1 = repo_path.join("file1.txt");
        let file2 = repo_path.join("file2.txt");
        std::fs::write(&file1, "line 1\nline 2\nline 3\nline 4\nline 5\n").unwrap();
        std::fs::write(&file2, "foo\nbar\nbaz\nqux\n").unwrap();

        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_path)
            .output()
            .unwrap();
        Command::new("git")
            .args(["commit", "-m", "Initial"])
            .current_dir(repo_path)
            .output()
            .unwrap();

        // Modify both files with multiple hunks each, stage ALL changes
        std::fs::write(&file1, "HUNK1\nline 2\nline 3\nHUNK2\nline 5\n").unwrap();
        std::fs::write(&file2, "HUNK3\nbar\nbaz\nHUNK4\n").unwrap();
        Command::new("git")
            .args(["add", "."])
            .current_dir(repo_path)
            .output()
            .unwrap();

        stoat.update(|s, cx| {
            // Open in WorkingVsHead mode (default)
            s.open_diff_review(cx);
            assert_eq!(s.mode(), "diff_review");
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::WorkingVsHead
            );

            let position_all = s.diff_review_hunk_position(cx);
            let (current_all, total_all) =
                position_all.expect("Should have position in WorkingVsHead");

            // Cycle to IndexVsHead (staged only)
            s.diff_review_cycle_comparison_mode(cx);
            s.diff_review_cycle_comparison_mode(cx);
            assert_eq!(
                s.review_comparison_mode(),
                crate::git::diff_review::DiffComparisonMode::IndexVsHead
            );

            let position_staged = s.diff_review_hunk_position(cx);
            let (current_staged, total_staged) =
                position_staged.expect("Should have position in IndexVsHead");

            // BUG: These should be equal since all changes are staged
            assert_eq!(
                total_all, total_staged,
                "All changes are staged, so WorkingVsHead ({current_all}/{total_all}) should match IndexVsHead ({current_staged}/{total_staged})"
            );

            // Try navigating in IndexVsHead mode to see if position goes beyond total
            s.diff_review_next_hunk(cx);
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
