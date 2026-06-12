//! Lane layout for the gitk-style ancestry graph shown beside the history list:
//! pure lane arithmetic over the newest-first commit list (no jj repo access, no
//! GTK), in the spirit of `plan_reorder_candidates`.
//!
//! Each row's geometry is split at its vertical center: `edges_above` run from
//! the row's top edge down to the center, `edges_below` from the center to the
//! bottom edge, and the commit's node sits at the center on `node_lane`. Drawing
//! every row edge-to-edge makes adjacent rows' lines connect seamlessly without
//! any cross-row state in the renderer.
//!
//! Coloring contract: color an above-edge by its `from` lane (the lane at the
//! shared row boundary), a below-edge by its `to` lane, and the node by
//! `node_lane`. Row *i*'s below-edge into lane *b* then matches row *i+1*'s
//! above-edge out of lane *b*, so a line keeps one color across rows.

use jj_lib::backend::CommitId;

use crate::history::CommitInfo;

/// Graph geometry of one history row. A lane merely passing through appears as
/// a straight `(l, l)` edge in both halves; a child's edge arriving at the node
/// bends `(l, node_lane)` in the top half only; a merge parent's edge departs
/// `(node_lane, l)` in the bottom half only.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GraphRow {
    /// Column the commit's node (circle) sits on.
    pub node_lane: usize,
    /// More than one (non-root) parent — drawn distinct from ordinary commits.
    pub is_merge: bool,
    /// (lane at the top edge, lane at the center).
    pub edges_above: Vec<(usize, usize)>,
    /// (lane at the center, lane at the bottom edge).
    pub edges_below: Vec<(usize, usize)>,
}

/// One ancestry line crossing a row boundary: the lane it occupies and the DAG
/// edges it bundles. Every listed child has `parent` as a direct parent —
/// usually one child, several when converging lines share the lane (a merge
/// fork reusing a lane that already descends to the same parent). Splicing a
/// commit into this line means parenting it on `parent` and re-parenting all
/// `children` onto it, which is exactly what the drawn line depicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneEdge {
    pub lane: usize,
    pub children: Vec<CommitId>,
    pub parent: CommitId,
}

/// The whole list's layout: one [`GraphRow`] per commit (same indexing as the
/// input slice) plus the widest lane count, so every row's drawing area can be
/// sized identically and the columns align.
#[derive(Debug, Clone, Default)]
pub struct GraphLayout {
    pub rows: Vec<GraphRow>,
    pub max_lanes: usize,
    /// `boundaries[i]` is the set of lines crossing row *i*'s bottom edge — the
    /// display gap *i + 1* (a gap `g > 0` is crossed by `boundaries[g - 1]`).
    /// Edges to the virtual root are never listed, matching the drawing (the
    /// oldest commit's line ends at its node); edges to parents truncated by
    /// pagination are, since their lines run off the loaded page.
    pub boundaries: Vec<Vec<LaneEdge>>,
}

