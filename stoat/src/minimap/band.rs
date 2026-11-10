//! What a band of buffer lines needs to become one strip row.
//!
//! A strip whose rows each stand for several lines shows one line of each band,
//! its representative, framed under the marks of every line in the band. Which
//! line represents a band comes from the statistics of its lines, which the
//! store keeps for the whole file, so the choice needs no line's text. Past
//! [`FIT_MAX_LINES_PER_ROW`](super::density::FIT_MAX_LINES_PER_ROW) lines per
//! row a row draws its band's outline from the same statistics.

use super::{
    density::CODE_SHARE_MIN, ClassTable, EdgeClass, LineRuns, RowMark, Run, CONTENT_COL, LANE_COLS,
    MAX_COLUMNS, TAB_WIDTH, TICK_COLS,
};

/// Where the text of one buffer line starts and ends and what the line holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct LineStat {
    /// The display column of the line's first non-blank char, with tabs at
    /// [`TAB_WIDTH`] stops, capped at [`MAX_COLUMNS`]. 0 for a blank line.
    pub indent: u8,
    /// The display column after the line's last non-blank char, with tabs at
    /// [`TAB_WIDTH`] stops, capped at [`MAX_COLUMNS`]. 0 for a blank line.
    pub end: u8,
    pub kind: LineKind,
}

/// What a buffer line holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum LineKind {
    /// Whitespace alone.
    #[default]
    Blank,
    /// A line that starts with a line-comment prefix of its language, or with
    /// `/*` or `*` as a block comment's lines do.
    Comment,
    Code,
}

/// The statistics of `line`, whose language starts a line comment with one of
/// `comment_prefixes`.
pub(crate) fn line_stat(line: &str, comment_prefixes: &[&str]) -> LineStat {
    let mut col: u32 = 0;
    let mut indent: Option<u32> = None;
    let mut end: u32 = 0;
    for ch in line.chars() {
        if col >= MAX_COLUMNS {
            break;
        }
        let at = col;
        col += match ch {
            '\t' => TAB_WIDTH - col % TAB_WIDTH,
            _ => 1,
        };
        if !ch.is_whitespace() {
            indent.get_or_insert(at);
            end = col;
        }
    }

    let text = line.trim();
    let kind = if text.is_empty() {
        LineKind::Blank
    } else if text.starts_with("/*")
        || text.starts_with('*')
        || comment_prefixes
            .iter()
            .any(|prefix| !prefix.is_empty() && text.starts_with(prefix))
    {
        LineKind::Comment
    } else {
        LineKind::Code
    };
    LineStat {
        indent: indent.unwrap_or(0).min(MAX_COLUMNS) as u8,
        end: end.min(MAX_COLUMNS) as u8,
        kind,
    }
}

/// The index of the line that represents a band, the non-blank line whose end
/// is the band's median, the upper median on an even count.
///
/// `None` when the non-blank lines are under [`CODE_SHARE_MIN`] of the band,
/// so a mostly blank band draws blank.
pub(crate) fn representative(stats: &[LineStat]) -> Option<usize> {
    let mut code: Vec<usize> = (0..stats.len())
        .filter(|&i| stats[i].kind != LineKind::Blank)
        .collect();
    if code.is_empty() || (code.len() as f32) < CODE_SHARE_MIN * stats.len() as f32 {
        return None;
    }
    code.sort_by_key(|&i| stats[i].end);
    Some(code[code.len() / 2])
}

/// The mark of a band, from the marks of its lines.
///
/// The lane is the first of added, modified, and removed that any line takes,
/// and the tick the first of error, warning, and info. The ranks come from
/// `table`'s kinds, since the edge classes do not number in that order.
pub(crate) fn band_mark(marks: impl Iterator<Item = RowMark>, table: &ClassTable) -> RowMark {
    let lanes = [EdgeClass::Added, EdgeClass::Modified, EdgeClass::Removed]
        .map(|kind| table.edge_class(kind));
    let ticks =
        [EdgeClass::Error, EdgeClass::Warning, EdgeClass::Info].map(|kind| table.edge_class(kind));
    let rank = |order: &[u8; 3], class: Option<u8>| {
        class.and_then(|class| order.iter().position(|&kind| kind == class))
    };

    let (mut lane, mut tick) = (None, None);
    for mark in marks {
        lane = [lane, rank(&lanes, mark.lane)].into_iter().flatten().min();
        tick = [tick, rank(&ticks, mark.tick)].into_iter().flatten().min();
    }
    RowMark {
        lane: lane.map(|at| lanes[at]),
        tick: tick.map(|at| ticks[at]),
    }
}

