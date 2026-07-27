//! Lane layout for the commit picker's node graph.
//!
//! Pure geometry over a parent DAG. Nothing here knows about cells, pixels, or
//! colors, so the cell-glyph fallback and the stroked APC path can share one
//! layout.

use crate::host::CommitInfo;

/// One line segment running from this row to the next.
///
/// A vertical run has equal lanes. A differing pair is a branch leaving its
/// lane or a merge converging into one, which the renderer draws as a curve
/// across the row boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct GraphEdge {
    pub(crate) from_lane: u16,
    pub(crate) to_lane: u16,
    /// Whether this edge runs to a merge's second or later parent.
    ///
    /// Such an edge is the merged-in branch's own line arriving at the merge
    /// rather than anything belonging to the row it lands on, which is the one
    /// case where the renderer takes a color from [`Self::to_lane`] instead of
    /// [`Self::from_lane`]. Ordered last so sorting a row's edges still keys on
    /// the lanes.
    pub(crate) second_parent: bool,
}

/// Where one commit sits in the graph and what runs below it.
///
/// [`Self::edges`] covers the gap between this row and the next, so the last
/// row's edges describe lines leaving the bottom of the list. That happens
/// whenever the walk was truncated or a parent lies off it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GraphRow {
    pub(crate) node_lane: u16,
    pub(crate) edges: Vec<GraphEdge>,
}

/// Lay `commits` out as lanes, returning one row per commit plus the number of
/// lanes the widest row needs.
///
/// `commits` must be in display order, newest first, with each commit's
/// parents appearing later in the list or not at all. A parent that never
/// appears keeps its lane running to the bottom of the list, matching what
/// `git log --graph` draws over truncated output.
///
/// Lanes are unbounded here. Clamping the graph to a drawable width is the
/// renderer's call, since only it knows how many columns it has.
pub(crate) fn assign_lanes(commits: &[CommitInfo]) -> (Vec<GraphRow>, u16) {
    // Each lane holds the sha it is waiting for. A sha is never in two lanes at
    // once, which is what lets a second child of the same parent converge into
    // the lane already expecting it rather than opening a rival one.
    let mut active: Vec<Option<String>> = Vec::new();
    let mut rows: Vec<GraphRow> = Vec::with_capacity(commits.len());
    let mut lane_count: usize = 0;

    for commit in commits {
        // Snapshotted before any mutation, because a lane opened for a second
        // parent below has no line above it and must not draw a pass-through.
        let occupied_before: Vec<bool> = active.iter().map(Option::is_some).collect();

        let node_lane = match lane_of(&active, &commit.sha) {
            Some(lane) => lane,
            None => free_lane(&mut active, 0),
        };
        active[node_lane] = None;

        let mut edges: Vec<GraphEdge> = Vec::new();
        for (nth, parent) in commit.parents.iter().enumerate() {
            let to_lane = match lane_of(&active, parent) {
                Some(existing) => existing,
                None => {
                    let lane = if nth == 0 {
                        node_lane
                    } else {
                        free_lane(&mut active, node_lane + 1)
                    };
                    active[lane] = Some(parent.clone());
                    lane
                },
            };
            edges.push(GraphEdge {
                from_lane: node_lane as u16,
                to_lane: to_lane as u16,
                second_parent: nth >= 1,
            });
        }

        for (lane, expected) in active.iter().enumerate() {
            let carried_over = occupied_before.get(lane).copied().unwrap_or(false);
            if lane == node_lane || expected.is_none() || !carried_over {
                continue;
            }
            edges.push(GraphEdge {
                from_lane: lane as u16,
                to_lane: lane as u16,
                second_parent: false,
            });
        }
        edges.sort();

        lane_count = lane_count.max(active.len()).max(node_lane + 1);
        while active.last().is_some_and(Option::is_none) {
            active.pop();
        }

        rows.push(GraphRow {
            node_lane: node_lane as u16,
            edges,
        });
    }

    (rows, lane_count as u16)
}

fn lane_of(active: &[Option<String>], sha: &str) -> Option<usize> {
    active.iter().position(|lane| lane.as_deref() == Some(sha))
}

/// The leftmost empty lane at or after `from`, growing the layout when every
/// lane from there on is taken.
fn free_lane(active: &mut Vec<Option<String>>, from: usize) -> usize {
    match active.iter().skip(from).position(Option::is_none) {
        Some(offset) => from + offset,
        None => {
            active.resize(from.max(active.len()) + 1, None);
            active.len() - 1
        },
    }
}

