//! Walk the repository into a flat, topologically ordered list of commits for
//! the history view (children before parents, like gitk).

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

/// List all visible commits in topological order (newest first), excluding the
/// virtual root commit.
pub fn history(repo: &ReadonlyRepo) -> Result<Vec<CommitInfo>> {
    // Mirror what `git log`/`gitk` show: commits reachable from the git refs and
    // git HEAD. jj's `all()` additionally surfaces divergent (pre-rewrite) and
    // working-copy commits, which git never created a ref for — they would show
    // up here as confusing duplicates of the commits that replaced them.
    let user_expression = RevsetExpression::git_refs()
        .union(&RevsetExpression::git_head())
        .ancestors();
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

/// Plan a drag of the commit at display index `from` (in a newest-first history)
/// to the insertion gap `to` (`0..=len`, meaning "before row `to`"). Returns
/// `None` for an out-of-range or no-op drop.
///
/// History is newest-first, so a row's *upper* neighbour is its child (newer) and
/// its *lower* neighbour is its parent (older). After conceptually removing the
/// dragged commit, the gap `g` sits between `others[g - 1]` (the new child) and
/// `others[g]` (the new parent).
pub fn plan_reorder(commits: &[CommitInfo], from: usize, to: usize) -> Option<ReorderMove> {
    let n = commits.len();
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
    // Index into `commits` skipping the dragged row.
    let other = |i: usize| -> &CommitInfo {
        if i < from {
            &commits[i]
        } else {
            &commits[i + 1]
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
        commits[from].id.clone()
    } else {
        other(0).id.clone()
    };

    Some(ReorderMove {
        target: commits[from].id.clone(),
        new_parents,
        new_children,
        new_tip,
    })
}

#[cfg(test)]
mod tests {
    use super::{format_timestamp, parse_timestamp, plan_reorder, CommitInfo};
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
        assert_eq!(plan_reorder(&h, 1, 1), None); // onto its own upper half
        assert_eq!(plan_reorder(&h, 1, 2), None); // onto its own lower half
    }

    #[test]
    fn moving_the_tip_down_to_the_oldest_position() {
        // Drag row 0 (tip "3") to the very bottom (gap 3).
        let mv = plan_reorder(&history(), 0, 3).expect("move");
        assert_eq!(mv.target, cid(3));
        assert_eq!(mv.new_parents, vec![cid(0)]); // onto the root
        assert_eq!(mv.new_children, vec![cid(1)]); // old oldest becomes its child
        assert_eq!(mv.new_tip, cid(2)); // "2" becomes the head
    }

    #[test]
    fn moving_a_commit_up_to_the_top() {
        // Drag row 2 (oldest "1") to the top (gap 0).
        let mv = plan_reorder(&history(), 2, 0).expect("move");
        assert_eq!(mv.target, cid(1));
        assert_eq!(mv.new_parents, vec![cid(3)]); // onto the old tip
        assert!(mv.new_children.is_empty()); // becomes the new head
        assert_eq!(mv.new_tip, cid(1)); // ...so it is the tip
    }

    #[test]
    fn moving_a_commit_into_the_middle() {
        // Drag row 0 (tip "3") down to sit between "2" and "1" (gap 2).
        let mv = plan_reorder(&history(), 0, 2).expect("move");
        assert_eq!(mv.target, cid(3));
        assert_eq!(mv.new_parents, vec![cid(1)]); // parent the older "1"
        assert_eq!(mv.new_children, vec![cid(2)]); // child the newer "2"
        assert_eq!(mv.new_tip, cid(2)); // "2" is now the head
    }

    #[test]
    fn out_of_range_is_rejected() {
        assert_eq!(plan_reorder(&history(), 3, 0), None);
        assert_eq!(plan_reorder(&history(), 0, 4), None);
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
