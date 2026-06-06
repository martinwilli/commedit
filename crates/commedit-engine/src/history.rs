//! Walk the repository into a flat, topologically ordered list of commits for
//! the history view (children before parents, like gitk).

use anyhow::{Context, Result};
use chrono::DateTime;
use jj_lib::backend::{ChangeId, CommitId, Timestamp};
use jj_lib::commit::Commit;
use jj_lib::object_id::ObjectId;
use jj_lib::repo::{ReadonlyRepo, Repo};
use jj_lib::revset::{
    RemoteRefSymbolExpression, RevsetExpression, SymbolResolver, SymbolResolverExtension,
};
use jj_lib::str_util::StringExpression;

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

/// List all visible commits in topological order (newest first), excluding the
/// virtual root commit.
pub fn history(repo: &ReadonlyRepo) -> Result<Vec<CommitInfo>> {
    // Mirror what `git log`/`gitk` show: commits reachable from real refs
    // (branches, remote branches, tags) and git HEAD. Two sources would otherwise
    // resurface stale, pre-rewrite commits as confusing duplicates of the commits
    // that replaced them:
    //   * `git_refs()` (which we avoid) also returns jj's internal `refs/jj/keep/*`
    //     refs, created to retain commits abandoned by a rewrite/reorder;
    //   * jj's `git_head()` keeps pointing at the old branch tip until re-imported,
    //     so its ancestors are the whole pre-reorder chain.
    // Intersecting with the ancestors of the *visible* heads drops those hidden
    // commits whatever ref still pins them, while leaving normal history
    // untouched. (`all()` is unsuitable: it deliberately includes hidden commits
    // referenced by the expression.)
    let any_remote = || RemoteRefSymbolExpression {
        name: StringExpression::all(),
        remote: StringExpression::all(),
    };
    let visible = RevsetExpression::visible_heads().ancestors();
    let user_expression = RevsetExpression::bookmarks(StringExpression::all())
        .union(&RevsetExpression::remote_bookmarks(any_remote(), None))
        .union(&RevsetExpression::tags(StringExpression::all()))
        .union(&RevsetExpression::git_head())
        .ancestors()
        .intersection(&visible);
    let symbol_resolver =
        SymbolResolver::new(repo, &([] as [&Box<dyn SymbolResolverExtension>; 0]));
    let expression = user_expression
        .resolve_user_expression(repo, &symbol_resolver)
        .context("resolving history revset")?;
    let revset = expression
        .evaluate(repo)
        .context("evaluating history revset")?;

    let store = repo.store();
    let root = store.root_commit_id().clone();
    let mut commits = Vec::new();
    for entry in revset.commit_change_ids() {
        let (id, _change_id) = entry.context("iterating history")?;
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
fn branch_chain(commits: &[CommitInfo], head: &CommitId) -> Vec<usize> {
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

#[cfg(test)]
mod tests {
    use super::{
        format_timestamp, is_linear_history, parse_timestamp, plan_reorder, CommitInfo,
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
