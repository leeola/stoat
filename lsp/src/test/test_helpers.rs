//! Test helper utilities for LSP testing.
//!
//! Provides range notation parsing and diagnostic comparison utilities.

use anyhow::{Context, Result};
use lsp_types::{Position, Range};

/// Parse range notation "line:col-line:col" into LSP Range.
///
/// # Format
///
/// - `"2:12-2:25"` - Range from line 2, col 12 to line 2, col 25
/// - Line and column numbers are 0-indexed
/// - Column offsets are in UTF-8 bytes
///
/// # Validation
///
/// The parser validates that:
/// - Start position is before or equal to end position
/// - Positions are within source bounds (if source provided)
/// - Column offsets respect UTF-8 character boundaries
///
/// # Examples
///
/// ```ignore
/// let source = "let foo = bar;";
/// let range = parse_range_notation("0:10-0:13", source)?;
/// assert_eq!(range.start.line, 0);
/// assert_eq!(range.start.character, 10);
/// ```
pub fn parse_range_notation(notation: &str, source: &str) -> Result<Range> {
    let parts: Vec<&str> = notation.split('-').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid range notation '{notation}': expected 'line:col-line:col'");
    }

    let start = parse_position(parts[0])?;
    let end = parse_position(parts[1])?;

    if start.line > end.line || (start.line == end.line && start.character > end.character) {
        anyhow::bail!("Invalid range: start {start:?} is after end {end:?}");
    }

    // Validate positions are within source bounds
    validate_position_in_source(&start, source)
        .with_context(|| format!("Start position {start:?} out of bounds"))?;
    validate_position_in_source(&end, source)
        .with_context(|| format!("End position {end:?} out of bounds"))?;

    Ok(Range { start, end })
}

/// Parse a position "line:col" into LSP Position.
fn parse_position(s: &str) -> Result<Position> {
    let parts: Vec<&str> = s.split(':').collect();
    if parts.len() != 2 {
        anyhow::bail!("Invalid position '{s}': expected 'line:col'");
    }

    let line = parts[0]
        .parse::<u32>()
        .with_context(|| format!("Invalid line number '{}'", parts[0]))?;

    let character = parts[1]
        .parse::<u32>()
        .with_context(|| format!("Invalid column number '{}'", parts[1]))?;

    Ok(Position { line, character })
}

/// Validate that a position is within source bounds.
fn validate_position_in_source(pos: &Position, source: &str) -> Result<()> {
    let lines: Vec<&str> = source.lines().collect();

    if pos.line as usize >= lines.len() {
        anyhow::bail!(
            "Line {} out of bounds (source has {} lines)",
            pos.line,
            lines.len()
        );
    }

    let line = lines[pos.line as usize];
    if pos.character as usize > line.len() {
        anyhow::bail!(
            "Column {} out of bounds on line {} (line has {} bytes)",
            pos.character,
            pos.line,
            line.len()
        );
    }

    // Validate UTF-8 character boundary
    if !line.is_char_boundary(pos.character as usize) {
        anyhow::bail!(
            "Column {} is not on a UTF-8 character boundary on line {}",
            pos.character,
            pos.line
        );
    }

    Ok(())
}

// FIXME: Re-enable once text dependency compilation is fixed
// These functions will convert between LSP Position (UTF-16) and buffer Point (UTF-8)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_simple_range() {
        let source = "let foo = bar;";
        let range = parse_range_notation("0:10-0:13", source).expect("Failed to parse");
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 10);
        assert_eq!(range.end.line, 0);
        assert_eq!(range.end.character, 13);
    }

    #[test]
    fn parse_multiline_range() {
        let source = "fn main() {\n    let x = 1;\n}";
        let range = parse_range_notation("0:0-2:1", source).expect("Failed to parse");
        assert_eq!(range.start.line, 0);
        assert_eq!(range.start.character, 0);
        assert_eq!(range.end.line, 2);
        assert_eq!(range.end.character, 1);
    }

    #[test]
    fn reject_invalid_range_format() {
        let source = "test";
        assert!(parse_range_notation("0:0", source).is_err());
        assert!(parse_range_notation("0:0-1:0-2:0", source).is_err());
    }

    #[test]
    fn reject_start_after_end() {
        let source = "let foo = bar;";
        assert!(parse_range_notation("0:10-0:5", source).is_err());
    }

    #[test]
    fn reject_out_of_bounds() {
        let source = "test";
        assert!(parse_range_notation("1:0-1:4", source).is_err());
        assert!(parse_range_notation("0:10-0:20", source).is_err());
    }
}
