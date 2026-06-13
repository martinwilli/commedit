//! The session op-log: listing recorded operations, time-travelling between
//! them, and reloading the repository to pick up external git changes.

use commedit_engine::repo::Repo;
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::op_entry_dto;
use crate::dto::{
    JumpToOperationReq, ListOperationsResp, ReloadRepoReq, ReloadResp, TimeTravelResp,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{ensure_not_pending, resolve_worktree_target};
use crate::wrapper::Yaml;

#[tool_router(router = router_ops, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "List this session's recorded operations (every landed mutation), oldest first, with the undo cursor. Index 0 is the session start; an entry's index is the state right after it — both are jump_to_operation targets."
    )]
    pub async fn list_operations(&self) -> Result<Yaml<ListOperationsResp>, ErrorData> {
        self.with_session(|repo, _| {
            Ok(ListOperationsResp {
                ops: repo
                    .session_ops()
                    .iter()
                    .enumerate()
                    .map(|(i, e)| op_entry_dto(i + 1, e))
                    .collect(),
                cursor: repo.op_cursor(),
                can_undo: repo.can_undo(),
                can_redo: repo.can_redo(),
                pending: repo.is_pending(),
            })
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Step one recorded operation back, restoring that state to git and the working tree (uncommitted changes made since are reset but stay recoverable by redo)."
    )]
    pub async fn undo(&self) -> Result<Yaml<TimeTravelResp>, ErrorData> {
        self.with_session(|repo, _| {
            ensure_not_pending(repo)?;
            if !repo.can_undo() {
                return Err(invalid(
                    "already at the session start — there is nothing to undo",
                ));
            }
            repo.undo().map_err(internal)?;
            Ok(time_travel_resp(repo))
        })
        .await
        .map(Yaml)
    }

    #[tool(description = "Step one undone operation forward again.")]
    pub async fn redo(&self) -> Result<Yaml<TimeTravelResp>, ErrorData> {
        self.with_session(|repo, _| {
            ensure_not_pending(repo)?;
            if !repo.can_redo() {
                return Err(invalid(
                    "already at the latest recorded state — there is nothing to redo",
                ));
            }
            repo.redo().map_err(internal)?;
            Ok(time_travel_resp(repo))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Travel to any recorded session state: 0 restores the session start (undoing everything), an entry's index from list_operations restores the state right after that operation."
    )]
    pub async fn jump_to_operation(
        &self,
        Parameters(req): Parameters<JumpToOperationReq>,
    ) -> Result<Yaml<TimeTravelResp>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let max = repo.session_ops().len();
            if req.index > max {
                return Err(invalid(format!(
                    "operation index {} is out of range (0 = session start, {max} = latest)",
                    req.index
                )));
            }
            repo.jump_to_op(req.index).map_err(internal)?;
            Ok(time_travel_resp(repo))
        })
        .await
        .map(Yaml)
    }

    #[tool(
        description = "Re-open the repository to pick up changes made outside this server (a git commit, branch switch, rebase, …) — git state is otherwise imported only at startup. This starts a fresh session in place: the trash, the operation log (the undo floor resets to now) and any pending rewrite are discarded; git itself is untouched. Pass `path` to re-home the session to a different worktree of the SAME repository (its main checkout or any linked worktree) — e.g. to edit history isolated in a `git worktree`; a path outside this repository's worktrees is refused."
    )]
    pub async fn reload_repo(
        &self,
        Parameters(req): Parameters<ReloadRepoReq>,
    ) -> Result<Yaml<ReloadResp>, ErrorData> {
        // Deliberately not pending-guarded: a held rewrite never touched git,
        // so dropping it with the session state is safe. Skips the out-of-band
        // catch-up too — reopening from scratch is how reload handles a moved
        // (or switched) HEAD, so it must not be pre-empted by the catch-up.
        self.with_session_no_sync(move |repo, trash| {
            // No-arg reload reopens the live root; a `path` re-homes to a sibling
            // worktree, scope-guarded so it can only target this same repository.
            let target = match req.path {
                Some(p) => resolve_worktree_target(repo.workspace_root(), &p)?,
                None => repo.workspace_root().to_path_buf(),
            };
            // Only swap on success — a failed reload keeps the current session.
            let fresh = Repo::open(&target).map_err(internal)?;
            *repo = fresh;
            trash.entries.clear();
            trash.staged = None;
            Ok(ReloadResp {
                head_sha: repo.head_commit_id().map(|id| id.hex()),
                root: repo.workspace_root().display().to_string(),
            })
        })
        .await
        .map(Yaml)
    }
}

/// The state both undo/redo and jump report back: the restored tip + cursor.
fn time_travel_resp(repo: &Repo) -> TimeTravelResp {
    TimeTravelResp {
        head_sha: repo.head_commit_id().map(|id| id.hex()),
        cursor: repo.op_cursor(),
    }
}