/// The outline of a band from the statistics of its lines.
///
/// The outline is the median indent and the nearest-rank 75th percentile of
/// the ends of the band's non-blank lines, so a few long lines do not stretch
/// it. The flag holds when comments are at least half of those lines. `None`
/// for an all-blank band.
pub(crate) fn shape(stats: &[LineStat]) -> Option<(u8, u8, bool)> {
    let lines: Vec<&LineStat> = stats
        .iter()
        .filter(|stat| stat.kind != LineKind::Blank)
        .collect();
    if lines.is_empty() {
        return None;
    }

    let mut indents: Vec<u8> = lines.iter().map(|stat| stat.indent).collect();
    let mut ends: Vec<u8> = lines.iter().map(|stat| stat.end).collect();
    indents.sort_unstable();
    ends.sort_unstable();
    let comments = lines
        .iter()
        .filter(|stat| stat.kind == LineKind::Comment)
        .count();

    let count = lines.len();
    Some((
        indents[count / 2],
        ends[(3 * count).div_ceil(4) - 1],
        2 * comments >= count,
    ))
}

/// A row of the Shape tier, which draws its band's outline where the band is
/// too long to show a line of.
///
/// A row with a lane takes the lane and a run in the lane's color across the
/// whole content, so a change shows at any fold. A row without one takes one
/// run from its outline's indent to its end, in the comment shape's color for
/// a band that is mostly comments, and an all-blank band draws blank. A tick
/// goes last at the right edge, as in every other tier.
pub(crate) fn shape_row(
    shape: Option<(u8, u8, bool)>,
    mark: RowMark,
    table: &ClassTable,
) -> LineRuns {
    let mut row = LineRuns::default();
    if let Some(lane) = mark.lane {
        row.push(Run {
            start_col: 0,
            len: LANE_COLS as u8,
            class: lane,
            weight: 255,
        });
        row.push(Run {
            start_col: CONTENT_COL as u8,
            len: (MAX_COLUMNS - CONTENT_COL) as u8,
            class: lane,
            weight: 255,
        });
    } else if let Some((indent, end, comments)) = shape {
        let start = CONTENT_COL + u32::from(indent);
        let end = (CONTENT_COL + u32::from(end)).min(MAX_COLUMNS);
        if start < end {
            let kind = match comments {
                true => EdgeClass::ShapeComment,
                false => EdgeClass::Shape,
            };
            row.push(Run {
                start_col: start as u8,
                len: (end - start) as u8,
                class: table.edge_class(kind),
                weight: 255,
            });
        }
    }
    if let Some(tick) = mark.tick {
        row.push(Run {
            start_col: (MAX_COLUMNS - TICK_COLS) as u8,
            len: TICK_COLS as u8,
            class: tick,
            weight: 255,
        });
    }
    row
}

/// `row` led by an Item Rule, a run in the rule class across the content that
/// the terminal draws as a hairline in the gap above the row.
pub(crate) fn with_rule(row: LineRuns, table: &ClassTable) -> LineRuns {
    let mut ruled = LineRuns::default();
    ruled.push(Run {
        start_col: CONTENT_COL as u8,
        len: (MAX_COLUMNS - CONTENT_COL) as u8,
        class: table.edge_class(EdgeClass::Rule),
        weight: 255,
    });
    for &run in row.as_slice() {
        ruled.push(run);
    }
    ruled
}

#[cfg(test)]
mod tests {
    use super::{
        band_mark, line_stat, representative, shape, shape_row, with_rule, LineKind, LineStat,
    };
    use crate::{
        minimap::{ClassTable, EdgeClass, LineRuns, RowMark, Run},
        theme::Theme,
    };

    fn stat(indent: u8, end: u8, kind: LineKind) -> LineStat {
        LineStat { indent, end, kind }
    }

    /// The stats of a band whose lines end at `ends`, a line ending at 0 blank.
    fn band(ends: &[u8]) -> Vec<LineStat> {
        ends.iter()
            .map(|&end| match end {
                0 => stat(0, 0, LineKind::Blank),
                _ => stat(0, end, LineKind::Code),
            })
            .collect()
    }

