use std::{collections::HashMap, ops::Range, path::PathBuf};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ConflictViewKind {
    Inline,
    #[default]
    Merge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConflictSide {
    Ours,
    Theirs,
    Both,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConflictRegion {
    pub range: Range<usize>,
    pub ours: Range<usize>,
    pub theirs: Range<usize>,
    pub base: Option<Range<usize>>,
    pub start_row: u32,
    pub end_row: u32,
    /// Row of the `=======` separator line
    pub separator_row: u32,
}

/// State for navigating conflicted files during conflict review mode.
#[derive(Clone, Debug, Default)]
pub struct ConflictReviewState {
    pub files: Vec<PathBuf>,
    pub file_idx: usize,
    pub conflict_idx: usize,
    /// Non-destructive resolutions keyed by `(file_idx, conflict_idx)`.
    pub resolutions: HashMap<(usize, usize), ConflictSide>,
}

/// Parse all conflict regions from buffer text.
///
/// Recognizes standard 2-way (`<<<<<<<`/`=======`/`>>>>>>>`) and diff3 3-way
/// (`<<<<<<<`/`|||||||`/`=======`/`>>>>>>>`) conflict markers. Adapted from
/// Zed's `ConflictSet::parse` but uses byte offsets instead of anchors.
pub fn parse_conflicts(text: &str) -> Vec<ConflictRegion> {
    let mut conflicts = Vec::new();
    let mut row: u32 = 0;

    let mut conflict_start: Option<usize> = None;
    let mut conflict_start_row: Option<u32> = None;
    let mut ours_start: Option<usize> = None;
    let mut ours_end: Option<usize> = None;
    let mut base_start: Option<usize> = None;
    let mut base_end: Option<usize> = None;
    let mut theirs_start: Option<usize> = None;
    let mut separator_row: Option<u32> = None;

    let mut line_pos: usize = 0;
    for line in text.split('\n') {
        let line_end = line_pos + line.len();

        if line.starts_with("<<<<<<< ") {
            conflict_start = Some(line_pos);
            conflict_start_row = Some(row);
            ours_start = Some(line_end + 1);
            ours_end = None;
            base_start = None;
            base_end = None;
            theirs_start = None;
        } else if line.starts_with("||||||| ") && conflict_start.is_some() && ours_start.is_some() {
            ours_end = Some(line_pos);
            base_start = Some(line_end + 1);
        } else if line.starts_with("=======") && conflict_start.is_some() && ours_start.is_some() {
            if ours_end.is_none() {
                ours_end = Some(line_pos);
            } else if base_start.is_some() {
                base_end = Some(line_pos);
            }
            theirs_start = Some(line_end + 1);
            separator_row = Some(row);
        } else if line.starts_with(">>>>>>> ")
            && conflict_start.is_some()
            && ours_start.is_some()
            && ours_end.is_some()
            && theirs_start.is_some()
        {
            let theirs_end = line_pos;
            let conflict_end = (line_end + 1).min(text.len());

            let base = base_start.zip(base_end).map(|(start, end)| start..end);

            conflicts.push(ConflictRegion {
                range: conflict_start.unwrap()..conflict_end,
                ours: ours_start.unwrap()..ours_end.unwrap(),
                theirs: theirs_start.unwrap()..theirs_end,
                base,
                start_row: conflict_start_row.unwrap(),
                end_row: row,
                separator_row: separator_row.unwrap(),
            });

            conflict_start = None;
            conflict_start_row = None;
            ours_start = None;
            ours_end = None;
            base_start = None;
            base_end = None;
            separator_row = None;
            theirs_start = None;
        }

        line_pos = line_end + 1;
        row += 1;
    }

    conflicts
}

/// Resolve a single conflict region by choosing a side.
///
/// Returns the byte range to replace and the replacement text.
pub fn resolve_conflict(
    text: &str,
    conflict: &ConflictRegion,
    side: ConflictSide,
) -> (Range<usize>, String) {
    let replacement = match side {
        ConflictSide::Ours => text[conflict.ours.clone()].to_string(),
        ConflictSide::Theirs => text[conflict.theirs.clone()].to_string(),
        ConflictSide::Both => {
            let mut s = text[conflict.ours.clone()].to_string();
            s.push_str(&text[conflict.theirs.clone()]);
            s
        },
    };
    (conflict.range.clone(), replacement)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SIMPLE_CONFLICT: &str = "\
before
<<<<<<< HEAD
our content
=======
their content
>>>>>>> branch
after
";

    const DIFF3_CONFLICT: &str = "\
before
<<<<<<< HEAD
our content
||||||| merged common ancestors
base content
=======
their content
>>>>>>> branch
after
";

    const MULTI_CONFLICT: &str = "\
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

    #[test]
    fn simple_two_way() {
        let regions = parse_conflicts(SIMPLE_CONFLICT);
        assert_eq!(regions.len(), 1);

        let r = &regions[0];
        assert_eq!(&SIMPLE_CONFLICT[r.ours.clone()], "our content\n");
        assert_eq!(&SIMPLE_CONFLICT[r.theirs.clone()], "their content\n");
        assert!(r.base.is_none());
        assert_eq!(r.start_row, 1);
        assert_eq!(r.end_row, 5);
    }

    #[test]
    fn diff3_three_way() {
        let regions = parse_conflicts(DIFF3_CONFLICT);
        assert_eq!(regions.len(), 1);

        let r = &regions[0];
        assert_eq!(&DIFF3_CONFLICT[r.ours.clone()], "our content\n");
        assert_eq!(&DIFF3_CONFLICT[r.theirs.clone()], "their content\n");
        let base = r.base.as_ref().unwrap();
        assert_eq!(&DIFF3_CONFLICT[base.clone()], "base content\n");
        assert_eq!(r.start_row, 1);
        assert_eq!(r.end_row, 7);
    }

    #[test]
    fn multiple_conflicts() {
        let regions = parse_conflicts(MULTI_CONFLICT);
        assert_eq!(regions.len(), 2);

        assert_eq!(&MULTI_CONFLICT[regions[0].ours.clone()], "ours1\n");
        assert_eq!(&MULTI_CONFLICT[regions[0].theirs.clone()], "theirs1\n");
        assert_eq!(&MULTI_CONFLICT[regions[1].ours.clone()], "ours2\n");
        assert_eq!(&MULTI_CONFLICT[regions[1].theirs.clone()], "theirs2\n");
    }

    #[test]
    fn no_conflicts() {
        let regions = parse_conflicts("just normal text\nwith lines\n");
        assert!(regions.is_empty());
    }

    #[test]
    fn empty_regions() {
        let text = "\
<<<<<<< HEAD
=======
>>>>>>> branch
";
        let regions = parse_conflicts(text);
        assert_eq!(regions.len(), 1);
        assert_eq!(&text[regions[0].ours.clone()], "");
        assert_eq!(&text[regions[0].theirs.clone()], "");
    }

    #[test]
    fn resolve_ours() {
        let regions = parse_conflicts(SIMPLE_CONFLICT);
        let (range, replacement) =
            resolve_conflict(SIMPLE_CONFLICT, &regions[0], ConflictSide::Ours);
        let mut result = String::from(SIMPLE_CONFLICT);
        result.replace_range(range, &replacement);
        assert_eq!(result, "before\nour content\nafter\n");
    }

    #[test]
    fn resolve_theirs() {
        let regions = parse_conflicts(SIMPLE_CONFLICT);
        let (range, replacement) =
            resolve_conflict(SIMPLE_CONFLICT, &regions[0], ConflictSide::Theirs);
        let mut result = String::from(SIMPLE_CONFLICT);
        result.replace_range(range, &replacement);
        assert_eq!(result, "before\ntheir content\nafter\n");
    }

    #[test]
    fn resolve_both() {
        let regions = parse_conflicts(SIMPLE_CONFLICT);
        let (range, replacement) =
            resolve_conflict(SIMPLE_CONFLICT, &regions[0], ConflictSide::Both);
        let mut result = String::from(SIMPLE_CONFLICT);
        result.replace_range(range, &replacement);
        assert_eq!(result, "before\nour content\ntheir content\nafter\n");
    }

    #[test]
    fn utf8_content() {
        let text = "\
<<<<<<< HEAD
héllo wörld
=======
日本語テスト
>>>>>>> branch
";
        let regions = parse_conflicts(text);
        assert_eq!(regions.len(), 1);
        assert_eq!(&text[regions[0].ours.clone()], "héllo wörld\n");
        assert_eq!(&text[regions[0].theirs.clone()], "日本語テスト\n");

        let (range, replacement) = resolve_conflict(text, &regions[0], ConflictSide::Theirs);
        let mut result = String::from(text);
        result.replace_range(range, &replacement);
        assert_eq!(result, "日本語テスト\n");
    }

    #[test]
    fn incomplete_markers_ignored() {
        let text = "\
<<<<<<< HEAD
some content
no closing marker
";
        let regions = parse_conflicts(text);
        assert!(regions.is_empty());
    }

    #[test]
    fn range_covers_full_block() {
        let regions = parse_conflicts(SIMPLE_CONFLICT);
        let r = &regions[0];
        let block = &SIMPLE_CONFLICT[r.range.clone()];
        assert!(block.starts_with("<<<<<<< HEAD"));
        assert!(block.ends_with(">>>>>>> branch\n"));
    }

    #[test]
    fn conflict_at_end_of_file_no_trailing_newline() {
        let text = "\
<<<<<<< HEAD
ours
=======
theirs
>>>>>>> branch";
        let regions = parse_conflicts(text);
        assert_eq!(regions.len(), 1);
        assert_eq!(&text[regions[0].ours.clone()], "ours\n");
        assert_eq!(&text[regions[0].theirs.clone()], "theirs\n");
    }
}