/// Lay the newest-first, topologically ordered `commits` (children always before
/// parents — `history()`'s order) out into lanes. `root` is the virtual root
/// commit id: edges to it are not drawn, so the oldest commit's line ends at its
/// node. Edges to parents *truncated by pagination* (absent from the list but not
/// the root) deliberately do run off the bottom edge of the last rows, signalling
/// that the history continues below the loaded page.
///
/// Classic active-lanes walk: each lane tracks the parent commit its line is
/// descending toward. A commit takes the leftmost lane expecting it (extra
/// expecting lanes — a fork point reached by several children — bend in and
/// close), its first parent continues on the node's lane, and every extra parent
/// of a merge forks out to a free lane (reusing a lane that already expects the
/// same parent, so converging lines meet early and the graph stays compact).
pub fn compute_graph(commits: &[CommitInfo], root: &CommitId) -> GraphLayout {
    // Each occupied lane tracks the parent its line descends toward and the
    // children whose edges it carries (several once converging lines merge).
    let mut lanes: Vec<Option<(CommitId, Vec<CommitId>)>> = Vec::new();
    let mut layout = GraphLayout::default();
    for commit in commits {
        let mut row = GraphRow::default();

        let matches: Vec<usize> = (0..lanes.len())
            .filter(|&l| lanes[l].as_ref().is_some_and(|(p, _)| p == &commit.id))
            .collect();
        row.node_lane = match matches.first() {
            Some(&l) => l,
            // Not expected by any lane: the topmost row (nothing above HEAD).
            None => take_free_lane(&mut lanes),
        };
        for (l, lane) in lanes.iter().enumerate() {
            if matches.contains(&l) {
                row.edges_above.push((l, row.node_lane));
            } else if lane.is_some() {
                row.edges_above.push((l, l));
            }
        }
        for &l in matches.iter().skip(1) {
            lanes[l] = None;
        }

        // The first (non-root) parent continues on the node's lane; each extra
        // parent forks out. A fork into a *fresh* lane opens a new line, so that
        // lane gets no straight continuation edge this row.
        let mut parents = commit.parents.iter().filter(|p| *p != root);
        lanes[row.node_lane] = parents.next().map(|p| (p.clone(), vec![commit.id.clone()]));
        let mut forks = Vec::new();
        let mut fresh = Vec::new();
        for parent in parents {
            row.is_merge = true;
            let found = (0..lanes.len()).find(|&l| {
                l != row.node_lane && lanes[l].as_ref().is_some_and(|(p, _)| p == parent)
            });
            let l = match found {
                // Converge into a lane already descending to this parent: the
                // line now carries this commit's edge too.
                Some(l) => {
                    lanes[l]
                        .as_mut()
                        .expect("occupied lane")
                        .1
                        .push(commit.id.clone());
                    l
                }
                None => {
                    let l = take_free_lane(&mut lanes);
                    lanes[l] = Some((parent.clone(), vec![commit.id.clone()]));
                    fresh.push(l);
                    l
                }
            };
            forks.push(l);
        }
        for (l, lane) in lanes.iter().enumerate() {
            if lane.is_some() && !fresh.contains(&l) {
                row.edges_below.push((l, l));
            }
        }
        for l in forks {
            row.edges_below.push((row.node_lane, l));
        }

        // Measured before the trim so a closing rightmost lane's bend still
        // fits inside every row's uniformly-sized drawing area.
        layout.max_lanes = layout.max_lanes.max(lanes.len());
        while lanes.last() == Some(&None) {
            lanes.pop();
        }
        layout.boundaries.push(
            lanes
                .iter()
                .enumerate()
                .filter_map(|(l, lane)| {
                    lane.as_ref().map(|(parent, children)| LaneEdge {
                        lane: l,
                        children: children.clone(),
                        parent: parent.clone(),
                    })
                })
                .collect(),
        );
        layout.rows.push(row);
    }
    layout
}

