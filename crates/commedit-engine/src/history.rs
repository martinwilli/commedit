//! Walk the current branch into a flat, topologically ordered list of commits
//! for the history view (children before parents) — the ancestors of HEAD, like
//! `git log <current-branch>`. Other branches, remote-tracking refs and tags are
//! not shown.

use anyhow::{Context, Result};
use chrono::DateTime;
use jj_lib::backend::{ChangeId, CommitId, Timestamp};
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::revset::{RevsetExpression, SymbolResolver, SymbolResolverExtension};

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

    fn from_commit(commit: &Commit) -> Self {
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
    for entry in revset.commit_change_ids() {
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
/// it must sit on the current branch's linear chain (see [`branch_chain`], which
/// already excludes merges, off-branch rows and the root), and dropping it must
/// not empty the branch (the chain has more than one commit). Returns `None`
/// otherwise — the UI uses this both to gate the drop and to validate it.
pub fn plan_drop(commits: &[CommitInfo], head: &CommitId, index: usize) -> Option<CommitId> {
    let chain = branch_chain(commits, head);
    if chain.len() < 2 || !chain.contains(&index) {
        return None;
    }
    Some(commits[index].id.clone())
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
        plan_restore, CommitInfo,
    };
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
