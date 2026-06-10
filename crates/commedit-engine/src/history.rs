//! Walk the current branch into a flat, topologically ordered list of commits
//! for the history view (children before parents) — the ancestors of HEAD, like
//! `git log <current-branch>`. Other branches, remote-tracking refs and tags are
//! not shown.

use std::collections::HashSet;

use anyhow::{Context, Result};
use chrono::DateTime;
use futures::StreamExt;
use jj_lib::backend::{ChangeId, CommitId, Timestamp};
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::revset::{RevsetExpression, SymbolResolver, SymbolResolverExtension};

use crate::graph::GraphLayout;

/// A single row in the history view.
#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub id: CommitId,
    pub change_id: ChangeId,
    /// First line of the commit description.
    pub subject: String,
    /// Full commit description (message), including the subject line.
    pub description: String,
    pub author_name: String,
    pub author_email: String,
    pub committer_name: String,
    pub committer_email: String,
    /// Author and committer timestamps, formatted for display and editing as
    /// `YYYY-MM-DD HH:MM:SS ±HHMM` (see [`format_timestamp`]).
    pub author_time: String,
    pub committer_time: String,
    pub parents: Vec<CommitId>,
}

impl CommitInfo {
    /// Hex commit id, for display and stable identification in the UI.
    pub fn id_hex(&self) -> String {
        self.id.hex()
    }

    /// Hex change id. Stable across rewrites (the commit id changes, the change
    /// id does not), so the UI uses it to re-select a commit after saving.
    pub fn change_id_hex(&self) -> String {
        self.change_id.hex()
    }

    pub(crate) fn from_commit(commit: &Commit) -> Self {
        let description = commit.description().to_string();
        let subject = description.lines().next().unwrap_or("").to_string();
        let author = commit.author();
        let committer = commit.committer();
        Self {
            id: commit.id().clone(),
            change_id: commit.change_id().clone(),
            subject,
            description,
            author_name: author.name.clone(),
            author_email: author.email.clone(),
            committer_name: committer.name.clone(),
            committer_email: committer.email.clone(),
            author_time: format_timestamp(&author.timestamp),
            committer_time: format_timestamp(&committer.timestamp),
            parents: commit.parent_ids().to_vec(),
        }
    }
}

/// Format a jj [`Timestamp`] as `YYYY-MM-DD HH:MM:SS ±HHMM` in its own recorded
/// time zone — the shape [`parse_timestamp`] reads back.
pub fn format_timestamp(ts: &Timestamp) -> String {
    match ts.to_datetime() {
        Ok(dt) => dt.format("%Y-%m-%d %H:%M:%S %z").to_string(),
        Err(_) => String::new(),
    }
}

/// Parse a timestamp edited in the UI back into a jj [`Timestamp`]. Accepts the
/// `YYYY-MM-DD HH:MM:SS ±HHMM` form produced by [`format_timestamp`] as well as
/// RFC 3339 (e.g. `2026-06-05T14:30:00+02:00`).
pub fn parse_timestamp(s: &str) -> Result<Timestamp> {
    let s = s.trim();
    let dt = DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S %z")
        .or_else(|_| DateTime::parse_from_rfc3339(s))
        .with_context(|| format!("unrecognized date {s:?} (use YYYY-MM-DD HH:MM:SS ±HHMM)"))?;
    Ok(Timestamp::from_datetime(dt))
}

/// List the current branch's commits in topological order (newest first): the
/// ancestors of `head` (the checked-out commit), excluding the virtual root.
///
/// This mirrors `git log <current-branch>` — only commits reachable from HEAD.
/// Commits on other local branches, remote-tracking branches (e.g. `origin/*`)
/// and tags that sit off the current branch are intentionally not shown. `head`
/// is the live branch tip (`Repo::head_commit_id`); using it rather than jj's
/// `git_head()` (which lags behind a rewrite until re-imported) keeps the view
/// current without resurfacing stale, pre-rewrite commits.
pub fn history(repo: &ReadonlyRepo, head: &CommitId) -> Result<Vec<CommitInfo>> {
    Ok(history_limited(repo, head, usize::MAX)?.0)
}

