use crate::git::repository::CommitLogEntry;
use smallvec::{smallvec, SmallVec};
use std::{collections::HashMap, ops::Range};

const NUM_COLORS: usize = 8;

#[derive(Debug, Clone, Copy)]
pub enum CurveKind {
    Merge,
    Checkout,
}

#[derive(Debug, Clone)]
pub enum CommitLineSegment {
    Straight {
        to_row: usize,
    },
    Curve {
        to_column: usize,
        on_row: usize,
        curve_kind: CurveKind,
    },
}

#[derive(Debug, Clone)]
pub struct CommitLine {
    pub child_column: usize,
    pub full_interval: Range<usize>,
    pub color_idx: usize,
    pub segments: SmallVec<[CommitLineSegment; 1]>,
}

impl CommitLine {
    pub fn get_first_visible_segment_idx(
        &self,
        first_visible_row: usize,
    ) -> Option<(usize, usize)> {
        if first_visible_row > self.full_interval.end {
            return None;
        }
        if first_visible_row <= self.full_interval.start {
            return Some((0, self.child_column));
        }

        let mut current_column = self.child_column;

        for (idx, segment) in self.segments.iter().enumerate() {
            match segment {
                CommitLineSegment::Straight { to_row } => {
                    if *to_row >= first_visible_row {
                        return Some((idx, current_column));
                    }
                },
                CommitLineSegment::Curve {
                    to_column, on_row, ..
                } => {
                    if *on_row >= first_visible_row {
                        return Some((idx, current_column));
                    }
                    current_column = *to_column;
                },
            }
        }

        None
    }
}

#[derive(Debug, Clone)]
pub struct CommitEntry {
    pub lane: usize,
    pub color_idx: usize,
}

#[derive(Debug, Default, Clone)]
pub struct GraphOutput {
    pub entries: Vec<CommitEntry>,
    pub lines: Vec<CommitLine>,
    pub max_lanes: usize,
}

#[derive(Debug)]
enum LaneState {
    Empty,
    Active {
        starting_row: usize,
        starting_col: usize,
        destination_column: Option<usize>,
        color: Option<usize>,
        segments: SmallVec<[CommitLineSegment; 1]>,
    },
}

impl LaneState {
    fn is_empty(&self) -> bool {
        matches!(self, LaneState::Empty)
    }

    fn to_commit_line(
        &mut self,
        ending_row: usize,
        lane_column: usize,
        parent_column: usize,
        parent_color: usize,
    ) -> Option<CommitLine> {
        let state = std::mem::replace(self, LaneState::Empty);

        match state {
            LaneState::Active {
                starting_row,
                starting_col,
                destination_column,
                color,
                mut segments,
            } => {
                let final_destination = destination_column.unwrap_or(parent_column);
                let final_color = color.unwrap_or(parent_color);

                match segments.last_mut() {
                    Some(CommitLineSegment::Straight { to_row }) if *to_row == usize::MAX => {
                        if final_destination != lane_column {
                            *to_row = ending_row - 1;

                            let curved = CommitLineSegment::Curve {
                                to_column: final_destination,
                                on_row: ending_row,
                                curve_kind: CurveKind::Checkout,
                            };

                            if *to_row == starting_row {
                                let last_index = segments.len() - 1;
                                segments[last_index] = curved;
                            } else {
                                segments.push(curved);
                            }
                        } else {
                            *to_row = ending_row;
                        }
                    },
                    Some(CommitLineSegment::Curve {
                        on_row,
                        to_column,
                        curve_kind,
                    }) if *on_row == usize::MAX => {
                        if *to_column == usize::MAX {
                            *to_column = final_destination;
                        }
                        if matches!(curve_kind, CurveKind::Merge) {
                            *on_row = starting_row + 1;
                            if *on_row < ending_row {
                                if *to_column != final_destination {
                                    segments.push(CommitLineSegment::Straight {
                                        to_row: ending_row - 1,
                                    });
                                    segments.push(CommitLineSegment::Curve {
                                        to_column: final_destination,
                                        on_row: ending_row,
                                        curve_kind: CurveKind::Checkout,
                                    });
                                } else {
                                    segments
                                        .push(CommitLineSegment::Straight { to_row: ending_row });
                                }
                            } else if *to_column != final_destination {
                                segments.push(CommitLineSegment::Curve {
                                    to_column: final_destination,
                                    on_row: ending_row,
                                    curve_kind: CurveKind::Checkout,
                                });
                            }
                        } else {
                            *on_row = ending_row;
                            if *to_column != final_destination {
                                segments.push(CommitLineSegment::Straight { to_row: ending_row });
                                segments.push(CommitLineSegment::Curve {
                                    to_column: final_destination,
                                    on_row: ending_row,
                                    curve_kind: CurveKind::Checkout,
                                });
                            }
                        }
                    },
                    Some(CommitLineSegment::Curve {
                        on_row, to_column, ..
                    }) => {
                        if *on_row < ending_row {
                            if *to_column != final_destination {
                                segments.push(CommitLineSegment::Straight {
                                    to_row: ending_row - 1,
                                });
                                segments.push(CommitLineSegment::Curve {
                                    to_column: final_destination,
                                    on_row: ending_row,
                                    curve_kind: CurveKind::Checkout,
                                });
                            } else {
                                segments.push(CommitLineSegment::Straight { to_row: ending_row });
                            }
                        } else if *to_column != final_destination {
                            segments.push(CommitLineSegment::Curve {
                                to_column: final_destination,
                                on_row: ending_row,
                                curve_kind: CurveKind::Checkout,
                            });
                        }
                    },
                    _ => {},
                }

                Some(CommitLine {
                    child_column: starting_col,
                    full_interval: starting_row..ending_row,
                    color_idx: final_color,
                    segments,
                })
            },
            LaneState::Empty => None,
        }
    }
}

