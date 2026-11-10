//! How many buffer lines each row of a minimap strip stands for.
//!
//! A strip keeps its row pitch at every file size. A file longer than the strip
//! folds several lines into each row, so the zoom-out is a summary of the file
//! and never thinner rows. The Thumb Minimum caps the fold at the viewport's
//! lines over the minimum, which keeps the viewport thumb at least that many
//! rows tall, and a file past the cap scrolls on the strip.

use std::ops::Range;

/// The most lines a row stands for in the [`Tier::Fit`] tier. Past it, a row
/// draws the outline of its band in the [`Tier::Shape`] tier.
pub(crate) const FIT_MAX_LINES_PER_ROW: u32 = 12;

/// The least share of non-blank lines a band holds before its row shows a
/// representative line. A band under it draws blank.
pub(crate) const CODE_SHARE_MIN: f32 = 0.4;

/// How a strip draws a row, by the lines the row stands for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Tier {
    /// One line per row, each row the summary of its line.
    Line,
    /// Up to [`FIT_MAX_LINES_PER_ROW`] lines per row, each row the summary of
    /// its band's representative line.
    Fit,
    /// More lines per row, each row the outline of its band.
    Shape,
}

/// The lines each row of a strip stands for and the tier that draws them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct Density {
    pub lines_per_row: u32,
    pub tier: Tier,
    /// Whether the whole file fits the strip at [`Self::lines_per_row`]. A file
    /// the Thumb Minimum's cap stops from fitting scrolls on the strip, and a
    /// rail beside it shows the whole file.
    pub fits: bool,
    /// The lines per row the whole file needs on the strip, before the cap.
    /// It equals [`Self::lines_per_row`] while the file fits, and a rail draws
    /// the whole file at it while the file does not.
    pub whole_lines_per_row: u32,
}

impl Density {
    /// One line per row, the density of a strip that holds the whole file.
    pub(crate) const LINE: Density = Density {
        lines_per_row: 1,
        tier: Tier::Line,
        fits: true,
        whole_lines_per_row: 1,
    };
}

/// The density of a strip of `strip_rows` rows over a file of `file_lines`
/// lines, under a viewport of `viewport_lines` lines.
///
/// A row stands for as many lines as the whole file needs to fit the strip,
/// capped at `viewport_lines / thumb_min_rows` so the thumb keeps at least
/// `thumb_min_rows` rows. A file past the cap does not fit and scrolls.
pub(crate) fn density(
    file_lines: u32,
    strip_rows: u32,
    viewport_lines: u32,
    thumb_min_rows: u32,
) -> Density {
    let cap = (viewport_lines / thumb_min_rows.max(1)).max(1);
    let wanted = file_lines.div_ceil(strip_rows.max(1)).max(1);
    let lines_per_row = wanted.min(cap);
    let tier = match lines_per_row {
        1 => Tier::Line,
        2..=FIT_MAX_LINES_PER_ROW => Tier::Fit,
        _ => Tier::Shape,
    };
    Density {
        lines_per_row,
        tier,
        fits: wanted <= cap,
        whole_lines_per_row: wanted,
    }
}

/// The rows a file of `file_lines` lines takes at `lines_per_row`.
pub(crate) fn rows(file_lines: u32, lines_per_row: u32) -> u32 {
    file_lines.div_ceil(lines_per_row.max(1))
}

/// The buffer lines strip row `row` stands for, cut at the file's end.
pub(crate) fn band(row: u32, lines_per_row: u32, file_lines: u32) -> Range<u32> {
    let start = row.saturating_mul(lines_per_row).min(file_lines);
    start..start.saturating_add(lines_per_row).min(file_lines)
}

/// The strip row that buffer line `line` falls in.
pub(crate) fn row_of(line: u32, lines_per_row: u32) -> u32 {
    line / lines_per_row.max(1)
}

/// The viewport thumb in strip rows, its top and its height each in 1/256ths
/// of a row, for a viewport of `viewport_lines` lines whose top is
/// `top_line_256` in 1/256ths of a line.
///
/// The height floors at `thumb_min_rows` rows, the Thumb Minimum.
pub(crate) fn thumb_rows_256(
    top_line_256: u64,
    viewport_lines: u32,
    lines_per_row: u32,
    thumb_min_rows: u32,
) -> (u32, u32) {
    let lines_per_row = u64::from(lines_per_row.max(1));
    let top = top_line_256 / lines_per_row;
    let visible = u64::from(viewport_lines) * 256 / lines_per_row;
    (
        u32::try_from(top).unwrap_or(u32::MAX),
        u32::try_from(visible)
            .unwrap_or(u32::MAX)
            .max(thumb_min_rows.saturating_mul(256)),
    )
}

#[cfg(test)]
mod tests {
    use super::{density, thumb_rows_256, Density, Tier};

    /// The lines per row, the tier, whether the file fits, and the fold the
    /// whole file needs, under a 44-line viewport.
    fn fold(file_lines: u32, strip_rows: u32, thumb_min_rows: u32) -> (u32, Tier, bool, u32) {
        let Density {
            lines_per_row,
            tier,
            fits,
            whole_lines_per_row,
        } = density(file_lines, strip_rows, 44, thumb_min_rows);
        (lines_per_row, tier, fits, whole_lines_per_row)
    }

    /// The MinimapThumb board's table: 40,000 lines on 337 rows under a
    /// 44-line viewport fold to the cap that the Thumb Minimum sets. The whole
    /// file needs 119 lines a row, so it fits at none of them.
    #[test]
    fn the_thumb_minimum_caps_the_lines_per_row() {
        assert_eq!(
            [
                fold(40_000, 337, 2),
                fold(40_000, 337, 4),
                fold(40_000, 337, 8),
                fold(40_000, 337, 1),
            ],
            [
                (22, Tier::Shape, false, 119),
                (11, Tier::Fit, false, 119),
                (5, Tier::Fit, false, 119),
                (44, Tier::Shape, false, 119),
            ],
        );
    }

    /// A file under the cap folds only as far as it needs to fit the strip,
    /// as the MinimapFit board's 2,600 lines on 364 rows do.
    #[test]
    fn a_file_under_the_cap_folds_to_fit_the_strip() {
        assert_eq!(
            [fold(2_600, 364, 2), fold(8_000, 364, 2), fold(110, 337, 2)],
            [
                (8, Tier::Fit, true, 8),
                (22, Tier::Shape, true, 22),
                (1, Tier::Line, true, 1),
            ],
        );
    }

    /// A 44-line viewport at 22 lines per row is two rows tall, from the row
    /// its top line falls in.
    #[test]
    fn the_thumb_takes_the_viewport_in_rows() {
        assert_eq!(thumb_rows_256(220 * 256, 44, 22, 2), (10 * 256, 512));
    }
}