/// Index of the leftmost free lane, growing the lane list by one if all are
/// occupied. Leaves the lane `None`; the caller assigns it.
fn take_free_lane(lanes: &mut Vec<Option<(CommitId, Vec<CommitId>)>>) -> usize {
    match lanes.iter().position(Option::is_none) {
        Some(l) => l,
        None => {
            lanes.push(None);
            lanes.len() - 1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{compute_graph, GraphRow, LaneEdge};
    use crate::history::CommitInfo;
    use jj_lib::backend::{ChangeId, CommitId};

    /// A bare [`CommitInfo`] with id `id` and parents `parents` (0 = root).
    fn ci(id: u8, parents: &[u8]) -> CommitInfo {
        CommitInfo {
            id: CommitId::new(vec![id]),
            change_id: ChangeId::new(vec![id]),
            subject: String::new(),
            description: String::new(),
            author_name: String::new(),
            author_email: String::new(),
            committer_name: String::new(),
            committer_email: String::new(),
            author_time: String::new(),
            committer_time: String::new(),
            parents: parents.iter().map(|&p| CommitId::new(vec![p])).collect(),
        }
    }

    fn cid(id: u8) -> CommitId {
        CommitId::new(vec![id])
    }

    fn row(
        node_lane: usize,
        is_merge: bool,
        above: &[(usize, usize)],
        below: &[(usize, usize)],
    ) -> GraphRow {
        GraphRow {
            node_lane,
            is_merge,
            edges_above: above.to_vec(),
            edges_below: below.to_vec(),
        }
    }

    fn edge(lane: usize, children: &[u8], parent: u8) -> LaneEdge {
        LaneEdge {
            lane,
            children: children.iter().map(|&c| cid(c)).collect(),
            parent: cid(parent),
        }
    }

    #[test]
    fn linear_chain_uses_a_single_lane() {
        let h = vec![ci(3, &[2]), ci(2, &[1]), ci(1, &[0])];
        let g = compute_graph(&h, &cid(0));
        assert_eq!(g.max_lanes, 1);
        assert_eq!(g.rows[0], row(0, false, &[], &[(0, 0)]));
        assert_eq!(g.rows[1], row(0, false, &[(0, 0)], &[(0, 0)]));
        assert_eq!(g.rows[2], row(0, false, &[(0, 0)], &[]));
    }

    #[test]
    fn merge_forks_and_rejoins() {
        // 4 merges 2 into 3; both branched off 1.
        let h = vec![ci(4, &[3, 2]), ci(3, &[1]), ci(2, &[1]), ci(1, &[0])];
        let g = compute_graph(&h, &cid(0));
        assert_eq!(g.max_lanes, 2);
        assert_eq!(g.rows[0], row(0, true, &[], &[(0, 0), (0, 1)]));
        assert_eq!(
            g.rows[1],
            row(0, false, &[(0, 0), (1, 1)], &[(0, 0), (1, 1)])
        );
        assert_eq!(
            g.rows[2],
            row(1, false, &[(0, 0), (1, 1)], &[(0, 0), (1, 1)])
        );
        // The fork point: both lanes' edges bend into the node, lane 1 closes.
        assert_eq!(g.rows[3], row(0, false, &[(0, 0), (1, 0)], &[]));
    }

    #[test]
    fn octopus_allocates_one_lane_per_extra_parent() {
        let h = vec![ci(7, &[3, 2, 1]), ci(3, &[0]), ci(2, &[0]), ci(1, &[0])];
        let g = compute_graph(&h, &cid(0));
        assert_eq!(g.max_lanes, 3);
        assert_eq!(g.rows[0], row(0, true, &[], &[(0, 0), (0, 1), (0, 2)]));
    }

    #[test]
    fn criss_cross_reuses_a_lane_expecting_the_same_parent() {
        // Two merges of the same two branches: 6(5, 4), 5(2, 1), 4(2, 1).
        let h = vec![
            ci(6, &[5, 4]),
            ci(5, &[2, 1]),
            ci(4, &[2, 1]),
            ci(2, &[0]),
            ci(1, &[0]),
        ];
        let g = compute_graph(&h, &cid(0));
        // The first merge forks commit 1's line out to lane 2; the second one
        // converges into that same lane instead of opening a fourth.
        assert_eq!(g.max_lanes, 3);
        assert_eq!(
            g.rows[1],
            row(0, true, &[(0, 0), (1, 1)], &[(0, 0), (1, 1), (0, 2)])
        );
        assert_eq!(
            g.rows[2],
            row(
                1,
                true,
                &[(0, 0), (1, 1), (2, 2)],
                &[(0, 0), (1, 1), (2, 2), (1, 2)]
            )
        );
        // Commit 2: lanes 0 and 1 (one per merge) both expected it and join here.
        assert_eq!(
            g.rows[3],
            row(0, false, &[(0, 0), (1, 0), (2, 2)], &[(2, 2)])
        );
    }

    #[test]
    fn truncated_history_runs_edges_off_the_bottom() {
        // Only the first two rows of a merge topology are loaded: the last
        // row's edges still descend toward the unloaded parents.
        let h = vec![ci(4, &[3, 2]), ci(3, &[1])];
        let g = compute_graph(&h, &cid(0));
        assert_eq!(g.rows[1].edges_below, vec![(0, 0), (1, 1)]);
    }

    #[test]
    fn root_parent_draws_no_edge() {
        let h = vec![ci(1, &[0])];
        let g = compute_graph(&h, &cid(0));
        assert_eq!(g.max_lanes, 1);
        assert_eq!(g.rows[0], row(0, false, &[], &[]));
    }

    #[test]
    fn linear_boundaries_carry_one_edge_each() {
        let h = vec![ci(3, &[2]), ci(2, &[1]), ci(1, &[0])];
        let g = compute_graph(&h, &cid(0));
        assert_eq!(g.boundaries[0], vec![edge(0, &[3], 2)]);
        assert_eq!(g.boundaries[1], vec![edge(0, &[2], 1)]);
        // The oldest commit sits on the root: its line ends, no edge crosses.
        assert_eq!(g.boundaries[2], vec![]);
    }

    #[test]
    fn merge_boundaries_list_both_descending_lines() {
        // 4 merges 2 into 3; both branched off 1.
        let h = vec![ci(4, &[3, 2]), ci(3, &[1]), ci(2, &[1]), ci(1, &[0])];
        let g = compute_graph(&h, &cid(0));
        assert_eq!(g.boundaries[0], vec![edge(0, &[4], 3), edge(1, &[4], 2)]);
        assert_eq!(g.boundaries[1], vec![edge(0, &[3], 1), edge(1, &[4], 2)]);
        // Two sibling lines both descend toward 1 — distinct edges, one per lane.
        assert_eq!(g.boundaries[2], vec![edge(0, &[3], 1), edge(1, &[2], 1)]);
        assert_eq!(g.boundaries[3], vec![]);
    }

    #[test]
    fn converged_lane_bundles_its_children() {
        // Criss-cross: the second merge's fork toward 1 reuses the lane the
        // first one opened, so that line carries both edges below row 2.
        let h = vec![
            ci(6, &[5, 4]),
            ci(5, &[2, 1]),
            ci(4, &[2, 1]),
            ci(2, &[0]),
            ci(1, &[0]),
        ];
        let g = compute_graph(&h, &cid(0));
        assert_eq!(
            g.boundaries[2],
            vec![edge(0, &[5], 2), edge(1, &[4], 2), edge(2, &[5, 4], 1)]
        );
    }

    #[test]
    fn truncated_page_keeps_offpage_edges_active() {
        // Only the first two rows are loaded: the last boundary still lists the
        // lines running off the page toward the unloaded parents.
        let h = vec![ci(4, &[3, 2]), ci(3, &[1])];
        let g = compute_graph(&h, &cid(0));
        assert_eq!(g.boundaries[1], vec![edge(0, &[3], 1), edge(1, &[4], 2)]);
    }

    #[test]
    fn closed_lane_is_reused_by_a_later_merge() {
        // Two merges in sequence: by the time 5 forks, the side lane 8's merge
        // used has rejoined and is free again — the graph stays two lanes wide.
        let h = vec![
            ci(8, &[7, 6]),
            ci(7, &[5]),
            ci(6, &[5]),
            ci(5, &[3, 2]),
            ci(3, &[1]),
            ci(2, &[1]),
            ci(1, &[0]),
        ];
        let g = compute_graph(&h, &cid(0));
        assert_eq!(g.max_lanes, 2);
        assert_eq!(
            g.rows[3],
            row(0, true, &[(0, 0), (1, 0)], &[(0, 0), (0, 1)])
        );
    }
}