/// Like [`history`], but stop after `limit` commits. Returns the loaded prefix
/// (newest first) together with a flag that is `true` when more commits remain
/// below it — i.e. the walk was cut short by the limit rather than reaching the
/// root. The revset iterates newest-first lazily, so the cost is `O(limit)`, not
/// `O(history length)`: this is what lets the UI page a deep history in chunks.
pub fn history_limited(
    repo: &ReadonlyRepo,
    head: &CommitId,
    limit: usize,
) -> Result<(Vec<CommitInfo>, bool)> {
    let symbol_resolver =
        SymbolResolver::new(repo, &([] as [&Box<dyn SymbolResolverExtension>; 0]));
    let expression = RevsetExpression::commits(vec![head.clone()])
        .ancestors()
        .resolve_user_expression(repo, &symbol_resolver)
        .context("resolving history revset")?;
    let revset = expression
        .evaluate(repo)
        .context("evaluating history revset")?;

    let store = repo.store();
    let root = store.root_commit_id().clone();
    let mut commits = Vec::new();
    let mut has_more = false;
    let mut ids = revset.commit_change_ids();
    while let Some(entry) = pollster::block_on(ids.next()) {
        let (id, _change_id) = entry.context("iterating history")?;
        if id == root {
            continue;
        }
        if commits.len() >= limit {
            has_more = true;
            break;
        }
        let commit = store.get_commit(&id).context("loading commit")?;
        commits.push(CommitInfo::from_commit(&commit));
    }
    Ok((commits, has_more))
}

/// The commits reachable from `head` but not from `base` — the range rewritten
/// since `base`, newest first (the jj `base..head` revset).
///
/// A rewrite/rebase only ever produces new commits *above* the unchanged base it
/// shares with the pre-rewrite tip; the untouched ancestors below stay byte-for-
/// byte identical (and so, having come from clean git history, conflict-free).
/// Only the rewritten commits can be conflicted, so conflict detection walks this
/// range instead of [`history`]'s full — possibly huge — ancestry of `head`.
/// `base` is the pre-rewrite branch tip; it and its ancestors are excluded.
pub fn history_range(
    repo: &ReadonlyRepo,
    base: &CommitId,
    head: &CommitId,
) -> Result<Vec<CommitInfo>> {
    let symbol_resolver =
        SymbolResolver::new(repo, &([] as [&Box<dyn SymbolResolverExtension>; 0]));
    let expression = RevsetExpression::commits(vec![base.clone()])
        .range(&RevsetExpression::commits(vec![head.clone()]))
        .resolve_user_expression(repo, &symbol_resolver)
        .context("resolving rewritten-range revset")?;
    let revset = expression
        .evaluate(repo)
        .context("evaluating rewritten-range revset")?;

    let store = repo.store();
    let root = store.root_commit_id().clone();
    let mut commits = Vec::new();
    let mut ids = revset.commit_change_ids();
    while let Some(entry) = pollster::block_on(ids.next()) {
        let (id, _change_id) = entry.context("iterating the rewritten range")?;
        if id == root {
            continue;
        }
        let commit = store.get_commit(&id).context("loading commit")?;
        commits.push(CommitInfo::from_commit(&commit));
    }
    Ok(commits)
}

/// A single-commit reorder, in the terms [`crate::repo::Repo::reorder_commit`]
/// wants: the moved commit, the parents to rebase it onto, the children to rebase
/// on top of it, and which commit should end up as the branch head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorderMove {
    pub target: CommitId,
    pub new_parents: Vec<CommitId>,
    pub new_children: Vec<CommitId>,
    pub new_tip: CommitId,
}

/// Whether the visible history is a single linear chain: every commit has exactly
/// one parent and is the sole parent of its predecessor in the list (newest
/// first). The oldest commit's only parent is the root, which is not listed.
///
/// Not used to gate reordering (the view legitimately shows other branches and
/// tags); kept as a precise description of "linear" for tests and callers.
pub fn is_linear_history(commits: &[CommitInfo]) -> bool {
    commits.iter().enumerate().all(|(i, c)| {
        c.parents.len() == 1
            && commits.get(i + 1).is_none_or(|parent| c.parents[0] == parent.id)
    })
}

/// The linear chain of the current branch, newest first: follow single-parent
/// edges from `head` until a merge or the root. Returned as indices into
/// `commits`.
///
/// The history view also shows other branches and tags, whose commits are
/// interleaved in `commits` by topological order. Reordering only ever touches
/// the current branch, so it is planned against this chain — foreign commits are
/// skipped, not treated as parents/children.
pub fn branch_chain(commits: &[CommitInfo], head: &CommitId) -> Vec<usize> {
    let mut chain = Vec::new();
    let mut id = head.clone();
    while let Some(pos) = commits.iter().position(|c| c.id == id) {
        let c = &commits[pos];
        // Stop at (and exclude) a merge or the root: the editable chain is the
        // single-parent run descending from the branch tip.
        if c.parents.len() != 1 {
            break;
        }
        chain.push(pos);
        id = c.parents[0].clone();
    }
    chain
}

/// One destination line for a reorder/restore drop: the concrete splice (`mv`)
/// plus the lane that line occupies at the drop boundary — which is what the UI
/// colors its pick-a-line swatch with, matching the drawn graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorderCandidate {
    pub mv: ReorderMove,
    pub lane: usize,
}

