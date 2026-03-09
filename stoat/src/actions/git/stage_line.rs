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
        provider::GitRepo,
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
    pub fn git_toggle_stage_line(&mut self, cx: &mut Context<Self>) {
        let file_path = match self.current_file_path.as_ref() {
            Some(p) => p.clone(),
            None => {
                tracing::error!("git_toggle_stage_line: No file path set for current buffer");
                return;
            },
        };

        let cursor_row = self.cursor.position().row;
        let buffer_item = self.active_buffer(cx);
        let buffer_snapshot = buffer_item.read(cx).buffer().read(cx).snapshot().clone();

        let diff = match buffer_item.read(cx).diff() {
            Some(d) => d,
            None => {
                tracing::error!("git_toggle_stage_line: No diff information available");
                return;
            },
        };

        let hunk_index = match diff.hunk_for_row(cursor_row, &buffer_snapshot) {
            Some(idx) => idx,
            None => {
                tracing::error!("git_toggle_stage_line: No hunk at cursor row {cursor_row}");
                return;
            },
        };

        let display_hunk = &diff.hunks[hunk_index];
        let is_deletion = display_hunk.status == DiffHunkStatus::Deleted;
        let display_start = display_hunk
            .buffer_range
            .start
            .to_point(&buffer_snapshot)
            .row;
        let display_old_start = display_hunk.old_start;
        let display_old_end = display_hunk.old_start + display_hunk.old_lines;

        let buffer_text = buffer_snapshot.text();
        let buffer_id = buffer_snapshot.remote_id();
        let services = self.services.clone();

        cx.spawn(async move |this, cx| {
            let result = async {
                let repo = services
                    .git
                    .discover(&file_path)
                    .await
                    .map_err(|e| format!("Repository not found: {e}"))?;
                let index_content = repo.index_content(&file_path).await.unwrap_or_default();

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
                    unstage_line_async(
                        &file_path,
                        &*repo,
                        cursor_row,
                        is_deletion,
                        display_old_start,
                        display_old_end,
                        &buffer_text,
                    )
                    .await?;
                } else {
                    stage_line_async(
                        &file_path,
                        &*repo,
                        cursor_row,
                        is_deletion,
                        display_start,
                        &index_content,
                        &buffer_text,
                        &wi_diff,
                        &buffer_snapshot,
                    )
                    .await?;
                }

                tracing::info!(
                    "Toggled stage for line at row {} in {:?}",
                    cursor_row,
                    file_path
                );
                Ok::<(), String>(())
            }
            .await;
            let _ = this.update(cx, |stoat, cx| {
                if let Err(e) = result {
                    tracing::error!("git_toggle_stage_line: {e}");
                    return;
                }
                stoat.refresh_git_diff(cx);
            });
        })
        .detach();
    }
}

