mod hunk_removal;
mod patch;

pub(crate) use hunk_removal::remove_chunks_from_buffer;
pub(crate) use patch::chunk_to_unified_diff;

#[cfg(test)]
mod tests {
    use crate::{
        review_session::ChunkStatus,
        test_harness::{TestHarness, REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER},
    };

    #[test]
    fn review_mode_lowercase_r_triggers_refresh() {
        let mut h = TestHarness::with_size(80, 14);
        h.open_review_from_texts(&[("a.rs", REVIEW_TWO_HUNK_BASE, REVIEW_TWO_HUNK_BUFFER)]);
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
}
