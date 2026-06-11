//! Session state and shared plumbing of the tool handlers: the blocking-work
//! wrapper, sha addressing against a fresh history read, the session trash,
//! and the reorder/restore splice planner.

use std::sync::PoisonError;

use commedit_engine::conflict::SaveOutcome;
use commedit_engine::graph::compute_graph;
use commedit_engine::history::{history, history_limited, CommitInfo, ReorderMove};
use commedit_engine::repo::Repo;
use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use rmcp::ErrorData;

use crate::error::{internal, invalid};
use crate::server::CommeditServer;

/// Dropped commits of this session, restorable until the server exits, plus
/// the trash mutation staged by an in-flight drop/restore/squash-from-trash.
///
/// The staged op mirrors the GTK side's `PendingTrashOp`: a mutation's effect
/// on the trash must not land while its rewrite is held back conflicted — only
/// [`TrashState::settle`] on a `Clean` outcome applies it, and an abort or
/// time-travel discards it.
#[derive(Default)]
pub struct TrashState {
    pub entries: Vec<CommitInfo>,
    pub staged: Option<PendingTrashOp>,
}

/// A trash mutation waiting for its rewrite to settle clean.
pub enum PendingTrashOp {
    /// `drop_commit`: push the dropped commit into the trash.
    Push(CommitInfo),
    /// `restore_commit` / squash-from-trash: remove the re-used commit.
    Remove(CommitId),
}

impl TrashState {
    /// Apply the staged trash op if `outcome` landed clean; keep it staged
    /// while conflicts hold the rewrite back.
    pub fn settle(&mut self, outcome: &SaveOutcome) {
        if matches!(outcome, SaveOutcome::Conflicts { .. }) {
            return;
        }
        match self.staged.take() {
            Some(PendingTrashOp::Push(info)) => self.entries.push(info),
            Some(PendingTrashOp::Remove(id)) => self.entries.retain(|c| c.id != id),
            None => {}
        }
    }
}

impl CommeditServer {
    /// Run `f` against the locked session on the blocking thread pool. The
    /// mutex serializes all tool work (single writer for free); it is taken
    /// inside the blocking task, never across an `.await`.
    pub(crate) async fn with_session<T, F>(&self, f: F) -> Result<T, ErrorData>
    where
        F: FnOnce(&mut Repo, &mut TrashState) -> Result<T, ErrorData> + Send + 'static,
        T: Send + 'static,
    {
        let repo = self.repo.clone();
        let trash = self.trash.clone();
        tokio::task::spawn_blocking(move || {
            let mut repo = repo.lock().unwrap_or_else(PoisonError::into_inner);
            let mut trash = trash.lock().unwrap_or_else(PoisonError::into_inner);
            f(&mut repo, &mut trash)
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("worker task failed: {e}"), None))?
    }
}

/// Refuse a mutation (or time-travel) while a conflicted rewrite is held: the
/// engine would silently drop the pending resolution; an explicit error
/// explaining the protocol is clearer for the agent.
pub fn ensure_not_pending(repo: &Repo) -> Result<(), ErrorData> {
    if repo.is_pending() {
        return Err(invalid(
            "a conflicted rewrite is pending: resolve it (pending_status, read_conflict, \
             resolve_conflicts) or abort_rewrite before any other operation",
        ));
    }
    Ok(())
}

/// The branch head plus the full ancestry walk. The full list (never a
/// truncated prefix) is what every planner runs on — the graph layout must
/// cover the whole history for the splice arithmetic to hold.
pub fn full_history(repo: &Repo) -> Result<(CommitId, Vec<CommitInfo>), ErrorData> {
    let head = head_commit(repo)?;
    let commits = history(&repo.repo, &head).map_err(internal)?;
    Ok((head, commits))
}

/// Like [`full_history`] but cut to `limit` commits, for `list_history`.
pub fn limited_history(
    repo: &Repo,
    limit: usize,
) -> Result<(CommitId, Vec<CommitInfo>, bool), ErrorData> {
    let head = head_commit(repo)?;
    let (commits, has_more) = history_limited(&repo.repo, &head, limit).map_err(internal)?;
    Ok((head, commits, has_more))
}

fn head_commit(repo: &Repo) -> Result<CommitId, ErrorData> {
    repo.head_commit_id().ok_or_else(|| {
        invalid("the repository has no branch head (detached or unborn HEAD); commedit edits the checked-out branch")
    })
}

/// A mutation outcome as the response DTO, with the (possibly moved) branch
/// tip read back after the save.
pub fn save_result(
    repo: &Repo,
    outcome: &SaveOutcome,
) -> crate::dto::SaveResultDto {
    crate::convert::save_result_dto(outcome, repo.head_commit_id().map(|id| id.hex()))
}