/// The commits reachable from `head` within the displayed list — the editable
/// subgraph. The list is a topological prefix of head's ancestry, so every
/// head-to-commit path is fully on-page and a parent walk over the list finds
/// them all; rows not in the set (foreign branches/tags, should the view ever
/// interleave them) are off-limits to structural edits.
pub(crate) fn branch_commits(commits: &[CommitInfo], head: &CommitId) -> HashSet<CommitId> {
    let mut reachable = HashSet::new();
    let mut stack = vec![head.clone()];
    while let Some(id) = stack.pop() {
        if !reachable.insert(id.clone()) {
            continue;
        }
        if let Some(c) = commits.iter().find(|c| c.id == id) {
            stack.extend(c.parents.iter().cloned());
        }
    }
    reachable
}

/// Enumerate the destination lines for splicing `target` into display gap `to`:
/// one candidate per ancestry line crossing the boundary (the lane edges of
/// [`GraphLayout::boundaries`]), plus — at the very bottom — a synthetic
/// candidate for re-rooting onto the virtual root, whose line the graph never
/// draws. The two halves of "dropped back onto its own line" skip out
/// naturally: the line descending *toward* `target` is its own upper edge
/// (`parent == target`), and a line left childless once `target` is removed
/// from it is its own lower edge. Gap `0` has no boundary — the single
/// candidate puts `target` on top of `head` as the new tip.
///
/// `new_tip` is the pre-splice id of the commit that ends up as the branch
/// head for every gap below the top.
fn splice_candidates(
    commits: &[CommitInfo],
    head: &CommitId,
    layout: &GraphLayout,
    root: &CommitId,
    branch: &HashSet<CommitId>,
    target: &CommitId,
    new_tip: &CommitId,
    to: usize,
) -> Vec<ReorderCandidate> {
    let n = commits.len();
    if to == 0 {
        return vec![ReorderCandidate {
            mv: ReorderMove {
                target: target.clone(),
                new_parents: vec![head.clone()],
                new_children: Vec::new(),
                new_tip: target.clone(),
            },
            lane: layout.rows[0].node_lane,
        }];
    }
    let mut out = Vec::new();
    for e in &layout.boundaries[to - 1] {
        if e.parent == *target {
            continue; // the line descending toward the dragged commit itself
        }
        let children: Vec<CommitId> =
            e.children.iter().filter(|c| *c != target).cloned().collect();
        if children.is_empty() || children.iter().any(|c| !branch.contains(c)) {
            continue;
        }
        out.push(ReorderCandidate {
            mv: ReorderMove {
                target: target.clone(),
                new_parents: vec![e.parent.clone()],
                new_children: children,
                new_tip: new_tip.clone(),
            },
            lane: e.lane,
        });
    }
    if to == n {
        // Below the oldest row: lines to off-page parents were listed above; the
        // re-root splice (parent the dragged commit on the virtual root, root
        // the current bottom commits on it) gets its synthetic candidate here,
        // on the lane of the line that visually ends at the last row.
        let children: Vec<CommitId> = commits
            .iter()
            .filter(|c| c.parents.contains(root) && c.id != *target && branch.contains(&c.id))
            .map(|c| c.id.clone())
            .collect();
        if !children.is_empty() {
            out.push(ReorderCandidate {
                mv: ReorderMove {
                    target: target.clone(),
                    new_parents: vec![root.clone()],
                    new_children: children,
                    new_tip: new_tip.clone(),
                },
                lane: layout.rows[n - 1].node_lane,
            });
        }
    }
    out
}

/// Plan a drag of the commit at display index `from` to the insertion gap `to`
/// (`0..=len`) in the full ancestry graph: one [`ReorderCandidate`] per
/// destination line crossing the gap (often exactly one; several where parallel
/// merge lanes pass). Empty for an out-of-range or no-op drop, a merge commit
/// (those stay fixed), an off-branch row, or a `layout` stale against `commits`.
pub fn plan_reorder_candidates(
    commits: &[CommitInfo],
    head: &CommitId,
    layout: &GraphLayout,
    root: &CommitId,
    from: usize,
    to: usize,
) -> Vec<ReorderCandidate> {
    let n = commits.len();
    if from >= n || to > n || layout.boundaries.len() != n {
        return Vec::new();
    }
    let dragged = &commits[from];
    let branch = branch_commits(commits, head);
    // A merge stays fixed — there is no single line to splice it into — and a
    // row off the editable subgraph is refused.
    if dragged.parents.len() != 1 || !branch.contains(&dragged.id) {
        return Vec::new();
    }
    if to == 0 && dragged.id == *head {
        return Vec::new(); // already the tip
    }
    // Moving the tip down exposes its sole parent as the new branch head; any
    // other move leaves the head commit in place (rewritten, same change id).
    let new_tip = if dragged.id == *head {
        dragged.parents[0].clone()
    } else {
        head.clone()
    };
    splice_candidates(commits, head, layout, root, &branch, &dragged.id, &new_tip, to)
}

