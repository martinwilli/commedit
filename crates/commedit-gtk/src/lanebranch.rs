//! Pure lane → branch mapping for the unified multi-branch DAG.
//!
//! Phase 3's cross-branch drag needs to know, for a dragged commit and a drop
//! destination, *which editable branch each sits on* — a drop that crosses a
//! branch boundary pops the Copy/Move chooser, and the lane picker labels each
//! candidate line with its branch name. The graph layout (`graph.rs`) only knows
//! lanes and ancestry edges; it carries no branch identity. This module derives
//! that identity from the displayed `commits` plus each editable branch's tip,
//! with no GTK or jj-repo access — so it is unit-tested headless (the only way to
//! prove the drag logic without a GUI test path).
//!
//! The model: a commit "belongs to" every editable branch whose tip can reach it
//! by walking parents over the displayed DAG. A commit shared by several branches
//! (a common ancestor) belongs to all of them. Two commits are *cross-branch*
//! when no single branch reaches both — i.e. their branch sets are disjoint. A
//! destination *line* (a graph lane edge, given by the commits it re-parents)
//! takes the branches reaching those commits.

use std::collections::{BTreeSet, HashMap};

use commedit_engine::history::CommitInfo;
use commedit_engine::CommitId;

/// One editable branch's identity for the mapping: its short name and current
/// tip. Built from `Repo::editable_branches()` paired with each branch's
/// `local_branches().head`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BranchTip {
    pub(crate) name: String,
    pub(crate) tip: CommitId,
}

/// The branches reaching each displayed commit, keyed by commit id. A commit
/// reachable from several editable tips (a shared ancestor) maps to all of them;
/// a commit reachable from none (off every editable branch, e.g. a mere ref-pill
/// ancestor) is absent. Branch names are kept sorted so labels read stably.
///
/// Computed by walking parents from each branch tip over the displayed DAG —
/// O(branches × commits), trivial for a history page. The walk only follows
/// commits that are *in* `commits`, so an off-page ancestor doesn't drag a branch
/// label onto rows it isn't shown reaching.
pub(crate) struct LaneBranches {
    by_commit: HashMap<CommitId, BTreeSet<String>>,
}

impl LaneBranches {
    /// Build the mapping from the displayed `commits` and the editable branch
    /// tips. A tip not present in `commits` (scrolled off, or stale) simply
    /// contributes nothing.
    pub(crate) fn compute(commits: &[CommitInfo], branches: &[BranchTip]) -> Self {
        // Index commits by id for O(1) parent lookups during the per-branch walk.
        let index: HashMap<&CommitId, &CommitInfo> = commits.iter().map(|c| (&c.id, c)).collect();
        let mut by_commit: HashMap<CommitId, BTreeSet<String>> = HashMap::new();
        for branch in branches {
            let mut stack = vec![branch.tip.clone()];
            let mut seen = BTreeSet::new();
            while let Some(id) = stack.pop() {
                if !seen.insert(id.clone()) {
                    continue;
                }
                let Some(c) = index.get(&id) else {
                    continue; // off-page (or the tip isn't displayed): stop here
                };
                by_commit
                    .entry(id.clone())
                    .or_default()
                    .insert(branch.name.clone());
                stack.extend(c.parents.iter().cloned());
            }
        }
        Self { by_commit }
    }

    /// The branches reaching commit `id` (empty if none, e.g. an off-branch
    /// ancestor-pill row).
    pub(crate) fn branches_of(&self, id: &CommitId) -> &BTreeSet<String> {
        static EMPTY: BTreeSet<String> = BTreeSet::new();
        self.by_commit.get(id).unwrap_or(&EMPTY)
    }

