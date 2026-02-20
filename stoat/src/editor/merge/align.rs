use crate::git::conflict::{ConflictRegion, ConflictSide};
use std::collections::HashMap;

#[derive(Clone, Debug)]
pub struct MergeHighlightRange {
    pub start_row: u32,
    pub end_row: u32,
    pub side: ConflictSide,
    pub conflict_idx: usize,
}

pub struct MergeContent {
    pub ours: String,
    pub result: String,
    pub theirs: String,
    pub ours_highlights: Vec<MergeHighlightRange>,
    pub result_highlights: Vec<MergeHighlightRange>,
    pub theirs_highlights: Vec<MergeHighlightRange>,
}

/// Build 3 content strings (ours / result / theirs) from conflicted file text.
///
/// Shared regions are duplicated into all 3 columns. Conflict regions are
/// extracted per-side, with the result column depending on the resolution
/// state. Shorter columns within a conflict are padded with blank lines so
/// all 3 have the same total line count (enabling simple scroll sync).
pub fn extract_merge_content(
    text: &str,
    conflicts: &[ConflictRegion],
    resolutions: &HashMap<(usize, usize), ConflictSide>,
    file_idx: usize,
) -> MergeContent {
    let mut ours = String::new();
    let mut result = String::new();
    let mut theirs = String::new();
    let mut ours_highlights = Vec::new();
    let mut result_highlights = Vec::new();
    let mut theirs_highlights = Vec::new();
    let mut pos: usize = 0;

    for (conflict_idx, conflict) in conflicts.iter().enumerate() {
        // Shared text before this conflict
        if pos < conflict.range.start {
            let shared = &text[pos..conflict.range.start];
            ours.push_str(shared);
            result.push_str(shared);
            theirs.push_str(shared);
        }

        let ours_text = &text[conflict.ours.clone()];
        let theirs_text = &text[conflict.theirs.clone()];

        let resolution = resolutions.get(&(file_idx, conflict_idx)).copied();
        let result_text = match resolution {
            Some(ConflictSide::Ours) => ours_text.to_string(),
            Some(ConflictSide::Theirs) => theirs_text.to_string(),
            Some(ConflictSide::Both) => {
                let mut s = ours_text.to_string();
                s.push_str(theirs_text);
                s
            },
            None => conflict
                .base
                .as_ref()
                .map(|r| text[r.clone()].to_string())
                .unwrap_or_default(),
        };

        let ours_lines = count_lines(ours_text);
        let theirs_lines = count_lines(theirs_text);
        let result_lines = count_lines(&result_text);
        let max_lines = ours_lines.max(theirs_lines).max(result_lines);

        // Row counting: current row is the number of newlines so far
        let current_row = count_lines(&ours) as u32;

        ours.push_str(ours_text);
        pad_to_lines(&mut ours, ours_lines, max_lines);

        result.push_str(&result_text);
        pad_to_lines(&mut result, result_lines, max_lines);

        theirs.push_str(theirs_text);
        pad_to_lines(&mut theirs, theirs_lines, max_lines);

        let end_row = current_row + max_lines.saturating_sub(1) as u32;

        if max_lines > 0 {
            ours_highlights.push(MergeHighlightRange {
                start_row: current_row,
                end_row,
                side: ConflictSide::Ours,
                conflict_idx,
            });
            theirs_highlights.push(MergeHighlightRange {
                start_row: current_row,
                end_row,
                side: ConflictSide::Theirs,
                conflict_idx,
            });
            let result_side = resolution.unwrap_or(ConflictSide::Ours);
            result_highlights.push(MergeHighlightRange {
                start_row: current_row,
                end_row,
                side: result_side,
                conflict_idx,
            });
        }

        pos = conflict.range.end;
    }

    // Trailing text after last conflict
    if pos < text.len() {
        let remaining = &text[pos..];
        ours.push_str(remaining);
        result.push_str(remaining);
        theirs.push_str(remaining);
    }

    MergeContent {
        ours,
        result,
        theirs,
        ours_highlights,
        result_highlights,
        theirs_highlights,
    }
}

fn count_lines(s: &str) -> usize {
    if s.is_empty() {
        return 0;
    }
    s.chars().filter(|&c| c == '\n').count()
}

fn pad_to_lines(buf: &mut String, current: usize, target: usize) {
    for _ in current..target {
        buf.push('\n');
    }
}

pub struct MergeDisplayRow {
    pub ours: Option<String>,
    pub base: Option<String>,
    pub theirs: Option<String>,
    pub is_conflict: bool,
}