/// Plan grafting a trashed commit (one not currently in `commits`) back into
/// the history at display gap `to`: like [`plan_reorder_candidates`], one
/// candidate per destination line, but without the own-line no-op cases (the
/// restored commit has no line in the graph). Empty for an out-of-range drop, a
/// commit that is in the history after all, or a stale `layout`.
pub fn plan_restore_candidates(
    commits: &[CommitInfo],
    head: &CommitId,
    layout: &GraphLayout,
    root: &CommitId,
    restored: &CommitInfo,
    to: usize,
) -> Vec<ReorderCandidate> {
    let n = commits.len();
    if n == 0 || to > n || layout.boundaries.len() != n {
        return Vec::new();
    }
    if commits.iter().any(|c| c.id == restored.id) {
        return Vec::new();
    }
    let branch = branch_commits(commits, head);
    splice_candidates(commits, head, layout, root, &branch, &restored.id, head, to)
}

/// Splice the commit at chain position `from` into chain gap `to` (`0..=len`).
/// `chain` is a linear list newest-first, so a row's upper neighbour is its child
/// and its lower neighbour is its parent. After removing the dragged commit, the
/// gap `g` sits between `chain[g - 1]` (the new child) and `chain[g]` (the new
/// parent).
fn plan_linear(chain: &[&CommitInfo], from: usize, to: usize) -> Option<ReorderMove> {
    let n = chain.len();
    if from >= n || to > n {
        return None;
    }
    // Gap in the list with `from` removed. Dropping into the original slot
    // (either half of the dragged row) is a no-op.
    let g = if to > from { to - 1 } else { to };
    if g == from {
        return None;
    }
    let others_len = n - 1;
    // Index into `chain` skipping the dragged row.
    let other = |i: usize| -> &CommitInfo {
        if i < from {
            chain[i]
        } else {
            chain[i + 1]
        }
    };

    let new_children = if g >= 1 {
        vec![other(g - 1).id.clone()]
    } else {
        Vec::new() // dropped at the top: the dragged commit becomes the new head
    };
    let new_parents = if g < others_len {
        vec![other(g).id.clone()]
    } else {
        // Dropped at the bottom: parent whatever the previously-oldest commit was
        // rooted on (usually the root commit).
        other(others_len - 1).parents.clone()
    };
    // The new head is the dragged commit if it went to the top, else the
    // unchanged newest commit.
    let new_tip = if g == 0 {
        chain[from].id.clone()
    } else {
        other(0).id.clone()
    };

    Some(ReorderMove {
        target: chain[from].id.clone(),
        new_parents,
        new_children,
        new_tip,
    })
}

/// Plan a drag of the commit at display index `from` (in the newest-first view)
/// to the insertion gap `to` (`0..=len`, meaning "before row `to`"). `head` is the
/// current branch tip. Returns `None` for an out-of-range or no-op drop, or when
/// the dragged row is not on the current branch's linear chain.
///
/// The drop point is mapped onto the branch chain (see [`branch_chain`]) by
/// counting how many chain commits sit above it, so interleaved commits from
/// other branches/tags in the view are ignored rather than mistaken for
/// neighbours — which is what produced spurious empty merge commits and the
/// `graph has cycle` abort.
pub fn plan_reorder(
    commits: &[CommitInfo],
    head: &CommitId,
    from: usize,
    to: usize,
) -> Option<ReorderMove> {
    let n = commits.len();
    if from >= n || to > n {
        return None;
    }
    let chain = branch_chain(commits, head);
    // Map the dragged display row to its chain position; reject rows off-chain.
    let p = chain.iter().position(|&i| i == from)?;
    // Map the display gap to a chain gap: the chain commits above the drop point.
    let cg = chain.iter().filter(|&&i| i < to).count();
    let chain_commits: Vec<&CommitInfo> = chain.iter().map(|&i| &commits[i]).collect();
    plan_linear(&chain_commits, p, cg)
}

/// The commit id of the display row `index`, if it can be dropped from history:
/// any single-parent commit reachable from the branch head, anywhere in the
/// graph — its children (possibly a merge, which keeps its other parents)
/// rebase onto its parent. A merge commit stays fixed: abandoning one would
/// fold both its lines into its children (and strand the bookmark between two
/// parents when it is the tip). Off-branch rows and the branch's only commit
/// are refused. Returns `None` otherwise — the UI uses this both to gate the
/// drop and to validate it.
pub fn plan_drop(commits: &[CommitInfo], head: &CommitId, index: usize) -> Option<CommitId> {
    let c = commits.get(index)?;
    if commits.len() < 2
        || c.parents.len() != 1
        || !branch_commits(commits, head).contains(&c.id)
    {
        return None;
    }
    Some(c.id.clone())
}

