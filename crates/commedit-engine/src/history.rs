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
    Ok(history_limited(repo, head, 0, usize::MAX)?.0)
}

/// Like [`history`], but skip the first `offset` commits (newest first) and stop
/// after `limit` more. Returns the loaded window together with a flag that is
/// `true` when more commits remain below it — i.e. the walk was cut short by the
/// limit rather than reaching the root. The revset iterates newest-first lazily,
/// so the cost is `O(offset + limit)`, not `O(history length)`: this is what lets
/// the UI (and the MCP server) page a deep history in chunks.
pub fn history_limited(
    repo: &ReadonlyRepo,
    head: &CommitId,
    offset: usize,
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
    let mut skipped = 0usize;
    let mut ids = revset.commit_change_ids();
    while let Some(entry) = pollster::block_on(ids.next()) {
        let (id, _change_id) = entry.context("iterating history")?;
        if id == root {
            continue;
        }
        if skipped < offset {
            skipped += 1;
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

/// Git-style shortest-unique-prefix abbreviator for one read of a repo. Holds the
/// repo so each call computes the shortest prefix that is unique *across the whole
/// visible index* — so an abbreviated id stays unique within any subset of it
/// (e.g. the branch history `lookup_ref` resolves against), and thus round-trips
/// straight back as a commit ref.
///
/// Lengths are floored at [`Self::MIN`]: jj's per-namespace shortest length could
/// otherwise let an abbreviated change id prefix-collide with some unrelated
/// commit's sha in `lookup_ref`'s OR-of-namespaces match. The floor makes that
/// negligible; the worst case is a recoverable "ambiguous ref" the caller retries
/// with a longer id. These are *display* hints — full ids still resolve exactly.
pub struct IdAbbrev<'a> {
    repo: Option<&'a ReadonlyRepo>,
}

impl<'a> IdAbbrev<'a> {
    /// Floor on an emitted prefix length, in hex chars.
    pub const MIN: usize = 8;

    /// Abbreviate against `repo`'s index.
    pub fn new(repo: &'a ReadonlyRepo) -> Self {
        Self { repo: Some(repo) }
    }

    /// A no-op abbreviator that returns full ids — for contexts without a repo
    /// (e.g. pure unit tests of the DTO conversions).
    pub fn full() -> Self {
        Self { repo: None }
    }

    /// Abbreviate a commit id to its shortest repo-unique prefix, floored at
    /// [`Self::MIN`]. Hex is ASCII, so byte slicing is char slicing.
    pub fn commit(&self, id: &CommitId) -> String {
        let full = id.hex();
        match self.repo {
            None => full,
            Some(repo) => {
                let n = repo
                    .index()
                    .shortest_unique_commit_id_prefix_len(id)
                    .unwrap_or(full.len())
                    .max(Self::MIN)
                    .min(full.len());
                full[..n].to_string()
            }
        }
    }

    /// Abbreviate a change id to its shortest repo-unique prefix, floored at
    /// [`Self::MIN`].
    pub fn change(&self, id: &ChangeId) -> String {
        let full = id.hex();
        match self.repo {
            None => full,
            Some(repo) => {
                let n = repo
                    .shortest_unique_change_id_prefix_len(id)
                    .unwrap_or(full.len())
                    .max(Self::MIN)
                    .min(full.len());
                full[..n].to_string()
            }
        }
    }
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
            && commits
                .get(i + 1)
                .is_none_or(|parent| c.parents[0] == parent.id)
    })
}

/// One destination line for a reorder/restore drop: the concrete splice (`mv`)
/// plus the lane that line occupies at the drop boundary — which is what the UI
/// colors its pick-a-line swatch with, matching the drawn graph.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorderCandidate {
    pub mv: ReorderMove,
    pub lane: usize,
}

/// A reorder of a *set* of commits, the multi-select generalization of
/// [`ReorderMove`] (see [`crate::repo::Repo::reorder_commits`]): the moved
/// commits, the parents/children to splice them between as a group, and which
/// commit ends up the branch head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorderSetMove {
    pub targets: Vec<CommitId>,
    pub new_parents: Vec<CommitId>,
    pub new_children: Vec<CommitId>,
    pub new_tip: CommitId,
}

