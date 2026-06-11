//! Read-only tools over the history and the session trash.

use commedit_engine::diff::commit_changes;
use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::{commit_dto, file_change_dto};
use crate::dto::{
    ListHistoryReq, ListHistoryResp, ListTrashResp, ShowCommitReq, ShowCommitResp,
};
use crate::error::{internal, invalid};
use crate::server::CommeditServer;
use crate::session::limited_history;

#[tool_router(router = router_read, vis = "pub")]
impl CommeditServer {
    #[tool(
        description = "List the commits of the checked-out branch (the ancestors of HEAD, newest first, like `git log`), with their branch/tag decorations. Merge commits are included but cannot be moved, dropped, split or used as a squash source. Shas change on every mutation — re-list instead of reusing them."
    )]
    pub async fn list_history(
        &self,
        Parameters(req): Parameters<ListHistoryReq>,
    ) -> Result<Json<ListHistoryResp>, ErrorData> {
        self.with_session(move |repo, trash| {
            let trash_count = trash.entries.len();
            let Some(_) = repo.head_commit_id() else {
                return Ok(ListHistoryResp {
                    head_sha: None,
                    commits: Vec::new(),
                    has_more: false,
                    trash_count,
                });
            };
            let (head, commits, has_more) =
                limited_history(repo, req.limit.unwrap_or(usize::MAX))?;
            let refs = repo.commit_refs();
            let root = repo.root_commit_id().hex();
            Ok(ListHistoryResp {
                head_sha: Some(head.hex()),
                commits: commits.iter().map(|c| commit_dto(c, &root, &refs)).collect(),
                has_more,
                trash_count,
            })
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "Show one commit's metadata and the files it changes, each as a unified diff. Accepts a history sha, a working-copy entry sha (the uncommitted diff) or a trashed commit's sha. Set include_contents to also get each text file's full old/new content."
    )]
    pub async fn show_commit(
        &self,
        Parameters(req): Parameters<ShowCommitReq>,
    ) -> Result<Json<ShowCommitResp>, ErrorData> {
        self.with_session(move |repo, trash| {
            let (_, commits) = crate::session::full_history(repo)?;
            let info = commits
                .iter()
                .find(|c| c.id_hex() == req.commit)
                .cloned()
                .or_else(|| {
                    repo.working_copy_chain()
                        .into_iter()
                        .map(|e| e.info)
                        .find(|i| i.id_hex() == req.commit)
                })
                .or_else(|| trash.entries.iter().find(|c| c.id_hex() == req.commit).cloned())
                .ok_or_else(|| {
                    invalid(format!(
                        "commit {} not found in the branch history, the working copy or the \
                         trash; shas change after every mutation — call list_history for \
                         fresh ones",
                        req.commit
                    ))
                })?;
            let refs = repo.commit_refs();
            let root = repo.root_commit_id().hex();
            let include = req.include_contents.unwrap_or(false);
            let files = commit_changes(&repo.repo, &info.id)
                .map_err(internal)?
                .iter()
                .map(|fc| file_change_dto(fc, include))
                .collect();
            Ok(ShowCommitResp { commit: commit_dto(&info, &root, &refs), files })
        })
        .await
        .map(Json)
    }

    #[tool(
        description = "List the commits dropped to the session trash. They stay restorable (restore_commit, or squash_commit with a trashed source) until the session ends."
    )]
    pub async fn list_trash(&self) -> Result<Json<ListTrashResp>, ErrorData> {
        self.with_session(|repo, trash| {
            let refs = repo.commit_refs();
            let root = repo.root_commit_id().hex();
            Ok(ListTrashResp {
                commits: trash.entries.iter().map(|c| commit_dto(c, &root, &refs)).collect(),
            })
        })
        .await
        .map(Json)
    }
}