/// Splice a commit that is *not* in the chain into gap `g` (`0..=len`). Mirrors
/// [`plan_linear`] but without removing a dragged row, since the restored commit
/// is being inserted rather than moved.
fn plan_linear_insert(chain: &[&CommitInfo], restored: &CommitId, g: usize) -> Option<ReorderMove> {
    let n = chain.len();
    if g > n {
        return None;
    }
    let new_children = if g >= 1 {
        vec![chain[g - 1].id.clone()]
    } else {
        Vec::new() // restored at the top: it becomes the new head
    };
    let new_parents = if g < n {
        vec![chain[g].id.clone()]
    } else {
        // Restored at the bottom: root it where the previously-oldest commit was.
        chain[n - 1].parents.clone()
    };
    let new_tip = if g == 0 {
        restored.clone()
    } else {
        chain[0].id.clone()
    };
    Some(ReorderMove {
        target: restored.clone(),
        new_parents,
        new_children,
        new_tip,
    })
}

/// Plan grafting a trashed commit (one not currently in `commits`) back into the
/// history at display gap `to` (`0..=len`). `head` is the current branch tip.
/// Returns `None` for an out-of-range drop or when HEAD has no linear chain.
///
/// The drop point is mapped onto the branch chain the same way [`plan_reorder`]
/// does, so commits interleaved from other branches/tags are ignored.
pub fn plan_restore(
    commits: &[CommitInfo],
    head: &CommitId,
    restored: &CommitInfo,
    to: usize,
) -> Option<ReorderMove> {
    if to > commits.len() {
        return None;
    }
    let chain = branch_chain(commits, head);
    let cg = chain.iter().filter(|&&i| i < to).count();
    let chain_commits: Vec<&CommitInfo> = chain.iter().map(|&i| &commits[i]).collect();
    plan_linear_insert(&chain_commits, &restored.id, cg)
}

#[cfg(test)]
mod tests {
    use super::{
        format_timestamp, is_linear_history, parse_timestamp, plan_drop, plan_reorder,
        plan_reorder_candidates, plan_restore, plan_restore_candidates, CommitInfo,
        ReorderCandidate, ReorderMove,
    };
    use crate::graph::compute_graph;
    use jj_lib::backend::{ChangeId, CommitId};

    /// A bare [`CommitInfo`] with id `id` and a single parent `parent`, enough to
    /// exercise [`plan_reorder`]'s index arithmetic.
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

    /// Newest-first history: 3 <- 2 <- 1 <- root(0).
    fn history() -> Vec<CommitInfo> {
        vec![ci(3, 2), ci(2, 1), ci(1, 0)]
    }

    #[test]
    fn dropping_into_the_same_slot_is_a_noop() {
        let h = history();
        assert_eq!(plan_reorder(&h, &cid(3), 1, 1), None); // onto its own upper half
        assert_eq!(plan_reorder(&h, &cid(3), 1, 2), None); // onto its own lower half
    }

    #[test]
    fn moving_the_tip_down_to_the_oldest_position() {
        // Drag row 0 (tip "3") to the very bottom (gap 3).
        let mv = plan_reorder(&history(), &cid(3), 0, 3).expect("move");
        assert_eq!(mv.target, cid(3));
        assert_eq!(mv.new_parents, vec![cid(0)]); // onto the root
        assert_eq!(mv.new_children, vec![cid(1)]); // old oldest becomes its child
        assert_eq!(mv.new_tip, cid(2)); // "2" becomes the head
    }

    #[test]
    fn moving_a_commit_up_to_the_top() {
        // Drag row 2 (oldest "1") to the top (gap 0).
        let mv = plan_reorder(&history(), &cid(3), 2, 0).expect("move");
        assert_eq!(mv.target, cid(1));
        assert_eq!(mv.new_parents, vec![cid(3)]); // onto the old tip
        assert!(mv.new_children.is_empty()); // becomes the new head
        assert_eq!(mv.new_tip, cid(1)); // ...so it is the tip
    }

    #[test]
    fn moving_a_commit_into_the_middle() {
        // Drag row 0 (tip "3") down to sit between "2" and "1" (gap 2).
        let mv = plan_reorder(&history(), &cid(3), 0, 2).expect("move");
        assert_eq!(mv.target, cid(3));
        assert_eq!(mv.new_parents, vec![cid(1)]); // parent the older "1"
        assert_eq!(mv.new_children, vec![cid(2)]); // child the newer "2"
        assert_eq!(mv.new_tip, cid(2)); // "2" is now the head
    }

    #[test]
    fn out_of_range_is_rejected() {
        assert_eq!(plan_reorder(&history(), &cid(3), 3, 0), None);
        assert_eq!(plan_reorder(&history(), &cid(3), 0, 4), None);
    }

    /// A commit with an explicit set of parents, for building non-linear graphs.
    fn merge(id: u8, parents: &[u8]) -> CommitInfo {
        let mut c = ci(id, 0);
        c.parents = parents.iter().map(|p| CommitId::new(vec![*p])).collect();
        c
    }

    #[test]
    fn recognizes_a_linear_chain() {
        assert!(is_linear_history(&history()));
    }