struct GraphBuilder {
    lane_states: SmallVec<[LaneState; 8]>,
    lane_colors: HashMap<usize, usize>,
    parent_to_lanes: HashMap<String, SmallVec<[usize; 1]>>,
    next_color: usize,
    entries: Vec<CommitEntry>,
    lines: Vec<CommitLine>,
    max_lanes: usize,
}

impl GraphBuilder {
    fn new() -> Self {
        Self {
            lane_states: SmallVec::new(),
            lane_colors: HashMap::new(),
            parent_to_lanes: HashMap::new(),
            next_color: 0,
            entries: Vec::new(),
            lines: Vec::new(),
            max_lanes: 0,
        }
    }

    fn first_empty_lane_idx(&mut self) -> usize {
        self.lane_states
            .iter()
            .position(LaneState::is_empty)
            .unwrap_or_else(|| {
                self.lane_states.push(LaneState::Empty);
                self.lane_states.len() - 1
            })
    }

    fn get_lane_color(&mut self, lane_idx: usize) -> usize {
        *self.lane_colors.entry(lane_idx).or_insert_with(|| {
            let color_idx = self.next_color;
            self.next_color = (self.next_color + 1) % NUM_COLORS;
            color_idx
        })
    }

    fn add_commits(&mut self, commits: &[CommitLogEntry]) {
        self.entries.reserve(commits.len());
        self.lines.reserve(commits.len() / 2);

        for commit in commits {
            let commit_row = self.entries.len();

            let commit_lane = self
                .parent_to_lanes
                .get(&commit.oid)
                .and_then(|lanes| lanes.first().copied());

            let commit_lane = commit_lane.unwrap_or_else(|| self.first_empty_lane_idx());

            let commit_color = self.get_lane_color(commit_lane);

            if let Some(lanes) = self.parent_to_lanes.remove(&commit.oid) {
                for lane_column in lanes {
                    let state = &mut self.lane_states[lane_column];

                    if let LaneState::Active {
                        starting_row,
                        segments,
                        ..
                    } = state
                    {
                        if let Some(CommitLineSegment::Curve {
                            to_column,
                            curve_kind: CurveKind::Merge,
                            ..
                        }) = segments.first_mut()
                        {
                            let curve_row = *starting_row + 1;
                            let would_overlap =
                                if lane_column != commit_lane && curve_row < commit_row {
                                    self.entries[curve_row..commit_row]
                                        .iter()
                                        .any(|c| c.lane == commit_lane)
                                } else {
                                    false
                                };

                            if would_overlap {
                                *to_column = lane_column;
                            }
                        }
                    }

                    if let Some(commit_line) =
                        state.to_commit_line(commit_row, lane_column, commit_lane, commit_color)
                    {
                        self.lines.push(commit_line);
                    }
                }
            }

            for (parent_idx, parent) in commit.parent_oids.iter().enumerate() {
                if parent_idx == 0 {
                    self.lane_states[commit_lane] = LaneState::Active {
                        starting_row: commit_row,
                        starting_col: commit_lane,
                        destination_column: None,
                        color: Some(commit_color),
                        segments: smallvec![CommitLineSegment::Straight { to_row: usize::MAX }],
                    };

                    self.parent_to_lanes
                        .entry(parent.clone())
                        .or_default()
                        .push(commit_lane);
                } else {
                    let new_lane = self.first_empty_lane_idx();

                    self.lane_states[new_lane] = LaneState::Active {
                        starting_row: commit_row,
                        starting_col: commit_lane,
                        destination_column: None,
                        color: None,
                        segments: smallvec![CommitLineSegment::Curve {
                            to_column: usize::MAX,
                            on_row: usize::MAX,
                            curve_kind: CurveKind::Merge,
                        }],
                    };

                    self.parent_to_lanes
                        .entry(parent.clone())
                        .or_default()
                        .push(new_lane);
                }
            }

            self.max_lanes = self.max_lanes.max(self.lane_states.len());

            self.entries.push(CommitEntry {
                lane: commit_lane,
                color_idx: commit_color,
            });
        }
    }