/// The display index of `sha` in the (newest-first) history.
pub fn find_commit(commits: &[CommitInfo], sha: &str) -> Result<usize, ErrorData> {
    commits.iter().position(|c| c.id_hex() == sha).ok_or_else(|| {
        invalid(format!(
            "commit {sha} is not in the current branch history; shas change after every \
             mutation — call list_history for fresh ones"
        ))
    })
}

/// The trash entry with id `sha`.
pub fn find_trashed(trash: &TrashState, sha: &str) -> Result<CommitInfo, ErrorData> {
    trash
        .entries
        .iter()
        .find(|c| c.id_hex() == sha)
        .cloned()
        .ok_or_else(|| invalid(format!("commit {sha} is not in the session trash (see list_trash)")))
}

/// What `plan_splice` is moving: a commit at a display index, or a trashed
/// commit being grafted back.
pub enum SpliceTarget {
    InHistory(usize),
    Trashed(CommitInfo),
}

/// Resolve the agent-facing move semantics — "make `new_parent` the
/// parent of the moved commit" (`"root"` = make it the repository's first
/// commit) — to the one concrete splice the graph planner offers, or a precise
/// error naming the alternatives.
pub fn plan_splice(
    repo: &Repo,
    commits: &[CommitInfo],
    target: SpliceTarget,
    new_parent: &str,
    child: Option<&str>,
) -> Result<ReorderMove, ErrorData> {
    let layout = compute_graph(commits, &repo.root_commit_id());

    // The insertion gap directly above the requested parent: gap i sits
    // between display rows i-1 and i, so the gap whose lower neighbor is the
    // parent P at index i is gap i; "root" is the synthetic gap below the
    // oldest row (commits.len()).
    let (to, parent_id) = if new_parent == "root" {
        (commits.len(), repo.root_commit_id())
    } else {
        let idx = find_commit(commits, new_parent)
            .map_err(|_| invalid(format!(
                "new_parent {new_parent} is not in the current branch history; \
                 use a ref from list_history or the literal \"root\""
            )))?;
        (idx, commits[idx].id.clone())
    };

    let candidates = match &target {
        SpliceTarget::InHistory(from) => {
            let moved = &commits[*from];
            if moved.id == parent_id {
                return Err(invalid("a commit cannot become its own parent"));
            }
            if moved.parents.len() != 1 {
                return Err(invalid("merge commits cannot be moved"));
            }
            if moved.parents[0] == parent_id {
                return Err(invalid(format!(
                    "commit {} is already a child of {new_parent}",
                    moved.id_hex()
                )));
            }
            repo.plan_reorder_candidates(commits, &layout, *from, to)
        }
        SpliceTarget::Trashed(info) => {
            if info.id == parent_id {
                return Err(invalid("a commit cannot become its own parent"));
            }
            repo.plan_restore_candidates(commits, &layout, info, to)
        }
    };

    let mut matching: Vec<ReorderMove> = candidates
        .iter()
        .filter(|c| c.mv.new_parents == [parent_id.clone()])
        .map(|c| c.mv.clone())
        .collect();

    match matching.len() {
        0 => {
            let offered: Vec<String> = candidates
                .iter()
                .flat_map(|c| c.mv.new_parents.iter().map(|p| p.hex()))
                .collect();
            if offered.is_empty() {
                Err(invalid(format!(
                    "no way to splice the commit under {new_parent}: the move is a no-op \
                     or the target is not reachable from the branch head"
                )))
            } else {
                Err(invalid(format!(
                    "no ancestry line at that position leads to parent {new_parent}; \
                     the gap's candidate parents are: {}",
                    offered.join(", ")
                )))
            }
        }
        1 => Ok(matching.remove(0)),
        _ => {
            // A fork: several child lines converge on the parent. The caller
            // picks which child the moved commit goes under.
            if let Some(child) = child {
                let child_id = CommitId::try_from_hex(child)
                    .ok_or_else(|| invalid(format!("invalid child {child:?}")))?;
                matching
                    .into_iter()
                    .find(|mv| mv.new_children.contains(&child_id))
                    .ok_or_else(|| {
                        invalid(format!(
                            "child {child} is not a child on any line converging on \
                             {new_parent}"
                        ))
                    })
            } else {
                let choices: Vec<String> = matching
                    .iter()
                    .map(|mv| {
                        let names: Vec<String> = mv
                            .new_children
                            .iter()
                            .map(|c| describe_commit(commits, c))
                            .collect();
                        names.join(" + ")
                    })
                    .collect();
                Err(invalid(format!(
                    "several lines converge on {new_parent}; pass child to pick which \
                     child the commit goes under: {}",
                    choices.join(" | ")
                )))
            }
        }
    }
}

fn describe_commit(commits: &[CommitInfo], id: &CommitId) -> String {
    match commits.iter().find(|c| c.id == *id) {
        Some(c) => format!("{} ({})", c.id_hex(), c.subject),
        None => id.hex(),
    }
}
