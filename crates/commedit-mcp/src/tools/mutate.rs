//! History mutations. Every tool here follows the engine's mutation pipeline:
//! resolve the target against a fresh history read, run the rewrite, and
//! report the [`SaveResultDto`] — `clean` (exported to git) or `conflicts`
//! (held back until the conflict tools settle it).

use std::collections::BTreeMap;

use commedit_engine::rewrite::Identity;
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::commit_dto;
use crate::dto::{
    DropCommitReq, DropCommitResp, EditIdentityReq, EditMessageReq, FileContentDto,
    ReorderCommitReq, ReplaceFilesReq, RestoreCommitReq, SaveResultDto, SplitCommitReq,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::{
    ensure_not_pending, find_commit, find_trashed, full_history, plan_splice, save_result,
    PendingTrashOp, SpliceTarget, TrashState,
};

/// Run a mutation whose trash effect is staged: on an engine error the staged
/// op must not linger (the mutation never happened), on success it lands only
/// once the outcome settles clean.
fn run_staged<F>(
    repo: &mut commedit_engine::repo::Repo,
    trash: &mut TrashState,
    staged: PendingTrashOp,
    mutate: F,
) -> Result<commedit_engine::conflict::SaveOutcome, ErrorData>
where
    F: FnOnce(&mut commedit_engine::repo::Repo) -> anyhow::Result<commedit_engine::conflict::SaveOutcome>,
{
    trash.staged = Some(staged);
    match mutate(repo) {
        Ok(outcome) => {
            trash.settle(&outcome);
            Ok(outcome)
        }
        Err(e) => {
            trash.staged = None;
            Err(internal(e))
        }
    }
}

/// Lower a request's file list to the engine's `(path, content)` pairs,
/// refusing an empty list up front (the engine's message names "the diff",
/// which means nothing to an MCP caller).
fn file_pairs(files: Vec<FileContentDto>) -> Result<Vec<(String, String)>, ErrorData> {
    if files.is_empty() {
        return Err(invalid("files must not be empty"));
    }
    Ok(files.into_iter().map(|f| (f.path, f.content)).collect())
}

#[tool_router(router = router_mutate, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "Replace a commit's message (subject + body). Descendants are rebased; the commit's sha changes."
    )]
    pub async fn edit_message(
        &self,
        Parameters(req): Parameters<EditMessageReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.sha)?;
            let outcome = repo
                .rewrite_message(&commits[idx].id, &req.message)
                .map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Change a commit's author and/or committer (name, email, date). Omitted fields keep their current value. Unlike other edits this also pins the committer timestamp instead of re-stamping it to now."
    )]
    pub async fn edit_identity(
        &self,
        Parameters(req): Parameters<EditIdentityReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.sha)?;
            let c = &commits[idx];
            let identity = Identity {
                author_name: req.author_name.unwrap_or_else(|| c.author_name.clone()),
                author_email: req.author_email.unwrap_or_else(|| c.author_email.clone()),
                author_time: req.author_time.unwrap_or_else(|| c.author_time.clone()),
                committer_name: req
                    .committer_name
                    .unwrap_or_else(|| c.committer_name.clone()),
                committer_email: req
                    .committer_email
                    .unwrap_or_else(|| c.committer_email.clone()),
                committer_time: req
                    .committer_time
                    .unwrap_or_else(|| c.committer_time.clone()),
            };
            let outcome = repo.rewrite_identity(&c.id, &identity).map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Replace file contents inside a commit (whole-file replacement, no patch format). A path the commit doesn't have is added; deleting a file from a commit is not supported. Descendants are rebased onto the edited tree and may report conflicts."
    )]
    pub async fn replace_files(
        &self,
        Parameters(req): Parameters<ReplaceFilesReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.sha)?;
            let files = file_pairs(req.files)?;
            let outcome = repo.rewrite_files(&commits[idx].id, &files).map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Split a commit in two: the commit keeps the given file contents (the subset to retain, as in replace_files), and a new `fixup!` child commit receives the remainder, so both combined reproduce the original change. Descendants are untouched."
    )]
    pub async fn split_commit(
        &self,
        Parameters(req): Parameters<SplitCommitReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.sha)?;
            let files = file_pairs(req.files)?;
            let outcome = repo.split_commit(&commits[idx].id, &files).map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Drop a commit from history: its children rebase onto its parent, and the commit moves to the session trash (restorable via restore_commit or squash_commit). Merge commits and the branch's only commit cannot be dropped."
    )]
    pub async fn drop_commit(
        &self,
        Parameters(req): Parameters<DropCommitReq>,
    ) -> Result<Json<DropCommitResp>, ErrorData> {
        self.with_session(move |repo, trash| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.sha)?;
            let id = repo.plan_drop(&commits, idx).ok_or_else(|| {
                invalid(
                    "this commit cannot be dropped: merge commits and the branch's only \
                     commit stay fixed",
                )
            })?;
            let info = commits[idx].clone();
            let root = repo.root_commit_id().hex();
            let dropped = commit_dto(&info, &root, &BTreeMap::new());
            let outcome = run_staged(repo, trash, PendingTrashOp::Push(info), |repo| {
                repo.abandon_commit(&id)
            })?;
            Ok(DropCommitResp { result: save_result(repo, &outcome), dropped })
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Move a commit to another place in the history: new_parent_sha names the commit that becomes its parent (or `root` for the very first position). A true rebase — commits that don't commute report conflicts. Merge commits cannot be moved."
    )]
    pub async fn reorder_commit(
        &self,
        Parameters(req): Parameters<ReorderCommitReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, _| {
            ensure_not_pending(repo)?;
            let (_, commits) = full_history(repo)?;
            let idx = find_commit(&commits, &req.sha)?;
            let mv = plan_splice(
                repo,
                &commits,
                SpliceTarget::InHistory(idx),
                &req.new_parent_sha,
                req.child_sha.as_deref(),
            )?;
            let outcome = repo
                .reorder_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
                .map_err(internal)?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Graft a trashed commit (see list_trash) back into the history, like reorder_commit: new_parent_sha names the commit that becomes its parent (or `root`). On success it leaves the trash."
    )]
    pub async fn restore_commit(
        &self,
        Parameters(req): Parameters<RestoreCommitReq>,
    ) -> Result<Json<SaveResultDto>, ErrorData> {
        self.with_session(move |repo, trash| {
            ensure_not_pending(repo)?;
            let info = find_trashed(trash, &req.sha)?;
            let (_, commits) = full_history(repo)?;
            let mv = plan_splice(
                repo,
                &commits,
                SpliceTarget::Trashed(info.clone()),
                &req.new_parent_sha,
                req.child_sha.as_deref(),
            )?;
            let outcome = run_staged(repo, trash, PendingTrashOp::Remove(info.id), |repo| {
                repo.restore_commit(&mv.target, mv.new_parents, mv.new_children, &mv.new_tip)
            })?;
            Ok(save_result(repo, &outcome))
        })
        .await
        .map(Json)
    }
}
