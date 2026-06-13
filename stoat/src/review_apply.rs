mod hunk_removal;
mod patch;

pub use hunk_removal::remove_chunks_from_buffer;
pub use patch::chunk_to_unified_diff;
