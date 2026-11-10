//! What a band of buffer lines needs to become one strip row.
//!
//! A strip whose rows each stand for several lines shows one line of each band,
//! its representative, framed under the marks of every line in the band. Which
//! line represents a band comes from the statistics of its lines, which the
//! store keeps for the whole file, so the choice needs no line's text.

use super::{density::CODE_SHARE_MIN, ClassTable, EdgeClass, RowMark, MAX_COLUMNS, TAB_WIDTH};

/// Where the text of one buffer line ends and what the line holds.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) struct LineStat {
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
    let mut end: u32 = 0;
    for ch in line.chars() {
        if col >= MAX_COLUMNS {
            break;
        }
        col += match ch {
            '\t' => TAB_WIDTH - col % TAB_WIDTH,
            _ => 1,
        };
        if !ch.is_whitespace() {
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

#[cfg(test)]
mod tests {
    use super::{band_mark, line_stat, representative, LineKind, LineStat};
    use crate::{
        minimap::{ClassTable, EdgeClass, RowMark},
        theme::Theme,
    };

    /// The stats of a band whose lines end at `ends`, a line ending at 0 blank.
    fn band(ends: &[u8]) -> Vec<LineStat> {
        ends.iter()
            .map(|&end| LineStat {
                end,
                kind: match end {
                    0 => LineKind::Blank,
                    _ => LineKind::Code,
                },
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

    /// A tab-indented line comment reads as a comment that ends past the tab
    /// stop, and a blank line ends at 0.
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
                LineStat {
                    end: 11,
                    kind: LineKind::Comment,
                },
                LineStat {
                    end: 10,
                    kind: LineKind::Code,
                },
                LineStat {
                    end: 7,
                    kind: LineKind::Comment,
                },
                LineStat {
                    end: 0,
                    kind: LineKind::Blank,
                },
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
}