    /// The upper median of the non-blank lines represents the band, and a band
    /// whose code is under the share has no representative.
    #[test]
    fn the_median_line_represents_a_band_with_enough_code() {
        assert_eq!(
            (
                representative(&band(&[0, 4, 9, 12, 0, 30, 7, 15])),
                representative(&band(&[0, 0, 5, 0, 0, 9, 0, 0, 7, 0])),
            ),
            (Some(3), None),
        );
    }

    /// A tab-indented line comment reads as a comment that starts and ends
    /// past the tab stop, and a blank line starts and ends at 0.
    #[test]
    fn a_line_stat_expands_tabs_and_reads_comments() {
        assert_eq!(
            [
                line_stat("\t// note", &["//"]),
                line_stat("let x = 1;", &["//"]),
                line_stat(" * more", &[]),
                line_stat("  \t ", &["//"]),
            ],
            [
                stat(4, 11, LineKind::Comment),
                stat(0, 10, LineKind::Code),
                stat(1, 7, LineKind::Comment),
                stat(0, 0, LineKind::Blank),
            ],
        );
    }

    /// A band over an added row and a warning row takes the added lane and the
    /// warning tick, and an added lane outranks a modified one.
    #[test]
    fn a_band_takes_the_first_lane_and_tick_of_its_lines() {
        let table = ClassTable::from_theme(&Theme::empty());
        let class = |kind| Some(table.edge_class(kind));
        let mark = |lane, tick| RowMark { lane, tick };

        assert_eq!(
            band_mark(
                [
                    mark(class(EdgeClass::Modified), None),
                    mark(class(EdgeClass::Added), None),
                    mark(None, class(EdgeClass::Warning)),
                    RowMark::NONE,
                ]
                .into_iter(),
                &table,
            ),
            mark(class(EdgeClass::Added), class(EdgeClass::Warning)),
        );
    }

    /// A band's outline is the median indent and the nearest-rank 75th
    /// percentile end of its non-blank lines, flagged when comments are at
    /// least half of them. An all-blank band has none.
    #[test]
    fn a_band_outline_reads_its_median_indent_and_upper_end() {
        let code = [
            stat(4, 20, LineKind::Code),
            stat(4, 40, LineKind::Code),
            stat(8, 60, LineKind::Code),
            stat(0, 80, LineKind::Code),
        ];
        let notes = [
            stat(2, 9, LineKind::Comment),
            stat(0, 0, LineKind::Blank),
            stat(2, 30, LineKind::Code),
        ];

        assert_eq!(
            [shape(&code), shape(&notes), shape(&band(&[0, 0]))],
            [Some((4, 60, false)), Some((2, 30, true)), None],
        );
    }

    /// A Shape row draws its outline from the content column in the shape
    /// color, or the comment shape's for a mostly comment band. A lane takes
    /// the whole row in its color, a blank band draws blank, and a rule leads
    /// the row it marks.
    #[test]
    fn a_shape_row_draws_its_outline_or_its_lane() {
        let table = ClassTable::from_theme(&Theme::empty());
        let added = table.edge_class(EdgeClass::Added);
        let run = |start_col, len, class| Run {
            start_col,
            len,
            class,
            weight: 255,
        };
        let lane = RowMark {
            lane: Some(added),
            tick: None,
        };
        let outline = shape_row(Some((4, 60, false)), RowMark::NONE, &table);

        assert_eq!(
            [
                outline.as_slice().to_vec(),
                shape_row(Some((4, 60, true)), RowMark::NONE, &table)
                    .as_slice()
                    .to_vec(),
                shape_row(Some((4, 60, false)), lane, &table)
                    .as_slice()
                    .to_vec(),
                shape_row(None, RowMark::NONE, &table).as_slice().to_vec(),
                with_rule(outline, &table).as_slice().to_vec(),
            ],
            [
                vec![run(12, 56, table.edge_class(EdgeClass::Shape))],
                vec![run(12, 56, table.edge_class(EdgeClass::ShapeComment))],
                vec![run(0, 6, added), run(8, 112, added)],
                LineRuns::default().as_slice().to_vec(),
                vec![
                    run(8, 112, table.edge_class(EdgeClass::Rule)),
                    run(12, 56, table.edge_class(EdgeClass::Shape)),
                ],
            ],
        );
    }
}