    #[test]
    fn a_merge_commit_makes_history_non_linear() {
        // 3 is a merge of 2 and 1; 2 <- 1 <- root.
        let h = vec![merge(3, &[2, 1]), ci(2, 1), ci(1, 0)];
        assert!(!is_linear_history(&h));
    }

    #[test]
    fn divergent_tips_make_history_non_linear() {
        // Two single-parent tips (3 and 4) both rooted on 1: not a single chain,
        // because 3's parent is 1, not the next listed row 4.
        let h = vec![ci(3, 1), ci(4, 1), ci(1, 0)];
        assert!(!is_linear_history(&h));
    }

    #[test]
    fn reorder_of_a_merge_branch_tip_is_refused() {
        // The branch tip itself is a merge: it has no single-parent chain to
        // splice into, so every drag is refused rather than dropping a parent.
        let h = vec![merge(3, &[2, 1]), ci(2, 1), ci(1, 0)];
        assert_eq!(plan_reorder(&h, &cid(3), 0, 3), None);
        assert_eq!(plan_reorder(&h, &cid(3), 2, 0), None);
    }

    /// A linear branch `4 <- 3 <- 2 <- 1 <- root` (head 4) whose view also shows a
    /// foreign commit `5` (branched off `2`) interleaved at the top — the davici
    /// shape: a clean branch plus a divergent ref in the gitk-style view.
    fn history_with_foreign_branch() -> Vec<CommitInfo> {
        vec![ci(5, 2), ci(4, 3), ci(3, 2), ci(2, 1), ci(1, 0)]
    }

    #[test]
    fn reorder_ignores_interleaved_foreign_commits() {
        // Drag the branch tip "4" (display row 1) to the bottom. Planning runs
        // over the branch chain [4,3,2,1]; the foreign "5" at row 0 is skipped,
        // not mistaken for a neighbour.
        let h = history_with_foreign_branch();
        let mv = plan_reorder(&h, &cid(4), 1, 5).expect("move");
        assert_eq!(mv.target, cid(4));
        assert_eq!(mv.new_parents, vec![cid(0)]); // onto the root
        assert_eq!(mv.new_children, vec![cid(1)]); // old oldest becomes its child
        assert_eq!(mv.new_tip, cid(3)); // "3" becomes the head
    }

    #[test]
    fn dragging_a_foreign_commit_is_refused() {
        // Row 0 is the foreign commit "5", not on the current branch's chain.
        let h = history_with_foreign_branch();
        assert_eq!(plan_reorder(&h, &cid(4), 0, 3), None);
    }

    #[test]
    fn drop_returns_an_on_chain_commit() {
        // Row 1 ("2") is on the branch chain [3,2,1]; dropping it yields its id.
        assert_eq!(plan_drop(&history(), &cid(3), 1), Some(cid(2)));
    }

    #[test]
    fn any_single_parent_commit_in_the_graph_is_droppable() {
        let h = merge_history();
        // Both sides of the merge and the fork point below it are droppable.
        assert_eq!(plan_drop(&h, &cid(4), 1), Some(cid(3)));
        assert_eq!(plan_drop(&h, &cid(4), 2), Some(cid(2)));
        assert_eq!(plan_drop(&h, &cid(4), 3), Some(cid(1)));
    }

    #[test]
    fn dropping_a_merge_commit_is_refused() {
        let h = merge_history();
        assert_eq!(plan_drop(&h, &cid(4), 0), None);
    }

    #[test]
    fn dropping_a_foreign_commit_is_refused() {
        // Row 0 is the foreign commit "5", not on the current branch's chain.
        let h = history_with_foreign_branch();
        assert_eq!(plan_drop(&h, &cid(4), 0), None);
    }

    #[test]
    fn dropping_the_only_commit_is_refused() {
        // Refuse to empty the branch: a single-commit chain has nothing to drop.
        let h = vec![ci(1, 0)];
        assert_eq!(plan_drop(&h, &cid(1), 0), None);
    }

    #[test]
    fn restoring_at_the_top_makes_the_commit_the_head() {
        // Graft "9" above the tip "3" (gap 0).
        let mv = plan_restore(&history(), &cid(3), &ci(9, 0), 0).expect("restore");
        assert_eq!(mv.target, cid(9));
        assert_eq!(mv.new_parents, vec![cid(3)]); // onto the old tip
        assert!(mv.new_children.is_empty()); // becomes the new head
        assert_eq!(mv.new_tip, cid(9));
    }

    #[test]
    fn restoring_in_the_middle_threads_the_chain() {
        // Graft "9" between "2" and "1" (gap 2).
        let mv = plan_restore(&history(), &cid(3), &ci(9, 0), 2).expect("restore");
        assert_eq!(mv.target, cid(9));
        assert_eq!(mv.new_parents, vec![cid(1)]); // parent the older "1"
        assert_eq!(mv.new_children, vec![cid(2)]); // child the newer "2"
        assert_eq!(mv.new_tip, cid(3)); // tip unchanged
    }

