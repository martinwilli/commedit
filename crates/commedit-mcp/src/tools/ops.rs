//! The session op-log (listing recorded operations and time-travelling between
//! them), reloading one session to pick up external git changes, and the session
//! registry tools (list/open/close).

use std::path::PathBuf;
use std::sync::{Arc, PoisonError};

use commedit_engine::index_cache::IndexCache;
use commedit_engine::repo::Repo;
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::op_entry_dto;
use crate::dto::{
    CloseSessionResp, JumpToOperationReq, ListOperationsResp, ListSessionsResp, OpenSessionReq,
    OpenSessionResp, ReloadRepoReq, ReloadResp, SessionSel, TimeTravelResp,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{
    ensure_not_pending, resolve_slot, resolve_worktree_target, session_id_for, sessions_view,
    SessionSlot,
};
use crate::wrapper::Yaml;

#[tool_router(router = router_ops, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "List a session's recorded operations (every landed mutation), oldest first, with the undo cursor. Index 0 is the session start; an entry's index is the state right after it — both are jump_to_operation targets."
    )]
    pub async fn list_operations(
        &self,
        Parameters(req): Parameters<SessionSel>,
    ) -> Result<Yaml<ListOperationsResp>, ErrorData> {
        self.with_session(req.session, |repo, _| {
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
        description = "Step one recorded operation back in a session, restoring that state to git and the working tree (uncommitted changes made since are reset but stay recoverable by redo)."
    )]
    pub async fn undo(
        &self,
        Parameters(req): Parameters<SessionSel>,
    ) -> Result<Yaml<TimeTravelResp>, ErrorData> {
        self.with_session(req.session, |repo, _| {
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

    #[tool(description = "Step one undone operation forward again in a session.")]
    pub async fn redo(
        &self,
        Parameters(req): Parameters<SessionSel>,
    ) -> Result<Yaml<TimeTravelResp>, ErrorData> {
        self.with_session(req.session, |repo, _| {
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
        description = "Travel to any recorded state of a session: 0 restores the session start (undoing everything), an entry's index from list_operations restores the state right after that operation."
    )]
    pub async fn jump_to_operation(
        &self,
        Parameters(req): Parameters<JumpToOperationReq>,
    ) -> Result<Yaml<TimeTravelResp>, ErrorData> {
        self.with_session(req.session.session.clone(), move |repo, _| {
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
        description = "List every editing session this server hosts (one per branch of the one repository it serves), with each session's id (its branch short-name), root, worktree-bound flag and branch tip. Start here to discover what's open — the launch session is already listed. Use a session's id as the `session` selector on the other tools."
    )]
    pub async fn list_sessions(&self) -> Result<Yaml<ListSessionsResp>, ErrorData> {
        let sessions = self.sessions.clone();
        tokio::task::spawn_blocking(move || {
            Yaml(ListSessionsResp {
                sessions: sessions_view(&sessions),
            })
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("worker task failed: {e}"), None))
    }

    #[tool(
        description = "Open an editing session over a branch and return its id (the branch short-name), so several branches can be edited in parallel. Binding follows git: a branch checked out in a worktree opens worktree-bound (live working copy); one checked out nowhere opens off-worktree (only its ref moves, working-copy tools refused). The branch must exist. Refused if already open, or checked out in a worktree commedit can't bind. Returns the new id plus the full session list."
    )]
    pub async fn open_session(
        &self,
        Parameters(req): Parameters<OpenSessionReq>,
    ) -> Result<Yaml<OpenSessionResp>, ErrorData> {
        let sessions = self.sessions.clone();
        tokio::task::spawn_blocking(move || {
            // The prospective id is the branch short-name; fail fast if already
            // open (and grab the repository root for the worktree lookup).
            let id = req
                .branch
                .strip_prefix("refs/heads/")
                .unwrap_or(&req.branch)
                .to_string();
            let root = {
                let reg = sessions.lock().unwrap_or_else(PoisonError::into_inner);
                if reg.slots.contains_key(&id) {
                    return Err(invalid(format!(
                        "a session for branch '{id}' is already open; address it by that id, \
                         or close_session('{id}') first"
                    )));
                }
                reg.root.clone()
            };
            // git's branch→worktree mapping picks the anchor: resolve+verify the
            // branch exists, then see which worktree (if any) has it checked out.
            let full = commedit_engine::transparency::resolve_local_branch(&root, &req.branch)
                .map_err(|e| invalid(format!("{e:#}")))?;
            let (anchor, branch_arg): (PathBuf, Option<String>) =
                match commedit_engine::transparency::worktree_for_branch(&root, &full)
                    .map_err(internal)?
                {
                    // Checked out in a worktree: anchor there → worktree-bound, so
                    // the engine's "branch checked out elsewhere" refusal never fires
                    // (the agent never picks the worktree; git's mapping does).
                    Some(w) => (w, None),
                    // Checked out nowhere: anchor at the repo root → off-worktree.
                    None => (root.clone(), Some(id.clone())),
                };
            let repo = Repo::open_branch(&anchor, IndexCache::Default, branch_arg.as_deref())
                .map_err(internal)?;
            let actual_id = session_id_for(&repo);
            let slot = Arc::new(SessionSlot::new(repo));
            // Insert under the registry lock, re-checking for a concurrent open of
            // the same branch (the fail-fast check raced); on a clash `slot` drops.
            {
                let mut reg = sessions.lock().unwrap_or_else(PoisonError::into_inner);
                if reg.slots.contains_key(&actual_id) {
                    return Err(invalid(format!(
                        "a session for branch '{actual_id}' is already open"
                    )));
                }
                reg.slots.insert(actual_id.clone(), slot);
            }
            // Build the list with the new session in it (locks each repo once; no
            // repo lock is held here, so this can't deadlock or self-block).
            let view = sessions_view(&sessions);
            let (worktree_bound, head_sha) = view
                .iter()
                .find(|s| s.session == actual_id)
                .map(|s| (s.worktree_bound, s.head_sha.clone()))
                .unwrap_or((false, None));
            Ok(Yaml(OpenSessionResp {
                session: actual_id,
                worktree_bound,
                head_sha,
                sessions: view,
            }))
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("worker task failed: {e}"), None))?
    }

    #[tool(
        description = "Close an editing session, dropping its trash and operation log (git is untouched). Refused for the LAST session — the server always hosts at least one. Returns the remaining sessions."
    )]
    pub async fn close_session(
        &self,
        Parameters(req): Parameters<SessionSel>,
    ) -> Result<Yaml<CloseSessionResp>, ErrorData> {
        let sessions = self.sessions.clone();
        let id = req.session;
        tokio::task::spawn_blocking(move || {
            let slot = {
                let mut reg = sessions.lock().unwrap_or_else(PoisonError::into_inner);
                resolve_slot(&reg, &id)?; // validate existence (nice not-found error)
                if reg.slots.len() == 1 {
                    return Err(invalid(
                        "cannot close the last open session; the server always hosts at least \
                         one. Open another session first if you mean to switch away.",
                    ));
                }
                reg.slots.remove(&id).expect("just validated it exists")
            };
            // Flush the closed session's index cache before its slot drops (the
            // engine's Drop is the backstop if another in-flight call still holds it).
            slot.repo
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .flush_index_cache();
            drop(slot);
            Ok(Yaml(CloseSessionResp {
                closed: id,
                sessions: sessions_view(&sessions),
            }))
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("worker task failed: {e}"), None))?
    }

    #[tool(
        description = "Re-open ONE session to pick up out-of-band changes (git state isn't re-imported after startup): a branch switch, rebase or reset. Restarts that session fresh — its trash, op log (undo floor resets) and any pending rewrite are discarded; git and other sessions untouched. A plain `git commit` on HEAD needs no reload (absorbed automatically). `path` re-homes the session to another worktree of the SAME repo; `branch` switches which branch it edits and RE-KEYS the session (new id returned in `session`), refused if that branch is already open."
    )]
    pub async fn reload_repo(
        &self,
        Parameters(req): Parameters<ReloadRepoReq>,
    ) -> Result<Yaml<ReloadResp>, ErrorData> {
        // Deliberately not pending-guarded: a held rewrite never touched git, so
        // dropping it with the session state is safe. Skips the out-of-band
        // catch-up too — reopening from scratch is how reload handles a moved (or
        // switched) HEAD, so it must not be pre-empted by the catch-up.
        let sessions = self.sessions.clone();
        tokio::task::spawn_blocking(move || {
            let id = req.session.session.clone();
            let slot = {
                let reg = sessions.lock().unwrap_or_else(PoisonError::into_inner);
                resolve_slot(&reg, &id)?
            };
            let mut repo = slot.repo.lock().unwrap_or_else(PoisonError::into_inner);
            let mut trash = slot.trash.lock().unwrap_or_else(PoisonError::into_inner);

            // No-arg reload reopens this slot's live root; a `path` re-homes to a
            // sibling worktree, scope-guarded so it can only target this repository.
            let target = match &req.path {
                Some(p) => resolve_worktree_target(repo.workspace_root(), p)?,
                None => repo.workspace_root().to_path_buf(),
            };
            // The branch to edit: an explicit `branch` wins; otherwise keep the
            // current off-worktree target when reloading in place, but reset to the
            // worktree's checked-out branch when re-homing to a different one.
            let branch: Option<String> = req.branch.clone().or_else(|| {
                req.path
                    .is_none()
                    .then(|| repo.target_branch_name().map(str::to_string))
                    .flatten()
            });
            // Reopen into a temporary; only commit once a re-key collision is ruled
            // out, so a refusal leaves the original session fully intact.
            let fresh = Repo::open_branch(&target, IndexCache::Default, branch.as_deref())
                .map_err(internal)?;
            let new_id = session_id_for(&fresh);
            {
                let mut reg = sessions.lock().unwrap_or_else(PoisonError::into_inner);
                if new_id != id && reg.slots.contains_key(&new_id) {
                    return Err(invalid(format!(
                        "a session for branch '{new_id}' is already open; close it before \
                         reloading '{id}' onto that branch"
                    )));
                }
                // Re-key while still holding this slot's repo lock, so a tool that
                // resolves the new id blocks on the lock until the swap below lands.
                if new_id != id {
                    if let Some(arc) = reg.slots.remove(&id) {
                        reg.slots.insert(new_id.clone(), arc);
                    }
                }
            }
            // Swap outside the registry lock so the old Repo's Drop (an index-cache
            // flush) doesn't hold up unrelated session lookups.
            *repo = fresh;
            trash.entries.clear();
            trash.staged = None;
            Ok(Yaml(ReloadResp {
                session: new_id,
                head_sha: repo.head_commit_id().map(|id| id.hex()),
                root: repo.workspace_root().display().to_string(),
                branch: repo.target_branch_name().map(str::to_string),
                worktree_bound: repo.is_worktree_bound(),
            }))
        })
        .await
        .map_err(|e| ErrorData::internal_error(format!("worker task failed: {e}"), None))?
    }
}

/// The state both undo/redo and jump report back: the restored tip + cursor.
fn time_travel_resp(repo: &Repo) -> TimeTravelResp {
    TimeTravelResp {
        head_sha: repo.head_commit_id().map(|id| id.hex()),
        cursor: repo.op_cursor(),
    }
}
