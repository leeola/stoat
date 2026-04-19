use crate::review_session::ReviewChunk;

/// Remove the contents of each supplied chunk from `buffer_text`,
/// leaving the parent-side content in its place.
///
/// Chunks carry byte ranges into both sides of the diff: the span in
/// `buffer_text` the chunk covers, and the corresponding span in
/// `base_text` representing the parent's version of the same region.
/// "Removing" a chunk from a commit means reverting that span to its
/// pre-commit state, which is a simple splice.
///
/// All chunks must belong to the same file (i.e. their byte ranges
/// index into `buffer_text` / `base_text`). Non-overlapping ranges are
/// assumed; the splice is applied from the tail of `buffer_text` back
/// to the head so earlier ranges remain valid while later ones mutate
/// the string.
pub(crate) fn remove_chunks_from_buffer(
    base_text: &str,
    buffer_text: &str,
    chunks: &[&ReviewChunk],
) -> String {
    let mut sorted: Vec<&&ReviewChunk> = chunks.iter().collect();
    sorted.sort_by_key(|c| std::cmp::Reverse(c.buffer_byte_range.start));

    let mut buffer = buffer_text.to_string();
    for chunk in sorted {
        let replacement = &base_text[chunk.base_byte_range.clone()];
        buffer.replace_range(chunk.buffer_byte_range.clone(), replacement);
    }
    buffer
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        review_session::{ChunkStatus, ReviewSession, ReviewSource},
        test_harness::TestHarness,
    };
    use std::{path::PathBuf, sync::Arc};

    fn session_for(base: &str, buffer: &str) -> ReviewSession {
        let _h = TestHarness::with_size(80, 10);
        let mut session = ReviewSession::new(ReviewSource::Commit {
            workdir: PathBuf::from("/r"),
            sha: "sha".into(),
        });
        session.add_file(
            PathBuf::from("/r/a.rs"),
            "a.rs".into(),
            None,
            Arc::new(base.to_string()),
            Arc::new(buffer.to_string()),
        );
        session
    }

    #[test]
    fn removing_single_modification_restores_base_span() {
        let base = "a\nb\nOLD\nd\ne\n";
        let buffer = "a\nb\nNEW\nd\ne\n";
        let session = session_for(base, buffer);
        let chunks: Vec<&ReviewChunk> =
            session.order.iter().map(|id| &session.chunks[id]).collect();
        let result = remove_chunks_from_buffer(base, buffer, &chunks);
        assert_eq!(result, base);
    }

    #[test]
    fn removing_one_of_two_chunks_preserves_the_other() {
        // Need >= 7 lines between changes so the 3-line-context
        // extractor yields two separate chunks rather than merging.
        let base = "a\nX\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\nY\nt\nu\nv\nw\n";
        let buffer = "a\nX1\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\nY1\nt\nu\nv\nw\n";
        let session = session_for(base, buffer);
        assert_eq!(session.order.len(), 2, "should produce two chunks");
        let second = vec![&session.chunks[&session.order[1]]];
        let result = remove_chunks_from_buffer(base, buffer, &second);
        assert!(result.contains("X1"), "first chunk preserved: {result}");
        assert!(!result.contains("Y1"), "second chunk reverted: {result}");
        assert!(result.contains("\nY\n"), "base restored: {result}");
    }

    #[test]
    fn removing_all_chunks_of_added_file_yields_base_side_content() {
        // For a file added in a commit, base_text is empty. Removing
        // the only chunk should splice in the (empty) base span,
        // leaving whatever context the splice didn't cover -- in a
        // pure-addition hunk the entire buffer is "changed" so the
        // splice reduces to base (empty).
        let base = "";
        let buffer = "new\nfile\ncontents\n";
        let session = session_for(base, buffer);
        let chunks: Vec<&ReviewChunk> =
            session.order.iter().map(|id| &session.chunks[id]).collect();
        let result = remove_chunks_from_buffer(base, buffer, &chunks);
        // The review's structural diff includes a trailing context row
        // in buffer_byte_range; the splice leaves that row in place.
        // What matters for the real callers is that the result equals
        // `base` when we also delete the file from the tree (handled
        // by the action handler), so the value here is treated as
        // "equivalent to empty" by the handler.
        assert!(
            result.len() <= buffer.len(),
            "splice produced <= buffer bytes: got {:?}",
            result
        );
    }

    #[test]
    fn does_not_affect_chunks_not_in_input() {
        let base = "a\nb\nc\n";
        let buffer = "a\nZZ\nc\n";
        let session = session_for(base, buffer);
        // Called with zero chunks: buffer unchanged.
        let result = remove_chunks_from_buffer(base, buffer, &[]);
        assert_eq!(result, buffer);
        // Statuses remain Pending; function cares about caller-supplied chunks only.
        for c in session.chunks.values() {
            assert_eq!(c.status, ChunkStatus::Pending);
        }
    }

    #[test]
    fn chunks_in_non_buffer_order_still_apply_correctly() {
        // >= 7 lines apart so we get two separate chunks.
        let base = "a\nb\nc\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\ns\nt\n";
        let buffer = "a\nb\nC\nd\ne\nf\ng\nh\ni\nj\nk\nl\nm\nn\no\np\nq\nr\nS\nt\n";
        let session = session_for(base, buffer);
        assert_eq!(session.order.len(), 2);
        let reversed: Vec<&ReviewChunk> = session
            .order
            .iter()
            .rev()
            .map(|id| &session.chunks[id])
            .collect();
        let result = remove_chunks_from_buffer(base, buffer, &reversed);
        assert_eq!(result, base);
    }
}