    #[test]
    fn restoring_at_the_bottom_roots_the_commit() {
        // Graft "9" below the oldest "1" (gap 3).
        let mv = plan_restore(&history(), &cid(3), &ci(9, 0), 3).expect("restore");
        assert_eq!(mv.target, cid(9));
        assert_eq!(mv.new_parents, vec![cid(0)]); // onto the root
        assert_eq!(mv.new_children, vec![cid(1)]); // old oldest becomes its child
        assert_eq!(mv.new_tip, cid(3)); // tip unchanged
    }

    /// [`plan_reorder_candidates`] over `h` with the graph computed on the fly
    /// (root `0`), the way the `Repo` wrapper calls it.
    fn reorder_cands(
        h: &[CommitInfo],
        head: u8,
        from: usize,
        to: usize,
    ) -> Vec<ReorderCandidate> {
        let g = compute_graph(h, &cid(0));
        plan_reorder_candidates(h, &cid(head), &g, &cid(0), from, to)
    }

    /// [`plan_restore_candidates`] over `h`, graph computed on the fly.
    fn restore_cands(
        h: &[CommitInfo],
        head: u8,
        restored: &CommitInfo,
        to: usize,
    ) -> Vec<ReorderCandidate> {
        let g = compute_graph(h, &cid(0));
        plan_restore_candidates(h, &cid(head), &g, &cid(0), restored, to)
    }

    fn mv(target: u8, parents: &[u8], children: &[u8], tip: u8) -> ReorderMove {
        ReorderMove {
            target: cid(target),
            new_parents: parents.iter().map(|&p| cid(p)).collect(),
            new_children: children.iter().map(|&c| cid(c)).collect(),
            new_tip: cid(tip),
        }
    }

    #[test]
    fn candidates_match_the_linear_plan_on_a_linear_chain() {
        // Every linear plan_reorder case yields exactly one candidate with the
        // same splice: the lane planner subsumes the chain planner.
        let h = history();
        for (from, to) in
            [(0, 2), (0, 3), (1, 0), (1, 3), (2, 0), (2, 1)]
        {
            let cands = reorder_cands(&h, 3, from, to);
            assert_eq!(cands.len(), 1, "one line crosses a linear gap ({from}->{to})");
            assert_eq!(cands[0].lane, 0);
            assert_eq!(
                Some(&cands[0].mv),
                plan_reorder(&h, &cid(3), from, to).as_ref(),
                "lane plan equals the chain plan ({from}->{to})"
            );
        }
    }

    #[test]
    fn dropping_back_onto_the_own_line_yields_no_candidates() {
        // Both halves of the dragged row's own slot: the line descending toward
        // it (gap above) and the line leaving it (gap below).
        let h = history();
        assert_eq!(reorder_cands(&h, 3, 1, 1), vec![]);
        assert_eq!(reorder_cands(&h, 3, 1, 2), vec![]);
        // The tip dropped at the very top is equally a no-op.
        assert_eq!(reorder_cands(&h, 3, 0, 0), vec![]);
        // The bottom commit dropped at the very bottom likewise.
        assert_eq!(reorder_cands(&h, 3, 2, 3), vec![]);
    }

    #[test]
    fn candidate_indices_out_of_range_are_rejected() {
        let h = history();
        assert_eq!(reorder_cands(&h, 3, 3, 0), vec![]);
        assert_eq!(reorder_cands(&h, 3, 0, 4), vec![]);
    }

    #[test]
    fn a_stale_layout_yields_no_candidates() {
        let h = history();
        let g = compute_graph(&h[..2], &cid(0)); // one row short
        assert_eq!(plan_reorder_candidates(&h, &cid(3), &g, &cid(0), 0, 2), vec![]);
    }

    /// A merge topology: 4 merges 2 into 3, both branched off 1 (head 4).
    /// Display order: [4, 3, 2, 1]; lanes: 3 on lane 0, 2 on lane 1.
    fn merge_history() -> Vec<CommitInfo> {
        vec![merge(4, &[3, 2]), ci(3, 1), ci(2, 1), ci(1, 0)]
    }

    #[test]
    fn a_merge_commit_is_not_a_reorder_source() {
        let h = merge_history();
        assert_eq!(reorder_cands(&h, 4, 0, 2), vec![]);
        assert_eq!(reorder_cands(&h, 4, 0, 4), vec![]);
    }

    #[test]
    fn a_parallel_lane_at_the_dragged_rows_height_is_a_candidate() {
        // Drag 3 (lane 0) into the gap just below it: its own line skips out,
        // but the sibling line 4->2 on lane 1 crosses there — moving 3 into the
        // other branch between the merge and 2.
        let h = merge_history();
        let cands = reorder_cands(&h, 4, 1, 2);
        assert_eq!(cands, vec![ReorderCandidate { mv: mv(3, &[2], &[4], 4), lane: 1 }]);
    }

