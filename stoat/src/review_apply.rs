mod hunk_removal;
mod patch;

pub(crate) use hunk_removal::remove_chunks_from_buffer;
pub(crate) use patch::chunk_to_unified_diff;

#[cfg(test)]
mod tests {
    use crate::{
        badge::{BadgeSource, BadgeState},
        review_session::ChunkStatus,
        test_harness::{TestHarness, REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER},
    };

    #[test]
    fn review_apply_stages_pure_deletion_from_working_tree() {
        let mut h = TestHarness::with_size(80, 14);
        h.stoat.active_workspace_mut().git_root = "/work".into();
        h.fake_git
            .add_repo("/work")
            .with_fs(&h.fake_fs)
            .deleted("gone.txt", "a\nb\nc\n");
        h.stoat.open_review();
        h.settle();

        {
            let session = h
                .stoat
                .active_workspace()
                .review
                .as_ref()
                .expect("session built from deletion");
            assert_eq!(session.files.len(), 1);
            assert_eq!(session.files[0].buffer_text.as_str(), "");
            assert_eq!(session.order.len(), 1);
            let chunk = &session.chunks[&session.order[0]];
            assert_eq!(chunk.base_line_range, 0..3);
            assert!(chunk.buffer_line_range.is_empty());
        }

        h.set_review_status(0, ChunkStatus::Staged);
        h.dispatch_review_apply();

        let patches = h.fake_git.applied_patches(std::path::Path::new("/work"));
        assert_eq!(patches.len(), 1, "exactly one patch applied");
        assert!(
            patches[0].contains("+++ /dev/null"),
            "deletion patch must target /dev/null, got:\n{}",
            patches[0],
        );
    }

    #[test]
    fn review_mode_capital_a_triggers_apply() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();
        h.set_review_status(0, ChunkStatus::Staged);

        h.type_keys("A");

        let patches = h.fake_git.applied_patches(std::path::Path::new("/work"));
        assert_eq!(
            patches.len(),
            1,
            "expected one staged patch, got {patches:?}"
        );
    }

    #[test]
    fn review_mode_lowercase_r_triggers_refresh() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();
        h.set_review_status(0, ChunkStatus::Staged);

        let before_editor = h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .unwrap()
            .view_editor;

        h.type_keys("r");

        let after_editor = h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .unwrap()
            .view_editor;
        assert_ne!(
            before_editor, after_editor,
            "refresh must rebuild session + editor"
        );

        let session = h.stoat.active_workspace().review.as_ref().unwrap();
        assert_eq!(
            session.chunks[&session.order[0]].status,
            ChunkStatus::Staged,
            "refresh must carry staged status"
        );
    }

    #[test]
    fn review_apply_emits_patch_per_staged_chunk() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        h.set_review_status(0, ChunkStatus::Staged);
        h.set_review_status(1, ChunkStatus::Staged);
        h.dispatch_review_apply();

        let by_path = h
            .fake_git
            .applied_patches_by_path(std::path::Path::new("/work"));
        assert_eq!(
            by_path.len(),
            2,
            "two staged chunks must produce two patches: {by_path:#?}"
        );
        for (abs, patch) in &by_path {
            assert_eq!(abs, &std::path::PathBuf::from("/work/a.rs"));
            assert!(patch.contains("--- a/a.rs"), "unexpected patch: {patch}");
            assert!(patch.contains("+++ b/a.rs"), "unexpected patch: {patch}");
            assert!(patch.contains("@@ "), "missing hunk header: {patch}");
        }
    }

    #[test]
    fn review_apply_skips_pending_unstaged_skipped() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        h.set_review_status(0, ChunkStatus::Unstaged);
        h.set_review_status(1, ChunkStatus::Skipped);
        h.dispatch_review_apply();

        assert!(
            h.fake_git
                .applied_patches(std::path::Path::new("/work"))
                .is_empty(),
            "non-staged chunks must not produce patches"
        );
    }

    #[test]
    fn review_apply_with_nothing_staged_is_noop() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        h.dispatch_review_apply();
        assert!(h
            .fake_git
            .applied_patches(std::path::Path::new("/work"))
            .is_empty());

        let ws = h.stoat.active_workspace();
        assert!(
            ws.badges.find_by_source(BadgeSource::Review).is_none(),
            "nothing staged must not create a badge"
        );
    }

    #[test]
    fn review_apply_surfaces_failure_badge() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.fake_git
            .add_repo("/work")
            .fail_apply_with("simulated backend failure");
        h.stoat.open_review();
        h.settle();

        h.set_review_status(0, ChunkStatus::Staged);
        let chunk0_id = h.stoat.active_workspace().review.as_ref().unwrap().order[0];
        h.dispatch_review_apply();

        let ws = h.stoat.active_workspace();
        let badge_id = ws
            .badges
            .find_by_source(BadgeSource::Review)
            .expect("error badge");
        let badge = ws.badges.get(badge_id).unwrap();
        assert_eq!(badge.state, BadgeState::Error);
        assert_eq!(
            badge.detail.as_deref(),
            Some("simulated backend failure"),
            "detail must carry the backend message"
        );

        let session = ws.review.as_ref().unwrap();
        assert_eq!(
            session.chunks[&chunk0_id].status,
            ChunkStatus::Staged,
            "failed chunks must not be cleared"
        );
    }

    #[test]
    fn review_apply_auto_refreshes_on_full_success() {
        let mut h = TestHarness::with_size(80, 14);
        h.stage_review_scenario(
            "/work",
            &[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)],
        );
        h.stoat.open_review();
        h.settle();

        h.set_review_status(0, ChunkStatus::Staged);
        h.set_review_status(1, ChunkStatus::Staged);

        let before_editor = h
            .stoat
            .active_workspace()
            .review
            .as_ref()
            .unwrap()
            .view_editor;
        h.dispatch_review_apply();

        let ws = h.stoat.active_workspace();
        let session = ws.review.as_ref().expect("session still present");
        assert_ne!(
            before_editor, session.view_editor,
            "auto-refresh must install a fresh editor via review_refresh"
        );

        let statuses: Vec<_> = session
            .order
            .iter()
            .map(|id| session.chunks[id].status)
            .collect();
        assert_eq!(
            statuses,
            vec![ChunkStatus::Staged, ChunkStatus::Staged],
            "carried statuses must survive auto-refresh"
        );

        let badge_id = ws
            .badges
            .find_by_source(BadgeSource::Review)
            .expect("complete badge");
        let badge = ws.badges.get(badge_id).unwrap();
        assert_eq!(badge.state, BadgeState::Complete);
        assert!(
            badge.label.contains("applied 2"),
            "badge must report count: {}",
            badge.label
        );
        assert_eq!(
            h.fake_git
                .applied_patches(std::path::Path::new("/work"))
                .len(),
            2,
            "both staged patches must have reached apply_to_index"
        );
    }
}