/// Build aligned display rows from conflicted file text.
///
/// For shared (non-conflict) regions all columns get the same line.
/// For conflict regions each column gets its side's content; shorter
/// sides are padded with `None` to match the tallest.
/// Conflict marker lines are omitted.
pub fn compute_merge_rows(text: &str, conflicts: &[ConflictRegion]) -> Vec<MergeDisplayRow> {
    let mut rows = Vec::new();
    let mut pos: usize = 0;

    for conflict in conflicts {
        let conflict_start = conflict.range.start;
        if pos < conflict_start {
            let shared_text = &text[pos..conflict_start];
            for line in shared_text.split('\n') {
                if pos == conflict_start {
                    break;
                }
                rows.push(MergeDisplayRow {
                    ours: Some(line.to_string()),
                    base: Some(line.to_string()),
                    theirs: Some(line.to_string()),
                    is_conflict: false,
                });
            }
        }

        let ours_lines: Vec<&str> = if conflict.ours.is_empty() {
            Vec::new()
        } else {
            split_region(text, &conflict.ours)
        };

        let theirs_lines: Vec<&str> = if conflict.theirs.is_empty() {
            Vec::new()
        } else {
            split_region(text, &conflict.theirs)
        };

        let base_lines: Vec<&str> = conflict
            .base
            .as_ref()
            .map(|r| {
                if r.is_empty() {
                    Vec::new()
                } else {
                    split_region(text, r)
                }
            })
            .unwrap_or_default();

        let max_height = ours_lines
            .len()
            .max(theirs_lines.len())
            .max(base_lines.len());

        for i in 0..max_height {
            rows.push(MergeDisplayRow {
                ours: ours_lines.get(i).map(|s| s.to_string()),
                base: if conflict.base.is_some() {
                    base_lines.get(i).map(|s| s.to_string())
                } else {
                    None
                },
                theirs: theirs_lines.get(i).map(|s| s.to_string()),
                is_conflict: true,
            });
        }

        pos = conflict.range.end;
    }

    if pos < text.len() {
        let remaining = &text[pos..];
        for line in remaining.split('\n') {
            rows.push(MergeDisplayRow {
                ours: Some(line.to_string()),
                base: Some(line.to_string()),
                theirs: Some(line.to_string()),
                is_conflict: false,
            });
        }
    }

    rows
}