    #[test]
    fn a_gap_crossed_by_two_lanes_yields_two_candidates() {
        // Drag the fork point 1 up into the gap below the merge: both parent
        // lines of the merge cross it, one candidate per lane.
        let h = merge_history();
        let cands = reorder_cands(&h, 4, 3, 1);
        assert_eq!(
            cands,
            vec![
                ReorderCandidate { mv: mv(1, &[3], &[4], 4), lane: 0 },
                ReorderCandidate { mv: mv(1, &[2], &[4], 4), lane: 1 },
            ]
        );
    }

    #[test]
    fn the_own_line_is_skipped_at_any_distance() {
        // Drag the fork point 1 into the gap between 3 and 2: lane 0 descends
        // toward 1 itself (skipped), lane 1 is the genuine sibling candidate.
        let h = merge_history();
        let cands = reorder_cands(&h, 4, 3, 2);
        assert_eq!(cands, vec![ReorderCandidate { mv: mv(1, &[2], &[4], 4), lane: 1 }]);
    }

    #[test]
    fn a_converged_lane_reparents_all_its_children() {
        // Criss-cross: 5 and 4 both fork toward 1 on the shared lane 2. A
        // restore into that line below the convergence re-parents both.
        let h = vec![
            merge(6, &[5, 4]),
            merge(5, &[2, 1]),
            merge(4, &[2, 1]),
            ci(2, 0),
            ci(1, 0),
        ];
        let cands = restore_cands(&h, 6, &ci(9, 0), 3);
        assert_eq!(
            cands,
            vec![
                ReorderCandidate { mv: mv(9, &[2], &[5], 6), lane: 0 },
                ReorderCandidate { mv: mv(9, &[2], &[4], 6), lane: 1 },
                ReorderCandidate { mv: mv(9, &[1], &[5, 4], 6), lane: 2 },
            ]
        );
    }

    #[test]
    fn a_truncated_page_offers_the_offpage_line_but_no_root_candidate() {
        // Page cut below 3: the line toward the unloaded 2 crosses the bottom
        // gap; no displayed commit sits on the root, so no re-root candidate.
        let h = vec![merge(4, &[3, 2]), ci(3, 1)];
        let cands = reorder_cands(&h, 4, 1, 2);
        assert_eq!(cands, vec![ReorderCandidate { mv: mv(3, &[2], &[4], 4), lane: 1 }]);
    }

    #[test]
    fn candidates_ignore_foreign_rows_and_lines() {
        let h = history_with_foreign_branch();
        // The foreign row 5 is not draggable…
        assert_eq!(reorder_cands(&h, 4, 0, 3), vec![]);
        // …and the gap under it is crossed only by its foreign line: refused.
        assert_eq!(reorder_cands(&h, 4, 1, 1), vec![]);
        // The bottom drop of the branch tip still plans like the chain did.
        let cands = reorder_cands(&h, 4, 1, 5);
        assert_eq!(cands, vec![ReorderCandidate { mv: mv(4, &[0], &[1], 3), lane: 0 }]);
    }

    #[test]
    fn restore_candidates_cover_top_lanes_and_root() {
        let h = merge_history();
        let nine = ci(9, 0);
        // Top: the restored commit becomes the tip.
        assert_eq!(
            restore_cands(&h, 4, &nine, 0),
            vec![ReorderCandidate { mv: mv(9, &[4], &[], 9), lane: 0 }]
        );
        // Below the merge: one candidate per parent line.
        assert_eq!(
            restore_cands(&h, 4, &nine, 1),
            vec![
                ReorderCandidate { mv: mv(9, &[3], &[4], 4), lane: 0 },
                ReorderCandidate { mv: mv(9, &[2], &[4], 4), lane: 1 },
            ]
        );
        // Bottom: re-root, the old bottom commit becomes the child.
        assert_eq!(
            restore_cands(&h, 4, &nine, 4),
            vec![ReorderCandidate { mv: mv(9, &[0], &[1], 4), lane: 0 }]
        );
    }

    #[test]
    fn restoring_a_commit_already_in_the_history_is_refused() {
        let h = history();
        assert_eq!(restore_cands(&h, 3, &ci(2, 1), 0), vec![]);
    }

    #[test]
    fn timestamp_round_trips_through_display_form() {
        let text = "2026-06-05 14:30:00 +0200";
        let ts = parse_timestamp(text).expect("parse");
        assert_eq!(format_timestamp(&ts), text);
    }

    #[test]
    fn parse_accepts_rfc3339_and_keeps_the_offset() {
        let ts = parse_timestamp("2026-06-05T14:30:00+02:00").expect("parse");
        assert_eq!(format_timestamp(&ts), "2026-06-05 14:30:00 +0200");
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(parse_timestamp("not a date").is_err());
    }
}
