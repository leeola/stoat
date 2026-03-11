use crate::git::repository::CommitLogEntry;

#[derive(Clone, Debug)]
pub struct GraphRow {
    pub column: usize,
    pub connections: Vec<Connection>,
    pub has_incoming: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionKind {
    Straight,
    MergeLeft,
    MergeRight,
    BranchLeft,
    BranchRight,
}

#[derive(Clone, Debug)]
pub struct Connection {
    pub from_col: usize,
    pub to_col: usize,
    pub kind: ConnectionKind,
    pub color_index: usize,
}

/// Compute visual graph layout from a list of commits with parent OIDs.
///
/// Each lane in `active_lanes` tracks which OID it expects next. Commits are
/// assigned to the lane expecting their OID (or a new lane if none). Parents
/// are then assigned to lanes for the next iteration.
pub fn compute_graph(commits: &[CommitLogEntry]) -> Vec<GraphRow> {
    let mut active_lanes: Vec<Option<String>> = Vec::new();
    let mut rows = Vec::with_capacity(commits.len());
    let mut color_counter: usize = 0;
    let mut lane_colors: Vec<usize> = Vec::new();

    for commit in commits {
        let col = active_lanes
            .iter()
            .position(|lane| lane.as_deref() == Some(&commit.oid));

        let has_incoming = col.is_some();

        let col = if let Some(c) = col {
            c
        } else {
            let free = active_lanes.iter().position(|l| l.is_none());
            if let Some(f) = free {
                active_lanes[f] = Some(commit.oid.clone());
                if f >= lane_colors.len() {
                    lane_colors.resize(f + 1, 0);
                }
                lane_colors[f] = color_counter;
                color_counter += 1;
                f
            } else {
                active_lanes.push(Some(commit.oid.clone()));
                lane_colors.push(color_counter);
                color_counter += 1;
                active_lanes.len() - 1
            }
        };

        let commit_color = lane_colors.get(col).copied().unwrap_or(0);

        let mut connections = Vec::new();

        for (i, lane) in active_lanes.iter().enumerate() {
            if i != col && lane.is_some() {
                let c = lane_colors.get(i).copied().unwrap_or(0);
                connections.push(Connection {
                    from_col: i,
                    to_col: i,
                    kind: ConnectionKind::Straight,
                    color_index: c,
                });
            }
        }

        if commit.parent_oids.is_empty() {
            active_lanes[col] = None;
        } else {
            let first_parent = &commit.parent_oids[0];

            let existing_lane = active_lanes
                .iter()
                .enumerate()
                .position(|(i, lane)| i != col && lane.as_deref() == Some(first_parent.as_str()));

            if let Some(target) = existing_lane {
                let kind = if target < col {
                    ConnectionKind::MergeLeft
                } else {
                    ConnectionKind::MergeRight
                };
                connections.push(Connection {
                    from_col: col,
                    to_col: target,
                    kind,
                    color_index: commit_color,
                });
                active_lanes[col] = None;
            } else {
                active_lanes[col] = Some(first_parent.clone());
                connections.push(Connection {
                    from_col: col,
                    to_col: col,
                    kind: ConnectionKind::Straight,
                    color_index: commit_color,
                });
            }

            for parent in &commit.parent_oids[1..] {
                let existing = active_lanes
                    .iter()
                    .position(|lane| lane.as_deref() == Some(parent.as_str()));

                if let Some(target) = existing {
                    let kind = if target < col {
                        ConnectionKind::MergeLeft
                    } else {
                        ConnectionKind::MergeRight
                    };
                    connections.push(Connection {
                        from_col: col,
                        to_col: target,
                        kind,
                        color_index: commit_color,
                    });
                } else {
                    let free = active_lanes.iter().position(|l| l.is_none());
                    let new_col = if let Some(f) = free {
                        active_lanes[f] = Some(parent.clone());
                        if f >= lane_colors.len() {
                            lane_colors.resize(f + 1, 0);
                        }
                        lane_colors[f] = color_counter;
                        color_counter += 1;
                        f
                    } else {
                        active_lanes.push(Some(parent.clone()));
                        lane_colors.push(color_counter);
                        color_counter += 1;
                        active_lanes.len() - 1
                    };

                    let kind = if new_col < col {
                        ConnectionKind::BranchLeft
                    } else {
                        ConnectionKind::BranchRight
                    };
                    connections.push(Connection {
                        from_col: col,
                        to_col: new_col,
                        kind,
                        color_index: lane_colors[new_col],
                    });
                }
            }
        }

        while active_lanes.last() == Some(&None) {
            active_lanes.pop();
            lane_colors.pop();
        }

        rows.push(GraphRow {
            column: col,
            connections,
            has_incoming,
        });
    }

    rows
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

        assert_eq!(graph.len(), 3);
        assert_eq!(graph[0].column, 0);
        assert_eq!(graph[1].column, 0);
        assert_eq!(graph[2].column, 0);
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

        assert_eq!(graph.len(), 4);
        assert_eq!(graph[0].column, 0, "merge commit A on col 0");
        assert_eq!(graph[1].column, 0, "B on col 0");
        assert_eq!(graph[2].column, 1, "C on col 1");
        assert_eq!(graph[3].column, 0, "D on col 0");
    }

    #[test]
    fn multiple_active_branches() {
        // A->B, C->D, B->D, D (root)
        // B on lane 0, C on lane 1; when B merges into D's lane, D ends up on lane 1
        let commits = vec![
            entry("A", &["B"]),
            entry("C", &["D"]),
            entry("B", &["D"]),
            entry("D", &[]),
        ];
        let graph = compute_graph(&commits);

        assert_eq!(graph.len(), 4);
        assert_eq!(graph[0].column, 0, "A on col 0");
        assert_eq!(graph[1].column, 1, "C on col 1 (new lane)");
        assert_eq!(graph[2].column, 0, "B on col 0");
        assert_eq!(graph[3].column, 1, "D on col 1 (C's lane expects D)");
    }

    #[test]
    fn root_commit() {
        let commits = vec![entry("A", &[])];
        let graph = compute_graph(&commits);
        assert_eq!(graph.len(), 1);
        assert_eq!(graph[0].column, 0);
    }

    #[test]
    fn empty_input() {
        let graph = compute_graph(&[]);
        assert!(graph.is_empty());
    }
}
