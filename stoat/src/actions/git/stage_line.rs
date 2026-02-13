//! Per-line stage/unstage toggle.
//!
//! Implements [`git_toggle_stage_line`](crate::Stoat::git_toggle_stage_line) for toggling
//! the staging state of a single `+` line at cursor, or the entire deletion block for
//! deletion hunks. Reuses [`extract_hunk_lines`], [`LineSelection`],
//! [`generate_partial_hunk_patch`], and [`apply_patch`] from the hunk-level staging
//! infrastructure.

use crate::{
    git::{
        diff::{extract_hunk_lines, BufferDiff, DiffHunkStatus, HunkLineOrigin},
        line_selection::LineSelection,
        repository::Repository,
    },
    stoat::Stoat,
};
use git2::DiffOptions;
use gpui::Context;
use text::ToPoint;

impl Stoat {
    /// Toggle the staging state of the line at cursor.
    ///
    /// For `+` lines (additions/modifications), toggles just that single line.
    /// For deletion hunks, toggles the entire deletion block since the cursor
    /// can't sit on individual phantom deleted lines.
    pub fn git_toggle_stage_line(&mut self, cx: &mut Context<Self>) -> Result<(), String> {
        let file_path = self
            .current_file_path
            .as_ref()
            .ok_or_else(|| "No file path set for current buffer".to_string())?
            .clone();

        let repo_dir = self.worktree_root_abs();
        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();

        let diff = buffer_item
            .read(cx)
            .diff()
            .ok_or_else(|| "No diff information available".to_string())?;

        let hunk_index = diff
            .hunk_for_row(cursor_row, &buffer_snapshot)
            .ok_or_else(|| format!("No hunk at cursor row {cursor_row}"))?;

        let display_hunk = &diff.hunks[hunk_index];
        let is_deletion = display_hunk.status == DiffHunkStatus::Deleted;
        let display_start = display_hunk
            .buffer_range
            .start
            .to_point(&buffer_snapshot)
            .row;
        let repo =
            Repository::discover(&file_path).map_err(|e| format!("Repository not found: {e}"))?;
        let index_content = repo.index_content(&file_path).unwrap_or_default();

        let buffer_text = buffer_snapshot.text();
        let buffer_id = buffer_snapshot.remote_id();

        let wi_diff = BufferDiff::new(buffer_id, index_content.clone(), &buffer_snapshot)
            .map_err(|e| format!("Working-vs-index diff failed: {e}"))?;

        let line_is_staged = if is_deletion {
            !wi_diff.hunks.iter().any(|h| {
                let s = h.buffer_range.start.to_point(&buffer_snapshot).row;
                let e = h.buffer_range.end.to_point(&buffer_snapshot).row;
                if s == e {
                    s == display_start
                } else {
                    s <= display_start && e > display_start
                }
            })
        } else {
            !wi_diff.hunks.iter().any(|h| {
                let s = h.buffer_range.start.to_point(&buffer_snapshot).row;
                let e = h.buffer_range.end.to_point(&buffer_snapshot).row;
                s <= cursor_row && e > cursor_row
            })
        };

        if line_is_staged {
            self.unstage_line(
                &file_path,
                &repo_dir,
                cursor_row,
                is_deletion,
                display_hunk.old_start,
                display_hunk.old_start + display_hunk.old_lines,
                cx,
            )?;
        } else {
            self.stage_line(
                &file_path,
                &repo_dir,
                cursor_row,
                is_deletion,
                display_start,
                &index_content,
                &buffer_text,
                &wi_diff,
                &buffer_snapshot,
                cx,
            )?;
        }

        if let Some((new_diff, staged_rows, staged_hunk_indices)) =
            self.compute_diff_for_review_mode(&file_path, cx)
        {
            buffer_item.update(cx, |item, _| {
                item.set_diff(Some(new_diff));
                item.set_staged_rows(staged_rows);
                item.set_staged_hunk_indices(staged_hunk_indices);
            });
        }

        tracing::info!(
            "Toggled stage for line at row {} in {:?}",
            cursor_row,
            file_path
        );
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn stage_line(
        &self,
        file_path: &std::path::Path,
        repo_dir: &std::path::Path,
        cursor_row: u32,
        is_deletion: bool,
        display_start: u32,
        index_content: &str,
        buffer_text: &str,
        wi_diff: &BufferDiff,
        buffer_snapshot: &text::BufferSnapshot,
        _cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let wi_hunk_index = find_wi_hunk_index(
            wi_diff,
            cursor_row,
            is_deletion,
            display_start,
            buffer_snapshot,
        )
        .ok_or_else(|| "No working-vs-index hunk found for this line".to_string())?;

        let mut hunk_lines = extract_hunk_lines(index_content, buffer_text, wi_hunk_index)
            .map_err(|e| format!("Failed to extract hunk lines: {e}"))?;

        // new_start from wi_diff is relative to the working tree, but the patch
        // is applied to the index. Recompute from old_start (which is index-relative).
        hunk_lines.new_start = if hunk_lines.old_lines == 0 {
            hunk_lines.old_start + 1
        } else {
            hunk_lines.old_start
        };

        let mut selection = LineSelection::new(hunk_lines);
        selection.deselect_all();

        if is_deletion {
            selection.select_all();
        } else {
            let target_lineno = cursor_row + 1;
            let line_idx = selection
                .hunk_lines
                .lines
                .iter()
                .position(|l| {
                    l.origin == HunkLineOrigin::Addition && l.new_lineno == Some(target_lineno)
                })
                .ok_or_else(|| format!("No addition line found at row {cursor_row} in wi hunk"))?;
            selection.selected[line_idx] = true;
        }

        let patch = super::hunk_patch::generate_partial_hunk_patch(&selection, file_path)?;
        super::hunk_patch::apply_patch(&patch, repo_dir, false, git2::ApplyLocation::Index)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn unstage_line(
        &self,
        file_path: &std::path::Path,
        repo_dir: &std::path::Path,
        cursor_row: u32,
        is_deletion: bool,
        display_old_start: u32,
        display_old_end: u32,
        cx: &mut Context<Self>,
    ) -> Result<(), String> {
        let repo =
            Repository::discover(file_path).map_err(|e| format!("Repository not found: {e}"))?;
        let head_content = repo.head_content(file_path).unwrap_or_default();
        let index_content = repo.index_content(file_path).unwrap_or_default();

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

        let ih_hunk_index = find_ih_hunk_index(&patch, display_old_start, display_old_end)
            .ok_or_else(|| "No index-vs-HEAD hunk found for this line".to_string())?;

        let mut hunk_lines = extract_hunk_lines(&head_content, &index_content, ih_hunk_index)
            .map_err(|e| format!("Failed to extract hunk lines: {e}"))?;

        // For pure deletions, libgit2 apply expects new_start == old_start
        if is_deletion && hunk_lines.new_lines == 0 {
            hunk_lines.new_start = hunk_lines.old_start;
        }

        let mut selection = LineSelection::new(hunk_lines);
        selection.deselect_all();

        if is_deletion {
            selection.select_all();
        } else {
            let buffer_item = self.active_buffer(cx);
            let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot();
            let buffer_text = buffer_snapshot.text();
            let target_content = buffer_text
                .lines()
                .nth(cursor_row as usize)
                .unwrap_or("")
                .to_string();

            let line_idx = find_line_by_content(&selection, &target_content).ok_or_else(|| {
                format!("No matching addition line found in ih hunk for row {cursor_row}")
            })?;
            selection.selected[line_idx] = true;
        }

        let patch_str = super::hunk_patch::generate_partial_hunk_patch(&selection, file_path)?;
        super::hunk_patch::apply_patch(&patch_str, repo_dir, true, git2::ApplyLocation::Index)?;
        Ok(())
    }
}

fn find_wi_hunk_index(
    wi_diff: &BufferDiff,
    cursor_row: u32,
    is_deletion: bool,
    display_start: u32,
    buffer_snapshot: &text::BufferSnapshot,
) -> Option<usize> {
    wi_diff.hunks.iter().position(|h| {
        let s = h.buffer_range.start.to_point(buffer_snapshot).row;
        let e = h.buffer_range.end.to_point(buffer_snapshot).row;
        if is_deletion {
            if s == e {
                s == display_start
            } else {
                s <= display_start && e > display_start
            }
        } else {
            s <= cursor_row
                && (if s == e {
                    s == cursor_row
                } else {
                    e > cursor_row
                })
        }
    })
}

fn find_ih_hunk_index(
    patch: &git2::Patch<'_>,
    display_old_start: u32,
    display_old_end: u32,
) -> Option<usize> {
    (0..patch.num_hunks()).find(|&idx| {
        let Ok((hdr, _)) = patch.hunk(idx) else {
            return false;
        };
        let old_start = hdr.old_start();
        let old_end = old_start + hdr.old_lines();
        old_start < display_old_end.max(display_old_start + 1)
            && old_end.max(old_start + 1) > display_old_start
    })
}

fn find_line_by_content(selection: &LineSelection, target_content: &str) -> Option<usize> {
    selection.hunk_lines.lines.iter().position(|l| {
        l.origin == HunkLineOrigin::Addition && l.content.trim_end_matches('\n') == target_content
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::*;
    use gpui::TestAppContext;

    fn setup_repo(
        stoat: &mut crate::test::TestStoat<'_>,
        initial: &str,
        modified: &str,
    ) -> std::path::PathBuf {
        let file_path = stoat.repo_path().unwrap().join("test.txt");
        std::fs::write(&file_path, initial).expect("write initial");

        std::process::Command::new("git")
            .args(["add", "test.txt"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("git add");
        std::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(stoat.repo_path().unwrap())
            .output()
            .expect("git commit");

        std::fs::write(&file_path, modified).expect("write modified");

        stoat.set_file_path(file_path.clone());
        stoat.update(|s, cx| {
            let buffer_item = s.active_buffer(cx);
            buffer_item.update(cx, |item, cx| {
                let content = std::fs::read_to_string(&file_path).unwrap();
                item.buffer().update(cx, |buffer, _| {
                    let len = buffer.len();
                    buffer.edit([(0..len, content.as_str())]);
                });

                let repo = crate::git::repository::Repository::discover(&file_path).unwrap();
                let head_content = repo.head_content(&file_path).unwrap();
                let snapshot = item.buffer().read(cx).snapshot();
                let diff = crate::git::diff::BufferDiff::new(
                    item.buffer().read(cx).remote_id(),
                    head_content,
                    &snapshot,
                )
                .unwrap();
                item.set_diff(Some(diff));
            });
        });

        file_path
    }

    fn git_cached_diff(repo_path: &std::path::Path) -> String {
        String::from_utf8_lossy(
            &std::process::Command::new("git")
                .args(["diff", "--cached"])
                .current_dir(repo_path)
                .output()
                .expect("git diff --cached")
                .stdout,
        )
        .to_string()
    }

    #[gpui::test]
    fn stages_addition_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        setup_repo(
            &mut stoat,
            "line 1\nline 2\nline 3\n",
            "line 1\nline 2\nline 3\nnew line\n",
        );
        stoat.update(|s, _cx| s.set_cursor_position(text::Point::new(3, 0)));

        stoat.dispatch(GitToggleStageLine);

        let cached = git_cached_diff(stoat.repo_path().unwrap());
        assert!(
            cached.contains("+new line"),
            "Addition should be staged: {cached}"
        );
    }

    #[gpui::test]
    fn stages_deletion(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        setup_repo(&mut stoat, "line 1\nline 2\nline 3\n", "line 1\nline 3\n");
        stoat.update(|s, _cx| s.set_cursor_position(text::Point::new(0, 0)));

        stoat.dispatch(GitToggleStageLine);

        let cached = git_cached_diff(stoat.repo_path().unwrap());
        assert!(
            cached.contains("-line 2"),
            "Deletion should be staged: {cached}"
        );
    }

    #[gpui::test]
    fn toggle_addition_round_trip(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        setup_repo(
            &mut stoat,
            "line 1\nline 2\nline 3\n",
            "line 1\nline 2\nline 3\nnew line\n",
        );
        stoat.update(|s, _cx| s.set_cursor_position(text::Point::new(3, 0)));

        stoat.dispatch(GitToggleStageLine);
        let cached = git_cached_diff(stoat.repo_path().unwrap());
        assert!(
            cached.contains("+new line"),
            "Should be staged first: {cached}"
        );

        stoat.dispatch(GitToggleStageLine);
        let cached = git_cached_diff(stoat.repo_path().unwrap());
        assert!(
            cached.trim().is_empty(),
            "Should be unstaged after second toggle: {cached}"
        );
    }

    #[gpui::test]
    fn toggle_deletion_round_trip(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        setup_repo(&mut stoat, "line 1\nline 2\nline 3\n", "line 1\nline 3\n");
        stoat.update(|s, _cx| s.set_cursor_position(text::Point::new(0, 0)));

        stoat.dispatch(GitToggleStageLine);
        let cached = git_cached_diff(stoat.repo_path().unwrap());
        assert!(
            cached.contains("-line 2"),
            "Should be staged first: {cached}"
        );

        stoat.dispatch(GitToggleStageLine);
        let cached = git_cached_diff(stoat.repo_path().unwrap());
        assert!(
            cached.trim().is_empty(),
            "Should be unstaged after second toggle: {cached}"
        );
    }

    #[gpui::test]
    fn stages_single_line_from_multi_line_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_git();
        setup_repo(
            &mut stoat,
            "line 1\nline 2\nline 3\n",
            "line 1\nline 2\nline 3\nnew A\nnew B\n",
        );
        stoat.update(|s, _cx| s.set_cursor_position(text::Point::new(3, 0)));

        stoat.dispatch(GitToggleStageLine);

        let cached = git_cached_diff(stoat.repo_path().unwrap());
        assert!(
            cached.contains("+new A"),
            "First new line should be staged: {cached}"
        );
        assert!(
            !cached.contains("+new B"),
            "Second new line should NOT be staged: {cached}"
        );
    }

    #[gpui::test]
    fn stages_addition_with_preceding_deletion(cx: &mut TestAppContext) {
        let fifty_lines: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        let initial = format!("{fifty_lines}middle marker\nend\n");
        // Delete the first 50 lines, add "charlie" after "middle marker"
        let modified = "middle marker\ncharlie\nend\n";

        let mut stoat = Stoat::test(cx).init_git();
        let file_path = setup_repo(&mut stoat, &initial, modified);

        // cursor_row 1 = "charlie" (row 0 = "middle marker")
        stoat.update(|s, _cx| s.set_cursor_position(text::Point::new(1, 0)));

        stoat.dispatch(GitToggleStageLine);

        let cached = git_cached_diff(stoat.repo_path().unwrap());
        assert!(
            cached.contains("+charlie"),
            "Addition should be staged: {cached}"
        );

        let repo = crate::git::repository::Repository::discover(&file_path).unwrap();
        let staged = repo.index_content(&file_path).unwrap();
        assert!(
            staged.contains("middle marker\ncharlie\n"),
            "charlie should appear right after 'middle marker' in index, got:\n{staged}"
        );
    }
}