/// One destination line for a set reorder, the set analogue of
/// [`ReorderCandidate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReorderSetCandidate {
    pub mv: ReorderSetMove,
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

/// The static graph context the splice planners share: the displayed commits,
/// the branch head, the lane layout, and the virtual root commit.
#[derive(Clone, Copy)]
struct SpliceCtx<'a> {
    commits: &'a [CommitInfo],
    head: &'a CommitId,
    layout: &'a GraphLayout,
    root: &'a CommitId,
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
    ctx: &SpliceCtx,
    branch: &HashSet<CommitId>,
    target: &CommitId,
    new_tip: &CommitId,
    to: usize,
) -> Vec<ReorderCandidate> {
    let SpliceCtx {
        commits,
        head,
        layout,
        root,
    } = *ctx;
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
        let children: Vec<CommitId> = e
            .children
            .iter()
            .filter(|c| *c != target)
            .cloned()
            .collect();
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
    let ctx = SpliceCtx {
        commits,
        head,
        layout,
        root,
    };
    splice_candidates(&ctx, &branch, &dragged.id, &new_tip, to)
}

/// Plan dragging a *set* of commits to the insertion gap `to` as a group — the
/// multi-select reorder. One [`ReorderSetCandidate`] per ancestry line crossing
/// the gap that is bounded by commits *outside* the set (lines whose parent is in
/// the set, or whose only children are in the set, are the set's own slots and
/// drop out as no-ops); plus the bottom re-root line. Empty when the set has
/// fewer than two members, contains a merge or off-branch/off-page commit, spans
/// the whole branch (nothing left to anchor the tip), the gap is out of range, or
/// `layout` is stale.
///
/// `new_tip`: gap 0 makes the set the new tip (its newest member), but is refused
/// when the head is in the set (it is already the top); for a lower gap the tip
/// stays `head`, unless the head is itself being moved, in which case it is the
/// newest commit left behind. The moved `targets` are listed newest-first (the
/// reverse-topological order `move_commits` wants).
pub fn plan_reorder_set_candidates(
    commits: &[CommitInfo],
    head: &CommitId,
    layout: &GraphLayout,
    root: &CommitId,
    set: &HashSet<CommitId>,
    to: usize,
) -> Vec<ReorderSetCandidate> {
    let n = commits.len();
    if set.len() < 2 || to > n || layout.boundaries.len() != n {
        return Vec::new();
    }
    let branch = branch_commits(commits, head);
    // The moved commits in display order (newest-first). Every member must be a
    // displayed, on-branch, single-parent commit — a merge has no single line to
    // splice and is never a drag source.
    let members: Vec<&CommitInfo> = commits.iter().filter(|c| set.contains(&c.id)).collect();
    if members.len() != set.len()
        || members
            .iter()
            .any(|c| c.parents.len() != 1 || !branch.contains(&c.id))
    {
        return Vec::new();
    }
    let targets: Vec<CommitId> = members.iter().map(|c| c.id.clone()).collect();
    let head_in_set = set.contains(head);

    if to == 0 {
        // Splice the set on top of the head as the new tip. Refused when the head
        // is itself in the set — it is already the top, so there is nowhere above.
        if head_in_set {
            return Vec::new();
        }
        return vec![ReorderSetCandidate {
            mv: ReorderSetMove {
                targets,
                new_parents: vec![head.clone()],
                new_children: Vec::new(),
                new_tip: members[0].id.clone(),
            },
            lane: layout.rows[0].node_lane,
        }];
    }

    // Moving the head exposes the newest commit left behind as the tip; any other
    // move leaves the head in place (rewritten, same change id). No commit left
    // behind means the whole branch is selected — nothing to anchor.
    let new_tip = if head_in_set {
        match commits.iter().find(|c| !set.contains(&c.id)) {
            Some(c) => c.id.clone(),
            None => return Vec::new(),
        }
    } else {
        head.clone()
    };

    let mut out = Vec::new();
    for e in &layout.boundaries[to - 1] {
        if set.contains(&e.parent) {
            continue; // a line descending toward a moved commit
        }
        let children: Vec<CommitId> = e
            .children
            .iter()
            .filter(|c| !set.contains(c))
            .cloned()
            .collect();
        if children.is_empty() || children.iter().any(|c| !branch.contains(c)) {
            continue;
        }
        out.push(ReorderSetCandidate {
            mv: ReorderSetMove {
                targets: targets.clone(),
                new_parents: vec![e.parent.clone()],
                new_children: children,
                new_tip: new_tip.clone(),
            },
            lane: e.lane,
        });
    }
    if to == n {
        // Re-root: parent the set on the virtual root, with the current bottom
        // commits (not in the set) rooted on it.
        let children: Vec<CommitId> = commits
            .iter()
            .filter(|c| c.parents.contains(root) && !set.contains(&c.id) && branch.contains(&c.id))
            .map(|c| c.id.clone())
            .collect();
        if !children.is_empty() {
            out.push(ReorderSetCandidate {
                mv: ReorderSetMove {
                    targets: targets.clone(),
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
    let ctx = SpliceCtx {
        commits,
        head,
        layout,
        root,
    };
    splice_candidates(&ctx, &branch, &restored.id, head, to)
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
    if commits.len() < 2 || c.parents.len() != 1 || !branch_commits(commits, head).contains(&c.id) {
        return None;
    }
    Some(c.id.clone())
}

#[cfg(test)]
mod tests {
    use super::{
        format_timestamp, is_linear_history, parse_timestamp, plan_drop, plan_reorder_candidates,
        plan_reorder_set_candidates, plan_restore_candidates, CommitInfo, ReorderCandidate,
        ReorderMove, ReorderSetCandidate, ReorderSetMove,
    };
    use crate::graph::compute_graph;
    use jj_lib::backend::{ChangeId, CommitId};
    use std::collections::HashSet;

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

    /// A linear branch `4 <- 3 <- 2 <- 1 <- root` (head 4) whose view also shows a
    /// foreign commit `5` (branched off `2`) interleaved at the top — the davici
    /// shape: a clean branch plus a divergent ref in the gitk-style view.
    fn history_with_foreign_branch() -> Vec<CommitInfo> {
        vec![ci(5, 2), ci(4, 3), ci(3, 2), ci(2, 1), ci(1, 0)]
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

    /// [`plan_reorder_candidates`] over `h` with the graph computed on the fly
    /// (root `0`), the way the `Repo` wrapper calls it.
    fn reorder_cands(h: &[CommitInfo], head: u8, from: usize, to: usize) -> Vec<ReorderCandidate> {
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
    fn linear_gaps_yield_one_candidate_each() {
        // On the plain chain 3 <- 2 <- 1 every real gap is crossed by exactly
        // one line; the splices match what the old chain planner produced.
        let h = history();
        let cases = [
            (0, 2, mv(3, &[1], &[2], 2)), // tip into the middle
            (0, 3, mv(3, &[0], &[1], 2)), // tip to the bottom, onto the root
            (1, 0, mv(2, &[3], &[], 2)),  // middle to the top: it becomes the head
            (1, 3, mv(2, &[0], &[1], 3)), // middle to the bottom
            (2, 0, mv(1, &[3], &[], 1)),  // oldest to the top
            (2, 1, mv(1, &[2], &[3], 3)), // oldest up under the tip
        ];
        for (from, to, expected) in cases {
            assert_eq!(
                reorder_cands(&h, 3, from, to),
                vec![ReorderCandidate {
                    mv: expected,
                    lane: 0
                }],
                "one line crosses a linear gap ({from}->{to})"
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
        assert_eq!(
            plan_reorder_candidates(&h, &cid(3), &g, &cid(0), 0, 2),
            vec![]
        );
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
        assert_eq!(
            cands,
            vec![ReorderCandidate {
                mv: mv(3, &[2], &[4], 4),
                lane: 1
            }]
        );
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
                ReorderCandidate {
                    mv: mv(1, &[3], &[4], 4),
                    lane: 0
                },
                ReorderCandidate {
                    mv: mv(1, &[2], &[4], 4),
                    lane: 1
                },
            ]
        );
    }

    #[test]
    fn the_own_line_is_skipped_at_any_distance() {
        // Drag the fork point 1 into the gap between 3 and 2: lane 0 descends
        // toward 1 itself (skipped), lane 1 is the genuine sibling candidate.
        let h = merge_history();
        let cands = reorder_cands(&h, 4, 3, 2);
        assert_eq!(
            cands,
            vec![ReorderCandidate {
                mv: mv(1, &[2], &[4], 4),
                lane: 1
            }]
        );
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
                ReorderCandidate {
                    mv: mv(9, &[2], &[5], 6),
                    lane: 0
                },
                ReorderCandidate {
                    mv: mv(9, &[2], &[4], 6),
                    lane: 1
                },
                ReorderCandidate {
                    mv: mv(9, &[1], &[5, 4], 6),
                    lane: 2
                },
            ]
        );
    }

    #[test]
    fn a_truncated_page_offers_the_offpage_line_but_no_root_candidate() {
        // Page cut below 3: the line toward the unloaded 2 crosses the bottom
        // gap; no displayed commit sits on the root, so no re-root candidate.
        let h = vec![merge(4, &[3, 2]), ci(3, 1)];
        let cands = reorder_cands(&h, 4, 1, 2);
        assert_eq!(
            cands,
            vec![ReorderCandidate {
                mv: mv(3, &[2], &[4], 4),
                lane: 1
            }]
        );
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
        assert_eq!(
            cands,
            vec![ReorderCandidate {
                mv: mv(4, &[0], &[1], 3),
                lane: 0
            }]
        );
    }

    #[test]
    fn restore_candidates_cover_top_lanes_and_root() {
        let h = merge_history();
        let nine = ci(9, 0);
        // Top: the restored commit becomes the tip.
        assert_eq!(
            restore_cands(&h, 4, &nine, 0),
            vec![ReorderCandidate {
                mv: mv(9, &[4], &[], 9),
                lane: 0
            }]
        );
        // Below the merge: one candidate per parent line.
        assert_eq!(
            restore_cands(&h, 4, &nine, 1),
            vec![
                ReorderCandidate {
                    mv: mv(9, &[3], &[4], 4),
                    lane: 0
                },
                ReorderCandidate {
                    mv: mv(9, &[2], &[4], 4),
                    lane: 1
                },
            ]
        );
        // Bottom: re-root, the old bottom commit becomes the child.
        assert_eq!(
            restore_cands(&h, 4, &nine, 4),
            vec![ReorderCandidate {
                mv: mv(9, &[0], &[1], 4),
                lane: 0
            }]
        );
    }

    #[test]
    fn restore_candidates_thread_a_linear_chain() {
        let h = history();
        let nine = ci(9, 0);
        // Graft at the top (it becomes the head), the middle, and the bottom
        // (onto the root) — one line per linear gap.
        assert_eq!(
            restore_cands(&h, 3, &nine, 0),
            vec![ReorderCandidate {
                mv: mv(9, &[3], &[], 9),
                lane: 0
            }]
        );
        assert_eq!(
            restore_cands(&h, 3, &nine, 2),
            vec![ReorderCandidate {
                mv: mv(9, &[1], &[2], 3),
                lane: 0
            }]
        );
        assert_eq!(
            restore_cands(&h, 3, &nine, 3),
            vec![ReorderCandidate {
                mv: mv(9, &[0], &[1], 3),
                lane: 0
            }]
        );
    }

    #[test]
    fn restoring_a_commit_already_in_the_history_is_refused() {
        let h = history();
        assert_eq!(restore_cands(&h, 3, &ci(2, 1), 0), vec![]);
    }

    fn set_of(ids: &[u8]) -> HashSet<CommitId> {
        ids.iter().map(|&i| cid(i)).collect()
    }

    /// [`plan_reorder_set_candidates`] over `h`, graph computed on the fly.
    fn reorder_set_cands(
        h: &[CommitInfo],
        head: u8,
        set: &[u8],
        to: usize,
    ) -> Vec<ReorderSetCandidate> {
        let g = compute_graph(h, &cid(0));
        plan_reorder_set_candidates(h, &cid(head), &g, &cid(0), &set_of(set), to)
    }

    fn set_mv(targets: &[u8], parents: &[u8], children: &[u8], tip: u8) -> ReorderSetMove {
        ReorderSetMove {
            targets: targets.iter().map(|&t| cid(t)).collect(),
            new_parents: parents.iter().map(|&p| cid(p)).collect(),
            new_children: children.iter().map(|&c| cid(c)).collect(),
            new_tip: cid(tip),
        }
    }

    /// Newest-first chain: 4 <- 3 <- 2 <- 1 <- root(0).
    fn history4() -> Vec<CommitInfo> {
        vec![ci(4, 3), ci(3, 2), ci(2, 1), ci(1, 0)]
    }

    #[test]
    fn a_set_moves_to_the_bottom_as_a_group() {
        // [3,2,1], move the top two {3,2} to the bottom (onto the root): they
        // splice between root and 1, and the commit left behind (1) becomes tip.
        let h = history();
        assert_eq!(
            reorder_set_cands(&h, 3, &[3, 2], 3),
            vec![ReorderSetCandidate {
                mv: set_mv(&[3, 2], &[0], &[1], 1),
                lane: 0
            }]
        );
    }

    #[test]
    fn a_set_moves_into_a_middle_gap() {
        // [4,3,2,1], move {4,3} (incl. the head) into the gap above 1: they splice
        // between 1 and 2, and 2 — the newest commit left behind — becomes tip.
        let h = history4();
        assert_eq!(
            reorder_set_cands(&h, 4, &[4, 3], 3),
            vec![ReorderSetCandidate {
                mv: set_mv(&[4, 3], &[1], &[2], 2),
                lane: 0
            }]
        );
    }

    #[test]
    fn a_set_moves_to_the_top_when_the_head_is_not_in_it() {
        // [3,2,1], move {2,1} to the very top (onto the head 3): the set becomes
        // the tip (its newest member, 2), 3 drops to the bottom.
        let h = history();
        assert_eq!(
            reorder_set_cands(&h, 3, &[2, 1], 0),
            vec![ReorderSetCandidate {
                mv: set_mv(&[2, 1], &[3], &[], 2),
                lane: 0
            }]
        );
    }

    #[test]
    fn a_set_at_the_top_with_the_head_in_it_is_a_no_op() {
        // The head is already the top; there is nowhere above it for the set.
        let h = history();
        assert_eq!(reorder_set_cands(&h, 3, &[3, 2], 0), vec![]);
    }

    #[test]
    fn a_contiguous_set_dropped_at_its_own_edges_yields_nothing() {
        // [4,3,2,1] with {3,2} selected: the gaps just above, within and just
        // below the block are all the set's own slots — no move.
        let h = history4();
        assert_eq!(reorder_set_cands(&h, 4, &[3, 2], 1), vec![]);
        assert_eq!(reorder_set_cands(&h, 4, &[3, 2], 2), vec![]);
        assert_eq!(reorder_set_cands(&h, 4, &[3, 2], 3), vec![]);
    }

    #[test]
    fn selecting_the_whole_branch_yields_nothing() {
        // Nothing is left behind to anchor the new tip.
        let h = history();
        assert_eq!(reorder_set_cands(&h, 3, &[3, 2, 1], 3), vec![]);
    }

    #[test]
    fn a_set_containing_a_merge_is_refused() {
        // 4 is a merge; a set holding it has no single line to splice.
        let h = merge_history();
        assert_eq!(reorder_set_cands(&h, 4, &[4, 3], 2), vec![]);
    }

    #[test]
    fn a_set_with_an_offpage_member_is_refused() {
        // 9 is not a displayed row; the set can't be fully placed.
        let h = history();
        assert_eq!(reorder_set_cands(&h, 3, &[3, 9], 3), vec![]);
    }

    #[test]
    fn a_set_move_across_parallel_merge_lanes_offers_one_per_line() {
        // 4 merges 3 and 2 (both off 1). Drag the fork point {1} is single; instead
        // drag {2,1}? 2 and 1 sit on different lanes. Move {3,2} (one per side)
        // into the gap below the merge: each side line that isn't a moved commit's
        // own slot crosses there.
        let h = merge_history(); // [4(3,2), 3, 2, 1]
                                 // Gap just below the merge (to=1): lane 0 carries 3 (in the set, its own
                                 // slot — skipped), lane 1 carries 2 (in the set — skipped). With both
                                 // sides selected the gap has no outside-bounded line.
        assert_eq!(reorder_set_cands(&h, 4, &[3, 2], 1), vec![]);
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
