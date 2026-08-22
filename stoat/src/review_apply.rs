mod patch;

pub(crate) use patch::{
    base_line_range, hunk_rows, hunk_to_patch, line_restricted_rows, rows_to_unified_diff,
    HUNK_CONTEXT,
};