fn split_region<'a>(text: &'a str, range: &std::ops::Range<usize>) -> Vec<&'a str> {
    let region = &text[range.clone()];
    let trimmed = region.strip_suffix('\n').unwrap_or(region);
    trimmed.split('\n').collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::conflict::parse_conflicts;

    #[test]
    fn shared_text_only() {
        let text = "line1\nline2\nline3\n";
        let conflicts = parse_conflicts(text);
        assert!(conflicts.is_empty());

        let rows = compute_merge_rows(text, &conflicts);
        assert!(rows.iter().all(|r| !r.is_conflict));
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].ours.as_deref(), Some("line1"));
        assert_eq!(rows[0].theirs.as_deref(), Some("line1"));
    }

    #[test]
    fn single_conflict_alignment() {
        let text = "\
before
<<<<<<< HEAD
our line 1
our line 2
=======
their line 1
>>>>>>> branch
after
";
        let conflicts = parse_conflicts(text);
        assert_eq!(conflicts.len(), 1);

        let rows = compute_merge_rows(text, &conflicts);

        assert!(!rows[0].is_conflict);
        assert_eq!(rows[0].ours.as_deref(), Some("before"));

        // ours has 2 lines, theirs has 1 -- 2 conflict rows with padding
        let conflict_rows: Vec<_> = rows.iter().filter(|r| r.is_conflict).collect();
        assert_eq!(conflict_rows.len(), 2);
        assert_eq!(conflict_rows[0].ours.as_deref(), Some("our line 1"));
        assert_eq!(conflict_rows[0].theirs.as_deref(), Some("their line 1"));
        assert_eq!(conflict_rows[1].ours.as_deref(), Some("our line 2"));
        assert!(conflict_rows[1].theirs.is_none());

        let last_shared: Vec<_> = rows.iter().filter(|r| !r.is_conflict).collect();
        assert!(last_shared
            .iter()
            .any(|r| r.ours.as_deref() == Some("after")));
    }

    #[test]
    fn multi_conflict_alignment() {
        let text = "\
start
<<<<<<< HEAD
ours1
=======
theirs1
>>>>>>> branch
middle
<<<<<<< HEAD
ours2
=======
theirs2
>>>>>>> branch
end
";
        let conflicts = parse_conflicts(text);
        assert_eq!(conflicts.len(), 2);

        let rows = compute_merge_rows(text, &conflicts);

        let shared: Vec<_> = rows
            .iter()
            .filter(|r| !r.is_conflict)
            .map(|r| r.ours.as_deref().unwrap())
            .collect();
        assert!(shared.contains(&"start"));
        assert!(shared.contains(&"middle"));
        assert!(shared.contains(&"end"));

        let conflict_rows: Vec<_> = rows.iter().filter(|r| r.is_conflict).collect();
        assert_eq!(conflict_rows.len(), 2);
        assert_eq!(conflict_rows[0].ours.as_deref(), Some("ours1"));
        assert_eq!(conflict_rows[0].theirs.as_deref(), Some("theirs1"));
        assert_eq!(conflict_rows[1].ours.as_deref(), Some("ours2"));
        assert_eq!(conflict_rows[1].theirs.as_deref(), Some("theirs2"));
    }

    #[test]
    fn diff3_with_base() {
        let text = "\
<<<<<<< HEAD
our content
||||||| merged common ancestors
base content
=======
their content
>>>>>>> branch
";
        let conflicts = parse_conflicts(text);
        assert_eq!(conflicts.len(), 1);
        assert!(conflicts[0].base.is_some());

        let rows = compute_merge_rows(text, &conflicts);
        let conflict_rows: Vec<_> = rows.iter().filter(|r| r.is_conflict).collect();
        assert_eq!(conflict_rows.len(), 1);
        assert_eq!(conflict_rows[0].ours.as_deref(), Some("our content"));
        assert_eq!(conflict_rows[0].base.as_deref(), Some("base content"));
        assert_eq!(conflict_rows[0].theirs.as_deref(), Some("their content"));
    }

    #[test]
    fn empty_side_gets_padding() {
        let text = "\
<<<<<<< HEAD
=======
their line
>>>>>>> branch
";
        let conflicts = parse_conflicts(text);
        assert_eq!(conflicts.len(), 1);

        let rows = compute_merge_rows(text, &conflicts);
        let conflict_rows: Vec<_> = rows.iter().filter(|r| r.is_conflict).collect();
        assert_eq!(conflict_rows.len(), 1);
        assert!(conflict_rows[0].ours.is_none());
        assert_eq!(conflict_rows[0].theirs.as_deref(), Some("their line"));
    }

    // --- extract_merge_content tests ---

    fn no_resolutions() -> HashMap<(usize, usize), ConflictSide> {
        HashMap::new()
    }

    #[test]
    fn extract_shared_text_only() {
        let text = "line1\nline2\n";
        let conflicts = parse_conflicts(text);
        let mc = extract_merge_content(text, &conflicts, &no_resolutions(), 0);
        assert_eq!(mc.ours, text);
        assert_eq!(mc.result, text);
        assert_eq!(mc.theirs, text);
    }

    #[test]
    fn extract_unresolved_2way() {
        let text = "\
before
<<<<<<< HEAD
our line
=======
their line
>>>>>>> branch
after
";
        let conflicts = parse_conflicts(text);
        let mc = extract_merge_content(text, &conflicts, &no_resolutions(), 0);

        assert!(mc.ours.contains("our line"));
        assert!(!mc.ours.contains("<<<<<<<"));
        assert!(mc.theirs.contains("their line"));
        assert!(!mc.theirs.contains("<<<<<<<"));
        // 2-way unresolved: result column is empty for the conflict region
        assert!(!mc.result.contains("our line"));
        assert!(!mc.result.contains("their line"));
        assert!(mc.result.contains("before"));
        assert!(mc.result.contains("after"));
    }

    #[test]
    fn extract_unresolved_diff3() {
        let text = "\
<<<<<<< HEAD
our
||||||| base
base text
=======
their
>>>>>>> branch
";
        let conflicts = parse_conflicts(text);
        let mc = extract_merge_content(text, &conflicts, &no_resolutions(), 0);

        assert!(mc.result.contains("base text"));
    }

    #[test]
    fn extract_resolved_ours() {
        let text = "\
<<<<<<< HEAD
our line
=======
their line
>>>>>>> branch
";
        let conflicts = parse_conflicts(text);
        let mut resolutions = HashMap::new();
        resolutions.insert((0, 0), ConflictSide::Ours);
        let mc = extract_merge_content(text, &conflicts, &resolutions, 0);

        assert!(mc.result.contains("our line"));
        assert!(!mc.result.contains("their line"));
    }

    #[test]
    fn extract_resolved_both() {
        let text = "\
<<<<<<< HEAD
ours
=======
theirs
>>>>>>> branch
";
        let conflicts = parse_conflicts(text);
        let mut resolutions = HashMap::new();
        resolutions.insert((0, 0), ConflictSide::Both);
        let mc = extract_merge_content(text, &conflicts, &resolutions, 0);

        assert!(mc.result.contains("ours"));
        assert!(mc.result.contains("theirs"));
    }

    #[test]
    fn extract_padding_alignment() {
        let text = "\
<<<<<<< HEAD
a
b
c
=======
x
>>>>>>> branch
";
        let conflicts = parse_conflicts(text);
        let mc = extract_merge_content(text, &conflicts, &no_resolutions(), 0);

        let ours_lines: Vec<_> = mc.ours.split('\n').collect();
        let theirs_lines: Vec<_> = mc.theirs.split('\n').collect();
        let result_lines: Vec<_> = mc.result.split('\n').collect();
        assert_eq!(ours_lines.len(), theirs_lines.len());
        assert_eq!(ours_lines.len(), result_lines.len());
    }

    #[test]
    fn extract_ignores_other_file_idx() {
        let text = "\
<<<<<<< HEAD
ours
=======
theirs
>>>>>>> branch
";
        let conflicts = parse_conflicts(text);
        let mut resolutions = HashMap::new();
        resolutions.insert((1, 0), ConflictSide::Ours);
        let mc = extract_merge_content(text, &conflicts, &resolutions, 0);

        // file_idx 0 has no resolution, so result should be empty for the conflict
        assert!(!mc.result.contains("ours"));
    }
}