#[allow(clippy::too_many_arguments)]
async fn stage_line_async(
    file_path: &std::path::Path,
    repo: &dyn GitRepo,
    cursor_row: u32,
    is_deletion: bool,
    display_start: u32,
    index_content: &str,
    buffer_text: &str,
    wi_diff: &BufferDiff,
    buffer_snapshot: &text::BufferSnapshot,
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
    super::hunk_patch::apply_patch(
        &patch,
        repo,
        false,
        crate::git::provider::ApplyLocation::Index,
    )
    .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn unstage_line_async(
    file_path: &std::path::Path,
    repo: &dyn GitRepo,
    cursor_row: u32,
    is_deletion: bool,
    display_old_start: u32,
    display_old_end: u32,
    buffer_text: &str,
) -> Result<(), String> {
    let head_content = repo.head_content(file_path).await.unwrap_or_default();
    let index_content = repo.index_content(file_path).await.unwrap_or_default();

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

    if is_deletion && hunk_lines.new_lines == 0 {
        hunk_lines.new_start = hunk_lines.old_start;
    }

    let mut selection = LineSelection::new(hunk_lines);
    selection.deselect_all();

    if is_deletion {
        selection.select_all();
    } else {
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
    super::hunk_patch::apply_patch(
        &patch_str,
        repo,
        true,
        crate::git::provider::ApplyLocation::Index,
    )
    .await?;
    Ok(())
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
    use crate::Stoat;
    use gpui::TestAppContext;

    fn setup(stoat: &mut crate::test::TestStoat<'_>, initial: &str, modified: &str) {
        stoat
            .with_committed_file("test.txt", initial)
            .with_working_change("test.txt", modified)
            .load_and_diff("test.txt");
    }

    #[gpui::test]
    fn stages_addition_line(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        setup(
            &mut stoat,
            "line 1\nline 2\nline 3\n",
            "line 1\nline 2\nline 3\nnew line\n",
        );
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(3, 0)));

        stoat.update(|s, cx| s.git_toggle_stage_line(cx));
        stoat.run_until_parked();

        let diffs = stoat.fake_git().applied_diffs();
        assert!(
            diffs.iter().any(|(p, _, _)| p.contains("+new line")),
            "Addition should be staged"
        );
    }

    #[gpui::test]
    fn stages_deletion(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        setup(&mut stoat, "line 1\nline 2\nline 3\n", "line 1\nline 3\n");
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(0, 0)));

        stoat.update(|s, cx| s.git_toggle_stage_line(cx));
        stoat.run_until_parked();

        let diffs = stoat.fake_git().applied_diffs();
        assert!(
            diffs.iter().any(|(p, _, _)| p.contains("-line 2")),
            "Deletion should be staged"
        );
    }

    #[gpui::test]
    fn toggle_addition_round_trip(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        setup(
            &mut stoat,
            "line 1\nline 2\nline 3\n",
            "line 1\nline 2\nline 3\nnew line\n",
        );
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(3, 0)));

        stoat.update(|s, cx| s.git_toggle_stage_line(cx));
        stoat.run_until_parked();
        let diffs = stoat.fake_git().applied_diffs();
        assert!(diffs.iter().any(|(p, _, _)| p.contains("+new line")));

        stoat.update(|s, cx| s.git_toggle_stage_line(cx));
        stoat.run_until_parked();
        let diffs = stoat.fake_git().applied_diffs();
        assert!(diffs.len() >= 2, "Should have stage + unstage diffs");
    }

    #[gpui::test]
    fn toggle_deletion_round_trip(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        setup(&mut stoat, "line 1\nline 2\nline 3\n", "line 1\nline 3\n");
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(0, 0)));

        stoat.update(|s, cx| s.git_toggle_stage_line(cx));
        stoat.run_until_parked();
        let diffs = stoat.fake_git().applied_diffs();
        assert!(diffs.iter().any(|(p, _, _)| p.contains("-line 2")));

        stoat.update(|s, cx| s.git_toggle_stage_line(cx));
        stoat.run_until_parked();
        let diffs = stoat.fake_git().applied_diffs();
        assert!(diffs.len() >= 2, "Should have stage + unstage diffs");
    }

    #[gpui::test]
    fn stages_single_line_from_multi_line_hunk(cx: &mut TestAppContext) {
        let mut stoat = Stoat::test(cx).init_fake_git();
        setup(
            &mut stoat,
            "line 1\nline 2\nline 3\n",
            "line 1\nline 2\nline 3\nnew A\nnew B\n",
        );
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(3, 0)));

        stoat.update(|s, cx| s.git_toggle_stage_line(cx));
        stoat.run_until_parked();

        let diffs = stoat.fake_git().applied_diffs();
        let patch = &diffs[0].0;
        assert!(patch.contains("+new A"), "First new line staged: {patch}");
        assert!(
            !patch.contains("+new B"),
            "Second new line NOT staged: {patch}"
        );
    }

    #[gpui::test]
    fn stages_addition_with_preceding_deletion(cx: &mut TestAppContext) {
        let fifty_lines: String = (1..=50).map(|i| format!("line {i}\n")).collect();
        let initial = format!("{fifty_lines}middle marker\nend\n");
        let modified = "middle marker\ncharlie\nend\n";

        let mut stoat = Stoat::test(cx).init_fake_git();
        setup(&mut stoat, &initial, modified);
        stoat.update(|s, _| s.set_cursor_position(text::Point::new(1, 0)));

        stoat.update(|s, cx| s.git_toggle_stage_line(cx));
        stoat.run_until_parked();

        let diffs = stoat.fake_git().applied_diffs();
        assert!(
            diffs.iter().any(|(p, _, _)| p.contains("+charlie")),
            "Addition should be staged"
        );

        let abs = std::path::PathBuf::from("/fake/repo/test.txt");
        let idx = stoat.fake_git().index_content(&abs).unwrap_or_default();
        assert!(
            idx.contains("middle marker\ncharlie\n"),
            "charlie should appear in index: {idx}"
        );
    }
}
