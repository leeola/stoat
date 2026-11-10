//! The rows a strip holds when each of its rows stands for a band of lines.

use super::{
    band,
    density::{self, Density, Tier},
    rail_row, ClassTable, LineRuns, RowMark,
};
use std::ops::Range;

/// The fewest rows from one Item Rule to the next.
const RULE_SPACING: u32 = 5;

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
    /// The rows a Shape density leads with an Item Rule, ascending, or `None`
    /// until a sync counts them from the item lines.
    pub(super) rules: Option<Vec<u32>>,
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
            rules: None,
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

    /// Count the rows that lead with an Item Rule from `item_lines`, the
    /// ascending first lines of the file's top-level items, and mark every row
    /// stale.
    ///
    /// The row of an item start takes a rule when it lies at least
    /// [`RULE_SPACING`] rows below the last row that took one, so a dense
    /// outline shows rules that stay apart enough to read.
    pub(super) fn count_rules(&mut self, item_lines: &[u32]) {
        let lines_per_row = self.density.lines_per_row;
        let mut rules: Vec<u32> = Vec::new();
        for &line in item_lines {
            let row = density::row_of(line, lines_per_row);
            if rules.last().is_none_or(|&last| row >= last + RULE_SPACING) {
                rules.push(row);
            }
        }
        self.rules = Some(rules);
        self.mark_all_stale();
    }

    /// Whether a row of the window still waits to be derived.
    pub(super) fn window_stale(&self) -> bool {
        let end = (self.window.end as usize).min(self.stale.len());
        let start = (self.window.start as usize).min(end);
        self.stale[start..end].contains(&true)
    }
}

/// The Coverage Rail of a strip whose file does not fit it, which shows the
/// whole file on the strip's rows with each band that holds a hunk marked.
///
/// A rail has a row per strip row, so a sync sends it whole whenever the
/// marks, its fold, or the file's length move. It keeps only what it was
/// built from.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct RailStore {
    pub(super) lines_per_row: u32,
    pub(super) decoration: u64,
    pub(super) file_lines: u32,
}

/// The rail frames a sync leaves for the emitter to send on the rail's id.
#[derive(PartialEq, Debug)]
pub(super) enum RailUpdate {
    /// Size the rail's store to these rows and replace every one.
    Rows(Vec<LineRuns>),
    /// Retire the rail's store.
    Drop,
}

/// The Coverage Rail of a strip whose rows a pass over every row derives, as
/// a diff view's strip and the changes view's strip derive theirs.
///
/// The pass reads the mark of each row it summarizes, so the rail folds those
/// marks rather than walk the rows again. The rail goes out whole at the end
/// of the pass, and only when its lanes differ from the held ones, so a pass
/// that only a parse started sends no rail.
pub(super) struct PassRail {
    /// The content id of the rail's store on the terminal.
    id: u32,
    /// The lanes of the rail rows the terminal holds, or `None` while it holds
    /// no rail store.
    held: Option<Vec<Option<u8>>>,
    /// The strip's rows that one rail row stands for.
    rows_per_row: u32,
    /// The mark of each rail row from the rows the pass in flight folded so
    /// far, or `None` while no pass builds the rail.
    building: Option<Vec<RowMark>>,
}

impl PassRail {
    pub(super) fn new(id: u32) -> Self {
        Self {
            id,
            held: None,
            rows_per_row: 1,
            building: None,
        }
    }

    /// The rail's content id while the terminal holds its store, which the
    /// paint declares in the strip's last cell.
    pub(super) fn held_id(&self) -> Option<u32> {
        self.held.as_ref().map(|_| self.id)
    }

    /// Start to fold a pass over `total` rows at `density`, and answer the
    /// frame that drops a held rail when the density shows none.
    ///
    /// A density that fits its strip shows no rail, and neither does one that
    /// keeps a row per row, since its rows carry no band marks.
    pub(super) fn start(&mut self, density: Density, total: u32) -> Option<(u32, RailUpdate)> {
        if density.fits || density.tier == Tier::Line {
            return self.stop();
        }
        self.rows_per_row = density.whole_lines_per_row;
        let rows = density::rows(total, self.rows_per_row) as usize;
        self.building = Some(vec![RowMark::NONE; rows]);
        None
    }

    /// Fold `marks`, the marks of the rows from row `first` on, into the rail
    /// rows that hold those rows.
    pub(super) fn note(&mut self, first: u32, marks: &[RowMark], table: &ClassTable) {
        let Some(building) = self.building.as_mut() else {
            return;
        };
        for (row, &mark) in (first..).zip(marks) {
            if let Some(slot) = building.get_mut(density::row_of(row, self.rows_per_row) as usize) {
                *slot = band::band_mark([*slot, mark].into_iter(), table);
            }
        }
    }

    /// End the pass, and answer the whole rail when its lanes differ from the
    /// held ones.
    pub(super) fn finish(&mut self) -> Option<(u32, RailUpdate)> {
        let lanes: Vec<Option<u8>> = self
            .building
            .take()?
            .into_iter()
            .map(|mark| mark.lane)
            .collect();
        if self.held.as_ref() == Some(&lanes) {
            return None;
        }
        let rows = lanes.iter().map(|&lane| rail_row(lane)).collect();
        self.held = Some(lanes);
        Some((self.id, RailUpdate::Rows(rows)))
    }

    /// Stop the rail, and answer the frame that drops its store when the
    /// terminal holds one.
    pub(super) fn stop(&mut self) -> Option<(u32, RailUpdate)> {
        self.building = None;
        self.held.take().map(|_| (self.id, RailUpdate::Drop))
    }
}

#[cfg(test)]
mod tests {
    use super::{PassRail, RailUpdate};
    use crate::{
        minimap::{
            density::{Density, Tier},
            rail_row, ClassTable, EdgeClass, RowMark,
        },
        theme::Theme,
    };

    /// A pass at 4 rows a rail row folds two slices that split rail row 1
    /// into it, the added lane over the removed one. The same pass again sends
    /// nothing, and a density that fits drops the rail.
    #[test]
    fn a_pass_folds_its_slices_into_rail_rows() {
        let table = ClassTable::from_theme(&Theme::empty());
        let lane = |kind| RowMark {
            lane: Some(table.edge_class(kind)),
            tick: None,
        };
        let (added, removed, none) = (
            lane(EdgeClass::Added),
            lane(EdgeClass::Removed),
            RowMark::NONE,
        );
        let railed = Density {
            lines_per_row: 2,
            tier: Tier::Fit,
            fits: false,
            whole_lines_per_row: 4,
        };
        let pass = |rail: &mut PassRail| {
            rail.start(railed, 10);
            rail.note(0, &[none, none, none, none, removed, none], &table);
            rail.note(6, &[added, none, none, none], &table);
            rail.finish()
        };
        let mut rail = PassRail::new(7);

        assert_eq!(
            (
                pass(&mut rail),
                pass(&mut rail),
                rail.start(
                    Density {
                        fits: true,
                        ..railed
                    },
                    10
                ),
            ),
            (
                Some((
                    7,
                    RailUpdate::Rows([None, added.lane, None].map(rail_row).to_vec())
                )),
                None,
                Some((7, RailUpdate::Drop)),
            ),
        );
    }
}
