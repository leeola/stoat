//! The rows a strip holds when each of its rows stands for a band of lines.

use super::{
    density::{self, Density},
    LineRuns,
};
use std::ops::Range;

/// The strip rows of a content store whose rows each stand for a band of
/// buffer lines, as the terminal holds them.
///
/// The line store under it keeps summarizing buffer lines, and this layer
/// derives one row per band from them. A row goes stale when a line of its
/// band or a mark over it changes. A sync derives the stale rows of its window
/// again and sends only the rows that come out different.
pub(super) struct RowStore {
    pub(super) density: Density,
    /// The rows the terminal holds, one per band.
    pub(super) sent: Vec<LineRuns>,
    /// Whether each row waits to be derived again.
    pub(super) stale: Vec<bool>,
    /// The rows the last sync asked to hold derived.
    pub(super) window: Range<u32>,
}

impl RowStore {
    /// A layer over a file of `file_lines` lines at `density`, every row stale,
    /// over a terminal store that holds `held` before its `minimap_len`.
    ///
    /// The `minimap_len` that sizes the store for the layer cuts `held` or
    /// extends it with blank rows, so a row is sent only where its derived
    /// summary differs from what the terminal still shows there.
    pub(super) fn new(density: Density, file_lines: u32, mut held: Vec<LineRuns>) -> RowStore {
        let count = density::rows(file_lines, density.lines_per_row) as usize;
        held.resize(count, LineRuns::default());
        RowStore {
            density,
            sent: held,
            stale: vec![true; count],
            window: 0..0,
        }
    }

    /// The rows the layer holds.
    pub(super) fn len(&self) -> u32 {
        self.sent.len() as u32
    }

    /// Mark stale the rows whose bands a line splice touched, the splice that
    /// replaced `removed` lines from `start` with `inserted`.
    ///
    /// A splice that changes the line count moves every band after it, so it
    /// marks every row from its own to the end.
    pub(super) fn note_lines(&mut self, start: u32, removed: u32, inserted: u32) {
        let lines_per_row = self.density.lines_per_row;
        let first = density::row_of(start, lines_per_row) as usize;
        let end = match removed == inserted {
            true => {
                let last = start + inserted.max(1) - 1;
                density::row_of(last, lines_per_row) as usize + 1
            },
            false => self.stale.len(),
        };
        let end = end.min(self.stale.len());
        if first < end {
            self.stale[first..end].fill(true);
        }
    }

    /// Mark every row stale, for a change to the marks of the whole file.
    pub(super) fn mark_all_stale(&mut self) {
        self.stale.fill(true);
    }

    /// Whether a row of the window still waits to be derived.
    pub(super) fn window_stale(&self) -> bool {
        let end = (self.window.end as usize).min(self.stale.len());
        let start = (self.window.start as usize).min(end);
        self.stale[start..end].contains(&true)
    }
}
