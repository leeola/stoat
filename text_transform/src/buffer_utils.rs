///! Utility functions for working with BufferSnapshot from the text crate.
use text::{BufferSnapshot, Point};

/// Read the text of a single line from the buffer.
///
/// Returns the line content without the trailing newline.
///
/// # Arguments
///
/// * `buffer` - The buffer snapshot to read from
/// * `row` - The zero-indexed row number
///
/// # Returns
///
/// The line text as a String. Returns empty string for rows beyond buffer end.
pub fn get_line_text(buffer: &BufferSnapshot, row: u32) -> String {
    let start = Point::new(row, 0);
    let line_len = buffer.line_len(row);
    let end = Point::new(row, line_len);

    buffer.text_for_range(start..end).collect()
}
