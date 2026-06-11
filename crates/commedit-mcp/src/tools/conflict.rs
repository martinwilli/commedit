//! The conflict-resolution loop: status while a rewrite is held back, reading
//! conflicted files, applying resolutions, and bailing out.

use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::conflicted_commit_dto;
use crate::dto::{
    AbortResp, PendingStatusResp, ReadConflictReq, ReadConflictResp, ResolveConflictsReq,
    SaveResultDto,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::save_result;

#[tool_router(router = router_conflict, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "Whether a conflicted rewrite is pending. While pending, git still shows the pre-rewrite history (git_head_sha) and the held rewrite's tip is jj_head_sha; no other mutation is allowed until the conflicts resolve or the rewrite is aborted."
    )]
    pub async fn pending_status(&self) -> Result<Json<PendingStatusResp>, ErrorData> {
        self.with_session(|repo, _| {
            Ok(PendingStatusResp {
                pending: repo.is_pending(),
                git_head_sha: repo.head_commit_id().map(|id| id.hex()),
                jj_head_sha: repo.jj_head_commit_id().map(|id| id.hex()),
                conflicts: repo
                    .pending_conflicts()
                    .unwrap_or(&[])
                    .iter()
                    .map(conflicted_commit_dto)
                    .collect(),
            })
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Read one conflicted file of a pending rewrite, materialized with git-style conflict markers. Address the commit by its change_id (shas churn on every resolution step). Resolve commits oldest-first — fixing the earliest often auto-clears its descendants."
    )]
    pub async fn read_conflict(
        &self,
        Parameters(req): Parameters<ReadConflictReq>,
    ) -> Result<Json<ReadConflictResp>, ErrorData> {
        self.with_session(move |repo, _| {
            let conflicts = repo
                .pending_conflicts()
                .ok_or_else(|| invalid("no conflicted rewrite is pending"))?;
            let commit = conflicts
                .iter()
                .find(|c| c.change_id_hex() == req.change_id)
                .ok_or_else(|| {
                    invalid(format!(
                        "change {} is not conflicted (see pending_status)",
                        req.change_id
                    ))
                })?;
            let path = commit
                .files
                .iter()
                .find(|f| f.path_str() == req.path)
                .ok_or_else(|| {
                    invalid(format!(
                        "{} is not a conflicted path of change {}; its conflicted files are: {}",
                        req.path,
                        req.change_id,
                        commit
                            .files
                            .iter()
                            .map(|f| f.path_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
            if !path.resolvable {
                return Err(invalid(format!(
                    "{} is a structural conflict (not plain file content) and cannot be \
                     resolved as text; abort_rewrite is the only way out",
                    req.path
                )));
            }
            let file = repo.read_conflict(&req.change_id, &req.path).map_err(internal)?;
            Ok(ReadConflictResp {
                text: file.text,
                marker_len: file.marker_len,
                num_sides: file.num_sides,
            })
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Apply resolved contents for one conflicted commit's files (all markers removed, each echoing its marker_len from read_conflict). Re-rebases the chain: the result is either still-conflicted (continue with the remaining commits) or clean — at which point the whole held-back rewrite is exported to git."
    )]
    pub async fn resolve_conflicts(
        &self,
        Parameters(req): Parameters<ResolveConflictsReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, trash| {
            if !repo.is_pending() {
                return Err(invalid("no conflicted rewrite is pending"));
            }
            if req.files.is_empty() {
                return Err(invalid("files must not be empty"));
            }
            let files: Vec<(String, String, usize)> = req
                .files
                .into_iter()
                .map(|f| (f.path, f.text, f.marker_len))
                .collect();
            let outcome = repo
                .resolve_conflicts(&req.change_id, &files)
                .map_err(internal)?;
            trash.settle(&outcome);
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Discard the pending conflicted rewrite. Git was never touched while it was held back, so the pre-rewrite history is simply still in place."
    )]
    pub async fn abort_rewrite(&self) -> Result<Json<AbortResp>, ErrorData> {
        self.with_session(|repo, trash| {
            if !repo.is_pending() {
                return Err(invalid("no conflicted rewrite is pending"));
            }
            repo.abort().map_err(internal)?;
            // The aborted mutation's trash effect must not land.
            trash.staged = None;
            Ok(AbortResp {
                ok: true,
                head_sha: repo.head_commit_id().map(|id| id.hex()),
            })
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Re-check a pending rewrite and export it if its chain is already clean (e.g. after resolutions that auto-cleared the rest). A no-op returning clean when nothing is pending."
    )]
    pub async fn finalize(&self) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(|repo, trash| {
            let outcome = repo.finalize().map_err(internal)?;
            trash.settle(&outcome);
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }
}