#[cfg(test)]
mod tests {
    use super::{assign_lanes, GraphEdge, GraphRow};
    use crate::host::CommitInfo;

    fn commit(sha: &str, parents: &[&str]) -> CommitInfo {
        CommitInfo {
            sha: sha.to_string(),
            short_sha: sha.to_string(),
            summary: String::new(),
            author_name: String::new(),
            author_email: String::new(),
            time: 0,
            parents: parents.iter().map(|p| (*p).to_string()).collect(),
        }
    }

    /// One row as `(node_lane, [(from, to, second_parent), ...])`, which reads
    /// as a table where the struct form would not.
    type FlatRow = (u16, Vec<(u16, u16, bool)>);

    fn laid_out(commits: &[CommitInfo]) -> (Vec<FlatRow>, u16) {
        let (rows, lanes) = assign_lanes(commits);
        let flat = rows
            .into_iter()
            .map(|GraphRow { node_lane, edges }| {
                let edges = edges
                    .into_iter()
                    .map(
                        |GraphEdge {
                             from_lane,
                             to_lane,
                             second_parent,
                         }| (from_lane, to_lane, second_parent),
                    )
                    .collect();
                (node_lane, edges)
            })
            .collect();
        (flat, lanes)
    }

    #[test]
    fn a_linear_chain_stays_in_one_lane() {
        let history = [
            commit("c3", &["c2"]),
            commit("c2", &["c1"]),
            commit("c1", &[]),
        ];

        assert_eq!(
            laid_out(&history),
            (
                vec![
                    (0, vec![(0, 0, false)]),
                    (0, vec![(0, 0, false)]),
                    (0, vec![]),
                ],
                1
            )
        );
    }

    #[test]
    fn two_children_of_one_commit_converge_into_its_lane() {
        let history = [
            commit("a", &["root"]),
            commit("b", &["root"]),
            commit("root", &[]),
        ];

        assert_eq!(
            laid_out(&history),
            (
                vec![
                    (0, vec![(0, 0, false)]),
                    // b opens lane 1, then merges straight back into root's lane.
                    (1, vec![(0, 0, false), (1, 0, false)]),
                    (0, vec![]),
                ],
                2
            )
        );
    }

    #[test]
    fn a_merge_spawns_a_lane_and_the_branch_converges_back() {
        let history = [
            commit("m", &["a", "b"]),
            commit("a", &["root"]),
            commit("b", &["root"]),
            commit("root", &[]),
        ];

        assert_eq!(
            laid_out(&history),
            (
                vec![
                    // m's second parent opens lane 1, and that edge is the
                    // only one belonging to the branch rather than to m.
                    (0, vec![(0, 0, false), (0, 1, true)]),
                    (0, vec![(0, 0, false), (1, 1, false)]),
                    (1, vec![(0, 0, false), (1, 0, false)]),
                    (0, vec![]),
                ],
                2
            )
        );
    }

    #[test]
    fn a_parent_that_never_appears_runs_its_lane_off_the_bottom() {
        let history = [commit("m", &["a", "offlist"]), commit("a", &[])];

        assert_eq!(
            laid_out(&history),
            (
                vec![
                    (0, vec![(0, 0, false), (0, 1, true)]),
                    // The last row still carries lane 1 downward, because the
                    // commit it waits for is not in the list.
                    (0, vec![(1, 1, false)]),
                ],
                2
            )
        );
    }

    #[test]
    fn interleaved_branches_keep_the_lane_they_started_in() {
        let history = [
            commit("c1", &["b1"]),
            commit("c2", &["b2"]),
            commit("b1", &["root"]),
            commit("b2", &["root"]),
            commit("root", &[]),
        ];

        assert_eq!(
            laid_out(&history),
            (
                vec![
                    (0, vec![(0, 0, false)]),
                    (1, vec![(0, 0, false), (1, 1, false)]),
                    (0, vec![(0, 0, false), (1, 1, false)]),
                    (1, vec![(0, 0, false), (1, 0, false)]),
                    (0, vec![]),
                ],
                2
            )
        );
    }

    #[test]
    fn an_empty_history_lays_out_to_nothing() {
        assert_eq!(laid_out(&[]), (Vec::new(), 0));
    }
}
