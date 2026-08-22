mod hunk_removal;
mod patch;

pub(crate) use hunk_removal::remove_chunks_from_buffer;
pub(crate) use patch::{
    base_line_range, chunk_to_unified_diff, hunk_rows, hunk_to_patch, line_restricted_rows,
    rows_to_unified_diff, HUNK_CONTEXT,
};