    /// The branches reaching *any* of `ids` — the identity of a destination line
    /// described by the commits it re-parents (a candidate's `new_children`, or
    /// `new_parents` when there are no children). Used both to detect a
    /// cross-branch drop and to label a lane in the picker.
    pub(crate) fn branches_of_any(&self, ids: &[CommitId]) -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for id in ids {
            out.extend(self.branches_of(id).iter().cloned());
        }
        out
    }

    /// Whether commit `source` is cross-branch from a destination *line* given by
    /// the commits it re-parents (`line` = a candidate's `new_children`, falling
    /// back to `new_parents` for a top/childless splice; a single-element `line`
    /// is the commit-to-commit case — a squash target, say). The line crosses a
    /// boundary when no single editable branch reaches both the source and any
    /// commit on the line. A shared ancestor (reached by both) is *not*
    /// cross-branch — squashing into it, or reordering onto its line, stays within
    /// a branch the source is already on. When the source or the whole line is off
    /// every editable branch the answer is `false` (no boundary to cross — the
    /// in-branch path handles it, as today).
    pub(crate) fn line_is_cross_branch(&self, source: &CommitId, line: &[CommitId]) -> bool {
        let a = self.branches_of(source);
        let b = self.branches_of_any(line);
        if a.is_empty() || b.is_empty() {
            return false;
        }
        a.is_disjoint(&b)
    }

    /// A short human label for the branches reaching `ids` — the lane picker's
    /// per-candidate caption (e.g. `feature`, or `main, feature` for a shared
    /// line). `None` when no editable branch reaches the line (an unlabelled
    /// lane, drawn as the colour swatch alone, as before).
    pub(crate) fn label_for(&self, ids: &[CommitId]) -> Option<String> {
        let set = self.branches_of_any(ids);
        if set.is_empty() {
            None
        } else {
            Some(set.into_iter().collect::<Vec<_>>().join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BranchTip, LaneBranches};
    use commedit_engine::history::CommitInfo;
    use commedit_engine::{ChangeId, CommitId};

    fn ci(id: u8, parent: u8) -> CommitInfo {
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
            parents: vec![CommitId::new(vec![parent])],
        }
    }

    fn cid(id: u8) -> CommitId {
        CommitId::new(vec![id])
    }

    fn tip(name: &str, id: u8) -> BranchTip {
        BranchTip {
            name: name.to_string(),
            tip: cid(id),
        }
    }

    /// Two branches forked at a shared ancestor 1: A is `3 <- 1`, B is `5 <- 4 <- 1`.
    /// Display order newest-first `[5, 4, 3, 1]`.
    fn two_branch_dag() -> Vec<CommitInfo> {
        vec![ci(5, 4), ci(4, 1), ci(3, 1), ci(1, 0)]
    }

    #[test]
    fn each_commit_maps_to_the_branches_reaching_it() {
        let h = two_branch_dag();
        let lb = LaneBranches::compute(&h, &[tip("a", 3), tip("b", 5)]);
        // A's tip and B's chain are each on their own branch…
        assert_eq!(lb.branches_of(&cid(3)).iter().collect::<Vec<_>>(), ["a"]);
        assert_eq!(lb.branches_of(&cid(4)).iter().collect::<Vec<_>>(), ["b"]);
        assert_eq!(lb.branches_of(&cid(5)).iter().collect::<Vec<_>>(), ["b"]);
        // …and the fork point 1 is shared by both.
        assert_eq!(
            lb.branches_of(&cid(1)).iter().collect::<Vec<_>>(),
            ["a", "b"]
        );
    }

    #[test]
    fn distinct_branch_tips_are_cross_branch() {
        let h = two_branch_dag();
        let lb = LaneBranches::compute(&h, &[tip("a", 3), tip("b", 5)]);
        // A's tip vs B's commits: different branches (commit-to-commit, a squash
        // target — a single-element line).
        assert!(lb.line_is_cross_branch(&cid(3), &[cid(5)]));
        assert!(lb.line_is_cross_branch(&cid(3), &[cid(4)]));
        // B's own commits are not cross-branch with each other.
        assert!(!lb.line_is_cross_branch(&cid(5), &[cid(4)]));
    }

    #[test]
    fn a_shared_ancestor_is_not_cross_branch() {
        let h = two_branch_dag();
        let lb = LaneBranches::compute(&h, &[tip("a", 3), tip("b", 5)]);
        // The fork point 1 is on both branches, so dragging A's tip onto it (or
        // squashing into it) stays within a branch the source already shares.
        assert!(!lb.line_is_cross_branch(&cid(3), &[cid(1)]));
        assert!(!lb.line_is_cross_branch(&cid(5), &[cid(1)]));
    }

    #[test]
    fn a_singleton_set_never_crosses_a_boundary() {
        // With only one branch ticked, every reachable commit shares that branch:
        // no drop is ever cross-branch — the in-branch path stays in force.
        let h = two_branch_dag();
        let lb = LaneBranches::compute(&h, &[tip("a", 3)]);
        assert!(!lb.line_is_cross_branch(&cid(3), &[cid(1)]));
        // 5/4 are off the (single) editable branch, so disjoint-but-empty → false.
        assert!(!lb.line_is_cross_branch(&cid(3), &[cid(5)]));
        assert!(lb.branches_of(&cid(5)).is_empty());
    }

    #[test]
    fn a_destination_line_takes_its_commits_branches() {
        let h = two_branch_dag();
        let lb = LaneBranches::compute(&h, &[tip("a", 3), tip("b", 5)]);
        // A line re-parenting B's commit 5 is on branch B; A's tip crosses onto it.
        assert!(lb.line_is_cross_branch(&cid(3), &[cid(5)]));
        assert_eq!(lb.label_for(&[cid(5)]).as_deref(), Some("b"));
        // A line on the shared fork point is labelled with both branches and is
        // not a boundary crossing for either tip.
        assert!(!lb.line_is_cross_branch(&cid(3), &[cid(1)]));
        assert_eq!(lb.label_for(&[cid(1)]).as_deref(), Some("a, b"));
        // An off-branch line (no editable branch reaches it) is unlabelled.
        assert_eq!(lb.label_for(&[cid(9)]), None);
        assert!(!lb.line_is_cross_branch(&cid(3), &[cid(9)]));
    }
}
