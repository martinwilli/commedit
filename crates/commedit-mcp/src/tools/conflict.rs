//! The conflict-resolution loop: status while a rewrite is held back, reading
//! conflicted files, applying resolutions, and bailing out.

use jj_lib::object_id::ObjectId as _;
use rmcp::handler::server::wrapper::Json;
use rmcp::{tool, tool_router, ErrorData};

use crate::convert::conflicted_commit_dto;
use crate::dto::PendingStatusResp;
use crate::server::CommeditServer;

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
}