    fn finalize(mut self) -> GraphOutput {
        let ending_row = self.entries.len();
        for (lane_column, state) in self.lane_states.iter_mut().enumerate() {
            if let Some(line) = state.to_commit_line(ending_row, lane_column, lane_column, 0) {
                self.lines.push(line);
            }
        }

        GraphOutput {
            entries: self.entries,
            lines: self.lines,
            max_lanes: self.max_lanes,
        }
    }
}

pub fn compute_graph(commits: &[CommitLogEntry]) -> GraphOutput {
    if commits.is_empty() {
        return GraphOutput::default();
    }
    let mut builder = GraphBuilder::new();
    builder.add_commits(commits);
    builder.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(oid: &str, parents: &[&str]) -> CommitLogEntry {
        CommitLogEntry {
            oid: oid.to_string(),
            short_hash: oid[..7.min(oid.len())].to_string(),
            author: "Test".to_string(),
            timestamp: 0,
            message: oid.to_string(),
            parent_oids: parents.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn linear_history() {
        let commits = vec![
            entry("aaa", &["bbb"]),
            entry("bbb", &["ccc"]),
            entry("ccc", &[]),
        ];
        let graph = compute_graph(&commits);

        assert_eq!(graph.entries.len(), 3);
        assert_eq!(graph.entries[0].lane, 0);
        assert_eq!(graph.entries[1].lane, 0);
        assert_eq!(graph.entries[2].lane, 0);

        assert_eq!(
            graph.lines.len(),
            2,
            "two straight lines: aaa->bbb, bbb->ccc"
        );
        for line in &graph.lines {
            assert_eq!(line.child_column, 0);
            assert_eq!(line.segments.len(), 1);
            assert!(matches!(
                line.segments[0],
                CommitLineSegment::Straight { .. }
            ));
        }
    }

    #[test]
    fn simple_branch_and_merge() {
        let commits = vec![
            entry("A", &["B", "C"]),
            entry("B", &["D"]),
            entry("C", &["D"]),
            entry("D", &[]),
        ];
        let graph = compute_graph(&commits);

        assert_eq!(graph.entries.len(), 4);
        assert_eq!(graph.entries[0].lane, 0, "merge commit A on lane 0");
        assert_eq!(graph.entries[1].lane, 0, "B on lane 0");
        assert_eq!(graph.entries[2].lane, 1, "C on lane 1");
        assert_eq!(graph.entries[3].lane, 0, "D on lane 0");

        assert!(
            graph.lines.len() >= 3,
            "at least 3 lines: A->B, A->C, B->D (C->D merges)"
        );
    }

    #[test]
    fn multiple_active_branches() {
        let commits = vec![
            entry("A", &["B"]),
            entry("C", &["D"]),
            entry("B", &["D"]),
            entry("D", &[]),
        ];
        let graph = compute_graph(&commits);

        assert_eq!(graph.entries.len(), 4);
        assert_eq!(graph.entries[0].lane, 0, "A on lane 0");
        assert_eq!(graph.entries[1].lane, 1, "C on lane 1 (new lane)");
        assert_eq!(graph.entries[2].lane, 0, "B on lane 0");
    }

    #[test]
    fn root_commit() {
        let commits = vec![entry("A", &[])];
        let graph = compute_graph(&commits);
        assert_eq!(graph.entries.len(), 1);
        assert_eq!(graph.entries[0].lane, 0);
        assert!(graph.lines.is_empty());
    }

    #[test]
    fn empty_input() {
        let graph = compute_graph(&[]);
        assert!(graph.entries.is_empty());
        assert!(graph.lines.is_empty());
    }
}
